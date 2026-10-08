// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation.
use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::{size_of, size_of_val},
};

/// DOC: Logical Rings, Logical Ring Contexts and Execlists
///
/// Motivation:
/// GEN8 brings an expansion of the HW contexts: "Logical Ring Contexts".
/// These expanded contexts enable a number of new abilities, especially
/// "Execlists" (also implemented in this file).
///
/// One of the main differences with the legacy HW contexts is that logical
/// ring contexts incorporate many more things to the context's state, like
/// PDPs or ringbuffer control registers:
///
/// The reason why PDPs are included in the context is straightforward: as
/// PPGTTs (per-process GTTs) are actually per-context, having the PDPs
/// contained there mean you don't need to do a ppgtt->switch_mm yourself,
/// instead, the GPU will do it for you on the context switch.
///
/// But, what about the ringbuffer control registers (head, tail, etc..)?
/// shouldn't we just need a set of those per engine command streamer? This is
/// where the name "Logical Rings" starts to make sense: by virtualizing the
/// rings, the engine cs shifts to a new "ring buffer" with every context
/// switch. When you want to submit a workload to the GPU you: A) choose your
/// context, B) find its appropriate virtualized ring, C) write commands to it
/// and then, finally, D) tell the GPU to switch to that context.
///
/// Instead of the legacy MI_SET_CONTEXT, the way you tell the GPU to switch
/// to a contexts is via a context execution list, ergo "Execlists".
///
/// LRC implementation:
/// Regarding the creation of contexts, we have:
///
/// - One global default context.
/// - One local default context for each opened fd.
/// - One local extra context for each context create ioctl call.
///
/// Now that ringbuffers belong per-context (and not per-engine, like before)
/// and that contexts are uniquely tied to a given engine (and not reusable,
/// like before) we need:
///
/// - One ringbuffer per-engine inside each context.
/// - One backing object per-engine inside each context.
///
/// The global default context starts its life with these new objects fully
/// allocated and populated. The local default context for each opened fd is
/// more complex, because we don't know at creation time which engine is going
/// to use them. To handle this, we have implemented a deferred creation of LR
/// contexts:
///
/// The local context starts its life as a hollow or blank holder, that only
/// gets populated for a given engine once we receive an execbuffer. If later
/// on we receive another execbuffer ioctl for the same context but a different
/// engine, we allocate/populate a new ringbuffer and context backing object and
/// so on.
///
/// Finally, regarding local contexts created using the ioctl call: as they are
/// only allowed with the render ring, we can allocate & populate them right
/// away (no need to defer anything, at least for now).
///
/// Execlists implementation:
/// Execlists are the new method by which, on gen8+ hardware, workloads are
/// submitted for execution (as opposed to the legacy, ringbuffer-based, method).
/// This method works as follows:
///
/// When a request is committed, its commands (the BB start and any leading or
/// trailing commands, like the seqno breadcrumbs) are placed in the ringbuffer
/// for the appropriate context. The tail pointer in the hardware context is not
/// updated at this time, but instead, kept by the driver in the ringbuffer
/// structure. A structure representing this request is added to a request queue
/// for the appropriate engine: this structure contains a copy of the context's
/// tail after the request was written to the ring buffer and a pointer to the
/// context itself.
///
/// If the engine's request queue was empty before the request was added, the
/// queue is processed immediately. Otherwise the queue will be processed during
/// a context switch interrupt. In any case, elements on the queue will get sent
/// (in pairs) to the GPU's ExecLists Submit Port (ELSP, for short) with a
/// globally unique 20-bits submission ID.
///
/// When execution of a request completes, the GPU updates the context status
/// buffer with a context complete event and generates a context switch interrupt.
/// During the interrupt handling, the driver examines the events in the buffer:
/// for each context complete event, if the announced ID matches that on the head
/// of the request queue, then that request is retired and removed from the queue.
///
/// After processing, if any requests were retired and the queue is not empty
/// then a new execution list can be submitted. The two requests at the front of
/// the queue are next to be submitted but since a context may not occur twice in
/// an execution list, if subsequent requests have the same ID as the first then
/// the two requests must be combined. This is done simply by discarding requests
/// at the head of the queue until either only one requests is left (in which case
/// we use a NULL second context) or the first two requests have unique IDs.
///
/// By always executing the first two requests in the queue the driver ensures
/// that the GPU is kept as busy as possible. In the case where a single context
/// completes but a second context is still executing, the request for this second
/// context will be at the head of the queue when we remove the first one. This
/// request will then be resubmitted along with a new request for a different context,
/// which will cause the hardware to continue executing the second request and queue
/// the new request (the GPU detects the condition of a context getting preempted
/// with the same context and optimizes the context switch flow by not doing
/// preemption, but just sampling the new tail pointer).
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_execlists_submission.c. This module is
// registered; kernel GEM, RCU, scheduler, interrupt, timer, and register
// services remain binding points, not substitute implementations. Preserve
// upstream source order and queue, preemption, time-slicing, reset, and
// submission ordering.
use crate::linux_list::*;
use crate::{
    for_each_signaler, for_each_waiter, intel_context_upstream::*, intel_engine_cs_upstream::*,
    linux_config::*,
};

// Binding points supplied by the surrounding kernel integration: IntelEngineCs,
// IntelContext, I915Request, IntelTimeline, IntelEngineExeclists, IntelSchedEngine,
// IntelPriolist, VirtualEngine, IntelEngineStats, ExecListCapture, Linux lists,
// rbtrees, RCU, atomics, timers, workqueues, MMIO, trace hooks, scheduler and
// request/fence helpers, allocation helpers, error codes, and BUG/WARN macros.

const RING_EXECLIST_QFULL: u32 = 1 << 0x2;
const RING_EXECLIST1_VALID: u32 = 1 << 0x3;
const RING_EXECLIST0_VALID: u32 = 1 << 0x4;
const RING_EXECLIST_ACTIVE_STATUS: u32 = 3 << 0xE;
const RING_EXECLIST1_ACTIVE: u32 = 1 << 0x11;
const RING_EXECLIST0_ACTIVE: u32 = 1 << 0x12;

const GEN8_CTX_STATUS_IDLE_ACTIVE: u32 = 1 << 0;
const GEN8_CTX_STATUS_PREEMPTED: u32 = 1 << 1;
const GEN8_CTX_STATUS_ELEMENT_SWITCH: u32 = 1 << 2;
const GEN8_CTX_STATUS_ACTIVE_IDLE: u32 = 1 << 3;
const GEN8_CTX_STATUS_COMPLETE: u32 = 1 << 4;
const GEN8_CTX_STATUS_LITE_RESTORE: u32 = 1 << 15;
const GEN8_CTX_STATUS_COMPLETED_MASK: u32 = GEN8_CTX_STATUS_COMPLETE | GEN8_CTX_STATUS_PREEMPTED;
const GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE: u32 = 0x1;
const GEN12_CSB_SW_CTX_ID_MASK: u32 = GENMASK!(25, 15);
const GEN12_IDLE_CTX_ID: u32 = 0x7ff;
const XEHP_CTX_STATUS_SWITCHED_TO_NEW_QUEUE: u32 = BIT!(1);
const XEHP_CSB_SW_CTX_ID_MASK: u32 = GENMASK!(31, 10);
const XEHP_IDLE_CTX_ID: u32 = 0xffff;
const EXECLISTS_REQUEST_SIZE: u32 = 64;

#[allow(non_snake_case)]
const fn GEN12_CTX_SWITCH_DETAIL(csb_dw: u32) -> u8 {
    (csb_dw & 0xF) as u8
}

#[allow(non_snake_case)]
const fn GEN12_CSB_CTX_VALID(csb_dw: u32) -> bool {
    ((csb_dw & GEN12_CSB_SW_CTX_ID_MASK) >> 15) != GEN12_IDLE_CTX_ID
}

#[allow(non_snake_case)]
const fn XEHP_CSB_CTX_VALID(csb_dw: u32) -> bool {
    ((csb_dw & XEHP_CSB_SW_CTX_ID_MASK) >> 10) != XEHP_IDLE_CTX_ID
}

#[repr(C)]
struct VirtualEngine {
    base: IntelEngineCs,
    context: IntelContext,
    rcu: RcuWork,
    request: *mut I915Request,
    nodes: [VeNode; I915_NUM_ENGINES],
    num_siblings: u32,
    siblings: [*mut IntelEngineCs; 0],
}

#[repr(C)]
struct VeNode {
    rb: RbNode,
    prio: i32,
}

// upstream: intel_execlists_submission.c to_virtual_engine()
unsafe fn to_virtual_engine(engine: *mut IntelEngineCs) -> *mut VirtualEngine {
    GEM_BUG_ON!(!intel_engine_is_virtual(engine));
    container_of!(engine, VirtualEngine, base)
}

// upstream: intel_execlists_submission.c __active_request()
unsafe fn __active_request(
    tl: *const IntelTimeline,
    mut rq: *mut I915Request,
    error: i32,
) -> *mut I915Request {
    let mut active = rq;

    list_for_each_entry_from_reverse!(rq, &(*tl).requests, link, {
        if __i915_request_is_complete(rq) {
            break;
        }

        if error != 0 {
            i915_request_set_error_once(rq, error);
            __i915_request_skip(rq);
        }
        active = rq;
    });

    active
}

// upstream: intel_execlists_submission.c active_request()
unsafe fn active_request(tl: *const IntelTimeline, rq: *mut I915Request) -> *mut I915Request {
    __active_request(tl, rq, 0)
}

// upstream: intel_execlists_submission.c ring_set_paused()
unsafe fn ring_set_paused(engine: *const IntelEngineCs, state: i32) {
    // We inspect HWS_PREEMPT with a semaphore inside
    // engine->emit_fini_breadcrumb. If the dword is true, the ring is paused
    // as the semaphore will busywait until the dword is false.
    *(*engine).status_page.addr.add(I915_GEM_HWS_PREEMPT) = state as u32;
    if state != 0 {
        wmb();
    }
}

// upstream: intel_execlists_submission.c to_priolist()
unsafe fn to_priolist(rb: *mut RbNode) -> *mut I915Priolist {
    rb_entry!(rb, I915Priolist, node)
}

// upstream: intel_execlists_submission.c rq_prio()
unsafe fn rq_prio(rq: *const I915Request) -> i32 {
    READ_ONCE!((*rq).sched.attr.priority)
}

// upstream: intel_execlists_submission.c effective_prio()
unsafe fn effective_prio(rq: *const I915Request) -> i32 {
    let mut prio = rq_prio(rq);

    // If this request is special and must not be interrupted at any cost,
    // so be it. We only inspect the most recent request in the context and
    // may therefore mask an earlier VIP request. Under nopreempt, all
    // requests to that context are expected to remain nopreempt as desired.
    if i915_request_has_nopreempt(rq) {
        prio = I915_PRIORITY_UNPREEMPTABLE;
    }

    prio
}

// upstream: intel_execlists_submission.c queue_prio()
unsafe fn queue_prio(sched_engine: *const I915SchedEngine) -> i32 {
    let rb = rb_first_cached(&(*sched_engine).queue);
    if rb.is_null() {
        return INT_MIN;
    }

    (*to_priolist(rb)).priority
}

// upstream: intel_execlists_submission.c virtual_prio()
unsafe fn virtual_prio(el: *const IntelEngineExeclists) -> i32 {
    let rb = rb_first_cached(&(*el).r#virtual);
    if rb.is_null() {
        INT_MIN
    } else {
        (*rb_entry!(rb, VeNode, rb)).prio
    }
}

// upstream: intel_execlists_submission.c need_preempt()
unsafe fn need_preempt(engine: *const IntelEngineCs, rq: *const I915Request) -> bool {
    let last_prio: i32;

    if !intel_engine_has_semaphores(engine) {
        return false;
    }

    // A priority hint records the highest priority seen while rescheduling
    // before dequeue. If it is strictly below the current ESLP[0] tail, a
    // preempt-to-idle cycle is unnecessary. Ignore stale hints and attempts
    // to preempt ourselves. Do not trigger at equal priority: preserve FIFO
    // ordering of dependencies for the running task.
    last_prio = max(effective_prio(rq), I915_PRIORITY_NORMAL - 1);
    if (*(*engine).sched_engine).queue_priority_hint <= last_prio {
        return false;
    }

    // Check ELSP[1], whose priority is the highest for that context via PI.
    if !list_is_last(&(*rq).sched.link, &(*(*engine).sched_engine).requests)
        && rq_prio(list_next_entry!(rq, sched.link)) > last_prio
    {
        return true;
    }

    // Otherwise compare the first active queued priolist and virtual queue.
    max(
        virtual_prio(&(*engine).execlists),
        queue_prio((*engine).sched_engine),
    ) > last_prio
}

// upstream: intel_execlists_submission.c assert_priority_queue()
unsafe fn assert_priority_queue(prev: *const I915Request, next: *const I915Request) -> bool {
    // Without preemption, prev may be the still-active element we refuse to release.
    if i915_request_is_active(prev) {
        return true;
    }

    rq_prio(prev) >= rq_prio(next)
}

// upstream: intel_execlists_submission.c __unwind_incomplete_requests()
unsafe fn __unwind_incomplete_requests(engine: *mut IntelEngineCs) -> *mut I915Request {
    let mut rq: *mut I915Request = core::ptr::null_mut();
    let mut rn: *mut I915Request;
    let mut active: *mut I915Request = core::ptr::null_mut();
    let mut pl: *mut ListHead;
    let mut prio = I915_PRIORITY_INVALID;

    lockdep_assert_held!(&(*(*engine).sched_engine).lock);

    list_for_each_entry_safe_reverse!(
        rq,
        rn,
        I915Request,
        &(*(*engine).sched_engine).requests,
        sched.link,
        {
            if __i915_request_is_complete(rq) {
                list_del_init(&mut (*rq).sched.link);
                continue;
            }

            __i915_request_unsubmit(rq);

            GEM_BUG_ON!(rq_prio(rq) == I915_PRIORITY_INVALID);
            if rq_prio(rq) != prio {
                prio = rq_prio(rq);
                pl = i915_sched_lookup_priolist((*engine).sched_engine, prio);
            }
            GEM_BUG_ON!(i915_sched_engine_is_empty((*engine).sched_engine));

            list_move(&mut (*rq).sched.link, pl);
            set_bit(I915_FENCE_FLAG_PQUEUE, &mut (*rq).fence.flags);

            // Check in case rollback wraps farther than [size / 2].
            if intel_ring_direction((*rq).ring, (*rq).tail, (*(*rq).ring).tail + 8) > 0 {
                (*(*rq).context).lrc.desc |= CTX_DESC_FORCE_RESTORE;
            }

            active = rq;
        }
    );

    active
}

// upstream: intel_execlists_submission.c execlists_context_status_change()
unsafe fn execlists_context_status_change(rq: *mut I915Request, status: c_ulong) {
    // The compiler eliminates this GVT-g notifier when GVT-g is disabled.
    if !IS_ENABLED!(CONFIG_DRM_I915_GVT) {
        return;
    }

    atomic_notifier_call_chain(&mut (*(*rq).engine).context_status_notifier, status, rq);
}

// upstream: intel_execlists_submission.c reset_active()
unsafe fn reset_active(rq: *mut I915Request, engine: *mut IntelEngineCs) {
    let ce = (*rq).context;
    let mut head: u32;

    // The executing context was cancelled. Rewind it to the start of the
    // incomplete request before propagating -EIO so that execution cannot
    // jump into the middle of the batch. Preserve breadcrumbs and semaphores
    // to keep inter-timeline dependencies ordered; __i915_request_submit()
    // handles asynchronous waits.
    ENGINE_TRACE!(
        engine,
        "{ reset rq=%llx:%lld }\n",
        (*rq).fence.context,
        (*rq).fence.seqno
    );

    // On resubmission of the active request, its payload will be scrubbed.
    if __i915_request_is_complete(rq) {
        head = (*rq).tail;
    } else {
        head = (*__active_request((*ce).timeline, rq, -EIO)).head;
    }
    head = intel_ring_wrap((*ce).ring, head);

    // Scrub the context image to prevent replaying the previous batch.
    lrc_init_regs(ce, engine, true);

    // We have switched away; this is a no-op, but the intent matters.
    (*ce).lrc.lrca = lrc_update_regs(ce, engine, head);
}

// upstream: intel_execlists_submission.c bad_request()
unsafe fn bad_request(rq: *const I915Request) -> bool {
    (*rq).fence.error != 0 && i915_request_started(rq)
}

// upstream: intel_execlists_submission.c __execlists_schedule_in()
unsafe fn __execlists_schedule_in(rq: *mut I915Request) -> *mut IntelEngineCs {
    let engine = (*rq).engine;
    let ce = (*rq).context;

    intel_context_get(ce);

    if unlikely!(intel_context_is_closed(ce) && !intel_engine_has_heartbeat(engine)) {
        intel_context_set_exiting(ce);
    }

    if unlikely!(!intel_context_is_schedulable(ce) || bad_request(rq)) {
        reset_active(rq, engine);
    }

    if IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
        lrc_check_regs(ce, engine, "before");
    }

    if (*ce).tag != 0 {
        // Use a fixed tag for OA and friends.
        GEM_BUG_ON!((*ce).tag <= BITS_PER_LONG);
        (*ce).lrc.ccid = (*ce).tag;
    } else if GRAPHICS_VER_FULL((*engine).i915) >= IP_VER(12, 55) {
        // We need distinct values, not strict matching.
        let tag = ffs(READ_ONCE!((*engine).context_tag));
        GEM_BUG_ON!(tag == 0 || tag >= BITS_PER_LONG);
        clear_bit(tag - 1, &mut (*engine).context_tag);
        (*ce).lrc.ccid = tag << (XEHP_SW_CTX_ID_SHIFT - 32);
        BUILD_BUG_ON!(BITS_PER_LONG > GEN12_MAX_CONTEXT_HW_ID);
    } else {
        // We need distinct values, not strict matching.
        let tag = __ffs((*engine).context_tag);
        GEM_BUG_ON!(tag >= BITS_PER_LONG);
        __clear_bit(tag, &mut (*engine).context_tag);
        (*ce).lrc.ccid = (1 + tag) << (GEN11_SW_CTX_ID_SHIFT - 32);
        BUILD_BUG_ON!(BITS_PER_LONG > GEN12_MAX_CONTEXT_HW_ID);
    }

    (*ce).lrc.ccid |= (*engine).execlists.ccid;

    __intel_gt_pm_get((*engine).gt);
    if (*engine).fw_domain != 0 && (*engine).fw_active == 0 {
        intel_uncore_forcewake_get((*engine).uncore, (*engine).fw_domain);
    }
    if (*engine).fw_domain != 0 {
        (*engine).fw_active += 1;
    }
    execlists_context_status_change(rq, INTEL_CONTEXT_SCHEDULE_IN);
    intel_engine_context_in(engine);

    CE_TRACE!(ce, "schedule-in, ccid:%x\n", (*ce).lrc.ccid);
    engine
}

