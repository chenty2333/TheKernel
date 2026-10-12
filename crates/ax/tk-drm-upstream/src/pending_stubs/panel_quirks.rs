// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_panel_orientation_quirks.c (no owner yet). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_panel_orientation_quirks_upstream.rs when it lands.
//!
//! Symbols: drm_get_panel_orientation_quirk.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

// upstream: drm_panel_orientation_quirks.c (no owner yet): no quirk matches.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_get_panel_orientation_quirk(_width: i32, _height: i32) -> i32 {
    -1
}
