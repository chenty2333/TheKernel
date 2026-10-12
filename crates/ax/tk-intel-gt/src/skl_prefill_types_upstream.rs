// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/skl_prefill.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct SklPrefillCtxPrefill {
    pub fixed: u32,
    pub wm0: u32,
    pub scaler_1st: u32,
    pub scaler_2nd: u32,
    pub dsc: u32,
    pub full: u32,
}

#[repr(C)]
pub struct SklPrefillCtxAdj {
    pub cdclk: u32,
    pub scaler_1st: u32,
    pub scaler_2nd: u32,
}

#[repr(C)]
pub struct SklPrefillCtx {
    pub prefill: SklPrefillCtxPrefill,
    pub adj: SklPrefillCtxAdj,
}

const _: () = assert!(size_of::<SklPrefillCtxPrefill>() == 24);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxPrefill, fixed) == 0);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxPrefill, wm0) == 4);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxPrefill, scaler_1st) == 8);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxPrefill, scaler_2nd) == 12);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxPrefill, dsc) == 16);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxPrefill, full) == 20);
const _: () = assert!(size_of::<SklPrefillCtxAdj>() == 12);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxAdj, cdclk) == 0);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxAdj, scaler_1st) == 4);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtxAdj, scaler_2nd) == 8);
const _: () = assert!(size_of::<SklPrefillCtx>() == 36);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtx, prefill) == 0);
const _: () = assert!(core::mem::offset_of!(SklPrefillCtx, adj) == 24);