// upstream: intel_execlists_submission.c execlists_schedule_in()
unsafe fn execlists_schedule_in(rq: *mut I915Request, idx: i32) {
    let ce = (*rq).context;
    let mut old: *mut IntelEngineCs;

    GEM_BUG_ON!(!intel_engine_pm_is_awake((*rq).engine));
    trace_i915_request_in(rq, idx);

    old = (*ce).inflight;
    if old.is_null() {
        old = __execlists_schedule_in(rq);
    }
    WRITE_ONCE!((*ce).inflight, ptr_inc(old));

    GEM_BUG_ON!(intel_context_inflight(ce) != (*rq).engine);
}

// upstream: intel_execlists_submission.c resubmit_virtual_request()
unsafe fn resubmit_virtual_request(rq: *mut I915Request, ve: *mut VirtualEngine) {
    let engine = (*rq).engine;

    spin_lock_irq(&mut (*(*engine).sched_engine).lock);

    clear_bit(I915_FENCE_FLAG_PQUEUE, &mut (*rq).fence.flags);
    WRITE_ONCE!((*rq).engine, &mut (*ve).base);
    ((*ve).base.submit_request)(rq);

    spin_unlock_irq(&mut (*(*engine).sched_engine).lock);
}

// upstream: intel_execlists_submission.c kick_siblings()
unsafe fn kick_siblings(rq: *mut I915Request, ce: *mut IntelContext) {
    let ve = container_of!(ce, VirtualEngine, context);
    let engine = (*rq).engine;

    // Before clearing ce->inflight, make sure the context has been removed
    // from b->signalers and the concurrent signal_irq_work iterator no longer
    // follows ce->signal_link; rq can transfer to a new sibling after this.
    if !list_empty(&(*ce).signals) {
        intel_context_remove_breadcrumbs(ce, (*engine).breadcrumbs);
    }

    // Move this virtual request if the current engine is too busy.
    if i915_request_in_priority_queue(rq) && (*rq).execution_mask != (*engine).mask {
        resubmit_virtual_request(rq, ve);
    }

    if READ_ONCE!((*ve).request) != core::ptr::null_mut() {
        tasklet_hi_schedule(&mut (*(*ve).base.sched_engine).tasklet);
    }
}

// upstream: intel_execlists_submission.c __execlists_schedule_out()
unsafe fn __execlists_schedule_out(rq: *mut I915Request, ce: *mut IntelContext) {
    let engine = (*rq).engine;
    let mut ccid: u32;

    // process_csb() is not under sched_engine->lock, so schedule-out can race
    // with schedule-in; refrain from non-trivial work here.
    CE_TRACE!(ce, "schedule-out, ccid:%x\n", (*ce).lrc.ccid);
    GEM_BUG_ON!((*ce).inflight != engine);

    if IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
        lrc_check_regs(ce, engine, "after");
    }

    // Re-enter power saving after the last request on this context completes.
    if intel_timeline_is_last((*ce).timeline, rq) && __i915_request_is_complete(rq) {
        intel_engine_add_retire(engine, (*ce).timeline);
    }

    ccid = (*ce).lrc.ccid;
    if GRAPHICS_VER_FULL((*engine).i915) >= IP_VER(12, 55) {
        ccid >>= XEHP_SW_CTX_ID_SHIFT - 32;
        ccid &= XEHP_MAX_CONTEXT_HW_ID;
    } else {
        ccid >>= GEN11_SW_CTX_ID_SHIFT - 32;
        ccid &= GEN12_MAX_CONTEXT_HW_ID;
    }

    if ccid < BITS_PER_LONG {
        GEM_BUG_ON!(ccid == 0);
        GEM_BUG_ON!(test_bit(ccid - 1, &(*engine).context_tag));
        __set_bit(ccid - 1, &mut (*engine).context_tag);
    }
    intel_engine_context_out(engine);
    execlists_context_status_change(rq, INTEL_CONTEXT_SCHEDULE_OUT);
    if (*engine).fw_domain != 0 {
        (*engine).fw_active -= 1;
        if (*engine).fw_active == 0 {
            intel_uncore_forcewake_put((*engine).uncore, (*engine).fw_domain);
        }
    }
    intel_gt_pm_put_async_untracked((*engine).gt);

    // Kick virtual siblings that may have been blocked on this active context.
    if (*ce).engine != engine {
        kick_siblings(rq, ce);
    }

    WRITE_ONCE!((*ce).inflight, core::ptr::null_mut());
    intel_context_put(ce);
}

// upstream: intel_execlists_submission.c execlists_schedule_out()
unsafe fn execlists_schedule_out(rq: *mut I915Request) {
    let ce = (*rq).context;

    trace_i915_request_out(rq);

    GEM_BUG_ON!((*ce).inflight.is_null());
    (*ce).inflight = ptr_dec((*ce).inflight);
    if !__intel_context_inflight_count((*ce).inflight) {
        __execlists_schedule_out(rq, ce);
    }

    i915_request_put(rq);
}

// upstream: intel_execlists_submission.c map_i915_prio_to_lrc_desc_prio()
unsafe fn map_i915_prio_to_lrc_desc_prio(prio: i32) -> u32 {
    if prio > I915_PRIORITY_NORMAL {
        GEN12_CTX_PRIORITY_HIGH
    } else if prio < I915_PRIORITY_NORMAL {
        GEN12_CTX_PRIORITY_LOW
    } else {
        GEN12_CTX_PRIORITY_NORMAL
    }
}

// upstream: intel_execlists_submission.c execlists_update_context()
unsafe fn execlists_update_context(rq: *mut I915Request) -> u64 {
    let ce = (*rq).context;
    let mut desc = (*ce).lrc.desc;
    let mut tail: u32;
    let prev: u32;

    if (*(*rq).engine).flags & I915_ENGINE_HAS_EU_PRIORITY != 0 {
        desc |= map_i915_prio_to_lrc_desc_prio(rq_prio(rq));
    }

    // WaIdleLiteRestore:bdw,skl
    // Never submit the same RING_TAIL twice: an empty ring can confuse HW.
    // Append NOOPs after normal requests so later lite-restores can advance
    // the tail. If not, force context reload. Returning to a preempted context
    // must also force reload, as HW may ignore rewinding TAIL to an earlier
    // request's end.
    GEM_BUG_ON!((*ce).lrc_reg_state[CTX_RING_TAIL] != (*(*rq).ring).tail);
    prev = (*(*rq).ring).tail;
    tail = intel_ring_set_tail((*rq).ring, (*rq).tail);
    if unlikely!(intel_ring_direction((*rq).ring, tail, prev) <= 0) {
        desc |= CTX_DESC_FORCE_RESTORE;
    }
    (*ce).lrc_reg_state[CTX_RING_TAIL] = tail;
    (*rq).tail = (*rq).wa_tail;

    // Ensure the context image is complete before submitting it to HW.
    wmb();

    (*ce).lrc.desc &= !CTX_DESC_FORCE_RESTORE;
    desc
}

// upstream: intel_execlists_submission.c write_desc()
unsafe fn write_desc(execlists: *mut IntelEngineExeclists, desc: u64, port: u32) {
    if !(*execlists).ctrl_reg.is_null() {
        writel(
            lower_32_bits(desc),
            (*execlists).submit_reg.add((port * 2) as usize),
        );
        writel(
            upper_32_bits(desc),
            (*execlists).submit_reg.add((port * 2 + 1) as usize),
        );
    } else {
        writel(upper_32_bits(desc), (*execlists).submit_reg);
        writel(lower_32_bits(desc), (*execlists).submit_reg);
    }
}

// upstream: intel_execlists_submission.c dump_port()
unsafe fn dump_port(
    buf: *mut c_char,
    buflen: i32,
    prefix: *const c_char,
    rq: *mut I915Request,
) -> *mut c_char {
    if rq.is_null() {
        return c"".as_ptr() as *mut c_char;
    }

    snprintf!(
        buf,
        buflen,
        "%sccid:%x %llx:%lld%s prio %d",
        prefix,
        (*(*rq).context).lrc.ccid,
        (*rq).fence.context,
        (*rq).fence.seqno,
        if __i915_request_is_complete(rq) {
            "!"
        } else if __i915_request_has_started(rq) {
            "*"
        } else {
            ""
        },
        rq_prio(rq)
    );
    buf
}

// upstream: intel_execlists_submission.c trace_ports()
unsafe fn trace_ports(
    execlists: *const IntelEngineExeclists,
    msg: *const c_char,
    ports: *const *mut I915Request,
) {
    let engine = container_of!(execlists, IntelEngineCs, execlists);
    let mut p0 = [0 as c_char; 40];
    let mut p1 = [0 as c_char; 40];

    if (*ports).is_null() {
        return;
    }

    ENGINE_TRACE!(
        engine,
        "%s { %s%s }\n",
        msg,
        dump_port(p0.as_mut_ptr(), p0.len() as i32, c"".as_ptr(), *ports),
        dump_port(
            p1.as_mut_ptr(),
            p1.len() as i32,
            c", ".as_ptr(),
            *ports.add(1)
        )
    );
}

// upstream: intel_execlists_submission.c reset_in_progress()
unsafe fn reset_in_progress(engine: *const IntelEngineCs) -> bool {
    unlikely!(!__tasklet_is_enabled(&(*(*engine).sched_engine).tasklet))
}

// upstream: intel_execlists_submission.c assert_pending_valid()
unsafe fn assert_pending_valid(execlists: *const IntelEngineExeclists, msg: *const c_char) -> bool {
    let engine = container_of!(execlists, IntelEngineCs, execlists);
    let mut port: *const *mut I915Request;
    let mut rq: *mut I915Request;
    let mut prev: *mut I915Request = core::ptr::null_mut();
    let mut ce: *mut IntelContext = core::ptr::null_mut();
    let mut ccid: u32 = !0;

    trace_ports(execlists, msg, (*execlists).pending.as_ptr());

    // We may be manipulating lists during reset.
    if reset_in_progress(engine) {
        return true;
    }

    if (*execlists).pending[0].is_null() {
        GEM_TRACE_ERR!("%s: Nothing pending for promotion!\n", (*engine).name);
        return false;
    }

    if !(*execlists).pending[execlists_num_ports(execlists) as usize].is_null() {
        GEM_TRACE_ERR!(
            "%s: Excess pending[%d] for promotion!\n",
            (*engine).name,
            execlists_num_ports(execlists)
        );
        return false;
    }

    port = (*execlists).pending.as_ptr();
    while !(*port).is_null() {
        let mut flags: c_ulong = 0;
        let mut ok = true;
        rq = *port;

        GEM_BUG_ON!(kref_read(&(*rq).fence.refcount) == 0);
        GEM_BUG_ON!(!i915_request_is_active(rq));

        if ce == (*rq).context {
            GEM_TRACE_ERR!(
                "%s: Dup context:%llx in pending[%zd]\n",
                (*engine).name,
                (*(*ce).timeline).fence_context,
                port.offset_from((*execlists).pending.as_ptr())
            );
            return false;
        }
        ce = (*rq).context;

        if ccid == (*ce).lrc.ccid {
            GEM_TRACE_ERR!(
                "%s: Dup ccid:%x context:%llx in pending[%zd]\n",
                (*engine).name,
                ccid,
                (*(*ce).timeline).fence_context,
                port.offset_from((*execlists).pending.as_ptr())
            );
            return false;
        }
        ccid = (*ce).lrc.ccid;

        // Sentinels flush current execution and must be the final request.
        if !prev.is_null()
            && i915_request_has_sentinel(prev)
            && READ_ONCE!((*prev).fence.error) == 0
        {
            GEM_TRACE_ERR!(
                "%s: context:%llx after sentinel in pending[%zd]\n",
                (*engine).name,
                (*(*ce).timeline).fence_context,
                port.offset_from((*execlists).pending.as_ptr())
            );
            return false;
        }
        prev = rq;

        // Virtual requests occupy the first slot to avoid sitting behind a hog.
        if (*rq).execution_mask != (*engine).mask && port != (*execlists).pending.as_ptr() {
            GEM_TRACE_ERR!(
                "%s: virtual engine:%llx not in prime position[%zd]\n",
                (*engine).name,
                (*(*ce).timeline).fence_context,
                port.offset_from((*execlists).pending.as_ptr())
            );
            return false;
        }

        // Prevent concurrent retire while checking pinned state.
        if !spin_trylock_irqsave(&mut (*rq).lock, &mut flags) {
            port = port.add(1);
            continue;
        }

        if __i915_request_is_complete(rq) {
            spin_unlock_irqrestore(&mut (*rq).lock, flags);
            port = port.add(1);
            continue;
        }

        if i915_active_is_idle(&(*ce).active) && !intel_context_is_barrier(ce) {
            GEM_TRACE_ERR!(
                "%s: Inactive context:%llx in pending[%zd]\n",
                (*engine).name,
                (*(*ce).timeline).fence_context,
                port.offset_from((*execlists).pending.as_ptr())
            );
            ok = false;
        } else if !i915_vma_is_pinned((*ce).state) {
            GEM_TRACE_ERR!(
                "%s: Unpinned context:%llx in pending[%zd]\n",
                (*engine).name,
                (*(*ce).timeline).fence_context,
                port.offset_from((*execlists).pending.as_ptr())
            );
            ok = false;
        } else if !i915_vma_is_pinned((*(*ce).ring).vma) {
            GEM_TRACE_ERR!(
                "%s: Unpinned ring:%llx in pending[%zd]\n",
                (*engine).name,
                (*(*ce).timeline).fence_context,
                port.offset_from((*execlists).pending.as_ptr())
            );
            ok = false;
        }

        spin_unlock_irqrestore(&mut (*rq).lock, flags);
        if !ok {
            return false;
        }
        port = port.add(1);
    }

    !ce.is_null()
}

// upstream: intel_execlists_submission.c execlists_submit_ports()
unsafe fn execlists_submit_ports(engine: *mut IntelEngineCs) {
    let execlists = &mut (*engine).execlists;
    let mut n: u32;

    GEM_BUG_ON!(!assert_pending_valid(execlists, c"submit".as_ptr()));

    // The request owns the runtime-PM reference until the device idles.
    GEM_BUG_ON!(!intel_engine_pm_is_awake(engine));

    // ELSQ is not cleared by HW, so always write the same number of entries.
    n = execlists_num_ports(execlists);
    while n != 0 {
        n -= 1;
        let rq = (*execlists).pending[n as usize];
        write_desc(
            execlists,
            if rq.is_null() {
                0
            } else {
                execlists_update_context(rq)
            },
            n,
        );
    }

    // Manually load the submit queue.
    if !(*execlists).ctrl_reg.is_null() {
        writel(EL_CTRL_LOAD, (*execlists).ctrl_reg);
    }
}

// upstream: intel_execlists_submission.c ctx_single_port_submission()
unsafe fn ctx_single_port_submission(ce: *const IntelContext) -> bool {
    IS_ENABLED!(CONFIG_DRM_I915_GVT) && intel_context_force_single_submission(ce)
}

// upstream: intel_execlists_submission.c can_merge_ctx()
unsafe fn can_merge_ctx(prev: *const IntelContext, next: *const IntelContext) -> bool {
    if prev != next {
        return false;
    }
    if ctx_single_port_submission(prev) {
        return false;
    }
    true
}

// upstream: intel_execlists_submission.c i915_request_flags()
unsafe fn i915_request_flags(rq: *const I915Request) -> c_ulong {
    READ_ONCE!((*rq).fence.flags)
}

// upstream: intel_execlists_submission.c can_merge_rq()
unsafe fn can_merge_rq(prev: *const I915Request, next: *const I915Request) -> bool {
    GEM_BUG_ON!(prev == next);
    GEM_BUG_ON!(!assert_priority_queue(prev, next));

    // Completed requests are not submitted, so treat them as merged.
    if __i915_request_is_complete(next) {
        return true;
    }

    if unlikely!(
        (i915_request_flags(prev) | i915_request_flags(next))
            & (BIT!(I915_FENCE_FLAG_NOPREEMPT) | BIT!(I915_FENCE_FLAG_SENTINEL))
            != 0
    ) {
        return false;
    }

    if !can_merge_ctx((*prev).context, (*next).context) {
        return false;
    }

    GEM_BUG_ON!(i915_seqno_passed((*prev).fence.seqno, (*next).fence.seqno));
    true
}

