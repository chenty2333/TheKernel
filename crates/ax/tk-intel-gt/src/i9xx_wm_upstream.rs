// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/i9xx_wm.c (Linux v7.2.3).
//! Pre-Ironlake watermark helpers: the CxSR (self-refresh) enable toggle.
#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};

use crate::{
    intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_write},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_reg_defs_upstream::mmio,
    intel_display_regs_upstream::{FW_BLC_SELF, FW_BLC_SELF_EN, FW_BLC_SELF_VLV, FW_CSPWRDWNEN},
    intel_wm_types_upstream::{G4xWmValues, VlvWmValues},
    intel_workarounds_types_upstream::I915RegT,
};

/// `INSTPM` (linux/registers.rs), `DSPFW3(dev_priv)` (i9xx_wm_regs.h).
const INSTPM: I915RegT = mmio(0x20c0);

/// `DSPFW3(dev_priv)` (i9xx_wm_regs.h).
#[inline]
unsafe fn DSPFW3(display: *mut IntelDisplay) -> I915RegT {
    mmio(unsafe { crate::intel_display_reg_defs_upstream::DISPLAY_MMIO_BASE(display) } + 0x7003c)
}

/// `PINEVIEW_SELF_REFRESH_EN` (i9xx_wm_regs.h).
const PINEVIEW_SELF_REFRESH_EN: u32 = 1 << 30;
/// `INSTPM_SELF_EN` (include/drm/intel/intel_gmd_misc_regs.h).
const INSTPM_SELF_EN: u32 = 1 << 12;

#[inline]
unsafe fn platform_has(display: *const IntelDisplay, bit: u32) -> bool {
    unsafe { (*display).platform._anon_0_0.bitmap[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0 }
}

// Platform bit indices in the display platform bitmap (declaration order of
// intel_display_device.h's platform list).
const PLATFORM_G4X: u32 = 0;
const PLATFORM_I915GM: u32 = 8;
const PLATFORM_I945G: u32 = 9;
const PLATFORM_I945GM: u32 = 10;
const PLATFORM_PINEVIEW: u32 = 12;
const PLATFORM_I965GM: u32 = 14;
const PLATFORM_VALLEYVIEW: u32 = 20;
const PLATFORM_CHERRYVIEW: u32 = 27;

/// `REG_MASKED_FIELD_ENABLE(v)` / `REG_MASKED_FIELD_DISABLE(v)` (reg_bits.h).
#[inline]
const fn masked_field_enable(v: u32) -> u32 {
    (v << 16) | v
}

#[inline]
const fn masked_field_disable(v: u32) -> u32 {
    v << 16
}

// upstream: i9xx_wm.c _intel_set_memory_cxsr()
unsafe fn _intel_set_memory_cxsr(display: *mut IntelDisplay, enable: bool) -> bool {
    let was_enabled: bool;

    unsafe {
        if platform_has(display, PLATFORM_VALLEYVIEW) || platform_has(display, PLATFORM_CHERRYVIEW)
        {
            was_enabled = intel_de_read(display, FW_BLC_SELF_VLV) & FW_CSPWRDWNEN != 0;
            intel_de_write(
                display,
                FW_BLC_SELF_VLV,
                if enable { FW_CSPWRDWNEN } else { 0 },
            );
            intel_de_posting_read(display, FW_BLC_SELF_VLV);
        } else if platform_has(display, PLATFORM_G4X) || platform_has(display, PLATFORM_I965GM) {
            was_enabled = intel_de_read(display, FW_BLC_SELF) & FW_BLC_SELF_EN != 0;
            intel_de_write(
                display,
                FW_BLC_SELF,
                if enable { FW_BLC_SELF_EN } else { 0 },
            );
            intel_de_posting_read(display, FW_BLC_SELF);
        } else if platform_has(display, PLATFORM_PINEVIEW) {
            let dspfw3 = DSPFW3(display);
            let val = intel_de_read(display, dspfw3);
            was_enabled = val & PINEVIEW_SELF_REFRESH_EN != 0;
            let val = if enable {
                val | PINEVIEW_SELF_REFRESH_EN
            } else {
                val & !PINEVIEW_SELF_REFRESH_EN
            };
            intel_de_write(display, dspfw3, val);
            intel_de_posting_read(display, dspfw3);
        } else if platform_has(display, PLATFORM_I945G) || platform_has(display, PLATFORM_I945GM) {
            was_enabled = intel_de_read(display, FW_BLC_SELF) & FW_BLC_SELF_EN != 0;
            let val = if enable {
                masked_field_enable(FW_BLC_SELF_EN)
            } else {
                masked_field_disable(FW_BLC_SELF_EN)
            };
            intel_de_write(display, FW_BLC_SELF, val);
            intel_de_posting_read(display, FW_BLC_SELF);
        } else if platform_has(display, PLATFORM_I915GM) {
            // FIXME in Linux: no bit like this for 915G; kept as written.
            was_enabled = intel_de_read(display, INSTPM) & INSTPM_SELF_EN != 0;
            let val = if enable {
                masked_field_enable(INSTPM_SELF_EN)
            } else {
                masked_field_disable(INSTPM_SELF_EN)
            };
            intel_de_write(display, INSTPM, val);
            intel_de_posting_read(display, INSTPM);
        } else {
            return false;
        }
    }

    was_enabled
}

/// `intel_set_memory_cxsr()`: allow or disallow CxSR; returns the previous state.
// upstream: i9xx_wm.c intel_set_memory_cxsr()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_set_memory_cxsr(display: *mut IntelDisplay, enable: bool) -> bool {
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*display).wm.wm_mutex));
        let ret = _intel_set_memory_cxsr(display, enable);
        if platform_has(display, PLATFORM_VALLEYVIEW) || platform_has(display, PLATFORM_CHERRYVIEW)
        {
            (*core::ptr::addr_of_mut!((*display).wm._anon_56_5.vlv).cast::<VlvWmValues>()).cxsr =
                enable;
        } else if platform_has(display, PLATFORM_G4X) {
            (*core::ptr::addr_of_mut!((*display).wm._anon_56_5.g4x).cast::<G4xWmValues>()).cxsr =
                enable;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*display).wm.wm_mutex));
        ret
    }
}
