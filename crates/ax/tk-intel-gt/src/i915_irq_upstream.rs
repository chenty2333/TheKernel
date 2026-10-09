// SPDX-License-Identifier: MIT
// Copyright 2003 Tungsten Graphics, Inc., Cedar Park, Texas.
// Source: Linux v7.2.3 drivers/gpu/drm/i915/i915_irq.c.
// The complete upstream permission and warranty disclaimer are reproduced here.
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
// The above copyright notice and this permission notice (including the next
// paragraph) shall be included in all copies or substantial portions of the
// Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NON-INFRINGEMENT. IN NO EVENT SHALL
// TUNGSTEN GRAPHICS AND/OR ITS SUPPLIERS BE LIABLE FOR ANY CLAIM, DAMAGES OR
// OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
// ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use crate::{
    intel_gt_types_upstream::IntelGt,
    intel_uncore_types_upstream::{
        IntelUncore, intel_uncore_posting_read, intel_uncore_read, intel_uncore_regs,
        intel_uncore_write, raw_reg_read, raw_reg_write,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux_i915_private::{DrmI915Private, intel_irqs_enabled},
};

const IRQ_NONE: i32 = 0;
const IRQ_HANDLED: i32 = 1;
const GEN11_GFX_MSTR_IRQ: I915RegT = reg(0x190010);
const DG1_MSTR_TILE_INTR: I915RegT = reg(0x190008);
const GEN11_MASTER_IRQ: u32 = 1 << 31;
const GEN11_DISPLAY_IRQ: u32 = 1 << 16;
const DG1_MSTR_IRQ: u32 = 1 << 31;
const DG1_MSTR_TILE0: u32 = 1;
const GEN11_GU_MISC_IMR: I915RegT = reg(0x444f4);
const GEN11_GU_MISC_IIR: I915RegT = reg(0x444f8);
const GEN11_GU_MISC_IER: I915RegT = reg(0x444fc);
const GEN11_GU_MISC_GSE: u32 = 1 << 27;
const GEN8_PCU_IMR: I915RegT = reg(0x444e4);
const GEN8_PCU_IIR: I915RegT = reg(0x444e8);
const GEN8_PCU_IER: I915RegT = reg(0x444ec);

