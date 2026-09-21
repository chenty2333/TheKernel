//! Post-arm scanline, surface-latch, and underrun checks.

use super::*;

// ---------------------------------------------------------------------------
// Phase 6: the pipe-side read-backs
// ---------------------------------------------------------------------------

/// Section 11 phase 6.1's verdict on `PIPEDSL`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanlineCheck {
    /// At least two of the samples differed, so the pipe is counting lines.
    ///
    /// The samples carry their timestamps as well as their line counts, so the
    /// verdict can report the line rate it observed and not only the fact that
    /// the counter moved.  Section 12.3 calls `PIPEDSL` sampled over time "the
    /// only way to verify your PLL arithmetic against reality without a
    /// scope"; a verdict that discarded the timestamps threw that measurement
    /// away.
    Scanning {
        samples: [ScanlineSample; SCANLINE_SAMPLES],
    },
    /// Every sample was identical: the pipe is not scanning.
    NotScanning {
        samples: [ScanlineSample; SCANLINE_SAMPLES],
    },
    /// `PIPEDSL` could not be read at all.
    Unreadable { register: &'static str },
}

/// One `PIPEDSL` reading: the line the pipe was on, and when that was read.
///
/// Both fields are raw: `line` is `PIPEDSL`'s `LINE[19:0]` and `micros` is the
/// timer's reading, so the difference between two samples is the only thing
/// with a meaning and [`ScanlineCheck`] is where the arithmetic over them
/// lives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScanlineSample {
    /// `PIPEDSL`'s `LINE[19:0]`.
    pub(crate) line: u32,
    /// The timer's reading when the register was read, in microseconds.
    pub(crate) micros: u64,
}

impl ScanlineSample {
    /// The lines between this sample and `next`, signed: negative means the
    /// counter went backwards, which is what a frame wrap looks like from
    /// inside one sampling window.
    pub(crate) const fn line_delta(self, next: Self) -> i64 {
        (next.line as i64) - (self.line as i64)
    }

    /// How long after this sample `next` was taken, in microseconds.
    ///
    /// Saturating rather than wrapping: a timer that went backwards is a
    /// broken timer, and the interval it produces is one no rate should be
    /// computed from.
    pub(crate) const fn micros_delta(self, next: Self) -> u64 {
        next.micros.saturating_sub(self.micros)
    }
}

impl ScanlineCheck {
    pub(crate) const fn is_ok(self) -> bool {
        matches!(self, Self::Scanning { .. })
    }

    /// Every interval between consecutive samples, as `(line delta, micros)`.
    ///
    /// Three entries for four samples, in order.  This is what the log renders,
    /// and it is here rather than in the log because it is also the raw
    /// material of [`Self::observed_lines_per_second`].
    pub(crate) fn intervals(self) -> [(i64, u64); SCANLINE_SAMPLES - 1] {
        let mut out = [(0i64, 0u64); SCANLINE_SAMPLES - 1];
        match self {
            Self::Scanning { samples } | Self::NotScanning { samples } => {
                for (index, pair) in samples.windows(2).enumerate() {
                    out[index] = (pair[0].line_delta(pair[1]), pair[0].micros_delta(pair[1]));
                }
            }
            Self::Unreadable { .. } => {}
        }
        out
    }

    /// The line rate the samples measured, in lines per second, or `None` when
    /// they cannot give one.
    ///
    /// A rate over the intervals that moved forward and took time: an interval
    /// whose counter did not move is not a measurement of zero lines per
    /// second, it is not a measurement, and one that went backwards is a frame
    /// wrap or a counter that is not counting.  `None` therefore means "no
    /// measurement", never "zero", and it is what a timer that does not advance
    /// produces.
    ///
    /// This is section 12.3's observation: the delta over a known interval
    /// gives the line rate, and the line rate is the pixel clock divided by the
    /// line total -- 67.5 klines/s at 148.5 MHz over 2200 pixels.  The number
    /// reported here is measured, not derived; comparing it with the mode is
    /// the reader's step, and the boot log prints both.
    pub(crate) fn observed_lines_per_second(self) -> Option<u32> {
        let mut lines = 0u64;
        let mut micros = 0u64;
        for (delta, span) in self.intervals() {
            if delta > 0 && span > 0 {
                lines += delta as u64;
                micros += span;
            }
        }
        if micros == 0 || lines == 0 {
            return None;
        }
        u32::try_from(lines * 1_000_000 / micros).ok()
    }

