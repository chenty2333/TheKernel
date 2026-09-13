//! Which surface drives the console and `/dev/fb0`.
//!
//! One machine can have several things able to feed the screen. A driver that
//! found its display controller and programmed a mode owns one. A DRM primary
//! device publishes one through an atomic commit. A machine whose firmware
//! already programmed the display has one lying in a linear aperture that
//! nothing in the kernel can replace. Exactly one of them may drive `/dev/fb0`
//! and the in-kernel console, and that choice is made here and nowhere else.
//!
//! # Priority
//!
//! Candidates are consulted in ascending rank and the first that produces a
//! surface the console can draw into takes the screen. Lowering a rank is how a
//! provider states that it is a better answer than the ones below it:
//!
//! | Rank | Tier | Why it sits there |
//! |---:|---|---|
//! | 0 | [`rank::DRIVER`] | It owns the display controller and programmed the mode it reports. It can present at the panel's own mode and it is the surface a graphics client on the same device will present through. |
//! | 100 | [`rank::DRM`] | A DRM primary device's fbdev emulation. A real driver with a real connector, but its console pixels live in a dumb GEM buffer and reach the screen only through a commit, which is strictly more that can fail than a surface the controller scans directly. |
//! | 200 | [`rank::FIRMWARE`] | The aperture the firmware programmed before the kernel started. It is last because it is the only candidate that cannot present at a mode of the kernel's choosing, and it exists only because nothing above it did. |
//!
//! A registered candidate is consulted before the built-in DRM emulation of the
//! same rank, because a driver that claims the screen explicitly outranks the
//! generic path. Registration order breaks any remaining tie, so the order is
//! deterministic rather than incidental.
//!
//! # Failure
//!
//! A candidate that has nothing to offer, or that offers a surface which cannot
//! be drawn into, does not end the search: the next candidate is consulted. On
//! a machine whose screen is its only output channel, losing the display to a
//! provider that failed and never trying the one below it is the difference
//! between a diagnosable boot and a black screen.
//!
//! Every candidate is accounted for at `info!`, including the ones that were
//! never asked because an earlier one had already won, so a boot log read off
//! that screen says both who won and why each of the others did not.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::fmt;

use axerrno::AxError;
use spin::Mutex;

use crate::pseudofs::dev::{bootfb, scanout::ScanoutSurface};

/// The rank tiers a candidate can claim. Lower is consulted earlier.
pub(crate) mod rank {
    /// A driver that owns the display controller and programmed its mode.
    pub(crate) const DRIVER: u32 = 0;
    /// The fbdev emulation of the registered DRM primary device.
    pub(crate) const DRM: u32 = 100;
    /// The linear aperture the firmware programmed before the kernel started.
    pub(crate) const FIRMWARE: u32 = 200;
}

/// Why a candidate could not offer a surface.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// There is no hardware for this candidate to offer.
    ///
    /// Kept distinct from [`Self::Failed`] because the two are fixed by
    /// different people: an absent device is a machine or a configuration
    /// question, a failure is a driver bug.
    Absent(&'static str),
    /// The candidate found its hardware but could not prepare a surface.
    Failed(String),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent(reason) => write!(f, "{reason}"),
            Self::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

/// One source of the console's scanout surface.
///
/// `reason` is the candidate's own statement of why it is entitled to the
/// screen at this rank. It is logged when the candidate is consulted, so the
/// grounds for a decision survive into the boot log instead of living only in
/// the head of whoever set the rank.
#[derive(Clone, Copy)]
pub(crate) struct Candidate {
    name: &'static str,
    rank: u32,
    reason: &'static str,
    acquire: fn() -> Result<Arc<dyn ScanoutSurface>, Unavailable>,
}

impl Candidate {
    pub(crate) const fn new(
        name: &'static str,
        rank: u32,
        reason: &'static str,
        acquire: fn() -> Result<Arc<dyn ScanoutSurface>, Unavailable>,
    ) -> Self {
        Self {
            name,
            rank,
            reason,
            acquire,
        }
    }
}

/// What happened to one candidate.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// This candidate's surface is the one the console and fbdev will use.
    Selected { width: u32, height: u32, pitch: u32 },
    /// The candidate had no surface to offer.
    Unavailable(Unavailable),
    /// The candidate offered a surface that cannot be drawn into.
    Unusable(String),
    /// An earlier candidate had already taken the screen, so this one was never
    /// asked. Recorded rather than omitted: a candidate that was never
    /// consulted and one that failed look identical in a log that only prints
    /// successes, and the two call for different investigations.
    Outranked { winner: &'static str },
}

/// One candidate and what happened to it.
#[derive(Debug)]
pub(crate) struct Considered {
    pub(crate) name: &'static str,
    pub(crate) rank: u32,
    pub(crate) reason: &'static str,
    pub(crate) verdict: Verdict,
}

/// The outcome of one selection.
pub(crate) struct Selection {
    /// The surface the console and `/dev/fb0` must use, if one was found.
    pub(crate) surface: Option<Arc<dyn ScanoutSurface>>,
    /// What happened to every candidate, in the order it was consulted.
    pub(crate) considered: Vec<Considered>,
}

impl Selection {
    /// The name of the candidate that took the screen.
    pub(crate) fn winner(&self) -> Option<&'static str> {
        self.considered
            .iter()
            .find(|candidate| matches!(candidate.verdict, Verdict::Selected { .. }))
            .map(|candidate| candidate.name)
    }
}

