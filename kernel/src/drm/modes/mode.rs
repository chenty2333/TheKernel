//! The timing shape a display engine is programmed with.
//!
//! Every quantity a pipe needs is explicit here: pixel clock, the four
//! horizontal edges, the four vertical edges, sync polarities, and the
//! attribute flags.  Nothing is derived from a "refresh rate" or a resolution
//! at program time, because the panel, not the kernel, decides what a valid
//! timing is.
//!
//! Vertical counts are **frame** lines even for interlaced timings, so
//! `vtotal` for 1080i is 1125, not 562.  [`Mode::refresh_millihz`] doubles the
//! rate for interlaced modes, which makes it report the *field* rate: that is
//! the number sinks and CTA-861 use to name a 1080i format.

use core::fmt;

/// Attribute flags that are not implied by the edge timings themselves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ModeFlags(u8);

impl ModeFlags {
    /// Progressive, single-clocked.
    pub const NONE: ModeFlags = ModeFlags(0);
    /// Fields alternate; vertical counts are frame lines.
    pub const INTERLACE: ModeFlags = ModeFlags(1 << 0);
    /// The encoder emits each pixel twice, so the pipe runs at twice the pixel
    /// clock this mode records.  No published timing table needs it, because
    /// tables that describe pixel-repeated formats already multiply the clock
    /// (CTA-861 VIC 21, for example, is listed as 1440x576 at 27.000 MHz);
    /// [`Mode::with_doubled_clock`] exists for encoders that must be told.
    pub const DOUBLE_CLOCK: ModeFlags = ModeFlags(1 << 1);

    /// True when every flag in `other` is set in `self`.
    pub const fn contains(self, other: ModeFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn union(self, other: ModeFlags) -> ModeFlags {
        ModeFlags(self.0 | other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl fmt::Debug for ModeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("-");
        }
        let mut first = true;
        for (flag, name) in [
            (ModeFlags::INTERLACE, "interlace"),
            (ModeFlags::DOUBLE_CLOCK, "double-clock"),
        ] {
            if !self.contains(flag) {
                continue;
            }
            if !first {
                f.write_str("|")?;
            }
            f.write_str(name)?;
            first = false;
        }
        Ok(())
    }
}

/// Where a mode's numbers came from.
///
/// Provenance is not decoration: a pixel clock that is off by one percent is a
/// blank screen, so a mode that was computed rather than read out of a table
/// must be distinguishable from one the sink pinned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimingSource {
    /// A VESA DMT 1.13 entry, by DMT code.
    Dmt(u8),
    /// A CTA-861 video identification code.
    CtaVic(u8),
    /// Generated with the VESA CVT formulas; the flag selects reduced
    /// blanking (CVT-RB).
    Cvt { reduced_blanking: bool },
    /// A detailed timing descriptor in the base block.
    EdidDtd { index: u8 },
    /// A detailed timing descriptor inside a CTA-861 extension block.
    CtaDtd { index: u8 },
    /// A timing this kernel ships because the sink advertised nothing usable.
    Builtin,
    /// The timing the firmware had already programmed.
    Firmware,
}

/// A complete display timing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    /// Pixel clock in kHz.  Tables publish 10 kHz units; kHz keeps the extra
    /// digit CVT needs (its finest step is 250 kHz, RBv2's is 1 kHz).
    pub clock_khz: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    /// Frame lines, also for interlaced timings.
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub hsync_positive: bool,
    pub vsync_positive: bool,
    pub flags: ModeFlags,
    pub source: TimingSource,
}

impl Mode {
    /// Builds a mode from the parameterisation every published timing table
    /// uses: active pixels/lines plus blank, front porch and sync width.
    ///
    /// The derived edges are `sync_start = active + front_porch` and
    /// `total = active + blank`.  A published table that also lists a border
    /// must fold it into the blanking before calling this (see
    /// [`super::dmt`]).
    #[allow(clippy::too_many_arguments)]
    pub const fn from_blanking(
        clock_khz: u32,
        hactive: u16,
        hblank: u16,
        hfront: u16,
        hsync: u16,
        vactive: u16,
        vblank: u16,
        vfront: u16,
        vsync: u16,
        flags: ModeFlags,
        source: TimingSource,
    ) -> Mode {
        Mode {
            clock_khz,
            hdisplay: hactive,
            hsync_start: hactive + hfront,
            hsync_end: hactive + hfront + hsync,
            htotal: hactive + hblank,
            vdisplay: vactive,
            vsync_start: vactive + vfront,
            vsync_end: vactive + vfront + vsync,
            vtotal: vactive + vblank,
            // Published tables that carry no polarity are programmed with
            // positive sync; CTA-861 entries and detailed timing descriptors
            // always carry theirs explicitly.
            hsync_positive: true,
            vsync_positive: true,
            flags,
            source,
        }
    }

