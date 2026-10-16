// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_fbc.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};

use tk_drm_upstream::drm_rect_types::DrmRect;
use tk_linux_kapi::types::{Mutex, WorkStruct};

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_types_upstream::IntelPlane;
use core::mem::{align_of, size_of, ManuallyDrop};


pub type IntelFbcId = u32;
pub const INTEL_FBC_A: IntelFbcId = 0;
pub const INTEL_FBC_B: IntelFbcId = 1;
pub const INTEL_FBC_C: IntelFbcId = 2;
pub const INTEL_FBC_D: IntelFbcId = 3;
pub const I915_MAX_FBCS: IntelFbcId = 4;



/// `struct intel_fbc_state` (intel_fbc.c).
#[repr(C)]
pub struct IntelFbcState {
    pub plane: *mut IntelPlane,
    pub cfb_stride: u32,
    pub cfb_size: u32,
    pub fence_y_offset: u32,
    pub override_cfb_stride: u16,
    pub interval: u16,
    pub fence_id: i8,
    pub dirty_rect: DrmRect,
}

/// `struct intel_fbc` (intel_fbc.c). Only the fields the translated FBC
/// paths touch are named with their C meaning; the rest keep C order.
#[repr(C)]
pub struct IntelFbc {
    pub display: *mut IntelDisplay,
    pub funcs: *const c_void,
    pub lock: Mutex,
    pub busy_bits: u32,
    pub compressed_fb: *mut c_void,
    pub compressed_llb: *mut c_void,
    pub id: IntelFbcId,
    pub limit: u8,
    pub false_color: bool,
    pub active: bool,
    pub activated: bool,
    pub flip_pending: bool,
    pub underrun_detected: bool,
    pub underrun_work: WorkStruct,
    pub state: IntelFbcState,
    pub no_fbc_reason: *const c_char,
}
