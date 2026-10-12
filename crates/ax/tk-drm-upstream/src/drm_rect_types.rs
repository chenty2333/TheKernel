// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/drm/drm_rect.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct DrmRect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

const _: () = assert!(size_of::<DrmRect>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmRect, x1) == 0);
const _: () = assert!(core::mem::offset_of!(DrmRect, y1) == 4);
const _: () = assert!(core::mem::offset_of!(DrmRect, x2) == 8);
const _: () = assert!(core::mem::offset_of!(DrmRect, y2) == 12);
