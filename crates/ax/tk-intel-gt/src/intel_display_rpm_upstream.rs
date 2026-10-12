// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_rpm.c

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_parent_upstream::intel_display_parent;

// upstream: intel_display_rpm.c intel_display_rpm_get_raw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_get_raw(display: *mut IntelDisplay) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).get_raw {
            return f((*display).drm);
        }
    }
    core::ptr::null_mut()
}

// upstream: intel_display_rpm.c intel_display_rpm_put_raw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_put_raw(
    display: *mut IntelDisplay,
    wakeref: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).put_raw {
            f((*display).drm, wakeref);
        }
    }
}

// upstream: intel_display_rpm.c intel_display_rpm_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_get(display: *mut IntelDisplay) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).get {
            return f((*display).drm);
        }
    }
    core::ptr::null_mut()
}

// upstream: intel_display_rpm.c intel_display_rpm_get_if_in_use()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_get_if_in_use(
    display: *mut IntelDisplay,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).get_if_in_use {
            return f((*display).drm);
        }
    }
    core::ptr::null_mut()
}

// upstream: intel_display_rpm.c intel_display_rpm_get_noresume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_get_noresume(
    display: *mut IntelDisplay,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).get_noresume {
            return f((*display).drm);
        }
    }
    core::ptr::null_mut()
}

// upstream: intel_display_rpm.c intel_display_rpm_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_put(
    display: *mut IntelDisplay,
    wakeref: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).put {
            f((*display).drm, wakeref);
        }
    }
}

// upstream: intel_display_rpm.c intel_display_rpm_put_unchecked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_put_unchecked(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).put_unchecked {
            f((*display).drm);
        }
    }
}

// upstream: intel_display_rpm.c intel_display_rpm_suspended()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_suspended(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).suspended {
            return f((*display).drm);
        }
    }
    false
}

// upstream: intel_display_rpm.c assert_display_rpm_held()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn assert_display_rpm_held(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).assert_held {
            f((*display).drm);
        }
    }
}

// upstream: intel_display_rpm.c intel_display_rpm_assert_block()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_assert_block(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).assert_block {
            f((*display).drm);
        }
    }
}

// upstream: intel_display_rpm.c intel_display_rpm_assert_unblock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rpm_assert_unblock(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rpm.is_null() {
        if let Some(f) = (*(*p).rpm).assert_unblock {
            f((*display).drm);
        }
    }
}
