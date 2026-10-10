// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_gt_pm.c.
// The full MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{
    ffi::{c_long, c_void},
    ptr,
    sync::atomic::{AtomicU32, Ordering, fence},
};

use crate::{
    i915_vma_api_upstream::i915_vma_parked,
    intel_context_upstream::IntelWakerefHandle,
    intel_engine_api_upstream::__intel_engine_reset,
    intel_engine_cs_upstream::Seqcount,
    intel_engine_types_upstream::I915_NUM_ENGINES,
    intel_gt_api_upstream::{
        intel_gt_bind_context_set_ready, intel_gt_bind_context_set_unready,
        intel_gt_check_and_clear_faults, intel_gt_has_unrecoverable_error, intel_gt_init_hw,
        intel_gt_wait_for_idle,
    },
    intel_gt_clock_utils_upstream::intel_gt_check_clock_frequency,
    intel_gt_mcr_impl_upstream::intel_gt_mcr_lock_sanitize,
    intel_gt_requests_upstream::{
        intel_gt_park_requests, intel_gt_retire_requests_timeout, intel_gt_unpark_requests,
    },
    intel_gt_types_upstream::IntelGt,
    intel_guc_submission_types_upstream::{intel_guc_busyness_park, intel_guc_busyness_unpark},
    intel_llc_upstream::{intel_llc_disable, intel_llc_enable},
    intel_rc6_types_upstream::IntelRc6,
    intel_reset_hw_upstream::intel_gt_reset_all_engines,
    intel_reset_upstream::intel_gt_gpu_reset_clobbers_display,
    intel_rps_types_upstream::IntelRps,
    intel_uc_upstream::{
        intel_uc_reset, intel_uc_reset_finish, intel_uc_reset_prepare, intel_uc_resume,
        intel_uc_runtime_resume, intel_uc_runtime_suspend, intel_uc_suspend,
    },
    intel_uncore_types_upstream::{
        FORCEWAKE_ALL, intel_uncore_forcewake_get, intel_uncore_forcewake_put,
        intel_uncore_resume_early,
    },
    intel_wakeref_types_upstream::{
        INTEL_WAKEREF_DEF, IntelWakeref, IntelWakerefOps, intel_wakeref_wait_for_idle,
    },
    linux::{
        i915::intel_gt_is_wedged,
        locks::{local_irq_disable, local_irq_enable},
        memory::{atomic_add, atomic_read},
        pm::{
            intel_engine_pm_get, intel_engine_pm_put, intel_gt_pm_get, intel_gt_pm_is_awake,
            intel_gt_pm_put, intel_runtime_pm_get, intel_runtime_pm_put, intel_wakeref_init,
        },
        primitives::{fetch_and_zero, ktime_get},
    },
    linux_i915_private::DrmI915Private,
};

const I915_GT_SUSPEND_IDLE_TIMEOUT: c_long = (crate::linux_config::HZ / 2) as c_long;
const POWER_DOMAIN_GT_IRQ: i32 = 72;
const PM_SUSPEND_ON: i32 = 0;
const PM_SUSPEND_TO_IDLE: i32 = 1;
const CONFIG_SUSPEND: bool = true;
const CONFIG_PM_SLEEP: bool = true;
const CONFIG_DRM_I915_DEBUG_RUNTIME_PM: bool = false;

/// Linux `kernel/power/suspend.c` exports this current target to device PM.
/// It defaults to `PM_SUSPEND_ON`, as in the source C global; a system-PM
/// owner may update it before invoking the imported GT suspend callbacks.
#[unsafe(no_mangle)]
pub static mut pm_suspend_target_state: i32 = PM_SUSPEND_ON;

unsafe extern "C" {
    fn intel_rc6_init(rc6: *mut IntelRc6);
    fn intel_rc6_fini(rc6: *mut IntelRc6);
    fn intel_rc6_unpark(rc6: *mut IntelRc6);
    fn intel_rc6_park(rc6: *mut IntelRc6);
    fn intel_rc6_sanitize(rc6: *mut IntelRc6);
    fn intel_rc6_enable(rc6: *mut IntelRc6);
    fn intel_rc6_disable(rc6: *mut IntelRc6);

    fn intel_rps_init(rps: *mut IntelRps);
    fn intel_rps_sanitize(rps: *mut IntelRps);
    fn intel_rps_unpark(rps: *mut IntelRps);
    fn intel_rps_park(rps: *mut IntelRps);
    fn intel_rps_enable(rps: *mut IntelRps);
    fn intel_rps_disable(rps: *mut IntelRps);

    fn intel_display_power_get(display: *mut c_void, domain: i32) -> IntelWakerefHandle;
    fn __intel_display_power_put_async(
        display: *mut c_void,
        domain: i32,
        wakeref: IntelWakerefHandle,
        delay_ms: i32,
    );
    fn intel_synchronize_irq(i915: *mut DrmI915Private);
    fn i915_pmu_gt_unparked(gt: *mut IntelGt);
    fn i915_pmu_gt_parked(gt: *mut IntelGt);
    fn intel_gt_set_wedged(gt: *mut IntelGt);
    fn intel_gt_unset_wedged(gt: *mut IntelGt) -> bool;
}

