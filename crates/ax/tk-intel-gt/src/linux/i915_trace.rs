// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Compile-time `i915_trace.h` inline dispatch for this target.
//!
//! The Linux 7.2.3 target configuration leaves
//! CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS unset. In that branch the trace-event
//! call sites have no runtime side effects; these typed functions represent
//! that configuration-selected header interface only.

use core::ffi::{c_ulong, c_void};

use crate::{
    i915_request_types_upstream::I915Request,
    i915_gem_object_types_upstream::DrmI915GemObject, intel_context_types_upstream::IntelContext,
    intel_context_upstream::DmaFence, intel_gtt_api_upstream::I915AddressSpace,
    linux_config::CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS, linux_i915_private::DrmI915Private,
};

const _: () = assert!(!CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS);

/// Linux tracepoint symbol; with low-level tracepoints disabled, calls have
/// no runtime effect, matching the configured Linux trace header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_dma_fence_signaled(_fence: *mut DmaFence) {}

#[inline]
pub fn trace_intel_context_ban(_context: *mut IntelContext) {}
#[inline]
pub fn trace_intel_context_create(_context: *mut IntelContext) {}
#[inline]
pub fn trace_intel_context_do_pin(_context: *mut IntelContext) {}
#[inline]
pub fn trace_intel_context_do_unpin(_context: *mut IntelContext) {}
#[inline]
pub fn trace_intel_context_free(_context: *mut IntelContext) {}

// These additional trace-event wrappers are also compile-time no-ops for the
// configured target, where CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS is disabled.
#[inline]
pub fn trace_intel_context_set_prio(_context: &IntelContext) {}
#[inline]
pub fn trace_intel_context_fence_release(_context: &IntelContext) {}
#[inline]
pub fn trace_intel_context_deregister_done(_context: &IntelContext) {}
#[inline]
pub fn trace_intel_context_sched_done(_context: &IntelContext) {}
#[inline]
pub fn trace_intel_context_reset(_context: &IntelContext) {}
#[inline]
pub fn trace_i915_gem_object_destroy(_object: *mut DrmI915GemObject) {}
#[inline]
pub fn trace_i915_gem_shrink(_i915: *mut DrmI915Private, _target: c_ulong, _shrink: u32) {}

/// Trace delivery is not enabled in this LinuxKPI configuration.
#[inline]
pub fn trace_i915_gem_object_fault(
    _obj: *mut DrmI915GemObject,
    _page: c_ulong,
    _gtt: bool,
    _write: bool,
) {
}
#[inline]
pub fn trace_i915_gem_object_clflush(_obj: *mut DrmI915GemObject) {}
#[inline]
pub fn trace_i915_request_queue(_request: *mut I915Request, _flags: u32) {}
#[inline]
pub fn trace_i915_request_add(_request: *mut I915Request) {}

/// `i915_ppgtt_create` is a trace event from `i915_trace.h`; the configured
/// TheKernel target has no i915 tracepoint sink, so this event compiles away.
#[cfg(feature = "upstream-gt")]
#[inline]
pub fn trace_i915_ppgtt_create(_vm: *mut I915AddressSpace) {}

/// `i915_ppgtt_release` is an optional i915 trace event with no enabled sink in
/// this target; release lifetime and ordering are still implemented by caller.
#[cfg(feature = "upstream-gt")]
#[inline]
pub fn trace_i915_ppgtt_release(_vm: *mut I915AddressSpace) {}

// Tracepoint symbols referenced by the source-order i915 translations. The oracle
// configuration sets CONFIG_TRACEPOINTS, but this kernel registers no probe for
// these events. Linux emits these as `trace_<event>()` calls; with no probe
// registered each call is a static-key check that does nothing, so the bodies
// are empty and the arguments are not evaluated.
// Pointer arguments are ABI-identical to the `*mut` forms in the declaring
// translations, so opaque `c_void` is used where the record types are not
// otherwise needed here.

/// Linux `trace_i915_context_create()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_context_create(_ctx: *mut c_void) {}

/// Linux `trace_i915_context_free()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_context_free(_ctx: *mut c_void) {}

/// Linux `trace_i915_gem_evict()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_gem_evict(
    _vm: *mut c_void,
    _size: u64,
    _alignment: u64,
    _flags: u32,
) {
}

/// Linux `trace_i915_gem_evict_node()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_gem_evict_node(_vm: *mut c_void, _target: *mut c_void, _flags: u32) {}

/// Linux `trace_i915_gem_evict_vm()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_gem_evict_vm(_vm: *mut c_void) {}

/// Linux `trace_i915_gem_object_create()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_gem_object_create(_obj: *mut c_void) {}

/// Linux `trace_i915_gem_object_pread()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_gem_object_pread(_obj: *mut c_void, _offset: u64, _size: u64) {}

/// Linux `trace_i915_gem_object_pwrite()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_gem_object_pwrite(_obj: *mut c_void, _offset: u64, _size: u64) {}

/// Linux `trace_i915_request_execute()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_execute(_rq: *mut c_void) {}

/// Linux `trace_i915_request_guc_submit()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_guc_submit(_rq: *mut c_void) {}

/// Linux `trace_i915_request_in()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_in(_rq: *mut c_void, _port: u32) {}

/// Linux `trace_i915_request_out()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_out(_rq: *mut c_void) {}

/// Linux `trace_i915_request_retire()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_retire(_rq: *mut c_void) {}

/// Linux `trace_i915_request_submit()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_submit(_rq: *mut c_void) {}

/// Linux `trace_i915_request_wait_begin()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_wait_begin(_rq: *mut c_void, _flags: u32) {}

/// Linux `trace_i915_request_wait_end()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_request_wait_end(_rq: *mut c_void) {}

/// Linux `trace_i915_vma_unbind()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_i915_vma_unbind(_vma: *mut c_void) {}

/// Linux `trace_intel_context_register()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_intel_context_register(_ce: *mut c_void) {}

/// Linux `trace_intel_context_deregister()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_intel_context_deregister(_ce: *mut c_void) {}

/// Linux `trace_intel_context_sched_enable()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_intel_context_sched_enable(_ce: *mut c_void) {}

/// Linux `trace_intel_context_sched_disable()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_intel_context_sched_disable(_ce: *const c_void) {}

/// Linux `trace_intel_context_steal_guc_id()`: no probe registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trace_intel_context_steal_guc_id(_ce: *const c_void) {}
