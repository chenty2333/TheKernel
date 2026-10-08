// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order type transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_context_types.h.
//
// This file is deliberately isolated from the older `intel_context_upstream`
// definitions. Consumers must explicitly switch to these context types and
// use matching module-qualified pointers at integration time.

use core::{
    ffi::{c_char, c_ulong},
    mem::ManuallyDrop,
};

// Header forward declarations whose instances are only referenced by pointer
// in the records below.
pub use crate::linux_i915_private::DrmI915Private;
use crate::{
    i915_scheduler_types_upstream::I915SchedAttr,
    intel_context_types_upstream::{I915SwFence, IntelContext, intel_wakeref_t},
    intel_context_upstream::{I915AddressSpace, Kref, RcuHead, XArray},
    intel_engine_cs_upstream::{
        AtomicT, IntelEngineCs, IntelSseu, ListHead, Mutex, Spinlock, WorkStruct,
    },
};

#[repr(C)]
pub struct DrmI915FilePrivate {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct DrmSyncobj {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct Pid {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct I915DrmClient {
    _opaque: [u8; 0],
}

#[repr(C)]
pub union I915GemEnginesLink {
    pub link: ManuallyDrop<ListHead>,
    pub rcu: ManuallyDrop<RcuHead>,
}

/// `struct i915_gem_engines`. `engines[]` is C's trailing flexible array;
/// the zero-length Rust array carries its pointer alignment and offset without
/// claiming any elements are embedded in `sizeof(struct)`.
#[repr(C)]
pub struct I915GemEngines {
    pub link_or_rcu: I915GemEnginesLink,
    pub fence: I915SwFence,
    pub ctx: *mut I915GemContext,
    pub num_engines: u32,
    pub engines: [*mut IntelContext; 0],
}

/// `struct i915_gem_engines_iter`.
#[repr(C)]
pub struct I915GemEnginesIter {
    pub idx: u32,
    pub engines: *const I915GemEngines,
}

/// `enum i915_gem_engine_type`.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I915GemEngineType {
    Invalid  = 0,
    Physical = 1,
    Balanced = 2,
    Parallel = 3,
}

/// `struct i915_gem_proto_engine`.
#[repr(C)]
pub struct I915GemProtoEngine {
    pub r#type: I915GemEngineType,
    pub engine: *mut IntelEngineCs,
    pub num_siblings: u32,
    pub width: u32,
    pub siblings: *mut *mut IntelEngineCs,
    pub sseu: IntelSseu,
}

/// `struct i915_gem_proto_context`.
#[repr(C)]
pub struct I915GemProtoContext {
    pub fpriv: *mut DrmI915FilePrivate,
    pub vm: *mut I915AddressSpace,
    pub user_flags: c_ulong,
    pub sched: I915SchedAttr,
    pub num_user_engines: i32,
    pub user_engines: *mut I915GemProtoEngine,
    pub legacy_rcs_sseu: IntelSseu,
    pub single_timeline: bool,
    pub uses_protected_content: bool,
    pub pxp_wakeref: intel_wakeref_t,
}

pub const UCONTEXT_NO_ERROR_CAPTURE: u32 = 1;
pub const UCONTEXT_BANNABLE: u32 = 2;
pub const UCONTEXT_RECOVERABLE: u32 = 3;
pub const UCONTEXT_PERSISTENCE: u32 = 4;
pub const UCONTEXT_LOW_LATENCY: u32 = 5;

pub const CONTEXT_CLOSED: u32 = 0;
pub const CONTEXT_USER_ENGINES: u32 = 1;
pub const CONTEXT_FAST_HANG_JIFFIES: u32 = 120 * crate::linux_config::HZ;

pub const TASK_COMM_LEN: usize = 16;

#[repr(C)]
pub struct I915GemContextStale {
    pub lock: Spinlock,
    pub engines: ListHead,
}

/// `struct i915_gem_context`.
#[repr(C)]
pub struct I915GemContext {
    pub i915: *mut DrmI915Private,
    pub file_priv: *mut DrmI915FilePrivate,
    pub engines: *mut I915GemEngines,
    pub engines_mutex: Mutex,
    pub syncobj: *mut DrmSyncobj,
    pub vm: *mut I915AddressSpace,
    pub pid: *mut Pid,
    pub link: ListHead,
    pub client: *mut I915DrmClient,
    pub client_link: ListHead,
    pub r#ref: Kref,
    pub release_work: WorkStruct,
    pub rcu: RcuHead,
    pub user_flags: c_ulong,
    pub flags: c_ulong,
    pub uses_protected_content: bool,
    pub pxp_wakeref: intel_wakeref_t,
    pub mutex: Mutex,
    pub sched: I915SchedAttr,
    pub guilty_count: AtomicT,
    pub active_count: AtomicT,
    pub hang_timestamp: [c_ulong; 2],
    pub remap_slice: u8,
    pub handles_vma: XArray,
    pub lut_mutex: Mutex,
    pub name: [c_char; TASK_COMM_LEN + 8],
    pub stale: I915GemContextStale,
}

// Layout assertions for the configured x86_64 wt-dev C ABI. They rely on the
// imported kref, rcu_head, mutex, list, XArray and work_struct definitions.
const _: [(); 72] = [(); core::mem::size_of::<I915GemEngines>()];
const _: [(); 8] = [(); core::mem::align_of::<I915GemEngines>()];
const _: [(); 16] = [(); core::mem::offset_of!(I915GemEngines, fence)];
const _: [(); 56] = [(); core::mem::offset_of!(I915GemEngines, ctx)];
const _: [(); 64] = [(); core::mem::offset_of!(I915GemEngines, num_engines)];
const _: [(); 72] = [(); core::mem::offset_of!(I915GemEngines, engines)];

const _: [(); 16] = [(); core::mem::size_of::<I915GemEnginesIter>()];
const _: [(); 4] = [(); core::mem::size_of::<I915GemEngineType>()];
const _: [(); 40] = [(); core::mem::size_of::<I915GemProtoEngine>()];
const _: [(); 8] = [(); core::mem::offset_of!(I915GemProtoEngine, engine)];
const _: [(); 32] = [(); core::mem::offset_of!(I915GemProtoEngine, sseu)];
const _: [(); 56] = [(); core::mem::size_of::<I915GemProtoContext>()];
const _: [(); 24] = [(); core::mem::offset_of!(I915GemProtoContext, sched)];
const _: [(); 48] = [(); core::mem::offset_of!(I915GemProtoContext, pxp_wakeref)];

const _: [(); 24] = [(); core::mem::size_of::<I915GemContextStale>()];
const _: [(); 352] = [(); core::mem::size_of::<I915GemContext>()];
const _: [(); 8] = [(); core::mem::align_of::<I915GemContext>()];
const _: [(); 8] = [(); core::mem::offset_of!(I915GemContext, file_priv)];
const _: [(); 72] = [(); core::mem::offset_of!(I915GemContext, link)];
const _: [(); 88] = [(); core::mem::offset_of!(I915GemContext, client)];
const _: [(); 112] = [(); core::mem::offset_of!(I915GemContext, r#ref)];
const _: [(); 120] = [(); core::mem::offset_of!(I915GemContext, release_work)];
const _: [(); 152] = [(); core::mem::offset_of!(I915GemContext, rcu)];
const _: [(); 168] = [(); core::mem::offset_of!(I915GemContext, user_flags)];
const _: [(); 192] = [(); core::mem::offset_of!(I915GemContext, pxp_wakeref)];
const _: [(); 224] = [(); core::mem::offset_of!(I915GemContext, sched)];
const _: [(); 264] = [(); core::mem::offset_of!(I915GemContext, handles_vma)];
const _: [(); 280] = [(); core::mem::offset_of!(I915GemContext, lut_mutex)];
const _: [(); 304] = [(); core::mem::offset_of!(I915GemContext, name)];
const _: [(); 328] = [(); core::mem::offset_of!(I915GemContext, stale)];
