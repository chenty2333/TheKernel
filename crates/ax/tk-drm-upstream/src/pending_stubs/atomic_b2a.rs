// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_atomic_helper.c (batch B2b). Each symbol is defined once
//! here; delete the entry when its owner lands.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_device_types::DrmDevice;
use crate::drm_crtc_types::DrmCrtcState;
use crate::drm_plane_types::DrmPlaneState;

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
pub unsafe extern "C" fn drm_atomic_helper_suspend(_dev: *mut DrmDevice) -> *mut DrmAtomicCommit {
    core::ptr::null_mut()
}
