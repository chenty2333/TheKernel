// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/intel_wakeref.c.
// The full MIT grant is retained in ../LICENSE-MIT.
//
// Configuration (oracle wt-dev build): CONFIG_LOCKDEP=n, CONFIG_DRM_I915_DEBUG_WAKEREF
// unset, CONFIG_REF_TRACKER unset. INTEL_WAKEREF_BUG_ON() therefore compiles out,
// lockdep map initialisation is absent, and ref_tracker_dir_snprint() is the
// header inline that reports zero bytes.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{ptr, sync::atomic::{Ordering, fence}};

use crate::{
    intel_context_types_upstream::IntelWakerefT,
    intel_runtime_pm_upstream::{
        assert_rpm_wakelock_held, intel_runtime_pm_get,
        intel_runtime_pm_get_if_in_use, intel_runtime_pm_put,
    },
    intel_engine_cs_upstream::{DelayedWork, WorkStruct},
    intel_uncore_types_upstream::IntelRuntimePm,
    intel_wakeref_types_upstream::{
        INTEL_WAKEREF_PUT_ASYNC, INTEL_WAKEREF_PUT_DELAY_MASK, IntelWakeref, IntelWakerefAuto,
        IntelWakerefLockclass, IntelWakerefOps, RefTrackerDir, intel_wakeref_is_active,
        intel_wakeref_unlock_wait,
    },
    linux::{
        memory::{
            atomic_add_unless, atomic_dec_and_test, atomic_inc, atomic_read, atomic_set, kmalloc,
            kfree, refcount_dec_and_test, refcount_inc_not_zero, refcount_set,
        },
        locks::{spin_lock_init, spin_lock_irqsave, spin_unlock_irqrestore},
        mutex::{mutex_init, mutex_lock, mutex_trylock, mutex_unlock},
        timer::{mod_timer, timer_delete_sync, timer_setup},
        wait::wait_until,
        workqueue::{INIT_DELAYED_WORK, mod_delayed_work},
    },
    linux_i915_private::DrmI915Private,
    linux_print::DrmPrinter,
};

unsafe extern "C" {
}

/// `GFP_NOWAIT` (Linux 7.2.3).
const GFP_NOWAIT: u32 = (1 << 11) | (1 << 13);
/// `PAGE_SIZE`.
const PAGE_SIZE: usize = 4096;

/// `ref_tracker_dir_snprint()` is the header inline (CONFIG_REF_TRACKER=n in the
/// oracle build) and reports zero bytes.
#[inline]
fn ref_tracker_dir_snprint(_dir: *mut RefTrackerDir, _buf: *mut u8, _size: usize) -> usize {
    0
}

#[inline]
unsafe fn wf_runtime_pm(wf: *mut IntelWakeref) -> *mut IntelRuntimePm {
    unsafe { ptr::addr_of_mut!((*(*wf).i915).runtime_pm).cast::<IntelRuntimePm>() }
}

#[inline]
unsafe fn wf_runtime_pm_of(wf: *mut IntelWakerefAuto) -> *mut IntelRuntimePm {
    unsafe { ptr::addr_of_mut!((*(*wf).i915).runtime_pm).cast::<IntelRuntimePm>() }
}

/// `__intel_wakeref_get_first()`.
// upstream: intel_wakeref.c __intel_wakeref_get_first()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_wakeref_get_first(wf: *mut IntelWakeref) -> i32 {
    let rpm = unsafe { wf_runtime_pm(wf) };
    let mut ret = 0;

    let mut wakeref: IntelWakerefT = unsafe { intel_runtime_pm_get(rpm) };

    unsafe { mutex_lock(ptr::addr_of_mut!((*wf).mutex)) };
    if unsafe { atomic_read(&(*wf).count) } == 0 {
        // INTEL_WAKEREF_BUG_ON(wf->wakeref) compiles out in this configuration.
        unsafe { (*wf).wakeref = wakeref };
        wakeref = ptr::null_mut();
        ret = unsafe { ((*(*wf).ops).get.expect("wakeref get op is unset"))(wf) };
        if ret != 0 {
            wakeref = unsafe { ptr::replace(ptr::addr_of_mut!((*wf).wakeref), ptr::null_mut()) };
            // wake_up_var(&wf->wakeref): waiters poll the same condition.
            unsafe { mutex_unlock(ptr::addr_of_mut!((*wf).mutex)) };
            if !wakeref.is_null() {
                unsafe { intel_runtime_pm_put(rpm, wakeref) };
            }
            return ret;
        }
        // smp_mb__before_atomic(): release wf->count.
        fence(Ordering::SeqCst);
    }
    unsafe { atomic_inc(&mut (*wf).count) };
    // INTEL_WAKEREF_BUG_ON(atomic_read(&wf->count) <= 0) compiles out.

    unsafe { mutex_unlock(ptr::addr_of_mut!((*wf).mutex)) };
    if !wakeref.is_null() {
        unsafe { intel_runtime_pm_put(rpm, wakeref) };
    }
    ret
}

