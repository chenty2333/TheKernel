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
//! [`set_mode`] is **§11 phases 3 to 6 in one sequence**: it chooses the mode,
//! paints the pattern into the framebuffer the caller allocated, computes the
//! whole pipe and output program *before* it writes anything, writes them in
//! the reference's order, and finishes with [`prove_it`].  It calls the three
//! sibling modules rather than duplicating them: `fb` for the surface, `pipe`
//! for §11 phases 3.4 and 4, `output` for phase 5, and WS-3's `pipe::prove` for
//! the phase 6 reads that are its registers.
//!
//! # Where the pattern fits
//!
//! §11 phase 6.5's colour bars are not a phase 6 action at all in this driver's
//! order: the framebuffer is filled *before* the plane is armed, in phase 3.2,
//! so that no window exists in which a correctly-programmed pipe scans out an
//! unfilled surface.  The fill goes through [`super::pattern::paint_row`], one
//! line at a time, so that a surface the caller owns can be written through
//! `fb::Surface::write_bytes` and the bar and marker geometry keeps exactly one
//! implementation.
//!
//! # Attribution: what the registers said before anything was written
//!
//! §11 phase 6.4 reads a *latched* bit, and this kernel's register table
//! declares `PIPESTAT` read-only, so nobody can clear it before arming the
//! plane.  [`set_mode`] therefore pre-samples the three registers phase 6 reads
//! before its first write ([`PreSample`]), and the verdict says when a reading
//! that looks like a failure was already there before this modeset: an underrun
//! bit that was already set, a DDI that was already idle, a `PLANE_SURFLIVE`
//! that already named our surface.  Without that, a stale bit is
//! indistinguishable from one this sequence caused.
//!
//! Nothing in this file has run against a display engine.  What the tests
//! establish is the arithmetic of the mode decision, the *order* of the whole
//! sequence against a mock register file, and the verdicts of the proof.

use alloc::{format, string::String, vec, vec::Vec};
use core::cmp::Ordering;

use super::{
    clk::{self, ClockError},
    fb::{self, FbError},
    gmbus::PollTimer,
    hpd::Ddi,
    output::{
        self, DDI_BUF_CTL_IS_IDLE, LinkRate, OutputError, OutputProgram, OutputRequest,
        OutputState, SwingProgram,
    },
    pattern::{self, PatternError, PatternGeometry},
    pipe::{self, ArmState, Pipe, PipeError, PipeProgram, PipeState},
    pll::PllFieldEncoding,
    regs::{
        Register, Registers,
        ddi::{DDI_BUF_CTL_A, DDI_BUF_CTL_B},
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

/// 6.1's evidence: what the sampler read, and the line rate it measured.
///
/// The samples are `pipe`'s, timestamps and all: the interval policy and the
/// wrap handling are its registers' business, and this module does not
/// re-derive a rate from numbers it was handed.  The values are carried beside
/// them because a report and a test both want the line fields alone.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ScanEvidence {
    /// The raw readings, in the order they were taken.
    pub(crate) samples: Vec<pipe::ScanlineSample>,
    /// `PIPEDSL`'s line field from those samples, in the same order.
    pub(crate) values: Vec<u32>,
    /// The line rate derived from the first pair of values that differed.
    /// Compare it with [`mode_line_rate_hz`] of the mode that was programmed:
    /// this is §12.3's scope-less check of the PLL arithmetic.
    pub(crate) observed_line_rate_hz: Option<u32>,
    /// The line rate the mode implies, when the caller supplied it.
    pub(crate) expected_line_rate_hz: Option<u32>,
}

impl ScanEvidence {
    /// Microseconds between the first and the last sample.
    pub(crate) fn elapsed_micros(&self) -> u64 {
        match (self.samples.first(), self.samples.last()) {
            (Some(first), Some(last)) => last.micros.saturating_sub(first.micros),
            _ => 0,
        }
    }

    /// Whether the observed line rate agrees with the mode's, within
    /// [`LINE_RATE_TOLERANCE_PERCENT`].  `None` when either rate is unknown.
    ///
    /// This is **evidence and not a verdict**, and it deliberately does not
    /// fail the check.  The sampling interval is a poll loop over the platform
    /// clock, not a hardware timer, and the line counter is 20 bits wide, so
    /// the derived rate carries several percent of slop; and a pipe scanning at
    /// the wrong rate is still scanning, which is what 6.1 asks.  What the
    /// number is for is §12.3's use: it is the only way to check the PLL
    /// arithmetic on the real machine without a scope.
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
///
/// The wait for the latch is `pipe::prove`'s, which polls for about two frame
/// times and separates "not latched yet" from "scanning another address"; this
/// carries what it found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SurfaceEvidence {
    /// The `PLANE_SURF` field that was written.
    pub(crate) expected: u32,
    /// What `PLANE_SURFLIVE` read.
    pub(crate) live: u32,
    /// Whether the register *already* named this surface before the modeset
    /// wrote anything.
    ///
    /// True means the read-back cannot be attributed to this sequence: the
    /// plane was armed on this address when the modeset started, so a match
    /// afterwards proves nothing about the write.  Recorded rather than
    /// suppressed, because a report that said only "SURFLIVE == the address"
    /// would be claiming evidence it does not have.
    pub(crate) pre_matching: bool,
}

/// 6.3's evidence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DdiEvidence {
    pub(crate) ddi_buf_ctl: u32,
    /// Whether the DDI was already out of idle *before* the modeset wrote
    /// anything.  On a pass that means the read-back does not attribute the
    /// running DDI to this sequence; on a failure it is the difference between
    /// a DDI that never came up and one that came up and went back to idle.
    pub(crate) pre_active: bool,
}

