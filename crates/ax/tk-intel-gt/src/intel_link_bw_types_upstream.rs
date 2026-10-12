// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_link_bw.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct IntelLinkBwLimits {
    pub link_dsc_pipes: u8,
    pub bpp_limit_reached_pipes: u8,
    pub _pad_2: [u8; 2],
    pub max_bpp_x16: [i32; 4],
}

const _: () = assert!(size_of::<IntelLinkBwLimits>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelLinkBwLimits, link_dsc_pipes) == 0);
const _: () = assert!(core::mem::offset_of!(IntelLinkBwLimits, bpp_limit_reached_pipes) == 1);
const _: () = assert!(core::mem::offset_of!(IntelLinkBwLimits, max_bpp_x16) == 4);
const _: () = assert!(size_of::<[i32; 4]>() == 16);