/// `intel_display_power_put_async()` header inline for the configured DRM PM
/// debug setting; without debug wakeref tracking, Linux passes the sentinel.
unsafe fn intel_display_power_put_async(
    display: *mut c_void,
    domain: i32,
    wakeref: IntelWakerefHandle,
) {
    let wakeref = if CONFIG_DRM_I915_DEBUG_RUNTIME_PM {
        wakeref
    } else {
        INTEL_WAKEREF_DEF
    };
    unsafe { __intel_display_power_put_async(display, domain, wakeref, -1) };
}

#[inline]
fn ktime_add(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}

#[inline]
fn ktime_sub(a: i64, b: i64) -> i64 {
    a.wrapping_sub(b)
}

#[inline]
fn seqcount_init(seq: *mut Seqcount) {
    unsafe { AtomicU32::from_ptr(ptr::addr_of_mut!((*seq).sequence)).store(0, Ordering::Relaxed) };
}

#[inline]
fn seqcount_mutex_init(seq: *mut Seqcount, _mutex: *mut crate::intel_engine_cs_upstream::Mutex) {
    // CONFIG_PREEMPT_RT=n in the configured Linux 7.2.3 ABI, where
    // seqcount_mutex_t is seqcount_t and has no embedded mutex pointer.
    seqcount_init(seq);
}

#[inline]
fn write_seqcount_begin(seq: *mut Seqcount) {
    unsafe {
        AtomicU32::from_ptr(ptr::addr_of_mut!((*seq).sequence)).fetch_add(1, Ordering::Relaxed)
    };
    fence(Ordering::Release);
}

#[inline]
fn write_seqcount_end(seq: *mut Seqcount) {
    fence(Ordering::Release);
    unsafe {
        AtomicU32::from_ptr(ptr::addr_of_mut!((*seq).sequence)).fetch_add(1, Ordering::Release)
    };
}

#[inline]
fn read_seqcount_begin(seq: *const Seqcount) -> u32 {
    loop {
        let value = unsafe {
            AtomicU32::from_ptr(ptr::addr_of!((*seq).sequence).cast_mut()).load(Ordering::Acquire)
        };
        if value & 1 == 0 {
            return value;
        }
        core::hint::spin_loop();
    }
}

#[inline]
fn read_seqcount_retry(seq: *const Seqcount, start: u32) -> bool {
    fence(Ordering::Acquire);
    unsafe {
        AtomicU32::from_ptr(ptr::addr_of!((*seq).sequence).cast_mut()).load(Ordering::Acquire)
            != start
    }
}

#[inline]
unsafe fn gt_pm_wait_for_idle(gt: *mut IntelGt) {
    unsafe { intel_wakeref_wait_for_idle(ptr::addr_of_mut!((*gt).wakeref)) };
}

#[inline]
unsafe fn gt_retire_requests(gt: *mut IntelGt) {
    let _ = unsafe { intel_gt_retire_requests_timeout(gt, 0, ptr::null_mut()) };
}

#[inline]
unsafe fn is_mock_gt(gt: *const IntelGt) -> bool {
    if !crate::linux_config::CONFIG_DRM_I915_SELFTEST {
        return false;
    }
    let mock = crate::linux_config::ERR_PTR::<c_void>(-crate::linux_config::ENODEV)
        .cast::<crate::intel_context_upstream::RefTracker>();
    unsafe { (*gt).awake == mock }
}

// upstream: intel_gt_pm.c user_forcewake()
unsafe fn user_forcewake(gt: *mut IntelGt, suspend: bool) {
    let count = unsafe { atomic_read(&(*gt).user_wakeref) };
    if count == 0 {
        return;
    }

    let wakeref = intel_gt_pm_get(gt);
    if suspend {
        GEM_BUG_ON!(count > unsafe { atomic_read(&(*gt).wakeref.count) });
        unsafe { atomic_add(-count, &mut (*gt).wakeref.count) };
    } else {
        unsafe { atomic_add(count, &mut (*gt).wakeref.count) };
    }
    intel_gt_pm_put(gt, wakeref);
}

