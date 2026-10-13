// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B5 (DDI, DPLL, trans, TC, DKL).
//! Replaced by intel_ddi_upstream.rs, intel_dpll_upstream.rs,
//! intel_dkl_phy_upstream.rs, intel_tc_upstream.rs (B5).

#![allow(non_snake_case, dead_code, unsafe_code)]

pub use crate::{
    intel_dpll_mgr_upstream::{
        icl_set_active_port_dpll, icl_tc_port_to_pll_id, intel_dpll_compare_hw_state,
        intel_dpll_compute, intel_dpll_crtc_put, intel_dpll_disable, intel_dpll_dump_hw_state,
        intel_dpll_enable, intel_dpll_get_freq, intel_dpll_get_hw_state, intel_dpll_init,
        intel_dpll_is_combophy, intel_dpll_readout_hw_state, intel_dpll_release,
        intel_dpll_reserve, intel_dpll_sanitize_state, intel_dpll_swap_state,
        intel_dpll_update_active, intel_dpll_update_ref_clks, intel_get_dpll_by_id,
    },
    intel_dpll_upstream::{
        intel_dpll_crtc_compute_clock, intel_dpll_crtc_get_dpll, intel_dpll_init_clock_hook,
    },
};

pub use crate::intel_combo_phy_upstream::{intel_combo_phy_init, intel_combo_phy_uninit};
pub use crate::intel_dkl_phy_upstream::intel_dkl_phy_init;