/// The candidates registered by display drivers, in registration order.
static REGISTERED: Mutex<Vec<Candidate>> = Mutex::new(Vec::new());

/// Offer the console a scanout surface.
///
/// A driver calls this once, during its own initialisation, before
/// `/dev/fb0` is published. Registering does not by itself take the screen: it
/// enters the driver into the order above, and the surface it returns is used
/// only if it is the first one that can actually be drawn into.
pub(crate) fn register(candidate: Candidate) {
    let mut registered = REGISTERED.lock();
    if registered
        .iter()
        .any(|existing| existing.name == candidate.name)
    {
        // Two candidates under one name would make the log ambiguous about
        // which of them won, and the second registration is always a bug in
        // the driver that made it.
        warn!(
            "scanout: candidate '{}' registered more than once; keeping the first",
            candidate.name
        );
        return;
    }
    registered.push(candidate);
}

/// Choose the surface the console and `/dev/fb0` will use.
///
/// This is the kernel's single selection point. It allocates nothing but the
/// verdict list, touches no hardware of its own (the candidates do), and is
/// pure with respect to the candidate list, so every branch in [`decide`] is
/// reachable from a host test.
pub(crate) fn console_scanout() -> Option<Arc<dyn ScanoutSurface>> {
    let mut candidates = REGISTERED.lock().clone();
    candidates.push(drm_primary());
    candidates.push(firmware_aperture());
    // Stable, so registration order breaks a tie between equal ranks.
    candidates.sort_by_key(|candidate| candidate.rank);

    let mut selection = decide(&candidates, &mut report);
    if selection.surface.is_none() {
        info!("scanout: no candidate produced a usable surface");
    }
    selection.surface.take()
}

/// Consult `candidates` in the order given and take the first usable surface.
///
/// The order is the caller's: [`console_scanout`] sorts by rank before calling
/// this, and a test can hand it any order at all. One surface is taken, so the
/// candidates after the winner are recorded as outranked rather than consulted
/// -- asking a driver to prepare a surface it will not be allowed to use is not
/// a harmless question.
///
/// `note` is called with each candidate's verdict *as it is decided*, before
/// the next candidate is asked. That ordering is the point: on a machine whose
/// only output is the screen, a losing verdict that is only printed once the
/// whole search has finished is a verdict that is lost if the fallback it
/// enabled faults on the way up. The one diagnostic a black screen can still
/// carry is the one that was written before the screen was touched.
pub(crate) fn decide(candidates: &[Candidate], note: &mut dyn FnMut(&Considered)) -> Selection {
    let mut considered = Vec::with_capacity(candidates.len());
    let mut surface: Option<Arc<dyn ScanoutSurface>> = None;
    let mut winner: Option<&'static str> = None;

    for candidate in candidates {
        let verdict = match winner {
            Some(winner) => Verdict::Outranked { winner },
            None => match (candidate.acquire)() {
                Ok(acquired) => match usable(acquired.as_ref()) {
                    Ok(()) => {
                        let verdict = Verdict::Selected {
                            width: acquired.width(),
                            height: acquired.height(),
                            pitch: acquired.pitch(),
                        };
                        winner = Some(candidate.name);
                        surface = Some(acquired);
                        verdict
                    }
                    Err(reason) => Verdict::Unusable(reason),
                },
                Err(reason) => Verdict::Unavailable(reason),
            },
        };
        let considered_candidate = Considered {
            name: candidate.name,
            rank: candidate.rank,
            reason: candidate.reason,
            verdict,
        };
        note(&considered_candidate);
        considered.push(considered_candidate);
    }

    Selection {
        surface,
        considered,
    }
}

