// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Linux v7.2.3 source translation of the engine retirement-work lifecycle
//! from `drivers/gpu/drm/i915/gt/intel_gt_requests.c`.

#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::c_long,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    i915_active_upstream::i915_active_fence_get,
    i915_request_types_upstream::I915Request,
    i915_request_upstream::i915_request_retire,
    intel_engine_api_upstream::intel_engine_flush_submission,
    intel_engine_cs_upstream::{IntelEngineCs, WorkStruct},
    intel_gt_types_upstream::IntelGt,
    intel_timeline_types_upstream::IntelTimeline,
    linux::{
        i915::intel_timeline_put,
        list::{INIT_LIST_HEAD, list_add_tail, list_del, list_empty, llist_del_all},
        locks::{spin_lock_raw, spin_unlock_raw},
        memory::{atomic_dec_and_test, atomic_inc, atomic_read, refcount_dec_and_test},
        mutex::{mutex_trylock, mutex_unlock},
        pm::intel_gt_pm_is_awake,
        rcu::{rcu_read_lock, rcu_read_unlock},
        requests::{
            dma_fence_put, dma_fence_wait_timeout, i915_request_completed, i915_request_put,
        },
        workqueue::{
            INIT_DELAYED_WORK, INIT_WORK_C, cancel_delayed_work, cancel_delayed_work_sync,
            flush_delayed_work, flush_work, queue_delayed_work, queue_work,
        },
    },
};

// upstream: intel_gt_requests.c retire_requests()
unsafe fn retire_requests(tl: *mut IntelTimeline) -> bool {
    let mut rq: *mut I915Request = core::ptr::null_mut();
    let mut next: *mut I915Request = core::ptr::null_mut();

    list_for_each_entry_safe!(rq, next, core::ptr::addr_of_mut!((*tl).requests), link, {
        if !i915_request_retire(rq) {
            return false;
        }
    });

    (*tl).last_request.fence.is_null()
}

// upstream: intel_gt_requests.c engine_active()
unsafe fn engine_active(engine: *const IntelEngineCs) -> bool {
    let context = unsafe { (*engine).kernel_context };
    let timeline = unsafe { (*context).timeline };
    !unsafe { list_empty(&(*timeline).requests) }
}

// upstream: intel_gt_requests.c flush_submission()
unsafe fn flush_submission(gt: *mut crate::intel_gt_types_upstream::IntelGt, timeout: i64) -> bool {
    if timeout == 0 || !unsafe { intel_gt_pm_is_awake(gt) } {
        return false;
    }

    let mut active = false;
    for_each_engine!(engine, id, gt, {
        unsafe {
            intel_engine_flush_submission(engine);
            flush_work(&mut engine.retire_work);
            flush_delayed_work(&mut engine.wakeref.work);
            active |= engine_active(engine);
        }
    });
    active
}

// upstream: intel_gt_requests.c engine_retire()
unsafe extern "C" fn engine_retire(work: *mut WorkStruct) {
    let engine = container_of!(work, IntelEngineCs, retire_work);
    let mut timeline = unsafe { core::ptr::replace(&mut (*engine).retire, core::ptr::null_mut()) };

    while !timeline.is_null() {
        let next = unsafe { core::ptr::replace(&mut (*timeline).retire, core::ptr::null_mut()) };

        if unsafe { mutex_trylock(&mut (*timeline).mutex) } {
            let _ = unsafe { retire_requests(timeline) };
            unsafe { mutex_unlock(&mut (*timeline).mutex) };
        }
        unsafe { intel_timeline_put(timeline) };

        GEM_BUG_ON!(next.is_null());
        timeline = ((next as usize) & !1usize) as *mut IntelTimeline;
    }
}

// upstream: intel_gt_requests.c add_retire()
unsafe fn add_retire(engine: *mut IntelEngineCs, timeline: *mut IntelTimeline) -> bool {
    const STUB: *mut IntelTimeline = 1usize as *mut IntelTimeline;

    let timeline_retire =
        unsafe { &*core::ptr::addr_of!((*timeline).retire).cast::<AtomicPtr<IntelTimeline>>() };
    if timeline_retire
        .compare_exchange(
            core::ptr::null_mut(),
            STUB,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return false;
    }

    unsafe { crate::intel_timeline_upstream::intel_timeline_get(timeline) };
    let engine_retire =
        unsafe { &*core::ptr::addr_of!((*engine).retire).cast::<AtomicPtr<IntelTimeline>>() };
    let mut first = engine_retire.load(Ordering::Acquire);
    loop {
        unsafe { (*timeline).retire = ((first as usize) | 1) as *mut IntelTimeline };
        match engine_retire.compare_exchange_weak(
            first,
            timeline,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => first = actual,
        }
    }

    first.is_null()
}