/// `____intel_wakeref_put_last()`: caller holds `wf->mutex`.
unsafe fn ____intel_wakeref_put_last(wf: *mut IntelWakeref) {
    let mut wakeref: IntelWakerefT = ptr::null_mut();

    // INTEL_WAKEREF_BUG_ON(atomic_read(&wf->count) <= 0) compiles out.
    if unsafe { !atomic_dec_and_test(&mut (*wf).count) } {
        unsafe { mutex_unlock(ptr::addr_of_mut!((*wf).mutex)) };
        return;
    }

    // ops->put() must reschedule its own release on error/deferral.
    let put = unsafe { (*(*wf).ops).put.expect("wakeref put op is unset") };
    if unsafe { put(wf) } == 0 {
        // INTEL_WAKEREF_BUG_ON(!wf->wakeref) compiles out.
        wakeref = unsafe { ptr::replace(ptr::addr_of_mut!((*wf).wakeref), ptr::null_mut()) };
        // wake_up_var(&wf->wakeref): waiters poll the same condition.
    }

    unsafe { mutex_unlock(ptr::addr_of_mut!((*wf).mutex)) };
    if !wakeref.is_null() {
        unsafe { intel_runtime_pm_put(wf_runtime_pm(wf), wakeref) };
    }
}

/// `__intel_wakeref_put_work()`: deferred release from the unordered workqueue.
// upstream: intel_wakeref.c __intel_wakeref_put_work()
fn __intel_wakeref_put_work(work: &mut WorkStruct) {
    // container_of(wrk, typeof(*wf), work.work): `work` is the inner `work_struct`
    // of `wf->work` (a `delayed_work`).
    let wf = unsafe {
        (work as *mut WorkStruct)
            .cast::<u8>()
            .sub(core::mem::offset_of!(DelayedWork, work))
            .sub(core::mem::offset_of!(IntelWakeref, work))
            .cast::<IntelWakeref>()
    };

    if unsafe { atomic_add_unless(&mut (*wf).count, -1, 1) } {
        return;
    }

    unsafe { mutex_lock(ptr::addr_of_mut!((*wf).mutex)) };
    unsafe { ____intel_wakeref_put_last(wf) };
}

/// `__intel_wakeref_put_last()`.
// upstream: intel_wakeref.c __intel_wakeref_put_last()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_wakeref_put_last(wf: *mut IntelWakeref, flags: u64) {
    // Assume we are not in process context and so cannot sleep.
    let async_put = flags & INTEL_WAKEREF_PUT_ASYNC != 0;
    if async_put || !unsafe { mutex_trylock(ptr::addr_of_mut!((*wf).mutex)) } {
        let unordered_wq = unsafe { (*(*wf).i915).unordered_wq };
        let delay = (flags & INTEL_WAKEREF_PUT_DELAY_MASK) as u64;
        unsafe {
            mod_delayed_work(
                unordered_wq,
                ptr::addr_of_mut!((*wf).work),
                delay,
            )
        };
        return;
    }

    unsafe { ____intel_wakeref_put_last(wf) };
}

/// `__intel_wakeref_init()`.
// upstream: intel_wakeref.c __intel_wakeref_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_wakeref_init(
    wf: *mut IntelWakeref,
    i915: *mut DrmI915Private,
    ops: *const IntelWakerefOps,
    _key: *mut IntelWakerefLockclass,
    _name: *const core::ffi::c_char,
) {
    unsafe {
        (*wf).i915 = i915;
        (*wf).ops = ops;
        // Lockdep class keys and the wakeref debug tracker are compiled out.
        mutex_init(ptr::addr_of_mut!((*wf).mutex));
        atomic_set(&mut (*wf).count, 0);
        (*wf).wakeref = ptr::null_mut();
        INIT_DELAYED_WORK(&mut (*wf).work, __intel_wakeref_put_work);
    }
}

/// `intel_wakeref_wait_for_idle()`.
// upstream: intel_wakeref.c intel_wakeref_wait_for_idle()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wakeref_wait_for_idle(wf: *mut IntelWakeref) -> i32 {
    // wait_var_event_killable(&wf->wakeref, !intel_wakeref_is_active(wf)): the
    // waiter polls the condition; there are no signals in this kernel, so the
    // killable wait never returns an error.
    wait_until(
        u64::MAX,
        || !unsafe { intel_wakeref_is_active(wf.cast_const()) },
        true,
    );
    unsafe { intel_wakeref_unlock_wait(wf) };
    0
}

