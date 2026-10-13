// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_cdclk.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntelCdclkConfig {
    pub cdclk: u32,
    pub vco: u32,
    pub r#ref: u32,
    pub bypass: u32,
    pub voltage_level: u8,
    pub joined_mbus: bool,
    pub _pad_18: [u8; 2],
}

const _: () = assert!(size_of::<IntelCdclkConfig>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelCdclkConfig, cdclk) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCdclkConfig, vco) == 4);
const _: () = assert!(core::mem::offset_of!(IntelCdclkConfig, r#ref) == 8);
const _: () = assert!(core::mem::offset_of!(IntelCdclkConfig, bypass) == 12);
const _: () = assert!(core::mem::offset_of!(IntelCdclkConfig, voltage_level) == 16);
const _: () = assert!(core::mem::offset_of!(IntelCdclkConfig, joined_mbus) == 17);
