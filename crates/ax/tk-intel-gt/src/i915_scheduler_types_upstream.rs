// SPDX-License-Identifier: MIT
// Copyright © 2018 Intel Corporation.
// Source-order type transcription of Linux 7.2.3
// drivers/gpu/drm/i915/i915_scheduler_types.h.
//
// The i915-owned scheduler types below are independent of the earlier
// `intel_context_upstream` mirrors. Included-header types needed by value are
// represented from their owning headers (kref, list/rbtree, tasklet, priolist).

use core::ffi::{c_ulong, c_void};

use crate::{
    intel_context_upstream::{I915Request, Kref},
    intel_engine_cs_upstream::{
        AtomicT, IntelEngineMask, ListHead, RbNode, RbRootCached, Spinlock,
    },
    linux_i915_private::DrmI915Private,
};

/// `struct i915_sched_attr`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915SchedAttr {
    pub priority: i32,
}

pub const I915_SCHED_HAS_EXTERNAL_CHAIN: u32 = 1 << 0;

/// `struct i915_sched_node`.
#[repr(C)]
pub struct I915SchedNode {
    pub signalers_list: ListHead,
    pub waiters_list: ListHead,
    pub link: ListHead,
    pub attr: I915SchedAttr,
    pub flags: u32,
    pub semaphores: IntelEngineMask,
}

pub const I915_DEPENDENCY_ALLOC: c_ulong = 1 << 0;
pub const I915_DEPENDENCY_EXTERNAL: c_ulong = 1 << 1;
pub const I915_DEPENDENCY_WEAK: c_ulong = 1 << 2;

/// `struct i915_dependency`.
#[repr(C)]
pub struct I915Dependency {
    pub signaler: *mut I915SchedNode,
    pub waiter: *mut I915SchedNode,
    pub signal_link: ListHead,
    pub wait_link: ListHead,
    pub dfs_link: ListHead,
    pub flags: c_ulong,
}

#[repr(C)]
pub union TaskletCallbacks {
    pub func: Option<unsafe extern "C" fn(c_ulong)>,
    pub callback: Option<unsafe extern "C" fn(*mut TaskletStruct)>,
}

/// `struct tasklet_struct` from linux/interrupt.h, embedded by the scheduler
/// engine header.
#[repr(C)]
pub struct TaskletStruct {
    pub next: *mut TaskletStruct,
    pub state: c_ulong,
    pub count: AtomicT,
    pub use_callback: bool,
    pub callbacks: TaskletCallbacks,
    pub data: c_ulong,
}

/// `struct i915_priolist` from i915_priolist_types.h, embedded by value in
/// `i915_sched_engine`.
#[repr(C)]
pub struct I915Priolist {
    pub requests: ListHead,
    pub node: RbNode,
    pub priority: i32,
}

/// `struct i915_sched_engine`.
#[repr(C)]
pub struct I915SchedEngine {
    pub r#ref: Kref,
    pub lock: Spinlock,
    pub requests: ListHead,
    pub hold: ListHead,
    pub tasklet: TaskletStruct,
    pub default_priolist: I915Priolist,
    pub queue_priority_hint: i32,
    pub queue: RbRootCached,
    pub no_priolist: bool,
    pub private_data: *mut c_void,
    pub destroy: Option<unsafe extern "C" fn(*mut Kref)>,
    pub disabled: Option<unsafe extern "C" fn(*mut I915SchedEngine) -> bool>,
    pub kick_backend: Option<unsafe extern "C" fn(*const I915Request, i32)>,
    pub bump_inflight_request_prio: Option<unsafe extern "C" fn(*mut I915Request, i32)>,
    pub retire_inflight_request_prio: Option<unsafe extern "C" fn(*mut I915Request)>,
    pub schedule: Option<unsafe extern "C" fn(*mut I915Request, *const I915SchedAttr)>,
}

// Layout assertions for the configured Linux 7.2.3 x86_64 target. Pointer,
// unsigned long and scheduler masks are 64-bit/32-bit as defined by the
// included headers; the source configuration has non-RT 4-byte spinlocks.
const _: [(); 4] = [(); core::mem::size_of::<I915SchedAttr>()];
const _: [(); 64] = [(); core::mem::size_of::<I915SchedNode>()];
const _: [(); 8] = [(); core::mem::align_of::<I915SchedNode>()];
const _: [(); 48] = [(); core::mem::offset_of!(I915SchedNode, attr)];
const _: [(); 56] = [(); core::mem::offset_of!(I915SchedNode, semaphores)];
const _: [(); 72] = [(); core::mem::size_of::<I915Dependency>()];
const _: [(); 64] = [(); core::mem::offset_of!(I915Dependency, flags)];
const _: [(); 40] = [(); core::mem::size_of::<TaskletStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<TaskletStruct>()];
const _: [(); 48] = [(); core::mem::size_of::<I915Priolist>()];
const _: [(); 216] = [(); core::mem::size_of::<I915SchedEngine>()];
const _: [(); 8] = [(); core::mem::align_of::<I915SchedEngine>()];
const _: [(); 8] = [(); core::mem::offset_of!(I915SchedEngine, requests)];
const _: [(); 40] = [(); core::mem::offset_of!(I915SchedEngine, tasklet)];
const _: [(); 80] = [(); core::mem::offset_of!(I915SchedEngine, default_priolist)];
const _: [(); 136] = [(); core::mem::offset_of!(I915SchedEngine, queue)];
const _: [(); 168] = [(); core::mem::offset_of!(I915SchedEngine, destroy)];
const _: [(); 208] = [(); core::mem::offset_of!(I915SchedEngine, schedule)];

/// C tag alias used by `i915_priolist_types.h` consumers.
pub type i915_priolist = I915Priolist;
