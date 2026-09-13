//! The modeset: which mode to program, and the proof that programming it took.
//!
//! This is the workstream's own half of reference §11's bring-up order.  Two
//! pieces of it depend on nothing but the mode layer and the register file, and
//! they are here, complete and tested:
//!
//! * [`choose_mode`] reconciles **§11 phase 3.1**, which prefers the EDID's
//!   preferred timing and prefers 1920x1080@60 when the sink offers it, with
//!   the decision `drm::modes::select` already made.  Which one wins, and why,
//!   is stated in the design document and encoded here; the mode layer is never
//!   overridden silently.
//! * [`prove_it`] is **§11 phase 6**, "prove it": `PIPEDSL` must change,
//!   `PLANE_SURFLIVE` must read back what was written, `DDI_BUF_CTL.IS_IDLE`
//!   must be 0, and `PIPESTAT` bit 31 must be clear.  It answers with one value,
//!   [`ScanoutVerdict`], which is the gate the console handover tests.
//!
//! # The console gate
//!
//! The target machine has no serial port: the screen is the only output the
//! kernel has.  So the Intel surface becomes the console's surface **only after
//! phase 6 has proven the pipe is scanning out**, and that handover is a
//! separate, explicit step rather than a side effect of programming registers.
//! `drm::screen` expresses it through ranks -- `rank::DRIVER` (0) outranks
//! `rank::FIRMWARE` (200) -- so a candidate that cannot confirm the verdict
//! reports [`ScanoutVerdict::unavailable_reason`] as `Unavailable::Failed` and
//! the console stays where it was.  The verdict is one value, not four results,
//! because four results are four chances to check three of them.
//!
//! The rest of the driver -- `set_mode`, which runs §11 phases 3 to 5 by
//! calling the framebuffer, pipe and output sequences, and the
//! `ModesetReport` that carries every step's outcome to the log and the debug
//! file -- is **not here yet on purpose**.  It is the layer that calls the
//! three sibling modules (GGTT/framebuffer, DDI/output, pipe/plane), and
//! writing it before their signatures exist would mean inventing them.  The
//! design it will implement is `docs/design/intel-modeset.md`, which is
//! finished; [`ProveTarget`] and [`ModeChoice`] are the two interfaces it
//! consumes from this file.
//!
//! # Where the pattern fits
//!
//! §11 phase 6.5's colour bars are not a phase 6 action at all in this driver's
//! order: the framebuffer is filled *before* the plane is armed, in phase 3.2,
//! so that no window exists in which a correctly-programmed pipe scans out an
//! unfilled surface.  [`super::pattern`] generates it; `set_mode` will call it.
//!
//! Nothing in this file has run against a display engine.  What the tests
//! establish is the arithmetic of the mode decision and the verdicts of the
//! proof against a register file in a `BTreeMap`.

use alloc::{format, string::String, vec::Vec};
use core::cmp::Ordering;

use super::{
    gmbus::PollTimer,
    regs::{
        Register, Registers,
        ddi::DDI_BUF_CTL_A,
        pipe::{PIPEDSL_A, PIPESTAT_A, PLANE_SURFLIVE_A},
    },
};
use crate::drm::modes::{
    Edid, FallbackReason, Mode, ModeFlags, ModeList, ModePlan, SelectionReason, collect_modes,
};

// ---------------------------------------------------------------------------
// Which mode: reference §11 phase 3.1 against the mode layer's own decision
// ---------------------------------------------------------------------------

/// The pixel clock at and above which an HDMI link needs scrambling.
///
/// Reference §8.6 records `[INF]` that "HDMI ≥ 300 MHz TMDS (approximately
/// 4K30 or 1080p with high pixel clock) requires scrambling and the high TMDS
/// character rate", and then says not to enable either "until the simple case
/// works" -- which stage 2 does not do: §11's sequence programs no scrambling
/// bit anywhere.  So a mode at or above this clock is a mode this driver cannot
/// put on the wire, and the reference's own preference for 1080p60 (148.5 MHz)
/// is exactly a preference for a timing comfortably inside this limit.
///
/// The 8 bpc RGB case is the one that applies: `PIPE_MISC_BPC_8` is what §11
/// phase 5.6 programs, and at 8 bpc the TMDS clock equals the pixel clock.
/// `[INF]` HDMI 2.0's formal scrambling threshold is 340 MHz; the reference's
/// more conservative 300 MHz is used here, because nothing about this driver
/// would benefit from discovering the difference on real hardware.
pub(crate) const HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ: u32 = 300_000;

/// The resolution §11 phase 3.1 prefers.
pub(crate) const REFERENCE_HDISPLAY: u16 = 1920;
/// The line count §11 phase 3.1 prefers.
pub(crate) const REFERENCE_VDISPLAY: u16 = 1080;
/// The refresh §11 phase 3.1 prefers, to the hertz.
pub(crate) const REFERENCE_REFRESH_HZ: u32 = 60;

/// What the display engine can be given.
///
/// One number today, and it is not a property of the mode: the pipe consumes
/// one pixel per clock (stage 2 programs no scaling and no pixel repetition,
/// §11's preamble), so the pixel clock cannot exceed CDCLK.  The caller passes
/// what the power step observed -- `power::PowerState::cdclk` keeps the
/// firmware's CDCLK when it was legal and the programmed one otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EngineLimits {
    /// The highest pixel clock the pipe can carry, in kHz.
    pub(crate) max_clock_khz: u32,
}

impl EngineLimits {
    /// A pipe fed at CDCLK.
    pub(crate) const fn at_cdclk(cdclk_khz: u32) -> EngineLimits {
        EngineLimits {
            max_clock_khz: cdclk_khz,
        }
    }

    /// The clock a mode must be strictly under to be programmable at all: the
    /// engine's limit or the link's, whichever bites first.
    pub(crate) const fn ceiling_khz(&self) -> u32 {
        if self.max_clock_khz < HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ {
            self.max_clock_khz
        } else {
            HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ
        }
    }
}

