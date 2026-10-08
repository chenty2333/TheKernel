// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
//
//! Linux v7.2.3 `drivers/gpu/drm/i915/gt/intel_timeline_types.h` type binding.
//! The i915 timeline layout is kept distinct from TheKernel's synchronization
//! objects; referenced Linux/i915 support records use their own ABI bindings.

use core::{
    ffi::c_void,
    mem::{align_of, offset_of, size_of},
};

use crate::{
    intel_context_upstream::{I915Active, I915ActiveFence, I915Vma, Kref, RcuHead},
    intel_engine_cs_upstream::{AtomicT, IntelGt, ListHead, Mutex},
};

/// Forward declaration from `intel_timeline_types.h`; the sync map's storage
/// and operations are owned by the separate i915 syncmap implementation.
#[repr(C)]
pub struct I915Syncmap {
    _opaque: [u8; 0],
}

/// Exact field order from Linux v7.2.3 `struct intel_timeline`.
#[repr(C)]
pub struct IntelTimeline {
    pub fence_context: u64,
    pub seqno: u32,
    pub mutex: Mutex,
    pub pin_count: AtomicT,
    pub active_count: AtomicT,
    pub hwsp_map: *mut c_void,
    pub hwsp_seqno: *const u32,
    pub hwsp_ggtt: *mut I915Vma,
    pub hwsp_offset: u32,
    pub has_initial_breadcrumb: bool,
    pub requests: ListHead,
    pub last_request: I915ActiveFence,
    pub active: I915Active,
    pub retire: *mut IntelTimeline,
    pub sync: *mut I915Syncmap,
    pub link: ListHead,
    pub gt: *mut IntelGt,
    pub engine_link: ListHead,
    pub kref: Kref,
    pub rcu: RcuHead,
}

const _: [(); 360] = [(); size_of::<IntelTimeline>()];
const _: [(); 8] = [(); align_of::<IntelTimeline>()];
const _: [(); 0] = [(); offset_of!(IntelTimeline, fence_context)];
const _: [(); 8] = [(); offset_of!(IntelTimeline, seqno)];
const _: [(); 16] = [(); offset_of!(IntelTimeline, mutex)];
const _: [(); 40] = [(); offset_of!(IntelTimeline, pin_count)];
const _: [(); 44] = [(); offset_of!(IntelTimeline, active_count)];
const _: [(); 48] = [(); offset_of!(IntelTimeline, hwsp_map)];
const _: [(); 56] = [(); offset_of!(IntelTimeline, hwsp_seqno)];
const _: [(); 64] = [(); offset_of!(IntelTimeline, hwsp_ggtt)];
const _: [(); 72] = [(); offset_of!(IntelTimeline, hwsp_offset)];
const _: [(); 76] = [(); offset_of!(IntelTimeline, has_initial_breadcrumb)];
const _: [(); 80] = [(); offset_of!(IntelTimeline, requests)];
const _: [(); 96] = [(); offset_of!(IntelTimeline, last_request)];
const _: [(); 128] = [(); offset_of!(IntelTimeline, active)];
const _: [(); 280] = [(); offset_of!(IntelTimeline, retire)];
const _: [(); 288] = [(); offset_of!(IntelTimeline, sync)];
const _: [(); 296] = [(); offset_of!(IntelTimeline, link)];
const _: [(); 312] = [(); offset_of!(IntelTimeline, gt)];
const _: [(); 320] = [(); offset_of!(IntelTimeline, engine_link)];
const _: [(); 336] = [(); offset_of!(IntelTimeline, kref)];
const _: [(); 344] = [(); offset_of!(IntelTimeline, rcu)];