/// 6.4's evidence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct UnderrunEvidence {
    pub(crate) pipestat: u32,
    /// Whether the sticky underrun bit was already set before the modeset wrote
    /// anything.
    pub(crate) pre_existing: bool,
}

/// What phase 6's three registers said **before the modeset's first write**.
///
/// §11 phase 6.4 reads a latched bit and this kernel's register table declares
/// `PIPESTAT` read-only, so nothing here can clear it before the plane is
/// armed.  The pre-sample is what makes the difference between "an underrun is
/// latched" and "an underrun happened during this modeset"; the same reasoning
/// applies to a DDI that was already idle and to a `PLANE_SURFLIVE` that
/// already named our surface.
///
/// A field is `None` when the register could not be read before the writes,
/// which is itself a finding: the verdict then says the reading cannot be
/// attributed rather than guessing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PreSample {
    pub(crate) pipestat: Option<u32>,
    pub(crate) plane_surflive: Option<u32>,
    pub(crate) ddi_buf_ctl: Option<u32>,
}

impl PreSample {
    /// Read the three registers phase 6 will read again, before anything is
    /// written.
    pub(crate) fn take<R: Registers>(
        regs: &R,
        program: &PipeProgram,
        ddi_buf_ctl: Register,
    ) -> PreSample {
        PreSample {
            pipestat: regs.read(program.pipe.pipestat()),
            plane_surflive: regs.read(program.pipe.plane_surflive()),
            ddi_buf_ctl: regs.read(ddi_buf_ctl),
        }
    }

    /// Whether the sticky FIFO-underrun bit was already set.
    pub(crate) fn underrun_already_set(&self) -> bool {
        self.pipestat
            .is_some_and(|value| value & pipe::PIPE_FIFO_UNDERRUN_STATUS != 0)
    }

    /// Whether the DDI was already out of idle (which is the *good* state:
    /// `IS_IDLE` clear means the DDI is running).
    pub(crate) fn ddi_already_active(&self) -> bool {
        self.ddi_buf_ctl
            .is_some_and(|value| value & DDI_BUF_CTL_IS_IDLE == 0)
    }

    /// Whether `PLANE_SURFLIVE` already named this surface.
    pub(crate) fn surface_already_live(&self, expected: u32) -> bool {
        self.plane_surflive
            .is_some_and(|value| value & pipe::PLANE_SURF_ADDRESS_MASK == expected)
    }
}

/// Why a check failed.  Every variant carries the reading it was decided from,
/// because "it failed" is not a diagnosis and these values are what the person
/// reading the log has instead of an oscilloscope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ProveFailure {
    /// 6.1: every sample read the same line.
    NotScanning { samples: Vec<pipe::ScanlineSample> },
    /// 6.2: `PLANE_SURFLIVE` still read zero at the deadline, so the arm has
    /// not latched.
    ///
    /// This is **not** "the address was rejected": the arm latches at a vblank
    /// and the wait is about two frame times, so a pipe that is not scanning
    /// has no vblank to latch at.  The 6.1 verdict is what says which of those
    /// it is, and the advice for this check says to read it first.
    SurfaceNotLatched {
        expected: u32,
        waited_micros: u64,
        /// See [`SurfaceEvidence::pre_matching`].
        pre_matching: bool,
    },
    /// 6.2: `PLANE_SURFLIVE` holds a different, non-zero address, so the plane
    /// is scanning another surface -- a wrong buffer, not a late latch.
    SurfaceWrongAddress {
        expected: u32,
        live: u32,
        /// See [`SurfaceEvidence::pre_matching`].
        pre_matching: bool,
    },
    /// 6.3: the DDI buffer says it is still idle.
    DdiIdle {
        ddi_buf_ctl: u32,
        /// Whether it was already out of idle before this modeset wrote
        /// anything, which is what separates "it never came up" from "it came
        /// up and went back to idle".
        pre_active: bool,
    },
    /// 6.4: the sticky FIFO-underrun bit is set.
    ///
    /// The bit is sticky and this kernel declares `PIPESTAT` read-only, so it
    /// cannot be cleared before the plane is armed.  `pre_existing` is what
    /// says whether the bit was already set before this modeset: when it was,
    /// the verdict reports the reading and says it cannot be attributed here.
    FifoUnderrun { pipestat: u32, pre_existing: bool },
    /// The register could not be read at all -- outside the mapped window.
    Unreadable {
        check: CheckId,
        register: &'static str,
    },
}

