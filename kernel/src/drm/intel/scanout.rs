//! The console's Intel scanout surface: [`super::fb::Surface`] offered to
//! [`crate::drm::screen`].
//!
//! This module is the seam between the memory half of the modeset (this
//! workstream: a framebuffer in memory the display engine can read) and the
//! console, which needs only a surface it can draw into.  It implements
//! [`ScanoutSurface`] over [`super::fb::Surface`] and registers a
//! [`screen::Candidate`] for it at [`screen::rank::DRIVER`].
//!
//! ## When it is safe to register
//!
//! **Registering does not program anything, and it does not prove anything.**
//! The console starts drawing into the surface the moment this candidate wins,
//! and the pixels reach the screen only if a pipe and a plane are *already*
//! scanning that graphics address out.
//!
//! That matters more on this machine than the usual "the screen would be
//! wrong".  The target has no serial port: the screen is the only output the
//! kernel has, and the log that would explain a bad modeset is printed on it.
//! A bring-up that programmed registers but did not get the display engine
//! scanning would, if the console had already moved onto this surface, take
//! that log away along with the picture -- strictly worse than never setting a
//! mode at all.
//!
//! So the console may only move onto this surface on evidence, and reference
//! §11 phase 6 is what the evidence is:
//!
//! * `PIPEDSL` changes between two reads (§11 phase 6.1) -- the pipe is
//!   scanning;
//! * `PLANE_SURFLIVE` reads back the address that was written to `PLANE_SURF`
//!   (§11 phase 6.2) -- the plane armed on *this* surface;
//! * `DDI_BUF_CTL.IS_IDLE` has cleared (§11 phase 6.3) -- the output is not
//!   idle.
//!
//! [`register`] takes that evidence as a [`Verdict`] rather than assuming it,
//! because a candidate that assumed it would report success to
//! [`crate::drm::screen`] for a display that is dark.  When the verdict is
//! [`Verdict::NotScanning`] -- or when the `PLANE_SURFLIVE` address in a
//! [`Verdict::Scanning`] is not this surface's -- the candidate is still
//! registered, and it answers [`Unavailable::Failed`] with the reason.  The
//! console then stays on the firmware's aperture, which is what
//! [`crate::drm::screen`] already does with a failing candidate at a better
//! rank, and the reason reaches the boot log on the screen the firmware is
//! still driving.  The three bad cases and what each one looks like are in
//! `docs/design/intel-scanout.md`.
//!
//! ## What each surface method does for a linear aperture
//!
//! A linear framebuffer the display engine already reads has no publication
//! step, no second page to pan to, and no power control in reach.  Each method
//! says which of those it is, rather than returning a bare `Ok(())` a reader
//! has to interpret:
//!
//! | Method | What it does here |
//! |---|---|
//! | [`present`](ScanoutSurface::present) | Nothing, and accepts: a write is visible as soon as it lands, so there is nothing to submit.  The damage tracker and the fbdev ABI's publication points keep one meaning across backends. |
//! | [`pan`](ScanoutSurface::pan) | Refuses: the visible window is the plane's, not this surface's, and the surface is one screen tall.  A caller that believed a pan happened would display the wrong page. |
//! | [`set_blank`](ScanoutSurface::set_blank) | Accepts and does nothing: blanking is a pipe/plane write this surface does not own.  Same choice, and the same reason, as [`crate::pseudofs::dev::bootfb`]. |
//! | [`restore_text`](ScanoutSurface::restore_text) | Nothing to do: the console's pixels *are* the surface's contents. |
//! | [`set_master`](ScanoutSurface::set_master) | Accepts: there is no exclusive ownership to yield, and whoever writes last is what the display shows. |

use alloc::{format, string::String, sync::Arc};

use axerrno::{AxError, AxResult};
use axgpu::PixelLayout;
use axhal::mem::{PhysAddr, PhysAddrRange};
use spin::Mutex;

use super::fb::Surface;
use crate::{
    drm::screen::{self, Candidate, Unavailable, rank},
    pseudofs::{DeviceMmap, dev::scanout::ScanoutSurface},
};

