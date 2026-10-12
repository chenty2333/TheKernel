// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_dram.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct DramInfo {
    pub r#type: IntelDramType,
    pub fsb_freq: u32,
    pub mem_freq: u32,
    pub num_channels: u8,
    pub num_qgv_points: u8,
    pub num_psf_gv_points: u8,
    pub ecc_impacting_de_bw: bool,
    pub symmetric_memory: bool,
    pub has_16gb_dimms: bool,
    pub _pad_18: [u8; 2],
}

pub type IntelDramType = u32;
pub const INTEL_DRAM_UNKNOWN: IntelDramType = 0;
pub const INTEL_DRAM_DDR2: IntelDramType = 1;
pub const INTEL_DRAM_DDR3: IntelDramType = 2;
pub const INTEL_DRAM_DDR4: IntelDramType = 3;
pub const INTEL_DRAM_LPDDR3: IntelDramType = 4;
pub const INTEL_DRAM_LPDDR4: IntelDramType = 5;
pub const INTEL_DRAM_DDR5: IntelDramType = 6;
pub const INTEL_DRAM_LPDDR5: IntelDramType = 7;
pub const INTEL_DRAM_GDDR: IntelDramType = 8;
pub const INTEL_DRAM_GDDR_ECC: IntelDramType = 9;
pub const __INTEL_DRAM_TYPE_MAX: IntelDramType = 10;

const _: () = assert!(size_of::<DramInfo>() == 20);
const _: () = assert!(core::mem::offset_of!(DramInfo, r#type) == 0);
const _: () = assert!(size_of::<IntelDramType>() == 4);
const _: () = assert!(core::mem::offset_of!(DramInfo, fsb_freq) == 4);
const _: () = assert!(core::mem::offset_of!(DramInfo, mem_freq) == 8);
const _: () = assert!(core::mem::offset_of!(DramInfo, num_channels) == 12);
const _: () = assert!(core::mem::offset_of!(DramInfo, num_qgv_points) == 13);
const _: () = assert!(core::mem::offset_of!(DramInfo, num_psf_gv_points) == 14);
const _: () = assert!(core::mem::offset_of!(DramInfo, ecc_impacting_de_bw) == 15);
const _: () = assert!(core::mem::offset_of!(DramInfo, symmetric_memory) == 16);
const _: () = assert!(core::mem::offset_of!(DramInfo, has_16gb_dimms) == 17);
