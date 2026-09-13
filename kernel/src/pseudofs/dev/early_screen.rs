//! The screen the kernel paints before it has a console.
//!
//! A machine whose only output device is the display its own firmware
//! programmed has nowhere to report a bring-up failure until something draws
//! into the aperture the firmware left behind.  The framebuffer console which
//! eventually owns that aperture is installed from the device filesystem,
//! which is far too late: memory management, platform initialization, the
//! scheduler, driver probing and secondary CPU bring-up have all run by then,
//! and every one of them can stop the machine.  This module is the screen
//! which exists *during* those steps, so a machine with no serial port shows
//! where it got to and what it said last.
//!
//! Three properties are load-bearing, and each follows from where this code
//! runs rather than from what it draws.
//!
//! * **Nothing is allocated and nothing blocks.**  [`report_panic`] runs from
//!   the panic handler, on any CPU, at any point, possibly with a lock held by
//!   the very code that panicked.  Every buffer is on the stack, every loop is
//!   bounded, and the log ring is read through a non-blocking snapshot.
//! * **Every extent is bounded before it is used.**  The surface geometry is
//!   taken from the bootloader's description only after the same validation
//!   the console's own surface applies, the addressable rows and columns have
//!   a fixed ceiling, and a line longer than the screen is wrapped or clipped
//!   rather than written past the end of the last scan line.
//! * **Exactly one mapping is trusted at a time.**  [`claim_boot_linear_map`]
//!   draws through the boot page table's linear map, [`paging_rebind`] draws
//!   through the kernel page table, and [`boot_freeze`] sits between them so
//!   the painter can never write through a mapping the running page table does
//!   not have.  See `docs/design/early-screen.md` for the exact live ranges.
//!
//! The renderer is deliberately a pure function of a geometry description and
//! a byte slice ([`paint`]), so every layout rule it enforces -- stride, pixel
//! depth, wrapping, clipping and the choice of log lines -- is exercised by
//! host tests on a plain buffer as well as on the real aperture.

