// SPDX-License-Identifier: MIT
// Copyright © 2016-2018 Intel Corporation.
//
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_timeline.c. This module is registered; its
// upstream GEM/RCU/locking operations remain binding points, not substitute
// implementations. Keep the source order and lifetime/locking edges intact.

use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::size_of,
};

use crate::{
    intel_context_upstream::*, intel_engine_cs_upstream::*, intel_ring::PAGE_SIZE,
    intel_timeline_types_upstream::IntelTimeline, linux_config::*, linux_list::*,
};

// Bindings supplied by the later integration layer: IntelTimeline/IntelGt and
// i915/GEM types, constants, Linux helper functions, and trace/BUG macros.

const TIMELINE_SEQNO_BYTES: usize = 8;

// upstream: intel_timeline.c hwsp_alloc()
unsafe fn hwsp_alloc(gt: *mut IntelGt) -> *mut I915Vma {
    let i915 = (*gt).i915;
    let obj: *mut DrmI915GemObject;
    let vma: *mut I915Vma;

    obj = i915_gem_object_create_internal(i915, PAGE_SIZE);
    if IS_ERR(obj) {
        return ERR_CAST(obj);
    }

    i915_gem_object_set_cache_coherency(obj, I915_CACHE_LLC);

    vma = i915_vma_instance(obj, &mut (*(*gt).ggtt).vm, core::ptr::null_mut());
    if IS_ERR(vma) {
        i915_gem_object_put(obj);
    }

    vma
}

// upstream: intel_timeline.c __timeline_retire()
unsafe fn __timeline_retire(active: *mut I915Active) {
    let tl = container_of!(active, IntelTimeline, active);

    i915_vma_unpin((*tl).hwsp_ggtt);
    intel_timeline_put(tl);
}

// upstream: intel_timeline.c __timeline_active()
unsafe fn __timeline_active(active: *mut I915Active) -> i32 {
    let tl = container_of!(active, IntelTimeline, active);

    __i915_vma_pin((*tl).hwsp_ggtt);
    intel_timeline_get(tl);
    0
}

// upstream: intel_timeline.c intel_timeline_pin_map()
pub unsafe fn intel_timeline_pin_map(timeline: *mut IntelTimeline) -> i32 {
    let obj = (*(*timeline).hwsp_ggtt).obj;
    let ofs = offset_in_page((*timeline).hwsp_offset) as usize;
    let mut vaddr: *mut u8;

    vaddr = i915_gem_object_pin_map(obj, I915_MAP_WB).cast::<u8>();
    if IS_ERR(vaddr) {
        return PTR_ERR(vaddr);
    }

    (*timeline).hwsp_map = vaddr.cast::<c_void>();
    (*timeline).hwsp_seqno = memset(vaddr.add(ofs).cast::<c_void>(), 0, TIMELINE_SEQNO_BYTES)
        .cast::<u32>() as *const u32;
    drm_clflush_virt_range(vaddr.add(ofs).cast::<c_void>(), TIMELINE_SEQNO_BYTES);

    0
}

// upstream: intel_timeline.c intel_timeline_init()
unsafe fn intel_timeline_init(
    timeline: *mut IntelTimeline,
    gt: *mut IntelGt,
    mut hwsp: *mut I915Vma,
    offset: u32,
) -> i32 {
    kref_init(&mut (*timeline).kref);
    atomic_set(&mut (*timeline).pin_count, 0);

    (*timeline).gt = gt;

    if !hwsp.is_null() {
        (*timeline).hwsp_offset = offset;
        (*timeline).hwsp_ggtt = i915_vma_get(hwsp);
    } else {
        (*timeline).has_initial_breadcrumb = true;
        hwsp = hwsp_alloc(gt);
        if IS_ERR(hwsp) {
            return PTR_ERR(hwsp);
        }
        (*timeline).hwsp_ggtt = hwsp;
    }

    (*timeline).hwsp_map = core::ptr::null_mut();
    (*timeline).hwsp_seqno = (*timeline).hwsp_offset as isize as *const u32;

    GEM_BUG_ON!((*timeline).hwsp_offset as u64 >= (*hwsp).size);

    (*timeline).fence_context = dma_fence_context_alloc(1);

    mutex_init(&mut (*timeline).mutex);

    INIT_ACTIVE_FENCE!(&mut (*timeline).last_request);
    INIT_LIST_HEAD(&mut (*timeline).requests);

    i915_syncmap_init(&mut (*timeline).sync);
    i915_active_init(
        &mut (*timeline).active,
        __timeline_active,
        __timeline_retire,
        0,
    );

    0
}

