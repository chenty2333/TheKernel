//! Phase 5 of the bring-up order: the output path.
//!
//! Phases 1 to 4 bring up power, find a monitor, choose a mode and program a
//! pipe.  None of that puts a picture on the wire: a **port clock** has to be
//! synthesised, a **DDI** has to be told to use it, its **lanes** have to be
//! powered and equalised, and a **transcoder** has to be connected to that DDI.
//! That is this module.  Its order is `docs/design/intel-display-registers.md`
//! §11 phase 5, with the output half of §8.6 and the routing rules of §6.3:
//!
//! ```text
//! 5.1  DPLL0/DPLL1: power on -> poll POWER_STATE -> dividers -> enable -> poll LOCK
//! 5.2  ICL_DPCLKA_CFGCR0: DDI_CLK_SEL, then DDI_CLK_OFF cleared
//!      in a *separate write*
//! 8.6.5 enable the port's DDI-IO power well, poll its state
//! 5.3  §8.5's voltage-swing / buffer-translation writes, then
//!      PORT_CL_DW10's PWR_DOWN_LN_MASK to power the lanes
//! 5.4  TRANS_CLK_SEL(A): the port's **PHY** on this display version
//! 5.5  TRANS_DDI_FUNC_CTL(A): the **DDI**, HDMI/DVI, 8 bpc, polarity
//! 5.6  TRANSCONF(A) = ENABLE only -- bit 30 is a status, not an enable
//! 5.7  DDI_BUF_CTL: enable, buffer-translation level, width,
//!      then poll IS_IDLE == 0
//! ```
//!
//! Which PLL, which port registers and which `DDI_BUF_CTL` those steps use is
//! the DDI's: combo PHY A is DPLL0's port and combo PHY B is DPLL1's (§6.3),
//! and [`port_registers`] is the one place that mapping lives.  The transcoder
//! side is not per-port: §5.1 gives the PRM's rule that *"Transcoders A-D can
//! connect to any DDI"*, so the transcoder registers are A's and the **values**
//! written into them name the port or the PHY the transcoder is being pointed
//! at.  Those two values are keyed differently, and §11's phase 5 does not
//! record it: §5.4's `(x + 1) << 28` takes the **PHY** on this display version
//! (`[I915]` `display/intel_ddi.c:999-1000`) while §5.5's `(port + 1) << 27`
//! takes the **DDI** on every one (`[I915]` `display/intel_ddi.c:481,488-490`).
//! Both are the same number for the two combo ports this sequence programs,
//! which is exactly why the difference is easy to miss: [`phy_index`] is what
//! the first field is computed from, and `Ddi::index()` the second.
//!
//! # Compute, then write
//!
//! [`OutputProgram::plan`] produces every register value and touches no
//! hardware -- it does not even take a register file.  [`program`] takes that
//! plan and writes it.  The split is the same one `pll.rs` and `timing.rs`
//! make, and it is what lets a caller **log the whole program before the first
//! write**: §11 phase 5.1 asks for `ref`, `(P, Q, K)` and the resulting symbol
//! rate to be printed before programming, and §11 phase 3.3 makes the same
//! demand of the dividers.  [`OutputProgram::render`] is that log, and the
//! numbers in it come from `pll.rs` rather than from arithmetic redone here.
//!
//! One write is the exception, and it is the one register that carries
//! somebody else's bit: `DDI_BUF_CTL` is enabled with a read-modify-write over
//! the fields the plan composes ([`DDI_BUF_CTL_OWNED`]), because
//! `PORT_REVERSAL[16]` belongs to the board.  So the plan holds the fields this
//! sequence owns in that register, not the whole word -- and
//! `docs/design/intel-output.md` §3.9 states which other fields were considered
//! and why `TRANSCONF` is *not* written that way.
//!
//! # The PLL is `pll.rs`'s, and so is its encoding question
//!
//! [`pll::ddi_pll_dividers`] computes the divider set and refuses what it
//! cannot make; this module calls it and never re-derives it.  The one thing
//! the caller has to decide is which of `pll.rs`'s two `CFGCR1` field encodings
//! to write ([`PllFieldEncoding`]), and there is deliberately no default:
//!
//! * §6.3's section "The `PDIV`/`KDIV` encoding -- resolved, and a real trap
//!   alongside it" concludes that on the ADL-N path write and read agree on the
//!   **Gen12 named-constant** values (`P = 2,3,5,7 -> 1,2,4,8`; `K = 1,2,3 ->
//!   1,2,4`), that the Skylake-convention codes are the trap the section warns
//!   about, and it works the target mode through to `CFGCR1 = 0x00000E84`.
//!   That is [`PllFieldEncoding::Named`], and
//!   `the_target_mode_matches_the_reference_worked_example` in the tests pins
//!   the two register values the section prints.
//! * §13.1 item 7 still lists the encoding as a `[GAP]` ("`[TGL12]` and i915's
//!   executed path disagree"), but §6.3 and §13.3 both record it as resolved
//!   and §13.3's own row says "resolved".  `pll.rs`'s module documentation was
//!   written against the earlier draft of §6.3 and still frames it as open.
//!   The contradiction is recorded in `docs/design/intel-output.md`; here it
//!   matters only in that the encoding is a caller decision rather than a
//!   constant, so which one was used is always visible in the log.
//!
//! So a caller that wants the reference's own worked example passes
//! [`PllFieldEncoding::Named`].  A caller that passes
//! [`PllFieldEncoding::Executed`] gets the codes the Skylake-convention path
//! would write -- a different `KDIV` for every `K` -- and the worked example no
//! longer holds.  Nothing here hides that.
//!
//! # What this module cannot source, and what it does about it
//!
//! The port's buffer-translation table is selected from the platform's i915
//! table set (`intel_ddi_buf_trans_get`); an optional firmware dump can override
//! the selected values. The ADL-N HDMI default is entry 6 of
//! `icl_combo_phy_trans_hdmi`. This is a source-faithful platform table, not a
//! claim that every connector's board-level signal integrity has been measured;
//! hardware validation remains necessary.
//!
//! **`DDI_BUF_CTL.PHY_LINK_RATE` has no HDMI encoding in the reference.**
//! §8.6 step 13 puts the field in the enable write and then gives a table of
//! *DisplayPort* link rates (162000, 216000, ... 810000 kHz) with no TMDS
//! entry, so a 148.5 MHz HDMI mode has no sourced code.  This module writes
//! **0** for it unless the caller supplies a code ([`LinkRate::Code`]) and
//! records that choice in the log: 0 is the field's reset value and the
//! document gives no other.  That is an inference, marked as one in
//! `docs/design/intel-output.md`, and §13.4's dump-diff is how to settle it.
//! It is deliberately *not* a refusal: the field tunes the DDI buffer's own
//! equalisation, and an error here would block a bring-up over a
//! signal-integrity margin, while the two refusals below are cases where the
//! sequence cannot proceed at all.
//!
//! Two things are refused rather than approximated:
//!
//! * **A DDI that is not a combo-PHY port.** §8.1: the rear HDMI is on combo
//!   PHY A or B; C and D are Type-C/DKL ports and §8.8 defers that whole path.
//! * **A pixel clock at or above the HDMI scrambling threshold.** §8.4's `[INF]`
//!   note says TMDS at 340 MHz and above needs scrambling and the high TMDS
//!   character rate, and §11 phase 3.1 steers a first light-up to 1080p60
//!   precisely because it needs neither.  The two bits *are* in §8.4, but
//!   enabling source-side scrambling without telling the sink (SCDC over the
//!   DDC bus, which this kernel does not write) gives a picture the monitor
//!   cannot lock.
//!
//! **Either combo PHY can be driven.**  The connector probe decides which one
//! at run time from the EDID read on the GMBUS pin, so a build that could only
//! program PHY A would refuse a machine whose HDMI socket is wired to B.  Both
//! now have every register phase 5 writes: DPLL1's config pair is declared in
//! `regs/dpll.rs` from the `[I915]` header region §6.3 cites for it, and
//! [`port_registers`] supplies the per-PHY set.
//!
//! # What has not been checked
//!
//! **Nothing in this module has run against real hardware.**  Every claim here
//! is a claim about the reference document and about a host test over
//! `regs::mock::MockRegisters`.  No register value has been written to, or read
//! from, a Gen12 display engine.  The mock proves the *sequence* -- order,
//! separate writes, polls, named failures, no write before a refusal -- and
//! says nothing about whether the hardware accepts any of it.  §11 phase 6 is
//! the machine-side check (`PIPEDSL` moving, `PLANE_SURFLIVE` matching,
//! `DDI_BUF_CTL.IS_IDLE` clear, `PIPESTAT` bit 31 clear), and §13.4's register
//! dump of a working configuration is still the highest-value next step.

use alloc::{format, string::String};
use core::fmt;

use super::{
    hpd::Ddi,
    pll::{
        self, ComboPhy, DcoFractionWorkaround, DdiPllDividers, PllError, PllFieldEncoding,
        PllRegisters,
    },
    power::{self, PowerError, Well, WellObservation},
    regs::{self, Register, Registers, ddi, dpll, pipe, port},
};
use crate::drm::modes::Mode;

// ---------------------------------------------------------------------------
// The bits, every one of them cited.
// ---------------------------------------------------------------------------

/// `DPLLn_ENABLE`'s `PLL_ENABLE` bit.  Reference §6.3.
const PLL_ENABLE: u32 = 1 << 31;

/// `DPLLn_ENABLE`'s `LOCK` bit, polled after `PLL_ENABLE` is set.
const PLL_LOCK: u32 = 1 << 30;

/// `DPLLn_ENABLE`'s `PLL_POWER_ENABLE` bit.
const PLL_POWER_ENABLE: u32 = 1 << 27;

/// `DPLLn_ENABLE`'s `PLL_POWER_STATE` bit, polled after `PLL_POWER_ENABLE`.
const PLL_POWER_STATE: u32 = 1 << 26;

/// How long to wait for `PLL_POWER_STATE`, in microseconds.
///
/// §6.3's enable sequence gives "timeout 1ms; spec says 'immediate'" for this
/// bit, and §2.3 records that figures marked in microseconds are the PRM's.
const PLL_POWER_STATE_TIMEOUT_US: u32 = 1_000;

/// How long to wait for `PLL_LOCK`, in microseconds.
///
/// §6.3 quotes `[I915]`'s comment directly: "Timeout is actually 600us".
const PLL_LOCK_TIMEOUT_US: u32 = 600;

/// `TRANS_CLK_SEL`'s port field shift, `TGL_TRANS_CLK_SEL_PORT(x) = (x + 1) <<
/// 28` (`[I915]` `i915_reg.h:4015`).  Reference §6.3 routing step 2 and §11
/// phase 5.4.
///
/// **`x` is the PHY on this display version, not the port.**  i915's
/// `intel_ddi_enable_transcoder_clock` passes `intel_encoder_to_phy(encoder)`
/// on `DISPLAY_VER >= 13` and falls back to `encoder->port` only for version 12
/// (`[I915]` `display/intel_ddi.c:993,996-1004`).  The two are the same number
/// for this machine's two combo ports -- `intel_port_to_phy` is `PHY_A + port -
/// PORT_A` below `PORT_TC1` (`display/intel_display.c:1950-1965`) -- so the
/// reference's port-keyed wording happened to give the right value here, and
/// would not for a port whose PHY is not its own letter.  Reference §6.3
/// routing step 2 and §11 phase 5.4 both print the encoding with `port` as the
/// argument; §6.3's own citation for the region, `[I915]`
/// `display/intel_ddi.c:987-1007`, is where the distinction is.
const TRANS_CLK_SEL_PORT_SHIFT: u32 = 28;