use core::{
    fmt::{self, Write as _},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use axhal::mem::{PhysAddr, phys_to_virt};

use super::{
    bootfb,
    console_font::{GLYPH_HEIGHT, GLYPH_WIDTH, glyph},
    scanout::PixelLayout,
};

/// Rows of text one frame may address.
///
/// A surface taller than this keeps the rows above the ceiling and leaves the
/// rest as the initial clear left them.  The ceiling exists so that a
/// malformed or hostile geometry cannot make the painter loop for an
/// unbounded time, not because any display the kernel supports reaches it.
const MAX_ROWS: usize = 128;

/// Columns of text one frame may address, for the same reason as [`MAX_ROWS`].
const MAX_COLS: usize = 512;

/// Bytes of the kernel log tail one frame reads.
const TAIL_BYTES: usize = 4096;

/// Bytes of panic message text one frame formats.
const MESSAGE_BYTES: usize = 2048;

/// Bytes of backtrace text one frame formats.
const TRACE_BYTES: usize = 2048;

/// Longest status line one frame composes.
const STATUS_BYTES: usize = 128;

/// Rows of panic message shown before the log tail is protected.
const MESSAGE_ROWS: usize = 8;

/// Rows of backtrace shown before the log tail is protected.
const TRACE_ROWS: usize = 6;

/// Rows of log tail a frame always tries to keep.
///
/// The log is what says *where* the machine stopped, so a panic frame spends
/// its rows on the message first, the backtrace second and the tail third --
/// but never on the message and the backtrace together at the tail's expense.
const TAIL_ROWS_MIN: usize = 4;

/// Highest physical address the boot page table's linear map covers.
///
/// `multiboot.S` maps the low 512 GiB twice: once as an identity map and once
/// at the linear-map offset, both with 1 GiB pages.  A framebuffer above this
/// would be reachable only through the kernel page table, so the boot claim
/// refuses it rather than faulting on the first glyph.
const BOOT_LINEAR_LIMIT: usize = 512 << 30;

/// Bytes one cache line occupies, which is the granularity `clflush` works at.
const CACHE_LINE: usize = 64;

/// Background colour.
const BACKGROUND: u32 = 0x0000_0000;

/// Kernel log text.
const LOG_TEXT: u32 = 0x00d0_d0d0;

/// Text on the status bar.
const STATUS_TEXT: u32 = 0x00ff_ffff;

/// Status bar for a milestone.
const STATUS_BAR: u32 = 0x0020_4060;

/// Status bar for a fault.
const PANIC_BAR: u32 = 0x00a0_0000;

/// Panic message text.
const MESSAGE_TEXT: u32 = 0x00ff_9090;

/// Backtrace text.
const TRACE_TEXT: u32 = 0x00b0_b0b0;

/// A byte range of one text buffer, as one scan line of the screen.
#[derive(Clone, Copy)]
struct RowRef {
    start: u32,
    len: u16,
}

impl RowRef {
    fn range(&self) -> core::ops::Range<usize> {
        let start = self.start as usize;
        start..start + self.len as usize
    }
}

/// The display rows one text buffer occupies.
///
/// A row set either *keeps the newest* rows or *stops* once it is full.  The
/// log tail wants its newest lines -- the last thing the kernel said before it
/// stopped is the thing worth reading -- while a panic message wants its first
/// ones.
#[derive(Clone, Copy)]
struct Rows {
    entries: [RowRef; MAX_ROWS],
    head: usize,
    count: usize,
    keep_newest: bool,
}

impl Rows {
    const fn new(keep_newest: bool) -> Self {
        Self {
            entries: [RowRef { start: 0, len: 0 }; MAX_ROWS],
            head: 0,
            count: 0,
            keep_newest,
        }
    }

    /// Whether a set which stops when full has reached `limit` rows.
    fn full(&self, limit: usize) -> bool {
        !self.keep_newest && self.count >= limit
    }

    fn push(&mut self, start: usize, len: usize) -> bool {
        let entry = RowRef {
            start: start as u32,
            len: len as u16,
        };
        if self.count < MAX_ROWS {
            self.entries[(self.head + self.count) % MAX_ROWS] = entry;
            self.count += 1;
            return true;
        }
        if !self.keep_newest {
            return false;
        }
        self.entries[self.head] = entry;
        self.head = (self.head + 1) % MAX_ROWS;
        true
    }

    /// Every row, oldest first.
    fn iter(&self) -> impl Iterator<Item = RowRef> + '_ {
        (0..self.count).map(move |index| self.entries[(self.head + index) % MAX_ROWS])
    }

    /// The newest `limit` rows, oldest first.
    fn newest(&self, limit: usize) -> impl Iterator<Item = RowRef> + '_ {
        let skip = self.count.saturating_sub(limit);
        (skip..self.count).map(move |index| self.entries[(self.head + index) % MAX_ROWS])
    }
}

/// Split `text` into screen rows of `cols` columns.
///
/// A line longer than the screen wraps rather than being dropped, and a
/// carriage return is a line break unless it is the first half of a CRLF pair.
/// At most `limit` rows are produced; see [`Rows`] for which end of a longer
/// text survives.
fn wrap_rows(text: &[u8], cols: usize, limit: usize, keep_newest: bool) -> Rows {
    let mut rows = Rows::new(keep_newest);
    let limit = limit.min(MAX_ROWS);
    if cols == 0 || limit == 0 {
        return rows;
    }
    let mut start = 0usize;
    let mut len = 0usize;
    for (index, &byte) in text.iter().enumerate() {
        if byte == b'\r' && text.get(index + 1) == Some(&b'\n') {
            continue;
        }
        if byte == b'\n' || byte == b'\r' {
            if !rows.push(start, len) || rows.full(limit) {
                return rows;
            }
            start = index + 1;
            len = 0;
            continue;
        }
        if len == cols {
            if !rows.push(start, len) || rows.full(limit) {
                return rows;
            }
            start = index;
            len = 0;
        }
        len += 1;
    }
    if len > 0 {
        rows.push(start, len);
    }
    rows
}

