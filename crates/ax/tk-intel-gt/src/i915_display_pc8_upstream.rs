// SPDX-License-Identifier: MIT
// Copyright 2025, Intel Corporation.
// Source: drivers/gpu/drm/i915/i915_display_pc8.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ptr;

use tk_drm_upstream::drm_device_types::DrmDevice;

use crate::{
    intel_parent_upstream::IntelDisplayPc8Interface,
    intel_uncore_types_upstream::{FORCEWAKE_ALL, IntelUncore},
    intel_uncore_upstream::{intel_uncore_forcewake_get, intel_uncore_forcewake_put},
    linux::i915::to_i915,
};

// upstream: i915_display_pc8.c i915_display_pc8_block()
unsafe extern "C" fn i915_display_pc8_block(drm: *mut DrmDevice) {
    unsafe {
        let i915 = to_i915(drm.cast());
        let uncore = ptr::addr_of_mut!((*i915).uncore).cast::<IntelUncore>();

        // To prevent PC8 state, just enable force_wake.
        intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL);
    }
}

// upstream: i915_display_pc8.c i915_display_pc8_unblock()
unsafe extern "C" fn i915_display_pc8_unblock(drm: *mut DrmDevice) {
    unsafe {
        let i915 = to_i915(drm.cast());
        let uncore = ptr::addr_of_mut!((*i915).uncore).cast::<IntelUncore>();

        intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL);
    }
}

/// i915-owned PC8 blocking callbacks exported to the display parent.
#[unsafe(no_mangle)]
pub static i915_display_pc8_interface: IntelDisplayPc8Interface = IntelDisplayPc8Interface {
    block: Some(i915_display_pc8_block),
    unblock: Some(i915_display_pc8_unblock),
};
