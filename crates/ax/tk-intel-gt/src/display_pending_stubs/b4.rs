// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B4 (plane, CRTC, FB, frontbuffer, DPT, DSB, watermarks, color).
//! Replaced by intel_plane_upstream.rs, intel_crtc_upstream.rs,
//! intel_fb_upstream.rs, intel_color_upstream.rs, skl_watermark_upstream.rs (B4).

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

pub use crate::{
    intel_atomic_upstream::{
        intel_atomic_state_alloc, intel_atomic_state_clear, intel_atomic_state_free,
    },
    intel_crtc_upstream::{intel_crtc_for_pipe, intel_crtc_init},
    intel_cursor_upstream::intel_cursor_mode_config_init,
    intel_display_upstream::{intel_atomic_check, intel_atomic_commit},
    intel_wm_upstream::{
        intel_atomic_update_watermarks, intel_compute_global_watermarks, intel_initial_watermarks,
        intel_optimize_watermarks, intel_print_wm_latency, intel_update_watermarks,
        intel_wm_compute, intel_wm_get_hw_state, intel_wm_init, intel_wm_plane_visible,
        intel_wm_sanitize,
    },
    skl_watermark_upstream::{
        intel_atomic_get_dbuf_state, intel_crtc_can_enable_sagv, intel_dbuf_init,
        intel_dbuf_mbus_post_ddb_update, intel_dbuf_mbus_pre_ddb_update,
        intel_dbuf_mdclk_cdclk_ratio_update, intel_dbuf_num_active_pipes,
        intel_dbuf_num_enabled_slices, intel_dbuf_pmdemand_needs_update,
        intel_dbuf_post_plane_update, intel_dbuf_pre_plane_update,
        intel_dbuf_state_set_mdclk_cdclk_ratio, intel_enabled_dbuf_slices_mask, intel_has_sagv,
        intel_sagv_post_plane_update, intel_sagv_pre_plane_update, intel_wm_state_verify,
        skl_ddb_allocation_overlaps, skl_ddb_dbuf_slice_mask, skl_plane_relative_data_rate,
        skl_plane_trans_wm, skl_plane_wm_level, skl_watermark_ipc_enabled, skl_watermark_ipc_init,
        skl_watermark_ipc_update, skl_watermark_max_latency, skl_wm_crtc_disable_noatomic,
        skl_wm_get_hw_state, skl_wm_init, skl_wm_plane_disable_noatomic, skl_wm_sanitize,
        skl_wm0_prefill_lines, skl_wm0_prefill_lines_worst,
    },
};
use crate::{
    intel_display_core_types_upstream::IntelDisplay, intel_workarounds_types_upstream::I915RegT,
};

// upstream: i9xx_wm.c ilk_wm_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_wm_sanitize(_display: *mut IntelDisplay) {}

// upstream: intel_dsb.c intel_dsb_reg_write()
pub unsafe extern "C" fn intel_dsb_reg_write(dsb: *mut c_void, reg: I915RegT, val: u32) {
    unsafe { crate::intel_dsb_upstream::intel_dsb_reg_write(dsb.cast(), reg, val) };
}