// upstream: intel_timeline.c intel_gt_init_timelines()
pub unsafe fn intel_gt_init_timelines(gt: *mut IntelGt) {
    let timelines = &mut (*gt).timelines;

    spin_lock_init(&mut timelines.lock);
    INIT_LIST_HEAD(&mut timelines.active_list);
}

// upstream: intel_timeline.c intel_timeline_fini()
unsafe extern "C" fn intel_timeline_fini(rcu: *mut RcuHead) {
    let timeline = container_of!(rcu, IntelTimeline, rcu);

    if !(*timeline).hwsp_map.is_null() {
        i915_gem_object_unpin_map((*(*timeline).hwsp_ggtt).obj);
    }

    i915_vma_put((*timeline).hwsp_ggtt);
    i915_active_fini(&mut (*timeline).active);

    // A small race exists between intel_gt_retire_requests_timeout and
    // intel_timeline_exit which could result in the syncmap not getting
    // free'd. Rather than work to hard to seal this race, simply cleanup the
    // syncmap on fini.
    i915_syncmap_free(&mut (*timeline).sync);

    kfree(timeline);
}

// upstream: intel_timeline.c __intel_timeline_create()
pub unsafe fn __intel_timeline_create(
    gt: *mut IntelGt,
    global_hwsp: *mut I915Vma,
    offset: u32,
) -> *mut IntelTimeline {
    let timeline = kzalloc_obj::<IntelTimeline>();
    let err: i32;

    if timeline.is_null() {
        return ERR_PTR(-ENOMEM);
    }

    err = intel_timeline_init(timeline, gt, global_hwsp, offset);
    if err != 0 {
        kfree(timeline);
        return ERR_PTR(err);
    }

    timeline
}

// upstream: intel_timeline.c intel_timeline_create_from_engine()
pub unsafe fn intel_timeline_create_from_engine(
    engine: *mut IntelEngineCs,
    offset: u32,
) -> *mut IntelTimeline {
    let hwsp = (*engine).status_page.vma;
    let tl: *mut IntelTimeline;

    tl = __intel_timeline_create((*engine).gt, hwsp, offset);
    if IS_ERR(tl) {
        return tl;
    }

    // Borrow a nearby lock; we only create these timelines during init
    mutex_lock(&mut (*(*hwsp).vm).mutex);
    list_add_tail(&mut (*tl).engine_link, &mut (*engine).status_page.timelines);
    mutex_unlock(&mut (*(*hwsp).vm).mutex);

    tl
}

// upstream: intel_timeline.c __intel_timeline_pin()
pub unsafe fn __intel_timeline_pin(tl: *mut IntelTimeline) {
    GEM_BUG_ON!(atomic_read(&(*tl).pin_count) == 0);
    atomic_inc(&mut (*tl).pin_count);
}

// upstream: intel_timeline.c intel_timeline_pin()
pub unsafe fn intel_timeline_pin(tl: *mut IntelTimeline, ww: *mut I915GemWwCtx) -> i32 {
    let mut err: i32;

    if atomic_add_unless(&mut (*tl).pin_count, 1, 0) {
        return 0;
    }

    if (*tl).hwsp_map.is_null() {
        err = intel_timeline_pin_map(tl);
        if err != 0 {
            return err;
        }
    }

    err = i915_ggtt_pin((*tl).hwsp_ggtt, ww, 0, PIN_HIGH);
    if err != 0 {
        return err;
    }

    (*tl).hwsp_offset =
        i915_ggtt_offset((*tl).hwsp_ggtt).wrapping_add(offset_in_page((*tl).hwsp_offset));
    GT_TRACE!(
        (*tl).gt,
        "timeline:%llx using HWSP offset:%x\n",
        (*tl).fence_context,
        (*tl).hwsp_offset
    );

    i915_active_acquire(&mut (*tl).active);
    if atomic_fetch_inc(&mut (*tl).pin_count) != 0 {
        i915_active_release(&mut (*tl).active);
        __i915_vma_unpin((*tl).hwsp_ggtt);
    }

    0
}

