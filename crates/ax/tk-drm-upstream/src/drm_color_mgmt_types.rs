// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/drm/drm_color_mgmt.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


pub type DrmColorEncoding = u32;
pub const DRM_COLOR_YCBCR_BT601: DrmColorEncoding = 0;
pub const DRM_COLOR_YCBCR_BT709: DrmColorEncoding = 1;
pub const DRM_COLOR_YCBCR_BT2020: DrmColorEncoding = 2;
pub const DRM_COLOR_ENCODING_MAX: DrmColorEncoding = 3;

pub type DrmColorRange = u32;
pub const DRM_COLOR_YCBCR_LIMITED_RANGE: DrmColorRange = 0;
pub const DRM_COLOR_YCBCR_FULL_RANGE: DrmColorRange = 1;
pub const DRM_COLOR_RANGE_MAX: DrmColorRange = 2;


