// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! ACPI and DMI fallbacks for disabled subsystems.
//! Upstream reference: `include/acpi/video.h`, `include/linux/acpi.h`, `include/linux/dmi.h`.

#![allow(unsafe_code)]

use core::ffi::{c_char, c_int, c_void};

/// `AE_NOT_FOUND` from `include/acpi/acexcep.h`.
pub const AE_NOT_FOUND: u32 = 5;

// upstream: include/acpi/video.h !CONFIG_ACPI_VIDEO acpi_video_register()
// !CONFIG_ACPI_VIDEO fallback returns -ENODEV when video extension is disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_video_register() -> c_int {
    -crate::config::ENODEV
}

// upstream: include/acpi/video.h !CONFIG_ACPI_VIDEO acpi_video_unregister()
// !CONFIG_ACPI_VIDEO fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_video_unregister() {}

// upstream: include/acpi/video.h !CONFIG_ACPI_VIDEO acpi_video_register_backlight()
// !CONFIG_ACPI_VIDEO fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_video_register_backlight() {}

// upstream: include/acpi/video.h !CONFIG_ACPI_VIDEO acpi_video_get_backlight_type()
// !CONFIG_ACPI_VIDEO fallback returns acpi_backlight_vendor (2).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_video_get_backlight_type() -> c_int {
    2
}

// upstream: include/acpi/video.h __acpi_video_get_backlight_type()
// When ACPI video is disabled, reports native (3) if requested, else vendor (2).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __acpi_video_get_backlight_type(
    native: bool,
    auto_detect: *mut bool,
) -> c_int {
    if !auto_detect.is_null() {
        unsafe { *auto_detect = false };
    }
    if native {
        3 // acpi_backlight_native
    } else {
        2 // acpi_backlight_vendor
    }
}

// upstream: include/acpi/video.h !CONFIG_ACPI_VIDEO acpi_video_backlight_use_native()
// !CONFIG_ACPI_VIDEO fallback returns true to favor driver native backlight.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_video_backlight_use_native() -> bool {
    true
}

// upstream: include/acpi/video.h !CONFIG_ACPI_VIDEO acpi_video_handles_brightness_key_presses()
// !CONFIG_ACPI_VIDEO fallback returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_video_handles_brightness_key_presses() -> bool {
    false
}

// upstream: include/linux/acpi.h !CONFIG_ACPI acpi_check_dsm()
// !CONFIG_ACPI fallback returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_check_dsm(
    _handle: *mut c_void,
    _guid: *const u8,
    _rev: u64,
    _funcs: u64,
) -> bool {
    false
}

// upstream: include/linux/acpi.h !CONFIG_ACPI acpi_evaluate_dsm()
// !CONFIG_ACPI fallback returns NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_evaluate_dsm(
    _handle: *mut c_void,
    _guid: *const u8,
    _rev: u64,
    _func: u64,
    _argv4: *mut c_void,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: include/linux/acpi.h !CONFIG_ACPI acpi_evaluate_dsm_typed()
// !CONFIG_ACPI fallback returns NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_evaluate_dsm_typed(
    _handle: *mut c_void,
    _guid: *const u8,
    _rev: u64,
    _func: u64,
    _argv4: *mut c_void,
    _type: u32,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: include/acpi/acpixf.h acpi_get_handle()
// When ACPI subsystem is unconfigured, return AE_NOT_FOUND (5).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_get_handle(
    _parent: *mut c_void,
    _pathname: *const c_char,
    _ret_handle: *mut *mut c_void,
) -> u32 {
    AE_NOT_FOUND
}

// upstream: include/acpi/acpixf.h acpi_get_name()
// When ACPI subsystem is unconfigured, return AE_NOT_FOUND (5).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_get_name(
    _handle: *mut c_void,
    _type: u32,
    _ret_buffer: *mut c_void,
) -> u32 {
    AE_NOT_FOUND
}

// upstream: include/acpi/acpi_bus.h acpi_find_child_device()
// When ACPI is disabled, no child device exists: return NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_find_child_device(
    _parent: *mut c_void,
    _addr: u64,
    _check_children: bool,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: include/linux/acpi.h acpi_dev_get_resources()