// upstream: intel_execlists_submission.c virtual_matches()
unsafe fn virtual_matches(
    ve: *const VirtualEngine,
    rq: *const I915Request,
    engine: *const IntelEngineCs,
) -> bool {
    let inflight: *mut IntelEngineCs;

    if rq.is_null() {
        return false;
    }
    if (*rq).execution_mask & (*engine).mask == 0 {
        // Peeked too soon.
        return false;
    }

    // Do not overwrite the context image until HW completed saving it.
    inflight = intel_context_inflight(&(*ve).context);
    if !inflight.is_null() && inflight != engine {
        return false;
    }
    true
}

// upstream: intel_execlists_submission.c first_virtual_engine()
unsafe fn first_virtual_engine(engine: *mut IntelEngineCs) -> *mut VirtualEngine {
    let el = &mut (*engine).execlists;
    let mut rb = rb_first_cached(&(*el).r#virtual);

    while !rb.is_null() {
        let ve = rb_entry!(rb, VirtualEngine, nodes[(*engine).id].rb);
        let rq = READ_ONCE!((*ve).request);

        // Lazily clean up after another engine handled the request.
        if rq.is_null() || !virtual_matches(ve, rq, engine) {
            rb_erase_cached(rb, &mut (*el).r#virtual);
            RB_CLEAR_NODE(rb);
            rb = rb_first_cached(&(*el).r#virtual);
            continue;
        }

        return ve;
    }
    core::ptr::null_mut()
}

// upstream: intel_execlists_submission.c virtual_xfer_context()
unsafe fn virtual_xfer_context(ve: *mut VirtualEngine, engine: *mut IntelEngineCs) {
    let mut n: u32;

    if likely!(engine == *(*ve).siblings) {
        return;
    }

    GEM_BUG_ON!(!READ_ONCE!((*ve).context.inflight).is_null());
    if !intel_engine_has_relative_mmio(engine) {
        lrc_update_offsets(&mut (*ve).context, engine);
    }

    // Promote the bound engine to the head for future execution; this tasklet
    // is kicked first so the chosen register bindings are preferentially reused.
    n = 1;
    while n < (*ve).num_siblings {
        if *(*ve).siblings.add(n as usize) == engine {
            swap(&mut *(*ve).siblings.add(n as usize), &mut *(*ve).siblings);
            break;
        }
        n += 1;
    }
}

// upstream: intel_execlists_submission.c defer_request()
unsafe fn defer_request(mut rq: *mut I915Request, pl: *mut ListHead) {
    let mut list = ListHead {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
    };
    INIT_LIST_HEAD!(&mut list);

    // Move the interrupted request to the end of its round-robin priority
    // list, then move behind it all in-flight requests waiting for it.
    loop {
        let mut p: *mut I915Dependency;

        GEM_BUG_ON!(i915_request_is_active(rq));
        list_move_tail(&mut (*rq).sched.link, pl);

        for_each_waiter!(p, rq, {
            let w = container_of!((*p).waiter, I915Request, sched);

            if (*p).flags & I915_DEPENDENCY_WEAK != 0 {
                continue;
            }
            // Leave semaphores spinning on other engines.
            if (*w).engine != (*rq).engine {
                continue;
            }
            GEM_BUG_ON!(
                i915_request_has_initial_breadcrumb(w)
                    && __i915_request_has_started(w)
                    && !__i915_request_is_complete(rq)
            );
            if !i915_request_is_ready(w) {
                continue;
            }
            if rq_prio(w) < rq_prio(rq) {
                continue;
            }

            GEM_BUG_ON!(rq_prio(w) > rq_prio(rq));
            GEM_BUG_ON!(i915_request_is_active(w));
            list_move_tail(&mut (*w).sched.link, &mut list);
        });

        rq = list_first_entry_or_null!(&mut list, I915Request, sched.link);
        if rq.is_null() {
            break;
        }
    }
}

// upstream: intel_execlists_submission.c defer_active()
unsafe fn defer_active(engine: *mut IntelEngineCs) {
    let rq = __unwind_incomplete_requests(engine);
    if rq.is_null() {
        return;
    }
    defer_request(
        rq,
        i915_sched_lookup_priolist((*engine).sched_engine, rq_prio(rq)),
    );
}

// upstream: intel_execlists_submission.c timeslice_yield()
unsafe fn timeslice_yield(el: *const IntelEngineExeclists, rq: *const I915Request) -> bool {
    // Once a semaphore miss occurs, treat this context as a hog for the rest
    // of its timeslice: CSB reports only the first miss and cannot tell us if
    // the semaphore later signaled or became blocked on another semaphore.
    (*(*rq).context).lrc.ccid == READ_ONCE!((*el).yield)
}

// upstream: intel_execlists_submission.c needs_timeslice()
unsafe fn needs_timeslice(engine: *const IntelEngineCs, rq: *const I915Request) -> bool {
    if !intel_engine_has_timeslices(engine) {
        return false;
    }

    // If inactive or about to switch, wait for the next event.
    if rq.is_null() || __i915_request_is_complete(rq) {
        return false;
    }
    // Start a timeslice only after the ACK.
    if !(*engine).execlists.pending[0].is_null() {
        return false;
    }
    // ELSP[1] occupied: check whether slicing is worthwhile.
    if !list_is_last_rcu(&(*rq).sched.link, &(*(*engine).sched_engine).requests) {
        ENGINE_TRACE!(engine, "timeslice required for second inflight context\n");
        return true;
    }
    if !i915_sched_engine_is_empty((*engine).sched_engine) {
        ENGINE_TRACE!(engine, "timeslice required for queue\n");
        return true;
    }
    if !RB_EMPTY_ROOT(&(*engine).execlists.r#virtual.rb_root) {
        ENGINE_TRACE!(engine, "timeslice required for virtual\n");
        return true;
    }
    false
}

// upstream: intel_execlists_submission.c timeslice_expired()
unsafe fn timeslice_expired(engine: *mut IntelEngineCs, rq: *const I915Request) -> bool {
    let el = &(*engine).execlists;

    if i915_request_has_nopreempt(rq) && __i915_request_has_started(rq) {
        return false;
    }
    if !needs_timeslice(engine, rq) {
        return false;
    }
    timer_expired(&el.timer) || timeslice_yield(el, rq)
}

// upstream: intel_execlists_submission.c timeslice()
unsafe fn timeslice(engine: *const IntelEngineCs) -> c_ulong {
    READ_ONCE!((*engine).props.timeslice_duration_ms)
}

// upstream: intel_execlists_submission.c start_timeslice()
unsafe fn start_timeslice(engine: *mut IntelEngineCs) {
    let el = &mut (*engine).execlists;
    let mut duration: c_ulong = 0;

    // Disable the timer when there is nothing to switch to.
    if needs_timeslice(engine, *el.active) {
        // Avoid continually extending an active timeslice.
        if timer_active(&el.timer) {
            // A newly submitted ELSP may inherit an already-consumed slice.
            if !timer_pending(&el.timer) {
                tasklet_hi_schedule(&mut (*(*engine).sched_engine).tasklet);
            }
            return;
        }
        duration = timeslice(engine);
    }
    set_timer_ms(&mut el.timer, duration);
}

// upstream: intel_execlists_submission.c record_preemption()
unsafe fn record_preemption(execlists: *mut IntelEngineExeclists) {
    I915_SELFTEST_ONLY!((*execlists).preempt_hang.count += 1);
}

// upstream: intel_execlists_submission.c active_preempt_timeout()
unsafe fn active_preempt_timeout(engine: *mut IntelEngineCs, rq: *const I915Request) -> c_ulong {
    if rq.is_null() {
        return 0;
    }

    // Only permit forcing a reset for the currently active context.
    (*engine).execlists.preempt_target = rq as *mut I915Request;

    // Force fast reset for terminated contexts (ignore sysfs).
    if unlikely!(intel_context_is_banned((*rq).context) || bad_request(rq)) {
        return INTEL_CONTEXT_BANNED_PREEMPT_TIMEOUT_MS;
    }
    READ_ONCE!((*engine).props.preempt_timeout_ms)
}

// upstream: intel_execlists_submission.c set_preempt_timeout()
unsafe fn set_preempt_timeout(engine: *mut IntelEngineCs, rq: *const I915Request) {
    if !intel_engine_has_preempt_reset(engine) {
        return;
    }
    set_timer_ms(
        &mut (*engine).execlists.preempt,
        active_preempt_timeout(engine, rq),
    );
}

// upstream: intel_execlists_submission.c completed()
unsafe fn completed(rq: *const I915Request) -> bool {
    if i915_request_has_sentinel(rq) {
        return false;
    }
    __i915_request_is_complete(rq)
}

// upstream: intel_execlists_submission.c execlists_dequeue()
unsafe fn execlists_dequeue(engine: *mut IntelEngineCs) {
    let execlists = &mut (*engine).execlists;
    let sched_engine = (*engine).sched_engine;
    let mut port = execlists.pending.as_mut_ptr();
    let last_port = port.add(execlists.port_mask as usize);
    let mut last: *mut I915Request;
    let mut active = execlists.active;
    let mut ve: *mut VirtualEngine;
    let mut rb: *mut RbNode;
    let mut submit = false;

    // HW submission uses two ports. Each has a (RING_START, RING_HEAD,
    // RING_TAIL) tuple. START is static and unique to a context; HEAD is
    // maintained by the CS; TAIL specifies the new execution end. Requests
    // are ordered and consecutive requests from one context are adjacent in
    // its ring, so we can point a port at the end of a consecutive sequence.
    spin_lock(&mut (*sched_engine).lock);

    // If the queue outranks the active context, submit afresh and then retry
    // preemption after the ACK.
    loop {
        last = *active;
        if last.is_null() || !completed(last) {
            break;
        }
        active = active.add(1);
    }

    if !last.is_null() {
        if need_preempt(engine, last) {
            ENGINE_TRACE!(
                engine,
                "preempting last=%llx:%lld, prio=%d, hint=%d\n",
                (*last).fence.context,
                (*last).fence.seqno,
                (*last).sched.attr.priority,
                (*sched_engine).queue_priority_hint
            );
            record_preemption(execlists);

            // Keep RING_HEAD from passing the breadcrumb while unwinding.
            ring_set_paused(engine, 1);

            // The GPU is still running: some unwound requests may complete
            // before preemption finishes.
            __unwind_incomplete_requests(engine);
            last = core::ptr::null_mut();
        } else if timeslice_expired(engine, last) {
            ENGINE_TRACE!(
                engine,
                "expired:%s last=%llx:%lld, prio=%d, hint=%d, yield?=%s\n",
                str_yes_no(timer_expired(&execlists.timer)),
                (*last).fence.context,
                (*last).fence.seqno,
                rq_prio(last),
                (*sched_engine).queue_priority_hint,
                str_yes_no(timeslice_yield(execlists, last))
            );

            // Consume the slice and ensure a fresh one begins. If dependency
            // order produces the same ports, submission is skipped; leaving an
            // expired timer armed would continually reschedule this tasklet.
            cancel_timer(&mut execlists.timer);
            ring_set_paused(engine, 1);
            defer_active(engine);

            // Rewinding and continuing the same context preserves monotonic
            // TAIL; lite restore is sufficient. Switching contexts also
            // leaves TAIL unrewound, so normal save/restore suffices.
            last = core::ptr::null_mut();
        } else {
            // An already-pending second request can wait for the next CS event.
            if !*active.add(1).is_null() {
                spin_unlock(&mut (*sched_engine).lock);
                return;
            }
        }
    }

    // Virtual requests take precedence.
    'virtuals: loop {
        ve = first_virtual_engine(engine);
        if ve.is_null() {
            break;
        }

        let virtual_sched = (*ve).base.sched_engine;
        spin_lock(&mut (*virtual_sched).lock);

        let rq = (*ve).request;
        if unlikely!(!virtual_matches(ve, rq, engine)) {
            // Lost the race to a sibling; drop the lock and look again.
            spin_unlock(&mut (*virtual_sched).lock);
            continue 'virtuals;
        }

        GEM_BUG_ON!((*rq).engine != &mut (*ve).base);
        GEM_BUG_ON!((*rq).context != &mut (*ve).context);

        if unlikely!(rq_prio(rq) < queue_prio(sched_engine)) {
            spin_unlock(&mut (*virtual_sched).lock);
            break 'virtuals;
        }

        if !last.is_null() && !can_merge_rq(last, rq) {
            spin_unlock(&mut (*virtual_sched).lock);
            spin_unlock(&mut (*sched_engine).lock);
            return; /* Leave this for another sibling. */
        }

        ENGINE_TRACE!(
            engine,
            "virtual rq=%llx:%lld%s, new engine? %s\n",
            (*rq).fence.context,
            (*rq).fence.seqno,
            if __i915_request_is_complete(rq) {
                "!"
            } else if __i915_request_has_started(rq) {
                "*"
            } else {
                ""
            },
            str_yes_no(engine != *(*ve).siblings)
        );

        WRITE_ONCE!((*ve).request, core::ptr::null_mut());
        WRITE_ONCE!((*virtual_sched).queue_priority_hint, INT_MIN);

        rb = &mut (*ve).nodes[(*engine).id as usize].rb;
        rb_erase_cached(rb, &mut execlists.r#virtual);
        RB_CLEAR_NODE(rb);

        GEM_BUG_ON!((*rq).execution_mask & (*engine).mask == 0);
        WRITE_ONCE!((*rq).engine, engine);

        if __i915_request_submit(rq) {
            // Only a real submission may change sibling bindings: this avoids
            // touching an idle context used by virtual_context_enter/exit.
            virtual_xfer_context(ve, engine);
            GEM_BUG_ON!(*(*ve).siblings != engine);
            submit = true;
            last = rq;
        }

        i915_request_put(rq);
        spin_unlock(&mut (*virtual_sched).lock);

        // Completed preempt-to-busy requests are ignored; keep scanning until
        // no more virtual requests are relevant or normal queue priority wins.
        if submit {
            break 'virtuals;
        }
    }

    'queue: loop {
        rb = rb_first_cached(&(*sched_engine).queue);
        if rb.is_null() {
            break 'queue;
        }
        let p = to_priolist(rb);
        let mut rq: *mut I915Request;
        let mut rn: *mut I915Request;

        priolist_for_each_request_consume!(rq, rn, p, {
            let mut merge = true;

            // Merge only requests sharing the same context/ring and without
            // exceptions such as GVT's single-port mode.
            if !last.is_null() && !can_merge_rq(last, rq) {
                if port == last_port {
                    break 'queue;
                }
                if (*last).context == (*rq).context {
                    break 'queue;
                }
                if i915_request_has_sentinel(last) {
                    break 'queue;
                }
                // Keep virtual requests out of secondary ports for migration.
                if (*rq).execution_mask != (*engine).mask {
                    break 'queue;
                }
                if ctx_single_port_submission((*last).context)
                    || ctx_single_port_submission((*rq).context)
                {
                    break 'queue;
                }
                merge = false;
            }

            if __i915_request_submit(rq) {
                if !merge {
                    *port = i915_request_get(last);
                    port = port.add(1);
                    last = core::ptr::null_mut();
                }

                GEM_BUG_ON!(!last.is_null() && !can_merge_ctx((*last).context, (*rq).context));
                GEM_BUG_ON!(
                    !last.is_null() && i915_seqno_passed((*last).fence.seqno, (*rq).fence.seqno)
                );

                submit = true;
                last = rq;
            }
        });

        rb_erase_cached(&mut (*p).node, &mut (*sched_engine).queue);
        i915_priolist_free(p);
    }

    *port = i915_request_get(last);
    port = port.add(1);

    // Choose the first priority hole as the queue hint; if every submission
    // slot is occupied, use the lowest executing request's priority.
    (*sched_engine).queue_priority_hint = queue_prio(sched_engine);
    i915_sched_engine_reset_on_empty(sched_engine);
    spin_unlock(&mut (*sched_engine).lock);

    // Skip HW poking if timeslicing/dependency order yielded identical ports.
    if submit
        && memcmp(
            active,
            execlists.pending.as_ptr(),
            port.offset_from(execlists.pending.as_mut_ptr()) as usize
                * size_of::<*mut I915Request>(),
        ) != 0
    {
        *port = core::ptr::null_mut();
        while port != execlists.pending.as_mut_ptr() {
            port = port.sub(1);
            execlists_schedule_in(
                *port,
                port.offset_from(execlists.pending.as_mut_ptr()) as i32,
            );
        }

        WRITE_ONCE!(execlists.yield, -1);
        set_preempt_timeout(engine, *active);
        execlists_submit_ports(engine);
    } else {
        ring_set_paused(engine, 0);
        while port != execlists.pending.as_mut_ptr() {
            port = port.sub(1);
            i915_request_put(*port);
        }
        *execlists.pending.as_mut_ptr() = core::ptr::null_mut();
    }
}

// upstream: intel_execlists_submission.c execlists_dequeue_irq()
unsafe fn execlists_dequeue_irq(engine: *mut IntelEngineCs) {
    local_irq_disable(); /* Suspend interrupts across request submission. */
    execlists_dequeue(engine);
    local_irq_enable(); /* Flush irq_work (for example breadcrumb enabling). */
}

// upstream: intel_execlists_submission.c clear_ports()
unsafe fn clear_ports(ports: *mut *mut I915Request, count: i32) {
    memset_p(
        ports as *mut *mut c_void,
        core::ptr::null_mut(),
        count as usize,
    );
}

// upstream: intel_execlists_submission.c copy_ports()
unsafe fn copy_ports(dst: *mut *mut I915Request, src: *mut *mut I915Request, mut count: i32) {
    // A memcpy_p() would be very useful here!
    while count != 0 {
        WRITE_ONCE!(*dst, *src); /* Avoid write tearing. */
        dst = dst.add(1);
        src = src.add(1);
        count -= 1;
    }
}

// upstream: intel_execlists_submission.c cancel_port_requests()
unsafe fn cancel_port_requests(
    execlists: *mut IntelEngineExeclists,
    mut inactive: *mut *mut I915Request,
) -> *mut *mut I915Request {
    let mut port: *mut *mut I915Request;

    port = (*execlists).pending.as_mut_ptr();
    while !(*port).is_null() {
        *inactive = *port;
        inactive = inactive.add(1);
        port = port.add(1);
    }
    clear_ports(
        (*execlists).pending.as_mut_ptr(),
        (*execlists).pending.len() as i32,
    );

    // Mark the end of active before overwriting *active.
    port = xchg(&mut (*execlists).active, (*execlists).pending.as_mut_ptr());
    while !(*port).is_null() {
        *inactive = *port;
        inactive = inactive.add(1);
        port = port.add(1);
    }
    clear_ports(
        (*execlists).inflight.as_mut_ptr(),
        (*execlists).inflight.len() as i32,
    );

    smp_wmb!(); /* Complete the seqlock for execlists_active(). */
    WRITE_ONCE!((*execlists).active, (*execlists).inflight.as_mut_ptr());

    // With all outstanding process_csb() cancelled, stop their timers.
    GEM_BUG_ON!(!(*execlists).pending[0].is_null());
    cancel_timer(&mut (*execlists).timer);
    cancel_timer(&mut (*execlists).preempt);

    inactive
}

// upstream: intel_execlists_submission.c __gen12_csb_parse()
unsafe fn __gen12_csb_parse(
    ctx_to_valid: bool,
    ctx_away_valid: bool,
    new_queue: bool,
    switch_detail: u8,
) -> bool {
    // Switch detail is not guaranteed to be 5 on preemption. The following
    // cases cover relevant preemptions, including WAIT and lite-restore.
    // Preempt-to-idle via CTRL would need extra handling and is unsupported.
    if !ctx_away_valid || new_queue {
        GEM_BUG_ON!(!ctx_to_valid);
        return true;
    }

    // Detail 5 is handled above; unsuccessful waits do not switch because we
    // always use polling mode.
    GEM_BUG_ON!(switch_detail != 0);
    false
}

// upstream: intel_execlists_submission.c xehp_csb_parse()
unsafe fn xehp_csb_parse(csb: u64) -> bool {
    __gen12_csb_parse(
        XEHP_CSB_CTX_VALID(lower_32_bits(csb)), // context to
        XEHP_CSB_CTX_VALID(upper_32_bits(csb)), // context away
        upper_32_bits(csb) & XEHP_CTX_STATUS_SWITCHED_TO_NEW_QUEUE != 0,
        GEN12_CTX_SWITCH_DETAIL(lower_32_bits(csb)),
    )
}

// upstream: intel_execlists_submission.c gen12_csb_parse()
unsafe fn gen12_csb_parse(csb: u64) -> bool {
    __gen12_csb_parse(
        GEN12_CSB_CTX_VALID(lower_32_bits(csb)), // context to
        GEN12_CSB_CTX_VALID(upper_32_bits(csb)), // context away
        lower_32_bits(csb) & GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE != 0,
        GEN12_CTX_SWITCH_DETAIL(upper_32_bits(csb)),
    )
}

// upstream: intel_execlists_submission.c gen8_csb_parse()
unsafe fn gen8_csb_parse(csb: u64) -> bool {
    csb & (GEN8_CTX_STATUS_IDLE_ACTIVE | GEN8_CTX_STATUS_PREEMPTED) != 0
}

// upstream: intel_execlists_submission.c wa_csb_read()
unsafe fn wa_csb_read(engine: *const IntelEngineCs, csb: *const u64) -> u64 {
    let mut entry: u64;

    // A HWSP read can detect a stale entry. Since the HWSP write is broken,
    // do not trust the HW at all: the MMIO entry can also be unordered, so
    // prefer this self-checking path and return MMIO as a final fallback.
    // tgl,dg1:HSDES#22011327657.
    preempt_disable();
    if wait_for_atomic_us!((entry = READ_ONCE!(*csb)) != !0u64, 10) {
        let mut idx = csb.offset_from((*engine).execlists.csb_status) as i32;
        let mut status = GEN8_EXECLISTS_STATUS_BUF;
        if idx >= 6 {
            status = GEN11_EXECLISTS_STATUS_BUF2;
            idx -= 6;
        }
        status += size_of::<u64>() as i32 * idx;
        entry = intel_uncore_read64((*engine).uncore, MMIO((*engine).mmio_base + status as u32));
    }
    preempt_enable();
    entry
}

// upstream: intel_execlists_submission.c csb_read()
unsafe fn csb_read(engine: *const IntelEngineCs, csb: *mut u64) -> u64 {
    let mut entry = READ_ONCE!(*csb);

    // GPU CSB entries are not always globally observed before the pointer;
    // an updated tail can otherwise expose stale entries and false-hang us.
    // icl:HSDES#1806554093; tgl:HSDES#22011248461.
    if unlikely!(entry == !0u64) {
        entry = wa_csb_read(engine, csb);
    }

    // Consume the entry so that future reuse is detectable.
    WRITE_ONCE!(*csb, !0u64);
    // ELSP is an implicit wmb() before HW wraps and overwrites the CSB.
    entry
}

// upstream: intel_execlists_submission.c new_timeslice()
unsafe fn new_timeslice(el: *mut IntelEngineExeclists) {
    // Cancellation causes start_timeslice() to begin afresh.
    cancel_timer(&mut (*el).timer);
}

// upstream: intel_execlists_submission.c process_csb()
unsafe fn process_csb(
    engine: *mut IntelEngineCs,
    mut inactive: *mut *mut I915Request,
) -> *mut *mut I915Request {
    let execlists = &mut (*engine).execlists;
    let buf = execlists.csb_status;
    let num_entries = execlists.csb_size;
    let mut prev: *mut *mut I915Request;
    let mut head: u8;
    let tail: u8;

    // CSB tracking is exclusive in the tasklet, or during serialized reset.
    GEM_BUG_ON!(
        !tasklet_is_locked(&mut (*(*engine).sched_engine).tasklet) && !reset_in_progress(engine)
    );

    // Read only the low byte of the MMIO write pointer: the following bits
    // are zero, so this matches the HWSP representation.
    head = execlists.csb_head;
    tail = READ_ONCE!(*execlists.csb_write) as u8;
    if unlikely!(head == tail) {
        return inactive;
    }

    // Consume all HW events; impossible sequence means tracking was lost and
    // the engine must reset rather than continue with corrupted state.
    execlists.csb_head = tail;
    ENGINE_TRACE!(engine, "cs-irq head=%d, tail=%d\n", head, tail);

    // Pair with HW ordering: finish reading the pointer before CSB entries.
    rmb();

    // Remember the request that was last running while the timer was active.
    prev = inactive;
    *prev = core::ptr::null_mut();

    loop {
        let promote: bool;
        let csb: u64;

        head = head.wrapping_add(1);
        if head == num_entries {
            head = 0;
        }

        // Request refs in execlist_port[] are our only ownership. This is
        // softirq context: no mutex or sleeping, and a breadcrumb has completed
        // before a context-switch event. Pointers under requests can still be
        // freed concurrently, so bookkeeping must stay in port[] and unguarded
        // dereferences (including notifier accesses) must be avoided.
        csb = csb_read(engine, buf.add(head as usize));
        ENGINE_TRACE!(
            engine,
            "csb[%d]: status=0x%08x:0x%08x\n",
            head,
            upper_32_bits(csb),
            lower_32_bits(csb)
        );

        if GRAPHICS_VER_FULL((*engine).i915) >= IP_VER(12, 55) {
            promote = xehp_csb_parse(csb);
        } else if GRAPHICS_VER((*engine).i915) >= 12 {
            promote = gen12_csb_parse(csb);
        } else {
            promote = gen8_csb_parse(csb);
        }

        if promote {
            let mut old = execlists.active;

            if GEM_WARN_ON!((*execlists).pending[0].is_null()) {
                execlists.error_interrupt |= ERROR_CSB;
                break;
            }

            ring_set_paused(engine, 0);

            // Point active at the new ELSP before overwriting it.
            WRITE_ONCE!(execlists.active, execlists.pending.as_mut_ptr());
            smp_wmb!(); /* Notify execlists_active(). */

            // Cancel old inflight and prepare for the context switch.
            trace_ports(execlists, c"preempted".as_ptr(), old);
            while !(*old).is_null() {
                *inactive = *old;
                inactive = inactive.add(1);
                old = old.add(1);
            }

            // Switch pending to inflight.
            GEM_BUG_ON!(!assert_pending_valid(execlists, c"promote".as_ptr()));
            copy_ports(
                execlists.inflight.as_mut_ptr(),
                execlists.pending.as_mut_ptr(),
                execlists_num_ports(execlists) as i32,
            );
            smp_wmb!(); /* Complete the seqlock. */
            WRITE_ONCE!(execlists.active, execlists.inflight.as_mut_ptr());

            // XXX Magic delay for tgl.
            ENGINE_POSTING_READ!(engine, RING_CONTEXT_STATUS_PTR);
            WRITE_ONCE!(execlists.pending[0], core::ptr::null_mut());
        } else {
            if GEM_WARN_ON!((*execlists.active).is_null()) {
                execlists.error_interrupt |= ERROR_CSB;
                break;
            }

            // Port 0 completed; advance to port 1.
            trace_ports(execlists, c"completed".as_ptr(), execlists.active);

            // Rely on strong HW ordering: the coherent breadcrumb write is
            // visible before the user interrupt. Debug-trace the supposedly
            // impossible case where the context completes first.
            if GEM_SHOW_DEBUG!() && !__i915_request_is_complete(*execlists.active) {
                let rq = *execlists.active;
                let regs = (*(*rq).context).lrc_reg_state.as_ptr();

                ENGINE_TRACE!(engine, "context completed before request!\n");
                ENGINE_TRACE!(
                    engine,
                    "ring:{start:0x%08x, head:%04x, tail:%04x, ctl:%08x, mode:%08x}\n",
                    ENGINE_READ!(engine, RING_START),
                    ENGINE_READ!(engine, RING_HEAD) & HEAD_ADDR,
                    ENGINE_READ!(engine, RING_TAIL) & TAIL_ADDR,
                    ENGINE_READ!(engine, RING_CTL),
                    ENGINE_READ!(engine, RING_MI_MODE)
                );
                ENGINE_TRACE!(
                    engine,
                    "rq:{start:%08x, head:%04x, tail:%04x, seqno:%llx:%d, hwsp:%d}, ",
                    i915_ggtt_offset((*(*rq).ring).vma),
                    (*rq).head,
                    (*rq).tail,
                    (*rq).fence.context,
                    lower_32_bits((*rq).fence.seqno),
                    hwsp_seqno(rq)
                );
                ENGINE_TRACE!(
                    engine,
                    "ctx:{start:%08x, head:%04x, tail:%04x}, ",
                    *regs.add(CTX_RING_START),
                    *regs.add(CTX_RING_HEAD),
                    *regs.add(CTX_RING_TAIL)
                );
            }

            *inactive = *execlists.active;
            inactive = inactive.add(1);
            execlists.active = execlists.active.add(1);

            GEM_BUG_ON!(
                execlists.active.offset_from(execlists.inflight.as_ptr())
                    > execlists_num_ports(execlists) as isize
            );
        }

        if head == tail {
            break;
        }
    }

    // Gen11 can violate global observation ordering between CSB entries and
    // tail updates. Flush entries for the next update even on working HW.
    drm_clflush_virt_range(buf, num_entries as usize * size_of::<u64>());

    // Any event changes context flow and merits a fresh slice; reinstall only
    // after examining whether a new submission is needed.
    if *prev != *execlists.active {
        let mut prev_ce: *mut IntelContext = core::ptr::null_mut();
        let mut active_ce: *mut IntelContext = core::ptr::null_mut();

        // CPU runtime adjustment may undershoot HW time due to CS-event delay;
        // later HW updates correct it without over-reporting runtime.
        if !(*prev).is_null() {
            prev_ce = (**prev).context;
        }
        if !(*execlists.active).is_null() {
            active_ce = (**execlists.active).context;
        }
        if prev_ce != active_ce {
            if !prev_ce.is_null() {
                lrc_runtime_stop(prev_ce);
            }
            if !active_ce.is_null() {
                lrc_runtime_start(active_ce);
            }
        }
        new_timeslice(execlists);
    }

    inactive
}

// upstream: intel_execlists_submission.c post_process_csb()
unsafe fn post_process_csb(mut port: *mut *mut I915Request, last: *mut *mut I915Request) {
    while port != last {
        let rq = *port;
        port = port.add(1);
        execlists_schedule_out(rq);
    }
}

// upstream: intel_execlists_submission.c __execlists_hold()
unsafe fn __execlists_hold(mut rq: *mut I915Request) {
    let mut list = ListHead {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
    };
    INIT_LIST_HEAD!(&mut list);

    loop {
        let mut p: *mut I915Dependency;

        if i915_request_is_active(rq) {
            __i915_request_unsubmit(rq);
        }

        clear_bit(I915_FENCE_FLAG_PQUEUE, &mut (*rq).fence.flags);
        list_move_tail(
            &mut (*rq).sched.link,
            &mut (*(*(*rq).engine).sched_engine).hold,
        );
        i915_request_set_hold(rq);
        RQ_TRACE!(rq, "on hold\n");

        for_each_waiter!(p, rq, {
            let w = container_of!((*p).waiter, I915Request, sched);

            if (*p).flags & I915_DEPENDENCY_WEAK != 0 {
                continue;
            }
            // Leave semaphores spinning on other engines.
            if (*w).engine != (*rq).engine {
                continue;
            }
            if !i915_request_is_ready(w) || __i915_request_is_complete(w) {
                continue;
            }
            if i915_request_on_hold(w) {
                continue;
            }
            list_move_tail(&mut (*w).sched.link, &mut list);
        });

        rq = list_first_entry_or_null!(&mut list, I915Request, sched.link);
        if rq.is_null() {
            break;
        }
    }
}

// upstream: intel_execlists_submission.c execlists_hold()
unsafe fn execlists_hold(engine: *mut IntelEngineCs, mut rq: *mut I915Request) -> bool {
    if i915_request_on_hold(rq) {
        return false;
    }

    spin_lock_irq(&mut (*(*engine).sched_engine).lock);

    if __i915_request_is_complete(rq) {
        // Too late!
        rq = core::ptr::null_mut();
        spin_unlock_irq(&mut (*(*engine).sched_engine).lock);
        return false;
    }

    // Move this request and any already-submitted successors to hold, keeping
    // them from being resubmitted/completed before release.
    GEM_BUG_ON!(i915_request_on_hold(rq));
    GEM_BUG_ON!((*rq).engine != engine);
    __execlists_hold(rq);
    GEM_BUG_ON!(list_empty(&(*(*engine).sched_engine).hold));

    spin_unlock_irq(&mut (*(*engine).sched_engine).lock);
    !rq.is_null()
}

// upstream: intel_execlists_submission.c hold_request()
unsafe fn hold_request(rq: *const I915Request) -> bool {
    let mut p: *mut I915Dependency;
    let mut result = false;

    // Holding an ancestor requires holding this request too, otherwise it
    // could bypass that ancestor and execute first.
    rcu_read_lock();
    for_each_signaler!(p, rq, {
        let s = container_of!((*p).signaler, I915Request, sched);

        if (*s).engine != (*rq).engine {
            continue;
        }
        result = i915_request_on_hold(s);
        if result {
            break;
        }
    });
    rcu_read_unlock();
    result
}

// upstream: intel_execlists_submission.c __execlists_unhold()
unsafe fn __execlists_unhold(mut rq: *mut I915Request) {
    let mut list = ListHead {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
    };
    INIT_LIST_HEAD!(&mut list);

    loop {
        let mut p: *mut I915Dependency;

        RQ_TRACE!(rq, "hold release\n");
        GEM_BUG_ON!(!i915_request_on_hold(rq));
        GEM_BUG_ON!(!i915_sw_fence_signaled(&(*rq).submit));

        i915_request_clear_hold(rq);
        list_move_tail(
            &mut (*rq).sched.link,
            i915_sched_lookup_priolist((*(*rq).engine).sched_engine, rq_prio(rq)),
        );
        set_bit(I915_FENCE_FLAG_PQUEUE, &mut (*rq).fence.flags);

        // Release ready children on this engine as well.
        for_each_waiter!(p, rq, {
            let w = container_of!((*p).waiter, I915Request, sched);
            if (*p).flags & I915_DEPENDENCY_WEAK != 0 {
                continue;
            }
            if (*w).engine != (*rq).engine || !i915_request_on_hold(w) {
                continue;
            }
            // Do not release while another parent remains on hold.
            if hold_request(w) {
                continue;
            }
            list_move_tail(&mut (*w).sched.link, &mut list);
        });

        rq = list_first_entry_or_null!(&mut list, I915Request, sched.link);
        if rq.is_null() {
            break;
        }
    }
}

// upstream: intel_execlists_submission.c execlists_unhold()
unsafe fn execlists_unhold(engine: *mut IntelEngineCs, rq: *mut I915Request) {
    spin_lock_irq(&mut (*(*engine).sched_engine).lock);

    // Return this request and its suspended children/grandchildren to queues.
    __execlists_unhold(rq);

    if rq_prio(rq) > (*(*engine).sched_engine).queue_priority_hint {
        (*(*engine).sched_engine).queue_priority_hint = rq_prio(rq);
        tasklet_hi_schedule(&mut (*(*engine).sched_engine).tasklet);
    }

    spin_unlock_irq(&mut (*(*engine).sched_engine).lock);
}

#[repr(C)]
struct ExeclistsCapture {
    work: WorkStruct,
    rq: *mut I915Request,
    error: *mut I915GpuCoredump,
}

// upstream: intel_execlists_submission.c execlists_capture_work()
unsafe extern "C" fn execlists_capture_work(work: *mut WorkStruct) {
    let cap = container_of!(work, ExeclistsCapture, work);
    let gfp = __GFP_KSWAPD_RECLAIM | __GFP_RETRY_MAYFAIL | __GFP_NOWARN;
    let engine = (*cap).rq.as_ref().unwrap_unchecked().engine;
    let gt = (*(*cap).error).gt;
    let mut vma: *mut IntelEngineCaptureVma;

    // Compress all objects attached to the request (slow).
    vma = intel_engine_coredump_add_request((*gt).engine, (*cap).rq, gfp);
    if !vma.is_null() {
        let compress = i915_vma_capture_prepare(gt);
        intel_engine_coredump_add_vma((*gt).engine, vma, compress);
        i915_vma_capture_finish(gt, compress);
    }

    (*gt).simulated = (*(*gt).engine).simulated;
    (*(*cap).error).simulated = (*gt).simulated;

    // Publish the error state and notify the system.
    i915_error_state_store((*cap).error);
    i915_gpu_coredump_put((*cap).error);

    // Return this request and dependents to signaling.
    execlists_unhold(engine, (*cap).rq);
    i915_request_put((*cap).rq);
    kfree(cap as *mut c_void);
}

// upstream: intel_execlists_submission.c capture_regs()
unsafe fn capture_regs(engine: *mut IntelEngineCs) -> *mut ExeclistsCapture {
    let gfp = GFP_ATOMIC | __GFP_NOWARN;
    let mut cap: *mut ExeclistsCapture;

    cap = kmalloc_obj!(ExeclistsCapture, gfp);
    if cap.is_null() {
        return core::ptr::null_mut();
    }

    (*cap).error = i915_gpu_coredump_alloc((*engine).i915, gfp);
    if (*cap).error.is_null() {
        kfree(cap as *mut c_void);
        return core::ptr::null_mut();
    }

    (*(*cap).error).gt = intel_gt_coredump_alloc((*engine).gt, gfp, CORE_DUMP_FLAG_NONE);
    if (*(*cap).error).gt.is_null() {
        kfree((*cap).error as *mut c_void);
        kfree(cap as *mut c_void);
        return core::ptr::null_mut();
    }

    (*(*(*cap).error).gt).engine = intel_engine_coredump_alloc(engine, gfp, CORE_DUMP_FLAG_NONE);
    if (*(*(*cap).error).gt).engine.is_null() {
        kfree((*(*cap).error).gt as *mut c_void);
        kfree((*cap).error as *mut c_void);
        kfree(cap as *mut c_void);
        return core::ptr::null_mut();
    }

    (*(*(*(*cap).error).gt).engine).hung = true;
    cap
}

// upstream: intel_execlists_submission.c active_context()
unsafe fn active_context(engine: *mut IntelEngineCs, ccid: u32) -> *mut I915Request {
    let el = &(*engine).execlists;
    let mut port: *mut *mut I915Request;
    let mut rq: *mut I915Request;

    // Prefer process_csb() state; check pending in case an error interrupt
    // arrives before the first CS event is written.
    port = el.active;
    while !(*port).is_null() {
        rq = *port;
        if (*(*rq).context).lrc.ccid == ccid {
            ENGINE_TRACE!(
                engine,
                "ccid:%x found at active:%zd\n",
                ccid,
                port.offset_from(el.active)
            );
            return rq;
        }
        port = port.add(1);
    }

    port = el.pending.as_ptr() as *mut *mut I915Request;
    while !(*port).is_null() {
        rq = *port;
        if (*(*rq).context).lrc.ccid == ccid {
            ENGINE_TRACE!(
                engine,
                "ccid:%x found at pending:%zd\n",
                ccid,
                port.offset_from(el.pending.as_ptr())
            );
            return rq;
        }
        port = port.add(1);
    }

    ENGINE_TRACE!(engine, "ccid:%x not found\n", ccid);
    core::ptr::null_mut()
}

// upstream: intel_execlists_submission.c active_ccid()
unsafe fn active_ccid(engine: *mut IntelEngineCs) -> u32 {
    ENGINE_READ_FW!(engine, RING_EXECLIST_STATUS_HI)
}

// upstream: intel_execlists_submission.c execlists_capture()
unsafe fn execlists_capture(engine: *mut IntelEngineCs) {
    let i915 = (*engine).i915;
    let mut cap: *mut ExeclistsCapture;

    if !IS_ENABLED!(CONFIG_DRM_I915_CAPTURE_ERROR) {
        return;
    }

    // Capture engine state quickly in softirq before reset delays preemption.
    cap = capture_regs(engine);
    if cap.is_null() {
        return;
    }

    spin_lock_irq(&mut (*(*engine).sched_engine).lock);
    (*cap).rq = active_context(engine, active_ccid(engine));
    if !(*cap).rq.is_null() {
        (*cap).rq = active_request((*(*(*cap).rq).context).timeline, (*cap).rq);
        (*cap).rq = i915_request_get_rcu((*cap).rq);
    }
    spin_unlock_irq(&mut (*(*engine).sched_engine).lock);
    if (*cap).rq.is_null() {
        i915_gpu_coredump_put((*cap).error);
        kfree(cap as *mut c_void);
        return;
    }

    // Remove and own this request while a worker slowly compresses the pages
    // requested for debugging. It will later return the request for signaling.
    // Removing it also prevents __unwind_incomplete_requests() during reset
    // from replaying it. It might complete before reset arbitration succeeds;
    // assume it is guilty of being non-preemptible long enough to force reset.
    if !execlists_hold(engine, (*cap).rq) {
        i915_request_put((*cap).rq);
        i915_gpu_coredump_put((*cap).error);
        kfree(cap as *mut c_void);
        return;
    }

    INIT_WORK_C(&mut (*cap).work, execlists_capture_work);
    queue_work((*i915).unordered_wq, &mut (*cap).work);
}

// upstream: intel_execlists_submission.c execlists_reset()
unsafe fn execlists_reset(engine: *mut IntelEngineCs, msg: *const c_char) {
    let bit = I915_RESET_ENGINE + (*engine).id;
    let lock = &mut (*(*engine).gt).reset.flags;

    if !intel_has_reset_engine((*engine).gt) {
        return;
    }
    if test_and_set_bit(bit, lock) {
        return;
    }

    ENGINE_TRACE!(engine, "reset for %s\n", msg);
    // Disable without waiting for the current tasklet to complete.
    tasklet_disable_nosync(&mut (*(*engine).sched_engine).tasklet);

    ring_set_paused(engine, 1); /* Freeze the current request in place. */
    execlists_capture(engine);
    intel_engine_reset(engine, msg);

    tasklet_enable(&mut (*(*engine).sched_engine).tasklet);
    clear_and_wake_up_bit(bit, lock);
}

// upstream: intel_execlists_submission.c preempt_timeout()
unsafe fn preempt_timeout(engine: *const IntelEngineCs) -> bool {
    let t = &(*engine).execlists.preempt;

    if CONFIG_DRM_I915_PREEMPT_TIMEOUT == 0 {
        return false;
    }
    if !timer_expired(t) {
        return false;
    }
    !(*engine).execlists.pending[0].is_null()
}

// upstream: intel_execlists_submission.c execlists_submission_tasklet()
unsafe fn execlists_submission_tasklet(t: *mut TaskletStruct) {
    let sched_engine = from_tasklet!(t, I915SchedEngine, tasklet);
    let engine = (*sched_engine).private_data;
    let mut post = [core::ptr::null_mut::<I915Request>(); 2 * EXECLIST_MAX_PORTS];
    let mut inactive: *mut *mut I915Request;

    rcu_read_lock();
    inactive = process_csb(engine, post.as_mut_ptr());
    GEM_BUG_ON!(inactive.offset_from(post.as_mut_ptr()) > post.len() as isize);

    if unlikely!(preempt_timeout(engine)) {
        let rq = *(*engine).execlists.active;

        // If still on the same request after timeout, reset. If a CS event
        // switched contexts but the pending preemption event has not arrived,
        // restart the timeout for the new context to exit gracefully.
        cancel_timer(&mut (*engine).execlists.preempt);
        if rq == (*engine).execlists.preempt_target {
            (*engine).execlists.error_interrupt |= ERROR_PREEMPT;
        } else {
            set_timer_ms(
                &mut (*engine).execlists.preempt,
                active_preempt_timeout(engine, rq),
            );
        }
    }

    if unlikely!(READ_ONCE!((*engine).execlists.error_interrupt) != 0) {
        let msg: *const c_char;
        // Prioritize errors in the order most meaningful to users.
        if (*engine).execlists.error_interrupt & GENMASK!(15, 0) != 0 {
            msg = c"CS error".as_ptr(); /* Thrown by a user payload. */
        } else if (*engine).execlists.error_interrupt & ERROR_CSB != 0 {
            msg = c"invalid CSB event".as_ptr();
        } else if (*engine).execlists.error_interrupt & ERROR_PREEMPT != 0 {
            msg = c"preemption time out".as_ptr();
        } else {
            msg = c"internal error".as_ptr();
        }

        (*engine).execlists.error_interrupt = 0;
        execlists_reset(engine, msg);
    }

    if (*engine).execlists.pending[0].is_null() {
        execlists_dequeue_irq(engine);
        start_timeslice(engine);
    }

    post_process_csb(post.as_mut_ptr(), inactive);
    rcu_read_unlock();
}

// upstream: intel_execlists_submission.c execlists_irq_handler()
unsafe fn execlists_irq_handler(engine: *mut IntelEngineCs, iir: u16) {
    let mut tasklet = false;

    if unlikely!(iir & GT_CS_MASTER_ERROR_INTERRUPT != 0) {
        let eir = ENGINE_READ!(engine, RING_EIR) & GENMASK!(15, 0);
        ENGINE_TRACE!(engine, "CS error: %x\n", eir);

        // Disable this interrupt until reset completes.
        if likely!(eir != 0) {
            ENGINE_WRITE!(engine, RING_EMR, !0u32);
            ENGINE_WRITE!(engine, RING_EIR, eir);
            WRITE_ONCE!((*engine).execlists.error_interrupt, eir);
            tasklet = true;
        }
    }

    if iir & GT_WAIT_SEMAPHORE_INTERRUPT != 0 {
        WRITE_ONCE!(
            (*engine).execlists.yield,
            ENGINE_READ_FW!(engine, RING_EXECLIST_STATUS_HI)
        );
        ENGINE_TRACE!(engine, "semaphore yield: %08x\n", (*engine).execlists.yield);
        if timer_delete(&mut (*engine).execlists.timer) {
            tasklet = true;
        }
    }

    if iir & GT_CONTEXT_SWITCH_INTERRUPT != 0 {
        tasklet = true;
    }
    if iir & GT_RENDER_USER_INTERRUPT != 0 {
        intel_engine_signal_breadcrumbs(engine);
    }
    if tasklet {
        tasklet_hi_schedule(&mut (*(*engine).sched_engine).tasklet);
    }
}

// upstream: intel_execlists_submission.c __execlists_kick()
unsafe fn __execlists_kick(execlists: *mut IntelEngineExeclists) {
    let engine = container_of!(execlists, IntelEngineCs, execlists);
    // Interrupt coalescing and reset handling are delegated to the tasklet.
    tasklet_hi_schedule(&mut (*(*engine).sched_engine).tasklet);
}

// upstream: intel_execlists_submission.c execlists_timeslice()
unsafe extern "C" fn execlists_timeslice(timer: *mut TimerList) {
    let el = container_of!(timer, IntelEngineExeclists, timer);
    __execlists_kick(el);
}

// upstream: intel_execlists_submission.c execlists_preempt()
unsafe extern "C" fn execlists_preempt(timer: *mut TimerList) {
    let el = container_of!(timer, IntelEngineExeclists, preempt);
    __execlists_kick(el);
}

// upstream: intel_execlists_submission.c queue_request()
unsafe fn queue_request(engine: *mut IntelEngineCs, rq: *mut I915Request) {
    GEM_BUG_ON!(!list_empty(&(*rq).sched.link));
    list_add_tail(
        &mut (*rq).sched.link,
        i915_sched_lookup_priolist((*engine).sched_engine, rq_prio(rq)),
    );
    set_bit(I915_FENCE_FLAG_PQUEUE, &mut (*rq).fence.flags);
}

// upstream: intel_execlists_submission.c submit_queue()
unsafe fn submit_queue(engine: *mut IntelEngineCs, rq: *const I915Request) -> bool {
    let sched_engine = (*engine).sched_engine;
    if rq_prio(rq) <= (*sched_engine).queue_priority_hint {
        return false;
    }
    (*sched_engine).queue_priority_hint = rq_prio(rq);
    true
}

// upstream: intel_execlists_submission.c ancestor_on_hold()
unsafe fn ancestor_on_hold(engine: *const IntelEngineCs, rq: *const I915Request) -> bool {
    GEM_BUG_ON!(i915_request_on_hold(rq));
    !list_empty(&(*(*engine).sched_engine).hold) && hold_request(rq)
}

// upstream: intel_execlists_submission.c execlists_submit_request()
unsafe fn execlists_submit_request(request: *mut I915Request) {
    let engine = (*request).engine;
    let mut flags: c_ulong = 0;

    // Foreign fences may call this in IRQ context.
    spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags);

    if unlikely!(ancestor_on_hold(engine, request)) {
        RQ_TRACE!(request, "ancestor on hold\n");
        list_add_tail(
            &mut (*request).sched.link,
            &mut (*(*engine).sched_engine).hold,
        );
        i915_request_set_hold(request);
    } else {
        queue_request(engine, request);
        GEM_BUG_ON!(i915_sched_engine_is_empty((*engine).sched_engine));
        GEM_BUG_ON!(list_empty(&(*request).sched.link));
        if submit_queue(engine, request) {
            __execlists_kick(&mut (*engine).execlists);
        }
    }

    spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags);
}

