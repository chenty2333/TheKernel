//! A display mode as the transcoder timing registers the hardware wants.
//!
//! Six registers on a Gen12 transcoder describe one timing -- `HTOTAL`,
//! `HBLANK`, `HSYNC`, `VTOTAL`, `VBLANK`, `VSYNC` -- and a seventh, `PIPESRC`,
//! describes how much of the pipe's source is active.  On this platform an
//! eighth value goes with them: display version 13 stopped reading
//! `TRANS_VBLANK`'s `VBLANK_START` field, and `TRANS_SET_CONTEXT_LATENCY`
//! replaces it.  This module turns a [`Mode`] into all eight values.  It
//! touches no hardware: the register writes belong to the modeset workstream,
//! which consumes this result.
//!
//! # The one thing this module exists to get right
//!
//! **Every one of those fields stores `value − 1`.**  All fourteen halves of
//! the six registers, and both halves of `PIPESRC`.  This is the classic
//! off-by-one of Intel display bring-up, and it is a nasty one because it does
//! not produce a black screen: it produces an image that rolls or sits
//! off-centre, which looks like a sync or a memory problem and sends the reader
//! somewhere else entirely.  The reference document says so twice -- §6.1
//! ("every timing field is `value − 1`") and §5.3 ("**All six registers store
//! `value − 1` in both halves.** This is the classic off-by-one that makes a
//! 'nothing on screen' bring-up") -- and §11 phase 3.4 repeats it at the point
//! of use.
//!
//! So the conversion happens in exactly one function, [`pack_minus_one`], and
//! there is no other way to build a register value in this module.  The type
//! that leaves this module carries packed register values and can hand back the
//! counts they encode through [`TimingRegisters::unpack`], which is the
//! inverse; a test checks that every mode in the kernel's DMT and CTA-861
//! tables survives the round trip, so forgetting or doubling the subtraction is
//! a test failure rather than a rolling picture on a machine with no serial
//! port.
//!
//! `TRANS_SET_CONTEXT_LATENCY` is the exception on both sides of that rule: it
//! is not one of the six, it holds a plain line count that is **not**
//! decremented, and it is `0` for every mode in those tables -- see
//! [`set_context_latency`].
//!
//! # Where each value goes, and what is not here
//!
//! The formulas are §6.1 of `docs/design/intel-display-registers.md`, with the
//! ADL+ substitution `[I915]` makes:
//!
//! ```text
//!     TRANS_SET_CONTEXT_LATENCY(T) = vblank_start - vdisplay   /* ADL+ only */
//!     HTOTAL(T)  = ((htotal  - 1) << 16) | (hdisplay - 1)
//!     HBLANK(T)  = ((htotal  - 1) << 16) | (hdisplay - 1)
//!     HSYNC(T)   = ((hsync_end - 1) << 16) | (hsync_start - 1)
//!     VTOTAL(T)  = ((vtotal  - 1) << 16) | (vdisplay - 1)
//!     VBLANK(T)  = ((vtotal  - 1) << 16) | 0                   /* ADL+ only */
//!     VSYNC(T)   = ((vsync_end - 1) << 16) | (vsync_start - 1)
//!     PIPESRC(T) = ((hdisplay - 1) << 16) | (vdisplay - 1)
//! ```
//!
//! The `VBLANK` line is the other half of the same substitution and is the
//! reason the two are in one table: `VBLANK_START` no longer works on ADL+, so
//! writing `vdisplay − 1` into it programs a field the hardware ignores, and
//! the value that used to go there goes into `TRANS_SET_CONTEXT_LATENCY`
//! instead, *undecorated*.  `[I915]` (`display/intel_display.c:2717-2735`):
//!
//! ```text
//!     if (DISPLAY_VER(dev_priv) >= 13) {
//!             intel_de_write(dev_priv, TRANS_SET_CONTEXT_LATENCY(...),
//!                            crtc_vblank_start - crtc_vdisplay);
//!             /* VBLANK_START not used by hw, just clear it ... */
//!             crtc_vblank_start = 1;
//!     }
//! ```
//!
//! after which `TRANS_VBLANK` is written as
//! `VBLANK_START(crtc_vblank_start - 1) | VBLANK_END(crtc_vblank_end - 1)`,
//! whose low half is therefore `0`.  The reference document has neither the
//! register nor the branch -- see `docs/design/intel-pipe.md` §4.
//!
//! Three things are deliberately absent.
//!
//! * **Sync polarity is not here.**  §6.1: "Polarity does **not** go here -- it
//!   goes in `TRANS_DDI_FUNC_CTL` as `TRANS_DDI_PHSYNC` / `TRANS_DDI_PVSYNC`."
//!   A caller reads `mode.hsync_positive` and `mode.vsync_positive` itself.
//! * **The interlace enable bit is not here**, and neither is the vertical sync
//!   shift that goes with it.  Interlaced timings are refused rather than
//!   guessed at; see [`TimingError::InterlaceNotSourced`].
//! * **Register offsets are not here.**  `regs.rs` owns the offset table and
//!   the rule that a register must sit in a band that needs no forcewake, and
//!   adding one there is that module's business.  §5.3 gives the offsets --
//!   base `0x60000` for transcoder A, `+0x00`, `+0x04`, `+0x08` for the
//!   horizontal three and `+0x0c`, `+0x10`, `+0x14` for the vertical three, and
//!   `PIPESRC` at `0x6001c` (§5.2) -- so a caller that has them from `regs.rs`
//!   can write these values straight out.
//!
//! # What has not been checked
//!
//! No value from this module has been written to real silicon.  The arithmetic
//! is checked against the reference document's formulas and against the
//! published totals in the kernel's own DMT and CTA-861 tables, on the host;
//! that is strong evidence and it is not a measurement.  §11 phase 3.4's
//! symptom list is the thing to keep in hand when it is first tried on the
//! machine: a rolling or off-centre image means a total is off by one, and a
//! black screen with correct sync means the problem is downstream of here.

