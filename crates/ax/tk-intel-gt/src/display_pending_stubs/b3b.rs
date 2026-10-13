// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B3b (power domains, PMDemand, PCH, HTI).
//! Replaced by intel_display_power_upstream.rs, intel_pch_upstream.rs,
//! intel_pmdemand_upstream.rs, intel_hti_upstream.rs (B3b).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

pub use crate::intel_pch_upstream::intel_pch_detect;

// upstream: intel_display_power.c intel_display_power_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_pmdemand.c intel_pmdemand_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_init_early(_display: *mut IntelDisplay) {}

// upstream: intel_display_power.c intel_display_power_init_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_init_hw(_display: *mut IntelDisplay) {}

// upstream: intel_pmdemand.c intel_pmdemand_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pmdemand_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_display_power.c intel_display_power_driver_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_driver_remove(_display: *mut IntelDisplay) {}

pub use crate::intel_hti_upstream::intel_hti_init;

