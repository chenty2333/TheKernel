// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_gt_mcr.c.
// The corresponding header/types stay owned by intel_gt_mcr_upstream.rs.
// Full MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{
    ffi::c_ulong,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use crate::{
    i915_request_types_upstream::DrmPrinter,
    intel_gt_types_upstream::{
        DSS, GAM, GT_MEDIA, INSTANCE0, IntelGt, IntelMmioRange, L3BANK, LNCF, MSLICE,
        NUM_STEERING_TYPES, OADDRM,
    },
    intel_sseu_types_upstream::{
        GEN_DSS_PER_GSLICE, GEN_DSS_PER_MSLICE, GEN_MAX_SS_PER_HSW_SLICE,
        intel_sseu_find_first_xehp_dss,
    },
    intel_uncore_types_upstream::{
        FORCEWAKE_GT, FW_REG_READ, FW_REG_WRITE, IS_GSI_REG, intel_uncore_forcewake_for_reg,
        intel_uncore_forcewake_get, intel_uncore_forcewake_get__locked, intel_uncore_forcewake_put,
        intel_uncore_forcewake_put__locked, intel_uncore_read, intel_uncore_read_fw,
        intel_uncore_write, intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::{I915McrRegT as I915McrReg, I915RegT},
    intel_workarounds_upstream::I915Reg,
    linux::{
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, INTEL_INFO, IP_VER, IS_DG2, IS_GFX_GT_IP_STEP,
            MEDIA_VER, STEP_A0, STEP_B0, VDBOX_MASK, VEBOX_MASK, i915_mmio_reg_offset,
        },
        locks::{
            spin_lock, spin_lock_init, spin_lock_irqsave, spin_unlock, spin_unlock_irqrestore,
        },
        primitives::jiffies,
        registers::{
            GEN8_MCR_SELECTOR, GEN8_MCR_SLICE, GEN8_MCR_SLICE_MASK, GEN8_MCR_SUBSLICE,
            GEN8_MCR_SUBSLICE_MASK, GEN11_MCR_SLICE, GEN11_MCR_SLICE_MASK, GEN11_MCR_SUBSLICE,
            GEN11_MCR_SUBSLICE_MASK, REG_FIELD_GET, REG_FIELD_PREP,
        },
    },
    linux_i915_private::DrmI915Private,
};

const MTL_STEER_SEMAPHORE: I915Reg = I915Reg { reg: 0x0fd0 };
const MTL_MCR_SELECTOR: I915Reg = I915Reg { reg: 0x0fd4 };
const MTL_MCR_GROUPID: u32 = 0xf << 8;
const MTL_MCR_INSTANCEID: u32 = 0xf;
const XEHP_FUSE4: I915Reg = I915Reg { reg: 0x9114 };
const GT_L3_EXC_MASK: u32 = 0x7 << 4;
const MTL_GT_ACTIVITY_FACTOR: I915Reg = I915Reg { reg: 0x138010 };
const MTL_GT_L3_EXC_MASK: u32 = 0x7 << 3;
const GEN10_MIRROR_FUSE3: I915Reg = I915Reg { reg: 0x9118 };
const GEN10_L3BANK_MASK: u32 = 0x0f;
const GEN11_MCR_MULTICAST: u32 = 1 << 31;
const GEN12_MEML3_EN_MASK: u32 = 0x0f;
const TAINT_WARN: u32 = 1 << 9;
const CI_TAINT_RATELIMIT_JIFFIES: u64 = 5 * crate::linux_config::HZ as u64;

static CI_TAINT_FLAGS: AtomicU32 = AtomicU32::new(0);
static LAST_MCR_LOCK_WARNING: AtomicU64 = AtomicU64::new(0);

/// Linux's i915 CI taint is kernel-global. TheKernel has no `/proc/sys/kernel/tainted`
/// ABI, so retain the same sticky bit in the LinuxKPI state and emit its notice.
fn add_taint_for_ci(i915: *mut DrmI915Private, taint: u32) {
    drm_notice!(
        unsafe { &mut (*i915).drm },
        "CI tainted: %#x by LinuxKPI MCR steering\n",
        taint
    );
    CI_TAINT_FLAGS.fetch_or(taint, Ordering::AcqRel);
}

