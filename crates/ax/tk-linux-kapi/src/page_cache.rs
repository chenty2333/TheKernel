// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux page-array cache-attribute operations backed by the x86 kernel
//! direct-map CPA/PAT implementation in `tk-axmm`.
#![allow(unsafe_code)]

use core::ffi::{c_int, c_void};

use crate::{
    config::{EINVAL, EIO, ENODEV, ENOMEM, EOPNOTSUPP, GFP_KERNEL},
    memory::{kfree, kmalloc},
    shmem::page_address,
    types::Page,
};

fn errno_from_axmm(error: axerrno::AxError) -> c_int {
    use axerrno::AxErrorKind;
    match AxErrorKind::try_from(error) {
        Ok(AxErrorKind::NoMemory) => -ENOMEM,
        Ok(AxErrorKind::NoSuchDevice) => -ENODEV,
        Ok(AxErrorKind::InvalidInput) => -EINVAL,
        Ok(AxErrorKind::OperationNotSupported | AxErrorKind::Unsupported) => -EOPNOTSUPP,
        _ => -EIO,
    }
}

unsafe fn set_pages_array_cache(pages: *mut *mut Page, count: c_int, wc: bool) -> c_int {
    if count < 0 || (count != 0 && pages.is_null()) {
        return -EINVAL;
    }
    if count == 0 {
        return 0;
    }

    let Some(bytes) = (count as usize).checked_mul(core::mem::size_of::<usize>()) else {
        return -EINVAL;
    };
    let addresses = kmalloc(bytes, GFP_KERNEL).cast::<usize>();
    if addresses.is_null() {
        return -ENOMEM;
    }

    let mut result = 0;
    for index in 0..count as usize {
        let page = unsafe { pages.add(index).read() };
        if page.is_null() {
            result = -EINVAL;
            break;
        }
        let address = unsafe { page_address(page) };
        if address.is_null() {
            result = -EINVAL;
            break;
        }
        unsafe { addresses.add(index).write(address as usize) };
    }

    if result == 0 {
        // Copy into a typed slice only after every input page has been checked.
        // The lower layer performs PAT reservation, direct-map PTE changes,
        // cache/TLB maintenance, and transactional rollback on error.
        let vaddrs = unsafe { core::slice::from_raw_parts(addresses, count as usize) };
        let changed = if wc {
            axmm::memtype::set_memory::set_pages_array_wc_kernel(vaddrs)
        } else {
            axmm::memtype::set_memory::set_pages_array_wb_kernel(vaddrs)
        };
        if let Err(error) = changed {
            result = errno_from_axmm(error);
        }
    }

    unsafe { kfree(addresses.cast::<c_void>()) };
    result
}

/// Change a page array's direct-map alias to write-combining.
///
/// # Safety
/// `pages` must point to `count` live LinuxKPI pages.
pub unsafe fn set_pages_array_wc_kernel(pages: *mut *mut Page, count: c_int) -> c_int {
    unsafe { set_pages_array_cache(pages, count, true) }
}

/// Restore a page array's direct-map alias to write-back.
///
/// # Safety
/// `pages` must point to `count` live pages previously changed to WC.
pub unsafe fn set_pages_array_wb_kernel(pages: *mut *mut Page, count: c_int) -> c_int {
    unsafe { set_pages_array_cache(pages, count, false) }
}

/// Linux x86 `set_pages_array_wc()` ABI.
#[unsafe(export_name = "set_pages_array_wc")]
pub unsafe extern "C" fn set_pages_array_wc(pages: *mut *mut Page, count: c_int) -> c_int {
    unsafe { set_pages_array_wc_kernel(pages, count) }
}

/// Linux x86 `set_pages_array_wb()` ABI.
#[unsafe(export_name = "set_pages_array_wb")]
pub unsafe extern "C" fn set_pages_array_wb(pages: *mut *mut Page, count: c_int) -> c_int {
    unsafe { set_pages_array_wb_kernel(pages, count) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_null_page_before_changing_direct_map_attributes() {
        let mut pages = [core::ptr::null_mut()];
        assert_eq!(
            unsafe { set_pages_array_wc_kernel(pages.as_mut_ptr(), 1) },
            -EINVAL
        );
    }
}
