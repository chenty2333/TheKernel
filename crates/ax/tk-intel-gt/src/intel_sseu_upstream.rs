// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_sseu.c.
// Register encodings follow i915_reg.h, intel_gt_regs.h, and intel_engine_regs.h.
// The existing get_hsw_subslices definition remains in intel_sseu_types_upstream.rs.
// Full MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    i915_request_types_upstream::DrmPrinter,
    intel_engine_cs_upstream::{I915PerfGt, Mutex},
    intel_gt_types_upstream::IntelGt,
    intel_sseu_types_upstream::{
        GEN_MAX_HSW_SLICES, GEN_SS_MASK_SIZE, I915_MAX_SS_FUSE_REGS, IntelSseu, IntelSseuSsMask,
        SseuDevInfo,
    },
    linux::{
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_POOLED_EU, INTEL_INFO, IP_VER, IS_BROADWELL,
            IS_CHERRYVIEW, IS_ELKHARTLAKE, IS_HASWELL, IS_JASPERLAKE,
        },
        mm_native::copy_to_user,
        primitives::{hweight8, hweight16, hweight32, str_yes_no},
        print::CFormatArg,
        registers::_MMIO,
        seq_file::{SeqFile, seq_printf},
    },
    linux_i915_private::DrmI915Private,
};

const GEN8_FUSE2: u32 = 0x9120;
const GEN8_F2_S_ENA_MASK: u32 = 0x7 << 25;
const GEN9_F2_SS_DIS_MASK: u32 = 0xf << 20;
const GEN8_EU_DISABLE0: u32 = 0x9134;
const GEN9_EU_DISABLE0: u32 = 0x9134;
const GEN8_EU_DISABLE1: u32 = 0x9138;
const GEN8_EU_DISABLE2: u32 = 0x913c;
const GEN8_EU_DIS0_S0_MASK: u32 = (1 << 24) - 1;
const GEN8_EU_DIS0_S1_MASK: u32 = 0xff << 24;
const GEN8_EU_DIS1_S1_MASK: u32 = 0xffff;
const GEN8_EU_DIS1_S2_MASK: u32 = 0xffff << 16;
const GEN8_EU_DIS2_S2_MASK: u32 = 0xff;
const GEN11_EU_DISABLE: u32 = 0x9134;
const GEN11_EU_DIS_MASK: u32 = 0xff;
const XEHP_EU_ENABLE: u32 = 0x9134;
const XEHP_EU_ENA_MASK: u32 = 0xff;
const GEN11_GT_SLICE_ENABLE: u32 = 0x9138;
const GEN11_GT_S_ENA_MASK: u32 = 0xff;
const GEN11_GT_SUBSLICE_DISABLE: u32 = 0x913c;
const GEN12_GT_GEOMETRY_DSS_ENABLE: u32 = 0x913c;
const GEN12_GT_COMPUTE_DSS_ENABLE: u32 = 0x9144;
const XEHPC_GT_COMPUTE_DSS_ENABLE_EXT: u32 = 0x9148;
const HSW_PAVP_FUSE1: u32 = 0x911c;
const HSW_F1_EU_DIS_MASK: u32 = 0x3 << 16;
const HSW_F1_EU_DIS_10EUS: u32 = 0;
const HSW_F1_EU_DIS_8EUS: u32 = 1;
const HSW_F1_EU_DIS_6EUS: u32 = 2;
const VLV_GUNIT_BASE: u32 = 0x180000;
const CHV_FUSE_GT: u32 = VLV_GUNIT_BASE + 0x2168;
const CHV_FGT_EU_DIS_SS1_R1_MASK: u32 = 0xf << 28;
const CHV_FGT_EU_DIS_SS1_R0_MASK: u32 = 0xf << 24;
const CHV_FGT_EU_DIS_SS0_R1_MASK: u32 = 0xf << 20;
const CHV_FGT_EU_DIS_SS0_R0_MASK: u32 = 0xf << 16;
const CHV_FGT_DISABLE_SS1: u32 = 1 << 11;
const CHV_FGT_DISABLE_SS0: u32 = 1 << 10;

const GEN8_RPCS_ENABLE: u32 = 1 << 31;
const GEN8_RPCS_S_CNT_ENABLE: u32 = 1 << 18;
const GEN8_RPCS_S_CNT_SHIFT: u32 = 15;
const GEN8_RPCS_S_CNT_MASK: u32 = 0x7 << GEN8_RPCS_S_CNT_SHIFT;
const GEN11_RPCS_S_CNT_SHIFT: u32 = 12;
const GEN11_RPCS_S_CNT_MASK: u32 = 0x3f << GEN11_RPCS_S_CNT_SHIFT;
const GEN8_RPCS_SS_CNT_ENABLE: u32 = 1 << 11;
const GEN8_RPCS_SS_CNT_SHIFT: u32 = 8;
const GEN8_RPCS_SS_CNT_MASK: u32 = 0x7 << GEN8_RPCS_SS_CNT_SHIFT;
const GEN8_RPCS_EU_MAX_SHIFT: u32 = 4;
const GEN8_RPCS_EU_MAX_MASK: u32 = 0xf << GEN8_RPCS_EU_MAX_SHIFT;
const GEN8_RPCS_EU_MIN_SHIFT: u32 = 0;
const GEN8_RPCS_EU_MIN_MASK: u32 = 0xf;

const PERF_GROUP_OAG: usize = 0;

