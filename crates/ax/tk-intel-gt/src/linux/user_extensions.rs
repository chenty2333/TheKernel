// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Checked i915 user-extension chain traversal over native MM.

#![allow(unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    i915_gem_context_upstream::I915UserExtension,
    linux::mm_native::copy_from_user,
    linux_config::{E2BIG, EFAULT, EINVAL},
};

type I915UserExtensionFn =
    Option<unsafe extern "C" fn(*mut I915UserExtension, *mut c_void) -> c_int>;

/// MIT i915 extension-chain helper: check reserved fields, dispatch only a
/// registered name, and bound traversal to Linux's 512-node recursion guard.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_user_extensions(
    mut extension: *mut I915UserExtension,
    functions: *const I915UserExtensionFn,
    count: u32,
    data: *mut c_void,
) -> c_int {
    let mut remaining = 512u32;
    while !extension.is_null() {
        if remaining == 0 {
            return -E2BIG;
        }
        remaining -= 1;
        let mut flags = 0u32;
        let mut reserved = [0u32; 4];
        let mut name = 0u32;
        let mut next = 0u64;
        unsafe {
            if copy_from_user(
                (&mut flags as *mut u32).cast(),
                ptr::addr_of!((*extension).flags).cast(),
                size_of::<u32>(),
            ) != 0
                || copy_from_user(
                    reserved.as_mut_ptr().cast(),
                    ptr::addr_of!((*extension).rsvd).cast(),
                    size_of::<[u32; 4]>(),
                ) != 0
                || copy_from_user(
                    (&mut name as *mut u32).cast(),
                    ptr::addr_of!((*extension).name).cast(),
                    size_of::<u32>(),
                ) != 0
            {
                return -EFAULT;
            }
        }
        if flags != 0 || reserved.iter().any(|value| *value != 0) {
            return -EINVAL;
        }
        if name >= count || functions.is_null() {
            return -EINVAL;
        }
        let Some(callback) = (unsafe { *functions.add(name as usize) }) else {
            return -EINVAL;
        };
        let result = unsafe { callback(extension, data) };
        if result != 0 {
            return result;
        }
        if unsafe {
            copy_from_user(
                (&mut next as *mut u64).cast(),
                ptr::addr_of!((*extension).next_extension).cast(),
                size_of::<u64>(),
            )
        } != 0
            || next > usize::MAX as u64
        {
            return -EFAULT;
        }
        extension = next as usize as *mut I915UserExtension;
    }
    0
}
