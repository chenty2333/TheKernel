// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_sseu.h.

#![allow(non_snake_case)]

use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::size_of,
};

use crate::{
    i915_request_types_upstream::DrmPrinter, intel_gt_types_upstream::IntelGt,
    linux_i915_private::DrmI915Private,
};

pub const GEN_MAX_HSW_SLICES: usize = 3;
pub const GEN_MAX_SS_PER_HSW_SLICE: usize = 8;
pub const I915_MAX_SS_FUSE_REGS: usize = 2;
pub const I915_MAX_SS_FUSE_BITS: usize = I915_MAX_SS_FUSE_REGS * 32;
pub const GEN_MAX_EUS_PER_SS: usize = 16;

pub const fn SSEU_MAX(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

pub const fn GEN_SSEU_STRIDE(max_entries: usize) -> usize {
    max_entries.div_ceil(8)
}

pub const GEN_SS_MASK_SIZE: usize = SSEU_MAX(
    I915_MAX_SS_FUSE_BITS,
    GEN_MAX_HSW_SLICES * GEN_MAX_SS_PER_HSW_SLICE,
);
pub const GEN_MAX_SUBSLICE_STRIDE: usize = GEN_SSEU_STRIDE(GEN_SS_MASK_SIZE);
pub const GEN_MAX_EU_STRIDE: usize = GEN_SSEU_STRIDE(GEN_MAX_EUS_PER_SS);

pub const GEN_DSS_PER_GSLICE: usize = 4;
pub const GEN_DSS_PER_CSLICE: usize = 8;
pub const GEN_DSS_PER_MSLICE: usize = 8;
pub const GEN_MAX_GSLICES: usize = I915_MAX_SS_FUSE_BITS / GEN_DSS_PER_GSLICE;
pub const GEN_MAX_CSLICES: usize = I915_MAX_SS_FUSE_BITS / GEN_DSS_PER_CSLICE;

/// `intel_sseu_ss_mask_t`: HSW byte mask and Xe_HP bitmap share storage.
#[repr(C)]
#[derive(Clone, Copy)]
pub union IntelSseuSsMask {
    pub hsw: [u8; GEN_MAX_HSW_SLICES],
    pub xehp: [c_ulong; 1],
}

/// `XEHP_BITMAP_BITS(mask)`. The source macro uses the type of `mask.xehp`;
/// the configured x86_64 target has a single unsigned-long bitmap word.
pub const fn XEHP_BITMAP_BITS(_mask: &IntelSseuSsMask) -> i32 {
    (size_of::<[c_ulong; 1]>() * 8) as i32
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union IntelSseuEuMask {
    pub hsw: [[u16; GEN_MAX_SS_PER_HSW_SLICE]; GEN_MAX_HSW_SLICES],
    pub xehp: [u16; I915_MAX_SS_FUSE_BITS],
}

/// `struct sseu_dev_info`. The four adjacent C `u8:1` members share the
/// `power_gating` byte; access them with the bit accessors below.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SseuDevInfo {
    pub slice_mask: u8,
    pub subslice_mask: IntelSseuSsMask,
    pub geometry_subslice_mask: IntelSseuSsMask,
    pub compute_subslice_mask: IntelSseuSsMask,
    pub eu_mask: IntelSseuEuMask,
    pub eu_total: u16,
    pub eu_per_subslice: u8,
    pub min_eu_in_pool: u8,
    pub subslice_7eu: [u8; 3],
    pub power_gating: u8,
    pub max_slices: u8,
    pub max_subslices: u8,
    pub max_eus_per_subslice: u8,
}

/// `intel_sseu_get_hsw_subslices()` from Linux 7.2.3 `gt/intel_sseu.c`.
/// The source warns on Xe_HP topology and invalid slice indices, then returns
/// the Haswell-style byte mask (zero on an invalid slice).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_get_hsw_subslices(
    sseu: *const SseuDevInfo,
    slice: u8,
) -> u32 {
    // upstream: intel_sseu.c intel_sseu_get_hsw_subslices()
    if unsafe { (*sseu).has_xehp_dss() } {
        WARN_ON!(true);
    }
    if slice >= unsafe { (*sseu).max_slices } {
        WARN_ON!(true);
        return 0;
    }
    unsafe { (*sseu).subslice_mask.hsw[slice as usize] as u32 }
}

impl SseuDevInfo {
    pub const fn has_slice_pg(&self) -> bool {
        self.power_gating & 1 != 0
    }
    pub fn set_has_slice_pg(&mut self, enabled: bool) {
        self.power_gating = (self.power_gating & !1) | enabled as u8;
    }
    pub const fn has_subslice_pg(&self) -> bool {
        self.power_gating & 2 != 0
    }
    pub fn set_has_subslice_pg(&mut self, enabled: bool) {
        self.power_gating = (self.power_gating & !2) | ((enabled as u8) << 1);
    }
    pub const fn has_eu_pg(&self) -> bool {
        self.power_gating & 4 != 0
    }
    pub fn set_has_eu_pg(&mut self, enabled: bool) {
        self.power_gating = (self.power_gating & !4) | ((enabled as u8) << 2);
    }
    pub const fn has_xehp_dss(&self) -> bool {
        self.power_gating & 8 != 0
    }
    pub fn set_has_xehp_dss(&mut self, enabled: bool) {
        self.power_gating = (self.power_gating & !8) | ((enabled as u8) << 3);
    }
}

/// `struct intel_sseu` power-gating configuration.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelSseu {
    pub slice_mask: u8,
    pub subslice_mask: u8,
    pub min_eus_per_subslice: u8,
    pub max_eus_per_subslice: u8,
}