#[repr(C)]
struct I915PerfGtOverlay {
    lock: Mutex,
    sseu: IntelSseu,
    num_perf_groups: u32,
    group: *mut I915PerfGroupPrefix,
}

#[repr(C)]
struct I915PerfGroupPrefix {
    exclusive_stream: *mut c_void,
}

const _: [(); size_of::<I915PerfGt>()] = [(); size_of::<I915PerfGtOverlay>()];

#[inline]
fn bit(bit: u32) -> u32 {
    1u32 << bit
}

#[inline]
fn genmask(high: u32, low: u32) -> u32 {
    (u32::MAX >> (31 - high)) & (u32::MAX << low)
}

#[inline]
fn reg_field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}

#[inline]
unsafe fn read_reg(gt: *mut IntelGt, offset: u32) -> u32 {
    unsafe { crate::intel_uncore_types_upstream::intel_uncore_read((*gt).uncore, _MMIO(offset)) }
}

#[inline]
unsafe fn test_bit(bitmap: *const c_ulong, bit: usize) -> bool {
    unsafe { *bitmap.add(bit / 64) & (1u64 << (bit % 64)) != 0 }
}

fn bitmap_weight(bitmap: &[c_ulong], nbits: usize) -> u32 {
    let words = nbits.div_ceil(64);
    let mut total = 0;
    for word in 0..words {
        let valid = core::cmp::min(64, nbits - word * 64);
        let mask = if valid == 64 {
            u64::MAX
        } else {
            (1u64 << valid) - 1
        };
        total += (bitmap[word] & mask).count_ones();
    }
    total
}

fn bitmap_or(dst: &mut [c_ulong], lhs: &[c_ulong], rhs: &[c_ulong], nbits: usize) {
    for word in 0..nbits.div_ceil(64) {
        dst[word] = lhs[word] | rhs[word];
    }
    if nbits % 64 != 0 {
        dst[nbits / 64] &= (1u64 << (nbits % 64)) - 1;
    }
}

fn bitmap_from_arr32(dst: &mut [c_ulong], src: &[u32], nbits: usize) {
    dst.fill(0);
    for (i, value) in src.iter().enumerate() {
        let bit = i * 32;
        if bit >= nbits {
            break;
        }
        dst[bit / 64] |= (*value as u64) << (bit % 64);
    }
    if nbits % 64 != 0 {
        dst[nbits / 64] &= (1u64 << (nbits % 64)) - 1;
    }
}

// upstream: intel_sseu.c intel_sseu_set_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_set_info(
    sseu: *mut SseuDevInfo,
    max_slices: u8,
    max_subslices: u8,
    max_eus_per_subslice: u8,
) {
    unsafe {
        (*sseu).max_slices = max_slices;
        (*sseu).max_subslices = max_subslices;
        (*sseu).max_eus_per_subslice = max_eus_per_subslice;
    }
}

// upstream: intel_sseu.c intel_sseu_subslice_total()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_subslice_total(sseu: *const SseuDevInfo) -> u32 {
    if unsafe { (*sseu).has_xehp_dss() } {
        return unsafe { bitmap_weight(&(*sseu).subslice_mask.xehp, 64) };
    }
    let mut total = 0;
    for slice in 0..GEN_MAX_HSW_SLICES {
        total += unsafe { hweight8((*sseu).subslice_mask.hsw[slice]) };
    }
    total
}

// `intel_sseu_get_hsw_subslices()` is source-ordered and marked in the existing
// `intel_sseu_types_upstream.rs`; this is its sole definition.

// upstream: intel_sseu.c sseu_get_eus()
unsafe fn sseu_get_eus(sseu: *const SseuDevInfo, slice: i32, subslice: i32) -> u16 {
    if unsafe { (*sseu).has_xehp_dss() } {
        WARN_ON!(slice > 0);
        unsafe { (*sseu).eu_mask.xehp[subslice as usize] }
    } else {
        unsafe { (*sseu).eu_mask.hsw[slice as usize][subslice as usize] }
    }
}

// upstream: intel_sseu.c sseu_set_eus()
unsafe fn sseu_set_eus(sseu: *mut SseuDevInfo, slice: i32, subslice: i32, eu_mask: u16) {
    if eu_mask != 0 {
        GEM_WARN_ON!(eu_mask.leading_zeros() < 16 - unsafe { (*sseu).max_eus_per_subslice } as u32);
    }
    if unsafe { (*sseu).has_xehp_dss() } {
        GEM_WARN_ON!(slice > 0);
        unsafe { (*sseu).eu_mask.xehp[subslice as usize] = eu_mask };
    } else {
        unsafe { (*sseu).eu_mask.hsw[slice as usize][subslice as usize] = eu_mask };
    }
}

// upstream: intel_sseu.c compute_eu_total()
unsafe fn compute_eu_total(sseu: *const SseuDevInfo) -> u16 {
    let mut total = 0u32;
    for slice in 0..unsafe { (*sseu).max_slices } {
        for subslice in 0..unsafe { (*sseu).max_subslices } {
            let mask = if unsafe { (*sseu).has_xehp_dss() } {
                unsafe { (*sseu).eu_mask.xehp[subslice as usize] }
            } else {
                unsafe { (*sseu).eu_mask.hsw[slice as usize][subslice as usize] }
            };
            total += hweight16(mask);
        }
    }
    total as u16
}