/// The name this candidate is registered under.
///
/// It is the name the boot log prints when the console is allocated, so it says
/// which driver produced the surface rather than which module registered it.
pub(crate) const CANDIDATE_NAME: &str = "intel-display";

/// Why this candidate is entitled to the screen at [`rank::DRIVER`].
pub(crate) const CANDIDATE_REASON: &str = "the display engine reads this framebuffer through the \
                                           GGTT, and a plane this kernel programmed is scanning \
                                           it out";

/// What the modeset observed about whether the display engine is scanning the
/// surface out.
///
/// The verdicts belong to the steps that read the registers (reference §11
/// phase 6); this module needs only the *conclusion*, plus the one reading it
/// can check against the surface it holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    /// The display engine is scanning this surface out.
    ///
    /// `surflive` is what `PLANE_SURFLIVE` read after `PLANE_SURF` was written
    /// (§11 phase 6.2).  It is checked against the surface here rather than
    /// trusted, because a plane that armed on some *other* address is a display
    /// that shows something, just not this framebuffer.
    Scanning { surflive: u64 },
    /// It is not, and here is what was seen.
    ///
    /// The string is the caller's observation -- `PIPEDSL` unchanged,
    /// `SURFLIVE` zero, `IS_IDLE` still set (§11 phase 6.1 to 6.3) -- and it is
    /// what the boot log will carry.
    NotScanning { reason: String },
}

/// One surface offered to the console, and what was decided about it.
struct Offered {
    surface: Arc<Surface>,
    /// `None` when the surface is being scanned out, or the reason the console
    /// must stay where it is.
    refused: Option<String>,
}

/// The surface this driver has offered, if it has one.
///
/// A candidate is a plain function pointer -- [`screen::Candidate`] holds a
/// `fn`, which cannot capture -- so what it hands back has to be reachable from
/// a static.  There is one Intel console surface per boot, which is why a
/// single slot is enough and why a second offer replaces the first rather than
/// accumulating.
static OFFERED: Mutex<Option<Offered>> = Mutex::new(None);

/// Offer an Intel framebuffer to the console, with the evidence for it.
///
/// The call sequence this expects is:
///
/// ```text
/// let surface = Arc::new(fb::Surface::allocate(&gtt, width, height, format)?);
/// // ... the modeset writes PLANE_SURF = surface.ggtt_address() and the rest,
/// //     then reads the phase 6 evidence ...
/// scanout::register(surface, verdict);
/// ```
///
/// Registering twice replaces the offer and warns: the candidate list refuses a
/// second candidate under one name ([`screen::register`]), so a second call
/// changes what the first candidate hands over rather than adding an entry, and
/// a reader of the log needs to know that happened.
pub(crate) fn register(surface: Arc<Surface>, verdict: Verdict) {
    let refused = refusal(&surface, &verdict);
    if let Some(reason) = &refused {
        warn!(
            "scanout: the Intel surface will not take the console: {reason}.  The firmware \
             framebuffer keeps the screen, and this reason is reported through the candidate list \
             (reference section 11 phase 6)"
        );
    }
    let replaced = OFFERED
        .lock()
        .replace(Offered { surface, refused })
        .is_some();
    if replaced {
        warn!(
            "scanout: a second Intel framebuffer surface replaced the first; the registered \
             candidate will hand over the newer one"
        );
    }
    screen::register(candidate());
}

/// Why the console must not move onto `surface`, or `None` when it may.
///
/// Everything checkable here is checked here, once, at the point the evidence
/// exists -- not on every consultation of the candidate.
fn refusal(surface: &Surface, verdict: &Verdict) -> Option<String> {
    match verdict {
        Verdict::NotScanning { reason } => Some(format!(
            "the modeset did not prove the display engine is scanning this surface out: {reason}"
        )),
        Verdict::Scanning { surflive } => (*surflive != surface.ggtt_address()).then(|| {
            format!(
                "PLANE_SURFLIVE reads {surflive:#x} but this surface is at {:#x}, so the plane is \
                 scanning a different buffer (reference section 11 phase 6.2)",
                surface.ggtt_address()
            )
        }),
    }
}