pub fn intel_sseu_from_device_info(sseu: &SseuDevInfo) -> IntelSseu {
    let subslice_mask = unsafe { sseu.subslice_mask.hsw[0] };
    IntelSseu {
        slice_mask: sseu.slice_mask,
        subslice_mask,
        min_eus_per_subslice: sseu.max_eus_per_subslice,
        max_eus_per_subslice: sseu.max_eus_per_subslice,
    }
}

pub fn intel_sseu_has_subslice(sseu: &SseuDevInfo, slice: i32, subslice: i32) -> bool {
    let max_slices = sseu.max_slices as i32;
    let max_subslices = sseu.max_subslices as i32;
    if slice < 0 || subslice < 0 || slice >= max_slices || subslice >= max_subslices {
        return false;
    }

    if sseu.has_xehp_dss() {
        if subslice as usize >= I915_MAX_SS_FUSE_BITS {
            return false;
        }
        let mask = unsafe { sseu.subslice_mask.xehp[0] };
        mask as u32 & (1u32 << subslice) != 0
    } else {
        if slice as usize >= GEN_MAX_HSW_SLICES {
            return false;
        }
        let mask = unsafe { sseu.subslice_mask.hsw[slice as usize] };
        mask & (1 << subslice) != 0
    }
}

pub fn intel_sseu_find_first_xehp_dss(sseu: &SseuDevInfo, groupsize: i32, groupnum: i32) -> u32 {
    let start = groupnum.saturating_mul(groupsize);
    if start < 0 || start >= I915_MAX_SS_FUSE_BITS as i32 {
        return I915_MAX_SS_FUSE_BITS as u32;
    }
    let word = unsafe { sseu.subslice_mask.xehp[0] };
    for bit in start as u32..I915_MAX_SS_FUSE_BITS as u32 {
        if word & (1 << bit) != 0 {
            return bit;
        }
    }
    I915_MAX_SS_FUSE_BITS as u32
}

unsafe extern "C" {
    pub fn intel_sseu_set_info(
        sseu: *mut SseuDevInfo,
        max_slices: u8,
        max_subslices: u8,
        max_eus_per_subslice: u8,
    );
    pub fn intel_sseu_subslice_total(sseu: *const SseuDevInfo) -> u32;
    pub fn intel_sseu_get_compute_subslices(sseu: *const SseuDevInfo) -> IntelSseuSsMask;
    pub fn intel_sseu_info_init(gt: *mut IntelGt);
    pub fn intel_sseu_make_rpcs(gt: *mut IntelGt, req_sseu: *const IntelSseu) -> u32;
    pub fn intel_sseu_dump(sseu: *const SseuDevInfo, printer: *mut DrmPrinter);
    pub fn intel_sseu_print_topology(
        i915: *mut DrmI915Private,
        sseu: *const SseuDevInfo,
        printer: *mut DrmPrinter,
    );
    pub fn intel_slicemask_from_xehp_dssmask(dss_mask: IntelSseuSsMask, dss_per_slice: i32) -> u16;
    pub fn intel_sseu_copy_eumask_to_user(to: *mut c_void, sseu: *const SseuDevInfo) -> i32;
    pub fn intel_sseu_copy_ssmask_to_user(to: *mut c_void, sseu: *const SseuDevInfo) -> i32;
    pub fn intel_sseu_print_ss_info(
        kind: *const c_char,
        sseu: *const SseuDevInfo,
        seq: *mut SeqFile,
    );
}

/// `struct seq_file` is only passed through a pointer in this header.
#[repr(C)]
pub struct SeqFile {
    _opaque: [u8; 0],
}

// x86_64 Linux target: BITS_PER_LONG=64, hence the Xe_HP mask is one word.
const _: [(); 8] = [(); size_of::<IntelSseuSsMask>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelSseuSsMask>()];
const _: [(); 128] = [(); size_of::<IntelSseuEuMask>()];
const _: [(); 2] = [(); core::mem::align_of::<IntelSseuEuMask>()];
const _: [(); 176] = [(); size_of::<SseuDevInfo>()];
const _: [(); 8] = [(); core::mem::align_of::<SseuDevInfo>()];
const _: [(); 0] = [(); core::mem::offset_of!(SseuDevInfo, slice_mask)];
const _: [(); 8] = [(); core::mem::offset_of!(SseuDevInfo, subslice_mask)];
const _: [(); 16] = [(); core::mem::offset_of!(SseuDevInfo, geometry_subslice_mask)];
const _: [(); 24] = [(); core::mem::offset_of!(SseuDevInfo, compute_subslice_mask)];
const _: [(); 32] = [(); core::mem::offset_of!(SseuDevInfo, eu_mask)];
const _: [(); 160] = [(); core::mem::offset_of!(SseuDevInfo, eu_total)];
const _: [(); 162] = [(); core::mem::offset_of!(SseuDevInfo, eu_per_subslice)];
const _: [(); 163] = [(); core::mem::offset_of!(SseuDevInfo, min_eu_in_pool)];
const _: [(); 164] = [(); core::mem::offset_of!(SseuDevInfo, subslice_7eu)];
const _: [(); 167] = [(); core::mem::offset_of!(SseuDevInfo, power_gating)];
const _: [(); 168] = [(); core::mem::offset_of!(SseuDevInfo, max_slices)];
const _: [(); 169] = [(); core::mem::offset_of!(SseuDevInfo, max_subslices)];
const _: [(); 170] = [(); core::mem::offset_of!(SseuDevInfo, max_eus_per_subslice)];
const _: [(); 4] = [(); size_of::<IntelSseu>()];
