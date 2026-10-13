// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! B7 C-ABI exports for the ADL-P display and fbdev implementations.

#![allow(non_snake_case, dead_code, unsafe_code)]

pub use crate::{
    intel_display_upstream::{
        intel_init_display_hooks, intel_initial_commit, intel_mode_valid, intel_setup_outputs,
    },
    intel_fbdev_upstream::{
        intel_fbdev_driver_fbdev_probe, intel_fbdev_framebuffer, intel_fbdev_get_map,
        intel_fbdev_setup,
    },
    intel_modeset_setup_upstream::intel_modeset_setup_hw_state,
};
