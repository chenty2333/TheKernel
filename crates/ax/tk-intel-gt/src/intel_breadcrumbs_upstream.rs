// SPDX-License-Identifier: MIT
// Copyright © 2015-2021 Intel Corporation.
//
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_breadcrumbs.c. This module is registered; its
// upstream GEM/RCU/locking operations remain binding points, not substitute
// implementations. Keep source order and IRQ, power-reference, list, and
// signal ordering intact.

use core::ffi::c_ulong;

use crate::{
    i915_request_types_upstream::*, intel_breadcrumbs_types_upstream::IntelBreadcrumbs,
    intel_execlists_submission_upstream::intel_timeline_is_last,
    intel_context_types_upstream::*, intel_context_upstream::*, intel_engine_cs_upstream::*,
    intel_engine_types_upstream::IntelEngineCs, intel_gt_types_upstream::IntelGt,
    intel_timeline_types_upstream::IntelTimeline, linux_config::*, linux_list::*,
};

// Prefer the i915 request-private fence bits over LinuxKPI compatibility
// aliases, which share the historical names but use the generic fence layout.
use crate::i915_request_types_upstream::{
    I915_FENCE_FLAG_ACTIVE, I915_FENCE_FLAG_SIGNAL,
};

unsafe extern "C" {
    fn intel_engine_add_retire(engine: *mut IntelEngineCs, timeline: *mut IntelTimeline);
    fn trace_dma_fence_signaled(fence: *mut DmaFence);
}

// Header-owned breadcrumb, engine, context, request and timeline records are
// imported above. Remaining Linux GEM/RCU/locking/PM services, allocation,
// trace hooks and BUG helpers are explicit integration boundaries.

// upstream: intel_breadcrumbs.c irq_enable()
unsafe extern "C" fn irq_enable(b: *mut IntelBreadcrumbs) -> bool {
    crate::intel_engine_cs_upstream::intel_engine_irq_enable((*b).irq_engine)
}

// upstream: intel_breadcrumbs.c irq_disable()
unsafe extern "C" fn irq_disable(b: *mut IntelBreadcrumbs) {
    crate::intel_engine_cs_upstream::intel_engine_irq_disable((*b).irq_engine);
}

// upstream: intel_breadcrumbs.c __intel_breadcrumbs_arm_irq()
unsafe fn __intel_breadcrumbs_arm_irq(b: *mut IntelBreadcrumbs) {
    // Since we are waiting on a request, the GPU should be busy
    // and should have its own rpm reference.
    let wakeref = intel_gt_pm_get_if_awake((*(*b).irq_engine).gt);
    if GEM_WARN_ON!(wakeref.is_null()) {
        return;
    }

    // The breadcrumb irq will be disarmed on the interrupt after the
    // waiters are signaled. This gives us a single interrupt window in
    // which we can add a new waiter and avoid the cost of re-enabling
    // the irq.
    WRITE_ONCE!((*b).irq_armed, wakeref);

    // Requests may have completed before we could enable the interrupt.
    let was_disabled = (*b).irq_enabled == 0;
    (*b).irq_enabled += 1;
    if was_disabled && ((*b).irq_enable.unwrap())(b) {
        irq_work_queue(&mut (*b).irq_work);
    }
}

// upstream: intel_breadcrumbs.c intel_breadcrumbs_arm_irq()
unsafe fn intel_breadcrumbs_arm_irq(b: *mut IntelBreadcrumbs) {
    if (*b).irq_engine.is_null() {
        return;
    }

    spin_lock(&mut (*b).irq_lock);
    if (*b).irq_armed.is_null() {
        __intel_breadcrumbs_arm_irq(b);
    }
    spin_unlock(&mut (*b).irq_lock);
}

