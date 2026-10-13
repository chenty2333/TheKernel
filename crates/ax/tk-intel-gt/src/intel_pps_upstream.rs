// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Shared panel power sequencer entry points.
//!
//! ADL-P external DP SST has no eDP panel sequencer.  Its AUX path still uses
//! the upstream PPS lock/VDD contracts: non-eDP VDD operations are no-ops and
//! `intel_pps_vdd_on_unlocked()` returns false.  eDP sequencing is kept
//! explicitly unavailable rather than reporting a fabricated successful
//! panel power transition.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of, ptr};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{INVALID_PIPE, Pipe},
    intel_display_power_types_upstream::POWER_DOMAIN_DISPLAY_CORE,
    intel_display_types_upstream::{
        IntelConnector, IntelCrtcState, IntelDigitalPort, IntelDp, IntelEncoder,
    },
};

unsafe fn display_from_dp(dp: *mut IntelDp) -> *mut IntelDisplay {
    if dp.is_null() {
        return ptr::null_mut();
    }
    let offset = offset_of!(IntelDigitalPort, dp);
    let dig = unsafe { dp.cast::<u8>().sub(offset).cast::<IntelDigitalPort>() };
    let dev = unsafe { (*dig).base.base.dev };
    if dev.is_null() {
        ptr::null_mut()
    } else {
        unsafe { crate::intel_display_conversion_upstream::__drm_to_display(dev) }
    }
}

// upstream: intel_pps.c intel_pps_lock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_lock(dp: *mut IntelDp) -> *mut c_void {
    let display = unsafe { display_from_dp(dp) };
    if display.is_null() {
        return ptr::null_mut();
    }
    let wakeref = unsafe {
        crate::intel_display_power_upstream::intel_display_power_get(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
        )
    };
    unsafe {
        tk_linux_kapi::mutex::mutex_lock(ptr::addr_of_mut!((*display).pps.mutex));
    }
    wakeref
}

// upstream: intel_pps.c intel_pps_unlock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_unlock(dp: *mut IntelDp, wakeref: *mut c_void) -> *mut c_void {
    let display = unsafe { display_from_dp(dp) };
    if display.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        tk_linux_kapi::mutex::mutex_unlock(ptr::addr_of_mut!((*display).pps.mutex));
        crate::intel_display_power_upstream::intel_display_power_put(
            display,
            POWER_DOMAIN_DISPLAY_CORE,
            wakeref,
        );
    }
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_vdd_on_unlocked(dp: *mut IntelDp) -> bool {
    // Source contract: VDD is a panel-only resource.  External DP has no VDD.
    if dp.is_null() {
        return false;
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_vdd_off_unlocked(_dp: *mut IntelDp, _sync: bool) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_check_power_unlocked(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_on_unlocked(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_off_unlocked(_dp: *mut IntelDp) {}

unsafe fn with_pps_lock(dp: *mut IntelDp, action: unsafe extern "C" fn(*mut IntelDp)) {
    if dp.is_null() {
        return;
    }
    let wakeref = unsafe { intel_pps_lock(dp) };
    if wakeref.is_null() {
        return;
    }
    unsafe {
        action(dp);
        intel_pps_unlock(dp, wakeref);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_vdd_on(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_vdd_off(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_on(dp: *mut IntelDp) {
    unsafe { with_pps_lock(dp, intel_pps_on_unlocked) };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_off(dp: *mut IntelDp) {
    unsafe { with_pps_lock(dp, intel_pps_off_unlocked) };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_vdd_off_sync(dp: *mut IntelDp) {
    if dp.is_null() {
        return;
    }
    let wakeref = unsafe { intel_pps_lock(dp) };
    if !wakeref.is_null() {
        unsafe {
            intel_pps_vdd_off_unlocked(dp, true);
            intel_pps_unlock(dp, wakeref);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_have_panel_power_or_vdd(_dp: *mut IntelDp) -> bool {
    false
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_wait_power_cycle(_dp: *mut IntelDp) {}

// eDP-only sequencing is intentionally fail-closed for this external-SST
// implementation.  AUX users must never infer panel power from this result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_init(_dp: *mut IntelDp) -> bool {
    false
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_init_late(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_encoder_reset(_dp: *mut IntelDp) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_backlight_on(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_backlight_off(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_backlight_power(_connector: *mut IntelConnector, _enable: bool) {
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_pps_pipe_init(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_pps_pipe_reset(_dp: *mut IntelDp) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_pps_backlight_initial_pipe(_dp: *mut IntelDp) -> Pipe {
    INVALID_PIPE
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_pps_port_enable_unlocked(
    _encoder: *mut IntelEncoder,
    _state: *const IntelCrtcState,
) {
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_pps_port_disable(
    _encoder: *mut IntelEncoder,
    _state: *const IntelCrtcState,
) {
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_pps_reset_all(_display: *mut IntelDisplay) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bxt_pps_reset_all(_display: *mut IntelDisplay) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_connector_debugfs_add(_connector: *mut IntelConnector) {}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn assert_pps_unlocked(_display: *mut IntelDisplay, _pipe: Pipe) {}

fn display_has_ddi(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let info = unsafe { (*display).info.__device_info };
    !info.is_null() && unsafe { (*info)._bitfields_46[0] & (1 << 3) != 0 }
}

// upstream no-ops on ADL-P because it has DDI and no panel sequencer registers
// that need the legacy write-unlock workaround.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_unlock_regs_wa(display: *mut IntelDisplay) {
    if display.is_null()
        || unsafe { (*display).info.__runtime_info.pipe_mask == 0 }
        || display_has_ddi(display)
    {
        return;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pps_setup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    use crate::{
        intel_display_limits_types_upstream::{I915_MAX_PIPES, Pipe},
        intel_pch_types_upstream::{
            PCH_CNP, PCH_CPT, PCH_IBX, PCH_ICP, PCH_LPT_H, PCH_LPT_LP, PCH_SPT, PCH_TGP,
        },
        intel_pps_regs_upstream::{PCH_PPS_BASE, PPS_BASE, VLV_PPS_BASE},
    };
    let pch = unsafe { (*display).pch_type };
    let pch_split = matches!(
        pch,
        PCH_IBX | PCH_CPT | PCH_LPT_H | PCH_LPT_LP | PCH_SPT | PCH_CNP | PCH_ICP | PCH_TGP
    );
    let platform = unsafe { (*display).platform._anon_0_0.bitmap[0] };
    let valleyview_or_cherryview = platform & ((1u64 << 20) | (1u64 << 27)) != 0;
    let base = if pch_split {
        PCH_PPS_BASE
    } else if valleyview_or_cherryview {
        VLV_PPS_BASE
    } else {
        PPS_BASE
    };
    unsafe { (*display).pps.mmio_base = base };
}

#[cfg(test)]
mod tests {
    #[test]
    fn external_dp_vdd_is_not_claimed() {
        assert!(!unsafe { super::intel_pps_vdd_on_unlocked(core::ptr::null_mut()) });
    }
}
