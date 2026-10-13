// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B3c (BIOS, DMC, DMC-WL, DRAM, MCHBAR).
//! Replaced by intel_bios_upstream.rs, intel_dmc_upstream.rs,
//! intel_dmc_wl_upstream.rs (B3c).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_workarounds_types_upstream::I915RegT;

// upstream: intel_dmc_wl.c intel_dmc_wl_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_get(_display: *mut IntelDisplay, _reg: I915RegT) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_put(_display: *mut IntelDisplay, _reg: I915RegT) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_init(_display: *mut IntelDisplay) {}

// upstream: intel_bios.c intel_bios_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_init(_display: *mut IntelDisplay) {}

// upstream: intel_dmc.c intel_dmc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_init(_display: *mut IntelDisplay) {}

// upstream: intel_dmc.c intel_dmc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_fini(_display: *mut IntelDisplay) {}

// upstream: intel_bios.c intel_bios_driver_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_driver_remove(_display: *mut IntelDisplay) {}