// upstream: intel_gt_pm.c runtime_begin()
unsafe fn runtime_begin(gt: *mut IntelGt) {
    local_irq_disable();
    unsafe {
        write_seqcount_begin(ptr::addr_of_mut!((*gt).stats.lock));
        (*gt).stats.start = ktime_get();
        (*gt).stats.active = true;
        write_seqcount_end(ptr::addr_of_mut!((*gt).stats.lock));
    }
    local_irq_enable();
}

// upstream: intel_gt_pm.c runtime_end()
unsafe fn runtime_end(gt: *mut IntelGt) {
    local_irq_disable();
    unsafe {
        write_seqcount_begin(ptr::addr_of_mut!((*gt).stats.lock));
        (*gt).stats.active = false;
        (*gt).stats.total = ktime_add((*gt).stats.total, ktime_sub(ktime_get(), (*gt).stats.start));
        write_seqcount_end(ptr::addr_of_mut!((*gt).stats.lock));
    }
    local_irq_enable();
}

// upstream: intel_gt_pm.c __gt_unpark()
unsafe extern "C" fn __gt_unpark(wf: *mut IntelWakeref) -> i32 {
    let gt = container_of!(wf, IntelGt, wakeref);
    let i915 = unsafe { (*gt).i915 };
    let display = unsafe { (*i915).display };

    GT_TRACE!(gt, "\n");
    unsafe {
        (*gt).awake = intel_display_power_get(display, POWER_DOMAIN_GT_IRQ);
        GEM_BUG_ON!((*gt).awake.is_null());

        intel_rc6_unpark(ptr::addr_of_mut!((*gt).rc6));
        intel_rps_unpark(ptr::addr_of_mut!((*gt).rps));
        i915_pmu_gt_unparked(gt);
        intel_guc_busyness_unpark(gt);
        intel_gt_unpark_requests(gt);
        runtime_begin(gt);
    }
    0
}

// upstream: intel_gt_pm.c __gt_park()
unsafe extern "C" fn __gt_park(wf: *mut IntelWakeref) -> i32 {
    let gt = container_of!(wf, IntelGt, wakeref);
    let wakeref = unsafe { fetch_and_zero(&mut (*gt).awake) };
    let i915 = unsafe { (*gt).i915 };
    let display = unsafe { (*i915).display };

    GT_TRACE!(gt, "\n");
    unsafe {
        runtime_end(gt);
        intel_gt_park_requests(gt);
        intel_guc_busyness_park(gt);
        i915_vma_parked(gt);
        i915_pmu_gt_parked(gt);
        intel_rps_park(ptr::addr_of_mut!((*gt).rps));
        intel_rc6_park(ptr::addr_of_mut!((*gt).rc6));

        // Everything switched off, flush any residual interrupt just in case.
        intel_synchronize_irq(i915);

        // Defer dropping the display power well for 100 ms; it is slow.
        GEM_BUG_ON!(wakeref.is_null());
        intel_display_power_put_async(display, POWER_DOMAIN_GT_IRQ, wakeref);
    }
    0
}

static WF_OPS: IntelWakerefOps = IntelWakerefOps {
    get: Some(__gt_unpark),
    put: Some(__gt_park),
};

// upstream: intel_gt_pm.c intel_gt_pm_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_pm_init_early(gt: *mut IntelGt) {
    // Runtime PM is device-wide, so initialize the GT wakeref through i915,
    // not `gt->uncore`, which need not exist for every tile yet.
    unsafe {
        intel_wakeref_init(ptr::addr_of_mut!((*gt).wakeref), (*gt).i915, &WF_OPS);
        seqcount_mutex_init(
            ptr::addr_of_mut!((*gt).stats.lock),
            ptr::addr_of_mut!((*gt).wakeref.mutex),
        );
    }
}

// upstream: intel_gt_pm.c intel_gt_pm_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_pm_init(gt: *mut IntelGt) {
    unsafe {
        intel_rc6_init(ptr::addr_of_mut!((*gt).rc6));
        intel_rps_init(ptr::addr_of_mut!((*gt).rps));
    }
}

// upstream: intel_gt_pm.c reset_engines()
unsafe fn reset_engines(gt: *mut IntelGt) -> bool {
    if unsafe { intel_gt_gpu_reset_clobbers_display(gt) } {
        return false;
    }
    unsafe { intel_gt_reset_all_engines(gt) == 0 }
}

