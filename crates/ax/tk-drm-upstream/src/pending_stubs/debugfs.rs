// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_debugfs_crc.c / drm_debugfs.c (debugfs batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_debugfs_upstream.rs when the debugfs batch lands; debugfs is off in the shipped config.
//!
//! Symbols: drm_debugfs_crtc_add, drm_debugfs_crtc_remove.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_crtc_types::DrmCrtc;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::{DrmFile, DrmMinor};

// upstream: drivers/gpu/drm/drm_debugfs_crc.c / drm_debugfs.c drm_debugfs_crtc_add()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_crtc_add(_crtc: *mut DrmCrtc) {}

// upstream: drivers/gpu/drm/drm_debugfs.c drm_debugfs_crtc_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_crtc_remove(_crtc: *mut DrmCrtc) {}

// upstream: drivers/gpu/drm/drm_debugfs_crc.c drm_crtc_add_crc_entry()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_add_crc_entry(
    _crtc: *mut DrmCrtc,
    _has_frame: bool,
    _frame: u32,
    _crcs: *const u32,
) -> i32 {
    -tk_linux_kapi::config::EINVAL
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_dev_init(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_dev_register(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_dev_fini(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_register(_minor: *mut DrmMinor, _minor_id: u32) -> i32 {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_unregister(_minor: *mut DrmMinor) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn accel_debugfs_register(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_clients_add(_file: *mut DrmFile) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_clients_remove(_file: *mut DrmFile) {}
