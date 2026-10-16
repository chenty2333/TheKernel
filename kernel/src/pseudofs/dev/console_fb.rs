//! The in-kernel console's scanout: the surface the VT text console draws
//! into, the damage that makes those writes visible, and the refresh worker
//! that presents them.
//!
//! The console does not depend on `/dev/fb0`. Devfs installs it on the surface
//! that `crate::drm::screen::console_scanout()` selected, and whichever
//! `/dev/fb0` implementation is then published shares that same surface.
//!
//! Leaving text mode for a graphics VT suspends the console and the fbdev
//! client behind the surface (Linux `drm_fb_helper_set_suspend`); returning to
//! text restores the fbdev mode and repaints (Linux
//! `drm_fb_helper_restore_fbdev_mode_unlocked`).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::VfsResult;
use axsync::Mutex;
use axtask::WaitQueue;
use kspin::SpinNoIrq;
use lazy_static::lazy_static;

use super::{fb::default_pseudo_palette, scanout::ScanoutSurface};

const DAMAGE_CLEAN: u64 = 0;
const DAMAGE_FULL: u64 = u64::MAX;

/// A single, bounded damage record.  This deliberately coalesces all writes
/// between commits instead of allocating an unbounded list of rectangles.
struct DamageTracker {
    state: AtomicU64,
}

impl DamageTracker {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(DAMAGE_CLEAN),
        }
    }

    fn mark(&self, start: usize, len: usize, size: usize) {
        if len == 0 || start >= size {
            return;
        }
        let end = start.saturating_add(len).min(size);
        // fb_fix_screeninfo.smem_len is u32, so this representation covers
        // every fbdev mapping we advertise.
        if end > u32::MAX as usize {
            self.state.store(DAMAGE_FULL, Ordering::Release);
            return;
        }
        let update = ((start as u64) << 32) | end as u64;
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let merged = match current {
                DAMAGE_CLEAN => update,
                DAMAGE_FULL => return,
                value => {
                    let old_start = (value >> 32) as usize;
                    let old_end = value as u32 as usize;
                    ((old_start.min(start) as u64) << 32) | old_end.max(end) as u64
                }
            };
            match self.state.compare_exchange_weak(
                current,
                merged,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(value) => current = value,
            }
        }
    }

    fn mark_full(&self) {
        self.state.store(DAMAGE_FULL, Ordering::Release);
    }

    fn take(&self) -> u64 {
        self.state.swap(DAMAGE_CLEAN, Ordering::AcqRel)
    }
}

/// The sole fbdev-side scanout authority. Its backing is whatever
/// [`ScanoutSurface`] the platform installed; fbdev and fbcon do not retain a
/// raw-display ownership path of their own.
pub(crate) struct DisplayCore {
    pub(crate) scanout: alloc::sync::Arc<dyn ScanoutSurface>,
    pub(crate) size: usize,
    /// Linux truecolor pseudo-palette, used by fbcon-style 0..15 pixel
    /// values. It is software state and never a hardware CLUT claim.
    pub(crate) pseudo_palette: Mutex<[u32; 16]>,
    damage: DamageTracker,
    /// Set after a writer publishes damage. The refresh task clears it before
    /// taking damage, so a concurrent writer cannot lose its wakeup.
    pending: AtomicBool,
    /// fbdev owns master only while the active VT is in KD_TEXT.  Damage may
    /// accumulate while false, but no refresh task may submit presentation.
    refresh_enabled: AtomicBool,
    /// Kept separate from `DisplayCore` in the worker: the worker holds only
    /// this queue plus a Weak, never a strong reference while idle.
    refresh: alloc::sync::Arc<WaitQueue>,
    /// Serializes fbcon drawing with driver flushes and explicit publication.
    commit: Mutex<()>,
}

lazy_static! {
    // fbcon has no ownership of fbdev.  The weak handle lets its rendering
    // path disappear together with /dev/fb0 and its refresh worker.
    static ref FBCON_DISPLAY: SpinNoIrq<Option<alloc::sync::Weak<DisplayCore>>> =
        SpinNoIrq::new(None);
}

