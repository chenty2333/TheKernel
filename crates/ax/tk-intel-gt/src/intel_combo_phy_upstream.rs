// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P combo-PHY initialization and lane power from Linux v7.2.3.
#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::{
    intel_combo_phy_regs_upstream::*,
    intel_de_upstream::{intel_de_read, intel_de_rmw, intel_de_write},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_regs_upstream::{ICL_PHY_MISC, ICL_PHY_MISC_DE_IO_COMP_PWR_DOWN},
    intel_display_wa_upstream::DISPLAY_VER,
};

const PHY_A: i32 = 0;
const PHY_B: i32 = 1;
const VOLTAGE_PROCESS_MASK: u32 = PROCESS_INFO_MASK | VOLTAGE_INFO_MASK;
const PROC_DW1_MASK: u32 = (0xff << 16) | 0xff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcmonValues {
    dw1: u32,
    dw9: u32,
    dw10: u32,
}

const PROCMON_085_DOT0: ProcmonValues = ProcmonValues {
    dw1: 0x0000_0000,
    dw9: 0x62ab_67bb,
    dw10: 0x5191_4f96,
};
const PROCMON_095_DOT0: ProcmonValues = ProcmonValues {
    dw1: 0x0000_0000,
    dw9: 0x86e1_72c7,
    dw10: 0x77ca_5eab,
};
const PROCMON_095_DOT1: ProcmonValues = ProcmonValues {
    dw1: 0x0000_0000,
    dw9: 0x93f8_7fe1,
    dw10: 0x8ae8_71c5,
};
const PROCMON_105_DOT0: ProcmonValues = ProcmonValues {
    dw1: 0x0000_0000,
    dw9: 0x98fa_82dd,
    dw10: 0x89e4_6dc1,
};
const PROCMON_105_DOT1: ProcmonValues = ProcmonValues {
    dw1: 0x0044_0000,
    dw9: 0x9a00_ab25,
    dw10: 0x8ae3_8ff1,
};

#[inline]
fn adlp_platform(display: *const IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    // `alderlake_p` is bit 52 in the Linux platform bitset.
    unsafe {
        let flags = core::ptr::addr_of!((*display).platform).cast::<u8>();
        *flags.add(52 / 8) & (1 << (52 % 8)) != 0
    }
}

fn procmon_ref_values(value: u32) -> ProcmonValues {
    match value & VOLTAGE_PROCESS_MASK {
        x if x == (VOLTAGE_INFO_0_95V | PROCESS_INFO_DOT_0) => PROCMON_095_DOT0,
        x if x == (VOLTAGE_INFO_0_95V | PROCESS_INFO_DOT_1) => PROCMON_095_DOT1,
        x if x == (VOLTAGE_INFO_1_05V | PROCESS_INFO_DOT_0) => PROCMON_105_DOT0,
        x if x == (VOLTAGE_INFO_1_05V | PROCESS_INFO_DOT_1) => PROCMON_105_DOT1,
        // The Linux switch falls through to the 0.85V/dot0 defaults for
        // unrecognized fuse encodings after reporting MISSING_CASE.
        _ => PROCMON_085_DOT0,
    }
}

unsafe fn check_phy_reg(
    display: *mut IntelDisplay,
    reg: crate::intel_workarounds_types_upstream::I915RegT,
    mask: u32,
    expected: u32,
) -> bool {
    unsafe { intel_de_read(display, reg) & mask == expected }
}

unsafe fn get_procmon_ref_values(display: *mut IntelDisplay, phy: i32) -> ProcmonValues {
    procmon_ref_values(unsafe { intel_de_read(display, ICL_PORT_COMP_DW3(phy as u32)) })
}

unsafe fn combo_phy_enabled(display: *mut IntelDisplay, phy: i32) -> bool {
    let powerdown = unsafe {
        intel_de_read(display, ICL_PHY_MISC(phy as u32)) & ICL_PHY_MISC_DE_IO_COMP_PWR_DOWN != 0
    };
    let comp_init =
        unsafe { intel_de_read(display, ICL_PORT_COMP_DW0(phy as u32)) & COMP_INIT != 0 };
    !powerdown && comp_init
}