/// `TRANS_DDI_FUNC_CTL`'s mode-select field, `[26:24]`.  Reference §8.4.
#[cfg(test)]
const TRANS_DDI_MODE_SELECT_SHIFT: u32 = 24;

/// `TRANS_DDI_PVSYNC`, bit 17.  Reference §8.4's corrected table.
#[cfg(test)]
const TRANS_DDI_PVSYNC: u32 = 1 << 17;

/// `TRANS_DDI_PHSYNC`, bit 16.  Reference §8.4's corrected table.
#[cfg(test)]
const TRANS_DDI_PHSYNC: u32 = 1 << 16;


/// `TRANSCONF`'s `ENABLE` bit.
///
/// This is the only bit §11 phase 5.6's write may set.  The output bit depth
/// and dithering are **not** written here: §8.4's correction and §11 phase 5.6
/// both put them in `PIPE_MISC` on Gen12, and §8.6 step 12's "| 8bpc" is the
/// pre-correction text.  Progressive is the interlace field reading zero, so no
/// interlace bits are set; §5.2's `[23:21]` and docs/design/intel-pll.md's HSW+
/// `[22:21]` disagree about the mask, and a zero value makes the disagreement
/// moot.
///
/// §11 phase 5.6 also sets `STATE_ENABLE` -- "`(1<<31) | (1<<30)`" -- and that
/// is a **defect in the reference**: bit 30 is the hardware's pipe-running
/// status, not a second enable, and writing it is writing a status bit as if it
/// were a request.  [`TRANSCONF_STATE_ENABLE_STATUS`] carries the citations and
/// `docs/design/intel-output.md` records the defect.
const TRANSCONF_ENABLE: u32 = 1 << 31;

/// `TRANSCONF`'s `STATE_ENABLE` bit, which is a **hardware status** and never a
/// value this sequence writes.
///
/// `[I915]` defines the bit as `TRANSCONF_STATE_ENABLE`, `REG_BIT(30)`,
/// "i965+" (`i915_reg.h:1591`; the same bit is `TRANSCONF_DOUBLE_WIDE` on the
/// pre-i965 parts, which is why the generation matters), and uses it for one
/// thing only: to watch the transcoder stop.  `intel_wait_for_pipe_off` waits
/// for it to read *clear* after a disable
/// (`display/intel_display.c:302-318`, the wait at `:312-313`), and the enable
/// path never sets it: `intel_enable_transcoder` reads `TRANSCONF` (`:459`) and
/// writes the value back with `TRANSCONF_ENABLE` OR'd in (`:474-475`), so bit
/// 30 keeps whatever the hardware had put there.
///
/// Reference §11 phase 5.6 states the write as `ENABLE | STATE_ENABLE`, and an
/// earlier revision of this module followed it -- see the "reference defect"
/// note in `docs/design/intel-output.md`.  §11's *disable* sequence ("Disable
/// `TRANSCONF`; poll for off state") is the reading that agrees with i915, and
/// is where the document gives the bit its status meaning.
const TRANSCONF_STATE_ENABLE_STATUS: u32 = 1 << 30;

/// `DDI_BUF_CTL`'s `ENABLE` bit.  Reference §8.4.
const DDI_BUF_CTL_ENABLE: u32 = 1 << 31;

/// `DDI_BUF_CTL`'s `BUF_TRANS_SELECT[27:24]`, the voltage-swing level index.
const DDI_BUF_CTL_BUF_TRANS_SELECT_SHIFT: u32 = 24;

/// `DDI_BUF_CTL`'s `PHY_LINK_RATE[23:20]`.
const DDI_BUF_CTL_PHY_LINK_RATE_SHIFT: u32 = 20;

/// `DDI_BUF_CTL`'s `IS_IDLE` bit: 1 means the DDI has no clock.
/// `DDI_BUF_CTL.IS_IDLE`, reference section 8.4.
///
/// Phase 5.7 polls it here, and phase 6.3 reads it again a few milliseconds
/// later; the modeset workstream takes this constant from here rather than
/// keeping a second copy of the bit.
pub(crate) const DDI_BUF_CTL_IS_IDLE: u32 = 1 << 7;

/// `DDI_BUF_CTL`'s `A_4_LANES` bit, set when the port drives four lanes.
const DDI_BUF_CTL_A_4_LANES: u32 = 1 << 4;

/// `DDI_BUF_CTL`'s `PORT_WIDTH[3:1]` shift: `(lanes - 1) << 1`.
const DDI_BUF_CTL_PORT_WIDTH_SHIFT: u32 = 1;

/// The bits of `DDI_BUF_CTL` the plan composes, and therefore the only bits the
/// enable write may clear.
///
/// `[I915]` `i915_reg.h:3858-3873`: `ENABLE[31]`, `BUF_TRANS_SELECT[27:24]`,
/// `PHY_LINK_RATE[23:20]`, `PORT_WIDTH[3:1]` and `A_4_LANES[4]`.  Everything
/// else in the register belongs to somebody else, and one of those bits is
/// load-bearing: `PORT_REVERSAL[16]` (`i915_reg.h:3868`) is the **board's**.
/// i915 reads it out of this same register while initialising the encoder and
/// keeps that bit alone for `DISPLAY_VER >= 11`
/// (`display/intel_ddi.c:5115-5120`), ORs in the VBT's own lane-reversal flag
/// (`:5124`), and composes the HDMI enable as
/// `saved_port_bits | DDI_BUF_CTL_ENABLE` (`:3353`, written at `:3375`).  A
/// whole-value write composed from constants would clear a lane order the
/// firmware declared, and the TMDS pairs would come out on the wrong lanes.
/// Reference §8.4 names `PORT_REVERSAL` in its `DDI_BUF_CTL` row and §11 phase
/// 5.7's write does not carry it; `docs/design/intel-output.md` §3.9 records the
/// difference and why this module keeps the bit.
///
/// The mask is the sequence's whole claim on the register, so the
/// read-modify-write in [`program`] also hands back the read-only bits it read
/// (`IS_IDLE[7]`, `DDI_INIT_DISPLAY_DETECTED[0]`), which hardware ignores on
/// write.  i915 read-modify-writes this register the same way where it does not
/// have a saved word -- `intel_de_rmw(..., DDI_BUF_CTL(port), 0,
/// DDI_BUF_CTL_ENABLE)` (`display/icl_dsi.c:514`) and the enable clear at
/// `display/intel_ddi.c:3615-3618`.
const DDI_BUF_CTL_OWNED: u32 = DDI_BUF_CTL_ENABLE
    | (0b1111 << DDI_BUF_CTL_BUF_TRANS_SELECT_SHIFT)
    | (0b1111 << DDI_BUF_CTL_PHY_LINK_RATE_SHIFT)
    | (0b111 << DDI_BUF_CTL_PORT_WIDTH_SHIFT)
    | DDI_BUF_CTL_A_4_LANES;

/// How long to wait for `IS_IDLE` to clear, in microseconds.
///
/// i915's `intel_wait_ddi_buf_active()` polls for 10 ms on display version 10+
/// (as used by ADL-N); preserve that source budget rather than the separate
/// PRM's 500-us HDMI note. §11 phase 5.7 calls this the single best "is my DDI
/// alive" bit and records a real `46d0` timeout under coreboot + EDK2.
const DDI_IDLE_TIMEOUT_US: u32 = 10_000;

/// `PORT_CL_DW5`'s `SUS_CLOCK_CONFIG[1:0]`, written `0b11` by §8.5 step 3.
///
/// The write is a read-modify-write because `CL_POWER_DOWN_ENABLE` is bit 4 of
/// the same register (§8.2) and §8.3 step 7 already set it: a plain write of
/// `0b11` would clear it.
const CL_DW5_SUS_CLOCK_CONFIG_MASK: u32 = 0b11;

/// `PORT_CL_DW10`'s `PWR_DOWN_LN_MASK` shift: the field is bits `[7:4]`
/// (reference §8.2).
///
/// §8.6 step 7 prints the *field* values -- `PWR_UP_ALL_LANES (0x0)`,
/// `PWR_DOWN_LN_3_2 (0xC)`, `PWR_DOWN_LN_3_2_1 (0xE)` -- and §8.2 puts the
/// field at `[7:4]`, so the register write is the field shifted into place.
/// Writing `0xC` at bit 0, which is what taking §8.6's parentheses literally
/// would do, would set bits outside the field.  Recorded in
/// `docs/design/intel-output.md`.
const PWR_DOWN_LN_MASK_SHIFT: u32 = 4;

/// The whole `PWR_DOWN_LN_MASK` field.
const PWR_DOWN_LN_MASK: u32 = 0b1111 << PWR_DOWN_LN_MASK_SHIFT;

/// The pixel clock at or above which HDMI needs scrambling and the high TMDS
/// character rate.
///
/// §8.4's bit table says 340 MHz for both bits; its `[INF]` note gives
/// "approximately 300 MHz" for the same boundary.  The higher, sourced figure
/// gates here because it is the one attached to the two bits, and
/// `docs/design/intel-output.md` records that §8.4 holds both.  See
/// [`OutputError::HdmiScramblingNotImplemented`].
const HDMI_SCRAMBLING_THRESHOLD_KHZ: u32 = 340_000;

/// `BUF_TRANS_SELECT` is four bits (`DDI_BUF_CTL[27:24]`, §8.4).
const BUF_TRANS_SELECT_MAX: u32 = 0b1111;

// ---------------------------------------------------------------------------
// The request: what the caller knows that this module does not.
// ---------------------------------------------------------------------------

/// Which encoder drives the port.
///
/// §8.6 treats HDMI and DVI as one sequence with one difference: the mode
/// select in `TRANS_DDI_FUNC_CTL` (`[26:24]`: **HDMI = 0, DVI = 1**, §8.4's
/// corrected table).  The buffer-translation table differs in name -- §8.5
/// selects `icl_combo_phy_trans_hdmi` for an HDMI port and names no DVI table
/// at all -- and since those values are caller-supplied either way, this type
/// carries the mode select and the name a diagnostic should print.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PortType {
    /// TMDS with the HDMI mode select.
    Hdmi,
    /// TMDS with the DVI mode select; no audio, no infoframes.
    Dvi,
}

impl PortType {
    /// The name a log line or an error uses.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Hdmi => "HDMI",
            Self::Dvi => "DVI",
        }
    }

    /// The i915 buffer-translation table this port type selects, when the
    /// reference names one.
    ///
    /// DVI and HDMI TMDS use the same combo-PHY translation table in i915.
    const fn buffer_translation_table(self) -> Option<&'static str> {
        Some("icl_combo_phy_trans_hdmi")
    }
}

/// How many lanes the port drives.
///
/// §8.6 step 7 gives the lane power-up values for four, two and one lane, and
/// §8.4 gives `PORT_WIDTH = (lanes - 1) << 1` in `DDI_BUF_CTL` (and in
/// `TRANS_DDI_FUNC_CTL` only on display version 14+). Four lanes is the HDMI case; the narrower widths exist
/// because the same sequence is what a two-lane or one-lane port would use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PortWidth {
    /// One lane.
    One,
    /// Two lanes.
    Two,
    /// Four lanes: HDMI, and the case §11's bring-up uses.
    Four,
}

