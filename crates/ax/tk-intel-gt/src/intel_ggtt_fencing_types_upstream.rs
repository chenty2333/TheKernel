// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
//
//! Linux v7.2.3 type, constant and declaration transcription of
//! `drivers/gpu/drm/i915/gt/intel_ggtt_fencing.h`.
//!
//! The source header has an explicit SPDX MIT grant. It owns
//! `I915FenceReg`; pointers to GGTT, VMA, GEM, GT and scatterlist records use
//! their existing owner modules. Integration must switch the current opaque
//! `I915FenceReg` placeholder and `I915Vma.fence` pointer to this owner rather
//! than define another fence-record layout.

use core::{
    ffi::c_ulong,
    mem::{align_of, offset_of, size_of},
};

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_vma_types_upstream::I915Vma,
    intel_context_types_upstream::I915Active,
    intel_context_upstream::SgTable,
    intel_engine_cs_upstream::{AtomicT, ListHead},
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::I915Ggtt,
};

/// `I965_FENCE_PAGE` (`intel_ggtt_fencing.h`).
pub const I965_FENCE_PAGE: c_ulong = 4096;

/// `struct i915_fence_reg` (`intel_ggtt_fencing.h`).
#[repr(C)]
pub struct I915FenceReg {
    /// LRU/reservation-list link.
    pub link: ListHead,
    pub ggtt: *mut I915Ggtt,
    pub vma: *mut I915Vma,
    pub pin_count: AtomicT,
    pub active: I915Active,
    pub id: i32,
    /// True when the associated fence register's tiling state needs rewriting.
    pub dirty: bool,
    pub start: u32,
    pub size: u32,
    pub tiling: u32,
    pub stride: u32,
}

// x86_64 Linux v7.2.3 target. `I915Active` is the canonical record from
// `intel_context_types_upstream`; its selected configuration is
// LOCKDEP=n, PREEMPT_RT=n and CONFIG_DRM_I915_SW_FENCE_CHECK_DAG=n.
const _: [(); 216] = [(); size_of::<I915FenceReg>()];
const _: [(); 8] = [(); align_of::<I915FenceReg>()];
const _: [(); 0] = [(); offset_of!(I915FenceReg, link)];
const _: [(); 16] = [(); offset_of!(I915FenceReg, ggtt)];
const _: [(); 24] = [(); offset_of!(I915FenceReg, vma)];
const _: [(); 32] = [(); offset_of!(I915FenceReg, pin_count)];
const _: [(); 40] = [(); offset_of!(I915FenceReg, active)];
const _: [(); 192] = [(); offset_of!(I915FenceReg, id)];
const _: [(); 196] = [(); offset_of!(I915FenceReg, dirty)];
const _: [(); 200] = [(); offset_of!(I915FenceReg, start)];
const _: [(); 204] = [(); offset_of!(I915FenceReg, size)];
const _: [(); 208] = [(); offset_of!(I915FenceReg, tiling)];
const _: [(); 212] = [(); offset_of!(I915FenceReg, stride)];

// C header prototypes. Their implementations are translated in
// `intel_ggtt_fencing_upstream.rs`; this separate owner module records the
// original C ABI declarations for integration callers.
unsafe extern "C" {
    pub fn i915_reserve_fence(ggtt: *mut I915Ggtt) -> *mut I915FenceReg;
    pub fn i915_unreserve_fence(fence: *mut I915FenceReg);
    pub fn intel_ggtt_restore_fences(ggtt: *mut I915Ggtt);
    pub fn i915_gem_object_do_bit_17_swizzle(obj: *mut DrmI915GemObject, pages: *mut SgTable);
    pub fn i915_gem_object_save_bit_17_swizzle(obj: *mut DrmI915GemObject, pages: *mut SgTable);
    pub fn intel_ggtt_init_fences(ggtt: *mut I915Ggtt);
    pub fn intel_ggtt_fini_fences(ggtt: *mut I915Ggtt);
    pub fn intel_gt_init_swizzling(gt: *mut IntelGt);
}
