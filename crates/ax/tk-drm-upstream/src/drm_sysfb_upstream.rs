// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/sysfb/drm_sysfb.c` (v7.2.3): validated
//! integer parsing for firmware-provided framebuffer parameters and the
//! pixel-format to DRM fourcc lookup.
//!
//! Deviation: `drm_sysfb_get_validated_int()` takes the parameter name as a
//! Rust `&str` instead of a C string; the EXPORT_SYMBOL is not reproduced.

#![allow(non_snake_case, dead_code, unsafe_code)]

use alloc::format;

use tk_linux_kapi::{config::EINVAL, print::DrmLogLevel};

use crate::{
    drm_device_types::DrmDevice,
    drm_fourcc_types::DrmFormatInfo,
    drm_fourcc_upstream::drm_format_info,
    drm_print_upstream::drm_dev_printk,
    drm_sysfb_helper_types::{DrmSysfbFormat, PixelFormat, pixel_format_equal},
};

// upstream: drm_print.c drm_warn(): routed to the TheKernel log, device-less.
pub(crate) fn drm_warn(dev: *const DrmDevice, text: &str) {
    drm_dev_printk(dev.cast(), DrmLogLevel::Warn, text);
}

// upstream: drm_sysfb.c drm_sysfb_get_validated_int()
pub fn drm_sysfb_get_validated_int(dev: *const DrmDevice, name: &str, value: u64, max: u32) -> i32 {
    if value > core::cmp::min(max as u64, i32::MAX as u64) {
        drm_warn(
            dev,
            &format!("{} of {} exceeds maximum of {}\n", name, value, max),
        );
        return -EINVAL;
    }
    value as i32
}

// upstream: drm_sysfb.c drm_sysfb_get_validated_int0()
pub fn drm_sysfb_get_validated_int0(
    dev: *const DrmDevice,
    name: &str,
    value: u64,
    max: u32,
) -> i32 {
    if value == 0 {
        drm_warn(dev, &format!("{} of 0 not allowed\n", name));
        return -EINVAL;
    }
    drm_sysfb_get_validated_int(dev, name, value, max)
}

// upstream: drm_sysfb.c drm_sysfb_get_format()
/// Returns the DRM format info for the first entry of `formats` whose pixel
/// description equals `pixel`, or null (after a warning) if none matches.
pub fn drm_sysfb_get_format(
    dev: *const DrmDevice,
    formats: &[DrmSysfbFormat],
    pixel: &PixelFormat,
) -> *const DrmFormatInfo {
    let mut format: *const DrmFormatInfo = core::ptr::null();
    for f in formats {
        if pixel_format_equal(pixel, &f.pixel) {
            // SAFETY: drm_format_info() is a pure table lookup.
            format = unsafe { drm_format_info(f.fourcc) };
            break;
        }
    }
    if format.is_null() {
        drm_warn(dev, "No compatible color format found\n");
    }
    format
}