/// The three registers phase 6 reads that are not the pipe's, plus the numbers
/// it compares against.
///
/// The pipe's own three reads come from WS-3's `pipe::prove`, which owns those
/// registers; this carries what that call cannot know: which DDI the mode was
/// set on, and the line rate the mode implies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ProveTarget {
    /// The `DDI_BUF_CTL` of the port the mode was set on.
    pub(crate) ddi_buf_ctl: Register,
    /// The line rate the mode implies, from [`mode_line_rate_hz`].
    pub(crate) line_rate_hz: Option<u32>,
}

impl ProveTarget {
    /// The target for a port the register table has a `DDI_BUF_CTL` for.
    pub(crate) const fn port(ddi: Ddi, line_rate_hz: Option<u32>) -> Option<ProveTarget> {
        match ddi_buf_ctl_register(ddi) {
            Some(ddi_buf_ctl) => Some(ProveTarget {
                ddi_buf_ctl,
                line_rate_hz,
            }),
            None => None,
        }
    }
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

    /// The `PLANE_SURFLIVE` address field §11 phase 6.2 read, as the
    /// **graphics address** `scanout::Verdict::Scanning` wants.
    ///
    /// The register holds `PLANE_SURF`'s `[31:12]`, which *is* the GGTT address
    /// with its low twelve bits zero -- the alignment `PipeProgram` refuses an
    /// address for not having.  So this is the value to hand to WS-1 rather
    /// than re-reading the register.  `None` when it could not be read at all.
    pub(crate) fn surflive(&self) -> Option<u64> {
        let live = match &self.surface {
            Ok(evidence) => evidence.live,
            // A wrong address is still a reading, and the caller may want it in
            // the log.  "Not latched" is not: the register read zero, and zero
            // is the absence of an address rather than one.
            Err(ProveFailure::SurfaceWrongAddress { live, .. }) => *live,
            Err(_) => return None,
        };
        Some(u64::from(live & pipe::PLANE_SURF_ADDRESS_MASK))
    }

    /// The verdict for one check, as the line a report carries.
    pub(crate) fn line(&self, check: CheckId) -> String {
        let text = match check {
            CheckId::Scanning => match &self.scan {
                Ok(evidence) => {
                    let rate = match (
                        evidence.observed_line_rate_hz,
                        evidence.expected_line_rate_hz,
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
                    };
                    format!(
                        "{} samples over {} us{}; {}",
                        evidence.values.len(),
                        evidence.elapsed_micros(),
                        rate,
                        match (evidence.values.first(), evidence.values.last()) {
                            (Some(first), Some(last)) =>
                                format!("changed: {first:#010x} -> {last:#010x}"),
                            _ => String::from("changed"),
                        }
                    )
                }
                Err(failure) => format!("FAILED -- {}", describe_failure(failure)),
            },
            CheckId::SurfaceLive => match &self.surface {
                Ok(evidence) => format!(
                    "{} == {}{}",
                    super::hex(u64::from(evidence.live), 8),
                    super::hex(u64::from(evidence.expected), 8),
                    if evidence.pre_matching {
                        "; the register already named this surface before the modeset wrote, so \
                         the read-back does not attribute the arm to this write"
                    } else {
                        ""
                    }
                ),
                Err(failure) => format!("FAILED -- {}", describe_failure(failure)),
            },
            CheckId::DdiActive => match &self.ddi {
                Ok(evidence) => format!(
                    "DDI_BUF_CTL = {}{}",
                    super::hex(u64::from(evidence.ddi_buf_ctl), 8),
                    if evidence.pre_active {
                        " (the DDI was already out of idle before this modeset wrote)"
                    } else {
                        ""
                    }
                ),
                Err(failure) => format!("FAILED -- {}", describe_failure(failure)),
            },
            CheckId::FifoUnderrun => match &self.underrun {
                Ok(evidence) => format!(
                    "PIPESTAT = {}{}",
                    super::hex(u64::from(evidence.pipestat), 8),
                    if evidence.pre_existing {
                        " (the bit was set before this modeset and is clear now)"
                    } else {
                        ""
                    }
                ),
                Err(failure) => format!("FAILED -- {}", describe_failure(failure)),
            },
        };
        format!("{} {}: {text}", check.reference(), check.title())
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

    /// Write the same lines to the kernel log.
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
/// **This is the console handover's gate** (`drm::screen` through
/// `scanout::register`).  The Intel surface may become the console's only when
/// this is [`ScanoutVerdict::ScanningOut`]; anything else must leave the console
/// where it is, which is what makes the handover a decision rather than a side
/// effect of programming registers.
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

    /// The reason to hand to `scanout::Verdict::NotScanning`, or `None` when
    /// the console may move.
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
            let readings: Vec<String> = samples
                .iter()
                .map(|sample| format!("{:#010x}@{}us", sample.line, sample.micros))
                .collect();
            let span = match (samples.first(), samples.last()) {
                (Some(first), Some(last)) => last.micros.saturating_sub(first.micros),
                _ => 0,
            };
            format!(
                "PIPEDSL read the same line on all {} samples over {span} us: {}",
                samples.len(),
                readings.join(", ")
            )
        }
        ProveFailure::SurfaceNotLatched {
            expected,
            waited_micros,
            pre_matching,
        } => format!(
            "PLANE_SURFLIVE still read zero {waited_micros} us after the arm, so {} has not \
             latched{}",
            super::hex(u64::from(*expected), 8),
            if *pre_matching {
                "; it named this surface before the modeset wrote anything, so the arm is not what \
                 put it there"
            } else {
                ".  Read the 6.1 verdict first: an arm latches at a vblank, and a pipe that is not \
                 scanning has none"
            }
        ),
        ProveFailure::SurfaceWrongAddress {
            expected,
            live,
            pre_matching,
        } => format!(
            "PLANE_SURFLIVE read {} where {} was written: the plane is scanning a different \
             surface{}",
            super::hex(u64::from(*live), 8),
            super::hex(u64::from(*expected), 8),
            if *pre_matching {
                ", and it was already on this one before the modeset wrote anything"
            } else {
                ""
            }
        ),
        ProveFailure::DdiIdle {
            ddi_buf_ctl,
            pre_active,
        } => format!(
            "DDI_BUF_CTL = {} still has IS_IDLE set{}",
            super::hex(u64::from(*ddi_buf_ctl), 8),
            if *pre_active {
                ", and it was already out of idle before this modeset wrote anything, so it came \
                 up and went back to idle"
            } else {
                ", and it was idle before this modeset wrote anything too: it never came up"
            }
        ),
        ProveFailure::FifoUnderrun {
            pipestat,
            pre_existing,
        } => format!(
            "PIPESTAT = {} has PIPE_FIFO_UNDERRUN_STATUS set{}",
            super::hex(u64::from(*pipestat), 8),
            if *pre_existing {
                ", and it was already set before this modeset wrote anything, so it may predate it \
                 -- the bit is sticky and this kernel's register table declares PIPESTAT read-only"
            } else {
                ""
            }
        ),
        ProveFailure::Unreadable { check, register } => format!(
            "{register} could not be read, so check {} could not run: the register is outside the \
             mapped window",
            check.reference()
        ),
    }
}