// upstream: intel_sseu.c intel_sseu_copy_eumask_to_user()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_copy_eumask_to_user(
    to: *mut c_void,
    sseu: *const SseuDevInfo,
) -> c_int {
    let mut eu_mask = [0u8; GEN_SS_MASK_SIZE * 2];
    let eu_stride = (unsafe { (*sseu).max_eus_per_subslice } as usize).div_ceil(8);
    let len = unsafe { (*sseu).max_slices as usize }
        * unsafe { (*sseu).max_subslices as usize }
        * eu_stride;
    for slice in 0..unsafe { (*sseu).max_slices as usize } {
        for subslice in 0..unsafe { (*sseu).max_subslices as usize } {
            let uapi_offset = slice * unsafe { (*sseu).max_subslices as usize } * eu_stride
                + subslice * eu_stride;
            let mask = unsafe { sseu_get_eus(sseu, slice as i32, subslice as i32) };
            for i in 0..eu_stride {
                eu_mask[uapi_offset + i] = ((mask >> (8 * i)) & 0xff) as u8;
            }
        }
    }
    unsafe { copy_to_user(to, eu_mask.as_ptr().cast(), len) as c_int }
}

// upstream: intel_sseu.c intel_sseu_copy_ssmask_to_user()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_copy_ssmask_to_user(
    to: *mut c_void,
    sseu: *const SseuDevInfo,
) -> c_int {
    let mut ss_mask = [0u8; GEN_SS_MASK_SIZE];
    let ss_stride = (unsafe { (*sseu).max_subslices as usize }).div_ceil(8);
    let len = unsafe { (*sseu).max_slices as usize } * ss_stride;
    for slice in 0..unsafe { (*sseu).max_slices as usize } {
        for subslice in 0..unsafe { (*sseu).max_subslices as usize } {
            let i = slice * ss_stride * 8 + subslice;
            if !crate::intel_sseu_types_upstream::intel_sseu_has_subslice(
                unsafe { &*sseu },
                slice as i32,
                subslice as i32,
            ) {
                continue;
            }
            ss_mask[i / 8] |= bit((i % 8) as u32) as u8;
        }
    }
    unsafe { copy_to_user(to, ss_mask.as_ptr().cast(), len) as c_int }
}

// upstream: intel_sseu.c gen11_compute_sseu_info()
unsafe fn gen11_compute_sseu_info(sseu: *mut SseuDevInfo, ss_en: u32, eu_en: u16) {
    let valid_ss_mask = genmask(unsafe { (*sseu).max_subslices as u32 - 1 }, 0);
    unsafe {
        (*sseu).slice_mask |= bit(0) as u8;
        (*sseu).subslice_mask.hsw[0] = (ss_en & valid_ss_mask) as u8;
    }
    for ss in 0..unsafe { (*sseu).max_subslices as i32 } {
        if crate::intel_sseu_types_upstream::intel_sseu_has_subslice(unsafe { &*sseu }, 0, ss) {
            unsafe { sseu_set_eus(sseu, 0, ss, eu_en) };
        }
    }
    unsafe {
        (*sseu).eu_per_subslice = hweight16(eu_en) as u8;
        (*sseu).eu_total = compute_eu_total(sseu);
    }
}

// upstream: intel_sseu.c xehp_compute_sseu_info()
unsafe fn xehp_compute_sseu_info(sseu: *mut SseuDevInfo, eu_en: u16) {
    unsafe {
        (*sseu).slice_mask |= bit(0) as u8;
        bitmap_or(
            &mut (*sseu).subslice_mask.xehp,
            &(*sseu).compute_subslice_mask.xehp,
            &(*sseu).geometry_subslice_mask.xehp,
            64,
        );
    }
    for ss in 0..unsafe { (*sseu).max_subslices as i32 } {
        if crate::intel_sseu_types_upstream::intel_sseu_has_subslice(unsafe { &*sseu }, 0, ss) {
            unsafe { sseu_set_eus(sseu, 0, ss, eu_en) };
        }
    }
    unsafe {
        (*sseu).eu_per_subslice = hweight16(eu_en) as u8;
        (*sseu).eu_total = compute_eu_total(sseu);
    }
}

// upstream: intel_sseu.c xehp_load_dss_mask()
unsafe fn xehp_load_dss_mask(
    uncore: *mut crate::intel_uncore_types_upstream::IntelUncore,
    ssmask: *mut IntelSseuSsMask,
    numregs: usize,
    regs: &[u32],
) {
    let count = if numregs > I915_MAX_SS_FUSE_REGS {
        WARN_ON!(true);
        I915_MAX_SS_FUSE_REGS
    } else {
        numregs
    };
    let mut fuse_val = [0u32; I915_MAX_SS_FUSE_REGS];
    for i in 0..count {
        fuse_val[i] = unsafe {
            crate::intel_uncore_types_upstream::intel_uncore_read(uncore, _MMIO(regs[i]))
        };
    }
    unsafe { bitmap_from_arr32(&mut (*ssmask).xehp, &fuse_val, count * 32) };
}