impl PortWidth {
    /// The lane count.
    pub(crate) const fn lanes(self) -> u32 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Four => 4,
        }
    }

    /// The `PORT_WIDTH` field value, `(lanes - 1) << 1`.
    const fn width_field(self) -> u32 {
        (self.lanes() - 1) << 1
    }

    /// The `PWR_DOWN_LN_MASK` *field* value for this width.
    ///
    /// §8.6 step 7: four lanes power all four (`0x0`), two lanes power down
    /// lanes 3 and 2 (`0xC`), one lane powers down 3, 2 and 1 (`0xE`).
    const fn power_down_lanes_field(self) -> u32 {
        super::phy::combo_phy_power_up_lane_mask(self.lanes() as u8, false, false) as u32
    }

    /// The `A_4_LANES` bit for `DDI_BUF_CTL`, set only for four lanes.  §8.6
    /// step 13.
    const fn four_lane_bit(self) -> u32 {
        match self {
            Self::Four => DDI_BUF_CTL_A_4_LANES,
            _ => 0,
        }
    }
}

/// `DDI_BUF_CTL`'s `PHY_LINK_RATE`, and where its value came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkRate {
    /// Write zero, because the reference has no encoding for this rate.
    ///
    /// §8.6 step 13 gives eight DP link rates and no HDMI one; the field's
    /// reset value is zero and that is what goes in.  An inference, recorded
    /// as one.
    NoSourcedEncoding,
    /// A code the caller has a source for -- typically a register dump of a
    /// working configuration (§13.4).
    Code {
        /// The `PHY_LINK_RATE[23:20]` code.
        code: u32,
        /// Where it came from, for the log.
        source: &'static str,
    },
}

impl LinkRate {
    /// The field value to write.
    const fn field(self) -> u32 {
        match self {
            Self::NoSourcedEncoding => 0,
            Self::Code { code, .. } => code,
        }
    }

    /// How a log line describes it.
    fn describe(self) -> String {
        match self {
            Self::NoSourcedEncoding => String::from(
                "PHY_LINK_RATE = 0 (no sourced HDMI encoding in reference section 8.6; this is \
                 the field's reset value and section 13.4's dump-diff settles it)",
            ),
            Self::Code { code, source } => format!("PHY_LINK_RATE = {code:#x}, from {source}"),
        }
    }
}

/// The voltage-swing and pre-emphasis values one swing level needs.
///
/// These are the numbers §8.5's tables hold for DisplayPort, and the numbers
/// §8.5 explicitly did **not** extract for HDMI (`[GAP]`; §13.1 item 12).  They
/// are a caller-supplied field rather than a constant here because there is no
/// sourced value to put in a constant, and because §8.5's own note records that
/// two PRMs disagree about them for the same nominal level: they are
/// board-tuned.
///
/// The *shape* is the reference's, as i915 implements it.  §8.5's write
/// sequence programs `PORT_TX_DW2`, `PORT_TX_DW4`, `PORT_TX_DW5` and
/// `PORT_TX_DW7` from one table entry, with `PORT_TX_DW2`, `PORT_TX_DW4` and
/// `PORT_TX_DW7` written **per lane** and `PORT_TX_DW5` written to the group
/// register.  For `DW4` §8.5 step 2 says so in capitals ("NOT group access --
/// each lane differs"); for the other two the evidence is i915, whose
/// `icl_ddi_combo_vswing_program` runs one read-modify-write per lane for each
/// of `DW2`, `DW4` and `DW7` (`ln = 0..3`, `[I915]` `display/intel_ddi.c:1148-1178`)
/// and reads `PORT_TX_DW5` from lane 0 to write the group
/// (`[I915]` `display/intel_ddi.c:1141-1146`, `:1218-1229`).  `DW5`'s
/// TX-training-enable bit is toggled off and back on around the batch (steps 4
/// and 6) -- that last write is what commits the settings.
///
/// The two `DW5` states are separate fields rather than a value plus a bit
/// because §8.5 names the register's `TX Training Enable` and `Scaling Mode
/// Sel` fields without giving either one's bit position, and a value computed
/// from an unstated position would be a guess dressed as a constant.  A caller
/// with a working dump has both states; that is the whole point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SwingProgram {
    /// The level index, written to `DDI_BUF_CTL.BUF_TRANS_SELECT[27:24]` and
    /// (in i915's table) the index of the entry these values came from.
    pub(crate) level: u8,
    /// `PORT_TX_DW2`, one value per lane, written in lane order 0 to 3.  i915
    /// takes the level per lane (`intel_ddi_level(encoder, crtc_state, ln)`) and
    /// writes the lane instance for each
    /// (`[I915]` `display/intel_ddi.c:1148-1157`); the group instance is a
    /// different address and this sequence does not write it.
    pub(crate) dw2: [u32; 4],
    pub(crate) dw2_mask: u32,
    /// `PORT_TX_DW4`, one value per lane, written in lane order 0 to 3.  §8.5
    /// step 2: the loadgen select differs per lane, so the group register must
    /// not be used.
    pub(crate) dw4: [u32; 4],
    pub(crate) dw4_mask: u32,
    /// `PORT_TX_DW5` with TX training disabled, which is step 4's state.
    pub(crate) dw5_training_disabled: u32,
    pub(crate) dw5_mask: u32,
    /// `PORT_TX_DW5` with the scaling mode set and TX training enabled, which
    /// is step 6's state and the write that triggers the update.
    pub(crate) dw5_training_enabled: u32,
    /// `PORT_TX_DW7`, one value per lane, written in lane order 0 to 3.  i915
    /// writes the lane instances here too
    /// (`[I915]` `display/intel_ddi.c:1171-1178`).
    pub(crate) dw7: [u32; 4],
    pub(crate) dw7_mask: u32,
    /// Where these numbers came from. Printed in the plan log.
    pub(crate) source: &'static str,
}

/// Build the ADL-N combo-PHY default from i915's platform-selected table.
/// The values are written through masks, preserving the unrelated bits in each
/// PHY dword exactly as the i915 RMW sequence does.
fn combo_default_swing(
    port_type: PortType,
    port_clock_khz: u32,
) -> Result<SwingProgram, OutputError> {
    use intel_display::ddi_buf_trans::{
        BufferOutput, BufferPhy, DdiBufferTransEntry, DdiBufferTransRequest,
        intel_ddi_buf_trans_get,
    };
    let output = match port_type {
        PortType::Hdmi => BufferOutput::Hdmi,
        PortType::Dvi => BufferOutput::Dvi,
    };
    let table = intel_ddi_buf_trans_get(DdiBufferTransRequest {
        platform: intel_display::device::Platform::AlderLakeN,
        phy: BufferPhy::Combo,
        output,
        port_clock_khz,
        use_edp_low_vswing: false,
        use_edp_hobl: false,
        tigerlake_uy: false,
    })
    .map_err(|_| OutputError::MissingBufferTranslation {
        port_type,
        table: port_type.buffer_translation_table(),
    })?;
    let level = table
        .hdmi_default_entry
        .ok_or(OutputError::MissingBufferTranslation {
            port_type,
            table: Some(table.name),
        })?;
    let Some(DdiBufferTransEntry::Combo {
        dw2_swing_sel,
        dw7_n_scalar,
        dw4_cursor_coeff,
        dw4_post_cursor_2,
        dw4_post_cursor_1,
    }) = table.entries.get(usize::from(level)).copied()
    else {
        return Err(OutputError::MissingBufferTranslation {
            port_type,
            table: Some(table.name),
        });
    };
    let dw2_mask = (1 << 15) | (0b111 << 11) | 0xff;
    let dw4_mask = (1 << 31) | (0b11_1111 << 12) | (0b11_1111 << 6) | 0b11_1111;
    let dw5_mask =
        (1 << 31) | (1 << 30) | (1 << 29) | (1 << 26) | (1 << 25) | (0b111 << 18) | (0b111 << 3);
    let dw2_value =
        (u32::from(dw2_swing_sel >> 3) << 15) | (u32::from(dw2_swing_sel & 7) << 11) | 0x98;
    let dw4_value = (u32::from(dw4_post_cursor_1) << 12)
        | (u32::from(dw4_post_cursor_2) << 6)
        | u32::from(dw4_cursor_coeff);
    let dw7_value = u32::from(dw7_n_scalar) << 24;
    let dw7_mask = 0x7f00_0000;
    let dw5_value = (2 << 18) | (6 << 3) | (1 << 29);
    Ok(SwingProgram {
        level,
        dw2: [dw2_value; 4],
        dw2_mask,
        dw4: [dw4_value; 4],
        dw4_mask,
        dw5_training_disabled: dw5_value,
        dw5_mask,
        dw5_training_enabled: dw5_value | (1 << 31),
        dw7: [dw7_value; 4],
        dw7_mask,
        source: table.name,
    })
}

/// Everything phase 5 needs told.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OutputRequest {
    /// Existing combo PLL route to use; boot uses the firmware-selected PLL.
    pub(crate) pll_id: u8,
    /// The DDI the monitor is on: `sink.rs` gets this from the GMBUS pin
    /// (`Pin::ddi()`), and §11 phase 2.3 makes that the authority for which
    /// physical port is in use.
    pub(crate) ddi: Ddi,
    /// HDMI or DVI.
    pub(crate) port_type: PortType,
    /// The mode whose pixel clock and sync polarities are programmed.  The
    /// timing registers themselves are not here -- `timing.rs` computes them
    /// and the pipe workstream writes them.
    pub(crate) mode: Mode,
    /// How many lanes the port drives.
    pub(crate) width: PortWidth,
    /// Which `CFGCR1` field encoding to write.  No default: see the module
    /// documentation.
    pub(crate) encoding: PllFieldEncoding,
    /// Optional explicit voltage-swing override. When absent, the platform
    /// DDI buffer-translation table supplies the default level.
    pub(crate) swing: Option<SwingProgram>,
    /// `DDI_BUF_CTL.PHY_LINK_RATE`.  See [`LinkRate`].
    pub(crate) link_rate: LinkRate,
}

impl OutputRequest {
    /// The first-light-up request: HDMI, four lanes. The platform table
    /// supplies its default buffer-translation level unless overridden.
    pub(crate) const fn hdmi(ddi: Ddi, mode: Mode, encoding: PllFieldEncoding) -> Self {
        Self {
            pll_id: ddi.index() as u8,
            ddi,
            port_type: PortType::Hdmi,
            mode,
            width: PortWidth::Four,
            encoding,
            swing: None,
            link_rate: LinkRate::NoSourcedEncoding,
        }
    }

    /// Attach the voltage-swing values the sequence writes.
    pub(crate) const fn with_swing(mut self, swing: SwingProgram) -> Self {
        self.swing = Some(swing);
        self
    }
}

// ---------------------------------------------------------------------------
// The registers one port's sequence writes.
// ---------------------------------------------------------------------------