/// Reference §11 phase 6, "prove it", against one pipe and port.
///
/// **The reads are WS-3's and the verdict is this module's.**  `pipe::prove`
/// owns 6.1, 6.2 and 6.4 because it owns those registers and their sampling
/// policy; this function gives the plane its arming window first, takes 6.3's
/// one read of `DDI_BUF_CTL` (which is the output workstream's register, read
/// here because phase 6 asks the question), and assembles the single
/// [`ScanoutVerdict`] the console gate tests.
///
/// It writes nothing and logs nothing, so a host test can drive it against a
/// mock register file and a clock it owns.
pub(crate) fn prove_it<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
    program: &PipeProgram,
    target: &ProveTarget,
    pre: &PreSample,
) -> ProveReport {
    let checks = pipe::prove(regs, program, timer);

    let scan = match checks.scanline {
        pipe::ScanlineCheck::Scanning { samples }
        | pipe::ScanlineCheck::NotScanning { samples } => {
            let values = samples.iter().map(|sample| sample.line).collect();
            let scan = ScanEvidence {
                // The sampler's own measurement, not a second arithmetic over
                // the same numbers: `pipe` owns the interval policy and the
                // wrap handling now.
                observed_line_rate_hz: checks.scanline.observed_lines_per_second(),
                expected_line_rate_hz: target.line_rate_hz,
                samples: samples.to_vec(),
                values,
            };
            if checks.scanline.is_ok() {
                Ok(scan)
            } else {
                Err(ProveFailure::NotScanning {
                    samples: scan.samples,
                })
            }
        }
        pipe::ScanlineCheck::Unreadable { register } => Err(ProveFailure::Unreadable {
            check: CheckId::Scanning,
            register,
        }),
    };

    // 6.2's read, and the wait for the latch, are `pipe::prove`'s: it polls
    // `PLANE_SURFLIVE` for about two frame times and distinguishes a plane that
    // has not latched yet from one that is scanning a different address.  This
    // module used to run its own 100 ms poll first; it was a duplicate of a
    // better one, and its "no change" verdict could not tell those two apart.
    let surface = match checks.surface {
        pipe::SurfaceCheck::Armed { live, wrote } => Ok(SurfaceEvidence {
            expected: wrote,
            live,
            pre_matching: pre.surface_already_live(wrote),
        }),
        pipe::SurfaceCheck::NotLatched {
            wrote,
            waited_micros,
        } => Err(ProveFailure::SurfaceNotLatched {
            expected: wrote,
            waited_micros,
            pre_matching: pre.surface_already_live(wrote),
        }),
        pipe::SurfaceCheck::WrongAddress { live, wrote } => {
            Err(ProveFailure::SurfaceWrongAddress {
                expected: wrote,
                live,
                pre_matching: pre.surface_already_live(wrote),
            })
        }
        pipe::SurfaceCheck::Unreadable { register } => Err(ProveFailure::Unreadable {
            check: CheckId::SurfaceLive,
            register,
        }),
    };

    let underrun = match checks.underrun {
        pipe::UnderrunCheck::Clear { stat } => Ok(UnderrunEvidence {
            pipestat: stat,
            pre_existing: pre.underrun_already_set(),
        }),
        pipe::UnderrunCheck::Underrun { stat, .. } => Err(ProveFailure::FifoUnderrun {
            pipestat: stat,
            pre_existing: pre.underrun_already_set(),
        }),
        pipe::UnderrunCheck::Unreadable { register } => Err(ProveFailure::Unreadable {
            check: CheckId::FifoUnderrun,
            register,
        }),
    };

    // 6.3: the DDI.  One read, taken now rather than during phase 5, because
    // the question phase 6 asks is whether it is *still* out of idle.
    let ddi = match regs.read(target.ddi_buf_ctl) {
        Some(value) if value & DDI_BUF_CTL_IS_IDLE == 0 => Ok(DdiEvidence {
            ddi_buf_ctl: value,
            pre_active: pre.ddi_already_active(),
        }),
        Some(value) => Err(ProveFailure::DdiIdle {
            ddi_buf_ctl: value,
            pre_active: pre.ddi_already_active(),
        }),
        None => Err(ProveFailure::Unreadable {
            check: CheckId::DdiActive,
            register: target.ddi_buf_ctl.name(),
        }),
    };

    ProveReport {
        scan,
        surface,
        ddi,
        underrun,
    }
}

