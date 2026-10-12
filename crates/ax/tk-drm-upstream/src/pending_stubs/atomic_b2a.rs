// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_atomic.c / drm_atomic_helper.c (batch B2a). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_atomic_upstream.rs and drm_atomic_helper_upstream.rs (B2a).
//!
//! Symbols: drm_atomic_get_property, drm_atomic_commit_alloc, drm_atomic_commit_clear, drm_atomic_commit, drm_atomic_connector_commit_dpms, drm_atomic_set_property, __drm_atomic_commit_free, drm_atomic_add_affected_connectors, drm_atomic_set_crtc_for_connector, drm_atomic_set_crtc_for_plane, drm_atomic_set_fb_for_plane, drm_atomic_set_mode_for_crtc, drm_atomic_get_crtc_state, drm_atomic_helper_check_plane_state, drm_atomic_helper_suspend.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_connector_types::{DrmConnector, DrmConnectorState};
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_mode_object_types::DrmModeObject;
use crate::drm_property_types::DrmProperty;
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState};
use crate::drm_modes_types::DrmDisplayMode;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_plane_types::DrmPlaneState;

// upstream: include/drm/drm_atomic.h (B2a)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_property(
    _obj: *mut DrmModeObject,
    _property: *mut DrmProperty,
    _val: *mut u64,
) -> i32 {
    -tk_linux_kapi::config::EINVAL
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_alloc(_dev: *mut DrmDevice) -> *mut DrmAtomicCommit {
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_clear(_state: *mut DrmAtomicCommit) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit(_state: *mut DrmAtomicCommit) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_connector_commit_dpms(
    _state: *mut DrmAtomicCommit,
    _connector: *mut DrmConnector,
    _mode: i32,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_property(
    _state: *mut DrmAtomicCommit,
    _file_priv: *mut DrmFile,
    _obj: *mut DrmModeObject,
    _prop: *mut DrmProperty,
    _prop_value: u64,
    _async_update: bool,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_commit_free(_kref: *mut tk_linux_kapi::types::Kref) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_add_affected_connectors(_state: *mut DrmAtomicCommit, _crtc: *mut DrmCrtc) -> i32 { -tk_linux_kapi::config::EOPNOTSUPP }

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_crtc_for_connector(_conn_state: *mut DrmConnectorState, _crtc: *mut DrmCrtc) -> i32 { -tk_linux_kapi::config::EOPNOTSUPP }

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_crtc_for_plane(_plane_state: *mut DrmPlaneState, _crtc: *mut DrmCrtc) -> i32 { -tk_linux_kapi::config::EOPNOTSUPP }

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_fb_for_plane(_plane_state: *mut DrmPlaneState, _fb: *mut DrmFramebuffer) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_set_mode_for_crtc(_crtc_state: *mut DrmCrtcState, _mode: *const DrmDisplayMode) -> i32 { -tk_linux_kapi::config::EOPNOTSUPP }

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_crtc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_crtc_state(_state: *mut DrmAtomicCommit, _crtc: *mut DrmCrtc) -> *mut DrmCrtcState {
    tk_linux_kapi::config::ERR_PTR::<DrmCrtcState>(-tk_linux_kapi::config::EOPNOTSUPP)
}

// upstream: drivers/gpu/drm/drm_atomic_helper.c drm_atomic_helper_check_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_check_plane_state(
    _plane_state: *mut DrmPlaneState,
    _crtc_state: *const DrmCrtcState,
    _min_scale: i32,
    _max_scale: i32,
    _can_position: bool,
    _can_update_disabled: bool,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_suspend(_dev: *mut DrmDevice) -> *mut DrmAtomicCommit { core::ptr::null_mut() }

// upstream: drivers/gpu/drm/drm_atomic.c drm_atomic_get_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_get_plane_state(
    _state: *mut DrmAtomicCommit,
    _plane: *mut crate::drm_plane_types::DrmPlane,
) -> *mut DrmPlaneState {
    tk_linux_kapi::config::ERR_PTR::<DrmPlaneState>(-tk_linux_kapi::config::EOPNOTSUPP)
}

// upstream: drivers/gpu/drm/drm_atomic_uapi.c drm_mode_atomic_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_atomic_ioctl(
    _dev: *mut DrmDevice,
    _data: *mut core::ffi::c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}
