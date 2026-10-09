// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//! Source-order Linux 7.2.3
//! `drivers/gpu/drm/i915/gt/intel_gt_pm_irq.c` translation.

#![allow(unsafe_code)]

use crate::{
    intel_gt_types_upstream::IntelGt,
    intel_uncore_types_upstream::{intel_uncore_posting_read, intel_uncore_write},
    intel_workarounds_types_upstream::I915RegT,
    linux::i915::GRAPHICS_VER,
};

const GEN6_PMIMR: I915RegT = I915RegT { reg: 0x44024 };
const GEN6_PMIIR: I915RegT = I915RegT { reg: 0x44028 };
const GEN6_PMIER: I915RegT = I915RegT { reg: 0x4402c };
const GEN8_GT_IMR2: I915RegT = I915RegT { reg: 0x44324 };
const GEN8_GT_IIR2: I915RegT = I915RegT { reg: 0x44328 };
const GEN8_GT_IER2: I915RegT = I915RegT { reg: 0x4432c };
const GEN11_GPM_WGBOXPERF_INTR_ENABLE: I915RegT = I915RegT { reg: 0x19003c };
const GEN11_GPM_WGBOXPERF_INTR_MASK: I915RegT = I915RegT { reg: 0x1900ec };

// upstream: intel_gt_pm_irq.c write_pm_imr()
unsafe fn write_pm_imr(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    let mut mask = unsafe { (*gt).pm_imr };
    let reg = if unsafe { GRAPHICS_VER(i915) >= 11 } {
        mask <<= 16;
        GEN11_GPM_WGBOXPERF_INTR_MASK
    } else if unsafe { GRAPHICS_VER(i915) >= 8 } {
        GEN8_GT_IMR2
    } else {
        GEN6_PMIMR
    };
    unsafe { intel_uncore_write(uncore, reg, mask) };
}

// upstream: intel_gt_pm_irq.c gen6_gt_pm_update_irq()
unsafe fn gen6_gt_pm_update_irq(
    gt: *mut IntelGt,
    interrupt_mask: u32,
    enabled_irq_mask: u32,
) {
    WARN_ON!(enabled_irq_mask & !interrupt_mask != 0);
    lockdep_assert_held!(unsafe { (*gt).irq_lock });

    let old = unsafe { (*gt).pm_imr };
    let new = (old & !interrupt_mask) | (!enabled_irq_mask & interrupt_mask);
    if new != old {
        unsafe {
            (*gt).pm_imr = new;
            write_pm_imr(gt);
        }
    }
}

// upstream: intel_gt_pm_irq.c gen6_gt_pm_unmask_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_gt_pm_unmask_irq(gt: *mut IntelGt, mask: u32) {
    unsafe { gen6_gt_pm_update_irq(gt, mask, mask) };
}

// upstream: intel_gt_pm_irq.c gen6_gt_pm_mask_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_gt_pm_mask_irq(gt: *mut IntelGt, mask: u32) {
    unsafe { gen6_gt_pm_update_irq(gt, mask, 0) };
}

// upstream: intel_gt_pm_irq.c gen6_gt_pm_reset_iir()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_gt_pm_reset_iir(gt: *mut IntelGt, reset_mask: u32) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    let reg = if unsafe { GRAPHICS_VER(i915) >= 8 } {
        GEN8_GT_IIR2
    } else {
        GEN6_PMIIR
    };
    lockdep_assert_held!(unsafe { (*gt).irq_lock });
    unsafe {
        intel_uncore_write(uncore, reg, reset_mask);
        intel_uncore_write(uncore, reg, reset_mask);
        intel_uncore_posting_read(uncore, reg);
    }
}

// upstream: intel_gt_pm_irq.c write_pm_ier()
unsafe fn write_pm_ier(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    let mut mask = unsafe { (*gt).pm_ier };
    let reg = if unsafe { GRAPHICS_VER(i915) >= 11 } {
        mask <<= 16;
        GEN11_GPM_WGBOXPERF_INTR_ENABLE
    } else if unsafe { GRAPHICS_VER(i915) >= 8 } {
        GEN8_GT_IER2
    } else {
        GEN6_PMIER
    };
    unsafe { intel_uncore_write(uncore, reg, mask) };
}

// upstream: intel_gt_pm_irq.c gen6_gt_pm_enable_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_gt_pm_enable_irq(gt: *mut IntelGt, enable_mask: u32) {
    lockdep_assert_held!(unsafe { (*gt).irq_lock });
    unsafe {
        (*gt).pm_ier |= enable_mask;
        write_pm_ier(gt);
        gen6_gt_pm_unmask_irq(gt, enable_mask);
    }
}

// upstream: intel_gt_pm_irq.c gen6_gt_pm_disable_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_gt_pm_disable_irq(gt: *mut IntelGt, disable_mask: u32) {
    lockdep_assert_held!(unsafe { (*gt).irq_lock });
    unsafe {
        (*gt).pm_ier &= !disable_mask;
        gen6_gt_pm_mask_irq(gt, disable_mask);
        write_pm_ier(gt);
    }
}
