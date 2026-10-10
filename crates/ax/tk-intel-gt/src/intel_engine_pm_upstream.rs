// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//! Linux 7.2.3 gt/intel_engine_pm.c in source function order. Lifecycle is
//! implemented here but remains unregistered by the default-off GT feature.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::{
    ffi::{c_int, c_void},
    mem::offset_of,
};

use crate::{
    i915_active_upstream::i915_request_add_active_barriers,
    i915_request_types_upstream::I915Request,
    i915_request_upstream::{
        __i915_request_commit, __i915_request_create, __i915_request_queue_bh,
    },
    intel_breadcrumbs_upstream::{intel_breadcrumbs_park, intel_breadcrumbs_unpark},
    intel_context_api_upstream::intel_context_is_barrier,
    intel_context_types_upstream::{CONTEXT_IS_PARKING, CONTEXT_VALID_BIT, IntelContext},
    intel_context_upstream::{DmaFence, DmaFenceCb},
    intel_engine_api_upstream::intel_engine_uses_guc,
    intel_engine_cs_upstream::intel_engine_flush_submission,
    intel_engine_heartbeat_upstream::{
        intel_engine_init_heartbeat, intel_engine_park_heartbeat, intel_engine_unpark_heartbeat,
    },
    intel_engine_types_upstream::{GSC0, IntelEngineCs},
    intel_wakeref_types_upstream::{
        __intel_wakeref_defer_park, IntelWakeref, IntelWakerefOps, intel_wakeref_track,
    },
    linux::{
        average::ewma__engine_latency_add,
        bits::{clear_bit, set_bit, test_bit},
        contexts::intel_context_inflight,
        i915::MEDIA_VER,
        list::llist_del_all,
        locks::{spin_lock, spin_unlock},
        memory::{atomic_fetch_inc, atomic_read},
        pm::{__intel_gt_pm_get, intel_gt_pm_get, intel_gt_pm_put_async, intel_wakeref_init},
        primitives::ktime_get,
    },
    linux_config::*,
};
// upstream: intel_engine_pm.c intel_gsc_idle_msg_enable()
unsafe fn intel_gsc_idle_msg_enable(engine: *mut IntelEngineCs) {
    if MEDIA_VER((*engine).i915) >= 13 && (*engine).id == GSC0 {
        crate::intel_uncore_types_upstream::intel_uncore_write(
            (*(*engine).gt).uncore,
            crate::intel_workarounds_types_upstream::I915RegT { reg: 0x11a050 },
            1 << 16,
        );
        crate::intel_uncore_types_upstream::intel_uncore_write(
            (*(*engine).gt).uncore,
            crate::intel_workarounds_types_upstream::I915RegT { reg: 0x11a054 },
            0xa,
        );
    }
}
// upstream: intel_engine_pm.c dbg_poison_ce()
unsafe fn dbg_poison_ce(ce: *mut IntelContext) {
    if !CONFIG_DRM_I915_DEBUG_GEM {
        return;
    }
    if !(*ce).state.is_null() {
        let obj = (*(*ce).state).obj;
        let map_type =
            crate::intel_gt_api_upstream::intel_gt_coherent_map_type((*(*ce).engine).gt, obj, true);
        if !crate::i915_gem_object_header_upstream::i915_gem_object_trylock(
            obj,
            core::ptr::null_mut(),
        ) {
            return;
        }
        let map = crate::i915_gem_pages_upstream::i915_gem_object_pin_map(obj, map_type);
        if (map as usize) < usize::MAX - 4094 {
            core::ptr::write_bytes(map.cast::<u8>(), 0x5a, (&(*obj).base.base).size as usize);
            crate::i915_gem_object_header_upstream::i915_gem_object_flush_map(obj);
            crate::i915_gem_object_header_upstream::i915_gem_object_unpin_map(obj);
        }
        crate::i915_gem_object_header_upstream::i915_gem_object_unlock(obj);
    }
}
// upstream: intel_engine_pm.c __engine_unpark()
unsafe extern "C" fn __engine_unpark(wf: *mut IntelWakeref) -> c_int {
    let engine = wf
        .cast::<u8>()
        .sub(offset_of!(IntelEngineCs, wakeref))
        .cast::<IntelEngineCs>();
    (*engine).wakeref_track = intel_gt_pm_get((*engine).gt);
    let ce = (*engine).kernel_context;
    if !ce.is_null() {
        GEM_BUG_ON!(test_bit(CONTEXT_VALID_BIT, &(*ce).flags));
        while !intel_context_inflight(ce).is_null() {
            intel_engine_flush_submission(engine);
        }
        dbg_poison_ce(ce);
        ((*(*ce).ops).reset.unwrap())(ce);
        let tl = (*ce).timeline;
        GEM_BUG_ON!((*tl).seqno != core::ptr::read_volatile((*tl).hwsp_seqno));
    }
    if let Some(unpark) = (*engine).unpark {
        unpark(engine);
    }
    intel_breadcrumbs_unpark((*engine).breadcrumbs);
    intel_engine_unpark_heartbeat(engine);
    0
}
// upstream: intel_engine_pm.c duration()
unsafe extern "C" fn duration(fence: *mut DmaFence, _cb: *mut DmaFenceCb) {
    let rq = fence.cast::<I915Request>();
    ewma__engine_latency_add(
        &mut (*(*rq).engine).latency,
        (((*fence).timestamp_union.timestamp - (&(*rq).submit_union.duration).emitted) / 1000) as u64,
    );
}
// upstream: intel_engine_pm.c __queue_and_release_pm()
unsafe fn __queue_and_release_pm(
    rq: *mut I915Request,
    tl: *mut crate::intel_timeline_types_upstream::IntelTimeline,
    engine: *mut IntelEngineCs,
) {
    let timelines = &mut (*(*engine).gt).timelines;
    GEM_BUG_ON!((*(*rq).context).active_count != 1);
    __intel_gt_pm_get((*engine).gt);
    (*(*rq).context).wakeref = intel_wakeref_track(&mut (*(*engine).gt).wakeref);
    spin_lock(&mut timelines.lock);
    if atomic_fetch_inc(&mut (*tl).active_count) == 0 {
        crate::linux_list::list_add_tail(&mut (*tl).link, &mut timelines.active_list);
    }
    __i915_request_queue_bh(rq);
    __intel_wakeref_defer_park(&mut (*engine).wakeref);
    spin_unlock(&mut timelines.lock);
}
// upstream: intel_engine_pm.c switch_to_kernel_context()
unsafe fn switch_to_kernel_context(engine: *mut IntelEngineCs) -> bool {
    let ce = (*engine).kernel_context;
    let mut result = true;
    if intel_engine_uses_guc(engine) {
        return true;
    }
    if crate::intel_gt_api_upstream::intel_gt_is_wedged((*engine).gt) {
        return true;
    }
    GEM_BUG_ON!(!intel_context_is_barrier(ce));
    GEM_BUG_ON!((*(*ce).timeline).hwsp_ggtt != (*engine).status_page.vma);
    if (*engine).wakeref_serial == (*engine).serial {
        return true;
    }
    set_bit(CONTEXT_IS_PARKING, &mut (*ce).flags);
    GEM_BUG_ON!(atomic_read(&(*(*ce).timeline).active_count) < 0);
    let rq = __i915_request_create(ce, GFP_NOWAIT);
    if (rq as usize) < usize::MAX - 4094 {
        (*engine).wakeref_serial = (*engine).serial + 1;
        i915_request_add_active_barriers(rq);
        (*rq).sched.attr.priority = i32::MAX - 1;
        if __i915_request_commit(rq).is_null() {
            crate::i915_gem_clflush_upstream::dma_fence_add_callback(
                &mut (*rq).fence,
                &mut (*(*rq).submit_union.duration).cb,
                duration,
            );
            (*(*rq).submit_union.duration).emitted = ktime_get();
        }
        __queue_and_release_pm(rq, (*ce).timeline, engine);
        result = false;
    }
    clear_bit(CONTEXT_IS_PARKING, &mut (*ce).flags);
    result
}
// upstream: intel_engine_pm.c call_idle_barriers()
unsafe fn call_idle_barriers(engine: *mut IntelEngineCs) {
    let mut node = llist_del_all(&mut (*engine).barrier_tasks);
    while !node.is_null() {
        let next = (*node).next;
        let cb = node
            .cast::<u8>()
            .sub(offset_of!(DmaFenceCb, node))
            .cast::<DmaFenceCb>();
        ((*cb).func.unwrap())((-EAGAIN as isize) as *mut DmaFence, cb);
        node = next;
    }
}
// upstream: intel_engine_pm.c __engine_park()
unsafe extern "C" fn __engine_park(wf: *mut IntelWakeref) -> c_int {
    let engine = wf
        .cast::<u8>()
        .sub(offset_of!(IntelEngineCs, wakeref))
        .cast::<IntelEngineCs>();
    (*engine).saturated = 0;
    if !switch_to_kernel_context(engine) {
        return -EBUSY;
    }
    call_idle_barriers(engine);
    intel_engine_park_heartbeat(engine);
    intel_breadcrumbs_park((*engine).breadcrumbs);
    if let Some(park) = (*engine).park {
        park(engine);
    }
    intel_gt_pm_put_async((*engine).gt, (*engine).wakeref_track);
    0
}
static WF_OPS: IntelWakerefOps = IntelWakerefOps {
    get: Some(__engine_unpark),
    put: Some(__engine_park),
};
// upstream: intel_engine_pm.c intel_engine_init__pm()
pub unsafe fn intel_engine_init__pm(engine: *mut IntelEngineCs) {
    intel_wakeref_init(&mut (*engine).wakeref, (*engine).i915, &WF_OPS);
    intel_engine_init_heartbeat(engine);
    intel_gsc_idle_msg_enable(engine);
}
// upstream: intel_engine_pm.c intel_engine_reset_pinned_contexts()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_reset_pinned_contexts(engine: *mut IntelEngineCs) {
    let head =
        &mut (*engine).pinned_contexts_list as *mut crate::intel_engine_cs_upstream::ListHead;
    let mut node = (*head).next;
    while node != head {
        let ce = node
            .cast::<u8>()
            .sub(offset_of!(IntelContext, pinned_contexts_link))
            .cast::<IntelContext>();
        node = (*node).next;
        if ce == (*engine).kernel_context {
            continue;
        }
        dbg_poison_ce(ce);
        ((*(*ce).ops).reset.unwrap())(ce);
    }
}