/// The registers one combo PHY's port sequence writes.
///
/// `Option` for the DPLL config pair although both combo PHYs on `XE_LPD` have
/// one: whether a PHY's config offsets have been sourced is a property of the
/// register table rather than of this sequence, and a port whose PLL config
/// registers are not in it must fail closed via
/// [`OutputError::PllConfigRegisterMissing`] rather than be pointed at a
/// neighbouring address.  Both arms below supply them, so nothing refuses
/// today; the check is what a platform with a third combo PHY would hit.
#[derive(Clone, Copy, Debug)]
struct PortRegisters {
    /// `DPLLn_ENABLE`: power, enable and lock.
    pll_enable: Register,
    /// `DPLLn_CFGCR0`, the DCO integer and fraction.
    pll_cfgcr0: Option<Register>,
    /// `DPLLn_CFGCR1`, the dividers.
    pll_cfgcr1: Option<Register>,
    /// `PORT_CL_DW5`: `SUS_CLOCK_CONFIG`, and `CL_POWER_DOWN_ENABLE` that
    /// phase 1 already set.
    cl_dw5: Register,
    /// `PORT_CL_DW10`: `PWR_DOWN_LN_MASK`.
    cl_dw10: Register,
    /// `PORT_TX_DW2` (**group** instance): the swing select as §8.2's worked
    /// example tabulates it.  The sequence writes the per-lane instances
    /// instead -- see [`TxLaneRegisters`] -- so this entry is the port's
    /// inventory of the register rather than a write target.
    tx_dw2: Register,
    /// `PORT_TX_DW4`, one per lane.  No group instance: §8.5 says group access
    /// must not be used for this register.  The sequence reads these through
    /// [`TxLaneRegisters`], beside the other two per-lane dwords.
    tx_dw4: [Register; 4],
    /// `PORT_TX_DW5` lane 0, the source read for both group writes.
    tx_dw5_lane0: Register,
    /// `PORT_TX_DW5` (group): the training-enable and scaling-mode register,
    /// and the instance both batch writes go to
    /// (`[I915]` `display/intel_ddi.c:1146`, `:1221`, `:1229`).
    tx_dw5: Register,
    /// `PORT_TX_DW7` (**group** instance): the N scalar as §8.2's worked example
    /// tabulates it.  See [`Self::tx_dw2`] -- the sequence writes the per-lane
    /// instances.
    tx_dw7: Register,
    /// `DDI_BUF_CTL` for this port.
    ddi_buf_ctl: Register,
}

/// The registers of the PHY that clocks one of the two combo-PHY ports.
///
/// Offsets are §8.2's (`CL` at `base + 4*dw`, the `TX` group at
/// `base + 0x680 + 4*dw`, the `TX` lane at `base + 0x880 + ln*0x100`), and the
/// declarations are `regs/port.rs`'s and `regs/mod.rs`'s.  The PLL pair is
/// §6.3's table: combo PHY A is on DPLL 0 and combo PHY B on DPLL 1, so B's
/// arm names the `DPLL1_*` registers -- including `DPLL1_CFGCR0`/`DPLL1_CFGCR1`
/// (`0x16428C`/`0x164290`, `[I915]` `i915_reg.h:4302,4317`), which §6.3's own
/// register table omits and its citation to that header supplies.
const fn port_registers(phy: ComboPhy) -> PortRegisters {
    match phy {
        ComboPhy::A => PortRegisters {
            pll_enable: dpll::DPLL0_ENABLE,
            pll_cfgcr0: Some(dpll::DPLL0_CFGCR0),
            pll_cfgcr1: Some(dpll::DPLL0_CFGCR1),
            cl_dw5: regs::COMBO_PHY_A.cl_dw5,
            cl_dw10: port::PORT_CL_DW10_A,
            tx_dw2: port::PORT_TX_DW2_GRP_A,
            tx_dw4: [
                port::PORT_TX_DW4_LN0_A,
                port::PORT_TX_DW4_LN1_A,
                port::PORT_TX_DW4_LN2_A,
                port::PORT_TX_DW4_LN3_A,
            ],
            tx_dw5_lane0: port::PORT_TX_DW5_LN0_A,
            tx_dw5: port::PORT_TX_DW5_GRP_A,
            tx_dw7: port::PORT_TX_DW7_GRP_A,
            ddi_buf_ctl: ddi::DDI_BUF_CTL_A,
        },
        ComboPhy::B => PortRegisters {
            pll_enable: dpll::DPLL1_ENABLE,
            pll_cfgcr0: Some(dpll::DPLL1_CFGCR0),
            pll_cfgcr1: Some(dpll::DPLL1_CFGCR1),
            cl_dw5: regs::COMBO_PHY_B.cl_dw5,
            cl_dw10: port::PORT_CL_DW10_B,
            tx_dw2: port::PORT_TX_DW2_GRP_B,
            tx_dw4: [
                port::PORT_TX_DW4_LN0_B,
                port::PORT_TX_DW4_LN1_B,
                port::PORT_TX_DW4_LN2_B,
                port::PORT_TX_DW4_LN3_B,
            ],
            tx_dw5_lane0: port::PORT_TX_DW5_LN0_B,
            tx_dw5: port::PORT_TX_DW5_GRP_B,
            tx_dw7: port::PORT_TX_DW7_GRP_B,
            ddi_buf_ctl: ddi::DDI_BUF_CTL_B,
        },
    }
}

fn selected_port_registers(phy: ComboPhy, index: u8) -> Result<PortRegisters, OutputError> {
    let mut registers = port_registers(phy);
    let (enable, cfg0, cfg1) = match index {
        0 => (dpll::DPLL0_ENABLE, dpll::DPLL0_CFGCR0, dpll::DPLL0_CFGCR1),
        1 => (dpll::DPLL1_ENABLE, dpll::DPLL1_CFGCR0, dpll::DPLL1_CFGCR1),
        _ => return Err(OutputError::ComboPllIndex { index }),
    };
    registers.pll_enable = enable;
    registers.pll_cfgcr0 = Some(cfg0);
    registers.pll_cfgcr1 = Some(cfg1);
    Ok(registers)
}

/// The three `PORT_TX_*` dwords §8.5's batch writes once per lane.
///
/// A TX dword has three instances -- AUX (`+0x380`), group (`+0x680`) and one
/// per lane (`0x880 + ln*0x100`) -- and they are three addresses, not three
/// names for one (`[I915]` `display/intel_combo_phy_regs.h:96-105`).  i915's
/// DDI voltage-swing sequence writes exactly these three dwords to the lane
/// instances, one read-modify-write per lane in lane order, and leaves the
/// group instances of `DW2` and `DW7` alone
/// (`[I915]` `display/intel_ddi.c:1148-1178`); `DW5` is the exception, read
/// from lane 0 and written to the group on both sides of the batch
/// (`[I915]` `display/intel_ddi.c:1218-1229`), so it has no entry here.
///
/// The registers come from `regs/port.rs` one named constant at a time rather
/// than by arithmetic on a base, so a dump of this machine can be compared with
/// the log by name.
#[derive(Clone, Copy, Debug)]
struct TxLaneRegisters {
    /// `PORT_TX_DW2`, one register per lane, in lane order.
    dw2: [Register; 4],
    /// `PORT_TX_DW4`, one register per lane, in lane order.
    dw4: [Register; 4],
    /// `PORT_TX_DW7`, one register per lane, in lane order.
    dw7: [Register; 4],
}

/// The per-lane TX registers of one combo PHY.
///
/// §8.2's lane rule is `0x880 + ln*0x100` with the dword at `+4*dw`; every
/// offset below is the register table's declaration of that instance, whose own
/// doc comment carries the arithmetic and the citation.
const fn tx_lane_registers(phy: ComboPhy) -> TxLaneRegisters {
    match phy {
        ComboPhy::A => TxLaneRegisters {
            dw2: [
                port::PORT_TX_DW2_LN0_A,
                port::PORT_TX_DW2_LN1_A,
                port::PORT_TX_DW2_LN2_A,
                port::PORT_TX_DW2_LN3_A,
            ],
            dw4: [
                port::PORT_TX_DW4_LN0_A,
                port::PORT_TX_DW4_LN1_A,
                port::PORT_TX_DW4_LN2_A,
                port::PORT_TX_DW4_LN3_A,
            ],
            dw7: [
                port::PORT_TX_DW7_LN0_A,
                port::PORT_TX_DW7_LN1_A,
                port::PORT_TX_DW7_LN2_A,
                port::PORT_TX_DW7_LN3_A,
            ],
        },
        ComboPhy::B => TxLaneRegisters {
            dw2: [
                port::PORT_TX_DW2_LN0_B,
                port::PORT_TX_DW2_LN1_B,
                port::PORT_TX_DW2_LN2_B,
                port::PORT_TX_DW2_LN3_B,
            ],
            dw4: [
                port::PORT_TX_DW4_LN0_B,
                port::PORT_TX_DW4_LN1_B,
                port::PORT_TX_DW4_LN2_B,
                port::PORT_TX_DW4_LN3_B,
            ],
            dw7: [
                port::PORT_TX_DW7_LN0_B,
                port::PORT_TX_DW7_LN1_B,
                port::PORT_TX_DW7_LN2_B,
                port::PORT_TX_DW7_LN3_B,
            ],
        },
    }
}

/// The DDI-IO power well for a combo PHY port (§8.6 step 5).
const fn ddi_io_well(phy: ComboPhy) -> Well {
    match phy {
        ComboPhy::A => power::DDI_IO_A,
        ComboPhy::B => power::DDI_IO_B,
    }
}

/// Which combo PHY a DDI is on, or a refusal.
///
/// Split out because both [`OutputProgram::plan`] and [`program`] check it: the
/// plan refuses a Type-C port when it is built, and the sequence refuses it
/// again before it writes anything, because a plan is data.
fn combo_phy_of(ddi: Ddi) -> Result<ComboPhy, OutputError> {
    match ddi {
        Ddi::A => Ok(ComboPhy::A),
        Ddi::B => Ok(ComboPhy::B),
        other => Err(OutputError::UnsupportedDdi { ddi: other }),
    }
}

/// The `enum phy` index of a combo PHY: what `TRANS_CLK_SEL`'s field carries on
/// this display version.
///
/// `[I915]` `enum phy` is `PHY_A = 0, PHY_B = 1, ...`
/// (`display/intel_display.h:192-204`), and `intel_ddi_enable_transcoder_clock`
/// hands the PHY to `TGL_TRANS_CLK_SEL_PORT` for `DISPLAY_VER >= 13`
/// (`display/intel_ddi.c:993,999-1000`).  The argument type is the point: this
/// value comes from a [`ComboPhy`] and never from a [`Ddi`], because on this
/// platform the two happen to be equal for ports A and B -- `intel_port_to_phy`
/// is `PHY_A + port - PORT_A` below `PORT_TC1` (`display/intel_display.c:1950-1965`)
/// -- and a port whose PHY is not its own letter gets a different number here,
/// not a different register. The translated DDI helper separately builds the
/// `TRANS_DDI_FUNC_CTL` port field from the encoder's DDI identity.
const fn phy_index(phy: ComboPhy) -> u32 {
    match phy {
        ComboPhy::A => 0,
        ComboPhy::B => 1,
    }
}

// ---------------------------------------------------------------------------
// The plan: every register value, computed before anything is written.
// ---------------------------------------------------------------------------