/// Why a timing this driver was offered cannot be put on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NotProgrammable {
    /// The timing is not self-consistent: edges out of order, or a zero total.
    Malformed,
    /// Interlaced.  §11's sequence is progressive, and `timing.rs` refuses to
    /// build the registers for an interlaced mode.
    Interlaced,
    /// The encoder is asked to repeat each pixel, so the pipe runs at twice the
    /// clock the mode records.  §11's sequence programs no pixel repetition
    /// (`TRANS_DDI_FUNC_CTL`'s fields in §5.6 and §8.4 carry none), so
    /// programming one would give a half-width picture at the wrong rate.
    PixelRepeated,
    /// The pixel clock is above the ceiling.  See
    /// [`HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ`].
    ClockTooHigh { clock_khz: u32, ceiling_khz: u32 },
}

impl NotProgrammable {
    /// One clause for the log line, explaining what is wrong with a timing.
    pub(crate) fn describe(&self) -> String {
        match self {
            NotProgrammable::Malformed => String::from("the timing is not self-consistent"),
            NotProgrammable::Interlaced => String::from(
                "it is interlaced, and neither §11's sequence nor `timing.rs` programs an \
                 interlaced mode",
            ),
            NotProgrammable::PixelRepeated => String::from(
                "it asks the encoder to repeat each pixel, and §11's sequence programs no pixel \
                 repetition",
            ),
            NotProgrammable::ClockTooHigh {
                clock_khz,
                ceiling_khz,
            } => format!(
                "its pixel clock {clock_khz} kHz is above the {ceiling_khz} kHz this driver can \
                 program (no HDMI scrambling, and no pixel clock above CDCLK)"
            ),
        }
    }
}

/// Why no mode will be programmed.
///
/// Both variants end the modeset before it starts, and both leave the firmware
/// framebuffer on the screen: §11 phase 2.3 says not to proceed past an EDID
/// that is not valid, and a timing the kernel guessed is a timing that turns a
/// parse bug into a display bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModeRefusal {
    /// The mode layer never saw a usable EDID, so its answer is the built-in
    /// fallback -- a timing this sink never advertised.
    NoAdvertisedMode {
        because: FallbackReason,
        fallback: Mode,
    },
    /// The sink's timing cannot be programmed and the sink offers nothing else
    /// this driver can program.
    NothingProgrammable {
        chosen: Mode,
        because: NotProgrammable,
        ceiling_khz: u32,
    },
}

impl ModeRefusal {
    pub(crate) fn describe(&self) -> String {
        match self {
            ModeRefusal::NoAdvertisedMode { because, fallback } => format!(
                "the mode layer had no usable EDID ({because:?}) and chose the built-in fallback \
                 {fallback}; programming a timing the sink never advertised would replace a \
                 missing display with a wrong one, so the firmware framebuffer is left alone"
            ),
            ModeRefusal::NothingProgrammable {
                chosen,
                because,
                ceiling_khz,
            } => format!(
                "the mode layer chose {chosen}, which cannot be programmed because {}; the sink \
                 offers no {REFERENCE_HDISPLAY}x{REFERENCE_VDISPLAY}@{REFERENCE_REFRESH_HZ} \
                 timing under the {ceiling_khz} kHz ceiling, so the firmware framebuffer is left \
                 alone",
                because.describe()
            ),
        }
    }
}

/// The mode this driver will program, and whose decision it was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModeChoice {
    /// The mode layer's own answer, programmed unchanged.
    ModeLayer { mode: Mode, reason: SelectionReason },
    /// §11 phase 3.1's preference replaced the mode layer's answer, because
    /// the mode layer's answer cannot be programmed at all.  `replaced` is
    /// carried so the log can name what was set aside and why -- an override
    /// that is logged is a decision, and one that is not is a bug.
    ReferencePreference {
        mode: Mode,
        replaced: Mode,
        because: NotProgrammable,
    },
    /// Nothing will be programmed; the firmware's framebuffer stays on screen.
    Refused(ModeRefusal),
}

impl ModeChoice {
    /// The mode to program, or `None` when nothing will be programmed.
    pub(crate) fn mode(&self) -> Option<Mode> {
        match self {
            ModeChoice::ModeLayer { mode, .. } | ModeChoice::ReferencePreference { mode, .. } => {
                Some(*mode)
            }
            ModeChoice::Refused(_) => None,
        }
    }

    /// Whether the mode layer's decision was set aside.
    pub(crate) fn overrode_the_mode_layer(&self) -> bool {
        matches!(self, ModeChoice::ReferencePreference { .. })
    }

    /// The one line the boot log and the debug file carry.
    pub(crate) fn describe(&self) -> String {
        match self {
            ModeChoice::ModeLayer { mode, reason } => format!(
                "mode: {mode} -- the mode layer's own choice ({reason}), which this driver can \
                 program"
            ),
            ModeChoice::ReferencePreference {
                mode,
                replaced,
                because,
            } => format!(
                "mode: {mode} -- reference section 11 phase 3.1's preference.  The mode layer \
                 chose {replaced}, which cannot be programmed because {}; the reference's timing \
                 is the safe one the sink also advertises",
                because.describe()
            ),
            ModeChoice::Refused(refusal) => {
                format!("mode: not set -- {}", refusal.describe())
            }
        }
    }
}

