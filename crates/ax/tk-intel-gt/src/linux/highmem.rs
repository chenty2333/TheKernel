// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! Linux highmem mapping entry points used by the imported GEM paths.

use core::ffi::c_void;
use crate::i915_gem_object_types_upstream::Page;

unsafe extern "C" {
    /// Map a struct page into the kernel address space (Linux highmem API).
    pub fn kmap(page: *mut Page) -> *mut c_void;
    /// Release a mapping created by `kmap`.
    pub fn kunmap(page: *mut Page);
}

// Linux x86_64 wt-dev is built with CONFIG_HIGHMEM=n. These are the exact
// `highmem-internal.h` inline expansions for this configuration.
unsafe extern "C" {
    fn page_address(page: *mut Page) -> *mut c_void;
    pub fn put_page(page: *mut Page);
    pub fn mark_page_accessed(page: *mut Page);
}

#[inline]
pub unsafe fn kmap_local_page(page: *mut Page) -> *mut c_void {
    unsafe { page_address(page) }
}

#[inline]
pub fn kunmap_local(_address: *mut c_void) {
    // CONFIG_HIGHMEM=n and ARCH_HAS_FLUSH_ON_KUNMAP=n in the target config.
}