#[cfg(test)]
pub(crate) fn replace_console_for_test(
    display: Option<Arc<DisplayCore>>,
) -> Option<Arc<DisplayCore>> {
    core::mem::replace(&mut *CONSOLE.lock(), display)
}

/// A short-lived, exclusive-in-practice view used only by the in-kernel
/// framebuffer console. fbdev users still own the ABI surface; fbcon merely
/// repaints before KD_TEXT scanout commits.
pub(crate) struct FbconFrame {
    scanout: alloc::sync::Arc<dyn ScanoutSurface>,
    size: usize,
    width: usize,
    height: usize,
    pitch: usize,
    /// Bytes one pixel occupies on this surface, which is not always four.
    bytes_per_pixel: usize,
}

impl FbconFrame {
    pub(crate) fn geometry(&self) -> (usize, usize, usize) {
        (self.width, self.height, self.pitch)
    }

    pub(crate) fn cell_size(&self) -> (usize, usize) {
        let scale = super::console_font::glyph_scale(self.width, self.height);
        (super::console_font::GLYPH_WIDTH * scale, super::console_font::GLYPH_HEIGHT * scale)
    }

    /// Clear only the strips not covered by opaque text cells. Clearing the
    /// text area before repainting makes a live firmware scanout flash black.
    pub(crate) fn clear_margins(&self, text_width: usize, text_height: usize) {
        for y in 0..self.height {
            let first = if y < text_height { text_width.min(self.width) } else { 0 };
            for x in first..self.width {
                self.write_pixel(x, y, 0);
            }
        }
    }

    pub(crate) fn glyph(&self, x: usize, y: usize, byte: u8, fg: u32, bg: u32, cursor: bool) -> bool {
        let rows = super::console_font::glyph(byte);
        let scale = super::console_font::glyph_scale(self.width, self.height);
        let (cell_width, cell_height) = self.cell_size();
        if x >= self.width || y >= self.height {
            return false;
        }
        let mut foreground = [0u8; 4];
        let mut background = [0u8; 4];
        let layout = self.scanout.pixel_layout();
        let bpp = layout.encode_into(fg, &mut foreground);
        if bpp == 0 || bpp != self.bytes_per_pixel || bpp > 4 {
            return false;
        }
        layout.encode_into(bg, &mut background);
        // One short row, not a framebuffer-sized stack allocation. Encoding
        // and writing a span once avoids a virtual call/MMIO store per pixel.
        let mut row = [0u8; super::console_font::GLYPH_WIDTH * super::console_font::UHD_GLYPH_SCALE * 4];
        let pixels = cell_width.min(self.width - x);
        let row_bytes = pixels * bpp;
        for dy in 0..cell_height.min(self.height - y) {
            let source_y = dy / scale;
            let bits = rows.map_or(0, |rows| rows[source_y]);
            for dx in 0..pixels {
                let source_x = dx / scale;
                let cursor_ink = cursor
                    && source_y == super::console_font::GLYPH_HEIGHT - 2
                    && source_x > 0
                    && source_x + 1 < super::console_font::GLYPH_WIDTH;
                let color = if bits & (0x80 >> source_x) != 0 || cursor_ink {
                    &foreground
                } else {
                    &background
                };
                row[dx * bpp..(dx + 1) * bpp].copy_from_slice(&color[..bpp]);
            }
            let Some(offset) = (y + dy).checked_mul(self.pitch)
                .and_then(|start| start.checked_add(x.checked_mul(bpp)?)) else {
                return false;
            };
            if offset.checked_add(row_bytes).is_none_or(|end| end > self.size)
                || self.scanout.write_bytes(offset, &row[..row_bytes]).is_err() {
                return false;
            }
        }
        true
    }

    fn write_pixel(&self, x: usize, y: usize, color: u32) {
        let Some(offset) = y
            .checked_mul(self.pitch)
            .and_then(|row| row.checked_add(x.checked_mul(self.bytes_per_pixel)?))
        else {
            return;
        };
        if offset + self.bytes_per_pixel > self.size {
            return;
        }
        // The surface encodes the colour in its own layout, so this path never
        // needs to know the pixel format.  A failed write is dropped rather
        // than propagated: the console clips its own output and must never be
        // able to fault the kernel.
        self.scanout.write_pixel(offset, color);
    }
}

