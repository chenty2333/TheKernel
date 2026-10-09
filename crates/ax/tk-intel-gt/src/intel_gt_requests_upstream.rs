// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Linux v7.2.3 source translation of the engine retirement-work lifecycle
//! from `drivers/gpu/drm/i915/gt/intel_gt_requests.c`.

#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]

use crate::{
    intel_engine_cs_upstream::{IntelEngineCs, WorkStruct},
    i915_request_types_upstream::I915Request,
    intel_timeline_types_upstream::IntelTimeline,
    i915_request_upstream::i915_request_retire,
    linux::{
        i915::intel_timeline_put,
        list::list_empty,
        mutex::{mutex_trylock, mutex_unlock},
        workqueue::{flush_work, queue_work, INIT_WORK_C},
    },
};
use core::sync::atomic::{AtomicPtr, Ordering};

// upstream: intel_gt_requests.c intel_engine_add_retire()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_add_retire(
    engine: *mut IntelEngineCs,
    timeline: *mut IntelTimeline,
) {
    const STUB: *mut IntelTimeline = 1usize as *mut IntelTimeline;
    GEM_BUG_ON!(unsafe { crate::intel_engine_types_upstream::intel_engine_is_virtual(engine) });

    let timeline_retire = unsafe { &*core::ptr::addr_of!((*timeline).retire).cast::<AtomicPtr<IntelTimeline>>() };
    if timeline_retire.compare_exchange(
        core::ptr::null_mut(), STUB, Ordering::AcqRel, Ordering::Acquire,
    ).is_err() {
        return;
    }

    unsafe { crate::intel_timeline_upstream::intel_timeline_get(timeline) };
    let engine_retire = unsafe { &*core::ptr::addr_of!((*engine).retire).cast::<AtomicPtr<IntelTimeline>>() };
    let mut first = engine_retire.load(Ordering::Acquire);
    loop {
        unsafe { (*timeline).retire = ((first as usize) | 1) as *mut IntelTimeline };
        match engine_retire.compare_exchange_weak(first, timeline, Ordering::Release, Ordering::Acquire) {
            Ok(_) => break,
            Err(actual) => first = actual,
        }
    }

    if first.is_null() {
        let wq = unsafe { (*(*engine).i915).unordered_wq };
        unsafe { queue_work(wq, core::ptr::addr_of_mut!((*engine).retire_work)) };
    }
}

// upstream: intel_gt_requests.c retire_requests()
unsafe fn retire_requests(tl: *mut IntelTimeline) -> bool {
    let mut rq: *mut I915Request = core::ptr::null_mut();
    let mut next: *mut I915Request = core::ptr::null_mut();

    list_for_each_entry_safe!(
        rq,
        next,
        core::ptr::addr_of_mut!((*tl).requests),
        link,
        {
            if !i915_request_retire(rq) {
                return false;
            }
        }
    );

    (*tl).last_request.fence.is_null()
}

// upstream: intel_gt_requests.c engine_active()
unsafe fn engine_active(engine: *const IntelEngineCs) -> bool {
    let context = unsafe { (*engine).kernel_context };
    let timeline = unsafe { (*context).timeline };
    !unsafe { list_empty(&(*timeline).requests) }
}

// upstream: intel_gt_requests.c engine_retire()
unsafe extern "C" fn engine_retire(work: *mut WorkStruct) {
    let engine = container_of!(work, IntelEngineCs, retire_work);
    let mut timeline = unsafe { core::ptr::replace(&mut (*engine).retire, core::ptr::null_mut()) };

    while !timeline.is_null() {
        let next = unsafe {
            core::ptr::replace(&mut (*timeline).retire, core::ptr::null_mut())
        };

        if unsafe { mutex_trylock(&mut (*timeline).mutex) } {
            let _ = unsafe { retire_requests(timeline) };
            unsafe { mutex_unlock(&mut (*timeline).mutex) };
        }
        unsafe { intel_timeline_put(timeline) };

        GEM_BUG_ON!(next.is_null());
        timeline = ((next as usize) & !1usize) as *mut IntelTimeline;
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