// upstream: intel_sseu.c xehp_sseu_info_init()
unsafe fn xehp_sseu_info_init(gt: *mut IntelGt) {
    let sseu = unsafe { ptr::addr_of_mut!((*gt).info.sseu) };
    let uncore = unsafe { (*gt).uncore };
    let mut eu_en = 0u16;
    let num_compute_regs = 1;
    let num_geometry_regs = 1;

    unsafe {
        intel_sseu_set_info(
            sseu,
            1,
            32 * core::cmp::max(num_geometry_regs, num_compute_regs) as u8,
            if has_one_eu_per_fuse_bit((*gt).i915) {
                8
            } else {
                16
            },
        );
        (*sseu).set_has_xehp_dss(true);
        xehp_load_dss_mask(
            uncore,
            ptr::addr_of_mut!((*sseu).geometry_subslice_mask),
            num_geometry_regs,
            &[GEN12_GT_GEOMETRY_DSS_ENABLE],
        );
        xehp_load_dss_mask(
            uncore,
            ptr::addr_of_mut!((*sseu).compute_subslice_mask),
            num_compute_regs,
            &[GEN12_GT_COMPUTE_DSS_ENABLE, XEHPC_GT_COMPUTE_DSS_ENABLE_EXT],
        );
    }

    let eu_en_fuse = reg_field_get(XEHP_EU_ENA_MASK, unsafe { read_reg(gt, XEHP_EU_ENABLE) }) as u8;
    if unsafe { has_one_eu_per_fuse_bit((*gt).i915) } {
        eu_en = eu_en_fuse as u16;
    } else {
        for eu in 0..unsafe { (*sseu).max_eus_per_subslice / 2 } {
            if eu_en_fuse & bit(eu as u32) as u8 != 0 {
                eu_en |= (bit(eu as u32 * 2) | bit(eu as u32 * 2 + 1)) as u16;
            }
        }
    }
    unsafe { xehp_compute_sseu_info(sseu, eu_en) };
}

// upstream: intel_sseu.c gen12_sseu_info_init()
unsafe fn gen12_sseu_info_init(gt: *mut IntelGt) {
    let sseu = unsafe { ptr::addr_of_mut!((*gt).info.sseu) };
    unsafe { intel_sseu_set_info(sseu, 1, 6, 16) };

    let s_en = reg_field_get(GEN11_GT_S_ENA_MASK, unsafe {
        read_reg(gt, GEN11_GT_SLICE_ENABLE)
    });
    WARN_ON!(s_en != 0x1);

    let g_dss_en = unsafe { read_reg(gt, GEN12_GT_GEOMETRY_DSS_ENABLE) };
    let eu_en_fuse = !reg_field_get(GEN11_EU_DIS_MASK, unsafe { read_reg(gt, GEN11_EU_DISABLE) });
    let mut eu_en = 0u16;
    for eu in 0..unsafe { (*sseu).max_eus_per_subslice / 2 } {
        if eu_en_fuse & bit(eu as u32) != 0 {
            eu_en |= (bit(eu as u32 * 2) | bit(eu as u32 * 2 + 1)) as u16;
        }
    }

    unsafe {
        gen11_compute_sseu_info(sseu, g_dss_en, eu_en);
        (*sseu).set_has_slice_pg(true);
    }
}

// upstream: intel_sseu.c gen11_sseu_info_init()
unsafe fn gen11_sseu_info_init(gt: *mut IntelGt) {
    let sseu = unsafe { ptr::addr_of_mut!((*gt).info.sseu) };
    let i915 = unsafe { (*gt).i915 };
    unsafe {
        if IS_JASPERLAKE(i915) || IS_ELKHARTLAKE(i915) {
            intel_sseu_set_info(sseu, 1, 4, 8);
        } else {
            intel_sseu_set_info(sseu, 1, 8, 8);
        }
    }

    let s_en = reg_field_get(GEN11_GT_S_ENA_MASK, unsafe {
        read_reg(gt, GEN11_GT_SLICE_ENABLE)
    });
    WARN_ON!(s_en != 0x1);
    let ss_en = !unsafe { read_reg(gt, GEN11_GT_SUBSLICE_DISABLE) };
    let eu_en = !reg_field_get(GEN11_EU_DIS_MASK, unsafe { read_reg(gt, GEN11_EU_DISABLE) }) as u8;
    unsafe {
        gen11_compute_sseu_info(sseu, ss_en, eu_en as u16);
        (*sseu).set_has_slice_pg(true);
        (*sseu).set_has_subslice_pg(true);
        (*sseu).set_has_eu_pg(true);
    }
}

// upstream: intel_sseu.c cherryview_sseu_info_init()
unsafe fn cherryview_sseu_info_init(gt: *mut IntelGt) {
    let sseu = unsafe { ptr::addr_of_mut!((*gt).info.sseu) };
    let fuse = unsafe { read_reg(gt, CHV_FUSE_GT) };
    unsafe {
        (*sseu).slice_mask = bit(0) as u8;
        intel_sseu_set_info(sseu, 1, 2, 8);
    }

    if fuse & CHV_FGT_DISABLE_SS0 == 0 {
        let disabled_mask = reg_field_get(CHV_FGT_EU_DIS_SS0_R0_MASK, fuse)
            | (reg_field_get(CHV_FGT_EU_DIS_SS0_R1_MASK, fuse)
                << hweight32(CHV_FGT_EU_DIS_SS0_R0_MASK));
        unsafe {
            (*sseu).subslice_mask.hsw[0] |= bit(0) as u8;
            sseu_set_eus(sseu, 0, 0, (!disabled_mask & 0xff) as u16);
        }
    }
    if fuse & CHV_FGT_DISABLE_SS1 == 0 {
        let disabled_mask = reg_field_get(CHV_FGT_EU_DIS_SS1_R0_MASK, fuse)
            | (reg_field_get(CHV_FGT_EU_DIS_SS1_R1_MASK, fuse)
                << hweight32(CHV_FGT_EU_DIS_SS1_R0_MASK));
        unsafe {
            (*sseu).subslice_mask.hsw[0] |= bit(1) as u8;
            sseu_set_eus(sseu, 0, 1, (!disabled_mask & 0xff) as u16);
        }
    }

    unsafe {
        (*sseu).eu_total = compute_eu_total(sseu);
        let total_ss = intel_sseu_subslice_total(sseu);
        (*sseu).eu_per_subslice = if total_ss != 0 {
            ((*sseu).eu_total as u32 / total_ss) as u8
        } else {
            0
        };
        (*sseu).set_has_slice_pg(false);
        (*sseu).set_has_subslice_pg(total_ss > 1);
        (*sseu).set_has_eu_pg((*sseu).eu_per_subslice > 2);
    }
}

