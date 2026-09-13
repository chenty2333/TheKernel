//! One block of the display engine's register table.
//!
//! Owner: [`super`].  Every offset here cites the reference section it came
//! from, and the access classification is the sequence's: a register the
//! bring-up writes is `read_write`, one it only reads back is `read_only`.

use super::{Meaning, Register};

/// `PORT_CL_DW10` (combo PHY A), reference section 8.2.
///
/// The CL block's lane power register.  `PWR_DOWN_LN_MASK` at bits `[7:4]`
/// selects which DDI lanes stay powered down, and section 11 phase 5.3 powers
/// the port's lanes up through it as the last PHY write before the DDI buffer
/// is enabled (`0x0` = all four lanes, `0xC` = two, `0xE` = one; section 8.6
/// step 7).
pub(crate) const PORT_CL_DW10_A: Register =
    Register::read_write("PORT_CL_DW10(A)", 0x16_2028, Meaning::BringUp, None);

/// `PORT_CL_DW10` (combo PHY B), reference section 8.2.
///
/// PHY B's lane power register, at `PHY_BASE + 4*dw` with no sub-block term
/// (the CL block is group-wide only).  Written the same way as PHY A's when
/// PHY B's port is brought up.
pub(crate) const PORT_CL_DW10_B: Register =
    Register::read_write("PORT_CL_DW10(B)", 0x6_c028, Meaning::BringUp, None);

/// `PORT_TX_DW2` (combo PHY A, group instance), reference section 8.2.
///
/// The TX block's swing-select register, written with the buffer translation
/// table's `dw2_swing_sel` value during the voltage-swing sequence of section
/// 8.5 that section 11 phase 5.3 runs before the lanes come up.
pub(crate) const PORT_TX_DW2_GRP_A: Register =
    Register::read_write("PORT_TX_DW2_GRP(A)", 0x16_2688, Meaning::BringUp, None);