// upstream: intel_breadcrumbs.c __intel_breadcrumbs_disarm_irq()
unsafe fn __intel_breadcrumbs_disarm_irq(b: *mut IntelBreadcrumbs) {
    let wakeref = (*b).irq_armed;

    GEM_BUG_ON!((*b).irq_enabled == 0);
    (*b).irq_enabled -= 1;
    if (*b).irq_enabled == 0 {
        ((*b).irq_disable.unwrap())(b);
    }

    WRITE_ONCE!((*b).irq_armed, core::ptr::null_mut());
    intel_gt_pm_put_async((*(*b).irq_engine).gt, wakeref);
}

// upstream: intel_breadcrumbs.c intel_breadcrumbs_disarm_irq()
unsafe fn intel_breadcrumbs_disarm_irq(b: *mut IntelBreadcrumbs) {
    spin_lock(&mut (*b).irq_lock);
    if !(*b).irq_armed.is_null() {
        __intel_breadcrumbs_disarm_irq(b);
    }
    spin_unlock(&mut (*b).irq_lock);
}

// upstream: intel_breadcrumbs.c add_signaling_context()
unsafe fn add_signaling_context(b: *mut IntelBreadcrumbs, ce: *mut IntelContext) {
    lockdep_assert_held!(&(*ce).signal_lock);

    spin_lock(&mut (*b).signalers_lock);
    list_add_rcu(&mut (*ce).signal_link, &mut (*b).signalers);
    spin_unlock(&mut (*b).signalers_lock);
}

// upstream: intel_breadcrumbs.c remove_signaling_context()
unsafe fn remove_signaling_context(b: *mut IntelBreadcrumbs, ce: *mut IntelContext) -> bool {
    lockdep_assert_held!(&(*ce).signal_lock);

    if !list_empty(&(*ce).signals) {
        return false;
    }

    spin_lock(&mut (*b).signalers_lock);
    list_del_rcu(&mut (*ce).signal_link);
    spin_unlock(&mut (*b).signalers_lock);

    true
}

// upstream: intel_breadcrumbs.c check_signal_order()
#[allow(dead_code)]
unsafe fn check_signal_order(ce: *mut IntelContext, rq: *mut I915Request) -> bool {
    if (*rq).context != ce {
        return false;
    }

    if !list_is_last(&(*rq).signal_link, &(*ce).signals)
        && i915_seqno_passed(
            (*rq).fence.seqno as u32,
            (*list_next_entry!(rq, signal_link)).fence.seqno as u32,
        )
    {
        return false;
    }

    if !list_is_first(&(*rq).signal_link, &(*ce).signals)
        && i915_seqno_passed(
            (*list_prev_entry!(rq, signal_link)).fence.seqno as u32,
            (*rq).fence.seqno as u32,
        )
    {
        return false;
    }

    true
}

// upstream: intel_breadcrumbs.c __dma_fence_signal()
unsafe fn __dma_fence_signal(fence: *mut DmaFence) -> bool {
    !test_and_set_bit(DMA_FENCE_FLAG_SIGNALED_BIT, &mut (*fence).flags)
}

// upstream: intel_breadcrumbs.c __dma_fence_signal__timestamp()
unsafe fn __dma_fence_signal__timestamp(fence: *mut DmaFence, timestamp: KtimeT) {
    unsafe { (*fence).timestamp_union.timestamp = timestamp };
    set_bit(DMA_FENCE_FLAG_TIMESTAMP_BIT, &mut (*fence).flags);
    trace_dma_fence_signaled(fence);
}

// upstream: intel_breadcrumbs.c __dma_fence_signal__notify()
unsafe fn __dma_fence_signal__notify(fence: *mut DmaFence, list: *mut ListHead) {
    let mut cur: *mut DmaFenceCb = core::ptr::null_mut();
    let mut tmp: *mut DmaFenceCb = core::ptr::null_mut();

    dma_fence_assert_held!(fence);

    list_for_each_entry_safe!(cur, tmp, list, node, {
        INIT_LIST_HEAD(&mut (*cur).node);
        ((*cur).func.unwrap())(fence, cur);
    });
}