    /// What to do about it, in the reference's own terms.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Scanning { samples } => format!(
                "PIPEDSL sampled {} times over {} ms and changed: {}.  Intervals: {}.  Observed \
                 line rate: {}.  The pipe is scanning (reference section 11 phase 6.1, and \
                 section 12.3 for the rate).",
                SCANLINE_SAMPLES,
                (SCANLINE_SAMPLES as u64 - 1) * SCANLINE_INTERVAL_MICROS / 1_000,
                render_samples(&samples),
                render_intervals(self.intervals()),
                render_rate(self.observed_lines_per_second()),
            ),
            Self::NotScanning { samples } => format!(
                "PIPEDSL read {} on every one of {SCANLINE_SAMPLES} samples over {} ms: the value \
                 never changed, so the pipe is not scanning and nothing downstream of it matters \
                 (reference section 11 phase 6.1).  Intervals: {}.  Observed line rate: {}.  \
                 Check, in this order: TRANSCONF is written with ENABLE | STATE_ENABLE (section \
                 11 step 5.6); TRANS_CLK_SEL names the port PLL the DDI is using (step 5.4); the \
                 PLL locked (step 5.1)",
                render_samples(&samples),
                (SCANLINE_SAMPLES as u64 - 1) * SCANLINE_INTERVAL_MICROS / 1_000,
                render_intervals(self.intervals()),
                render_rate(self.observed_lines_per_second()),
            ),
            Self::Unreadable { register } => format!(
                "{register} could not be read, so whether the pipe is scanning is unknown.  It is \
                 outside the mapped register window, which is a fault in this kernel's mapping \
                 rather than in the mode"
            ),
        }
    }
}

/// The samples as `line@micros`, in order.
fn render_samples(samples: &[ScanlineSample; SCANLINE_SAMPLES]) -> String {
    let mut out = String::new();
    for (index, sample) in samples.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{}@{}us", sample.line, sample.micros));
    }
    out
}

/// The intervals as `+lines/micros`, in order.
fn render_intervals(intervals: [(i64, u64); SCANLINE_SAMPLES - 1]) -> String {
    let mut out = String::new();
    for (index, (lines, micros)) in intervals.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{lines:+}/{micros}us"));
    }
    out
}

/// The measured rate as a log phrase, or why there is none.
fn render_rate(rate: Option<u32>) -> String {
    match rate {
        Some(rate) => format!("{rate} lines/s"),
        None => String::from(
            "none: no interval both moved the counter forward and took time, so nothing was \
             measured",
        ),
    }
}

/// Section 11 phase 6.2's verdict on `PLANE_SURFLIVE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceCheck {
    /// The live address is the one that was written.
    Armed { live: u32, wrote: u32 },
    /// `PLANE_SURFLIVE` still read zero at the deadline: the plane has not
    /// latched.
    ///
    /// Since the arm is a separate step that runs after the pipe is enabled,
    /// this is the *expected* result of checking too early, and it is not
    /// evidence about the surface address: see [`SurfaceCheck::describe`].
    NotLatched {
        wrote: u32,
        /// How long the poll waited, in microseconds.
        waited_micros: u64,
    },
    /// `PLANE_SURFLIVE` holds a different, non-zero address: the plane is
    /// scanning something else, and that is a wrong buffer rather than a late
    /// latch.
    WrongAddress { live: u32, wrote: u32 },
    /// `PLANE_SURFLIVE` could not be read at all.
    Unreadable { register: &'static str },
}

impl SurfaceCheck {
    pub(crate) const fn is_ok(self) -> bool {
        matches!(self, Self::Armed { .. })
    }

