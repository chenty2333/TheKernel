// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Compile-time `i915_trace.h` inline dispatch for this target.
//!
//! The Linux 7.2.3 target configuration leaves
//! CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS unset. In that branch the trace-event
//! call sites have no runtime side effects; these typed functions represent
//! that configuration-selected header interface only.

use crate::{
    intel_context_types_upstream::IntelContext, linux_config::CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS,
};

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
