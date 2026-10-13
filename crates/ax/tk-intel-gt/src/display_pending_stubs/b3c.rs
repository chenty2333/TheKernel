// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for DMC, DMC-WL, DRAM and MCHBAR callees.
//! BIOS functions are implemented in `intel_bios_upstream.rs`.

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_workarounds_types_upstream::I915RegT;

pub use crate::intel_bios_upstream::{intel_bios_driver_remove, intel_bios_init};

// upstream: intel_dmc_wl.c intel_dmc_wl_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_get(_display: *mut IntelDisplay, _reg: I915RegT) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_put(_display: *mut IntelDisplay, _reg: I915RegT) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_init(_display: *mut IntelDisplay) {}

// upstream: intel_dmc.c intel_dmc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_init(_display: *mut IntelDisplay) {}

// upstream: intel_dmc.c intel_dmc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_fini(_display: *mut IntelDisplay) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_get_noreg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_get_noreg(_display: *mut IntelDisplay) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_put_noreg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_put_noreg(_display: *mut IntelDisplay) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_disable(_display: *mut IntelDisplay) {}

// upstream: intel_dmc.c intel_dmc_has_payload()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_has_payload(_display: *mut IntelDisplay) -> bool {
    false
}

// upstream: intel_dmc.c intel_dmc_update_dc6_allowed_count()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_update_dc6_allowed_count(
    _display: *mut IntelDisplay,
    _allowed: bool,
) {}

// upstream: intel_dmc.c intel_dmc_load_program()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_load_program(_display: *mut IntelDisplay) {}

// upstream: intel_dmc.c intel_dmc_disable_program()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_disable_program(_display: *mut IntelDisplay) {}

// upstream: intel_dram.c intel_dram_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dram_info(
    display: *mut IntelDisplay,
) -> *const crate::intel_dram_types_upstream::DramInfo {
    if display.is_null() {
        core::ptr::null()
    } else {
        (*display).dram.info as *const _
    }
}
