// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display power domains, translated from `drivers/gpu/drm/i915/display/intel_display_power.c`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use core::ffi::{c_char, c_void};

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_power_map_upstream::{
    intel_display_power_map_cleanup, intel_display_power_map_init,
};
use crate::intel_display_power_types_upstream::*;
use crate::intel_display_power_well_types_upstream::I915PowerWell;
use crate::intel_display_power_well_upstream::{
    gen9_sanitize_dc_state, intel_power_well_get, intel_power_well_is_enabled_cached,
    intel_power_well_put, intel_power_well_sync_hw,
};

#[inline]
unsafe fn __intel_display_power_is_enabled(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) -> bool {
    let power_domains = unsafe { core::ptr::addr_of_mut!((*display).power.domains) };
    let count = unsafe { (*power_domains).power_well_count };
    let wells = unsafe { (*power_domains).power_wells };
    if wells.is_null() || count <= 0 {
        return true;
    }
    let idx = (domain / 64) as usize;
    let bit = domain % 64;
    for i in 0..count {
        let pw = unsafe { wells.add(i as usize) };
        if (unsafe { (*pw).domains.bits[idx] } & (1u64 << bit)) != 0 {
            if !unsafe { intel_power_well_is_enabled_cached(pw) } {
                return false;
            }
        }
    }
    true
}

// upstream: intel_display_power.c __intel_display_power_get_domain()
unsafe fn __intel_display_power_get_domain(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) {
    let power_domains = unsafe { core::ptr::addr_of_mut!((*display).power.domains) };
    let count = unsafe { (*power_domains).power_well_count };
    let wells = unsafe { (*power_domains).power_wells };
    let idx = (domain / 64) as usize;
    let bit = domain % 64;

    if !wells.is_null() {
        for i in 0..count {
            let pw = unsafe { wells.add(i as usize) };
            if (unsafe { (*pw).domains.bits[idx] } & (1u64 << bit)) != 0 {
                unsafe {
                    intel_power_well_get(display, pw);
                }
            }
        }
    }
    if (domain as usize) < POWER_DOMAIN_NUM as usize {
        unsafe {
            (*power_domains).domain_use_count[domain as usize] += 1;
        }
    }
}

// upstream: intel_display_power.c __intel_display_power_put_domain()
unsafe fn __intel_display_power_put_domain(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) {
    let power_domains = unsafe { core::ptr::addr_of_mut!((*display).power.domains) };
    let count = unsafe { (*power_domains).power_well_count };
    let wells = unsafe { (*power_domains).power_wells };
    let idx = (domain / 64) as usize;
    let bit = domain % 64;

    if (domain as usize) < POWER_DOMAIN_NUM as usize {
        unsafe {
            if (*power_domains).domain_use_count[domain as usize] > 0 {
                (*power_domains).domain_use_count[domain as usize] -= 1;
            }
        }
    }

    if !wells.is_null() && count > 0 {
        for i in (0..count).rev() {
            let pw = unsafe { wells.add(i as usize) };
            if (unsafe { (*pw).domains.bits[idx] } & (1u64 << bit)) != 0 {
                unsafe {
                    intel_power_well_put(display, pw);
                }
            }
        }
    }
}

// upstream: intel_display_power.c intel_display_power_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_get(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) -> *mut c_void {
    if display.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let power_domains = core::ptr::addr_of_mut!((*display).power.domains);
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*power_domains).lock));
        __intel_display_power_get_domain(display, domain);
        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*power_domains).lock));
    }
    1 as *mut c_void
}

// upstream: intel_display_power.c intel_display_power_get_if_enabled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_get_if_enabled(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) -> *mut c_void {
    if display.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let power_domains = core::ptr::addr_of_mut!((*display).power.domains);
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*power_domains).lock));
        let enabled = __intel_display_power_is_enabled(display, domain);
        if enabled {
            __intel_display_power_get_domain(display, domain);
        }
        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*power_domains).lock));
        if enabled {
            1 as *mut c_void
        } else {
            core::ptr::null_mut()
        }
    }
}

// upstream: intel_display_power.c intel_display_power_put_unchecked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_put_unchecked(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) {
    if display.is_null() {
        return;
    }
    unsafe {
        let power_domains = core::ptr::addr_of_mut!((*display).power.domains);
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*power_domains).lock));
        __intel_display_power_put_domain(display, domain);
        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*power_domains).lock));
    }
}

// upstream: intel_display_power.c intel_display_power_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_put(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
    _wakeref: *mut c_void,
) {
    unsafe {
        intel_display_power_put_unchecked(display, domain);
    }
}