/// Reconcile §11 phase 3.1 with the decision the mode layer already made.
///
/// The policy, in order, and the reason it is the policy:
///
/// 1. **The mode layer did not use the EDID** (`plan.used_edid()` is false).
///    Refuse: see [`ModeRefusal::NoAdvertisedMode`].
/// 2. **The mode layer's choice can be programmed.**  Take it, unchanged, and
///    say so.  §11 phase 3.1's first clause is "prefer the EDID's preferred
///    timing", which is the first thing `drm::modes::select` prefers too, so
///    the two policies agree unless the choice is not programmable at all.
/// 3. **It cannot be programmed, and the sink offers a 1920x1080@60 timing
///    that can.**  Take that one, and record what was replaced and why.  This
///    is §11 phase 3.1's second clause doing exactly the job it was written
///    for: 148.5 MHz is comfortably inside the HDMI table and needs no
///    scrambling, so when the sink's preferred timing is 4K60 the reference's
///    "robust first light-up" timing is the one that will actually appear.
/// 4. **Neither.**  Refuse: see [`ModeRefusal::NothingProgrammable`].
///
/// What this deliberately does not do is second-guess a programmable choice.
/// A sink whose preferred timing is 2560x1440@60 (241.5 MHz -- under the
/// ceiling, inside CDCLK on the target) is programmed at 2560x1440@60, not
/// dropped to 1080p because the reference mentions 1080p; the reference's
/// preference is a robustness argument, not a rule that a smaller mode is
/// better.
///
/// `edid_bytes` is read again rather than taken from the plan because the plan
/// keeps the *decision* and not the list it was made from, and the reference's
/// preference has to be searched for among the timings the sink actually
/// advertises.  A re-parse that fails is not fatal: the mode layer made its
/// decision from these bytes, so the search simply finds nothing and step 2
/// stands.
///
/// This function is pure: it reads no register and writes nothing.  The caller
/// logs [`ModeChoice::describe`].
pub(crate) fn choose_mode(plan: &ModePlan, edid_bytes: &[u8], limits: EngineLimits) -> ModeChoice {
    let chosen = plan.selection.mode;
    if !plan.used_edid() {
        let SelectionReason::BuiltinFallback { because } = plan.selection.reason else {
            // `used_edid` is defined as "the reason is not the built-in
            // fallback", so this arm cannot be reached from a `ModePlan` the
            // mode layer produced.  Refusing is still the honest answer for a
            // plan that says it used the EDID but carries a fallback reason.
            return ModeChoice::Refused(ModeRefusal::NothingProgrammable {
                chosen,
                because: NotProgrammable::Malformed,
                ceiling_khz: limits.ceiling_khz(),
            });
        };
        return ModeChoice::Refused(ModeRefusal::NoAdvertisedMode {
            because,
            fallback: chosen,
        });
    }

    match programmable(&chosen, limits) {
        Ok(()) => ModeChoice::ModeLayer {
            mode: chosen,
            reason: plan.selection.reason,
        },
        Err(because) => match reference_timing(edid_bytes, limits) {
            Some(mode) => ModeChoice::ReferencePreference {
                mode,
                replaced: chosen,
                because,
            },
            None => ModeChoice::Refused(ModeRefusal::NothingProgrammable {
                chosen,
                because,
                ceiling_khz: limits.ceiling_khz(),
            }),
        },
    }
}

/// Whether this driver can put a timing on the wire at all.
pub(crate) fn programmable(mode: &Mode, limits: EngineLimits) -> Result<(), NotProgrammable> {
    if !mode.is_well_formed() {
        return Err(NotProgrammable::Malformed);
    }
    if mode.is_interlaced() {
        return Err(NotProgrammable::Interlaced);
    }
    if mode.flags.contains(ModeFlags::DOUBLE_CLOCK) {
        return Err(NotProgrammable::PixelRepeated);
    }
    let ceiling_khz = limits.ceiling_khz();
    if mode.clock_khz > ceiling_khz {
        return Err(NotProgrammable::ClockTooHigh {
            clock_khz: mode.clock_khz,
            ceiling_khz,
        });
    }
    Ok(())
}

/// Whether a timing is the one §11 phase 3.1 prefers: 1920x1080 at 60 Hz.
///
/// The refresh is matched with the mode layer's own rounding
/// ([`Mode::refresh_hz_rounded`]), so 59.94 Hz -- which is what "1080p60" means
/// on most sinks and in CTA-861 -- counts, and 59 Hz or 61 Hz does not.
pub(crate) fn is_reference_timing(mode: &Mode) -> bool {
    mode.hdisplay == REFERENCE_HDISPLAY
        && mode.vdisplay == REFERENCE_VDISPLAY
        && !mode.is_interlaced()
        && mode.refresh_hz_rounded() == REFERENCE_REFRESH_HZ
}

/// The best 1920x1080@60 timing the sink advertises that this driver can
/// program, if it advertises one.
fn reference_timing(edid_bytes: &[u8], limits: EngineLimits) -> Option<Mode> {
    let edid = Edid::parse(edid_bytes)
        .or_else(|_| Edid::parse_lossy(edid_bytes))
        .ok()?;
    let mut list = ModeList::new();
    let _ = collect_modes(&edid, &mut list);
    list.iter()
        .map(|candidate| candidate.mode)
        .filter(|mode| is_reference_timing(mode) && programmable(mode, limits).is_ok())
        .min_by(preferred_order)
}

/// The mode layer's own within-step order, applied to the reference's timing
/// so that a sink advertising several 1080p60 rows always yields the same one:
/// larger active area, then higher refresh, then lower pixel clock, then lower
/// horizontal total.  (Every candidate here is 1920x1080, so the first key
/// never decides; it is kept because this is a copy of a stated policy and a
/// copy that silently drops a rule is how two policies drift apart.)
fn preferred_order(a: &Mode, b: &Mode) -> Ordering {
    b.active_area()
        .cmp(&a.active_area())
        .then_with(|| b.refresh_millihz().cmp(&a.refresh_millihz()))
        .then_with(|| a.clock_khz.cmp(&b.clock_khz))
        .then_with(|| a.htotal.cmp(&b.htotal))
        .then_with(|| a.hsync_positive.cmp(&b.hsync_positive))
        .then_with(|| a.vsync_positive.cmp(&b.vsync_positive))
}