/// The `DDI_BUF_CTL` register of a DDI, for the ports the register table has
/// one for.
///
/// §8.1: the combo-PHY ports are A and B; C and D are the Type-C/DKL ports,
/// whose table entries `output.rs` refuses before it computes a value and which
/// the reference's §8.8 defers entirely.
pub(crate) const fn ddi_buf_ctl_register(ddi: Ddi) -> Option<Register> {
    match ddi {
        Ddi::A => Some(DDI_BUF_CTL_A),
        Ddi::B => Some(DDI_BUF_CTL_B),
        Ddi::C | Ddi::D => None,
    }
}

// ---------------------------------------------------------------------------
// The sequence: reference §11 phases 3.2 to 6
// ---------------------------------------------------------------------------

/// Everything [`set_mode`] is told.
///
/// The surface is a parameter and is never allocated here: §11 phase 3.2's
/// allocation is the framebuffer workstream's, and a sequence that allocated
/// its own memory could not be run twice against the same buffer, which is what
/// a repaint needs.
pub(crate) struct ModeRequest<'a> {
    /// The DDI the monitor answered on -- `sink.rs`'s `Pin::ddi()` is the
    /// authority for which physical port that is (§11 phase 2.3).
    pub(crate) ddi: Ddi,
    /// §11's preamble assumes pipe A; the type allows any pipe the register
    /// table has, and the pipe program carries which one it is.
    pub(crate) pipe: Pipe,
    /// What the mode layer decided, from the EDID below.
    pub(crate) plan: &'a ModePlan,
    /// The validated EDID bytes, for [`choose_mode`]'s search.
    pub(crate) edid: &'a [u8],
    /// The framebuffer to scan out and to paint the pattern into.
    pub(crate) surface: &'a fb::Surface,
    /// §11 phase 6.5's frame counter.  Zero for the first fill; a caller that
    /// repaints advances it, which is what makes the marker move.
    pub(crate) frame: u64,
    /// §6.3's `CFGCR1` field encoding.  No default: `output.rs` refuses to
    /// guess between the two conventions the sources disagree about.
    pub(crate) encoding: PllFieldEncoding,
    /// §8.5's voltage-swing values, or `None` when nobody has them, which
    /// makes the sequence refuse with `output`'s `MissingBufferTranslation`
    /// naming the table and §13.1 item 12.
    pub(crate) swing: Option<SwingProgram>,
    /// `DDI_BUF_CTL.PHY_LINK_RATE`.  §8.6 gives no sourced HDMI encoding for
    /// it, so the honest value is [`LinkRate::NoSourcedEncoding`] until a
    /// working dump settles it (§13.4).
    pub(crate) link_rate: LinkRate,
}

