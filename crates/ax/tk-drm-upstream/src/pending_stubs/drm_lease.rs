// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_lease.c (lease batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_lease_upstream.rs when the lease batch lands.
//!
//! Symbols: _drm_lease_held, drm_lease_held, drm_lease_filter_crtcs.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_file_types::DrmFile;

// upstream: drm_lease.c (no owner yet): no lease is ever active in these tests.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _drm_lease_held(_file_priv: *mut DrmFile, _id: i32) -> bool {
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_lease_held(_file_priv: *mut DrmFile, _id: i32) -> bool {
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_lease_filter_crtcs(_file_priv: *mut DrmFile, crtcs: u32) -> u32 {
    crtcs
}