/// `intel_ref_tracker_show()`: the report is empty when the ref tracker is off.
// upstream: intel_wakeref.c intel_ref_tracker_show()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ref_tracker_show(dir: *mut RefTrackerDir, p: *mut DrmPrinter) {
    let buf = unsafe { kmalloc(PAGE_SIZE, GFP_NOWAIT) }.cast::<u8>();
    if buf.is_null() {
        return;
    }

    let count = ref_tracker_dir_snprint(dir, buf, PAGE_SIZE);
    if count != 0 {
        // The report is split by line through drm_printf(); with the tracker
        // compiled out, `count` is always zero and this path is unreachable.
        let _ = p;
    }

    unsafe { kfree(buf) };
}

/// `wakeref_auto_timeout()`: timer callback. `timer` is `wf->timer`.
// upstream: intel_wakeref.c wakeref_auto_timeout()
unsafe extern "C" fn wakeref_auto_timeout(timer: *mut crate::intel_engine_cs_upstream::TimerList) {
    // timer_container_of(wf, t, timer)
    let wf = unsafe {
        timer
            .cast::<u8>()
            .sub(core::mem::offset_of!(IntelWakerefAuto, timer))
            .cast::<IntelWakerefAuto>()
    };

    let mut flags = 0;
    if !unsafe { refcount_dec_and_lock_irqsave(&mut (*wf).count, &mut (*wf).lock, &mut flags) } {
        return;
    }

    let wakeref = unsafe { ptr::replace(ptr::addr_of_mut!((*wf).wakeref), ptr::null_mut()) };
    unsafe { spin_unlock_irqrestore(&mut (*wf).lock, flags) };

    unsafe { intel_runtime_pm_put(wf_runtime_pm_of(wf), wakeref) };
}

/// `refcount_dec_and_lock_irqsave()`: decrement, and when the count reaches zero
/// return with `lock` held (the C helper avoids taking the lock otherwise).
unsafe fn refcount_dec_and_lock_irqsave(
    count: *mut crate::intel_context_upstream::RefcountT,
    lock: *mut crate::intel_engine_cs_upstream::Spinlock,
    flags: &mut core::ffi::c_ulong,
) -> bool {
    unsafe { spin_lock_irqsave(&mut *lock, flags) };
    if unsafe { refcount_dec_and_test(&mut *count) } {
        return true;
    }
    unsafe { spin_unlock_irqrestore(&mut *lock, *flags) };
    false
}

/// `intel_wakeref_auto_init()`.
// upstream: intel_wakeref.c intel_wakeref_auto_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wakeref_auto_init(wf: *mut IntelWakerefAuto, i915: *mut DrmI915Private) {
    unsafe {
        spin_lock_init(&mut (*wf).lock);
        timer_setup(ptr::addr_of_mut!((*wf).timer), wakeref_auto_timeout, 0);
        refcount_set(&mut (*wf).count, 0);
        (*wf).wakeref = ptr::null_mut();
        (*wf).i915 = i915;
    }
}

/// `intel_wakeref_auto()`: extend an active wakeref by `timeout` jiffies.
// upstream: intel_wakeref.c intel_wakeref_auto()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wakeref_auto(wf: *mut IntelWakerefAuto, timeout: core::ffi::c_ulong) {
    if timeout == 0 {
        if unsafe { timer_delete_sync(ptr::addr_of_mut!((*wf).timer)) } {
            unsafe { wakeref_auto_timeout(ptr::addr_of_mut!((*wf).timer)) };
        }
        return;
    }

    // Our mission is that we only extend an already active wakeref.
    let rpm = unsafe { wf_runtime_pm_of(wf) };
    unsafe { assert_rpm_wakelock_held(rpm) };

    if !unsafe { refcount_inc_not_zero(&mut (*wf).count) } {
        let mut flags = 0;
        unsafe { spin_lock_irqsave(&mut (*wf).lock, &mut flags) };
        if !unsafe { refcount_inc_not_zero(&mut (*wf).count) } {
            // INTEL_WAKEREF_BUG_ON(wf->wakeref) compiles out.
            unsafe {
                (*wf).wakeref = intel_runtime_pm_get_if_in_use(rpm);
                refcount_set(&mut (*wf).count, 1);
            }
        }
        unsafe { spin_unlock_irqrestore(&mut (*wf).lock, flags) };
    }

    // If we extend a pending timer we get a single timer callback, so cancel the
    // local inc by running the elided callback to keep wf->count balanced.
    let expires = crate::linux::primitives::jiffies() as core::ffi::c_ulong + timeout;
    if unsafe { mod_timer(ptr::addr_of_mut!((*wf).timer), expires) } {
        unsafe { wakeref_auto_timeout(ptr::addr_of_mut!((*wf).timer)) };
    }
}

/// `intel_wakeref_auto_fini()`.
// upstream: intel_wakeref.c intel_wakeref_auto_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wakeref_auto_fini(wf: *mut IntelWakerefAuto) {
    unsafe { intel_wakeref_auto(wf, 0) };
    // INTEL_WAKEREF_BUG_ON(wf->wakeref) compiles out.
}
