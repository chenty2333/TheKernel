// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! Linux highmem mapping entry points used by the imported GEM paths.

use core::ffi::c_void;
use crate::i915_gem_object_types_upstream::Page;

pub use crate::linux::shmem::{page_address,put_page,mark_page_accessed};
pub unsafe fn kmap(page:*mut Page)->*mut c_void {unsafe {page_address(page)}}
pub unsafe fn kunmap(_page:*mut Page) {}

#[inline]
pub unsafe fn kmap_local_page(page: *mut Page) -> *mut c_void {
    unsafe { page_address(page) }
}

#[inline]
pub fn kunmap_local(_address: *mut c_void) {
    // CONFIG_HIGHMEM=n and ARCH_HAS_FLUSH_ON_KUNMAP=n in the target config.
}