impl<'a> ModeRequest<'a> {
    /// The first-light-up request: HDMI, frame zero, the swing values the
    /// caller has (possibly none), and no sourced link-rate encoding.
    pub(crate) const fn new(
        ddi: Ddi,
        pipe: Pipe,
        plan: &'a ModePlan,
        edid: &'a [u8],
        surface: &'a fb::Surface,
        encoding: PllFieldEncoding,
    ) -> Self {
        Self {
            ddi,
            pipe,
            plan,
            edid,
            surface,
            frame: 0,
            encoding,
            swing: None,
            link_rate: LinkRate::NoSourcedEncoding,
        }
    }

    /// The pattern's frame counter, for a caller that repaints.
    pub(crate) const fn with_frame(mut self, frame: u64) -> Self {
        self.frame = frame;
        self
    }

    /// §8.5's voltage-swing values.
    pub(crate) const fn with_swing(mut self, swing: SwingProgram) -> Self {
        self.swing = Some(swing);
        self
    }

    /// `DDI_BUF_CTL.PHY_LINK_RATE`.
    pub(crate) const fn with_link_rate(mut self, link_rate: LinkRate) -> Self {
        self.link_rate = link_rate;
        self
    }
}

/// What one mode set did, step by step.
///
/// Everything is here for the same reason the power and sink reports keep
/// their observations: on the target the console is the only output device, so
/// the boot log's copy of this is what a person reads, and the debug file's
/// copy is what they read again an hour later.
pub(crate) struct ModeOutcome {
    /// Whose decision the mode was, and why.
    pub(crate) choice: ModeChoice,
    pub(crate) mode: Mode,
    /// §11 phase 6.5's pattern: what was drawn, and where its marker is.
    pub(crate) pattern: PatternGeometry,
    /// What phase 6's registers said before the first write.
    pub(crate) pre_sample: PreSample,
    /// What phase 3.4 and 4's shadow half wrote, and the exact list of writes.
    pub(crate) pipe: PipeProgram,
    pub(crate) pipe_state: PipeState,
    /// Phase 4.3, which is a separate step: `PLANE_CTL` then `PLANE_SURF`,
    /// adjacent, after the output is up.  See [`set_mode`]'s order note.
    pub(crate) arm: ArmState,
    /// What phase 5 planned and what it left behind.
    pub(crate) output: OutputProgram,
    pub(crate) output_state: OutputState,
    /// §11 phase 6.
    pub(crate) prove: ProveReport,
}

impl ModeOutcome {
    /// §11 phase 6's answer, which is what the console handover is gated on.
    pub(crate) fn verdict(&self) -> ScanoutVerdict {
        self.prove.verdict()
    }

    /// The `PLANE_SURFLIVE` reading, for `scanout::Verdict::Scanning`.
    pub(crate) fn surflive(&self) -> Option<u64> {
        self.prove.surflive()
    }

    /// Everything the run observed, as text.
    ///
    /// The mode choice, the pattern and its marker, every pipe write in order,
    /// the PLL's dividers and the rate they achieve, and phase 6's four
    /// readings with the verdict: enough for a person looking at the panel to
    /// check what they see against what the driver says it did.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("intel-modeset: {}\n", self.choice.describe()));
        out.push_str(&format!("intel-modeset: {}\n", self.pattern.describe()));
        out.push_str(&format!(
            "intel-modeset: mode {} ({} lines, {} Hz refresh, {} kHz pixel clock)\n",
            self.mode,
            self.mode.vtotal,
            self.mode.refresh_hz_rounded(),
            self.mode.clock_khz
        ));
        out.push_str(&self.pipe_state.render());
        out.push_str(&self.output.render());
        out.push_str(&self.output_state.render());
        out.push_str(&self.arm.render());
        out.push_str(&self.prove.render());
        out
    }

    /// The same lines, to the kernel log.
    pub(crate) fn log(&self) {
        axlog::info!("intel-modeset: {}", self.choice.describe());
        axlog::info!("intel-modeset: {}", self.pattern.describe());
        self.pipe_state.log();
        self.output.log();
        self.output_state.log();
        self.arm.log();
        self.prove.log();
    }
}

/// Why [`set_mode`] did not finish.
///
/// Every variant names the step and carries the reading or the inner error
/// that explains it, because the caller is a boot path whose only output is the
/// screen and the log line is the whole diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModesetError {
    /// §11 phase 3.1: nothing will be programmed.  Not a failure of the
    /// sequence -- it is the sequence declining to guess -- but the caller must
    /// not treat it as a mode set either, which is why it is an error here
    /// rather than a field of an outcome.
    Refused(ModeRefusal),
    /// The CDCLK registers could not be read.
    Clock(ClockError),
    /// The display has no usable CDCLK, so there is no pixel clock to program.
    NoCdclk { cdclk_khz: u32, detail: String },
    /// The DDI has no `DDI_BUF_CTL` in the register table.
    UnsupportedPort { ddi: Ddi },
    /// The pattern does not fit the surface.
    Pattern(PatternError),
    /// The framebuffer refused a write.
    Surface(FbError),
    /// §11 phases 3.4 and 4: the pipe and the plane's shadow registers.
    Pipe(PipeError),
    /// §11 phase 5: the PLL, the DDI and the transcoder.
    Output(OutputError),
    /// §11 phase 4.3, after phase 5: `PLANE_CTL` and `PLANE_SURF`.
    Arm(PipeError),
}