/// Geometry and pixel encoding of one linear text surface.
///
/// This is the whole of what the renderer knows about a display, which is what
/// lets a host test drive the identical code over a plain `Vec<u8>`.
#[derive(Clone, Copy)]
struct TextView {
    width: u32,
    height: u32,
    /// Bytes between the starts of two consecutive scan lines, as the
    /// bootloader reported it.  Never recomputed from the visible width: a
    /// firmware surface pads its scan lines, and a caller which assumed
    /// otherwise would shear the image.
    pitch: u32,
    /// Bytes addressable from the first pixel.
    len: usize,
    layout: PixelLayout,
}

impl TextView {
    fn cols(&self) -> usize {
        (self.width as usize / GLYPH_WIDTH).min(MAX_COLS)
    }

    fn rows(&self) -> usize {
        (self.height as usize / GLYPH_HEIGHT).min(MAX_ROWS)
    }

    /// The encoded bytes of one pixel, or `None` for a layout this renderer
    /// cannot write at a known depth.
    fn pattern(&self, color: u32) -> Option<([u8; 4], usize)> {
        let bytes = self.layout.bytes_per_pixel() as usize;
        if bytes == 0 || bytes > size_of::<u32>() {
            return None;
        }
        let mut pixel = [0u8; size_of::<u32>()];
        let written = self.layout.encode_into(color, &mut pixel);
        (written == bytes).then_some((pixel, bytes))
    }

    /// Byte offset of the pixel `(x, y)`, if it lies inside the surface.
    fn offset_of(&self, x: usize, y: usize) -> Option<usize> {
        if x >= self.width as usize || y >= self.height as usize {
            return None;
        }
        let bytes = self.layout.bytes_per_pixel() as usize;
        let offset = y
            .checked_mul(self.pitch as usize)?
            .checked_add(x.checked_mul(bytes)?)?;
        (offset + bytes <= self.len).then_some(offset)
    }

    fn put_pattern(&self, dst: &mut [u8], x: usize, y: usize, pattern: &[u8]) {
        let Some(offset) = self.offset_of(x, y) else {
            return;
        };
        let end = offset + pattern.len();
        if end > dst.len() {
            return;
        }
        dst[offset..end].copy_from_slice(pattern);
    }

    /// Fill whole scan lines in `first..end` with `color`.
    ///
    /// The full stride is filled, padding included, so that no byte of the
    /// aperture -- including one the firmware's own splash left outside the
    /// visible width -- survives a repaint.
    fn fill_rows(&self, dst: &mut [u8], first: usize, end: usize, color: u32) {
        let Some((pixel, bytes)) = self.pattern(color) else {
            return;
        };
        let stride = self.pitch as usize;
        for y in first..end.min(self.height as usize) {
            let start = y * stride;
            if start >= dst.len() {
                return;
            }
            let stop = (start + stride).min(dst.len());
            let row = &mut dst[start..stop];
            let whole = row.len() / bytes * bytes;
            for chunk in row[..whole].chunks_exact_mut(bytes) {
                chunk.copy_from_slice(&pixel[..bytes]);
            }
        }
    }

    /// Draw `text` into one scan line of cells, clipping anything that does
    /// not fit.
    fn draw_row(&self, dst: &mut [u8], row: usize, text: &[u8], cols: usize, fg: u32, bg: u32) {
        for col in 0..cols.min(text.len()) {
            self.draw_cell(dst, col, row, text[col], fg, bg);
        }
    }

    /// Draw one character cell.
    ///
    /// A byte with no glyph is drawn blank rather than as a substitute: the
    /// log carries control bytes, UTF-8 continuation bytes and bytes outside
    /// the font's range, and a screen of replacement boxes would bury the text
    /// around them.
    fn draw_cell(&self, dst: &mut [u8], col: usize, row: usize, byte: u8, fg: u32, bg: u32) {
        let Some((foreground, bytes)) = self.pattern(fg) else {
            return;
        };
        let Some((background, _)) = self.pattern(bg) else {
            return;
        };
        let bits = glyph(byte);
        let x0 = col * GLYPH_WIDTH;
        let y0 = row * GLYPH_HEIGHT;
        for dy in 0..GLYPH_HEIGHT {
            let line = bits.map_or(0, |rows| rows[dy]);
            for dx in 0..GLYPH_WIDTH {
                let pattern = if line & (0x80 >> dx) != 0 {
                    &foreground
                } else {
                    &background
                };
                self.put_pattern(dst, x0 + dx, y0 + dy, &pattern[..bytes]);
            }
        }
    }
}