use core::fmt;

use crate::drm::modes::Mode;

/// Which timing register a value or an error is about.
///
/// The names are §5.3's, which are also the names i915 uses for `[I915]`
/// `i915_reg.h:1072-1110`.  A name is carried into error messages because
/// "field too large" is useless to a person reading a boot log and "VSYNC low
/// half is zero" is not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimingRegister {
    /// The ADL+ context latency, which replaces `VBLANK`'s start field.
    SetContextLatency,
    /// Total and active pixels.
    Htotal,
    /// Blanking end and start, horizontally.
    Hblank,
    /// Sync end and start, horizontally.
    Hsync,
    /// Total and active lines.
    Vtotal,
    /// Blanking end and start, vertically.
    Vblank,
    /// Sync end and start, vertically.
    Vsync,
    /// Pipe source size: active width and height.
    Pipesrc,
}

impl TimingRegister {
    const fn name(self) -> &'static str {
        match self {
            Self::SetContextLatency => "SET_CONTEXT_LATENCY",
            Self::Htotal => "HTOTAL",
            Self::Hblank => "HBLANK",
            Self::Hsync => "HSYNC",
            Self::Vtotal => "VTOTAL",
            Self::Vblank => "VBLANK",
            Self::Vsync => "VSYNC",
            Self::Pipesrc => "PIPESRC",
        }
    }
}

/// How many register values [`TimingRegisters::in_write_order`] returns.
///
/// The six timing registers, `PIPESRC`, and `TRANS_SET_CONTEXT_LATENCY`.  It is
/// named here rather than counted in `pipe.rs` so that the capacity of that
/// module's write list and this table cannot drift apart.
pub(crate) const TIMING_REGISTERS: usize = 8;

/// Which half of a register a value goes in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimingHalf {
    /// Bits `[31:16]`.
    High,
    /// Bits `[15:0]`.
    Low,
}

/// Why a mode could not be turned into timing registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimingError {
    /// The mode's own edges are not ordered, or a count is zero.
    ///
    /// [`Mode::is_well_formed`] is the check; a mode that fails it must never
    /// reach a display engine, and this module refuses it rather than packing
    /// nonsense into a register whose only symptom would be a picture that is
    /// wrong in a way nobody can interpret.
    Malformed,
    /// A count that the register stores as `count − 1` was zero.
    ///
    /// Unreachable behind [`TimingError::Malformed`], which rejects a
    /// zero-valued timing first.  It exists because [`pack_minus_one`] is the
    /// single place the subtraction happens and it is written with a checked
    /// subtraction, so there is no arithmetic path in this module that can
    /// silently wrap.
    ZeroCount {
        register: TimingRegister,
        half: TimingHalf,
    },
    /// An interlaced timing was asked for.
    ///
    /// **Not guessed at, and this is the one refusal in this file.**  The six
    /// timing registers would pack the same way, but an interlaced mode cannot
    /// be programmed from them alone and the reference document does not source
    /// the rest:
    ///
    /// * `TRANSCONF`'s interlace field is named but not defined -- §5.2 lists
    ///   `INTERLACE[23:21]` with no values, and i915's Gen12 mask is
    ///   `TRANSCONF_INTERLACE_MASK_HSW = REG_GENMASK(22, 21)` with
    ///   `TRANSCONF_INTERLACE_W_SYNC_SHIFT` written for an interlaced
    ///   progressive/HDMI output (`[I915]` `i915_reg.h:1615-1618`,
    ///   `display/intel_display.c:2963-2971`).  The reference document's field
    ///   position for this generation is therefore also wrong by one bit.
    /// * `TRANS_VSYNCSHIFT` (`+0x28`) matters only for interlaced, which §5.3
    ///   says by saying to leave it at reset "for a progressive RGB mode", and
    ///   it does not give the value.
    /// * `[I915]` `intel_set_transcoder_timings`
    ///   (`display/intel_display.c:2706-2718`) subtracts one more line from
    ///   `VTOTAL` and `VBLANK_END` for interlaced, with the comment "the chip
    ///   adds 2 halflines automatically", and computes `vsyncshift` as
    ///   `crtc_hsync_start - crtc_htotal / 2`.  Following that faithfully needs
    ///   the convention i915's `crtc_vtotal` is in for an interlaced mode --
    ///   frame lines, field lines, or half-lines -- and i915 does not say:
    ///   `drm_mode_set_crtcinfo` is called with `CRTC_STEREO_DOUBLE` and not
    ///   `CRTC_INTERLACE_HALVE_V` (`intel_display.c:4736-4737`), which settles
    ///   that nothing is halved but not what the hardware counts.
    ///
    /// A wrong vertical total on an interlaced mode is a rolling picture, and a
    /// wrong `VSYNCSHIFT` is a picture torn between fields.  Both are exactly
    /// the failure this module's single-subtraction design exists to prevent,
    /// so the honest answer is to refuse until the convention is established
    /// from hardware -- which §12 and §13.4 say is a register dump away: read
    /// `VTOTAL`, `VBLANK` and `TRANS_VSYNCSHIFT` from a firmware-programmed
    /// 1080i mode and diff them against this module's output for the same
    /// timing.
    InterlaceNotSourced,
}