// upstream: intel_execlists_submission.c __execlists_context_pre_pin()
unsafe fn __execlists_context_pre_pin(
    ce: *mut IntelContext,
    engine: *mut IntelEngineCs,
    ww: *mut I915GemWwCtx,
    vaddr: *mut *mut c_void,
) -> i32 {
    let mut err = lrc_pre_pin(ce, engine, ww, vaddr);
    if err != 0 {
        return err;
    }

    if !__test_and_set_bit(CONTEXT_INIT_BIT, &mut (*ce).flags) {
        lrc_init_state(ce, engine, *vaddr);
        __i915_gem_object_flush_map((*(*ce).state).obj, 0, (*engine).context_size);
    }
    0
}

// upstream: intel_execlists_submission.c execlists_context_pre_pin()
unsafe fn execlists_context_pre_pin(
    ce: *mut IntelContext,
    ww: *mut I915GemWwCtx,
    vaddr: *mut *mut c_void,
) -> i32 {
    __execlists_context_pre_pin(ce, (*ce).engine, ww, vaddr)
}

// upstream: intel_execlists_submission.c execlists_context_pin()
unsafe fn execlists_context_pin(ce: *mut IntelContext, vaddr: *mut c_void) -> i32 {
    lrc_pin(ce, (*ce).engine, vaddr)
}

