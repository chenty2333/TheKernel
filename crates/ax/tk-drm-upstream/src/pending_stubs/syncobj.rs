// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY definitions for syncobj ioctls (drivers/gpu/drm/drm_syncobj.c).

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;

macro_rules! syncobj_stub {
    ($name:ident) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            _dev: *mut DrmDevice,
            _data: *mut c_void,
            _file_priv: *mut DrmFile,
        ) -> i32 {
            -tk_linux_kapi::config::EOPNOTSUPP
        }
    };
}

syncobj_stub!(drm_syncobj_create_ioctl);
syncobj_stub!(drm_syncobj_destroy_ioctl);
syncobj_stub!(drm_syncobj_handle_to_fd_ioctl);
syncobj_stub!(drm_syncobj_fd_to_handle_ioctl);
syncobj_stub!(drm_syncobj_transfer_ioctl);
syncobj_stub!(drm_syncobj_wait_ioctl);
syncobj_stub!(drm_syncobj_timeline_wait_ioctl);
syncobj_stub!(drm_syncobj_eventfd_ioctl);
syncobj_stub!(drm_syncobj_reset_ioctl);
syncobj_stub!(drm_syncobj_signal_ioctl);
syncobj_stub!(drm_syncobj_timeline_signal_ioctl);
syncobj_stub!(drm_syncobj_query_ioctl);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_open(_file_priv: *mut DrmFile) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_release(_file_priv: *mut DrmFile) {}

