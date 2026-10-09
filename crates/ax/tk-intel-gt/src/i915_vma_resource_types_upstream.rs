// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
//
//! Source-order type bindings from Linux v7.2.3
//! `drivers/gpu/drm/i915/i915_vma_resource.h`.
//!
//! The target configuration has CONFIG_DRM_I915_CAPTURE_ERROR enabled and
//! CONFIG_DRM_I915_SW_FENCE_CHECK_DAG disabled. Kernel framework records are
//! imported from their existing owner bindings; this module defines only the
//! i915-owned records from this header.

use crate::{
    intel_context_types_upstream::{I915SwFence, IntelWakerefT},
    intel_context_upstream::{DmaFence, I915AddressSpace, I915VmaOps, RefcountT, SgTable},
    intel_engine_cs_upstream::{RbNode, Spinlock, WorkStruct},
    linux::gem_memory::IntelMemoryRegion,
};

// Pointer-only dependency declared by i915_gem_ttm.h.
#[repr(C)]
pub struct I915RefctSgt {
    _opaque: [u8; 0],
}

/// `struct i915_page_sizes`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I915PageSizes {
    /// Physical page-size mask from the scatter-gather table.
    pub phys: u32,
    /// Allowed GTT page-size mask.
    pub sg: u32,
}

/// `struct i915_vma_bindinfo`.
#[repr(C)]
pub struct I915VmaBindinfo {
    pub pages: *mut SgTable,
    pub page_sizes: I915PageSizes,
    pub pages_rsgt: *mut I915RefctSgt,
    /// Storage byte for the adjacent C `bool readonly:1` and `bool lmem:1`.
    /// Bit 0 is readonly; bit 1 is local-memory.
    pub flags: u8,
}

/// `struct i915_vma_resource`.
#[repr(C)]
pub struct I915VmaResource {
    pub unbind_fence: DmaFence,
    pub lock: Spinlock,
    pub hold_count: RefcountT,
    pub work: WorkStruct,
    pub chain: I915SwFence,
    pub rb: RbNode,
    pub __subtree_last: u64,
    pub vm: *mut I915AddressSpace,
    pub wakeref: IntelWakerefT,

    pub bi: I915VmaBindinfo,

    pub mr: *mut IntelMemoryRegion,
    pub ops: *const I915VmaOps,
    pub private: *mut core::ffi::c_void,
    pub start: u64,
    pub node_size: u64,
    pub vma_size: u64,
    pub guard: u32,
    pub page_sizes_gtt: u32,

    pub bound_flags: u32,
    /// Storage byte for C bitfields `allocated`, `immediate_unbind`,
    /// `needs_wakeref`, and `skip_pte_rewrite` (bits 0 through 3).
    pub state_bits: u8,

    pub tlb: *mut u32,
}

const _: [(); 8] = [(); core::mem::size_of::<I915PageSizes>()];
const _: [(); 4] = [(); core::mem::align_of::<I915PageSizes>()];
const _: [(); 32] = [(); core::mem::size_of::<I915VmaBindinfo>()];
const _: [(); 8] = [(); core::mem::align_of::<I915VmaBindinfo>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915VmaBindinfo, pages)];
const _: [(); 8] = [(); core::mem::offset_of!(I915VmaBindinfo, page_sizes)];
const _: [(); 16] = [(); core::mem::offset_of!(I915VmaBindinfo, pages_rsgt)];
const _: [(); 24] = [(); core::mem::offset_of!(I915VmaBindinfo, flags)];

const _: [(); 64] = [(); core::mem::size_of::<DmaFence>()];
const _: [(); 4] = [(); core::mem::size_of::<Spinlock>()];
const _: [(); 4] = [(); core::mem::size_of::<RefcountT>()];
const _: [(); 32] = [(); core::mem::size_of::<WorkStruct>()];
const _: [(); 24] = [(); core::mem::size_of::<RbNode>()];
const _: [(); 40] = [(); core::mem::size_of::<I915SwFence>()];
const _: [(); 8] = [(); core::mem::size_of::<IntelWakerefT>()];

// x86_64 Linux 7.2.3, CONFIG_DRM_I915_CAPTURE_ERROR=y,
// CONFIG_DRM_I915_SW_FENCE_CHECK_DAG=n, LOCKDEP=n, PREEMPT_RT=n.
const _: [(); 296] = [(); core::mem::size_of::<I915VmaResource>()];
const _: [(); 8] = [(); core::mem::align_of::<I915VmaResource>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915VmaResource, unbind_fence)];
const _: [(); 64] = [(); core::mem::offset_of!(I915VmaResource, lock)];
const _: [(); 68] = [(); core::mem::offset_of!(I915VmaResource, hold_count)];
const _: [(); 72] = [(); core::mem::offset_of!(I915VmaResource, work)];
const _: [(); 104] = [(); core::mem::offset_of!(I915VmaResource, chain)];
const _: [(); 144] = [(); core::mem::offset_of!(I915VmaResource, rb)];
const _: [(); 168] = [(); core::mem::offset_of!(I915VmaResource, __subtree_last)];
const _: [(); 176] = [(); core::mem::offset_of!(I915VmaResource, vm)];
const _: [(); 184] = [(); core::mem::offset_of!(I915VmaResource, wakeref)];
const _: [(); 192] = [(); core::mem::offset_of!(I915VmaResource, bi)];
const _: [(); 224] = [(); core::mem::offset_of!(I915VmaResource, mr)];
const _: [(); 232] = [(); core::mem::offset_of!(I915VmaResource, ops)];
const _: [(); 240] = [(); core::mem::offset_of!(I915VmaResource, private)];
const _: [(); 248] = [(); core::mem::offset_of!(I915VmaResource, start)];
const _: [(); 256] = [(); core::mem::offset_of!(I915VmaResource, node_size)];
const _: [(); 264] = [(); core::mem::offset_of!(I915VmaResource, vma_size)];
const _: [(); 272] = [(); core::mem::offset_of!(I915VmaResource, guard)];
const _: [(); 276] = [(); core::mem::offset_of!(I915VmaResource, page_sizes_gtt)];
const _: [(); 280] = [(); core::mem::offset_of!(I915VmaResource, bound_flags)];
const _: [(); 284] = [(); core::mem::offset_of!(I915VmaResource, state_bits)];
const _: [(); 288] = [(); core::mem::offset_of!(I915VmaResource, tlb)];