    /// Builds a mode from explicit edges, for tables that publish totals
    /// directly (interlaced CTA-861 formats whose frame total is not the sum
    /// of the field blanking).
    #[allow(clippy::too_many_arguments)]
    pub const fn from_fields(
        clock_khz: u32,
        hdisplay: u16,
        hsync_start: u16,
        hsync_end: u16,
        htotal: u16,
        vdisplay: u16,
        vsync_start: u16,
        vsync_end: u16,
        vtotal: u16,
        flags: ModeFlags,
        source: TimingSource,
    ) -> Mode {
        Mode {
            clock_khz,
            hdisplay,
            hsync_start,
            hsync_end,
            htotal,
            vdisplay,
            vsync_start,
            vsync_end,
            vtotal,
            hsync_positive: true,
            vsync_positive: true,
            flags,
            source,
        }
    }

    pub const fn with_polarity(mut self, hsync_positive: bool, vsync_positive: bool) -> Mode {
        self.hsync_positive = hsync_positive;
        self.vsync_positive = vsync_positive;
        self
    }

    pub const fn with_flags(mut self, flags: ModeFlags) -> Mode {
        self.flags = flags;
        self
    }

    pub const fn with_source(mut self, source: TimingSource) -> Mode {
        self.source = source;
        self
    }

    /// Returns a copy whose pixels are repeated by the encoder, i.e. the pipe
    /// runs at twice this mode's clock.  See [`ModeFlags::DOUBLE_CLOCK`].
    pub const fn with_doubled_clock(mut self) -> Mode {
        self.clock_khz *= 2;
        self.flags = self.flags.union(ModeFlags::DOUBLE_CLOCK);
        self
    }

    pub const fn is_interlaced(&self) -> bool {
        self.flags.contains(ModeFlags::INTERLACE)
    }

    pub const fn hblank(&self) -> u16 {
        self.htotal - self.hdisplay
    }

    pub const fn vblank(&self) -> u16 {
        self.vtotal - self.vdisplay
    }

    pub const fn hsync_len(&self) -> u16 {
        self.hsync_end - self.hsync_start
    }

    pub const fn vsync_len(&self) -> u16 {
        self.vsync_end - self.vsync_start
    }

    /// Refresh rate in millihertz, rounded to the nearest millihertz.
    ///
    /// Interlaced modes report the field rate (twice the frame rate), matching
    /// how CTA-861 names them.  Zero is returned for a degenerate timing whose
    /// totals are zero; the parser rejects those, so this cannot be reached
    /// from an EDID.
    pub fn refresh_millihz(&self) -> u32 {
        if self.htotal == 0 || self.vtotal == 0 {
            return 0;
        }
        let lines = u64::from(self.htotal) * u64::from(self.vtotal);
        let mut numerator = u64::from(self.clock_khz) * 1_000_000;
        if self.is_interlaced() {
            numerator *= 2;
        }
        ((numerator + lines / 2) / lines) as u32
    }

    /// Refresh rate rounded to whole hertz, for matching against the integer
    /// refresh field of an EDID standard timing descriptor.
    pub fn refresh_hz_rounded(&self) -> u32 {
        (self.refresh_millihz() + 500) / 1000
    }

    pub fn active_area(&self) -> u32 {
        u32::from(self.hdisplay) * u32::from(self.vdisplay)
    }

    /// True when two modes program identical timings, regardless of where they
    /// were found or what attributes carry no timing meaning.
    pub fn same_timing(&self, other: &Mode) -> bool {
        self.clock_khz == other.clock_khz
            && self.hdisplay == other.hdisplay
            && self.hsync_start == other.hsync_start
            && self.hsync_end == other.hsync_end
            && self.htotal == other.htotal
            && self.vdisplay == other.vdisplay
            && self.vsync_start == other.vsync_start
            && self.vsync_end == other.vsync_end
            && self.vtotal == other.vtotal
            && self.is_interlaced() == other.is_interlaced()
    }

    /// Structural sanity: every edge ordered, totals non-zero.  A mode that
    /// fails this must never reach a display engine.
    pub fn is_well_formed(&self) -> bool {
        self.clock_khz > 0
            && self.hdisplay > 0
            && self.vdisplay > 0
            && self.hdisplay <= self.hsync_start
            && self.hsync_start <= self.hsync_end
            && self.hsync_end <= self.htotal
            && self.vdisplay <= self.vsync_start
            && self.vsync_start <= self.vsync_end
            && self.vsync_end <= self.vtotal
    }
}

impl fmt::Debug for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Mode {
    /// A mode worth putting in a boot log: everything needed to spot a wrong
    /// clock or an inverted polarity by eye.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{}{}@{}.{:03} {}.{:03}MHz h {} {} {} v {} {} {} {}{} {}",
            self.hdisplay,
            self.vdisplay,
            if self.is_interlaced() { "i" } else { "p" },
            self.refresh_millihz() / 1000,
            self.refresh_millihz() % 1000,
            self.clock_khz / 1000,
            self.clock_khz % 1000,
            self.hsync_start,
            self.hsync_end,
            self.htotal,
            self.vsync_start,
            self.vsync_end,
            self.vtotal,
            if self.hsync_positive { "+H" } else { "-H" },
            if self.vsync_positive { "+V" } else { "-V" },
            self.source,
        )
    }
}

