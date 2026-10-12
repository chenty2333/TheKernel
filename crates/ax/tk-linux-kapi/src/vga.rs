// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! VGA arbiter and vga_switcheroo fallbacks for disabled subsystems.
//! Upstream reference: `include/linux/vgaarb.h`, `include/linux/vga_switcheroo.h`.

#![allow(unsafe_code)]

use core::ffi::{c_int, c_uint, c_void};

/// `VGA_SWITCHEROO_ON` from `include/linux/vga_switcheroo.h`.
pub const VGA_SWITCHEROO_ON: c_int = 1;

// upstream: include/linux/vgaarb.h !CONFIG_VGA_ARB vga_set_legacy_decoding()
// !CONFIG_VGA_ARB fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_set_legacy_decoding(_pdev: *mut c_void, _decodes: c_uint) {}

// upstream: include/linux/vgaarb.h !CONFIG_VGA_ARB vga_get()
// !CONFIG_VGA_ARB fallback returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_get(
    _pdev: *mut c_void,
    _rsrc: c_uint,
    _interruptible: c_int,
) -> c_int {
    0
}

// upstream: include/linux/vgaarb.h !CONFIG_VGA_ARB vga_get_uninterruptible()
// !CONFIG_VGA_ARB fallback returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_get_uninterruptible(_pdev: *mut c_void, _rsrc: c_uint) -> c_int {
    0
}

// upstream: include/linux/vgaarb.h !CONFIG_VGA_ARB vga_get_interruptible()
// !CONFIG_VGA_ARB fallback returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_get_interruptible(_pdev: *mut c_void, _rsrc: c_uint) -> c_int {
    0
}

// upstream: include/linux/vgaarb.h !CONFIG_VGA_ARB vga_put()
// !CONFIG_VGA_ARB fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_put(_pdev: *mut c_void, _rsrc: c_uint) {}

// upstream: include/linux/vgaarb.h !CONFIG_VGA_ARB vga_client_register()
// !CONFIG_VGA_ARB fallback returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_client_register(
    _pdev: *mut c_void,
    _set_decode: *mut c_void,
) -> c_int {
    0
}

// upstream: include/linux/vgaarb.h !CONFIG_VGA_ARB vga_client_unregister()
// !CONFIG_VGA_ARB fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_client_unregister(_pdev: *mut c_void) {}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_register_client()
// !CONFIG_VGA_SWITCHEROO fallback returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_register_client(
    _dev: *mut c_void,
    _ops: *const c_void,
    _driver_power_control: bool,
) -> c_int {
    0
}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_register_audio_client()
// !CONFIG_VGA_SWITCHEROO fallback returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_register_audio_client(
    _pdev: *mut c_void,
    _ops: *const c_void,
    _id: c_int,
) -> c_int {
    0
}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_unregister_client()
// !CONFIG_VGA_SWITCHEROO fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_unregister_client(_dev: *mut c_void) {}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_handler_flags()
// !CONFIG_VGA_SWITCHEROO fallback returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_handler_flags() -> c_uint {
    0
}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_lock_ddc()
// !CONFIG_VGA_SWITCHEROO fallback returns -ENODEV.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_lock_ddc(_pdev: *mut c_void) -> c_int {
    -crate::config::ENODEV
}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_unlock_ddc()
// !CONFIG_VGA_SWITCHEROO fallback returns -ENODEV.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_unlock_ddc(_pdev: *mut c_void) -> c_int {
    -crate::config::ENODEV
}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_get_client_state()
// !CONFIG_VGA_SWITCHEROO fallback returns VGA_SWITCHEROO_ON (1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_get_client_state(_dev: *mut c_void) -> c_int {
    VGA_SWITCHEROO_ON
}

// upstream: include/linux/vga_switcheroo.h !CONFIG_VGA_SWITCHEROO vga_switcheroo_client_probe_defer()
// !CONFIG_VGA_SWITCHEROO fallback returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_client_probe_defer(_pdev: *mut c_void) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vga_and_switcheroo_fallbacks() {
        unsafe { vga_set_legacy_decoding(core::ptr::null_mut(), 0) };
        assert_eq!(unsafe { vga_get(core::ptr::null_mut(), 0, 0) }, 0);
        assert_eq!(unsafe { vga_get_uninterruptible(core::ptr::null_mut(), 0) }, 0);
        assert_eq!(unsafe { vga_get_interruptible(core::ptr::null_mut(), 0) }, 0);
        unsafe { vga_put(core::ptr::null_mut(), 0) };
        assert_eq!(
            unsafe { vga_client_register(core::ptr::null_mut(), core::ptr::null_mut()) },
            0
        );
        unsafe { vga_client_unregister(core::ptr::null_mut()) };

        assert_eq!(
            unsafe {
                vga_switcheroo_register_client(core::ptr::null_mut(), core::ptr::null(), false)
            },
            0
        );
        assert_eq!(
            unsafe {
                vga_switcheroo_register_audio_client(core::ptr::null_mut(), core::ptr::null(), 0)
            },
            0
        );
        unsafe { vga_switcheroo_unregister_client(core::ptr::null_mut()) };
        assert_eq!(unsafe { vga_switcheroo_handler_flags() }, 0);
        assert_eq!(
            unsafe { vga_switcheroo_lock_ddc(core::ptr::null_mut()) },
            -crate::config::ENODEV
        );
        assert_eq!(
            unsafe { vga_switcheroo_unlock_ddc(core::ptr::null_mut()) },
            -crate::config::ENODEV
        );
        assert_eq!(
            unsafe { vga_switcheroo_get_client_state(core::ptr::null_mut()) },
            VGA_SWITCHEROO_ON
        );
        assert!(!unsafe { vga_switcheroo_client_probe_defer(core::ptr::null_mut()) });
    }
}