unsafe fn verify_procmon_ref_values(display: *mut IntelDisplay, phy: i32) -> bool {
    let values = unsafe { get_procmon_ref_values(display, phy) };
    unsafe {
        check_phy_reg(
            display,
            ICL_PORT_COMP_DW1(phy as u32),
            PROC_DW1_MASK,
            values.dw1,
        ) && check_phy_reg(display, ICL_PORT_COMP_DW9(phy as u32), u32::MAX, values.dw9)
            && check_phy_reg(
                display,
                ICL_PORT_COMP_DW10(phy as u32),
                u32::MAX,
                values.dw10,
            )
    }
}

unsafe fn combo_phy_verify_state(display: *mut IntelDisplay, phy: i32) -> bool {
    if !unsafe { combo_phy_enabled(display, phy) } {
        return false;
    }
    let mut ret = true;
    unsafe {
        ret &= check_phy_reg(
            display,
            ICL_PORT_TX_DW8_LN(0, phy as u32),
            ICL_PORT_TX_DW8_ODCC_CLK_SEL | ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_MASK,
            ICL_PORT_TX_DW8_ODCC_CLK_SEL | ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_DIV2,
        );
        ret &= check_phy_reg(
            display,
            ICL_PORT_PCS_DW1_LN(0, phy as u32),
            DCC_MODE_SELECT_MASK,
            RUN_DCC_ONCE,
        );
        ret &= verify_procmon_ref_values(display, phy);
        if phy == PHY_A {
            ret &= check_phy_reg(display, ICL_PORT_COMP_DW8(phy as u32), IREFGEN, IREFGEN);
        }
        ret &= check_phy_reg(
            display,
            ICL_PORT_CL_DW5(phy as u32),
            CL_POWER_DOWN_ENABLE,
            CL_POWER_DOWN_ENABLE,
        );
    }
    ret
}

unsafe fn set_procmon_ref_values(display: *mut IntelDisplay, phy: i32) {
    let values = unsafe { get_procmon_ref_values(display, phy) };
    unsafe {
        intel_de_rmw(
            display,
            ICL_PORT_COMP_DW1(phy as u32),
            PROC_DW1_MASK,
            values.dw1,
        );
        intel_de_write(display, ICL_PORT_COMP_DW9(phy as u32), values.dw9);
        intel_de_write(display, ICL_PORT_COMP_DW10(phy as u32), values.dw10);
    }
}

unsafe fn combo_phy_init_one(display: *mut IntelDisplay, phy: i32) {
    if unsafe { combo_phy_verify_state(display, phy) } {
        return;
    }

    let mut value = unsafe { intel_de_read(display, ICL_PHY_MISC(phy as u32)) };
    value &= !ICL_PHY_MISC_DE_IO_COMP_PWR_DOWN;
    unsafe { intel_de_write(display, ICL_PHY_MISC(phy as u32), value) };

    value = unsafe { intel_de_read(display, ICL_PORT_TX_DW8_LN(0, phy as u32)) };
    value &= !ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_MASK;
    value |= ICL_PORT_TX_DW8_ODCC_CLK_SEL | ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_DIV2;
    unsafe { intel_de_write(display, ICL_PORT_TX_DW8_GRP(phy as u32), value) };

    value = unsafe { intel_de_read(display, ICL_PORT_PCS_DW1_LN(0, phy as u32)) };
    value &= !DCC_MODE_SELECT_MASK;
    value |= RUN_DCC_ONCE;
    unsafe { intel_de_write(display, ICL_PORT_PCS_DW1_GRP(phy as u32), value) };

    unsafe { set_procmon_ref_values(display, phy) };
    if phy == PHY_A {
        unsafe { intel_de_rmw(display, ICL_PORT_COMP_DW8(phy as u32), 0, IREFGEN) };
    }
    unsafe {
        intel_de_rmw(display, ICL_PORT_COMP_DW0(phy as u32), 0, COMP_INIT);
        intel_de_rmw(
            display,
            ICL_PORT_CL_DW5(phy as u32),
            0,
            CL_POWER_DOWN_ENABLE,
        );
    }
}

/// Initialize the ADL-P combo PHY A/B compensation, DCC/ODCC and procmon state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_combo_phy_init(display: *mut IntelDisplay) {
    if display.is_null() || !adlp_platform(display) || unsafe { DISPLAY_VER(display) != 13 } {
        return;
    }
    for phy in [PHY_A, PHY_B] {
        unsafe { combo_phy_init_one(display, phy) };
    }
}

