// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Backlight class fallbacks for disabled subsystems.
//! Upstream reference: `include/linux/backlight.h`.

#![allow(unsafe_code)]

use core::ffi::{c_char, c_int, c_void};

#[inline(always)]
fn err_ptr_nodev() -> *mut c_void {
    (-(crate::config::ENODEV as isize)) as *mut c_void
}

// upstream: include/linux/backlight.h backlight_device_register()
// When backlight class is disabled, return ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn backlight_device_register(
    _name: *const c_char,
    _dev: *mut c_void,
    _devdata: *mut c_void,
    _ops: *const c_void,
    _props: *const c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/backlight.h devm_backlight_device_register()
// When backlight class is disabled, return ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devm_backlight_device_register(
    _dev: *mut c_void,
    _name: *const c_char,
    _parent: *mut c_void,
    _devdata: *mut c_void,
    _ops: *const c_void,
    _props: *const c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/backlight.h backlight_device_unregister()
// When backlight class is disabled, unregister is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn backlight_device_unregister(_bd: *mut c_void) {}

// upstream: include/linux/backlight.h devm_backlight_device_unregister()
// When backlight class is disabled, unregister is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devm_backlight_device_unregister(_dev: *mut c_void, _bd: *mut c_void) {}

// upstream: include/linux/backlight.h backlight_device_get_by_name()
// When backlight class is disabled, return NULL (no device found).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn backlight_device_get_by_name(_name: *const c_char) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: include/linux/backlight.h backlight_device_get_by_type()
// When backlight class is disabled, return NULL (no device found).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn backlight_device_get_by_type(_type: c_int) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: include/linux/backlight.h backlight_force_update()
// When backlight class is disabled, force update is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn backlight_force_update(_bd: *mut c_void, _reason: c_int) {}

// upstream: include/linux/backlight.h backlight_enable()
// When backlight class is disabled, enable is a no-op returning 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn backlight_enable(_bd: *mut c_void) -> c_int {
    0
}

// upstream: include/linux/backlight.h backlight_disable()
// When backlight class is disabled, disable is a no-op returning 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn backlight_disable(_bd: *mut c_void) -> c_int {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backlight_fallbacks() {
        let err_expected = (-(crate::config::ENODEV as isize)) as *mut c_void;
        assert_eq!(
            unsafe {
                backlight_device_register(
                    core::ptr::null(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    core::ptr::null(),
                )
            },
            err_expected
        );
        assert_eq!(
            unsafe {
                devm_backlight_device_register(
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    core::ptr::null(),
                )
            },
            err_expected
        );
        unsafe { backlight_device_unregister(core::ptr::null_mut()) };
        unsafe { devm_backlight_device_unregister(core::ptr::null_mut(), core::ptr::null_mut()) };
        assert!(unsafe { backlight_device_get_by_name(core::ptr::null()) }.is_null());
        assert!(unsafe { backlight_device_get_by_type(0) }.is_null());
        unsafe { backlight_force_update(core::ptr::null_mut(), 0) };
        assert_eq!(unsafe { backlight_enable(core::ptr::null_mut()) }, 0);
        assert_eq!(unsafe { backlight_disable(core::ptr::null_mut()) }, 0);
    }
}