/// The line rate a mode implies: the pixel clock over the horizontal total.
///
/// This is the number §12.3 says is the only way to check the PLL arithmetic
/// against reality without a scope -- "sample it at a known interval; the delta
/// gives the line rate, from which you can *derive* the true pixel clock".  It
/// is derived here so that the observed value from [`prove_it`] has something
/// to be compared against, and it is *not* a register value: `timing.rs`
/// programs the register and this compares against it.
pub(crate) fn mode_line_rate_hz(mode: &Mode) -> Option<u32> {
    if mode.htotal == 0 {
        return None;
    }
    Some(mode.clock_khz.saturating_mul(1000) / u32::from(mode.htotal))
}

// ---------------------------------------------------------------------------
// Prove it: reference §11 phase 6
// ---------------------------------------------------------------------------

/// `PIPEDSL.LINE`, reference §5.2: the low 20 bits are the current scanline.
pub(crate) const PIPEDSL_LINE_MASK: u32 = (1 << 20) - 1;

/// `DDI_BUF_CTL.IS_IDLE`, reference §8.4.
pub(crate) const DDI_BUF_CTL_IS_IDLE: u32 = 1 << 7;

/// `PIPESTAT.PIPE_FIFO_UNDERRUN_STATUS`, reference §5.2 and §11 step 6.4.
pub(crate) const PIPESTAT_FIFO_UNDERRUN: u32 = 1 << 31;

/// The address bits of `PLANE_SURF` / `PLANE_SURFLIVE`, reference §5.4:
/// `[31:12]`.  Bits below 12 are not address, and bit 2 is the decrypt flag,
/// which is not part of the surface's identity for this comparison.
pub(crate) const PLANE_SURFACE_ADDRESS_MASK: u32 = 0xffff_f000;

/// How long the scan check holds between samples of `PIPEDSL`.
///
/// §11 phase 6.1 says "a few milliseconds apart"; 5 ms is a third of a frame at
/// 60 Hz and about 337 lines at 1920x1080@60 (67.5 kHz line rate), so a pipe
/// that is scanning at all cannot show the same value twice.  It is short
/// enough that a pipe which is *not* scanning costs 15 ms of boot rather than a
/// second.
pub(crate) const SCAN_SAMPLE_INTERVAL_MICROS: u64 = 5_000;

/// How many samples the scan check takes before it concludes the pipe is not
/// scanning: the first, then three more across 15 ms.
///
/// The check stops at the first change, so the healthy path costs two reads.
/// The extra two exist because phase 5 has just enabled the transcoder: a pipe
/// that was enabled microseconds ago may not have advanced a line yet, and
/// declaring failure before a full frame has passed would report a slow start
/// as a dead pipe.
pub(crate) const SCAN_SAMPLES: usize = 4;

/// How long `PLANE_SURFLIVE` is given to read back the surface address.
///
/// The plane arms at a frame boundary after `PLANE_SURF` is written (§5.6:
/// "`PLANE_SURF` is the commit"), so an immediate read may legitimately show
/// zero.  100 ms is at least two frames at any refresh a sink is likely to
/// report down to 20 Hz, and it is a bound chosen for that reason -- not a
/// measured arming latency, because nothing here has run on real hardware.
pub(crate) const SURFACE_ARM_TIMEOUT_MICROS: u64 = 100_000;

/// A hard bound on the polls the surface check may make.
///
/// `SURFACE_ARM_TIMEOUT_MICROS` is the policy; this is the guarantee that a
/// broken clock cannot hang the boot path.  It is set well above the number of
/// polls the timeout needs at the real poll interval, so it can only ever be
/// reached when time is not moving.
pub(crate) const SURFACE_ARM_POLL_LIMIT: u32 = 100_000;

/// A hard bound on the pauses one sampling interval may take.
///
/// [`PollTimer`]'s contract is that `pause` advances `now_micros`, and every
/// implementation in this kernel honours it; this is the boot path refusing to
/// depend on another module's contract for its ability to finish.
const MAX_PAUSES_PER_INTERVAL: u32 = 50_000;

/// How far the observed line rate may differ from the mode's before
/// [`ScanEvidence::rate_agrees`] says so.
///
/// Five percent: loose enough that the sampling interval's own error cannot
/// trip it, tight enough that a wrong `(P,Q,K)` -- which is wrong by a factor,
/// not by a percent -- always does.
pub(crate) const LINE_RATE_TOLERANCE_PERCENT: u32 = 5;

/// Which of §11 phase 6's four checks a verdict is about.
///
/// The reference numbering is carried in the code because a failure is read by
/// someone with the reference open, and "6.4" is a shorter path to the fix than
/// "the underrun check".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CheckId {
    /// 6.1 -- `PIPEDSL` read twice must differ.
    Scanning,
    /// 6.2 -- `PLANE_SURFLIVE` must equal the address written.
    SurfaceLive,
    /// 6.3 -- `DDI_BUF_CTL.IS_IDLE` must be 0.
    DdiActive,
    /// 6.4 -- `PIPESTAT` bit 31 must be clear.
    FifoUnderrun,
}

impl CheckId {
    /// Every check, in the order the reference lists them.
    pub(crate) const ALL: [CheckId; 4] = [
        CheckId::Scanning,
        CheckId::SurfaceLive,
        CheckId::DdiActive,
        CheckId::FifoUnderrun,
    ];