// upstream: intel_sseu.c gen9_sseu_info_init()
unsafe fn gen9_sseu_info_init(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let sseu = unsafe { ptr::addr_of_mut!((*gt).info.sseu) };
    let fuse2 = unsafe { read_reg(gt, GEN8_FUSE2) };
    unsafe { (*sseu).slice_mask = reg_field_get(GEN8_F2_S_ENA_MASK, fuse2) as u8 };
    unsafe {
        intel_sseu_set_info(
            sseu,
            if crate::linux::i915::IS_GEN9_LP(i915) {
                1
            } else {
                3
            },
            if crate::linux::i915::IS_GEN9_LP(i915) {
                3
            } else {
                4
            },
            8,
        );
    }

    let mut subslice_mask = bit(unsafe { (*sseu).max_subslices as u32 }) - 1;
    subslice_mask &= !reg_field_get(GEN9_F2_SS_DIS_MASK, fuse2);
    let eu_mask = 0xffu8;

    for slice in 0..unsafe { (*sseu).max_slices as i32 } {
        if unsafe { (*sseu).slice_mask & bit(slice as u32) as u8 == 0 } {
            continue;
        }
        unsafe { (*sseu).subslice_mask.hsw[slice as usize] = subslice_mask as u8 };
        let eu_disable = unsafe { read_reg(gt, GEN9_EU_DISABLE0 + slice as u32 * 4) };
        for ss in 0..unsafe { (*sseu).max_subslices as i32 } {
            if !crate::intel_sseu_types_upstream::intel_sseu_has_subslice(
                unsafe { &*sseu },
                slice,
                ss,
            ) {
                continue;
            }
            let eu_disabled_mask = (eu_disable >> (ss * 8)) as u8 & eu_mask;
            unsafe { sseu_set_eus(sseu, slice, ss, (!eu_disabled_mask & eu_mask) as u16) };
            let eu_per_ss = unsafe { (*sseu).max_eus_per_subslice }
                .wrapping_sub(hweight8(eu_disabled_mask) as u8);
            if eu_per_ss == 7 {
                unsafe { (*sseu).subslice_7eu[slice as usize] |= bit(ss as u32) as u8 };
            }
        }
    }

    unsafe {
        (*sseu).eu_total = compute_eu_total(sseu);
        let total_ss = intel_sseu_subslice_total(sseu);
        (*sseu).eu_per_subslice = if total_ss != 0 {
            ((*sseu).eu_total as u32).div_ceil(total_ss) as u8
        } else {
            0
        };
        (*sseu).set_has_slice_pg(
            !crate::linux::i915::IS_GEN9_LP(i915) && hweight8((*sseu).slice_mask) > 1,
        );
        (*sseu).set_has_subslice_pg(crate::linux::i915::IS_GEN9_LP(i915) && total_ss > 1);
        (*sseu).set_has_eu_pg((*sseu).eu_per_subslice > 2);
    }

    if crate::linux::i915::IS_GEN9_LP(i915) {
        unsafe {
            (*i915).runtime.has_pooled_eu = hweight8((*sseu).subslice_mask.hsw[0]) == 3;
            (*sseu).min_eu_in_pool = 0;
            if HAS_POOLED_EU(i915) {
                let mask = (*sseu).subslice_mask.hsw[0];
                (*sseu).min_eu_in_pool = if mask & bit(2) as u8 == 0 || mask & bit(0) as u8 == 0 {
                    3
                } else if mask & bit(1) as u8 == 0 {
                    6
                } else {
                    9
                };
            }
        }
    }
}

