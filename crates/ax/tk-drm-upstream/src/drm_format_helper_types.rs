// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/drm/drm_format_helper.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct DrmFormatConvStateTmp {
    pub mem: *mut c_void,
    pub size: usize,
    pub preallocated: bool,
}

#[repr(C)]
pub struct DrmFormatConvState {
    pub tmp: DrmFormatConvStateTmp,
}

const _: () = assert!(size_of::<DrmFormatConvStateTmp>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmFormatConvStateTmp, mem) == 0);
const _: () = assert!(core::mem::offset_of!(DrmFormatConvStateTmp, size) == 8);
const _: () = assert!(core::mem::offset_of!(DrmFormatConvStateTmp, preallocated) == 16);
const _: () = assert!(size_of::<DrmFormatConvState>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmFormatConvState, tmp) == 0);
