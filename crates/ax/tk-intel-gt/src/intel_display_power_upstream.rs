// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display power domains, translated from `drivers/gpu/drm/i915/display/intel_display_power.c`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use core::ffi::{c_char, c_void};
use core::ptr::addr_of_mut;

use crate::intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_write};
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_limits_types_upstream::*;
use crate::intel_display_power_map_upstream::{
    intel_display_power_map_cleanup, intel_display_power_map_init,
};
use crate::intel_display_power_types_upstream::*;
use crate::intel_display_power_well_types_upstream::*;
use crate::intel_display_power_well_upstream::{
    bxt_disable_dc9, bxt_enable_dc9, gen9_disable_dc_states, gen9_enable_dc5,
    gen9_sanitize_dc_state, gen9_set_dc_state, intel_power_well_disable,
    intel_power_well_enable, intel_power_well_get, intel_power_well_is_always_on,
    intel_power_well_is_enabled, intel_power_well_is_enabled_cached, intel_power_well_name,
    intel_power_well_put, intel_power_well_refcount, intel_power_well_sync_hw, lookup_power_well,
    skl_enable_dc6,
};
use crate::intel_display_regs_upstream::*;
use crate::intel_display_rpm_upstream::{
    intel_display_rpm_get_noresume, intel_display_rpm_get_raw, intel_display_rpm_put,
    intel_display_rpm_put_raw,
};
use crate::intel_display_wa_upstream::{__intel_display_wa, IntelDisplayWa};
use crate::skl_watermark_regs_upstream::{
    DBUF_CTL_S, DBUF_POWER_REQUEST, DBUF_POWER_STATE, DBUF_TRACKER_STATE_SERVICE,
    DBUF_TRACKER_STATE_SERVICE_MASK,
};
use tk_linux_kapi::primitives::msecs_to_jiffies;
use tk_linux_kapi::types::WorkStruct;
#[cfg(not(test))]
use tk_linux_kapi::workqueue::INIT_DELAYED_WORK;

#[cfg(test)]
fn INIT_DELAYED_WORK<F: Fn(&mut WorkStruct)>(delayed: &mut tk_linux_kapi::types::DelayedWork, _function: F) {
    let entry = core::ptr::addr_of_mut!(delayed.work.entry);
    delayed.work.entry.next = entry;
    delayed.work.entry.prev = entry;
    delayed.cpu = -1;
}

#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_init, mutex_lock, mutex_unlock};

#[cfg(test)]
unsafe fn mutex_init<M: tk_linux_kapi::mutex::MutexPointer>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_lock<M: tk_linux_kapi::mutex::MutexPointer>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M: tk_linux_kapi::mutex::MutexPointer>(_mutex: M) {}

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

// upstream: intel_display_power.c __intel_display_power_is_enabled()
#[inline]
unsafe fn __intel_display_power_is_enabled(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) -> bool {
    let power_domains = addr_of_mut!((*display).power.domains);
    let count = (*power_domains).power_well_count;
    let wells = (*power_domains).power_wells;
    if wells.is_null() || count <= 0 {
        return true;
    }
    let idx = (domain / 64) as usize;
    let bit = domain % 64;
    for i in 0..count {
        let pw = wells.add(i as usize);
        if ((*pw).domains.bits[idx] & (1u64 << bit)) != 0 {
            if !intel_power_well_is_enabled_cached(pw) {
                return false;
            }
        }
    }
    true
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
    let power_domains = addr_of_mut!((*display).power.domains);
    mutex_lock(addr_of_mut!((*power_domains).lock));
    let ret = __intel_display_power_is_enabled(display, domain);
    mutex_unlock(addr_of_mut!((*power_domains).lock));
    ret
}

// upstream: intel_display_power.c sanitize_target_dc_state()
unsafe fn sanitize_target_dc_state(
    display: *mut IntelDisplay,
    mut target_dc_state: u32,
) -> u32 {
    let power_domains = addr_of_mut!((*display).power.domains);
    let states = [
        DC_STATE_EN_UPTO_DC6,
        DC_STATE_EN_UPTO_DC5,
        DC_STATE_EN_DC3CO,
        DC_STATE_DISABLE,
    ];
    for i in 0..states.len() - 1 {
        if target_dc_state != states[i] {
            continue;
        }
        if ((*power_domains).allowed_dc_mask & target_dc_state) != 0 {
            break;
        }
        target_dc_state = states[i + 1];
    }
    target_dc_state
}

// upstream: intel_display_power.c intel_display_power_set_target_dc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_set_target_dc_state(
    display: *mut IntelDisplay,
    mut state: u32,
) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    mutex_lock(addr_of_mut!((*power_domains).lock));

    let well = lookup_power_well(display, SKL_DISP_DC_OFF);
    if well.is_null() {
        mutex_unlock(addr_of_mut!((*power_domains).lock));
        return;
    }

    state = sanitize_target_dc_state(display, state);
    if state == (*power_domains).target_dc_state {
        mutex_unlock(addr_of_mut!((*power_domains).lock));
        return;
    }

    let dc_off_enabled = intel_power_well_is_enabled(display, well);
    if !dc_off_enabled {
        intel_power_well_enable(display, well);
    }

    (*power_domains).target_dc_state = state;

    if !dc_off_enabled {
        intel_power_well_disable(display, well);
    }

    mutex_unlock(addr_of_mut!((*power_domains).lock));
}

