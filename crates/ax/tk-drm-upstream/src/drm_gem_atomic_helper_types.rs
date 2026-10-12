// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/drm/drm_gem_atomic_helper.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::drm_format_helper_types::{DrmFormatConvState};
use crate::drm_plane_types::{DrmPlaneState};
use crate::iosys_map_types::{IosysMap};

#[repr(C)]
pub struct DrmShadowPlaneState {
    pub base: DrmPlaneState,
    pub fmtcnv_state: DrmFormatConvState,
    pub map: [IosysMap; 4],
    pub data: [IosysMap; 4],
}

const _: () = assert!(size_of::<DrmShadowPlaneState>() == 336);
const _: () = assert!(core::mem::offset_of!(DrmShadowPlaneState, base) == 0);
const _: () = assert!(size_of::<DrmPlaneState>() == 184);
const _: () = assert!(core::mem::offset_of!(DrmShadowPlaneState, fmtcnv_state) == 184);
const _: () = assert!(size_of::<DrmFormatConvState>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmShadowPlaneState, map) == 208);
const _: () = assert!(size_of::<[IosysMap; 4]>() == 64);
const _: () = assert!(core::mem::offset_of!(DrmShadowPlaneState, data) == 272);
const _: () = assert!(size_of::<[IosysMap; 4]>() == 64);
