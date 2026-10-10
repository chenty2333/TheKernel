// SPDX-License-Identifier: MIT
// Copyright © 2013 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/intel_uncore.c.
// The MIT grant is retained in ../../LICENSE-MIT.
//! Intel i915 forcewake reference accounting for the LinuxKPI boundary.
//!
//! These routines intentionally rely on the chipset-specific `force_wake_get`
//! callback installed in `IntelUncoreFwGet` for the request/ack protocol.  This
//! module owns only the common domain selection, reference counts, and release
//! writes shared by the uncore code.

#![allow(unsafe_code, non_snake_case)]

use crate::{
    intel_runtime_pm_upstream::assert_rpm_wakelock_held,
    intel_uncore_types_upstream::{
        FW_DOMAIN_ID_COUNT, FW_REG_READ, FW_REG_WRITE, ForcewakeDomains, IntelUncore,
        IntelUncoreForcewakeDomain, intel_uncore_has_forcewake,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::locks::{spin_lock_irqsave, spin_unlock_irqrestore},
};

const FORCEWAKE_KERNEL: u32 = 1;

/// Resolve a domain advertised in `uncore.fw_domains`.
///
/// A missing domain pointer for an advertised bit is malformed initialized
/// state; silently skipping it would make a forcewake-protected MMIO access
/// proceed without its required power domain.
unsafe fn required_domain(uncore: *mut IntelUncore, id: usize) -> *mut IntelUncoreForcewakeDomain {
    assert!(
        id < FW_DOMAIN_ID_COUNT as usize,
        "forcewake domain id out of range"
    );
    let domain = unsafe { (*uncore).fw_domain[id] };
    assert!(
        !domain.is_null(),
        "advertised forcewake domain is uninitialized"
    );
    assert_eq!(unsafe { (*domain).mask }, 1_i32 << id);
    domain
}

/// Return the domains required to access `reg` in the requested mode.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_for_reg(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    op: u32,
) -> ForcewakeDomains {
    assert!(
        op != 0,
        "forcewake register access requires read and/or write"
    );
    let uncore_ref = unsafe { &mut *uncore };
    if !intel_uncore_has_forcewake(uncore_ref) {
        return 0;
    }

    let mut domains = 0;
    if op & FW_REG_READ != 0 {
        let read_domains = uncore_ref
            .funcs
            .read_fw_domains
            .expect("forcewake read-domain callback is uninitialized");
        domains |= unsafe { read_domains(uncore, reg) };
    }
    if op & FW_REG_WRITE != 0 {
        let write_domains = uncore_ref
            .funcs
            .write_fw_domains
            .expect("forcewake write-domain callback is uninitialized");
        domains |= unsafe { write_domains(uncore, reg) };
    }

    assert_eq!(
        domains & !uncore_ref.fw_domains,
        0,
        "register callback returned unavailable forcewake domains"
    );
    domains
}

/// Increment references while the caller holds `uncore.lock`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_get__locked(
    uncore: *mut IntelUncore,
    domains: ForcewakeDomains,
) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    let available = unsafe { (*uncore).fw_domains };
    let requested = domains & available;
    let mut newly_awake = 0;
    for id in 0..FW_DOMAIN_ID_COUNT as usize {
        let mask = 1_i32 << id;
        if requested & mask == 0 {
            continue;
        }
        let domain = unsafe { required_domain(uncore, id) };
        let old_count = unsafe { (*domain).wake_count };
        assert!(old_count != u32::MAX, "forcewake reference count overflow");
        unsafe { (*domain).wake_count = old_count + 1 };
        if old_count == 0 {
            newly_awake |= mask;
        } else {
            // Prevent a delayed release from dropping a domain while a new
            // explicit reference is held (matching Linux's active marker).
            unsafe { (*domain).active = true };
        }
    }

    if newly_awake != 0 {
        let callback = unsafe { (*(*uncore).fw_get_funcs).force_wake_get }
            .expect("forcewake get callback is uninitialized");
        unsafe { callback(uncore, newly_awake) };
        unsafe { (*uncore).fw_domains_active |= newly_awake };
    }
}

/// Drop references while the caller holds `uncore.lock`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_put__locked(
    uncore: *mut IntelUncore,
    domains: ForcewakeDomains,
) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    let requested = domains & unsafe { (*uncore).fw_domains };
    let mut released = 0;
    for id in 0..FW_DOMAIN_ID_COUNT as usize {
        let mask = 1_i32 << id;
        if requested & mask == 0 {
            continue;
        }
        let domain = unsafe { required_domain(uncore, id) };
        let old_count = unsafe { (*domain).wake_count };
        assert!(old_count != 0, "unbalanced forcewake put");
        let new_count = old_count - 1;
        unsafe { (*domain).wake_count = new_count };
        if new_count == 0 {
            let reg_set = unsafe { (*domain).reg_set };
            assert!(
                !reg_set.is_null(),
                "forcewake release register is uninitialized"
            );
            // `fw_domain_put()` writes REG_MASKED_FIELD_DISABLE(KERNEL) to
            // the domain's MMIO set register. The chipset callback handles
            // request acknowledgement on acquire; upstream does not wait on
            // the clear path.
            unsafe {
                core::ptr::write_volatile(reg_set, REG_MASKED_FIELD_DISABLE!(FORCEWAKE_KERNEL))
            };
            released |= mask;
        } else {
            unsafe { (*domain).active = true };
        }
    }
    unsafe { (*uncore).fw_domains_active &= !released };
}

/// Acquire references, serializing with other uncore state changes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_get(
    uncore: *mut IntelUncore,
    domains: ForcewakeDomains,
) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }
    unsafe { assert_rpm_wakelock_held((*uncore).rpm) };
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut flags) };
    unsafe { intel_uncore_forcewake_get__locked(uncore, domains) };
    unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, flags) };
}

/// Release references, serializing with other uncore state changes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_put(
    uncore: *mut IntelUncore,
    domains: ForcewakeDomains,
) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut flags) };
    unsafe { intel_uncore_forcewake_put__locked(uncore, domains) };
    unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, flags) };
}