// upstream: intel_display_power.c intel_display_power_get_current_dc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_get_current_dc_state(
    display: *mut IntelDisplay,
) -> u32 {
    if display.is_null() {
        return DC_STATE_DISABLE;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    mutex_lock(addr_of_mut!((*power_domains).lock));

    let well = lookup_power_well(display, SKL_DISP_DC_OFF);
    if well.is_null() {
        mutex_unlock(addr_of_mut!((*power_domains).lock));
        return DC_STATE_DISABLE;
    }

    let current = if intel_power_well_is_enabled(display, well) {
        DC_STATE_DISABLE
    } else {
        (*power_domains).target_dc_state
    };

    mutex_unlock(addr_of_mut!((*power_domains).lock));
    current
}

// upstream: intel_display_power.c __async_put_domains_mask()
unsafe fn __async_put_domains_mask(
    power_domains: *mut I915PowerDomains,
    mask: *mut IntelPowerDomainMask,
) {
    (*mask).bits[0] = (*power_domains).async_put_domains[0].bits[0]
        | (*power_domains).async_put_domains[1].bits[0];
    (*mask).bits[1] = (*power_domains).async_put_domains[0].bits[1]
        | (*power_domains).async_put_domains[1].bits[1];
}

// upstream: intel_display_power.c assert_async_put_domain_masks_disjoint()
unsafe fn assert_async_put_domain_masks_disjoint(power_domains: *mut I915PowerDomains) -> bool {
    let d0 = (*power_domains).async_put_domains[0].bits;
    let d1 = (*power_domains).async_put_domains[1].bits;
    (d0[0] & d1[0]) == 0 && (d0[1] & d1[1]) == 0
}

// upstream: intel_display_power.c __async_put_domains_state_ok()
unsafe fn __async_put_domains_state_ok(power_domains: *mut I915PowerDomains) -> bool {
    if !assert_async_put_domain_masks_disjoint(power_domains) {
        return false;
    }
    let mut async_put_mask = IntelPowerDomainMask { bits: [0, 0] };
    __async_put_domains_mask(power_domains, &mut async_put_mask);
    let is_empty = async_put_mask.bits[0] == 0 && async_put_mask.bits[1] == 0;
    let has_wakeref = !(*power_domains).async_put_wakeref.is_null();
    if has_wakeref == is_empty {
        return false;
    }
    for domain in 0..POWER_DOMAIN_NUM {
        let idx = (domain / 64) as usize;
        let bit = domain % 64;
        if (async_put_mask.bits[idx] & (1u64 << bit)) != 0 {
            if (*power_domains).domain_use_count[domain as usize] != 1 {
                return false;
            }
        }
    }
    true
}

// upstream: intel_display_power.c print_power_domains()
unsafe fn print_power_domains(
    _power_domains: *mut I915PowerDomains,
    _prefix: *const c_char,
    _mask: *mut IntelPowerDomainMask,
) {}

// upstream: intel_display_power.c print_async_put_domains_state()
unsafe fn print_async_put_domains_state(_power_domains: *mut I915PowerDomains) {}

// upstream: intel_display_power.c verify_async_put_domains_state()
unsafe fn verify_async_put_domains_state(power_domains: *mut I915PowerDomains) {
    if !__async_put_domains_state_ok(power_domains) {
        print_async_put_domains_state(power_domains);
    }
}

// upstream: intel_display_power.c async_put_domains_mask()
unsafe fn async_put_domains_mask(
    power_domains: *mut I915PowerDomains,
    mask: *mut IntelPowerDomainMask,
) {
    assert_async_put_domain_masks_disjoint(power_domains);
    __async_put_domains_mask(power_domains, mask);
}

// upstream: intel_display_power.c async_put_domains_clear_domain()
unsafe fn async_put_domains_clear_domain(
    power_domains: *mut I915PowerDomains,
    domain: IntelDisplayPowerDomain,
) {
    assert_async_put_domain_masks_disjoint(power_domains);
    let idx = (domain / 64) as usize;
    let bit = domain % 64;
    (*power_domains).async_put_domains[0].bits[idx] &= !(1u64 << bit);
    (*power_domains).async_put_domains[1].bits[idx] &= !(1u64 << bit);
}

// upstream: intel_display_power.c cancel_async_put_work()
unsafe fn cancel_async_put_work(power_domains: *mut I915PowerDomains, sync: bool) {
    #[cfg(not(test))]
    if sync {
        tk_linux_kapi::workqueue::cancel_delayed_work_sync(&mut (*power_domains).async_put_work);
    } else {
        tk_linux_kapi::workqueue::cancel_delayed_work(&mut (*power_domains).async_put_work);
    }
    #[cfg(test)]
    let _ = sync;

    (*power_domains).async_put_next_delay = 0;
}

// upstream: intel_display_power.c intel_display_power_grab_async_put_ref()
unsafe fn intel_display_power_grab_async_put_ref(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) -> bool {
    let power_domains = addr_of_mut!((*display).power.domains);
    let mut async_put_mask = IntelPowerDomainMask { bits: [0, 0] };
    let mut ret = false;

    async_put_domains_mask(power_domains, &mut async_put_mask);
    let idx = (domain / 64) as usize;
    let bit = domain % 64;
    if (async_put_mask.bits[idx] & (1u64 << bit)) == 0 {
        verify_async_put_domains_state(power_domains);
        return false;
    }

    async_put_domains_clear_domain(power_domains, domain);
    ret = true;

    async_put_domains_mask(power_domains, &mut async_put_mask);
    if async_put_mask.bits[0] != 0 || async_put_mask.bits[1] != 0 {
        verify_async_put_domains_state(power_domains);
        return ret;
    }

    cancel_async_put_work(power_domains, false);
    let wakeref = core::ptr::replace(&mut (*power_domains).async_put_wakeref, core::ptr::null_mut());
    intel_display_rpm_put_raw(display, wakeref);

    verify_async_put_domains_state(power_domains);
    ret
}

// upstream: intel_display_power.c __intel_display_power_get_domain()
unsafe fn __intel_display_power_get_domain(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) {
    let power_domains = addr_of_mut!((*display).power.domains);
    if intel_display_power_grab_async_put_ref(display, domain) {
        return;
    }

    let count = (*power_domains).power_well_count;
    let wells = (*power_domains).power_wells;
    let idx = (domain / 64) as usize;
    let bit = domain % 64;

    if !wells.is_null() {
        for i in 0..count {
            let pw = wells.add(i as usize);
            if ((*pw).domains.bits[idx] & (1u64 << bit)) != 0 {
                intel_power_well_get(display, pw);
            }
        }
    }
    if (domain as usize) < POWER_DOMAIN_NUM as usize {
        (*power_domains).domain_use_count[domain as usize] += 1;
    }
}

// upstream: intel_display_power.c intel_display_power_get()
// Collides with kernel/src/drm/intel/upstream_gt.rs: exported without no_mangle
pub unsafe extern "C" fn intel_display_power_get(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) -> *mut c_void {
    if display.is_null() {
        return core::ptr::null_mut();
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    mutex_lock(addr_of_mut!((*power_domains).lock));
    __intel_display_power_get_domain(display, domain);
    mutex_unlock(addr_of_mut!((*power_domains).lock));
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
    let power_domains = addr_of_mut!((*display).power.domains);
    mutex_lock(addr_of_mut!((*power_domains).lock));
    let enabled = __intel_display_power_is_enabled(display, domain);
    if enabled {
        __intel_display_power_get_domain(display, domain);
    }
    mutex_unlock(addr_of_mut!((*power_domains).lock));
    if enabled {
        1 as *mut c_void
    } else {
        core::ptr::null_mut()
    }
}

// upstream: intel_display_power.c __intel_display_power_put_domain()
unsafe fn __intel_display_power_put_domain(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) {
    let power_domains = addr_of_mut!((*display).power.domains);
    let count = (*power_domains).power_well_count;
    let wells = (*power_domains).power_wells;
    let idx = (domain / 64) as usize;
    let bit = domain % 64;

    if (domain as usize) < POWER_DOMAIN_NUM as usize {
        if (*power_domains).domain_use_count[domain as usize] > 0 {
            (*power_domains).domain_use_count[domain as usize] -= 1;
        }
    }

    if !wells.is_null() && count > 0 {
        for i in (0..count).rev() {
            let pw = wells.add(i as usize);
            if ((*pw).domains.bits[idx] & (1u64 << bit)) != 0 {
                intel_power_well_put(display, pw);
            }
        }
    }
}

// upstream: intel_display_power.c __intel_display_power_put()
pub unsafe extern "C" fn __intel_display_power_put(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) {
    let power_domains = addr_of_mut!((*display).power.domains);
    mutex_lock(addr_of_mut!((*power_domains).lock));
    __intel_display_power_put_domain(display, domain);
    mutex_unlock(addr_of_mut!((*power_domains).lock));
}

// upstream: intel_display_power.c queue_async_put_domains_work()
unsafe fn queue_async_put_domains_work(
    power_domains: *mut I915PowerDomains,
    wakeref: *mut c_void,
    delay_ms: i32,
) {
    (*power_domains).async_put_wakeref = wakeref;
    #[cfg(not(test))]
    {
        let delay_jiffies = msecs_to_jiffies(delay_ms as u64);
        tk_linux_kapi::workqueue::queue_delayed_work(
            tk_linux_kapi::workqueue::system_dfl_wq,
            addr_of_mut!((*power_domains).async_put_work),
            delay_jiffies,
        );
    }
    #[cfg(test)]
    let _ = delay_ms;
}

// upstream: intel_display_power.c release_async_put_domains()
unsafe fn release_async_put_domains(
    power_domains: *mut I915PowerDomains,
    mask: *mut IntelPowerDomainMask,
) {
    let offset = core::mem::offset_of!(IntelDisplay, power)
        + core::mem::offset_of!(crate::intel_display_core_types_upstream::IntelDisplayPower, domains);
    let display = (power_domains as *mut u8).sub(offset) as *mut IntelDisplay;

    let mut wakeref = intel_display_rpm_get_noresume(display);
    if wakeref.is_null() {
        wakeref = 1 as *mut c_void;
    }

    for domain in 0..POWER_DOMAIN_NUM {
        let idx = (domain / 64) as usize;
        let bit = domain % 64;
        if ((*mask).bits[idx] & (1u64 << bit)) != 0 {
            async_put_domains_clear_domain(power_domains, domain);
            __intel_display_power_put_domain(display, domain);
        }
    }

    intel_display_rpm_put(display, wakeref);
}

// upstream: intel_display_power.c intel_display_power_put_async_work()
unsafe fn intel_display_power_put_async_work(work: *mut WorkStruct) {
    let offset = core::mem::offset_of!(IntelDisplay, power)
        + core::mem::offset_of!(crate::intel_display_core_types_upstream::IntelDisplayPower, domains)
        + core::mem::offset_of!(I915PowerDomains, async_put_work);
    let display = (work as *mut u8).sub(offset) as *mut IntelDisplay;
    let power_domains = addr_of_mut!((*display).power.domains);
    let mut new_work_wakeref = intel_display_rpm_get_raw(display);
    if new_work_wakeref.is_null() {
        new_work_wakeref = 1 as *mut c_void;
    }
    let old_work_wakeref: *mut c_void;

    mutex_lock(addr_of_mut!((*power_domains).lock));

    old_work_wakeref = core::ptr::replace(&mut (*power_domains).async_put_wakeref, core::ptr::null_mut());
    if old_work_wakeref.is_null() {
        verify_async_put_domains_state(power_domains);
        mutex_unlock(addr_of_mut!((*power_domains).lock));
        if !new_work_wakeref.is_null() {
            intel_display_rpm_put_raw(display, new_work_wakeref);
        }
        return;
    }

    release_async_put_domains(power_domains, &mut (*power_domains).async_put_domains[0]);

    cancel_async_put_work(power_domains, false);

    let d1 = (*power_domains).async_put_domains[1].bits;
    if d1[0] != 0 || d1[1] != 0 {
        (*power_domains).async_put_domains[0].bits = (*power_domains).async_put_domains[1].bits;
        (*power_domains).async_put_domains[1].bits = [0, 0];
        let next_wakeref = core::ptr::replace(&mut new_work_wakeref, core::ptr::null_mut());
        queue_async_put_domains_work(
            power_domains,
            next_wakeref,
            (*power_domains).async_put_next_delay,
        );
        (*power_domains).async_put_next_delay = 0;
    }

    verify_async_put_domains_state(power_domains);

    mutex_unlock(addr_of_mut!((*power_domains).lock));

    if !old_work_wakeref.is_null() {
        intel_display_rpm_put_raw(display, old_work_wakeref);
    }
    if !new_work_wakeref.is_null() {
        intel_display_rpm_put_raw(display, new_work_wakeref);
    }
}

// upstream: intel_display_power.c __intel_display_power_put_async()
// Collides with kernel/src/drm/intel/upstream_gt.rs: exported without no_mangle
pub unsafe extern "C" fn __intel_display_power_put_async(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
    wakeref: *mut c_void,
    delay_ms: i32,
) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    let mut work_wakeref = intel_display_rpm_get_raw(display);
    if work_wakeref.is_null() {
        work_wakeref = 1 as *mut c_void;
    }
    let effective_delay = if delay_ms >= 0 { delay_ms } else { 100 };

    mutex_lock(addr_of_mut!((*power_domains).lock));

    if (*power_domains).domain_use_count[domain as usize] > 1 {
        __intel_display_power_put_domain(display, domain);
    } else {
        let idx = (domain / 64) as usize;
        let bit = domain % 64;
        if !(*power_domains).async_put_wakeref.is_null() {
            (*power_domains).async_put_domains[1].bits[idx] |= 1u64 << bit;
            (*power_domains).async_put_next_delay = (*power_domains).async_put_next_delay.max(effective_delay);
        } else {
            (*power_domains).async_put_domains[0].bits[idx] |= 1u64 << bit;
            let next_ref = core::ptr::replace(&mut work_wakeref, core::ptr::null_mut());
            queue_async_put_domains_work(power_domains, next_ref, effective_delay);
        }
    }

    verify_async_put_domains_state(power_domains);
    mutex_unlock(addr_of_mut!((*power_domains).lock));

    if !work_wakeref.is_null() {
        intel_display_rpm_put_raw(display, work_wakeref);
    }
    intel_display_rpm_put(display, wakeref);
}

// upstream: intel_display_power.c intel_display_power_flush_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_flush_work(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    let mut async_put_mask = IntelPowerDomainMask { bits: [0, 0] };

    mutex_lock(addr_of_mut!((*power_domains).lock));

    let work_wakeref = core::ptr::replace(&mut (*power_domains).async_put_wakeref, core::ptr::null_mut());
    if !work_wakeref.is_null() {
        async_put_domains_mask(power_domains, &mut async_put_mask);
        release_async_put_domains(power_domains, &mut async_put_mask);
        cancel_async_put_work(power_domains, false);
    }

    verify_async_put_domains_state(power_domains);
    mutex_unlock(addr_of_mut!((*power_domains).lock));

    if !work_wakeref.is_null() {
        intel_display_rpm_put_raw(display, work_wakeref);
    }
}

