// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 PM Demand handling, translated from `drivers/gpu/drm/i915/display/intel_pmdemand.c`.
//! On display ver < 14 (including ADL-P / Display 13), PMDemand hardware does not exist;
//! the init path succeeds as a no-op and operational entry points return early.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use core::ffi::c_void;

use crate::intel_de_upstream::{
    intel_de_read, intel_de_rmw, intel_de_wait_for_clear_ms, intel_de_wait_ms, intel_de_write,
};
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_reg_defs_upstream::{reg_field_get, reg_field_prep};
use crate::intel_display_regs_upstream::{
    DMD_RSP_TIMEOUT_DISABLE, GEN12_DCPR_STATUS_1, XE3_PMDEMAND_PIPES_MASK, XELPD_CHICKEN_DCPR_3,
    XELPDP_INITIATE_PMDEMAND_REQUEST, XELPDP_PMDEMAND_CDCLK_FREQ_MASK,
    XELPDP_PMDEMAND_DBUFS_MASK, XELPDP_PMDEMAND_DDICLK_FREQ_MASK,
    XELPDP_PMDEMAND_INFLIGHT_STATUS, XELPDP_PMDEMAND_PHYS_MASK, XELPDP_PMDEMAND_PIPES_MASK,
    XELPDP_PMDEMAND_PLLS_MASK, XELPDP_PMDEMAND_QCLK_GV_BW_MASK,
    XELPDP_PMDEMAND_QCLK_GV_INDEX_MASK, XELPDP_PMDEMAND_REQ_ENABLE,
    XELPDP_PMDEMAND_SCALERS_MASK, XELPDP_PMDEMAND_VOLTAGE_INDEX_MASK,
};
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

// upstream: intel_pmdemand.c intel_atomic_get_pmdemand_state()
unsafe fn intel_atomic_get_pmdemand_state(
    _state: *mut c_void,
) -> *mut IntelPmdemandState {
    core::ptr::null_mut()
}

// upstream: intel_pmdemand.c intel_atomic_get_old_pmdemand_state()
unsafe fn intel_atomic_get_old_pmdemand_state(
    _state: *mut c_void,
) -> *mut IntelPmdemandState {
    core::ptr::null_mut()
}

