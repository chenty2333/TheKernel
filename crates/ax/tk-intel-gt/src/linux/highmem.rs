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