    /// The step number in reference §11.
    pub(crate) const fn reference(self) -> &'static str {
        match self {
            CheckId::Scanning => "6.1",
            CheckId::SurfaceLive => "6.2",
            CheckId::DdiActive => "6.3",
            CheckId::FifoUnderrun => "6.4",
        }
    }

    /// What the check reads, as a person would name it.
    pub(crate) const fn title(self) -> &'static str {
        match self {
            CheckId::Scanning => "PIPEDSL is advancing",
            CheckId::SurfaceLive => "PLANE_SURFLIVE reads back the surface",
            CheckId::DdiActive => "DDI_BUF_CTL.IS_IDLE is clear",
            CheckId::FifoUnderrun => "PIPESTAT has no FIFO underrun",
        }
    }

    /// What to do about a failure, in §11's own words rather than new ones.
    pub(crate) const fn advice(self) -> &'static str {
        match self {
            CheckId::Scanning => {
                "the pipe is not scanning and nothing downstream matters: check the PLL (\u{a7}11 \
                 phase 5.1, including `ref` and the `(P,Q,K)` used), then the DDI clock mapping \
                 (5.2), then TRANSCONF's ENABLE and STATE_ENABLE (5.6), in that order"
            }
            CheckId::SurfaceLive => {
                "the plane never armed: re-read PLANE_CTL, and if ENABLE is set while SURFLIVE is \
                 not the address that was written, the surface address was rejected -- alignment, \
                 or a GGTT entry that is not valid (\u{a7}11 phase 4.3)"
            }
            CheckId::DdiActive => {
                "the DDI has no clock: check the DDI-to-PLL mapping (\u{a7}11 phase 5.2) and then \
                 the PLL itself (5.1), in that order -- IS_IDLE is the single best \"is my DDI \
                 alive\" bit on the chip"
            }
            CheckId::FifoUnderrun => {
                "the watermarks or the DDB allocation are wrong: go back to \u{a7}11 phase 4.2 \
                 (watermarks) before changing anything else"
            }
        }
    }
}

/// One reading of `PIPEDSL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ScanSample {
    /// Microseconds after the first sample of this check.
    pub(crate) micros: u64,
    /// The raw `PIPEDSL` value.
    pub(crate) value: u32,
}

/// 6.1's evidence: the samples, and the line rate they imply.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ScanEvidence {
    pub(crate) samples: Vec<ScanSample>,
    /// The line rate derived from the first pair of samples that differed.
    /// Compare it with [`mode_line_rate_hz`] of the mode that was programmed:
    /// this is §12.3's scope-less check of the PLL arithmetic.
    pub(crate) observed_line_rate_hz: Option<u32>,
    /// The line rate the mode implies, when the caller supplied it.
    pub(crate) expected_line_rate_hz: Option<u32>,
}

impl ScanEvidence {
    /// Whether the observed line rate agrees with the mode's, within
    /// [`LINE_RATE_TOLERANCE_PERCENT`].  `None` when either rate is unknown.
    ///
    /// This is **evidence and not a verdict**, and it deliberately does not
    /// fail the check.  The sampling interval is a poll loop over the platform
    /// clock, not a hardware timer, and the line counter is 20 bits wide, so
    /// the derived rate carries several percent of slop; and a pipe scanning at
    /// the wrong rate is still scanning, which is what 6.1 asks.  What the
    /// number is for is §12.3's use: it is the only way to check the PLL
    /// arithmetic on the real machine without a scope, and a mismatch of tens
    /// of percent is worth a person's attention even though it is not a
    /// failure.
    pub(crate) fn rate_agrees(&self) -> Option<bool> {
        let (observed, expected) = (self.observed_line_rate_hz?, self.expected_line_rate_hz?);
        if expected == 0 {
            return None;
        }
        let difference = observed.abs_diff(expected);
        Some(
            u64::from(difference) * 100
                <= u64::from(expected) * u64::from(LINE_RATE_TOLERANCE_PERCENT),
        )
    }
}

/// 6.2's evidence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SurfaceEvidence {
    pub(crate) expected: u32,
    pub(crate) live: u32,
    /// How many reads it took to see the address, which is how long the plane
    /// took to arm in polls.
    pub(crate) polls: u32,
    pub(crate) waited_micros: u64,
}

/// 6.3's evidence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DdiEvidence {
    pub(crate) ddi_buf_ctl: u32,
}

/// 6.4's evidence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct UnderrunEvidence {
    pub(crate) pipestat: u32,
}

/// Why a check failed.  Every variant carries the reading it was decided from,
/// because "it failed" is not a diagnosis and these values are what the person
/// reading the log has instead of an oscilloscope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ProveFailure {
    /// 6.1: every sample read the same value.
    NotScanning { samples: Vec<ScanSample> },
    /// 6.2: `PLANE_SURFLIVE` never read back the surface address.
    SurfaceNotLive {
        expected: u32,
        live: u32,
        polls: u32,
        waited_micros: u64,
    },
    /// 6.3: the DDI buffer says it is still idle.
    DdiIdle { ddi_buf_ctl: u32 },
    /// 6.4: the sticky FIFO-underrun bit is set.
    ///
    /// The bit is sticky and this kernel declares `PIPESTAT` read-only, so it
    /// cannot be cleared before the plane is armed: a set bit may predate this
    /// modeset.  It is still the first thing to check, which is why the verdict
    /// points at the watermarks rather than staying silent.
    FifoUnderrun { pipestat: u32 },
    /// The register could not be read at all -- outside the mapped window.
    Unreadable {
        check: CheckId,
        register: &'static str,
    },
}

/// What phase 6 found, check by check.
///
/// All four checks always run, including after one has failed.  This is not the
/// fail-fast rule the phases themselves follow, and it is deliberate: phase 6
/// *reads* and programs nothing, so running it to the end cannot leave the
/// hardware half-programmed -- and a report that stops at the first failure
/// throws away the reading that would explain it.  A dead pipe and a set
/// underrun bit together say "the pipe never started"; a dead pipe alone says
/// something else.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ProveReport {
    pub(crate) scan: Result<ScanEvidence, ProveFailure>,
    pub(crate) surface: Result<SurfaceEvidence, ProveFailure>,
    pub(crate) ddi: Result<DdiEvidence, ProveFailure>,
    pub(crate) underrun: Result<UnderrunEvidence, ProveFailure>,
}

impl ProveReport {
    /// §11 phase 6's answer, as **one value**.
    ///
    /// This is the gate the console handover tests, and it is deliberately the
    /// only way to ask the question.  A caller that combined four separate
    /// results could hand the console to a pipe that is not scanning out --
    /// and on a machine whose only output device is the screen, that mistake
    /// costs the log that would have explained it.  `ScanningOut` is returned
    /// only when every check passed; anything else carries every failure, in
    /// §11's order, so the reason survives into the log.
    pub(crate) fn verdict(&self) -> ScanoutVerdict {
        let failures: Vec<(CheckId, ProveFailure)> = CheckId::ALL
            .iter()
            .filter_map(|check| {
                self.failure_of(*check)
                    .map(|failure| (*check, failure.clone()))
            })
            .collect();
        if failures.is_empty() {
            ScanoutVerdict::ScanningOut
        } else {
            ScanoutVerdict::NotScanningOut(failures)
        }
    }