/// Whether the console and fbdev can draw into `surface` at all.
///
/// A provider can succeed and still hand back a surface nothing can be written
/// into: zero-sized, at a depth whose pixels have no width in bytes, with scan
/// lines shorter than a row of pixels, or with fewer bytes addressable than
/// the rows it claims. Any of those paints a garbled screen rather than
/// reporting an error, so each is rejected here, while the surface is still
/// only a candidate, and the search moves on to the next provider.
fn usable(surface: &dyn ScanoutSurface) -> Result<(), String> {
    let (width, height) = (surface.width(), surface.height());
    if width == 0 || height == 0 {
        return Err(format!("its extent is {width}x{height}"));
    }
    let layout = surface.pixel_layout();
    if !layout.is_drawable() {
        return Err(format!("its pixel layout {layout:?} is not drawable"));
    }
    let bytes_per_pixel = u64::from(layout.bytes_per_pixel());
    let minimum_pitch = u64::from(width) * bytes_per_pixel;
    let pitch = u64::from(surface.pitch());
    if pitch < minimum_pitch {
        return Err(format!(
            "its pitch {pitch} is shorter than one scan line of {width} pixels at \
             {bytes_per_pixel} bytes"
        ));
    }
    // `virtual_height` rather than `height`: fbdev can pan to the rows below
    // the visible ones, and a surface that claims them has to hold them.
    let addressable = surface.len() as u64;
    let needed = pitch * u64::from(surface.virtual_height());
    if addressable < needed {
        return Err(format!(
            "it addresses {addressable} bytes but claims {} scan lines of {pitch}",
            surface.virtual_height()
        ));
    }
    Ok(())
}

/// Say what happened to one candidate, as soon as it happened.
///
/// Every verdict is `info!`, including the losing ones. A person reading this
/// boot log on a machine with no serial port has only the screen, and the
/// screen only exists once some candidate has won: the reasons the others lost
/// are printed before that point or they are never read at all.
fn report(candidate: &Considered) {
    match &candidate.verdict {
        Verdict::Selected {
            width,
            height,
            pitch,
        } => info!(
            "scanout: candidate '{}' (rank {}) selected: {}x{} pitch {}, because {}",
            candidate.name, candidate.rank, width, height, pitch, candidate.reason
        ),
        Verdict::Unavailable(reason) => info!(
            "scanout: candidate '{}' (rank {}) has nothing to offer: {reason}",
            candidate.name, candidate.rank
        ),
        Verdict::Unusable(reason) => info!(
            "scanout: candidate '{}' (rank {}) offered an unusable surface: {reason}",
            candidate.name, candidate.rank
        ),
        Verdict::Outranked { winner } => info!(
            "scanout: candidate '{}' (rank {}) not consulted: '{winner}' already won",
            candidate.name, candidate.rank
        ),
    }
}

/// The registered DRM primary device, if one exists.
fn drm_primary() -> Candidate {
    Candidate::new(
        "drm-primary",
        rank::DRM,
        "a driver published this device and presents through it",
        || {
            let device = super::primary_device()
                .ok_or(Unavailable::Absent("no DRM primary device is registered"))?;
            super::drm_scanout(device).map_err(|error: AxError| {
                Unavailable::Failed(format!(
                    "the DRM fbdev surface could not be prepared: {error}"
                ))
            })
        },
    )
}