/// The text one frame is composed of.
struct Content<'a> {
    /// The status line, drawn highlighted in the top row.
    status: &'a [u8],
    /// Whether the frame is reporting a fault rather than progress.
    alert: bool,
    /// Panic message, drawn under the status line.
    message: &'a [u8],
    /// Backtrace, drawn after the message.
    trace: &'a [u8],
    /// Kernel log tail, filling whatever rows are left.
    tail: &'a [u8],
}

impl<'a> Content<'a> {
    /// A frame which only reports a milestone.
    fn milestone(status: &'a [u8], tail: &'a [u8]) -> Content<'a> {
        Content {
            status,
            alert: false,
            message: b"",
            trace: b"",
            tail,
        }
    }
}

/// What one call to [`paint`] touched.
struct Painted {
    /// Text rows the frame's content occupies, which is what the next frame
    /// has to treat as its predecessor's extent.
    used: usize,
    /// Scan lines cleared to the background, which is what a cacheable mapping
    /// has to flush and never covers fewer lines than `used` needs.
    dirty: usize,
}

/// Compose and draw one frame into `dst`.
///
/// `previous` is how many rows the last frame used, so rows a shorter frame no
/// longer covers are cleared instead of leaving the older text behind.  It is
/// `None` for the first frame, which clears the whole text area: the aperture
/// arrives holding whatever the firmware's splash left there, and on a machine
/// whose only output is the screen a stale image is indistinguishable from a
/// kernel which never booted.
fn paint(
    view: &TextView,
    dst: &mut [u8],
    content: &Content<'_>,
    previous: Option<usize>,
) -> Painted {
    let cols = view.cols();
    let rows = view.rows();
    if cols == 0 || rows == 0 {
        return Painted { used: 0, dirty: 0 };
    }

    // The status line is never surrendered, and the tail keeps at least
    // `TAIL_ROWS_MIN` rows; the message and the backtrace divide the rest.
    let body = rows - 1;
    let pinned_budget = body - TAIL_ROWS_MIN.min(body);
    let message = wrap_rows(
        content.message,
        cols,
        pinned_budget.min(MESSAGE_ROWS),
        false,
    );
    let trace = wrap_rows(
        content.trace,
        cols,
        pinned_budget.saturating_sub(message.count).min(TRACE_ROWS),
        false,
    );
    let pinned = message.count + trace.count;
    let tail = wrap_rows(content.tail, cols, body - pinned, true);

    let used = 1 + pinned + tail.count;
    // The first frame clears the whole surface, not just the lines its own
    // cells occupy: whatever the firmware drew also covers the strips of a
    // scan line and a screen edge that no glyph cell lands on.
    let dirty = previous.map_or(view.height as usize, |previous| {
        (previous * GLYPH_HEIGHT).max(used * GLYPH_HEIGHT)
    });
    view.fill_rows(dst, 0, dirty, BACKGROUND);

    let bar = if content.alert { PANIC_BAR } else { STATUS_BAR };
    view.fill_rows(dst, 0, GLYPH_HEIGHT, bar);
    view.draw_row(dst, 0, content.status, cols, STATUS_TEXT, bar);

    let mut row = 1;
    for entry in message.iter() {
        view.draw_row(
            dst,
            row,
            &content.message[entry.range()],
            cols,
            MESSAGE_TEXT,
            BACKGROUND,
        );
        row += 1;
    }
    for entry in trace.iter() {
        view.draw_row(
            dst,
            row,
            &content.trace[entry.range()],
            cols,
            TRACE_TEXT,
            BACKGROUND,
        );
        row += 1;
    }
    for entry in tail.newest(body - pinned) {
        view.draw_row(
            dst,
            row,
            &content.tail[entry.range()],
            cols,
            LOG_TEXT,
            BACKGROUND,
        );
        row += 1;
    }
    Painted { used, dirty }
}

