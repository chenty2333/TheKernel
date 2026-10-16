// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/hsw_ips.c (Linux v7.2.3).
//! Haswell/Broadwell pixel-pipe IPS disable path.
#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_drm_upstream::drm_device_types::DrmDevice;

use crate::{
    intel_de_upstream::{intel_de_posting_read, intel_de_wait_for_clear_ms, intel_de_write},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_regs_upstream::{IPS_CTL, IPS_ENABLE},
    intel_display_types_upstream::IntelCrtcState,
    intel_parent_upstream::intel_parent_pcode_write,
};

/// `DISPLAY_IPS_CONTROL` (include/drm/intel/intel_pcode_regs.h).
const DISPLAY_IPS_CONTROL: u32 = 0x19;

/// Bit index of `platform.broadwell` in the display platform bitmap.
const PLATFORM_BROADWELL: u32 = 24;

#[inline]
unsafe fn platform_has(display: *const IntelDisplay, bit: u32) -> bool {
    unsafe { (*display).platform._anon_0_0.bitmap[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0 }
}

/// `to_intel_display(crtc_state)`.
#[inline]
unsafe fn display_of_crtc_state(crtc_state: *const IntelCrtcState) -> *mut IntelDisplay {
    unsafe {
        crate::intel_display_conversion_upstream::__drm_to_display(
            (*(*crtc_state).uapi.crtc).dev as *mut DrmDevice,
        )
    }
}

/// `hsw_ips_disable()`: returns true when the caller must wait for a vblank
/// before disabling the plane.
// upstream: hsw_ips.c hsw_ips_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hsw_ips_disable(crtc_state: *const IntelCrtcState) -> bool {
    let display = unsafe { display_of_crtc_state(crtc_state) };
    let mut need_vblank_wait = false;

    if !unsafe { (*crtc_state).ips_enabled } {
        return need_vblank_wait;
    }

    if unsafe { platform_has(display, PLATFORM_BROADWELL) } {
        let ret = unsafe { intel_parent_pcode_write(display, DISPLAY_IPS_CONTROL, 0) };
        // drm_WARN_ON(display->drm, intel_parent_pcode_write(...))
        drm_WARN_ON!(unsafe { (*display).drm }, ret != 0);
        // Wait for PCODE to finish disabling IPS (100 ms, see hsw_ips.c).
        if unsafe { intel_de_wait_for_clear_ms(display, IPS_CTL, IPS_ENABLE, 100) } != 0 {
            axlog::error!("i915 display: timed out waiting for IPS disable");
        }
    } else {
        unsafe {
            intel_de_write(display, IPS_CTL, 0);
            intel_de_posting_read(display, IPS_CTL);
        }
    }

    // We need to wait for a vblank before we can disable the plane.
    need_vblank_wait = true;

    need_vblank_wait
}
