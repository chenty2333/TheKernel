// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_dpll.c (Linux v7.2.3).
#![allow(non_snake_case, dead_code, unsafe_code, unsafe_op_in_unsafe_fn)]

use core::{ffi::c_void, ptr};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{IntelAtomicState, IntelCrtc, IntelCrtcState, IntelEncoder},
    intel_display_wa_upstream::DISPLAY_VER,
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
struct IntelDpllGlobalFuncs {
    crtc_compute_clock: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc) -> i32>,
    crtc_get_dpll: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc) -> i32>,
}

static HSW_DPLL_FUNCS: IntelDpllGlobalFuncs = IntelDpllGlobalFuncs {
    crtc_compute_clock: Some(hsw_crtc_compute_clock),
    crtc_get_dpll: Some(hsw_crtc_get_dpll),
};

unsafe fn display_from_atomic(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() || (*state).base.dev.is_null() || (*(*state).base.dev).dev_private.is_null()
    {
        return ptr::null_mut();
    }
    (*(*(*state).base.dev).dev_private.cast::<DrmI915Private>())
        .display
        .cast::<IntelDisplay>()
}

unsafe fn atomic_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> *mut IntelCrtcState {
    tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_new_crtc_state(
        ptr::addr_of!((*state).base),
        ptr::addr_of_mut!((*crtc).base),
    )
    .cast()
}

unsafe extern "C" {
    fn intel_get_crtc_new_encoder(
        state: *mut IntelAtomicState,
        crtc_state: *const IntelCrtcState,
    ) -> *mut IntelEncoder;
    fn intel_crtc_dotclock(crtc_state: *const IntelCrtcState) -> i32;
    fn intel_crtc_has_type(crtc_state: *const IntelCrtcState, output_type: u32) -> bool;
    fn intel_dpll_compute(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
        encoder: *mut IntelEncoder,
    ) -> i32;
    fn intel_dpll_reserve(
        state: *mut IntelAtomicState,
        crtc: *mut IntelCrtc,
        encoder: *mut IntelEncoder,
    ) -> i32;
}

unsafe extern "C" fn hsw_crtc_compute_clock(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    let crtc_state = atomic_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let encoder = intel_get_crtc_new_encoder(state, crtc_state);
    if encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let ret = intel_dpll_compute(state, crtc, encoder);
    if ret != 0 {
        return ret;
    }

    // DSI's clocking is handled separately by its PHY PLL. ADLP's external
    // DP/HDMI paths use the port clock selected by their DPLL manager.
    if intel_crtc_has_type(
        crtc_state,
        crate::intel_display_types_upstream::INTEL_OUTPUT_DSI,
    ) {
        return 0;
    }
    if !(*crtc_state).has_pch_encoder {
        (*crtc_state).hw.adjusted_mode.crtc_clock = intel_crtc_dotclock(crtc_state);
    }
    0
}

unsafe extern "C" fn hsw_crtc_get_dpll(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> i32 {
    let crtc_state = atomic_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    if !(*crtc_state).hw.enable || !(*crtc_state).intel_dpll.is_null() {
        return 0;
    }
    let encoder = intel_get_crtc_new_encoder(state, crtc_state);
    if encoder.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    intel_dpll_reserve(state, crtc, encoder)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_init_clock_hook(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    if DISPLAY_VER(display) >= 9 {
        (*display).dpll.funcs = ptr::addr_of!(HSW_DPLL_FUNCS) as *mut c_void;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_crtc_compute_clock(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    let display = display_from_atomic(state);
    if display.is_null() || crtc.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let crtc_state = atomic_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    ptr::write_bytes(ptr::addr_of_mut!((*crtc_state).dpll_hw_state), 0, 1);
    if !(*crtc_state).hw.enable {
        return 0;
    }
    let funcs = (*display).dpll.funcs.cast::<IntelDpllGlobalFuncs>();
    if funcs.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*funcs)
        .crtc_compute_clock
        .map_or(0, |compute| compute(state, crtc))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_crtc_get_dpll(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> i32 {
    let display = display_from_atomic(state);
    if display.is_null() || crtc.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    let crtc_state = atomic_crtc_state(state, crtc);
    if crtc_state.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    if !(*crtc_state).hw.enable || !(*crtc_state).intel_dpll.is_null() {
        return 0;
    }
    let funcs = (*display).dpll.funcs.cast::<IntelDpllGlobalFuncs>();
    if funcs.is_null() {
        return -tk_linux_kapi::config::EINVAL;
    }
    (*funcs).crtc_get_dpll.map_or(0, |get| get(state, crtc))
}