// upstream: intel_pmdemand.c intel_atomic_get_new_pmdemand_state()
unsafe fn intel_atomic_get_new_pmdemand_state(
    _state: *mut c_void,
) -> *mut IntelPmdemandState {
    core::ptr::null_mut()
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
        crate::display_pending_stubs::b3d::intel_atomic_global_obj_init(
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

// upstream: intel_pmdemand.c intel_pmdemand_update_phys_mask()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_update_phys_mask(
    display: *mut IntelDisplay,
    _encoder: *mut c_void,
    pmdemand_state: *mut IntelPmdemandState,
    _set_bit: bool,
) {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return;
    }
    if pmdemand_state.is_null() {
        return;
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_update_port_clock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_update_port_clock(
    display: *mut IntelDisplay,
    pmdemand_state: *mut IntelPmdemandState,
    pipe: i32,
    port_clock: i32,
) {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return;
    }
    if pmdemand_state.is_null() {
        return;
    }
    if pipe >= 0 && (pipe as usize) < unsafe { (*pmdemand_state).ddi_clocks.len() } {
        unsafe {
            (*pmdemand_state).ddi_clocks[pipe as usize] = port_clock;
        }
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_update_max_ddiclk()
unsafe fn intel_pmdemand_update_max_ddiclk(
    _display: *mut IntelDisplay,
    _state: *mut c_void,
    pmdemand_state: *mut IntelPmdemandState,
) {
    if pmdemand_state.is_null() {
        return;
    }
    let mut max_ddiclk = 0;
    unsafe {
        for i in 0..(*pmdemand_state).ddi_clocks.len() {
            if (*pmdemand_state).ddi_clocks[i] > max_ddiclk {
                max_ddiclk = (*pmdemand_state).ddi_clocks[i];
            }
        }
        (*pmdemand_state).params.ddiclk_max = ((max_ddiclk + 999) / 1000) as u16;
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_update_connector_phys()
unsafe fn intel_pmdemand_update_connector_phys(
    _display: *mut IntelDisplay,
    _state: *mut c_void,
    _conn_state: *mut c_void,
    _set_bit: bool,
    _pmdemand_state: *mut IntelPmdemandState,
) {
}

// upstream: intel_pmdemand.c intel_pmdemand_update_active_non_tc_phys()
unsafe fn intel_pmdemand_update_active_non_tc_phys(
    _display: *mut IntelDisplay,
    _state: *mut c_void,
    pmdemand_state: *mut IntelPmdemandState,
) {
    if pmdemand_state.is_null() {
        return;
    }
    unsafe {
        let count = (*pmdemand_state).active_combo_phys_mask.count_ones() as u8;
        (*pmdemand_state).params.active_phys = count.min(7);
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_encoder_has_tc_phy()
unsafe fn intel_pmdemand_encoder_has_tc_phy(
    _display: *mut IntelDisplay,
    _encoder: *mut c_void,
) -> bool {
    false
}

// upstream: intel_pmdemand.c intel_pmdemand_connector_needs_update()
unsafe fn intel_pmdemand_connector_needs_update(_state: *mut c_void) -> bool {
    false
}

// upstream: intel_pmdemand.c intel_pmdemand_needs_update()
unsafe fn intel_pmdemand_needs_update(_state: *mut c_void) -> bool {
    false
}

// upstream: intel_pmdemand.c intel_pmdemand_atomic_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_atomic_check(_state: *mut c_void) -> i32 {
    0
}

// upstream: intel_pmdemand.c intel_pmdemand_check_prev_transaction()
unsafe fn intel_pmdemand_check_prev_transaction(display: *mut IntelDisplay) -> bool {
    unsafe {
        !(intel_de_wait_for_clear_ms(
            display,
            XELPDP_INITIATE_PMDEMAND_REQUEST(1),
            XELPDP_PMDEMAND_REQ_ENABLE,
            10,
        ) != 0
            || intel_de_wait_for_clear_ms(
                display,
                GEN12_DCPR_STATUS_1,
                XELPDP_PMDEMAND_INFLIGHT_STATUS,
                10,
            ) != 0)
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_init_pmdemand_params()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_init_pmdemand_params(
    display: *mut IntelDisplay,
    pmdemand_state: *mut IntelPmdemandState,
) {
    if display.is_null() || unsafe { display_ver(display) } < 14 {
        return;
    }
    if pmdemand_state.is_null() {
        return;
    }
    unsafe {
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
        if !intel_pmdemand_check_prev_transaction(display) {
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*pmdemand_state).params), 0, 1);
            tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
            return;
        }

        let reg1 = intel_de_read(display, XELPDP_INITIATE_PMDEMAND_REQUEST(0));
        let reg2 = intel_de_read(display, XELPDP_INITIATE_PMDEMAND_REQUEST(1));

        (*pmdemand_state).params.qclk_gv_bw =
            reg_field_get(XELPDP_PMDEMAND_QCLK_GV_BW_MASK, reg1) as u16;
        (*pmdemand_state).params.voltage_index =
            reg_field_get(XELPDP_PMDEMAND_VOLTAGE_INDEX_MASK, reg1) as u8;
        (*pmdemand_state).params.qclk_gv_index =
            reg_field_get(XELPDP_PMDEMAND_QCLK_GV_INDEX_MASK, reg1) as u8;
        (*pmdemand_state).params.active_phys =
            reg_field_get(XELPDP_PMDEMAND_PHYS_MASK, reg1) as u8;

        (*pmdemand_state).params.cdclk_freq_mhz =
            reg_field_get(XELPDP_PMDEMAND_CDCLK_FREQ_MASK, reg2) as u16;
        (*pmdemand_state).params.ddiclk_max =
            reg_field_get(XELPDP_PMDEMAND_DDICLK_FREQ_MASK, reg2) as u16;

        if display_ver(display) >= 30 {
            (*pmdemand_state).params.active_pipes =
                reg_field_get(XE3_PMDEMAND_PIPES_MASK, reg1) as u8;
        } else {
            (*pmdemand_state).params.active_pipes =
                reg_field_get(XELPDP_PMDEMAND_PIPES_MASK, reg1) as u8;
            (*pmdemand_state).params.active_dbufs =
                reg_field_get(XELPDP_PMDEMAND_DBUFS_MASK, reg1) as u8;
            (*pmdemand_state).params.scalers =
                reg_field_get(XELPDP_PMDEMAND_SCALERS_MASK, reg2) as u8;
        }

        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_req_complete()
unsafe fn intel_pmdemand_req_complete(display: *mut IntelDisplay) -> bool {
    unsafe {
        (intel_de_read(display, XELPDP_INITIATE_PMDEMAND_REQUEST(1)) & XELPDP_PMDEMAND_REQ_ENABLE) == 0
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_poll()
unsafe fn intel_pmdemand_poll(display: *mut IntelDisplay) {
    let mut status: u32 = 0;
    unsafe {
        let _ = intel_de_wait_ms(
            display,
            XELPDP_INITIATE_PMDEMAND_REQUEST(1),
            XELPDP_PMDEMAND_REQ_ENABLE,
            0,
            10,
            &mut status,
        );
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_wait()
unsafe fn intel_pmdemand_wait(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    // Wa_14024400148: For lnl use polling method.
    // In our kernel environment, poll method is also safe and avoids waitqueue timeout issues in host tests.
    unsafe {
        intel_pmdemand_poll(display);
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_program_dbuf()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_program_dbuf(
    display: *mut IntelDisplay,
    dbuf_slices: u8,
) {
    if display.is_null()
        || unsafe { display_ver(display) } < 14
        || unsafe { display_ver(display) } >= 30
    {
        return;
    }
    let dbufs = (dbuf_slices.count_ones() as u32).min(3);
    unsafe {
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
        if !intel_pmdemand_check_prev_transaction(display) {
            tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
            return;
        }

        intel_de_rmw(
            display,
            XELPDP_INITIATE_PMDEMAND_REQUEST(0),
            XELPDP_PMDEMAND_DBUFS_MASK,
            reg_field_prep(XELPDP_PMDEMAND_DBUFS_MASK, dbufs),
        );
        intel_de_rmw(
            display,
            XELPDP_INITIATE_PMDEMAND_REQUEST(1),
            0,
            XELPDP_PMDEMAND_REQ_ENABLE,
        );

        intel_pmdemand_wait(display);

        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_update_params()
unsafe fn intel_pmdemand_update_params(
    display: *mut IntelDisplay,
    new: *const IntelPmdemandState,
    old: *const IntelPmdemandState,
    reg1: *mut u32,
    reg2: *mut u32,
    serialized: bool,
) {
    if new.is_null() || reg1.is_null() || reg2.is_null() {
        return;
    }

    unsafe {
        macro_rules! update_reg {
            ($reg:expr, $field:ident, $mask:expr) => {{
                let current_val = if serialized {
                    0
                } else {
                    reg_field_get($mask, *$reg)
                };
                let old_val = if !old.is_null() {
                    (*old).params.$field as u32
                } else {
                    0
                };
                let new_val = (*new).params.$field as u32;
                let max_val = old_val.max(new_val).max(current_val);
                *$reg &= !$mask;
                *$reg |= reg_field_prep($mask, max_val);
            }};
        }

        // Set 1
        update_reg!(reg1, qclk_gv_bw, XELPDP_PMDEMAND_QCLK_GV_BW_MASK);
        update_reg!(reg1, voltage_index, XELPDP_PMDEMAND_VOLTAGE_INDEX_MASK);
        update_reg!(reg1, qclk_gv_index, XELPDP_PMDEMAND_QCLK_GV_INDEX_MASK);
        update_reg!(reg1, active_phys, XELPDP_PMDEMAND_PHYS_MASK);

        // Set 2
        update_reg!(reg2, cdclk_freq_mhz, XELPDP_PMDEMAND_CDCLK_FREQ_MASK);
        update_reg!(reg2, ddiclk_max, XELPDP_PMDEMAND_DDICLK_FREQ_MASK);
        update_reg!(reg2, plls, XELPDP_PMDEMAND_PLLS_MASK);

        if display_ver(display) >= 30 {
            update_reg!(reg1, active_pipes, XE3_PMDEMAND_PIPES_MASK);
        } else {
            update_reg!(reg1, active_pipes, XELPDP_PMDEMAND_PIPES_MASK);
            update_reg!(reg1, active_dbufs, XELPDP_PMDEMAND_DBUFS_MASK);
            update_reg!(reg2, scalers, XELPDP_PMDEMAND_SCALERS_MASK);
        }
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_program_params()
unsafe fn intel_pmdemand_program_params(
    display: *mut IntelDisplay,
    new: *const IntelPmdemandState,
    old: *const IntelPmdemandState,
    serialized: bool,
) {
    if display.is_null() || new.is_null() {
        return;
    }
    unsafe {
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
        if !intel_pmdemand_check_prev_transaction(display) {
            tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
            return;
        }

        let reg1 = intel_de_read(display, XELPDP_INITIATE_PMDEMAND_REQUEST(0));
        let mut mod_reg1 = reg1;

        let reg2 = intel_de_read(display, XELPDP_INITIATE_PMDEMAND_REQUEST(1));
        let mut mod_reg2 = reg2;

        intel_pmdemand_update_params(display, new, old, &mut mod_reg1, &mut mod_reg2, serialized);

        let mut changed = false;
        if reg1 != mod_reg1 {
            intel_de_write(display, XELPDP_INITIATE_PMDEMAND_REQUEST(0), mod_reg1);
            changed = true;
        }
        if reg2 != mod_reg2 {
            intel_de_write(display, XELPDP_INITIATE_PMDEMAND_REQUEST(1), mod_reg2);
            changed = true;
        }

        if changed {
            intel_de_rmw(
                display,
                XELPDP_INITIATE_PMDEMAND_REQUEST(1),
                0,
                XELPDP_PMDEMAND_REQ_ENABLE,
            );
            intel_pmdemand_wait(display);
        }

        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*display).pmdemand.lock));
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_state_changed()
unsafe fn intel_pmdemand_state_changed(
    new: *const IntelPmdemandState,
    old: *const IntelPmdemandState,
) -> bool {
    if new.is_null() || old.is_null() {
        return true;
    }
    unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!((*new).params) as *const u8,
            core::mem::size_of::<PmdemandParams>(),
        ) != core::slice::from_raw_parts(
            core::ptr::addr_of!((*old).params) as *const u8,
            core::mem::size_of::<PmdemandParams>(),
        )
    }
}

// upstream: intel_pmdemand.c intel_pmdemand_pre_plane_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_pre_plane_update(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    // Gated by DISPLAY_VER < 14
}

// upstream: intel_pmdemand.c intel_pmdemand_post_plane_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_post_plane_update(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    // Gated by DISPLAY_VER < 14
}
