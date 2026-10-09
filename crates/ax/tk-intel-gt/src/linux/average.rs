// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Target-bound expansion of Linux `DECLARE_EWMA` instances used by i915.
//!
//! The arithmetic follows the header's fixed-point EWMA contract. The source
//! defines `runtime` as precision 3/weight 8 and `_engine_latency` as
//! precision 6/weight 4; `unsigned long` is 64-bit in the selected x86_64 ABI.

#![allow(unsafe_code)]

use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    intel_context_types_upstream::EwmaRuntime, intel_engine_types_upstream::EwmaEngineLatency,
};

#[inline]
unsafe fn load(value: *const u64) -> u64 {
    unsafe { AtomicU64::from_ptr(value.cast_mut()).load(Ordering::Relaxed) }
}

#[inline]
unsafe fn store(value: *mut u64, new: u64) {
    unsafe { AtomicU64::from_ptr(value).store(new, Ordering::Relaxed) }
}

#[inline]
fn ewma_add(internal: u64, value: u64, precision: u32, weight_log2: u32) -> u64 {
    if internal == 0 {
        value.wrapping_shl(precision)
    } else {
        (internal
            .wrapping_shl(weight_log2)
            .wrapping_sub(internal)
            .wrapping_add(value.wrapping_shl(precision)))
            >> weight_log2
    }
}

#[inline]
pub unsafe fn ewma_runtime_init(avg: *mut EwmaRuntime) {
    unsafe { store(core::ptr::addr_of_mut!((*avg).internal), 0) }
}

#[inline]
pub unsafe fn ewma_runtime_read(avg: *const EwmaRuntime) -> u64 {
    unsafe { load(core::ptr::addr_of!((*avg).internal)) >> 3 }
}

#[inline]
pub unsafe fn ewma_runtime_add(avg: *mut EwmaRuntime, value: u64) {
    let slot = unsafe { core::ptr::addr_of_mut!((*avg).internal) };
    let old = unsafe { load(slot) };
    unsafe { store(slot, ewma_add(old, value, 3, 3)) };
}

#[inline]
pub unsafe fn ewma__engine_latency_init(avg: *mut EwmaEngineLatency) {
    unsafe { store(core::ptr::addr_of_mut!((*avg).internal), 0) }
}

#[inline]
pub unsafe fn ewma__engine_latency_read(avg: *const EwmaEngineLatency) -> u64 {
    unsafe { load(core::ptr::addr_of!((*avg).internal)) >> 6 }
}

#[inline]
pub unsafe fn ewma__engine_latency_add(avg: *mut EwmaEngineLatency, value: u64) {
    let slot = unsafe { core::ptr::addr_of_mut!((*avg).internal) };
    let old = unsafe { load(slot) };
    unsafe { store(slot, ewma_add(old, value, 6, 2)) };
}