/// Make writes to a cacheable mapping visible to the display controller.
///
/// The boot page table maps the aperture with a huge page that names no cache
/// attribute, so before the kernel page table exists those writes are
/// write-back and the display controller -- which reads memory directly -- may
/// not see them.  Flushing the frame is cheaper than depending on firmware
/// MTRRs this kernel did not program.
fn flush_range(base: *const u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let mut offset = 0;
        while offset < len {
            // SAFETY: `base` is the first byte of a mapped surface of at least
            // `len` bytes, and `clflush` accepts any address in a mapped page.
            unsafe { core::arch::x86_64::_mm_clflush(base.add(offset)) };
            offset += CACHE_LINE;
        }
        // SAFETY: `sfence` has no operands and no memory effects of its own.
        unsafe { core::arch::x86_64::_mm_sfence() };
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (base, len);
}

/// A fixed-capacity text buffer which never allocates.
///
/// The panic handler has to render a `PanicInfo` and a backtrace somewhere,
/// and the heap may be exactly what failed.  Truncation is reported rather
/// than hidden, and it stops on a UTF-8 boundary so the result is always
/// printable.
struct BoundedText<const N: usize> {
    bytes: [u8; N],
    len: usize,
    truncated: bool,
}

impl<const N: usize> BoundedText<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
            truncated: false,
        }
    }

    fn as_bytes(&mut self) -> &[u8] {
        if self.truncated {
            const MARK: &[u8] = b" [truncated]";
            self.truncated = false;
            if N < MARK.len() {
                self.len = 0;
            } else {
                self.len = self.len.min(N - MARK.len());
                while self.len > 0 && self.bytes[self.len] & 0xc0 == 0x80 {
                    self.len -= 1;
                }
                self.bytes[self.len..self.len + MARK.len()].copy_from_slice(MARK);
                self.len += MARK.len();
            }
        }
        &self.bytes[..self.len]
    }
}

impl<const N: usize> fmt::Write for BoundedText<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut take = text.len().min(N.saturating_sub(1 + self.len));
        while take > 0 && !text.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        self.truncated |= take != text.len();
        Ok(())
    }
}

/// The aperture's physical address, kept so the kernel page table can map the
/// same surface after the boot page table is gone.
static PHYSICAL: AtomicUsize = AtomicUsize::new(0);

/// Virtual address of the aperture's first byte.
static BASE: AtomicUsize = AtomicUsize::new(0);

static WIDTH: AtomicUsize = AtomicUsize::new(0);
static HEIGHT: AtomicUsize = AtomicUsize::new(0);
static PITCH: AtomicUsize = AtomicUsize::new(0);
static LEN: AtomicUsize = AtomicUsize::new(0);

/// `PixelLayout` packed into one word; see [`pack_layout`].
static LAYOUT: AtomicU64 = AtomicU64::new(0);

/// Whether the aperture has been claimed and described.
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// Whether the mapping the painter writes through is live right now.
static MAPPED: AtomicBool = AtomicBool::new(false);

/// Whether that mapping is cacheable, so a frame must be flushed after it.
static CACHEABLE: AtomicBool = AtomicBool::new(false);

/// Whether a paint is in progress, which is also the re-entrancy guard.
static PAINTING: AtomicBool = AtomicBool::new(false);

/// Rows the last frame used.
static PAINTED_ROWS: AtomicUsize = AtomicUsize::new(0);

/// Whether any frame has been drawn yet.
static PAINTED_ONCE: AtomicBool = AtomicBool::new(false);

fn pack_layout(layout: PixelLayout) -> u64 {
    let mut packed = u64::from(layout.bits);
    let channels = [layout.red, layout.green, layout.blue];
    for (index, channel) in channels.into_iter().enumerate() {
        let shift = 8 + index * 16;
        packed |= u64::from(channel.position) << shift;
        packed |= u64::from(channel.size) << (shift + 8);
    }
    packed
}