// upstream: intel_display_power.c intel_display_power_flush_work_sync()
unsafe fn intel_display_power_flush_work_sync(display: *mut IntelDisplay) {
    let power_domains = addr_of_mut!((*display).power.domains);
    intel_display_power_flush_work(display);
    cancel_async_put_work(power_domains, true);
    verify_async_put_domains_state(power_domains);
}

// upstream: intel_display_power.c intel_display_power_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_put(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
    _wakeref: *mut c_void,
) {
    __intel_display_power_put(display, domain);
}

// upstream: intel_display_power.c intel_display_power_put_unchecked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_put_unchecked(
    display: *mut IntelDisplay,
    domain: IntelDisplayPowerDomain,
) {
    __intel_display_power_put(display, domain);
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
    intel_display_power_get(display, domain);
    let idx = (domain / 64) as usize;
    let bit = domain % 64;
    (*power_domain_set).mask.bits[idx] |= 1u64 << bit;
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
        if ((*power_domain_set).mask.bits[idx] & (1u64 << bit)) != 0 {
            (*power_domain_set).mask.bits[idx] &= !(1u64 << bit);
            intel_display_power_put_unchecked(display, domain);
        }
    }
}

// upstream: intel_display_power.c sanitize_disable_power_well_option()
unsafe fn sanitize_disable_power_well_option(disable_power_well: i32) -> i32 {
    if disable_power_well >= 0 {
        if disable_power_well != 0 { 1 } else { 0 }
    } else {
        1
    }
}

