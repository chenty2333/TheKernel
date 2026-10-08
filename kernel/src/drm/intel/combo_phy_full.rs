// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_combo_phy.c.
// Copyright © 2018 Intel Corporation. Full grant: ../../../../LICENSE-MIT.

//! Source-faithful translation of the combo-PHY part of i915's display
//! bring-up.  The caller supplies the platform facts, VBT port-presence facts,
//! and the combo-PHY register instances; the register access and diagnostic
//! adapters here keep the ordering and decisions of the upstream file visible.

use alloc::{format, string::String, vec::Vec};

use super::regs::{ComboPhyRegisters, Register, Registers};

const PHY_A: u8 = 0;
const PHY_C: u8 = 2;
const PHY_D: u8 = 3;
const I915_MAX_PHYS: u8 = 9;

const PROCESS_INFO_MASK: u32 = 0x7 << 26;
const PROCESS_INFO_DOT_0: u32 = 0 << 26;
const PROCESS_INFO_DOT_1: u32 = 1 << 26;
const VOLTAGE_INFO_MASK: u32 = 0x3 << 24;
const VOLTAGE_INFO_0_85V: u32 = 0 << 24;
const VOLTAGE_INFO_0_95V: u32 = 1 << 24;
const VOLTAGE_INFO_1_05V: u32 = 2 << 24;
const PROC_DW1_MASK: u32 = (0xff << 16) | 0xff;
const COMP_INIT: u32 = 1 << 31;
const IREFGEN: u32 = 1 << 24;
const ODCC_CLK_SEL: u32 = 1 << 31;
const ODCC_CLK_DIV_SEL_MASK: u32 = 0b11 << 29;
const ODCC_CLK_DIV_SEL_DIV2: u32 = 1 << 29;
const DCC_MODE_SELECT_MASK: u32 = 0b11 << 20;
const RUN_DCC_ONCE: u32 = 0;
const CL_POWER_DOWN_ENABLE: u32 = 1 << 4;
const DE_IO_COMP_PWR_DOWN: u32 = 1 << 23;
const PHY_MISC_MUX_DDID: u32 = 1 << 28;
const PWR_DOWN_LN_MASK: u32 = 0xf << 4;
const PWR_UP_ALL_LANES: u32 = 0x0 << 4;
const PWR_DOWN_LN_3_2_1: u32 = 0xe << 4;
const PWR_DOWN_LN_3_2: u32 = 0xc << 4;
const PWR_DOWN_LN_3: u32 = 0x8 << 4;
const PWR_DOWN_LN_2_1_0: u32 = 0x7 << 4;
const PWR_DOWN_LN_1_0: u32 = 0x3 << 4;
const PWR_DOWN_LN_3_1: u32 = 0xa << 4;
const PWR_DOWN_LN_3_1_0: u32 = 0xb << 4;

/// The source's five reference-value slots, in their declared order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
enum ProcmonIndex {
    V0_85Dot0,
    V0_95Dot0,
    V0_95Dot1,
    V1_05Dot0,
    V1_05Dot1,
}

struct ProcmonValues {
    name: &'static str,
    dw1: u32,
    dw9: u32,
    dw10: u32,
}

static ICL_PROCMON_VALUES: [ProcmonValues; 5] = [
    ProcmonValues {
        name: "0.85V dot0 (low-voltage)",
        dw1: 0x0000_0000,
        dw9: 0x62ab_67bb,
        dw10: 0x5191_4f96,
    },
    ProcmonValues {
        name: "0.95V dot0",
        dw1: 0x0000_0000,
        dw9: 0x86e1_72c7,
        dw10: 0x77ca_5eab,
    },
    ProcmonValues {
        name: "0.95V dot1",
        dw1: 0x0000_0000,
        dw9: 0x93f8_7fe1,
        dw10: 0x8ae8_71c5,
    },
    ProcmonValues {
        name: "1.05V dot0",
        dw1: 0x0000_0000,
        dw9: 0x98fa_82dd,
        dw10: 0x89e4_6dc1,
    },
    ProcmonValues {
        name: "1.05V dot1",
        dw1: 0x0044_0000,
        dw9: 0x9a00_ab25,
        dw10: 0x8ae3_8ff1,
    },
];

