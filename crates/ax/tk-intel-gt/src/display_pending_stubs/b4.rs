// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B4 (plane, CRTC, FB, frontbuffer, DPT, DSB, watermarks, color).
//! Replaced by intel_plane_upstream.rs, intel_crtc_upstream.rs,
//! intel_fb_upstream.rs, intel_color_upstream.rs, skl_watermark_upstream.rs (B4).

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use tk_drm_upstream::{
    drm_atomic_types::DrmAtomicCommit, drm_device_types::DrmDevice, drm_file_types::DrmFile,
    drm_fourcc_types::DrmFormatInfo, drm_framebuffer_types::DrmFramebuffer,
    uapi_drm_mode_types::DrmModeFbCmd2,
};

use crate::{
    intel_display_core_types_upstream::IntelDisplay, intel_display_types_upstream::IntelCrtc,
    intel_workarounds_types_upstream::I915RegT,
};
pub use crate::{
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

// upstream: intel_fb.c intel_user_framebuffer_create()
// Degraded stub returns ERR_PTR(-ENODEV) when display FB support is absent, matching Linux drm_mode_addfb2 error handling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_user_framebuffer_create(
    _dev: *mut DrmDevice,
    _filp: *mut DrmFile,
    _info: *mut DrmFormatInfo,
    _user_mode_cmd: *mut DrmModeFbCmd2,
) -> *mut DrmFramebuffer {
    tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENODEV)
}

// upstream: intel_fb.c intel_fb_get_format_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fb_get_format_info(
    _pixel_format: u32,
    _modifier: u64,
) -> *mut DrmFormatInfo {
    core::ptr::null_mut()
}

// upstream: intel_atomic.c intel_atomic_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_check(
    _dev: *mut DrmDevice,
    _state: *mut DrmAtomicCommit,
) -> i32 {
    0
}

// upstream: intel_atomic.c intel_atomic_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_commit(
    _dev: *mut DrmDevice,
    _state: *mut DrmAtomicCommit,
    _nonblock: bool,
) -> i32 {
    0
}

// upstream: intel_atomic.c intel_atomic_state_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_state_alloc(_dev: *mut DrmDevice) -> *mut DrmAtomicCommit {
    core::ptr::null_mut()
}

// upstream: intel_atomic.c intel_atomic_state_clear()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_state_clear(_state: *mut DrmAtomicCommit) {}

// upstream: intel_atomic.c intel_atomic_state_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_state_free(_state: *mut DrmAtomicCommit) {}

// upstream: intel_cursor.c intel_cursor_mode_config_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cursor_mode_config_init(_display: *mut IntelDisplay) {}

// upstream: intel_crtc.c intel_crtc_for_pipe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_for_pipe(
    _display: *mut IntelDisplay,
    _pipe: i32,
) -> *mut IntelCrtc {
    core::ptr::null_mut()
}

// upstream: intel_color.c intel_color_init_hooks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_init_hooks(_display: *mut IntelDisplay) {}

// upstream: intel_color.c intel_color_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_crtc.c intel_crtc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_initial_plane.c intel_initial_plane_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_initial_plane_config(_display: *mut IntelDisplay) {}

// upstream: i9xx_wm.c ilk_wm_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_wm_sanitize(_display: *mut IntelDisplay) {}

// upstream: intel_dsb.c intel_dsb_reg_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_reg_write(_dsb: *mut c_void, _reg: I915RegT, _val: u32) {}
