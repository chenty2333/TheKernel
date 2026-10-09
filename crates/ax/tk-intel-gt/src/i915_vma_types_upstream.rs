// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
//
//! Linux v7.2.3 type/constants transcription of
//! `drivers/gpu/drm/i915/i915_vma_types.h`.
//!
//! The source carries an explicit SPDX MIT license. This module owns the i915
//! `I915Vma` record and VMA flag constants. DRM/Linux/GEM records are imported
//! from their existing owner bindings, and the resource pointer uses the
//! dedicated `i915_vma_resource.h` owner rather than a duplicate layout.
//!
//! The `DrmMmNode`, `I915GttView`, and several other imported framework
//! records currently have opaque or partial owning bindings. Their enclosing
//! sizes/alignment are represented by those owners; their internal members are
//! not recreated here. This matters to inline helpers in `i915_vma.h`, which
//! need a future full `drm_mm.h` / `i915_gtt_view_types.h` binding.

use core::ffi::{c_ulong, c_void};

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_vma_resource_types_upstream::{I915PageSizes, I915VmaResource},
    intel_context_types_upstream::I915Active,
    intel_context_upstream::{DrmMmNode, I915GttView, I915MmapOffset, I915VmaOps, SgTable},
    intel_engine_cs_upstream::{AtomicT, ListHead, RbNode},
    intel_ggtt_fencing_types_upstream::I915FenceReg,
    intel_gtt_api_upstream::I915AddressSpace,
};

// Flag constants from i915_vma_types.h:190-221, kept in source order.
pub const I915_VMA_PIN_MASK: i32 = 0x3ff;
pub const I915_VMA_OVERFLOW: i32 = 0x200;

pub const I915_VMA_GLOBAL_BIND_BIT: i32 = 10;
pub const I915_VMA_LOCAL_BIND_BIT: i32 = 11;

pub const I915_VMA_GLOBAL_BIND: i32 = 1 << I915_VMA_GLOBAL_BIND_BIT;
pub const I915_VMA_LOCAL_BIND: i32 = 1 << I915_VMA_LOCAL_BIND_BIT;
pub const I915_VMA_BIND_MASK: i32 = I915_VMA_GLOBAL_BIND | I915_VMA_LOCAL_BIND;

pub const I915_VMA_ERROR_BIT: i32 = 12;
pub const I915_VMA_ERROR: i32 = 1 << I915_VMA_ERROR_BIT;

pub const I915_VMA_GGTT_BIT: i32 = 13;
pub const I915_VMA_CAN_FENCE_BIT: i32 = 14;
pub const I915_VMA_USERFAULT_BIT: i32 = 15;
pub const I915_VMA_GGTT_WRITE_BIT: i32 = 16;

pub const I915_VMA_GGTT: i32 = 1 << I915_VMA_GGTT_BIT;
pub const I915_VMA_CAN_FENCE: i32 = 1 << I915_VMA_CAN_FENCE_BIT;
pub const I915_VMA_USERFAULT: i32 = 1 << I915_VMA_USERFAULT_BIT;
pub const I915_VMA_GGTT_WRITE: i32 = 1 << I915_VMA_GGTT_WRITE_BIT;

pub const I915_VMA_SCANOUT_BIT: i32 = 17;
pub const I915_VMA_SCANOUT: i32 = 1 << I915_VMA_SCANOUT_BIT;

pub const I915_VMA_PAGES_BIAS: i32 = 24;
pub const I915_VMA_PAGES_ACTIVE: c_ulong = ((1 as c_ulong) << I915_VMA_PAGES_BIAS) | 1;

// The source's `assert_i915_gem_gtt_types()` at i915_vma_types.h:100-123
// validates packed rotation/partial/remapped metadata owned by the included
// `i915_gtt_view_types.h`. That owner currently exposes only the enclosing
// I915GttView discriminant and storage size, so the branch-specific BUILD_BUG_ON
// checks and enum uniqueness switch cannot be represented without duplicating
// the adjacent header's record definitions here.

/// `struct i915_vma` (`i915_vma_types.h:135-253`).
///
/// The VMA's effective lifetime is bounded by its GEM object; this declaration
/// preserves the exact C field order. Configuration-sensitive framework
/// records are imported from their existing owner modules.
#[repr(C)]
pub struct I915Vma {
    pub node: DrmMmNode,

    pub vm: *mut I915AddressSpace,
    pub ops: *const I915VmaOps,

    pub obj: *mut DrmI915GemObject,