// upstream: intel_gt_requests.c intel_engine_add_retire()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_add_retire(
    engine: *mut IntelEngineCs,
    timeline: *mut IntelTimeline,
) {
    GEM_BUG_ON!(unsafe { crate::intel_engine_types_upstream::intel_engine_is_virtual(engine) });
    if unsafe { add_retire(engine, timeline) } {
        let wq = unsafe { (*(*engine).i915).unordered_wq };
        unsafe { queue_work(wq, core::ptr::addr_of_mut!((*engine).retire_work)) };
    }
}

// upstream: intel_gt_requests.c intel_engine_init_retire()
pub unsafe fn intel_engine_init_retire(engine: *mut IntelEngineCs) {
    unsafe { INIT_WORK_C(&mut (*engine).retire_work, engine_retire) };
}

// upstream: intel_gt_requests.c intel_engine_fini_retire()
pub unsafe fn intel_engine_fini_retire(engine: *mut IntelEngineCs) {
    unsafe { flush_work(&mut (*engine).retire_work) };
    GEM_BUG_ON!(unsafe { !(*engine).retire.is_null() });
}

// upstream: intel_gt_requests.h intel_gt_retire_requests()
unsafe fn intel_gt_retire_requests(gt: *mut IntelGt) {
    let _ = unsafe { intel_gt_retire_requests_timeout(gt, 0, core::ptr::null_mut()) };
}

// upstream: intel_gt_requests.c intel_gt_retire_requests_timeout()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_retire_requests_timeout(
    gt: *mut IntelGt,
    mut timeout: c_long,
    remaining_timeout: *mut c_long,
) -> c_long {
    let timelines = unsafe { core::ptr::addr_of_mut!((*gt).timelines) };
    let active_list = unsafe { core::ptr::addr_of_mut!((*timelines).active_list) };
    let lock = unsafe { core::ptr::addr_of_mut!((*timelines).lock) };
    let mut active_count = 0usize;
    let mut free = unsafe { core::mem::zeroed::<crate::intel_engine_cs_upstream::ListHead>() };
    unsafe { INIT_LIST_HEAD(&mut free) };

    // Kick pending tasklets/submission and synchronously drain the idle work.
    let _ = unsafe { flush_submission(gt, timeout as i64) };
    unsafe { spin_lock_raw(lock) };
    let mut link = unsafe { (*active_list).next };
    while link != active_list {
        let timeline = container_of!(link, IntelTimeline, link);
        let mut next = unsafe { (*link).next };

        if !unsafe { mutex_trylock(core::ptr::addr_of_mut!((*timeline).mutex)) } {
            active_count += 1;
        } else {
            unsafe {
                crate::intel_timeline_upstream::intel_timeline_get(timeline);
                GEM_BUG_ON!(atomic_read(&(*timeline).active_count) == 0);
                atomic_inc(&mut (*timeline).active_count);
                spin_unlock_raw(lock);
            }

            let mut mutex_held = true;
            if timeout > 0 {
                let fence = unsafe {
                    i915_active_fence_get(core::ptr::addr_of_mut!((*timeline).last_request))
                };
                if !fence.is_null() {
                    unsafe { mutex_unlock(core::ptr::addr_of_mut!((*timeline).mutex)) };
                    mutex_held = false;
                    timeout = unsafe { dma_fence_wait_timeout(fence, true, timeout) };
                    unsafe { dma_fence_put(fence) };
                    if unsafe { mutex_trylock(core::ptr::addr_of_mut!((*timeline).mutex)) } {
                        mutex_held = true;
                    } else {
                        active_count += 1;
                    }
                }
            }

            if mutex_held {
                if !unsafe { retire_requests(timeline) } {
                    active_count += 1;
                }
                unsafe { mutex_unlock(core::ptr::addr_of_mut!((*timeline).mutex)) };
            }

            unsafe { spin_lock_raw(lock) };
            // The lock was dropped while waiting. Re-read the successor under
            // the list lock before unlinking this pinned element.
            next = unsafe { (*link).next };
            if unsafe { atomic_dec_and_test(&mut (*timeline).active_count) } {
                unsafe { list_del(link) };
            }
            if unsafe { refcount_dec_and_test(&mut (*timeline).kref.refcount) } {
                GEM_BUG_ON!(atomic_read(&(*timeline).active_count) != 0);
                unsafe { list_add_tail(link, &mut free) };
            }
        }

        link = next;
    }
    unsafe { spin_unlock_raw(lock) };

    let mut link = unsafe { free.next };
    while link != &mut free {
        let next = unsafe { (*link).next };
        let timeline = container_of!(link, IntelTimeline, link);
        unsafe { crate::intel_timeline_upstream::__intel_timeline_free(&mut (*timeline).kref) };
        link = next;
    }

    if unsafe { flush_submission(gt, timeout as i64) } {
        active_count += 1;
    }
    if !remaining_timeout.is_null() {
        unsafe { *remaining_timeout = timeout };
    }
    if active_count != 0 {
        if timeout != 0 {
            timeout
        } else {
            -(crate::linux_config::ETIME as c_long)
        }
    } else {
        0
    }
}