// upstream: intel_sseu.c bdw_sseu_info_init()
unsafe fn bdw_sseu_info_init(gt: *mut IntelGt) {
    let sseu = unsafe { ptr::addr_of_mut!((*gt).info.sseu) };
    let fuse2 = unsafe { read_reg(gt, GEN8_FUSE2) };
    unsafe { (*sseu).slice_mask = reg_field_get(GEN8_F2_S_ENA_MASK, fuse2) as u8 };
    unsafe { intel_sseu_set_info(sseu, 3, 3, 8) };
    let mut subslice_mask = genmask(unsafe { (*sseu).max_subslices as u32 - 1 }, 0);
    subslice_mask &= !reg_field_get(GEN9_F2_SS_DIS_MASK, fuse2);

    let eu_disable0 = unsafe { read_reg(gt, GEN8_EU_DISABLE0) };
    let eu_disable1 = unsafe { read_reg(gt, GEN8_EU_DISABLE1) };
    let eu_disable2 = unsafe { read_reg(gt, GEN8_EU_DISABLE2) };
    let eu_disable = [
        reg_field_get(GEN8_EU_DIS0_S0_MASK, eu_disable0),
        reg_field_get(GEN8_EU_DIS0_S1_MASK, eu_disable0)
            | (reg_field_get(GEN8_EU_DIS1_S1_MASK, eu_disable1) << hweight32(GEN8_EU_DIS0_S1_MASK)),
        reg_field_get(GEN8_EU_DIS1_S2_MASK, eu_disable1)
            | (reg_field_get(GEN8_EU_DIS2_S2_MASK, eu_disable2) << hweight32(GEN8_EU_DIS1_S2_MASK)),
    ];

    for slice in 0..unsafe { (*sseu).max_slices as i32 } {
        if unsafe { (*sseu).slice_mask & bit(slice as u32) as u8 == 0 } {
            continue;
        }
        unsafe { (*sseu).subslice_mask.hsw[slice as usize] = subslice_mask as u8 };
        for ss in 0..unsafe { (*sseu).max_subslices as i32 } {
            if !crate::intel_sseu_types_upstream::intel_sseu_has_subslice(
                unsafe { &*sseu },
                slice,
                ss,
            ) {
                continue;
            }
            let eu_disabled_mask = (eu_disable[slice as usize]
                >> (ss * unsafe { (*sseu).max_eus_per_subslice } as i32))
                as u8;
            unsafe { sseu_set_eus(sseu, slice, ss, (!eu_disabled_mask) as u16) };
            let n_disabled = hweight8(eu_disabled_mask) as u8;
            if unsafe { (*sseu).max_eus_per_subslice }.wrapping_sub(n_disabled) == 7 {
                unsafe { (*sseu).subslice_7eu[slice as usize] |= bit(ss as u32) as u8 };
            }
        }
    }

    unsafe {
        (*sseu).eu_total = compute_eu_total(sseu);
        let total_ss = intel_sseu_subslice_total(sseu);
        (*sseu).eu_per_subslice = if total_ss != 0 {
            ((*sseu).eu_total as u32).div_ceil(total_ss) as u8
        } else {
            0
        };
        (*sseu).set_has_slice_pg(hweight8((*sseu).slice_mask) > 1);
        (*sseu).set_has_subslice_pg(false);
        (*sseu).set_has_eu_pg(false);
    }
}

// upstream: intel_sseu.c hsw_sseu_info_init()
unsafe fn hsw_sseu_info_init(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let sseu = unsafe { ptr::addr_of_mut!((*gt).info.sseu) };
    let mut subslice_mask = 0u8;
    match unsafe { (*INTEL_INFO(i915)).gt } {
        1 => {
            unsafe { (*sseu).slice_mask = bit(0) as u8 };
            subslice_mask = bit(0) as u8;
        }
        2 => {
            unsafe { (*sseu).slice_mask = bit(0) as u8 };
            subslice_mask = (bit(0) | bit(1)) as u8;
        }
        3 => {
            unsafe { (*sseu).slice_mask = (bit(0) | bit(1)) as u8 };
            subslice_mask = (bit(0) | bit(1)) as u8;
        }
        other => {
            MISSING_CASE!(other);
            unsafe {
                (*sseu).slice_mask = bit(0) as u8;
                subslice_mask = bit(0) as u8;
            }
        }
    }

    let fuse1 = unsafe { read_reg(gt, HSW_PAVP_FUSE1) };
    let eu_per_subslice = match reg_field_get(HSW_F1_EU_DIS_MASK, fuse1) {
        HSW_F1_EU_DIS_10EUS => 10,
        HSW_F1_EU_DIS_8EUS => 8,
        HSW_F1_EU_DIS_6EUS => 6,
        unknown => {
            MISSING_CASE!(unknown);
            10
        }
    };
    unsafe {
        intel_sseu_set_info(
            sseu,
            hweight8((*sseu).slice_mask) as u8,
            hweight8(subslice_mask) as u8,
            eu_per_subslice,
        );
    }

    for slice in 0..unsafe { (*sseu).max_slices as i32 } {
        unsafe { (*sseu).subslice_mask.hsw[slice as usize] = subslice_mask };
        for ss in 0..unsafe { (*sseu).max_subslices as i32 } {
            unsafe {
                sseu_set_eus(sseu, slice, ss, ((1u32 << eu_per_subslice) - 1) as u16);
            }
        }
    }
    unsafe {
        (*sseu).eu_total = compute_eu_total(sseu);
        (*sseu).set_has_slice_pg(false);
        (*sseu).set_has_subslice_pg(false);
        (*sseu).set_has_eu_pg(false);
    }
}

// upstream: intel_sseu.c intel_sseu_info_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_info_init(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 55) {
        unsafe { xehp_sseu_info_init(gt) };
    } else if unsafe { GRAPHICS_VER(i915) } >= 12 {
        unsafe { gen12_sseu_info_init(gt) };
    } else if unsafe { GRAPHICS_VER(i915) } >= 11 {
        unsafe { gen11_sseu_info_init(gt) };
    } else if unsafe { GRAPHICS_VER(i915) } >= 9 {
        unsafe { gen9_sseu_info_init(gt) };
    } else if unsafe { IS_BROADWELL(i915) } {
        unsafe { bdw_sseu_info_init(gt) };
    } else if unsafe { IS_CHERRYVIEW(i915) } {
        unsafe { cherryview_sseu_info_init(gt) };
    } else if unsafe { IS_HASWELL(i915) } {
        unsafe { hsw_sseu_info_init(gt) };
    }
}