// upstream: intel_breadcrumbs.c add_retire()
unsafe fn add_retire(b: *mut IntelBreadcrumbs, tl: *mut IntelTimeline) {
    if !(*b).irq_engine.is_null() {
        intel_engine_add_retire((*b).irq_engine, tl);
    }
}

// upstream: intel_breadcrumbs.c slist_add()
unsafe fn slist_add(node: *mut LlistNode, head: *mut LlistNode) -> *mut LlistNode {
    (*node).next = head;
    node
}

// upstream: intel_breadcrumbs.c signal_irq_work()
unsafe extern "C" fn signal_irq_work(work: *mut IrqWork) {
    let b = container_of!(work, IntelBreadcrumbs, irq_work);
    let timestamp = ktime_get();
    let mut signal: *mut LlistNode;
    let mut sn: *mut LlistNode;
    let mut ce: *mut IntelContext = core::ptr::null_mut();

    signal = core::ptr::null_mut();
    if unlikely!(!llist_empty(&(*b).signaled_requests)) {
        signal = llist_del_all(&mut (*b).signaled_requests);
    }

    // Keep the irq armed until the interrupt after all listeners are gone.
    //
    // Enabling/disabling the interrupt is rather costly, roughly a couple
    // of hundred microseconds. If we are proactive and enable/disable the
    // interrupt around every request that wants a breadcrumb, we
    // quickly drown in the extra orders of magnitude of latency imposed
    // on request submission.
    //
    // So we try to be lazy, and keep the interrupts enabled until no
    // more listeners appear within a breadcrumb interrupt interval (that
    // is until a request completes that no one cares about). The
    // observation is that listeners come in batches, and will often
    // listen to a bunch of requests in succession. Though note on icl+,
    // interrupts are always enabled due to concerns with rc6 being
    // dysfunctional with per-engine interrupt masking.
    //
    // We also try to avoid raising too many interrupts, as they may
    // be generated by userspace batches and it is unfortunately rather
    // too easy to drown the CPU under a flood of GPU interrupts. Thus
    // whenever no one appears to be listening, we turn off the interrupts.
    // Fewer interrupts should conserve power -- at the very least, fewer
    // interrupt draw less ire from other users of the system and tools
    // like powertop.
    if signal.is_null() && !READ_ONCE!((*b).irq_armed).is_null() && list_empty(&(*b).signalers) {
        intel_breadcrumbs_disarm_irq(b);
    }

    rcu_read_lock();
    atomic_inc(&mut (*b).signaler_active);
    list_for_each_entry_rcu!(ce, &(*b).signalers, signal_link, {
        let mut rq: *mut I915Request = core::ptr::null_mut();

        list_for_each_entry_rcu!(rq, &(*ce).signals, signal_link, {
            let release: bool;

            if !__i915_request_is_complete(rq) {
                break;
            }

            if !test_and_clear_bit(I915_FENCE_FLAG_SIGNAL, &mut (*rq).fence.flags) {
                break;
            }

            // Queue for execution after dropping the signaling
            // spinlock as the callback chain may end up adding
            // more signalers to the same context or engine.
            spin_lock(&mut (*ce).signal_lock);
            list_del_rcu(&mut (*rq).signal_link);
            release = remove_signaling_context(b, ce);
            spin_unlock(&mut (*ce).signal_lock);
            if release {
                if intel_timeline_is_last((*ce).timeline, rq) {
                    add_retire(b, (*ce).timeline);
                }
                intel_context_put(ce);
            }

            if __dma_fence_signal(&mut (*rq).fence) {
                // We own signal_node now, xfer to local list
                signal = slist_add(&mut (*rq).signal_node, signal);
            } else {
                i915_request_put(rq);
            }
        });
    });
    atomic_dec(&mut (*b).signaler_active);
    rcu_read_unlock();

    llist_for_each_safe!(signal, sn, signal, {
        let rq = llist_entry!(signal, I915Request, signal_node);
        let mut cb_list = ListHead {
            next: core::ptr::null_mut(),
            prev: core::ptr::null_mut(),
        };
        INIT_LIST_HEAD!(&mut cb_list);

        if let Some(retire_inflight_request_prio) =
            (*(*(*rq).engine).sched_engine).retire_inflight_request_prio
        {
            retire_inflight_request_prio(rq);
        }

        spin_lock(&mut (*rq).lock);
        list_replace(
            core::ptr::addr_of_mut!((*rq).fence.timestamp_union.cb_list).cast::<ListHead>(),
            &mut cb_list,
        );
        __dma_fence_signal__timestamp(&mut (*rq).fence, timestamp);
        __dma_fence_signal__notify(&mut (*rq).fence, &mut cb_list);
        spin_unlock(&mut (*rq).lock);

        i915_request_put(rq);
    });

    // Lazy irq enabling after HW submission
    if READ_ONCE!((*b).irq_armed).is_null() && !list_empty(&(*b).signalers) {
        intel_breadcrumbs_arm_irq(b);
    }

    // And confirm that we still want irqs enabled before we yield
    if !READ_ONCE!((*b).irq_armed).is_null() && atomic_read(&(*b).active) == 0 {
        intel_breadcrumbs_disarm_irq(b);
    }
}