    /// The first failed check in §11's order, with its verdict.
    pub(crate) fn failure(&self) -> Option<(CheckId, &ProveFailure)> {
        CheckId::ALL
            .iter()
            .find_map(|check| self.failure_of(*check).map(|failure| (*check, failure)))
    }

    /// One check's failure, if it failed.
    pub(crate) fn failure_of(&self, check: CheckId) -> Option<&ProveFailure> {
        match check {
            CheckId::Scanning => self.scan.as_ref().err(),
            CheckId::SurfaceLive => self.surface.as_ref().err(),
            CheckId::DdiActive => self.ddi.as_ref().err(),
            CheckId::FifoUnderrun => self.underrun.as_ref().err(),
        }
    }

    /// The verdict for one check, as the line a report carries.
    pub(crate) fn line(&self, check: CheckId) -> String {
        match check {
            CheckId::Scanning => match &self.scan {
                Ok(evidence) => {
                    let first = evidence.samples.first();
                    let last = evidence.samples.last();
                    format!(
                        "{} {}: {} samples over {} us{}; {}",
                        check.reference(),
                        check.title(),
                        evidence.samples.len(),
                        last.map(|sample| sample.micros).unwrap_or(0),
                        match (
                            evidence.observed_line_rate_hz,
                            evidence.expected_line_rate_hz
                        ) {
                            (Some(observed), Some(expected)) => format!(
                                ", line rate {observed} Hz against the mode's {expected} Hz ({})",
                                match evidence.rate_agrees() {
                                    Some(true) => "agrees",
                                    Some(false) => "DOES NOT AGREE",
                                    None => "not comparable",
                                }
                            ),
                            (Some(observed), None) => format!(", line rate {observed} Hz"),
                            _ => String::new(),
                        },
                        match (first, last) {
                            (Some(first), Some(last)) =>
                                format!("changed: {:#010x} -> {:#010x}", first.value, last.value),
                            _ => String::from("changed"),
                        },
                    )
                }
                Err(failure) => format!(
                    "{} {}: FAILED -- {}",
                    check.reference(),
                    check.title(),
                    describe_failure(failure)
                ),
            },
            CheckId::SurfaceLive => match &self.surface {
                Ok(evidence) => format!(
                    "{} {}: {} == {} after {} poll(s), {} us",
                    check.reference(),
                    check.title(),
                    super::hex(u64::from(evidence.live), 8),
                    super::hex(u64::from(evidence.expected), 8),
                    evidence.polls,
                    evidence.waited_micros,
                ),
                Err(failure) => format!(
                    "{} {}: FAILED -- {}",
                    check.reference(),
                    check.title(),
                    describe_failure(failure)
                ),
            },
            CheckId::DdiActive => match &self.ddi {
                Ok(evidence) => format!(
                    "{} {}: DDI_BUF_CTL = {}",
                    check.reference(),
                    check.title(),
                    super::hex(u64::from(evidence.ddi_buf_ctl), 8),
                ),
                Err(failure) => format!(
                    "{} {}: FAILED -- {}",
                    check.reference(),
                    check.title(),
                    describe_failure(failure)
                ),
            },
            CheckId::FifoUnderrun => match &self.underrun {
                Ok(evidence) => format!(
                    "{} {}: PIPESTAT = {}",
                    check.reference(),
                    check.title(),
                    super::hex(u64::from(evidence.pipestat), 8),
                ),
                Err(failure) => format!(
                    "{} {}: FAILED -- {}",
                    check.reference(),
                    check.title(),
                    describe_failure(failure)
                ),
            },
        }
    }

    /// The phase's report, for `/sys/kernel/debug/dri/0/intel_gpu`.
    ///
    /// On the target machine the console is the only output device and the boot
    /// log scrolls away, so this is how a modeset that happened at t=3s is read
    /// at t=300s.
    pub(crate) fn render(&self) -> String {
        let mut out =
            String::from("\n--- the modeset (reference section 11 phase 6, \"prove it\") ---\n");
        for check in CheckId::ALL {
            out.push_str("  ");
            out.push_str(&self.line(check));
            out.push('\n');
        }
        let verdict = self.verdict();
        out.push_str(&format!("  verdict: {}\n", verdict.describe()));
        if let Some((check, _)) = self.failure() {
            out.push_str(&format!(
                "  first failure: {} -- {}\n",
                check.reference(),
                check.advice()
            ));
        }
        out
    }

    /// Write the same lines to the kernel log as they are produced.
    ///
    /// The last line is the verdict, which is also what the console handover
    /// reads: one line in the log and one value in memory say the same thing.
    pub(crate) fn log(&self) {
        for check in CheckId::ALL {
            let line = self.line(check);
            if self.check_passed(check) {
                axlog::info!("intel-modeset: {line}");
            } else {
                axlog::warn!("intel-modeset: {line}");
            }
        }
        let verdict = self.verdict();
        if verdict.is_scanning_out() {
            axlog::info!("intel-modeset: phase 6 verdict: {}", verdict.describe());
        } else {
            axlog::warn!("intel-modeset: phase 6 verdict: {}", verdict.describe());
        }
    }

    /// Whether one check passed, for the log's choice of level.
    pub(crate) fn check_passed(&self, check: CheckId) -> bool {
        match check {
            CheckId::Scanning => self.scan.is_ok(),
            CheckId::SurfaceLive => self.surface.is_ok(),
            CheckId::DdiActive => self.ddi.is_ok(),
            CheckId::FifoUnderrun => self.underrun.is_ok(),
        }
    }
}

