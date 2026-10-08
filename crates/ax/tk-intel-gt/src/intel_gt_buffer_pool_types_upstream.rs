// SPDX-License-Identifier: MIT
// Copyright © 2014-2018 Intel Corporation.
// Source-order bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_gt_buffer_pool_types.h.

use core::{ffi::c_ulong, mem::ManuallyDrop};

use crate::{
    i915_request_types_upstream::DrmI915GemObject,
    intel_context_types_upstream::I915Active,
    intel_context_upstream::RcuHead,
    intel_engine_cs_upstream::{DelayedWork, ListHead, Spinlock},
};

#[repr(C)]
pub struct IntelGtBufferPool {
    pub lock: Spinlock,
    pub cache_list: [ListHead; 4],
    pub work: DelayedWork,
}

/// C `enum i915_map_type` has an unsigned 32-bit representation on the target
/// compiler because its force-map enumerators use bit 31.
pub type I915MapType = u32;

#[repr(C)]
pub union IntelGtBufferPoolNodeUnion {
    pub pool: *mut IntelGtBufferPool,
    pub free: *mut IntelGtBufferPoolNode,
    pub rcu: ManuallyDrop<RcuHead>,
}

#[repr(C)]
pub struct IntelGtBufferPoolNode {
    pub active: I915Active,
    pub obj: *mut DrmI915GemObject,
    pub link: ListHead,
    pub rcu_or_pool: IntelGtBufferPoolNodeUnion,
    pub age: c_ulong,
    pub r#type: I915MapType,
    pub pinned: u32,
}

// x86_64 target: CONFIG_LOCKDEP=n, CONFIG_DEBUG_MUTEXES=n,
// CONFIG_PREEMPT_RT=n. The active/list/RCU/work members are imported canonical
// owner layouts; this record pins their exact embedding.
const _: [(); 160] = [(); core::mem::size_of::<IntelGtBufferPool>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGtBufferPool>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGtBufferPool, lock)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGtBufferPool, cache_list)];
const _: [(); 72] = [(); core::mem::offset_of!(IntelGtBufferPool, work)];

const _: [(); 16] = [(); core::mem::size_of::<IntelGtBufferPoolNodeUnion>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGtBufferPoolNodeUnion>()];
const _: [(); 208] = [(); core::mem::size_of::<IntelGtBufferPoolNode>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGtBufferPoolNode>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGtBufferPoolNode, active)];
const _: [(); 152] = [(); core::mem::offset_of!(IntelGtBufferPoolNode, obj)];
const _: [(); 160] = [(); core::mem::offset_of!(IntelGtBufferPoolNode, link)];
const _: [(); 176] = [(); core::mem::offset_of!(IntelGtBufferPoolNode, rcu_or_pool)];
const _: [(); 192] = [(); core::mem::offset_of!(IntelGtBufferPoolNode, age)];
const _: [(); 200] = [(); core::mem::offset_of!(IntelGtBufferPoolNode, r#type)];
const _: [(); 204] = [(); core::mem::offset_of!(IntelGtBufferPoolNode, pinned)];
