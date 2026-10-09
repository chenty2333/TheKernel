// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI `request_firmware_nowarn()` adapter backed by the installed rootfs
//! firmware reader. Requests preserve failure (missing/oversized data is not
//! fabricated); the i915 caller is expected to run after its rootfs-ready gate.

#![allow(unsafe_code)]

use alloc::{boxed::Box, vec::Vec};
use core::{
    ffi::{CStr, c_char, c_void},
    ptr,
};

const MAX_I915_FIRMWARE_BYTES: usize = 8 * 1024 * 1024;

/// Linux 7.2.3 `struct firmware` layout for its public `size`/`data` prefix
/// and loader-owned private pointer on x86_64.
#[repr(C)]
pub struct Firmware {
    pub size: usize,
    pub data: *const u8,
    pub priv_: *mut c_void,
}

#[repr(C)]
struct FirmwareBlob {
    record: Firmware,
    bytes: Vec<u8>,
}

/// Stable C binding consumed by the i915 source translation. The opaque
/// device is intentionally not used by TheKernel's rootfs reader.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tk_linux_firmware_request_nowarn(
    firmware: *mut *const Firmware,
    name: *const c_char,
    _device: *mut c_void,
) -> i32 {
    if firmware.is_null() || name.is_null() {
        return -crate::linux_config::EINVAL;
    }
    unsafe { *firmware = ptr::null() };

    let path = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(path) if !path.is_empty() => path,
        _ => return -crate::linux_config::EINVAL,
    };
    let Some(bytes) = axdriver_base::firmware::request(path, MAX_I915_FIRMWARE_BYTES) else {
        return -crate::linux_config::ENOENT;
    };

    let mut blob = Box::new(FirmwareBlob {
        record: Firmware {
            size: bytes.len(),
            data: ptr::null(),
            priv_: ptr::null_mut(),
        },
        bytes,
    });
    let blob_ptr: *mut FirmwareBlob = &mut *blob;
    blob.record.data = blob.bytes.as_ptr();
    blob.record.priv_ = blob_ptr.cast();
    let record = ptr::addr_of!(blob.record);
    let _ = Box::into_raw(blob);
    unsafe { *firmware = record };
    0
}

/// Release only firmware objects created by the request adapter above.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tk_linux_firmware_release(firmware: *const Firmware) {
    if firmware.is_null() {
        return;
    }
    let owner = unsafe { (*firmware).priv_.cast::<FirmwareBlob>() };
    assert!(!owner.is_null(), "foreign Linux firmware object");
    assert!(ptr::eq(firmware, unsafe { ptr::addr_of!((*owner).record) }));
    drop(unsafe { Box::from_raw(owner) });
}

const _: [(); 24] = [(); core::mem::size_of::<Firmware>()];
const _: [(); 0] = [(); core::mem::offset_of!(Firmware, size)];
const _: [(); 8] = [(); core::mem::offset_of!(Firmware, data)];
const _: [(); 16] = [(); core::mem::offset_of!(Firmware, priv_)];