// upstream: intel_timeline.c intel_timeline_reset_seqno()
pub unsafe fn intel_timeline_reset_seqno(tl: *const IntelTimeline) {
    let hwsp_seqno = (*tl).hwsp_seqno as *mut u32;
    // Must be pinned to be writable, and no requests in flight.
    GEM_BUG_ON!(atomic_read(&(*tl).pin_count) == 0);

    memset(
        hwsp_seqno.add(1).cast::<c_void>(),
        0,
        TIMELINE_SEQNO_BYTES - size_of::<u32>(),
    );
    WRITE_ONCE!(*hwsp_seqno, (*tl).seqno);
    drm_clflush_virt_range(hwsp_seqno.cast::<c_void>(), TIMELINE_SEQNO_BYTES);
}

// upstream: intel_timeline.c intel_timeline_enter()
pub unsafe fn intel_timeline_enter(tl: *mut IntelTimeline) {
    let timelines = &mut (*(*tl).gt).timelines;

    // Pretend we are serialised by the timeline->mutex.
    //
    // While generally true, there are a few exceptions to the rule
    // for the engine->kernel_context being used to manage power
    // transitions. As the engine_park may be called from under any
    // timeline, it uses the power mutex as a global serialisation
    // lock to prevent any other request entering its timeline.
    //
    // The rule is generally tl->mutex, otherwise engine->wakeref.mutex.
    //
    // However, intel_gt_retire_request() does not know which engine
    // it is retiring along and so cannot partake in the engine-pm
    // barrier, and there we use the tl->active_count as a means to
    // pin the timeline in the active_list while the locks are dropped.
    // Ergo, as that is outside of the engine-pm barrier, we need to
    // use atomic to manipulate tl->active_count.
    lockdep_assert_held(&(*tl).mutex);

    if atomic_add_unless(&mut (*tl).active_count, 1, 0) {
        return;
    }

    spin_lock(&mut timelines.lock);
    if atomic_fetch_inc(&mut (*tl).active_count) == 0 {
        // The HWSP is volatile, and may have been lost while inactive,
        // e.g. across suspend/resume. Be paranoid, and ensure that
        // the HWSP value matches our seqno so we don't proclaim
        // the next request as already complete.
        intel_timeline_reset_seqno(tl);
        list_add_tail(&mut (*tl).link, &mut timelines.active_list);
    }
    spin_unlock(&mut timelines.lock);
}

// upstream: intel_timeline.c intel_timeline_exit()
pub unsafe fn intel_timeline_exit(tl: *mut IntelTimeline) {
    let timelines = &mut (*(*tl).gt).timelines;

    // See intel_timeline_enter()
    lockdep_assert_held(&(*tl).mutex);

    GEM_BUG_ON!(atomic_read(&(*tl).active_count) == 0);
    if atomic_add_unless(&mut (*tl).active_count, -1, 1) {
        return;
    }

    spin_lock(&mut timelines.lock);
    if atomic_dec_and_test(&mut (*tl).active_count) {
        list_del(&mut (*tl).link);
    }
    spin_unlock(&mut timelines.lock);

    // Since this timeline is idle, all bariers upon which we were waiting
    // must also be complete and so we can discard the last used barriers
    // without loss of information.
    i915_syncmap_free(&mut (*tl).sync);
}

// upstream: intel_timeline.c timeline_advance()
unsafe fn timeline_advance(tl: *mut IntelTimeline) -> u32 {
    GEM_BUG_ON!(atomic_read(&(*tl).pin_count) == 0);
    GEM_BUG_ON!((*tl).seqno & ((*tl).has_initial_breadcrumb as u32) != 0);

    (*tl).seqno = (*tl)
        .seqno
        .wrapping_add(1 + ((*tl).has_initial_breadcrumb as u32));
    (*tl).seqno
}

