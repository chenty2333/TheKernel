// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_bridge.c (display-bridge batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_bridge_upstream.rs when the bridge batch lands.
//!
//! Symbols: drm_bridge_detach.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

// upstream: drm_bridge.c (no owner yet): the bridge chain is always empty here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_bridge_detach(_bridge: *mut c_void) {}