/// Everything phase 5 will write, computed before the first write.
///
/// Produced by [`OutputProgram::plan`], consumed by [`program`].  The fields
/// are `pub(crate)` so that a caller can log them and a test can pin them
/// without going through a register file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OutputProgram {
    pub(crate) pll_id: u8,
    /// The DDI this plan is for.
    pub(crate) ddi: Ddi,
    /// The combo PHY that DDI is on: A ↔ DPLL0, B ↔ DPLL1 (§6.3's table).
    pub(crate) phy: ComboPhy,
    /// HDMI or DVI.
    pub(crate) port_type: PortType,
    /// The lane count.
    pub(crate) width: PortWidth,
    /// The pixel clock the port will run at, in kHz.
    pub(crate) pixel_clock_khz: u32,
    /// The reference frequency the platform straps, in kHz -- `SKL_DSSM`'s
    /// `[31:29]` as [`read_platform_reference_khz`] decodes it.
    pub(crate) platform_ref_khz: u32,
    /// The `CFGCR1` encoding the caller chose.
    pub(crate) encoding: PllFieldEncoding,
    /// Whether the ADL-P/N 38.4 MHz fraction workaround is applied.
    pub(crate) fraction_workaround: DcoFractionWorkaround,
    /// `pll.rs`'s divider set: `(P, Q, K)`, the DCO, the achieved rate.
    pub(crate) dividers: DdiPllDividers,
    /// The two `CFGCR` values, from `pll.rs`.
    pub(crate) pll_registers: PllRegisters,
    /// The DDI-IO power well §8.6 step 5 enables.
    pub(crate) ddi_io_well: Well,
    /// The selected voltage-swing values and per-field write masks.
    pub(crate) swing: SwingProgram,
    /// The `PHY_LINK_RATE` field and its provenance.
    pub(crate) link_rate: LinkRate,
    /// The platform-mapped DPCLKA field and the separate clock-off write.
    pub(crate) ddi_clock_plan: intel_display::ddi::DdiClockPlan,
    /// `TRANS_CLK_SEL(A)`.
    pub(crate) trans_clk_sel: u32,
    /// `TRANS_DDI_FUNC_CTL(A)`.
    pub(crate) trans_ddi_func_ctl: u32,
    /// `TRANSCONF(A)`, the register `regs/pipe.rs` names `PIPECONF_A`.
    pub(crate) transconf: u32,
    /// The fields this sequence owns in `DDI_BUF_CTL` for this port.  Written
    /// as a read-modify-write, so the register keeps the bits the plan does not
    /// name -- the board's `PORT_REVERSAL` among them; see
    /// [`DDI_BUF_CTL_OWNED`].
    pub(crate) ddi_buf_ctl: u32,
}

/// No-register backend for the translated source register-value builder. The
/// supported HDMI/DVI path consumes only the supplied encoder/CRTC state.
struct DdiPolicyIo;

impl intel_display::intel_ddi_full::DdiIo for DdiPolicyIo {
    fn read(&mut self, _reg: u32) -> u32 { 0 }
    fn write(&mut self, _reg: u32, _value: u32) {}
    fn combo_phy_read(&mut self, _phy: u8, _reg: intel_display::intel_ddi_full::ComboPhyRegister) -> u32 { 0 }
    fn combo_phy_write(&mut self, _phy: u8, _reg: intel_display::intel_ddi_full::ComboPhyRegister, _value: u32) {}
    fn combo_phy_rmw(&mut self, _phy: u8, _reg: intel_display::intel_ddi_full::ComboPhyRegister, _clear: u32, _set: u32) {}
    fn mg_phy_rmw(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::MgPhyRegister, _clear: u32, _set: u32) {}
    fn dkl_phy_read(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::DklPhyRegister) -> u32 { 0 }
    fn dkl_phy_write(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::DklPhyRegister, _value: u32) {}
    fn dkl_phy_rmw(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::DklPhyRegister, _clear: u32, _set: u32) {}
    fn mg_dp_mode_read(&mut self, _port: intel_display::intel_ddi_full::Port, _lane: u8) -> u32 { 0 }
    fn mg_dp_mode_write(&mut self, _port: intel_display::intel_ddi_full::Port, _lane: u8, _value: u32) {}
}

fn source_trans_ddi_func_ctl(request: &OutputRequest) -> u32 {
    use intel_display::intel_ddi_full as i915;

    let port = match request.ddi {
        Ddi::A => i915::Port::A,
        Ddi::B => i915::Port::B,
        Ddi::C => i915::Port::C,
        Ddi::D => i915::Port::D,
    };
    let output = match request.port_type {
        PortType::Hdmi => i915::OutputType::Hdmi,
        PortType::Dvi => i915::OutputType::Dvi,
    };
    let encoder = i915::DdiEncoder {
        port,
        output,
        display: i915::Platform { display_ver: 13, alderlake_p: true, ..i915::Platform::default() },
        ..i915::DdiEncoder::default()
    };
    let state = i915::CrtcState {
        pipe: i915::Pipe::A,
        cpu_transcoder: i915::Transcoder::A,
        output,
        port_clock: request.mode.clock_khz,
        lane_count: request.width.lanes() as u8,
        pipe_bpp: 24,
        mode_flags: u32::from(request.mode.hsync_positive) | (u32::from(request.mode.vsync_positive) << 2),
        has_hdmi_sink: request.port_type == PortType::Hdmi,
        ..i915::CrtcState::default()
    };
    i915::intel_ddi_transcoder_func_reg_val_get(&mut DdiPolicyIo, &encoder, &state)
}

/// Kernel adapter for the translated source DDI-buffer enable handshake. It
/// admits exactly the selected combo-PHY buffer register and implements the
/// source's bounded `IS_IDLE` poll without exposing any other MMIO address.
struct DdiBufferEnableIo<'a, R> {
    registers: &'a R,
    ddi_buf_ctl: Register,
    read_failed: bool,
    write_failed: bool,
    timed_out: bool,
}

impl<'a, R: Registers> DdiBufferEnableIo<'a, R> {
    fn new(registers: &'a R, ddi_buf_ctl: Register) -> Self {
        Self { registers, ddi_buf_ctl, read_failed: false, write_failed: false, timed_out: false }
    }
}

impl<R: Registers> intel_display::intel_ddi_full::DdiIo for DdiBufferEnableIo<'_, R>
{
    fn read(&mut self, reg: u32) -> u32 {
        if reg != self.ddi_buf_ctl.offset() {
            self.read_failed = true;
            return u32::MAX;
        }
        match self.registers.read(self.ddi_buf_ctl) {
            Some(value) => value,
            None => {
                self.read_failed = true;
                u32::MAX
            }
        }
    }

    fn write(&mut self, reg: u32, value: u32) {
        if reg != self.ddi_buf_ctl.offset() || !self.registers.write(self.ddi_buf_ctl, value) {
            self.write_failed = true;
        }
    }

    fn combo_phy_read(&mut self, _phy: u8, _reg: intel_display::intel_ddi_full::ComboPhyRegister) -> u32 { 0 }
    fn combo_phy_write(&mut self, _phy: u8, _reg: intel_display::intel_ddi_full::ComboPhyRegister, _value: u32) {}
    fn combo_phy_rmw(&mut self, _phy: u8, _reg: intel_display::intel_ddi_full::ComboPhyRegister, _clear: u32, _set: u32) {}
    fn mg_phy_rmw(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::MgPhyRegister, _clear: u32, _set: u32) {}
    fn dkl_phy_read(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::DklPhyRegister) -> u32 { 0 }
    fn dkl_phy_write(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::DklPhyRegister, _value: u32) {}
    fn dkl_phy_rmw(&mut self, _port: intel_display::intel_ddi_full::Port, _reg: intel_display::intel_ddi_full::DklPhyRegister, _clear: u32, _set: u32) {}
    fn mg_dp_mode_read(&mut self, _port: intel_display::intel_ddi_full::Port, _lane: u8) -> u32 { 0 }
    fn mg_dp_mode_write(&mut self, _port: intel_display::intel_ddi_full::Port, _lane: u8, _value: u32) {}

    fn wait_clear(&mut self, reg: u32, mask: u32, timeout_ms: u32) -> bool {
        if reg != self.ddi_buf_ctl.offset() {
            self.read_failed = true;
            return true;
        }
        match super::regs::poll(
            self.registers,
            self.ddi_buf_ctl,
            mask,
            0,
            timeout_ms.saturating_mul(1000),
        ) {
            Some(true) => false,
            Some(false) => {
                self.timed_out = true;
                true
            }
            None => {
                self.read_failed = true;
                true
            }
        }
    }
}

impl OutputProgram {
    /// Compute every register value phase 5 writes, without touching hardware.
    ///
    /// `platform_ref_khz` is the reference frequency the platform straps --
    /// [`read_platform_reference_khz`] reads it from `SKL_DSSM`, which §11
    /// phase 5.1 puts first for the reason that everything downstream is scaled
    /// by it.  It is a parameter rather than something this function reads so
    /// that the whole program can be computed and logged where no register
    /// window exists.
    ///
    /// The checks run before anything is computed, so a caller that holds a
    /// plan holds one that passed every check the sequence makes.
    pub(crate) fn plan(
        request: &OutputRequest,
        platform_ref_khz: u32,
    ) -> Result<Self, OutputError> {
        // §8.1: the combo-PHY ports are A and B.  C and D are the Type-C/DKL
        // ports, and §8.8 defers that entire path -- there is no DKL PLL, no
        // FIA and no Type-C state machine in this kernel, so a request for one
        // is refused before a single register value is computed.
        let phy = combo_phy_of(request.ddi)?;
        let registers = selected_port_registers(phy, request.pll_id)?;

        // Both combo PHYs have a config pair in the table, so this is a check
        // on the table's completeness rather than on the request: a PHY whose
        // offsets were never sourced is refused instead of being written to a
        // neighbouring address.  The dividers themselves are PHY-independent --
        // `pll.rs` computes the same set for A and B, and only the address the
        // two values go to differs.
        if registers.pll_cfgcr0.is_none() || registers.pll_cfgcr1.is_none() {
            return Err(OutputError::PllConfigRegisterMissing { phy });
        }

        // §8.4's `[INF]`: TMDS at or above 340 MHz needs scrambling and the
        // high TMDS character rate, and enabling scrambling without telling the
        // sink gives a picture it cannot lock.  See the error's own text.
        if request.mode.clock_khz >= HDMI_SCRAMBLING_THRESHOLD_KHZ {
            return Err(OutputError::HdmiScramblingNotImplemented {
                pixel_clock_khz: request.mode.clock_khz,
            });
        }

        // The table selector follows intel_ddi_buf_trans_get() for the
        // supported ADL-N combo-PHY HDMI/DVI route. Explicit firmware dumps
        // remain an override for board-specific validation/replay.
        let swing = match request.swing {
            Some(swing) => swing,
            None => combo_default_swing(request.port_type, request.mode.clock_khz)?,
        };
        if u32::from(swing.level) > BUF_TRANS_SELECT_MAX {
            return Err(OutputError::SwingLevelOutOfRange { level: swing.level });
        }
        if let LinkRate::Code { code, .. } = request.link_rate
            && code > BUF_TRANS_SELECT_MAX
        {
            return Err(OutputError::LinkRateOutOfRange { code });
        }

        // The arithmetic is `pll.rs`'s, including the 38.4 → 19.2 MHz
        // reference division and the platform workaround predicate.  For HDMI
        // and DVI the TMDS character rate is the pixel clock, which is the case
        // §6.3 works through; that is what `ddi_pll_dividers` is for.
        let dividers = pll::ddi_pll_dividers(request.mode.clock_khz, platform_ref_khz, phy)?;
        let fraction_workaround = DcoFractionWorkaround::for_adl_p_n(platform_ref_khz);
        // The active Gen12 path takes the parameters from the translated
        // `icl_calc_wrpll()` / `icl_calc_dpll_state()` implementation rather
        // than merely keeping a second local copy of the same search. Keep
        // the old `PllFieldEncoding::Executed` variant available for the
        // explicit encoding-comparison tests, but the production/default
        // `Named` path is the i915 display-12/13 code path.
        let pll_registers = match request.encoding {
            PllFieldEncoding::Named => {
                let params = intel_display::dpll_mgr::icl_calc_wrpll(
                    request.mode.clock_khz,
                    platform_ref_khz,
                )
                .map_err(|_| {
                    OutputError::Pll(PllError::NoLegalDividerSet {
                        symbol_rate_khz: request.mode.clock_khz,
                        ref_khz: platform_ref_khz,
                    })
                })?;
                let state = intel_display::dpll_mgr::icl_calc_dpll_state(
                    params,
                    13,
                    platform_ref_khz,
                    None,
                )
                .map_err(|_| {
                    OutputError::Pll(PllError::NoLegalDividerSet {
                        symbol_rate_khz: request.mode.clock_khz,
                        ref_khz: platform_ref_khz,
                    })
                })?;
                PllRegisters {
                    cfgcr0: state.cfgcr0,
                    cfgcr1: state.cfgcr1,
                }
            }
            PllFieldEncoding::Executed => {
                dividers.registers(request.encoding, fraction_workaround)?
            }
        };

        // `TRANS_CLK_SEL` takes the PHY on this display version. The translated
        // DDI helper builds the independent transcoder port/mode/polarity
        // fields from the encoder and CRTC state.
        let trans_clk_sel = (phy_index(phy) + 1) << TRANS_CLK_SEL_PORT_SHIFT;
        let trans_ddi_func_ctl = source_trans_ddi_func_ctl(request);

        // §11 phase 5.6's write, restricted to what is actually a control bit.
        // No bit depth: §8.4's correction and §11 phase 5.6 both put it in
        // `PIPE_MISC` on Gen12, and §8.6 step 12's "| 8bpc" is the
        // pre-correction text.  No `STATE_ENABLE` either: §11 phase 5.6 prints
        // `(1<<31) | (1<<30)` and bit 30 is the hardware's pipe-running status
        // (`[I915]` `i915_reg.h:1591`, polled clear by `intel_wait_for_pipe_off`
        // at `display/intel_display.c:312-313`), which
        // `intel_enable_transcoder` never sets (`:459`, `:474-475`).  That is
        // the reference defect this composition used to repeat.
        let transconf = TRANSCONF_ENABLE;

        let ddi_buf_ctl = DDI_BUF_CTL_ENABLE
            | (u32::from(swing.level) << DDI_BUF_CTL_BUF_TRANS_SELECT_SHIFT)
            | (request.link_rate.field() << DDI_BUF_CTL_PHY_LINK_RATE_SHIFT)
            | request.width.width_field()
            | request.width.four_lane_bit();

        let ddi_clock_plan = intel_display::ddi::ddi_combo_clock_plan(
            intel_display::ddi::DdiClockPlatform::AlderLakeN,
            phy_index(phy) as u8,
            request.pll_id,
        )
        .map_err(|_| OutputError::ComboPllIndex {
            index: request.pll_id,
        })?;

        Ok(Self {
            pll_id: request.pll_id,
            ddi: request.ddi,
            phy,
            port_type: request.port_type,
            width: request.width,
            pixel_clock_khz: request.mode.clock_khz,
            platform_ref_khz,
            encoding: request.encoding,
            fraction_workaround,
            dividers,
            pll_registers,
            ddi_io_well: ddi_io_well(phy),
            swing,
            link_rate: request.link_rate,
            ddi_clock_plan,
            trans_clk_sel,
            trans_ddi_func_ctl,
            transconf,
            ddi_buf_ctl,
        })
    }

