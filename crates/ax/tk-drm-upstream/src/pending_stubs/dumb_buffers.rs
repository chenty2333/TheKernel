// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY definitions for dumb buffer ioctls (drivers/gpu/drm/drm_dumb_buffers.c).

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;

// upstream: drivers/gpu/drm/drm_dumb_buffers.c drm_mode_create_dumb_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_dumb_ioctl(
    _dev: *mut DrmDevice,
    _data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

// upstream: drivers/gpu/drm/drm_dumb_buffers.c drm_mode_mmap_dumb_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_mmap_dumb_ioctl(
    _dev: *mut DrmDevice,
    _data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}

// upstream: drivers/gpu/drm/drm_dumb_buffers.c drm_mode_destroy_dumb_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_destroy_dumb_ioctl(
    _dev: *mut DrmDevice,
    _data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    -tk_linux_kapi::config::EOPNOTSUPP
}
