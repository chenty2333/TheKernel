// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_dp.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct LinkConfigLimitsPipe {
    pub min_bpp: i32,
    pub max_bpp: i32,
}

#[repr(C)]
pub struct LinkConfigLimitsLink {
    pub min_bpp_x16: i32,
    pub max_bpp_x16: i32,
}

#[repr(C)]
pub struct LinkConfigLimits {
    pub min_rate: i32,
    pub max_rate: i32,
    pub min_lane_count: i32,
    pub max_lane_count: i32,
    pub pipe: LinkConfigLimitsPipe,
    pub link: LinkConfigLimitsLink,
}

const _: () = assert!(size_of::<LinkConfigLimitsPipe>() == 8);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimitsPipe, min_bpp) == 0);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimitsPipe, max_bpp) == 4);
const _: () = assert!(size_of::<LinkConfigLimitsLink>() == 8);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimitsLink, min_bpp_x16) == 0);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimitsLink, max_bpp_x16) == 4);
const _: () = assert!(size_of::<LinkConfigLimits>() == 32);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimits, min_rate) == 0);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimits, max_rate) == 4);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimits, min_lane_count) == 8);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimits, max_lane_count) == 12);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimits, pipe) == 16);
const _: () = assert!(core::mem::offset_of!(LinkConfigLimits, link) == 24);