    /// The log §11 phase 5.1 asks for, and the whole program with it.
    ///
    /// One line per fact, so a caller can print it before the first write and a
    /// reader can diff it against §13.4's dump of a working configuration.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "intel-output: DDI {} on combo PHY {}, {}, {} lane(s), pixel clock {} kHz\n",
            self.ddi.name(),
            match self.phy {
                ComboPhy::A => "A",
                ComboPhy::B => "B",
            },
            self.port_type.name(),
            self.width.lanes(),
            self.pixel_clock_khz,
        ));
        out.push_str(&format!(
            "intel-output: reference {} kHz from SKL_DSSM, {} kHz into the DCO arithmetic, \
             dco_fraction {}\n",
            self.platform_ref_khz,
            self.dividers.wrpll_ref_khz(),
            match self.fraction_workaround {
                DcoFractionWorkaround::HalveFraction => "halved per WA #22010492432",
                DcoFractionWorkaround::NotNeeded => "left as computed",
            },
        ));
        out.push_str(&format!(
            "intel-output: (P, Q, K) = ({}, {}, {}), total divider {}, DCO {} kHz aimed at {} kHz \
             ({} centipercent away), symbol rate {} kHz, achieved {} Hz, error {} ppb\n",
            self.dividers.p(),
            self.dividers.q(),
            self.dividers.k(),
            self.dividers.total_divider(),
            self.dividers.target_dco_khz(),
            self.dividers.central_freq_khz(),
            self.dividers.deviation_centipercent(),
            self.dividers.symbol_rate_khz(),
            self.dividers.achieved_symbol_rate_hz(),
            self.dividers.rate_error_ppb(),
        ));
        out.push_str(&format!(
            "intel-output: PLL CFGCR0 = {:#010x}, CFGCR1 = {:#010x} under the {} encoding\n",
            self.pll_registers.cfgcr0, self.pll_registers.cfgcr1, self.encoding,
        ));
        out.push_str(&format!(
            "intel-output: ICL_DPCLKA_CFGCR0: DDI_CLK_SEL = {:#x}, then the DDI_CLK_OFF bit {:#x} \
             cleared in a separate write\n",
            self.ddi_clock_plan.selector_value, self.ddi_clock_plan.clock_off_mask,
        ));
        out.push_str(&format!(
            "intel-output: voltage-swing level {} from {} ({}) , DW2 per lane {:#010x?}, DW4 per \
             lane {:#010x?}, DW7 per lane {:#010x?}; {} lane(s) -> PWR_DOWN_LN_MASK field {:#x}\n",
            self.swing.level,
            self.swing.source,
            match self.port_type.buffer_translation_table() {
                Some(table) => format!("platform buffer-translation table `{table}`"),
                None => String::from("unsupported output selection"),
            },
            self.swing.dw2,
            self.swing.dw4,
            self.swing.dw7,
            self.width.lanes(),
            self.width.power_down_lanes_field(),
        ));
        out.push_str(&format!("intel-output: {}\n", self.link_rate.describe()));
        out.push_str(&format!(
            "intel-output: TRANS_CLK_SEL(A) = {:#010x}, TRANS_DDI_FUNC_CTL(A) = {:#010x}, \
             TRANSCONF(A) = {:#010x} (ENABLE only: {:#010x} is STATE_ENABLE, the hardware's \
             pipe-running status, and is not written), DDI_BUF_CTL({}) = {:#010x}\n",
            self.trans_clk_sel,
            self.trans_ddi_func_ctl,
            self.transconf,
            TRANSCONF_STATE_ENABLE_STATUS,
            self.ddi.name(),
            self.ddi_buf_ctl,
        ));
        out.push_str(&format!(
            "intel-output: that DDI_BUF_CTL({}) word is the plan's fields only -- ENABLE, \
             BUF_TRANS_SELECT, PHY_LINK_RATE, PORT_WIDTH and A_4_LANES.  The write is a \
             read-modify-write, so the board's PORT_REVERSAL[16] and every other bit keep the \
             value the firmware left\n",
            self.ddi.name(),
        ));
        out
    }

    /// Emit [`Self::render`] to the kernel log, one line at a time.
    ///
    /// §11 phase 5.1 asks for `ref`, `(P, Q, K)` and the symbol rate to be
    /// visible before the write; calling this before [`program`] is what makes
    /// that true even when a later step fails on a machine with no serial port.
    pub(crate) fn log(&self) {
        for text in self.render().lines() {
            axlog::debug!("{text}");
        }
    }
}

/// Read and decode `SKL_DSSM`'s reference-clock field, in kHz.
///
/// §11 phase 5.1's first line and §12.1's table: `SKL_DSSM[31:29]` is `000b` =
/// 24 MHz, `001b` = 19.2 MHz, `010b` = 38.4 MHz.  The decode itself is
/// `pll.rs`'s, because the same three values select the arithmetic there.
pub(crate) fn read_platform_reference_khz(regs: &impl Registers) -> Result<u32, OutputError> {
    let dssm = read(regs, regs::SKL_DSSM)?;
    Ok(pll::dssm_reference_khz(dssm)?)
}

// ---------------------------------------------------------------------------
// The sequence.
// ---------------------------------------------------------------------------

/// What phase 5 left behind, for the log and for the "prove it" phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputState {
    /// The DDI that was programmed.
    pub(crate) ddi: Ddi,
    /// The divider values that were written.
    pub(crate) pll_registers: PllRegisters,
    /// The symbol rate the arithmetic asked for, in kHz.
    pub(crate) symbol_rate_khz: u32,
    /// The symbol rate the registers produce, in Hz.
    pub(crate) achieved_symbol_rate_hz: u64,
    /// How far that sits from the request, in parts per billion.
    pub(crate) rate_error_ppb: i64,
    /// What the DDI-IO well handshake observed.
    pub(crate) ddi_io_well: WellObservation,
    /// `TRANS_CLK_SEL(A)` as written.
    pub(crate) trans_clk_sel: u32,
    /// `TRANS_DDI_FUNC_CTL(A)` as written.
    pub(crate) trans_ddi_func_ctl: u32,
    /// `TRANSCONF(A)` as written.
    pub(crate) transconf: u32,
    /// The fields this sequence owns in `DDI_BUF_CTL`, as written; the rest of
    /// the register held what it held ([`DDI_BUF_CTL_OWNED`]).
    pub(crate) ddi_buf_ctl: u32,
    /// The `DDI_BUF_CTL` readback that showed `IS_IDLE` clear.
    pub(crate) ddi_buf_ctl_readback: u32,
    /// The `DPLLn_ENABLE` readback that showed `LOCK` set.
    pub(crate) pll_enable_readback: u32,
}

impl OutputState {
    /// The log line for what happened, in the same shape as the plan's.
    pub(crate) fn render(&self) -> String {
        format!(
            "intel-output: DDI {} up: PLL CFGCR0 {:#010x} CFGCR1 {:#010x}, symbol rate {} kHz ({} \
             Hz achieved, {} ppb error), PLL enable readback {:#010x} (LOCK set), {}, DDI_BUF_CTL \
             {:#010x} (IS_IDLE clear, read back {:#010x}), TRANS_CLK_SEL(A) {:#010x}, \
             TRANS_DDI_FUNC_CTL(A) {:#010x}, TRANSCONF(A) {:#010x}",
            self.ddi.name(),
            self.pll_registers.cfgcr0,
            self.pll_registers.cfgcr1,
            self.symbol_rate_khz,
            self.achieved_symbol_rate_hz,
            self.rate_error_ppb,
            self.pll_enable_readback,
            self.ddi_io_well.describe(),
            self.ddi_buf_ctl,
            self.ddi_buf_ctl_readback,
            self.trans_clk_sel,
            self.trans_ddi_func_ctl,
            self.transconf,
        )
    }

    /// Emit [`Self::render`] to the kernel log, one line at a time.
    pub(crate) fn log(&self) {
        for text in self.render().lines() {
            axlog::debug!("{text}");
        }
    }
}