/// Current LinuxKPI CI-taint word, kept sticky like Linux's global taint mask.
pub fn intel_gt_mcr_ci_taint_flags() -> u32 {
    CI_TAINT_FLAGS.load(Ordering::Acquire)
}

fn gt_err_ratelimited_mcr(gt: *mut IntelGt) {
    let now = jiffies().max(1);
    let mut old = LAST_MCR_LOCK_WARNING.load(Ordering::Acquire);
    loop {
        if old != 0 && now.wrapping_sub(old) < CI_TAINT_RATELIMIT_JIFFIES {
            return;
        }
        match LAST_MCR_LOCK_WARNING.compare_exchange_weak(
            old,
            now,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                gt_err!(gt, "hardware MCR steering semaphore timed out\n");
                return;
            }
            Err(actual) => old = actual,
        }
    }
}

#[inline]
unsafe fn has_mslice_steering(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    // `has_mslice_steering` is DEV_INFO_FOR_EACH_FLAG bit 22.
    unsafe { (*info).flags[2] & (1 << 6) != 0 }
}

#[inline]
fn reg(reg: I915Reg) -> I915RegT {
    I915RegT { reg: reg.reg }
}

static INTEL_STEERING_TYPES: [&str; NUM_STEERING_TYPES as usize] = [
    "L3BANK",
    "MSLICE",
    "LNCF",
    "GAM",
    "DSS",
    "OADDRM",
    "INSTANCE 0",
];

static ICL_L3BANK_STEERING_TABLE: [IntelMmioRange; 2] = [
    IntelMmioRange {
        start: 0x00b100,
        end: 0x00b3ff,
    },
    IntelMmioRange { start: 0, end: 0 },
];

static DG2_MSLICE_STEERING_TABLE: [IntelMmioRange; 3] = [
    IntelMmioRange {
        start: 0x00dd00,
        end: 0x00ddff,
    },
    IntelMmioRange {
        start: 0x00e900,
        end: 0x00ffff,
    },
    IntelMmioRange { start: 0, end: 0 },
];

static DG2_LNCF_STEERING_TABLE: [IntelMmioRange; 3] = [
    IntelMmioRange {
        start: 0x00b000,
        end: 0x00b0ff,
    },
    IntelMmioRange {
        start: 0x00d880,
        end: 0x00d8ff,
    },
    IntelMmioRange { start: 0, end: 0 },
];

static XELPG_INSTANCE0_STEERING_TABLE: [IntelMmioRange; 9] = [
    IntelMmioRange {
        start: 0x000b00,
        end: 0x000bff,
    },
    IntelMmioRange {
        start: 0x001000,
        end: 0x001fff,
    },
    IntelMmioRange {
        start: 0x004000,
        end: 0x0048ff,
    },
    IntelMmioRange {
        start: 0x008700,
        end: 0x0087ff,
    },
    IntelMmioRange {
        start: 0x00b000,
        end: 0x00b0ff,
    },
    IntelMmioRange {
        start: 0x00c800,
        end: 0x00cfff,
    },
    IntelMmioRange {
        start: 0x00d880,
        end: 0x00d8ff,
    },
    IntelMmioRange {
        start: 0x00dd00,
        end: 0x00ddff,
    },
    IntelMmioRange { start: 0, end: 0 },
];

static XELPG_L3BANK_STEERING_TABLE: [IntelMmioRange; 2] = [
    IntelMmioRange {
        start: 0x00b100,
        end: 0x00b3ff,
    },
    IntelMmioRange { start: 0, end: 0 },
];

static XELPG_DSS_STEERING_TABLE: [IntelMmioRange; 9] = [
    IntelMmioRange {
        start: 0x005200,
        end: 0x0052ff,
    },
    IntelMmioRange {
        start: 0x005500,
        end: 0x007fff,
    },
    IntelMmioRange {
        start: 0x008140,
        end: 0x00815f,
    },
    IntelMmioRange {
        start: 0x0094d0,
        end: 0x00955f,
    },
    IntelMmioRange {
        start: 0x009680,
        end: 0x0096ff,
    },
    IntelMmioRange {
        start: 0x00d800,
        end: 0x00d87f,
    },
    IntelMmioRange {
        start: 0x00dc00,
        end: 0x00dcff,
    },
    IntelMmioRange {
        start: 0x00de80,
        end: 0x00e8ff,
    },
    IntelMmioRange { start: 0, end: 0 },
];