fn unpack_layout(packed: u64) -> PixelLayout {
    let channel = |index: u64| {
        let shift = 8 + index * 16;
        super::scanout::ColorChannel {
            position: (packed >> shift) as u8,
            size: (packed >> (shift + 8)) as u8,
        }
    };
    PixelLayout {
        bits: packed as u8,
        red: channel(0),
        green: channel(1),
        blue: channel(2),
    }
}

/// The surface the painter would write into, and where it starts.
///
/// `None` means the frame must not be drawn at all: either nothing has been
/// claimed, or the page table the painter would write through is not the one
/// the CPU is running on.
fn live() -> Option<(TextView, usize)> {
    if !CLAIMED.load(Ordering::Acquire) || !MAPPED.load(Ordering::Acquire) {
        return None;
    }
    let view = TextView {
        width: WIDTH.load(Ordering::Relaxed) as u32,
        height: HEIGHT.load(Ordering::Relaxed) as u32,
        pitch: PITCH.load(Ordering::Relaxed) as u32,
        len: LEN.load(Ordering::Relaxed),
        layout: unpack_layout(LAYOUT.load(Ordering::Relaxed)),
    };
    Some((view, BASE.load(Ordering::Acquire)))
}

/// Draw one composed frame onto the live surface.
///
/// Repainting while another CPU is already painting is dropped rather than
/// waited for: the frame in progress is at least as new as this one's inputs,
/// and the panic path must never spin on a peer which may itself be stopped.
fn present(content: &Content<'_>) {
    let Some((view, base)) = live() else {
        return;
    };
    if base == 0 || PAINTING.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: `base` and `view.len` describe the mapping established by the
    // claim or the rebind, both of which validate the extent against the
    // bootloader's description before publishing it, and the mapping stays
    // live for as long as `MAPPED` says so.  The guard above makes this the
    // only writer.
    let dst = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, view.len) };
    let first = !PAINTED_ONCE.swap(true, Ordering::AcqRel);
    let previous = if first {
        None
    } else {
        Some(PAINTED_ROWS.load(Ordering::Relaxed))
    };
    let painted = paint(&view, dst, content, previous);
    PAINTED_ROWS.store(painted.used, Ordering::Relaxed);
    if CACHEABLE.load(Ordering::Relaxed) {
        flush_range(dst.as_ptr(), painted.dirty * view.pitch as usize);
    }
    PAINTING.store(false, Ordering::Release);
}

/// Read the newest `dst.len()` bytes of the kernel log ring.
///
/// `blocking` selects the ring's ordinary snapshot, which is correct at a
/// milestone but not from the panic handler: a panic may have interrupted the
/// holder of the ring's lock, and waiting for a lock a stopped CPU owns would
/// turn a legible panic into a silent hang.
fn read_tail(dst: &mut [u8], blocking: bool) -> usize {
    if blocking {
        return axruntime::klog::snapshot_into(0, dst, true).0;
    }
    axruntime::klog::try_snapshot_into(0, dst, true).map_or(0, |(count, _)| count)
}