/// Phase 5: program the output path for a plan that was computed earlier.
///
/// The order is §8.6's enable sequence, steps 3 to 14, restricted to the output
/// half (the timings, the plane and the watermarks are the pipe workstream's
/// and happen between step 8 and step 11):
///
/// ```text
/// PLL power on, poll POWER_STATE, dividers, PLL enable, poll LOCK
/// DDI_CLK_SEL, then DDI_CLK_OFF cleared in a separate write
/// DDI-IO power well, poll STATE
/// SUS clock config, the swing writes, the lanes powered
/// TRANS_CLK_SEL, TRANS_DDI_FUNC_CTL, TRANSCONF
/// DDI_BUF_CTL, then poll IS_IDLE == 0
/// ```
///
/// Everything it writes was computed by [`OutputProgram::plan`], so a failure
/// part-way through leaves a log that says what the whole program was.  It
/// never panics: every failure is an [`OutputError`] whose `describe()` names
/// the register, the value and the section that explains the symptom.
pub(crate) fn program(
    regs: &impl Registers,
    plan: &OutputProgram,
) -> Result<OutputState, OutputError> {
    // A plan is data.  The DDI is checked again here, before the first write,
    // because "refused before anything is written" has to hold for the plan
    // that is actually programmed and not only for the one the builder accepts.
    // The register set is looked up from that same DDI, so a plan whose `phy`
    // field disagreed with its `ddi` field could not redirect a write.
    let phy = combo_phy_of(plan.ddi)?;
    let registers = selected_port_registers(phy, plan.pll_id)?;
    let pll_cfgcr0 = registers
        .pll_cfgcr0
        .ok_or(OutputError::PllConfigRegisterMissing { phy })?;
    let pll_cfgcr1 = registers
        .pll_cfgcr1
        .ok_or(OutputError::PllConfigRegisterMissing { phy })?;

    // 5.1 -- the PLL.  Power the block first: §6.3's sequence sets
    // POWER_ENABLE, polls POWER_STATE, and only then loads the dividers.
    rmw(regs, registers.pll_enable, 0, PLL_POWER_ENABLE)?;
    match poll(
        regs,
        registers.pll_enable,
        PLL_POWER_STATE,
        PLL_POWER_STATE,
        PLL_POWER_STATE_TIMEOUT_US,
    ) {
        Some(true) => {}
        Some(false) => {
            return Err(OutputError::PllPowerNeverCameUp {
                register: registers.pll_enable.name(),
                readback: read(regs, registers.pll_enable)?,
                timeout_us: PLL_POWER_STATE_TIMEOUT_US,
            });
        }
        None => {
            return Err(OutputError::Unreadable {
                register: registers.pll_enable.name(),
            });
        }
    }

    write(regs, pll_cfgcr0, plan.pll_registers.cfgcr0)?;
    write(regs, pll_cfgcr1, plan.pll_registers.cfgcr1)?;
    // §6.3's posting read: the divider write has to have landed before the
    // enable, and MMIO writes are posted (§2.2).
    read(regs, pll_cfgcr1)?;

    rmw(regs, registers.pll_enable, 0, PLL_ENABLE)?;
    match poll(
        regs,
        registers.pll_enable,
        PLL_LOCK,
        PLL_LOCK,
        PLL_LOCK_TIMEOUT_US,
    ) {
        Some(true) => {}
        Some(false) => {
            return Err(OutputError::PllNeverLocked {
                register: registers.pll_enable.name(),
                readback: read(regs, registers.pll_enable)?,
                p: plan.dividers.p(),
                q: plan.dividers.q(),
                k: plan.dividers.k(),
                wrpll_ref_khz: plan.dividers.wrpll_ref_khz(),
                timeout_us: PLL_LOCK_TIMEOUT_US,
            });
        }
        None => {
            return Err(OutputError::Unreadable {
                register: registers.pll_enable.name(),
            });
        }
    }
    let pll_enable_readback = read(regs, registers.pll_enable)?;

    // 5.2 -- map the DDI to that PLL, in two writes.  §6.3 quotes the spec
    // through `_icl_ddi_enable_clock`: the clock-select write and the clock-off
    // clear "must be done with separate register writes".  They are two `rmw`
    // calls, and a test asserts on the write count, because merging them is the
    // kind of change a later cleanup makes innocently.
    super::ddi::enable_combo_clock(regs, plan.ddi_clock_plan).map_err(|error| match error {
        super::ddi::DdiClockError::Unreadable => OutputError::Unreadable {
            register: dpll::ICL_DPCLKA_CFGCR0.name(),
        },
        super::ddi::DdiClockError::WriteRefused => OutputError::WriteRefused {
            register: dpll::ICL_DPCLKA_CFGCR0.name(),
        },
        super::ddi::DdiClockError::Unsupported => OutputError::ComboPllIndex { index: plan.pll_id },
    })?;

    // §8.6 step 5 -- the port's DDI-IO power, before anything drives a lane.
    // This is `power.rs`'s handshake, with its own rollback: a well that never
    // reports its state leaves the request bit withdrawn.
    let ddi_io_well = power::enable_well(
        regs,
        ddi_io_well(phy),
        intel_display::dmc::DmcPlatform::AlderLakeN,
    )?;

    // 5.3 -- §8.5's voltage-swing sequence: step 3's SUS clock config, steps 4
    // to 6's register batch.  `PORT_TX_DW2`, `PORT_TX_DW4` and `PORT_TX_DW7` are
    // written to the **per-lane** instances, once each in lane order, and never
    // to the group instance: §8.5 step 2 demands that for `DW4` ("NOT group
    // access -- each lane differs"), and i915's `icl_ddi_combo_vswing_program`
    // does the same for all three dwords, one read-modify-write per lane over
    // `ln = 0..3` (`[I915]` `display/intel_ddi.c:1148-1178`; the `DW4` loop's
    // own comment is that a group write "would overwrite individual loadgen",
    // `:1160`).  `PORT_TX_DW5` is the group register on both sides of that
    // batch -- i915 reads lane 0 and writes the group each time
    // (`[I915]` `display/intel_ddi.c:1141-1146`, `:1218-1229`) -- and the second
    // write is step 6's training-enable, which is what commits the settings.
    let lanes = tx_lane_registers(phy);
    rmw(regs, registers.cl_dw5, 0, CL_DW5_SUS_CLOCK_CONFIG_MASK)?;
    write_group_from_lane0(
        regs,
        registers.tx_dw5_lane0,
        registers.tx_dw5,
        plan.swing.dw5_mask,
        plan.swing.dw5_training_disabled,
    )?;
    for (register, value) in lanes.dw2.iter().zip(plan.swing.dw2) {
        rmw(regs, *register, plan.swing.dw2_mask, value)?;
    }
    for (lane, (register, value)) in lanes.dw4.iter().zip(plan.swing.dw4).enumerate() {
        let loadgen = if plan.pixel_clock_khz <= 600_000
            && ((plan.width == PortWidth::Four && lane >= 1)
                || (plan.width != PortWidth::Four && (lane == 1 || lane == 2)))
        {
            1 << 31
        } else {
            0
        };
        rmw(regs, *register, plan.swing.dw4_mask, value | loadgen)?;
    }
    for (register, value) in lanes.dw7.iter().zip(plan.swing.dw7) {
        rmw(regs, *register, plan.swing.dw7_mask, value)?;
    }
    write_group_from_lane0(
        regs,
        registers.tx_dw5_lane0,
        registers.tx_dw5,
        plan.swing.dw5_mask,
        plan.swing.dw5_training_enabled,
    )?;

    // §8.6 step 7 -- power the lanes.  A read-modify-write so that whatever
    // else `PORT_CL_DW10` holds survives; the field is `[7:4]` (§8.2).
    rmw(
        regs,
        registers.cl_dw10,
        PWR_DOWN_LN_MASK,
        plan.width.power_down_lanes_field() << PWR_DOWN_LN_MASK_SHIFT,
    )?;

    // 5.4 and 5.5 -- connect the transcoder to the port's clock, then to the
    // DDI.  Both are plain writes: these registers have no other field in play
    // on this path.  The transcoder is A's whatever DDI the plan names -- §5.1
    // gives the PRM's "Transcoders A-D can connect to any DDI" -- and the DDI
    // is inside the value (`plan.trans_clk_sel`'s port field and
    // `plan.trans_ddi_func_ctl`'s), so a plan for DDI B writes A's transcoder
    // registers with B's port number in them.
    write(regs, ddi::TRANS_CLK_SEL_A, plan.trans_clk_sel)?;
    write(regs, ddi::TRANS_DDI_FUNC_CTL_A, plan.trans_ddi_func_ctl)?;

    // 5.6 -- the transcoder itself.  `regs/pipe.rs` calls this register
    // `PIPECONF_A`; §5.2 records that i915 v6.12 calls the same offset
    // `TRANSCONF` and that they are one register, not two.
    // The pipe enable follows the HDMI encoder enable below.

    // 5.7 -- preserve unowned board bits, then hand the DDI_BUF_CTL write,
    // posting read and bounded IS_IDLE poll to the source helper. Its backend
    // is restricted to this one typed buffer register.
    let before_buffer = read(regs, registers.ddi_buf_ctl)?;
    let buffer_value = (before_buffer & !DDI_BUF_CTL_OWNED) | plan.ddi_buf_ctl;
    let source_port = match plan.ddi {
        Ddi::A => intel_display::intel_ddi_full::Port::A,
        Ddi::B => intel_display::intel_ddi_full::Port::B,
        Ddi::C => intel_display::intel_ddi_full::Port::C,
        Ddi::D => intel_display::intel_ddi_full::Port::D,
    };
    let source_output = match plan.port_type {
        PortType::Hdmi => intel_display::intel_ddi_full::OutputType::Hdmi,
        PortType::Dvi => intel_display::intel_ddi_full::OutputType::Dvi,
    };
    let source_encoder = intel_display::intel_ddi_full::DdiEncoder {
        port: source_port,
        output: source_output,
        display: intel_display::intel_ddi_full::Platform {
            display_ver: 13,
            alderlake_p: true,
            ..intel_display::intel_ddi_full::Platform::default()
        },
        ..intel_display::intel_ddi_full::DdiEncoder::default()
    };
    let mut source_io = DdiBufferEnableIo::new(regs, registers.ddi_buf_ctl);
    intel_display::intel_ddi_full::intel_ddi_buf_enable(
        &mut source_io,
        &source_encoder,
        buffer_value,
    );
    if source_io.read_failed {
        return Err(OutputError::Unreadable {
            register: registers.ddi_buf_ctl.name(),
        });
    }
    if source_io.write_failed {
        return Err(OutputError::WriteRefused {
            register: registers.ddi_buf_ctl.name(),
        });
    }
    let ddi_buf_ctl_readback = read(regs, registers.ddi_buf_ctl)?;
    if source_io.timed_out || ddi_buf_ctl_readback & DDI_BUF_CTL_IS_IDLE != 0 {
        return Err(OutputError::DdiNeverIdle {
            ddi: plan.ddi,
            register: registers.ddi_buf_ctl.name(),
            readback: ddi_buf_ctl_readback,
            wrote: buffer_value,
            timeout_us: DDI_IDLE_TIMEOUT_US,
        });
    }
    // ADL-P/N i915 enables the HDMI encoder before the CPU transcoder;
    // plane arm follows both. A stopped firmware pipe is mandatory at boot.
    write(regs, pipe::PIPECONF_A, plan.transconf)?;
    let ddi_buf_ctl_readback = read(regs, registers.ddi_buf_ctl)?;

    Ok(OutputState {
        ddi: plan.ddi,
        pll_registers: plan.pll_registers,
        symbol_rate_khz: plan.dividers.symbol_rate_khz(),
        achieved_symbol_rate_hz: plan.dividers.achieved_symbol_rate_hz(),
        rate_error_ppb: plan.dividers.rate_error_ppb(),
        ddi_io_well,
        trans_clk_sel: plan.trans_clk_sel,
        trans_ddi_func_ctl: plan.trans_ddi_func_ctl,
        transconf: plan.transconf,
        ddi_buf_ctl: plan.ddi_buf_ctl,
        ddi_buf_ctl_readback,
        pll_enable_readback,
    })
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// What can go wrong programming the output path.
///
/// Every variant is a thing a person can act on, in the style of
/// `pll::PllError` and `power::PowerError`; `describe()` is the text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OutputError {
    ComboPllIndex {
        index: u8,
    },
    /// The DDI is not a combo-PHY port, so this sequence does not cover it.
    UnsupportedDdi {
        /// The DDI that was asked for.
        ddi: Ddi,
    },
    /// The port's PLL config registers are not in the register table.
    ///
    /// Both combo PHYs on `XE_LPD` have theirs -- DPLL0's from §6.3 and
    /// DPLL1's from the `[I915]` header region §6.3 cites for that block -- so
    /// nothing raises this today.  It stays for the case it names: a PHY whose
    /// config offsets are genuinely unsourced must be refused rather than
    /// written to a guessed address.
    PllConfigRegisterMissing {
        /// Which combo PHY's PLL.
        phy: ComboPhy,
    },
    /// The voltage-swing values for this port type are not in the reference.
    MissingBufferTranslation {
        /// HDMI or DVI.
        port_type: PortType,
        /// The i915 table the reference names, when it names one.
        table: Option<&'static str>,
    },
    /// The swing level does not fit `BUF_TRANS_SELECT`, which is four bits.
    SwingLevelOutOfRange {
        /// The level that was asked for.
        level: u8,
    },
    /// The `PHY_LINK_RATE` code does not fit its four-bit field.
    LinkRateOutOfRange {
        /// The code that was asked for.
        code: u32,
    },
    /// The mode needs HDMI scrambling, which this sequence does not enable.
    HdmiScramblingNotImplemented {
        /// The pixel clock that crossed the threshold, in kHz.
        pixel_clock_khz: u32,
    },
    /// `pll.rs` refused the mode, the reference or the field encoding.
    Pll(PllError),
    /// A register could not be read: outside the window, or not there.
    Unreadable {
        /// The register's name.
        register: &'static str,
    },
    /// A register refused the write: not writable, or outside the window.
    WriteRefused {
        /// The register's name.
        register: &'static str,
    },
    /// `PLL_POWER_STATE` never set after `PLL_POWER_ENABLE`.
    PllPowerNeverCameUp {
        /// `DPLLn_ENABLE`.
        register: &'static str,
        /// What it read at the timeout.
        readback: u32,
        /// The poll budget in microseconds.
        timeout_us: u32,
    },
    /// `PLL_LOCK` never set after `PLL_ENABLE`.
    ///
    /// The divider set is carried so that the log says *which* program failed
    /// to lock: §11 phase 5.1's whole point is that a wrong reference or a
    /// skipped fraction workaround makes the PLL fail in a way that looks like
    /// a cable.
    PllNeverLocked {
        /// `DPLLn_ENABLE`.
        register: &'static str,
        /// What it read at the timeout.
        readback: u32,
        /// The post divider that was written.
        p: u32,
        /// The qdiv ratio that was written.
        q: u32,
        /// The `K` divider that was written.
        k: u32,
        /// The reference the arithmetic used, in kHz.
        wrpll_ref_khz: u32,
        /// The poll budget in microseconds.
        timeout_us: u32,
    },
    /// The DDI-IO power well did not come up.  Carries `power.rs`'s error.
    Well(PowerError),
    /// `DDI_BUF_CTL.IS_IDLE` never cleared.
    ///
    /// §11 phase 5.7 calls `IS_IDLE` the single best "is my DDI alive" bit on
    /// the chip and records a real `46d0` field report of this poll timing out
    /// under coreboot + EDK2.  Its advice -- check the port/`aux_ch` mapping and
    /// which DDI is actually wired before suspecting the PLL -- is in the text,
    /// because a wrong DDI is the far more common cause.
    DdiNeverIdle {
        /// The DDI that was enabled.
        ddi: Ddi,
        /// `DDI_BUF_CTL` for that port.
        register: &'static str,
        /// What the register read at the timeout.
        readback: u32,
        /// What was written to it.
        wrote: u32,
        /// The poll budget in microseconds.
        timeout_us: u32,
    },
}

