// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order type translation of Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_ring_types.h.

use crate::{
    i915_vma_types_upstream::I915Vma, intel_context_upstream::Kref,
    intel_engine_cs_upstream::AtomicT,
};

pub const CACHELINE_BYTES: usize = 64;
pub const CACHELINE_DWORDS: usize = CACHELINE_BYTES / core::mem::size_of::<u32>();

/// `struct intel_ring`; the atomic pin count permits the source's concurrent
/// updates while actual pinning is serialized by context ownership.
#[repr(C)]
pub struct IntelRing {
    pub ref_: Kref,
    pub vma: *mut I915Vma,
    pub vaddr: *mut core::ffi::c_void,
    pub pin_count: AtomicT,
    pub head: u32,
    pub tail: u32,
    pub emit: u32,
    pub space: u32,
    pub size: u32,
    pub wrap: u32,
    pub effective_size: u32,
}

const _: [(); 56] = [(); core::mem::size_of::<IntelRing>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelRing>()];
const _: [(); 24] = [(); core::mem::offset_of!(IntelRing, pin_count)];
const _: [(); 28] = [(); core::mem::offset_of!(IntelRing, head)];
const _: [(); 52] = [(); core::mem::offset_of!(IntelRing, effective_size)];
