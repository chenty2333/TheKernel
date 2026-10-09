// SPDX-License-Identifier: MIT
// Copyright © 2008-2018 Intel Corporation.
// Source-order type transcription of Linux 7.2.3
// drivers/gpu/drm/i915/i915_request.h.
//
// This file independently models the i915 request header's records. Root
// integration must switch consumers and embedded scheduler/context pointers
// together; these are not aliases to the older `intel_context_upstream`
// request/context records.

use core::{
    ffi::{c_char, c_ulong},
    mem::ManuallyDrop,
};

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_scheduler_types_upstream::{I915Dependency, I915SchedNode},
    intel_context_types_upstream::{I915SwFence, IntelContext},
    intel_context_upstream::{
        DmaFence, DmaFenceCb, Hrtimer, I915Vma, I915VmaResource, IntelTimeline, IrqWork, PinCookie,
        WaitQueueEntry,
    },
    intel_engine_cs_upstream::{
        IntelEngineCs, IntelEngineMask, ListHead, LlistHead, LlistNode, Spinlock,
    },
    intel_ring_types_upstream::IntelRing,
    linux_i915_private::DrmI915Private,
};

// Header forward declarations. These are only referenced through pointers in
// the request record and do not pretend to define their external layouts.
#[repr(C)]
pub struct DrmFile {
    _opaque: [u8; 0],
}

// Shared DRM printer forward declaration is owned by the LinuxKPI fields
// binding, not duplicated in this i915 header translation.
pub use crate::linux::fields::DrmPrinter;

#[repr(C)]
pub struct I915Deps {
    _opaque: [u8; 0],
}

/// `struct i915_capture_list`; present because this target enables
/// CONFIG_DRM_I915_CAPTURE_ERROR.
#[repr(C)]
pub struct I915CaptureList {
    pub vma_res: *mut I915VmaResource,
    pub next: *mut I915CaptureList,
}

pub const DMA_FENCE_FLAG_USER_BITS: u32 = 6;
pub const I915_FENCE_FLAG_ACTIVE: u32 = DMA_FENCE_FLAG_USER_BITS;
pub const I915_FENCE_FLAG_PQUEUE: u32 = I915_FENCE_FLAG_ACTIVE + 1;
pub const I915_FENCE_FLAG_HOLD: u32 = I915_FENCE_FLAG_ACTIVE + 2;
pub const I915_FENCE_FLAG_INITIAL_BREADCRUMB: u32 = I915_FENCE_FLAG_ACTIVE + 3;
pub const I915_FENCE_FLAG_SIGNAL: u32 = I915_FENCE_FLAG_ACTIVE + 4;
pub const I915_FENCE_FLAG_NOPREEMPT: u32 = I915_FENCE_FLAG_ACTIVE + 5;
pub const I915_FENCE_FLAG_SENTINEL: u32 = I915_FENCE_FLAG_ACTIVE + 6;
pub const I915_FENCE_FLAG_BOOST: u32 = I915_FENCE_FLAG_ACTIVE + 7;
pub const I915_FENCE_FLAG_SUBMIT_PARALLEL: u32 = I915_FENCE_FLAG_ACTIVE + 8;
pub const I915_FENCE_FLAG_SKIP_PARALLEL: u32 = I915_FENCE_FLAG_ACTIVE + 9;
pub const I915_FENCE_FLAG_COMPOSITE: u32 = I915_FENCE_FLAG_ACTIVE + 10;

#[repr(C)]
pub struct I915SwDmaFenceCb {
    pub base: DmaFenceCb,
    pub fence: *mut I915SwFence,
}

#[repr(C)]
pub struct I915RequestDurationCb {
    pub cb: DmaFenceCb,
    pub emitted: i64,
}

#[repr(C)]
pub union I915RequestSubmitUnion {
    pub submitq: ManuallyDrop<WaitQueueEntry>,
    pub dmaq: ManuallyDrop<I915SwDmaFenceCb>,
    pub duration: ManuallyDrop<I915RequestDurationCb>,
}

#[repr(C)]
pub struct I915RequestWatchdog {
    pub link: LlistNode,
    pub timer: Hrtimer,
}

pub const GUC_PRIO_INIT: u8 = 0xff;
pub const GUC_PRIO_FINI: u8 = 0xfe;