// upstream: intel_display_power.c get_allowed_dc_mask()
unsafe fn get_allowed_dc_mask(display: *mut IntelDisplay, enable_dc: i32) -> u32 {
    if display.is_null() {
        return 0;
    }
    let mut mask = 0u32;
    let ver = (*display).info.__runtime_info.ip.ver;
    let mut max_dc = if ver >= 20 {
        2
    } else if (*display).platform._anon_0_0.bitmap[0] & (1u64 << 54) != 0 {
        1
    } else if (*display).platform._anon_0_0.bitmap[0] & (1u64 << 50) != 0 {
        3
    } else if ver >= 12 {
        4
    } else if ver >= 9 {
        2
    } else {
        0
    };

    if ver >= 11 {
        mask = DC_STATE_EN_DC9;
    }

    if (*display).params.disable_power_well == 0 {
        max_dc = 0;
    }

    let requested_dc = if enable_dc >= 0 && enable_dc <= max_dc {
        enable_dc
    } else if enable_dc == -1 {
        max_dc
    } else if enable_dc > max_dc && enable_dc <= 4 {
        max_dc
    } else {
        max_dc
    };

    match requested_dc {
        4 => mask |= DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC6,
        3 => mask |= DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC5,
        2 => mask |= DC_STATE_EN_UPTO_DC6,
        1 => mask |= DC_STATE_EN_UPTO_DC5,
        _ => {}
    }

    mask
}

// upstream: intel_display_power.c intel_display_power_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() {
        return 0;
    }
    let power_domains = addr_of_mut!((*display).power.domains);

    (*display).params.disable_power_well =
        sanitize_disable_power_well_option((*display).params.disable_power_well);
    (*power_domains).allowed_dc_mask =
        get_allowed_dc_mask(display, (*display).params.enable_dc);
    (*power_domains).target_dc_state =
        sanitize_target_dc_state(display, DC_STATE_EN_UPTO_DC6);

    mutex_init(addr_of_mut!((*power_domains).lock));

    INIT_DELAYED_WORK(&mut (*power_domains).async_put_work, |work| {
        intel_display_power_put_async_work(work as *mut WorkStruct);
    });

    intel_display_power_map_init(power_domains)
}

#[inline]
pub unsafe fn intel_power_domains_init(display: *mut IntelDisplay) -> i32 {
    intel_display_power_init(display)
}

#[inline]
pub unsafe fn intel_power_domains_init_hw(display: *mut IntelDisplay) {
    intel_display_power_init_hw(display);
}

// upstream: intel_display_power.c intel_display_power_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_cleanup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    intel_display_power_map_cleanup(power_domains);
}

// upstream: intel_display_power.c intel_power_domains_sync_hw()
pub(crate) unsafe fn intel_power_domains_sync_hw(display: *mut IntelDisplay) {
    let power_domains = addr_of_mut!((*display).power.domains);
    let count = (*power_domains).power_well_count;
    let wells = (*power_domains).power_wells;
    if wells.is_null() || count <= 0 {
        return;
    }
    mutex_lock(addr_of_mut!((*power_domains).lock));
    for i in 0..count {
        let pw = wells.add(i as usize);
        intel_power_well_sync_hw(display, pw);
    }
    mutex_unlock(addr_of_mut!((*power_domains).lock));
}

// upstream: intel_display_power.c gen9_dbuf_slice_set()
pub(crate) unsafe fn gen9_dbuf_slice_set(
    display: *mut IntelDisplay,
    slice: u32,
    enable: bool,
) {
    let reg = DBUF_CTL_S(slice);
    intel_de_rmw(
        display,
        reg,
        DBUF_POWER_REQUEST,
        if enable { DBUF_POWER_REQUEST } else { 0 },
    );
    intel_de_posting_read(display, reg);
    #[cfg(not(test))]
    tk_linux_kapi::primitives::udelay(10);

    let state = (intel_de_read(display, reg) & DBUF_POWER_STATE) != 0;
    let _ = state;
}