/// §11 phase 6's answer, as one value a caller can test.
///
/// **This is the console handover's gate** (`drm::screen`).  The Intel surface
/// may become the console's only when this is [`ScanoutVerdict::ScanningOut`];
/// anything else must leave the console where it is, which is what makes the
/// handover a decision rather than a side effect of programming registers.
///
/// It is one value on purpose.  Four separate results are four chances for a
/// caller to check three of them, and a console handed to a pipe that is not
/// scanning out is a black screen with the explanation already written to a
/// surface nobody is reading.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ScanoutVerdict {
    /// Every check agreed: the display engine is scanning out this surface.
    /// The only value that entitles the console to move.
    ScanningOut,
    /// At least one check did not agree, with every failure in §11's order.
    NotScanningOut(Vec<(CheckId, ProveFailure)>),
}

impl ScanoutVerdict {
    /// Whether the display engine is scanning out, stated once.
    pub(crate) fn is_scanning_out(&self) -> bool {
        matches!(self, ScanoutVerdict::ScanningOut)
    }

    /// Every check that failed, in §11's order.  Empty when scanning out.
    pub(crate) fn failures(&self) -> &[(CheckId, ProveFailure)] {
        match self {
            ScanoutVerdict::ScanningOut => &[],
            ScanoutVerdict::NotScanningOut(failures) => failures,
        }
    }

    /// The reason to hand to `screen::Unavailable::Failed`, or `None` when the
    /// console may move.
    ///
    /// Written here rather than at the call site so that the sentence a person
    /// reads in the boot log is the sentence a test asserts on, and so the
    /// verdict's own readings -- the samples, the addresses, the raw registers
    /// -- are in it.
    pub(crate) fn unavailable_reason(&self) -> Option<String> {
        let ScanoutVerdict::NotScanningOut(failures) = self else {
            return None;
        };
        let Some((first, _)) = failures.first() else {
            return Some(String::from(
                "phase 6 produced no verdict at all, so nothing is known about the display engine",
            ));
        };
        let mut text =
            String::from("the mode was programmed but the display engine is not scanning out: ");
        for (index, (check, failure)) in failures.iter().enumerate() {
            if index > 0 {
                text.push_str("; ");
            }
            text.push_str(&format!(
                "{} {} failed -- {}",
                check.reference(),
                check.title(),
                describe_failure(failure)
            ));
        }
        text.push_str(".  ");
        text.push_str(first.advice());
        Some(text)
    }

    /// One line for the log and the debug file.
    pub(crate) fn describe(&self) -> String {
        match self.unavailable_reason() {
            Some(reason) => reason,
            None => String::from(
                "the display engine is scanning out the surface this mode set programmed",
            ),
        }
    }
}

/// The reading a failure was decided from, as one clause.
fn describe_failure(failure: &ProveFailure) -> String {
    match failure {
        ProveFailure::NotScanning { samples } => {
            let values: Vec<String> = samples
                .iter()
                .map(|sample| {
                    format!(
                        "{}@{}us",
                        super::hex(u64::from(sample.value), 8),
                        sample.micros
                    )
                })
                .collect();
            format!(
                "PIPEDSL read the same value {} times across the whole window: {}",
                samples.len(),
                values.join(", ")
            )
        }
        ProveFailure::SurfaceNotLive {
            expected,
            live,
            polls,
            waited_micros,
        } => format!(
            "PLANE_SURFLIVE read {} but {} was written, after {polls} poll(s) over \
             {waited_micros} us",
            super::hex(u64::from(*live), 8),
            super::hex(u64::from(*expected), 8),
        ),
        ProveFailure::DdiIdle { ddi_buf_ctl } => format!(
            "DDI_BUF_CTL = {} still has IS_IDLE set",
            super::hex(u64::from(*ddi_buf_ctl), 8)
        ),
        ProveFailure::FifoUnderrun { pipestat } => format!(
            "PIPESTAT = {} has PIPE_FIFO_UNDERRUN_STATUS set",
            super::hex(u64::from(*pipestat), 8)
        ),
        ProveFailure::Unreadable { check, register } => format!(
            "{register} could not be read, so check {} could not run: the register is outside the \
             mapped window",
            check.reference()
        ),
    }
}

/// The pipe and port phase 6 proves things about, and the numbers it checks
/// them against.
///
/// The registers are values rather than a pipe index because the pipe and DDI
/// sequences belong to the sibling workstreams: this is the seam, so that
/// whatever type those modules end up using for "the pipe we programmed" only
/// has to produce four named registers and two numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ProveTarget {
    /// The address written to `PLANE_SURF`, compared against `PLANE_SURFLIVE`.
    /// A 32-bit value because that is the whole of the register (§5.4).
    pub(crate) surface: u32,
    /// The line rate the mode implies, from [`mode_line_rate_hz`], when the
    /// caller knows the mode.  Reported next to the observed rate.
    pub(crate) line_rate_hz: Option<u32>,
    pub(crate) pipedsl: Register,
    pub(crate) plane_surflive: Register,
    pub(crate) ddi_buf_ctl: Register,
    pub(crate) pipestat: Register,
}

impl ProveTarget {
    /// Pipe A, plane 1 and port A -- the pipeline §11's checklist assumes
    /// ("pipe A, transcoder A, combo PHY A, HDMI/DVI, one plane").
    pub(crate) const fn pipe_a(surface: u32, line_rate_hz: Option<u32>) -> ProveTarget {
        ProveTarget {
            surface,
            line_rate_hz,
            pipedsl: PIPEDSL_A,
            plane_surflive: PLANE_SURFLIVE_A,
            ddi_buf_ctl: DDI_BUF_CTL_A,
            pipestat: PIPESTAT_A,
        }
    }
}