impl DisplayCore {
    pub(crate) fn new(scanout: alloc::sync::Arc<dyn ScanoutSurface>) -> Self {
        Self {
            size: scanout.len(),
            scanout,
            pseudo_palette: Mutex::new(default_pseudo_palette()),
            damage: DamageTracker::new(),
            pending: AtomicBool::new(false),
            refresh_enabled: AtomicBool::new(true),
            refresh: alloc::sync::Arc::new(WaitQueue::new()),
            commit: Mutex::new(()),
        }
    }

    pub(crate) fn mark_write(&self, offset: usize, len: usize) {
        self.damage.mark(offset, len, self.size);
        self.schedule_refresh();
    }

    pub(crate) fn mark_full(&self) {
        self.damage.mark_full();
        self.schedule_refresh();
    }

    fn schedule_refresh(&self) {
        if !self.refresh_enabled.load(Ordering::Acquire) {
            return;
        }
        // The condition is published before waking the waiter. WaitQueue's
        // check-arm-check wait closes the other side of this race.
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.refresh.notify_one(false);
        }
    }

    fn take_scheduled(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    fn suspend_refresh(&self) {
        // Publish disablement before waiting for an in-flight presentation.
        // A worker which woke during this transition sees false under the same
        // commit lock and leaves its bounded damage record intact.
        self.refresh_enabled.store(false, Ordering::Release);
        self.pending.store(false, Ordering::Release);
        let _commit = self.commit.lock();
    }

    fn resume_refresh(&self) {
        let _commit = self.commit.lock();
        self.refresh_enabled.store(true, Ordering::Release);
        self.damage.mark_full();
        drop(_commit);
        self.schedule_refresh();
    }

    fn refresh_wait_queue(&self) -> alloc::sync::Arc<WaitQueue> {
        self.refresh.clone()
    }

    pub(crate) fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> VfsResult<()> {
        self.scanout.read_bytes(offset, dst)
    }

    pub(crate) fn write_bytes(&self, offset: usize, src: &[u8]) -> VfsResult<()> {
        self.scanout.write_bytes(offset, src)
    }

    pub(crate) fn commit_if_needed(&self) {
        let _commit = self.commit.lock();
        if !self.refresh_enabled.load(Ordering::Acquire) {
            return;
        }
        let damage = self.damage.take();
        if damage == DAMAGE_CLEAN {
            return;
        }
        if let Err(error) = self.scanout.present() {
            debug!("Failed to commit framebuffer scanout: {error:?}");
            // Retain correctness after a transient device failure.
            self.mark_full();
        }
    }
}

impl Drop for DisplayCore {
    fn drop(&mut self) {
        // The idle worker owns only a Weak. Wake it so it observes teardown
        // instead of depending on a periodic timer.
        self.refresh.notify_all(false);
    }
}

fn refresh_task(
    display: alloc::sync::Weak<DisplayCore>,
    refresh: alloc::sync::Arc<WaitQueue>,
) -> Result<AxResult<()>, axtask::future::BlockOnError> {
    let delay = core::time::Duration::from_secs_f32(1. / 60.);
    loop {
        // `wait_until` checks the predicate before and after it arms the
        // listener, so a write racing the idle transition cannot lose a wake.
        if let Err(error) = refresh.wait_until(|| {
            display.upgrade().is_none()
                || display
                    .upgrade()
                    .is_some_and(|core| core.pending.load(Ordering::Acquire))
        }) {
            return Ok(Err(error.into()));
        }
        let Some(display) = display.upgrade() else {
            return Ok(Ok(()));
        };
        if !display.take_scheduled() {
            continue;
        }
        // Coalesce bursty write_at/fbcon updates, but stay completely idle
        // when no writer has published damage.
        match axtask::future::block_on(axtask::future::sleep(delay)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Ok(Err(AxError::from(error))),
            Err(error) => return Err(error),
        }
        display.commit_if_needed();
    }
}