// upstream: intel_timeline.c __intel_timeline_get_seqno()
#[inline(never)]
unsafe fn __intel_timeline_get_seqno(tl: *mut IntelTimeline, seqno: *mut u32) -> i32 {
    let mut next_ofs: u32 =
        offset_in_page((*tl).hwsp_offset.wrapping_add(TIMELINE_SEQNO_BYTES as u32));

    // w/a: bit 5 needs to be zero for MI_FLUSH_DW address.
    if TIMELINE_SEQNO_BYTES as u32 <= BIT!(5) && (next_ofs & BIT!(5)) != 0 {
        next_ofs = offset_in_page(next_ofs + BIT!(5));
    }

    (*tl).hwsp_offset = i915_ggtt_offset((*tl).hwsp_ggtt).wrapping_add(next_ofs);
    (*tl).hwsp_seqno = (*tl)
        .hwsp_map
        .cast::<u8>()
        .add(next_ofs as usize)
        .cast::<u32>() as *const u32;
    intel_timeline_reset_seqno(tl);

    *seqno = timeline_advance(tl);
    GEM_BUG_ON!(i915_seqno_passed(*(*tl).hwsp_seqno, *seqno));
    0
}

// upstream: intel_timeline.c intel_timeline_get_seqno()
pub unsafe fn intel_timeline_get_seqno(
    tl: *mut IntelTimeline,
    rq: *mut I915Request,
    seqno: *mut u32,
) -> i32 {
    *seqno = timeline_advance(tl);

    // Replace the HWSP on wraparound for HW semaphores
    if unlikely(*seqno == 0 && (*tl).has_initial_breadcrumb) {
        return __intel_timeline_get_seqno(tl, seqno);
    }

    0
}

// upstream: intel_timeline.c intel_timeline_read_hwsp()
pub unsafe fn intel_timeline_read_hwsp(
    from: *mut I915Request,
    to: *mut I915Request,
    hwsp: *mut u32,
) -> i32 {
    let mut tl: *mut IntelTimeline;
    let err: i32;

    rcu_read_lock();
    tl = rcu_dereference((*from).timeline);
    if i915_request_signaled(from) || !i915_active_acquire_if_busy(&mut (*tl).active) {
        tl = core::ptr::null_mut();
    }

    if !tl.is_null() {
        // hwsp_offset may wraparound, so use from->hwsp_seqno
        *hwsp = i915_ggtt_offset((*tl).hwsp_ggtt)
            .wrapping_add(offset_in_page((*from).hwsp_seqno) as u32);
    }

    // ensure we wait on the right request, if not, we completed
    if !tl.is_null() && __i915_request_is_complete(from) {
        i915_active_release(&mut (*tl).active);
        tl = core::ptr::null_mut();
    }
    rcu_read_unlock();

    if tl.is_null() {
        return 1;
    }

    // Can't do semaphore waits on kernel context
    if !(*tl).has_initial_breadcrumb {
        err = -EINVAL;
    } else {
        err = i915_active_add_request(&mut (*tl).active, to);
    }

    i915_active_release(&mut (*tl).active);
    err
}

// upstream: intel_timeline.c intel_timeline_unpin()
pub unsafe fn intel_timeline_unpin(tl: *mut IntelTimeline) {
    GEM_BUG_ON!(atomic_read(&(*tl).pin_count) == 0);
    if !atomic_dec_and_test(&mut (*tl).pin_count) {
        return;
    }

    i915_active_release(&mut (*tl).active);
    __i915_vma_unpin((*tl).hwsp_ggtt);
}

// upstream: intel_timeline.c __intel_timeline_free()
pub unsafe fn __intel_timeline_free(kref: *mut Kref) {
    let timeline = container_of!(kref, IntelTimeline, kref);

    GEM_BUG_ON!(atomic_read(&(*timeline).pin_count) != 0);
    GEM_BUG_ON!(!list_empty(&(*timeline).requests));
    GEM_BUG_ON!(!(*timeline).retire.is_null());

    call_rcu(&mut (*timeline).rcu, intel_timeline_fini);
}