// upstream: intel_breadcrumbs.c intel_breadcrumbs_create()
pub unsafe fn intel_breadcrumbs_create(irq_engine: *mut IntelEngineCs) -> *mut IntelBreadcrumbs {
    let mut b: *mut IntelBreadcrumbs;

    b = kzalloc_obj::<IntelBreadcrumbs>();
    if b.is_null() {
        return core::ptr::null_mut();
    }

    kref_init(&mut (*b).r#ref);

    spin_lock_init(&mut (*b).signalers_lock);
    INIT_LIST_HEAD(&mut (*b).signalers);
    init_llist_head(&mut (*b).signaled_requests);

    spin_lock_init(&mut (*b).irq_lock);
    init_irq_work(&mut (*b).irq_work, signal_irq_work);

    (*b).irq_engine = irq_engine;
    (*b).irq_enable = Some(irq_enable);
    (*b).irq_disable = Some(irq_disable);

    b
}

// upstream: intel_breadcrumbs.c intel_breadcrumbs_reset()
pub unsafe fn intel_breadcrumbs_reset(b: *mut IntelBreadcrumbs) {
    let mut flags: c_ulong = 0;

    if (*b).irq_engine.is_null() {
        return;
    }

    spin_lock_irqsave(&mut (*b).irq_lock, &mut flags);

    if (*b).irq_enabled != 0 {
        ((*b).irq_enable.unwrap())(b);
    } else {
        ((*b).irq_disable.unwrap())(b);
    }

    spin_unlock_irqrestore(&mut (*b).irq_lock, flags);
}

// upstream: intel_breadcrumbs.c __intel_breadcrumbs_park()
unsafe fn __intel_breadcrumbs_park(b: *mut IntelBreadcrumbs) {
    if READ_ONCE!((*b).irq_armed).is_null() {
        return;
    }

    // Kick the work once more to drain the signalers, and disarm the irq
    irq_work_queue(&mut (*b).irq_work);
}

// upstream: intel_breadcrumbs.c intel_breadcrumbs_free()
unsafe extern "C" fn intel_breadcrumbs_free(kref: *mut Kref) {
    let b = container_of!(kref, IntelBreadcrumbs, r#ref);

    irq_work_sync(&mut (*b).irq_work);
    GEM_BUG_ON!(!list_empty(&(*b).signalers));
    GEM_BUG_ON!(!(*b).irq_armed.is_null());

    kfree(b);
}

// upstream: intel_breadcrumbs.h intel_breadcrumbs_get()
pub unsafe fn intel_breadcrumbs_get(b: *mut IntelBreadcrumbs) -> *mut IntelBreadcrumbs {
    crate::linux_memory::kref_get(&mut (*b).r#ref);
    b
}

// upstream: intel_breadcrumbs.h intel_breadcrumbs_put()
pub unsafe fn intel_breadcrumbs_put(b: *mut IntelBreadcrumbs) {
    crate::linux_memory::kref_put(&mut (*b).r#ref, intel_breadcrumbs_free);
}

// upstream: intel_breadcrumbs.c irq_signal_request()
unsafe fn irq_signal_request(rq: *mut I915Request, b: *mut IntelBreadcrumbs) {
    if !__dma_fence_signal(&mut (*rq).fence) {
        return;
    }

    i915_request_get(rq);
    if llist_add(&mut (*rq).signal_node, &mut (*b).signaled_requests) {
        irq_work_queue(&mut (*b).irq_work);
    }
}

// upstream: intel_breadcrumbs.c insert_breadcrumb()
unsafe fn insert_breadcrumb(rq: *mut I915Request) {
    let b = READ_ONCE!((*(*rq).engine).breadcrumbs);
    let ce = (*rq).context;
    let mut pos: *mut ListHead;

    if test_bit(I915_FENCE_FLAG_SIGNAL, &(*rq).fence.flags) {
        return;
    }

    // If the request is already completed, we can transfer it
    // straight onto a signaled list, and queue the irq worker for
    // its signal completion.
    if __i915_request_is_complete(rq) {
        irq_signal_request(rq, b);
        return;
    }

    if list_empty(&(*ce).signals) {
        intel_context_get(ce);
        add_signaling_context(b, ce);
        pos = &mut (*ce).signals;
    } else {
        // We keep the seqno in retirement order, so we can break
        // inside intel_engine_signal_breadcrumbs as soon as we've
        // passed the last completed request (or seen a request that
        // hasn't event started). We could walk the timeline->requests,
        // but keeping a separate signalers_list has the advantage of
        // hopefully being much smaller than the full list and so
        // provides faster iteration and detection when there are no
        // more interrupts required for this context.
        //
        // We typically expect to add new signalers in order, so we
        // start looking for our insertion point from the tail of
        // the list.
        pos = core::ptr::null_mut();
        list_for_each_prev!(pos, &(*ce).signals, {
            let it = list_entry!(pos, I915Request, signal_link);

            if i915_seqno_passed((*rq).fence.seqno as u32, (*it).fence.seqno as u32) {
                break;
            }
        });
    }

    i915_request_get(rq);
    list_add_rcu(&mut (*rq).signal_link, pos);
    GEM_BUG_ON!(!check_signal_order(ce, rq));
    GEM_BUG_ON!(test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, &(*rq).fence.flags));
    set_bit(I915_FENCE_FLAG_SIGNAL, &mut (*rq).fence.flags);

    // Defer enabling the interrupt to after HW submission and recheck
    // the request as it may have completed and raised the interrupt as
    // we were attaching it into the lists.
    if READ_ONCE!((*b).irq_armed).is_null() || __i915_request_is_complete(rq) {
        irq_work_queue(&mut (*b).irq_work);
    }
}

// upstream: intel_breadcrumbs.c i915_request_enable_breadcrumb()
unsafe fn i915_request_enable_breadcrumb(rq: *mut I915Request) -> bool {
    let ce = (*rq).context;

    // Serialises with i915_request_retire() using rq->lock
    if test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, &(*rq).fence.flags) {
        return true;
    }

    // Peek at i915_request_submit()/i915_request_unsubmit() status.
    //
    // If the request is not yet active (and not signaled), we will
    // attach the breadcrumb later.
    if !test_bit(I915_FENCE_FLAG_ACTIVE, &(*rq).fence.flags) {
        return true;
    }

    spin_lock(&mut (*ce).signal_lock);
    if test_bit(I915_FENCE_FLAG_ACTIVE, &(*rq).fence.flags) {
        insert_breadcrumb(rq);
    }
    spin_unlock(&mut (*ce).signal_lock);

    true
}

// upstream: intel_breadcrumbs.c i915_request_cancel_breadcrumb()
unsafe fn i915_request_cancel_breadcrumb(rq: *mut I915Request) {
    let b = READ_ONCE!((*(*rq).engine).breadcrumbs);
    let ce = (*rq).context;
    let release: bool;

    spin_lock(&mut (*ce).signal_lock);
    if !test_and_clear_bit(I915_FENCE_FLAG_SIGNAL, &mut (*rq).fence.flags) {
        spin_unlock(&mut (*ce).signal_lock);
        return;
    }

    list_del_rcu(&mut (*rq).signal_link);
    release = remove_signaling_context(b, ce);
    spin_unlock(&mut (*ce).signal_lock);
    if release {
        intel_context_put(ce);
    }

    if __i915_request_is_complete(rq) {
        irq_signal_request(rq, b);
    }

    i915_request_put(rq);
}

// upstream: intel_breadcrumbs.c intel_context_remove_breadcrumbs()
pub(crate) unsafe fn intel_context_remove_breadcrumbs(
    ce: *mut IntelContext,
    b: *mut IntelBreadcrumbs,
) {
    let mut rq: *mut I915Request = core::ptr::null_mut();
    let mut rn: *mut I915Request = core::ptr::null_mut();
    let mut release = false;
    let mut flags: c_ulong = 0;

    spin_lock_irqsave(&mut (*ce).signal_lock, &mut flags);

    if !list_empty(&(*ce).signals) {
        list_for_each_entry_safe!(rq, rn, &(*ce).signals, signal_link, {
            GEM_BUG_ON!(!__i915_request_is_complete(rq));
            if !test_and_clear_bit(I915_FENCE_FLAG_SIGNAL, &mut (*rq).fence.flags) {
                continue;
            }

            list_del_rcu(&mut (*rq).signal_link);
            irq_signal_request(rq, b);
            i915_request_put(rq);
        });
        release = remove_signaling_context(b, ce);
    }

    spin_unlock_irqrestore(&mut (*ce).signal_lock, flags);
    if release {
        intel_context_put(ce);
    }

    while atomic_read(&(*b).signaler_active) != 0 {
        core::hint::spin_loop();
    }
}

// upstream: intel_breadcrumbs.c print_signals()
unsafe fn print_signals(b: *mut IntelBreadcrumbs, p: *mut DrmPrinter) {
    let mut ce: *mut IntelContext = core::ptr::null_mut();
    let mut rq: *mut I915Request = core::ptr::null_mut();

    drm_printf!(p, "Signals:\n");

    rcu_read_lock();
    list_for_each_entry_rcu!(ce, &(*b).signalers, signal_link, {
        list_for_each_entry_rcu!(rq, &(*ce).signals, signal_link, {
            drm_printf!(
                p,
                "\t[%llx:%llx%s] @ %dms\n",
                (*rq).fence.context,
                (*rq).fence.seqno,
                if __i915_request_is_complete(rq) {
                    "!"
                } else if __i915_request_has_started(rq) {
                    "*"
                } else {
                    ""
                },
                jiffies_to_msecs(jiffies() - (*rq).emitted_jiffies),
            );
        });
    });
    rcu_read_unlock();
}

// upstream: intel_breadcrumbs.c intel_engine_print_breadcrumbs()
pub(crate) unsafe fn intel_engine_print_breadcrumbs(
    engine: *mut IntelEngineCs,
    p: *mut DrmPrinter,
) {
    let b: *mut IntelBreadcrumbs;

    b = (*engine).breadcrumbs;
    if b.is_null() {
        return;
    }

    drm_printf!(
        p,
        "IRQ: %s\n",
        if !READ_ONCE!((*b).irq_armed).is_null() { "enabled" } else { "disabled" },
    );
    if !list_empty(&(*b).signalers) {
        print_signals(b, p);
    }
}