/// The aperture the firmware programmed before the kernel started.
fn firmware_aperture() -> Candidate {
    Candidate::new(
        "firmware-aperture",
        rank::FIRMWARE,
        "the firmware programmed this display and nothing in the kernel did",
        || {
            let framebuffer = axhal::boot::framebuffer().ok_or(Unavailable::Absent(
                // The platform parses seven distinct rejections out of the
                // bootloader's tag and reports which one it was on the
                // diagnostic channel only, which a machine with no serial port
                // cannot read. This is everything the kernel can say until that
                // accessor exists; see docs/design/display-dispatch.md 5.4.
                "the bootloader handed over no framebuffer this kernel can draw into",
            ))?;
            let surface = bootfb::BootFb::new(&framebuffer).map_err(|error| {
                Unavailable::Failed(format!(
                    "the firmware framebuffer could not be mapped: {error}"
                ))
            })?;
            let surface: Arc<dyn ScanoutSurface> = Arc::try_new(surface)
                .map_err(|_| Unavailable::Failed(String::from("out of memory")))?;
            info!(
                "Firmware framebuffer scanout: {}x{} pitch {} at {:#x}",
                framebuffer.width, framebuffer.height, framebuffer.pitch, framebuffer.address
            );
            Ok(surface)
        },
    )
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicBool, Ordering};

    use axerrno::{AxError, AxResult};
    use axgpu::{ColorChannel, PixelLayout};

    use super::{Candidate, Selection, Unavailable, Verdict, decide, rank};
    use crate::pseudofs::{DeviceMmap, dev::scanout::ScanoutSurface};

    /// A surface that reports whatever a test tells it to, and nothing else.
    struct SyntheticSurface {
        width: u32,
        height: u32,
        pitch: u32,
        layout: PixelLayout,
        len: usize,
        virtual_height: u32,
    }

    impl SyntheticSurface {
        /// A surface a console can actually be drawn into.
        fn drawable() -> Self {
            Self {
                width: 640,
                height: 480,
                pitch: 640 * 4,
                layout: PixelLayout::B8G8R8A8,
                len: 640 * 4 * 480,
                virtual_height: 480,
            }
        }
    }

    impl ScanoutSurface for SyntheticSurface {
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
        fn write_pixel(&self, _offset: usize, _color: u32) {}
        fn virtual_height(&self) -> u32 {
            self.virtual_height
        }
        fn yoffset(&self) -> u32 {
            0
        }
        fn len(&self) -> usize {
            self.len
        }
        fn read_bytes(&self, _offset: usize, _dst: &mut [u8]) -> AxResult<()> {
            Err(AxError::Unsupported)
        }
        fn write_bytes(&self, _offset: usize, _src: &[u8]) -> AxResult<()> {
            Err(AxError::Unsupported)
        }
        fn mmap(&self) -> DeviceMmap {
            DeviceMmap::None
        }
        fn present(&self) -> AxResult<()> {
            Ok(())
        }
        fn pan(&self, _yoffset: u32) -> AxResult<()> {
            Err(AxError::Unsupported)
        }
        fn set_blank(&self, _blank: bool) -> AxResult<()> {
            Ok(())
        }
        fn restore_text(&self, _nonblocking: bool) -> AxResult<()> {
            Ok(())
        }
        fn set_master(&self, _master: bool) -> AxResult<()> {
            Ok(())
        }
    }

    type Acquire = fn() -> Result<Arc<dyn ScanoutSurface>, Unavailable>;

    /// A candidate with the given factory. `rank` is supplied by each test.
    fn candidate(name: &'static str, rank: u32, acquire: Acquire) -> Candidate {
        Candidate::new(name, rank, "a test provider", acquire)
    }

    fn good() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        Ok(Arc::new(SyntheticSurface::drawable()))
    }

    fn also_good() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        Ok(Arc::new(SyntheticSurface {
            width: 800,
            height: 600,
            pitch: 800 * 4,
            len: 800 * 4 * 600,
            ..SyntheticSurface::drawable()
        }))
    }

    fn absent() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        Err(Unavailable::Absent("no such device"))
    }

    fn failed() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        Err(Unavailable::Failed(String::from(
            "the device answered wrongly",
        )))
    }

    fn empty_extent() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        Ok(Arc::new(SyntheticSurface {
            width: 0,
            ..SyntheticSurface::drawable()
        }))
    }

    fn undrawable_layout() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        // A 16-bit pixel whose red field reaches past the end of the pixel.
        Ok(Arc::new(SyntheticSurface {
            layout: PixelLayout {
                bits: 16,
                red: ColorChannel {
                    position: 12,
                    size: 8,
                },
                green: ColorChannel {
                    position: 5,
                    size: 6,
                },
                blue: ColorChannel {
                    position: 0,
                    size: 5,
                },
            },
            ..SyntheticSurface::drawable()
        }))
    }

    fn short_pitch() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        Ok(Arc::new(SyntheticSurface {
            pitch: 512,
            ..SyntheticSurface::drawable()
        }))
    }

    fn short_allocation() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        Ok(Arc::new(SyntheticSurface {
            len: 1024,
            ..SyntheticSurface::drawable()
        }))
    }

    fn taller_than_the_allocation() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        // The visible extent fits; the rows fbdev can pan to do not.
        Ok(Arc::new(SyntheticSurface {
            virtual_height: 960,
            ..SyntheticSurface::drawable()
        }))
    }

    /// Set once any verdict has been handed to an observer.
    static SAW_A_VERDICT_FIRST: AtomicBool = AtomicBool::new(false);

    /// A provider that refuses unless the previous candidate's verdict has
    /// already been reported.
    fn good_only_after_a_verdict() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
        if !SAW_A_VERDICT_FIRST.load(Ordering::SeqCst) {
            return Err(Unavailable::Failed(String::from(
                "the previous candidate's verdict had not been reported yet",
            )));
        }
        Ok(Arc::new(SyntheticSurface::drawable()))
    }

    fn verdicts(selection: &Selection) -> Vec<(&'static str, &Verdict)> {
        selection
            .considered
            .iter()
            .map(|candidate| (candidate.name, &candidate.verdict))
            .collect()
    }

    /// `decide` with the verdict observer discarded. The decision it returns is
    /// the same one the kernel uses; only the logging differs.
    fn decide_quietly(candidates: &[Candidate]) -> Selection {
        decide(candidates, &mut |_| {})
    }

    #[test]
    fn no_candidate_at_all_selects_nothing_and_says_so() {
        let selection = decide_quietly(&[]);
        assert!(selection.surface.is_none());
        assert!(selection.considered.is_empty());
        assert_eq!(selection.winner(), None);
    }

    #[test]
    fn a_candidate_that_has_nothing_to_offer_falls_through_to_the_next() {
        // The failure this pins down is the expensive one: a provider that
        // cannot deliver must not end the search on a machine whose screen is
        // its only output.
        let candidates = [
            candidate("absent", rank::DRIVER, absent),
            candidate("good", rank::DRM, good),
        ];
        let selection = decide_quietly(&candidates);
        assert_eq!(selection.winner(), Some("good"));
        assert_eq!(
            verdicts(&selection),
            vec![
                (
                    "absent",
                    &Verdict::Unavailable(Unavailable::Absent("no such device"))
                ),
                (
                    "good",
                    &Verdict::Selected {
                        width: 640,
                        height: 480,
                        pitch: 2560,
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_candidate_that_fails_falls_through_to_the_next() {
        let candidates = [
            candidate("failed", rank::DRIVER, failed),
            candidate("good", rank::DRM, good),
        ];
        let selection = decide_quietly(&candidates);
        assert_eq!(selection.winner(), Some("good"));
        assert_eq!(
            verdicts(&selection)[0],
            (
                "failed",
                &Verdict::Unavailable(Unavailable::Failed(String::from(
                    "the device answered wrongly"
                )))
            )
        );
    }

    #[test]
    fn every_candidate_is_accounted_for_in_the_order_it_was_consulted() {
        let candidates = [
            candidate("absent", rank::DRIVER, absent),
            candidate("failed", rank::DRM, failed),
            candidate("good", rank::FIRMWARE, good),
        ];
        let selection = decide_quietly(&candidates);
        assert_eq!(
            verdicts(&selection)
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            vec!["absent", "failed", "good"]
        );
    }

    #[test]
    fn two_working_candidates_leave_the_later_one_unconsulted() {
        // Both could drive the screen. The earlier one owns it, and the later
        // one is recorded as never asked rather than as a silent absence, so a
        // log distinguishes "lost to a better provider" from "never ran".
        let candidates = [
            candidate("good", rank::DRIVER, good),
            candidate("also-good", rank::DRM, also_good),
        ];
        let selection = decide_quietly(&candidates);
        assert_eq!(selection.winner(), Some("good"));
        let surface = selection.surface.as_ref().unwrap();
        assert_eq!((surface.width(), surface.height()), (640, 480));
        assert_eq!(
            verdicts(&selection)[1],
            ("also-good", &Verdict::Outranked { winner: "good" })
        );
    }

    #[test]
    fn a_surface_that_cannot_be_drawn_into_falls_through() {
        // The provider succeeded; the surface it produced did not. That is a
        // different failure from an absent device and it must not be the last
        // word when a provider below can still deliver.
        let candidates = [
            candidate("unusable-geometry", rank::DRIVER, short_pitch),
            candidate("good", rank::DRM, good),
        ];
        let selection = decide_quietly(&candidates);
        assert_eq!(selection.winner(), Some("good"));
        assert_eq!(
            verdicts(&selection)[1],
            (
                "good",
                &Verdict::Selected {
                    width: 640,
                    height: 480,
                    pitch: 2560,
                }
            )
        );
        match verdicts(&selection)[0].1 {
            Verdict::Unusable(reason) => assert!(
                reason.contains("pitch 512"),
                "the verdict must name the geometry that was rejected: {reason}"
            ),
            other => panic!("expected an unusable verdict, got {other:?}"),
        }
    }

    #[test]
    fn every_geometry_a_console_cannot_draw_into_is_rejected() {
        // Each row is one way a provider can succeed while handing back a
        // surface nothing can be written into. Only the first is a property of
        // the provider; the rest are properties of what it returned.
        let rejections: [(&str, Acquire); 5] = [
            ("empty extent", empty_extent),
            ("undrawable layout", undrawable_layout),
            ("short pitch", short_pitch),
            ("short allocation", short_allocation),
            ("taller than the allocation", taller_than_the_allocation),
        ];
        for (name, acquire) in rejections {
            let selection = decide_quietly(&[candidate(name, rank::DRIVER, acquire)]);
            assert!(
                selection.surface.is_none(),
                "{name} must not be selected as a console surface"
            );
            match verdicts(&selection)[0].1 {
                Verdict::Unusable(reason) => assert!(
                    !reason.is_empty(),
                    "{name} must be rejected with a stated reason"
                ),
                other => panic!("{name} must be unusable, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_last_candidate_still_wins_when_everyone_above_it_failed() {
        let candidates = [
            candidate("absent", rank::DRIVER, absent),
            candidate("empty", rank::DRM, empty_extent),
            candidate("good", rank::FIRMWARE, good),
        ];
        let selection = decide_quietly(&candidates);
        assert_eq!(selection.winner(), Some("good"));
    }

    #[test]
    fn a_verdict_is_reported_before_the_next_candidate_is_asked() {
        // The log is the only diagnostic a machine with no serial port has, and
        // the screen it is read from exists only once some candidate has won. A
        // losing verdict must therefore be emitted while the search is still
        // running: collected and printed afterwards, it is lost exactly when it
        // is needed -- if the fallback it enabled is what faults.
        SAW_A_VERDICT_FIRST.store(false, Ordering::SeqCst);
        let candidates = [
            candidate("absent", rank::DRIVER, absent),
            candidate("good", rank::DRM, good_only_after_a_verdict),
        ];
        let selection = decide(&candidates, &mut |_| {
            SAW_A_VERDICT_FIRST.store(true, Ordering::SeqCst);
        });
        assert_eq!(selection.winner(), Some("good"));
    }
}
