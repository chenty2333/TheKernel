// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 PM Demand handling, translated from `drivers/gpu/drm/i915/display/intel_pmdemand.c`.
//! On display ver < 14 (including ADL-P / Display 13), PMDemand hardware does not exist;
//! the init path succeeds as a no-op and operational entry points return early.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use core::ffi::c_void;

use crate::intel_de_upstream::intel_de_rmw;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_regs_upstream::{DMD_RSP_TIMEOUT_DISABLE, XELPD_CHICKEN_DCPR_3};
use crate::intel_display_wa_upstream::{__intel_display_wa, IntelDisplayWa};
use crate::intel_global_state_types_upstream::{
    IntelGlobalObj, IntelGlobalState, IntelGlobalStateFuncs,
};
use crate::linux_memory::{kfree, kzalloc_obj};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PmdemandParams {
    pub qclk_gv_bw: u16,
    pub voltage_index: u8,
    pub qclk_gv_index: u8,
    pub active_pipes: u8,
    pub active_dbufs: u8,
    pub active_phys: u8,
    pub plls: u8,
    pub cdclk_freq_mhz: u16,
    pub ddiclk_max: u16,
    pub scalers: u8,
}

#[repr(C)]
pub struct IntelPmdemandState {
    pub base: IntelGlobalState,
    pub ddi_clocks: [i32; 4], // I915_MAX_PIPES
    pub active_combo_phys_mask: u16,
    pub params: PmdemandParams,
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).info.__runtime_info.ip.ver }
    }
}

// upstream: intel_pmdemand.c to_intel_pmdemand_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn to_intel_pmdemand_state(
    obj_state: *mut IntelGlobalState,
) -> *mut IntelPmdemandState {
    if obj_state.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe {
            (obj_state as *mut u8).sub(core::mem::offset_of!(IntelPmdemandState, base))
                as *mut IntelPmdemandState
        }
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_duplicate_state()
unsafe extern "C" fn intel_pmdemand_duplicate_state(
    obj: *mut IntelGlobalObj,
) -> *mut IntelGlobalState {
    if obj.is_null() || unsafe { (*obj).state.is_null() } {
        return core::ptr::null_mut();
    }
    let src = unsafe { to_intel_pmdemand_state((*obj).state) };
    let dst = kzalloc_obj::<IntelPmdemandState>();
    if dst.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, 1);
        core::ptr::addr_of_mut!((*dst).base)
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_destroy_state()
unsafe extern "C" fn intel_pmdemand_destroy_state(
    _obj: *mut IntelGlobalObj,
    state: *mut IntelGlobalState,
) {
    if !state.is_null() {
        let pmdemand_state = unsafe { to_intel_pmdemand_state(state) };
        kfree(pmdemand_state as *mut c_void);
    }
}

static INTEL_PMDEMAND_FUNCS: IntelGlobalStateFuncs = IntelGlobalStateFuncs {
    atomic_duplicate_state: Some(intel_pmdemand_duplicate_state),
    atomic_destroy_state: Some(intel_pmdemand_destroy_state),
};

unsafe extern "C" {
    fn intel_atomic_global_obj_init(
        display: *mut IntelDisplay,
        obj: *mut IntelGlobalObj,
        state: *mut IntelGlobalState,
        funcs: *const IntelGlobalStateFuncs,
    );
}

// upstream: intel_pmdemand.c intel_pmdemand_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_init_early(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*display).pmdemand.lock));
        tk_linux_kapi::wait::init_waitqueue_head(&mut (*display).pmdemand.waitqueue);
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return 0;
    }

    let pmdemand_state = kzalloc_obj::<IntelPmdemandState>();
    if pmdemand_state.is_null() {
        return -12; // -ENOMEM
    }

    unsafe {
        intel_atomic_global_obj_init(
            display,
            core::ptr::addr_of_mut!((*display).pmdemand.obj),
            core::ptr::addr_of_mut!((*pmdemand_state).base),
            core::ptr::addr_of!(INTEL_PMDEMAND_FUNCS),
        );

        // Wa_14016740474
        if __intel_display_wa(display, IntelDisplayWa::Wa14016740474, core::ptr::null()) {
            intel_de_rmw(display, XELPD_CHICKEN_DCPR_3, 0, DMD_RSP_TIMEOUT_DISABLE);
        }
    }

    0
}

// upstream: intel_pmdemand.c intel_pmdemand_update_phys_mask()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_update_phys_mask(
    display: *mut IntelDisplay,
    _encoder: *mut c_void,
    _pmdemand_state: *mut IntelPmdemandState,
    _clear_all: bool,
) {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return;
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_update_port_clock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_update_port_clock(
    display: *mut IntelDisplay,
    _pmdemand_state: *mut IntelPmdemandState,
    _pipe: i32,
    _port_clock: i32,
) {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return;
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_atomic_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_atomic_check(_state: *mut c_void) -> i32 {
    0
}

// upstream: intel_pmdemand.c intel_pmdemand_init_pmdemand_params()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_init_pmdemand_params(
    display: *mut IntelDisplay,
    _pmdemand_state: *mut IntelPmdemandState,
) {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return;
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_program_dbuf()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_program_dbuf(
    display: *mut IntelDisplay,
    _dbuf_slices: u8,
) {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return;
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_pre_plane_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_pre_plane_update(_state: *mut c_void) {}

// upstream: intel_pmdemand.c intel_pmdemand_post_plane_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_post_plane_update(_state: *mut c_void) {}