// upstream: intel_execlists_submission.c execlists_context_alloc()
unsafe fn execlists_context_alloc(ce: *mut IntelContext) -> i32 {
    lrc_alloc(ce, (*ce).engine)
}

// upstream: intel_execlists_submission.c execlists_context_cancel_request()
unsafe fn execlists_context_cancel_request(ce: *mut IntelContext, rq: *mut I915Request) {
    let mut engine: *mut IntelEngineCs = core::ptr::null_mut();

    i915_request_active_engine(rq, &mut engine);
    if !engine.is_null() && intel_engine_pulse(engine) {
        intel_gt_handle_error(
            (*engine).gt,
            (*engine).mask,
            0,
            c"request cancellation by %s".as_ptr(),
            current_comm(),
        );
    }
}

// upstream: intel_execlists_submission.c execlists_create_parallel()
unsafe fn execlists_create_parallel(
    engines: *mut *mut IntelEngineCs,
    num_siblings: u32,
    width: u32,
) -> *mut IntelContext {
    let mut parent: *mut IntelContext = core::ptr::null_mut();
    let mut ce: *mut IntelContext;
    let mut err: *mut IntelContext = core::ptr::null_mut();
    let mut i = 0;

    GEM_BUG_ON!(num_siblings != 1);

    while i < width {
        ce = intel_context_create(*engines.add(i as usize));
        if IS_ERR!(ce) {
            err = ce;
            break;
        }

        if i == 0 {
            parent = ce;
        } else {
            intel_context_bind_parent_child(parent, ce);
        }
        i += 1;
    }

    if i < width {
        if !parent.is_null() {
            intel_context_put(parent);
        }
        return err;
    }

    (*parent).parallel.fence_context = dma_fence_context_alloc(1);
    intel_context_set_nopreempt(parent);
    for_each_child!(parent, ce, intel_context_set_nopreempt(ce));
    parent
}

#[allow(non_upper_case_globals)]
static execlists_context_ops: IntelContextOps = IntelContextOps {
    flags: COPS_HAS_INFLIGHT | COPS_RUNTIME_CYCLES,
    alloc: execlists_context_alloc,
    cancel_request: execlists_context_cancel_request,
    pre_pin: execlists_context_pre_pin,
    pin: execlists_context_pin,
    unpin: lrc_unpin,
    post_unpin: lrc_post_unpin,
    enter: intel_context_enter_engine,
    exit: intel_context_exit_engine,
    reset: lrc_reset,
    destroy: lrc_destroy,
    create_parallel: execlists_create_parallel,
    create_virtual: Some(execlists_create_virtual),
};