impl From<PllError> for OutputError {
    fn from(error: PllError) -> Self {
        Self::Pll(error)
    }
}

impl From<PowerError> for OutputError {
    fn from(error: PowerError) -> Self {
        Self::Well(error)
    }
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

impl OutputError {
    /// The error in words a person on the machine can act on.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::ComboPllIndex { index } => {
                format!("unsupported combo PLL {index}: only 0 and 1 are implemented")
            }
            Self::UnsupportedDdi { ddi } => format!(
                "DDI {} is not a combo-PHY port.  Reference section 8.1: the ports this sequence \
                 programs are combo PHY A and B; C and D are Type-C/DKL ports, and section 8.8 \
                 defers the whole DKL path (different PLL family, FIA, TC state machine).  \
                 Nothing was written",
                ddi.name()
            ),
            Self::PllConfigRegisterMissing { phy } => format!(
                "combo PHY {}'s PLL (DPLL{}) has no CFGCR0/CFGCR1 offset in the register table.  \
                 Reference section 6.3 states the DPLL config offsets it carries, and \
                 regs/dpll.rs transcribes those and no others rather than pointing the divider \
                 write at a neighbouring address: the dividers would land on another PLL and this \
                 port would simply have no clock.  The arithmetic is not the problem -- pll.rs \
                 computes a divider set for either combo PHY -- so what is missing is an address; \
                 the `[I915]` register header that section 6.3 cites for the block, or a section \
                 13.4 dump of a working configuration, is what supplies one.  Nothing was written",
                match phy {
                    ComboPhy::A => "A",
                    ComboPhy::B => "B",
                },
                phy.dpll_index(),
            ),
            Self::MissingBufferTranslation { port_type, table } => format!(
                "no buffer-translation table is available for {} on this platform (table {:?}); \
                 nothing was written",
                port_type.name(),
                match table {
                    Some(table) => format!("`{table}`"),
                    None => String::from(
                        "no table at all -- section 8.5 names one for HDMI and none for DVI"
                    ),
                }
            ),
            Self::SwingLevelOutOfRange { level } => format!(
                "voltage-swing level {level} does not fit DDI_BUF_CTL's BUF_TRANS_SELECT, which \
                 is four bits (reference section 8.4).  Nothing was written"
            ),
            Self::LinkRateOutOfRange { code } => format!(
                "PHY_LINK_RATE code {code:#x} does not fit DDI_BUF_CTL's four-bit field \
                 (reference section 8.4).  Nothing was written"
            ),
            Self::HdmiScramblingNotImplemented { pixel_clock_khz } => format!(
                "a {pixel_clock_khz} kHz pixel clock is at or above the \
                 {HDMI_SCRAMBLING_THRESHOLD_KHZ} kHz HDMI scrambling threshold, and this sequence \
                 does not enable scrambling or the high TMDS character rate.  Reference section \
                 8.4's [INF] note: TMDS at that rate needs both, and enabling scrambling without \
                 telling the sink over SCDC gives a picture the monitor cannot lock.  Section 11 \
                 phase 3.1 steers a first light-up to 1080p60 (148.5 MHz) for exactly this \
                 reason.  Nothing was written"
            ),
            Self::Pll(error) => format!(
                "the PLL arithmetic refused the mode: {error}.  Reference section 6.3 and \
                 docs/design/intel-pll.md.  Nothing was written"
            ),
            Self::Unreadable { register } => format!(
                "{register} could not be read: it is outside the mapped register window.  Section \
                 2.1 records that display registers need no forcewake, but a register outside the \
                 aperture is unreadable all the same"
            ),
            Self::WriteRefused { register } => format!(
                "{register} refused the write: either it is not declared writable or it is \
                 outside the mapped register window"
            ),
            Self::PllPowerNeverCameUp {
                register,
                readback,
                timeout_us,
            } => format!(
                "{register}'s PLL_POWER_STATE never set within {timeout_us} us of \
                 PLL_POWER_ENABLE; it reads {readback:#010x}.  Reference section 6.3 step 1: that \
                 state is the acknowledgement that the PLL block is powered, and the divider \
                 write that follows would be dropped by an unpowered block.  Check the power well \
                 the PLL sits in before the divider arithmetic"
            ),
            Self::PllNeverLocked {
                register,
                readback,
                p,
                q,
                k,
                wrpll_ref_khz,
                timeout_us,
            } => format!(
                "{register}'s LOCK never set within {timeout_us} us with the divider set (P={p}, \
                 Q={q}, K={k}) computed against a {wrpll_ref_khz} kHz reference; it reads \
                 {readback:#010x}.  Reference section 11 phase 5.1: the two usual causes are a \
                 reference that was not divided by two (38.4 -> 19.2 MHz) and a fraction \
                 workaround that was skipped -- re-read SKL_DSSM and compare the symbol rate in \
                 the plan against the mode's pixel clock before suspecting the PHY"
            ),
            Self::Well(error) => {
                format!(
                    "the port's DDI-IO power well did not come up: {}",
                    error.describe()
                )
            }
            Self::DdiNeverIdle {
                ddi,
                register,
                readback,
                wrote,
                timeout_us,
            } => format!(
                "DDI {} never left idle: {register} still reads IS_IDLE set ({readback:#010x}) \
                 {timeout_us} us after writing {wrote:#010x}.  Reference section 11 phase 5.7: \
                 IS_IDLE is the best \"is my DDI alive\" bit on the chip and it stays set while \
                 the DDI has no clock -- so check the DDI-to-PLL mapping (ICL_DPCLKA_CFGCR0, step \
                 5.2) and the PLL (step 5.1), in that order.  Then check which DDI is actually \
                 wired before suspecting the divider arithmetic: i915 bug #10932 is this exact \
                 poll timing out on an N200/46d0 machine under coreboot + EDK2, and a wrong \
                 port/aux_ch mapping is a far more common cause than a wrong divider",
                ddi.name()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Register access, in `power.rs`'s convention.
// ---------------------------------------------------------------------------

/// Read a register the sequence cannot do without.
fn read(regs: &impl Registers, register: Register) -> Result<u32, OutputError> {
    regs.read(register).ok_or(OutputError::Unreadable {
        register: register.name(),
    })
}

/// Write a register, refusing to continue if the write did not happen.
fn write(regs: &impl Registers, register: Register, value: u32) -> Result<(), OutputError> {
    if regs.write(register, value) {
        Ok(())
    } else {
        Err(OutputError::WriteRefused {
            register: register.name(),
        })
    }
}

/// Read-modify-write, `(read & !clear) | set`.
fn rmw(
    regs: &impl Registers,
    register: Register,
    clear: u32,
    set: u32,
) -> Result<u32, OutputError> {
    let current = read(regs, register)?;
    write(regs, register, (current & !clear) | set)?;
    Ok(current)
}

/// Match i915's PORT_TX_DW5 sequence: read lane 0, then write the group
/// instance with only the named fields changed.
fn write_group_from_lane0(
    regs: &impl Registers,
    source: Register,
    group: Register,
    clear: u32,
    set: u32,
) -> Result<(), OutputError> {
    let current = read(regs, source)?;
    write(regs, group, (current & !clear) | set)
}

/// Poll a register until `mask` reads `value`, or the budget runs out.
///
/// `None` means the register could not be read, which is not the same as a
/// register that read zero and must not be collapsed into one.
fn poll(
    regs: &impl Registers,
    register: Register,
    mask: u32,
    value: u32,
    timeout_us: u32,
) -> Option<bool> {
    regs::poll(regs, register, mask, value, timeout_us)
}

#[cfg(test)]
mod tests;
