// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B7 (modeset core, HW readout, fbdev).
//! Replaced by intel_display_upstream.rs, intel_modeset_setup_upstream.rs,
//! intel_fbdev_upstream.rs (B7).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;
use tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx;

pub use crate::intel_display_upstream::{
    intel_mode_valid, intel_init_display_hooks, intel_setup_outputs, intel_initial_commit,
};

// upstream: intel_modeset_setup.c intel_modeset_setup_hw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_setup_hw_state(
    _display: *mut IntelDisplay,
    _ctx: *mut DrmModesetAcquireCtx,
) {}

// upstream: intel_fbdev.c intel_fbdev_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbdev_setup(_display: *mut IntelDisplay) {}
