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

pub use crate::{
    intel_atomic_upstream::{
        intel_atomic_state_alloc, intel_atomic_state_clear, intel_atomic_state_free,
    },
    intel_crtc_upstream::{intel_crtc_for_pipe, intel_crtc_init},
};
use crate::{
    intel_display_core_types_upstream::IntelDisplay, intel_workarounds_types_upstream::I915RegT,
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

// The ADL-P atomic validation and commit callbacks are owned by B7a's
// intel_display.c translation. Fail closed until that complete path is wired;
// returning success here would falsely accept an unchecked state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_check(
    _dev: *mut DrmDevice,
    _state: *mut DrmAtomicCommit,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

// B7a also owns intel_atomic_commit(); never claim a no-op commit succeeded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_commit(
    _dev: *mut DrmDevice,
    _state: *mut DrmAtomicCommit,
    _nonblock: bool,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

// upstream: intel_cursor.c intel_cursor_mode_config_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cursor_mode_config_init(_display: *mut IntelDisplay) {}

// upstream: intel_color.c intel_color_init_hooks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_init_hooks(_display: *mut IntelDisplay) {}

// upstream: intel_color.c intel_color_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_color_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_wm.c intel_wm_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_wm_init(_display: *mut IntelDisplay) {}

// upstream: intel_initial_plane.c intel_initial_plane_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_initial_plane_config(_display: *mut IntelDisplay) {}

// upstream: i9xx_wm.c ilk_wm_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_wm_sanitize(_display: *mut IntelDisplay) {}

// upstream: skl_watermark.c skl_watermark_ipc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_watermark_ipc_init(_display: *mut IntelDisplay) {}

// upstream: skl_watermark.c skl_watermark_ipc_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_watermark_ipc_update(_display: *mut IntelDisplay) {}

// upstream: intel_dsb.c intel_dsb_reg_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsb_reg_write(_dsb: *mut c_void, _reg: I915RegT, _val: u32) {}