// upstream: intel_display_power.c gen9_dbuf_slices_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen9_dbuf_slices_update(
    display: *mut IntelDisplay,
    req_slices: u8,
) {
    let power_domains = addr_of_mut!((*display).power.domains);
    let slice_mask = if !(*display).info.__device_info.is_null() {
        (*(*display).info.__device_info).dbuf.slice_mask
    } else {
        0x3
    };

    mutex_lock(addr_of_mut!((*power_domains).lock));

    for slice in 0..4 {
        if (slice_mask & (1 << slice)) != 0 {
            gen9_dbuf_slice_set(display, slice, (req_slices & (1 << slice)) != 0);
        }
    }

    (*display).dbuf.enabled_slices = req_slices;

    mutex_unlock(addr_of_mut!((*power_domains).lock));
}

// upstream: intel_display_power.c gen9_dbuf_enable()
unsafe fn gen9_dbuf_enable(display: *mut IntelDisplay) {
    let mut hw_enabled: u8 = 0;
    for slice in 0..4 {
        let reg = DBUF_CTL_S(slice);
        if (intel_de_read(display, reg) & DBUF_POWER_STATE) != 0 {
            hw_enabled |= 1 << slice;
        }
    }
    (*display).dbuf.enabled_slices = hw_enabled;

    let slices_mask = 1 | (*display).dbuf.enabled_slices;

    if (*display).info.__runtime_info.ip.ver >= 14 {
        crate::intel_pmdemand_upstream::intel_pmdemand_program_dbuf(display, slices_mask);
    }

    gen9_dbuf_slices_update(display, slices_mask);
}

// upstream: intel_display_power.c gen9_dbuf_disable()
unsafe fn gen9_dbuf_disable(display: *mut IntelDisplay) {
    gen9_dbuf_slices_update(display, 0);

    if (*display).info.__runtime_info.ip.ver >= 14 {
        crate::intel_pmdemand_upstream::intel_pmdemand_program_dbuf(display, 0);
    }
}

// upstream: intel_display_power.c gen12_dbuf_slices_config()
unsafe fn gen12_dbuf_slices_config(display: *mut IntelDisplay) {
    let slice_mask = if !(*display).info.__device_info.is_null() {
        (*(*display).info.__device_info).dbuf.slice_mask
    } else {
        0x3
    };
    for slice in 0..4 {
        if (slice_mask & (1 << slice)) != 0 {
            intel_de_rmw(
                display,
                DBUF_CTL_S(slice),
                DBUF_TRACKER_STATE_SERVICE_MASK,
                DBUF_TRACKER_STATE_SERVICE(8),
            );
        }
    }
}

// upstream: intel_display_power.c icl_mbus_init()
unsafe fn icl_mbus_init(display: *mut IntelDisplay) {
    let is_adlp = (*display).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0;
    if is_adlp || (*display).info.__runtime_info.ip.ver >= 14 {
        return;
    }

    let mask = MBUS_ABOX_BT_CREDIT_POOL1_MASK
        | MBUS_ABOX_BT_CREDIT_POOL2_MASK
        | MBUS_ABOX_B_CREDIT_MASK
        | MBUS_ABOX_BW_CREDIT_MASK;
    let val = MBUS_ABOX_BT_CREDIT_POOL1(16)
        | MBUS_ABOX_BT_CREDIT_POOL2(16)
        | MBUS_ABOX_B_CREDIT(1)
        | MBUS_ABOX_BW_CREDIT(1);

    let mut abox_regs = if !(*display).info.__device_info.is_null() {
        (*(*display).info.__device_info).abox_mask as u32
    } else {
        0
    };

    if (*display).info.__runtime_info.ip.ver == 12 {
        abox_regs |= 1;
    }

    for i in 0..8 {
        if (abox_regs & (1 << i)) != 0 {
            intel_de_rmw(display, MBUS_ABOX_CTL(i), mask, val);
        }
    }
}