    pub(crate) fn describe(self) -> String {
        match self {
            Self::Armed { live, wrote } => format!(
                "PLANE_SURFLIVE {live:#010x} equals the address written to PLANE_SURF \
                 {wrote:#010x}: the plane is armed (reference section 11 phase 6.2)"
            ),
            Self::NotLatched {
                wrote,
                waited_micros,
            } => format!(
                "PLANE_SURFLIVE still reads zero {waited_micros} us after the arm, which is about \
                 {SURFACE_LATCH_FRAMES} frame times, so PLANE_SURF's {wrote:#010x} has not been \
                 latched yet.  **This is not \"the surface address was rejected\"**: the arm is \
                 latched at a vblank, and while the transcoder is disabled there is no vblank to \
                 latch it at (\"Until the pipe starts PIPEDSL reads will return a stale value\", \
                 display/intel_display.c:478-486).  Read the phase-6.1 verdict first: if the pipe \
                 is not scanning, the mode is not running and this check has nothing to say yet.  \
                 If the pipe *is* scanning and this stays zero, then read PLANE_CTL back for \
                 ENABLE and treat the address as rejected -- section 11 step 4.3 names alignment \
                 and an invalid GGTT entry as the two causes"
            ),
            Self::WrongAddress { live, wrote } => format!(
                "PLANE_SURFLIVE reads {live:#010x} after the whole \
                 {SURFACE_LATCH_FRAMES}-frame-time wait, but PLANE_SURF was written with \
                 {wrote:#010x}: the plane is scanning a different surface -- most likely the \
                 firmware's -- so the arm did not take (reference section 11 phase 6.2).  This is \
                 a wrong buffer rather than a late latch: a plane whose arm had not latched yet \
                 would read zero"
            ),
            Self::Unreadable { register } => {
                format!("{register} could not be read, so whether the plane armed is unknown")
            }
        }
    }
}

/// Section 11 phase 6.4's verdict on the FIFO underrun bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnderrunCheck {
    /// `PIPESTAT` bit 31 is clear.
    Clear { stat: u32 },
    /// `PIPESTAT` bit 31 is set: the watermarks or the DDB are wrong.
    Underrun {
        stat: u32,
        /// The level-0 `PLANE_WM` value this program wrote, so the reader can
        /// compare it with the register without another lookup.
        watermark: u32,
        /// The `PLANE_BUF_CFG` value this program wrote.
        ddb: u32,
    },
    /// `PIPESTAT` could not be read at all.
    Unreadable { register: &'static str },
}

impl UnderrunCheck {
    pub(crate) const fn is_ok(self) -> bool {
        matches!(self, Self::Clear { .. })
    }

    pub(crate) fn describe(self) -> String {
        match self {
            Self::Clear { stat } => format!(
                "PIPESTAT {stat:#010x} has bit 31 clear: no FIFO underrun (reference section 11 \
                 phase 6.4)"
            ),
            Self::Underrun {
                stat,
                watermark,
                ddb,
            } => format!(
                "PIPESTAT {stat:#010x} has bit 31 set: the pipe's FIFO underran.  Reference \
                 section 11 step 6.4 says to go back to phase 4.2 before changing anything else, \
                 because the watermarks or the DDB are wrong: this program wrote PLANE_WM(0) = \
                 {watermark:#010x} and PLANE_BUF_CFG = {ddb:#010x}.  Reference section 7.1: with \
                 PLANE_WM_EN clear the plane reads nothing, and with the reset values the display \
                 engine does not operate at all.  The generous level 0 this bring-up uses is \
                 section 7.3's, not the section 7.4 latency calculation, so an underrun here is \
                 the expected cost of that choice rather than a surprising one"
            ),
            Self::Unreadable { register } => format!(
                "{register} could not be read, so whether the pipe underran is unknown.  \
                 Reference section 10.8: an underrun that is never seen is an underrun that gets \
                 misattributed to the timing or the monitor"
            ),
        }
    }
}

/// The three pipe-side reads of reference section 11 phase 6.
///
/// Every check runs, and every check reports, even when an earlier one failed:
/// on a machine whose only console is the screen, three findings are worth more
/// than one, and a caller that stops at the first failure learns less about
/// which half of the sequence is wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PipeChecks {
    pub(crate) pipe: Pipe,
    pub(crate) scanline: ScanlineCheck,
    pub(crate) surface: SurfaceCheck,
    pub(crate) underrun: UnderrunCheck,
}

impl PipeChecks {
    /// Whether all three checks passed.
    pub(crate) const fn ok(&self) -> bool {
        self.scanline.is_ok() && self.surface.is_ok() && self.underrun.is_ok()
    }

