// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_vblank.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::intel_display_types_upstream::{IntelCrtc};

#[repr(C)]
pub struct IntelVblankEvadeCtx {
    pub crtc: *mut IntelCrtc,
    pub min: i32,
    pub max: i32,
    pub vblank_start: i32,
    pub need_vlv_dsi_wa: bool,
    pub _pad_21: [u8; 3],
}

const _: () = assert!(size_of::<IntelVblankEvadeCtx>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelVblankEvadeCtx, crtc) == 0);
const _: () = assert!(core::mem::offset_of!(IntelVblankEvadeCtx, min) == 8);
const _: () = assert!(core::mem::offset_of!(IntelVblankEvadeCtx, max) == 12);
const _: () = assert!(core::mem::offset_of!(IntelVblankEvadeCtx, vblank_start) == 16);
const _: () = assert!(core::mem::offset_of!(IntelVblankEvadeCtx, need_vlv_dsi_wa) == 20);
