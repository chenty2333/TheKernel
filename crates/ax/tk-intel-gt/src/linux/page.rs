// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux page helpers used by the upstream i915 translations.

use core::ffi::c_ulong;

use crate::i915_gem_object_types_upstream::Page;
use crate::linux_config::PAGE_SHIFT;

unsafe extern "C" {
    /// Linux `page_to_phys()` from `linux/mm.h`.
    pub fn page_to_phys(page: *const Page) -> usize;
}

/// `page_to_pfn()` derives the page-frame number from the physical address.
#[inline]
pub unsafe fn page_to_pfn(page: *const Page) -> c_ulong {
    (unsafe { page_to_phys(page) } >> PAGE_SHIFT) as c_ulong
}

/// `folio_pfn()` is the page-frame number of the folio's first page.
#[inline]
pub unsafe fn folio_pfn<T>(folio: *const T) -> c_ulong {
    // Linux struct folio embeds struct page at offset zero.
    unsafe { page_to_pfn(folio.cast::<Page>()) }
}
