// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of Linux v7.2.3 drivers/gpu/drm/i915/display/intel_cmtg.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::{
    intel_cmtg_regs_upstream::{
        CMTG_CLK_SEL, CMTG_CLK_SEL_A_DISABLED, CMTG_CLK_SEL_A_MASK, CMTG_CLK_SEL_B_DISABLED,
        CMTG_CLK_SEL_B_MASK, CMTG_ENABLE, TRANS_CMTG_CTL_A, TRANS_CMTG_CTL_B,
    },
    intel_de_upstream::{intel_de_read, intel_de_rmw},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{TRANSCODER_A, TRANSCODER_B},
    intel_display_power_types_upstream::{POWER_DOMAIN_TRANSCODER_A, POWER_DOMAIN_TRANSCODER_B},
    intel_display_power_upstream::{
        intel_display_power_get_if_enabled, intel_display_power_put_unchecked,
    },
    intel_display_regs_upstream::{CMTG_SECONDARY_MODE, TRANS_DDI_FUNC_CTL2},
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IntelCmtgConfig {
    cmtg_a_enable: bool,
    cmtg_b_enable: bool,
    trans_a_secondary: bool,
    trans_b_secondary: bool,
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    unsafe { (*display).info.__runtime_info.ip.ver }
}

#[inline]
fn cmtg_supported(display_ver: u16, is_dg2: bool) -> bool {
    !is_dg2 && display_ver >= 13
}

#[inline]
unsafe fn has_cmtg(display: *const IntelDisplay) -> bool {
    // Linux HAS_CMTG: !platform.dg2 && DISPLAY_VER >= 13. DG2's display IP
    // is earlier than 13, but retain the platform test as upstream does.
    let platform = unsafe { (*display).platform._anon_0_0.bitmap[0] };
    let is_dg2 = platform & (1u64 << 56) != 0;
    cmtg_supported(unsafe { display_ver(display) }, is_dg2)
}

#[inline]
unsafe fn has_transcoder(display: *const IntelDisplay, transcoder: u32) -> bool {
    unsafe { (*display).info.__runtime_info.cpu_transcoder_mask & (1u8 << transcoder) != 0 }
}

#[inline]
unsafe fn transcoder_is_secondary(
    display: *mut IntelDisplay,
    transcoder: u32,
    power_domain: u32,
) -> bool {
    if !unsafe { has_transcoder(display, transcoder) } {
        return false;
    }

    // with_intel_display_power_if_enabled() deliberately avoids waking a
    // transcoder during state readout. A temporary reference brackets MMIO.
    let wakeref = unsafe { intel_display_power_get_if_enabled(display, power_domain) };
    if wakeref.is_null() {
        return false;
    }
    let reg = unsafe { TRANS_DDI_FUNC_CTL2(display, transcoder) };
    let val = unsafe { intel_de_read(display, reg) };
    unsafe { intel_display_power_put_unchecked(display, power_domain) };
    val & CMTG_SECONDARY_MODE != 0
}

unsafe fn get_config(display: *mut IntelDisplay) -> IntelCmtgConfig {
    let mut config = IntelCmtgConfig::default();
    let val = unsafe { intel_de_read(display, TRANS_CMTG_CTL_A) };
    config.cmtg_a_enable = val & CMTG_ENABLE != 0;

    if unsafe { display_ver(display) >= 20 } {
        let val = unsafe { intel_de_read(display, TRANS_CMTG_CTL_B) };
        config.cmtg_b_enable = val & CMTG_ENABLE != 0;
    }

    config.trans_a_secondary =
        unsafe { transcoder_is_secondary(display, TRANSCODER_A as u32, POWER_DOMAIN_TRANSCODER_A) };
    config.trans_b_secondary =
        unsafe { transcoder_is_secondary(display, TRANSCODER_B as u32, POWER_DOMAIN_TRANSCODER_B) };
    config
}

#[inline]
fn disable_requires_modeset(display_ver: u16, config: &IntelCmtgConfig) -> bool {
    display_ver < 20 && (config.trans_a_secondary || config.trans_b_secondary)
}

unsafe fn disable(display: *mut IntelDisplay, config: &IntelCmtgConfig) {
    let mut clk_sel_clr = 0;
    let mut clk_sel_set = 0;

    if config.trans_a_secondary {
        let reg = unsafe { TRANS_DDI_FUNC_CTL2(display, TRANSCODER_A as u32) };
        unsafe { intel_de_rmw(display, reg, CMTG_SECONDARY_MODE, 0) };
    }
    if config.trans_b_secondary {
        let reg = unsafe { TRANS_DDI_FUNC_CTL2(display, TRANSCODER_B as u32) };
        unsafe { intel_de_rmw(display, reg, CMTG_SECONDARY_MODE, 0) };
    }

    if config.cmtg_a_enable {
        unsafe { intel_de_rmw(display, TRANS_CMTG_CTL_A, CMTG_ENABLE, 0) };
        clk_sel_clr |= CMTG_CLK_SEL_A_MASK;
        clk_sel_set |= CMTG_CLK_SEL_A_DISABLED;
    }
    if config.cmtg_b_enable {
        unsafe { intel_de_rmw(display, TRANS_CMTG_CTL_B, CMTG_ENABLE, 0) };
        clk_sel_clr |= CMTG_CLK_SEL_B_MASK;
        clk_sel_set |= CMTG_CLK_SEL_B_DISABLED;
    }

    if unsafe { display_ver(display) >= 14 } && clk_sel_clr != 0 {
        unsafe { intel_de_rmw(display, CMTG_CLK_SEL, clk_sel_clr, clk_sel_set) };
    }
}

// upstream: intel_cmtg.c intel_cmtg_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cmtg_sanitize(display: *mut IntelDisplay) {
    if display.is_null() || !unsafe { has_cmtg(display) } {
        return;
    }

    let config = unsafe { get_config(display) };
    // The upstream driver cannot safely turn off CMTG while a transcoder is
    // synchronized to it on pre-Xe2 hardware; defer those cases to a modeset.
    if disable_requires_modeset(unsafe { display_ver(display) }, &config) {
        return;
    }
    unsafe { disable(display, &config) };
}

#[cfg(test)]
mod tests {
    use super::{IntelCmtgConfig, cmtg_supported, disable_requires_modeset};

    #[test]
    fn cmtg_capability_is_display_ip_gated_and_excludes_dg2() {
        assert!(!cmtg_supported(12, false));
        assert!(cmtg_supported(13, false));
        assert!(!cmtg_supported(13, true));
    }

    #[test]
    fn cmtg_disable_requires_modeset_only_for_old_secondary_transcoders() {
        let mut config = IntelCmtgConfig::default();
        assert!(!disable_requires_modeset(13, &config));
        config.trans_a_secondary = true;
        assert!(disable_requires_modeset(13, &config));
        assert!(!disable_requires_modeset(20, &config));
    }
}