    /// The three checks as log lines.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        for (phase, text) in [
            ("6.1", self.scanline.describe()),
            ("6.2", self.surface.describe()),
            ("6.4", self.underrun.describe()),
        ] {
            out.push_str(&format!(
                "intel-pipe: pipe {} phase {phase}: {text}\n",
                self.pipe
            ));
        }
        out
    }

    /// Put [`Self::render`] into the kernel log.
    pub(crate) fn log(&self) {
        for line in self.render().lines() {
            axlog::debug!("{line}");
        }
    }
}

/// Reference section 11 phase 6.1, 6.2 and 6.4 against one pipe.
///
/// The clock is a parameter for the same reason `sink::probe_one`'s is: the
/// scanline check has to let a few milliseconds pass and the surface check has
/// to give the plane's arm a couple of frame times to latch, and a host test
/// cannot wait for real ones, so the test supplies its own [`PollTimer`] and
/// the boot path passes [`MonotonicTimer`].
///
/// This is the shape the top-level "prove it" step calls.  It reads registers
/// and writes nothing: it is safe to run at any point, including against a pipe
/// someone else programmed.
pub(crate) fn prove(
    regs: &impl Registers,
    plan: &PipeProgram,
    timer: &impl PollTimer,
) -> PipeChecks {
    let pipe = plan.pipe;

    let scanline = match sample_scanline(regs, pipe.pipedsl(), timer) {
        Ok(samples) => {
            // The verdict is on the *lines*.  Two samples with the same line
            // and different timestamps are a stopped counter, not a slow one:
            // the timestamps are the measurement, never the thing measured.
            if samples.windows(2).any(|pair| pair[0].line != pair[1].line) {
                ScanlineCheck::Scanning { samples }
            } else {
                ScanlineCheck::NotScanning { samples }
            }
        }
        Err(register) => ScanlineCheck::Unreadable { register },
    };

    let surface = surface_check(regs, plan, timer);

    let underrun = match regs.read(pipe.pipestat()) {
        Some(stat) if stat & PIPE_FIFO_UNDERRUN_STATUS != 0 => UnderrunCheck::Underrun {
            stat,
            watermark: plan.watermark.level_zero_value(),
            ddb: plan.ddb.register_value(),
        },
        Some(stat) => UnderrunCheck::Clear { stat },
        None => UnderrunCheck::Unreadable {
            register: pipe.pipestat().name(),
        },
    };

    PipeChecks {
        pipe,
        scanline,
        surface,
        underrun,
    }
}

/// Reference section 11 phase 6.1, 6.2 and 6.4 against one pipe, on the
/// machine's own clock.
///
/// This is the form the boot path calls; [`prove`] is the same three checks
/// with the clock supplied, which is what a host test calls so that the
/// scanline interval costs no wall time.
pub(crate) fn prove_at_boot(regs: &impl Registers, plan: &PipeProgram) -> PipeChecks {
    prove(regs, plan, &MonotonicTimer)
}

/// How many frame times [`prove`] waits for the plane's arm to latch.
///
/// Two, so that an arm written just after one vblank is still seen by the next
/// one: the wait bounds how long a *failed* check takes, it is not a claim
/// about how long a latch takes.  The frame time comes from the mode
/// ([`frame_micros`]), so the bound follows the mode rather than a constant
/// that is wrong at 4K.
pub(crate) const SURFACE_LATCH_FRAMES: u64 = 2;

/// How many times one surface poll may call [`PollTimer::pause`] before giving
/// up on the clock.
///
/// The same contract as [`SCANLINE_PAUSE_BUDGET`] and a larger number for a
/// larger wait: the real timer's pause is two microseconds and two frame times
/// is 33 ms at 1080p60, which is about 16 500 pauses.
const SURFACE_PAUSE_BUDGET: u32 = 32_768;

/// How long one frame takes at the timing a mode carries, in microseconds.
///
/// `Mode::clock_khz` is pixels per millisecond, so a frame of
/// `htotal * vtotal` pixels takes `htotal * vtotal * 1000 / clock_khz`
/// microseconds -- 16 666 for 1920x1080@60 (2200 * 1125 / 148 500).  A mode
/// whose clock is zero cannot give one; the caller gets a zero deadline and
/// reads `PLANE_SURFLIVE` once, which is the honest answer for a timing that
/// says nothing about its own rate.
fn frame_micros(mode: &Mode) -> u64 {
    let pixels = u64::from(mode.htotal) * u64::from(mode.vtotal);
    let khz = u64::from(mode.clock_khz);
    if khz == 0 {
        return 0;
    }
    pixels.saturating_mul(1_000) / khz
}

