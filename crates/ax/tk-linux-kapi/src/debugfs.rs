// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Debugfs fallbacks for disabled subsystems.
//! Upstream reference: `include/linux/debugfs.h`.

#![allow(unsafe_code)]

use core::ffi::{c_char, c_void};

#[inline(always)]
fn err_ptr_nodev() -> *mut c_void {
    (-(crate::config::ENODEV as isize)) as *mut c_void
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_lookup()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_lookup(
    _name: *const c_char,
    _parent: *mut c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_file()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_file(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _data: *mut c_void,
    _fops: *const c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_file_aux()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_file_aux(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _data: *mut c_void,
    _aux: *mut c_void,
    _fops: *const c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_file_unsafe()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_file_unsafe(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _data: *mut c_void,
    _fops: *const c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_file_size()
// !CONFIG_DEBUG_FS fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_file_size(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _data: *mut c_void,
    _fops: *const c_void,
    _file_size: i64,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_dir()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_dir(
    _name: *const c_char,
    _parent: *mut c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_symlink()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_symlink(
    _name: *const c_char,
    _parent: *mut c_void,
    _dest: *const c_char,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_automount()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_automount(
    _name: *const c_char,
    _parent: *mut c_void,
    _f: *mut c_void,
    _data: *mut c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_blob()
// !CONFIG_DEBUG_FS fallback returns ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_blob(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _blob: *mut c_void,
) -> *mut c_void {
    err_ptr_nodev()
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_remove()
// !CONFIG_DEBUG_FS fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_remove(_dentry: *mut c_void) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_remove_recursive()
// !CONFIG_DEBUG_FS fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_remove_recursive(_dentry: *mut c_void) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_lookup_and_remove()
// !CONFIG_DEBUG_FS fallback is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_lookup_and_remove(
    _name: *const c_char,
    _parent: *mut c_void,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_initialized()
// !CONFIG_DEBUG_FS fallback returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_initialized() -> bool {
    false
}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_u8()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_u8(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u8,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_u16()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_u16(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u16,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_u32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_u32(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u32,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_u64()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_u64(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u64,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_ulong()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_ulong(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut usize,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_x8()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_x8(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u8,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_x16()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_x16(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u16,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_x32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_x32(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u32,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_x64()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_x64(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u64,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_size_t()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_size_t(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut usize,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_atomic_t()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_atomic_t(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut c_void,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_bool()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_bool(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut bool,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_str()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_str(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut *mut c_char,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_regset32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_regset32(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _regset: *mut c_void,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_u32_array()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_u32_array(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _array: *mut c_void,
) {}

// upstream: include/linux/debugfs.h !CONFIG_DEBUG_FS debugfs_create_devm_seqfile()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_devm_seqfile(
    _dev: *mut c_void,
    _name: *const c_char,
    _parent: *mut c_void,
    _read_fn: *mut c_void,
) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debugfs_fallbacks() {
        let err_expected = (-(crate::config::ENODEV as isize)) as *mut c_void;
        assert_eq!(
            unsafe { debugfs_lookup(core::ptr::null(), core::ptr::null_mut()) },
            err_expected
        );
        assert_eq!(
            unsafe {
                debugfs_create_file(
                    core::ptr::null(),
                    0,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                )
            },
            err_expected
        );
        assert_eq!(
            unsafe {
                debugfs_create_file_aux(
                    core::ptr::null(),
                    0,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                )
            },
            err_expected
        );
        assert_eq!(
            unsafe {
                debugfs_create_file_unsafe(
                    core::ptr::null(),
                    0,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                )
            },
            err_expected
        );
        unsafe {
            debugfs_create_file_size(
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null(),
                0,
            )
        };
        assert_eq!(
            unsafe { debugfs_create_dir(core::ptr::null(), core::ptr::null_mut()) },
            err_expected
        );
        assert_eq!(
            unsafe {
                debugfs_create_symlink(core::ptr::null(), core::ptr::null_mut(), core::ptr::null())
            },
            err_expected
        );
        assert_eq!(
            unsafe {
                debugfs_create_automount(
                    core::ptr::null(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            err_expected
        );
        assert_eq!(
            unsafe {
                debugfs_create_blob(
                    core::ptr::null(),
                    0,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            err_expected
        );
        unsafe { debugfs_remove(core::ptr::null_mut()) };
        unsafe { debugfs_remove_recursive(core::ptr::null_mut()) };
        unsafe { debugfs_lookup_and_remove(core::ptr::null(), core::ptr::null_mut()) };
        assert!(!unsafe { debugfs_initialized() });

        // Typed helpers
        let mut u8_val = 0u8;
        unsafe { debugfs_create_u8(core::ptr::null(), 0, core::ptr::null_mut(), &mut u8_val) };
        let mut u16_val = 0u16;
        unsafe { debugfs_create_u16(core::ptr::null(), 0, core::ptr::null_mut(), &mut u16_val) };
        let mut u32_val = 0u32;
        unsafe { debugfs_create_u32(core::ptr::null(), 0, core::ptr::null_mut(), &mut u32_val) };
        let mut u64_val = 0u64;
        unsafe { debugfs_create_u64(core::ptr::null(), 0, core::ptr::null_mut(), &mut u64_val) };
        let mut ulong_val = 0usize;
        unsafe {
            debugfs_create_ulong(core::ptr::null(), 0, core::ptr::null_mut(), &mut ulong_val)
        };
        let mut x8_val = 0u8;
        unsafe { debugfs_create_x8(core::ptr::null(), 0, core::ptr::null_mut(), &mut x8_val) };
        let mut x16_val = 0u16;
        unsafe { debugfs_create_x16(core::ptr::null(), 0, core::ptr::null_mut(), &mut x16_val) };
        let mut x32_val = 0u32;
        unsafe { debugfs_create_x32(core::ptr::null(), 0, core::ptr::null_mut(), &mut x32_val) };
        let mut x64_val = 0u64;
        unsafe { debugfs_create_x64(core::ptr::null(), 0, core::ptr::null_mut(), &mut x64_val) };
        let mut size_val = 0usize;
        unsafe {
            debugfs_create_size_t(core::ptr::null(), 0, core::ptr::null_mut(), &mut size_val)
        };
        unsafe {
            debugfs_create_atomic_t(
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        let mut bool_val = false;
        unsafe {
            debugfs_create_bool(core::ptr::null(), 0, core::ptr::null_mut(), &mut bool_val)
        };
        let mut str_val = core::ptr::null_mut();
        unsafe { debugfs_create_str(core::ptr::null(), 0, core::ptr::null_mut(), &mut str_val) };
        unsafe {
            debugfs_create_regset32(
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        unsafe {
            debugfs_create_u32_array(
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        unsafe {
            debugfs_create_devm_seqfile(
                core::ptr::null_mut(),
                core::ptr::null(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
    }
}
