// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_wm_types.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct IlkWmValues {
    pub wm_pipe: [u32; 3],
    pub wm_lp: [u32; 3],
    pub wm_lp_spr: [u32; 3],
    pub enable_fbc_wm: bool,
    pub _pad_37: [u8; 3],
    pub partitioning: IntelDdbPartitioning,
}

#[repr(C)]
pub struct G4xPipeWm {
    pub plane: [u16; 8],
    pub fbc: u16,
}

#[repr(C)]
pub struct G4xSrWm {
    pub plane: u16,
    pub cursor: u16,
    pub fbc: u16,
}

#[repr(C)]
pub struct VlvWmDdlValues {
    pub plane: [u8; 8],
}

#[repr(C)]
pub struct VlvWmValues {
    pub pipe: [G4xPipeWm; 3],
    pub sr: G4xSrWm,
    pub ddl: [VlvWmDdlValues; 3],
    pub level: u8,
    pub cxsr: bool,
}

#[repr(C)]
pub struct G4xWmValues {
    pub pipe: [G4xPipeWm; 2],
    pub sr: G4xSrWm,
    pub hpll: G4xSrWm,
    pub cxsr: bool,
    pub hpll_en: bool,
    pub fbc_en: bool,
    pub _pad_51: [u8; 1],
}

#[repr(C)]
pub struct SklDdbEntry {
    pub start: u16,
    pub end: u16,
}

pub type IntelDdbPartitioning = u32;
pub const INTEL_DDB_PART_1_2: IntelDdbPartitioning = 0;
pub const INTEL_DDB_PART_5_6: IntelDdbPartitioning = 1;

const _: () = assert!(size_of::<IlkWmValues>() == 44);
const _: () = assert!(core::mem::offset_of!(IlkWmValues, wm_pipe) == 0);
const _: () = assert!(size_of::<[u32; 3]>() == 12);
const _: () = assert!(core::mem::offset_of!(IlkWmValues, wm_lp) == 12);
const _: () = assert!(size_of::<[u32; 3]>() == 12);
const _: () = assert!(core::mem::offset_of!(IlkWmValues, wm_lp_spr) == 24);
const _: () = assert!(size_of::<[u32; 3]>() == 12);
const _: () = assert!(core::mem::offset_of!(IlkWmValues, enable_fbc_wm) == 36);
const _: () = assert!(core::mem::offset_of!(IlkWmValues, partitioning) == 40);
const _: () = assert!(size_of::<IntelDdbPartitioning>() == 4);
const _: () = assert!(size_of::<G4xPipeWm>() == 18);
const _: () = assert!(core::mem::offset_of!(G4xPipeWm, plane) == 0);
const _: () = assert!(size_of::<[u16; 8]>() == 16);
const _: () = assert!(core::mem::offset_of!(G4xPipeWm, fbc) == 16);
const _: () = assert!(size_of::<G4xSrWm>() == 6);
const _: () = assert!(core::mem::offset_of!(G4xSrWm, plane) == 0);
const _: () = assert!(core::mem::offset_of!(G4xSrWm, cursor) == 2);
const _: () = assert!(core::mem::offset_of!(G4xSrWm, fbc) == 4);
const _: () = assert!(size_of::<VlvWmDdlValues>() == 8);
const _: () = assert!(core::mem::offset_of!(VlvWmDdlValues, plane) == 0);
const _: () = assert!(size_of::<[u8; 8]>() == 8);
const _: () = assert!(size_of::<VlvWmValues>() == 86);
const _: () = assert!(core::mem::offset_of!(VlvWmValues, pipe) == 0);
const _: () = assert!(size_of::<[G4xPipeWm; 3]>() == 54);
const _: () = assert!(core::mem::offset_of!(VlvWmValues, sr) == 54);
const _: () = assert!(size_of::<G4xSrWm>() == 6);
const _: () = assert!(core::mem::offset_of!(VlvWmValues, ddl) == 60);
const _: () = assert!(size_of::<[VlvWmDdlValues; 3]>() == 24);
const _: () = assert!(core::mem::offset_of!(VlvWmValues, level) == 84);
const _: () = assert!(core::mem::offset_of!(VlvWmValues, cxsr) == 85);
const _: () = assert!(size_of::<G4xWmValues>() == 52);
const _: () = assert!(core::mem::offset_of!(G4xWmValues, pipe) == 0);
const _: () = assert!(size_of::<[G4xPipeWm; 2]>() == 36);
const _: () = assert!(core::mem::offset_of!(G4xWmValues, sr) == 36);
const _: () = assert!(size_of::<G4xSrWm>() == 6);
const _: () = assert!(core::mem::offset_of!(G4xWmValues, hpll) == 42);
const _: () = assert!(size_of::<G4xSrWm>() == 6);
const _: () = assert!(core::mem::offset_of!(G4xWmValues, cxsr) == 48);
const _: () = assert!(core::mem::offset_of!(G4xWmValues, hpll_en) == 49);
const _: () = assert!(core::mem::offset_of!(G4xWmValues, fbc_en) == 50);
const _: () = assert!(size_of::<SklDdbEntry>() == 4);
const _: () = assert!(core::mem::offset_of!(SklDdbEntry, start) == 0);
const _: () = assert!(core::mem::offset_of!(SklDdbEntry, end) == 2);
