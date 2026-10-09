// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Compile-time `i915_trace.h` inline dispatch for this target.
//!
//! The Linux 7.2.3 target configuration leaves
//! CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS unset. In that branch the trace-event
//! call sites have no runtime side effects; these typed functions represent
//! that configuration-selected header interface only.

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    intel_context_types_upstream::IntelContext,
    linux_i915_private::DrmI915Private,
    linux_config::CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS,
};
use core::ffi::c_ulong;

const _: () = assert!(!CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS);

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
pub fn trace_i915_gem_object_fault(_obj: *mut DrmI915GemObject, _page: c_ulong, _gtt: bool, _write: bool) {}
#[inline]
pub fn trace_i915_gem_object_clflush(_obj: *mut DrmI915GemObject) {}