lazy_static! {
    /// The one in-kernel console. Installed once at devfs setup and never
    /// removed: the console is the kernel's last-resort output, so it outlives
    /// every `/dev/fb0` node, each of which only borrows it.
    static ref CONSOLE: SpinNoIrq<Option<Arc<DisplayCore>>> = SpinNoIrq::new(None);
}

/// Install the console on `scanout` and start its refresh worker.
///
/// Called once by devfs with the surface `crate::drm::screen` selected. Nothing
/// here consults `/dev/fb0`, so the console works whether that node is the
/// upstream fbdev or the legacy device.
pub(crate) fn install(scanout: Arc<dyn ScanoutSurface>) -> AxResult<Arc<DisplayCore>> {
    let core = Arc::try_new(DisplayCore::new(scanout)).map_err(|_| AxError::NoMemory)?;
    let refresh_core = Arc::downgrade(&core);
    let refresh = core.refresh_wait_queue();
    axtask::spawn_with_name(
        move || match refresh_task(refresh_core, refresh) {
            // Normal owner teardown releases the final strong reference.
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!("Framebuffer refresh timer stopped: {error}"),
            Err(error) => error!("Framebuffer refresh worker stopped: {error}"),
        },
        "fb-refresh".into(),
    )?;
    *CONSOLE.lock() = Some(core.clone());
    super::tty::fbcon::install();
    Ok(core)
}

pub(crate) fn fbcon_dimensions() -> Option<(usize, usize)> {
    let display = CONSOLE.lock().clone()?;
    Some((
        display.scanout.width() as usize,
        display.scanout.height() as usize,
    ))
}

/// Runs a bounded synchronous fbcon repaint and publishes only changed frames.
/// No fbcon state lock is held by this module, so this cannot invert TTY/VT
/// locking with refresh or user-memory paths.
///
/// Nothing is drawn while the console is suspended for a graphics VT. The check
/// is made under the commit lock, which `suspend_refresh` also takes, so a
/// repaint cannot begin after the suspension has been published.
pub(crate) fn fbcon_draw(draw: impl FnOnce(&FbconFrame) -> bool) {
    let Some(display) = CONSOLE.lock().clone() else {
        return;
    };
    // A software scanout copies this RAM while presenting. Keep a repaint
    // whole with respect to that copy instead of publishing half-written
    // glyphs. The VT presentation gate is acquired before this mutex on both
    // drawing and seat-transition paths.
    let commit = display.commit.lock();
    if !display.refresh_enabled.load(Ordering::Acquire) {
        return;
    }
    let scanout = display.scanout.clone();
    let changed = draw(&FbconFrame {
        width: scanout.width() as usize,
        height: scanout.height() as usize,
        pitch: scanout.pitch() as usize,
        bytes_per_pixel: scanout.bytes_per_pixel() as usize,
        scanout,
        size: display.size,
    });
    drop(commit);
    if changed {
        display.mark_full();
    }
}

/// A primary client can take master without ever changing KD_TEXT (for
/// example a bare KMS demo). Its final close must restore the real console
/// mode and framebuffer, serialized with VT mode changes.
pub(crate) fn restore_console_after_master_close() {
    if CONSOLE.lock().is_none() {
        return;
    }
    let manager = &super::tty::VT_MANAGER;
    manager.with_text_active(manager.active(), resume_text);
}

/// Stop the console drawing because a graphics VT is taking the screen.
///
/// Called from the VT seat's `prepare_release` when text is the owner being
/// released. Suspends the fbdev client that owns the surface, so its pending
/// damage flush is cancelled and no console pixels reach the display afterwards.
pub(crate) fn suspend_text() {
    let Some(display) = CONSOLE.lock().clone() else {
        return;
    };
    display.suspend_refresh();
    if let Err(error) = display.scanout.set_master(false) {
        warn!("failed to yield scanout ownership to the graphics VT: {error}");
    }
}

/// Give the screen back to the console after a graphics VT let go of it.
///
/// Resumes the fbdev client, restores its mode and repaints the whole text
/// screen. Idempotent: a repeat call with the console already running only
/// repeats the restore.
pub(crate) fn resume_text() {
    let Some(display) = CONSOLE.lock().clone() else {
        return;
    };
    if let Err(error) = display
        .scanout
        .set_master(true)
        .and_then(|_| display.scanout.restore_text(true))
    {
        warn!("failed to restore scanout ownership for KD_TEXT: {error}");
    }
    // Coalesce all writes made while the graphics VT owned the seat into
    // exactly one full text repaint after the surface is reacquired.
    super::tty::fbcon::invalidate_rendered();
    display.resume_refresh();
}


