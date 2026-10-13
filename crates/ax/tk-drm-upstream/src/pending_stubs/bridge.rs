// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_bridge.c (display-bridge batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_bridge_upstream.rs when the bridge batch lands.
//!
//! Symbols: drm_bridge_detach.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

// upstream: drm_bridge.c (no owner yet): the bridge chain is always empty here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_bridge_detach(_bridge: *mut c_void) {}

// upstream: drm_writeback.c (no owner yet)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_writeback_cleanup_job(_job: *mut c_void) {}

// upstream: drivers/gpu/drm/drm_writeback.c drm_writeback_set_fb() absent feature fallback
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_writeback_set_fb(_conn_state: *mut c_void, _fb: *mut c_void) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_writeback_get_out_fence(_wb_conn: *mut c_void) -> *mut tk_linux_kapi::types::DmaFence {
    core::ptr::null_mut()
}

// upstream: drm_bridge.c drm_atomic_bridge_chain_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_bridge_chain_check(
    _bridge: *mut c_void,
    _crtc_state: *mut c_void,
    _conn_state: *mut c_void,
) -> i32 {
    0
}

// upstream: drm_bridge.c drm_atomic_bridge_chain_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_bridge_chain_disable(_bridge: *mut c_void, _state: *mut c_void) {}

// upstream: drm_bridge.c drm_atomic_bridge_chain_post_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_bridge_chain_post_disable(_bridge: *mut c_void, _state: *mut c_void) {}

// upstream: drm_bridge.c drm_atomic_bridge_chain_pre_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_bridge_chain_pre_enable(_bridge: *mut c_void, _state: *mut c_void) {}

// upstream: drm_bridge.c drm_atomic_bridge_chain_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_bridge_chain_enable(_bridge: *mut c_void, _state: *mut c_void) {}

// upstream: drm_bridge.c drm_bridge_chain_get_first_bridge()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_bridge_chain_get_first_bridge(_encoder: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: drm_bridge.c drm_bridge_chain_mode_set()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_bridge_chain_mode_set(
    _bridge: *mut c_void,
    _mode: *const c_void,
    _adj_mode: *const c_void,
) {}

// upstream: drm_bridge.c drm_bridge_chain_mode_valid()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_bridge_chain_mode_valid(
    _bridge: *mut c_void,
    _info: *const c_void,
    _mode: *const c_void,
) -> i32 {
    0
}

// upstream: drm_bridge.c drm_bridge_helper_reset_crtc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_bridge_helper_reset_crtc(_crtc: *mut c_void) {}

// upstream: drm_bridge.c drm_bridge_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_bridge_put(_bridge: *mut c_void) {}

// upstream: drm_writeback.c drm_writeback_prepare_job()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_writeback_prepare_job(_job: *mut c_void) -> i32 {
    0
}
