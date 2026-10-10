// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/i915_driver.c and the
// i915_probe_error() macro in drivers/gpu/drm/i915/i915_utils.h (which expands
// to drm_err(&(i915)->drm, ...)). The complete MIT grant is retained in
// ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::ffi::{c_char, c_int, c_long, c_ulong, c_void, VaList};
use core::ffi::CStr;

use crate::{
    linux_i915_private::DrmI915Private,
    linux_print::{drm_log_at, format_message, CFormatArg, DrmLogLevel},
};

/// Owned value for one printf conversion pulled from the C va_list.
type OwnedArg = alloc::boxed::Box<dyn CFormatArg>;

/// Reads the C string referenced by a `%s` argument, mapping NULL to
/// `(null)` as the Linux vsnprintf implementation does.
unsafe fn c_str_arg(ptr: *const c_char) -> alloc::string::String {
    if ptr.is_null() {
        return alloc::string::String::from("(null)");
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Walks a printf format and pulls one correctly typed argument per
/// conversion from the C variadic list, following the C default promotions
/// (`int`, `unsigned int`, `long`, `unsigned long`, pointers, strings).
unsafe fn collect_c_args(format: &[u8], args: &mut VaList<'_>) -> alloc::vec::Vec<OwnedArg> {
    let mut out: alloc::vec::Vec<OwnedArg> = alloc::vec::Vec::new();
    let mut i = 0;
    while i < format.len() {
        if format[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        if i < format.len() && format[i] == b'%' {
            i += 1;
            continue;
        }
        // flags, width, precision
        while i < format.len() && b"-+ #0123456789.*".contains(&format[i]) {
            if format[i] == b'*' {
                // Width or precision taken from the argument list.
                out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_int>() }));
            }
            i += 1;
        }
        // length modifiers
        let mut long_len = false;
        while i < format.len() && b"hlLqjzt".contains(&format[i]) {
            if format[i] == b'l' || format[i] == b'L' || format[i] == b'q' || format[i] == b'j' || format[i] == b'z' || format[i] == b't' {
                long_len = true;
            }
            i += 1;
        }
        if i >= format.len() {
            break;
        }
        let conv = format[i];
        i += 1;
        match conv {
            b'd' | b'i' => {
                if long_len {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_long>() }));
                } else {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_int>() }));
                }
            }
            b'u' | b'x' | b'X' | b'o' => {
                if long_len {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_ulong>() }));
                } else {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<u32>() }));
                }
            }
            b'c' => {
                out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_int>() }));
            }
            b's' => {
                let text = unsafe { c_str_arg(args.next_arg::<*const c_char>()) };
                out.push(alloc::boxed::Box::new(text));
            }
            b'p' => {
                out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<*const c_void>() } as usize));
            }
            _ => {}
        }
    }
    out
}

/// Source `i915_probe_error(i915, fmt, ...)` from i915_utils.h: a
/// `drm_err(&(i915)->drm, fmt, ...)` probe-time error report.
///
/// # Safety
/// `fmt` must point to a NUL-terminated C format string and the variadic
/// arguments must match its conversions, as in the C call sites.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_probe_error(
    i915: *mut DrmI915Private,
    fmt: *const c_char,
    mut args: ...
) {
    let _ = i915;
    if fmt.is_null() {
        return;
    }
    let format_bytes = unsafe { CStr::from_ptr(fmt) }.to_bytes();
    let owned = unsafe { collect_c_args(format_bytes, &mut args) };
    let refs: alloc::vec::Vec<&dyn CFormatArg> = owned.iter().map(|a| a.as_ref() as &dyn CFormatArg).collect();
    let format = match core::str::from_utf8(format_bytes) {
        Ok(text) => text,
        Err(_) => return,
    };
    let message = format_message(format, &refs);
    drm_log_at(
        DrmLogLevel::Error,
        "i915 DRM error",
        file!(),
        line!(),
        &message,
    );
}

/// Helper declared by intel_gt_upstream.rs for gt probe failures. It has no
/// direct upstream counterpart; it logs through the same drm_err path as
/// `i915_probe_error`, with the gt name and return code as its fixed fields.
///
/// # Safety
/// `fmt` and `name` must be NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_probe_error(
    i915: *mut DrmI915Private,
    fmt: *const c_char,
    name: *const c_char,
    ret: i32,
) {
    let _ = i915;
    if fmt.is_null() {
        return;
    }
    let format = match unsafe { CStr::from_ptr(fmt) }.to_str() {
        Ok(text) => text,
        Err(_) => return,
    };
    let name_text = unsafe { c_str_arg(name) };
    let message = format_message(format, &[&name_text as &dyn CFormatArg, &ret as &dyn CFormatArg]);
    drm_log_at(
        DrmLogLevel::Error,
        "i915 DRM error",
        file!(),
        line!(),
        &message,
    );
}