// upstream: intel_gt_requests.c retire_work_handler()
unsafe fn retire_work_handler(work: *mut WorkStruct) {
    let gt = container_of!(work, IntelGt, requests.retire_work.work);
    let i915 = unsafe { (*gt).i915 };
    unsafe {
        queue_delayed_work(
            (*i915).unordered_wq,
            core::ptr::addr_of_mut!((*gt).requests.retire_work),
            crate::linux::primitives::round_jiffies_up_relative(crate::linux_config::HZ as u64),
        );
        intel_gt_retire_requests(gt);
    }
}

// upstream: intel_gt_requests.c intel_gt_init_requests()
pub unsafe fn intel_gt_init_requests(gt: *mut IntelGt) {
    unsafe {
        INIT_DELAYED_WORK(&mut (*gt).requests.retire_work, |work| {
            retire_work_handler(work as *mut WorkStruct)
        });
    }
}

// upstream: intel_gt_requests.c intel_gt_park_requests()
pub unsafe fn intel_gt_park_requests(gt: *mut IntelGt) {
    unsafe { cancel_delayed_work(core::ptr::addr_of_mut!((*gt).requests.retire_work)) };
}

// upstream: intel_gt_requests.c intel_gt_unpark_requests()
pub unsafe fn intel_gt_unpark_requests(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    unsafe {
        queue_delayed_work(
            (*i915).unordered_wq,
            core::ptr::addr_of_mut!((*gt).requests.retire_work),
            crate::linux::primitives::round_jiffies_up_relative(crate::linux_config::HZ as u64),
        );
    }
}

// upstream: intel_gt_requests.c intel_gt_fini_requests()
pub unsafe fn intel_gt_fini_requests(gt: *mut IntelGt) {
    unsafe {
        cancel_delayed_work_sync(core::ptr::addr_of_mut!((*gt).requests.retire_work));
        flush_work(core::ptr::addr_of_mut!((*gt).watchdog.work));
    }
}

// upstream: intel_gt_requests.c intel_gt_watchdog_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_watchdog_work(work: *mut WorkStruct) {
    let gt = container_of!(work, IntelGt, watchdog.work);
    let mut node = unsafe { llist_del_all(core::ptr::addr_of_mut!((*gt).watchdog.list)) };
    if node.is_null() {
        return;
    }

    while !node.is_null() {
        let next = unsafe { (*node).next };
        let request = container_of!(node, I915Request, watchdog.link);
        if !i915_request_completed(request) {
            let fence = unsafe { core::ptr::addr_of_mut!((*request).fence) };
            unsafe { rcu_read_lock() };
            let driver = unsafe { crate::i915_gem_clflush_upstream::dma_fence_driver_name(fence) };
            let timeline =
                unsafe { crate::i915_gem_clflush_upstream::dma_fence_timeline_name(fence) };
            pr_info!(
                "Fence expiration time out i915-%s:%s:%llx!\n",
                driver,
                timeline,
                unsafe { (*fence).seqno }
            );
            unsafe { rcu_read_unlock() };
            unsafe {
                crate::i915_request_upstream::i915_request_cancel(
                    request,
                    -crate::linux_config::EINTR,
                )
            };
        }
        i915_request_put(request);
        node = next;
    }
}
