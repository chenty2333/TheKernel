//! One block of the display engine's register table.
//!
//! Owner: [`super`].  Every offset here cites the reference section it came
//! from, and the access classification is the sequence's: a register the
//! bring-up writes is `read_write`, one it only reads back is `read_only`.

use super::{Meaning, Register};

/// `DPLL0_CFGCR0` (combo DPLL 0), reference section 6.3.
///
/// The combo PLL's DCO fields -- `DCO_INTEGER[9:0]` and `DCO_FRACTION[24:10]`
/// -- which set the VCO that the port clock is divided down from.  Bring-up
/// writes it after powering the PLL block and before enabling it, and reading
/// it first is how a driver reuses a divider set the firmware already
/// programmed.  Section 6.3's worked 1080p60 example writes `(1024 << 10) |
/// 464` here; the section cites `[I915]` `i915_reg.h:4301-4322`.
pub(crate) const DPLL0_CFGCR0: Register =
    Register::read_write("DPLL0_CFGCR0", 0x16_4284, Meaning::BringUp, None);

/// `DPLL0_CFGCR1` (combo DPLL 0), reference section 6.3.
///
/// The post-divider fields `QDIV_RATIO[17:10]`, `QDIV_MODE[9]`, `KDIV[8:6]`,
/// `PDIV[5:2]` and `CFSELOVRD[1:0]`, in the Gen12 positions; on ADL-N
/// `CFSELOVRD` is normal-XTAL, so bits `[1:0]` are written as zero.  The
/// Skylake `DPLL_CFGCR2` positions sit two bits lower and do not apply, and a
/// posting read of this register closes the divider write.
pub(crate) const DPLL0_CFGCR1: Register =
    Register::read_write("DPLL0_CFGCR1", 0x16_4288, Meaning::BringUp, None);

/// `DPLL0_ENABLE` (alias `LCPLL1_CTL`), reference section 6.3.
///
/// The combo PLL's power and enable control: `PLL_ENABLE[31]`, `LOCK[30]`,
/// `POWER_ENABLE[27]` and `POWER_STATE[26]`.  Bring-up clears `POWER_ENABLE`
/// and polls `POWER_STATE` before loading the dividers, then sets `PLL_ENABLE`
/// and polls `LOCK`; the document notes that this register and `LCPLL1_CTL`
/// are one register at one address, not two.
pub(crate) const DPLL0_ENABLE: Register =
    Register::read_write("DPLL0_ENABLE", 0x4_6010, Meaning::BringUp, None);

/// `DPLL1_ENABLE` (alias `LCPLL2_CTL`), reference section 6.3.
///
/// The same power, enable and lock bits as `DPLL0_ENABLE`, for the second and
/// last combo PLL on ADL-N -- the one that clocks combo PHY B.  The TBT PLL
/// and the TC PLLs are a different (DKL/Type-C) family and are not part of
/// this bring-up.
pub(crate) const DPLL1_ENABLE: Register =
    Register::read_write("DPLL1_ENABLE", 0x4_6014, Meaning::BringUp, None);

/// `ICL_DPCLKA_CFGCR0` (DDI port clock select), reference section 6.3.
///
/// Routes each DDI to a PLL and gates its clock: the two-bit `DDI_CLK_SEL`
/// field for a PHY starts at bit `phy * 2` and carries the PLL id, and the
/// per-DDI `DDI_CLK_OFF` bit has to be cleared before the DDI has a clock.
/// Section 6.3 requires the select write and the clock-off clear to be
/// separate register writes; section 11 phase 5.2 is where bring-up does it.
pub(crate) const ICL_DPCLKA_CFGCR0: Register =
    Register::read_write("ICL_DPCLKA_CFGCR0", 0x16_4280, Meaning::BringUp, None);