#[cfg(test)]
pub(crate) mod tests {
    use alloc::{sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use axerrno::{AxError, AxResult};
    use axsync::Mutex;

    use super::{DAMAGE_CLEAN, DAMAGE_FULL, DamageTracker, DisplayCore, FbconFrame};
    use crate::{
        pseudofs::{
            DeviceMmap,
            dev::{
                fb::current_var_screen_info,
                scanout::{ColorChannel, PixelLayout, ScanoutSurface},
            },
        },
    };
    #[test]
    fn damage_tracker_coalesces_and_clears() {
        let damage = DamageTracker::new();
        damage.mark(32, 8, 128);
        damage.mark(8, 16, 128);
        assert_eq!(damage.take(), ((8u64) << 32) | 40);
        assert_eq!(damage.take(), DAMAGE_CLEAN);
    }

    #[test]
    fn damage_tracker_clamps_and_marks_full() {
        let damage = DamageTracker::new();
        damage.mark(120, 32, 128);
        assert_eq!(damage.take(), ((120u64) << 32) | 128);
        damage.mark_full();
        assert_eq!(damage.take(), DAMAGE_FULL);
    }


    /// A scanout surface with no DRM device and no display hardware behind it.
    ///
    /// It stands in for the linear aperture a firmware programs, which is the
    /// only display a machine without a virtio-gpu device can have.
    pub(crate) struct MemorySurface {
        bytes: Mutex<Vec<u8>>,
        width: u32,
        height: u32,
        pitch: u32,
        layout: PixelLayout,
        presents: AtomicU64,
        write_batches: AtomicU64,
        master: AtomicBool,
        restore_text_calls: AtomicU64,
    }

    impl MemorySurface {
        /// A surface in the virtio-gpu layout, which is what the reference
        /// display device produces.
        pub(crate) fn new(width: u32, height: u32) -> Arc<Self> {
            Self::with_layout(width, height, PixelLayout::B8G8R8A8)
        }

        /// A surface in an arbitrary layout, standing in for a firmware
        /// framebuffer whose GOP mode is not 32 bits deep.
        fn with_layout(width: u32, height: u32, layout: PixelLayout) -> Arc<Self> {
            let pitch = width * layout.bytes_per_pixel();
            let mut bytes = Vec::new();
            bytes.resize((pitch * height) as usize, 0);
            Arc::new(Self {
                bytes: Mutex::new(bytes),
                width,
                height,
                pitch,
                layout,
                presents: AtomicU64::new(0),
                write_batches: AtomicU64::new(0),
                master: AtomicBool::new(true),
                restore_text_calls: AtomicU64::new(0),
            })
        }

        pub(crate) fn is_master(&self) -> bool {
            self.master.load(Ordering::Acquire)
        }

        pub(crate) fn restore_text_calls(&self) -> u64 {
            self.restore_text_calls.load(Ordering::Acquire)
        }
    }

    impl ScanoutSurface for MemorySurface {
        fn width(&self) -> u32 {
            self.width
        }

        fn height(&self) -> u32 {
            self.height
        }

        fn pitch(&self) -> u32 {
            self.pitch
        }

        fn pixel_layout(&self) -> PixelLayout {
            self.layout
        }

        fn write_pixel(&self, offset: usize, color: u32) {
            let mut pixel = [0u8; size_of::<u32>()];
            let width = self.layout.encode_into(color, &mut pixel);
            let _ = self.write_bytes(offset, &pixel[..width]);
        }

        fn virtual_height(&self) -> u32 {
            self.height
        }

        fn yoffset(&self) -> u32 {
            0
        }

        fn len(&self) -> usize {
            self.bytes.lock().len()
        }

        fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> AxResult<()> {
            let bytes = self.bytes.lock();
            let end = offset.checked_add(dst.len()).ok_or(AxError::InvalidInput)?;
            dst.copy_from_slice(bytes.get(offset..end).ok_or(AxError::InvalidInput)?);
            Ok(())
        }

        fn write_bytes(&self, offset: usize, src: &[u8]) -> AxResult<()> {
            let mut bytes = self.bytes.lock();
            let end = offset.checked_add(src.len()).ok_or(AxError::InvalidInput)?;
            bytes
                .get_mut(offset..end)
                .ok_or(AxError::InvalidInput)?
                .copy_from_slice(src);
            self.write_batches.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn mmap(&self) -> DeviceMmap {
            DeviceMmap::None
        }

        fn present(&self) -> AxResult<()> {
            self.presents.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn pan(&self, _yoffset: u32) -> AxResult<()> {
            Err(AxError::Unsupported)
        }

        fn set_blank(&self, _blank: bool) -> AxResult<()> {
            Ok(())
        }

        fn restore_text(&self, _nonblocking: bool) -> AxResult<()> {
            self.restore_text_calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn set_master(&self, master: bool) -> AxResult<()> {
            self.master.store(master, Ordering::Release);
            Ok(())
        }
    }

    #[test]
    fn console_repaint_excludes_scanout_copy_until_pixels_are_complete() {
        let _context = crate::test_support::scheduler_test_context();
        let surface = MemorySurface::new(64, 48);
        let core = Arc::new(DisplayCore::new(surface.clone()));
        let previous = super::CONSOLE.lock().replace(core.clone());
        super::fbcon_draw(|frame| {
            assert!(core.commit.try_lock().is_none());
            frame.glyph(0, 0, b'A', 0xffffff, 0, false)
        });
        *super::CONSOLE.lock() = previous;
        assert!(core.commit.try_lock().is_some());
        assert_eq!(surface.presents.load(Ordering::Acquire), 0);
        core.commit_if_needed();
        assert_eq!(surface.presents.load(Ordering::Acquire), 1);
    }

    #[test]
    fn fbdev_console_drives_a_surface_with_no_drm_device() {
        // `/dev/fb0` has to exist on a machine whose only display is a
        // firmware-programmed aperture, so the fbdev path is exercised here
        // over a surface with no DRM device anywhere behind it.  If this ever
        // needs a `DrmDevice` to pass, the seam has been closed again.
        //
        // `mark_full` wakes the refresh worker and waking a wait queue needs a
        // scheduler context.  Installing it here rather than inheriting
        // whatever a previously run test happened to leave behind is what
        // makes this test pass on its own as well as inside the full suite.
        let _context = crate::test_support::scheduler_test_context();
        let surface = MemorySurface::new(64, 48);
        let core = DisplayCore::new(surface.clone());

        // Geometry comes from the surface, not from a DRM mode: a firmware
        // framebuffer has no refresh rate to report and must not be given one.
        let var = current_var_screen_info(&core);
        assert_eq!((var.xres, var.yres), (64, 48));
        assert_eq!(var.xres_virtual, 64);
        assert_eq!(var.xres_virtual * 4, core.scanout.pitch());
        assert_eq!(core.size, 64 * 4 * 48);

        // A write through the surface contract lands in the surface.
        core.write_bytes(0, &[0x11, 0x22, 0x33, 0x44]).unwrap();
        assert_eq!(&surface.bytes.lock()[..4], &[0x11, 0x22, 0x33, 0x44]);
        let mut readback = [0u8; 4];
        core.read_bytes(0, &mut readback).unwrap();
        assert_eq!(readback, [0x11, 0x22, 0x33, 0x44]);

        // Publication keeps one meaning across backends: damage accumulates and
        // the commit point is what asks the surface to present.
        assert_eq!(surface.presents.load(Ordering::Acquire), 0);
        core.mark_full();
        core.commit_if_needed();
        assert_eq!(surface.presents.load(Ordering::Acquire), 1);

        // The console's ownership of the seat follows the same contract.
        assert!(surface.master.load(Ordering::Acquire));
        core.scanout.set_master(false).unwrap();
        assert!(!surface.master.load(Ordering::Acquire));
    }

    /// The 16-bit RGB565 layout a firmware may report for a 16-bit GOP mode.
    const RGB565: PixelLayout = PixelLayout {
        bits: 16,
        red: ColorChannel {
            position: 11,
            size: 5,
        },
        green: ColorChannel {
            position: 5,
            size: 6,
        },
        blue: ColorChannel {
            position: 0,
            size: 5,
        },
    };

    #[test]
    fn console_writes_pixels_at_the_surfaces_own_depth() {
        // The console used to write four bytes per pixel unconditionally.  On
        // a 16-bit framebuffer that walks the surface at twice the correct
        // stride and paints a garbled screen -- and on a machine whose only
        // output device is that screen, a garbled console is indistinguishable
        // from a kernel that never booted.
        //
        // The surface's buffer is behind a sleeping lock, and taking one needs
        // a current task.  Installing the context here is what makes this test
        // pass on its own as well as after whichever test ran before it.
        let _context = crate::test_support::scheduler_test_context();
        let surface = MemorySurface::with_layout(4, 2, RGB565);
        let frame = FbconFrame {
            scanout: surface.clone(),
            size: 16,
            width: 4,
            height: 2,
            pitch: 8,
            bytes_per_pixel: 2,
        };

        // White at column 3 of row 1 belongs in the last two bytes, and every
        // byte before it must stay untouched.
        frame.write_pixel(3, 1, 0x00ff_ffff);
        let bytes = surface.bytes.lock();
        assert_eq!(&bytes[14..16], &[0xff, 0xff]);
        assert!(bytes[..14].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn uhd_console_glyph_and_cursor_use_the_same_threefold_cell() {
        let _context = crate::test_support::scheduler_test_context();
        let surface = MemorySurface::new(3840, 2160);
        let frame = FbconFrame {
            scanout: surface.clone(),
            size: 3840 * 2160 * 4,
            width: 3840,
            height: 2160,
            pitch: 3840 * 4,
            bytes_per_pixel: 4,
        };
        assert_eq!(frame.cell_size(), (24, 48));
        assert!(frame.glyph(0, 0, b'A', 0x00ff_ffff, 0x0012_3456, true));
        assert_eq!(surface.write_batches.load(Ordering::Relaxed), 48);
        // No full-screen black clear when text cells cover the UHD surface.
        frame.clear_margins(3840, 2160);
        assert_eq!(surface.write_batches.load(Ordering::Relaxed), 48);
        let glyph = super::super::console_font::glyph(b'A').unwrap();
        let bytes = surface.bytes.lock();
        for y in 0..48 {
            for x in 0..24 {
                let (source_x, source_y) = (x / 3, y / 3);
                let cursor = source_y == 14 && source_x > 0 && source_x < 7;
                let ink = glyph[source_y] & (0x80 >> source_x) != 0 || cursor;
                let mut expected = [0; 4];
                surface.layout.encode_into(
                    if ink { 0x00ff_ffff } else { 0x0012_3456 },
                    &mut expected,
                );
                let offset = y * frame.pitch + x * 4;
                assert_eq!(&bytes[offset..offset + 4], &expected);
            }
            assert_eq!(&bytes[y * frame.pitch + 24 * 4..y * frame.pitch + 25 * 4], &[0; 4]);
        }
        drop(bytes);
        // Malformed coordinates must not wrap into the start of the surface.
        assert!(!frame.glyph(usize::MAX, usize::MAX, b'A', 0, 0, false));
        assert_eq!(surface.write_batches.load(Ordering::Relaxed), 48);
    }

    #[test]
    fn console_pixel_write_stops_at_the_surface_end() {
        // A frame whose geometry claims a column beyond the surface must clip
        // rather than run past the mapping it was given.
        let _context = crate::test_support::scheduler_test_context();
        let surface = MemorySurface::with_layout(2, 1, RGB565);
        let frame = FbconFrame {
            scanout: surface.clone(),
            size: 4,
            width: 2,
            height: 1,
            pitch: 4,
            bytes_per_pixel: 2,
        };
        frame.write_pixel(2, 0, 0x00ff_ffff);
        assert!(surface.bytes.lock().iter().all(|byte| *byte == 0));
    }
}