/// The candidate this driver offers the console, in registration order.
///
/// Kept separate from [`register`] so that a test can drive the decision with
/// this candidate without mutating the process-wide candidate list.
pub(crate) fn candidate() -> Candidate {
    Candidate::new(CANDIDATE_NAME, rank::DRIVER, CANDIDATE_REASON, acquire)
}

/// Hand the console the surface this driver offered, or say why not.
///
/// The only way this can be absent is a caller that registered its own
/// candidate without going through [`register`], which is why the reason is
/// spelled out rather than being an empty string.
fn acquire() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
    let offered = OFFERED.lock();
    let offered = offered.as_ref().ok_or(Unavailable::Absent(
        "the Intel display driver has not produced a framebuffer surface",
    ))?;
    match &offered.refused {
        Some(reason) => Err(Unavailable::Failed(format!(
            "{reason}.  The firmware framebuffer keeps the console, which is what makes the \
             reason readable: reference section 11 phase 6"
        ))),
        None => Ok(offered.surface.clone()),
    }
}

/// Offer a surface that is being scanned out, for tests in this module.
#[cfg(test)]
fn publish(surface: Arc<Surface>) {
    let refused = refusal(
        &surface,
        &Verdict::Scanning {
            surflive: surface.ggtt_address(),
        },
    );
    OFFERED.lock().replace(Offered { surface, refused });
}

impl ScanoutSurface for Surface {
    fn width(&self) -> u32 {
        Surface::width(self)
    }

    fn height(&self) -> u32 {
        Surface::height(self)
    }

    fn pitch(&self) -> u32 {
        Surface::stride(self)
    }

    fn pixel_layout(&self) -> PixelLayout {
        Surface::layout(self)
    }

    fn write_pixel(&self, offset: usize, color: u32) {
        // The surface is written as bytes, so it gets its own depth and nothing
        // more: a 16-bit surface must not receive four bytes, which would walk
        // it at twice its stride.  An offset outside the surface is ignored
        // rather than reported, because the console clips its own output and a
        // console write must not be able to fault the kernel.
        let mut pixel = [0u8; size_of::<u32>()];
        let width = self.layout().encode_into(color, &mut pixel);
        let _ = Surface::write_bytes(self, offset, &pixel[..width]);
    }

    fn virtual_height(&self) -> u32 {
        // One screen tall.  This surface is a single scanout buffer: there is
        // no second page behind it to scroll to, and pan() says so rather than
        // pretending a taller surface exists.
        Surface::height(self)
    }

    fn yoffset(&self) -> u32 {
        0
    }

    fn len(&self) -> usize {
        Surface::len(self)
    }

    fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> AxResult<()> {
        Surface::read_bytes(self, offset, dst).map_err(|_| AxError::InvalidInput)
    }

    fn write_bytes(&self, offset: usize, src: &[u8]) -> AxResult<()> {
        Surface::write_bytes(self, offset, src).map_err(|_| AxError::InvalidInput)
    }

    fn mmap(&self) -> DeviceMmap {
        // The surface is one contiguous physical range, so userspace maps it
        // directly rather than through page objects, and the framebuffer's
        // address can be reported truthfully instead of as a fictitious zero.
        // The mapping a user gets is cacheable while the kernel's view of the
        // same pages is device-uncached; that is the same arrangement the
        // firmware aperture already has, and it is recorded in
        // docs/design/intel-scanout.md.
        match PhysAddrRange::try_from_start_size(
            PhysAddr::from_usize(self.physical_address() as usize),
            self.len(),
        ) {
            Some(range) => DeviceMmap::Physical(range),
            None => DeviceMmap::None,
        }
    }

    fn present(&self) -> AxResult<()> {
        // A linear aperture is the display controller's own memory: a write is
        // visible as soon as it lands and there is nothing to submit.  The call
        // is still accepted so that the damage tracker and the explicit
        // publication points in the fbdev ABI keep one meaning on every
        // backend.
        Ok(())
    }

