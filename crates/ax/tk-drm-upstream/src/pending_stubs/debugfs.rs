// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_debugfs_crc.c / drm_debugfs.c (debugfs batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_debugfs_upstream.rs when the debugfs batch lands; debugfs is off in the shipped config.
//!
//! Symbols: drm_debugfs_crtc_add, drm_debugfs_crtc_remove.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_crtc_types::DrmCrtc;

// upstream: drivers/gpu/drm/drm_debugfs_crc.c / drm_debugfs.c drm_debugfs_crtc_add()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_crtc_add(_crtc: *mut DrmCrtc) {}

// upstream: drivers/gpu/drm/drm_debugfs.c drm_debugfs_crtc_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_debugfs_crtc_remove(_crtc: *mut DrmCrtc) {}