impl ModesetError {
    /// One line for the boot log.
    pub(crate) fn describe(&self) -> String {
        match self {
            ModesetError::Refused(refusal) => refusal.describe(),
            ModesetError::Clock(error) => {
                format!(
                    "the CDCLK registers could not be read: {}",
                    error.describe()
                )
            }
            ModesetError::NoCdclk { cdclk_khz, detail } => format!(
                "no usable CDCLK, so there is no pixel clock to program: {detail}.  The \
                 observation was {cdclk_khz} kHz, which is the bypass clock or a PLL the \
                 platform's table does not know; reference section 11 phase 1.4 is the step that \
                 leaves a usable one"
            ),
            ModesetError::UnsupportedPort { ddi } => format!(
                "DDI {} is not a combo-PHY port this kernel can program: the register table has \
                 DDI_BUF_CTL for A and B, and reference section 8.8 defers the Type-C/DKL path \
                 entirely",
                ddi.name()
            ),
            ModesetError::Pattern(error) => {
                format!(
                    "the test pattern does not fit the surface: {}",
                    error.describe()
                )
            }
            ModesetError::Surface(error) => {
                format!("the framebuffer refused a write: {}", error.describe())
            }
            ModesetError::Pipe(error) => error.describe(),
            ModesetError::Output(error) => error.describe(),
            ModesetError::Arm(error) => {
                format!("arming the plane failed: {}", error.describe())
            }
        }
    }
}

