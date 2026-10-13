// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/clients/drm_client_setup.c` (batch B2c).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_device_types::DrmDevice;
use crate::drm_fourcc_types::DrmFormatInfo;
use crate::drm_mode_object_upstream::{drm_core_check_feature, DRIVER_MODESET};

// Linux's CONFIG_DRM_CLIENT_DEFAULT is "fbdev" in this x86_64 kernel build;
// CONFIG_DRM_FBDEV_EMULATION and CONFIG_FB are enabled. There is no module
// parameter backend in the portable crate, so this is the configured client.
const DRM_CLIENT_DEFAULT_FBDEV: bool = true;

// upstream: clients/drm_client_setup.c drm_client_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_setup(dev: *mut DrmDevice, format: *const DrmFormatInfo) {
    if dev.is_null() || !unsafe { drm_core_check_feature(dev, DRIVER_MODESET) } {
        return;
    }
    if DRM_CLIENT_DEFAULT_FBDEV {
        let ret = unsafe { crate::drm_fbdev_client_upstream::drm_fbdev_client_setup(dev, format) };
        if ret != 0 {
            tk_linux_kapi::print::drm_log_at(
                tk_linux_kapi::print::DrmLogLevel::Warn,
                "drm",
                file!(),
                line!(),
                &alloc::format!("Failed to set up DRM client; error {ret}"),
            );
        }
    }
}

// upstream: clients/drm_client_setup.c drm_client_setup_with_fourcc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_setup_with_fourcc(dev: *mut DrmDevice, fourcc: u32) {
    let format = crate::drm_fourcc_upstream::drm_format_info(fourcc);
    unsafe { drm_client_setup(dev, format) };
}

// upstream: clients/drm_client_setup.c drm_client_setup_with_color_mode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_setup_with_color_mode(dev: *mut DrmDevice, color_mode: u32) {
    if dev.is_null() {
        return;
    }
    let fourcc = unsafe { crate::drm_fourcc_upstream::drm_driver_color_mode_format(dev, color_mode) };
    unsafe { drm_client_setup_with_fourcc(dev, fourcc) };
}