/// Platform predicates consumed by the upstream conditions in this file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ComboPhyPlatform {
    pub(crate) display_version: u8,
    pub(crate) alderlake_s: bool,
    pub(crate) jasperlake: bool,
    pub(crate) elkhartlake: bool,
    pub(crate) rocketlake: bool,
    pub(crate) dg1: bool,
    pub(crate) tigerlake: bool,
}

/// The three BIOS/VBT presence queries used to choose EHL's PHY-A mux.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct VbtPortPresence {
    pub(crate) ddi_a_present: bool,
    pub(crate) ddi_d_present: bool,
    pub(crate) dsi_present: bool,
}

/// A PHY number paired with its register bank.  The slice supplied to
/// [`ComboPhyDisplay`] may contain only PHYs which the platform reports as
/// combo PHYs; iteration still uses the upstream A-to-I order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ComboPhyInstance {
    pub(crate) phy: u8,
    pub(crate) registers: ComboPhyRegisters,
}

/// The display facts needed by this source file's functions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ComboPhyDisplay<'a> {
    pub(crate) platform: ComboPhyPlatform,
    pub(crate) vbt_ports: VbtPortPresence,
    pub(crate) phys: &'a [ComboPhyInstance],
}

/// Log records corresponding to i915's DRM and missing-case diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticLevel {
    Debug,
    Warning,
    Error,
    MissingCase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComboPhyDiagnostic {
    pub(crate) level: DiagnosticLevel,
    pub(crate) message: String,
}

/// Register I/O failures are possible in the bounded Rust aperture interface;
/// i915's `intel_de_read`/`write` interface does not express them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComboPhyIoError {
    Read(Register),
    Write(Register),
}

fn phy_name(phy: u8) -> char {
    (b'A' + phy) as char
}

fn find_phy(display: &ComboPhyDisplay<'_>, phy: u8) -> Option<ComboPhyInstance> {
    display
        .phys
        .iter()
        .copied()
        .find(|instance| instance.phy == phy)
}

fn de_read(regs: &impl Registers, register: Register) -> Result<u32, ComboPhyIoError> {
    regs.read(register).ok_or(ComboPhyIoError::Read(register))
}

fn de_write(regs: &impl Registers, register: Register, value: u32) -> Result<(), ComboPhyIoError> {
    if regs.write(register, value) {
        Ok(())
    } else {
        Err(ComboPhyIoError::Write(register))
    }
}

fn de_rmw(
    regs: &impl Registers,
    register: Register,
    clear: u32,
    set: u32,
) -> Result<(), ComboPhyIoError> {
    let value = de_read(regs, register)?;
    de_write(regs, register, (value & !clear) | set)
}

fn push_diagnostic(
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
    level: DiagnosticLevel,
    message: String,
) {
    diagnostics.push(ComboPhyDiagnostic { level, message });
}

fn procmon_index(value: u32) -> Option<ProcmonIndex> {
    match value & (PROCESS_INFO_MASK | VOLTAGE_INFO_MASK) {
        x if x == (VOLTAGE_INFO_0_85V | PROCESS_INFO_DOT_0) => Some(ProcmonIndex::V0_85Dot0),
        x if x == (VOLTAGE_INFO_0_95V | PROCESS_INFO_DOT_0) => Some(ProcmonIndex::V0_95Dot0),
        x if x == (VOLTAGE_INFO_0_95V | PROCESS_INFO_DOT_1) => Some(ProcmonIndex::V0_95Dot1),
        x if x == (VOLTAGE_INFO_1_05V | PROCESS_INFO_DOT_0) => Some(ProcmonIndex::V1_05Dot0),
        x if x == (VOLTAGE_INFO_1_05V | PROCESS_INFO_DOT_1) => Some(ProcmonIndex::V1_05Dot1),
        _ => None,
    }
}