// upstream: intel_sseu.c intel_sseu_make_rpcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_make_rpcs(gt: *mut IntelGt, req_sseu: *const IntelSseu) -> u32 {
    let i915 = unsafe { (*gt).i915 };
    let sseu = unsafe { ptr::addr_of!((*gt).info.sseu) };
    let perf = unsafe { ptr::addr_of_mut!((*gt).perf).cast::<I915PerfGtOverlay>() };
    let mut subslice_pg = unsafe { (*sseu).has_subslice_pg() };
    let mut req_sseu = req_sseu;
    let mut rpcs = 0;

    if unsafe { GRAPHICS_VER(i915) } < 9 {
        return 0;
    }

    let groups = unsafe { (*perf).group };
    if !groups.is_null() && !unsafe { (*groups.add(PERF_GROUP_OAG)).exclusive_stream }.is_null() {
        req_sseu = ptr::addr_of!((*perf).sseu);
    }

    let mut slices = hweight8(unsafe { (*req_sseu).slice_mask }) as u8;
    let subslices = hweight8(unsafe { (*req_sseu).subslice_mask }) as u8;
    if unsafe { GRAPHICS_VER(i915) } == 11
        && slices == 1
        && subslices
            > core::cmp::min(
                4,
                hweight8(unsafe { (*sseu).subslice_mask.hsw[0] }) as u8 / 2,
            )
    {
        GEM_BUG_ON!(subslices & 1 != 0);
        subslice_pg = false;
        slices *= 2;
    }

    if unsafe { (*sseu).has_slice_pg() } {
        let (mask, mut value) = if unsafe { GRAPHICS_VER(i915) } >= 11 {
            (
                GEN11_RPCS_S_CNT_MASK,
                (slices as u32) << GEN11_RPCS_S_CNT_SHIFT,
            )
        } else {
            (
                GEN8_RPCS_S_CNT_MASK,
                (slices as u32) << GEN8_RPCS_S_CNT_SHIFT,
            )
        };
        GEM_BUG_ON!(value & !mask != 0);
        value &= mask;
        rpcs |= GEN8_RPCS_ENABLE | GEN8_RPCS_S_CNT_ENABLE | value;
    }

    if subslice_pg {
        let mut value = (subslices as u32) << GEN8_RPCS_SS_CNT_SHIFT;
        GEM_BUG_ON!(value & !GEN8_RPCS_SS_CNT_MASK != 0);
        value &= GEN8_RPCS_SS_CNT_MASK;
        rpcs |= GEN8_RPCS_ENABLE | GEN8_RPCS_SS_CNT_ENABLE | value;
    }

    if unsafe { (*sseu).has_eu_pg() } {
        let mut value =
            (unsafe { (*req_sseu).min_eus_per_subslice } as u32) << GEN8_RPCS_EU_MIN_SHIFT;
        GEM_BUG_ON!(value & !GEN8_RPCS_EU_MIN_MASK != 0);
        value &= GEN8_RPCS_EU_MIN_MASK;
        rpcs |= value;

        value = (unsafe { (*req_sseu).max_eus_per_subslice } as u32) << GEN8_RPCS_EU_MAX_SHIFT;
        GEM_BUG_ON!(value & !GEN8_RPCS_EU_MAX_MASK != 0);
        value &= GEN8_RPCS_EU_MAX_MASK;
        rpcs |= value | GEN8_RPCS_ENABLE;
    }
    rpcs
}

fn bitmap_hex(mask: &[c_ulong; 1]) -> alloc::string::String {
    // Linux `%*pb` prints 32-bit chunks, most-significant first.
    alloc::format!("{:08x},{:08x}", (mask[0] >> 32) as u32, mask[0] as u32)
}

// upstream: intel_sseu.c intel_sseu_dump()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_dump(sseu: *const SseuDevInfo, p: *mut DrmPrinter) {
    if unsafe { (*sseu).has_xehp_dss() } {
        let geometry = unsafe { bitmap_hex(&(*sseu).geometry_subslice_mask.xehp) };
        let compute = unsafe { bitmap_hex(&(*sseu).compute_subslice_mask.xehp) };
        drm_printf!(p, "subslice total: %u\n", unsafe {
            intel_sseu_subslice_total(sseu)
        });
        drm_printf!(p, "geometry dss mask=%s\n", geometry.as_str());
        drm_printf!(p, "compute dss mask=%s\n", compute.as_str());
    } else {
        drm_printf!(
            p,
            "slice total: %u, mask=%04x\n",
            hweight8(unsafe { (*sseu).slice_mask }),
            unsafe { (*sseu).slice_mask }
        );
        drm_printf!(p, "subslice total: %u\n", unsafe {
            intel_sseu_subslice_total(sseu)
        });
        for slice in 0..unsafe { (*sseu).max_slices as i32 } {
            let ss_mask = unsafe { (*sseu).subslice_mask.hsw[slice as usize] };
            drm_printf!(
                p,
                "slice%d: %u subslices, mask=%08x\n",
                slice,
                hweight8(ss_mask),
                ss_mask
            );
        }
    }
    drm_printf!(p, "EU total: %u\n", unsafe { (*sseu).eu_total });
    drm_printf!(p, "EU per subslice: %u\n", unsafe {
        (*sseu).eu_per_subslice
    });
    drm_printf!(
        p,
        "has slice power gating: %s\n",
        str_yes_no(unsafe { (*sseu).has_slice_pg() })
    );
    drm_printf!(
        p,
        "has subslice power gating: %s\n",
        str_yes_no(unsafe { (*sseu).has_subslice_pg() })
    );
    drm_printf!(
        p,
        "has EU power gating: %s\n",
        str_yes_no(unsafe { (*sseu).has_eu_pg() })
    );
}

