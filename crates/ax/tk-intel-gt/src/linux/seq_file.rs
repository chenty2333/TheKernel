// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Small Linux `seq_file` output adapter for source-order i915 diagnostics.
//!
//! `seq_printf` is C-varargs in Linux; translated call sites pass a typed
//! argument slice to the same formatter used by DRM printf. The sequence-file
//! prefix is the Linux v7.2.3 ABI, and writes preserve its count/overflow and
//! NUL-reservation semantics so the enclosing read operation can retry with a
//! larger buffer.

#![allow(unsafe_code)]

use core::ffi::c_char;

use crate::linux::print::CFormatArg;

/// Prefix of Linux `struct seq_file` through the state used by `seq_printf`.
/// The pointer passed to this type must be a live kernel-owned seq_file.
#[repr(C)]
pub struct SeqFile {
    pub buf: *mut c_char,
    pub size: usize,
    pub from: usize,
    pub count: usize,
    pub pad_until: usize,
}

/// Format and append one record using the kernel's bounded `seq_printf`
/// behavior. A failed/truncated append sets `count == size`, which is the
/// Linux seq-file overflow signal; the buffer owner must retry with more room.
pub unsafe fn seq_printf(seq: *mut SeqFile, format: &str, args: &[&dyn CFormatArg]) {
    assert!(!seq.is_null());
    let message = crate::linux::print::format_message(format, args);
    let count = unsafe { (*seq).count };
    let size = unsafe { (*seq).size };
    assert!(count <= size, "seq_file count exceeds its buffer size");

    if count == size {
        return;
    }
    let buffer = unsafe { (*seq).buf };
    assert!(
        !buffer.is_null(),
        "seq_file has available space but no buffer"
    );
    let available = size - count;
    if message.len() < available {
        unsafe {
            core::ptr::copy_nonoverlapping(
                message.as_ptr(),
                buffer.cast::<u8>().add(count),
                message.len(),
            );
            *buffer.cast::<u8>().add(count + message.len()) = 0;
            (*seq).count = count + message.len();
        }
    } else {
        // Linux vscnprintf/vsnprintf leaves a terminating NUL when there is
        // space and seq_set_overflow marks the whole buffer consumed.
        if available != 0 {
            let copied = available - 1;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    message.as_ptr(),
                    buffer.cast::<u8>().add(count),
                    copied,
                );
                *buffer.cast::<u8>().add(size - 1) = 0;
            }
        }
        unsafe { (*seq).count = size };
    }
}

/// Copy raw bytes using `seq_write()`'s all-or-overflow contract.
pub unsafe fn seq_write(seq: *mut SeqFile, bytes: &[u8]) {
    assert!(!seq.is_null());
    let count = unsafe { (*seq).count };
    let size = unsafe { (*seq).size };
    assert!(count <= size, "seq_file count exceeds its buffer size");
    if bytes.len() > size - count {
        unsafe { (*seq).count = size };
        return;
    }
    assert!(!unsafe { (*seq).buf }.is_null() || bytes.is_empty());
    if !bytes.is_empty() {
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (*seq).buf.cast::<u8>().add(count),
                bytes.len(),
            );
            (*seq).count = count + bytes.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use core::ptr;

    use super::*;

    #[test]
    fn formatted_write_uses_linux_count_and_overflow_semantics() {
        let mut buf = [0u8; 7];
        let mut seq = SeqFile {
            buf: buf.as_mut_ptr().cast(),
            size: buf.len(),
            from: 0,
            count: 0,
            pad_until: 0,
        };
        let number = 12u32;
        unsafe { seq_printf(&mut seq, "v=%u", &[&number]) };
        assert_eq!(seq.count, 4);
        assert_eq!(&buf[..5], b"v=12\0");

        seq.count = 0;
        buf.fill(0);
        let long = "1234567";
        unsafe { seq_printf(&mut seq, "%s", &[&long]) };
        assert_eq!(seq.count, seq.size);
        assert_eq!(buf[6], 0);

        seq.buf = ptr::null_mut();
        seq.size = 0;
        seq.count = 0;
        unsafe { seq_printf(&mut seq, "", &[]) };
        assert_eq!(seq.count, 0);
    }

    #[test]
    fn raw_write_overflows_as_a_unit() {
        let mut buf = [0u8; 3];
        let mut seq = SeqFile {
            buf: buf.as_mut_ptr().cast(),
            size: buf.len(),
            from: 0,
            count: 1,
            pad_until: 0,
        };
        unsafe { seq_write(&mut seq, b"abc") };
        assert_eq!(seq.count, seq.size);
        assert_eq!(buf, [0; 3]);
    }
}