/// Reference §11 phases 3.2 to 6 against one device, in one sequence.
///
/// The order is the reference's and is not negotiable:
///
/// ```text
/// 3.1  choose the mode (and read CDCLK, which bounds the pixel clock)
/// 3.2  paint the pattern into the caller's framebuffer   <- before anything is armed
/// 3.4  compute the pipe's timings      \
/// 4.1  compute the DDB                  |  everything computed before the first write
/// 4.2  compute the watermarks           |
/// 5.x  compute the PLL, DDI and transcoder  /
/// --   pre-sample PIPESTAT, PLANE_SURFLIVE and DDI_BUF_CTL, before the first write
/// 3.4  write the timings               \
/// 4.1  write the DDB                    |  `pipe::program`: the shadow half, no PLANE_SURF
/// 4.2  write the watermarks             |
/// 4.3  write the plane's shadow values  /
/// 5.1  PLL power, dividers, enable, lock \
/// 5.2  DDI clock select, clock-off clear  |
/// 5.3  swing values, lane power           |  `output::program`, never ahead of the pipe
/// 5.4  TRANS_CLK_SEL                      |
/// 5.5  TRANS_DDI_FUNC_CTL                 |
/// 5.6  TRANSCONF                          |
/// 5.7  DDI_BUF_CTL, poll IS_IDLE clear   /
/// 4.3  PLANE_CTL then PLANE_SURF          `pipe::arm` -- the commit, and it comes *after*
///                                          the output, which is where this deviates from
///                                          section 11's numbering (see below)
/// 6    prove it
/// ```
///
/// **Why the arm is after phase 5 and not inside phase 4.**  `PLANE_SURF` is
/// only the arm: the shadow registers `pipe::program` writes are latched at the
/// plane's update event, which is the pipe's vblank, and while the transcoder
/// is disabled there is no vblank to latch them at -- "Until the pipe starts
/// PIPEDSL reads will return a stale value" (`[I915]`
/// `display/intel_display.c:478-486`).  A `PLANE_SURF` written before
/// `TRANSCONF` therefore cannot take effect until after it, and this driver
/// does not depend on a pre-enable write latching later: `[I915]` enables the
/// crtc first (`intel_enable_crtc`, `:7200`) and arms the plane afterwards
/// (`intel_update_crtc`, `:7249`).  Reference section 11 phase 4.3 arms the
/// plane inside phase 4, which is the order coreboot's libgfxinit ships; the
/// reference is followed everywhere else, and its own step 4.3 text ("if
/// ENABLE is set but SURFLIVE is 0, the surface address was rejected") is
/// exactly the misreading this order avoids.
///
/// A failure at any write stops the sequence and is reported in full: a mode
/// that was half programmed is not a mode.  Nothing is unwound, and the design
/// document argues why (§4.2 of `docs/design/intel-modeset.md`): the failure's
/// own signature on the panel is the primary diagnostic on a machine with no
/// serial port, and there is no saved firmware state to restore.
///
/// It allocates nothing, enables no interrupt, and writes no register outside
/// the two programs it computed.
pub(crate) fn set_mode<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
    request: &ModeRequest<'_>,
) -> Result<ModeOutcome, ModesetError> {
    // §11 phase 3.1's sanity check, and the ceiling the mode choice needs: one
    // pixel per clock, so the pixel clock cannot exceed CDCLK.  Reading it here
    // rather than taking it as a parameter means the choice reflects the
    // registers as they are now, which is what the pipe will actually run on.
    let cdclk = clk::observe(regs).map_err(ModesetError::Clock)?;
    if !cdclk.usable() {
        return Err(ModesetError::NoCdclk {
            cdclk_khz: cdclk.cdclk_khz,
            detail: cdclk.describe(),
        });
    }

    let choice = choose_mode(
        request.plan,
        request.edid,
        EngineLimits::at_cdclk(cdclk.cdclk_khz),
    );
    if let ModeChoice::Refused(refusal) = choice {
        return Err(ModesetError::Refused(refusal));
    }
    let mode = choice.mode().expect("a non-refusal names a mode");

    // The port's register, which phase 6.3 needs as well as phase 5.
    let Some(ddi_buf_ctl) = ddi_buf_ctl_register(request.ddi) else {
        return Err(ModesetError::UnsupportedPort { ddi: request.ddi });
    };

    // §11 phase 3.2 and 6.5: the pattern, before anything can scan it out.
    let pattern = paint_pattern(request.surface, request.frame)?;

    // Compute the whole program before the first write, so that a computation
    // that cannot succeed costs no register writes at all.
    let pipe_program = pipe::compute(
        request.pipe,
        &mode,
        pipe::PlaneSurface {
            ggtt_address: request.surface.ggtt_address(),
            stride_bytes: request.surface.stride(),
        },
    )
    .map_err(ModesetError::Pipe)?;

    let platform_ref_khz =
        output::read_platform_reference_khz(regs).map_err(ModesetError::Output)?;
    let mut output_request = OutputRequest::hdmi(request.ddi, mode, request.encoding);
    output_request.link_rate = request.link_rate;
    if let Some(swing) = request.swing {
        output_request = output_request.with_swing(swing);
    }
    let output_program =
        OutputProgram::plan(&output_request, platform_ref_khz).map_err(ModesetError::Output)?;

    // What phase 6's registers said *before* the modeset writes anything.  This
    // is what makes a latched underrun bit attributable (§11 6.4).
    let pre_sample = PreSample::take(regs, &pipe_program, ddi_buf_ctl);

    // §11 phases 3.4 and 4: the timings, the DDB, the watermarks and the
    // plane's shadow values.  None of them has taken effect yet -- every one is
    // double buffered and the arm is what latches them.
    let pipe_state = pipe::program(regs, &pipe_program).map_err(ModesetError::Pipe)?;

    // §11 phase 5: the output, and it enables the transcoder, which is what
    // makes a vblank exist for the arm below to latch at.
    let output_state = output::program(regs, &output_program).map_err(ModesetError::Output)?;

    // §11 phase 4.3, *after* phase 5: `PLANE_CTL` then `PLANE_SURF`, adjacent.
    // This is the one place the sequence reorders the reference's numbering,
    // and `set_mode`'s own documentation argues it: the arm latches at a
    // vblank, and until the transcoder is running there is none.
    let arm = pipe::arm(regs, &pipe_program).map_err(ModesetError::Arm)?;

    // §11 phase 6.
    let target = ProveTarget {
        ddi_buf_ctl,
        line_rate_hz: mode_line_rate_hz(&mode),
    };
    let prove = prove_it(regs, timer, &pipe_program, &target, &pre_sample);

    Ok(ModeOutcome {
        choice,
        mode,
        pattern,
        pre_sample,
        pipe: pipe_program,
        pipe_state,
        arm,
        output: output_program,
        output_state,
        prove,
    })
}

/// Paint §11 phase 6.5's pattern into a surface the caller allocated.
///
/// One reusable row buffer, painted by [`pattern::paint_row`] and written
/// through `Surface::write_bytes`: the framebuffer is device-uncached memory,
/// so the pattern is composed where the CPU can write it quickly and moved a
/// scan line at a time.  Only the visible pixels of each line are written, so
/// the surface's row padding is never touched -- the same guarantee
/// [`pattern::fill_xrgb8888`] makes for a byte slice.
fn paint_pattern(surface: &fb::Surface, frame: u64) -> Result<PatternGeometry, ModesetError> {
    let width = surface.width() as usize;
    let height = surface.height() as usize;
    let stride = surface.stride() as usize;
    pattern::check_geometry(stride, width, height).map_err(ModesetError::Pattern)?;

    let marker = pattern::marker_rect(width, height, frame);
    let mut row = vec![0u8; stride];
    for y in 0..height {
        pattern::paint_row(&mut row, width, marker, y);
        surface
            .write_bytes(y * stride, &row[..width * 4])
            .map_err(ModesetError::Surface)?;
    }
    Ok(PatternGeometry {
        stride,
        width,
        height,
        frame,
        marker,
    })
}

#[cfg(test)]
mod tests;