impl fmt::Display for TimingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str(
                "the mode's edges are not ordered or one of its counts is zero, so it is not a \
                 timing any display engine should be given",
            ),
            Self::ZeroCount { register, half } => write!(
                f,
                "{}'s {half:?} half is a count of zero, which the register's `value - 1` \
                 encoding cannot represent",
                register.name()
            ),
            Self::InterlaceNotSourced => f.write_str(
                "interlaced timings are not implemented: the reference sources the six timing \
                 registers but not the TRANSCONF interlace field, the TRANS_VSYNCSHIFT value, or \
                 the extra line i915 removes from VTOTAL and VBLANK_END for interlaced. The six \
                 registers alone cannot program an interlaced mode, and a wrong vertical total \
                 rolls the picture, so this refuses rather than guesses",
            ),
        }
    }
}

/// The transcoder timing register values for one mode.
///
/// Every field is a packed register value, ready to write.  Nothing here is a
/// count: the `value − 1` conversion has already happened, once, and
/// [`Self::unpack`] is how a caller gets the counts back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TimingRegisters {
    set_context_latency: u32,
    htotal: u32,
    hblank: u32,
    hsync: u32,
    vtotal: u32,
    vblank: u32,
    vsync: u32,
    pipesrc: u32,
}

impl TimingRegisters {
    /// `TRANS_SET_CONTEXT_LATENCY`: `vblank_start − vdisplay`, undecorated.
    pub(crate) const fn set_context_latency(self) -> u32 {
        self.set_context_latency
    }

    /// `HTOTAL`: `[31:16]` total pixels, `[15:0]` active pixels.
    pub(crate) const fn htotal(self) -> u32 {
        self.htotal
    }

    /// `HBLANK`: `[31:16]` blanking end, `[15:0]` blanking start.
    pub(crate) const fn hblank(self) -> u32 {
        self.hblank
    }

    /// `HSYNC`: `[31:16]` sync end, `[15:0]` sync start.
    pub(crate) const fn hsync(self) -> u32 {
        self.hsync
    }

    /// `VTOTAL`: `[31:16]` total lines, `[15:0]` active lines.
    pub(crate) const fn vtotal(self) -> u32 {
        self.vtotal
    }

    /// `VBLANK`: `[31:16]` blanking end, `[15:0]` blanking start.
    pub(crate) const fn vblank(self) -> u32 {
        self.vblank
    }

    /// `VSYNC`: `[31:16]` sync end, `[15:0]` sync start.
    pub(crate) const fn vsync(self) -> u32 {
        self.vsync
    }

    /// `PIPESRC`: `[31:16]` active width, `[15:0]` active height.
    pub(crate) const fn pipesrc(self) -> u32 {
        self.pipesrc
    }

    /// Every value this table produces, in the order §5.3 lists the registers,
    /// each with its name.
    ///
    /// The order is the one a register dump is written in, so this is what a
    /// bring-up logs before writing anything -- §11 phase 3.3's "log them
    /// before writing" applies to the PLL and to these alike.
    /// `TRANS_SET_CONTEXT_LATENCY` comes first because that is where `[I915]`
    /// writes it in `intel_set_transcoder_timings`, before the register whose
    /// field it replaces (`display/intel_display.c:2717-2735`); the order
    /// matters to a reader rather than to the hardware, because none of these
    /// registers is double buffered behind a commit.
    pub(crate) const fn in_write_order(self) -> [(TimingRegister, u32); TIMING_REGISTERS] {
        [
            (TimingRegister::SetContextLatency, self.set_context_latency),
            (TimingRegister::Htotal, self.htotal),
            (TimingRegister::Hblank, self.hblank),
            (TimingRegister::Hsync, self.hsync),
            (TimingRegister::Vtotal, self.vtotal),
            (TimingRegister::Vblank, self.vblank),
            (TimingRegister::Vsync, self.vsync),
            (TimingRegister::Pipesrc, self.pipesrc),
        ]
    }
}

/// The two counts a timing register value encodes, as `(high, low)`.
///
/// This is the inverse of [`pack_minus_one`] and exists for two reasons: a
/// bring-up that has read the registers back can check them against the mode it
/// meant to program, and the round trip is how the tests prove that the
/// subtraction happens exactly once.  §6.2 of the reference document's own
/// advice to read back before trusting applies here as much as to the PLL.
pub(crate) const fn unpack(register: u32) -> (u32, u32) {
    (((register >> 16) & 0xffff) + 1, (register & 0xffff) + 1)
}

/// `TRANS_VBLANK`'s low half, `VBLANK_START[15:0]`.
///
/// The field display version 13 stopped reading; `[I915]` clears it and moves
/// the blanking start into `TRANS_SET_CONTEXT_LATENCY`
/// (`display/intel_display.c:2729-2734`).  See [`set_context_latency`].
pub(crate) const VBLANK_START_MASK: u32 = 0xffff;

/// Pack two counts into one register, each stored as `count − 1`.
///
/// **This is the only place in the driver where a timing count is decremented**
/// -- see the module documentation for why that matters.  Both halves are
/// checked, so a zero count is an error rather than a wrap to `0xffff`, which
/// would be a timing 65535 pixels wide and is exactly the kind of value that
/// looks plausible in a register dump.
const fn pack_minus_one(
    high: u16,
    low: u16,
    register: TimingRegister,
) -> Result<u32, TimingError> {
    let Some(high) = high.checked_sub(1) else {
        return Err(TimingError::ZeroCount {
            register,
            half: TimingHalf::High,
        });
    };
    let Some(low) = low.checked_sub(1) else {
        return Err(TimingError::ZeroCount {
            register,
            half: TimingHalf::Low,
        });
    };
    Ok(((high as u32) << 16) | low as u32)
}

