// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
//
//! C entry-point declarations from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_migrate.h`.
//!
//! The `intel_migrate` record is owned separately by
//! `intel_migrate_types.h` and is imported from its Rust binding.

use crate::{
    i915_request_types_upstream::{I915Deps, I915Request},
    intel_context_types_upstream::IntelContext,
    intel_context_upstream::{I915GemWwCtx, SgEntry as Scatterlist},
    intel_gt_types_upstream::IntelGt,
    intel_migrate_types_upstream::IntelMigrate,
};

// Out-of-line implementations owned by intel_migrate.c.
unsafe extern "C" {
    pub fn intel_migrate_init(migrate: *mut IntelMigrate, gt: *mut IntelGt) -> i32;
    pub fn intel_migrate_create_context(migrate: *mut IntelMigrate) -> *mut IntelContext;
    pub fn intel_migrate_copy(
        migrate: *mut IntelMigrate,
        ww: *mut I915GemWwCtx,
        deps: *const I915Deps,
        src: *mut Scatterlist,
        src_pat_index: u32,
        src_is_lmem: bool,
        dst: *mut Scatterlist,
        dst_pat_index: u32,
        dst_is_lmem: bool,
        out: *mut *mut I915Request,
    ) -> i32;
    pub fn intel_context_migrate_copy(
        ce: *mut IntelContext,
        deps: *const I915Deps,
        src: *mut Scatterlist,
        src_pat_index: u32,
        src_is_lmem: bool,
        dst: *mut Scatterlist,
        dst_pat_index: u32,
        dst_is_lmem: bool,
        out: *mut *mut I915Request,
    ) -> i32;
    pub fn intel_migrate_clear(
        migrate: *mut IntelMigrate,
        ww: *mut I915GemWwCtx,
        deps: *const I915Deps,
        sg: *mut Scatterlist,
        pat_index: u32,
        is_lmem: bool,
        value: u32,
        out: *mut *mut I915Request,
    ) -> i32;
    pub fn intel_context_migrate_clear(
        ce: *mut IntelContext,
        deps: *const I915Deps,
        sg: *mut Scatterlist,
        pat_index: u32,
        is_lmem: bool,
        value: u32,
        out: *mut *mut I915Request,
    ) -> i32;
    pub fn intel_migrate_fini(migrate: *mut IntelMigrate);
}