/// Claim the firmware aperture through the boot page table's linear map.
///
/// This runs before the kernel page table exists, so the only mapping it can
/// use is the one `multiboot.S` installed: 1 GiB pages covering the low
/// 512 GiB both identity-mapped and at the linear-map offset.  The claim
/// therefore checks the whole surface against that window and refuses the
/// screen entirely if it does not fit, rather than faulting on the first
/// frame.
pub(crate) fn claim_boot_linear_map() {
    if CLAIMED.load(Ordering::Acquire) {
        return;
    }
    let Some(framebuffer) = axhal::boot::framebuffer() else {
        return;
    };
    // The console's own surface applies the identical validation; sharing it
    // is what keeps the two descriptions of one aperture from diverging.
    let Ok(layout) = bootfb::pixel_layout(&framebuffer) else {
        return;
    };
    let Some(len) = framebuffer.byte_len() else {
        return;
    };
    let Ok(address) = usize::try_from(framebuffer.address) else {
        return;
    };
    if address == 0 || len == 0 || framebuffer.width == 0 || framebuffer.height == 0 {
        return;
    }
    let Some(end) = address.checked_add(len) else {
        return;
    };
    if end > BOOT_LINEAR_LIMIT {
        return;
    }

    PHYSICAL.store(address, Ordering::Relaxed);
    WIDTH.store(framebuffer.width as usize, Ordering::Relaxed);
    HEIGHT.store(framebuffer.height as usize, Ordering::Relaxed);
    PITCH.store(framebuffer.pitch as usize, Ordering::Relaxed);
    LEN.store(len, Ordering::Relaxed);
    LAYOUT.store(pack_layout(layout), Ordering::Relaxed);
    BASE.store(
        phys_to_virt(PhysAddr::from_usize(address)).as_usize(),
        Ordering::Relaxed,
    );
    // The claim and the mapping it makes usable are published together, after
    // every field the painter reads.
    MAPPED.store(true, Ordering::Relaxed);
    CACHEABLE.store(true, Ordering::Relaxed);
    CLAIMED.store(true, Ordering::Release);
}

/// Stop painting because the mapping the painter uses is about to go away.
pub(crate) fn boot_freeze() {
    MAPPED.store(false, Ordering::Release);
}

/// Map the aperture in the kernel page table and start painting again.
///
/// `axmm::iomap` returns the same linear address the boot page table already
/// gave the aperture, so only its validity changes.  A failure leaves the
/// screen frozen: the frame painted before the switch stays on the display,
/// which is more use than a fault inside the memory-management path.
pub(crate) fn paging_rebind() {
    if !CLAIMED.load(Ordering::Acquire) {
        return;
    }
    let address = PHYSICAL.load(Ordering::Relaxed);
    let len = LEN.load(Ordering::Relaxed);
    if axmm::iomap(PhysAddr::from_usize(address), len).is_err() {
        return;
    }
    // `iomap` maps the aperture device-uncached, so frames need no flush.
    CACHEABLE.store(false, Ordering::Relaxed);
    MAPPED.store(true, Ordering::Release);
}

/// Repaint the screen, naming the step the runtime is about to run.
pub(crate) fn report_milestone(name: &str) {
    let mut status = BoundedText::<STATUS_BYTES>::new();
    let _ = write!(status, "THEKERNEL  {name}");
    let mut tail = [0u8; TAIL_BYTES];
    let count = read_tail(&mut tail, true);
    let content = Content::milestone(status.as_bytes(), &tail[..count]);
    present(&content);
}

/// Repaint the screen from the panic handler.
pub(crate) fn report_panic(
    info: &core::panic::PanicInfo<'_>,
    backtrace: &fmt::Arguments<'_>,
) {
    let mut message = BoundedText::<MESSAGE_BYTES>::new();
    let _ = write!(message, "{info}");
    let mut trace = BoundedText::<TRACE_BYTES>::new();
    let _ = write!(trace, "{backtrace}");
    let mut tail = [0u8; TAIL_BYTES];
    let count = read_tail(&mut tail, false);
    let content = Content {
        status: b"*** PANIC ***",
        alert: true,
        message: message.as_bytes(),
        trace: trace.as_bytes(),
        tail: &tail[..count],
    };
    present(&content);
}

/// The kernel's early screen, as the runtime sees it.
///
/// The runtime declares the interface and the kernel supplies the painter; see
/// `axruntime::EarlyScreen` for why the binding is made at link time rather
/// than through a registration slot.
struct EarlyScreenIf;

#[crate_interface::impl_interface]
impl axruntime::EarlyScreen for EarlyScreenIf {
    fn claim_boot_linear_map() {
        claim_boot_linear_map();
    }

    fn boot_freeze() {
        boot_freeze();
    }

    fn rebind_after_paging() {
        paging_rebind();
    }

    fn milestone(name: &str) {
        report_milestone(name);
    }

    fn panic(info: &core::panic::PanicInfo<'_>, backtrace: &fmt::Arguments<'_>) {
        report_panic(info, backtrace);
    }
}

#[cfg(test)]
mod tests;