// upstream: intel_sseu.c sseu_print_hsw_topology()
unsafe fn sseu_print_hsw_topology(sseu: *const SseuDevInfo, p: *mut DrmPrinter) {
    for slice in 0..unsafe { (*sseu).max_slices as i32 } {
        let ss_mask = unsafe { (*sseu).subslice_mask.hsw[slice as usize] };
        drm_printf!(
            p,
            "slice%d: %u subslice(s) (0x%08x):\n",
            slice,
            hweight8(ss_mask),
            ss_mask
        );
        for ss in 0..unsafe { (*sseu).max_subslices as i32 } {
            let enabled_eus = unsafe { sseu_get_eus(sseu, slice, ss) };
            drm_printf!(
                p,
                "\tsubslice%d: %u EUs (0x%hx)\n",
                ss,
                hweight16(enabled_eus),
                enabled_eus
            );
        }
    }
}

// upstream: intel_sseu.c sseu_print_xehp_topology()
unsafe fn sseu_print_xehp_topology(sseu: *const SseuDevInfo, p: *mut DrmPrinter) {
    for dss in 0..unsafe { (*sseu).max_subslices as i32 } {
        let enabled_eus = unsafe { sseu_get_eus(sseu, 0, dss) };
        let geometry =
            unsafe { test_bit((*sseu).geometry_subslice_mask.xehp.as_ptr(), dss as usize) };
        let compute =
            unsafe { test_bit((*sseu).compute_subslice_mask.xehp.as_ptr(), dss as usize) };
        drm_printf!(
            p,
            "DSS_%02d: G:%3s C:%3s, %2u EUs (0x%04hx)\n",
            dss,
            str_yes_no(geometry),
            str_yes_no(compute),
            hweight16(enabled_eus),
            enabled_eus
        );
    }
}

// upstream: intel_sseu.c intel_sseu_print_topology()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_print_topology(
    i915: *mut DrmI915Private,
    sseu: *const SseuDevInfo,
    p: *mut DrmPrinter,
) {
    if unsafe { (*sseu).max_slices } == 0 {
        drm_printf!(p, "Unavailable\n");
    } else if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 55) {
        unsafe { sseu_print_xehp_topology(sseu, p) };
    } else {
        unsafe { sseu_print_hsw_topology(sseu, p) };
    }
}

// upstream: intel_sseu.c intel_sseu_print_ss_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sseu_print_ss_info(
    type_: *const c_char,
    sseu: *const SseuDevInfo,
    m: *mut SeqFile,
) {
    let type_name = if type_.is_null() {
        alloc::string::String::new()
    } else {
        unsafe { core::ffi::CStr::from_ptr(type_) }
            .to_string_lossy()
            .into_owned()
    };

    if unsafe { (*sseu).has_xehp_dss() } {
        let geometry = unsafe { bitmap_weight(&(*sseu).geometry_subslice_mask.xehp, 64) };
        let compute = unsafe { bitmap_weight(&(*sseu).compute_subslice_mask.xehp, 64) };
        let args: &[&dyn CFormatArg] = &[&type_name.as_str(), &geometry];
        unsafe { seq_printf(m, "  %s Geometry DSS: %u\n", args) };
        let args: &[&dyn CFormatArg] = &[&type_name.as_str(), &compute];
        unsafe { seq_printf(m, "  %s Compute DSS: %u\n", args) };
    } else {
        let slice_mask = unsafe { (*sseu).slice_mask };
        let slice_count = if slice_mask == 0 {
            0
        } else {
            (8 - slice_mask.leading_zeros()) as usize
        };
        for slice in 0..slice_count {
            let count = hweight8(unsafe { (*sseu).subslice_mask.hsw[slice] });
            let slice_index = slice as i32;
            let args: &[&dyn CFormatArg] = &[&type_name.as_str(), &slice_index, &count];
            unsafe { seq_printf(m, "  %s Slice%i subslices: %u\n", args) };
        }
    }
}

// upstream: intel_sseu.c intel_slicemask_from_xehp_dssmask()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_slicemask_from_xehp_dssmask(
    mut dss_mask: IntelSseuSsMask,
    dss_per_slice: c_int,
) -> u16 {
    let mut per_slice_mask = IntelSseuSsMask { xehp: [0] };
    let mut slice_mask = 0u64;
    let bits: usize = 64;
    let dss_per_slice = dss_per_slice as usize;
    WARN_ON!(dss_per_slice == 0 || bits.div_ceil(dss_per_slice) > 8 * size_of::<c_ulong>());
    assert!(dss_per_slice > 0 && dss_per_slice <= bits);
    unsafe { (*(&mut per_slice_mask as *mut IntelSseuSsMask)).xehp[0] = u64::MAX };
    if dss_per_slice < bits {
        unsafe {
            (*(&mut per_slice_mask as *mut IntelSseuSsMask)).xehp[0] = (1u64 << dss_per_slice) - 1
        };
    }

    let mut current = unsafe { dss_mask.xehp[0] };
    for i in 0..bits {
        if current == 0 {
            break;
        }
        if current & unsafe { per_slice_mask.xehp[0] } != 0 {
            slice_mask |= 1u64 << i;
        }
        current = if dss_per_slice == bits {
            0
        } else {
            current >> dss_per_slice
        };
    }
    slice_mask as u16
}

unsafe fn has_one_eu_per_fuse_bit(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    let flags = unsafe { (*info).flags };
    // Linux DEV_INFO_FOR_EACH_FLAG has_one_eu_per_fuse_bit is bit 26.
    flags[26 / 8] & (1 << (26 % 8)) != 0
}