    fn pan(&self, _yoffset: u32) -> AxResult<()> {
        // Moving the displayed window would mean writing a plane's surface
        // register, which belongs to the modeset that owns the pipe and is not
        // reachable from here.  Refusing is the honest answer: a caller that
        // believed a pan happened would show the wrong page.
        Err(AxError::Unsupported)
    }

    fn set_blank(&self, _blank: bool) -> AxResult<()> {
        // Blanking is a pipe or plane write, and this surface owns neither.
        // Accepting keeps a blanking client working; refusing would abort it
        // over a request whose failure costs nothing.  This is the same choice
        // `pseudofs::dev::bootfb` documents for the firmware aperture.
        Ok(())
    }

    fn restore_text(&self, _nonblocking: bool) -> AxResult<()> {
        // The console's pixels *are* this surface's contents, so there is no
        // saved copy to put back and nothing a graphics client could have
        // displaced.
        Ok(())
    }

    fn set_master(&self, _master: bool) -> AxResult<()> {
        // No exclusive owner exists to yield: this is not a device with a
        // master, and whoever writes last is what the display shows.
        Ok(())
    }
}

/// A one-line description of the surface, for a boot log.
pub(crate) fn describe(surface: &Surface) -> String {
    format!(
        "{}x{} {} pitch {} (PLANE_STRIDE {}), physical {:#x}, ggtt {:#x}, {} bytes",
        surface.width(),
        surface.height(),
        surface.plan().format().name(),
        surface.stride(),
        surface.stride_units_for_log(),
        surface.physical_address(),
        surface.ggtt_address(),
        surface.len()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            fb::Format,
            gtt::{Gtt, mock::MockPageTable},
        },
        *,
    };
    use crate::drm::screen::decide;

    /// A page table with room for the alignment and the padding a run needs.
    ///
    /// A run is placed on a 256 KiB boundary with 64 entries of padding after
    /// it, so a mock table sized to the surface alone would refuse it.
    const TEST_TABLE_ENTRIES: usize = 4096;

    /// A surface over a page table in ordinary memory.
    fn surface(width: u32, height: u32) -> (Arc<Surface>, Gtt) {
        let gtt = Gtt::over(alloc::boxed::Box::new(MockPageTable::new(
            TEST_TABLE_ENTRIES,
        )))
        .unwrap();
        let surface = Arc::new(Surface::allocate(&gtt, width, height, Format::Xrgb8888).unwrap());
        (surface, gtt)
    }

    /// A surface with room in the table for several allocations.
    fn surface_over(gtt: &Gtt, width: u32, height: u32) -> Arc<Surface> {
        Arc::new(Surface::allocate(gtt, width, height, Format::Xrgb8888).unwrap())
    }

    #[test]
    fn the_console_geometry_check_accepts_this_surface_and_selects_it() {
        let _guard = crate::test_support::scheduler_test_context();
        let gtt = Gtt::over(alloc::boxed::Box::new(MockPageTable::new(4096))).unwrap();
        let ours = surface_over(&gtt, 640, 480);
        publish(ours);

        // The same decision the kernel makes, with a provider below ours that
        // must not be consulted: a better candidate that is usable ends the
        // search, and that is the property this test is about.
        let firmware = Candidate::new(
            "firmware-aperture",
            rank::FIRMWARE,
            "a test provider",
            || {
                Err(Unavailable::Absent(
                    "no firmware framebuffer in a host test",
                ))
            },
        );
        let selection = decide(&[candidate(), firmware], &mut |_| {});
        assert_eq!(selection.winner(), Some(CANDIDATE_NAME));
        let surface = selection.surface.as_ref().expect("a surface was selected");
        assert_eq!(
            (surface.width(), surface.height(), surface.pitch()),
            (640, 480, 640 * 4)
        );
        // The console's own criterion, restated here because `screen::usable`
        // is private: the surface must address whole scan lines.
        assert!(surface.len() as u64 >= u64::from(surface.pitch()) * u64::from(surface.height()));
    }

    #[test]
    fn a_write_through_the_console_surface_reaches_the_framebuffer() {
        let _guard = crate::test_support::scheduler_test_context();
        let (surface, _gtt) = surface(64, 64);
        // A canonical 0x00RRGGBB colour is stored the way the display engine
        // reads it: B, G, R, X in memory.
        ScanoutSurface::write_pixel(surface.as_ref(), 0, 0x0012_3456);
        let mut pixel = [0u8; 4];
        ScanoutSurface::read_bytes(surface.as_ref(), 0, &mut pixel).unwrap();
        assert_eq!(pixel, [0x56, 0x34, 0x12, 0x00]);
        // The next pixel along is untouched: the write went where it was asked
        // to, not to a rounded offset.
        let mut next = [0xffu8; 4];
        ScanoutSurface::read_bytes(surface.as_ref(), 4, &mut next).unwrap();
        assert_eq!(next, [0, 0, 0, 0]);
    }

    #[test]
    fn the_stride_the_plane_needs_is_the_pitch_divided_by_sixty_four() {
        let _guard = crate::test_support::scheduler_test_context();
        // PLANE_STRIDE counts a linear surface in 64-byte units, so the number
        // a plane register gets is not the pitch the console is told.  This is
        // the one place the two meet, and it is a test because getting it wrong
        // is a sheared or unstartable image rather than an error.
        let (surface, _gtt) = surface(100, 10);
        let pitch = ScanoutSurface::pitch(surface.as_ref());
        // 100 pixels at 32 bits is 400 bytes, padded up to a whole 256-byte
        // multiple: 512, which is eight 64-byte units.
        assert_eq!(pitch, 512);
        assert_eq!(surface.stride_units_for_log(), pitch / 64);
        assert_eq!(surface.stride_units_for_log() * 64, pitch);
    }

    #[test]
    fn present_is_accepted_and_pan_is_refused() {
        let _guard = crate::test_support::scheduler_test_context();
        let (surface, _gtt) = surface(64, 64);
        // A linear aperture has nothing to publish, but the call is part of the
        // trait's contract on every backend.
        assert!(ScanoutSurface::present(surface.as_ref()).is_ok());
        // There is no second page to pan to and no plane register in reach.
        assert_eq!(
            ScanoutSurface::pan(surface.as_ref(), 1),
            Err(AxError::Unsupported)
        );
        assert_eq!(ScanoutSurface::yoffset(surface.as_ref()), 0);
        assert_eq!(
            ScanoutSurface::virtual_height(surface.as_ref()),
            ScanoutSurface::height(surface.as_ref())
        );
    }

    #[test]
    fn the_methods_with_nothing_to_do_accept_and_say_why() {
        let _guard = crate::test_support::scheduler_test_context();
        let (surface, _gtt) = surface(64, 64);
        assert!(ScanoutSurface::set_blank(surface.as_ref(), true).is_ok());
        assert!(ScanoutSurface::set_blank(surface.as_ref(), false).is_ok());
        assert!(ScanoutSurface::restore_text(surface.as_ref(), true).is_ok());
        assert!(ScanoutSurface::set_master(surface.as_ref(), true).is_ok());
    }

    #[test]
    fn a_console_write_past_the_end_is_refused_and_not_clipped() {
        let _guard = crate::test_support::scheduler_test_context();
        let (surface, _gtt) = surface(64, 64);
        let len = ScanoutSurface::len(surface.as_ref());
        assert_eq!(
            ScanoutSurface::write_bytes(surface.as_ref(), len, &[0]),
            Err(AxError::InvalidInput)
        );
        let mut dst = [0u8; 8];
        assert_eq!(
            ScanoutSurface::read_bytes(surface.as_ref(), len - 4, &mut dst),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn userspace_maps_the_range_the_page_table_entries_name() {
        let _guard = crate::test_support::scheduler_test_context();
        let (surface, _gtt) = surface(64, 64);
        match ScanoutSurface::mmap(surface.as_ref()) {
            DeviceMmap::Physical(range) => {
                assert_eq!(range.start.as_usize() as u64, surface.physical_address());
                assert_eq!(range.size(), surface.len());
            }
            // `DeviceMmap` has no `Debug`, so a failure names the class rather
            // than the value.
            _ => panic!("a contiguous physical surface must map as a physical range"),
        }
    }

    #[test]
    fn register_publishes_the_surface_the_candidate_hands_over() {
        let _guard = crate::test_support::scheduler_test_context();
        // The one test that goes through the real `register`, which is the
        // entry point the coordinator calls.  It mutates the process-wide
        // candidate list, which is why it is the only test that does.
        let (surface, _gtt) = surface(800, 600);
        let surflive = surface.ggtt_address();
        register(surface, Verdict::Scanning { surflive });
        let selection = decide(&[candidate()], &mut |_| {});
        assert_eq!(selection.winner(), Some(CANDIDATE_NAME));
        assert_eq!(
            selection.surface.as_ref().map(|surface| surface.width()),
            Some(800)
        );
    }

    #[test]
    fn a_modeset_that_did_not_prove_it_is_scanning_leaves_the_firmware_console_alone() {
        let _guard = crate::test_support::scheduler_test_context();
        // The failure this pins down is the expensive one on a machine with no
        // serial port: a candidate that took the screen for a display that is
        // dark would take the log with it.
        let (surface, _gtt) = surface(640, 480);
        register(
            surface,
            Verdict::NotScanning {
                reason: String::from(
                    "PIPEDSL did not change between two reads (reference section 11 phase 6.1)",
                ),
            },
        );
        let firmware = Candidate::new(
            "firmware-aperture",
            rank::FIRMWARE,
            "the firmware programmed this display",
            || {
                Err(Unavailable::Absent(
                    "no firmware framebuffer in a host test",
                ))
            },
        );
        let selection = decide(&[candidate(), firmware], &mut |_| {});
        // Ours is refused, and the search moves on rather than ending on a
        // surface nothing scans out.
        assert_eq!(selection.winner(), None);
        match &selection.considered[0].verdict {
            screen::Verdict::Unavailable(Unavailable::Failed(reason)) => {
                assert!(
                    reason.contains("PIPEDSL"),
                    "the log must carry what was observed: {reason}"
                );
                assert!(
                    reason.contains("firmware framebuffer keeps the console"),
                    "the log must say what happens next: {reason}"
                );
            }
            other => panic!("expected the candidate to refuse, got {other:?}"),
        }
        assert!(matches!(
            selection.considered[1].verdict,
            screen::Verdict::Unavailable(Unavailable::Absent(_))
        ));
    }

    #[test]
    fn a_plane_that_armed_on_another_address_is_refused_with_both_addresses() {
        let _guard = crate::test_support::scheduler_test_context();
        let (surface, _gtt) = surface(640, 480);
        let ours = surface.ggtt_address();
        register(
            surface,
            Verdict::Scanning {
                surflive: ours + 0x1000,
            },
        );
        let selection = decide(&[candidate()], &mut |_| {});
        assert!(selection.surface.is_none());
        match &selection.considered[0].verdict {
            screen::Verdict::Unavailable(Unavailable::Failed(reason)) => {
                // Both addresses, so a reader can tell "the plane did not arm"
                // from "the plane armed on a different buffer".
                assert!(
                    reason.contains(&format!("{:#x}", ours + 0x1000)),
                    "{reason}"
                );
                assert!(reason.contains(&format!("{:#x}", ours)), "{reason}");
            }
            other => panic!("expected the candidate to refuse, got {other:?}"),
        }
    }

    #[test]
    fn a_candidate_whose_surface_was_never_published_says_so() {
        let _guard = crate::test_support::scheduler_test_context();
        // Not reachable through `register`, and still worth pinning down: a
        // factory that panicked here would take the boot with it, and the
        // verdict has to say which driver failed to produce anything.
        let verdict = decide(
            &[Candidate::new(
                "intel-display",
                rank::DRIVER,
                "test",
                || Err(Unavailable::Absent("nothing published")),
            )],
            &mut |_| {},
        );
        assert!(matches!(
            verdict.considered[0].verdict,
            screen::Verdict::Unavailable(Unavailable::Absent(_))
        ));
    }
}