impl fmt::Display for TimingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimingSource::Dmt(code) => write!(f, "dmt:{code:#04x}"),
            TimingSource::CtaVic(vic) => write!(f, "vic:{vic}"),
            TimingSource::Cvt { reduced_blanking } => {
                if *reduced_blanking {
                    f.write_str("cvt-rb")
                } else {
                    f.write_str("cvt")
                }
            }
            TimingSource::EdidDtd { index } => write!(f, "dtd:{index}"),
            TimingSource::CtaDtd { index } => write!(f, "cta-dtd:{index}"),
            TimingSource::Builtin => f.write_str("builtin"),
            TimingSource::Firmware => f.write_str("firmware"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    fn sample() -> Mode {
        Mode::from_blanking(
            148_500,
            1920,
            280,
            88,
            44,
            1080,
            45,
            4,
            5,
            ModeFlags::NONE,
            TimingSource::Dmt(0x52),
        )
        .with_polarity(true, true)
    }

    #[test]
    fn derived_edges_follow_the_table_parameterisation() {
        let mode = sample();
        assert_eq!(mode.hsync_start, 2008);
        assert_eq!(mode.hsync_end, 2052);
        assert_eq!(mode.htotal, 2200);
        assert_eq!(mode.vsync_start, 1084);
        assert_eq!(mode.vsync_end, 1089);
        assert_eq!(mode.vtotal, 1125);
        assert_eq!(mode.hblank(), 280);
        assert_eq!(mode.vblank(), 45);
        assert_eq!(mode.hsync_len(), 44);
        assert_eq!(mode.vsync_len(), 5);
    }

    #[test]
    fn refresh_is_computed_from_the_programmed_timing() {
        let mode = sample();
        assert_eq!(mode.refresh_millihz(), 60_000);
        // A one-percent clock error moves the rate by one percent: this is the
        // failure mode a blank screen comes from.
        let wrong = Mode {
            clock_khz: 147_000,
            ..mode
        };
        assert_eq!(wrong.refresh_millihz(), 59_394);
        assert_ne!(wrong.refresh_millihz(), mode.refresh_millihz());
        assert_eq!(mode.refresh_hz_rounded(), 60);
    }

    #[test]
    fn interlaced_refresh_reports_the_field_rate() {
        let mode = Mode::from_blanking(
            74_250,
            1920,
            280,
            88,
            44,
            1080,
            45,
            4,
            5,
            ModeFlags::INTERLACE,
            TimingSource::CtaVic(5),
        );
        // 74.25MHz / (2200 * 1125) = 30.0 frames/s, reported as 60 fields/s.
        assert_eq!(mode.refresh_millihz(), 60_000);
        assert!(mode.is_interlaced());
    }

    #[test]
    fn degenerate_timing_has_no_refresh_rate_and_is_not_well_formed() {
        let degenerate = Mode {
            htotal: 0,
            ..sample()
        };
        assert_eq!(degenerate.refresh_millihz(), 0);
        assert!(!degenerate.is_well_formed());
        assert!(sample().is_well_formed());
    }

    #[test]
    fn doubled_clock_mode_records_the_pipe_clock() {
        let doubled = sample().with_doubled_clock();
        assert_eq!(doubled.clock_khz, 297_000);
        assert_eq!(doubled.refresh_millihz(), 120_000);
        assert!(doubled.flags.contains(ModeFlags::DOUBLE_CLOCK));
        assert!(!sample().flags.contains(ModeFlags::DOUBLE_CLOCK));
        assert_eq!(
            format!("{:?}", ModeFlags::INTERLACE.union(ModeFlags::DOUBLE_CLOCK)),
            "interlace|double-clock"
        );
    }

    #[test]
    fn same_timing_ignores_provenance_but_not_the_blanking() {
        let mode = sample();
        let renamed = mode.with_source(TimingSource::CtaVic(16));
        assert!(mode.same_timing(&renamed));
        let stretched = Mode {
            htotal: 2201,
            ..mode
        };
        assert!(!mode.same_timing(&stretched));
        let interlaced = mode.with_flags(ModeFlags::INTERLACE);
        assert!(!mode.same_timing(&interlaced));
    }

    #[test]
    fn display_reports_the_numbers_a_blank_screen_is_diagnosed_from() {
        let text = format!("{}", sample());
        assert!(text.starts_with("1920x1080p@60.000 148.500MHz"), "{text}");
        assert!(text.ends_with("+H+V dmt:0x52"), "{text}");
    }
}