/// A mode as the transcoder timing registers it needs.
///
/// The formulas are §6.1, plus the ADL+ substitution `[I915]` makes for
/// `VBLANK_START`; see the module documentation for what is deliberately not
/// here.
///
/// Note that `HTOTAL` and `HBLANK` hold the same value, and that this is not a
/// shortcut.  §5.3 names `HBLANK`'s halves "end" and "start" of the *blanking*
/// interval, and §6.1 writes the packing as the line total and the active width
/// -- which are the blank interval's end and start precisely because blanking
/// runs from the end of the active region to the end of the line.  A mode whose
/// blanking started somewhere other than the end of active would need a
/// different value, and the tables carry no such mode.  `VTOTAL` and `VBLANK`
/// agreed the same way before display version 13; `VBLANK`'s **low** half no
/// longer does, for the reason [`set_context_latency`] gives.
pub(crate) fn timing_registers(mode: &Mode) -> Result<TimingRegisters, TimingError> {
    if mode.is_interlaced() {
        return Err(TimingError::InterlaceNotSourced);
    }
    if !mode.is_well_formed() {
        return Err(TimingError::Malformed);
    }

    Ok(TimingRegisters {
        set_context_latency: set_context_latency(mode),
        htotal: pack_minus_one(mode.htotal, mode.hdisplay, TimingRegister::Htotal)?,
        // Blanking runs to the end of the line, so the blank interval is
        // [hdisplay, htotal) and HBLANK packs the same two counts HTOTAL does.
        hblank: pack_minus_one(mode.htotal, mode.hdisplay, TimingRegister::Hblank)?,
        hsync: pack_minus_one(mode.hsync_end, mode.hsync_start, TimingRegister::Hsync)?,
        vtotal: pack_minus_one(mode.vtotal, mode.vdisplay, TimingRegister::Vtotal)?,
        // `VBLANK_START` is not read on display version 13, so the low half is
        // cleared -- `[I915]`'s `crtc_vblank_start = 1` after the branch above
        // -- and the blanking start lives in `TRANS_SET_CONTEXT_LATENCY`
        // instead.  The high half, the blanking *end*, is still `vtotal` and is
        // still stored minus one.  Zero is written here as a field value and
        // not as a count, which is why it is not `pack_minus_one`'s business.
        vblank: pack_minus_one(mode.vtotal, mode.vdisplay, TimingRegister::Vblank)?
            & !VBLANK_START_MASK,
        vsync: pack_minus_one(mode.vsync_end, mode.vsync_start, TimingRegister::Vsync)?,
        // PIPESRC is width in the high half and height in the low half, which
        // is the opposite pairing from the other registers but the same
        // `value - 1` rule (§6.1, and §5.2's `WIDTH[31:16]`, `HEIGHT[15:0]`).
        pipesrc: pack_minus_one(mode.hdisplay, mode.vdisplay, TimingRegister::Pipesrc)?,
    })
}