// upstream: intel_execlists_submission.c emit_pdps()
unsafe fn emit_pdps(rq: *mut I915Request) -> i32 {
    let engine = (*rq).engine;
    let ppgtt = i915_vm_to_ppgtt((*(*rq).context).vm);
    let mut err: i32;
    let mut i: u32;
    let mut cs: *mut u32;

    GEM_BUG_ON!(intel_vgpu_active(rq));

    // This magic sequence is fragile: small changes can cause GPU hangs,
    // forcewake failures, or machine lockups.
    cs = intel_ring_begin(rq, 2);
    if IS_ERR!(cs) {
        return PTR_ERR!(cs);
    }
    *cs = MI_ARB_ON_OFF | MI_ARB_DISABLE;
    cs = cs.add(1);
    *cs = MI_NOOP;
    cs = cs.add(1);
    intel_ring_advance(rq, cs);

    // Flush residual operations from context load.
    err = ((*engine).emit_flush)(rq, EMIT_FLUSH);
    if err != 0 {
        return err;
    }
    // Required magic to prevent forcewake errors.
    err = ((*engine).emit_flush)(rq, EMIT_INVALIDATE);
    if err != 0 {
        return err;
    }

    cs = intel_ring_begin(rq, 4 * GEN8_3LVL_PDPES + 2);
    if IS_ERR!(cs) {
        return PTR_ERR!(cs);
    }

    // Ensure posted LRIs have landed before invalidating and continuing.
    *cs = MI_LOAD_REGISTER_IMM(2 * GEN8_3LVL_PDPES) | MI_LRI_FORCE_POSTED;
    cs = cs.add(1);
    i = GEN8_3LVL_PDPES;
    while i != 0 {
        i -= 1;
        let pd_daddr = i915_page_dir_dma_addr(ppgtt, i);
        let base = (*engine).mmio_base;

        *cs = i915_mmio_reg_offset(GEN8_RING_PDP_UDW(base, i));
        cs = cs.add(1);
        *cs = upper_32_bits(pd_daddr);
        cs = cs.add(1);
        *cs = i915_mmio_reg_offset(GEN8_RING_PDP_LDW(base, i));
        cs = cs.add(1);
        *cs = lower_32_bits(pd_daddr);
        cs = cs.add(1);
    }
    *cs = MI_ARB_ON_OFF | MI_ARB_ENABLE;
    cs = cs.add(1);
    intel_ring_advance(rq, cs);

    intel_ring_advance(rq, cs);
    0
}

// upstream: intel_execlists_submission.c execlists_request_alloc()
unsafe fn execlists_request_alloc(request: *mut I915Request) -> i32 {
    GEM_BUG_ON!(!intel_context_is_pinned((*request).context));

    // Reserve enough space to reduce waits after request construction starts.
    (*request).reserved_space += EXECLISTS_REQUEST_SIZE;

    // This request tracks engine initialization and golden-renderstate
    // liveness; after this point cancellation/unwind is unsafe.
    if !i915_vm_is_4lvl((*(*request).context).vm) {
        let ret = emit_pdps(request);
        if ret != 0 {
            return ret;
        }
    }

    // Always invalidate GPU caches and TLBs.
    let ret = ((*(*request).engine).emit_flush)(request, EMIT_INVALIDATE);
    if ret != 0 {
        return ret;
    }

    (*request).reserved_space -= EXECLISTS_REQUEST_SIZE;
    0
}

// upstream: intel_execlists_submission.c reset_csb_pointers()
unsafe fn reset_csb_pointers(engine: *mut IntelEngineCs) {
    let execlists = &mut (*engine).execlists;
    let reset_value = execlists.csb_size as u32 - 1;

    ring_set_paused(engine, 0);

    // Icelake can forget to reset these pointers on GPU reset; force MMIO.
    ENGINE_WRITE!(
        engine,
        RING_CONTEXT_STATUS_PTR,
        0xffff << 16 | reset_value << 8 | reset_value
    );
    ENGINE_POSTING_READ!(engine, RING_CONTEXT_STATUS_PTR);

    // HW starts at CSB[0] after reset, so set HEAD to the last entry and fake
    // the write pointer too, allowing comparison before the first interrupt.
    execlists.csb_head = reset_value as u8;
    WRITE_ONCE!(*execlists.csb_write, reset_value);
    wmb(); /* Ensure visibility to HW (paranoia?). */

    // Check that the GPU updates CSB entries.
    memset(
        execlists.csb_status as *mut c_void,
        !0,
        (reset_value as usize + 1) * size_of::<u64>(),
    );
    drm_clflush_virt_range(
        execlists.csb_status,
        execlists.csb_size as usize * size_of_val(&execlists.csb_status),
    );

    // Once more for luck and our trusty paranoia.
    ENGINE_WRITE!(
        engine,
        RING_CONTEXT_STATUS_PTR,
        0xffff << 16 | reset_value << 8 | reset_value
    );
    ENGINE_POSTING_READ!(engine, RING_CONTEXT_STATUS_PTR);

    GEM_BUG_ON!(READ_ONCE!(*execlists.csb_write) != reset_value);
}

// upstream: intel_execlists_submission.c sanitize_hwsp()
unsafe fn sanitize_hwsp(engine: *mut IntelEngineCs) {
    let mut tl: *mut IntelTimeline;
    list_for_each_entry!(tl, &mut (*engine).status_page.timelines, engine_link, {
        intel_timeline_reset_seqno(tl);
    });
}

// upstream: intel_execlists_submission.c execlists_sanitize()
unsafe fn execlists_sanitize(engine: *mut IntelEngineCs) {
    GEM_BUG_ON!(execlists_active(&mut (*engine).execlists));

    // Poison possible lost/replaced pinned state after suspend/resume.
    if IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
        memset(
            (*engine).status_page.addr as *mut c_void,
            POISON_INUSE,
            PAGE_SIZE,
        );
    }

    reset_csb_pointers(engine);

    // Reset the kernel-context HWSP stored in status_page.
    sanitize_hwsp(engine);

    // Scrub dirty cachelines for the HWSP.
    drm_clflush_virt_range((*engine).status_page.addr, PAGE_SIZE);
    intel_engine_reset_pinned_contexts(engine);
}

// upstream: intel_execlists_submission.c enable_error_interrupt()
unsafe fn enable_error_interrupt(engine: *mut IntelEngineCs) {
    let mut status: u32;

    (*engine).execlists.error_interrupt = 0;
    ENGINE_WRITE!(engine, RING_EMR, !0u32);
    ENGINE_WRITE!(engine, RING_EIR, !0u32); /* Clear existing errors. */

    status = ENGINE_READ!(engine, RING_ESR);
    if unlikely!(status != 0) {
        drm_err!(
            &(*(*engine).i915).drm,
            "engine '%s' resumed still in error: %08x\n",
            (*engine).name,
            status
        );
        intel_gt_reset_engine(engine);
    }

    // Only unmask instruction errors. CP_PRIV is nonfatal and fires when HW
    // validates/suppresses ignored writes; instruction errors are fatal.
    ENGINE_WRITE!(engine, RING_EMR, !I915_ERROR_INSTRUCTION);
}

// upstream: intel_execlists_submission.c enable_execlists()
unsafe fn enable_execlists(engine: *mut IntelEngineCs) {
    let mode: u32;

    assert_forcewakes_active((*engine).uncore, FORCEWAKE_ALL);
    intel_engine_set_hwsp_writemask(engine, !0u32); /* HWSTAM. */

    if GRAPHICS_VER((*engine).i915) >= 11 {
        mode = REG_MASKED_FIELD_ENABLE!(GEN11_GFX_DISABLE_LEGACY_MODE);
    } else {
        mode = REG_MASKED_FIELD_ENABLE!(GFX_RUN_LIST_ENABLE);
    }
    ENGINE_WRITE_FW!(engine, RING_MODE_GEN7, mode);
    ENGINE_WRITE_FW!(engine, RING_MI_MODE, REG_MASKED_FIELD_DISABLE!(STOP_RING));
    ENGINE_WRITE_FW!(
        engine,
        RING_HWS_PGA,
        i915_ggtt_offset((*engine).status_page.vma)
    );
    ENGINE_POSTING_READ!(engine, RING_HWS_PGA);
    enable_error_interrupt(engine);
}

// upstream: intel_execlists_submission.c execlists_resume()
unsafe fn execlists_resume(engine: *mut IntelEngineCs) -> i32 {
    intel_mocs_init_engine(engine);
    intel_breadcrumbs_reset((*engine).breadcrumbs);
    enable_execlists(engine);

    if (*engine).flags & I915_ENGINE_FIRST_RENDER_COMPUTE != 0 {
        xehp_enable_ccs_engines(engine);
    }
    0
}

// upstream: intel_execlists_submission.c execlists_reset_prepare()
unsafe fn execlists_reset_prepare(engine: *mut IntelEngineCs) {
    ENGINE_TRACE!(
        engine,
        "depth<-%d\n",
        atomic_read(&(*(*engine).sched_engine).tasklet.count)
    );

    // Stop submissions until reset_finish; otherwise another engine may queue
    // work into the ELSP while this engine resumes and writes its own ELSP.
    __tasklet_disable_sync_once(&mut (*(*engine).sched_engine).tasklet);
    GEM_BUG_ON!(!reset_in_progress(engine));

    // Stop CS before reset: failed resets can deadlock old parts, and newer
    // GPUs can hang if a batch is still progressing even after READY_TO_RESET.
    ring_set_paused(engine, 1);
    intel_engine_stop_cs(engine);

    // Wa_22011802037: wait for pending MI forcewake after stopping CS.
    if intel_engine_reset_needs_wa_22011802037((*engine).gt) {
        intel_engine_wait_for_pending_mi_fw(engine);
    }

    (*engine).execlists.reset_ccid = active_ccid(engine);
}

// upstream: intel_execlists_submission.c reset_csb()
unsafe fn reset_csb(
    engine: *mut IntelEngineCs,
    inactive: *mut *mut I915Request,
) -> *mut *mut I915Request {
    let execlists = &mut (*engine).execlists;
    drm_clflush_virt_range(execlists.csb_write, size_of::<u32>());

    let inactive = process_csb(engine, inactive); /* Drain preemption events. */

    // Reload CSB read/write pointers after reset.
    reset_csb_pointers(engine);
    inactive
}

// upstream: intel_execlists_submission.c execlists_reset_active()
unsafe fn execlists_reset_active(engine: *mut IntelEngineCs, stalled: bool) {
    let mut ce: *mut IntelContext;
    let mut rq: *mut I915Request;
    let mut head: u32;

    // Preserve the executing context even if its request completed: reset
    // clobbers it if it was running at the time.
    rq = active_context(engine, (*engine).execlists.reset_ccid);
    if rq.is_null() {
        return;
    }

    ce = (*rq).context;
    GEM_BUG_ON!(!i915_vma_is_pinned((*ce).state));

    if __i915_request_is_complete(rq) {
        // Idle context: tidy ring to restart afresh.
        head = intel_ring_wrap((*ce).ring, (*rq).tail);
    } else {
        // Requests are still inflight, so engine must be awake and context
        // active.
        GEM_BUG_ON!(!intel_engine_pm_is_awake(engine));
        GEM_BUG_ON!(i915_active_is_idle(&(*ce).active));

        rq = active_request((*ce).timeline, rq);
        head = intel_ring_wrap((*ce).ring, (*rq).head);
        GEM_BUG_ON!(head == (*(*ce).ring).tail);

        // Unstarted requests (for example semaphore waits) must replay to
        // preserve signaling. A context loading during reset can be corrupt,
        // but a request not yet started should normally replay without error.
        if !__i915_request_has_started(rq) {
            // Continue to the common context replay path below.
        } else {
            // Innocent requests remain for replay. Guilty requests have their
            // result marked and ring registers repaired to skip corrupted work.
            __i915_request_reset(rq, stalled);
        }
    }

    // Rebuild a simple context/ring for breadcrumb update; future user work
    // follows only after userspace can recreate state.
    ENGINE_TRACE!(
        engine,
        "replay {head:%04x, tail:%04x}\n",
        head,
        (*(*ce).ring).tail
    );
    lrc_reset_regs(ce, engine);
    (*ce).lrc.lrca = lrc_update_regs(ce, engine, head);
}

// upstream: intel_execlists_submission.c execlists_reset_csb()
unsafe fn execlists_reset_csb(engine: *mut IntelEngineCs, stalled: bool) {
    let execlists = &mut (*engine).execlists;
    let mut post = [core::ptr::null_mut::<I915Request>(); 2 * EXECLIST_MAX_PORTS];

    rcu_read_lock();
    let mut inactive = reset_csb(engine, post.as_mut_ptr());
    execlists_reset_active(engine, true);
    inactive = cancel_port_requests(execlists, inactive);
    post_process_csb(post.as_mut_ptr(), inactive);
    rcu_read_unlock();
}

// upstream: intel_execlists_submission.c execlists_reset_rewind()
unsafe fn execlists_reset_rewind(engine: *mut IntelEngineCs, stalled: bool) {
    let mut flags: c_ulong = 0;
    ENGINE_TRACE!(engine, "\n");

    // Process CSB, find guilty context, and discard its state.
    execlists_reset_csb(engine, stalled);

    // Queue incomplete requests for replay after reset.
    rcu_read_lock();
    spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags);
    __unwind_incomplete_requests(engine);
    spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags);
    rcu_read_unlock();
}

// upstream: intel_execlists_submission.c nop_submission_tasklet()
unsafe fn nop_submission_tasklet(t: *mut TaskletStruct) {
    let sched_engine = from_tasklet!(t, I915SchedEngine, tasklet);
    let engine = (*sched_engine).private_data;
    // Driver is wedged; do not process further events.
    WRITE_ONCE!((*(*engine).sched_engine).queue_priority_hint, INT_MIN);
}