static XELPMP_OADDRM_STEERING_TABLE: [IntelMmioRange; 3] = [
    IntelMmioRange {
        start: 0x393200,
        end: 0x39323f,
    },
    IntelMmioRange {
        start: 0x393400,
        end: 0x3934ff,
    },
    IntelMmioRange { start: 0, end: 0 },
];

// upstream: intel_gt_mcr.c intel_gt_mcr_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_init(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    unsafe { spin_lock_init(&mut (*gt).mcr_lock) };

    if unsafe { has_mslice_steering(i915) } {
        let mask = unsafe {
            crate::intel_sseu_upstream::intel_slicemask_from_xehp_dssmask(
                (*gt).info.sseu.subslice_mask,
                GEN_DSS_PER_MSLICE as i32,
            )
        };
        unsafe { (*gt).info.mslice_mask = mask as c_ulong };
        let meml3 = unsafe { intel_uncore_read((*gt).uncore, reg(GEN10_MIRROR_FUSE3)) };
        unsafe {
            (*gt).info.mslice_mask |= REG_FIELD_GET(GEN12_MEML3_EN_MASK, meml3) as c_ulong;
        }
        if unsafe { (*gt).info.mslice_mask } == 0 {
            drm_warn!(
                unsafe { &mut (*i915).drm },
                "GT%u: mslice mask all zero!\n",
                unsafe { (*gt).info.id }
            );
        }
    }

    if unsafe { MEDIA_VER(i915) } >= 13 && unsafe { (*gt).type_ } == GT_MEDIA {
        unsafe { (*gt).steering_table[OADDRM as usize] = XELPMP_OADDRM_STEERING_TABLE.as_ptr() };
    } else if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 70) {
        let fuse = if unsafe {
            IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
                || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0)
        } {
            REG_FIELD_GET(MTL_GT_L3_EXC_MASK, unsafe {
                intel_uncore_read((*gt).uncore, reg(MTL_GT_ACTIVITY_FACTOR))
            })
        } else {
            REG_FIELD_GET(GT_L3_EXC_MASK, unsafe {
                intel_uncore_read((*gt).uncore, reg(XEHP_FUSE4))
            })
        };
        for bank in 0..3 {
            if fuse & (1 << bank) != 0 {
                unsafe { (*gt).info.l3bank_mask |= 0x3 << (2 * bank) };
            }
        }
        unsafe {
            (*gt).steering_table[INSTANCE0 as usize] = XELPG_INSTANCE0_STEERING_TABLE.as_ptr();
            (*gt).steering_table[L3BANK as usize] = XELPG_L3BANK_STEERING_TABLE.as_ptr();
            (*gt).steering_table[DSS as usize] = XELPG_DSS_STEERING_TABLE.as_ptr();
        }
    } else if unsafe { IS_DG2(i915) } {
        unsafe {
            (*gt).steering_table[MSLICE as usize] = DG2_MSLICE_STEERING_TABLE.as_ptr();
            (*gt).steering_table[LNCF as usize] = DG2_LNCF_STEERING_TABLE.as_ptr();
        }
    } else if unsafe { GRAPHICS_VER(i915) } >= 11
        && unsafe { GRAPHICS_VER_FULL(i915) } < IP_VER(12, 55)
    {
        unsafe { (*gt).steering_table[L3BANK as usize] = ICL_L3BANK_STEERING_TABLE.as_ptr() };
        let fuse3 = unsafe { intel_uncore_read((*gt).uncore, reg(GEN10_MIRROR_FUSE3)) };
        unsafe { (*gt).info.l3bank_mask = !fuse3 & GEN10_L3BANK_MASK };
        if unsafe { (*gt).info.l3bank_mask } == 0 {
            drm_warn!(
                unsafe { &mut (*i915).drm },
                "GT%u: L3 bank mask is all zero!\n",
                unsafe { (*gt).info.id }
            );
        }
    } else if unsafe { GRAPHICS_VER(i915) } >= 11 {
        MISSING_CASE!(unsafe { GRAPHICS_VER_FULL(i915) });
    }
}

// upstream: intel_gt_mcr.c mcr_reg_cast()
unsafe fn mcr_reg_cast(mcr: I915McrReg) -> I915Reg {
    I915Reg { reg: mcr.reg }
}