// upstream: intel_display_power.c hsw_assert_cdclk()
// skipped: not display 13
unsafe fn hsw_assert_cdclk(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c assert_can_disable_lcpll()
// skipped: not display 13
unsafe fn assert_can_disable_lcpll(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c hsw_read_dcomp()
// skipped: not display 13
unsafe fn hsw_read_dcomp(_display: *mut IntelDisplay) -> u32 {
    0
}

// upstream: intel_display_power.c hsw_write_dcomp()
// skipped: not display 13
unsafe fn hsw_write_dcomp(_display: *mut IntelDisplay, _val: u32) {}

// upstream: intel_display_power.c hsw_disable_lcpll()
// skipped: not display 13
unsafe fn hsw_disable_lcpll(
    _display: *mut IntelDisplay,
    _switch_to_fclk: bool,
    _allow_power_down: bool,
) {}

// upstream: intel_display_power.c hsw_restore_lcpll()
// skipped: not display 13
unsafe fn hsw_restore_lcpll(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c hsw_enable_pc8()
// skipped: not display 13
unsafe fn hsw_enable_pc8(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c hsw_disable_pc8()
// skipped: not display 13
unsafe fn hsw_disable_pc8(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c intel_pch_reset_handshake()
unsafe fn intel_pch_reset_handshake(display: *mut IntelDisplay, enable: bool) {
    if (*display).info.__runtime_info.ip.ver >= 35 {
        return;
    }

    let is_ivb = (*display).platform._anon_0_0.bitmap[0] & (1u64 << 4) != 0;
    let (reg, mut reset_bits) = if is_ivb {
        (GEN7_MSG_CTL, 0x18)
    } else {
        (HSW_NDE_RSTWRN_OPT, RESET_PCH_HANDSHAKE_ENABLE)
    };

    if (*display).info.__runtime_info.ip.ver >= 14 {
        reset_bits |= 1 << 0;
    }

    intel_de_rmw(display, reg, reset_bits, if enable { reset_bits } else { 0 });
}

// upstream: intel_display_power.c skl_display_core_init()
// skipped: not display 13
unsafe fn skl_display_core_init(_display: *mut IntelDisplay, _resume: bool) {}

// upstream: intel_display_power.c skl_display_core_uninit()
// skipped: not display 13
unsafe fn skl_display_core_uninit(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c bxt_display_core_init()
// skipped: not display 13
unsafe fn bxt_display_core_init(_display: *mut IntelDisplay, _resume: bool) {}

// upstream: intel_display_power.c bxt_display_core_uninit()
// skipped: not display 13
unsafe fn bxt_display_core_uninit(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c tgl_bw_buddy_init()
unsafe fn tgl_bw_buddy_init(display: *mut IntelDisplay) {
    let dram_info = crate::display_pending_stubs::b3c::intel_dram_info(display);
    let is_dgfx = (*display).platform._anon_0_0.bitmap[0] & (1u64 << 49) != 0;
    let is_dg1 = (*display).platform._anon_0_0.bitmap[0] & (1u64 << 50) != 0;

    if is_dgfx && !is_dg1 {
        return;
    }

    let wa_1409767108 = __intel_display_wa(
        display,
        IntelDisplayWa::Wa1409767108,
        core::ptr::null(),
    );

    let abox_mask = if !(*display).info.__device_info.is_null() {
        (*(*display).info.__device_info).abox_mask as u32
    } else {
        0
    };

    let (num_channels, dram_type) = if !dram_info.is_null() {
        ((*dram_info).num_channels, (*dram_info).r#type)
    } else {
        (0, 0)
    };

    let page_mask = if wa_1409767108 {
        match num_channels {
            1 => 0x1,
            2 => 0x3,
            _ => 0,
        }
    } else {
        match (num_channels, dram_type) {
            (1, _) => 0xF,
            (2, 2 | 3) => 0x1C,
            (2, 0) => 0x1F,
            (2, 1) => 0x1E,
            (4, 2 | 3) => 0x38,
            _ => 0,
        }
    };

    for i in 0..8 {
        if (abox_mask & (1 << i)) != 0 {
            if page_mask == 0 {
                intel_de_write(display, BW_BUDDY_CTL(i), BW_BUDDY_DISABLE);
            } else {
                intel_de_write(display, BW_BUDDY_PAGE_MASK(i), page_mask);
            }
        }
    }
}

// upstream: intel_display_power.c icl_display_core_init()
pub(crate) unsafe fn icl_display_core_init(display: *mut IntelDisplay, resume: bool) {
    let power_domains = addr_of_mut!((*display).power.domains);

    gen9_set_dc_state(display, DC_STATE_DISABLE);

    if __intel_display_wa(display, IntelDisplayWa::Wa14011294188, core::ptr::null()) {
        intel_de_rmw(display, SOUTH_DSPCLK_GATE_D, 0, PCH_DPMGUNIT_CLOCK_GATE_DISABLE);
    }

    intel_pch_reset_handshake(display, true);

    crate::display_pending_stubs::b5::intel_combo_phy_init(display);

    mutex_lock(addr_of_mut!((*power_domains).lock));
    let well = lookup_power_well(display, SKL_DISP_PW_1);
    if !well.is_null() {
        intel_power_well_enable(display, well);
    }
    mutex_unlock(addr_of_mut!((*power_domains).lock));

    crate::display_pending_stubs::b3e::intel_cdclk_init_hw(display);

    let ver = (*display).info.__runtime_info.ip.ver;
    if ver == 12 || (*display).platform._anon_0_0.bitmap[0] & (1u64 << 54) != 0 {
        gen12_dbuf_slices_config(display);
    }

    gen9_dbuf_enable(display);
    icl_mbus_init(display);

    if ver >= 12 {
        tgl_bw_buddy_init(display);
    }

    if resume {
        crate::display_pending_stubs::b3c::intel_dmc_load_program(display);
    }

    if __intel_display_wa(display, IntelDisplayWa::Wa14011508470, core::ptr::null()) {
        intel_de_rmw(
            display,
            GEN11_CHICKEN_DCPR_2,
            0,
            (1 << 23) | (1 << 20) | (1 << 16) | (1 << 15),
        );
    }
}

// upstream: intel_display_power.c icl_display_core_uninit()
unsafe fn icl_display_core_uninit(display: *mut IntelDisplay) {
    let power_domains = addr_of_mut!((*display).power.domains);

    gen9_disable_dc_states(display);
    crate::display_pending_stubs::b3c::intel_dmc_disable_program(display);

    gen9_dbuf_disable(display);

    crate::display_pending_stubs::b3e::intel_cdclk_uninit_hw(display);

    mutex_lock(addr_of_mut!((*power_domains).lock));
    let well = lookup_power_well(display, SKL_DISP_PW_1);
    if !well.is_null() {
        intel_power_well_disable(display, well);
    }
    mutex_unlock(addr_of_mut!((*power_domains).lock));

    crate::display_pending_stubs::b5::intel_combo_phy_uninit(display);
}

// upstream: intel_display_power.c chv_phy_control_init()
// skipped: not display 13
unsafe fn chv_phy_control_init(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c vlv_cmnlane_wa()
// skipped: not display 13
unsafe fn vlv_cmnlane_wa(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c vlv_punit_is_power_gated()
// skipped: not display 13
unsafe fn vlv_punit_is_power_gated(_display: *mut IntelDisplay, _reg0: u32) -> bool {
    false
}

// upstream: intel_display_power.c assert_ved_power_gated()
// skipped: not display 13
unsafe fn assert_ved_power_gated(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c assert_isp_power_gated()
// skipped: not display 13
unsafe fn assert_isp_power_gated(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c __intel_display_power_init_hw()
unsafe fn __intel_display_power_init_hw(display: *mut IntelDisplay, resume: bool) {
    let power_domains = addr_of_mut!((*display).power.domains);
    (*power_domains).initializing = true;

    let ver = (*display).info.__runtime_info.ip.ver;
    if ver >= 11 {
        icl_display_core_init(display, resume);
    }

    (*power_domains).init_wakeref = intel_display_power_get(display, POWER_DOMAIN_INIT);

    if (*display).params.disable_power_well == 0 {
        (*power_domains).disable_wakeref = intel_display_power_get(display, POWER_DOMAIN_INIT);
    }

    intel_power_domains_sync_hw(display);
    (*power_domains).initializing = false;
}

// upstream: intel_display_power.c intel_display_power_init_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_init_hw(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    __intel_display_power_init_hw(display, false);
}

// upstream: intel_display_power.c intel_display_power_driver_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_driver_remove(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    let _wakeref = core::ptr::replace(&mut (*power_domains).init_wakeref, core::ptr::null_mut());

    if (*display).params.disable_power_well == 0 {
        let disable_wf = core::ptr::replace(&mut (*power_domains).disable_wakeref, core::ptr::null_mut());
        intel_display_power_put(display, POWER_DOMAIN_INIT, disable_wf);
    }

    intel_display_power_cleanup(display);
}

// upstream: intel_display_power.c intel_display_power_sanitize_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_sanitize_state(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    gen9_sanitize_dc_state(display);
}

// upstream: intel_display_power.c intel_display_power_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_enable(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    let wakeref = core::ptr::replace(&mut (*power_domains).init_wakeref, core::ptr::null_mut());
    intel_display_power_put(display, POWER_DOMAIN_INIT, wakeref);
}

// upstream: intel_display_power.c intel_display_power_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_disable(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    (*power_domains).init_wakeref = intel_display_power_get(display, POWER_DOMAIN_INIT);
}

// upstream: intel_display_power.c intel_power_domains_suspend()
unsafe fn intel_power_domains_suspend(display: *mut IntelDisplay, s2idle: bool) {
    let power_domains = addr_of_mut!((*display).power.domains);
    let wakeref = core::ptr::replace(&mut (*power_domains).init_wakeref, core::ptr::null_mut());
    intel_display_power_put(display, POWER_DOMAIN_INIT, wakeref);

    if ((*power_domains).allowed_dc_mask & DC_STATE_EN_DC9) == 0 && s2idle {
        intel_display_power_flush_work(display);
        intel_power_domains_verify_state(display);
        return;
    }

    if (*display).params.disable_power_well == 0 {
        let disable_wf = core::ptr::replace(&mut (*power_domains).disable_wakeref, core::ptr::null_mut());
        intel_display_power_put(display, POWER_DOMAIN_INIT, disable_wf);
    }

    intel_display_power_flush_work(display);
    intel_power_domains_verify_state(display);

    icl_display_core_uninit(display);
    (*power_domains).display_core_suspended = true;
}

// upstream: intel_display_power.c intel_power_domains_resume()
unsafe fn intel_power_domains_resume(display: *mut IntelDisplay) {
    let power_domains = addr_of_mut!((*display).power.domains);
    if (*power_domains).display_core_suspended {
        __intel_display_power_init_hw(display, true);
        (*power_domains).display_core_suspended = false;
    } else {
        (*power_domains).init_wakeref = intel_display_power_get(display, POWER_DOMAIN_INIT);
    }
}

// upstream: intel_display_power.c intel_power_domains_dump_info()
unsafe fn intel_power_domains_dump_info(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c intel_power_domains_verify_state()
unsafe fn intel_power_domains_verify_state(display: *mut IntelDisplay) {
    let power_domains = addr_of_mut!((*display).power.domains);
    mutex_lock(addr_of_mut!((*power_domains).lock));
    verify_async_put_domains_state(power_domains);
    mutex_unlock(addr_of_mut!((*power_domains).lock));
}

// upstream: intel_display_power.c intel_display_power_suspend_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_suspend_late(
    display: *mut IntelDisplay,
    s2idle: bool,
) {
    if display.is_null() {
        return;
    }
    intel_power_domains_suspend(display, s2idle);
    bxt_enable_dc9(display);
}

// upstream: intel_display_power.c intel_display_power_resume_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_resume_early(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    gen9_sanitize_dc_state(display);
    bxt_disable_dc9(display);
    intel_power_domains_resume(display);
}

// upstream: intel_display_power.c intel_display_power_runtime_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_runtime_suspend(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    icl_display_core_uninit(display);
    bxt_enable_dc9(display);
}

// upstream: intel_display_power.c intel_display_power_runtime_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_runtime_resume(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let power_domains = addr_of_mut!((*display).power.domains);
    bxt_disable_dc9(display);
    icl_display_core_init(display, true);
    if ((*power_domains).allowed_dc_mask & DC_STATE_EN_UPTO_DC6) != 0 {
        skl_enable_dc6(display);
    } else if ((*power_domains).allowed_dc_mask & DC_STATE_EN_UPTO_DC5) != 0 {
        gen9_enable_dc5(display);
    }
}

// upstream: intel_display_power.c intel_display_power_debug()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_debug(
    _display: *mut IntelDisplay,
    _m: *mut c_void,
) {}

// Port domains tables and platform mapping
#[derive(Clone, Copy)]
struct IntelDdiPortDomains {
    port_start: Port,
    port_end: Port,
    aux_ch_start: AuxCh,
    aux_ch_end: AuxCh,
    ddi_lanes: IntelDisplayPowerDomain,
    ddi_io: IntelDisplayPowerDomain,
    aux_io: IntelDisplayPowerDomain,
    aux_legacy_usbc: IntelDisplayPowerDomain,
    aux_tbt: IntelDisplayPowerDomain,
}

static I9XX_PORT_DOMAINS: [IntelDdiPortDomains; 1] = [
    IntelDdiPortDomains {
        port_start: PORT_A,
        port_end: PORT_F,
        aux_ch_start: AUX_CH_A,
        aux_ch_end: AUX_CH_F,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_A,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_A,
        aux_io: POWER_DOMAIN_AUX_IO_A,
        aux_legacy_usbc: POWER_DOMAIN_AUX_A,
        aux_tbt: POWER_DOMAIN_INVALID,
    },
];

static D11_PORT_DOMAINS: [IntelDdiPortDomains; 2] = [
    IntelDdiPortDomains {
        port_start: PORT_A,
        port_end: PORT_B,
        aux_ch_start: AUX_CH_A,
        aux_ch_end: AUX_CH_B,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_A,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_A,
        aux_io: POWER_DOMAIN_AUX_IO_A,
        aux_legacy_usbc: POWER_DOMAIN_AUX_A,
        aux_tbt: POWER_DOMAIN_INVALID,
    },
    IntelDdiPortDomains {
        port_start: PORT_C,
        port_end: PORT_F,
        aux_ch_start: AUX_CH_C,
        aux_ch_end: AUX_CH_F,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_C,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_C,
        aux_io: POWER_DOMAIN_AUX_IO_C,
        aux_legacy_usbc: POWER_DOMAIN_AUX_C,
        aux_tbt: POWER_DOMAIN_AUX_TBT1,
    },
];

static D12_PORT_DOMAINS: [IntelDdiPortDomains; 2] = [
    IntelDdiPortDomains {
        port_start: PORT_A,
        port_end: PORT_C,
        aux_ch_start: AUX_CH_A,
        aux_ch_end: AUX_CH_C,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_A,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_A,
        aux_io: POWER_DOMAIN_AUX_IO_A,
        aux_legacy_usbc: POWER_DOMAIN_AUX_A,
        aux_tbt: POWER_DOMAIN_INVALID,
    },
    IntelDdiPortDomains {
        port_start: PORT_TC1,
        port_end: PORT_TC6,
        aux_ch_start: AUX_CH_USBC1,
        aux_ch_end: AUX_CH_USBC6,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_TC1,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_TC1,
        aux_io: POWER_DOMAIN_INVALID,
        aux_legacy_usbc: POWER_DOMAIN_AUX_USBC1,
        aux_tbt: POWER_DOMAIN_AUX_TBT1,
    },
];

static D13_PORT_DOMAINS: [IntelDdiPortDomains; 3] = [
    IntelDdiPortDomains {
        port_start: PORT_A,
        port_end: PORT_C,
        aux_ch_start: AUX_CH_A,
        aux_ch_end: AUX_CH_C,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_A,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_A,
        aux_io: POWER_DOMAIN_AUX_IO_A,
        aux_legacy_usbc: POWER_DOMAIN_AUX_A,
        aux_tbt: POWER_DOMAIN_INVALID,
    },
    IntelDdiPortDomains {
        port_start: PORT_TC1,
        port_end: PORT_TC4,
        aux_ch_start: AUX_CH_USBC1,
        aux_ch_end: AUX_CH_USBC4,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_TC1,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_TC1,
        aux_io: POWER_DOMAIN_INVALID,
        aux_legacy_usbc: POWER_DOMAIN_AUX_USBC1,
        aux_tbt: POWER_DOMAIN_AUX_TBT1,
    },
    IntelDdiPortDomains {
        port_start: PORT_D_XELPD,
        port_end: PORT_E_XELPD,
        aux_ch_start: AUX_CH_D_XELPD,
        aux_ch_end: AUX_CH_E_XELPD,
        ddi_lanes: POWER_DOMAIN_PORT_DDI_LANES_D,
        ddi_io: POWER_DOMAIN_PORT_DDI_IO_D,
        aux_io: POWER_DOMAIN_AUX_IO_D,
        aux_legacy_usbc: POWER_DOMAIN_AUX_D,
        aux_tbt: POWER_DOMAIN_INVALID,
    },
];

// upstream: intel_display_power.c intel_port_domains_for_platform()
unsafe fn intel_port_domains_for_platform(
    display: *mut IntelDisplay,
    domains: &mut &'static [IntelDdiPortDomains],
) {
    let ver = if !display.is_null() {
        (*display).info.__runtime_info.ip.ver
    } else {
        13
    };

    if ver >= 13 {
        *domains = &D13_PORT_DOMAINS;
    } else if ver >= 12 {
        *domains = &D12_PORT_DOMAINS;
    } else if ver >= 11 {
        *domains = &D11_PORT_DOMAINS;
    } else {
        *domains = &I9XX_PORT_DOMAINS;
    }
}

// upstream: intel_display_power.c intel_port_domains_for_port()
unsafe fn intel_port_domains_for_port(
    display: *mut IntelDisplay,
    port: Port,
) -> Option<&'static IntelDdiPortDomains> {
    let mut domains: &'static [IntelDdiPortDomains] = &[];
    intel_port_domains_for_platform(display, &mut domains);
    for d in domains {
        if port >= d.port_start && port <= d.port_end {
            return Some(d);
        }
    }
    None
}

// upstream: intel_display_power.c intel_display_power_ddi_io_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_ddi_io_domain(
    display: *mut IntelDisplay,
    port: Port,
) -> IntelDisplayPowerDomain {
    if let Some(d) = intel_port_domains_for_port(display, port) {
        if d.ddi_io != POWER_DOMAIN_INVALID {
            return (d.ddi_io as i32 + (port - d.port_start)) as IntelDisplayPowerDomain;
        }
    }
    POWER_DOMAIN_PORT_DDI_IO_A
}

// upstream: intel_display_power.c intel_display_power_ddi_lanes_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_ddi_lanes_domain(
    display: *mut IntelDisplay,
    port: Port,
) -> IntelDisplayPowerDomain {
    if let Some(d) = intel_port_domains_for_port(display, port) {
        if d.ddi_lanes != POWER_DOMAIN_INVALID {
            return (d.ddi_lanes as i32 + (port - d.port_start)) as IntelDisplayPowerDomain;
        }
    }
    POWER_DOMAIN_PORT_DDI_LANES_A
}