// upstream: intel_execlists_submission.c execlists_reset_cancel()
unsafe fn execlists_reset_cancel(engine: *mut IntelEngineCs) {
    let execlists = &mut (*engine).execlists;
    let sched_engine = (*engine).sched_engine;
    let mut rq: *mut I915Request;
    let mut rn: *mut I915Request;
    let mut rb: *mut RbNode;
    let mut flags: c_ulong = 0;

    ENGINE_TRACE!(engine, "\n");

    // Caller disabled interrupts, tasklet, and competing state users. Keep
    // IRQ masking explicit for lockdep and keep submission IRQ state separate.
    execlists_reset_csb(engine, true);

    rcu_read_lock();
    spin_lock_irqsave(&mut (*sched_engine).lock, &mut flags);

    // Mark executing requests skipped.
    list_for_each_entry!(rq, &mut (*sched_engine).requests, sched.link, {
        i915_request_put(i915_request_mark_eio(unsafe { &mut *rq }));
    });
    intel_engine_signal_breadcrumbs(engine);

    // Flush queued requests to timelines for retirement.
    loop {
        rb = rb_first_cached(&(*sched_engine).queue);
        if rb.is_null() {
            break;
        }
        let p = to_priolist(rb);
        priolist_for_each_request_consume!(rq, rn, p, {
            if i915_request_mark_eio(unsafe { &mut *rq }) {
                __i915_request_submit(rq);
                i915_request_put(rq);
            }
        });
        rb_erase_cached(&mut (*p).node, &mut (*sched_engine).queue);
        i915_priolist_free(p);
    }

    // Held requests flush to timeline on release.
    list_for_each_entry!(rq, &mut (*sched_engine).hold, sched.link, {
        i915_request_put(i915_request_mark_eio(unsafe { &mut *rq }));
    });

    // Cancel virtual engines attached to this physical engine.
    loop {
        rb = rb_first_cached(&(*execlists).r#virtual);
        if rb.is_null() {
            break;
        }
        let ve = rb_entry!(rb, VirtualEngine, nodes[(*engine).id].rb);
        rb_erase_cached(rb, &mut (*execlists).r#virtual);
        RB_CLEAR_NODE(rb);

        spin_lock(&mut (*(*ve).base.sched_engine).lock);
        rq = fetch_and_zero(&mut (*ve).request);
        if !rq.is_null() {
            if i915_request_mark_eio(unsafe { &mut *rq }) {
                (*rq).engine = engine;
                __i915_request_submit(rq);
                i915_request_put(rq);
            }
            i915_request_put(rq);
            (*(*ve).base.sched_engine).queue_priority_hint = INT_MIN;
        }
        spin_unlock(&mut (*(*ve).base.sched_engine).lock);
    }

    // Remaining unready requests are nopped when submitted.
    (*sched_engine).queue_priority_hint = INT_MIN;
    (*sched_engine).queue = RB_ROOT_CACHED!();

    GEM_BUG_ON!(__tasklet_is_enabled(&mut (*(*engine).sched_engine).tasklet));
    (*(*engine).sched_engine).tasklet.callback = nop_submission_tasklet;

    spin_unlock_irqrestore(&mut (*sched_engine).lock, flags);
    rcu_read_unlock();
}

// upstream: intel_execlists_submission.c execlists_reset_finish()
unsafe fn execlists_reset_finish(engine: *mut IntelEngineCs) {
    let execlists = &mut (*engine).execlists;

    // Replay requests while forcewake is held, before the GPU can sleep. On
    // failed reset, inflight work may still complete or be recovered at a
    // higher level, with wedging the final fallback.
    GEM_BUG_ON!(!reset_in_progress(engine));

    if __tasklet_enable(&mut (*(*engine).sched_engine).tasklet) {
        __execlists_kick(execlists);
    }
    ENGINE_TRACE!(
        engine,
        "depth->%d\n",
        atomic_read(&(*(*engine).sched_engine).tasklet.count)
    );
}

// upstream: intel_execlists_submission.c gen8_logical_ring_enable_irq()
unsafe fn gen8_logical_ring_enable_irq(engine: *mut IntelEngineCs) {
    ENGINE_WRITE!(
        engine,
        RING_IMR,
        !((*engine).irq_enable_mask | (*engine).irq_keep_mask)
    );
    ENGINE_POSTING_READ!(engine, RING_IMR);
}

// upstream: intel_execlists_submission.c gen8_logical_ring_disable_irq()
unsafe fn gen8_logical_ring_disable_irq(engine: *mut IntelEngineCs) {
    ENGINE_WRITE!(engine, RING_IMR, !(*engine).irq_keep_mask);
}

// upstream: intel_execlists_submission.c execlists_park()
unsafe fn execlists_park(engine: *mut IntelEngineCs) {
    cancel_timer(&mut (*engine).execlists.timer);
    cancel_timer(&mut (*engine).execlists.preempt);
    // Reset on idle so we do not delay busy wakeup.
    WRITE_ONCE!((*(*engine).sched_engine).queue_priority_hint, INT_MIN);
}

// upstream: intel_execlists_submission.c add_to_engine()
unsafe fn add_to_engine(rq: *mut I915Request) {
    lockdep_assert_held!(&mut (*(*(*rq).engine).sched_engine).lock);
    list_move_tail(
        &mut (*rq).sched.link,
        &mut (*(*(*rq).engine).sched_engine).requests,
    );
}

// upstream: intel_execlists_submission.c remove_from_engine()
unsafe fn remove_from_engine(rq: *mut I915Request) {
    let mut engine: *mut IntelEngineCs;
    let mut locked: *mut IntelEngineCs;

    // A virtual request's rq->engine is unstable until under that engine lock.
    // Lock the observed engine and retry if ownership changed.
    locked = READ_ONCE!((*rq).engine);
    spin_lock_irq(&mut (*(*locked).sched_engine).lock);
    loop {
        engine = READ_ONCE!((*rq).engine);
        if likely!(locked == engine) {
            break;
        }
        spin_unlock(&mut (*(*locked).sched_engine).lock);
        spin_lock(&mut (*(*engine).sched_engine).lock);
        locked = engine;
    }

    list_del_init(&mut (*rq).sched.link);
    clear_bit(I915_FENCE_FLAG_PQUEUE, &mut (*rq).fence.flags);
    clear_bit(I915_FENCE_FLAG_HOLD, &mut (*rq).fence.flags);

    // Block future __await_execution() callback registration, then flush.
    set_bit(I915_FENCE_FLAG_ACTIVE, &mut (*rq).fence.flags);
    spin_unlock_irq(&mut (*(*locked).sched_engine).lock);

    i915_request_notify_execute_cb_imm(rq);
}

// upstream: intel_execlists_submission.c can_preempt()
unsafe fn can_preempt(engine: *mut IntelEngineCs) -> bool {
    GRAPHICS_VER((*engine).i915) > 8
}

// upstream: intel_execlists_submission.c kick_execlists()
unsafe fn kick_execlists(rq: *const I915Request, prio: i32) {
    let engine = (*rq).engine;
    let sched_engine = (*engine).sched_engine;
    let mut inflight: *const I915Request;

    // Only kick once for a high-priority new context.
    if prio <= (*sched_engine).queue_priority_hint {
        return;
    }

    rcu_read_lock();

    // If nothing is active, submission is overdue.
    inflight = execlists_active(&(*engine).execlists);
    if inflight.is_null() {
        rcu_read_unlock();
        return;
    }

    // Do not consider preempting our own current context.
    if (*inflight).context == (*rq).context {
        rcu_read_unlock();
        return;
    }

    ENGINE_TRACE!(
        engine,
        "bumping queue-priority-hint:%d for rq:%llx:%lld, inflight:%llx:%lld prio %d\n",
        prio,
        (*rq).fence.context,
        (*rq).fence.seqno,
        (*inflight).fence.context,
        (*inflight).fence.seqno,
        (*inflight).sched.attr.priority
    );

    (*sched_engine).queue_priority_hint = prio;

    // Permit low->normal->high preemption, but do not let low-priority work
    // preempt other low-priority work where background throughput matters more.
    if prio >= max(I915_PRIORITY_NORMAL, rq_prio(inflight)) {
        tasklet_hi_schedule(&mut (*sched_engine).tasklet);
    }
    rcu_read_unlock();
}

// upstream: intel_execlists_submission.c execlists_set_default_submission()
unsafe fn execlists_set_default_submission(engine: *mut IntelEngineCs) {
    (*engine).submit_request = execlists_submit_request;
    (*(*engine).sched_engine).schedule = i915_schedule;
    (*(*engine).sched_engine).kick_backend = kick_execlists;
    (*(*engine).sched_engine).tasklet.callback = execlists_submission_tasklet;
}

// upstream: intel_execlists_submission.c execlists_shutdown()
unsafe fn execlists_shutdown(engine: *mut IntelEngineCs) {
    // Synchronize residual timers and any softirqs they raise.
    timer_delete_sync(&mut (*engine).execlists.timer);
    timer_delete_sync(&mut (*engine).execlists.preempt);
    tasklet_kill(&mut (*(*engine).sched_engine).tasklet);
}

// upstream: intel_execlists_submission.c execlists_release()
unsafe fn execlists_release(engine: *mut IntelEngineCs) {
    (*engine).sanitize = None; /* No longer controlled; nothing to sanitize. */
    execlists_shutdown(engine);
    intel_engine_cleanup_common(engine);
    lrc_fini_wa_ctx(engine);
}

// upstream: intel_execlists_submission.c __execlists_engine_busyness()
unsafe fn __execlists_engine_busyness(engine: *mut IntelEngineCs, now: *mut KtimeT) -> KtimeT {
    let stats = &mut (*engine).stats.execlists;
    let mut total = stats.total;

    // Include current execution in accumulated busyness.
    *now = ktime_get();
    if READ_ONCE!(stats.active) {
        total = ktime_add(total, ktime_sub(*now, stats.start));
    }
    total
}

// upstream: intel_execlists_submission.c execlists_engine_busyness()
unsafe fn execlists_engine_busyness(engine: *mut IntelEngineCs, now: *mut KtimeT) -> KtimeT {
    let stats = &mut (*engine).stats.execlists;
    let mut seq: u32;
    let mut total: KtimeT;

    loop {
        seq = read_seqcount_begin(&mut stats.lock);
        total = __execlists_engine_busyness(engine, now);
        if !read_seqcount_retry(&mut stats.lock, seq) {
            break;
        }
    }
    total
}

// upstream: intel_execlists_submission.c logical_ring_default_vfuncs()
unsafe fn logical_ring_default_vfuncs(engine: *mut IntelEngineCs) {
    // Default virtual functions, overridable per engine.
    (*engine).resume = execlists_resume;
    (*engine).cops = &execlists_context_ops;
    (*engine).request_alloc = execlists_request_alloc;
    (*engine).add_active_request = add_to_engine;
    (*engine).remove_active_request = remove_from_engine;

    (*engine).reset.prepare = execlists_reset_prepare;
    (*engine).reset.rewind = execlists_reset_rewind;
    (*engine).reset.cancel = execlists_reset_cancel;
    (*engine).reset.finish = execlists_reset_finish;

    (*engine).park = execlists_park;
    (*engine).unpark = None;
    (*engine).emit_flush = gen8_emit_flush_xcs;
    (*engine).emit_init_breadcrumb = gen8_emit_init_breadcrumb;
    (*engine).emit_fini_breadcrumb = gen8_emit_fini_breadcrumb_xcs;
    if GRAPHICS_VER((*engine).i915) >= 12 {
        (*engine).emit_fini_breadcrumb = gen12_emit_fini_breadcrumb_xcs;
        (*engine).emit_flush = gen12_emit_flush_xcs;
    }
    (*engine).set_default_submission = execlists_set_default_submission;

    if GRAPHICS_VER((*engine).i915) < 11 {
        (*engine).irq_enable = gen8_logical_ring_enable_irq;
        (*engine).irq_disable = gen8_logical_ring_disable_irq;
    } else {
        // TODO: Gen11 masks must be clear for C6; keep interrupts enabled and
        // accept extra interrupts until a more refined solution exists.
    }
    intel_engine_set_irq_handler(engine, execlists_irq_handler);

    (*engine).flags |= I915_ENGINE_SUPPORTS_STATS;
    if !intel_vgpu_active((*engine).i915) {
        (*engine).flags |= I915_ENGINE_HAS_SEMAPHORES;
        if can_preempt(engine) {
            (*engine).flags |= I915_ENGINE_HAS_PREEMPTION;
            if CONFIG_DRM_I915_TIMESLICE_DURATION != 0 {
                (*engine).flags |= I915_ENGINE_HAS_TIMESLICES;
            }
        }
    }

    if GRAPHICS_VER_FULL((*engine).i915) >= IP_VER(12, 55) {
        if intel_engine_has_preemption(engine) {
            (*engine).emit_bb_start = xehp_emit_bb_start;
        } else {
            (*engine).emit_bb_start = xehp_emit_bb_start_noarb;
        }
    } else if intel_engine_has_preemption(engine) {
        (*engine).emit_bb_start = gen8_emit_bb_start;
    } else {
        (*engine).emit_bb_start = gen8_emit_bb_start_noarb;
    }

    (*engine).busyness = execlists_engine_busyness;
}

// upstream: intel_execlists_submission.c logical_ring_default_irqs()
unsafe fn logical_ring_default_irqs(engine: *mut IntelEngineCs) {
    let mut shift = 0u32;

    if GRAPHICS_VER((*engine).i915) < 11 {
        let irq_shifts = [
            [RCS0, GEN8_RCS_IRQ_SHIFT],
            [BCS0, GEN8_BCS_IRQ_SHIFT],
            [VCS0, GEN8_VCS0_IRQ_SHIFT],
            [VCS1, GEN8_VCS1_IRQ_SHIFT],
            [VECS0, GEN8_VECS_IRQ_SHIFT],
        ];
        shift = irq_shifts[(*engine).id as usize].1;
    }

    (*engine).irq_enable_mask = GT_RENDER_USER_INTERRUPT << shift;
    (*engine).irq_keep_mask = GT_CONTEXT_SWITCH_INTERRUPT << shift;
    (*engine).irq_keep_mask |= GT_CS_MASTER_ERROR_INTERRUPT << shift;
    (*engine).irq_keep_mask |= GT_WAIT_SEMAPHORE_INTERRUPT << shift;
}

// upstream: intel_execlists_submission.c rcs_submission_override()
unsafe fn rcs_submission_override(engine: *mut IntelEngineCs) {
    match GRAPHICS_VER((*engine).i915) {
        12 => {
            (*engine).emit_flush = gen12_emit_flush_rcs;
            (*engine).emit_fini_breadcrumb = gen12_emit_fini_breadcrumb_rcs;
        }
        11 => {
            (*engine).emit_flush = gen11_emit_flush_rcs;
            (*engine).emit_fini_breadcrumb = gen11_emit_fini_breadcrumb_rcs;
        }
        _ => {
            (*engine).emit_flush = gen8_emit_flush_rcs;
            (*engine).emit_fini_breadcrumb = gen8_emit_fini_breadcrumb_rcs;
        }
    }
}

// upstream: intel_execlists_submission.c intel_execlists_submission_setup()
unsafe fn intel_execlists_submission_setup(engine: *mut IntelEngineCs) -> i32 {
    let execlists = &mut (*engine).execlists;
    let i915 = (*engine).i915;
    let uncore = (*engine).uncore;
    let base = (*engine).mmio_base;

    tasklet_setup(
        &mut (*(*engine).sched_engine).tasklet,
        execlists_submission_tasklet,
    );
    timer_setup(&mut execlists.timer, execlists_timeslice, 0);
    timer_setup(&mut execlists.preempt, execlists_preempt, 0);

    logical_ring_default_vfuncs(engine);
    logical_ring_default_irqs(engine);
    seqcount_init(&mut (*engine).stats.execlists.lock);

    if (*engine).flags & I915_ENGINE_HAS_RCS_REG_STATE != 0 {
        rcs_submission_override(engine);
    }
    lrc_init_wa_ctx(engine);

    if HAS_LOGICAL_RING_ELSQ(i915) {
        execlists.submit_reg = intel_uncore_regs(uncore)
            .add(i915_mmio_reg_offset(RING_EXECLIST_SQ_CONTENTS(base)) as usize);
        execlists.ctrl_reg = intel_uncore_regs(uncore)
            .add(i915_mmio_reg_offset(RING_EXECLIST_CONTROL(base)) as usize);
        (*engine).fw_domain = intel_uncore_forcewake_for_reg(
            (*engine).uncore,
            RING_EXECLIST_CONTROL((*engine).mmio_base),
            FW_REG_WRITE,
        );
    } else {
        execlists.submit_reg =
            intel_uncore_regs(uncore).add(i915_mmio_reg_offset(RING_ELSP(base)) as usize);
    }

    execlists.csb_status = ((*engine).status_page.addr.add(I915_HWS_CSB_BUF0_INDEX)) as *mut u64;
    execlists.csb_write = (*engine)
        .status_page
        .addr
        .add(INTEL_HWS_CSB_WRITE_INDEX(i915));

    if GRAPHICS_VER(i915) < 11 {
        execlists.csb_size = GEN8_CSB_ENTRIES;
    } else {
        execlists.csb_size = GEN11_CSB_ENTRIES;
    }

    (*engine).context_tag = GENMASK!(BITS_PER_LONG - 2, 0);
    if GRAPHICS_VER(i915) >= 11 && GRAPHICS_VER_FULL(i915) < IP_VER(12, 55) {
        execlists.ccid |= (*engine).instance << (GEN11_ENGINE_INSTANCE_SHIFT - 32);
        execlists.ccid |= (*engine).class << (GEN11_ENGINE_CLASS_SHIFT - 32);
    }

    // Take ownership and responsibility for cleanup.
    (*engine).sanitize = execlists_sanitize;
    (*engine).release = execlists_release;
    0
}

// upstream: intel_execlists_submission.c virtual_queue()
unsafe fn virtual_queue(ve: *mut VirtualEngine) -> *mut ListHead {
    &mut (*(*ve).base.sched_engine).default_priolist.requests
}

// upstream: intel_execlists_submission.c rcu_virtual_context_destroy()
unsafe fn rcu_virtual_context_destroy(wrk: *mut WorkStruct) {
    let ve = container_of!(wrk, VirtualEngine, rcu.work);
    let mut n: u32;

    GEM_BUG_ON!(!(*ve).context.inflight.is_null());

    // Preempt-to-busy may leave a stale request behind.
    if unlikely!(!(*ve).request.is_null()) {
        spin_lock_irq(&mut (*(*(*ve).base.sched_engine)).lock);

        let old = fetch_and_zero(&mut (*ve).request);
        if !old.is_null() {
            GEM_BUG_ON!(!__i915_request_is_complete(old));
            __i915_request_submit(old);
            i915_request_put(old);
        }

        spin_unlock_irq(&mut (*(*(*ve).base.sched_engine)).lock);
    }

    // Flush tasklet before removing sibling rbtrees; a concurrent tasklet
    // could otherwise reinsert its rb_node into a sibling.
    tasklet_kill(&mut (*(*ve).base.sched_engine).tasklet);

    // Detach from siblings; no access is permitted after this.
    n = 0;
    while n < (*ve).num_siblings {
        let sibling = *(*ve).siblings.add(n as usize);
        let node = &mut (*ve).nodes[(*sibling).id as usize].rb;

        if RB_EMPTY_NODE(node) {
            n += 1;
            continue;
        }

        spin_lock_irq(&mut (*(*sibling).sched_engine).lock);
        // Detachment is otherwise lazy in sched_engine->tasklet.
        if !RB_EMPTY_NODE(node) {
            rb_erase_cached(node, &mut (*sibling).execlists.r#virtual);
        }
        spin_unlock_irq(&mut (*(*sibling).sched_engine).lock);
        n += 1;
    }

    GEM_BUG_ON!(__tasklet_is_scheduled(
        &mut (*(*ve).base.sched_engine).tasklet
    ));
    GEM_BUG_ON!(!list_empty(virtual_queue(ve)));

    lrc_fini(&mut (*ve).context);
    intel_context_fini(&mut (*ve).context);

    if !(*ve).base.breadcrumbs.is_null() {
        intel_breadcrumbs_put((*ve).base.breadcrumbs);
    }
    if !(*ve).base.sched_engine.is_null() {
        i915_sched_engine_put((*ve).base.sched_engine);
    }
    intel_engine_free_request_pool(&mut (*ve).base);
    kfree(ve as *mut c_void);
}

// upstream: intel_execlists_submission.c virtual_context_destroy()
unsafe fn virtual_context_destroy(kref: *mut Kref) {
    let ve = container_of!(kref, VirtualEngine, context.r#ref);

    GEM_BUG_ON!(!list_empty(&(*ve).context.signals));

    // IRQ/softirq may still resubmit a completed preempt-to-busy request.
    // Flush submission and tasklets before freeing. Since flushing needs
    // process context and RCU protects resubmit, delegate to an RCU worker.
    INIT_RCU_WORK(&mut (*ve).rcu, rcu_virtual_context_destroy);
    queue_rcu_work((*(*(*ve).context.engine).i915).unordered_wq, &mut (*ve).rcu);
}

// upstream: intel_execlists_submission.c virtual_engine_initial_hint()
unsafe fn virtual_engine_initial_hint(ve: *mut VirtualEngine) {
    let swp = get_random_u32_below((*ve).num_siblings);

    // Randomize initial sibling order to spread batches of similarly created
    // contexts. sibling[0] is inspected first, but does not force execution.
    if swp != 0 {
        swap(&mut *(*ve).siblings.add(swp as usize), &mut *(*ve).siblings);
    }
}

// upstream: intel_execlists_submission.c virtual_context_alloc()
unsafe fn virtual_context_alloc(ce: *mut IntelContext) -> i32 {
    let ve = container_of!(ce, VirtualEngine, context);
    lrc_alloc(ce, *(*ve).siblings)
}

// upstream: intel_execlists_submission.c virtual_context_pre_pin()
unsafe fn virtual_context_pre_pin(
    ce: *mut IntelContext,
    ww: *mut I915GemWwCtx,
    vaddr: *mut *mut c_void,
) -> i32 {
    let ve = container_of!(ce, VirtualEngine, context);
    // Use a real engine class to set up register state.
    __execlists_context_pre_pin(ce, *(*ve).siblings, ww, vaddr)
}

// upstream: intel_execlists_submission.c virtual_context_pin()
unsafe fn virtual_context_pin(ce: *mut IntelContext, vaddr: *mut c_void) -> i32 {
    let ve = container_of!(ce, VirtualEngine, context);
    lrc_pin(ce, *(*ve).siblings, vaddr)
}

// upstream: intel_execlists_submission.c virtual_context_enter()
unsafe fn virtual_context_enter(ce: *mut IntelContext) {
    let ve = container_of!(ce, VirtualEngine, context);
    let mut n = 0;

    while n < (*ve).num_siblings {
        intel_engine_pm_get(*(*ve).siblings.add(n as usize));
        n += 1;
    }
    intel_timeline_enter((*ce).timeline);
}

// upstream: intel_execlists_submission.c virtual_context_exit()
unsafe fn virtual_context_exit(ce: *mut IntelContext) {
    let ve = container_of!(ce, VirtualEngine, context);
    let mut n = 0;

    intel_timeline_exit((*ce).timeline);
    while n < (*ve).num_siblings {
        intel_engine_pm_put(*(*ve).siblings.add(n as usize));
        n += 1;
    }
}

// upstream: intel_execlists_submission.c virtual_get_sibling()
unsafe fn virtual_get_sibling(engine: *mut IntelEngineCs, sibling: u32) -> *mut IntelEngineCs {
    let ve = to_virtual_engine(engine);
    if sibling >= (*ve).num_siblings {
        return core::ptr::null_mut();
    }
    *(*ve).siblings.add(sibling as usize)
}

#[allow(non_upper_case_globals)]
static virtual_context_ops: IntelContextOps = IntelContextOps {
    flags: COPS_HAS_INFLIGHT | COPS_RUNTIME_CYCLES,
    alloc: virtual_context_alloc,
    cancel_request: execlists_context_cancel_request,
    pre_pin: virtual_context_pre_pin,
    pin: virtual_context_pin,
    unpin: lrc_unpin,
    post_unpin: lrc_post_unpin,
    enter: virtual_context_enter,
    exit: virtual_context_exit,
    destroy: virtual_context_destroy,
    get_sibling: Some(virtual_get_sibling),
};

// upstream: intel_execlists_submission.c virtual_submission_mask()
unsafe fn virtual_submission_mask(ve: *mut VirtualEngine) -> IntelEngineMaskT {
    let rq = READ_ONCE!((*ve).request);
    let mut mask: IntelEngineMaskT;

    if rq.is_null() {
        return 0;
    }

    // Ready for submit: execution_mask is now stable.
    mask = (*rq).execution_mask;
    if unlikely!(mask == 0) {
        // Invalid selection: submit to a random engine in error.
        i915_request_set_error_once(rq, -ENODEV);
        mask = (**(*ve).siblings).mask;
    }

    ENGINE_TRACE!(
        &(*ve).base,
        "rq=%llx:%lld, mask=%x, prio=%d\n",
        (*rq).fence.context,
        (*rq).fence.seqno,
        mask,
        (*(*ve).base.sched_engine).queue_priority_hint
    );
    mask
}

// upstream: intel_execlists_submission.c virtual_submission_tasklet()
unsafe fn virtual_submission_tasklet(t: *mut TaskletStruct) {
    let sched_engine = from_tasklet!(t, I915SchedEngine, tasklet);
    let ve = (*sched_engine).private_data as *mut VirtualEngine;
    let prio = READ_ONCE!((*sched_engine).queue_priority_hint);
    let mut mask: IntelEngineMaskT;
    let mut n = 0;

    rcu_read_lock();
    mask = virtual_submission_mask(ve);
    rcu_read_unlock();
    if unlikely!(mask == 0) {
        return;
    }

    while n < (*ve).num_siblings {
        let sibling = READ_ONCE!(*(*ve).siblings.add(n as usize));
        let node = &mut (*ve).nodes[(*sibling).id as usize];
        let mut parent: *mut *mut RbNode;
        let mut rb: *mut RbNode;
        let mut first: bool;

        if READ_ONCE!((*ve).request).is_null() {
            break; /* Already handled by another sibling tasklet. */
        }

        spin_lock_irq(&mut (*(*sibling).sched_engine).lock);

        if unlikely!(mask & (*sibling).mask == 0) {
            if !RB_EMPTY_NODE(&mut node.rb) {
                rb_erase_cached(&mut node.rb, &mut (*sibling).execlists.r#virtual);
                RB_CLEAR_NODE(&mut node.rb);
            }
            spin_unlock_irq(&mut (*(*sibling).sched_engine).lock);
            n += 1;
            continue;
        }

        if unlikely!(!RB_EMPTY_NODE(&mut node.rb)) {
            // Reuse in place when it avoids tree rebalancing.
            first = rb_first_cached(&(*sibling).execlists.r#virtual) == &mut node.rb;
            if prio == node.prio || (prio > node.prio && first) {
                node.prio = prio;
                if first && prio > (*(*sibling).sched_engine).queue_priority_hint {
                    tasklet_hi_schedule(&mut (*(*sibling).sched_engine).tasklet);
                }
                spin_unlock_irq(&mut (*(*sibling).sched_engine).lock);
                if !intel_context_inflight(&(*ve).context).is_null() {
                    break;
                }
                n += 1;
                continue;
            }
            rb_erase_cached(&mut node.rb, &mut (*sibling).execlists.r#virtual);
        }

        rb = core::ptr::null_mut();
        first = true;
        parent = &mut (*sibling).execlists.r#virtual.rb_root.rb_node;
        while !(*parent).is_null() {
            let other_node = *parent;
            let other = rb_entry!(other_node, VeNode, rb);
            rb = other_node;
            if prio > (*other).prio {
                parent = &mut (*other).rb.rb_left;
            } else {
                parent = &mut (*other).rb.rb_right;
                first = false;
            }
        }

        rb_link_node(&mut node.rb, rb, parent);
        rb_insert_color_cached(&mut node.rb, &mut (*sibling).execlists.r#virtual, first);

        GEM_BUG_ON!(RB_EMPTY_NODE(&mut node.rb));
        node.prio = prio;
        if first && prio > (*(*sibling).sched_engine).queue_priority_hint {
            tasklet_hi_schedule(&mut (*(*sibling).sched_engine).tasklet);
        }

        spin_unlock_irq(&mut (*(*sibling).sched_engine).lock);
        if !intel_context_inflight(&(*ve).context).is_null() {
            break;
        }
        n += 1;
    }
}

// upstream: intel_execlists_submission.c virtual_submit_request()
unsafe fn virtual_submit_request(rq: *mut I915Request) {
    let ve = to_virtual_engine((*rq).engine);
    let mut flags: c_ulong = 0;

    ENGINE_TRACE!(
        &mut (*ve).base,
        "rq=%llx:%lld\n",
        (*rq).fence.context,
        (*rq).fence.seqno
    );
    GEM_BUG_ON!((*ve).base.submit_request != virtual_submit_request);

    spin_lock_irqsave(&mut (*(*ve).base.sched_engine).lock, &mut flags);

    // A resubmitted request may already have completed.
    if __i915_request_is_complete(rq) {
        __i915_request_submit(rq);
        spin_unlock_irqrestore(&mut (*(*ve).base.sched_engine).lock, flags);
        return;
    }

    if !(*ve).request.is_null() {
        // Background preempt-to-busy completion.
        GEM_BUG_ON!(!__i915_request_is_complete((*ve).request));
        __i915_request_submit((*ve).request);
        i915_request_put((*ve).request);
    }

    (*(*ve).base.sched_engine).queue_priority_hint = rq_prio(rq);
    (*ve).request = i915_request_get(rq);

    GEM_BUG_ON!(!list_empty(virtual_queue(ve)));
    list_move_tail(&mut (*rq).sched.link, virtual_queue(ve));

    tasklet_hi_schedule(&mut (*(*ve).base.sched_engine).tasklet);
    spin_unlock_irqrestore(&mut (*(*ve).base.sched_engine).lock, flags);
}

// upstream: intel_execlists_submission.c execlists_create_virtual()
unsafe fn execlists_create_virtual(
    siblings: *mut *mut IntelEngineCs,
    count: u32,
    flags: c_ulong,
) -> *mut IntelContext {
    let i915 = (**siblings).i915;
    let mut ve: *mut VirtualEngine;
    let mut n: u32;
    let mut err = -ENOMEM;

    ve = kzalloc_flex!(VirtualEngine, siblings, count);
    if ve.is_null() {
        return ERR_PTR!(err);
    }

    (*ve).base.i915 = i915;
    (*ve).base.gt = (**siblings).gt;
    (*ve).base.uncore = (**siblings).uncore;
    (*ve).base.id = -1;

    (*ve).base.class = OTHER_CLASS;
    (*ve).base.uabi_class = I915_ENGINE_CLASS_INVALID as u16;
    (*ve).base.instance = I915_ENGINE_CLASS_INVALID_VIRTUAL as u8;
    (*ve).base.uabi_instance = I915_ENGINE_CLASS_INVALID_VIRTUAL as u16;

    // Semaphore submission policy depends on global engine saturation. Since
    // virtual engines span physical engines, precomputing one engine's state
    // can unfairly starve a semaphore user. The compromise is global.
    (*ve).base.saturated = ALL_ENGINES;
    snprintf!(
        (*ve).base.name.as_mut_ptr(),
        (*ve).base.name.len(),
        "virtual"
    );

    intel_engine_init_execlists(&mut (*ve).base);
    (*ve).base.sched_engine = i915_sched_engine_create(ENGINE_VIRTUAL);
    if (*ve).base.sched_engine.is_null() {
        kfree(ve as *mut c_void);
        return ERR_PTR!(err);
    }
    (*(*ve).base.sched_engine).private_data = &mut (*ve).base;

    (*ve).base.cops = &virtual_context_ops;
    (*ve).base.request_alloc = execlists_request_alloc;
    (*(*ve).base.sched_engine).schedule = i915_schedule;
    (*(*ve).base.sched_engine).kick_backend = kick_execlists;
    (*ve).base.submit_request = virtual_submit_request;

    INIT_LIST_HEAD(virtual_queue(ve));
    tasklet_setup(
        &mut (*(*ve).base.sched_engine).tasklet,
        virtual_submission_tasklet,
    );
    intel_context_init(&mut (*ve).context, &mut (*ve).base);

    (*ve).base.breadcrumbs = intel_breadcrumbs_create(core::ptr::null_mut());
    if (*ve).base.breadcrumbs.is_null() {
        intel_context_put(&mut (*ve).context);
        return ERR_PTR!(err);
    }

    n = 0;
    while n < count {
        let sibling = *siblings.add(n as usize);

        GEM_BUG_ON!(!is_power_of_2((*sibling).mask));
        if (*sibling).mask & (*ve).base.mask != 0 {
            drm_dbg!(
                &(*i915).drm,
                "duplicate %s entry in load balancer\n",
                (*sibling).name
            );
            err = -EINVAL;
            intel_context_put(&mut (*ve).context);
            return ERR_PTR!(err);
        }

        // Backend is coupled to execlists: requests are inserted directly
        // into each physical engine's tree. Layering would need cloned requests.
        if (*(*sibling).sched_engine).tasklet.callback != execlists_submission_tasklet {
            err = -ENODEV;
            intel_context_put(&mut (*ve).context);
            return ERR_PTR!(err);
        }

        GEM_BUG_ON!(RB_EMPTY_NODE(&mut (*ve).nodes[(*sibling).id as usize].rb));
        RB_CLEAR_NODE(&mut (*ve).nodes[(*sibling).id as usize].rb);

        *(*ve).siblings.add((*ve).num_siblings as usize) = sibling;
        (*ve).num_siblings += 1;
        (*ve).base.mask |= (*sibling).mask;
        (*ve).base.logical_mask |= (*sibling).logical_mask;

        // Emission functions must be compatible because commands are built
        // before submission; engine class is the current compatibility guide.
        if (*ve).base.class != OTHER_CLASS {
            if (*ve).base.class != (*sibling).class {
                drm_dbg!(
                    &(*i915).drm,
                    "invalid mixing of engine class, sibling %d, already %d\n",
                    (*sibling).class,
                    (*ve).base.class
                );
                err = -EINVAL;
                intel_context_put(&mut (*ve).context);
                return ERR_PTR!(err);
            }
            n += 1;
            continue;
        }

        (*ve).base.class = (*sibling).class;
        (*ve).base.uabi_class = (*sibling).uabi_class;
        snprintf!(
            (*ve).base.name.as_mut_ptr(),
            (*ve).base.name.len(),
            "v%dx%d",
            (*ve).base.class,
            count
        );
        (*ve).base.context_size = (*sibling).context_size;

        (*ve).base.add_active_request = (*sibling).add_active_request;
        (*ve).base.remove_active_request = (*sibling).remove_active_request;
        (*ve).base.emit_bb_start = (*sibling).emit_bb_start;
        (*ve).base.emit_flush = (*sibling).emit_flush;
        (*ve).base.emit_init_breadcrumb = (*sibling).emit_init_breadcrumb;
        (*ve).base.emit_fini_breadcrumb = (*sibling).emit_fini_breadcrumb;
        (*ve).base.emit_fini_breadcrumb_dw = (*sibling).emit_fini_breadcrumb_dw;
        (*ve).base.flags = (*sibling).flags;
        n += 1;
    }

    (*ve).base.flags |= I915_ENGINE_IS_VIRTUAL;
    virtual_engine_initial_hint(ve);
    &mut (*ve).context
}

// upstream: intel_execlists_submission.c intel_execlists_show_requests()
unsafe fn intel_execlists_show_requests(
    engine: *mut IntelEngineCs,
    m: *mut DrmPrinter,
    show_request: unsafe fn(*mut DrmPrinter, *const I915Request, *const c_char, i32),
    max: u32,
) {
    let execlists = &(*engine).execlists;
    let sched_engine = (*engine).sched_engine;
    let mut rq: *mut I915Request = core::ptr::null_mut();
    let mut last: *mut I915Request;
    let mut flags: c_ulong = 0;
    let mut count: u32;
    let mut rb: *mut RbNode;

    spin_lock_irqsave(&mut (*sched_engine).lock, &mut flags);

    last = core::ptr::null_mut();
    count = 0;
    list_for_each_entry!(rq, &mut (*sched_engine).requests, sched.link, {
        if count < max.wrapping_sub(1) {
            show_request(m, rq, c"\t\t".as_ptr(), 0);
        } else {
            last = rq;
        }
        count += 1;
    });
    if !last.is_null() {
        if count > max {
            drm_printf!(m, "\t\t...skipping %d executing requests...\n", count - max);
        }
        show_request(m, last, c"\t\t".as_ptr(), 0);
    }

    if (*sched_engine).queue_priority_hint != INT_MIN {
        drm_printf!(
            m,
            "\t\tQueue priority hint: %d\n",
            READ_ONCE!((*sched_engine).queue_priority_hint)
        );
    }

    last = core::ptr::null_mut();
    count = 0;
    rb = rb_first_cached(&(*sched_engine).queue);
    while !rb.is_null() {
        let p = rb_entry!(rb, I915Priolist, node);
        priolist_for_each_request!(rq, p, {
            if count < max.wrapping_sub(1) {
                show_request(m, rq, c"\t\t".as_ptr(), 0);
            } else {
                last = rq;
            }
            count += 1;
        });
        rb = rb_next(rb);
    }
    if !last.is_null() {
        if count > max {
            drm_printf!(m, "\t\t...skipping %d queued requests...\n", count - max);
        }
        show_request(m, last, c"\t\t".as_ptr(), 0);
    }

    last = core::ptr::null_mut();
    count = 0;
    rb = rb_first_cached(&execlists.r#virtual);
    while !rb.is_null() {
        let ve = rb_entry!(rb, VirtualEngine, nodes[(*engine).id].rb);
        rq = READ_ONCE!((*ve).request);
        if !rq.is_null() {
            if count < max.wrapping_sub(1) {
                show_request(m, rq, c"\t\t".as_ptr(), 0);
            } else {
                last = rq;
            }
            count += 1;
        }
        rb = rb_next(rb);
    }
    if !last.is_null() {
        if count > max {
            drm_printf!(m, "\t\t...skipping %d virtual requests...\n", count - max);
        }
        show_request(m, last, c"\t\t".as_ptr(), 0);
    }

    spin_unlock_irqrestore(&mut (*sched_engine).lock, flags);
}

// upstream: intel_execlists_submission.c intel_execlists_dump_active_requests()
unsafe fn intel_execlists_dump_active_requests(
    engine: *mut IntelEngineCs,
    hung_rq: *mut I915Request,
    m: *mut DrmPrinter,
) {
    let mut flags: c_ulong = 0;

    spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags);
    intel_engine_dump_active_requests(&mut (*(*engine).sched_engine).requests, hung_rq, m);
    drm_printf!(
        m,
        "\tOn hold?: %zu\n",
        list_count_nodes(&mut (*(*engine).sched_engine).hold)
    );
    spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags);
}

// Upstream tail, kept as an integration boundary rather than inlined here:
// #if IS_ENABLED(CONFIG_DRM_I915_SELFTEST)
// #include "selftest_execlists.c"
// #endif
// The self-test implementation is a separate upstream source file.