// upstream: intel_gt_mcr.c rw_with_mcr_steering_fw()
unsafe fn rw_with_mcr_steering_fw(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    rw_flag: u8,
    group: i32,
    instance: i32,
    value: u32,
) -> u32 {
    let uncore = unsafe { (*gt).uncore };
    let i915 = unsafe { (*uncore).i915 };
    let mut mcr_mask;
    let mcr_ss;
    let mut mcr;
    let mut old_mcr = 0;
    let mut val = 0;

    lockdep_assert_held!(unsafe { &(*gt).mcr_lock });
    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 70) {
        let selector = REG_FIELD_PREP(MTL_MCR_GROUPID, group as u32)
            | REG_FIELD_PREP(MTL_MCR_INSTANCEID, instance as u32)
            | if rw_flag == FW_REG_READ as u8 {
                GEN11_MCR_MULTICAST
            } else {
                0
            };
        unsafe { intel_uncore_write_fw(uncore, reg(MTL_MCR_SELECTOR), selector) };
    } else if unsafe { GRAPHICS_VER(i915) } >= 11 {
        mcr_mask = GEN11_MCR_SLICE_MASK | GEN11_MCR_SUBSLICE_MASK;
        mcr_ss = GEN11_MCR_SLICE(group as u32) | GEN11_MCR_SUBSLICE(instance as u32);
        if rw_flag == FW_REG_WRITE as u8 {
            mcr_mask |= GEN11_MCR_MULTICAST;
        }
        mcr = unsafe { intel_uncore_read_fw(uncore, reg(GEN8_MCR_SELECTOR)) };
        old_mcr = mcr;
        mcr &= !mcr_mask;
        mcr |= mcr_ss;
        unsafe { intel_uncore_write_fw(uncore, reg(GEN8_MCR_SELECTOR), mcr) };
    } else {
        mcr_mask = GEN8_MCR_SLICE_MASK | GEN8_MCR_SUBSLICE_MASK;
        mcr_ss = GEN8_MCR_SLICE(group as u32) | GEN8_MCR_SUBSLICE(instance as u32);
        mcr = unsafe { intel_uncore_read_fw(uncore, reg(GEN8_MCR_SELECTOR)) };
        old_mcr = mcr;
        mcr &= !mcr_mask;
        mcr |= mcr_ss;
        unsafe { intel_uncore_write_fw(uncore, reg(GEN8_MCR_SELECTOR), mcr) };
    }

    if rw_flag == FW_REG_READ as u8 {
        val = unsafe { intel_uncore_read_fw(uncore, reg(mcr_reg_cast(reg_))) };
    } else {
        unsafe { intel_uncore_write_fw(uncore, reg(mcr_reg_cast(reg_)), value) };
    }

    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 70) && rw_flag == FW_REG_WRITE as u8 {
        unsafe { intel_uncore_write_fw(uncore, reg(MTL_MCR_SELECTOR), GEN11_MCR_MULTICAST) };
    } else if unsafe { GRAPHICS_VER_FULL(i915) } < IP_VER(12, 70) {
        unsafe { intel_uncore_write_fw(uncore, reg(GEN8_MCR_SELECTOR), old_mcr) };
    }
    val
}

// upstream: intel_gt_mcr.c rw_with_mcr_steering()
unsafe fn rw_with_mcr_steering(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    rw_flag: u8,
    group: i32,
    instance: i32,
    value: u32,
) -> u32 {
    let uncore = unsafe { (*gt).uncore };
    let mut fw_domains =
        unsafe { intel_uncore_forcewake_for_reg(uncore, reg(mcr_reg_cast(reg_)), rw_flag as u32) };
    fw_domains |= unsafe {
        intel_uncore_forcewake_for_reg(uncore, reg(GEN8_MCR_SELECTOR), FW_REG_READ | FW_REG_WRITE)
    };

    let mut flags = 0;
    unsafe { intel_gt_mcr_lock(gt, &mut flags) };
    unsafe { spin_lock(&mut (*uncore).lock) };
    unsafe { intel_uncore_forcewake_get__locked(uncore, fw_domains) };
    let val = unsafe { rw_with_mcr_steering_fw(gt, reg_, rw_flag, group, instance, value) };
    unsafe { intel_uncore_forcewake_put__locked(uncore, fw_domains) };
    unsafe { spin_unlock(&mut (*uncore).lock) };
    unsafe { intel_gt_mcr_unlock(gt, flags) };
    val
}