const fn reg(reg: u32) -> I915RegT {
    I915RegT { reg }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915IrqRegs {
    pub imr: I915RegT,
    pub ier: I915RegT,
    pub iir: I915RegT,
}

const GEN11_GU_MISC_IRQ_REGS: I915IrqRegs = I915IrqRegs {
    imr: GEN11_GU_MISC_IMR,
    ier: GEN11_GU_MISC_IER,
    iir: GEN11_GU_MISC_IIR,
};
const GEN8_PCU_IRQ_REGS: I915IrqRegs = I915IrqRegs {
    imr: GEN8_PCU_IMR,
    ier: GEN8_PCU_IER,
    iir: GEN8_PCU_IIR,
};

#[inline]
unsafe fn i915_uncore(i915: *mut DrmI915Private) -> *mut IntelUncore {
    let gt = unsafe { (*i915).gt[0] };
    assert!(!gt.is_null(), "GT0 must own the device uncore");
    unsafe { (*gt).uncore }
}

/// Display-specific work stays in the kernel display IRQ owner. These hooks
/// preserve the exact call ordering around GT acknowledge and misc IRQ handling.
pub trait Gen11DisplayIrqHooks {
    unsafe fn display_irq_handler(&mut self, display: *mut c_void);
    unsafe fn gu_misc_irq_ack(&mut self, display: *mut c_void, master_ctl: u32) -> u32;
    unsafe fn gu_misc_irq_handler(&mut self, display: *mut c_void, gu_misc_iir: u32);
    unsafe fn display_irq_reset(&mut self, display: *mut c_void);
    unsafe fn display_irq_postinstall(&mut self, display: *mut c_void);
    unsafe fn pmu_irq_stats(&mut self, i915: *mut DrmI915Private);
}

// upstream: i915_irq.c pmu_irq_stats()
unsafe fn pmu_irq_stats<H: Gen11DisplayIrqHooks>(
    i915: *mut DrmI915Private,
    result: i32,
    hooks: &mut H,
) {
    if result != IRQ_HANDLED {
        return;
    }
    unsafe { hooks.pmu_irq_stats(i915) };
}

// upstream: i915_irq.c gen2_irq_reset()
unsafe fn gen2_irq_reset(uncore: *mut IntelUncore, regs: I915IrqRegs) {
    unsafe {
        intel_uncore_write(uncore, regs.imr, u32::MAX);
        intel_uncore_posting_read(uncore, regs.imr);
        intel_uncore_write(uncore, regs.ier, 0);
        intel_uncore_write(uncore, regs.iir, u32::MAX);
        intel_uncore_posting_read(uncore, regs.iir);
        intel_uncore_write(uncore, regs.iir, u32::MAX);
        intel_uncore_posting_read(uncore, regs.iir);
    }
}

// upstream: i915_irq.c gen2_irq_init()
unsafe fn gen2_irq_init(uncore: *mut IntelUncore, regs: I915IrqRegs, imr: u32, ier: u32) {
    let value = unsafe { intel_uncore_read(uncore, regs.iir) };
    if value != 0 {
        unsafe {
            drm_warn!(
                &(*(*uncore).i915).drm,
                "Interrupt register 0x%x is not zero: 0x%08x\n",
                regs.iir.reg,
                value
            )
        };
        unsafe {
            intel_uncore_write(uncore, regs.iir, u32::MAX);
            intel_uncore_posting_read(uncore, regs.iir);
            intel_uncore_write(uncore, regs.iir, u32::MAX);
            intel_uncore_posting_read(uncore, regs.iir);
        }
    }
    unsafe {
        intel_uncore_write(uncore, regs.ier, ier);
        intel_uncore_write(uncore, regs.imr, imr);
        intel_uncore_posting_read(uncore, regs.imr);
    }
}

// upstream: i915_irq.c gen11_master_intr_disable()
unsafe fn gen11_master_intr_disable(regs: *mut u8) -> u32 {
    unsafe {
        raw_reg_write(regs, GEN11_GFX_MSTR_IRQ, 0);
        raw_reg_read(regs, GEN11_GFX_MSTR_IRQ)
    }
}

// upstream: i915_irq.c gen11_master_intr_enable()
unsafe fn gen11_master_intr_enable(regs: *mut u8) {
    unsafe { raw_reg_write(regs, GEN11_GFX_MSTR_IRQ, GEN11_MASTER_IRQ) };
}

// upstream: i915_irq.c gen11_irq_handler()
pub unsafe fn gen11_irq_handler<H: Gen11DisplayIrqHooks>(
    i915: *mut DrmI915Private,
    hooks: &mut H,
) -> i32 {
    if !unsafe { intel_irqs_enabled(i915) } {
        return IRQ_NONE;
    }
    let uncore = unsafe { i915_uncore(i915) };
    let regs = unsafe { intel_uncore_regs(uncore) }.cast::<u8>();
    let gt = unsafe { (*i915).gt[0] };
    let display = unsafe { (*i915).display };

    let master_ctl = unsafe { gen11_master_intr_disable(regs) };
    if master_ctl == 0 {
        unsafe { gen11_master_intr_enable(regs) };
        return IRQ_NONE;
    }

    unsafe { crate::intel_gt_irq_upstream::gen11_gt_irq_handler(gt, master_ctl) };
    if master_ctl & GEN11_DISPLAY_IRQ != 0 {
        unsafe { hooks.display_irq_handler(display) };
    }
    let gu_misc_iir = unsafe { hooks.gu_misc_irq_ack(display, master_ctl) };
    unsafe { gen11_master_intr_enable(regs) };
    unsafe { hooks.gu_misc_irq_handler(display, gu_misc_iir) };
    unsafe { pmu_irq_stats(i915, IRQ_HANDLED, hooks) };
    IRQ_HANDLED
}

// upstream: i915_irq.c dg1_master_intr_disable()
unsafe fn dg1_master_intr_disable(regs: *mut u8) -> u32 {
    unsafe { raw_reg_write(regs, DG1_MSTR_TILE_INTR, 0) };
    let value = unsafe { raw_reg_read(regs, DG1_MSTR_TILE_INTR) };
    if value != 0 {
        unsafe { raw_reg_write(regs, DG1_MSTR_TILE_INTR, value) };
    }
    value
}

// upstream: i915_irq.c dg1_master_intr_enable()
unsafe fn dg1_master_intr_enable(regs: *mut u8) {
    unsafe { raw_reg_write(regs, DG1_MSTR_TILE_INTR, DG1_MSTR_IRQ) };
}

// upstream: i915_irq.c dg1_irq_handler()
pub unsafe fn dg1_irq_handler<H: Gen11DisplayIrqHooks>(
    i915: *mut DrmI915Private,
    hooks: &mut H,
) -> i32 {
    if !unsafe { intel_irqs_enabled(i915) } {
        return IRQ_NONE;
    }
    let uncore = unsafe { i915_uncore(i915) };
    let regs = unsafe { intel_uncore_regs(uncore) }.cast::<u8>();
    let display = unsafe { (*i915).display };
    let master_tile_ctl = unsafe { dg1_master_intr_disable(regs) };
    if master_tile_ctl == 0 {
        unsafe { dg1_master_intr_enable(regs) };
        return IRQ_NONE;
    }
    if master_tile_ctl & DG1_MSTR_TILE0 == 0 {
        unsafe {
            drm_err!(
                &(*i915).drm,
                "Tile not supported: 0x%08x\n",
                master_tile_ctl
            )
        };
        unsafe { dg1_master_intr_enable(regs) };
        return IRQ_NONE;
    }
    let master_ctl = unsafe { raw_reg_read(regs, GEN11_GFX_MSTR_IRQ) };
    unsafe { raw_reg_write(regs, GEN11_GFX_MSTR_IRQ, master_ctl) };
    let gt = unsafe { (*i915).gt[0] };
    unsafe { crate::intel_gt_irq_upstream::gen11_gt_irq_handler(gt, master_ctl) };
    if master_ctl & GEN11_DISPLAY_IRQ != 0 {
        unsafe { hooks.display_irq_handler(display) };
    }
    let gu_misc_iir = unsafe { hooks.gu_misc_irq_ack(display, master_ctl) };
    unsafe { dg1_master_intr_enable(regs) };
    unsafe { hooks.gu_misc_irq_handler(display, gu_misc_iir) };
    unsafe { pmu_irq_stats(i915, IRQ_HANDLED, hooks) };
    IRQ_HANDLED
}

// upstream: i915_irq.c gen11_irq_reset()
pub unsafe fn gen11_irq_reset<H: Gen11DisplayIrqHooks>(i915: *mut DrmI915Private, hooks: &mut H) {
    let uncore = unsafe { i915_uncore(i915) };
    let gt = unsafe { (*i915).gt[0] };
    unsafe {
        gen11_master_intr_disable(intel_uncore_regs(uncore).cast());
        crate::intel_gt_irq_upstream::gen11_gt_irq_reset(gt);
        hooks.display_irq_reset((*i915).display);
        gen2_irq_reset(uncore, GEN11_GU_MISC_IRQ_REGS);
        gen2_irq_reset(uncore, GEN8_PCU_IRQ_REGS);
    }
}

// upstream: i915_irq.c dg1_irq_reset()
pub unsafe fn dg1_irq_reset<H: Gen11DisplayIrqHooks>(i915: *mut DrmI915Private, hooks: &mut H) {
    let uncore = unsafe { i915_uncore(i915) };
    let regs = unsafe { intel_uncore_regs(uncore) }.cast();
    unsafe { dg1_master_intr_disable(regs) };
    crate::for_each_gt!(gt, i915, _id, {
        unsafe { crate::intel_gt_irq_upstream::gen11_gt_irq_reset(gt) };
    });
    unsafe {
        hooks.display_irq_reset((*i915).display);
        gen2_irq_reset(uncore, GEN11_GU_MISC_IRQ_REGS);
        gen2_irq_reset(uncore, GEN8_PCU_IRQ_REGS);
        intel_uncore_write(uncore, GEN11_GFX_MSTR_IRQ, u32::MAX);
    }
}

// upstream: i915_irq.c gen11_irq_postinstall()
pub unsafe fn gen11_irq_postinstall<H: Gen11DisplayIrqHooks>(
    i915: *mut DrmI915Private,
    hooks: &mut H,
) {
    let uncore = unsafe { i915_uncore(i915) };
    let gt = unsafe { (*i915).gt[0] };
    unsafe {
        crate::intel_gt_irq_upstream::gen11_gt_irq_postinstall(gt);
        hooks.display_irq_postinstall((*i915).display);
        gen2_irq_init(
            uncore,
            GEN11_GU_MISC_IRQ_REGS,
            !GEN11_GU_MISC_GSE,
            GEN11_GU_MISC_GSE,
        );
        gen11_master_intr_enable(intel_uncore_regs(uncore).cast());
        intel_uncore_posting_read(uncore, GEN11_GFX_MSTR_IRQ);
    }
}

// upstream: i915_irq.c dg1_irq_postinstall()
pub unsafe fn dg1_irq_postinstall<H: Gen11DisplayIrqHooks>(
    i915: *mut DrmI915Private,
    hooks: &mut H,
) {
    let uncore = unsafe { i915_uncore(i915) };
    crate::for_each_gt!(gt, i915, _id, {
        unsafe { crate::intel_gt_irq_upstream::gen11_gt_irq_postinstall(gt) };
    });
    unsafe {
        gen2_irq_init(
            uncore,
            GEN11_GU_MISC_IRQ_REGS,
            !GEN11_GU_MISC_GSE,
            GEN11_GU_MISC_GSE,
        );
        hooks.display_irq_postinstall((*i915).display);
        dg1_master_intr_enable(intel_uncore_regs(uncore).cast());
        intel_uncore_posting_read(uncore, DG1_MSTR_TILE_INTR);
    }
}