/// Reference §11 phase 6, "prove it", against one pipe and port.
///
/// This is deliberately pure: it reads registers, waits on the caller's timer
/// and returns a verdict.  It writes nothing, enables nothing and logs nothing,
/// so a host test can drive it against a mock register file and a clock it
/// owns, and the boot path can hand it the mapped aperture and the machine's
/// clock.
///
/// The four checks and the policy behind each:
///
/// * **6.1** `PIPEDSL` is sampled up to [`SCAN_SAMPLES`] times,
///   [`SCAN_SAMPLE_INTERVAL_MICROS`] apart, and the first change ends the
///   check.  Equal values throughout are [`ProveFailure::NotScanning`].  The
///   samples travel with the verdict, and so does the line rate they imply.
/// * **6.2** `PLANE_SURFLIVE` is polled until it matches the address written,
///   for up to [`SURFACE_ARM_TIMEOUT_MICROS`], because the plane arms at a
///   frame boundary rather than at the write.
/// * **6.3** and **6.4** are read once each.  They are live and latched status
///   bits: a retry would not make them more true, and a retry that *passed*
///   after a first read failed would hide exactly the fault the check exists to
///   report.
pub(crate) fn prove_it<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
    target: &ProveTarget,
) -> ProveReport {
    ProveReport {
        scan: scan_check(regs, timer, target),
        surface: surface_check(regs, timer, target),
        ddi: ddi_check(regs, target),
        underrun: underrun_check(regs, target),
    }
}

/// 6.1: `PIPEDSL` must change.
fn scan_check<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
    target: &ProveTarget,
) -> Result<ScanEvidence, ProveFailure> {
    let first = read(regs, target.pipedsl, CheckId::Scanning)?;
    let mut samples = Vec::with_capacity(SCAN_SAMPLES);
    samples.push(ScanSample {
        micros: 0,
        value: first,
    });
    let start = timer.now_micros();
    let mut changed = false;
    for _ in 1..SCAN_SAMPLES {
        wait(timer, SCAN_SAMPLE_INTERVAL_MICROS);
        let value = read(regs, target.pipedsl, CheckId::Scanning)?;
        samples.push(ScanSample {
            micros: timer.now_micros().saturating_sub(start),
            value,
        });
        if value != first {
            changed = true;
            break;
        }
    }
    if !changed {
        return Err(ProveFailure::NotScanning { samples });
    }
    let observed_line_rate_hz = line_rate(&samples);
    Ok(ScanEvidence {
        samples,
        observed_line_rate_hz,
        expected_line_rate_hz: target.line_rate_hz,
    })
}

/// The line rate implied by the first pair of samples that differed.
fn line_rate(samples: &[ScanSample]) -> Option<u32> {
    let pair = samples
        .windows(2)
        .find(|pair| pair[0].value != pair[1].value)?;
    let elapsed = pair[1].micros.saturating_sub(pair[0].micros);
    if elapsed == 0 {
        return None;
    }
    let lines = u64::from(
        (pair[1].value & PIPEDSL_LINE_MASK).wrapping_sub(pair[0].value & PIPEDSL_LINE_MASK),
    );
    if lines == 0 {
        return None;
    }
    Some((lines.saturating_mul(1_000_000) / elapsed).min(u64::from(u32::MAX)) as u32)
}

/// 6.2: `PLANE_SURFLIVE` must read back the address that was written.
fn surface_check<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
    target: &ProveTarget,
) -> Result<SurfaceEvidence, ProveFailure> {
    let start = timer.now_micros();
    let mut polls = 0u32;
    loop {
        let live = read(regs, target.plane_surflive, CheckId::SurfaceLive)?;
        polls += 1;
        if address_bits(live) == address_bits(target.surface) {
            return Ok(SurfaceEvidence {
                expected: target.surface,
                live,
                polls,
                waited_micros: timer.now_micros().saturating_sub(start),
            });
        }
        let waited_micros = timer.now_micros().saturating_sub(start);
        if waited_micros >= SURFACE_ARM_TIMEOUT_MICROS || polls >= SURFACE_ARM_POLL_LIMIT {
            return Err(ProveFailure::SurfaceNotLive {
                expected: target.surface,
                live,
                polls,
                waited_micros,
            });
        }
        timer.pause();
    }
}

/// 6.3: the DDI buffer must not be idle.
fn ddi_check<R: Registers>(regs: &R, target: &ProveTarget) -> Result<DdiEvidence, ProveFailure> {
    let value = read(regs, target.ddi_buf_ctl, CheckId::DdiActive)?;
    if value & DDI_BUF_CTL_IS_IDLE != 0 {
        return Err(ProveFailure::DdiIdle { ddi_buf_ctl: value });
    }
    Ok(DdiEvidence { ddi_buf_ctl: value })
}

/// 6.4: no FIFO underrun may be latched.
fn underrun_check<R: Registers>(
    regs: &R,
    target: &ProveTarget,
) -> Result<UnderrunEvidence, ProveFailure> {
    let value = read(regs, target.pipestat, CheckId::FifoUnderrun)?;
    if value & PIPESTAT_FIFO_UNDERRUN != 0 {
        return Err(ProveFailure::FifoUnderrun { pipestat: value });
    }
    Ok(UnderrunEvidence { pipestat: value })
}

/// The address part of a surface register.  See
/// [`PLANE_SURFACE_ADDRESS_MASK`].
fn address_bits(value: u32) -> u32 {
    value & PLANE_SURFACE_ADDRESS_MASK
}

/// Read a register, naming it if it is not there.
fn read<R: Registers>(regs: &R, register: Register, check: CheckId) -> Result<u32, ProveFailure> {
    regs.read(register).ok_or(ProveFailure::Unreadable {
        check,
        register: register.name(),
    })
}

/// Let an interval pass, with a bound that does not depend on the clock
/// advancing.  Returns the time that actually passed.
fn wait<T: PollTimer>(timer: &T, interval_micros: u64) -> u64 {
    let start = timer.now_micros();
    let mut pauses = 0u32;
    while timer.now_micros().saturating_sub(start) < interval_micros
        && pauses < MAX_PAUSES_PER_INTERVAL
    {
        timer.pause();
        pauses += 1;
    }
    timer.now_micros().saturating_sub(start)
}

#[cfg(test)]
mod tests;