/// `TRANS_SET_CONTEXT_LATENCY`'s value: `vblank_start − vdisplay`, undecorated.
///
/// `[I915]` computes exactly this difference for display version 13 and later
/// and writes it in place of `TRANS_VBLANK`'s `VBLANK_START` field, which the
/// hardware stopped reading (`display/intel_display.c:2717-2735`, the write at
/// `:2725-2727`).  The quantity has a second, independent use in the same tree
/// -- ALPM's guard band is `crtc_vtotal - crtc_vdisplay - context_latency`
/// (`display/intel_alpm.c:296-298`) -- which is a second witness that it is
/// the blanking start less the active height rather than a count minus one.
///
/// **It is zero for every mode in this kernel's tables**, and that is a
/// statement about [`Mode`], not about the register: vertical blanking in this
/// kernel runs from the end of the active region to the end of the frame --
/// [`Mode::vblank`] is `vtotal - vdisplay`, and the type carries no separate
/// blanking-start field (`drm/modes/mode.rs:103-115`, `:226-228`) -- so the
/// blanking start is the active height and the difference is zero.  A mode
/// whose blanking started earlier would need this function changed *and*
/// `VBLANK`'s high half is unaffected, because only the start moved.
///
/// It is still a value this module **writes** rather than leaves alone: the
/// register's reset value is not a value this bring-up has any source for.
///
/// Not verified against the source that fills `crtc_vblank_start` in: that is
/// DRM's `drm_mode_set_crtcinfo`, which is not in the cached tree, so "the
/// difference is zero" is argued from this kernel's own mode type.  Recorded
/// as such in `docs/design/intel-pipe.md`.
const fn set_context_latency(mode: &Mode) -> u32 {
    // The blanking start, in i915's terms, is the active height: see above.
    // The subtraction is written out rather than folded to `0` so that the two
    // counts it is made of stay visible.
    let vblank_start = mode.vdisplay;
    (vblank_start - mode.vdisplay) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::modes::{CTA_VIC_TIMINGS, DMT_TIMINGS, MAX_VIC, ModeFlags, TimingSource};

    /// The DMT row with this code.
    ///
    /// The `modes` module keeps its `dmt` and `vic` submodules private and
    /// re-exports only the tables, so a lookup is a search here rather than a
    /// call to its `by_code`.  That is deliberate: reaching into another
    /// workstream's module to add an export is a change to their file, and this
    /// test needs nothing their public surface does not already carry.
    fn dmt(code: u8) -> Mode {
        DMT_TIMINGS
            .iter()
            .find(|entry| entry.code == code)
            .unwrap_or_else(|| panic!("DMT {code:#04x} is not in the table"))
            .mode
    }

    /// The CTA-861 row with this video identification code.
    fn vic(code: u8) -> Mode {
        CTA_VIC_TIMINGS
            .iter()
            .find(|entry| entry.vic == code)
            .unwrap_or_else(|| panic!("VIC {code} is not in the table"))
            .mode
    }

    /// CTA-861 VIC 16 as the kernel's own table carries it: 1920x1080@60 over
    /// HDMI, 148.5 MHz, htotal 2200, vtotal 1125.  §6.3 uses the same mode as
    /// its worked example, and §11 phase 3.1 names it as the preferred mode for
    /// a first light-up.  VESA DMT 0x52 is the same timing published by the
    /// other table, and both are used here on purpose.
    fn vic16() -> Mode {
        vic(16)
    }

    // -- the published modelines ---------------------------------------------

    /// Every half of `HTOTAL` is the count minus one, and the totals are the
    /// ones the tables publish.
    #[test]
    fn cta_vic_16_1080p60_packs_the_published_totals() {
        let mode = vic16();
        assert_eq!(mode.htotal, 2200);
        assert_eq!(mode.vtotal, 1125);
        assert_eq!(mode.clock_khz, 148_500);

        let timings = timing_registers(&mode).expect("a progressive mode");
        assert_eq!(timings.htotal(), 0x0897_077f, "(2200-1) << 16 | (1920-1)");
        assert_eq!(timings.hblank(), timings.htotal());
        assert_eq!(timings.hsync(), 0x0803_07d7, "(2052-1) << 16 | (2008-1)");
        assert_eq!(timings.vtotal(), 0x0464_0437, "(1125-1) << 16 | (1080-1)");
        assert_eq!(
            timings.vblank(),
            0x0464_0000,
            "(1125-1) << 16 | 0: VBLANK_START is not read on display version 13"
        );
        assert_eq!(
            timings.set_context_latency(),
            0,
            "vblank_start - vdisplay, the substitution ADL+ makes for VBLANK_START"
        );
        assert_eq!(timings.vsync(), 0x0440_043b, "(1089-1) << 16 | (1084-1)");
        assert_eq!(timings.pipesrc(), 0x077f_0437, "(1920-1) << 16 | (1080-1)");
    }

    /// CTA-861 VIC 4: 1280x720@60, 74.25 MHz.
    #[test]
    fn cta_vic_4_720p60_packs_the_published_totals() {
        let mode = vic(4);
        assert_eq!(mode.clock_khz, 74_250);
        assert_eq!(mode.htotal, 1650);
        assert_eq!(mode.vtotal, 750);

        let timings = timing_registers(&mode).expect("a progressive mode");
        assert_eq!(timings.htotal(), 0x0671_04ff);
        assert_eq!(timings.hsync(), 0x0595_056d);
        assert_eq!(timings.vtotal(), 0x02ed_02cf);
        assert_eq!(timings.vsync(), 0x02d9_02d4);
        assert_eq!(timings.pipesrc(), 0x04ff_02cf);
    }

    /// VESA DMT 0x04: 640x480@60, 25.175 MHz.  The oldest timing there is, and
    /// the one whose numbers everybody has memorised.
    #[test]
    fn dmt_0x04_640x480_60_packs_the_published_totals() {
        let mode = dmt(0x04);
        assert_eq!(mode.hdisplay, 640);
        assert_eq!(mode.hsync_start, 656);
        assert_eq!(mode.hsync_end, 752);
        assert_eq!(mode.htotal, 800);
        assert_eq!(mode.vdisplay, 480);
        assert_eq!(mode.vsync_start, 490);
        assert_eq!(mode.vsync_end, 492);
        assert_eq!(mode.vtotal, 525);

        let timings = timing_registers(&mode).expect("a progressive mode");
        assert_eq!(timings.htotal(), 0x031f_027f);
        assert_eq!(timings.hsync(), 0x02ef_028f);
        assert_eq!(timings.vtotal(), 0x020c_01df);
        assert_eq!(timings.vsync(), 0x01eb_01e9);
        assert_eq!(timings.pipesrc(), 0x027f_01df);
    }

    /// VESA DMT 0x53: 1600x900@60 with reduced blanking.  Reduced blanking
    /// changes the totals, not the encoding, so this is the case that proves
    /// the brief's "reduced-blanking modes are in scope" needs no special path
    /// -- and it is checked against a row the table marks as reduced blanking
    /// rather than one whose name merely suggests it.
    #[test]
    fn dmt_0x53_reduced_blanking_packs_like_any_other_mode() {
        let entry = DMT_TIMINGS
            .iter()
            .find(|entry| entry.code == 0x53)
            .expect("DMT 0x53");
        assert!(entry.reduced_blanking, "0x53 is a reduced-blanking row");
        let mode = entry.mode;
        // Reduced blanking is visible in the totals: a 1600-wide line with
        // only 200 pixels of blank, and a 1-line front porch.
        assert_eq!(mode.htotal, 1800);
        assert_eq!(mode.hblank(), 200);
        assert_eq!(mode.vtotal, 1000);
        assert_eq!(mode.vblank(), 100);
        assert_eq!(mode.vsync_start, 901);

        let timings = timing_registers(&mode).expect("a progressive mode");
        assert_eq!(timings.htotal(), 0x0707_063f);
        assert_eq!(timings.hsync(), 0x06a7_0657);
        assert_eq!(timings.vtotal(), 0x03e7_0383);
        assert_eq!(timings.vsync(), 0x0387_0384);
        assert_eq!(timings.pipesrc(), 0x063f_0383);
    }

    // -- the round trip over every published mode ----------------------------

    /// Every mode in the DMT and CTA-861 tables survives the packing round
    /// trip.
    ///
    /// This is the property the module exists for.  If the `− 1` were applied
    /// twice, forgotten, or applied to the wrong half, `unpack` would not
    /// return the counts the mode carried -- and because this walks the whole
    /// of both tables rather than a handful of hand-picked modes, it also
    /// covers blanking, sync and total combinations no one would think to write
    /// out by hand.
    #[test]
    fn every_published_mode_round_trips_through_its_registers() {
        let mut checked = 0;
        let mut interlaced = 0;

        let dmt = DMT_TIMINGS.iter().map(|entry| entry.mode);
        let cta = CTA_VIC_TIMINGS.iter().map(|entry| entry.mode);
        for mode in dmt.chain(cta) {
            assert!(mode.is_well_formed(), "{mode}");
            if mode.is_interlaced() {
                // Refused on purpose; asserted separately.
                assert_eq!(
                    timing_registers(&mode),
                    Err(TimingError::InterlaceNotSourced),
                    "{mode}"
                );
                interlaced += 1;
                continue;
            }

            let timings = timing_registers(&mode).unwrap_or_else(|error| panic!("{mode}: {error}"));
            checked += 1;

            assert_eq!(
                unpack(timings.htotal()),
                (u32::from(mode.htotal), u32::from(mode.hdisplay)),
                "{mode}"
            );
            assert_eq!(
                unpack(timings.hblank()),
                (u32::from(mode.htotal), u32::from(mode.hdisplay)),
                "{mode}"
            );
            assert_eq!(
                unpack(timings.hsync()),
                (u32::from(mode.hsync_end), u32::from(mode.hsync_start)),
                "{mode}"
            );
            assert_eq!(
                unpack(timings.vtotal()),
                (u32::from(mode.vtotal), u32::from(mode.vdisplay)),
                "{mode}"
            );
            // `VBLANK` is no longer `VTOTAL`'s twin: the blanking *end* still
            // round-trips, and the *start* field is cleared because display
            // version 13 does not read it.
            assert_eq!(
                unpack(timings.vblank()),
                (u32::from(mode.vtotal), 1),
                "{mode}: VBLANK's low half is the cleared VBLANK_START field"
            );
            assert_eq!(timings.vblank() & VBLANK_START_MASK, 0, "{mode}");
            assert_eq!(timings.vblank() >> 16, timings.vtotal() >> 16, "{mode}");
            // The blanking start lives in the context latency now, and it is
            // `vblank_start - vdisplay`, zero for every mode in the tables.
            assert_eq!(timings.set_context_latency(), 0, "{mode}");
            assert_eq!(
                unpack(timings.vsync()),
                (u32::from(mode.vsync_end), u32::from(mode.vsync_start)),
                "{mode}"
            );
            assert_eq!(
                unpack(timings.pipesrc()),
                (u32::from(mode.hdisplay), u32::from(mode.vdisplay)),
                "{mode}"
            );
        }

        // Both tables must have been walked, and both cases exercised, or this
        // proves less than it looks like it does.
        assert!(checked > 150, "only {checked} progressive modes round-tripped");
        assert!(interlaced > 0, "no interlaced mode was found to refuse");
        assert_eq!(checked + interlaced, DMT_TIMINGS.len() + CTA_VIC_TIMINGS.len());
    }

    /// The two halves of every register are the two counts the formulas name,
    /// in the order the formulas name them.
    ///
    /// The round trip above would still pass if both halves were swapped
    /// consistently, so this pins the halves against the reference document's
    /// formulas explicitly.
    #[test]
    fn the_high_half_is_the_later_edge_in_every_register() {
        let mode = vic16();
        let timings = timing_registers(&mode).unwrap();
        // §6.1: total/end in [31:16], active/start in [15:0].
        for (register, high, low) in [
            (timings.htotal(), mode.htotal, mode.hdisplay),
            (timings.hblank(), mode.htotal, mode.hdisplay),
            (timings.hsync(), mode.hsync_end, mode.hsync_start),
            (timings.vtotal(), mode.vtotal, mode.vdisplay),
            (timings.vsync(), mode.vsync_end, mode.vsync_start),
            (timings.pipesrc(), mode.hdisplay, mode.vdisplay),
        ] {
            assert_eq!(register >> 16, u32::from(high) - 1);
            assert_eq!(register & 0xffff, u32::from(low) - 1);
        }
        // `VBLANK`'s two halves are the blanking end (minus one, like every
        // other high half) and a *cleared* start field, and the start it used
        // to hold is `TRANS_SET_CONTEXT_LATENCY`'s now.
        assert_eq!(timings.vblank() >> 16, u32::from(mode.vtotal) - 1);
        assert_eq!(timings.vblank() & VBLANK_START_MASK, 0);
        assert_eq!(timings.set_context_latency(), 0);
    }

    // -- the `value - 1` check itself ----------------------------------------

    /// A register built by this module is one less than the count in both
    /// halves -- and specifically *one* less, not the count and not two less.
    ///
    /// This is deliberately written against a mode whose counts are easy to
    /// reason about, because its purpose is to fail loudly if anyone ever
    /// "simplifies" `pack_minus_one`.
    #[test]
    fn every_field_is_exactly_the_count_minus_one() {
        let mode = dmt(0x04);
        let timings = timing_registers(&mode).unwrap();
        for (name, register, high, low) in [
            ("HTOTAL", timings.htotal(), 800u32, 640u32),
            ("HBLANK", timings.hblank(), 800, 640),
            ("HSYNC", timings.hsync(), 752, 656),
            ("VTOTAL", timings.vtotal(), 525, 480),
            ("VBLANK", timings.vblank(), 525, 0),
            ("VSYNC", timings.vsync(), 492, 490),
            ("PIPESRC", timings.pipesrc(), 640, 480),
        ] {
            assert_eq!(register >> 16, high - 1, "{name} high half");
            if name == "VBLANK" {
                // The one low half that is a cleared field rather than a count
                // minus one: display version 13 does not read VBLANK_START.
                assert_eq!(register & VBLANK_START_MASK, 0, "{name} low half");
                assert_eq!(unpack(register), (high, 1), "{name} decode");
                continue;
            }
            assert_eq!(register & 0xffff, low - 1, "{name} low half");
            // And the decode returns the count, not the field value.
            assert_eq!(unpack(register), (high, low), "{name} round trip");
        }
    }

    /// Display version 13 stops reading `TRANS_VBLANK`'s `VBLANK_START` field,
    /// and the count that used to go there moves to
    /// `TRANS_SET_CONTEXT_LATENCY` undecorated.
    ///
    /// `[I915]`'s ADL+ branch (`display/intel_display.c:2717-2735`) writes
    /// `crtc_vblank_start - crtc_vdisplay` to the new register and then sets
    /// `crtc_vblank_start = 1`, so the `TRANS_VBLANK` it writes is
    /// `((vtotal - 1) << 16) | 0`.  The reference document carries neither the
    /// register nor the branch -- its §6.1 formula still says the low half is
    /// `vdisplay - 1` -- so a bring-up built from the document alone programs a
    /// field the hardware ignores and leaves a register it does read at reset.
    #[test]
    fn the_vblank_start_field_is_cleared_and_the_context_latency_replaces_it() {
        // VIC 4: 1280x720@60, vtotal 750.
        let timings = timing_registers(&vic(4)).unwrap();
        assert_eq!(timings.vtotal(), 0x02ed_02cf, "(750-1) << 16 | (720-1)");
        assert_eq!(
            timings.vblank(),
            0x02ed_0000,
            "(750-1) << 16 | 0: the blanking end survives, the start field does not"
        );
        assert_eq!(timings.set_context_latency(), 0);

        // The zero is not a count that was decremented: the register holds the
        // substitution itself.  In this kernel the vertical blanking interval
        // starts at the active height -- `Mode::vblank()` is `vtotal - vdisplay`
        // and there is no other blanking-start field to read -- so
        // `vblank_start - vdisplay` is zero for every mode in the tables.
        let mode = dmt(0x04);
        assert_eq!(mode.vblank(), mode.vtotal - mode.vdisplay);
        let timings = timing_registers(&mode).unwrap();
        assert_eq!(timings.set_context_latency(), 0);
        assert_eq!(timings.vblank(), 0x020c_0000, "(525-1) << 16 | 0");
    }

    /// `pack_minus_one` refuses a zero rather than wrapping to `0xffff`.
    #[test]
    fn a_zero_count_is_an_error_and_not_a_wrap() {
        assert_eq!(
            pack_minus_one(0, 640, TimingRegister::Htotal),
            Err(TimingError::ZeroCount {
                register: TimingRegister::Htotal,
                half: TimingHalf::High,
            })
        );
        assert_eq!(
            pack_minus_one(800, 0, TimingRegister::Htotal),
            Err(TimingError::ZeroCount {
                register: TimingRegister::Htotal,
                half: TimingHalf::Low,
            })
        );
        // The smallest representable count is 1, which packs to 0.
        assert_eq!(pack_minus_one(1, 1, TimingRegister::Htotal), Ok(0));
        // And the largest fits both halves without touching its neighbour.
        assert_eq!(
            pack_minus_one(u16::MAX, u16::MAX, TimingRegister::Htotal),
            Ok(0xfffe_fffe)
        );
    }

    /// A mode whose edges are not ordered is refused before anything is packed.
    #[test]
    fn a_malformed_mode_is_refused() {
        let mut mode = dmt(0x04);
        // A sync that starts after it ends: the two halves would still pack,
        // and the picture would be wrong in a way nobody could read off the
        // register dump.
        mode.hsync_start = 752;
        mode.hsync_end = 656;
        assert!(!mode.is_well_formed());
        assert_eq!(timing_registers(&mode), Err(TimingError::Malformed));

        // A zero total, which is what a zero count would look like.
        let mut mode = dmt(0x04);
        mode.htotal = 0;
        assert_eq!(timing_registers(&mode), Err(TimingError::Malformed));
    }

    // -- interlaced ----------------------------------------------------------

    /// Interlaced is refused, and the refusal names what is missing.
    ///
    /// CTA-861 VIC 5 is 1920x1080i: the same active size as VIC 16 and the same
    /// 148.5 MHz clock, so this also shows that the refusal is about the
    /// interlace and not about the timing being unusual.
    #[test]
    fn an_interlaced_mode_is_refused_with_a_reason() {
        let mode = vic(5);
        assert!(mode.is_interlaced());
        assert!(mode.flags.contains(ModeFlags::INTERLACE));
        assert_eq!(mode.hdisplay, 1920);
        assert_eq!(mode.vdisplay, 1080);
        assert_eq!(mode.vtotal, 1125, "frame lines, not field lines");
        assert!(mode.is_well_formed(), "it is well formed, just unsourceable");

        assert_eq!(
            timing_registers(&mode),
            Err(TimingError::InterlaceNotSourced)
        );
        // The message has to be enough for someone reading a boot log with no
        // other context to know this was deliberate.
        use alloc::string::ToString;
        let text = TimingError::InterlaceNotSourced.to_string();
        for expected in ["TRANSCONF", "TRANS_VSYNCSHIFT", "VTOTAL", "refuses"] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
    }

    // -- shape of the output --------------------------------------------------

    /// The seven values come back with their register names, in the order a
    /// register dump is written.
    #[test]
    fn the_write_order_names_every_register() {
        let timings = timing_registers(&vic16()).unwrap();
        let order = timings.in_write_order();
        assert_eq!(order.len(), TIMING_REGISTERS);
        assert_eq!(order.len(), 8);
        assert_eq!(
            order.map(|(register, _)| register.name()),
            [
                "SET_CONTEXT_LATENCY",
                "HTOTAL",
                "HBLANK",
                "HSYNC",
                "VTOTAL",
                "VBLANK",
                "VSYNC",
                "PIPESRC"
            ]
        );
        // Every value in the list is the value the accessor returns.
        assert_eq!(order[0].1, timings.set_context_latency());
        assert_eq!(order[1].1, timings.htotal());
        assert_eq!(order[7].1, timings.pipesrc());
    }

    /// Polarity is not in these registers, and the mode's polarity does not
    /// change them.  §6.1: it goes in `TRANS_DDI_FUNC_CTL`.
    #[test]
    fn polarity_does_not_reach_the_timing_registers() {
        let mode = vic16();
        let flipped = mode.with_polarity(!mode.hsync_positive, !mode.vsync_positive);
        assert_ne!(mode.hsync_positive, flipped.hsync_positive);
        assert_eq!(
            timing_registers(&mode).unwrap(),
            timing_registers(&flipped).unwrap(),
            "polarity belongs in TRANS_DDI_FUNC_CTL, not here"
        );
    }

    /// A doubled-clock mode has the same edges, and this module does not touch
    /// the clock: the doubled rate is already in `Mode::clock_khz` and the
    /// timing registers do not carry it.
    #[test]
    fn a_doubled_clock_does_not_change_the_timing_registers() {
        let mode = vic16();
        let doubled = mode.with_doubled_clock();
        assert_eq!(doubled.clock_khz, 2 * mode.clock_khz);
        assert!(doubled.flags.contains(ModeFlags::DOUBLE_CLOCK));
        assert_eq!(
            timing_registers(&mode).unwrap(),
            timing_registers(&doubled).unwrap()
        );
    }

    /// The `MAX_VIC` bound and the VIC table agree, so a test that says "every
    /// VIC" is not quietly skipping codes.
    #[test]
    fn the_vic_table_covers_the_codes_it_claims_to() {
        let present = (1..=MAX_VIC)
            .filter(|code| CTA_VIC_TIMINGS.iter().any(|entry| entry.vic == *code))
            .count();
        assert_eq!(present, CTA_VIC_TIMINGS.len());
        assert!(present > 100, "only {present} VICs are defined");
        // And no row names a code outside the range a short video descriptor
        // can carry.
        for entry in CTA_VIC_TIMINGS {
            assert!(entry.vic >= 1 && entry.vic <= MAX_VIC, "VIC {}", entry.vic);
        }
    }

    /// The provenance a mode carries does not change its registers; the timing
    /// is the timing whatever table it came from.  VIC 16 and DMT 0x52 are the
    /// same timing published twice, and they must pack identically.
    #[test]
    fn the_same_timing_from_two_tables_packs_identically() {
        let from_dmt = dmt(0x52);
        let from_cta = vic(16);
        assert!(from_dmt.same_timing(&from_cta), "same timing, two sources");
        assert_eq!(from_dmt.source, TimingSource::Dmt(0x52));
        assert_eq!(from_cta.source, TimingSource::CtaVic(16));
        assert_eq!(
            timing_registers(&from_dmt).unwrap(),
            timing_registers(&from_cta).unwrap()
        );
        // They differ only in polarity, which is exactly what §6.1 says does
        // not live in these registers.
        assert_ne!(from_dmt.hsync_positive, from_cta.hsync_positive);
    }

    /// Errors describe themselves well enough to be read from a boot log.
    #[test]
    fn errors_describe_themselves() {
        use alloc::string::ToString;
        for error in [
            TimingError::Malformed,
            TimingError::ZeroCount {
                register: TimingRegister::Vsync,
                half: TimingHalf::Low,
            },
            TimingError::InterlaceNotSourced,
        ] {
            let text = error.to_string();
            assert!(text.len() > 30, "{error:?} rendered as {text:?}");
            assert!(!text.contains("Err("), "{error:?} rendered as {text:?}");
        }
        // The zero-count message names the register and the half.
        let text = TimingError::ZeroCount {
            register: TimingRegister::Vsync,
            half: TimingHalf::Low,
        }
        .to_string();
        assert!(text.contains("VSYNC"), "{text}");
        assert!(text.contains("Low"), "{text}");
    }
}
