// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation
// Upstream: Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_submission.c
//
// This file is registered in lib.rs. It keeps the upstream implementation in
// source order while kernel GT/GEM helper bindings are integrated.

use crate::intel_guc_capture_upstream::{intel_guc_capture_is_matching_engine,intel_guc_capture_process};

use crate::i915_gpu_error_upstream::i915_capture_error_state;

use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::{offset_of, size_of},
};

use crate::{
    guc_ads::EngineUsageRecord as guc_engine_usage_record,
    guc_submission::{
        CACHELINE_BYTES, GUC_INVALID_CONTEXT_ID, GUC_MAX_CONTEXT_ID,
        GuCContextRegistrationInfo as guc_ctxt_registration_info,
        GuCLrcDescV69 as guc_lrc_desc_v69, GuCProcessDescV69 as guc_process_desc_v69,
        GuCSchedWqDesc as guc_sched_wq_desc, GuCWorkQueueItem as guc_wq_item, MAX_ENGINE_INSTANCE,
        PARENT_SCRATCH_SIZE,
    },
    i915_gem_object_types_upstream::DrmI915GemObject as drm_i915_gem_object,
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    i915_request_types_upstream::{
        I915Request as i915_request, i915_request_active_timeline,
        i915_request_notify_execute_cb_imm, i915_test_request_state,
    },
    i915_request_upstream::{__i915_request_skip, __i915_request_submit, __i915_request_unsubmit},
    i915_scheduler_types_upstream::{
        I915SchedAttr, I915SchedEngine as i915_sched_engine, TaskletStruct as tasklet_struct,
        i915_priolist,
    },
    i915_scheduler_upstream::{
        i915_sched_engine_create, i915_sched_lookup_priolist, i915_schedule,
    },
    i915_sw_fence_upstream::{
        i915_sw_fence_commit, i915_sw_fence_complete, i915_sw_fence_fini, i915_sw_fence_reinit,
    },
    i915_vma_api_upstream::{I915_VMA_RELEASE_MAP, *},
    intel_breadcrumbs_types_upstream::intel_breadcrumbs,
    intel_context_api_upstream::intel_context_is_exiting,
    intel_context_types_upstream::{
        CONTEXT_GUC_INIT, CONTEXT_LOW_LATENCY, CONTEXT_LRCA_DIRTY, COPS_RUNTIME_CYCLES,
        I915SwFence as i915_sw_fence, IntelContext as intel_context, IntelContextOps,
        IntelWakerefT, *,
    },
    intel_context_upstream::{
        I915GemWwCtx as i915_gem_ww_ctx, I915Vma as i915_vma, IrqWork as irq_work, Kref as kref,
        WaitQueueEntry as wait_queue_entry, WaitQueueHead as wait_queue_head,
        intel_context_bind_parent_child, intel_context_init,
    },
    intel_engine_api_upstream::{drm_clflush_virt_range, intel_engine_dump_active_requests},
    intel_engine_cs_upstream::{
        ALL_ENGINES, AtomicT as atomic_t, COMPUTE_CLASS, DelayedWork as delayed_work,
        I915_NUM_ENGINES, ListHead as list_head, LlistHead as llist_head, LlistNode as llist_node,
        Mutex as mutex, RbNode as rb_node, Spinlock as spinlock_t, WorkStruct as work_struct,
    },
    intel_engine_heartbeat_upstream::{intel_gt_park_heartbeats, intel_gt_unpark_heartbeats},
    intel_engine_types_upstream::{
        IntelEngineCs as intel_engine_cs, IntelEngineId as intel_engine_id_t,
        IntelEngineMask as intel_engine_mask_t, RENDER_CLASS, VIRTUAL_ENGINES,
    },
    intel_gt_api_upstream::guc_to_i915,
    intel_reset_upstream::{intel_gt_reset_trylock, intel_gt_reset_unlock},
    intel_gt_types_upstream::IntelGt as intel_gt,
    gen8_engine_cs_upstream::gen8_emit_ggtt_write,
    intel_guc_actions_abi_types_upstream::{
        INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_MASK, INTEL_GUC_TLB_INVAL_FLUSH_CACHE,
        INTEL_GUC_TLB_INVAL_MODE_MASK, INTEL_GUC_TLB_INVAL_TYPE_MASK, *,
    },
    intel_guc_ct_types_upstream::{
        IntelGucCt, IntelGucCtBuffer, IntelGucCtBuffers, IntelGucCtRequests,
    },
    intel_guc_fwif_types_upstream::{
        CONTEXT_POLICY_FLAG_PREEMPT_TO_IDLE_V69, CONTEXT_REGISTRATION_FLAG_KMD,
        G2H_LEN_DW_DEREGISTER_CONTEXT, G2H_LEN_DW_INVALIDATE_TLB,
        G2H_LEN_DW_SCHED_CONTEXT_MODE_SET, GLOBAL_SCHEDULE_POLICY_RC_YIELD_DURATION,
        GLOBAL_SCHEDULE_POLICY_RC_YIELD_RATIO, GUC_CLIENT_PRIORITY_HIGH,
        GUC_CLIENT_PRIORITY_KMD_HIGH, GUC_CLIENT_PRIORITY_KMD_NORMAL, GUC_CLIENT_PRIORITY_NORMAL,
        GUC_CLIENT_PRIORITY_NUM, GUC_CONTEXT_DISABLE, GUC_CONTEXT_ENABLE,
        GUC_CONTEXT_POLICIES_KLV_NUM_IDS, WQ_GUC_ID_MASK, WQ_LEN_MASK, WQ_RING_TAIL_MASK,
        WQ_STATUS_ACTIVE, WQ_TYPE_MASK, WQ_TYPE_MULTI_LRC, WQ_TYPE_NOOP, engine_class_to_guc_class,
        guc_class_to_engine_class, *,
    },
    intel_guc_log_types_upstream::IntelGucLog as IntelGucLogLayout,
    intel_guc_slpc_types_upstream::IntelGucSlpc as IntelGucSlpcLayout,
    intel_guc_submission_types_upstream::intel_guc_submission_is_supported,
    intel_guc_upstream::{
        intel_guc_engine_usage_offset, intel_guc_engine_usage_record_map, intel_guc_ggtt_offset,
        intel_guc_write_barrier,
    },
    linux::i915::HAS_GUC_TLB_INVALIDATION,
    intel_guc_types_upstream::{
        IntelGuc, IntelGucInterrupts, IntelGucSendRegs, IntelGucSubmissionState, IntelGucTimestamp,
        IntelGucTlbWait as intel_guc_tlb_wait, intel_guc_is_fw_running, intel_guc_is_supported,
    },
    intel_ring_upstream::intel_ring_begin,
    intel_timeline_types_upstream::IntelTimeline,
    intel_uc_fw_types_upstream::{
        __intel_uc_fw_status, INTEL_UC_FIRMWARE_RUNNING, IntelUcFw as IntelUcFwLayout,
        IntelUcFwVersion as IntelUcFwVer,
    },
    intel_uc_types_upstream::{IntelUc as intel_uc, intel_uc_uses_guc_submission},
    intel_uncore_types_upstream::{
        IntelRuntimePm, assert_forcewakes_active, intel_uncore_read, intel_uncore_read64_2x32,
        intel_uncore_write,
    },
    intel_workarounds_types_upstream::I915RegT as i915_reg_t,
    linux::{
        i915_trace::{trace_intel_context_deregister_done, trace_intel_context_fence_release, trace_intel_context_reset, trace_intel_context_sched_done, trace_intel_context_set_prio},
        bitmap::{bitmap_find_free_region, bitmap_free, bitmap_release_region, bitmap_zalloc},
        idr::{Ida, ida_alloc_range, ida_free, ida_init},
        iosys_map::IosysMap,
        primitives::{max_t, min_t, time_after},
        wait::{msleep, schedule_timeout},
        xarray::{XArray, xa_init_flags},
    },
    linux_config::*,
};

// Exported Linux/i915 functions not yet surfaced through a shared Rust owner.
// These are direct ABI bindings, not substitute implementations.
unsafe extern "C" {
    fn might_sleep();
    fn signal_pending_state(state: i32, task: *mut c_void) -> bool;
    fn io_schedule_timeout(timeout: i64) -> i64;
    fn prepare_to_wait(wq: *const wait_queue_head, wait: *mut wait_queue_entry, state: i32);
    fn finish_wait(wq: *const wait_queue_head, wait: *mut wait_queue_entry);
    fn set_current_state(state: i32);
    fn __set_current_state(state: i32);
    fn __i915_request_reset(rq: *mut i915_request, guilty: bool);
    fn i915_priolist_free(pl: *mut i915_priolist);
    fn trace_i915_request_in(rq: *mut i915_request, port: u32);
    fn trace_i915_request_guc_submit(rq: *mut i915_request);
    fn trace_intel_context_sched_enable(ce: *mut intel_context);
    fn trace_intel_context_sched_disable(ce: *const intel_context);
    fn trace_intel_context_register(ce: *mut intel_context);
    fn trace_intel_context_deregister(ce: *mut intel_context);
    fn trace_intel_context_steal_guc_id(ce: *const intel_context);
    fn intel_guc_send_busy_loop(
        guc: *mut IntelGuc,
        action: *const u32,
        len: u32,
        g2h_len_dw: u32,
        loop_on_busy: bool,
    ) -> i32;
    fn dma_fence_context_alloc(num: usize) -> u64;
    fn intel_engine_reset_pinned_contexts(engine: *mut intel_engine_cs);
    fn intel_mocs_init_engine(engine: *mut intel_engine_cs);
    fn gen8_emit_flush_xcs(rq: *mut i915_request, mode: u32) -> i32;
    fn gen8_emit_init_breadcrumb(rq: *mut i915_request) -> i32;
    fn gen8_emit_fini_breadcrumb_xcs(rq: *mut i915_request, cs: *mut u32) -> *mut u32;
    fn gen12_emit_fini_breadcrumb_xcs(rq: *mut i915_request, cs: *mut u32) -> *mut u32;
    fn gen12_emit_flush_xcs(rq: *mut i915_request, mode: u32) -> i32;
    fn gen8_emit_bb_start(rq: *mut i915_request, offset: u64, len: u32, flags: u32) -> i32;
    fn xehp_emit_bb_start(rq: *mut i915_request, offset: u64, len: u32, flags: u32) -> i32;
    fn gen12_emit_flush_rcs(rq: *mut i915_request, mode: u32) -> i32;
    fn gen12_emit_fini_breadcrumb_rcs(rq: *mut i915_request, cs: *mut u32) -> *mut u32;
    fn gen11_emit_flush_rcs(rq: *mut i915_request, mode: u32) -> i32;
    fn gen11_emit_fini_breadcrumb_rcs(rq: *mut i915_request, cs: *mut u32) -> *mut u32;
    fn gen8_emit_flush_rcs(rq: *mut i915_request, mode: u32) -> i32;
    fn gen8_emit_fini_breadcrumb_rcs(rq: *mut i915_request, cs: *mut u32) -> *mut u32;
    fn intel_runtime_pm_get_if_active(rpm: *mut IntelRuntimePm) -> IntelWakerefT;
    fn intel_guc_ct_send(
        ct: *mut IntelGucCt,
        action: *const u32,
        len: u32,
        response: *mut u32,
        response_len: u32,
        flags: u32,
    ) -> i32;
    fn intel_guc_allocate_and_map_vma(
        guc: *mut IntelGuc,
        size: u32,
        vma: *mut *mut i915_vma,
        vaddr: *mut *mut c_void,
    ) -> i32;
    fn intel_gt_retire_requests_timeout(
        gt: *mut intel_gt,
        timeout: i64,
        remaining_timeout: *mut i64,
    ) -> i64;
    fn intel_gt_handle_error(
        gt: *mut intel_gt,
        engine_mask: u32,
        flags: c_ulong,
        fmt: *const c_char,
        ...
    );
}

#[allow(non_snake_case)]
fn NUM_SCHED_DISABLE_GUCIDS_DEFAULT_THRESHOLD(guc: &intel_guc) -> i32 {
    (intel_guc_sched_disable_gucid_threshold_max(guc) * 3) / 4
}