// When ACPI is disabled, return -ENODEV.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_dev_get_resources(
    _adev: *mut c_void,
    _list: *mut c_void,
    _preproc: *mut c_void,
    _preproc_data: *mut c_void,
) -> c_int {
    -crate::config::ENODEV
}

// upstream: include/linux/acpi.h acpi_dev_free_resource_list()
// When ACPI is disabled, free is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_dev_free_resource_list(_list: *mut c_void) {}

// upstream: include/linux/acpi.h !CONFIG_ACPI acpi_fwnode_handle()
// !CONFIG_ACPI fallback returns NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_fwnode_handle(_adev: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: include/linux/acpi.h !CONFIG_ACPI acpi_device_handle()
// !CONFIG_ACPI fallback returns NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_device_handle(_adev: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: include/linux/dmi.h !CONFIG_DMI dmi_check_system()
// !CONFIG_DMI fallback returns 0 (no match).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dmi_check_system(_list: *const c_void) -> c_int {
    0
}

// upstream: include/linux/dmi.h !CONFIG_DMI dmi_first_match()
// !CONFIG_DMI fallback returns NULL (no match).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dmi_first_match(_list: *const c_void) -> *const c_void {
    core::ptr::null()
}

// upstream: include/linux/i2c.h !CONFIG_ACPI i2c_acpi_get_i2c_resource()
// !CONFIG_ACPI fallback returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i2c_acpi_get_i2c_resource(
    _ares: *mut c_void,
    _i2c: *mut *mut c_void,
) -> bool {
    false
}

// upstream: include/linux/i2c.h !CONFIG_ACPI i2c_acpi_find_adapter_by_handle()
// !CONFIG_ACPI fallback returns NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i2c_acpi_find_adapter_by_handle(_handle: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acpi_and_dmi_fallbacks() {
        assert_eq!(unsafe { acpi_video_register() }, -crate::config::ENODEV);
        unsafe { acpi_video_unregister() };
        unsafe { acpi_video_register_backlight() };
        assert_eq!(unsafe { acpi_video_get_backlight_type() }, 2);
        assert_eq!(unsafe { __acpi_video_get_backlight_type(true, core::ptr::null_mut()) }, 3);
        assert_eq!(unsafe { __acpi_video_get_backlight_type(false, core::ptr::null_mut()) }, 2);
        assert!(unsafe { acpi_video_backlight_use_native() });
        assert!(!unsafe { acpi_video_handles_brightness_key_presses() });

        assert!(!unsafe {
            acpi_check_dsm(core::ptr::null_mut(), core::ptr::null(), 0, 0)
        });
        assert!(unsafe {
            acpi_evaluate_dsm(core::ptr::null_mut(), core::ptr::null(), 0, 0, core::ptr::null_mut())
        }
        .is_null());
        assert!(unsafe {
            acpi_evaluate_dsm_typed(
                core::ptr::null_mut(),
                core::ptr::null(),
                0,
                0,
                core::ptr::null_mut(),
                0,
            )
        }
        .is_null());

        assert_eq!(
            unsafe { acpi_get_handle(core::ptr::null_mut(), core::ptr::null(), core::ptr::null_mut()) },
            AE_NOT_FOUND
        );
        assert_eq!(
            unsafe { acpi_get_name(core::ptr::null_mut(), 0, core::ptr::null_mut()) },
            AE_NOT_FOUND
        );
        assert!(unsafe { acpi_find_child_device(core::ptr::null_mut(), 0, false) }.is_null());
        assert_eq!(
            unsafe {
                acpi_dev_get_resources(
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            -crate::config::ENODEV
        );
        unsafe { acpi_dev_free_resource_list(core::ptr::null_mut()) };
        assert!(unsafe { acpi_fwnode_handle(core::ptr::null_mut()) }.is_null());
        assert!(unsafe { acpi_device_handle(core::ptr::null_mut()) }.is_null());

        assert_eq!(unsafe { dmi_check_system(core::ptr::null()) }, 0);
        assert!(unsafe { dmi_first_match(core::ptr::null()) }.is_null());

        assert!(!unsafe {
            i2c_acpi_get_i2c_resource(core::ptr::null_mut(), core::ptr::null_mut())
        });
        assert!(unsafe { i2c_acpi_find_adapter_by_handle(core::ptr::null_mut()) }.is_null());
    }
}