// upstream: intel_gt_pm.c gt_sanitize()
unsafe fn gt_sanitize(gt: *mut IntelGt, force: bool) {
    let uncore = unsafe { (*gt).uncore };
    GT_TRACE!(
        gt,
        "force:%s\n",
        crate::linux::primitives::str_yes_no(force)
    );

    // Use a raw runtime-PM wakeref to avoid early display-power access.
    let wakeref = unsafe { intel_runtime_pm_get((*uncore).rpm) };
    unsafe { intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL) };
    unsafe { intel_gt_check_clock_frequency(gt) };

    // After deep PCI sleep the hardware is assumed reset, so clear a
    // recoverable wedge left by the previous suspend cycle.
    if unsafe { intel_gt_is_wedged(gt) } {
        unsafe { intel_gt_unset_wedged(gt) };
    }

    unsafe { intel_uc_reset_prepare(ptr::addr_of_mut!((*gt).uc)) };
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        unsafe {
            if let Some(prepare) = (*engine).reset.prepare {
                prepare(engine);
            }
            if let Some(sanitize) = (*engine).sanitize {
                sanitize(engine);
            }
        }
    }

    if unsafe { reset_engines(gt) } || force {
        for id in 0..I915_NUM_ENGINES as usize {
            let engine = unsafe { (*gt).engine[id] };
            if engine.is_null() {
                continue;
            }
            unsafe { __intel_engine_reset(engine, false) };
        }
    }

    unsafe { intel_uc_reset(ptr::addr_of_mut!((*gt).uc), 0) };
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        if let Some(finish) = unsafe { (*engine).reset.finish } {
            unsafe { finish(engine) };
        }
    }

    unsafe {
        intel_rps_sanitize(ptr::addr_of_mut!((*gt).rps));
        intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL);
        intel_runtime_pm_put((*uncore).rpm, wakeref);
    }
}

// upstream: intel_gt_pm.c intel_gt_pm_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_pm_fini(gt: *mut IntelGt) {
    unsafe { intel_rc6_fini(ptr::addr_of_mut!((*gt).rc6)) };
}

// upstream: intel_gt_pm.c intel_gt_resume_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_resume_early(gt: *mut IntelGt) {
    // The GT is quiescent during driver resume, so sanitize any steering
    // semaphore left held across suspend before ordinary register access.
    unsafe {
        intel_gt_mcr_lock_sanitize(gt);
        intel_uncore_resume_early((*gt).uncore);
        intel_gt_check_and_clear_faults(gt);
    }
}

// upstream: intel_gt_pm.c intel_gt_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_resume(gt: *mut IntelGt) -> i32 {
    let mut err = unsafe { intel_gt_has_unrecoverable_error(gt) as i32 };
    if err != 0 {
        return err;
    }
    GT_TRACE!(gt, "\n");

    // The pinned kernel contexts are repaired after this forced sanitize.
    unsafe { gt_sanitize(gt, true) };
    let wakeref = intel_gt_pm_get(gt);
    let uncore = unsafe { (*gt).uncore };

    unsafe {
        intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL);
        intel_rc6_sanitize(ptr::addr_of_mut!((*gt).rc6));
        if intel_gt_is_wedged(gt) {
            err = -crate::linux_config::EIO;
            goto_out_forcewake(gt, wakeref);
            return err;
        }

        // Only after reinitializing hardware can outstanding requests replay.
        err = intel_gt_init_hw(gt);
        if err != 0 {
            gt_err!(gt, "Failed to initialize GPU, declaring it wedged!\n");
            intel_gt_set_wedged(gt);
            goto_out_forcewake(gt, wakeref);
            return err;
        }

        intel_uc_reset_finish(ptr::addr_of_mut!((*gt).uc));
        intel_rps_enable(ptr::addr_of_mut!((*gt).rps));
        intel_llc_enable(ptr::addr_of_mut!((*gt).llc));

        for id in 0..I915_NUM_ENGINES as usize {
            let engine = (*gt).engine[id];
            if engine.is_null() {
                continue;
            }
            intel_engine_pm_get(engine);
            (*engine).serial = (*engine).serial.wrapping_add(1); // kernel context lost
            err = crate::intel_engine_cs_upstream::intel_engine_resume(engine);
            intel_engine_pm_put(engine);
            if err != 0 {
                gt_err!(
                    gt,
                    "Failed to restart %s (%d)\n",
                    (*engine).name.as_ptr(),
                    err
                );
                intel_gt_set_wedged(gt);
                goto_out_forcewake(gt, wakeref);
                return err;
            }
        }

        intel_rc6_enable(ptr::addr_of_mut!((*gt).rc6));
        let _ = intel_uc_resume(ptr::addr_of_mut!((*gt).uc));
        user_forcewake(gt, false);
        goto_out_forcewake(gt, wakeref);
    }

    err
}