/// `PORT_TX_DW2` (combo PHY B, group instance), reference section 8.2.
///
/// PHY B's swing-select register: the same buffer-translation `dw2_swing_sel`
/// value as PHY A's, at PHY B's own `+0x680` TX group.
pub(crate) const PORT_TX_DW2_GRP_B: Register =
    Register::read_write("PORT_TX_DW2_GRP(B)", 0x6_c688, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 0 of combo PHY A, reference section 8.5.
///
/// Lane 0's loadgen select, and the register the buffer translation table's
/// `dw4_cursor_coeff` / post-cursor columns land in.  Section 8.5 step 2
/// programmes it one lane at a time and warns that group access must not be
/// used for it, because each lane's value differs.
pub(crate) const PORT_TX_DW4_LN0_A: Register =
    Register::read_write("PORT_TX_DW4_LN0(A)", 0x16_2890, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 1 of combo PHY A, reference section 8.5.
///
/// Lane 1's loadgen select and cursor coefficient.  It gets its own value
/// (`ln1 = 1` for the rates section 8.5 step 2 lists), which is why the write
/// has to be per lane rather than to the group instance.
pub(crate) const PORT_TX_DW4_LN1_A: Register =
    Register::read_write("PORT_TX_DW4_LN1(A)", 0x16_2990, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 2 of combo PHY A, reference section 8.5.
///
/// Lane 2's loadgen select and cursor coefficient, written per lane for the
/// same reason as lanes 0 and 1.
pub(crate) const PORT_TX_DW4_LN2_A: Register =
    Register::read_write("PORT_TX_DW4_LN2(A)", 0x16_2a90, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 3 of combo PHY A, reference section 8.5.
///
/// Lane 3's loadgen select and cursor coefficient; it is the lane that is
/// programmed differently for a one- or two-lane port (`ln3 = 0`) than for a
/// four-lane one.
pub(crate) const PORT_TX_DW4_LN3_A: Register =
    Register::read_write("PORT_TX_DW4_LN3(A)", 0x16_2b90, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 0 of combo PHY B, reference section 8.5.
///
/// Lane 0's loadgen select and cursor coefficient on the second combo PHY,
/// written per lane exactly as PHY A's is.
pub(crate) const PORT_TX_DW4_LN0_B: Register =
    Register::read_write("PORT_TX_DW4_LN0(B)", 0x6_c890, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 1 of combo PHY B, reference section 8.5.
///
/// Lane 1's loadgen select and cursor coefficient on combo PHY B.
pub(crate) const PORT_TX_DW4_LN1_B: Register =
    Register::read_write("PORT_TX_DW4_LN1(B)", 0x6_c990, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 2 of combo PHY B, reference section 8.5.
///
/// Lane 2's loadgen select and cursor coefficient on combo PHY B.
pub(crate) const PORT_TX_DW4_LN2_B: Register =
    Register::read_write("PORT_TX_DW4_LN2(B)", 0x6_ca90, Meaning::BringUp, None);

/// `PORT_TX_DW4`, lane 3 of combo PHY B, reference section 8.5.
///
/// Lane 3's loadgen select and cursor coefficient on combo PHY B; the value
/// differs with the port width, as it does on PHY A.
pub(crate) const PORT_TX_DW4_LN3_B: Register =
    Register::read_write("PORT_TX_DW4_LN3(B)", 0x6_cb90, Meaning::BringUp, None);

/// `PORT_TX_DW5` (combo PHY A, group instance), reference section 8.2.
///
/// The TX block's training-enable and scaling-mode register.  Section 8.5
/// steps 4 to 6 clear TX training enable, set the scaling mode, write the table
/// values, then set training enable again -- that last write is what commits
/// the swing and pre-emphasis settings.
pub(crate) const PORT_TX_DW5_GRP_A: Register =
    Register::read_write("PORT_TX_DW5_GRP(A)", 0x16_2694, Meaning::BringUp, None);

/// `PORT_TX_DW5` (combo PHY B, group instance), reference section 8.2.
///
/// PHY B's training-enable and scaling-mode register, written by the same
/// sequence as PHY A's when PHY B's port is programmed.
pub(crate) const PORT_TX_DW5_GRP_B: Register =
    Register::read_write("PORT_TX_DW5_GRP(B)", 0x6_c694, Meaning::BringUp, None);

/// `PORT_TX_DW7` (combo PHY A, group instance), reference section 8.2.
///
/// The TX block's N-scalar register, written with the buffer translation
/// table's `dw7_n_scalar` value as part of the section 8.5 voltage-swing
/// sequence.
pub(crate) const PORT_TX_DW7_GRP_A: Register =
    Register::read_write("PORT_TX_DW7_GRP(A)", 0x16_269c, Meaning::BringUp, None);

/// `PORT_TX_DW7` (combo PHY B, group instance), reference section 8.2.
///
/// PHY B's N-scalar register, carrying the same `dw7_n_scalar` value as PHY A's
/// for a given swing level.
pub(crate) const PORT_TX_DW7_GRP_B: Register =
    Register::read_write("PORT_TX_DW7_GRP(B)", 0x6_c69c, Meaning::BringUp, None);
/*
 * Transcription notes
 * ===================
 *
 * Already declared in `kernel/src/drm/intel/regs.rs` (read and checked, not
 * re-declared here):
 *   - `PORT_COMP_DW0`  -- 0x162100 (A) / 0x06c100 (B), `ComboPhyRegisters::comp_dw0`.
 *   - `PORT_COMP_DW1`  -- 0x162104 (A) / 0x06c104 (B), the masked procmon write of section 8.3 step 4.
 *   - `PORT_COMP_DW3`  -- 0x16210c (A) / 0x06c10c (B), read for the process/voltage row; read-only there.
 *   - `PORT_COMP_DW8`  -- 0x162120 (A) / 0x06c120 (B), the PHY A `IREFGEN` bit of section 8.3 step 5.
 *   - `PORT_COMP_DW9`  -- 0x162124 (A) / 0x06c124 (B), procmon value, full 32-bit write.
 *   - `PORT_COMP_DW10` -- 0x162128 (A) / 0x06c128 (B), procmon value, full 32-bit write.
 *   - `PORT_TX_DW8` (group) -- 0x1626a0 (A) / 0x06c6a0 (B), the ODCC clock select of section 8.3 step 1.
 *   - `PORT_TX_DW8_LN0` -- 0x1628a0 (A) / 0x06c8a0 (B), read-only; where that step reads the initial value.
 *   - `PORT_PCS_DW1` (group) -- 0x162604 (A) / 0x06c604 (B), DCC mode select (section 8.3 step 1) and `cmnkeeper_enable` (section 8.5 step 1).
 *   - `PORT_PCS_DW1_LN0` -- 0x162804 (A) / 0x06c804 (B), read-only; lane 0's copy, read for the same reason.
 *   - `PORT_CL_DW5` -- 0x162014 (A) / 0x06c014 (B), `CL_POWER_DOWN_ENABLE` (section 8.3 step 7) and `SUS_CLOCK_CONFIG` (section 8.5 step 3).
 *   - `ICL_PHY_MISC` -- 0x64c00 (A) / 0x64c04 (B), `DE_IO_COMP_PWR_DOWN` cleared by section 8.3 step 3; note it is in the DDI block, not the PHY aperture.
 *   - `HSW_PWR_WELL_CTL1`/`2`/`3`/`4` -- 0x45400/0x45404/0x45408/0x4540c, the section 4.4 handshake's request registers.
 *   - `ICL_PWR_WELL_CTL_AUX2` -- 0x45444, the AUX/DDC well section 11 phase 2.1 enables; deliberately not re-declared.
 *   - `ICL_PWR_WELL_CTL_DDI2` -- 0x45454, the DDI-IO well of section 8.6 step 5.
 *   - `SKL_FUSE_STATUS` -- 0x42000, the `PG_DIST_STATUS` fuse poll of section 4.4.
 *   - `SKL_DSSM` -- 0x51004, read by section 11 phase 1.4 before the CDCLK/port PLL work.
 *   - `GEN8_CHICKEN_DCPR_1` -- 0x46430, the `Wa_16013190616` write section 11 phase 1.3 does before the `PW_1` request.
 *   - `XELPD_DISPLAY_ERR_FATAL_MASK` -- 0x4421c, the display error mask of section 4.9 step 11; it is the only error/fatal mask section 4 places in this group, and it is already declared.
 *
 * Offsets I could not find in the document:
 *   - None that I needed.  Two things I did need were only partly tabulated,
 *     so their arithmetic is recorded here rather than left implicit:
 *   - `PORT_CL_DW10`: section 8.2 tabulates only `CL_DW5` as a worked example
 *     (0x162014); `DW10` is the document's own `_ICL_PORT_CL_DW(dw, phy) =
 *     PHY_BASE + 4*dw` with no sub-block term, giving 0x162028 (A) and 0x06c028 (B).
 *   - `PORT_TX_DW7` (group): not a worked-example row; from the stated TX group
 *     base `+0x680` plus `4*7`, giving 0x16269c (A) and 0x06c69c (B).
 *   - `PORT_TX_DW4` lanes 1-3: section 8.2 states the lane base
 *     `0x880 + ln*0x100` but works no example past lane 0; `DW4` adds `0x10`,
 *     giving 0x162890/0x162990/0x162a90/0x162b90 (A) and 0x06c890/0x06c990/
 *     0x06ca90/0x06cb90 (B).  The same arithmetic reproduces the `PORT_TX_DW8`
 *     group and lane-0 offsets regs.rs already declares, which is the check
 *     that it is the document's rule and not a guess.
 *   - Every PHY B offset: section 8.2 gives the PHY B base 0x06c000 and no
 *     worked PHY B example; section 12.2's PHY B `PORT_COMP_DW0 = 0x06C100`
 *     confirms the base.  PHY B is PHY A minus 0xf6000 throughout.
 *
 * Places where the document's two mentions of a register disagreed:
 *   - The `PORT_TX_DW*` group base: section 8.2's correction box records an
 *     earlier draft's `+0x400` against the correct `+0x680`; `+0x400` is not a
 *     defined sub-block and would miss every `PORT_TX` register.  Used `+0x680`
 *     (as regs.rs already does).
 *   - `PORT_TX_DW4`: section 8.2 lists `ICL_PORT_TX_DW4_GRP(A) = 0x162690` as a
 *     group register, while section 8.5 step 2 says the register must be
 *     programmed per lane and the paragraph after the sequence says group
 *     access must never be used for it.  Declared the four per-lane instances
 *     and no group instance.
 *   - `PORT_TX_DW4`'s contents: section 8.2 calls it the cursor coefficient,
 *     section 8.5 step 2 calls the same register the per-lane loadgen select.
 *     Both are written through it, so the per-lane declaration covers both.
 *   - `PORT_PCS_DW1` DCC mode: the TGL text says "DCC continuous mode" and the
 *     DG1 text says "divide by 2"; section 8.3 and section 13.3 resolve it to
 *     `RUN_DCC_ONCE` for Gen12.  The register is already declared in regs.rs.
 *   - `DP_AUX_CH_CTL`: section 8.4's layout column points at "section 11", but
 *     section 11 never describes the register; the field list is in section
 *     9.6.  The two places that give its offset (8.4 and 9.6) agree exactly.
 *   - Section 4.9 step 2 cites `intel_combo_phy_init()` as "section 9.3", but
 *     the combo PHY initialisation is section 8.3 (9.3 is the GMBUS transaction
 *     protocol).  Offsets unaffected; recorded because it is the cross
 *     reference a reader of this group would follow.
 *
 * Registers deliberately left out, and why:
 *   - `PORT_COMP_DW2`, `DW4`-`DW7`, `DW11`, `DW12`: section 8.2's COMP row
 *     names fields only for `DW0`, `DW1`, `DW3`, `DW8`, `DW9` and `DW10`, and
 *     no step of section 8.3, 8.5 or 11 touches the others, so neither their
 *     purpose nor their values are stated.
 *   - `PORT_CL_DW12` (`LANE_ENABLE_AUX[0]`, section 8.2): no step of section 11
 *     writes it, and the AUX path that would need it is deferred (sections 8.7,
 *     8.8, 9.6).  It belongs in the table on the day AUX is implemented.
 *   - `PORT_CL_DW0`: the CL block's implicit `+0x000`; the document names no
 *     register of that number.
 *   - The combo PHY bases 0x162000 and 0x06c000 themselves: they are addresses,
 *     not registers, and the document names no register at `PHY_BASE + 0`, so
 *     no constant was invented for them (regs.rs's `ComboPhyRegisters` carries
 *     no base field either).
 *   - `PORT_TX_AUX` (`+0x380`), `PORT_PCS_AUX` (`+0x300`), and PCS/TX lanes 1-3
 *     apart from `PORT_TX_DW4`: no sequence in the named sections names them.
 *   - `DDI_BUF_TRANS_LO`/`DDI_BUF_TRANS_HI` (section 8.4: `0x64e00 + i*8` and
 *     `0x64e60 + i*8`, high half at `+4`): section 8.5's write sequence programs
 *     the combo PHY's `PORT_TX_DW2`/`DW4`/`DW5`/`DW7` directly, and no step of
 *     section 11 writes the indexed table registers -- `BUF_TRANS_SELECT` is
 *     only an index field in `DDI_BUF_CTL`.
 *   - `DP_AUX_CH_CTL` and `DP_AUX_CH_DATA(i)` (section 8.4: 0x64010/0x64110 and
 *     0x64014 + 4i / 0x64114 + 4i; section 9.6 repeats both): the document
 *     places them in the DDI table, but section 9.6 says to skip AUX unless the
 *     output is DisplayPort, and no step of section 11 touches them -- the EDID
 *     path is GMBUS (section 11 phase 2.3) and the mode path is HDMI/DVI
 *     (section 11 phase 5.5).  The AUX/DDC power well section 11 phase 2.1 does
 *     need is `ICL_PWR_WELL_CTL_AUX2`, already declared.
 *   - `TRANS_CLK_SEL` (used by section 11 phase 5.4 with the value 0x10000000
 *     for port A): its offset `0x46140 + tran*4` is stated in section 6.3, not
 *     in sections 8.2-8.5, so it belongs to the port-clock/PLL group.
 *   - `ICL_DPCLKA_CFGCR0` (used by section 11 phase 5.2 at 0x164280): the same
 *     case -- its offset and fields are section 6.3 material, and `pll.rs`
 *     already names it.
 *   - `TRANS_DDI_FUNC_CTL` for transcoders B, C and D: section 11's sequence
 *     fixes transcoder A (pipe A, section 4.3) and the document gives only the
 *     per-transcoder rule `0x60400 + T*0x1000`.  `DDI_BUF_CTL_B` was declared
 *     where `TRANS_DDI_FUNC_CTL_B` was not because section 8.4's own table
 *     tabulates Port A and Port B side by side and section 8.1 says the
 *     bring-up port is a choice between those two combo PHY ports, while the
 *     transcoder stays A.
 *   - `DDI_BUF_CTL` for the Type-C ports (TC1-TC4): section 8.8 defers the DKL
 *     PHY path and section 8.4's table gives only ports A and B.
 *   - `PIPE_MISC`, `TRANSCONF`, the transcoder timing registers and the plane
 *     registers named in section 11 phases 4 and 5: they are the pipe and
 *     timing groups, not this one.
 */
