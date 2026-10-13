// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display HTI handling, translated from `drivers/gpu/drm/i915/display/intel_hti.c`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use crate::intel_de_upstream::intel_de_read;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_reg_defs_upstream::reg_field_get;
use crate::intel_hti_regs_upstream::{
    HDPORT_DDI_USED, HDPORT_DPLL_USED_MASK, HDPORT_ENABLED, HDPORT_STATE,
};

pub type Phy = i32;
pub const PHY_NONE: Phy = -1;
pub const PHY_A: Phy = 0;
pub const PHY_B: Phy = 1;
pub const PHY_C: Phy = 2;
pub const PHY_D: Phy = 3;
pub const PHY_E: Phy = 4;
pub const PHY_F: Phy = 5;
pub const PHY_G: Phy = 6;
pub const PHY_H: Phy = 7;
pub const PHY_I: Phy = 8;
pub const I915_MAX_PHYS: Phy = 9;

#[inline]
unsafe fn has_hti(display: *const IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let info = (*display).info.__device_info;
    if info.is_null() {
        return false;
    }
    // bit-fields in _bitfields_46[1]: has_hotplug:1 (bit 0), has_hti:1 (bit 1)
    ((*info)._bitfields_46[1] & 0x02) != 0
}

// upstream: intel_hti.c intel_hti_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hti_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    /*
     * If the platform has HTI, we need to find out whether it has reserved
     * any display resources before we create our display outputs.
     */
    if has_hti(display) {
        (*display).hti.state = intel_de_read(display, HDPORT_STATE);
    }
}

// upstream: intel_hti.c intel_hti_uses_phy(struct intel_display *display, enum phy phy)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hti_uses_phy(display: *mut IntelDisplay, phy: Phy) -> bool {
    if display.is_null() || phy == PHY_NONE {
        return false;
    }

    ((*display).hti.state & HDPORT_ENABLED != 0)
        && ((*display).hti.state & HDPORT_DDI_USED(phy as u32) != 0)
}

// upstream: intel_hti.c intel_hti_dpll_mask(struct intel_display *display)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hti_dpll_mask(display: *mut IntelDisplay) -> u32 {
    if display.is_null() || ((*display).hti.state & HDPORT_ENABLED == 0) {
        return 0;
    }

    /*
     * Note: This is subtle. The values must coincide with what's defined
     * for the platform.
     */
    reg_field_get(HDPORT_DPLL_USED_MASK, (*display).hti.state)
}