/*
 * Transcription notes
 *
 * Offsets the document does not state, and which were therefore not
 * transcribed:
 * - `DPLL1_CFGCR0` / `DPLL1_CFGCR1`: section 6.3 names a `DPLL1_*` family and
 *   gives `DPLL1_ENABLE` as `0x46014`, but it states config offsets only for
 *   DPLL0 ("`DPLLn_CFGCR0` (`0x164284` for DPLL0)", "`DPLLn_CFGCR1`
 *   (`0x164288` for DPLL0)").  No DPLL1 config offset appears anywhere in the
 *   document, so combo PHY B's PLL cannot be configured from this table.
 * - Per-PHY siblings of the port clock select: the document names exactly one
 *   such register, `ICL_DPCLKA_CFGCR0` (`0x164280`, sections 6.3 and 11 phase
 *   5.2), and handles the PHYs with fields inside it (`DDI_CLK_SEL_SHIFT(phy)
 *   = phy * 2`, `DDI_CLK_OFF` at `_PICK(phy, 10, 11, 24, 4, 5)`).  No
 *   `DPCLKB`/`DPCLKC`/`DPCLKD` name or offset occurs in any section, so no
 *   sibling is declared and none is guessed.
 * - `DPLL_CFGCR2`, which the group list names: it occurs only as the
 *   Skylake-era `DPLL_CFGCR2_*` field layout that section 6.3 says "do not
 *   apply" on Gen12, and no `DPLL_CFGCR2` register offset is stated anywhere.
 *   Not transcribed; the Gen12 config register is `DPLL0_CFGCR1`.
 *
 * Places where the document's mentions of a register disagree:
 * - `CDCLK_PLL_ENABLE` (`0x46070`) bits `[27]`/`[26]`: `[TGL12]` calls them
 *   Slow Clock Enable/Lock, while `[I915]`'s `PLL_POWER_ENABLE` /
 *   `PLL_POWER_STATE` names belong to the combo DPLL registers
 *   `0x46010`/`0x46014`.  Section 4.6 reconciles this -- different bits on
 *   different registers -- and records that an earlier draft wrongly carried
 *   the combo-DPLL power-up step into the CDCLK PLL sequence.
 * - `DPLL_CFGCR1` field positions: section 6.3 gives `QDIV_RATIO[17:10]`,
 *   `QDIV_MODE[9]`, `KDIV[8:6]`, `PDIV[5:2]` and `CFSELOVRD[1:0]`, while the
 *   Skylake `DPLL_CFGCR2_*` definitions it quotes are "two bits lower";
 *   section 13.3 records that this replaced an earlier draft which used the
 *   Skylake layout.  The Gen12 positions are the ones used above.
 * - `PDIV`/`KDIV` encoding: section 13.1 item 7 still lists "[TGL12] and
 *   i915's executed path disagree", but section 6.3 states "On the ADL-N path
 *   there is no discrepancy: write and read agree" and explains the earlier
 *   draft's error.  That is about field values, not the offset, and section
 *   13.1 item 7 reads as a stale entry.
 * - CDCLK PLL reference frequency: `[PRM]` (DG1) says 38.4 MHz fixed and "not
 *   programmable"; `[I915]` reads `SKL_DSSM[31:29]` as 24/19.2/38.4 MHz.  The
 *   ratio written to `CDCLK_PLL_ENABLE` depends on which is right, so read
 *   `SKL_DSSM` rather than assuming.
 * - Raw clock: `[PRM]` (DG1) expects 38.4 MHz, while `[I915]`'s `cnp_rawclk`
 *   programs 24 or 19.2 MHz from `SFUSE_STRAP[8]`.  A disagreement about the
 *   value `PCH_RAWCLK_FREQ` should hold, not about its offset.
 * - Not a disagreement: `DPLL0_ENABLE`/`LCPLL1_CTL` (`0x46010`) and
 *   `DPLL1_ENABLE`/`LCPLL2_CTL` (`0x46014`) are two names for one address
 *   each, and section 6.3 says so explicitly.
 * - Not a disagreement: section 5.3 calls the port clock select
 *   `DPCLKA_CFGCR0` while sections 6.3 and 11 call it `ICL_DPCLKA_CFGCR0`;
 *   the offset `0x164280` is stated once, in section 6.3.
 *
 * Registers deliberately left out:
 * - `CDCLK_CTL` (`0x46000`), `CDCLK_PLL_ENABLE`/`BXT_DE_PLL_ENABLE`
 *   (`0x46070`), `PCH_RAWCLK_FREQ` (`0xC6204`) and `SKL_DSSM` (`0x51004`, the
 *   PLL reference read of sections 4.6 and 11 phase 0.3): already declared in
 *   `regs.rs` as `POWER_AND_CLOCK_REGISTERS`; re-declaring them here would
 *   duplicate them.
 * - `CDCLK_SQUASH_CTL` (`0x46008`): section 4.6 states it is not present or
 *   used on `XE_LPD`.
 * - `DPLL0_DIV0` (`0x164B00`): section 6.3 lists it but marks the AFC-startup
 *   write "only if VBT overrides it"; section 11 never writes it and this
 *   kernel reads no VBT.
 * - TBT PLL (`0x46020`) and TC PLL 1-4 (`PORTTC1/2_PLL_ENABLE`,
 *   `0x46038`/`0x46040`): section 6.3 says to ignore the DKL/Type-C PLLs and
 *   section 8.8 defers the whole Type-C path.
 * - `TRANS_CLK_SEL(tran)` (`0x46140 + tran*4`): section 6.3 routing step 2 and
 *   section 11 phase 5.4 need it, but it is a transcoder register and belongs
 *   to the transcoder/timing group rather than this port-PLL and CDCLK group.
 * - `GEN6_PCODE_MAILBOX` / `GEN6_PCODE_DATA` / `GEN6_PCODE_DATA1`
 *   (`0x138124`/`0x138128`/`0x13812C`): section 4.6's full CDCLK-change
 *   sequence uses the mailbox, but section 11 phase 1.4's simple path -- write
 *   the ratio, enable, poll lock, then write `CDCLK_CTL` -- does not, and no
 *   pipe is running at that point.
 * - Nothing read-only is declared for the "prove it" step: section 11 phase 6
 *   reads `PIPEDSL`, `PLANE_SURFLIVE`, `DDI_BUF_CTL.IS_IDLE` and `PIPESTAT`,
 *   none of which are in this group.  Every constant above is written by the
 *   bring-up sequence, so every one of them is `read_write`.
 */
