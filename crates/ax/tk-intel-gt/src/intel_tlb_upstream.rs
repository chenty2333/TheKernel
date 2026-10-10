// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation.
//! Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_tlb.c` translation.

#![allow(unsafe_code)]

use core::{
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    intel_engine_types_upstream::{IntelEngineCs, IntelEngineMask},
    intel_gt_types_upstream::IntelGt,
    intel_uncore_types_upstream::{
        FORCEWAKE_ALL, intel_uncore_forcewake_get, intel_uncore_forcewake_put_delayed,
        intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::{I915McrRegT, I915RegT},
    linux::{
        i915::{
            GRAPHICS_VER, HAS_GUC_TLB_INVALIDATION, IS_PLATFORM,
        },
        locks::{spin_lock_raw, spin_unlock_raw},
        mutex::mutex_destroy,
        pm::{
            intel_engine_pm_is_awake, intel_gt_pm_get_if_awake, intel_gt_pm_put_async,
        },
    },
    linux_config::{CONFIG_DRM_I915_SELFTEST, ENODEV},
};

const TLB_INVAL_TIMEOUT_US: u32 = 100;
const TLB_INVAL_TIMEOUT_MS: u32 = 4;
const GEN12_OA_TLB_INV_CR: I915RegT = I915RegT { reg: 0xceec };
const INTEL_TIGERLAKE: u32 = 31;
const INTEL_DG1: u32 = 33;
const INTEL_ROCKETLAKE: u32 = 32;
const INTEL_ALDERLAKE_S: u32 = 34;
const INTEL_ALDERLAKE_P: u32 = 35;
static LAST_TLB_ERROR_NS: AtomicU64 = AtomicU64::new(0);

fn tlb_error_ratelimit_allow() -> bool {
    const INTERVAL_NS: u64 = 5_000_000_000;
    let now = axhal::time::monotonic_time_nanos();
    let mut previous = LAST_TLB_ERROR_NS.load(Ordering::Acquire);
    loop {
        if previous != 0 && now.saturating_sub(previous) < INTERVAL_NS {
            return false;
        }
        match LAST_TLB_ERROR_NS.compare_exchange_weak(
            previous,
            now.max(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => previous = actual,
        }
    }
}

// upstream: intel_tlb.h intel_gt_tlb_seqno()
#[inline]
unsafe fn intel_gt_tlb_seqno(gt: *const IntelGt) -> u32 {
    unsafe { ptr::read_volatile(ptr::addr_of!((*gt).tlb.seqno.sequence)) }
}

// upstream: intel_tlb.c wait_for_invalidate()
unsafe fn wait_for_invalidate(engine: *mut IntelEngineCs) -> i32 {
    if unsafe { (*engine).tlb_inv.mcr } {
        unsafe {
            crate::intel_gt_mcr_upstream::intel_gt_mcr_wait_for_reg(
                (*engine).gt,
                (*engine).tlb_inv.reg.mcr_reg,
                (*engine).tlb_inv.done,
                0,
                TLB_INVAL_TIMEOUT_US,
                TLB_INVAL_TIMEOUT_MS,
            )
        }
    } else {
        unsafe {
            crate::intel_uncore_types_upstream::__intel_wait_for_register_fw(
                (*(*engine).gt).uncore,
                (*engine).tlb_inv.reg.reg,
                (*engine).tlb_inv.done,
                0,
                TLB_INVAL_TIMEOUT_US,
                TLB_INVAL_TIMEOUT_MS,
                ptr::null_mut(),
            )
        }
    }
}

// upstream: intel_tlb.c mmio_invalidate_full()
unsafe fn mmio_invalidate_full(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    if unsafe { GRAPHICS_VER(i915) < 8 } {
        return;
    }

    unsafe { intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL) };
    let mut flags: usize = 0;
    unsafe {
        crate::intel_gt_mcr_upstream::intel_gt_mcr_lock(gt, &mut flags);
        spin_lock_raw(ptr::addr_of_mut!((*uncore).lock));
    }

    let mut awake: IntelEngineMask = 0;
    let mut engine: *mut IntelEngineCs = ptr::null_mut();
    let mut id = 0usize;
    for_each_engine!(engine, id, gt, {
        if !unsafe { intel_engine_pm_is_awake(engine) } {
            continue;
        }
        unsafe {
            if (*engine).tlb_inv.mcr {
                crate::intel_gt_mcr_upstream::intel_gt_mcr_multicast_write_fw(
                    gt,
                    (*engine).tlb_inv.reg.mcr_reg,
                    (*engine).tlb_inv.request,
                );
            } else {
                intel_uncore_write_fw(uncore, (*engine).tlb_inv.reg.reg, (*engine).tlb_inv.request);
            }
            awake |= (*engine).mask;
        }
    });
    GT_TRACE!(gt, "invalidated engines %08x\n", awake);

    if awake != 0
        && unsafe {
            [INTEL_TIGERLAKE, INTEL_DG1, INTEL_ROCKETLAKE, INTEL_ALDERLAKE_S, INTEL_ALDERLAKE_P]
                .into_iter()
                .any(|platform| IS_PLATFORM(i915, platform))
        }
    {
        unsafe { intel_uncore_write_fw(uncore, GEN12_OA_TLB_INV_CR, 1) };
    }

    unsafe {
        spin_unlock_raw(ptr::addr_of_mut!((*uncore).lock));
        crate::intel_gt_mcr_upstream::intel_gt_mcr_unlock(gt, flags);
    }

    let mut id = 0usize;
    for_each_engine_masked!(engine, id, gt, awake, {
        if unsafe { wait_for_invalidate(engine) } != 0 {
            if tlb_error_ratelimit_allow() {
                gt_err!(
                    gt,
                    "%s TLB invalidation did not complete in %ums!\n",
                    unsafe { (*engine).name },
                    TLB_INVAL_TIMEOUT_MS
                );
            }
        }
    });
    unsafe { intel_uncore_forcewake_put_delayed(uncore, FORCEWAKE_ALL) };
}

// upstream: intel_tlb.c tlb_seqno_passed()
unsafe fn tlb_seqno_passed(gt: *const IntelGt, seqno: u32) -> bool {
    let cur = unsafe { intel_gt_tlb_seqno(gt) };
    let full_seqno = seqno.wrapping_add(1) & !1;
    (cur.wrapping_sub(full_seqno) as i32) > 0
}

// upstream: intel_tlb.c intel_gt_invalidate_tlb_full()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_invalidate_tlb_full(gt: *mut IntelGt, seqno: u32) {
    if CONFIG_DRM_I915_SELFTEST
        && unsafe { (*gt).awake == ((-(ENODEV as isize)) as *mut _) }
    {
        return;
    }
    if unsafe { crate::intel_gt_api_upstream::intel_gt_is_wedged(gt) }
        || unsafe { tlb_seqno_passed(gt, seqno) }
    {
        return;
    }

    let wakeref = unsafe { intel_gt_pm_get_if_awake(gt) };
    if wakeref.is_null() {
        return;
    }
    let tlb = unsafe { ptr::addr_of_mut!((*gt).tlb) };
    unsafe { crate::linux::mutex::mutex_lock(ptr::addr_of_mut!((*tlb).invalidate_lock)) };
    if !unsafe { tlb_seqno_passed(gt, seqno) } {
        let guc = unsafe { ptr::addr_of_mut!((*gt).uc.guc) };
        if unsafe { HAS_GUC_TLB_INVALIDATION((*gt).i915) } {
            let guc_ready = unsafe {
                crate::intel_uc_fw_types_upstream::intel_uc_fw_is_running(ptr::addr_of!((*guc).fw))
                    && (*guc).ct.enabled
            };
            if guc_ready {
                let _ = unsafe { crate::guc_submission_upstream::intel_guc_invalidate_tlb_engines(&mut *guc) };
            }
        } else {
            unsafe { mmio_invalidate_full(gt) };
        }
        unsafe { (*tlb).seqno.sequence = (*tlb).seqno.sequence.wrapping_add(2) };
    }
    unsafe {
        crate::linux::mutex::mutex_unlock(ptr::addr_of_mut!((*tlb).invalidate_lock));
        intel_gt_pm_put_async(gt, wakeref);
    }
}

// upstream: intel_tlb.c intel_gt_init_tlb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_init_tlb(gt: *mut IntelGt) {
    unsafe {
        crate::linux::mutex::mutex_init(ptr::addr_of_mut!((*gt).tlb.invalidate_lock));
        (*gt).tlb.seqno.sequence = 0;
    }
}

// upstream: intel_tlb.c intel_gt_fini_tlb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_fini_tlb(gt: *mut IntelGt) {
    unsafe { mutex_destroy(&mut (*gt).tlb.invalidate_lock) };
}