// upstream: intel_gt_mcr.c intel_gt_mcr_lock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_lock(gt: *mut IntelGt, flags: *mut c_ulong) {
    let uncore = unsafe { (*gt).uncore };
    lockdep_assert_not_held!(unsafe { &(*uncore).lock });
    let i915 = unsafe { (*gt).i915 };
    let mut err = 0;
    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 70) {
        unsafe { intel_uncore_forcewake_get(uncore, FORCEWAKE_GT) };
        if wait_for!(
            unsafe { intel_uncore_read_fw(uncore, reg(MTL_STEER_SEMAPHORE)) == 0x1 },
            100
        ) {
            err = -crate::linux_config::ETIMEDOUT;
        }
    }

    let mut irq_flags = 0;
    unsafe { spin_lock_irqsave(&mut (*gt).mcr_lock, &mut irq_flags) };
    unsafe { *flags = irq_flags };

    if err == -crate::linux_config::ETIMEDOUT {
        gt_err_ratelimited_mcr(gt);
        add_taint_for_ci(i915, TAINT_WARN);
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_unlock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_unlock(gt: *mut IntelGt, flags: c_ulong) {
    unsafe { spin_unlock_irqrestore(&mut (*gt).mcr_lock, flags) };
    if unsafe { GRAPHICS_VER_FULL((*gt).i915) } >= IP_VER(12, 70) {
        unsafe { intel_uncore_write_fw((*gt).uncore, reg(MTL_STEER_SEMAPHORE), 0x1) };
        unsafe { intel_uncore_forcewake_put((*gt).uncore, FORCEWAKE_GT) };
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_lock_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_lock_sanitize(gt: *mut IntelGt) {
    lockdep_assert_not_held!(unsafe { &(*gt).mcr_lock });
    if unsafe { GRAPHICS_VER_FULL((*gt).i915) } >= IP_VER(12, 70) {
        unsafe { intel_uncore_write_fw((*gt).uncore, reg(MTL_STEER_SEMAPHORE), 0x1) };
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_read(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    group: i32,
    instance: i32,
) -> u32 {
    unsafe { rw_with_mcr_steering(gt, reg_, FW_REG_READ as u8, group, instance, 0) }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_unicast_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_unicast_write(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    value: u32,
    group: i32,
    instance: i32,
) {
    let _ = unsafe { rw_with_mcr_steering(gt, reg_, FW_REG_WRITE as u8, group, instance, value) };
}

// upstream: intel_gt_mcr.c intel_gt_mcr_multicast_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_multicast_write(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    value: u32,
) {
    let mut flags = 0;
    unsafe { intel_gt_mcr_lock(gt, &mut flags) };
    if unsafe { GRAPHICS_VER_FULL((*gt).i915) } >= IP_VER(12, 70) {
        unsafe { intel_uncore_write_fw((*gt).uncore, reg(MTL_MCR_SELECTOR), GEN11_MCR_MULTICAST) };
    }
    unsafe { intel_uncore_write((*gt).uncore, reg(mcr_reg_cast(reg_)), value) };
    unsafe { intel_gt_mcr_unlock(gt, flags) };
}

// upstream: intel_gt_mcr.c intel_gt_mcr_multicast_write_fw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_multicast_write_fw(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    value: u32,
) {
    lockdep_assert_held!(unsafe { &(*gt).mcr_lock });
    if unsafe { GRAPHICS_VER_FULL((*gt).i915) } >= IP_VER(12, 70) {
        unsafe { intel_uncore_write_fw((*gt).uncore, reg(MTL_MCR_SELECTOR), GEN11_MCR_MULTICAST) };
    }
    unsafe { intel_uncore_write_fw((*gt).uncore, reg(mcr_reg_cast(reg_)), value) };
}

// upstream: intel_gt_mcr.c intel_gt_mcr_multicast_rmw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_multicast_rmw(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    clear: u32,
    set: u32,
) -> u32 {
    let val = unsafe { intel_gt_mcr_read_any(gt, reg_) };
    unsafe { intel_gt_mcr_multicast_write(gt, reg_, (val & !clear) | set) };
    val
}

// upstream: intel_gt_mcr.c reg_needs_read_steering()
unsafe fn reg_needs_read_steering(gt: *mut IntelGt, reg_: I915McrReg, type_: i32) -> bool {
    let table = unsafe { (*gt).steering_table[type_ as usize] };
    if likely!(!table.is_null()) {
        let mut offset = i915_mmio_reg_offset(reg_);
        if IS_GSI_REG(offset) {
            offset = offset.wrapping_add(unsafe { (*(*gt).uncore).gsi_offset });
        }
        let mut entry = table;
        while unsafe { (*entry).end } != 0 {
            if offset >= unsafe { (*entry).start } && offset <= unsafe { (*entry).end } {
                return true;
            }
            entry = unsafe { entry.add(1) };
        }
    }
    false
}

// upstream: intel_gt_mcr.c get_nonterminated_steering()
unsafe fn get_nonterminated_steering(
    gt: *mut IntelGt,
    type_: i32,
    group: *mut u8,
    instance: *mut u8,
) {
    let i915 = unsafe { (*gt).i915 };
    match type_ {
        L3BANK => unsafe {
            *group = 0;
            *instance = (*gt).info.l3bank_mask.trailing_zeros() as u8;
        },
        MSLICE => {
            GEM_WARN_ON!(!unsafe { has_mslice_steering(i915) });
            unsafe {
                *group = (*gt).info.mslice_mask.trailing_zeros() as u8;
                *instance = 0;
            }
        }
        LNCF => {
            GEM_WARN_ON!(!unsafe { has_mslice_steering(i915) });
            unsafe {
                *group = ((*gt).info.mslice_mask.trailing_zeros() << 1) as u8;
                *instance = 0;
            }
        }
        GAM => unsafe {
            *group = if IS_DG2(i915) { 1 } else { 0 };
            *instance = 0;
        },
        DSS => unsafe {
            let dss = intel_sseu_find_first_xehp_dss(&(*gt).info.sseu, 0, 0);
            *group = (dss / GEN_DSS_PER_GSLICE as u32) as u8;
            *instance = (dss % GEN_DSS_PER_GSLICE as u32) as u8;
        },
        INSTANCE0 => unsafe {
            *group = 0;
            *instance = 0;
        },
        OADDRM => unsafe {
            if (VDBOX_MASK(gt) | VEBOX_MASK(gt) | (*gt).info.sfc_mask as u32) & 1 != 0 {
                *group = 0;
            } else {
                *group = 1;
            }
            *instance = 0;
        },
        other => unsafe {
            MISSING_CASE!(other);
            *group = 0;
            *instance = 0;
        },
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_get_nonterminated_steering()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_get_nonterminated_steering(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    group: *mut u8,
    instance: *mut u8,
) {
    for type_ in 0..NUM_STEERING_TYPES {
        if unsafe { reg_needs_read_steering(gt, reg_, type_) } {
            unsafe { get_nonterminated_steering(gt, type_, group, instance) };
            return;
        }
    }
    unsafe {
        *group = (*gt).default_steering.groupid;
        *instance = (*gt).default_steering.instanceid;
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_read_any_fw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_read_any_fw(gt: *mut IntelGt, reg_: I915McrReg) -> u32 {
    lockdep_assert_held!(unsafe { &(*gt).mcr_lock });
    for type_ in 0..NUM_STEERING_TYPES {
        if unsafe { reg_needs_read_steering(gt, reg_, type_) } {
            let mut group = 0;
            let mut instance = 0;
            unsafe { get_nonterminated_steering(gt, type_, &mut group, &mut instance) };
            return unsafe {
                rw_with_mcr_steering_fw(
                    gt,
                    reg_,
                    FW_REG_READ as u8,
                    group as i32,
                    instance as i32,
                    0,
                )
            };
        }
    }
    unsafe { intel_uncore_read_fw((*gt).uncore, reg(mcr_reg_cast(reg_))) }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_read_any()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_read_any(gt: *mut IntelGt, reg_: I915McrReg) -> u32 {
    for type_ in 0..NUM_STEERING_TYPES {
        if unsafe { reg_needs_read_steering(gt, reg_, type_) } {
            let mut group = 0;
            let mut instance = 0;
            unsafe { get_nonterminated_steering(gt, type_, &mut group, &mut instance) };
            return unsafe {
                rw_with_mcr_steering(
                    gt,
                    reg_,
                    FW_REG_READ as u8,
                    group as i32,
                    instance as i32,
                    0,
                )
            };
        }
    }
    unsafe { intel_uncore_read((*gt).uncore, reg(mcr_reg_cast(reg_))) }
}

// upstream: intel_gt_mcr.c report_steering_type()
unsafe fn report_steering_type(p: *mut DrmPrinter, gt: *mut IntelGt, type_: i32, dump_table: bool) {
    const _: [(); NUM_STEERING_TYPES as usize] = [(); INTEL_STEERING_TYPES.len()];
    let table = unsafe { (*gt).steering_table[type_ as usize] };
    if table.is_null() {
        drm_printf!(
            p,
            "%s steering: uses default steering\n",
            INTEL_STEERING_TYPES[type_ as usize]
        );
        return;
    }
    let mut group = 0;
    let mut instance = 0;
    unsafe { get_nonterminated_steering(gt, type_, &mut group, &mut instance) };
    drm_printf!(
        p,
        "%s steering: group=0x%x, instance=0x%x\n",
        INTEL_STEERING_TYPES[type_ as usize],
        group,
        instance
    );
    if !dump_table {
        return;
    }
    let mut entry = table;
    while unsafe { (*entry).end } != 0 {
        drm_printf!(
            p,
            "\t0x%06x - 0x%06x\n",
            unsafe { (*entry).start },
            unsafe { (*entry).end }
        );
        entry = unsafe { entry.add(1) };
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_report_steering()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_report_steering(
    p: *mut DrmPrinter,
    gt: *mut IntelGt,
    dump_table: bool,
) {
    let i915 = unsafe { (*gt).i915 };
    if unsafe { GRAPHICS_VER_FULL(i915) } < IP_VER(12, 70) {
        drm_printf!(
            p,
            "Default steering: group=0x%x, instance=0x%x\n",
            unsafe { (*gt).default_steering.groupid },
            unsafe { (*gt).default_steering.instanceid }
        );
    }
    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 70) {
        for type_ in 0..NUM_STEERING_TYPES {
            if !unsafe { (*gt).steering_table[type_ as usize] }.is_null() {
                unsafe { report_steering_type(p, gt, type_, dump_table) };
            }
        }
    } else if unsafe { has_mslice_steering(i915) } {
        unsafe { report_steering_type(p, gt, MSLICE, dump_table) };
        unsafe { report_steering_type(p, gt, LNCF, dump_table) };
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_get_ss_steering()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_get_ss_steering(
    gt: *mut IntelGt,
    dss: u32,
    group: *mut u32,
    instance: *mut u32,
) {
    if unsafe { GRAPHICS_VER_FULL((*gt).i915) } >= IP_VER(12, 55) {
        unsafe {
            *group = dss / GEN_DSS_PER_GSLICE as u32;
            *instance = dss % GEN_DSS_PER_GSLICE as u32;
        }
    } else {
        unsafe {
            *group = dss / GEN_MAX_SS_PER_HSW_SLICE as u32;
            *instance = dss % GEN_MAX_SS_PER_HSW_SLICE as u32;
        }
    }
}

// upstream: intel_gt_mcr.c intel_gt_mcr_wait_for_reg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_mcr_wait_for_reg(
    gt: *mut IntelGt,
    reg_: I915McrReg,
    mask: u32,
    value: u32,
    fast_timeout_us: u32,
    slow_timeout_ms: u32,
) -> i32 {
    lockdep_assert_not_held!(unsafe { &(*gt).mcr_lock });
    if slow_timeout_ms != 0 {
        crate::linux::wait::might_sleep();
    }
    GEM_BUG_ON!(fast_timeout_us > 20_000);
    GEM_BUG_ON!(fast_timeout_us == 0 && slow_timeout_ms == 0);

    let mut ret = -crate::linux_config::ETIMEDOUT;
    if fast_timeout_us != 0 && fast_timeout_us <= 20_000 {
        let timed_out = wait_for_atomic_us!(
            unsafe { intel_gt_mcr_read_any(gt, reg_) & mask == value },
            fast_timeout_us
        );
        if !timed_out {
            ret = 0;
        }
    }
    if ret != 0 && slow_timeout_ms != 0 {
        let timed_out = wait_for!(
            unsafe { intel_gt_mcr_read_any(gt, reg_) & mask == value },
            slow_timeout_ms
        );
        ret = if timed_out {
            -crate::linux_config::ETIMEDOUT
        } else {
            0
        };
    }
    ret
}