/// Power down the ADL-P combo PHYs in reverse order, after checking PHY-A state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_combo_phy_uninit(display: *mut IntelDisplay) {
    if display.is_null() || !adlp_platform(display) || unsafe { DISPLAY_VER(display) != 13 } {
        return;
    }
    for phy in [PHY_B, PHY_A] {
        if phy == PHY_A {
            let _ =
                tk_linux_kapi::assert::warn_on(!unsafe { combo_phy_verify_state(display, phy) });
        }
        unsafe {
            intel_de_rmw(
                display,
                ICL_PHY_MISC(phy as u32),
                0,
                ICL_PHY_MISC_DE_IO_COMP_PWR_DOWN,
            );
            intel_de_rmw(display, ICL_PORT_COMP_DW0(phy as u32), COMP_INIT, 0);
        }
    }
}

fn combo_lane_power_mask(lane_count: i32, lane_reversal: bool, is_dsi: bool) -> Option<u32> {
    Some(if is_dsi {
        if lane_reversal {
            return None;
        }
        match lane_count {
            1 => PWR_DOWN_LN_3_1_0,
            2 => PWR_DOWN_LN_3_1,
            3 => PWR_DOWN_LN_3,
            4 => PWR_UP_ALL_LANES,
            _ => return None,
        }
    } else {
        match lane_count {
            1 if lane_reversal => PWR_DOWN_LN_2_1_0,
            1 => PWR_DOWN_LN_3_2_1,
            2 if lane_reversal => PWR_DOWN_LN_1_0,
            2 => PWR_DOWN_LN_3_2,
            4 => PWR_UP_ALL_LANES,
            _ => return None,
        }
    })
}

/// Program the combo-PHY lane power mask using the exact DSI/DP lane mapping.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_combo_phy_power_up_lanes(
    display: *mut IntelDisplay,
    phy: i32,
    is_dsi: bool,
    lane_count: i32,
    lane_reversal: bool,
) {
    if display.is_null()
        || !adlp_platform(display)
        || unsafe { DISPLAY_VER(display) != 13 }
        || !matches!(phy, PHY_A | PHY_B)
    {
        return;
    }
    let Some(power_mask) = combo_lane_power_mask(lane_count, lane_reversal, is_dsi) else {
        return;
    };
    unsafe {
        intel_de_rmw(
            display,
            ICL_PORT_CL_DW10(phy as u32),
            PWR_DOWN_LN_MASK,
            power_mask,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procmon_values_cover_each_adlp_fuse_encoding() {
        assert_eq!(
            procmon_ref_values(VOLTAGE_INFO_0_85V | PROCESS_INFO_DOT_0),
            PROCMON_085_DOT0
        );
        assert_eq!(
            procmon_ref_values(VOLTAGE_INFO_0_95V | PROCESS_INFO_DOT_0),
            PROCMON_095_DOT0
        );
        assert_eq!(
            procmon_ref_values(VOLTAGE_INFO_0_95V | PROCESS_INFO_DOT_1),
            PROCMON_095_DOT1
        );
        assert_eq!(
            procmon_ref_values(VOLTAGE_INFO_1_05V | PROCESS_INFO_DOT_0),
            PROCMON_105_DOT0
        );
        assert_eq!(
            procmon_ref_values(VOLTAGE_INFO_1_05V | PROCESS_INFO_DOT_1),
            PROCMON_105_DOT1
        );
        assert_eq!(procmon_ref_values(u32::MAX), PROCMON_085_DOT0);
    }

    #[test]
    fn combo_lane_power_masks_match_lane_reversal_rules() {
        assert_eq!(
            combo_lane_power_mask(1, false, false),
            Some(PWR_DOWN_LN_3_2_1)
        );
        assert_eq!(
            combo_lane_power_mask(1, true, false),
            Some(PWR_DOWN_LN_2_1_0)
        );
        assert_eq!(
            combo_lane_power_mask(2, false, false),
            Some(PWR_DOWN_LN_3_2)
        );
        assert_eq!(combo_lane_power_mask(2, true, false), Some(PWR_DOWN_LN_1_0));
        assert_eq!(
            combo_lane_power_mask(4, false, false),
            Some(PWR_UP_ALL_LANES)
        );
        assert_eq!(combo_lane_power_mask(3, false, false), None);
        assert_eq!(combo_lane_power_mask(3, false, true), Some(PWR_DOWN_LN_3));
        assert_eq!(combo_lane_power_mask(1, true, true), None);
    }
}