unsafe fn intel_engine_set_irq_handler(
    engine: *mut intel_engine_cs,
    handler: unsafe extern "C" fn(*mut intel_engine_cs, u16),
) {
    // SAFETY: this mirrors the source header's single-writer IRQ callback
    // publication and is called during engine setup.
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    unsafe { (*engine).irq_handler = Some(handler) };
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

#[inline]
fn intel_guc_send(guc: &mut IntelGuc, action: &[u32], len: u32) -> i32 {
    unsafe {
        intel_guc_ct_send(
            &mut guc.ct,
            action.as_ptr(),
            len,
            core::ptr::null_mut(),
            0,
            0,
        )
    }
}

#[inline]
pub(crate) fn intel_guc_send_nb(guc: &mut IntelGuc, action: &[u32], g2h_len_dw: u32) -> i32 {
    const INTEL_GUC_CT_SEND_NB: u32 = 1 << 31;
    const INTEL_GUC_CT_SEND_G2H_DW_MASK: u32 = 0xff;
    gem_bug_on!(g2h_len_dw & !INTEL_GUC_CT_SEND_G2H_DW_MASK != 0);
    unsafe {
        intel_guc_ct_send(
            &mut guc.ct,
            action.as_ptr(),
            action.len() as u32,
            core::ptr::null_mut(),
            0,
            INTEL_GUC_CT_SEND_NB | g2h_len_dw,
        )
    }
}

#[inline]
fn intel_gt_retire_requests(gt: *mut intel_gt) {
    unsafe {
        let _ = intel_gt_retire_requests_timeout(gt, 0, core::ptr::null_mut());
    }
}

#[inline]
unsafe fn i915_sw_fence_wait(fence: *mut i915_sw_fence) {
    wait_until(
        u64::MAX,
        || unsafe { crate::linux::sw_fence::i915_sw_fence_done(&*fence) },
        true,
    );
}

#[allow(non_camel_case_types)]
type ktime_t = i64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GuCUpdateContextPolicyHeader {
    action: u32,
    ctx_id: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct GuCKlvGenericDw {
    kl: u32,
    value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GuCUpdateContextPolicy {
    header: GuCUpdateContextPolicyHeader,
    klv: [GuCKlvGenericDw; 5],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ContextPolicy {
    count: u32,
    h2g: GuCUpdateContextPolicy,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GuCUpdateSchedulingPolicy {
    action: u32,
    data: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SchedulingPolicy {
    max_words: u32,
    num_words: u32,
    count: u32,
    h2g: GuCUpdateSchedulingPolicy,
}

#[repr(C)]
#[derive(Clone, Copy)]
union GuCParentDescriptors {
    wq_desc: guc_sched_wq_desc,
    pdesc: guc_process_desc_v69,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct sync_semaphore {
    semaphore: u32,
    unused: [u8; 60],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct parent_scratch {
    descs: GuCParentDescriptors,
    go: sync_semaphore,
    join: [sync_semaphore; MAX_ENGINE_INSTANCE + 1],
    unused: [u8; 1216],
    wq: [u32; 512],
}

#[allow(non_camel_case_types)]
type intel_guc = IntelGuc;
#[allow(non_camel_case_types)]
type intel_guc_ct = IntelGucCt;
#[allow(non_camel_case_types)]
type intel_guc_ct_buffer = IntelGucCtBuffer;
#[allow(non_camel_case_types)]
type intel_guc_ct_buffers = IntelGucCtBuffers;
#[allow(non_camel_case_types)]
type intel_guc_ct_requests = IntelGucCtRequests;

trait IntelGucPtr {
    fn intel_guc_ptr(self) -> *const IntelGuc;
}
impl IntelGucPtr for *const IntelGuc {
    fn intel_guc_ptr(self) -> *const IntelGuc {
        self
    }
}
impl IntelGucPtr for *mut IntelGuc {
    fn intel_guc_ptr(self) -> *const IntelGuc {
        self.cast_const()
    }
}
impl IntelGucPtr for &IntelGuc {
    fn intel_guc_ptr(self) -> *const IntelGuc {
        self
    }
}
impl IntelGucPtr for &mut IntelGuc {
    fn intel_guc_ptr(self) -> *const IntelGuc {
        self
    }
}

// upstream: intel_guc.h intel_guc_is_ready()
unsafe fn intel_guc_is_ready<G: IntelGucPtr>(guc: G) -> bool {
    let guc = guc.intel_guc_ptr();
    assert!(!guc.is_null());
    unsafe { __intel_uc_fw_status(&(*guc).fw) == INTEL_UC_FIRMWARE_RUNNING && (*guc).ct.enabled }
}

// upstream: intel_guc_ads.c intel_guc_global_policies_update().
fn intel_guc_global_policies_update(guc: &mut intel_guc) -> i32 {
    let guc_ptr = guc as *mut intel_guc;
    let mapped_len = unsafe {
        GEM_BUG_ON!((*guc_ptr).ads_vma.is_null());
        if (*guc_ptr).ads_vma.is_null() {
            return -EOPNOTSUPP;
        }
        (*(*guc_ptr).ads_vma).size as usize
    };
    let i915 = unsafe { guc_to_i915(guc_ptr) };
    let policy_address = unsafe {
        crate::guc_ads::runtime_policy_init(
            core::ptr::addr_of_mut!((*guc_ptr).ads_map),
            mapped_len,
            (*i915).params.reset as u8,
        )
    };
    let Some(policy_address) = policy_address else {
        return -EOPNOTSUPP;
    };
    GEM_BUG_ON!(policy_address == 0);
    if !unsafe { intel_guc_is_ready(&*guc_ptr) } {
        return 0;
    }

    let action = [crate::guc_ads::ACTION_GLOBAL_SCHED_POLICY_CHANGE, policy_address];
    unsafe { intel_guc_send_busy_loop(guc_ptr, action.as_ptr(), action.len() as u32, 0, true) }
}

// `GUC_SUBMIT_VER(guc)` from intel_guc.h, adapted from the C macro to a typed
// helper so the private firmware-version layout stays encapsulated here.
#[allow(non_snake_case)]
fn GUC_SUBMIT_VER(guc: &IntelGuc) -> u32 {
    MAKE_GUC_VER(
        guc.submission_version.major,
        guc.submission_version.minor,
        guc.submission_version.patch,
    )
}

// Source macro from intel_guc_submission.c; 1/16 of IDs are reserved for
// contiguous multi-LRC contexts.
#[allow(non_snake_case)]
fn NUMBER_MULTI_LRC_GUC_ID(guc: &IntelGuc) -> i32 {
    guc.submission_state.num_guc_ids / 16
}
#[allow(non_camel_case_types)]
type intel_guc_log = IntelGucLogLayout;
#[allow(non_camel_case_types)]
type intel_uc_fw = IntelUcFwLayout;
#[allow(non_camel_case_types)]
type intel_guc_slpc = IntelGucSlpcLayout;
#[allow(non_camel_case_types)]
type xarray = XArray;
#[allow(non_camel_case_types)]
type ida = Ida;
#[allow(non_camel_case_types)]
type iosys_map = IosysMap;
#[allow(non_camel_case_types)]
type intel_uc_fw_ver = IntelUcFwVer;
#[allow(non_camel_case_types)]
type intel_guc_interrupts = IntelGucInterrupts;
#[allow(non_camel_case_types)]
type intel_guc_submission_state = IntelGucSubmissionState;
#[allow(non_camel_case_types)]
type intel_guc_timestamp = IntelGucTimestamp;
#[allow(non_camel_case_types)]
type context_policy = ContextPolicy;
#[allow(non_camel_case_types)]
type scheduling_policy = SchedulingPolicy;
#[allow(non_camel_case_types)]
type guc_update_context_policy = GuCUpdateContextPolicy;
#[allow(non_camel_case_types)]
type guc_update_scheduling_policy = GuCUpdateSchedulingPolicy;
#[allow(non_camel_case_types)]
type guc_klv_generic_dw_t = GuCKlvGenericDw;
#[allow(non_camel_case_types)]
type context_policy_header = GuCUpdateContextPolicyHeader;
#[allow(non_camel_case_types)]
type guc_klv = GuCKlvGenericDw;

// intel_guc_submission.c embeds the engine and context records in this exact
// order for a virtual engine.  The record is private to this source unit.
#[repr(C)]
struct guc_virtual_engine {
    base: intel_engine_cs,
    context: intel_context,
}

// Layout checks against the Linux v7.2.3 x86_64 i915 headers and the local
// intel_guc_submission.c records. Keep the by-value ABI records in sync with
// their upstream C counterparts.
const _: () = {
    assert!(size_of::<XArray>() == 16);
    assert!(size_of::<Ida>() == 16);
    assert!(size_of::<IosysMap>() == 16);
    assert!(size_of::<IntelGucCtBuffer>() == 48);
    assert!(size_of::<IntelGucCt>() == 256);
    assert!(size_of::<IntelGucTimestamp>() == 128);
    assert!(size_of::<intel_guc_tlb_wait>() == 32);
    assert!(size_of::<GuCUpdateContextPolicy>() == 48);
    assert!(size_of::<ContextPolicy>() == 52);
    assert!(size_of::<GuCUpdateSchedulingPolicy>() == 16);
    assert!(size_of::<SchedulingPolicy>() == 28);
    assert!(size_of::<sync_semaphore>() == CACHELINE_BYTES);
    assert!(size_of::<parent_scratch>() == PARENT_SCRATCH_SIZE);
    assert!(offset_of!(parent_scratch, wq) == PARENT_SCRATCH_SIZE / 2);
    assert!(offset_of!(IntelGuc, ads_vma) == 1304);
    assert!(offset_of!(IntelGuc, params) == 1496);
    assert!(offset_of!(IntelGuc, timestamp) == 1600);
    assert!(offset_of!(IntelGuc, last_dead_guc_jiffies) == 1760);
    assert!(size_of::<IntelGuc>() == 1768);
};

const GUC_REQUEST_SIZE: usize = 64;
const NUMBER_MULTI_LRC_GUC_ID_DIVISOR: u32 = 16;

const SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER: u32 = 1 << 0;
const SCHED_STATE_DESTROYED: u32 = 1 << 1;
const SCHED_STATE_PENDING_DISABLE: u32 = 1 << 2;
const SCHED_STATE_BANNED: u32 = 1 << 3;
const SCHED_STATE_ENABLED: u32 = 1 << 4;
const SCHED_STATE_PENDING_ENABLE: u32 = 1 << 5;
const SCHED_STATE_REGISTERED: u32 = 1 << 6;
const SCHED_STATE_POLICY_REQUIRED: u32 = 1 << 7;
const SCHED_STATE_CLOSED: u32 = 1 << 8;
const SCHED_STATE_BLOCKED_SHIFT: u32 = 9;
const SCHED_STATE_BLOCKED: u32 = 1 << SCHED_STATE_BLOCKED_SHIFT;
const SCHED_STATE_BLOCKED_MASK: u32 = 0xfff << SCHED_STATE_BLOCKED_SHIFT;
const SCHED_STATE_VALID_INIT: u32 =
    SCHED_STATE_BLOCKED_MASK | SCHED_STATE_CLOSED | SCHED_STATE_REGISTERED;

// upstream: intel_guc_submission.c init_sched_state()
#[inline]
fn init_sched_state(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= SCHED_STATE_BLOCKED_MASK;
}

// upstream: intel_guc_submission.c sched_state_is_init()
#[allow(dead_code)]
fn sched_state_is_init(ce: &intel_context) -> bool {
    (ce.guc_state.sched_state & !SCHED_STATE_VALID_INIT) == 0
}

// upstream: intel_guc_submission.c context_wait_for_deregister_to_register()
#[inline]
fn context_wait_for_deregister_to_register(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER != 0
}

// upstream: intel_guc_submission.c set_context_wait_for_deregister_to_register()
#[inline]
fn set_context_wait_for_deregister_to_register(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER;
}

// upstream: intel_guc_submission.c clr_context_wait_for_deregister_to_register()
#[inline]
fn clr_context_wait_for_deregister_to_register(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER;
}

// upstream: intel_guc_submission.c context_destroyed()
#[inline]
fn context_destroyed(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_DESTROYED != 0
}

// upstream: intel_guc_submission.c set_context_destroyed()
#[inline]
fn set_context_destroyed(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_DESTROYED;
}

// upstream: intel_guc_submission.c clr_context_destroyed()
#[inline]
fn clr_context_destroyed(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_DESTROYED;
}

// upstream: intel_guc_submission.c context_pending_disable()
#[inline]
fn context_pending_disable(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_PENDING_DISABLE != 0
}

// upstream: intel_guc_submission.c set_context_pending_disable()
#[inline]
fn set_context_pending_disable(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_PENDING_DISABLE;
}

// upstream: intel_guc_submission.c clr_context_pending_disable()
#[inline]
fn clr_context_pending_disable(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_PENDING_DISABLE;
}

// upstream: intel_guc_submission.c context_banned()
#[inline]
fn context_banned(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_BANNED != 0
}

// upstream: intel_guc_submission.c set_context_banned()
#[inline]
fn set_context_banned(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_BANNED;
}

// upstream: intel_guc_submission.c clr_context_banned()
#[inline]
fn clr_context_banned(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_BANNED;
}

// upstream: intel_guc_submission.c context_enabled()
#[inline]
fn context_enabled(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_ENABLED != 0
}

// upstream: intel_guc_submission.c set_context_enabled()
#[inline]
fn set_context_enabled(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_ENABLED;
}

// upstream: intel_guc_submission.c clr_context_enabled()
#[inline]
fn clr_context_enabled(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_ENABLED;
}

// upstream: intel_guc_submission.c context_pending_enable()
#[inline]
fn context_pending_enable(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_PENDING_ENABLE != 0
}

// upstream: intel_guc_submission.c set_context_pending_enable()
#[inline]
fn set_context_pending_enable(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_PENDING_ENABLE;
}

// upstream: intel_guc_submission.c clr_context_pending_enable()
#[inline]
fn clr_context_pending_enable(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_PENDING_ENABLE;
}

// upstream: intel_guc_submission.c context_registered()
#[inline]
fn context_registered(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_REGISTERED != 0
}

// upstream: intel_guc_submission.c set_context_registered()
#[inline]
fn set_context_registered(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_REGISTERED;
}

// upstream: intel_guc_submission.c clr_context_registered()
#[inline]
fn clr_context_registered(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_REGISTERED;
}

// upstream: intel_guc_submission.c context_policy_required()
#[inline]
fn context_policy_required(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_POLICY_REQUIRED != 0
}

// upstream: intel_guc_submission.c set_context_policy_required()
#[inline]
fn set_context_policy_required(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_POLICY_REQUIRED;
}

// upstream: intel_guc_submission.c clr_context_policy_required()
#[inline]
fn clr_context_policy_required(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state &= !SCHED_STATE_POLICY_REQUIRED;
}

// upstream: intel_guc_submission.c context_close_done()
#[inline]
fn context_close_done(ce: &intel_context) -> bool {
    ce.guc_state.sched_state & SCHED_STATE_CLOSED != 0
}

// upstream: intel_guc_submission.c set_context_close_done()
#[inline]
fn set_context_close_done(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state |= SCHED_STATE_CLOSED;
}

// upstream: intel_guc_submission.c context_blocked()
#[inline]
fn context_blocked(ce: &intel_context) -> u32 {
    (ce.guc_state.sched_state & SCHED_STATE_BLOCKED_MASK) >> SCHED_STATE_BLOCKED_SHIFT
}

// upstream: intel_guc_submission.c incr_context_blocked()
#[inline]
fn incr_context_blocked(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state += SCHED_STATE_BLOCKED;
    gem_bug_on!(context_blocked(ce) == 0); // Overflow check
}

// upstream: intel_guc_submission.c decr_context_blocked()
#[inline]
fn decr_context_blocked(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    gem_bug_on!(context_blocked(ce) == 0); // Underflow check
    ce.guc_state.sched_state -= SCHED_STATE_BLOCKED;
}

// upstream: intel_guc_submission.c request_to_scheduling_context()
fn request_to_scheduling_context(rq: &i915_request) -> &intel_context {
    unsafe { &*intel_context_to_parent(rq.context) }
}

#[inline]
unsafe fn request_to_scheduling_context_mut(rq: &mut i915_request) -> &mut intel_context {
    // The request owns the context reference; callers must preserve the source
    // scheduler/object-lock serialization while mutating that context.
    unsafe { &mut *intel_context_to_parent(rq.context) }
}

// upstream: intel_guc_submission.c context_guc_id_invalid()
#[inline]
fn context_guc_id_invalid(ce: &intel_context) -> bool {
    ce.guc_id.id == GUC_INVALID_CONTEXT_ID as u16
}

// upstream: intel_guc_submission.c set_context_guc_id_invalid()
#[inline]
fn set_context_guc_id_invalid(ce: &mut intel_context) {
    ce.guc_id.id = GUC_INVALID_CONTEXT_ID as u16;
}

// upstream: intel_guc_submission.c ce_to_guc()
#[inline]
fn ce_to_guc(ce: &intel_context) -> &intel_guc {
    unsafe { &*gt_to_guc((*ce.engine).gt) }
}

#[inline]
fn ce_to_guc_mut(ce: *mut intel_context) -> *mut intel_guc {
    unsafe {
        let engine = (*ce).engine;
        let gt = (*engine).gt;
        core::ptr::addr_of_mut!((*gt).uc.guc)
    }
}

#[inline]
fn guc_to_gt_const(guc: *const intel_guc) -> *const intel_gt {
    unsafe {
        guc.cast::<u8>()
            .sub(offset_of!(intel_gt, uc) + offset_of!(intel_uc, guc))
            .cast::<intel_gt>()
    }
}

#[inline]
fn guc_to_i915_const(guc: *const intel_guc) -> *mut crate::linux_i915_private::DrmI915Private {
    unsafe { (*guc_to_gt_const(guc)).i915 }
}

// upstream: intel_guc_submission.c to_priolist()
#[inline]
fn to_priolist(rb: &rb_node) -> &i915_priolist {
    unsafe { &*rb_entry!(rb, i915_priolist, node) }
}

const WQ_SIZE: usize = PARENT_SCRATCH_SIZE / 2;
const WQ_OFFSET: usize = PARENT_SCRATCH_SIZE - WQ_SIZE;

// upstream: intel_guc_submission.c __get_parent_scratch_offset()
fn __get_parent_scratch_offset(ce: &intel_context) -> u32 {
    gem_bug_on!(ce.parallel.guc.parent_page == 0);
    ce.parallel.guc.parent_page as u32 * PAGE_SIZE as u32
}

// upstream: intel_guc_submission.c __get_wq_offset()
fn __get_wq_offset(ce: &intel_context) -> u32 {
    build_bug_on!(offset_of!(parent_scratch, wq) != WQ_OFFSET);
    __get_parent_scratch_offset(ce) + WQ_OFFSET as u32
}

// upstream: intel_guc_submission.c __get_parent_scratch()
fn __get_parent_scratch(ce: &mut intel_context) -> *mut parent_scratch {
    build_bug_on!(size_of::<parent_scratch>() != PARENT_SCRATCH_SIZE);
    build_bug_on!(size_of::<sync_semaphore>() != CACHELINE_BYTES);

    // The parent_page is relative to ce.state, whereas lrc_reg_state starts
    // at ce.state + LRC_STATE_OFFSET.
    let offset =
        (__get_parent_scratch_offset(ce) - LRC_STATE_OFFSET as u32) / size_of::<u32>() as u32;
    unsafe { ce.lrc_reg_state.add(offset as usize).cast() }
}

// upstream: intel_guc_submission.c __get_process_desc_v69()
fn __get_process_desc_v69(ce: &mut intel_context) -> *mut guc_process_desc_v69 {
    let ps = __get_parent_scratch(ce);
    unsafe { core::ptr::addr_of_mut!((*ps).descs.pdesc) }
}

// upstream: intel_guc_submission.c __get_wq_desc_v70()
fn __get_wq_desc_v70(ce: &mut intel_context) -> *mut guc_sched_wq_desc {
    let ps = __get_parent_scratch(ce);
    unsafe { core::ptr::addr_of_mut!((*ps).descs.wq_desc) }
}

// upstream: intel_guc_submission.c get_wq_pointer()
fn get_wq_pointer(ce: &mut intel_context, wqi_size: u32) -> *mut u32 {
    let available_space = circ_space(
        ce.parallel.guc.wqi_tail as u32,
        ce.parallel.guc.wqi_head as u32,
        WQ_SIZE as u32,
    );
    if wqi_size > available_space {
        ce.parallel.guc.wqi_head = unsafe { read_once(ce.parallel.guc.wq_head) } as u16;
        if wqi_size
            > circ_space(
                ce.parallel.guc.wqi_tail as u32,
                ce.parallel.guc.wqi_head as u32,
                WQ_SIZE as u32,
            )
        {
            return core::ptr::null_mut();
        }
    }

    unsafe {
        (*__get_parent_scratch(ce))
            .wq
            .as_mut_ptr()
            .add((ce.parallel.guc.wqi_tail as u32 / size_of::<u32>() as u32) as usize)
    }
}

// upstream: intel_guc_submission.c __get_context()
fn __get_context(guc: &mut intel_guc, id: u32) -> *mut intel_context {
    let ce = xa_load(&mut guc.context_lookup, id);
    gem_bug_on!(id >= GUC_MAX_CONTEXT_ID as u32);
    ce
}

// upstream: intel_guc_submission.c __get_lrc_desc_v69()
fn __get_lrc_desc_v69(guc: &mut intel_guc, index: u32) -> *mut guc_lrc_desc_v69 {
    let base = guc.lrc_desc_pool_vaddr_v69.cast::<guc_lrc_desc_v69>();
    if base.is_null() {
        return core::ptr::null_mut();
    }
    gem_bug_on!(index >= GUC_MAX_CONTEXT_ID as u32);
    unsafe { base.add(index as usize) }
}

// upstream: intel_guc_submission.c guc_lrc_desc_pool_create_v69()
fn guc_lrc_desc_pool_create_v69(guc: &mut intel_guc) -> i32 {
    let size = page_align(size_of::<guc_lrc_desc_v69>() * GUC_MAX_CONTEXT_ID as usize) as u32;
    let ret = unsafe {
        intel_guc_allocate_and_map_vma(
            guc,
            size,
            &mut guc.lrc_desc_pool_v69,
            &mut guc.lrc_desc_pool_vaddr_v69,
        )
    };
    if ret != 0 {
        return ret;
    }
    0
}

// upstream: intel_guc_submission.c guc_lrc_desc_pool_destroy_v69()
fn guc_lrc_desc_pool_destroy_v69(guc: &mut intel_guc) {
    if guc.lrc_desc_pool_vaddr_v69.is_null() {
        return;
    }
    guc.lrc_desc_pool_vaddr_v69 = core::ptr::null_mut();
    unsafe { i915_vma_unpin_and_release(&mut guc.lrc_desc_pool_v69, I915_VMA_RELEASE_MAP) };
}

// upstream: intel_guc_submission.c guc_submission_initialized()
#[inline]
fn guc_submission_initialized(guc: &intel_guc) -> bool {
    guc.submission_initialized
}

// upstream: intel_guc_submission.c _reset_lrc_desc_v69()
#[inline]
fn _reset_lrc_desc_v69(guc: &mut intel_guc, id: u32) {
    let desc = __get_lrc_desc_v69(guc, id);
    if !desc.is_null() {
        unsafe { core::ptr::write_bytes(desc, 0, 1) };
    }
}

// upstream: intel_guc_submission.c ctx_id_mapped()
#[inline]
fn ctx_id_mapped(guc: &mut intel_guc, id: u32) -> bool {
    !__get_context(guc, id).is_null()
}

// upstream: intel_guc_submission.c set_ctx_id_mapping()
#[inline]
fn set_ctx_id_mapping(guc: &mut intel_guc, id: u32, ce: *mut intel_context) {
    let mut flags: c_ulong = 0;
    // The xarray API has no xa_save_irqsave wrapper, so invoke the lower-level
    // lock/store/unlock functions directly.
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    __xa_store(&mut guc.context_lookup, id, ce, GFP_ATOMIC);
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
}

// upstream: intel_guc_submission.c clr_ctx_id_mapping()
#[inline]
fn clr_ctx_id_mapping(guc: &mut intel_guc, id: u32) {
    let mut flags: c_ulong = 0;
    if !guc_submission_initialized(guc) {
        return;
    }
    _reset_lrc_desc_v69(guc, id);

    // The xarray API has no xa_erase_irqsave wrapper, so invoke the lower-level
    // lock/erase/unlock functions directly.
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    __xa_erase::<_, intel_context>(&mut guc.context_lookup, id);
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
}

// upstream: intel_guc_submission.c decr_outstanding_submission_g2h()
fn decr_outstanding_submission_g2h(guc: &mut intel_guc) {
    if atomic_dec_and_test(&mut guc.outstanding_submission_g2h) {
        wake_up_all(&mut guc.ct.wq);
    }
}

// upstream: intel_guc_submission.c guc_submission_send_busy_loop()
fn guc_submission_send_busy_loop(
    guc: &mut intel_guc,
    action: &[u32],
    len: u32,
    g2h_len_dw: u32,
    loop_on_busy: bool,
) -> i32 {
    // A send requiring a reply always loops; there is no reply when an aborted
    // busy-channel send was requested without looping.
    gem_bug_on!(g2h_len_dw != 0 && !loop_on_busy);
    if g2h_len_dw != 0 {
        atomic_inc(&mut guc.outstanding_submission_g2h);
    }

    let ret =
        unsafe { intel_guc_send_busy_loop(guc, action.as_ptr(), len, g2h_len_dw, loop_on_busy) };
    if ret != 0 && g2h_len_dw != 0 {
        atomic_dec(&mut guc.outstanding_submission_g2h);
    }
    ret
}

// upstream: intel_guc_submission.c intel_guc_wait_for_pending_msg()
fn intel_guc_wait_for_pending_msg(
    guc: &intel_guc,
    wait_var: &atomic_t,
    interruptible: bool,
    mut timeout: i64,
) -> i32 {
    let state = if interruptible {
        TASK_INTERRUPTIBLE
    } else {
        TASK_UNINTERRUPTIBLE
    } as i32;
    let mut wait = DEFINE_WAIT!();

    unsafe { might_sleep() };
    gem_bug_on!(timeout < 0);
    if atomic_read(wait_var) == 0 {
        return 0;
    }
    if timeout == 0 {
        return -ETIME;
    }

    loop {
        unsafe { prepare_to_wait(&guc.ct.wq, &mut wait, state) };
        if atomic_read(wait_var) == 0 {
            break;
        }
        if unsafe {
            signal_pending_state(
                state,
                axhal::percpu::current_task_ptr::<()>() as *mut c_void,
            )
        } {
            timeout = -(EINTR as i64);
            break;
        }
        if timeout == 0 {
            timeout = -(ETIME as i64);
            break;
        }
        timeout = unsafe { io_schedule_timeout(timeout) };
    }
    unsafe { finish_wait(&guc.ct.wq, &mut wait) };
    if timeout < 0 { timeout as i32 } else { 0 }
}

// upstream: intel_guc_submission.c intel_guc_wait_for_idle()
fn intel_guc_wait_for_idle(guc: &intel_guc, timeout: i64) -> i32 {
    let gt = guc_to_gt_const(guc);
    let uc = unsafe { core::ptr::addr_of!((*gt).uc).cast_mut() };
    if !unsafe { intel_uc_uses_guc_submission(uc) } {
        return 0;
    }
    intel_guc_wait_for_pending_msg(guc, &guc.outstanding_submission_g2h, true, timeout)
}

// upstream: intel_guc_submission.c __guc_add_request()
fn __guc_add_request(guc: &mut intel_guc, rq: &mut i915_request) -> i32 {
    let mut err = 0;
    let rq_ptr = rq as *mut i915_request;
    lockdep_assert_held(unsafe { &(*(*rq.engine).sched_engine).lock });
    let ce = unsafe { request_to_scheduling_context_mut(rq) };
    let mut action = [0u32; 3];
    let mut len = 0usize;
    let mut g2h_len_dw = 0;
    let mut enabled;

    // Requests queued before a context was banned are completed with EIO.
    if unlikely(!intel_context_is_schedulable(&mut *ce)) {
        i915_request_put(i915_request_mark_eio(unsafe { &mut *rq_ptr }));
        unsafe { intel_engine_signal_breadcrumbs(ce.engine) };
        return 0;
    }

    gem_bug_on!(atomic_read(&ce.guc_id.r#ref) == 0);
    gem_bug_on!(context_guc_id_invalid(ce));
    if context_policy_required(ce) {
        err = guc_context_policy_init_v70(ce, false);
        if err != 0 {
            return err;
        }
    }

    spin_lock(&mut ce.guc_state.lock);

    // A blocked non-parent request is submitted by the unblock path instead.
    if unlikely(context_blocked(&mut *ce) != 0 && !unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) }) {
        spin_unlock(&mut ce.guc_state.lock);
        return 0;
    }

    enabled = context_enabled(ce) || context_blocked(&mut *ce) != 0;
    if !enabled {
        action[len] = INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_SET;
        len += 1;
        action[len] = ce.guc_id.id as u32;
        len += 1;
        action[len] = GUC_CONTEXT_ENABLE;
        len += 1;
        set_context_pending_enable(ce);
        intel_context_get(&mut *ce);
        g2h_len_dw = G2H_LEN_DW_SCHED_CONTEXT_MODE_SET;
    } else {
        action[len] = INTEL_GUC_ACTION_SCHED_CONTEXT;
        len += 1;
        action[len] = ce.guc_id.id as u32;
        len += 1;
    }

    err = intel_guc_send_nb(guc, &action[..len], g2h_len_dw);
    if !enabled && err == 0 {
        unsafe { trace_intel_context_sched_enable(&mut *ce) };
        atomic_inc(&mut guc.outstanding_submission_g2h);
        set_context_enabled(ce);

        // Multi-LRC needs an additional H2G after enabling to move its tails.
        if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
            action[0] = INTEL_GUC_ACTION_SCHED_CONTEXT;
            err = intel_guc_send_nb(guc, &action[..len - 1], 0);
        }
    } else if !enabled {
        clr_context_pending_enable(ce);
        unsafe { intel_context_put(&mut *ce) };
    }
    if likely(err == 0) {
        unsafe { trace_i915_request_guc_submit(rq_ptr) };
    }

    spin_unlock(&mut ce.guc_state.lock);
    err
}

// upstream: intel_guc_submission.c guc_add_request()
fn guc_add_request(guc: &mut intel_guc, rq: &mut i915_request) -> i32 {
    let ret = __guc_add_request(guc, rq);
    if unlikely(ret == -EBUSY) {
        guc.stalled_request = rq;
        guc.submission_stall_reason = STALL_ADD_REQUEST;
    }
    ret
}

// upstream: intel_guc_submission.c guc_set_lrc_tail()
#[inline]
fn guc_set_lrc_tail(rq: &mut i915_request) {
    unsafe {
        *(*rq.context).lrc_reg_state.add(CTX_RING_TAIL) = intel_ring_set_tail(rq.ring, rq.tail)
    };
}

// upstream: intel_guc_submission.c rq_prio()
#[inline]
fn rq_prio(rq: &i915_request) -> i32 {
    rq.sched.attr.priority
}

// upstream: intel_guc_submission.c is_multi_lrc_rq()
fn is_multi_lrc_rq(rq: &i915_request) -> bool {
    intel_context_is_parallel(rq.context)
}

// upstream: intel_guc_submission.c can_merge_rq()
fn can_merge_rq(rq: &i915_request, last: &i915_request) -> bool {
    core::ptr::eq(
        request_to_scheduling_context(rq),
        request_to_scheduling_context(last),
    )
}

// upstream: intel_guc_submission.c wq_space_until_wrap()
fn wq_space_until_wrap(ce: &intel_context) -> u32 {
    WQ_SIZE as u32 - ce.parallel.guc.wqi_tail as u32
}

// upstream: intel_guc_submission.c write_wqi()
fn write_wqi(ce: &mut intel_context, wqi_size: u32) {
    build_bug_on!(!WQ_SIZE.is_power_of_two());
    // Ensure WQIs are visible before publishing the tail.
    unsafe { intel_guc_write_barrier(ce_to_guc(ce)) };
    ce.parallel.guc.wqi_tail =
        ((ce.parallel.guc.wqi_tail as u32 + wqi_size) & (WQ_SIZE as u32 - 1)) as u16;
    WRITE_ONCE!(*ce.parallel.guc.wq_tail, ce.parallel.guc.wqi_tail as u32);
}

// upstream: intel_guc_submission.c guc_wq_noop_append()
fn guc_wq_noop_append(ce: &mut intel_context) -> i32 {
    let wqi = get_wq_pointer(ce, wq_space_until_wrap(ce));
    let len_dw = wq_space_until_wrap(ce) / size_of::<u32>() as u32 - 1;
    if wqi.is_null() {
        return -EBUSY;
    }
    gem_bug_on!(!field_fit(WQ_LEN_MASK, len_dw));
    unsafe {
        *wqi = field_prep(WQ_TYPE_MASK, WQ_TYPE_NOOP) | field_prep(WQ_LEN_MASK, len_dw);
    }
    ce.parallel.guc.wqi_tail = 0;
    0
}

// upstream: intel_guc_submission.c __guc_wq_item_append()
fn __guc_wq_item_append(rq: &mut i915_request) -> i32 {
    let ce = unsafe { request_to_scheduling_context_mut(rq) };
    let wqi_size = (ce.parallel.number_children as u32 + 4) * size_of::<u32>() as u32;
    let len_dw = wqi_size / size_of::<u32>() as u32 - 1;

    // Ensure the context and descriptor are in the expected state.
    gem_bug_on!(atomic_read(&ce.guc_id.r#ref) == 0);
    gem_bug_on!(context_guc_id_invalid(ce));
    gem_bug_on!(context_wait_for_deregister_to_register(ce));
    gem_bug_on!(!ctx_id_mapped(
        unsafe { &mut *ce_to_guc_mut(ce as *mut _) },
        ce.guc_id.id as u32
    ));

    // Insert a NOOP before this item if writing it would wrap the tail.
    if wqi_size > wq_space_until_wrap(ce) {
        let ret = guc_wq_noop_append(ce);
        if ret != 0 {
            return ret;
        }
    }

    let mut wqi = get_wq_pointer(ce, wqi_size);
    if wqi.is_null() {
        return -EBUSY;
    }
    gem_bug_on!(!field_fit(WQ_LEN_MASK, len_dw));
    unsafe {
        *wqi = field_prep(WQ_TYPE_MASK, WQ_TYPE_MULTI_LRC) | field_prep(WQ_LEN_MASK, len_dw);
        wqi = wqi.add(1);
        *wqi = ce.lrc.lrca;
        wqi = wqi.add(1);
        *wqi = field_prep(WQ_GUC_ID_MASK, ce.guc_id.id as u32)
            | field_prep(WQ_RING_TAIL_MASK, (*ce.ring).tail / size_of::<u64>() as u32);
        wqi = wqi.add(1);
        *wqi = 0; // fence_id
        wqi = wqi.add(1);
        let parent = &mut *ce;
        for_each_child!(parent, child, {
            *wqi = (*(*child).ring).tail / size_of::<u64>() as u32;
            wqi = wqi.add(1);
        });
    }
    write_wqi(ce, wqi_size);
    0
}

// upstream: intel_guc_submission.c guc_wq_item_append()
fn guc_wq_item_append(guc: &mut intel_guc, rq: &mut i915_request) -> i32 {
    let ce = unsafe { request_to_scheduling_context_mut(rq) };
    if unlikely(!intel_context_is_schedulable(ce)) {
        return 0;
    }
    let ret = __guc_wq_item_append(rq);
    if unlikely(ret == -EBUSY) {
        guc.stalled_request = rq;
        guc.submission_stall_reason = STALL_MOVE_LRC_TAIL;
    }
    ret
}

// upstream: intel_guc_submission.c multi_lrc_submit()
fn multi_lrc_submit(rq: &mut i915_request) -> bool {
    let submit_parallel = test_bit(I915_FENCE_FLAG_SUBMIT_PARALLEL, &rq.fence.flags);
    unsafe { intel_ring_set_tail(rq.ring, rq.tail) };
    let ce = unsafe { request_to_scheduling_context_mut(rq) };
    // The final request in a multi-BB execbuf sets SUBMIT_PARALLEL; that tells
    // the backend to submit the context and all parallel-generated requests.
    submit_parallel || !intel_context_is_schedulable(ce)
}

// upstream: intel_guc_submission.c guc_dequeue_one_context()
fn guc_dequeue_one_context(guc: &mut intel_guc) -> bool {
    #[derive(Clone, Copy)]
    enum Stage {
        Scan,
        RegisterContext,
        MoveLrcTail,
        AddRequest,
        Done,
        Deadlock,
        Schedule,
    }

    let sched_engine = guc.sched_engine;
    let mut last: *mut i915_request = core::ptr::null_mut();
    let mut submit = false;
    let mut stage = Stage::Scan;
    let mut ret;
    lockdep_assert_held(unsafe { &(*sched_engine).lock });

    if !guc.stalled_request.is_null() {
        submit = true;
        last = guc.stalled_request;
        stage = match guc.submission_stall_reason {
            STALL_REGISTER_CONTEXT => Stage::RegisterContext,
            STALL_MOVE_LRC_TAIL => Stage::MoveLrcTail,
            STALL_ADD_REQUEST => Stage::AddRequest,
            other => {
                missing_case!(other);
                Stage::Deadlock
            }
        };
    }

    loop {
        match stage {
            Stage::Scan => {
                while let Some(rb) =
                    core::ptr::NonNull::new(rb_first_cached(unsafe { &(*sched_engine).queue }))
                {
                    let rb = rb.as_ptr();
                    let p = to_priolist(unsafe { &*rb });
                    priolist_for_each_request_consume!(rq, rn, p, {
                        if !last.is_null() && !can_merge_rq(unsafe { &*rq }, unsafe { &*last }) {
                            stage = Stage::RegisterContext;
                            break;
                        }
                        unsafe {
                            list_del_init(&mut (*rq).sched.link);
                            __i915_request_submit(rq);
                            trace_i915_request_in(rq, 0);
                        }
                        last = rq;

                        if is_multi_lrc_rq(unsafe { &*rq }) {
                            // Requests in a multi-LRC relationship are
                            // sequential and must be combined into one H2G.
                            if multi_lrc_submit(unsafe { &mut *rq }) {
                                submit = true;
                                stage = Stage::RegisterContext;
                                break;
                            }
                        } else {
                            submit = true;
                        }
                    });
                    if matches!(stage, Stage::RegisterContext) {
                        break;
                    }
                    rb_erase_cached(&p.node, unsafe { &mut (*sched_engine).queue });
                    unsafe { i915_priolist_free(p as *const _ as *mut _) };
                }
                if matches!(stage, Stage::Scan) {
                    stage = Stage::RegisterContext;
                }
            }
            Stage::RegisterContext => {
                if submit {
                    let ce = unsafe { request_to_scheduling_context_mut(&mut *last) };
                    if unlikely(
                        !ctx_id_mapped(guc, ce.guc_id.id as u32)
                            && intel_context_is_schedulable(&*ce),
                    ) {
                        ret = try_context_registration(ce, false);
                        if unlikely(ret == -EPIPE) {
                            stage = Stage::Deadlock;
                            continue;
                        } else if ret == -EBUSY {
                            guc.stalled_request = last;
                            guc.submission_stall_reason = STALL_REGISTER_CONTEXT;
                            stage = Stage::Schedule;
                            continue;
                        } else if ret != 0 {
                            gem_warn_on!(ret != 0); // Unexpected
                            stage = Stage::Deadlock;
                            continue;
                        }
                    }
                    stage = Stage::MoveLrcTail;
                } else {
                    stage = Stage::Done;
                }
            }
            Stage::MoveLrcTail => {
                if is_multi_lrc_rq(unsafe { &mut *last }) {
                    ret = guc_wq_item_append(guc, unsafe { &mut *last });
                    if ret == -EBUSY {
                        stage = Stage::Schedule;
                        continue;
                    } else if ret != 0 {
                        gem_warn_on!(ret != 0); // Unexpected
                        stage = Stage::Deadlock;
                        continue;
                    }
                } else {
                    guc_set_lrc_tail(unsafe { &mut *last });
                }
                stage = Stage::AddRequest;
            }
            Stage::AddRequest => {
                ret = guc_add_request(guc, unsafe { &mut *last });
                if unlikely(ret == -EPIPE) {
                    stage = Stage::Deadlock;
                    continue;
                } else if ret == -EBUSY {
                    stage = Stage::Schedule;
                    continue;
                } else if ret != 0 {
                    gem_warn_on!(ret != 0); // Unexpected
                    stage = Stage::Deadlock;
                    continue;
                }
                stage = Stage::Done;
            }
            Stage::Done => {
                guc.stalled_request = core::ptr::null_mut();
                guc.submission_stall_reason = STALL_NONE;
                return submit;
            }
            Stage::Deadlock => {
                unsafe { (*sched_engine).tasklet.callbacks.callback = None };
                unsafe { tasklet_disable_nosync(core::ptr::addr_of_mut!((*sched_engine).tasklet)) };
                return false;
            }
            Stage::Schedule => {
                unsafe { tasklet_schedule(core::ptr::addr_of_mut!((*sched_engine).tasklet)) };
                return false;
            }
        }
    }
}

// upstream: intel_guc_submission.c guc_submission_tasklet()
fn guc_submission_tasklet(t: &mut tasklet_struct) {
    let sched_engine = from_tasklet!(t, tasklet);
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(unsafe { &mut (*sched_engine).lock }, &mut flags);
    loop {
        if !guc_dequeue_one_context(unsafe {
            &mut *(*sched_engine).private_data.cast::<intel_guc>()
        }) {
            break;
        }
    }
    unsafe { i915_sched_engine_reset_on_empty(sched_engine) };
    spin_unlock_irqrestore(unsafe { &mut (*sched_engine).lock }, flags);
}

// upstream: intel_guc_submission.c cs_irq_handler()
fn cs_irq_handler(engine: &intel_engine_cs, iir: u16) {
    if iir & GT_RENDER_USER_INTERRUPT as u16 != 0 {
        unsafe { intel_engine_signal_breadcrumbs(engine as *const _ as *mut _) };
    }
}

unsafe extern "C" fn cs_irq_handler_callback(engine: *mut intel_engine_cs, iir: u16) {
    // SAFETY: the IRQ handler contract supplies a live engine pointer.
    cs_irq_handler(unsafe { &*engine }, iir);
}

// upstream: intel_guc_submission.c scrub_guc_desc_for_outstanding_g2h()
fn scrub_guc_desc_for_outstanding_g2h(guc: &mut intel_guc) {
    let mut flags: c_ulong = 0;
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    for (_, entry) in crate::linux::xarray::xa_snapshot(&mut guc.context_lookup) {
        let ce = unsafe { &mut *entry.cast::<intel_context>() };
        // If the refcount is already zero, the lost deregister G2H must not
        // prevent final destruction; do not acquire/release an extra ref.
        let do_put = kref_get_unless_zero(unsafe { &mut *ce.r#ref.refcount });
        xa_unlock(&mut guc.context_lookup);

        if test_bit(CONTEXT_GUC_INIT, &ce.flags)
            && unsafe { cancel_delayed_work(&mut ce.guc_state.sched_disable_delay_work) }
        {
            // A successful cancel lets us close immediately.
            intel_context_sched_disable_unpin(&mut *ce);
        }

        spin_lock(&mut ce.guc_state.lock);
        // submission_disabled() is now visible to callers which set these
        // flags; those callers must not set the flags after observing it.
        let destroyed = context_destroyed(ce);
        let pending_enable = context_pending_enable(ce);
        let pending_disable = context_pending_disable(ce);
        let deregister = context_wait_for_deregister_to_register(ce);
        let banned = context_banned(ce);
        init_sched_state(ce);
        spin_unlock(&mut ce.guc_state.lock);

        if pending_enable || destroyed || deregister {
            decr_outstanding_submission_g2h(guc);
            if deregister {
                guc_signal_context_fence(&mut *ce);
            }
            if destroyed {
                unsafe { intel_gt_pm_put_async_untracked(guc_to_gt(guc)) };
                release_guc_id(guc, &mut *ce);
                __guc_context_destroy(&mut *ce);
            }
            if pending_enable || deregister {
                intel_context_put(&mut *ce);
            }
        }

        // This is intentionally independent of the preceding conditional.
        if pending_disable {
            guc_signal_context_fence(&mut *ce);
            if banned {
                guc_cancel_context_requests(&mut *ce);
                unsafe { intel_engine_signal_breadcrumbs(ce.engine) };
            }
            intel_context_sched_disable_unpin(&mut *ce);
            decr_outstanding_submission_g2h(guc);
            spin_lock(&mut ce.guc_state.lock);
            guc_blocked_fence_complete(&mut *ce);
            spin_unlock(&mut ce.guc_state.lock);
            intel_context_put(&mut *ce);
        }

        if do_put {
            intel_context_put(&mut *ce);
        }
        xa_lock(&mut guc.context_lookup);
    }
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
}

const WRAP_TIME_CLKS: u32 = u32::MAX;
const POLL_TIME_CLKS: u32 = WRAP_TIME_CLKS >> 3;

// upstream: intel_guc_submission.c __extend_last_switch()
fn __extend_last_switch(guc: &intel_guc, prev_start: &mut u64, new_start: u32) {
    let mut gt_stamp_hi = upper_32_bits(guc.timestamp.gt_stamp);
    let gt_stamp_last = lower_32_bits(guc.timestamp.gt_stamp);
    if new_start == lower_32_bits(*prev_start) {
        return;
    }
    if new_start < gt_stamp_last && new_start.wrapping_sub(gt_stamp_last) <= POLL_TIME_CLKS {
        gt_stamp_hi += 1;
    }
    if new_start > gt_stamp_last
        && gt_stamp_last.wrapping_sub(new_start) <= POLL_TIME_CLKS
        && gt_stamp_hi != 0
    {
        gt_stamp_hi -= 1;
    }
    *prev_start = ((gt_stamp_hi as u64) << 32) | new_start as u64;
}

// upstream: intel_guc_submission.c __get_engine_usage_record()
fn __get_engine_usage_record(
    engine: &intel_engine_cs,
    last_in: &mut u32,
    id: &mut u32,
    total: &mut u32,
) {
    let rec_map = unsafe { intel_guc_engine_usage_record_map(engine) };
    let mut i = 0;
    loop {
        *last_in = iosys_map_rd_field!(&rec_map, 0, guc_engine_usage_record, last_switch_in_stamp);
        *id = iosys_map_rd_field!(&rec_map, 0, guc_engine_usage_record, current_context_index);
        *total = iosys_map_rd_field!(&rec_map, 0, guc_engine_usage_record, total_runtime);
        let check_last: u32 = iosys_map_rd_field!(&rec_map, 0, guc_engine_usage_record, last_switch_in_stamp);
        let check_id: u32 = iosys_map_rd_field!(&rec_map, 0, guc_engine_usage_record, current_context_index);
        let check_total: u32 = iosys_map_rd_field!(&rec_map, 0, guc_engine_usage_record, total_runtime);
        if check_last == *last_in && check_id == *id && check_total == *total {
            break;
        }
        i += 1;
        if i >= 6 {
            break;
        }
    }
}

// upstream: intel_guc_submission.c __set_engine_usage_record()
fn __set_engine_usage_record(engine: &intel_engine_cs, last_in: u32, id: u32, total: u32) {
    let rec_map = unsafe { intel_guc_engine_usage_record_map(engine) };
    iosys_map_wr_field!(
        &rec_map,
        0,
        guc_engine_usage_record,
        last_switch_in_stamp,
        last_in,
    );
    iosys_map_wr_field!(
        &rec_map,
        0,
        guc_engine_usage_record,
        current_context_index,
        id,
    );
    iosys_map_wr_field!(&rec_map, 0, guc_engine_usage_record, total_runtime, total);
}

// upstream: intel_guc_submission.c guc_update_engine_gt_clks()
fn guc_update_engine_gt_clks(engine: &mut intel_engine_cs) {
    let stats = core::ptr::addr_of_mut!(engine.stats.data.guc);
    let guc = unsafe { &mut *gt_to_guc(engine.gt) };
    let (mut last_switch, mut ctx_id, mut total) = (0, 0, 0);
    lockdep_assert_held(&guc.timestamp.lock);
    __get_engine_usage_record(engine, &mut last_switch, &mut ctx_id, &mut total);
    unsafe { (*stats).running = ctx_id != !0u32 && last_switch != 0 };
    if unsafe { (*stats).running } {
        __extend_last_switch(guc, unsafe { &mut (*stats).start_gt_clk }, last_switch);
    }
    // Accumulate the delta from the prior sample rather than extending total
    // for 32-bit overflow.
    if total != 0 && total != !0u32 {
        unsafe {
            (*stats).total_gt_clks += total.wrapping_sub((*stats).prev_total) as u64;
            (*stats).prev_total = total;
        }
    }
}

// upstream: intel_guc_submission.c gpm_timestamp_shift()
fn gpm_timestamp_shift(gt: &intel_gt) -> u32 {
    let mut reg = 0;
    let gt = gt as *const _ as *mut intel_gt;
    with_intel_runtime_pm!(unsafe { (*(*gt).uncore).rpm }, wakeref, {
        reg = unsafe { intel_uncore_read((*gt).uncore, RPM_CONFIG0) };
    });
    3u32 - ((reg & GEN10_RPM_CONFIG0_CTC_SHIFT_PARAMETER_MASK)
        >> GEN10_RPM_CONFIG0_CTC_SHIFT_PARAMETER_MASK.trailing_zeros())
}

// upstream: intel_guc_submission.c guc_update_pm_timestamp()
fn guc_update_pm_timestamp(guc: &mut intel_guc, now: &mut ktime_t) {
    let gt = unsafe { guc_to_gt(guc) };
    let mut gt_stamp_hi = upper_32_bits(guc.timestamp.gt_stamp);
    let gpm_ts = unsafe {
        intel_uncore_read64_2x32((*gt).uncore, MISC_STATUS0, MISC_STATUS1) >> guc.timestamp.shift
    };
    let gt_stamp_lo = lower_32_bits(gpm_ts);
    *now = ktime_get();
    if gt_stamp_lo < lower_32_bits(guc.timestamp.gt_stamp) {
        gt_stamp_hi += 1;
    }
    guc.timestamp.gt_stamp = ((gt_stamp_hi as u64) << 32) | gt_stamp_lo as u64;
}

// upstream: intel_guc_submission.c guc_engine_busyness()
fn guc_engine_busyness(engine: &mut intel_engine_cs, now: &mut ktime_t) -> ktime_t {
    let stats = core::ptr::addr_of_mut!(engine.stats.data.guc);
    let gpu_error = unsafe { &(*engine.i915).gpu_error };
    let gt = engine.gt;
    let guc = unsafe { &mut *gt_to_guc(gt) };
    let mut total;
    let mut gt_stamp_saved;
    let mut flags: c_ulong = 0;
    let reset_count;
    let in_reset;
    let wakeref;

    spin_lock_irqsave(&mut guc.timestamp.lock, &mut flags);
    // During reset, use the driver's stored copy because GuC busyness may be
    // only partially updated.  Recheck reset_count after reading the backoff
    // flag, since reset updates the count after setting that flag.
    reset_count = unsafe { i915_reset_count(gpu_error) };
    in_reset = test_bit(I915_RESET_BACKOFF, unsafe { &(*gt).reset.flags });
    *now = ktime_get();

    // GuC state is queried only while the GT is awake, so the active clock
    // delta uses a consistent view of GT and context timestamps.
    wakeref = if in_reset {
        core::ptr::null_mut()
    } else {
        unsafe { intel_gt_pm_get_if_awake(gt) }
    };
    if !wakeref.is_null() {
        let stats_saved = unsafe { *stats };
        gt_stamp_saved = guc.timestamp.gt_stamp;
        guc_update_engine_gt_clks(engine);
        guc_update_pm_timestamp(guc, now);
        unsafe { intel_gt_pm_put_async(gt, wakeref) };
        if unsafe { i915_reset_count(gpu_error) } != reset_count {
            unsafe { *stats = stats_saved };
            guc.timestamp.gt_stamp = gt_stamp_saved;
        }
    }

    total = unsafe { intel_gt_clock_interval_to_ns(gt, (*stats).total_gt_clks) };
    if unsafe { (*stats).running } {
        let clk = guc.timestamp.gt_stamp - unsafe { (*stats).start_gt_clk };
        total += unsafe { intel_gt_clock_interval_to_ns(gt, clk) };
    }
    if total > unsafe { (*stats).total } {
        unsafe { (*stats).total = total };
    }
    spin_unlock_irqrestore(&mut guc.timestamp.lock, flags);
    unsafe { (*stats).total as ktime_t }
}

// Match intel_engine_cs.busyness's C callback ABI while keeping the translated
// implementation's safe borrowed form above.
unsafe extern "C" fn guc_engine_busyness_callback(
    engine: *mut intel_engine_cs,
    now: *mut ktime_t,
) -> ktime_t {
    // SAFETY: the engine callback contract supplies live, writable arguments.
    guc_engine_busyness(unsafe { &mut *engine }, unsafe { &mut *now })
}

// upstream: intel_guc_submission.c guc_enable_busyness_worker()
fn guc_enable_busyness_worker(guc: &mut intel_guc) {
    unsafe {
        mod_delayed_work(
            system_highpri_wq,
            &mut guc.timestamp.work,
            guc.timestamp.ping_delay,
        )
    };
}

// upstream: intel_guc_submission.c guc_cancel_busyness_worker()
fn guc_cancel_busyness_worker(guc: &mut intel_guc) {
    // Callers have different reset-lock contexts. Synchronous cancellation
    // while reset is held can appear to deadlock because the worker tries the
    // reset lock, but it uses try-lock and exits immediately when unavailable.
    // Use asynchronous cancellation while reset is locked/in backoff; otherwise
    // synchronously stop the worker before its storage can be released.
    if unsafe { mutex_is_locked(&(*guc_to_gt(guc)).reset.mutex) }
        || test_bit(I915_RESET_BACKOFF, unsafe {
            &(*guc_to_gt(guc)).reset.flags
        })
    {
        unsafe { cancel_delayed_work(&mut guc.timestamp.work) };
    } else {
        unsafe { cancel_delayed_work_sync(&mut guc.timestamp.work) };
    }
}

// upstream: intel_guc_submission.c __reset_guc_busyness_stats()
fn __reset_guc_busyness_stats(guc: &mut intel_guc) {
    let gt = unsafe { guc_to_gt(guc) };
    let mut flags: c_ulong = 0;
    let mut unused = ktime_t::default();
    spin_lock_irqsave(&mut guc.timestamp.lock, &mut flags);
    guc_update_pm_timestamp(guc, &mut unused);
    for_each_engine!(engine, id, gt, {
        let stats = core::ptr::addr_of_mut!(engine.stats.data.guc);
        guc_update_engine_gt_clks(engine);
        // A running context reset has no context-switch record, so account for
        // its current active interval explicitly.
        if unsafe { (*stats).running } {
            let clk = guc.timestamp.gt_stamp - unsafe { (*stats).start_gt_clk };
            unsafe { (*stats).total_gt_clks += clk };
        }
        unsafe {
            (*stats).prev_total = 0;
            (*stats).running = false;
        }
    });
    spin_unlock_irqrestore(&mut guc.timestamp.lock, flags);
}

// upstream: intel_guc_submission.c __update_guc_busyness_running_state()
fn __update_guc_busyness_running_state(guc: &mut intel_guc) {
    let gt = unsafe { guc_to_gt(guc) };
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut guc.timestamp.lock, &mut flags);
    for_each_engine!(engine, id, gt, {
        unsafe { engine.stats.data.guc.running = false };
    });
    spin_unlock_irqrestore(&mut guc.timestamp.lock, flags);
}

// upstream: intel_guc_submission.c __update_guc_busyness_stats()
fn __update_guc_busyness_stats(guc: &mut intel_guc) {
    let gt = unsafe { guc_to_gt(guc) };
    let mut flags: c_ulong = 0;
    let mut unused = ktime_t::default();
    guc.timestamp.last_stat_jiffies = jiffies();
    spin_lock_irqsave(&mut guc.timestamp.lock, &mut flags);
    guc_update_pm_timestamp(guc, &mut unused);
    for_each_engine!(engine, id, gt, {
        guc_update_engine_gt_clks(engine);
    });
    spin_unlock_irqrestore(&mut guc.timestamp.lock, flags);
}

// upstream: intel_guc_submission.c __guc_context_update_stats()
fn __guc_context_update_stats(ce: &mut intel_context) {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(unsafe { &mut (*guc).timestamp.lock }, &mut flags);
    unsafe { lrc_update_runtime(&mut *ce) };
    spin_unlock_irqrestore(unsafe { &mut (*guc).timestamp.lock }, flags);
}

// upstream: intel_guc_submission.c guc_context_update_stats()
fn guc_context_update_stats(ce: &mut intel_context) {
    if !intel_context_pin_if_active(&mut *ce) {
        return;
    }
    __guc_context_update_stats(&mut *ce);
    intel_context_unpin(&mut *ce);
}

// upstream: intel_guc_submission.c guc_timestamp_ping()
fn guc_timestamp_ping(wrk: &mut work_struct) {
    let guc = container_of!(wrk, intel_guc, timestamp.work.work);
    let guc = unsafe { &mut *guc };
    let uc = container_of!(guc, intel_uc, guc);
    let gt = unsafe { guc_to_gt(guc) };
    let mut wakeref;
    let mut index = 0usize;
    let mut srcu = 0;
    let ret;

    // The worker must not hold a GT wakeref: releasing it may call gt_park,
    // which synchronously cancels this worker and would deadlock. Hold a global
    // runtime-PM reference only if runtime PM is already active. If it is not,
    // parking has already made busyness stats current. Do not requeue on this
    // path; unpark requeues it. Any timestamp wrap while parked is immaterial.
    wakeref = unsafe { intel_runtime_pm_get_if_active(
        core::ptr::addr_of_mut!((*(*gt).i915).runtime_pm).cast::<IntelRuntimePm>(),
    ) };
    if wakeref.is_null() {
        return;
    }

    // Reset cannot be waited out here because reset flushes this worker.
    ret = unsafe { intel_gt_reset_trylock(gt, &mut srcu) };
    if ret != 0 {
        unsafe { intel_runtime_pm_put(&(*(*gt).i915).runtime_pm, wakeref) };
        return;
    }
    __update_guc_busyness_stats(guc);
    // Update context counters too, so their 32-bit runtime values are extended.
    xa_for_each!(&mut guc.context_lookup, index, ce, {
        let ce: *mut intel_context = ce;
        guc_context_update_stats(unsafe { &mut *ce });
    });
    unsafe { intel_gt_reset_unlock(gt, srcu) };
    guc_enable_busyness_worker(guc);
    unsafe { intel_runtime_pm_put(&(*(*gt).i915).runtime_pm, wakeref) };
}

// upstream: intel_guc_submission.c guc_action_enable_usage_stats()
fn guc_action_enable_usage_stats(guc: &mut intel_guc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let offset = unsafe { intel_guc_engine_usage_offset(guc) };
    let action = [INTEL_GUC_ACTION_SET_ENG_UTIL_BUFF, offset, 0];
    for_each_engine!(engine, id, gt, {
        __set_engine_usage_record(engine, 0, 0xffff_ffff, 0);
    });
    intel_guc_send(guc, &action, action.len() as u32)
}

// upstream: intel_guc_submission.c guc_init_engine_stats()
fn guc_init_engine_stats(guc: &mut intel_guc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let mut ret = 0;
    with_intel_runtime_pm!(unsafe { &mut (*(*gt).i915).runtime_pm }, wakeref, {
        ret = guc_action_enable_usage_stats(guc);
    });
    if ret != 0 {
        guc_err!(
            guc,
            "Failed to enable usage stats: %pe\n",
            ERR_PTR::<c_void>(ret)
        );
    } else {
        guc_enable_busyness_worker(guc);
    }
    ret
}

// upstream: intel_guc_submission.c guc_fini_engine_stats()
fn guc_fini_engine_stats(guc: &mut intel_guc) {
    guc_cancel_busyness_worker(guc);
}

// upstream: intel_guc_submission.c intel_guc_busyness_park()
fn intel_guc_busyness_park(gt: &mut intel_gt) {
    let guc = unsafe { &mut *gt_to_guc(gt as *mut _) };
    if !guc_submission_initialized(guc) {
        return;
    }
    // Assume engines are stopped while parked.
    __update_guc_busyness_running_state(guc);
    // Synchronous cancel avoids unclaimed register access after suspend.
    guc_cancel_busyness_worker(guc);
    // Sample only when at least half of a ping period has elapsed.
    if guc.timestamp.last_stat_jiffies != 0
        && !time_after(
            jiffies(),
            guc.timestamp.last_stat_jiffies + guc.timestamp.ping_delay / 2,
        )
    {
        return;
    }
    __update_guc_busyness_stats(guc);
}

// upstream: intel_guc_submission.c intel_guc_busyness_unpark()
fn intel_guc_busyness_unpark(gt: &mut intel_gt) {
    let guc = unsafe { &mut *gt_to_guc(gt as *mut _) };
    let mut flags: c_ulong = 0;
    let mut unused = ktime_t::default();
    if !guc_submission_initialized(guc) {
        return;
    }
    spin_lock_irqsave(&mut guc.timestamp.lock, &mut flags);
    guc_update_pm_timestamp(guc, &mut unused);
    spin_unlock_irqrestore(&mut guc.timestamp.lock, flags);
    guc_enable_busyness_worker(guc);
}

// upstream: intel_guc_submission.c submission_disabled()
#[inline]
fn submission_disabled(guc: &intel_guc) -> bool {
    let sched_engine = guc.sched_engine;
    unlikely(
        sched_engine.is_null()
            || !unsafe { __tasklet_is_enabled(core::ptr::addr_of!((*sched_engine).tasklet)) }
            || unsafe { intel_gt_is_wedged(guc_to_gt_const(guc)) },
    )
}

// upstream: intel_guc_submission.c disable_submission()
fn disable_submission(guc: &mut intel_guc) {
    let sched_engine = guc.sched_engine;
    let tasklet = unsafe { core::ptr::addr_of_mut!((*sched_engine).tasklet) };
    if unsafe { __tasklet_is_enabled(tasklet) } {
        gem_bug_on!(!guc.ct.enabled);
        unsafe { __tasklet_disable_sync_once(tasklet) };
        unsafe { (*sched_engine).tasklet.callbacks.callback = None };
    }
}

unsafe extern "C" fn guc_submission_tasklet_callback(tasklet: *mut tasklet_struct) {
    // SAFETY: the scheduler tasklet invokes this callback with its live object.
    guc_submission_tasklet(unsafe { &mut *tasklet });
}

// upstream: intel_guc_submission.c enable_submission()
fn enable_submission(guc: &mut intel_guc) {
    let sched_engine = guc.sched_engine;
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(unsafe { &mut (*sched_engine).lock }, &mut flags);
    unsafe { (*sched_engine).tasklet.callbacks.callback = Some(guc_submission_tasklet_callback) };
    wmb(); // Make sure the callback is visible.
    let tasklet = unsafe { core::ptr::addr_of_mut!((*sched_engine).tasklet) };
    if !unsafe { __tasklet_is_enabled(tasklet) } && unsafe { __tasklet_enable(tasklet) } {
        gem_bug_on!(!guc.ct.enabled);
        // Kick in case a new request submission was missed.
        unsafe { tasklet_hi_schedule(tasklet) };
    }
    spin_unlock_irqrestore(unsafe { &mut (*sched_engine).lock }, flags);
}

// upstream: intel_guc_submission.c guc_flush_submissions()
fn guc_flush_submissions(guc: &mut intel_guc) {
    let sched_engine = guc.sched_engine;
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(unsafe { &mut (*sched_engine).lock }, &mut flags);
    spin_unlock_irqrestore(unsafe { &mut (*sched_engine).lock }, flags);
}

// upstream: intel_guc_submission.c intel_guc_submission_flush_work()
fn intel_guc_submission_flush_work(guc: &mut intel_guc) {
    unsafe { flush_work(&mut guc.submission_state.destroyed_worker) };
}

// upstream: intel_guc_submission.c intel_guc_submission_reset_prepare()
fn intel_guc_submission_reset_prepare(guc: &mut intel_guc) {
    if unlikely(!guc_submission_initialized(guc)) {
        // Reset may be called during driver load, before GuC initialization.
        return;
    }
    intel_gt_park_heartbeats(unsafe { guc_to_gt(guc) });
    disable_submission(guc);
    if let Some(disable) = guc.interrupts.disable {
        unsafe { disable(guc) };
    }
    __reset_guc_busyness_stats(guc);
    // Flush the IRQ handler.
    spin_lock_irq(unsafe { &mut *(*guc_to_gt(guc)).irq_lock });
    spin_unlock_irq(unsafe { &mut *(*guc_to_gt(guc)).irq_lock });
    // Flush the receive tasklet.
    unsafe { tasklet_disable(&mut guc.ct.receive_tasklet) };
    unsafe { tasklet_enable(&mut guc.ct.receive_tasklet) };
    guc_flush_submissions(guc);
    guc_flush_destroyed_contexts(guc);
    unsafe { flush_work(&mut guc.ct.requests.worker) };
    scrub_guc_desc_for_outstanding_g2h(guc);
}

// upstream: intel_guc_submission.c guc_virtual_get_sibling()
fn guc_virtual_get_sibling(ve: &intel_engine_cs, sibling: u32) -> Option<&intel_engine_cs> {
    let mask = ve.mask;
    let mut num_siblings = 0;
    for_each_engine_masked!(engine, tmp, ve.gt, mask, {
        if num_siblings == sibling {
            return Some(unsafe { &*engine });
        }
        num_siblings += 1;
    });
    None
}

// upstream: intel_guc_submission.c __context_to_physical_engine()
#[inline]
fn __context_to_physical_engine(ce: &intel_context) -> &intel_engine_cs {
    let mut engine = ce.engine;
    if unsafe { intel_engine_is_virtual(engine) } {
        engine = guc_virtual_get_sibling(unsafe { &*engine }, 0).unwrap() as *const _ as *mut _;
    }
    unsafe { &*engine }
}

// upstream: intel_guc_submission.c guc_reset_state()
fn guc_reset_state(ce: &mut intel_context, head: u32, scrub: bool) {
    let engine = __context_to_physical_engine(&*ce) as *const _ as *mut _;
    if !intel_context_is_schedulable(&mut *ce) {
        return;
    }
    gem_bug_on!(!intel_context_is_pinned(&mut *ce));
    // Build a simple context and ring for the breadcrumb update. The context
    // may be corrupt after the hang, so rebuild only the needed state; pending
    // requests are zapped and future requests follow userspace re-creation.
    if scrub {
        unsafe { lrc_init_regs(&mut *ce, &*engine, true) };
    }
    // Rerun the request with its payload neutered if it was guilty.
    unsafe { lrc_update_regs(&mut *ce, &*engine, head) };
}

// upstream: intel_guc_submission.c guc_engine_reset_prepare()
fn guc_engine_reset_prepare(engine: &mut intel_engine_cs) {
    // Wa_22011802037: stop the CS and wait for pending MI force wakeups.
    if unsafe { intel_engine_reset_needs_wa_22011802037(engine.gt) } {
        unsafe { intel_engine_stop_cs(engine) };
        unsafe { intel_engine_wait_for_pending_mi_fw(engine) };
    }
}

unsafe extern "C" fn guc_engine_reset_prepare_callback(engine: *mut intel_engine_cs) {
    // SAFETY: reset callbacks receive a live engine pointer.
    guc_engine_reset_prepare(unsafe { &mut *engine });
}

// upstream: intel_guc_submission.c guc_reset_nop()
fn guc_reset_nop(_engine: &mut intel_engine_cs) {}

unsafe extern "C" fn guc_reset_nop_callback(engine: *mut intel_engine_cs) {
    // SAFETY: the callback receives a live engine pointer; the helper does not
    // access it, but retaining the ABI argument matches the C callback.
    guc_reset_nop(unsafe { &mut *engine });
}

// upstream: intel_guc_submission.c guc_rewind_nop()
fn guc_rewind_nop(_engine: &mut intel_engine_cs, _stalled: bool) {}

unsafe extern "C" fn guc_rewind_nop_callback(engine: *mut intel_engine_cs, stalled: bool) {
    // SAFETY: reset callbacks receive a live engine pointer.
    guc_rewind_nop(unsafe { &mut *engine }, stalled);
}

// upstream: intel_guc_submission.c __unwind_incomplete_requests()
fn __unwind_incomplete_requests(ce: &mut intel_context) {
    let sched_engine = unsafe { (*ce.engine).sched_engine };
    let mut prio = I915_PRIORITY_INVALID;
    let mut pl = core::ptr::null_mut();
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(unsafe { &mut (*sched_engine).lock }, &mut flags);
    spin_lock(&mut ce.guc_state.lock);
    list_for_each_entry_safe_reverse!(rq, rn, I915Request, &ce.guc_state.requests, sched.link, {
        if i915_request_completed(rq) {
            continue;
        }
        unsafe { list_del_init(&mut (*rq).sched.link) };
        unsafe { __i915_request_unsubmit(rq) };
        // Requeue the request for later resubmission.
        gem_bug_on!(rq_prio(unsafe { &*rq }) == I915_PRIORITY_INVALID);
        if rq_prio(unsafe { &*rq }) != prio {
            prio = rq_prio(unsafe { &*rq });
            pl = unsafe { i915_sched_lookup_priolist(sched_engine, prio) };
        }
        gem_bug_on!(unsafe { i915_sched_engine_is_empty(sched_engine) });
        unsafe {
            list_add(&mut (*rq).sched.link, pl);
            set_bit(I915_FENCE_FLAG_PQUEUE, &mut (*rq).fence.flags);
        }
    });
    spin_unlock(&mut ce.guc_state.lock);
    spin_unlock_irqrestore(unsafe { &mut (*sched_engine).lock }, flags);
}

// upstream: intel_guc_submission.c __guc_reset_context()
fn __guc_reset_context(mut ce: &mut intel_context, stalled: intel_engine_mask_t) {
    let number_children = ce.parallel.number_children;
    let parent = ce as *const intel_context as *mut intel_context;
    gem_bug_on!(intel_context_is_child(&mut *ce));
    intel_context_get(&mut *ce);
    // GuC implicitly makes the context non-schedulable on reset notification.
    // Mirror that state; it becomes enabled again on resubmission.
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    clr_context_enabled(ce);
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);

    // Reset every context in the parent/children relationship as needed.
    for i in 0..=number_children {
        if !intel_context_is_pinned(&mut *ce) {
            if i != number_children {
                ce = list_next_entry!(ce, parallel.children.child_link);
            }
            continue;
        }
        let mut guilty = false;
        let rq = unsafe { intel_context_get_active_request(ce as *mut _) };
        let (head, guilty) = if !rq.is_null() {
            if i915_request_started(rq) {
                guilty = stalled & unsafe { (*ce.engine).mask } != 0;
            }
            gem_bug_on!(i915_active_is_idle(&ce.active));
            let head = unsafe { intel_ring_wrap(ce.ring, (*rq).head) };
            unsafe { __i915_request_reset(rq, guilty) };
            i915_request_put(rq);
            (head, guilty)
        } else {
            (unsafe { (*ce.ring).tail }, guilty)
        };
        guc_reset_state(ce, head, guilty);
        if i != number_children {
            ce = list_next_entry!(ce, parallel.children.child_link);
        }
    }
    __unwind_incomplete_requests(unsafe { &mut *parent });
    intel_context_put(unsafe { &mut *parent });
}

// upstream: intel_guc_submission.c wake_up_all_tlb_invalidate()
fn wake_up_all_tlb_invalidate(guc: &mut intel_guc) {
    if !intel_guc_tlb_invalidation_is_available(guc) {
        return;
    }
    let mut i = 0usize;
    xa_lock_irq(&mut guc.tlb_lookup);
    xa_for_each!(&mut guc.tlb_lookup, i, wait, {
        let wait: *mut intel_guc_tlb_wait = wait as *mut c_void as *mut intel_guc_tlb_wait;
        unsafe { wake_up(&mut (*wait).wq) };
    });
    xa_unlock_irq(&mut guc.tlb_lookup);
}

// upstream: intel_guc_submission.c intel_guc_submission_reset()
fn intel_guc_submission_reset(guc: &mut intel_guc, stalled: intel_engine_mask_t) {
    let mut index = 0usize;
    let mut flags: c_ulong = 0;
    if unlikely(!guc_submission_initialized(guc)) {
        // Reset may be called before GuC is initialized during driver load.
        return;
    }
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    xa_for_each!(&mut guc.context_lookup, index, ce, {
        let ce: *mut intel_context = ce as *mut c_void as *mut intel_context;
        let ce = unsafe { &mut *ce };
        if !kref_get_unless_zero(unsafe { &mut *ce.r#ref.refcount }) {
            continue;
        }
        xa_unlock(&mut guc.context_lookup);
        if intel_context_is_pinned(&mut *ce) && !intel_context_is_child(&mut *ce) {
            __guc_reset_context(&mut *ce, stalled);
        }
        intel_context_put(unsafe { &mut *ce });
        xa_lock(&mut guc.context_lookup);
    });
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
    // GuC was reset; drop all context references.
    xa_destroy(&mut guc.context_lookup);
}

// upstream: intel_guc_submission.c guc_cancel_context_requests()
fn guc_cancel_context_requests(ce: &mut intel_context) {
    let sched_engine = ce_to_guc(ce).sched_engine;
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(unsafe { &mut (*sched_engine).lock }, &mut flags);
    spin_lock(&mut ce.guc_state.lock);
    let mut rq: *mut i915_request = core::ptr::null_mut();
    list_for_each_entry!(rq, &ce.guc_state.requests, sched.link, {
        // Mark each executing request as skipped.
        i915_request_put(i915_request_mark_eio(unsafe { &mut *rq }));
    });
    spin_unlock(&mut ce.guc_state.lock);
    spin_unlock_irqrestore(unsafe { &mut (*sched_engine).lock }, flags);
}

// upstream: intel_guc_submission.c guc_cancel_sched_engine_requests()
fn guc_cancel_sched_engine_requests(sched_engine: Option<&mut i915_sched_engine>) {
    // GuC firmware load can fail during boot, before a scheduler exists.
    let Some(sched_engine) = sched_engine else {
        return;
    };
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut sched_engine.lock, &mut flags);
    // Caller disables interrupt generation, tasklet execution, and other
    // threads before resetting submission state. Keep IRQ-state tracking and
    // the submission lock's scope explicit while draining its queue.
    while let Some(rb) = core::ptr::NonNull::new(rb_first_cached(&sched_engine.queue)) {
        let rb = rb.as_ptr();
        let p = to_priolist(unsafe { &*rb });
        priolist_for_each_request_consume!(rq, rn, p, {
            unsafe { list_del_init(&mut (*rq).sched.link) };
            unsafe { __i915_request_submit(rq) };
            i915_request_put(i915_request_mark_eio(unsafe { &mut *rq }));
        });
        rb_erase_cached(&p.node, &mut sched_engine.queue);
        unsafe { i915_priolist_free(p as *const _ as *mut _) };
    }
    // Remaining unready requests are NOPed when submitted.
    sched_engine.queue_priority_hint = INT_MIN;
    sched_engine.queue = RB_ROOT_CACHED!();
    spin_unlock_irqrestore(&mut sched_engine.lock, flags);
}

// upstream: intel_guc_submission.c intel_guc_submission_cancel_requests()
fn intel_guc_submission_cancel_requests(guc: &mut intel_guc) {
    let mut index = 0usize;
    let mut flags: c_ulong = 0;
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    xa_for_each!(&mut guc.context_lookup, index, ce, {
        let ce: *mut intel_context = ce as *mut c_void as *mut intel_context;
        let ce = unsafe { &mut *ce };
        if !kref_get_unless_zero(unsafe { &mut *ce.r#ref.refcount }) {
            continue;
        }
        xa_unlock(&mut guc.context_lookup);
        if intel_context_is_pinned(&mut *ce) && !intel_context_is_child(&mut *ce) {
            guc_cancel_context_requests(&mut *ce);
        }
        intel_context_put(&mut *ce);
        xa_lock(&mut guc.context_lookup);
    });
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
    guc_cancel_sched_engine_requests(unsafe { guc.sched_engine.as_mut() });
    // GuC is gone; drop all context references.
    xa_destroy(&mut guc.context_lookup);
    // A wedged GT cannot respond to TLB invalidation; unblock all waiters.
    wake_up_all_tlb_invalidate(guc);
}

// upstream: intel_guc_submission.c intel_guc_submission_reset_finish()
fn intel_guc_submission_reset_finish(guc: &mut intel_guc) {
    if unlikely(
        !guc_submission_initialized(guc)
            || !unsafe { intel_guc_is_fw_running(guc) }
            || unsafe { intel_gt_is_wedged(guc_to_gt(guc)) },
    ) {
        // Reset during driver load or a wedge.
        return;
    }
    let outstanding = atomic_read(&guc.outstanding_submission_g2h);
    if outstanding != 0 {
        guc_err!(
            &mut *guc,
            "Unexpected outstanding GuC to Host response(s) in reset finish: %d\n",
            outstanding
        );
    }
    atomic_set(&mut guc.outstanding_submission_g2h, 0);
    intel_guc_global_policies_update(&mut *guc);
    enable_submission(guc);
    intel_gt_unpark_heartbeats(unsafe { guc_to_gt(guc) });
    // Full GT reset cleared the TLB caches and G2H queue; release blocked waiters.
    wake_up_all_tlb_invalidate(guc);
}

// upstream: intel_guc_submission.c intel_guc_tlb_invalidation_is_available()
fn intel_guc_tlb_invalidation_is_available(guc: &intel_guc) -> bool {
    return (unsafe { HAS_GUC_TLB_INVALIDATION((*guc_to_gt_const(guc)).i915) })
        && (unsafe { intel_guc_is_ready(guc) });
}

// upstream: intel_guc_submission.c init_tlb_lookup()
fn init_tlb_lookup(guc: &mut intel_guc) -> i32 {
    if !unsafe { HAS_GUC_TLB_INVALIDATION((*guc_to_gt(guc as *mut _)).i915) } {
        return 0;
    }
    xa_init_flags(&mut guc.tlb_lookup, XA_FLAGS_ALLOC);
    let mut wait = kzalloc_obj::<intel_guc_tlb_wait>();
    if wait.is_null() {
        return -ENOMEM;
    }
    unsafe { init_waitqueue_head(&mut (*wait).wq) };
    // Preallocate the shared id used under memory pressure.
    let err = xa_alloc_cyclic_irq(
        &mut guc.tlb_lookup,
        &mut guc.serial_slot,
        wait,
        xa_limit_32b,
        &mut guc.next_seqno,
        GFP_KERNEL,
    );
    if err < 0 {
        unsafe { kfree(wait) };
        return err;
    }
    0
}

// upstream: intel_guc_submission.c fini_tlb_lookup()
fn fini_tlb_lookup(guc: &mut intel_guc) {
    if !unsafe { HAS_GUC_TLB_INVALIDATION((*guc_to_gt(guc as *mut _)).i915) } {
        return;
    }
    let wait: *mut intel_guc_tlb_wait = xa_load(&mut guc.tlb_lookup, guc.serial_slot);
    if !wait.is_null() && unsafe { (*wait).busy } {
        guc_err!(&*guc, "Unexpected busy item in tlb_lookup on fini\n");
    }
    unsafe { kfree(wait) };
    xa_destroy(&mut guc.tlb_lookup);
}

// upstream: intel_guc_submission.c intel_guc_submission_init()
fn intel_guc_submission_init(guc: &mut intel_guc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let mut ret;
    if guc.submission_initialized {
        return 0;
    }
    if GUC_SUBMIT_VER(guc) < MAKE_GUC_VER(1, 0, 0) {
        ret = guc_lrc_desc_pool_create_v69(guc);
        if ret != 0 {
            return ret;
        }
    }
    ret = init_tlb_lookup(guc);
    if ret != 0 {
        guc_lrc_desc_pool_destroy_v69(guc);
        return ret;
    }
    guc.submission_state.guc_ids_bitmap = unsafe { bitmap_zalloc(NUMBER_MULTI_LRC_GUC_ID(guc) as u32, GFP_KERNEL) };
    if guc.submission_state.guc_ids_bitmap.is_null() {
        fini_tlb_lookup(guc);
        guc_lrc_desc_pool_destroy_v69(guc);
        return -ENOMEM;
    }
    guc.timestamp.ping_delay =
        ((POLL_TIME_CLKS / unsafe { (*gt).clock_frequency } + 1) * HZ) as u64;
    guc.timestamp.shift = gpm_timestamp_shift(unsafe { &*gt });
    guc.submission_initialized = true;
    return 0;
}

// upstream: intel_guc_submission.c intel_guc_submission_fini()
fn intel_guc_submission_fini(guc: &mut intel_guc) {
    if !guc.submission_initialized {
        return;
    }
    guc_fini_engine_stats(guc);
    guc_flush_destroyed_contexts(guc);
    guc_lrc_desc_pool_destroy_v69(guc);
    unsafe { i915_sched_engine_put(guc.sched_engine) };
    unsafe { bitmap_free(guc.submission_state.guc_ids_bitmap) };
    fini_tlb_lookup(guc);
    guc.submission_initialized = false;
}

// upstream: intel_guc_submission.c queue_request()
#[inline]
fn queue_request(sched_engine: &mut i915_sched_engine, rq: &mut i915_request, prio: i32) {
    gem_bug_on!(!list_empty(&rq.sched.link));
    unsafe {
        list_add_tail(
            &mut rq.sched.link,
            i915_sched_lookup_priolist(sched_engine, prio),
        );
    }
    set_bit(I915_FENCE_FLAG_PQUEUE, &mut rq.fence.flags);
    unsafe { tasklet_hi_schedule(&mut sched_engine.tasklet) };
}

// upstream: intel_guc_submission.c guc_bypass_tasklet_submit()
fn guc_bypass_tasklet_submit(guc: &mut intel_guc, rq: &mut i915_request) -> i32 {
    let mut ret = 0;
    unsafe { __i915_request_submit(rq) };
    unsafe { trace_i915_request_in(rq, 0) };
    if is_multi_lrc_rq(rq) {
        if multi_lrc_submit(rq) {
            ret = guc_wq_item_append(guc, rq);
            if ret == 0 {
                ret = guc_add_request(guc, rq);
            }
        }
    } else {
        guc_set_lrc_tail(rq);
        ret = guc_add_request(guc, rq);
    }
    if unlikely(ret == -EPIPE) {
        disable_submission(guc);
    }
    ret
}

// upstream: intel_guc_submission.c need_tasklet()
fn need_tasklet(guc: &mut intel_guc, rq: &i915_request) -> bool {
    let sched_engine = unsafe { (*rq.engine).sched_engine };
    let ce = request_to_scheduling_context(rq);
    submission_disabled(guc)
        || !guc.stalled_request.is_null()
        || !unsafe { i915_sched_engine_is_empty(sched_engine) }
        || !ctx_id_mapped(guc, ce.guc_id.id as u32)
}

// upstream: intel_guc_submission.c guc_submit_request()
fn guc_submit_request(rq: &mut i915_request) {
    let sched_engine = unsafe { (*rq.engine).sched_engine };
    let guc = unsafe { gt_to_guc((*rq.engine).gt) };
    let mut flags: c_ulong = 0;
    // May be called from IRQ context for foreign fences.
    spin_lock_irqsave(unsafe { &mut (*sched_engine).lock }, &mut flags);
    if need_tasklet(unsafe { &mut *guc }, rq) {
        queue_request(unsafe { &mut *sched_engine }, rq, rq_prio(rq));
    } else if guc_bypass_tasklet_submit(unsafe { &mut *guc }, rq) == -EBUSY {
        unsafe { tasklet_hi_schedule(core::ptr::addr_of_mut!((*sched_engine).tasklet)) };
    }
    spin_unlock_irqrestore(unsafe { &mut (*sched_engine).lock }, flags);
}

unsafe extern "C" fn guc_submit_request_callback(rq: *mut i915_request) {
    // SAFETY: IntelEngineCs.submit_request is invoked with a live request.
    guc_submit_request(unsafe { &mut *rq });
}

// upstream: intel_guc_submission.c new_guc_id()
fn new_guc_id(guc: &mut intel_guc, ce: &mut intel_context) -> i32 {
    gem_bug_on!(unsafe { intel_context_is_child(ce as *const intel_context as *mut intel_context) });
    let num_guc_ids = NUMBER_MULTI_LRC_GUC_ID(guc) as u32;
    let ret = if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
        unsafe { bitmap_find_free_region(
            guc.submission_state.guc_ids_bitmap,
            num_guc_ids,
            order_base_2(ce.parallel.number_children as u32 + 1) as core::ffi::c_long,
        ) }
    } else {
        ida_alloc_range(
            &mut guc.submission_state.guc_ids,
            num_guc_ids,
            (guc.submission_state.num_guc_ids - 1) as u32,
            GFP_KERNEL | __GFP_RETRY_MAYFAIL | __GFP_NOWARN,
        )
    };
    if unlikely(ret < 0) {
        return ret;
    }
    if !unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
        guc.submission_state.guc_ids_in_use += 1;
    }
    ce.guc_id.id = ret as u16;
    0
}

// upstream: intel_guc_submission.c __release_guc_id()
fn __release_guc_id(guc: &mut intel_guc, ce: &mut intel_context) {
    gem_bug_on!(unsafe { intel_context_is_child(ce as *const intel_context as *mut intel_context) });
    if !context_guc_id_invalid(ce) {
        if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
            unsafe { bitmap_release_region(
                guc.submission_state.guc_ids_bitmap,
                ce.guc_id.id as c_ulong,
                order_base_2(ce.parallel.number_children as u32 + 1) as core::ffi::c_long,
            ) };
        } else {
            guc.submission_state.guc_ids_in_use -= 1;
            ida_free(&mut guc.submission_state.guc_ids, ce.guc_id.id as u32);
        }
        clr_ctx_id_mapping(guc, ce.guc_id.id as u32);
        set_context_guc_id_invalid(ce);
    }
    if !list_empty(&ce.guc_id.link) {
        unsafe { list_del_init(&mut ce.guc_id.link) };
    }
}

// upstream: intel_guc_submission.c release_guc_id()
fn release_guc_id(guc: &mut intel_guc, ce: &mut intel_context) {
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
    __release_guc_id(guc, ce);
    spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
}

// upstream: intel_guc_submission.c steal_guc_id()
fn steal_guc_id(guc: &mut intel_guc, ce: &mut intel_context) -> i32 {
    lockdep_assert_held(&guc.submission_state.lock);
    gem_bug_on!(intel_context_is_child(&mut *ce));
    gem_bug_on!(unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    if !list_empty(&guc.submission_state.guc_id_list) {
        let cn = list_first_entry!(
            &mut guc.submission_state.guc_id_list,
            intel_context,
            guc_id.link,
        );
        let cn = unsafe { &mut *cn };
        gem_bug_on!(atomic_read(&cn.guc_id.r#ref) != 0);
        gem_bug_on!(context_guc_id_invalid(cn));
        gem_bug_on!(intel_context_is_child(&mut *cn));
        gem_bug_on!(intel_context_is_parent(&mut *cn));
        unsafe { list_del_init(&mut cn.guc_id.link) };
        ce.guc_id.id = cn.guc_id.id;
        spin_lock(&mut cn.guc_state.lock);
        clr_context_registered(cn);
        spin_unlock(&mut cn.guc_state.lock);
        set_context_guc_id_invalid(&mut *cn);
        #[cfg(CONFIG_DRM_I915_SELFTEST)]
        {
            guc.number_guc_id_stolen += 1;
        }
        0
    } else {
        -EAGAIN
    }
}

// upstream: intel_guc_submission.c assign_guc_id()
fn assign_guc_id(guc: &mut intel_guc, ce: &mut intel_context) -> i32 {
    lockdep_assert_held(&guc.submission_state.lock);
    gem_bug_on!(intel_context_is_child(&mut *ce));
    let mut ret = new_guc_id(guc, &mut *ce);
    if unlikely(ret < 0) {
        if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
            return -ENOSPC;
        }
        ret = steal_guc_id(guc, &mut *ce);
        if ret < 0 {
            return ret;
        }
    }
    if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
        let mut i = 1;
        for_each_child!(ce, child, {
            unsafe { (*child).guc_id.id = (*ce).guc_id.id + i };
            i += 1;
        });
    }
    0
}

const PIN_GUC_ID_TRIES: usize = 4;

// upstream: intel_guc_submission.c pin_guc_id()
fn pin_guc_id(guc: &mut intel_guc, ce: &mut intel_context) -> i32 {
    let mut ret = 0;
    let mut tries = PIN_GUC_ID_TRIES;
    gem_bug_on!(atomic_read(&ce.guc_id.r#ref) != 0);
    loop {
        let mut flags: c_ulong = 0;
        spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
        might_lock!(&ce.guc_state.lock);
        if context_guc_id_invalid(ce) {
            ret = assign_guc_id(guc, ce);
            if ret == 0 {
                ret = 1; // Newly assigned guc_id.
            }
        }
        if ret >= 0 {
            if !list_empty(&ce.guc_id.link) {
                unsafe { list_del_init(&mut ce.guc_id.link) };
            }
            atomic_inc(&mut ce.guc_id.r#ref);
        }
        spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
        // -EAGAIN means the ID space is exhausted. Retire outstanding requests;
        // after the first unsuccessful pass, back off exponentially by the
        // engine timeslice, bounded to [1ms, 100ms], for the remaining tries.
        if ret == -EAGAIN {
            tries -= 1;
            if tries == 0 {
                break;
            }
            if PIN_GUC_ID_TRIES - tries > 1 {
                let timeslice_shifted = unsafe { (*ce.engine).props.timeslice_duration_ms }
                    << (PIN_GUC_ID_TRIES - tries - 2);
                let max = min_t(100, timeslice_shifted);
                msleep(max_t(max, 1) as u32);
            }
            intel_gt_retire_requests(unsafe { guc_to_gt(guc) });
            continue;
        }
        break;
    }
    ret
}

// upstream: intel_guc_submission.c unpin_guc_id()
fn unpin_guc_id(guc: &mut intel_guc, ce: &mut intel_context) {
    gem_bug_on!(atomic_read(&ce.guc_id.r#ref) < 0);
    gem_bug_on!(intel_context_is_child(&mut *ce));
    if unlikely(context_guc_id_invalid(&mut *ce) || unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) }) {
        return;
    }
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
    if !context_guc_id_invalid(&mut *ce)
        && list_empty(&ce.guc_id.link)
        && atomic_read(&ce.guc_id.r#ref) == 0
    {
        unsafe { list_add_tail(&mut ce.guc_id.link, &mut guc.submission_state.guc_id_list) };
    }
    spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
}

// upstream: intel_guc_submission.c __guc_action_register_multi_lrc_v69()
fn __guc_action_register_multi_lrc_v69(
    guc: &mut intel_guc,
    ce: &intel_context,
    guc_id: u32,
    mut offset: u32,
    loop_on_busy: bool,
) -> i32 {
    let mut action = [0u32; 4 + MAX_ENGINE_INSTANCE];
    let mut len = 0usize;
    gem_bug_on!(ce.parallel.number_children as usize > MAX_ENGINE_INSTANCE);
    action[len] = INTEL_GUC_ACTION_REGISTER_CONTEXT_MULTI_LRC;
    len += 1;
    action[len] = guc_id;
    len += 1;
    action[len] = ce.parallel.number_children as u32 + 1;
    len += 1;
    action[len] = offset;
    len += 1;
    for_each_child!(ce, child, {
        offset += size_of::<guc_lrc_desc_v69>() as u32;
        action[len] = offset;
        len += 1;
    });
    guc_submission_send_busy_loop(guc, &action[..len], len as u32, 0, loop_on_busy)
}

// upstream: intel_guc_submission.c __guc_action_register_multi_lrc_v70()
fn __guc_action_register_multi_lrc_v70(
    guc: &mut intel_guc,
    ce: &intel_context,
    info: &guc_ctxt_registration_info,
    loop_on_busy: bool,
) -> i32 {
    let mut action = [0u32; 13 + MAX_ENGINE_INSTANCE * 2];
    let mut len = 0usize;
    gem_bug_on!(ce.parallel.number_children as usize > MAX_ENGINE_INSTANCE);
    for value in [
        INTEL_GUC_ACTION_REGISTER_CONTEXT_MULTI_LRC,
        info.flags,
        info.context_idx,
        info.engine_class,
        info.engine_submit_mask,
        info.wq_desc_lo,
        info.wq_desc_hi,
        info.wq_base_lo,
        info.wq_base_hi,
        info.wq_size,
        ce.parallel.number_children as u32 + 1,
        info.hwlrca_lo,
        info.hwlrca_hi,
    ] {
        action[len] = value;
        len += 1;
    }
    let mut next_id = info.context_idx + 1;
    for_each_child!(ce, child, {
        gem_bug_on!(next_id != unsafe { (*child).guc_id.id as u32 });
        next_id += 1;
        // GuC supports 64-bit LRCA, although i915/HW currently supports only
        // 32-bit LRCA.
        action[len] = unsafe { lower_32_bits((&(*child).lrc).lrca) };
        len += 1;
        action[len] = unsafe { upper_32_bits((&(*child).lrc).lrca) };
        len += 1;
    });
    gem_bug_on!(len > action.len());
    guc_submission_send_busy_loop(guc, &action[..len], len as u32, 0, loop_on_busy)
}

// upstream: intel_guc_submission.c __guc_action_register_context_v69()
fn __guc_action_register_context_v69(
    guc: &mut intel_guc,
    guc_id: u32,
    offset: u32,
    loop_on_busy: bool,
) -> i32 {
    let action = [INTEL_GUC_ACTION_REGISTER_CONTEXT, guc_id, offset];
    guc_submission_send_busy_loop(guc, &action, action.len() as u32, 0, loop_on_busy)
}

// upstream: intel_guc_submission.c __guc_action_register_context_v70()
fn __guc_action_register_context_v70(
    guc: &mut intel_guc,
    info: &guc_ctxt_registration_info,
    loop_on_busy: bool,
) -> i32 {
    let action = [
        INTEL_GUC_ACTION_REGISTER_CONTEXT,
        info.flags,
        info.context_idx,
        info.engine_class,
        info.engine_submit_mask,
        info.wq_desc_lo,
        info.wq_desc_hi,
        info.wq_base_lo,
        info.wq_base_hi,
        info.wq_size,
        info.hwlrca_lo,
        info.hwlrca_hi,
    ];
    guc_submission_send_busy_loop(guc, &action, action.len() as u32, 0, loop_on_busy)
}

// upstream: intel_guc_submission.c register_context_v69()
fn register_context_v69(guc: &mut intel_guc, ce: &mut intel_context, loop_on_busy: bool) -> i32 {
    let offset = unsafe { intel_guc_ggtt_offset(guc as *mut _, guc.lrc_desc_pool_v69) }
        + ce.guc_id.id as u32 * size_of::<guc_lrc_desc_v69>() as u32;
    prepare_context_registration_info_v69(ce);
    if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
        __guc_action_register_multi_lrc_v69(guc, ce, ce.guc_id.id as u32, offset, loop_on_busy)
    } else {
        __guc_action_register_context_v69(guc, ce.guc_id.id as u32, offset, loop_on_busy)
    }
}

// upstream: intel_guc_submission.c register_context_v70()
fn register_context_v70(guc: &mut intel_guc, ce: &mut intel_context, loop_on_busy: bool) -> i32 {
    let mut info = guc_ctxt_registration_info::default();
    prepare_context_registration_info_v70(&mut *ce, &mut info);
    if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
        __guc_action_register_multi_lrc_v70(guc, &mut *ce, &info, loop_on_busy)
    } else {
        __guc_action_register_context_v70(guc, &info, loop_on_busy)
    }
}

// upstream: intel_guc_submission.c register_context()
fn register_context(ce: &mut intel_context, loop_on_busy: bool) -> i32 {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    gem_bug_on!(intel_context_is_child(&mut *ce));
    unsafe { trace_intel_context_register(ce as *mut _) };
    let ret = if GUC_SUBMIT_VER(guc) >= MAKE_GUC_VER(1, 0, 0) {
        register_context_v70(guc, &mut *ce, loop_on_busy)
    } else {
        register_context_v69(guc, &mut *ce, loop_on_busy)
    };
    if likely(ret == 0) {
        let mut flags: c_ulong = 0;
        spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
        set_context_registered(&mut *ce);
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        if GUC_SUBMIT_VER(guc) >= MAKE_GUC_VER(1, 0, 0) {
            guc_context_policy_init_v70(&mut *ce, loop_on_busy);
        }
    }
    ret
}

// upstream: intel_guc_submission.c __guc_action_deregister_context()
fn __guc_action_deregister_context(guc: &mut intel_guc, guc_id: u32) -> i32 {
    let action = [INTEL_GUC_ACTION_DEREGISTER_CONTEXT, guc_id];
    guc_submission_send_busy_loop(
        guc,
        &action,
        action.len() as u32,
        G2H_LEN_DW_DEREGISTER_CONTEXT,
        true,
    )
}

// upstream: intel_guc_submission.c deregister_context()
fn deregister_context(ce: &mut intel_context, guc_id: u32) -> i32 {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    gem_bug_on!(unsafe { intel_context_is_child(ce as *const intel_context as *mut intel_context) });
    unsafe { trace_intel_context_deregister(ce as *const _ as *mut _) };
    __guc_action_deregister_context(guc, guc_id)
}

// upstream: intel_guc_submission.c clear_children_join_go_memory()
#[inline]
fn clear_children_join_go_memory(ce: &mut intel_context) {
    let ps = __get_parent_scratch(ce);
    unsafe {
        (*ps).go.semaphore = 0;
        for i in 0..=ce.parallel.number_children {
            (*ps).join[i as usize].semaphore = 0;
        }
    }
}

// upstream: intel_guc_submission.c get_children_go_value()
#[inline]
fn get_children_go_value(ce: &mut intel_context) -> u32 {
    unsafe { (*__get_parent_scratch(ce)).go.semaphore }
}

// upstream: intel_guc_submission.c get_children_join_value()
#[inline]
fn get_children_join_value(ce: &mut intel_context, child_index: u8) -> u32 {
    unsafe { (*__get_parent_scratch(ce)).join[child_index as usize].semaphore }
}

// upstream: intel_guc_submission.c __guc_context_policy_action_size()
fn __guc_context_policy_action_size(policy: &context_policy) -> u32 {
    let bytes = size_of::<context_policy_header>() + size_of::<guc_klv>() * policy.count as usize;
    (bytes / size_of::<u32>()) as u32
}

// upstream: intel_guc_submission.c __guc_context_policy_start_klv()
fn __guc_context_policy_start_klv(policy: &mut context_policy, guc_id: u16) {
    policy.h2g.header.action = INTEL_GUC_ACTION_HOST2GUC_UPDATE_CONTEXT_POLICIES;
    policy.h2g.header.ctx_id = guc_id as u32;
    policy.count = 0;
}

// upstream: intel_guc_submission.c __guc_context_policy_add_execution_quantum()
fn __guc_context_policy_add_execution_quantum(policy: &mut context_policy, data: u32) {
    gem_bug_on!(policy.count as usize >= GUC_CONTEXT_POLICIES_KLV_NUM_IDS);
    policy.h2g.klv[policy.count as usize].kl =
        field_prep(GUC_KLV_0_KEY, GUC_CONTEXT_POLICIES_KLV_ID_EXECUTION_QUANTUM)
            | field_prep(GUC_KLV_0_LEN, 1);
    policy.h2g.klv[policy.count as usize].value = data;
    policy.count += 1;
}

// upstream: intel_guc_submission.c __guc_context_policy_add_preemption_timeout()
fn __guc_context_policy_add_preemption_timeout(policy: &mut context_policy, data: u32) {
    gem_bug_on!(policy.count as usize >= GUC_CONTEXT_POLICIES_KLV_NUM_IDS);
    policy.h2g.klv[policy.count as usize].kl = field_prep(
        GUC_KLV_0_KEY,
        GUC_CONTEXT_POLICIES_KLV_ID_PREEMPTION_TIMEOUT,
    ) | field_prep(GUC_KLV_0_LEN, 1);
    policy.h2g.klv[policy.count as usize].value = data;
    policy.count += 1;
}

// upstream: intel_guc_submission.c __guc_context_policy_add_priority()
fn __guc_context_policy_add_priority(policy: &mut context_policy, data: u32) {
    gem_bug_on!(policy.count as usize >= GUC_CONTEXT_POLICIES_KLV_NUM_IDS);
    policy.h2g.klv[policy.count as usize].kl = field_prep(
        GUC_KLV_0_KEY,
        GUC_CONTEXT_POLICIES_KLV_ID_SCHEDULING_PRIORITY,
    ) | field_prep(GUC_KLV_0_LEN, 1);
    policy.h2g.klv[policy.count as usize].value = data;
    policy.count += 1;
}

// upstream: intel_guc_submission.c __guc_context_policy_add_preempt_to_idle()
fn __guc_context_policy_add_preempt_to_idle(policy: &mut context_policy, data: u32) {
    gem_bug_on!(policy.count as usize >= GUC_CONTEXT_POLICIES_KLV_NUM_IDS);
    policy.h2g.klv[policy.count as usize].kl = field_prep(
        GUC_KLV_0_KEY,
        GUC_CONTEXT_POLICIES_KLV_ID_PREEMPT_TO_IDLE_ON_QUANTUM_EXPIRY,
    ) | field_prep(GUC_KLV_0_LEN, 1);
    policy.h2g.klv[policy.count as usize].value = data;
    policy.count += 1;
}

// upstream: intel_guc_submission.c __guc_context_policy_add_slpc_ctx_freq_req()
fn __guc_context_policy_add_slpc_ctx_freq_req(policy: &mut context_policy, data: u32) {
    gem_bug_on!(policy.count as usize >= GUC_CONTEXT_POLICIES_KLV_NUM_IDS);
    policy.h2g.klv[policy.count as usize].kl =
        field_prep(GUC_KLV_0_KEY, GUC_CONTEXT_POLICIES_KLV_ID_SLPM_GT_FREQUENCY)
            | field_prep(GUC_KLV_0_LEN, 1);
    policy.h2g.klv[policy.count as usize].value = data;
    policy.count += 1;
}

// upstream: intel_guc_submission.c __guc_context_set_context_policies()
fn __guc_context_set_context_policies(
    guc: &mut intel_guc,
    policy: &context_policy,
    loop_on_busy: bool,
) -> i32 {
    guc_submission_send_busy_loop(
        guc,
        unsafe {
            core::slice::from_raw_parts(
                core::ptr::addr_of!(policy.h2g).cast::<u32>(),
                size_of::<GuCUpdateContextPolicy>() / size_of::<u32>(),
            )
        },
        __guc_context_policy_action_size(policy),
        0,
        loop_on_busy,
    )
}

// upstream: intel_guc_submission.c guc_context_policy_init_v70()
fn guc_context_policy_init_v70(ce: &mut intel_context, loop_on_busy: bool) -> i32 {
    let engine = ce.engine;
    let guc = unsafe { gt_to_guc((*engine).gt) };
    let mut policy = context_policy::default();
    // Zero disables either time quantum.
    gem_bug_on!(overflows_u32(unsafe {
        (*engine).props.timeslice_duration_ms * 1000
    }));
    gem_bug_on!(overflows_u32(unsafe {
        (*engine).props.preempt_timeout_ms * 1000
    }));
    let execution_quantum = (unsafe { (*engine).props.timeslice_duration_ms * 1000 }) as u32;
    let preemption_timeout = (unsafe { (*engine).props.preempt_timeout_ms * 1000 }) as u32;
    let mut slpc_ctx_freq_req = 0;
    if ce.flags & (bit(CONTEXT_LOW_LATENCY) as u64) != 0 {
        slpc_ctx_freq_req |= SLPC_CTX_FREQ_REQ_IS_COMPUTE;
    }
    __guc_context_policy_start_klv(&mut policy, ce.guc_id.id as u16);
    __guc_context_policy_add_priority(&mut policy, ce.guc_state.prio as u32);
    __guc_context_policy_add_execution_quantum(&mut policy, execution_quantum);
    __guc_context_policy_add_preemption_timeout(&mut policy, preemption_timeout);
    __guc_context_policy_add_slpc_ctx_freq_req(&mut policy, slpc_ctx_freq_req as u32);
    if unsafe { (*engine).flags } & I915_ENGINE_WANT_FORCED_PREEMPTION != 0 {
        __guc_context_policy_add_preempt_to_idle(&mut policy, 1);
    }
    let ret = __guc_context_set_context_policies(unsafe { &mut *guc }, &policy, loop_on_busy);
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    if ret != 0 {
        set_context_policy_required(ce);
    } else {
        clr_context_policy_required(ce);
    }
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    ret
}

// upstream: intel_guc_submission.c guc_context_policy_init_v69()
fn guc_context_policy_init_v69(engine: &intel_engine_cs, desc: &mut guc_lrc_desc_v69) {
    desc.policy_flags = 0;
    if engine.flags & I915_ENGINE_WANT_FORCED_PREEMPTION != 0 {
        desc.policy_flags |= CONTEXT_POLICY_FLAG_PREEMPT_TO_IDLE_V69;
    }
    // Zero disables either time quantum.
    gem_bug_on!(overflows_u32(engine.props.timeslice_duration_ms * 1000));
    gem_bug_on!(overflows_u32(engine.props.preempt_timeout_ms * 1000));
    desc.execution_quantum = (engine.props.timeslice_duration_ms * 1000) as u32;
    desc.preemption_timeout = (engine.props.preempt_timeout_ms * 1000) as u32;
}

// upstream: intel_guc_submission.c map_guc_prio_to_lrc_desc_prio()
fn map_guc_prio_to_lrc_desc_prio(prio: u8) -> u8 {
    // Matches map_i915_prio_to_guc_prio(): i915 priority below NORMAL maps to
    // GUC_CLIENT_PRIORITY_NORMAL.
    match prio as u32 {
        GUC_CLIENT_PRIORITY_KMD_NORMAL => GEN12_CTX_PRIORITY_NORMAL as u8,
        GUC_CLIENT_PRIORITY_NORMAL => GEN12_CTX_PRIORITY_LOW as u8,
        GUC_CLIENT_PRIORITY_HIGH | GUC_CLIENT_PRIORITY_KMD_HIGH => GEN12_CTX_PRIORITY_HIGH as u8,
        other => {
            missing_case!(other);
            GEN12_CTX_PRIORITY_NORMAL as u8
        }
    }
}

// upstream: intel_guc_submission.c prepare_context_registration_info_v69()
fn prepare_context_registration_info_v69(ce: &mut intel_context) {
    let engine = ce.engine;
    let guc = unsafe { gt_to_guc((*engine).gt) };
    let ctx_id = ce.guc_id.id as u32;
    gem_bug_on!(unsafe { (*engine).mask } == 0);
    // LRC and CT VMAs must use the same memory region because the write barrier
    // is performed according to the CT VMA region.
    gem_bug_on!(
        unsafe { i915_gem_object_is_lmem((*(*guc).ct.vma).obj) }
            != unsafe { i915_gem_object_is_lmem((*(*ce.ring).vma).obj) }
    );

    let mut desc = __get_lrc_desc_v69(unsafe { &mut *guc }, ctx_id);
    gem_bug_on!(desc.is_null());
    unsafe {
        (*desc).engine_class =
            crate::intel_guc_fwif_types_upstream::engine_class_to_guc_class(unsafe {
                (*engine).class
            });
        (*desc).engine_submit_mask = unsafe { (*engine).logical_mask as u32 };
        (*desc).hw_context_desc = ce.lrc.lrca;
        (*desc).priority = ce.guc_state.prio as u32;
        (*desc).context_flags = CONTEXT_REGISTRATION_FLAG_KMD;
        guc_context_policy_init_v69(unsafe { &*engine }, &mut *desc);
    }

    // A parent registers its process descriptor/work queue and all children.
    if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
        ce.parallel.guc.wqi_tail = 0;
        ce.parallel.guc.wqi_head = 0;
        unsafe {
            (*desc).process_desc = i915_ggtt_offset(ce.state) + __get_parent_scratch_offset(ce);
            (*desc).wq_addr = i915_ggtt_offset(ce.state) + __get_wq_offset(ce);
            (*desc).wq_size = WQ_SIZE as u32;
        }
        let pdesc = __get_process_desc_v69(ce);
        unsafe {
            core::ptr::write_bytes(pdesc, 0, 1);
            (*pdesc).stage_id = ce.guc_id.id as u32;
            (*pdesc).wq_base_addr = (*desc).wq_addr as u64;
            (*pdesc).wq_size_bytes = (*desc).wq_size;
            (*pdesc).wq_status = WQ_STATUS_ACTIVE;
            ce.parallel.guc.wq_head = core::ptr::addr_of_mut!((*pdesc).head);
            ce.parallel.guc.wq_tail = core::ptr::addr_of_mut!((*pdesc).tail);
            ce.parallel.guc.wq_status = core::ptr::addr_of_mut!((*pdesc).wq_status);
        }
        for_each_child!(ce, child, {
            desc = __get_lrc_desc_v69(unsafe { &mut *guc }, unsafe { (*child).guc_id.id as u32 });
            unsafe {
                (*desc).engine_class =
                    crate::intel_guc_fwif_types_upstream::engine_class_to_guc_class(unsafe {
                        (*engine).class
                    });
                (*desc).hw_context_desc = unsafe { (&(*child).lrc).lrca };
                (*desc).priority = (*ce).guc_state.prio as u32;
                (*desc).context_flags = CONTEXT_REGISTRATION_FLAG_KMD;
                guc_context_policy_init_v69(unsafe { &*engine }, &mut *desc);
            }
        });
        clear_children_join_go_memory(ce);
    }
}

// upstream: intel_guc_submission.c prepare_context_registration_info_v70()
fn prepare_context_registration_info_v70(
    ce: &mut intel_context,
    info: &mut guc_ctxt_registration_info,
) {
    let engine = ce.engine;
    let guc = unsafe { gt_to_guc((*engine).gt) };
    let ctx_id = ce.guc_id.id as u32;
    gem_bug_on!(unsafe { (*engine).mask } == 0);
    // LRC and CT VMAs must use the same memory region because the write barrier
    // is performed according to the CT VMA region.
    gem_bug_on!(
        unsafe { i915_gem_object_is_lmem((*(*guc).ct.vma).obj) }
            != unsafe { i915_gem_object_is_lmem((*(*ce.ring).vma).obj) }
    );

    unsafe { core::ptr::write_bytes(info, 0, 1) };
    info.context_idx = ctx_id;
    info.engine_class =
        crate::intel_guc_fwif_types_upstream::engine_class_to_guc_class(unsafe { (*engine).class })
            as u32;
    info.engine_submit_mask = unsafe { (*engine).logical_mask as u32 };
    // GuC accepts a 64-bit LRCA although i915/HW currently supports 32 bits.
    info.hwlrca_lo = lower_32_bits(ce.lrc.lrca);
    info.hwlrca_hi = upper_32_bits(ce.lrc.lrca);
    if unsafe { (*engine).flags } & I915_ENGINE_HAS_EU_PRIORITY != 0 {
        info.hwlrca_lo |= map_guc_prio_to_lrc_desc_prio(ce.guc_state.prio) as u32;
    }
    info.flags = CONTEXT_REGISTRATION_FLAG_KMD as u32;

    // A parent registers a process descriptor/work queue and all children.
    if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
        ce.parallel.guc.wqi_tail = 0;
        ce.parallel.guc.wqi_head = 0;
        let wq_desc_offset =
            unsafe { i915_ggtt_offset(ce.state) } as u64 + __get_parent_scratch_offset(ce) as u64;
        let wq_base_offset = unsafe { i915_ggtt_offset(ce.state) } as u64 + __get_wq_offset(ce) as u64;
        info.wq_desc_lo = lower_32_bits(wq_desc_offset);
        info.wq_desc_hi = upper_32_bits(wq_desc_offset);
        info.wq_base_lo = lower_32_bits(wq_base_offset);
        info.wq_base_hi = upper_32_bits(wq_base_offset);
        info.wq_size = WQ_SIZE as u32;
        let wq_desc = __get_wq_desc_v70(ce);
        unsafe {
            core::ptr::write_bytes(wq_desc, 0, 1);
            (*wq_desc).wq_status = WQ_STATUS_ACTIVE;
            ce.parallel.guc.wq_head = core::ptr::addr_of_mut!((*wq_desc).head);
            ce.parallel.guc.wq_tail = core::ptr::addr_of_mut!((*wq_desc).tail);
            ce.parallel.guc.wq_status = core::ptr::addr_of_mut!((*wq_desc).wq_status);
        }
        clear_children_join_go_memory(ce);
    }
}

// upstream: intel_guc_submission.c try_context_registration()
fn try_context_registration(ce: &mut intel_context, loop_on_busy: bool) -> i32 {
    let engine = ce.engine;
    let runtime_pm = unsafe { (*(*engine).uncore).rpm };
    let guc = unsafe { gt_to_guc((*engine).gt) };
    let ctx_id = ce.guc_id.id as u32;
    gem_bug_on!(!sched_state_is_init(ce));
    let context_registered = ctx_id_mapped(unsafe { &mut *guc }, ctx_id);
    clr_ctx_id_mapping(unsafe { &mut *guc }, ctx_id);
    set_ctx_id_mapping(unsafe { &mut *guc }, ctx_id, ce as *mut _);

    // The lookup xarray says whether this hardware context is registered. It
    // can be registered because an ID was stolen or its LRCA descriptor moved;
    // either case must deregister the old context before registering this one.
    let mut ret = 0;
    if context_registered {
        let mut flags: c_ulong = 0;
        unsafe { trace_intel_context_steal_guc_id(ce) };
        gem_bug_on!(!loop_on_busy);
        spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
        let disabled = submission_disabled(unsafe { &*guc });
        if likely(!disabled) {
            set_context_wait_for_deregister_to_register(ce);
            intel_context_get(&mut *ce);
        }
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        if unlikely(disabled) {
            clr_ctx_id_mapping(unsafe { &mut *guc }, ctx_id);
            return 0; // It will be registered later.
        }
        // The stolen id is also the id of the context being deregistered.
        with_intel_runtime_pm!(runtime_pm, wakeref, {
            ret = deregister_context(ce, ce.guc_id.id as u32);
        });
        if unlikely(ret == -ENODEV) {
            ret = 0; // It will be registered later.
        }
    } else {
        with_intel_runtime_pm!(runtime_pm, wakeref, {
            ret = register_context(ce, loop_on_busy);
        });
        if unlikely(ret == -EBUSY) {
            clr_ctx_id_mapping(unsafe { &mut *guc }, ctx_id);
        } else if unlikely(ret == -ENODEV) {
            clr_ctx_id_mapping(unsafe { &mut *guc }, ctx_id);
            ret = 0; // It will be registered later.
        }
    }
    ret
}

// upstream: intel_guc_submission.c __guc_context_pre_pin()
fn __guc_context_pre_pin(
    ce: &mut intel_context,
    engine: &intel_engine_cs,
    ww: &mut i915_gem_ww_ctx,
    vaddr: &mut *mut c_void,
) -> i32 {
    unsafe {
        lrc_pre_pin(
            ce,
            engine as *const intel_engine_cs as *mut intel_engine_cs,
            ww,
            vaddr,
        )
    }
}

// upstream: intel_guc_submission.c __guc_context_pin()
fn __guc_context_pin(ce: &mut intel_context, engine: &intel_engine_cs, vaddr: *mut c_void) -> i32 {
    if unsafe { i915_ggtt_offset(ce.state) } != ce.lrc.lrca & CTX_GTT_ADDRESS_MASK {
        set_bit(CONTEXT_LRCA_DIRTY, &mut ce.flags);
    }
    // GuC contexts are pinned in guc_request_alloc; see that function for why.
    unsafe {
        lrc_pin(
            ce,
            engine as *const intel_engine_cs as *mut intel_engine_cs,
            vaddr,
        )
    }
}

// upstream: intel_guc_submission.c guc_context_pre_pin()
fn guc_context_pre_pin(
    ce: &mut intel_context,
    ww: &mut i915_gem_ww_ctx,
    vaddr: &mut *mut c_void,
) -> i32 {
    __guc_context_pre_pin(ce, unsafe { &*ce.engine }, ww, vaddr)
}

// upstream: intel_guc_submission.c guc_context_pin()
fn guc_context_pin(ce: &mut intel_context, vaddr: *mut c_void) -> i32 {
    let engine = unsafe { &*ce.engine };
    let ret = __guc_context_pin(&mut *ce, engine, vaddr);
    if likely(ret == 0 && !intel_context_is_barrier(&mut *ce)) {
        unsafe { intel_engine_pm_get(ce.engine) };
    }
    ret
}

// upstream: intel_guc_submission.c guc_context_unpin()
fn guc_context_unpin(ce: &mut intel_context) {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    __guc_context_update_stats(&mut *ce);
    unpin_guc_id(guc, &mut *ce);
    unsafe { lrc_unpin(ce) };
    if likely(!intel_context_is_barrier(&mut *ce)) {
        unsafe { intel_engine_pm_put_async(ce.engine) };
    }
}

// upstream: intel_guc_submission.c guc_context_post_unpin()
fn guc_context_post_unpin(ce: &mut intel_context) {
    unsafe { lrc_post_unpin(ce) };
}

// upstream: intel_guc_submission.c __guc_context_sched_enable()
fn __guc_context_sched_enable(guc: &mut intel_guc, ce: &intel_context) {
    let action = [
        INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_SET,
        ce.guc_id.id as u32,
        GUC_CONTEXT_ENABLE,
    ];
    unsafe { trace_intel_context_sched_enable(ce as *const _ as *mut _) };
    guc_submission_send_busy_loop(
        guc,
        &action,
        action.len() as u32,
        G2H_LEN_DW_SCHED_CONTEXT_MODE_SET,
        true,
    );
}

// upstream: intel_guc_submission.c __guc_context_sched_disable()
fn __guc_context_sched_disable(guc: &mut intel_guc, ce: &intel_context, guc_id: u16) {
    let action = [
        INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_SET,
        guc_id as u32, // ce->guc_id.id is not stable.
        crate::intel_guc_fwif_types_upstream::GUC_CONTEXT_DISABLE,
    ];
    gem_bug_on!(guc_id as u32 == GUC_INVALID_CONTEXT_ID);
    gem_bug_on!(unsafe { intel_context_is_child(ce as *const intel_context as *mut intel_context) });
    unsafe { trace_intel_context_sched_disable(ce) };
    guc_submission_send_busy_loop(
        guc,
        &action,
        action.len() as u32,
        G2H_LEN_DW_SCHED_CONTEXT_MODE_SET,
        true,
    );
}

// upstream: intel_guc_submission.c guc_blocked_fence_complete()
fn guc_blocked_fence_complete(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    if !i915_sw_fence_done(&ce.guc_state.blocked) {
        unsafe { i915_sw_fence_complete(&mut ce.guc_state.blocked) };
    }
}

// upstream: intel_guc_submission.c guc_blocked_fence_reinit()
fn guc_blocked_fence_reinit(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    gem_bug_on!(!i915_sw_fence_done(&ce.guc_state.blocked));
    // This fence is normally complete. Arm it while schedule-disable is pending
    // and complete it when the matching disable-done message arrives.
    unsafe { i915_sw_fence_fini(&mut ce.guc_state.blocked) };
    unsafe { i915_sw_fence_reinit(&mut ce.guc_state.blocked) };
    i915_sw_fence_await(&mut ce.guc_state.blocked);
    unsafe { i915_sw_fence_commit(&mut ce.guc_state.blocked) };
}

// upstream: intel_guc_submission.c prep_context_pending_disable()
fn prep_context_pending_disable(ce: &mut intel_context) -> u16 {
    lockdep_assert_held(&ce.guc_state.lock);
    set_context_pending_disable(ce);
    clr_context_enabled(ce);
    guc_blocked_fence_reinit(ce);
    intel_context_get(&mut *ce);
    ce.guc_id.id as u16
}

// upstream: intel_guc_submission.c guc_context_block()
fn guc_context_block(ce: &mut intel_context) -> &i915_sw_fence {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let mut flags: c_ulong = 0;
    let runtime_pm = unsafe { (*(*ce.engine).uncore).rpm };
    gem_bug_on!(intel_context_is_child(&mut *ce));
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    incr_context_blocked(&mut *ce);
    let enabled = context_enabled(&*ce);
    if unlikely(!enabled || submission_disabled(guc)) {
        if enabled {
            clr_context_enabled(&mut *ce);
        }
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        return &ce.guc_state.blocked;
    }
    // +2 balances the schedule-disable-complete CTB handler's -2 pin_count.
    atomic_add(2, &mut ce.pin_count);
    let guc_id = prep_context_pending_disable(&mut *ce);
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    with_intel_runtime_pm!(runtime_pm, wakeref, {
        __guc_context_sched_disable(guc, ce, guc_id);
    });
    &ce.guc_state.blocked
}

const SCHED_STATE_MULTI_BLOCKED_MASK: u32 = SCHED_STATE_BLOCKED_MASK & !SCHED_STATE_BLOCKED;
const SCHED_STATE_NO_UNBLOCK: u32 =
    SCHED_STATE_MULTI_BLOCKED_MASK | SCHED_STATE_PENDING_DISABLE | SCHED_STATE_BANNED;

// upstream: intel_guc_submission.c context_cant_unblock()
fn context_cant_unblock(guc: &mut intel_guc, ce: &mut intel_context) -> bool {
    lockdep_assert_held(&ce.guc_state.lock);
    ce.guc_state.sched_state & SCHED_STATE_NO_UNBLOCK != 0
        || context_guc_id_invalid(ce)
        || !ctx_id_mapped(guc, ce.guc_id.id as u32)
        || !unsafe { intel_context_is_pinned(ce as *const _ as *mut _) }
}

// upstream: intel_guc_submission.c guc_context_unblock()
fn guc_context_unblock(ce: &mut intel_context) {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let mut flags: c_ulong = 0;
    let runtime_pm = unsafe { (*(*ce.engine).uncore).rpm };
    gem_bug_on!(context_enabled(ce));
    gem_bug_on!(intel_context_is_child(&mut *ce));
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    let enable = if unlikely(submission_disabled(guc) || context_cant_unblock(guc, ce)) {
        false
    } else {
        set_context_pending_enable(ce);
        set_context_enabled(ce);
        intel_context_get(&mut *ce);
        true
    };
    decr_context_blocked(ce);
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    if enable {
        with_intel_runtime_pm!(runtime_pm, wakeref, {
            __guc_context_sched_enable(guc, ce);
        });
    }
}

// upstream: intel_guc_submission.c guc_context_cancel_request()
fn guc_context_cancel_request(ce: &mut intel_context, rq: &mut i915_request) {
    if i915_sw_fence_signaled(&rq.submit) {
        // Keep the request's embedded context as a raw pointer: the upstream
        // operation intentionally accesses distinct fields of the same request.
        let block_context = unsafe { request_to_scheduling_context_mut(rq) as *mut intel_context };
        intel_context_get(&mut *ce);
        let fence = guc_context_block(unsafe { &mut *block_context });
        unsafe { i915_sw_fence_wait(fence as *const _ as *mut _) };
        if !i915_request_completed(&mut *rq) {
            unsafe { __i915_request_skip(&mut *rq) };
            let head = unsafe { intel_ring_wrap(ce.ring, rq.head) };
            guc_reset_state(ce, head, true);
        }
        guc_context_unblock(unsafe { &mut *block_context });
        intel_context_put(&mut *ce);
    }
}

// upstream: intel_guc_submission.c __guc_context_set_preemption_timeout()
fn __guc_context_set_preemption_timeout(guc: &mut intel_guc, guc_id: u16, preemption_timeout: u32) {
    if GUC_SUBMIT_VER(guc) >= MAKE_GUC_VER(1, 0, 0) {
        let mut policy = context_policy::default();
        __guc_context_policy_start_klv(&mut policy, guc_id);
        __guc_context_policy_add_preemption_timeout(&mut policy, preemption_timeout);
        __guc_context_set_context_policies(guc, &policy, true);
    } else {
        let action = [
            INTEL_GUC_ACTION_V69_SET_CONTEXT_PREEMPTION_TIMEOUT,
            guc_id as u32,
            preemption_timeout,
        ];
        unsafe { intel_guc_send_busy_loop(guc, action.as_ptr(), action.len() as u32, 0, true) };
    }
}

// upstream: intel_guc_submission.c guc_context_revoke()
fn guc_context_revoke(ce: &mut intel_context, rq: &mut i915_request, preempt_timeout_ms: u32) {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let runtime_pm = unsafe { &mut (*(*(*ce.engine).gt).i915).runtime_pm };
    let mut flags: c_ulong = 0;
    gem_bug_on!(intel_context_is_child(&mut *ce));
    guc_flush_submissions(guc);
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    set_context_banned(ce);
    if submission_disabled(guc) || (!context_enabled(&*ce) && !context_pending_disable(&*ce)) {
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        guc_cancel_context_requests(&mut *ce);
        unsafe { intel_engine_signal_breadcrumbs(ce.engine) };
    } else if !context_pending_disable(&*ce) {
        // Schedule-disable completion calls sched_disable_unpin and decrements
        // pin_count by two, so take the corresponding two pins here.
        atomic_add(2, &mut ce.pin_count);
        let guc_id = prep_context_pending_disable(&mut *ce);
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        // Set the minimum preemption timeout as well as disabling scheduling,
        // to kick the banned context off hardware as soon as possible.
        with_intel_runtime_pm!(runtime_pm, wakeref, {
            __guc_context_set_preemption_timeout(guc, guc_id, preempt_timeout_ms);
            __guc_context_sched_disable(guc, ce, guc_id);
        });
    } else {
        if !context_guc_id_invalid(&*ce) {
            with_intel_runtime_pm!(runtime_pm, wakeref, {
                __guc_context_set_preemption_timeout(guc, ce.guc_id.id as u16, preempt_timeout_ms);
            });
        }
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    }
}

// upstream: intel_guc_submission.c do_sched_disable()
fn do_sched_disable(guc: &mut intel_guc, ce: &mut intel_context, flags: u64) {
    let runtime_pm = unsafe { &(*(*(*ce.engine).gt).i915).runtime_pm };
    lockdep_assert_held(&ce.guc_state.lock);
    let guc_id = prep_context_pending_disable(ce);
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    with_intel_runtime_pm!(runtime_pm, wakeref, {
        __guc_context_sched_disable(guc, &mut *ce, guc_id);
    });
}

// upstream: intel_guc_submission.c bypass_sched_disable()
fn bypass_sched_disable(guc: &mut intel_guc, ce: &mut intel_context) -> bool {
    lockdep_assert_held(&ce.guc_state.lock);
    gem_bug_on!(intel_context_is_child(&mut *ce));
    if submission_disabled(guc)
        || context_guc_id_invalid(&ce)
        || !ctx_id_mapped(guc, ce.guc_id.id as u32)
    {
        clr_context_enabled(ce);
        return true;
    }
    !context_enabled(ce)
}

// upstream: intel_guc_submission.c __delay_sched_disable()
fn __delay_sched_disable(wrk: &mut work_struct) {
    let ce = container_of!(wrk, intel_context, guc_state.sched_disable_delay_work.work);
    let ce = unsafe { &mut *ce };
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    if bypass_sched_disable(guc, ce) {
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        intel_context_sched_disable_unpin(ce as *mut _);
    } else {
        do_sched_disable(guc, ce, flags);
    }
}

// upstream: intel_guc_submission.c guc_id_pressure()
fn guc_id_pressure(guc: &intel_guc, ce: &intel_context) -> bool {
    // Parent contexts are permanently pinned, so disable scheduling immediately
    // when unpinning them.
    if unsafe { intel_context_is_parent(ce as *const _ as *mut _) } {
        return true;
    }
    // Otherwise disable immediately after the available-ID threshold is passed.
    guc.submission_state.guc_ids_in_use > guc.submission_state.sched_disable_gucid_threshold
}

// upstream: intel_guc_submission.c guc_context_sched_disable()
fn guc_context_sched_disable(ce: &mut intel_context) {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let delay = guc.submission_state.sched_disable_delay_ms;
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    if bypass_sched_disable(guc, &mut *ce) {
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        intel_context_sched_disable_unpin(ce as *mut _);
    } else if !intel_context_is_closed(&mut *ce) && !guc_id_pressure(guc, ce) && delay != 0 {
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        unsafe {
            mod_delayed_work(
                system_dfl_wq,
                &mut ce.guc_state.sched_disable_delay_work,
                msecs_to_jiffies(delay),
            )
        };
    } else {
        do_sched_disable(guc, &mut *ce, flags);
    }
}

// upstream: intel_guc_submission.c guc_context_close()
fn guc_context_close(ce: &mut intel_context) {
    if test_bit(CONTEXT_GUC_INIT, &ce.flags)
        && unsafe { cancel_delayed_work(&mut ce.guc_state.sched_disable_delay_work) }
    {
        __delay_sched_disable(&mut ce.guc_state.sched_disable_delay_work.work);
    }
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    set_context_close_done(ce);
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
}

// upstream: intel_guc_submission.c guc_lrc_desc_unpin()
#[inline]
fn guc_lrc_desc_unpin(ce: &mut intel_context) -> i32 {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let gt = unsafe { guc_to_gt(guc) };
    let mut flags: c_ulong = 0;
    gem_bug_on!(!unsafe { intel_gt_pm_is_awake(gt) });
    gem_bug_on!(!ctx_id_mapped(guc, ce.guc_id.id as u32));
    gem_bug_on!(ce as *mut _ != __get_context(guc, ce.guc_id.id as u32));
    gem_bug_on!(context_enabled(ce));
    // Close the race with reset while marking the context destroyed.
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    let disabled = submission_disabled(guc);
    if likely(!disabled) {
        // Take a GT PM ref; a later G2H IRQ releases it.
        unsafe { __intel_gt_pm_get(gt) };
        set_context_destroyed(ce);
        clr_context_registered(ce);
    }
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    if unlikely(disabled) {
        release_guc_id(guc, ce);
        __guc_context_destroy(ce);
        return 0;
    }

    // GuC is active, but suspend can race the deregister H2G. Undo the state if
    // it fails, unless reset already changed the state and released its ref.
    let ret = deregister_context(ce, ce.guc_id.id as u32);
    if ret != 0 {
        let mut flags: c_ulong = 0;
        spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
        let pending_destroyed = context_destroyed(ce);
        if pending_destroyed {
            set_context_registered(ce);
            clr_context_destroyed(ce);
        }
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        // GT is awake here; this asynchronous put decrements now, but the API
        // requires it to happen after releasing the context lock.
        if pending_destroyed {
            unsafe { intel_wakeref_put_async(&mut (*gt).wakeref) };
        }
    }
    ret
}

// upstream: intel_guc_submission.c __guc_context_destroy()
fn __guc_context_destroy(ce: &mut intel_context) {
    gem_bug_on!(
        ce.guc_state.prio_count[GUC_CLIENT_PRIORITY_KMD_HIGH as usize] != 0
            || ce.guc_state.prio_count[GUC_CLIENT_PRIORITY_HIGH as usize] != 0
            || ce.guc_state.prio_count[GUC_CLIENT_PRIORITY_KMD_NORMAL as usize] != 0
            || ce.guc_state.prio_count[GUC_CLIENT_PRIORITY_NORMAL as usize] != 0
    );
    unsafe { lrc_fini(ce) };
    unsafe { intel_context_fini(ce) };
    if unsafe { intel_engine_is_virtual(ce.engine) } {
        let ve = container_of!(ce, guc_virtual_engine, context);
        if unsafe { !(*ve).base.breadcrumbs.is_null() } {
            unsafe { intel_breadcrumbs_put((*ve).base.breadcrumbs) };
        }
        unsafe { kfree(ve) };
    } else {
        unsafe { intel_context_free(ce) };
    }
}

// upstream: intel_guc_submission.c guc_flush_destroyed_contexts()
fn guc_flush_destroyed_contexts(guc: &mut intel_guc) {
    gem_bug_on!(!submission_disabled(guc) && guc_submission_initialized(guc));
    while !list_empty(&guc.submission_state.destroyed_contexts) {
        let mut flags: c_ulong = 0;
        spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
        let ce = list_first_entry_or_null!(
            &mut guc.submission_state.destroyed_contexts,
            intel_context,
            destroyed_link,
        );
        if !ce.is_null() {
            unsafe { list_del_init(&mut (*ce).destroyed_link) };
        }
        spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
        if ce.is_null() {
            break;
        }
        release_guc_id(guc, unsafe { &mut *ce });
        __guc_context_destroy(unsafe { &mut *ce });
    }
}

// upstream: intel_guc_submission.c deregister_destroyed_contexts()
fn deregister_destroyed_contexts(guc: &mut intel_guc) {
    while !list_empty(&guc.submission_state.destroyed_contexts) {
        let mut flags: c_ulong = 0;
        spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
        let ce = list_first_entry_or_null!(
            &mut guc.submission_state.destroyed_contexts,
            intel_context,
            destroyed_link,
        );
        if !ce.is_null() {
            unsafe { list_del_init(&mut (*ce).destroyed_link) };
        }
        spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
        if ce.is_null() {
            break;
        }
        if guc_lrc_desc_unpin(unsafe { &mut *ce }) != 0 {
            // CT may have disconnected in a suspend/resume race. Requeue this
            // context for a later deregister event or GuC sanitize/reset and
            // stop now because failed H2Gs could keep the list nonempty forever.
            let mut flags: c_ulong = 0;
            spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
            unsafe {
                list_add_tail(
                    unsafe { &mut (*ce).destroyed_link },
                    &mut guc.submission_state.destroyed_contexts,
                )
            };
            spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
            break;
        }
    }
}

// upstream: intel_guc_submission.c destroyed_worker_func()
fn destroyed_worker_func(w: &mut work_struct) {
    let guc_ptr = container_of!(w, intel_guc, submission_state.destroyed_worker);
    let gt = unsafe { guc_to_gt(guc_ptr) };
    let guc = unsafe { &mut *guc_ptr };
    if !unsafe { intel_guc_is_ready(&mut *guc) } {
        // Pending destroys are handled at GuC reset at suspend, or another
        // destruction trigger after resume.
        return;
    }
    with_intel_gt_pm!(gt, wakeref, {
        deregister_destroyed_contexts(&mut *guc);
    });
}

// upstream: intel_guc_submission.c guc_context_destroy()
fn guc_context_destroy(kref: &mut kref) {
    let ce = container_of!(kref, intel_context, r#ref);
    let ce = unsafe { &mut *ce };
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
    let destroy = submission_disabled(guc)
        || context_guc_id_invalid(ce)
        || !ctx_id_mapped(guc, ce.guc_id.id as u32);
    if likely(!destroy) {
        if !list_empty(&ce.guc_id.link) {
            unsafe { list_del_init(&mut ce.guc_id.link) };
        }
        unsafe { list_add_tail(
            &mut ce.destroyed_link,
            &mut guc.submission_state.destroyed_contexts,
        ) };
    } else {
        __release_guc_id(guc, ce);
    }
    spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
    if unlikely(destroy) {
        __guc_context_destroy(ce);
        return;
    }
    // A worker issues the H2G because it may need the first GT PM wakeref,
    // which is not permitted from an atomic context.
    unsafe { queue_work(system_dfl_wq, &mut guc.submission_state.destroyed_worker) };
}

// upstream: intel_guc_submission.c guc_context_alloc()
fn guc_context_alloc(ce: &mut intel_context) -> i32 {
    unsafe { lrc_alloc(ce, ce.engine) }
}

// upstream: intel_guc_submission.c __guc_context_set_prio()
fn __guc_context_set_prio(guc: &mut intel_guc, ce: &intel_context) {
    if GUC_SUBMIT_VER(guc) >= MAKE_GUC_VER(1, 0, 0) {
        let mut policy = context_policy::default();
        __guc_context_policy_start_klv(&mut policy, ce.guc_id.id as u16);
        __guc_context_policy_add_priority(&mut policy, ce.guc_state.prio as u32);
        __guc_context_set_context_policies(guc, &policy, true);
    } else {
        let action = [
            INTEL_GUC_ACTION_V69_SET_CONTEXT_PRIORITY,
            ce.guc_id.id as u32,
            ce.guc_state.prio as u32,
        ];
        guc_submission_send_busy_loop(guc, &action, action.len() as u32, 0, true);
    }
}

// upstream: intel_guc_submission.c guc_context_set_prio()
fn guc_context_set_prio(guc: &mut intel_guc, ce: &mut intel_context, prio: u8) {
    gem_bug_on!(
        (prio as u32) < GUC_CLIENT_PRIORITY_KMD_HIGH || (prio as u32) > GUC_CLIENT_PRIORITY_NORMAL
    );
    lockdep_assert_held(&ce.guc_state.lock);
    if ce.guc_state.prio == prio || submission_disabled(guc) || !context_registered(ce) {
        ce.guc_state.prio = prio;
        return;
    }
    ce.guc_state.prio = prio;
    __guc_context_set_prio(guc, ce);
    trace_intel_context_set_prio(ce);
}

// upstream: intel_guc_submission.c map_i915_prio_to_guc_prio()
#[inline]
fn map_i915_prio_to_guc_prio(prio: i32) -> u8 {
    if prio == I915_PRIORITY_NORMAL {
        GUC_CLIENT_PRIORITY_KMD_NORMAL as u8
    } else if prio < I915_PRIORITY_NORMAL {
        GUC_CLIENT_PRIORITY_NORMAL as u8
    } else if prio < I915_PRIORITY_DISPLAY {
        GUC_CLIENT_PRIORITY_HIGH as u8
    } else {
        GUC_CLIENT_PRIORITY_KMD_HIGH as u8
    }
}

// upstream: intel_guc_submission.c add_context_inflight_prio()
#[inline]
fn add_context_inflight_prio(ce: &mut intel_context, guc_prio: u8) {
    lockdep_assert_held(&ce.guc_state.lock);
    gem_bug_on!(guc_prio as usize >= ce.guc_state.prio_count.len());
    ce.guc_state.prio_count[guc_prio as usize] += 1;
    gem_warn_on!(ce.guc_state.prio_count[guc_prio as usize] == 0); // Overflow protection.
}

// upstream: intel_guc_submission.c sub_context_inflight_prio()
#[inline]
fn sub_context_inflight_prio(ce: &mut intel_context, guc_prio: u8) {
    lockdep_assert_held(&ce.guc_state.lock);
    gem_bug_on!(guc_prio as usize >= ce.guc_state.prio_count.len());
    gem_warn_on!(ce.guc_state.prio_count[guc_prio as usize] == 0); // Underflow protection.
    ce.guc_state.prio_count[guc_prio as usize] -= 1;
}

// upstream: intel_guc_submission.c update_context_prio()
#[inline]
fn update_context_prio(ce: &mut intel_context) {
    let guc = unsafe { &mut (*(*ce.engine).gt).uc.guc };
    build_bug_on!(GUC_CLIENT_PRIORITY_KMD_HIGH != 0);
    build_bug_on!(GUC_CLIENT_PRIORITY_KMD_HIGH > GUC_CLIENT_PRIORITY_NORMAL);
    lockdep_assert_held(&ce.guc_state.lock);
    for i in 0..ce.guc_state.prio_count.len() {
        if ce.guc_state.prio_count[i] != 0 {
            guc_context_set_prio(guc, ce, i as u8);
            break;
        }
    }
}

// upstream: intel_guc_submission.c new_guc_prio_higher()
#[inline]
fn new_guc_prio_higher(old_guc_prio: u8, new_guc_prio: u8) -> bool {
    // Lower values represent higher priorities.
    new_guc_prio < old_guc_prio
}

// upstream: intel_guc_submission.c add_to_context()
fn add_to_context(rq: &mut i915_request) {
    let new_guc_prio = map_i915_prio_to_guc_prio(rq_prio(rq));
    let ce = unsafe { intel_context_to_parent(rq.context) };
    gem_bug_on!(intel_context_is_child(unsafe { &mut *ce }));
    gem_bug_on!(rq.guc_prio == GUC_PRIO_FINI);
    spin_lock(unsafe { &mut (*ce).guc_state.lock });
    unsafe { list_move_tail(&mut rq.sched.link, &mut (*ce).guc_state.requests) };
    if rq.guc_prio == GUC_PRIO_INIT {
        rq.guc_prio = new_guc_prio;
        add_context_inflight_prio(unsafe { &mut *ce }, rq.guc_prio);
    } else if new_guc_prio_higher(rq.guc_prio, new_guc_prio) {
        sub_context_inflight_prio(unsafe { &mut *ce }, rq.guc_prio);
        rq.guc_prio = new_guc_prio;
        add_context_inflight_prio(unsafe { &mut *ce }, rq.guc_prio);
    }
    update_context_prio(unsafe { &mut *ce });
    spin_unlock(unsafe { &mut (*ce).guc_state.lock });
}

unsafe extern "C" fn add_to_context_callback(rq: *mut i915_request) {
    // SAFETY: IntelEngineCs.add_active_request supplies a live request.
    add_to_context(unsafe { &mut *rq });
}

// upstream: intel_guc_submission.c guc_prio_fini()
fn guc_prio_fini(rq: &mut i915_request, ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    if rq.guc_prio != GUC_PRIO_INIT && rq.guc_prio != GUC_PRIO_FINI {
        sub_context_inflight_prio(ce, rq.guc_prio);
        update_context_prio(ce);
    }
    rq.guc_prio = GUC_PRIO_FINI;
}

// upstream: intel_guc_submission.c remove_from_context()
fn remove_from_context(rq: &mut i915_request) {
    let ce = unsafe { intel_context_to_parent(rq.context) };
    gem_bug_on!(intel_context_is_child(unsafe { &mut *ce }));
    spin_lock_irq(unsafe { &mut (*ce).guc_state.lock });
    unsafe { list_del_init(&mut rq.sched.link) };
    clear_bit(I915_FENCE_FLAG_PQUEUE, &mut rq.fence.flags);
    // Prevent future __await_execution() callbacks, then flush existing ones.
    set_bit(I915_FENCE_FLAG_ACTIVE, &mut rq.fence.flags);
    guc_prio_fini(rq, unsafe { &mut *ce });
    spin_unlock_irq(unsafe { &mut (*ce).guc_state.lock });
    atomic_dec(unsafe { &mut (*ce).guc_id.r#ref });
    unsafe { i915_request_notify_execute_cb_imm(rq) };
}

unsafe extern "C" fn remove_from_context_callback(rq: *mut i915_request) {
    // SAFETY: IntelEngineCs.remove_active_request supplies a live request.
    remove_from_context(unsafe { &mut *rq });
}

// upstream: intel_guc_submission.c submit_work_cb()
unsafe extern "C" fn submit_work_cb(wrk: *mut irq_work) {
    let rq = container_of!(wrk, i915_request, submit_work);
    might_lock!(&(*(*(*rq).engine).sched_engine).lock);
    i915_sw_fence_complete(&mut (*rq).submit);
}

// upstream: intel_guc_submission.c __guc_signal_context_fence()
fn __guc_signal_context_fence(ce: &mut intel_context) {
    lockdep_assert_held(&ce.guc_state.lock);
    if !list_empty(&ce.guc_state.fences) {
        trace_intel_context_fence_release(ce);
    }
    // Queue IRQ work to preserve sched_engine.lock -> guc_state.lock ordering.
    let mut rq: *mut i915_request = core::ptr::null_mut();
    let mut rn: *mut i915_request = core::ptr::null_mut();
    list_for_each_entry_safe!(rq, rn, &ce.guc_state.fences, guc_fence_link, {
        unsafe { list_del(&mut (*rq).guc_fence_link) };
        unsafe { irq_work_queue(&mut (*rq).submit_work) };
    });
    unsafe { INIT_LIST_HEAD(&mut ce.guc_state.fences) };
}

// upstream: intel_guc_submission.c guc_signal_context_fence()
fn guc_signal_context_fence(ce: &mut intel_context) {
    gem_bug_on!(intel_context_is_child(&mut *ce));
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    clr_context_wait_for_deregister_to_register(&mut *ce);
    __guc_signal_context_fence(&mut *ce);
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
}

// upstream: intel_guc_submission.c context_needs_register()
fn context_needs_register(ce: &mut intel_context, new_guc_id: bool) -> bool {
    (new_guc_id
        || test_bit(CONTEXT_LRCA_DIRTY, &ce.flags)
        || !ctx_id_mapped(unsafe { &mut *ce_to_guc_mut(ce as *mut _) }, ce.guc_id.id as u32))
        && !submission_disabled(ce_to_guc(ce))
}

// upstream: intel_guc_submission.c guc_context_init()
fn guc_context_init(ce: &mut intel_context) {
    let mut prio = I915_CONTEXT_DEFAULT_PRIORITY;
    rcu_read_lock();
    let ctx = rcu_dereference!(ce.gem_context);
    if !ctx.is_null() {
        prio = unsafe { (*ctx).sched.priority };
    }
    rcu_read_unlock();
    ce.guc_state.prio = map_i915_prio_to_guc_prio(prio);
    INIT_DELAYED_WORK(
        &mut ce.guc_state.sched_disable_delay_work,
        __delay_sched_disable,
    );
    set_bit(CONTEXT_GUC_INIT, &mut ce.flags);
}

// upstream: intel_guc_submission.c guc_request_alloc()
fn guc_request_alloc(rq: &mut i915_request) -> i32 {
    let rq_ptr = rq as *mut i915_request;
    let context = rq.context;
    let ce = unsafe { request_to_scheduling_context_mut(&mut *rq_ptr) };
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    let mut flags: c_ulong = 0;
    gem_bug_on!(!intel_context_is_pinned(context));
    // Reserve space before building the request, reducing the chance that we
    // wait after doing work that would then have to be repeated.
    unsafe { (*rq_ptr).reserved_space += GUC_REQUEST_SIZE as u32 };
    // From here the request tracks engine initialization and golden renderstate
    // liveness; cancellation/unwind is no longer safe without considering both.
    let ret = unsafe { ((*(*rq_ptr).engine).emit_flush.unwrap())(rq_ptr, EMIT_INVALIDATE) };
    if ret != 0 {
        return ret;
    }
    unsafe { (*rq_ptr).reserved_space -= GUC_REQUEST_SIZE as u32 };
    if unlikely(!test_bit(CONTEXT_GUC_INIT, &ce.flags)) {
        guc_context_init(&mut *ce);
    }
    // A concurrent context close cancels this delayed work and may issue the
    // disable immediately. Wait for close to finish before trusting scheduling
    // state, otherwise GuC may drop this request. A full CT can take ~1.5 sec.
    if unsafe { cancel_delayed_work_sync(&mut ce.guc_state.sched_disable_delay_work) } {
        intel_context_sched_disable_unpin(&mut *ce);
    } else if intel_context_is_closed(&mut *ce) {
        if wait_for!(context_close_done(&*ce), 1500) {
            guc_warn!(
                &mut *guc,
                "timed out waiting on context sched close before realloc\n"
            );
        }
    }
    // Allocate the guc_id here, not at context pin time: dma_resv may pin and
    // unpin repeatedly, which can trash IDs and race especially under ID theft.
    // Once here, the timeline mutex guarantees persistence until retirement.
    // The refcount changes once per in-flight request; pin_guc_id locks the
    // zero-to-one transition against unpin_guc_id. Exhaustion is retryable.
    if !atomic_add_unless(&mut ce.guc_id.r#ref, 1, 0) {
        let ret = pin_guc_id(&mut *guc, &mut *ce); // Returns 1 when a new ID was assigned.
        if unlikely(ret < 0) {
            return ret;
        }
        if context_needs_register(&mut *ce, ret != 0) {
            let ret = try_context_registration(&mut *ce, true);
            if unlikely(ret != 0) {
                if ret == -EPIPE {
                    disable_submission(&mut *guc);
                    // GPU reset will recover this state.
                } else {
                    atomic_dec(&mut ce.guc_id.r#ref);
                    unpin_guc_id(&mut *guc, &mut *ce);
                    return ret;
                }
            } else {
                clear_bit(CONTEXT_LRCA_DIRTY, &mut ce.flags);
            }
        } else {
            clear_bit(CONTEXT_LRCA_DIRTY, &mut ce.flags);
        }
    }

    // A G2H for schedule-disable or deregistration blocks requests on this
    // context; GuC cannot enable/register until the response releases the fence.
    spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
    if context_wait_for_deregister_to_register(&*ce) || context_pending_disable(&*ce) {
        init_irq_work(&mut rq.submit_work, submit_work_cb);
        i915_sw_fence_await(unsafe { &mut (*rq_ptr).submit });
        unsafe { list_add_tail(&mut (*rq_ptr).guc_fence_link, &mut ce.guc_state.fences) };
    }
    spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    0
}

unsafe extern "C" fn guc_request_alloc_callback(rq: *mut i915_request) -> i32 {
    // SAFETY: IntelEngineCs.request_alloc supplies a live request.
    guc_request_alloc(unsafe { &mut *rq })
}

// upstream: intel_guc_submission.c guc_virtual_context_pre_pin()
fn guc_virtual_context_pre_pin(
    ce: &mut intel_context,
    ww: &mut i915_gem_ww_ctx,
    vaddr: &mut *mut c_void,
) -> i32 {
    let engine = guc_virtual_get_sibling(unsafe { &*ce.engine }, 0).unwrap();
    __guc_context_pre_pin(ce, engine, ww, vaddr)
}

// upstream: intel_guc_submission.c guc_virtual_context_pin()
fn guc_virtual_context_pin(ce: &mut intel_context, vaddr: *mut c_void) -> i32 {
    let engine = guc_virtual_get_sibling(unsafe { &*ce.engine }, 0).unwrap();
    let gt = unsafe { (*ce.engine).gt };
    let mask = unsafe { (*ce.engine).mask };
    let ret = __guc_context_pin(&mut *ce, engine, vaddr);
    if likely(ret == 0) {
        for_each_engine_masked!(sibling, tmp, gt, mask, {
            unsafe { intel_engine_pm_get(sibling) };
        });
    }
    ret
}

// upstream: intel_guc_submission.c guc_virtual_context_unpin()
fn guc_virtual_context_unpin(ce: &mut intel_context) {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    gem_bug_on!(context_enabled(&*ce));
    gem_bug_on!(intel_context_is_barrier(&mut *ce));
    unpin_guc_id(&mut *guc, &mut *ce);
    unsafe { lrc_unpin(ce) };
    let gt = unsafe { (*ce.engine).gt };
    let mask = unsafe { (*ce.engine).mask };
    for_each_engine_masked!(sibling, tmp, gt, mask, {
        unsafe { intel_engine_pm_put_async(sibling) };
    });
}

// upstream: intel_guc_submission.c guc_virtual_context_enter()
fn guc_virtual_context_enter(ce: &mut intel_context) {
    let gt = unsafe { (*ce.engine).gt };
    let mask = unsafe { (*ce.engine).mask };
    for_each_engine_masked!(sibling, tmp, gt, mask, {
        unsafe { intel_engine_pm_get(sibling) };
    });
    unsafe { intel_timeline_enter(ce.timeline) };
}

// upstream: intel_guc_submission.c guc_virtual_context_exit()
fn guc_virtual_context_exit(ce: &mut intel_context) {
    let gt = unsafe { (*ce.engine).gt };
    let mask = unsafe { (*ce.engine).mask };
    for_each_engine_masked!(sibling, tmp, gt, mask, {
        unsafe { intel_engine_pm_put(sibling) };
    });
    unsafe { intel_timeline_exit(ce.timeline) };
}

// upstream: intel_guc_submission.c guc_virtual_context_alloc()
fn guc_virtual_context_alloc(ce: &mut intel_context) -> i32 {
    let _engine = guc_virtual_get_sibling(unsafe { &*ce.engine }, 0).unwrap();
    unsafe { lrc_alloc(ce, ce.engine) }
}

// upstream: intel_guc_submission.c guc_parent_context_pin()
fn guc_parent_context_pin(ce: &mut intel_context, vaddr: *mut c_void) -> i32 {
    let engine = guc_virtual_get_sibling(unsafe { &*ce.engine }, 0).unwrap();
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    gem_bug_on!(!unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    gem_bug_on!(!unsafe { intel_engine_is_virtual(ce.engine) });
    let ret = pin_guc_id(&mut *guc, &mut *ce);
    if unlikely(ret < 0) {
        return ret;
    }
    __guc_context_pin(&mut *ce, engine, vaddr)
}

// upstream: intel_guc_submission.c guc_child_context_pin()
fn guc_child_context_pin(ce: &mut intel_context, vaddr: *mut c_void) -> i32 {
    let engine = guc_virtual_get_sibling(unsafe { &*ce.engine }, 0).unwrap();
    gem_bug_on!(!intel_context_is_child(&mut *ce));
    gem_bug_on!(!unsafe { intel_engine_is_virtual(ce.engine) });
    __intel_context_pin(ce.parallel.parent);
    __guc_context_pin(&mut *ce, engine, vaddr)
}

// upstream: intel_guc_submission.c guc_parent_context_unpin()
fn guc_parent_context_unpin(ce: &mut intel_context) {
    let guc = unsafe { &mut *ce_to_guc_mut(ce) };
    gem_bug_on!(context_enabled(&mut *ce));
    gem_bug_on!(intel_context_is_barrier(&mut *ce));
    gem_bug_on!(!unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    gem_bug_on!(!unsafe { intel_engine_is_virtual(ce.engine) });
    unpin_guc_id(unsafe { &mut *guc }, &mut *ce);
    unsafe { lrc_unpin(&mut *ce) };
}

// upstream: intel_guc_submission.c guc_child_context_unpin()
fn guc_child_context_unpin(ce: &mut intel_context) {
    gem_bug_on!(context_enabled(&mut *ce));
    gem_bug_on!(intel_context_is_barrier(&mut *ce));
    gem_bug_on!(!intel_context_is_child(&mut *ce));
    gem_bug_on!(!unsafe { intel_engine_is_virtual(ce.engine) });
    unsafe { lrc_unpin(&mut *ce) };
}

// upstream: intel_guc_submission.c guc_child_context_post_unpin()
fn guc_child_context_post_unpin(ce: &mut intel_context) {
    gem_bug_on!(!intel_context_is_child(&mut *ce));
    gem_bug_on!(!intel_context_is_pinned(ce.parallel.parent));
    gem_bug_on!(!unsafe { intel_engine_is_virtual(ce.engine) });
    unsafe { lrc_post_unpin(&mut *ce) };
    intel_context_unpin(ce.parallel.parent);
}

// upstream: intel_guc_submission.c guc_child_context_destroy()
fn guc_child_context_destroy(kref: &mut kref) {
    let ce = container_of!(kref, intel_context, r#ref);
    __guc_context_destroy(unsafe { &mut *ce });
}

// upstream: intel_guc_submission.c guc_create_parallel()
fn guc_create_parallel(
    engines: &[*mut intel_engine_cs],
    num_siblings: u32,
    width: u32,
) -> Result<*mut intel_context, i32> {
    let mut siblings = kmalloc_objs::<*mut intel_engine_cs, u32>(num_siblings);
    if siblings.is_null() {
        return Err(-ENOMEM);
    }
    let mut parent: *mut intel_context = core::ptr::null_mut();
    let mut result: *mut intel_context = core::ptr::null_mut();
    for i in 0..width {
        for j in 0..num_siblings {
            unsafe { *siblings.add(j as usize) = engines[(i * num_siblings + j) as usize] };
        }
        let ce =
            unsafe { intel_engine_create_virtual(siblings, num_siblings, FORCE_VIRTUAL as u64) };
        if IS_ERR(ce) {
            result = ce;
            if !parent.is_null() {
                intel_context_put(parent);
            }
            unsafe { kfree(siblings) };
            return Err(PTR_ERR(result));
        }
        if i == 0 {
            parent = ce;
            unsafe { (*parent).ops = &virtual_parent_context_ops };
        } else {
            unsafe {
                (*ce).ops = &virtual_child_context_ops;
                intel_context_bind_parent_child(parent, ce);
            }
        }
    }
    unsafe {
        (*parent).parallel.fence_context = dma_fence_context_alloc(1);
        (*(*parent).engine).emit_bb_start =
            Some(emit_bb_start_parent_no_preempt_mid_batch_callback);
        (*(*parent).engine).emit_fini_breadcrumb =
            Some(emit_fini_breadcrumb_parent_no_preempt_mid_batch_callback);
        (*(*parent).engine).emit_fini_breadcrumb_dw =
            12 + 4 * (*parent).parallel.number_children as u32;
        for_each_child!(parent, ce, {
            unsafe {
                (*(*ce).engine).emit_bb_start =
                    Some(emit_bb_start_child_no_preempt_mid_batch_callback)
            };
            unsafe {
                (*(*ce).engine).emit_fini_breadcrumb =
                    Some(emit_fini_breadcrumb_child_no_preempt_mid_batch_callback)
            };
            unsafe { (*(*ce).engine).emit_fini_breadcrumb_dw = 16 };
        });
    }
    unsafe { kfree(siblings) };
    Ok(parent)
}

// upstream: intel_guc_submission.c guc_irq_enable_breadcrumbs()
unsafe fn guc_irq_enable_breadcrumbs(b: *mut intel_breadcrumbs) -> bool {
    let mut result = false;
    let gt = unsafe { (*(*b).irq_engine).gt };
    let engine_mask = unsafe { (*b).engine_mask };
    for_each_engine_masked!(sibling, tmp, gt, engine_mask, {
        result |= intel_engine_irq_enable(sibling);
    });
    result
}

// upstream: intel_guc_submission.c guc_irq_disable_breadcrumbs()
unsafe fn guc_irq_disable_breadcrumbs(b: *mut intel_breadcrumbs) {
    let gt = unsafe { (*(*b).irq_engine).gt };
    let engine_mask = unsafe { (*b).engine_mask };
    for_each_engine_masked!(sibling, tmp, gt, engine_mask, {
        intel_engine_irq_disable(sibling);
    });
}

unsafe extern "C" fn guc_irq_enable_breadcrumbs_callback(b: *mut intel_breadcrumbs) -> bool {
    // SAFETY: breadcrumb IRQ callbacks are invoked with their live object.
    unsafe { guc_irq_enable_breadcrumbs(b) }
}

unsafe extern "C" fn guc_irq_disable_breadcrumbs_callback(b: *mut intel_breadcrumbs) {
    // SAFETY: breadcrumb IRQ callbacks are invoked with their live object.
    unsafe { guc_irq_disable_breadcrumbs(b) };
}

// upstream: intel_guc_submission.c guc_init_breadcrumbs()
fn guc_init_breadcrumbs(engine: &mut intel_engine_cs) {
    // GuC decides the physical engine for each request, but breadcrumbs are
    // per physical engine. Route all class interrupts to the first instance and
    // enable/disable them in unison across the class.
    for i in 0..MAX_ENGINE_INSTANCE {
        let sibling = unsafe { (*engine.gt).engine_class[engine.class as usize][i] };
        if !sibling.is_null() {
            if engine.breadcrumbs != unsafe { (*sibling).breadcrumbs } {
                unsafe { intel_breadcrumbs_put(engine.breadcrumbs) };
                engine.breadcrumbs = unsafe { intel_breadcrumbs_get((*sibling).breadcrumbs) };
            }
            break;
        }
    }
    if !engine.breadcrumbs.is_null() {
        unsafe {
            (*engine.breadcrumbs).engine_mask |= engine.mask;
            (*engine.breadcrumbs).irq_enable = Some(guc_irq_enable_breadcrumbs_callback);
            (*engine.breadcrumbs).irq_disable = Some(guc_irq_disable_breadcrumbs_callback);
        }
    }
}

// upstream: intel_guc_submission.c guc_bump_inflight_request_prio()
fn guc_bump_inflight_request_prio(rq: &mut i915_request, prio: i32) {
    let new_guc_prio = map_i915_prio_to_guc_prio(prio);
    // Short-circuit requests below normal priority.
    if prio < I915_PRIORITY_NORMAL {
        return;
    }
    let ce = unsafe { intel_context_to_parent(rq.context) };
    spin_lock(unsafe { &mut (*ce).guc_state.lock });
    if rq.guc_prio == GUC_PRIO_FINI {
        spin_unlock(unsafe { &mut (*ce).guc_state.lock });
        return;
    }
    if !new_guc_prio_higher(rq.guc_prio, new_guc_prio) {
        spin_unlock(unsafe { &mut (*ce).guc_state.lock });
        return;
    }
    if rq.guc_prio != GUC_PRIO_INIT {
        sub_context_inflight_prio(unsafe { &mut *ce }, rq.guc_prio);
    }
    rq.guc_prio = new_guc_prio;
    add_context_inflight_prio(unsafe { &mut *ce }, rq.guc_prio);
    update_context_prio(unsafe { &mut *ce });
    spin_unlock(unsafe { &mut (*ce).guc_state.lock });
}

// upstream: intel_guc_submission.c guc_retire_inflight_request_prio()
fn guc_retire_inflight_request_prio(rq: &mut i915_request) {
    let ce = unsafe { intel_context_to_parent(rq.context) };
    spin_lock(unsafe { &mut (*ce).guc_state.lock });
    guc_prio_fini(rq, unsafe { &mut *ce });
    spin_unlock(unsafe { &mut (*ce).guc_state.lock });
}

// upstream: intel_guc_submission.c sanitize_hwsp()
fn sanitize_hwsp(engine: &mut intel_engine_cs) {
    let mut tl: *mut IntelTimeline = core::ptr::null_mut();
    list_for_each_entry!(tl, &engine.status_page.timelines, engine_link, {
        unsafe { intel_timeline_reset_seqno(tl) };
    });
}

// upstream: intel_guc_submission.c guc_sanitize()
fn guc_sanitize(engine: &mut intel_engine_cs) {
    // Poison retained state on resume: pinned buffers may have been replaced
    // by garbage across suspend/resume even when that loss is not observable.
    if IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
        unsafe { memset(engine.status_page.addr, POISON_INUSE as i32, PAGE_SIZE) };
    }
    // The kernel context HWSP lives in this status page and must be reset.
    sanitize_hwsp(engine);
    // Scrub dirty HWSP cache lines.
    unsafe { drm_clflush_virt_range(engine.status_page.addr.cast(), PAGE_SIZE as u64) };
    unsafe { intel_engine_reset_pinned_contexts(engine) };
}

unsafe extern "C" fn guc_sanitize_callback(engine: *mut intel_engine_cs) {
    // SAFETY: IntelEngineCs.sanitize supplies a live engine.
    guc_sanitize(unsafe { &mut *engine });
}

// upstream: intel_guc_submission.c setup_hwsp()
fn setup_hwsp(engine: &mut intel_engine_cs) {
    unsafe { intel_engine_set_hwsp_writemask(engine, !0u32) }; // HWSTAM
    ENGINE_WRITE_FW!(engine, RING_HWS_PGA, unsafe {
        i915_ggtt_offset(engine.status_page.vma)
    });
}

// upstream: intel_guc_submission.c start_engine()
fn start_engine(engine: &mut intel_engine_cs) {
    ENGINE_WRITE_FW!(
        engine,
        RING_MODE_GEN7,
        REG_MASKED_FIELD_ENABLE!(GEN11_GFX_DISABLE_LEGACY_MODE)
    );
    ENGINE_WRITE_FW!(engine, RING_MI_MODE, REG_MASKED_FIELD_DISABLE!(STOP_RING));
    ENGINE_POSTING_READ!(engine, RING_MI_MODE);
}

// upstream: intel_guc_submission.c guc_resume()
fn guc_resume(engine: &mut intel_engine_cs) -> i32 {
    unsafe { assert_forcewakes_active(engine.uncore, FORCEWAKE_ALL) };
    unsafe { intel_mocs_init_engine(engine) };
    unsafe { intel_breadcrumbs_reset(engine.breadcrumbs) };
    setup_hwsp(engine);
    start_engine(engine);
    if engine.flags & I915_ENGINE_FIRST_RENDER_COMPUTE != 0 {
        unsafe { xehp_enable_ccs_engines(engine) };
    }
    0
}

unsafe extern "C" fn guc_resume_callback(engine: *mut intel_engine_cs) -> i32 {
    // SAFETY: IntelEngineCs.resume supplies a live engine.
    guc_resume(unsafe { &mut *engine })
}

// upstream: intel_guc_submission.c guc_sched_engine_disabled()
fn guc_sched_engine_disabled(sched_engine: &i915_sched_engine) -> bool {
    unsafe { sched_engine.tasklet.callbacks.callback.is_none() }
}

// upstream: intel_guc_submission.c guc_set_default_submission()
fn guc_set_default_submission(engine: &mut intel_engine_cs) {
    engine.submit_request = Some(guc_submit_request_callback);
}

unsafe extern "C" fn guc_set_default_submission_callback(engine: *mut intel_engine_cs) {
    // SAFETY: IntelEngineCs.set_default_submission supplies a live engine.
    guc_set_default_submission(unsafe { &mut *engine });
}

// upstream: intel_guc_submission.c guc_kernel_context_pin()
#[inline]
fn guc_kernel_context_pin(guc: &mut intel_guc, ce: &mut intel_context) -> i32 {
    // Registration fails only when reset is beginning. This is called at reset
    // completion, so a second reset is not expected; if it occurs, this runs again.
    if context_guc_id_invalid(ce) {
        let ret = pin_guc_id(guc, ce);
        if ret < 0 {
            return ret;
        }
    }
    if !test_bit(CONTEXT_GUC_INIT, &ce.flags) {
        guc_context_init(ce);
    }
    let ret = try_context_registration(ce, true);
    if ret != 0 {
        unpin_guc_id(guc, ce);
    }
    ret
}

// upstream: intel_guc_submission.c guc_init_submission()
#[inline]
fn guc_init_submission(guc: &mut intel_guc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    xa_destroy(&mut guc.context_lookup); // Ensure all descriptors are clean.
    // A reset may have happened with a stalled request; discard that state.
    guc.stalled_request = core::ptr::null_mut();
    guc.submission_stall_reason = STALL_NONE;
    // Register contexts pinned before GuC submission was enabled. After reset,
    // also rebuild the data shared with GuC; kernel LRCs need separate handling.
    for_each_engine!(engine, id, gt, {
        let mut ce: *mut intel_context = core::ptr::null_mut();
        list_for_each_entry!(ce, &engine.pinned_contexts_list, pinned_contexts_link, {
            let ret = guc_kernel_context_pin(unsafe { &mut *guc }, unsafe { &mut *ce });
            if ret != 0 {
                // No cleanup is useful: i915 will wedge on this failure.
                return ret;
            }
        });
    });
    0
}

// upstream: intel_guc_submission.c guc_release()
fn guc_release(engine: &mut intel_engine_cs) {
    engine.sanitize = None; // No longer in control, so nothing to sanitize.
    unsafe { intel_engine_cleanup_common(engine) };
    unsafe { lrc_fini_wa_ctx(engine) };
}

unsafe extern "C" fn guc_release_callback(engine: *mut intel_engine_cs) {
    // SAFETY: IntelEngineCs.release supplies a live engine.
    guc_release(unsafe { &mut *engine });
}

// upstream: intel_guc_submission.c virtual_guc_bump_serial()
fn virtual_guc_bump_serial(engine: &mut intel_engine_cs) {
    for_each_engine_masked!(sibling, tmp, engine.gt, engine.mask, {
        unsafe { (*sibling).serial += 1 };
    });
}

unsafe extern "C" fn virtual_guc_bump_serial_callback(engine: *mut intel_engine_cs) {
    // SAFETY: IntelEngineCs.bump_serial supplies a live virtual engine.
    virtual_guc_bump_serial(unsafe { &mut *engine });
}

// upstream: intel_guc_submission.c guc_default_vfuncs()
fn guc_default_vfuncs(engine: &mut intel_engine_cs) {
    // Default virtual functions, overridable by individual engines.
    engine.resume = Some(guc_resume_callback);
    engine.cops = &guc_context_ops;
    engine.request_alloc = Some(guc_request_alloc_callback);
    engine.add_active_request = Some(add_to_context_callback);
    engine.remove_active_request = Some(remove_from_context_callback);
    unsafe { (*engine.sched_engine).schedule = Some(i915_schedule_callback) };
    engine.reset.prepare = Some(guc_engine_reset_prepare_callback);
    engine.reset.rewind = Some(guc_rewind_nop_callback);
    engine.reset.cancel = Some(guc_reset_nop_callback);
    engine.reset.finish = Some(guc_reset_nop_callback);
    engine.emit_flush = Some(gen8_emit_flush_xcs);
    engine.emit_init_breadcrumb = Some(gen8_emit_init_breadcrumb);
    engine.emit_fini_breadcrumb = Some(gen8_emit_fini_breadcrumb_xcs);
    if unsafe { GRAPHICS_VER(engine.i915) } >= 12 {
        engine.emit_fini_breadcrumb = Some(gen12_emit_fini_breadcrumb_xcs);
        engine.emit_flush = Some(gen12_emit_flush_xcs);
    }
    engine.set_default_submission = Some(guc_set_default_submission_callback);
    engine.busyness = Some(guc_engine_busyness_callback);
    engine.flags |= I915_ENGINE_SUPPORTS_STATS;
    engine.flags |= I915_ENGINE_HAS_PREEMPTION;
    engine.flags |= I915_ENGINE_HAS_TIMESLICES;
    // Wa_14014475959:dg2
    if engine.class as i32 == COMPUTE_CLASS
        && (unsafe { IS_GFX_GT_IP_STEP(engine.gt, IP_VER(12, 70), STEP_A0, STEP_B0) }
            || unsafe { IS_DG2(engine.i915) })
    {
        engine.flags |= I915_ENGINE_USES_WA_HOLD_SWITCHOUT;
    }
    // Wa_16019325821 and Wa_14019159160.
    if (engine.class as i32 == COMPUTE_CLASS || engine.class as i32 == RENDER_CLASS)
        && unsafe { IS_GFX_GT_IP_RANGE(engine.gt, IP_VER(12, 70), IP_VER(12, 74)) }
    {
        engine.flags |= I915_ENGINE_USES_WA_HOLD_SWITCHOUT;
    }
    // GuC supports timeslicing and semaphores too, but firmware handles them;
    // enabling semaphores needs minor changes and remains disabled upstream.
    engine.emit_bb_start = Some(gen8_emit_bb_start);
    if unsafe { GRAPHICS_VER_FULL(engine.i915) } >= IP_VER(12, 55) {
        engine.emit_bb_start = Some(xehp_emit_bb_start);
    }
}

// upstream: intel_guc_submission.c rcs_submission_override()
fn rcs_submission_override(engine: &mut intel_engine_cs) {
    match unsafe { GRAPHICS_VER(engine.i915) } {
        12 => {
            engine.emit_flush = Some(gen12_emit_flush_rcs);
            engine.emit_fini_breadcrumb = Some(gen12_emit_fini_breadcrumb_rcs);
        }
        11 => {
            engine.emit_flush = Some(gen11_emit_flush_rcs);
            engine.emit_fini_breadcrumb = Some(gen11_emit_fini_breadcrumb_rcs);
        }
        _ => {
            engine.emit_flush = Some(gen8_emit_flush_rcs);
            engine.emit_fini_breadcrumb = Some(gen8_emit_fini_breadcrumb_rcs);
        }
    }
}

// upstream: intel_guc_submission.c guc_default_irqs()
#[inline]
fn guc_default_irqs(engine: &mut intel_engine_cs) {
    engine.irq_keep_mask = GT_RENDER_USER_INTERRUPT;
    unsafe { intel_engine_set_irq_handler(engine as *mut _, cs_irq_handler_callback) };
}

// upstream: intel_guc_submission.c guc_sched_engine_destroy()
fn guc_sched_engine_destroy(kref: &mut kref) {
    let sched_engine = container_of!(kref, i915_sched_engine, r#ref);
    let sched_engine = unsafe { &mut *sched_engine };
    let guc = unsafe { &mut *sched_engine.private_data.cast::<IntelGuc>() };
    guc.sched_engine = core::ptr::null_mut();
    unsafe {
        tasklet_kill(core::ptr::addr_of_mut!(sched_engine.tasklet)); // Flush the callback.
        kfree(sched_engine);
    }
}

unsafe extern "C" fn guc_sched_engine_destroy_callback(kref: *mut kref) {
    // SAFETY: scheduler destroy callback supplies its live kref.
    guc_sched_engine_destroy(unsafe { &mut *kref });
}

unsafe extern "C" fn guc_sched_engine_disabled_callback(engine: *mut i915_sched_engine) -> bool {
    // SAFETY: scheduler disabled callback supplies its live engine.
    guc_sched_engine_disabled(unsafe { &*engine })
}

unsafe extern "C" fn guc_bump_inflight_request_prio_callback(rq: *mut i915_request, prio: i32) {
    // SAFETY: scheduler priority callback supplies its live request.
    guc_bump_inflight_request_prio(unsafe { &mut *rq }, prio);
}

unsafe extern "C" fn guc_retire_inflight_request_prio_callback(rq: *mut i915_request) {
    // SAFETY: scheduler priority callback supplies its live request.
    guc_retire_inflight_request_prio(unsafe { &mut *rq });
}

unsafe extern "C" fn i915_schedule_callback(rq: *mut i915_request, attr: *const I915SchedAttr) {
    // SAFETY: scheduler callback provides live request and attributes.
    unsafe { i915_schedule(rq, attr) };
}

unsafe fn guc_submission_tasklet_rust(tasklet: *mut tasklet_struct) {
    // SAFETY: tasklet setup stores this callback with its live tasklet object.
    guc_submission_tasklet(unsafe { &mut *tasklet });
}

// upstream: intel_guc_submission.c intel_guc_submission_setup()
fn intel_guc_submission_setup(engine: &mut intel_engine_cs) -> i32 {
    let i915 = engine.i915;
    let guc = unsafe { gt_to_guc(engine.gt) };
    // These setup assumptions (including always-enabled IRQs) hold only gen11+.
    gem_bug_on!(unsafe { GRAPHICS_VER(i915) } < 11);
    if unsafe { (*guc).sched_engine }.is_null() {
        unsafe { (*guc).sched_engine = i915_sched_engine_create(ENGINE_VIRTUAL) };
        if unsafe { (*guc).sched_engine }.is_null() {
            return -ENOMEM;
        }
        let sched_engine = unsafe { (*guc).sched_engine };
        unsafe { (*sched_engine).schedule = Some(i915_schedule_callback) };
        unsafe { (*sched_engine).disabled = Some(guc_sched_engine_disabled_callback) };
        unsafe { (*sched_engine).private_data = guc.cast() };
        unsafe { (*sched_engine).destroy = Some(guc_sched_engine_destroy_callback) };
        unsafe {
            (*sched_engine).bump_inflight_request_prio =
                Some(guc_bump_inflight_request_prio_callback)
        };
        unsafe {
            (*sched_engine).retire_inflight_request_prio =
                Some(guc_retire_inflight_request_prio_callback)
        };
        unsafe {
            tasklet_setup(
                core::ptr::addr_of_mut!((*(*guc).sched_engine).tasklet),
                guc_submission_tasklet_callback,
            )
        };
    }
    unsafe { i915_sched_engine_put(engine.sched_engine) };
    engine.sched_engine = unsafe { i915_sched_engine_get((*guc).sched_engine) };
    guc_default_vfuncs(engine);
    guc_default_irqs(engine);
    guc_init_breadcrumbs(engine);
    if engine.flags & I915_ENGINE_HAS_RCS_REG_STATE != 0 {
        rcs_submission_override(engine);
    }
    unsafe { lrc_init_wa_ctx(engine) };
    // Take ownership and responsibility for cleanup last.
    engine.sanitize = Some(guc_sanitize_callback);
    engine.release = Some(guc_release_callback);
    0
}

// upstream: intel_guc_submission.c __guc_scheduling_policy_action_size()
fn __guc_scheduling_policy_action_size(policy: &scheduling_policy) -> u32 {
    (size_of::<guc_update_scheduling_policy_header>() / size_of::<u32>()
        + policy.num_words as usize) as u32
}

// upstream: intel_guc_submission.c __guc_scheduling_policy_start_klv()
fn __guc_scheduling_policy_start_klv(policy: &mut scheduling_policy) -> &mut scheduling_policy {
    policy.h2g.action = INTEL_GUC_ACTION_UPDATE_SCHEDULING_POLICIES_KLV;
    policy.max_words = policy.h2g.data.len() as u32;
    policy.num_words = 0;
    policy.count = 0;
    policy
}

// upstream: intel_guc_submission.c __guc_scheduling_policy_add_klv()
fn __guc_scheduling_policy_add_klv(
    policy: &mut scheduling_policy,
    action: u32,
    data: &[u32],
    len: u32,
) {
    let mut klv_ptr = unsafe {
        core::ptr::addr_of_mut!(policy.h2g.data)
            .cast::<u32>()
            .add(policy.num_words as usize)
    };
    gem_bug_on!(policy.num_words + 1 + len > policy.max_words);
    unsafe {
        *klv_ptr = field_prep(GUC_KLV_0_KEY, action) | field_prep(GUC_KLV_0_LEN, len);
        klv_ptr = klv_ptr.add(1);
        memcpy(klv_ptr, data.as_ptr(), size_of::<u32>() * len as usize);
    }
    policy.num_words += 1 + len;
    policy.count += 1;
}

// upstream: intel_guc_submission.c __guc_action_set_scheduling_policies()
fn __guc_action_set_scheduling_policies(guc: &mut intel_guc, policy: &scheduling_policy) -> i32 {
    let ret = intel_guc_send(
        guc,
        unsafe {
            core::slice::from_raw_parts(
                core::ptr::addr_of!(policy.h2g).cast::<u32>(),
                size_of::<GuCUpdateSchedulingPolicy>() / size_of::<u32>(),
            )
        },
        __guc_scheduling_policy_action_size(policy),
    );
    if ret < 0 {
        guc_probe_error!(
            guc,
            "Failed to configure global scheduling policies: %pe!\n",
            ERR_PTR::<c_void>(ret)
        );
        return ret;
    }
    if ret as u32 != policy.count {
        guc_warn!(
            guc,
            "global scheduler policy processed %d of %d KLVs!",
            ret,
            policy.count
        );
        if ret as u32 > policy.count {
            return -EPROTO;
        }
    }
    0
}

// upstream: intel_guc_submission.c guc_init_global_schedule_policy()
fn guc_init_global_schedule_policy(guc: &mut intel_guc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    if GUC_SUBMIT_VER(guc) < MAKE_GUC_VER(1, 1, 0) {
        return 0;
    }
    let mut policy = scheduling_policy::default();
    __guc_scheduling_policy_start_klv(&mut policy);
    let mut ret = 0;
    with_intel_runtime_pm!(unsafe { &mut (*(*gt).i915).runtime_pm }, wakeref, {
        let yield_data = [
            GLOBAL_SCHEDULE_POLICY_RC_YIELD_DURATION,
            GLOBAL_SCHEDULE_POLICY_RC_YIELD_RATIO,
        ];
        __guc_scheduling_policy_add_klv(
            &mut policy,
            GUC_SCHEDULING_POLICIES_KLV_ID_RENDER_COMPUTE_YIELD,
            &yield_data,
            yield_data.len() as u32,
        );
        ret = __guc_action_set_scheduling_policies(guc, &policy);
    });
    ret
}

// upstream: intel_guc_submission.c guc_route_semaphores()
fn guc_route_semaphores(guc: &mut intel_guc, to_guc: bool) {
    let gt = unsafe { guc_to_gt(guc) };
    if unsafe { GRAPHICS_VER((*gt).i915) } < 12 {
        return;
    }
    let val = if to_guc {
        GUC_SEM_INTR_ROUTE_TO_GUC | GUC_SEM_INTR_ENABLE_ALL
    } else {
        0
    };
    unsafe { intel_uncore_write((*gt).uncore, GEN12_GUC_SEM_INTR_ENABLES, val) };
}

// upstream: intel_guc_submission.c intel_guc_submission_enable()
fn intel_guc_submission_enable(guc: &mut intel_guc) -> i32 {
    // Enable semaphore interrupts and route them to GuC.
    guc_route_semaphores(guc, true);
    let ret = guc_init_submission(guc);
    if ret != 0 {
        guc_route_semaphores(guc, false);
        return ret;
    }
    let ret = guc_init_engine_stats(guc);
    if ret != 0 {
        guc_route_semaphores(guc, false);
        return ret;
    }
    let ret = guc_init_global_schedule_policy(guc);
    if ret != 0 {
        guc_fini_engine_stats(guc);
        guc_route_semaphores(guc, false);
        return ret;
    }
    0
}

// upstream: intel_guc_submission.c intel_guc_submission_disable()
fn intel_guc_submission_disable(guc: &mut intel_guc) {
    // GuC may already have reset by the time this runs.
    guc_cancel_busyness_worker(guc);
    // Disable semaphore interrupts and route them to host.
    guc_route_semaphores(guc, false);
}

// upstream: intel_guc_submission.c __guc_submission_supported()
fn __guc_submission_supported(guc: &intel_guc) -> bool {
    // GuC submission is unavailable before Gen11.
    let supported = unsafe { intel_guc_is_supported(guc as *const intel_guc) };
    supported && unsafe { GRAPHICS_VER(guc_to_i915_const(guc)) } >= 11
}

// upstream: intel_guc_submission.c __guc_submission_selected()
fn __guc_submission_selected(guc: &intel_guc) -> bool {
    let i915 = guc_to_i915_const(guc);
    if !guc.submission_supported {
        return false;
    }
    (unsafe { (*i915).params.enable_guc }) & ENABLE_GUC_SUBMISSION as i32 != 0
}

// upstream: intel_guc_submission.c intel_guc_sched_disable_gucid_threshold_max()
fn intel_guc_sched_disable_gucid_threshold_max(guc: &intel_guc) -> i32 {
    (guc.submission_state.num_guc_ids - NUMBER_MULTI_LRC_GUC_ID(guc)) as i32
}

const SCHED_DISABLE_DELAY_MS: u64 = 34;

// upstream: intel_guc_submission.c intel_guc_submission_init_early()
fn intel_guc_submission_init_early(guc: &mut intel_guc) {
    xa_init_flags(&mut guc.context_lookup, XA_FLAGS_LOCK_IRQ);
    spin_lock_init(&mut guc.submission_state.lock);
    unsafe { INIT_LIST_HEAD(&mut guc.submission_state.guc_id_list) };
    ida_init(&mut guc.submission_state.guc_ids);
    unsafe { INIT_LIST_HEAD(&mut guc.submission_state.destroyed_contexts) };
    INIT_WORK(
        &mut guc.submission_state.destroyed_worker,
        destroyed_worker_func,
    );
    INIT_WORK(
        &mut guc.submission_state.reset_fail_worker,
        reset_fail_worker_func,
    );
    spin_lock_init(&mut guc.timestamp.lock);
    INIT_DELAYED_WORK(&mut guc.timestamp.work, guc_timestamp_ping);
    guc.submission_state.sched_disable_delay_ms = SCHED_DISABLE_DELAY_MS as u32;
    guc.submission_state.num_guc_ids = GUC_MAX_CONTEXT_ID as i32;
    guc.submission_state.sched_disable_gucid_threshold =
        NUM_SCHED_DISABLE_GUCIDS_DEFAULT_THRESHOLD(guc) as u32;
    guc.submission_supported = __guc_submission_supported(guc);
    guc.submission_selected = __guc_submission_selected(guc);
}

// upstream: intel_guc_submission.c g2h_context_lookup()
#[inline]
fn g2h_context_lookup(guc: &mut intel_guc, ctx_id: u32) -> *mut intel_context {
    if unlikely(ctx_id >= GUC_MAX_CONTEXT_ID as u32) {
        guc_err!(guc, "Invalid ctx_id %u\n", ctx_id);
        return core::ptr::null_mut();
    }
    let ce = __get_context(guc, ctx_id);
    if unlikely(ce.is_null()) {
        guc_err!(guc, "Context is NULL, ctx_id %u\n", ctx_id);
        return core::ptr::null_mut();
    }
    if unlikely(intel_context_is_child(ce)) {
        guc_err!(guc, "Context is child, ctx_id %u\n", ctx_id);
        return core::ptr::null_mut();
    }
    ce
}

// upstream: intel_guc_submission.c wait_wake_outstanding_tlb_g2h()
fn wait_wake_outstanding_tlb_g2h(guc: &mut intel_guc, seqno: u32) {
    let mut flags: c_ulong = 0;
    xa_lock_irqsave(&mut guc.tlb_lookup, &mut flags);
    let wait = xa_load::<_, intel_guc_tlb_wait>(&mut guc.tlb_lookup, seqno);
    if !wait.is_null() {
        unsafe { wake_up(&mut (*wait).wq) };
    } else {
        guc_dbg!(
            &mut *guc,
            "Stale TLB invalidation response with seqno %d\n",
            seqno
        );
    }
    xa_unlock_irqrestore(&mut guc.tlb_lookup, flags);
}

// upstream: intel_guc_submission.c intel_guc_tlb_invalidation_done()
fn intel_guc_tlb_invalidation_done(guc: &mut intel_guc, payload: &[u32], len: u32) -> i32 {
    if len < 1 {
        return -EPROTO;
    }
    wait_wake_outstanding_tlb_g2h(guc, payload[0]);
    0
}

// upstream: intel_guc_submission.c must_wait_woken()
fn must_wait_woken(wq_entry: &mut wait_queue_entry, mut timeout: i64) -> i64 {
    // Like wait_woken(), but do not return early when a kthread has completed:
    // page reclaim may call this from a stopped kthread and HW must finish wait.
    loop {
        unsafe { set_current_state(TASK_UNINTERRUPTIBLE as i32) };
        if wq_entry.flags & WQ_FLAG_WOKEN != 0 {
            break;
        }
        timeout = schedule_timeout(timeout);
        if timeout == 0 {
            break;
        }
    }
    // Match wait_woken()/woken_wake_function() state and wake-flag handling.
    unsafe { __set_current_state(TASK_RUNNING as i32) };
    WRITE_ONCE!(wq_entry.flags, wq_entry.flags & !WQ_FLAG_WOKEN);
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    timeout
}

// upstream: intel_guc_submission.c intel_gt_is_enabled()
fn intel_gt_is_enabled(gt: &intel_gt) -> bool {
    // Treat a wedged or suspended GT as disabled.
    if unsafe { intel_gt_is_wedged(gt) || !intel_irqs_enabled(gt.i915) } {
        return false;
    }
    true
}

// upstream: intel_guc_submission.c guc_send_invalidate_tlb()
fn guc_send_invalidate_tlb(guc: &mut intel_guc, ty: intel_guc_tlb_invalidation_type) -> i32 {
    let mut local_wait: intel_guc_tlb_wait = unsafe { core::mem::zeroed() };
    let mut wq: *mut intel_guc_tlb_wait = &mut local_wait;
    let gt = unsafe { guc_to_gt(guc) };
    let mut wait = DEFINE_WAIT_FUNC!(woken_wake_function);
    let mut seqno = 0u32;
    let mut action = [
        INTEL_GUC_ACTION_TLB_INVALIDATION,
        0,
        REG_FIELD_PREP(INTEL_GUC_TLB_INVAL_TYPE_MASK, ty as u32)
            | REG_FIELD_PREP(
                INTEL_GUC_TLB_INVAL_MODE_MASK,
                INTEL_GUC_TLB_INVAL_MODE_HEAVY,
            )
            | INTEL_GUC_TLB_INVAL_FLUSH_CACHE,
    ];
    let size = action.len() as u32;

    // Do not issue invalidation while GT is disabled by suspend or wedge.
    if !intel_gt_is_enabled(unsafe { &*gt }) {
        return -EINVAL;
    }
    init_waitqueue_head(&mut local_wait.wq);
    if xa_alloc_cyclic_irq(
        &mut guc.tlb_lookup,
        &mut seqno,
        wq,
        xa_limit_32b,
        &mut guc.next_seqno,
        GFP_ATOMIC | __GFP_NOWARN,
    ) < 0
    {
        // Under severe memory pressure, serialize on the preallocated slot.
        xa_lock_irq(&mut guc.tlb_lookup);
        wq = xa_load::<_, intel_guc_tlb_wait>(&mut guc.tlb_lookup, guc.serial_slot);
        unsafe {
            wait_event_lock_irq!((*wq).wq, !READ_ONCE!((*wq).busy), guc.tlb_lookup.xa_lock);
        }
        // Set busy under the lock so only one caller owns the serial slot;
        // keep it set until the response wakes the next caller.
        unsafe { (*wq).busy = true };
        xa_unlock_irq(&mut guc.tlb_lookup);
        seqno = guc.serial_slot;
    }
    action[1] = seqno;
    unsafe { add_wait_queue(&mut (*wq).wq, &mut wait) };
    // Reclaim is a critical path, so always loop on a busy CT channel.
    let mut err = unsafe {
        intel_guc_send_busy_loop(guc, action.as_ptr(), size, G2H_LEN_DW_INVALIDATE_TLB, true)
    };
    if err != 0 {
        unsafe { remove_wait_queue(&mut (*wq).wq, &mut wait) };
        if seqno != guc.serial_slot {
            xa_erase_irq::<_, intel_guc_tlb_wait>(&mut guc.tlb_lookup, seqno);
        }
        return err;
    }
    // A timeout is an error only if GT remained enabled. Suspend/wedge cancels
    // the invalidation, which is not reported as a timeout.
    if must_wait_woken(&mut wait, intel_guc_ct_max_queue_time_jiffies()) == 0
        && intel_gt_is_enabled(unsafe { &*gt })
    {
        guc_err!(
            &mut *guc,
            "TLB invalidation response timed out for seqno %u\n",
            seqno
        );
        err = -ETIME;
    }
    unsafe { remove_wait_queue(&mut (*wq).wq, &mut wait) };
    if seqno != guc.serial_slot {
        xa_erase_irq::<_, intel_guc_tlb_wait>(&mut guc.tlb_lookup, seqno);
    }
    err
}

// upstream: intel_guc_submission.c intel_guc_invalidate_tlb_engines()
pub(crate) fn intel_guc_invalidate_tlb_engines(guc: &mut intel_guc) -> i32 {
    guc_send_invalidate_tlb(
        guc,
        intel_guc_tlb_invalidation_type::INTEL_GUC_TLB_INVAL_ENGINES,
    )
}

// upstream: intel_guc_submission.c intel_guc_invalidate_tlb_guc()
fn intel_guc_invalidate_tlb_guc(guc: &mut intel_guc) -> i32 {
    guc_send_invalidate_tlb(
        guc,
        intel_guc_tlb_invalidation_type::INTEL_GUC_TLB_INVAL_GUC,
    )
}

// upstream: intel_guc_submission.c intel_guc_deregister_done_process_msg()
fn intel_guc_deregister_done_process_msg(guc: &mut intel_guc, msg: &[u32], len: u32) -> i32 {
    if unlikely(len < 1) {
        guc_err!(guc, "Invalid length %u\n", len);
        return -EPROTO;
    }
    let ctx_id = msg[0];
    let ce_ptr = g2h_context_lookup(guc, ctx_id);
    if unlikely(ce_ptr.is_null()) {
        return -EPROTO;
    }
    let ce = unsafe { &mut *ce_ptr };
    trace_intel_context_deregister_done(ce);
    #[cfg(CONFIG_DRM_I915_SELFTEST)]
    if unlikely(ce.drop_deregister) {
        ce.drop_deregister = false;
        return 0;
    }
    if context_wait_for_deregister_to_register(ce) {
        let runtime_pm = unsafe { &(*(*(*ce.engine).gt).i915).runtime_pm };
        with_intel_runtime_pm!(runtime_pm, wakeref, {
            // The previous owner of this ID is gone; register this context now.
            register_context(ce, true);
        });
        guc_signal_context_fence(ce);
        unsafe { intel_context_put(&mut *ce) };
    } else if context_destroyed(ce) {
        // The context has already been destroyed.
        unsafe { intel_gt_pm_put_async_untracked(guc_to_gt(guc)) };
        release_guc_id(guc, ce);
        __guc_context_destroy(ce);
    }
    decr_outstanding_submission_g2h(guc);
    0
}

// upstream: intel_guc_submission.c intel_guc_sched_done_process_msg()
fn intel_guc_sched_done_process_msg(guc: &mut intel_guc, msg: &[u32], len: u32) -> i32 {
    if unlikely(len < 2) {
        guc_err!(guc, "Invalid length %u\n", len);
        return -EPROTO;
    }
    let ctx_id = msg[0];
    let ce_ptr = g2h_context_lookup(guc, ctx_id);
    if unlikely(ce_ptr.is_null()) {
        return -EPROTO;
    }
    let ce = unsafe { &mut *ce_ptr };
    if unlikely(
        context_destroyed(ce) || (!context_pending_enable(ce) && !context_pending_disable(ce)),
    ) {
        guc_err!(
            &mut *guc,
            "Bad context sched_state 0x%x, ctx_id %u\n",
            ce.guc_state.sched_state,
            ctx_id
        );
        return -EPROTO;
    }
    trace_intel_context_sched_done(ce);
    if context_pending_enable(ce) {
        #[cfg(CONFIG_DRM_I915_SELFTEST)]
        if unlikely(ce.drop_schedule_enable) {
            ce.drop_schedule_enable = false;
            return 0;
        }
        let mut flags: c_ulong = 0;
        spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
        clr_context_pending_enable(ce);
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
    } else if context_pending_disable(ce) {
        #[cfg(CONFIG_DRM_I915_SELFTEST)]
        if unlikely(ce.drop_schedule_disable) {
            ce.drop_schedule_disable = false;
            return 0;
        }
        // Unpin before signaling fences: requests can submit and retire before
        // the unpin finishes, which could otherwise drop pin_count to zero while
        // the context remains enabled.
        intel_context_sched_disable_unpin(ce as *mut _);
        let mut flags: c_ulong = 0;
        spin_lock_irqsave(&mut ce.guc_state.lock, &mut flags);
        let banned = context_banned(ce);
        clr_context_banned(ce);
        clr_context_pending_disable(ce);
        __guc_signal_context_fence(ce);
        guc_blocked_fence_complete(ce);
        spin_unlock_irqrestore(&mut ce.guc_state.lock, flags);
        if banned {
            guc_cancel_context_requests(&mut *ce);
            unsafe { intel_engine_signal_breadcrumbs(ce.engine) };
        }
    }
    decr_outstanding_submission_g2h(guc);
    intel_context_put(&mut *ce);
    0
}

// upstream: intel_guc_submission.c capture_error_state()
fn capture_error_state(guc: &mut intel_guc, ce: &mut intel_context) {
    // Source caller retains the live GT, engine and context owners.
    unsafe {

    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let mut engine_mask;
    if intel_engine_is_virtual(ce.engine) {
        let mut virtual_mask = unsafe { (*ce.engine).mask };
        engine_mask = 0;
        for_each_engine_masked!(engine, tmp, unsafe { (*ce.engine).gt }, virtual_mask, {
            let matched = intel_guc_capture_is_matching_engine(gt, ce, engine);
            if matched {
                intel_engine_set_hung_context(engine, ce);
                engine_mask |= unsafe { (*engine).mask };
                i915_increase_reset_engine_count(&mut (*i915).gpu_error, engine);
            }
        });
        if engine_mask == 0 {
            guc_warn!(
                guc,
                "No matching physical engine capture for virtual engine context 0x%04X / %s",
                ce.guc_id.id as c_ulong,
                unsafe { (*ce.engine).name }
            );
            engine_mask = !0;
        }
    } else {
        intel_engine_set_hung_context(ce.engine, ce);
        engine_mask = unsafe { (*ce.engine).mask };
        i915_increase_reset_engine_count(&mut (*i915).gpu_error, ce.engine);
    }
    with_intel_runtime_pm!(&(*i915).runtime_pm, wakeref, {
        i915_capture_error_state(gt, engine_mask, CORE_DUMP_FLAG_IS_GUC_CAPTURE);
    });
    }
}

// upstream: intel_guc_submission.c guc_context_replay()
fn guc_context_replay(ce: &mut intel_context) {
    let sched_engine = unsafe { (*ce.engine).sched_engine };
    __guc_reset_context(ce, unsafe { (*ce.engine).mask });
    unsafe { tasklet_hi_schedule(core::ptr::addr_of_mut!((*sched_engine).tasklet)) };
}

// upstream: intel_guc_submission.c guc_handle_context_reset()
fn guc_handle_context_reset(guc: &mut intel_guc, ce: &mut intel_context) {
    let capture = intel_context_is_schedulable(ce as *const _);
    trace_intel_context_reset(ce);
    guc_dbg!(
        &mut *guc,
        "%s context reset notification: 0x%04X on %s, exiting = %s, banned = %s\n",
        if capture { "Got" } else { "Ignoring" },
        ce.guc_id.id,
        unsafe { (*ce.engine).name },
        str_yes_no(unsafe { intel_context_is_exiting(ce as *const _) }),
        str_yes_no(unsafe { intel_context_is_banned(ce as *const _) })
    );
    if capture {
        capture_error_state(&mut *guc, &mut *ce);
        guc_context_replay(&mut *ce);
    }
}

// upstream: intel_guc_submission.c intel_guc_context_reset_process_msg()
fn intel_guc_context_reset_process_msg(guc: &mut intel_guc, msg: &[u32], len: u32) -> i32 {
    if unlikely(len != 1) {
        guc_err!(guc, "Invalid length %u", len);
        return -EPROTO;
    }
    let ctx_id = msg[0];
    let mut flags: c_ulong = 0;
    // xarray lookup itself needs only RCU, but hold the lock until acquiring a
    // reference so asynchronous destruction cannot overlap reset handling.
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    let ce = g2h_context_lookup(guc, ctx_id);
    if !ce.is_null() {
        unsafe { intel_context_get(&mut *ce) };
    }
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
    if unlikely(ce.is_null()) {
        return -EPROTO;
    }
    guc_handle_context_reset(guc, unsafe { &mut *ce });
    unsafe { intel_context_put(&mut *ce) };
    0
}

// upstream: intel_guc_submission.c intel_guc_error_capture_process_msg()
fn intel_guc_error_capture_process_msg(guc: &mut intel_guc, msg: &[u32], len: u32) -> i32 {
    if unlikely(len != 1) {
        guc_dbg!(guc, "Invalid length %u", len);
        return -EPROTO;
    }
    let status = msg[0] & INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_MASK;
    if status == INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_NOSPACE {
        guc_warn!(&mut *guc, "No space for error capture");
    }
    unsafe { intel_guc_capture_process(&mut *guc) };
    0
}

// upstream: intel_guc_submission.c intel_guc_lookup_engine()
fn intel_guc_lookup_engine(guc: &intel_guc, guc_class: u8, instance: u8) -> *mut intel_engine_cs {
    let gt = guc_to_gt_const(guc);
    let engine_class = crate::intel_guc_fwif_types_upstream::guc_class_to_engine_class(guc_class);
    // Class index is validated by the class converter.
    gem_bug_on!(instance > MAX_ENGINE_INSTANCE as u8);
    unsafe { (*gt).engine_class[engine_class as usize][instance as usize] }
}

// upstream: intel_guc_submission.c reset_fail_worker_func()
fn reset_fail_worker_func(w: &mut work_struct) {
    let guc = container_of!(w, intel_guc, submission_state.reset_fail_worker);
    let gt = unsafe { guc_to_gt(guc) };
    let guc = unsafe { &mut *guc };
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
    let reset_fail_mask = guc.submission_state.reset_fail_mask;
    guc.submission_state.reset_fail_mask = 0;
    spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
    if likely(reset_fail_mask != 0) {
        // GuC loops after reporting reset failure. Determine the guilty context
        // manually; with GuC stopped it cannot schedule behind KMD's back.
        for_each_engine_masked!(engine, id, gt, reset_fail_mask, {
            intel_guc_find_hung_context(unsafe { &mut *engine });
        });
        unsafe {
            intel_gt_handle_error(
                gt,
                reset_fail_mask,
                I915_ERROR_CAPTURE as c_ulong,
                c"GuC failed to reset engine mask=0x%x".as_ptr(),
                reset_fail_mask,
            )
        };
    }
}

// upstream: intel_guc_submission.c intel_guc_engine_failure_process_msg()
fn intel_guc_engine_failure_process_msg(guc: &mut intel_guc, msg: &[u32], len: u32) -> i32 {
    if unlikely(len != 3) {
        guc_err!(guc, "Invalid length %u", len);
        return -EPROTO;
    }
    let guc_class = msg[0] as u8;
    let instance = msg[1] as u8;
    let reason = msg[2];
    let engine = intel_guc_lookup_engine(guc, guc_class, instance);
    if unlikely(engine.is_null()) {
        guc_err!(guc, "Invalid engine %d:%d", guc_class, instance);
        return -EPROTO;
    }
    // Unexpected hardware feature failure: report a real error, not only reset info.
    guc_err!(
        &mut *guc,
        "Engine reset failed on %d:%d (%s) because 0x%08X",
        guc_class,
        instance,
        unsafe { (*engine).name },
        reason
    );
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(&mut guc.submission_state.lock, &mut flags);
    guc.submission_state.reset_fail_mask |= unsafe { (*engine).mask };
    spin_unlock_irqrestore(&mut guc.submission_state.lock, flags);
    // GT reset flushes the G2H worker, so schedule another worker to trigger it.
    unsafe { queue_work(system_dfl_wq, &mut guc.submission_state.reset_fail_worker) };
    0
}

// upstream: intel_guc_submission.c intel_guc_find_hung_context()
fn intel_guc_find_hung_context(engine: &mut intel_engine_cs) {
    let guc = unsafe { &mut *gt_to_guc(engine.gt) };
    if unlikely(!guc_submission_initialized(guc)) {
        // Reset during load, before GuC initialization.
        return;
    }
    let mut flags: c_ulong = 0;
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    for (_, entry) in crate::linux::xarray::xa_snapshot(&mut guc.context_lookup) {
        let ce = entry.cast::<intel_context>();
        if !kref_get_unless_zero(unsafe { &mut *(*ce).r#ref.refcount }) {
            continue;
        }
        xa_unlock(&mut guc.context_lookup);
        let ce_ref = unsafe { &mut *ce };
        let matches_engine = if !intel_context_is_pinned(&mut *ce_ref) {
            false
        } else if unsafe { intel_engine_is_virtual(ce_ref.engine) } {
            (unsafe { (*ce_ref.engine).mask }) & engine.mask != 0
        } else {
            ce_ref.engine == engine as *mut _
        };
        if matches_engine {
            let mut found = false;
            spin_lock(&mut ce_ref.guc_state.lock);
            let mut rq: *mut i915_request = core::ptr::null_mut();
            list_for_each_entry!(rq, &ce_ref.guc_state.requests, sched.link, {
                if unsafe { i915_test_request_state(rq) } == I915_REQUEST_ACTIVE {
                    found = true;
                    break;
                }
            });
            spin_unlock(&mut ce_ref.guc_state.lock);
            if found {
                unsafe { intel_engine_set_hung_context(engine, ce_ref) };
                // Only one hung context can be handled at a time.
                unsafe { intel_context_put(&mut *ce) };
                xa_lock(&mut guc.context_lookup);
                break;
            }
        }
        unsafe { intel_context_put(&mut *ce) };
        xa_lock(&mut guc.context_lookup);
    }
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
}

// upstream: intel_guc_submission.c intel_guc_dump_active_requests()
fn intel_guc_dump_active_requests(
    engine: &mut intel_engine_cs,
    hung_rq: &mut i915_request,
    m: &mut drm_printer,
) {
    let guc = unsafe { &mut *gt_to_guc(engine.gt) };
    if unlikely(!guc_submission_initialized(guc)) {
        // Reset during load, before GuC initialization.
        return;
    }
    let mut flags: c_ulong = 0;
    xa_lock_irqsave(&mut guc.context_lookup, &mut flags);
    for (_, entry) in crate::linux::xarray::xa_snapshot(&mut guc.context_lookup) {
        let ce = entry.cast::<intel_context>();
        if !kref_get_unless_zero(unsafe { &mut *(*ce).r#ref.refcount }) {
            continue;
        }
        xa_unlock(&mut guc.context_lookup);
        let ce_ref = unsafe { &mut *ce };
        let matches_engine = if !intel_context_is_pinned(&mut *ce_ref) {
            false
        } else if unsafe { intel_engine_is_virtual(ce_ref.engine) } {
            (unsafe { (*ce_ref.engine).mask }) & engine.mask != 0
        } else {
            ce_ref.engine == engine as *mut _
        };
        if matches_engine {
            spin_lock(&mut ce_ref.guc_state.lock);
            unsafe {
                intel_engine_dump_active_requests(
                    core::ptr::addr_of_mut!(ce_ref.guc_state.requests),
                    hung_rq,
                    m,
                )
            };
            spin_unlock(&mut ce_ref.guc_state.lock);
        }
        unsafe { intel_context_put(&mut *ce) };
        xa_lock(&mut guc.context_lookup);
    }
    xa_unlock_irqrestore(&mut guc.context_lookup, flags);
}

// upstream: intel_guc_submission.c intel_guc_submission_print_info()
fn intel_guc_submission_print_info(guc: &mut intel_guc, p: &mut drm_printer) {
    let sched_engine = guc.sched_engine;
    if sched_engine.is_null() {
        return;
    }
    drm_printf!(
        p,
        "GuC Submission API Version: %d.%d.%d\n",
        guc.submission_version.major,
        guc.submission_version.minor,
        guc.submission_version.patch
    );
    drm_printf!(
        p,
        "GuC Number Outstanding Submission G2H: %u\n",
        atomic_read(&guc.outstanding_submission_g2h)
    );
    drm_printf!(p, "GuC tasklet count: %u\n", unsafe {
        atomic_read(&(*sched_engine).tasklet.count)
    });
    let mut flags: c_ulong = 0;
    spin_lock_irqsave(unsafe { &mut (*sched_engine).lock }, &mut flags);
    drm_printf!(p, "Requests in GuC submit tasklet:\n");
    let mut rb = rb_first_cached(unsafe { &(*sched_engine).queue });
    while !rb.is_null() {
        let pl = to_priolist(unsafe { &*rb });
        priolist_for_each_request!(rq, pl, {
            drm_printf!(
                p,
                "guc_id=%u, seqno=%llu\n",
                unsafe { (*(*rq).context).guc_id.id },
                unsafe { (*rq).fence.seqno }
            );
        });
        rb = unsafe { rb_next(rb) };
    }
    spin_unlock_irqrestore(unsafe { &mut (*sched_engine).lock }, flags);
    drm_printf!(p, "\n");
}

// upstream: intel_guc_submission.c guc_log_context_priority()
#[inline]
fn guc_log_context_priority(p: &mut drm_printer, ce: &intel_context) {
    drm_printf!(p, "\t\tPriority: %d\n", ce.guc_state.prio);
    drm_printf!(p, "\t\tNumber Requests (lower index == higher priority)\n");
    for i in GUC_CLIENT_PRIORITY_KMD_HIGH..GUC_CLIENT_PRIORITY_NUM {
        drm_printf!(
            p,
            "\t\tNumber requests in priority band[%d]: %d\n",
            i,
            ce.guc_state.prio_count[i as usize]
        );
    }
    drm_printf!(p, "\n");
}

// upstream: intel_guc_submission.c guc_log_context()
#[inline]
fn guc_log_context(p: &mut drm_printer, ce: &mut intel_context) {
    drm_printf!(p, "GuC lrc descriptor %u:\n", ce.guc_id.id);
    drm_printf!(p, "\tHW Context Desc: 0x%08x\n", ce.lrc.lrca);
    if intel_context_pin_if_active(&mut *ce) {
        drm_printf!(
            p,
            "\t\tLRC Head: Internal %u, Memory %u\n",
            unsafe { (*ce.ring).head },
            unsafe { *ce.lrc_reg_state.add(CTX_RING_HEAD) }
        );
        drm_printf!(
            p,
            "\t\tLRC Tail: Internal %u, Memory %u\n",
            unsafe { (*ce.ring).tail },
            unsafe { *ce.lrc_reg_state.add(CTX_RING_TAIL) }
        );
        intel_context_unpin(&mut *ce);
    } else {
        drm_printf!(
            p,
            "\t\tLRC Head: Internal %u, Memory not pinned\n",
            unsafe { (*ce.ring).head }
        );
        drm_printf!(
            p,
            "\t\tLRC Tail: Internal %u, Memory not pinned\n",
            unsafe { (*ce.ring).tail }
        );
    }
    drm_printf!(p, "\t\tContext Pin Count: %u\n", atomic_read(&ce.pin_count));
    drm_printf!(
        p,
        "\t\tGuC ID Ref Count: %u\n",
        atomic_read(&ce.guc_id.r#ref)
    );
    drm_printf!(p, "\t\tSchedule State: 0x%x\n", ce.guc_state.sched_state);
}

// upstream: intel_guc_submission.c intel_guc_submission_print_context_info()
fn intel_guc_submission_print_context_info(guc: &mut intel_guc, p: &mut drm_printer) {
    for (_, entry) in crate::linux::xarray::xa_snapshot(&mut guc.context_lookup) {
        let ce = unsafe { &mut *entry.cast::<intel_context>() };
        gem_bug_on!(intel_context_is_child(&mut *ce));
        guc_log_context(p, &mut *ce);
        guc_log_context_priority(p, &*ce);
        if unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) } {
            drm_printf!(p, "\t\tNumber children: %u\n", ce.parallel.number_children);
            if !ce.parallel.guc.wq_status.is_null() {
                drm_printf!(p, "\t\tWQI Head: %u\n", READ_ONCE!(ce.parallel.guc.wq_head));
                drm_printf!(p, "\t\tWQI Tail: %u\n", READ_ONCE!(ce.parallel.guc.wq_tail));
                drm_printf!(
                    p,
                    "\t\tWQI Status: %u\n",
                    READ_ONCE!(ce.parallel.guc.wq_status)
                );
            }
            if unsafe { (*ce.engine).emit_bb_start }
                == Some(emit_bb_start_parent_no_preempt_mid_batch_callback)
            {
                drm_printf!(p, "\t\tChildren Go: %u\n", get_children_go_value(ce));
                for i in 0..ce.parallel.number_children {
                    drm_printf!(
                        p,
                        "\t\tChildren Join: %u\n",
                        get_children_join_value(ce, i as u8)
                    );
                }
            }
            for_each_child!(ce, child, {
                guc_log_context(p, unsafe { &mut *child });
            });
        }
    }
}

// upstream: intel_guc_submission.c get_children_go_addr()
#[inline]
fn get_children_go_addr(ce: &intel_context) -> u32 {
    gem_bug_on!(!unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    (unsafe { i915_ggtt_offset(ce.state) })
        + __get_parent_scratch_offset(ce)
        + offset_of!(parent_scratch, go.semaphore) as u32
}

// upstream: intel_guc_submission.c get_children_join_addr()
#[inline]
fn get_children_join_addr(ce: &intel_context, child_index: u8) -> u32 {
    gem_bug_on!(!unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    (unsafe { i915_ggtt_offset(ce.state) })
        + __get_parent_scratch_offset(ce)
        + (offset_of!(parent_scratch, join)
            + child_index as usize * size_of::<sync_semaphore>()
            + offset_of!(sync_semaphore, semaphore)) as u32
}

const PARENT_GO_BB: u32 = 1;
const PARENT_GO_FINI_BREADCRUMB: u32 = 0;
const CHILD_GO_BB: u32 = 1;
const CHILD_GO_FINI_BREADCRUMB: u32 = 0;

// upstream: intel_guc_submission.c emit_bb_start_parent_no_preempt_mid_batch()
fn emit_bb_start_parent_no_preempt_mid_batch(
    rq: &mut i915_request,
    offset: u64,
    _len: u32,
    flags: u32,
) -> i32 {
    let ce = unsafe { &*rq.context };
    gem_bug_on!(!unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    let mut cs = unsafe { intel_ring_begin(rq, 10 + 4 * ce.parallel.number_children as u32) };
    if IS_ERR(cs) {
        return PTR_ERR(cs);
    }
    // Wait for every child to report that its batch has reached the boundary.
    for i in 0..ce.parallel.number_children {
        unsafe {
            *cs = MI_SEMAPHORE_WAIT
                | MI_SEMAPHORE_GLOBAL_GTT
                | MI_SEMAPHORE_POLL
                | MI_SEMAPHORE_SAD_EQ_SDD;
            cs = cs.add(1);
            *cs = PARENT_GO_BB;
            cs = cs.add(1);
            *cs = get_children_join_addr(ce, i as u8);
            cs = cs.add(1);
            *cs = 0;
            cs = cs.add(1);
        }
    }
    // Disable preemption around the handshake.
    unsafe {
        *cs = MI_ARB_ON_OFF | MI_ARB_DISABLE;
        cs = cs.add(1);
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    cs = unsafe { gen8_emit_ggtt_write(cs, CHILD_GO_BB, get_children_go_addr(ce), 0) };
    unsafe {
        *cs = MI_BATCH_BUFFER_START_GEN8
            | (if flags & I915_DISPATCH_SECURE != 0 {
                0
            } else {
                bit(8)
            });
        cs = cs.add(1);
        *cs = lower_32_bits(offset);
        cs = cs.add(1);
        *cs = upper_32_bits(offset);
        cs = cs.add(1);
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    unsafe { intel_ring_advance(rq, cs) };
    0
}

unsafe extern "C" fn emit_bb_start_parent_no_preempt_mid_batch_callback(
    rq: *mut i915_request,
    offset: u64,
    len: u32,
    flags: u32,
) -> i32 {
    // SAFETY: the engine emission callback receives a live request.
    emit_bb_start_parent_no_preempt_mid_batch(unsafe { &mut *rq }, offset, len, flags)
}

// upstream: intel_guc_submission.c emit_bb_start_child_no_preempt_mid_batch()
fn emit_bb_start_child_no_preempt_mid_batch(
    rq: &mut i915_request,
    offset: u64,
    _len: u32,
    flags: u32,
) -> i32 {
    let ce = unsafe { &*rq.context };
    let parent = intel_context_to_parent(ce);
    gem_bug_on!(!intel_context_is_child(ce));
    let mut cs = unsafe { intel_ring_begin(rq, 12) };
    if IS_ERR(cs) {
        return PTR_ERR(cs);
    }
    // Signal the parent that the child reached the batch boundary.
    cs = unsafe { gen8_emit_ggtt_write(
        cs,
        PARENT_GO_BB,
        get_children_join_addr(unsafe { &*parent }, ce.parallel.child_index),
        0,
    ) };
    // Wait until the parent releases this child.
    unsafe {
        *cs = MI_SEMAPHORE_WAIT
            | MI_SEMAPHORE_GLOBAL_GTT
            | MI_SEMAPHORE_POLL
            | MI_SEMAPHORE_SAD_EQ_SDD;
        cs = cs.add(1);
        *cs = CHILD_GO_BB;
        cs = cs.add(1);
        *cs = get_children_go_addr(unsafe { &*parent });
        cs = cs.add(1);
        *cs = 0;
        cs = cs.add(1);
        // Disable preemption during the batch.
        *cs = MI_ARB_ON_OFF | MI_ARB_DISABLE;
        cs = cs.add(1);
        *cs = MI_BATCH_BUFFER_START_GEN8
            | (if flags & I915_DISPATCH_SECURE != 0 {
                0
            } else {
                bit(8)
            });
        cs = cs.add(1);
        *cs = lower_32_bits(offset);
        cs = cs.add(1);
        *cs = upper_32_bits(offset);
        cs = cs.add(1);
    }
    unsafe { intel_ring_advance(rq, cs) };
    0
}

unsafe extern "C" fn emit_bb_start_child_no_preempt_mid_batch_callback(
    rq: *mut i915_request,
    offset: u64,
    len: u32,
    flags: u32,
) -> i32 {
    // SAFETY: the engine emission callback receives a live request.
    emit_bb_start_child_no_preempt_mid_batch(unsafe { &mut *rq }, offset, len, flags)
}

// upstream: intel_guc_submission.c __emit_fini_breadcrumb_parent_no_preempt_mid_batch()
fn __emit_fini_breadcrumb_parent_no_preempt_mid_batch(
    rq: &mut i915_request,
    mut cs: *mut u32,
) -> *mut u32 {
    let ce = unsafe { &*rq.context };
    gem_bug_on!(!unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    // Wait for children to reach the fini-breadcrumb boundary.
    for i in 0..ce.parallel.number_children {
        unsafe {
            *cs = MI_SEMAPHORE_WAIT
                | MI_SEMAPHORE_GLOBAL_GTT
                | MI_SEMAPHORE_POLL
                | MI_SEMAPHORE_SAD_EQ_SDD;
            cs = cs.add(1);
            *cs = PARENT_GO_FINI_BREADCRUMB;
            cs = cs.add(1);
            *cs = get_children_join_addr(ce, i as u8);
            cs = cs.add(1);
            *cs = 0;
            cs = cs.add(1);
        }
    }
    // Re-enable preemption, then release children.
    unsafe {
        *cs = MI_ARB_ON_OFF | MI_ARB_ENABLE;
        cs = cs.add(1);
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    unsafe { gen8_emit_ggtt_write(cs, CHILD_GO_FINI_BREADCRUMB, get_children_go_addr(ce), 0) }
}

// upstream: intel_guc_submission.c skip_handshake()
#[inline]
fn skip_handshake(rq: &i915_request) -> bool {
    test_bit(I915_FENCE_FLAG_SKIP_PARALLEL, &rq.fence.flags)
}

const NON_SKIP_LEN: usize = 6;

// upstream: intel_guc_submission.c emit_fini_breadcrumb_parent_no_preempt_mid_batch()
fn emit_fini_breadcrumb_parent_no_preempt_mid_batch(
    rq: &mut i915_request,
    mut cs: *mut u32,
) -> *mut u32 {
    let ce = unsafe { &*rq.context };
    gem_bug_on!(!unsafe { intel_context_is_parent(ce as *const intel_context as *mut intel_context) });
    let start_fini_breadcrumb_cs = cs;
    let before_fini_breadcrumb_user_interrupt_cs;
    if unlikely(skip_handshake(rq)) {
        // NOP the complete handshake when a parallel submission has an error.
        unsafe {
            core::ptr::write_bytes(
                cs,
                0,
                (*ce.engine).emit_fini_breadcrumb_dw as usize - NON_SKIP_LEN,
            );
            cs = cs.add((*ce.engine).emit_fini_breadcrumb_dw as usize - NON_SKIP_LEN);
        }
    } else {
        cs = __emit_fini_breadcrumb_parent_no_preempt_mid_batch(rq, cs);
    }
    // Emit the fini breadcrumb sequence-number write.
    before_fini_breadcrumb_user_interrupt_cs = cs;
    cs = unsafe { gen8_emit_ggtt_write(
        cs,
        rq.fence.seqno as u32,
        unsafe { (*i915_request_active_timeline(rq)).hwsp_offset },
        0,
    ) };
    // Emit user interrupt.
    unsafe {
        *cs = MI_USER_INTERRUPT;
        cs = cs.add(1);
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    gem_bug_on!(unsafe { before_fini_breadcrumb_user_interrupt_cs.add(NON_SKIP_LEN) } != cs);
    gem_bug_on!(unsafe { start_fini_breadcrumb_cs.add((*ce.engine).emit_fini_breadcrumb_dw as usize) } != cs);
    rq.tail = unsafe { intel_ring_offset(rq, cs.cast()) };
    cs
}

unsafe extern "C" fn emit_fini_breadcrumb_parent_no_preempt_mid_batch_callback(
    rq: *mut i915_request,
    cs: *mut u32,
) -> *mut u32 {
    // SAFETY: the engine emission callback receives a live request and CS.
    emit_fini_breadcrumb_parent_no_preempt_mid_batch(unsafe { &mut *rq }, cs)
}

// upstream: intel_guc_submission.c __emit_fini_breadcrumb_child_no_preempt_mid_batch()
fn __emit_fini_breadcrumb_child_no_preempt_mid_batch(
    rq: &mut i915_request,
    mut cs: *mut u32,
) -> *mut u32 {
    let ce = unsafe { &*rq.context };
    let parent = unsafe { &*intel_context_to_parent(rq.context) };
    gem_bug_on!(!intel_context_is_child(ce));
    // Re-enable preemption, then signal the parent.
    unsafe {
        *cs = MI_ARB_ON_OFF | MI_ARB_ENABLE;
        cs = cs.add(1);
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    cs = unsafe { gen8_emit_ggtt_write(
        cs,
        PARENT_GO_FINI_BREADCRUMB,
        get_children_join_addr(parent, ce.parallel.child_index),
        0,
    ) };
    // Wait for parent release before the next batch.
    unsafe {
        *cs = MI_SEMAPHORE_WAIT
            | MI_SEMAPHORE_GLOBAL_GTT
            | MI_SEMAPHORE_POLL
            | MI_SEMAPHORE_SAD_EQ_SDD;
        cs = cs.add(1);
        *cs = CHILD_GO_FINI_BREADCRUMB;
        cs = cs.add(1);
        *cs = get_children_go_addr(parent);
        cs = cs.add(1);
        *cs = 0;
        cs = cs.add(1);
    }
    cs
}

// upstream: intel_guc_submission.c emit_fini_breadcrumb_child_no_preempt_mid_batch()
fn emit_fini_breadcrumb_child_no_preempt_mid_batch(
    rq: &mut i915_request,
    mut cs: *mut u32,
) -> *mut u32 {
    let ce = unsafe { &*rq.context };
    gem_bug_on!(!intel_context_is_child(ce));
    let start_fini_breadcrumb_cs = cs;
    let before_fini_breadcrumb_user_interrupt_cs;
    if unlikely(skip_handshake(rq)) {
        // NOP the handshake after an error in any request in the relationship.
        unsafe {
            core::ptr::write_bytes(
                cs,
                0,
                (*ce.engine).emit_fini_breadcrumb_dw as usize - NON_SKIP_LEN,
            );
            cs = cs.add((*ce.engine).emit_fini_breadcrumb_dw as usize - NON_SKIP_LEN);
        }
    } else {
        cs = __emit_fini_breadcrumb_child_no_preempt_mid_batch(rq, cs);
    }
    // Emit the fini breadcrumb sequence-number write.
    before_fini_breadcrumb_user_interrupt_cs = cs;
    cs = unsafe { gen8_emit_ggtt_write(
        cs,
        rq.fence.seqno as u32,
        unsafe { (*i915_request_active_timeline(rq)).hwsp_offset },
        0,
    ) };
    // Emit user interrupt.
    unsafe {
        *cs = MI_USER_INTERRUPT;
        cs = cs.add(1);
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    gem_bug_on!(unsafe { before_fini_breadcrumb_user_interrupt_cs.add(NON_SKIP_LEN) } != cs);
    gem_bug_on!(unsafe { start_fini_breadcrumb_cs.add((*ce.engine).emit_fini_breadcrumb_dw as usize) } != cs);
    rq.tail = unsafe { intel_ring_offset(rq, cs.cast()) };
    cs
}

unsafe extern "C" fn emit_fini_breadcrumb_child_no_preempt_mid_batch_callback(
    rq: *mut i915_request,
    cs: *mut u32,
) -> *mut u32 {
    // SAFETY: the engine emission callback receives a live request and CS.
    emit_fini_breadcrumb_child_no_preempt_mid_batch(unsafe { &mut *rq }, cs)
}

// upstream: intel_guc_submission.c guc_create_virtual()
fn guc_create_virtual(
    siblings: &[*mut intel_engine_cs],
    count: u32,
    flags: u64,
) -> Result<*mut intel_context, i32> {
    let mut ve = kzalloc_obj::<guc_virtual_engine>();
    if ve.is_null() {
        return Err(-ENOMEM);
    }
    let guc = unsafe { gt_to_guc((*siblings[0]).gt) };
    unsafe {
        (*ve).base.i915 = (*siblings[0]).i915;
        (*ve).base.gt = (*siblings[0]).gt;
        (*ve).base.uncore = (*siblings[0]).uncore;
        (*ve).base.id = -1;
        (*ve).base.uabi_class = I915_ENGINE_CLASS_INVALID as u16;
        (*ve).base.instance = I915_ENGINE_CLASS_INVALID_VIRTUAL as u8;
        (*ve).base.uabi_instance = I915_ENGINE_CLASS_INVALID_VIRTUAL as u16;
        (*ve).base.saturated = ALL_ENGINES;
        snprintf!(
            &mut (*ve).base.name,
            size_of_val(&(*ve).base.name),
            "virtual",
        );
        (*ve).base.sched_engine = i915_sched_engine_get((*guc).sched_engine);
        (*ve).base.cops = &virtual_guc_context_ops;
        (*ve).base.request_alloc = Some(guc_request_alloc_callback);
        (*ve).base.bump_serial = Some(virtual_guc_bump_serial_callback);
        (*ve).base.submit_request = Some(guc_submit_request_callback);
        (*ve).base.flags = I915_ENGINE_IS_VIRTUAL;
        build_bug_on!(VIRTUAL_ENGINES.trailing_zeros() < I915_NUM_ENGINES as u32);
        (*ve).base.mask = VIRTUAL_ENGINES as u32;
        intel_context_init(&mut (*ve).context, &mut (*ve).base);
    }

    for n in 0..count {
        let sibling = unsafe { &mut *siblings[n as usize] };
        gem_bug_on!(!is_power_of_2(sibling.mask));
        unsafe {
            if sibling.mask & (*ve).base.mask != 0 {
                guc_dbg!(guc, "duplicate %s entry in load balancer\n", sibling.name);
                intel_context_put(&mut (*ve).context);
                kfree(ve);
                return Err(-EINVAL);
            }
            (*ve).base.mask |= sibling.mask;
            (*ve).base.logical_mask |= sibling.logical_mask;
            if n != 0 && (*ve).base.class != sibling.class {
                guc_dbg!(
                    guc,
                    "invalid mixing of engine class, sibling %d, already %d\n",
                    sibling.class,
                    (*ve).base.class
                );
                intel_context_put(&mut (*ve).context);
                kfree(ve);
                return Err(-EINVAL);
            } else if n == 0 {
                (*ve).base.class = sibling.class;
                (*ve).base.uabi_class = sibling.uabi_class;
                snprintf!(
                    &mut (*ve).base.name,
                    size_of_val(&(*ve).base.name),
                    "v%dx%d",
                    (*ve).base.class,
                    count,
                );
                (*ve).base.context_size = sibling.context_size;
                (*ve).base.add_active_request = sibling.add_active_request;
                (*ve).base.remove_active_request = sibling.remove_active_request;
                (*ve).base.emit_bb_start = sibling.emit_bb_start;
                (*ve).base.emit_flush = sibling.emit_flush;
                (*ve).base.emit_init_breadcrumb = sibling.emit_init_breadcrumb;
                (*ve).base.emit_fini_breadcrumb = sibling.emit_fini_breadcrumb;
                (*ve).base.emit_fini_breadcrumb_dw = sibling.emit_fini_breadcrumb_dw;
                (*ve).base.breadcrumbs = intel_breadcrumbs_get(sibling.breadcrumbs);
                (*ve).base.flags |= sibling.flags;
                (*ve).base.props.timeslice_duration_ms = sibling.props.timeslice_duration_ms;
                (*ve).base.props.preempt_timeout_ms = sibling.props.preempt_timeout_ms;
            }
        }
    }
    let context = unsafe { core::ptr::addr_of_mut!((*ve).context) };
    Ok(context)
}

// upstream: intel_guc_submission.c intel_guc_virtual_engine_has_heartbeat()
fn intel_guc_virtual_engine_has_heartbeat(ve: &intel_engine_cs) -> bool {
    let mut heartbeat = false;
    for_each_engine_masked!(engine, tmp, ve.gt, ve.mask, {
        if unsafe { READ_ONCE!((*engine).props.heartbeat_interval_ms) } != 0 {
            heartbeat = true;
            break;
        }
    });
    heartbeat
}

// Linux `static const intel_context_ops` tables. The source helpers above use
// Rust references internally, so these adapters restore the C pointer ABI at
// each table boundary without changing helper semantics.
macro_rules! context_ops_mut_i32 {
    ($adapter:ident, $target:ident) => {
        unsafe extern "C" fn $adapter(ce: *mut intel_context) -> i32 {
            assert!(!ce.is_null());
            $target(unsafe { &mut *ce })
        }
    };
}

macro_rules! context_ops_mut_void {
    ($adapter:ident, $target:ident) => {
        unsafe extern "C" fn $adapter(ce: *mut intel_context) {
            assert!(!ce.is_null());
            $target(unsafe { &mut *ce });
        }
    };
}

context_ops_mut_i32!(guc_context_alloc_op, guc_context_alloc);
context_ops_mut_void!(guc_context_close_op, guc_context_close);
context_ops_mut_void!(guc_context_unpin_op, guc_context_unpin);
context_ops_mut_void!(guc_context_post_unpin_op, guc_context_post_unpin);
context_ops_mut_void!(guc_context_sched_disable_op, guc_context_sched_disable);
context_ops_mut_void!(guc_context_update_stats_op, guc_context_update_stats);
context_ops_mut_i32!(guc_virtual_context_alloc_op, guc_virtual_context_alloc);
context_ops_mut_void!(guc_virtual_context_unpin_op, guc_virtual_context_unpin);
context_ops_mut_void!(guc_virtual_context_enter_op, guc_virtual_context_enter);
context_ops_mut_void!(guc_virtual_context_exit_op, guc_virtual_context_exit);
context_ops_mut_void!(guc_parent_context_unpin_op, guc_parent_context_unpin);
context_ops_mut_void!(guc_child_context_unpin_op, guc_child_context_unpin);
context_ops_mut_void!(
    guc_child_context_post_unpin_op,
    guc_child_context_post_unpin
);

unsafe extern "C" fn guc_context_pin_op(ce: *mut intel_context, vaddr: *mut c_void) -> i32 {
    assert!(!ce.is_null());
    guc_context_pin(unsafe { &mut *ce }, vaddr)
}

unsafe extern "C" fn guc_virtual_context_pin_op(ce: *mut intel_context, vaddr: *mut c_void) -> i32 {
    assert!(!ce.is_null());
    guc_virtual_context_pin(unsafe { &mut *ce }, vaddr)
}

unsafe extern "C" fn guc_parent_context_pin_op(ce: *mut intel_context, vaddr: *mut c_void) -> i32 {
    assert!(!ce.is_null());
    guc_parent_context_pin(unsafe { &mut *ce }, vaddr)
}

unsafe extern "C" fn guc_child_context_pin_op(ce: *mut intel_context, vaddr: *mut c_void) -> i32 {
    assert!(!ce.is_null());
    guc_child_context_pin(unsafe { &mut *ce }, vaddr)
}

unsafe extern "C" fn lrc_reset_op(ce: *mut intel_context) {
    // SAFETY: context reset callback supplies a live context.
    unsafe { lrc_reset(ce) };
}

unsafe extern "C" fn guc_context_pre_pin_op(
    ce: *mut intel_context,
    ww: *mut i915_gem_ww_ctx,
    vaddr: *mut *mut c_void,
) -> i32 {
    assert!(!ce.is_null() && !ww.is_null() && !vaddr.is_null());
    guc_context_pre_pin(unsafe { &mut *ce }, unsafe { &mut *ww }, unsafe {
        &mut *vaddr
    })
}

unsafe extern "C" fn guc_virtual_context_pre_pin_op(
    ce: *mut intel_context,
    ww: *mut i915_gem_ww_ctx,
    vaddr: *mut *mut c_void,
) -> i32 {
    assert!(!ce.is_null() && !ww.is_null() && !vaddr.is_null());
    guc_virtual_context_pre_pin(unsafe { &mut *ce }, unsafe { &mut *ww }, unsafe {
        &mut *vaddr
    })
}

unsafe extern "C" fn guc_context_revoke_op(
    ce: *mut intel_context,
    rq: *mut i915_request,
    timeout: u32,
) {
    assert!(!ce.is_null() && !rq.is_null());
    guc_context_revoke(unsafe { &mut *ce }, unsafe { &mut *rq }, timeout);
}

unsafe extern "C" fn guc_context_cancel_request_op(ce: *mut intel_context, rq: *mut i915_request) {
    assert!(!ce.is_null() && !rq.is_null());
    guc_context_cancel_request(unsafe { &mut *ce }, unsafe { &mut *rq });
}

unsafe extern "C" fn guc_context_destroy_op(kref: *mut kref) {
    assert!(!kref.is_null());
    guc_context_destroy(unsafe { &mut *kref });
}

unsafe extern "C" fn guc_child_context_destroy_op(kref: *mut kref) {
    assert!(!kref.is_null());
    guc_child_context_destroy(unsafe { &mut *kref });
}

unsafe extern "C" fn guc_create_virtual_op(
    siblings: *mut *mut intel_engine_cs,
    count: u32,
    flags: c_ulong,
) -> *mut intel_context {
    assert!(!siblings.is_null() && count != 0);
    let siblings = unsafe { core::slice::from_raw_parts(siblings, count as usize) };
    match guc_create_virtual(siblings, count, flags as u64) {
        Ok(context) => context,
        Err(error) => ERR_PTR!(error),
    }
}

unsafe extern "C" fn guc_create_parallel_op(
    engines: *mut *mut intel_engine_cs,
    num_siblings: u32,
    width: u32,
) -> *mut intel_context {
    assert!(!engines.is_null() && num_siblings != 0 && width != 0);
    let len = (num_siblings as usize)
        .checked_mul(width as usize)
        .expect("parallel engine list length overflow");
    let engines = unsafe { core::slice::from_raw_parts(engines, len) };
    match guc_create_parallel(engines, num_siblings, width) {
        Ok(context) => context,
        Err(error) => ERR_PTR!(error),
    }
}

unsafe extern "C" fn guc_virtual_get_sibling_op(
    engine: *mut intel_engine_cs,
    sibling: u32,
) -> *mut intel_engine_cs {
    assert!(!engine.is_null());
    guc_virtual_get_sibling(unsafe { &*engine }, sibling)
        .map(|engine| (engine as *const intel_engine_cs).cast_mut())
        .unwrap_or(core::ptr::null_mut())
}

static guc_context_ops: IntelContextOps = IntelContextOps {
    flags: COPS_RUNTIME_CYCLES,
    alloc: Some(guc_context_alloc_op),
    revoke: Some(guc_context_revoke_op),
    close: Some(guc_context_close_op),
    pre_pin: Some(guc_context_pre_pin_op),
    pin: Some(guc_context_pin_op),
    unpin: Some(guc_context_unpin_op),
    post_unpin: Some(guc_context_post_unpin_op),
    cancel_request: Some(guc_context_cancel_request_op),
    enter: Some(intel_context_enter_engine),
    exit: Some(intel_context_exit_engine),
    sched_disable: Some(guc_context_sched_disable_op),
    update_stats: Some(guc_context_update_stats_op),
    reset: Some(lrc_reset_op),
    destroy: Some(guc_context_destroy_op),
    create_virtual: Some(guc_create_virtual_op),
    create_parallel: Some(guc_create_parallel_op),
    get_sibling: None,
};

static virtual_guc_context_ops: IntelContextOps = IntelContextOps {
    flags: COPS_RUNTIME_CYCLES,
    alloc: Some(guc_virtual_context_alloc_op),
    revoke: Some(guc_context_revoke_op),
    close: Some(guc_context_close_op),
    pre_pin: Some(guc_virtual_context_pre_pin_op),
    pin: Some(guc_virtual_context_pin_op),
    unpin: Some(guc_virtual_context_unpin_op),
    post_unpin: Some(guc_context_post_unpin_op),
    cancel_request: Some(guc_context_cancel_request_op),
    enter: Some(guc_virtual_context_enter_op),
    exit: Some(guc_virtual_context_exit_op),
    sched_disable: Some(guc_context_sched_disable_op),
    update_stats: Some(guc_context_update_stats_op),
    reset: None,
    destroy: Some(guc_context_destroy_op),
    create_virtual: None,
    create_parallel: None,
    get_sibling: Some(guc_virtual_get_sibling_op),
};

static virtual_parent_context_ops: IntelContextOps = IntelContextOps {
    flags: 0,
    alloc: Some(guc_virtual_context_alloc_op),
    revoke: Some(guc_context_revoke_op),
    close: Some(guc_context_close_op),
    pre_pin: Some(guc_context_pre_pin_op),
    pin: Some(guc_parent_context_pin_op),
    unpin: Some(guc_parent_context_unpin_op),
    post_unpin: Some(guc_context_post_unpin_op),
    cancel_request: Some(guc_context_cancel_request_op),
    enter: Some(guc_virtual_context_enter_op),
    exit: Some(guc_virtual_context_exit_op),
    sched_disable: Some(guc_context_sched_disable_op),
    update_stats: None,
    reset: None,
    destroy: Some(guc_context_destroy_op),
    create_virtual: None,
    create_parallel: None,
    get_sibling: Some(guc_virtual_get_sibling_op),
};

static virtual_child_context_ops: IntelContextOps = IntelContextOps {
    flags: 0,
    alloc: Some(guc_virtual_context_alloc_op),
    revoke: None,
    close: None,
    pre_pin: Some(guc_context_pre_pin_op),
    pin: Some(guc_child_context_pin_op),
    unpin: Some(guc_child_context_unpin_op),
    post_unpin: Some(guc_child_context_post_unpin_op),
    cancel_request: Some(guc_context_cancel_request_op),
    enter: Some(guc_virtual_context_enter_op),
    exit: Some(guc_virtual_context_exit_op),
    sched_disable: None,
    update_stats: None,
    reset: None,
    destroy: Some(guc_child_context_destroy_op),
    create_virtual: None,
    create_parallel: None,
    get_sibling: Some(guc_virtual_get_sibling_op),
};