// upstream: intel_timeline.c intel_gt_fini_timelines()
pub unsafe fn intel_gt_fini_timelines(gt: *mut IntelGt) {
    let timelines = &mut (*gt).timelines;

    GEM_BUG_ON!(!list_empty(&timelines.active_list));
}

// upstream: intel_timeline.c intel_gt_show_timelines()
pub unsafe fn intel_gt_show_timelines(
    gt: *mut IntelGt,
    m: *mut DrmPrinter,
    show_request: Option<
        unsafe fn(m: *mut DrmPrinter, rq: *const I915Request, prefix: *const c_char, indent: i32),
    >,
) {
    let timelines = &mut (*gt).timelines;
    let mut tl: *mut IntelTimeline;
    let mut free = ListHead {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
    };
    INIT_LIST_HEAD!(&mut free);

    let active_list = &mut timelines.active_list as *mut ListHead;
    spin_lock(&mut timelines.lock);
    let mut cursor = (*active_list).next;
    while !core::ptr::eq(cursor, active_list) {
        // Save the next entry before dropping timelines->lock. If another
        // thread changes the list while the lock is dropped, refresh cursor
        // from tl->link after reacquiring it, matching list_safe_reset_next.
        let next = (*cursor).next;
        tl = container_of!(cursor, IntelTimeline, link);
        cursor = next;

        let mut count: c_ulong;
        let mut ready: c_ulong;
        let mut inflight: c_ulong;
        let mut rq: *mut I915Request;
        let mut rn: *mut I915Request;
        let mut fence: *mut DmaFence;

        if !mutex_trylock(&mut (*tl).mutex) {
            drm_printf!(m, "Timeline %llx: busy; skipping\n", (*tl).fence_context);
            continue;
        }

        intel_timeline_get(tl);
        GEM_BUG_ON!(atomic_read(&(*tl).active_count) == 0);
        atomic_inc(&mut (*tl).active_count); // pin the list element
        spin_unlock(&mut timelines.lock);

        count = 0;
        ready = 0;
        inflight = 0;
        list_for_each_entry_safe!(rq, rn, &mut (*tl).requests, link, {
            if i915_request_completed(rq) {
                continue;
            }

            count += 1;
            if i915_request_is_ready(rq) {
                ready += 1;
            }
            if i915_request_is_active(rq) {
                inflight += 1;
            }
        });

        drm_printf!(m, "Timeline %llx: { ", (*tl).fence_context);
        drm_printf!(
            m,
            "count: %lu, ready: %lu, inflight: %lu",
            count,
            ready,
            inflight
        );
        drm_printf!(
            m,
            ", seqno: { current: %d, last: %d }",
            *(*tl).hwsp_seqno,
            (*tl).seqno
        );
        fence = i915_active_fence_get(&mut (*tl).last_request);
        if !fence.is_null() {
            drm_printf!(m, ", engine: %s", (*(*to_request(fence)).engine).name);
            dma_fence_put(fence);
        }
        drm_printf!(m, " }\n");

        if let Some(show_request) = show_request {
            list_for_each_entry_safe!(rq, rn, &mut (*tl).requests, link, {
                show_request(m, rq, b"\0".as_ptr().cast::<c_char>(), 2);
            });
        }

        mutex_unlock(&mut (*tl).mutex);
        spin_lock(&mut timelines.lock);

        // Resume list iteration after reacquiring spinlock
        cursor = (*tl).link.next;
        if atomic_dec_and_test(&mut (*tl).active_count) {
            list_del(&mut (*tl).link);
        }

        // Defer the final release to after the spinlock
        if refcount_dec_and_test(&mut (*tl).kref.refcount) {
            GEM_BUG_ON!(atomic_read(&(*tl).active_count) != 0);
            list_add(&mut (*tl).link, &mut free);
        }
    }
    spin_unlock(&mut timelines.lock);

    list_for_each_entry_safe!(tl, tn, &mut free, link, {
        __intel_timeline_free(&mut (*tl).kref);
    });
}