unsafe fn goto_out_forcewake(gt: *mut IntelGt, wakeref: IntelWakerefHandle) {
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL);
        intel_gt_pm_put(gt, wakeref);
        intel_gt_bind_context_set_ready(gt);
    }
}

// upstream: intel_gt_pm.c wait_for_suspend()
unsafe fn wait_for_suspend(gt: *mut IntelGt) {
    if !unsafe { intel_gt_pm_is_awake(gt) } {
        return;
    }

    if unsafe { intel_gt_wait_for_idle(gt, I915_GT_SUSPEND_IDLE_TIMEOUT) }
        == -crate::linux_config::ETIME
    {
        // Forcibly cancel outstanding work and leave the GPU quiet.
        unsafe {
            intel_gt_set_wedged(gt);
            gt_retire_requests(gt);
        }
    }
    unsafe { gt_pm_wait_for_idle(gt) };
}

// upstream: intel_gt_pm.c intel_gt_suspend_prepare()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_suspend_prepare(gt: *mut IntelGt) {
    unsafe {
        intel_gt_bind_context_set_unready(gt);
        user_forcewake(gt, true);
        wait_for_suspend(gt);
    }
}

// upstream: intel_gt_pm.c pm_suspend_target()
unsafe fn pm_suspend_target() -> i32 {
    if CONFIG_SUSPEND && CONFIG_PM_SLEEP {
        unsafe { ptr::read_volatile(ptr::addr_of!(pm_suspend_target_state)) }
    } else {
        PM_SUSPEND_TO_IDLE
    }
}

// upstream: intel_gt_pm.c intel_gt_suspend_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_suspend_late(gt: *mut IntelGt) {
    unsafe { wait_for_suspend(gt) };
    if unsafe { is_mock_gt(gt) } {
        return;
    }
    GEM_BUG_ON!(!unsafe { (*gt).awake }.is_null());

    unsafe { intel_uc_suspend(ptr::addr_of_mut!((*gt).uc)) };

    // S2idle leaves the device powered and runtime-PM state intact; deeper
    // suspend targets require explicitly disabling RC6/RPS/LLC first.
    if unsafe { pm_suspend_target() } == PM_SUSPEND_TO_IDLE {
        return;
    }

    let uncore = unsafe { (*gt).uncore };
    let wakeref = unsafe { intel_runtime_pm_get((*uncore).rpm) };
    unsafe {
        intel_rps_disable(ptr::addr_of_mut!((*gt).rps));
        intel_rc6_disable(ptr::addr_of_mut!((*gt).rc6));
        intel_llc_disable(ptr::addr_of_mut!((*gt).llc));
        intel_runtime_pm_put((*uncore).rpm, wakeref);
        gt_sanitize(gt, false);
    }

    GT_TRACE!(gt, "\n");
}

// upstream: intel_gt_pm.c intel_gt_runtime_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_runtime_suspend(gt: *mut IntelGt) {
    unsafe {
        intel_gt_bind_context_set_unready(gt);
        intel_uc_runtime_suspend(ptr::addr_of_mut!((*gt).uc));
    }
    GT_TRACE!(gt, "\n");
}

// upstream: intel_gt_pm.c intel_gt_runtime_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_runtime_resume(gt: *mut IntelGt) -> i32 {
    GT_TRACE!(gt, "\n");
    unsafe {
        crate::intel_ggtt_fencing_upstream::intel_gt_init_swizzling(gt);
        crate::intel_ggtt_fencing_upstream::intel_ggtt_restore_fences((*gt).ggtt);
        let ret = intel_uc_runtime_resume(ptr::addr_of_mut!((*gt).uc));
        if ret != 0 {
            return ret;
        }
        intel_gt_bind_context_set_ready(gt);
    }
    0
}

// upstream: intel_gt_pm.c __intel_gt_get_awake_time()
unsafe fn __intel_gt_get_awake_time(gt: *const IntelGt) -> i64 {
    let mut total = unsafe { (*gt).stats.total };
    if unsafe { (*gt).stats.active } {
        total = ktime_add(total, ktime_sub(ktime_get(), unsafe { (*gt).stats.start }));
    }
    total
}

// upstream: intel_gt_pm.c intel_gt_get_awake_time()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_get_awake_time(gt: *const IntelGt) -> i64 {
    let seq = unsafe { ptr::addr_of!((*gt).stats.lock) };
    let mut total;
    loop {
        let start = read_seqcount_begin(seq);
        total = unsafe { __intel_gt_get_awake_time(gt) };
        if !read_seqcount_retry(seq, start) {
            break;
        }
    }
    total
}