// upstream: intel_display_power.c intel_display_power_is_enabled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_is_enabled(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) -> bool {
    if display.is_null() {
        return false;
    }
    unsafe {
        let power_domains = core::ptr::addr_of_mut!((*display).power.domains);
        tk_linux_kapi::mutex::mutex_lock(core::ptr::addr_of_mut!((*power_domains).lock));
        let ret = __intel_display_power_is_enabled(display, domain);
        tk_linux_kapi::mutex::mutex_unlock(core::ptr::addr_of_mut!((*power_domains).lock));
        ret
    }
}

// upstream: intel_display_power.c __intel_display_power_put_async()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_display_power_put_async(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
    _wakeref: *mut c_void,
    _delay_ms: i32,
) {
    unsafe {
        intel_display_power_put_unchecked(display, domain);
    }
}

// upstream: intel_display_power.c intel_display_power_flush_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_flush_work(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c intel_power_domains_sync_hw()
unsafe fn intel_power_domains_sync_hw(display: *mut IntelDisplay) {
    let power_domains = unsafe { core::ptr::addr_of_mut!((*display).power.domains) };
    let count = unsafe { (*power_domains).power_well_count };
    let wells = unsafe { (*power_domains).power_wells };
    if wells.is_null() || count <= 0 {
        return;
    }
    for i in 0..count {
        let pw = unsafe { wells.add(i as usize) };
        unsafe {
            intel_power_well_sync_hw(display, pw);
        }
    }
}

// upstream: intel_display_power.c intel_display_power_init_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_init_hw(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        intel_power_domains_sync_hw(display);
    }
}

// upstream: intel_display_power.c intel_display_power_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() {
        return 0;
    }
    unsafe {
        let power_domains = core::ptr::addr_of_mut!((*display).power.domains);
        tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*power_domains).lock));
        intel_display_power_map_init(power_domains)
    }
}

// upstream: intel_display_power.c intel_display_power_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_cleanup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        let power_domains = core::ptr::addr_of_mut!((*display).power.domains);
        intel_display_power_map_cleanup(power_domains);
    }
}

// upstream: intel_display_power.c intel_display_power_driver_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_driver_remove(display: *mut IntelDisplay) {
    unsafe {
        intel_display_power_cleanup(display);
    }
}

// upstream: intel_display_power.c intel_display_power_get_in_set()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_get_in_set(
    display: *mut IntelDisplay,
    power_domain_set: *mut IntelDisplayPowerDomainSet,
    domain: IntelDisplayPowerDomain,
) {
    if display.is_null() || power_domain_set.is_null() {
        return;
    }
    unsafe {
        intel_display_power_get(display, domain);
        let idx = (domain / 64) as usize;
        let bit = domain % 64;
        (*power_domain_set).mask.bits[idx] |= 1u64 << bit;
    }
}

// upstream: intel_display_power.c intel_display_power_get_in_set_if_enabled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_get_in_set_if_enabled(
    display: *mut IntelDisplay,
    power_domain_set: *mut IntelDisplayPowerDomainSet,
    domain: IntelDisplayPowerDomain,
) -> bool {
    if display.is_null() || power_domain_set.is_null() {
        return false;
    }
    unsafe {
        let wf = intel_display_power_get_if_enabled(display, domain);
        if wf.is_null() {
            false
        } else {
            let idx = (domain / 64) as usize;
            let bit = domain % 64;
            (*power_domain_set).mask.bits[idx] |= 1u64 << bit;
            true
        }
    }
}

// upstream: intel_display_power.c intel_display_power_put_mask_in_set()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_put_mask_in_set(
    display: *mut IntelDisplay,
    power_domain_set: *mut IntelDisplayPowerDomainSet,
) {
    if display.is_null() || power_domain_set.is_null() {
        return;
    }
    for domain in 0..POWER_DOMAIN_NUM {
        let idx = (domain / 64) as usize;
        let bit = domain % 64;
        unsafe {
            if ((*power_domain_set).mask.bits[idx] & (1u64 << bit)) != 0 {
                (*power_domain_set).mask.bits[idx] &= !(1u64 << bit);
                intel_display_power_put_unchecked(display, domain);
            }
        }
    }
}

// upstream: intel_display_power.c intel_display_power_ddi_io_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_ddi_io_domain(
    _display: *mut IntelDisplay,
    port: i32,
) -> IntelDisplayPowerDomain {
    match port {
        0 => POWER_DOMAIN_PORT_DDI_IO_A,
        1 => POWER_DOMAIN_PORT_DDI_IO_B,
        2 => POWER_DOMAIN_PORT_DDI_IO_C,
        3 => POWER_DOMAIN_PORT_DDI_IO_D,
        4 => POWER_DOMAIN_PORT_DDI_IO_E,
        _ => POWER_DOMAIN_PORT_DDI_IO_A,
    }
}

