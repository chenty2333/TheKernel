// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Component framework fallbacks for disabled subsystems.
//! Upstream reference: `include/linux/component.h`.

#![allow(unsafe_code)]

use core::ffi::{c_int, c_void};

// upstream: include/linux/component.h component_add_typed()
// HDA audio component binding is disabled; always report success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn component_add_typed(
    _dev: *mut c_void,
    _ops: *const c_void,
    _subcomponent: c_int,
) -> c_int {
    0
}

// upstream: include/linux/component.h component_add()
// Component binding is disabled; report success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn component_add(_dev: *mut c_void, _ops: *const c_void) -> c_int {
    0
}

// upstream: include/linux/component.h component_del()
// Component unbinding is a no-op when the framework is disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn component_del(_dev: *mut c_void, _ops: *const c_void) {}

// upstream: include/linux/component.h component_bind_all()
// Reports success (nothing to bind).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn component_bind_all(_parent: *mut c_void, _data: *mut c_void) -> c_int {
    0
}

// upstream: include/linux/component.h component_unbind_all()
// No-op when component framework is disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn component_unbind_all(_parent: *mut c_void, _data: *mut c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_component_fallbacks() {
        assert_eq!(
            unsafe { component_add_typed(core::ptr::null_mut(), core::ptr::null(), 0) },
            0
        );
        assert_eq!(unsafe { component_add(core::ptr::null_mut(), core::ptr::null()) }, 0);
        unsafe { component_del(core::ptr::null_mut(), core::ptr::null()) };
        assert_eq!(
            unsafe { component_bind_all(core::ptr::null_mut(), core::ptr::null_mut()) },
            0
        );
        unsafe { component_unbind_all(core::ptr::null_mut(), core::ptr::null_mut()) };
    }
}
