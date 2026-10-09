// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux page helpers used by the upstream i915 translations.

use crate::i915_gem_object_types_upstream::Page;

unsafe extern "C" {
    /// Linux `page_to_phys()` from `linux/mm.h`.
    pub fn page_to_phys(page: *const Page) -> usize;
}