// upstream: intel_display_power.c intel_display_power_ddi_lanes_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_ddi_lanes_domain(
    _display: *mut IntelDisplay,
    port: i32,
) -> IntelDisplayPowerDomain {
    match port {
        0 => POWER_DOMAIN_PORT_DDI_LANES_A,
        1 => POWER_DOMAIN_PORT_DDI_LANES_B,
        2 => POWER_DOMAIN_PORT_DDI_LANES_C,
        3 => POWER_DOMAIN_PORT_DDI_LANES_D,
        4 => POWER_DOMAIN_PORT_DDI_LANES_E,
        _ => POWER_DOMAIN_PORT_DDI_LANES_A,
    }
}

// upstream: intel_display_power.c intel_display_power_aux_io_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_aux_io_domain(
    _display: *mut IntelDisplay,
    aux_ch: i32,
) -> IntelDisplayPowerDomain {
    match aux_ch {
        0 => POWER_DOMAIN_AUX_IO_A,
        1 => POWER_DOMAIN_AUX_IO_B,
        2 => POWER_DOMAIN_AUX_IO_C,
        3 => POWER_DOMAIN_AUX_IO_D,
        4 => POWER_DOMAIN_AUX_IO_E,
        _ => POWER_DOMAIN_AUX_IO_A,
    }
}

// upstream: intel_display_power.c intel_display_power_legacy_aux_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_legacy_aux_domain(
    _display: *mut IntelDisplay,
    aux_ch: i32,
) -> IntelDisplayPowerDomain {
    match aux_ch {
        0 => POWER_DOMAIN_AUX_A,
        1 => POWER_DOMAIN_AUX_B,
        2 => POWER_DOMAIN_AUX_C,
        3 => POWER_DOMAIN_AUX_D,
        4 => POWER_DOMAIN_AUX_E,
        _ => POWER_DOMAIN_AUX_A,
    }
}

// upstream: intel_display_power.c intel_display_power_tbt_aux_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_tbt_aux_domain(
    _display: *mut IntelDisplay,
    aux_ch: i32,
) -> IntelDisplayPowerDomain {
    match aux_ch {
        0 => POWER_DOMAIN_AUX_TBT1,
        1 => POWER_DOMAIN_AUX_TBT2,
        2 => POWER_DOMAIN_AUX_TBT3,
        3 => POWER_DOMAIN_AUX_TBT4,
        _ => POWER_DOMAIN_AUX_TBT1,
    }
}

// upstream: intel_display_power.c intel_display_power_domain_str()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_domain_str(
    domain: IntelDisplayPowerDomain,
) -> *const c_char {
    match domain {
        POWER_DOMAIN_DISPLAY_CORE => b"DISPLAY_CORE\0".as_ptr() as *const c_char,
        POWER_DOMAIN_PIPE_A => b"PIPE_A\0".as_ptr() as *const c_char,
        POWER_DOMAIN_PIPE_B => b"PIPE_B\0".as_ptr() as *const c_char,
        POWER_DOMAIN_PIPE_C => b"PIPE_C\0".as_ptr() as *const c_char,
        POWER_DOMAIN_PIPE_D => b"PIPE_D\0".as_ptr() as *const c_char,
        POWER_DOMAIN_DC_OFF => b"DC_OFF\0".as_ptr() as *const c_char,
        POWER_DOMAIN_INIT => b"INIT\0".as_ptr() as *const c_char,
        _ => b"UNKNOWN\0".as_ptr() as *const c_char,
    }
}

// upstream: intel_display_power.c intel_display_power_get_current_dc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_get_current_dc_state(
    display: *mut IntelDisplay,
) -> u32 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).power.domains.dc_state }
    }
}

// upstream: intel_display_power.c intel_display_power_set_target_dc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_set_target_dc_state(
    display: *mut IntelDisplay,
    state: u32,
) {
    if !display.is_null() {
        unsafe {
            (*display).power.domains.target_dc_state = state;
        }
    }
}

// upstream: intel_display_power.c intel_display_power_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_enable(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
}

// upstream: intel_display_power.c intel_display_power_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_disable(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
}

// upstream: intel_display_power.c intel_display_power_sanitize_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_sanitize_state(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        gen9_sanitize_dc_state(display);
    }
}

// upstream: intel_display_power.c intel_display_power_suspend_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_suspend_late(
    _display: *mut IntelDisplay,
    _s2idle: bool,
) {}

// upstream: intel_display_power.c intel_display_power_resume_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_resume_early(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c intel_display_power_runtime_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_runtime_suspend(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c intel_display_power_runtime_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_runtime_resume(_display: *mut IntelDisplay) {}