// upstream: intel_combo_phy.c icl_get_procmon_ref_values()
fn icl_get_procmon_ref_values<'a>(
    regs: &impl Registers,
    phy: ComboPhyInstance,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<&'a ProcmonValues, ComboPhyIoError> {
    let val = de_read(regs, phy.registers.comp_dw3)?;
    let index = match procmon_index(val) {
        Some(index) => index,
        None => {
            push_diagnostic(
                diagnostics,
                DiagnosticLevel::MissingCase,
                format!("MISSING_CASE({val:#x})"),
            );
            ProcmonIndex::V0_85Dot0
        }
    };
    // The array is static and every `ProcmonIndex` is a valid slot.
    Ok(&ICL_PROCMON_VALUES[index as usize])
}

#[cfg(test)]
mod procmon_tests {
    use super::*;

    #[test]
    fn procmon_voltage_and_process_fields_are_combined_as_bitfields() {
        assert_eq!(procmon_index(0), Some(ProcmonIndex::V0_85Dot0));
        assert_eq!(
            procmon_index(VOLTAGE_INFO_0_95V),
            Some(ProcmonIndex::V0_95Dot0)
        );
        assert_eq!(
            procmon_index(VOLTAGE_INFO_0_95V | PROCESS_INFO_DOT_1),
            Some(ProcmonIndex::V0_95Dot1)
        );
        assert_eq!(
            procmon_index(VOLTAGE_INFO_1_05V),
            Some(ProcmonIndex::V1_05Dot0)
        );
        assert_eq!(
            procmon_index(VOLTAGE_INFO_1_05V | PROCESS_INFO_DOT_1),
            Some(ProcmonIndex::V1_05Dot1)
        );
        assert_eq!(procmon_index(PROCESS_INFO_DOT_1), None);
    }
}

