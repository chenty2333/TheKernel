// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order type transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_context_types.h.
//
// This is a separate header binding; it does not replace or alias the existing
// `intel_context_upstream` records. Root integration must explicitly switch
// consumers to these types and ensure their dependent records use the same
// module-qualified context pointer types.
//
// Layout assertions target the local Linux 7.2.3 x86_64/SMP configuration:
// PREEMPT_RT, LOCKDEP, DEBUG_MUTEXES and CONFIG_DRM_I915_SELFTEST are unset;
// CONFIG_DRM_I915_SW_FENCE_CHECK_DAG is unset. The selftest-only context tail
// and runtime underflow counters are therefore absent.

use core::{
    ffi::{c_ulong, c_void},
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

use crate::{
    i915_vma_types_upstream::I915Vma,
    intel_context_upstream::{
        I915ActiveFence, I915AddressSpace, I915GemContext, I915GemWwCtx, I915Request, Kref,
        RcuHead, WaitQueueHead,
    },
    intel_engine_cs_upstream::{
        AtomicT, DelayedWork, IntelEngineCs, ListHead, LlistHead, Mutex, RbRoot, Spinlock,
        WorkStruct,
    },
    intel_ring_types_upstream::IntelRing,
    intel_sseu_types_upstream::IntelSseu,
    intel_timeline_types_upstream::IntelTimeline,
};

// linux/average.h DECLARE_EWMA(runtime, 3, 8).
#[repr(C)]
pub struct EwmaRuntime {
    pub internal: c_ulong,
}

// Linux forward declarations used only behind pointers in this header.
#[repr(C)]
pub struct File {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct IntelRefTracker {
    _opaque: [u8; 0],
}

/// `intel_wakeref_t` from intel_wakeref.h.
#[allow(non_camel_case_types)]
pub type intel_wakeref_t = *mut IntelRefTracker;
pub type IntelWakerefT = intel_wakeref_t;

// The header embeds these records from i915_active_types.h and
// i915_sw_fence.h. They are repeated here with their C callback ABI rather
// than relying on the earlier Rust-callable mirrors.
#[repr(C)]
pub struct ActiveNode {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct I915Active {
    pub count: AtomicT,
    pub mutex: Mutex,
    pub tree_lock: Spinlock,
    pub cache: *mut ActiveNode,
    pub tree: RbRoot,
    pub excl: I915ActiveFence,
    pub flags: c_ulong,
    pub active: Option<unsafe extern "C" fn(*mut I915Active) -> i32>,
    pub retire: Option<unsafe extern "C" fn(*mut I915Active)>,
    pub work: WorkStruct,
    pub preallocated_barriers: LlistHead,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum I915SwFenceNotify {
    FenceComplete = 0,
    FenceFree     = 1,
}

#[repr(C)]
pub struct I915SwFence {
    pub wait: WaitQueueHead,
    pub fn_: Option<unsafe extern "C" fn(*mut I915SwFence, I915SwFenceNotify) -> i32>,
    pub pending: AtomicT,
    pub error: i32,
}

pub const CONTEXT_REDZONE: u32 = 0x5a;

pub const COPS_HAS_INFLIGHT_BIT: u32 = 0;
pub const COPS_HAS_INFLIGHT: c_ulong = 1 << COPS_HAS_INFLIGHT_BIT;
pub const COPS_RUNTIME_CYCLES_BIT: u32 = 1;
pub const COPS_RUNTIME_CYCLES: c_ulong = 1 << COPS_RUNTIME_CYCLES_BIT;

pub const CONTEXT_BARRIER_BIT: u32 = 0;
pub const CONTEXT_ALLOC_BIT: u32 = 1;
pub const CONTEXT_INIT_BIT: u32 = 2;
pub const CONTEXT_VALID_BIT: u32 = 3;
pub const CONTEXT_CLOSED_BIT: u32 = 4;
pub const CONTEXT_USE_SEMAPHORES: u32 = 5;
pub const CONTEXT_BANNED: u32 = 6;
pub const CONTEXT_FORCE_SINGLE_SUBMISSION: u32 = 7;
pub const CONTEXT_NOPREEMPT: u32 = 8;
pub const CONTEXT_LRCA_DIRTY: u32 = 9;
pub const CONTEXT_GUC_INIT: u32 = 10;
pub const CONTEXT_PERMA_PIN: u32 = 11;
pub const CONTEXT_IS_PARKING: u32 = 12;
pub const CONTEXT_EXITING: u32 = 13;
pub const CONTEXT_LOW_LATENCY: u32 = 14;
pub const CONTEXT_OWN_STATE: u32 = 15;

pub const GUC_CLIENT_PRIORITY_NUM: usize = 4;

/// `struct intel_context_ops`.
#[repr(C)]
pub struct IntelContextOps {
    pub flags: c_ulong,
    pub alloc: Option<unsafe extern "C" fn(*mut IntelContext) -> i32>,
    pub revoke: Option<unsafe extern "C" fn(*mut IntelContext, *mut I915Request, u32)>,
    pub close: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub pre_pin:
        Option<unsafe extern "C" fn(*mut IntelContext, *mut I915GemWwCtx, *mut *mut c_void) -> i32>,
    pub pin: Option<unsafe extern "C" fn(*mut IntelContext, *mut c_void) -> i32>,
    pub unpin: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub post_unpin: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub cancel_request: Option<unsafe extern "C" fn(*mut IntelContext, *mut I915Request)>,
    pub enter: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub exit: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub sched_disable: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub update_stats: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub reset: Option<unsafe extern "C" fn(*mut IntelContext)>,
    pub destroy: Option<unsafe extern "C" fn(*mut Kref)>,
    pub create_virtual:
        Option<unsafe extern "C" fn(*mut *mut IntelEngineCs, u32, c_ulong) -> *mut IntelContext>,
    pub create_parallel:
        Option<unsafe extern "C" fn(*mut *mut IntelEngineCs, u32, u32) -> *mut IntelContext>,
    pub get_sibling: Option<unsafe extern "C" fn(*mut IntelEngineCs, u32) -> *mut IntelEngineCs>,
}

#[repr(C)]
pub union IntelContextRef {
    pub refcount: ManuallyDrop<Kref>,
    pub rcu: ManuallyDrop<RcuHead>,
}

#[repr(C)]
pub struct IntelContextRuntimeStats {
    pub avg: EwmaRuntime,
    pub total: u64,
    pub last: u32,
    // CONFIG_DRM_I915_SELFTEST is false in this target configuration, so the
    // I915_SELFTEST_DECLARE underflow counters are absent.
}

#[repr(C)]
pub struct IntelContextStats {
    pub active: u64,
    pub runtime: IntelContextRuntimeStats,
}

#[repr(C)]
pub struct IntelContextWatchdog {
    pub timeout_us: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelContextLrcRegs {
    pub lrca: u32,
    pub ccid: u32,
}

#[repr(C)]
pub union IntelContextLrc {
    pub regs: ManuallyDrop<IntelContextLrcRegs>,
    pub desc: u64,
}

impl Deref for IntelContextLrc {
    type Target = IntelContextLrcRegs;

    fn deref(&self) -> &Self::Target {
        unsafe { &self.regs }
    }
}

impl DerefMut for IntelContextLrc {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut self.regs }
    }
}

#[repr(C)]
pub struct IntelContextGucState {
    pub lock: Spinlock,
    pub sched_state: u32,
    pub fences: ListHead,
    pub blocked: I915SwFence,
    pub requests: ListHead,
    pub prio: u8,
    pub prio_count: [u32; GUC_CLIENT_PRIORITY_NUM],
    pub sched_disable_delay_work: DelayedWork,
}

#[repr(C)]
pub struct IntelContextGucId {
    pub id: u16,
    pub r#ref: AtomicT,
    pub link: ListHead,
}

#[repr(C)]
pub union IntelContextParallelChildLink {
    pub child_list: ManuallyDrop<ListHead>,
    pub child_link: ManuallyDrop<ListHead>,
}

#[repr(C)]
pub struct IntelContextParallelGuc {
    pub wqi_head: u16,
    pub wqi_tail: u16,
    pub wq_head: *mut u32,
    pub wq_tail: *mut u32,
    pub wq_status: *mut u32,
    pub parent_page: u8,
}

#[repr(C)]
pub struct IntelContextParallel {
    pub children: IntelContextParallelChildLink,
    pub parent: *mut IntelContext,
    pub last_rq: *mut I915Request,
    pub fence_context: u64,
    pub seqno: u32,
    pub number_children: u8,
    pub child_index: u8,
    pub guc: IntelContextParallelGuc,
}

/// `struct intel_context` with the target's configured conditional fields.
#[repr(C)]
pub struct IntelContext {
    pub r#ref: IntelContextRef,
    pub engine: *mut IntelEngineCs,
    pub inflight: *mut IntelEngineCs,
    pub vm: *mut I915AddressSpace,
    pub gem_context: *mut I915GemContext,
    pub default_state: *mut File,
    pub signal_link: ListHead,
    pub signals: ListHead,
    pub signal_lock: Spinlock,
    pub state: *mut I915Vma,
    pub ring_size: u32,
    pub ring: *mut IntelRing,
    pub timeline: *mut IntelTimeline,
    pub wakeref: intel_wakeref_t,
    pub flags: c_ulong,
    pub watchdog: IntelContextWatchdog,
    pub lrc_reg_state: *mut u32,
    pub lrc: IntelContextLrc,
    pub tag: u32,
    pub stats: IntelContextStats,
    pub active_count: u32,
    pub pin_count: AtomicT,
    pub pin_mutex: Mutex,
    pub active: I915Active,
    pub ops: *const IntelContextOps,
    pub sseu: IntelSseu,
    pub pinned_contexts_link: ListHead,
    pub wa_bb_page: u8,
    pub guc_state: IntelContextGucState,
    pub guc_id: IntelContextGucId,
    pub destroyed_link: ListHead,
    pub parallel: IntelContextParallel,
}

// Layout expectations for the wt-dev x86_64 configuration. The embedded
// linux/i915 base records are shared dependencies; these assertions do not
// claim any runtime behavior for their helpers.
const _: [(); 144] = [(); core::mem::size_of::<IntelContextOps>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelContextOps>()];
const _: [(); 8] = [(); core::mem::offset_of!(IntelContextOps, alloc)];
const _: [(); 120] = [(); core::mem::offset_of!(IntelContextOps, create_virtual)];

const _: [(); 152] = [(); core::mem::size_of::<I915Active>()];
const _: [(); 32] = [(); core::mem::offset_of!(I915Active, tree_lock)];
const _: [(); 56] = [(); core::mem::offset_of!(I915Active, excl)];
const _: [(); 96] = [(); core::mem::offset_of!(I915Active, active)];
const _: [(); 40] = [(); core::mem::size_of::<I915SwFence>()];
const _: [(); 24] = [(); core::mem::offset_of!(I915SwFence, fn_)];

const _: [(); 16] = [(); core::mem::size_of::<IntelContextRef>()];
const _: [(); 152] = [(); core::mem::size_of::<I915Active>()];
const _: [(); 40] = [(); core::mem::size_of::<I915SwFence>()];
const _: [(); 24] = [(); core::mem::size_of::<IntelContextRuntimeStats>()];
const _: [(); 32] = [(); core::mem::size_of::<IntelContextStats>()];
const _: [(); 192] = [(); core::mem::size_of::<IntelContextGucState>()];
const _: [(); 84] = [(); core::mem::offset_of!(IntelContextGucState, prio_count)];
const _: [(); 104] = [(); core::mem::offset_of!(IntelContextGucState, sched_disable_delay_work)];
const _: [(); 24] = [(); core::mem::size_of::<IntelContextGucId>()];
const _: [(); 40] = [(); core::mem::size_of::<IntelContextParallelGuc>()];
const _: [(); 88] = [(); core::mem::size_of::<IntelContextParallel>()];

const _: [(); 752] = [(); core::mem::size_of::<IntelContext>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelContext>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelContext, r#ref)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelContext, engine)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelContext, signal_link)];
const _: [(); 96] = [(); core::mem::offset_of!(IntelContext, state)];
const _: [(); 176] = [(); core::mem::offset_of!(IntelContext, stats)];
const _: [(); 216] = [(); core::mem::offset_of!(IntelContext, pin_mutex)];
const _: [(); 240] = [(); core::mem::offset_of!(IntelContext, active)];
const _: [(); 392] = [(); core::mem::offset_of!(IntelContext, ops)];
const _: [(); 432] = [(); core::mem::offset_of!(IntelContext, guc_state)];
const _: [(); 624] = [(); core::mem::offset_of!(IntelContext, guc_id)];
const _: [(); 648] = [(); core::mem::offset_of!(IntelContext, destroyed_link)];
const _: [(); 664] = [(); core::mem::offset_of!(IntelContext, parallel)];