// upstream: intel_display_power.c intel_port_domains_for_aux_ch()
unsafe fn intel_port_domains_for_aux_ch(
    display: *mut IntelDisplay,
    aux_ch: AuxCh,
) -> Option<&'static IntelDdiPortDomains> {
    let mut domains: &'static [IntelDdiPortDomains] = &[];
    intel_port_domains_for_platform(display, &mut domains);
    for d in domains {
        if aux_ch >= d.aux_ch_start && aux_ch <= d.aux_ch_end {
            return Some(d);
        }
    }
    None
}

// upstream: intel_display_power.c intel_display_power_aux_io_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_aux_io_domain(
    display: *mut IntelDisplay,
    aux_ch: AuxCh,
) -> IntelDisplayPowerDomain {
    if let Some(d) = intel_port_domains_for_aux_ch(display, aux_ch) {
        if d.aux_io != POWER_DOMAIN_INVALID {
            return (d.aux_io as i32 + (aux_ch - d.aux_ch_start)) as IntelDisplayPowerDomain;
        }
    }
    POWER_DOMAIN_AUX_IO_A
}

// upstream: intel_display_power.c intel_display_power_legacy_aux_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_legacy_aux_domain(
    display: *mut IntelDisplay,
    aux_ch: AuxCh,
) -> IntelDisplayPowerDomain {
    if let Some(d) = intel_port_domains_for_aux_ch(display, aux_ch) {
        if d.aux_legacy_usbc != POWER_DOMAIN_INVALID {
            return (d.aux_legacy_usbc as i32 + (aux_ch - d.aux_ch_start)) as IntelDisplayPowerDomain;
        }
    }
    POWER_DOMAIN_AUX_A
}

// upstream: intel_display_power.c intel_display_power_tbt_aux_domain()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_tbt_aux_domain(
    display: *mut IntelDisplay,
    aux_ch: AuxCh,
) -> IntelDisplayPowerDomain {
    if let Some(d) = intel_port_domains_for_aux_ch(display, aux_ch) {
        if d.aux_tbt != POWER_DOMAIN_INVALID {
            return (d.aux_tbt as i32 + (aux_ch - d.aux_ch_start)) as IntelDisplayPowerDomain;
        }
    }
    POWER_DOMAIN_AUX_TBT1
}