// upstream: intel_combo_phy.c icl_set_procmon_ref_values()
fn icl_set_procmon_ref_values(
    regs: &impl Registers,
    phy: ComboPhyInstance,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<(), ComboPhyIoError> {
    let procmon = icl_get_procmon_ref_values(regs, phy, diagnostics)?;
    de_rmw(regs, phy.registers.comp_dw1, PROC_DW1_MASK, procmon.dw1)?;
    de_write(regs, phy.registers.comp_dw9, procmon.dw9)?;
    de_write(regs, phy.registers.comp_dw10, procmon.dw10)
}

// upstream: intel_combo_phy.c check_phy_reg()
fn check_phy_reg(
    regs: &impl Registers,
    phy: ComboPhyInstance,
    reg: Register,
    mask: u32,
    expected_val: u32,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<bool, ComboPhyIoError> {
    let val = de_read(regs, reg)?;
    if val & mask != expected_val {
        push_diagnostic(
            diagnostics,
            DiagnosticLevel::Debug,
            format!(
                "Combo PHY {} reg {:08x} state mismatch: current {:08x} mask {:08x} expected \
                 {:08x}",
                phy_name(phy.phy),
                reg.offset(),
                val,
                mask,
                expected_val,
            ),
        );
        return Ok(false);
    }
    Ok(true)
}

// upstream: intel_combo_phy.c icl_verify_procmon_ref_values()
fn icl_verify_procmon_ref_values(
    regs: &impl Registers,
    phy: ComboPhyInstance,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<bool, ComboPhyIoError> {
    let procmon = icl_get_procmon_ref_values(regs, phy, diagnostics)?;
    let mut ret = true;
    ret &= check_phy_reg(
        regs,
        phy,
        phy.registers.comp_dw1,
        PROC_DW1_MASK,
        procmon.dw1,
        diagnostics,
    )?;
    ret &= check_phy_reg(
        regs,
        phy,
        phy.registers.comp_dw9,
        u32::MAX,
        procmon.dw9,
        diagnostics,
    )?;
    ret &= check_phy_reg(
        regs,
        phy,
        phy.registers.comp_dw10,
        u32::MAX,
        procmon.dw10,
        diagnostics,
    )?;
    Ok(ret)
}

// upstream: intel_combo_phy.c has_phy_misc()
fn has_phy_misc(platform: ComboPhyPlatform, phy: u8) -> bool {
    // Some platforms only expect PHY_MISC to be programmed for PHY-A and PHY-B
    // and may not even have instances of the register for the other combo PHYs.
    // ADL-S has three PHY_MISC instances but needs programming only on PHY A.
    if platform.alderlake_s {
        phy == PHY_A
    } else if platform.jasperlake || platform.elkhartlake || platform.rocketlake || platform.dg1 {
        phy < PHY_C
    } else {
        true
    }
}

// upstream: intel_combo_phy.c icl_combo_phy_enabled()
fn icl_combo_phy_enabled(
    regs: &impl Registers,
    display: &ComboPhyDisplay<'_>,
    phy: ComboPhyInstance,
) -> Result<bool, ComboPhyIoError> {
    // EHL's PHY C has no PHY_MISC register.
    if !has_phy_misc(display.platform, phy.phy) {
        Ok(de_read(regs, phy.registers.comp_dw0)? & COMP_INIT != 0)
    } else {
        let misc = de_read(regs, phy.registers.phy_misc)?;
        if misc & DE_IO_COMP_PWR_DOWN != 0 {
            return Ok(false);
        }
        Ok(de_read(regs, phy.registers.comp_dw0)? & COMP_INIT != 0)
    }
}

// upstream: intel_combo_phy.c ehl_vbt_ddi_d_present()
fn ehl_vbt_ddi_d_present(
    display: &ComboPhyDisplay<'_>,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> bool {
    let ddi_a_present = display.vbt_ports.ddi_a_present;
    let ddi_d_present = display.vbt_ports.ddi_d_present;
    let dsi_present = display.vbt_ports.dsi_present;

    // VBT child-device `dvo port` names the DDI, not the PHY.  Thus an external
    // device driven by combo PHY A appears on PORT_D, with neither PORT_A nor
    // DSI present.
    if ddi_d_present && !ddi_a_present && !dsi_present {
        return true;
    }

    // If VBT says DDI-D is external while A/DSI is internal, keep the internal
    // display and leave an error in the log.
    if ddi_d_present {
        push_diagnostic(
            diagnostics,
            DiagnosticLevel::Error,
            "VBT claims to have both internal and external displays on PHY A. Configuring for \
             internal."
                .into(),
        );
    }
    false
}

// upstream: intel_combo_phy.c phy_is_master()
fn phy_is_master(platform: ComboPhyPlatform, phy: u8) -> bool {
    // Some PHYs attach to compensation resistors and are masters for other PHYs:
    // ICL/TGL A -> B,C; RKL/DG1 A -> B and C -> D; ADL-S A -> B,C and D -> E.
    // Every master needs IREFGEN.
    if phy == PHY_A {
        true
    } else if platform.alderlake_s {
        phy == PHY_D
    } else if platform.dg1 || platform.rocketlake {
        phy == PHY_C
    } else {
        false
    }
}

// upstream: intel_combo_phy.c icl_combo_phy_verify_state()
fn icl_combo_phy_verify_state(
    regs: &impl Registers,
    display: &ComboPhyDisplay<'_>,
    phy: ComboPhyInstance,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<bool, ComboPhyIoError> {
    let mut ret = true;
    let mut expected_val = 0;

    if !icl_combo_phy_enabled(regs, display, phy)? {
        return Ok(false);
    }

    if display.platform.display_version >= 12 {
        ret &= check_phy_reg(
            regs,
            phy,
            phy.registers.tx_dw8_ln0,
            ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_MASK,
            ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_DIV2,
            diagnostics,
        )?;
        ret &= check_phy_reg(
            regs,
            phy,
            phy.registers.pcs_dw1_ln0,
            DCC_MODE_SELECT_MASK,
            RUN_DCC_ONCE,
            diagnostics,
        )?;
    }

    ret &= icl_verify_procmon_ref_values(regs, phy, diagnostics)?;

    if phy_is_master(display.platform, phy.phy) {
        ret &= check_phy_reg(
            regs,
            phy,
            phy.registers.comp_dw8,
            IREFGEN,
            IREFGEN,
            diagnostics,
        )?;

        if display.platform.jasperlake || display.platform.elkhartlake {
            if ehl_vbt_ddi_d_present(display, diagnostics) {
                expected_val = PHY_MISC_MUX_DDID;
            }
            ret &= check_phy_reg(
                regs,
                phy,
                phy.registers.phy_misc,
                PHY_MISC_MUX_DDID,
                expected_val,
                diagnostics,
            )?;
        }
    }

    ret &= check_phy_reg(
        regs,
        phy,
        phy.registers.cl_dw5,
        CL_POWER_DOWN_ENABLE,
        CL_POWER_DOWN_ENABLE,
        diagnostics,
    )?;
    Ok(ret)
}

// upstream: intel_combo_phy.c intel_combo_phy_power_up_lanes()
pub(crate) fn intel_combo_phy_power_up_lanes(
    regs: &impl Registers,
    cl_dw10: Register,
    is_dsi: bool,
    lane_count: i32,
    lane_reversal: bool,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<(), ComboPhyIoError> {
    let lane_mask;

    if is_dsi {
        if lane_reversal {
            push_diagnostic(
                diagnostics,
                DiagnosticLevel::Warning,
                "WARN_ON(lane_reversal) for DSI combo PHY".into(),
            );
        }
        lane_mask = match lane_count {
            1 => PWR_DOWN_LN_3_1_0,
            2 => PWR_DOWN_LN_3_1,
            3 => PWR_DOWN_LN_3,
            _ => {
                if lane_count != 4 {
                    push_diagnostic(
                        diagnostics,
                        DiagnosticLevel::MissingCase,
                        format!("MISSING_CASE({lane_count})"),
                    );
                }
                PWR_UP_ALL_LANES
            }
        };
    } else {
        lane_mask = match lane_count {
            1 if lane_reversal => PWR_DOWN_LN_2_1_0,
            1 => PWR_DOWN_LN_3_2_1,
            2 if lane_reversal => PWR_DOWN_LN_1_0,
            2 => PWR_DOWN_LN_3_2,
            _ => {
                if lane_count != 4 {
                    push_diagnostic(
                        diagnostics,
                        DiagnosticLevel::MissingCase,
                        format!("MISSING_CASE({lane_count})"),
                    );
                }
                PWR_UP_ALL_LANES
            }
        };
    }

    de_rmw(regs, cl_dw10, PWR_DOWN_LN_MASK, lane_mask)
}

// upstream: intel_combo_phy.c icl_combo_phys_init()
fn icl_combo_phys_init(
    regs: &impl Registers,
    display: &ComboPhyDisplay<'_>,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<(), ComboPhyIoError> {
    for phy_number in PHY_A..I915_MAX_PHYS {
        let Some(phy) = find_phy(display, phy_number) else {
            continue;
        };
        let mut val;

        if icl_combo_phy_verify_state(regs, display, phy, diagnostics)? {
            continue;
        }

        let procmon = icl_get_procmon_ref_values(regs, phy, diagnostics)?;
        push_diagnostic(
            diagnostics,
            DiagnosticLevel::Debug,
            format!(
                "Initializing combo PHY {} (Voltage/Process Info : {})",
                phy_name(phy.phy),
                procmon.name,
            ),
        );

        if has_phy_misc(display.platform, phy.phy) {
            // EHL's PHY A can drive an external display through DDI-D or an
            // internal display through DDI-A/DSI.  This board choice is fixed,
            // so initialise its mux from the VBT's internal-child devices.
            val = de_read(regs, phy.registers.phy_misc)?;
            if (display.platform.jasperlake || display.platform.elkhartlake) && phy.phy == PHY_A {
                val &= !PHY_MISC_MUX_DDID;
                if ehl_vbt_ddi_d_present(display, diagnostics) {
                    val |= PHY_MISC_MUX_DDID;
                }
            }
            val &= !DE_IO_COMP_PWR_DOWN;
            de_write(regs, phy.registers.phy_misc, val)?;
        }

        if display.platform.display_version >= 12 {
            val = de_read(regs, phy.registers.tx_dw8_ln0)?;
            val &= !ODCC_CLK_DIV_SEL_MASK;
            val |= ODCC_CLK_SEL;
            val |= ODCC_CLK_DIV_SEL_DIV2;
            de_write(regs, phy.registers.tx_dw8, val)?;

            val = de_read(regs, phy.registers.pcs_dw1_ln0)?;
            val &= !DCC_MODE_SELECT_MASK;
            val |= RUN_DCC_ONCE;
            de_write(regs, phy.registers.pcs_dw1, val)?;
        }

        icl_set_procmon_ref_values(regs, phy, diagnostics)?;

        if phy_is_master(display.platform, phy.phy) {
            de_rmw(regs, phy.registers.comp_dw8, 0, IREFGEN)?;
        }

        de_rmw(regs, phy.registers.comp_dw0, 0, COMP_INIT)?;
        de_rmw(regs, phy.registers.cl_dw5, 0, CL_POWER_DOWN_ENABLE)?;
    }
    Ok(())
}

// upstream: intel_combo_phy.c icl_combo_phys_uninit()
fn icl_combo_phys_uninit(
    regs: &impl Registers,
    display: &ComboPhyDisplay<'_>,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<(), ComboPhyIoError> {
    for phy_number in (PHY_A..I915_MAX_PHYS).rev() {
        let Some(phy) = find_phy(display, phy_number) else {
            continue;
        };

        if phy.phy == PHY_A && !icl_combo_phy_verify_state(regs, display, phy, diagnostics)? {
            let level = if display.platform.tigerlake || display.platform.dg1 {
                // Suppress the known old-ifwi CI warning, as upstream does.
                DiagnosticLevel::Debug
            } else {
                DiagnosticLevel::Warning
            };
            push_diagnostic(
                diagnostics,
                level,
                format!(
                    "Combo PHY {} HW state changed unexpectedly",
                    phy_name(phy.phy)
                ),
            );
        }

        if has_phy_misc(display.platform, phy.phy) {
            de_rmw(regs, phy.registers.phy_misc, 0, DE_IO_COMP_PWR_DOWN)?;
        }
        de_rmw(regs, phy.registers.comp_dw0, COMP_INIT, 0)?;
    }
    Ok(())
}

// upstream: intel_combo_phy.c intel_combo_phy_init()
pub(crate) fn intel_combo_phy_init(
    regs: &impl Registers,
    display: &ComboPhyDisplay<'_>,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<(), ComboPhyIoError> {
    icl_combo_phys_init(regs, display, diagnostics)
}

// upstream: intel_combo_phy.c intel_combo_phy_uninit()
pub(crate) fn intel_combo_phy_uninit(
    regs: &impl Registers,
    display: &ComboPhyDisplay<'_>,
    diagnostics: &mut Vec<ComboPhyDiagnostic>,
) -> Result<(), ComboPhyIoError> {
    icl_combo_phys_uninit(regs, display, diagnostics)
}
