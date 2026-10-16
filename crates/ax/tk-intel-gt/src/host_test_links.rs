// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Host-test link stubs for C-ABI symbols that the kernel image defines
//! (`kernel/src/file/event.rs`, `kernel/src/drm/intel/upstream_gt.rs`). Compiled
//! only under `cfg(test)`; no host unit test reaches these bodies.

#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_int, c_void};

// upstream: drivers/gpu/drm/i915/display/intel_overlay.c intel_overlay_available()
#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_overlay_available(_display: *mut c_void) -> bool {
    false
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_eventfd_signal(_ctx: *mut c_void, _count: u64) -> c_int {
    panic!("host test: thekernel_eventfd_signal is kernel-provided")
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_eventfd_ctx_put(_ctx: *mut c_void) {
    panic!("host test: thekernel_eventfd_ctx_put is kernel-provided")
}