    pub pages: *mut SgTable,
    pub iomap: *mut c_void,
    pub private: *mut c_void,

    pub fence: *mut I915FenceReg,

    pub size: u64,
    pub page_sizes: I915PageSizes,

    /// mmap offset associated with fencing for this VMA.
    pub mmo: *mut I915MmapOffset,

    pub guard: u32,
    pub fence_size: u32,
    pub fence_alignment: u32,
    pub display_alignment: u32,

    pub open_count: AtomicT,
    /// Includes the pin count and the bit flags declared above.
    pub flags: AtomicT,
    pub active: I915Active,
    pub pages_count: AtomicT,

    pub vm_ddestroy: bool,

    pub gtt_view: I915GttView,

    pub vm_link: ListHead,
    /// Link in the object's VMA list.
    pub obj_link: ListHead,
    pub obj_node: RbNode,

    pub evict_link: ListHead,
    pub closed_link: ListHead,

    /// Async resource; protected by the VM mutex.
    pub resource: *mut I915VmaResource,
}

// x86_64 Linux v7.2.3 target layout with CONFIG_DRM_I915_SELFTEST=n,
// CONFIG_DRM_I915_SW_FENCE_CHECK_DAG=n, LOCKDEP/PREEMPT_RT/DEBUG_LOCK_ALLOC/
// DEBUG_MUTEXES/DEBUG_SPINLOCK unset, and MUTEX_SPIN_ON_OWNER enabled. These
// checks also enforce the selected imported framework record sizes.
const _: [(); 168] = [(); core::mem::size_of::<DrmMmNode>()];
const _: [(); 8] = [(); core::mem::align_of::<DrmMmNode>()];
const _: [(); 56] = [(); core::mem::size_of::<I915GttView>()];
const _: [(); 4] = [(); core::mem::align_of::<I915GttView>()];
const _: [(); 8] = [(); core::mem::size_of::<I915PageSizes>()];
const _: [(); 152] = [(); core::mem::size_of::<I915Active>()];
const _: [(); 584] = [(); core::mem::size_of::<I915Vma>()];
const _: [(); 8] = [(); core::mem::align_of::<I915Vma>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915Vma, node)];
const _: [(); 168] = [(); core::mem::offset_of!(I915Vma, vm)];
const _: [(); 176] = [(); core::mem::offset_of!(I915Vma, ops)];
const _: [(); 184] = [(); core::mem::offset_of!(I915Vma, obj)];
const _: [(); 192] = [(); core::mem::offset_of!(I915Vma, pages)];
const _: [(); 200] = [(); core::mem::offset_of!(I915Vma, iomap)];
const _: [(); 208] = [(); core::mem::offset_of!(I915Vma, private)];
const _: [(); 216] = [(); core::mem::offset_of!(I915Vma, fence)];
const _: [(); 224] = [(); core::mem::offset_of!(I915Vma, size)];
const _: [(); 232] = [(); core::mem::offset_of!(I915Vma, page_sizes)];
const _: [(); 240] = [(); core::mem::offset_of!(I915Vma, mmo)];
const _: [(); 248] = [(); core::mem::offset_of!(I915Vma, guard)];
const _: [(); 252] = [(); core::mem::offset_of!(I915Vma, fence_size)];
const _: [(); 256] = [(); core::mem::offset_of!(I915Vma, fence_alignment)];
const _: [(); 260] = [(); core::mem::offset_of!(I915Vma, display_alignment)];
const _: [(); 264] = [(); core::mem::offset_of!(I915Vma, open_count)];
const _: [(); 268] = [(); core::mem::offset_of!(I915Vma, flags)];
const _: [(); 272] = [(); core::mem::offset_of!(I915Vma, active)];
const _: [(); 424] = [(); core::mem::offset_of!(I915Vma, pages_count)];
const _: [(); 428] = [(); core::mem::offset_of!(I915Vma, vm_ddestroy)];
const _: [(); 432] = [(); core::mem::offset_of!(I915Vma, gtt_view)];
const _: [(); 488] = [(); core::mem::offset_of!(I915Vma, vm_link)];
const _: [(); 504] = [(); core::mem::offset_of!(I915Vma, obj_link)];
const _: [(); 520] = [(); core::mem::offset_of!(I915Vma, obj_node)];
const _: [(); 544] = [(); core::mem::offset_of!(I915Vma, evict_link)];
const _: [(); 560] = [(); core::mem::offset_of!(I915Vma, closed_link)];
const _: [(); 576] = [(); core::mem::offset_of!(I915Vma, resource)];
