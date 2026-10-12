// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/i915_gtt_view_types.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct IntelRemappedPlaneInfoAnon4x2Anon4x0 {
    pub width: u16,
    pub height: u16,
    pub src_stride: u16,
    pub dst_stride: u16,
}

#[repr(C)]
pub union IntelRemappedPlaneInfoAnon4x2 {
    pub _anon_4_0: ManuallyDrop<IntelRemappedPlaneInfoAnon4x2Anon4x0>,
    pub size: u32,
}

#[repr(C)]
pub struct IntelRemappedPlaneInfo {
    /// bit-fields offset:31, linear:1
    pub _bitfields_0: [u8; 4],
    pub _anon_4_2: IntelRemappedPlaneInfoAnon4x2,
}

#[repr(C)]
pub struct IntelRotationInfo {
    pub plane: [IntelRemappedPlaneInfo; 2],
}

#[repr(C, packed)]
pub struct IntelPartialInfo {
    pub offset: u64,
    pub size: u32,
}

#[repr(C)]
pub struct IntelRemappedInfo {
    pub plane: [IntelRemappedPlaneInfo; 4],
    pub plane_alignment: u32,
}

#[repr(C)]
pub union I915GttViewAnon4x1 {
    pub partial: ManuallyDrop<IntelPartialInfo>,
    pub rotated: ManuallyDrop<IntelRotationInfo>,
    pub remapped: ManuallyDrop<IntelRemappedInfo>,
}

#[repr(C, packed)]
pub struct I915GttView {
    pub r#type: I915GttViewType,
    pub _anon_4_1: I915GttViewAnon4x1,
}

pub type I915GttViewType = u32;
pub const I915_GTT_VIEW_NORMAL: I915GttViewType = 0;
pub const I915_GTT_VIEW_ROTATED: I915GttViewType = 24;
pub const I915_GTT_VIEW_PARTIAL: I915GttViewType = 12;
pub const I915_GTT_VIEW_REMAPPED: I915GttViewType = 52;

const _: () = assert!(size_of::<IntelRemappedPlaneInfoAnon4x2Anon4x0>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelRemappedPlaneInfoAnon4x2Anon4x0, width) == 0);
const _: () = assert!(core::mem::offset_of!(IntelRemappedPlaneInfoAnon4x2Anon4x0, height) == 2);
const _: () = assert!(core::mem::offset_of!(IntelRemappedPlaneInfoAnon4x2Anon4x0, src_stride) == 4);
const _: () = assert!(core::mem::offset_of!(IntelRemappedPlaneInfoAnon4x2Anon4x0, dst_stride) == 6);
const _: () = assert!(size_of::<IntelRemappedPlaneInfoAnon4x2>() == 8);
const _: () = assert!(size_of::<IntelRemappedPlaneInfo>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelRemappedPlaneInfo, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(IntelRemappedPlaneInfo, _anon_4_2) == 4);
const _: () = assert!(size_of::<IntelRotationInfo>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelRotationInfo, plane) == 0);
const _: () = assert!(size_of::<[IntelRemappedPlaneInfo; 2]>() == 24);
const _: () = assert!(size_of::<IntelPartialInfo>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelPartialInfo, offset) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPartialInfo, size) == 8);
const _: () = assert!(size_of::<IntelRemappedInfo>() == 52);
const _: () = assert!(core::mem::offset_of!(IntelRemappedInfo, plane) == 0);
const _: () = assert!(size_of::<[IntelRemappedPlaneInfo; 4]>() == 48);
const _: () = assert!(core::mem::offset_of!(IntelRemappedInfo, plane_alignment) == 48);
const _: () = assert!(size_of::<I915GttViewAnon4x1>() == 52);
const _: () = assert!(size_of::<I915GttView>() == 56);
const _: () = assert!(core::mem::offset_of!(I915GttView, r#type) == 0);
const _: () = assert!(size_of::<I915GttViewType>() == 4);
const _: () = assert!(core::mem::offset_of!(I915GttView, _anon_4_1) == 4);