/// `struct i915_request` in the target configuration:
/// CONFIG_DRM_I915_CAPTURE_ERROR=y, CONFIG_DRM_I915_SELFTEST=n.
#[repr(C)]
pub struct I915Request {
    pub fence: DmaFence,
    pub lock: Spinlock,
    pub i915: *mut DrmI915Private,
    pub engine: *mut IntelEngineCs,
    pub context: *mut IntelContext,
    pub ring: *mut IntelRing,
    pub timeline: *mut IntelTimeline,
    pub signal_link: ListHead,
    pub signal_node: LlistNode,
    pub rcustate: c_ulong,
    pub cookie: PinCookie,
    pub submit: I915SwFence,
    pub submit_union: I915RequestSubmitUnion,
    pub execute_cb: LlistHead,
    pub semaphore: I915SwFence,
    pub submit_work: IrqWork,
    pub sched: I915SchedNode,
    pub dep: I915Dependency,
    pub execution_mask: IntelEngineMask,
    pub hwsp_seqno: *const u32,
    pub head: u32,
    pub infix: u32,
    pub postfix: u32,
    pub tail: u32,
    pub wa_tail: u32,
    pub reserved_space: u32,
    pub batch_res: *mut I915VmaResource,
    pub capture_list: *mut I915CaptureList,
    pub emitted_jiffies: c_ulong,
    pub link: ListHead,
    pub watchdog: I915RequestWatchdog,
    pub guc_fence_link: ListHead,
    pub guc_prio: u8,
    pub hucq: WaitQueueEntry,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I915RequestState {
    Unknown  = 0,
    Complete = 1,
    Pending  = 2,
    Queued   = 3,
    Active   = 4,
}
pub const I915_REQUEST_UNKNOWN: I915RequestState = I915RequestState::Unknown;
pub const I915_REQUEST_COMPLETE: I915RequestState = I915RequestState::Complete;
pub const I915_REQUEST_PENDING: I915RequestState = I915RequestState::Pending;
pub const I915_REQUEST_QUEUED: I915RequestState = I915RequestState::Queued;
pub const I915_REQUEST_ACTIVE: I915RequestState = I915RequestState::Active;

// Layout assertions for the configured Linux 7.2.3 x86_64 target. The target
// has 64-bit long/pointers, lockdep disabled (zero-sized pin_cookie), capture
// error enabled, and i915 selftests disabled.
const _: [(); 16] = [(); core::mem::size_of::<I915CaptureList>()];
const _: [(); 32] = [(); core::mem::size_of::<I915SwDmaFenceCb>()];
const _: [(); 32] = [(); core::mem::size_of::<I915RequestDurationCb>()];
const _: [(); 40] = [(); core::mem::size_of::<I915RequestSubmitUnion>()];
const _: [(); 88] = [(); core::mem::size_of::<I915RequestWatchdog>()];
const _: [(); 4] = [(); core::mem::size_of::<I915RequestState>()];
const _: [(); 672] = [(); core::mem::size_of::<I915Request>()];
const _: [(); 8] = [(); core::mem::align_of::<I915Request>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915Request, fence)];
const _: [(); 64] = [(); core::mem::offset_of!(I915Request, lock)];
const _: [(); 72] = [(); core::mem::offset_of!(I915Request, i915)];
const _: [(); 80] = [(); core::mem::offset_of!(I915Request, engine)];
const _: [(); 88] = [(); core::mem::offset_of!(I915Request, context)];
const _: [(); 112] = [(); core::mem::offset_of!(I915Request, signal_link)];
const _: [(); 144] = [(); core::mem::offset_of!(I915Request, submit)];
const _: [(); 184] = [(); core::mem::offset_of!(I915Request, submit_union)];
const _: [(); 304] = [(); core::mem::offset_of!(I915Request, sched)];
const _: [(); 368] = [(); core::mem::offset_of!(I915Request, dep)];
const _: [(); 440] = [(); core::mem::offset_of!(I915Request, execution_mask)];
const _: [(); 480] = [(); core::mem::offset_of!(I915Request, batch_res)];
const _: [(); 488] = [(); core::mem::offset_of!(I915Request, capture_list)];
const _: [(); 520] = [(); core::mem::offset_of!(I915Request, watchdog)];
const _: [(); 608] = [(); core::mem::offset_of!(I915Request, guc_fence_link)];
const _: [(); 632] = [(); core::mem::offset_of!(I915Request, hucq)];

// upstream: i915_request.h i915_request_timeline()
#[inline]
pub unsafe fn i915_request_timeline(request: *const I915Request) -> *mut IntelTimeline {
    // With CONFIG_LOCKDEP=n, rcu_dereference_protected() retains only the
    // source's dependency-ordered single-copy pointer load.
    unsafe { rcu_dereference!((*request).timeline) }
}

// Out-of-line declarations from i915_request.h. Implementations are owned by
// the separately translated i915_request.c module.
unsafe extern "C" {
    pub fn i915_request_show(
        printer: *mut DrmPrinter,
        request: *const I915Request,
        prefix: *const c_char,
        indent: i32,
    );
    pub fn i915_test_request_state(request: *mut I915Request) -> I915RequestState;
    pub fn i915_request_active_engine(
        request: *mut I915Request,
        active: *mut *mut IntelEngineCs,
    ) -> bool;
    pub fn i915_request_notify_execute_cb_imm(request: *mut I915Request);
}
