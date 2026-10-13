// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B5 (DDI, DPLL, trans, TC, DKL).
//! Replaced by intel_ddi_upstream.rs, intel_dpll_upstream.rs,
//! intel_dkl_phy_upstream.rs, intel_tc_upstream.rs (B5).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

// upstream: intel_dkl_phy.c intel_dkl_phy_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dkl_phy_init(_display: *mut IntelDisplay) {}

// upstream: intel_dpll.c intel_dpll_init_clock_hook()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_init_clock_hook(_display: *mut IntelDisplay) {}

// upstream: intel_dpll.c intel_dpll_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_init(_display: *mut IntelDisplay) {}

// upstream: intel_dpll.c intel_dpll_update_ref_clks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dpll_update_ref_clks(_display: *mut IntelDisplay) {}

// upstream: intel_combo_phy.c intel_combo_phy_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_combo_phy_init(_display: *mut IntelDisplay) {}