/// Section 11 phase 6.2: poll `PLANE_SURFLIVE` until the arm latches, or until
/// [`SURFACE_LATCH_FRAMES`] frame times have passed.
///
/// **Why a poll rather than one read.**  The arm is a separate step that runs
/// after `TRANSCONF` (see [`arm`]), so the transfer from `PLANE_SURF` to the
/// live register happens at the pipe's next vblank.  A single eager read would
/// race that: it would report a plane that is about to arm as one that never
/// did, on the boot where the arm landed microseconds before the read.
///
/// The comparison is on the address field only.  `PLANE_SURF` also carries bit
/// 2 as a decrypt flag (§5.4), which this bring-up writes as zero; a mask that
/// ignored it would call a decrypt-enabled read-back a mismatch, and one that
/// included it would call a decrypt flag a wrong address.
fn surface_check(
    regs: &impl Registers,
    plan: &PipeProgram,
    timer: &impl PollTimer,
) -> SurfaceCheck {
    let wrote = plan.plane.surf;
    let register = plan.pipe.plane_surflive();
    let started = timer.now_micros();
    let deadline =
        started.saturating_add(SURFACE_LATCH_FRAMES.saturating_mul(frame_micros(&plan.mode)));
    let mut pauses = 0;
    loop {
        let Some(live) = regs.read(register) else {
            return SurfaceCheck::Unreadable {
                register: register.name(),
            };
        };
        if live & PLANE_SURF_ADDRESS_MASK == wrote & PLANE_SURF_ADDRESS_MASK {
            return SurfaceCheck::Armed { live, wrote };
        }
        let now = timer.now_micros();
        if now >= deadline || pauses >= SURFACE_PAUSE_BUDGET {
            // Zero means the arm has not been latched; anything else means the
            // plane is scanning something that is not what was written, which
            // no amount of waiting fixes.
            return if live == 0 {
                SurfaceCheck::NotLatched {
                    wrote,
                    waited_micros: now.saturating_sub(started),
                }
            } else {
                SurfaceCheck::WrongAddress { live, wrote }
            };
        }
        timer.pause();
        pauses += 1;
    }
}

/// Read `PIPEDSL` [`SCANLINE_SAMPLES`] times, [`SCANLINE_INTERVAL_MICROS`]
/// apart.///
/// Only `LINE[19:0]` is kept: the register's other bits are reserved, and a
/// changing reserved bit is not a scanning pipe.  Each sample keeps the timer's
/// reading as well, taken immediately before the register read, because the
/// line rate section 12.3 asks for is the delta between two of these and a
/// timestamp thrown away is an observation thrown away.
fn sample_scanline(
    regs: &impl Registers,
    register: Register,
    timer: &impl PollTimer,
) -> Result<[ScanlineSample; SCANLINE_SAMPLES], &'static str> {
    let mut samples = [ScanlineSample { line: 0, micros: 0 }; SCANLINE_SAMPLES];
    for (index, sample) in samples.iter_mut().enumerate() {
        if index > 0 {
            wait_micros(timer, SCANLINE_INTERVAL_MICROS);
        }
        // Stamped before the read, so the stamp is never later than the value
        // it belongs to; the read itself is microseconds of MMIO and the
        // interval is a millisecond.
        let micros = timer.now_micros();
        let Some(value) = regs.read(register) else {
            return Err(register.name());
        };
        *sample = ScanlineSample {
            line: value & PIPEDSL_LINE_MASK,
            micros,
        };
    }
    Ok(samples)
}

/// Let `micros` microseconds pass, as far as the timer can tell.
///
/// Bounded twice: by the clock, which is what makes it a wait, and by
/// [`SCANLINE_PAUSE_BUDGET`] pauses, which is what keeps a timer that does not
/// advance its own clock -- the one way a [`PollTimer`] can be wrong -- from
/// hanging the boot.
fn wait_micros(timer: &impl PollTimer, micros: u64) {
    let deadline = timer.now_micros().saturating_add(micros);
    let mut pauses = 0;
    while timer.now_micros() < deadline && pauses < SCANLINE_PAUSE_BUDGET {
        timer.pause();
        pauses += 1;
    }
}
