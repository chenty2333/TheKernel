// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Reset records and flag indices from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_reset_types.h`.

use core::ffi::c_ulong;

use crate::{
    intel_context_upstream::WaitQueueHead, intel_engine_cs_upstream::Mutex, linux::srcu::SrcuStruct,
};

/// `struct intel_reset`.
#[repr(C)]
pub struct IntelReset {
    pub flags: c_ulong,
    pub mutex: Mutex,
    pub queue: WaitQueueHead,
    pub backoff_srcu: SrcuStruct,
}

// `I915_RESET_*` and `I915_WEDGED*` are bit positions, not bit masks.
pub const I915_RESET_BACKOFF: u32 = 0;
pub const I915_RESET_ENGINE: u32 = 1;
pub const I915_WEDGED_ON_INIT: u32 = crate::linux_config::BITS_PER_LONG - 3;
pub const I915_WEDGED_ON_FINI: u32 = crate::linux_config::BITS_PER_LONG - 2;
pub const I915_WEDGED: u32 = crate::linux_config::BITS_PER_LONG - 1;

// Target x86_64 Linux 7.2.3; LOCKDEP, DEBUG_MUTEXES and PREEMPT_RT are off.
const _: [(); 8] = [(); core::mem::size_of::<c_ulong>()];
const _: [(); 24] = [(); core::mem::size_of::<Mutex>()];
const _: [(); 24] = [(); core::mem::size_of::<WaitQueueHead>()];
const _: [(); 32] = [(); core::mem::size_of::<SrcuStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelReset>()];
const _: [(); 88] = [(); core::mem::size_of::<IntelReset>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelReset, flags)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelReset, mutex)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelReset, queue)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelReset, backoff_srcu)];
