// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_gt_irq.c.
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{ffi::c_void, ptr};

use crate::{
    intel_engine_cs_upstream::{
        BCS1, BCS2, BCS3, BCS4, BCS5, BCS6, BCS7, BCS8, CCS0, CCS1, CCS2, CCS3, GSC0, VCS4, VCS5,
        VCS6, VCS7, VECS0, VECS2, VECS3,
    },
    intel_engine_types_upstream::{
        COPY_ENGINE_CLASS, IntelEngineCs, MAX_ENGINE_CLASS, MAX_ENGINE_INSTANCE, OTHER_CLASS,
        RENDER_CLASS, VIDEO_DECODE_CLASS, VIDEO_ENHANCEMENT_CLASS,
    },
    intel_gsc_types_upstream::intel_gsc_irq_handler,
    intel_gt_api_upstream::gt_to_guc,
    intel_gt_types_upstream::{GT_MEDIA, IntelGt},
    intel_guc_ct_types_upstream::IntelGucCt,
    intel_guc_types_upstream::IntelGuc,
    intel_rps_types_upstream::IntelRps,
    intel_uncore_types_upstream::{
        IntelUncore, intel_uncore_posting_read, intel_uncore_posting_read_fw, intel_uncore_regs,
        intel_uncore_rmw, intel_uncore_write, raw_reg_read, raw_reg_write,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{CCS_MASK, GRAPHICS_VER, HAS_ENGINE},
        locks::{spin_lock, spin_unlock},
        registers::REG_FIELD_PREP,
        workqueue::queue_work,
    },
    linux_i915_private::DrmI915Private,
};

const GT_RENDER_USER_INTERRUPT: u32 = 1 << 0;
const GT_RENDER_L3_PARITY_ERROR_INTERRUPT: u32 = 1 << 5;
const GT_RENDER_L3_PARITY_ERROR_INTERRUPT_S1: u32 = 1 << 11;
const GT_BLT_USER_INTERRUPT: u32 = 1 << 22;
const GT_BLT_CS_ERROR_INTERRUPT: u32 = 1 << 25;
const GT_BSD_USER_INTERRUPT: u32 = 1 << 12;
const GT_BSD_CS_ERROR_INTERRUPT: u32 = 1 << 15;
const GT_CONTEXT_SWITCH_INTERRUPT: u32 = 1 << 8;
const GT_WAIT_SEMAPHORE_INTERRUPT: u32 = 1 << 11;
const GT_CS_MASTER_ERROR_INTERRUPT: u32 = 1 << 3;
const ILK_BSD_USER_INTERRUPT: u32 = 1 << 5;
const PM_VEBOX_USER_INTERRUPT: u32 = 1 << 10;

const GEN8_GT_RCS_IRQ: u32 = 1 << 0;
const GEN8_GT_BCS_IRQ: u32 = 1 << 1;
const GEN8_GT_VCS0_IRQ: u32 = 1 << 2;
const GEN8_GT_VCS1_IRQ: u32 = 1 << 3;
const GEN8_GT_PM_IRQ: u32 = 1 << 4;
const GEN8_GT_GUC_IRQ: u32 = 1 << 5;
const GEN8_GT_VECS_IRQ: u32 = 1 << 6;
const GEN8_RCS_IRQ_SHIFT: u32 = 0;
const GEN8_BCS_IRQ_SHIFT: u32 = 16;
const GEN8_VCS0_IRQ_SHIFT: u32 = 0;
const GEN8_VCS1_IRQ_SHIFT: u32 = 16;
const GEN8_VECS_IRQ_SHIFT: u32 = 0;

const GEN11_INTR_DATA_VALID: u32 = 1 << 31;
const GEN11_RENDER_COPY_INTR_ENABLE: I915RegT = reg(0x190030);
const GEN11_VCS_VECS_INTR_ENABLE: I915RegT = reg(0x190034);
const GEN11_GUC_SG_INTR_ENABLE: I915RegT = reg(0x190038);
const GEN11_GPM_WGBOXPERF_INTR_ENABLE: I915RegT = reg(0x19003c);
const GEN11_CRYPTO_RSVD_INTR_ENABLE: I915RegT = reg(0x190040);
const GEN11_GUNIT_CSME_INTR_ENABLE: I915RegT = reg(0x190044);
const GEN12_CCS_RSVD_INTR_ENABLE: I915RegT = reg(0x190048);
const GEN11_RCS0_RSVD_INTR_MASK: I915RegT = reg(0x190090);
const GEN11_BCS_RSVD_INTR_MASK: I915RegT = reg(0x1900a0);
const GEN11_VCS0_VCS1_INTR_MASK: I915RegT = reg(0x1900a8);
const GEN11_VCS2_VCS3_INTR_MASK: I915RegT = reg(0x1900ac);
const GEN12_VCS4_VCS5_INTR_MASK: I915RegT = reg(0x1900b0);
const GEN12_VCS6_VCS7_INTR_MASK: I915RegT = reg(0x1900b4);
const GEN11_VECS0_VECS1_INTR_MASK: I915RegT = reg(0x1900d0);
const GEN12_VECS2_VECS3_INTR_MASK: I915RegT = reg(0x1900d4);
const GEN12_HECI2_RSVD_INTR_MASK: I915RegT = reg(0x1900e4);
const GEN11_GUC_SG_INTR_MASK: I915RegT = reg(0x1900e8);
const MTL_GUC_MGUC_INTR_MASK: I915RegT = reg(0x1900e8);
const GEN11_GPM_WGBOXPERF_INTR_MASK: I915RegT = reg(0x1900ec);
const GEN11_CRYPTO_RSVD_INTR_MASK: I915RegT = reg(0x1900f0);
const GEN11_GUNIT_CSME_INTR_MASK: I915RegT = reg(0x1900f4);
const GEN12_CCS0_CCS1_INTR_MASK: I915RegT = reg(0x190100);
const GEN12_CCS2_CCS3_INTR_MASK: I915RegT = reg(0x190104);
const XEHPC_BCS1_BCS2_INTR_MASK: I915RegT = reg(0x190110);
const XEHPC_BCS3_BCS4_INTR_MASK: I915RegT = reg(0x190114);
const XEHPC_BCS5_BCS6_INTR_MASK: I915RegT = reg(0x190118);
const XEHPC_BCS7_BCS8_INTR_MASK: I915RegT = reg(0x19011c);
const ENGINE0_MASK: u32 = 0x0000_ffff;
const ENGINE1_MASK: u32 = 0xffff_0000;
const GUC_INTR_GUC2HOST: u16 = 1 << 15;
const OTHER_GUC_INSTANCE: u8 = 0;
const OTHER_GTPM_INSTANCE: u8 = 1;
const OTHER_GSC_HECI_2_INSTANCE: u8 = 3;
const OTHER_KCR_INSTANCE: u8 = 4;
const OTHER_GSC_INSTANCE: u8 = 6;
const OTHER_MEDIA_GUC_INSTANCE: u8 = 16;
const OTHER_MEDIA_GTPM_INSTANCE: u8 = 17;

const GTIMR: I915RegT = reg(0x44014);
const GTIER: I915RegT = reg(0x4401c);
const GTIIR: I915RegT = reg(0x44018);
const GEN6_PMIMR: I915RegT = reg(0x44024);
const GEN6_PMIIR: I915RegT = reg(0x44028);
const GEN6_PMIER: I915RegT = reg(0x4402c);

const fn reg(offset: u32) -> I915RegT {
    I915RegT { reg: offset }
}

const fn gen11_gt_intr_dw(bank: u32) -> I915RegT {
    reg(0x190018 + bank * 4)
}

const fn gen11_intr_identity_reg(bank: u32) -> I915RegT {
    reg(0x190060 + bank * 4)
}

const fn gen11_iir_reg_selector(bank: u32) -> I915RegT {
    reg(0x190070 + bank * 4)
}

const fn gen8_gt_iir(which: u32) -> I915RegT {
    reg(0x44308 + 0x10 * which)
}

const fn gen8_gt_irq_regs(which: u32) -> I915IrqRegs {
    I915IrqRegs {
        imr: reg(0x44304 + 0x10 * which),
        ier: reg(0x4430c + 0x10 * which),
        iir: gen8_gt_iir(which),
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct I915IrqRegs {
    imr: I915RegT,
    ier: I915RegT,
    iir: I915RegT,
}

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn intel_guc_ct_event_handler(ct: *mut IntelGucCt);
    fn gen11_rps_irq_handler(rps: *mut IntelRps, pm_iir: u32);
    fn gen6_rps_irq_handler(rps: *mut IntelRps, pm_iir: u32);
    fn intel_gsc_proxy_irq_handler(gsc: *mut c_void, iir: u32);
}

fn local_clock() -> u64 {
    axhal::time::monotonic_time_nanos()
}

#[inline]
fn time_after32(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) < 0
}

unsafe fn engine_irq(engine: *mut IntelEngineCs, iir: u32) {
    if iir == 0 {
        return;
    }
    assert!(
        !engine.is_null(),
        "interrupt targeted an uninitialized engine"
    );
    let handler =
        unsafe { (*engine).irq_handler }.expect("i915 engine interrupt handler is not installed");
    unsafe { handler(engine, iir as u16) };
}

unsafe fn has_heci_gsc(i915: *mut DrmI915Private) -> bool {
    let info = unsafe {
        (*i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    !info.is_null() && unsafe { (*info).flags[1] & ((1 << 4) | (1 << 5)) != 0 }
}

unsafe fn has_l3_dpf(i915: *mut DrmI915Private) -> bool {
    let info = unsafe {
        (*i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    !info.is_null() && unsafe { (*info).flags[2] & (1 << 1) != 0 }
}

unsafe fn is_haswell(i915: *mut DrmI915Private) -> bool {
    const INTEL_HASWELL: u32 = 19;
    let info = unsafe { (*i915).info.cast::<u8>() };
    !info.is_null() && unsafe { ptr::read_unaligned(info.cast::<u32>()) == INTEL_HASWELL }
}

unsafe fn gt_parity_error(i915: *mut DrmI915Private) -> u32 {
    GT_RENDER_L3_PARITY_ERROR_INTERRUPT
        | if unsafe { is_haswell(i915) } {
            GT_RENDER_L3_PARITY_ERROR_INTERRUPT_S1
        } else {
            0
        }
}

unsafe fn raw_regs(uncore: *mut IntelUncore) -> *mut u8 {
    unsafe { intel_uncore_regs(uncore).cast::<u8>() }
}

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

unsafe fn gen2_assert_iir_is_zero(uncore: *mut IntelUncore, reg_: I915RegT) {
    let val = unsafe { crate::intel_uncore_types_upstream::intel_uncore_read(uncore, reg_) };
    if val == 0 {
        return;
    }
    WARN_ONCE!(
        true,
        "Interrupt register 0x{:x} is not zero: 0x{:08x}\n",
        reg_.reg,
        val
    );
    unsafe {
        intel_uncore_write(uncore, reg_, u32::MAX);
        intel_uncore_posting_read(uncore, reg_);
        intel_uncore_write(uncore, reg_, u32::MAX);
        intel_uncore_posting_read(uncore, reg_);
    }
}

unsafe fn gen2_irq_init(uncore: *mut IntelUncore, regs: I915IrqRegs, imr_val: u32, ier_val: u32) {
    unsafe {
        gen2_assert_iir_is_zero(uncore, regs.iir);
        intel_uncore_write(uncore, regs.ier, ier_val);
        intel_uncore_write(uncore, regs.imr, imr_val);
        intel_uncore_posting_read(uncore, regs.imr);
    }
}

// upstream: intel_gt_irq.c guc_irq_handler()
unsafe fn guc_irq_handler(guc: *mut IntelGuc, iir: u16) {
    if !unsafe { (*guc).interrupts.enabled } {
        return;
    }
    if iir & GUC_INTR_GUC2HOST != 0 {
        unsafe { intel_guc_ct_event_handler(ptr::addr_of_mut!((*guc).ct)) };
    }
}

// upstream: intel_gt_irq.c gen11_gt_engine_identity()
unsafe fn gen11_gt_engine_identity(gt: *mut IntelGt, bank: u32, bit: u32) -> u32 {
    let regs = unsafe { raw_regs((*gt).uncore) };
    lockdep_assert_held!(unsafe { &mut *(*gt).irq_lock });
    unsafe {
        raw_reg_write(regs, gen11_iir_reg_selector(bank), 1 << bit);
    }

    // Linux local_clock() is a CPU-local monotonic clock. The HAL monotonic
    // clock supplies the same microsecond-scale timeout basis here.
    let timeout_ts = ((local_clock() >> 10) as u32).wrapping_add(100);
    let ident = loop {
        let ident = unsafe { raw_reg_read(regs, gen11_intr_identity_reg(bank)) };
        if ident & GEN11_INTR_DATA_VALID != 0
            || time_after32((local_clock() >> 10) as u32, timeout_ts)
        {
            break ident;
        }
    };

    if ident & GEN11_INTR_DATA_VALID == 0 {
        gt_err!(
            gt,
            "INTR_IDENTITY_REG%u:%u 0x%08x not valid!\n",
            bank,
            bit,
            ident
        );
        return 0;
    }
    unsafe { raw_reg_write(regs, gen11_intr_identity_reg(bank), GEN11_INTR_DATA_VALID) };
    ident
}

// upstream: intel_gt_irq.c gen11_other_irq_handler()
unsafe fn gen11_other_irq_handler(gt: *mut IntelGt, instance: u8, iir: u16) {
    let i915 = unsafe { (*gt).i915 };
    let media_gt = unsafe { (*i915).media_gt };

    if instance == OTHER_GUC_INSTANCE {
        return unsafe { guc_irq_handler(gt_to_guc(gt), iir) };
    }
    if instance == OTHER_MEDIA_GUC_INSTANCE && !media_gt.is_null() {
        return unsafe { guc_irq_handler(gt_to_guc(media_gt), iir) };
    }
    if instance == OTHER_GTPM_INSTANCE {
        return unsafe { gen11_rps_irq_handler(ptr::addr_of_mut!((*gt).rps), iir as u32) };
    }
    if instance == OTHER_MEDIA_GTPM_INSTANCE && !media_gt.is_null() {
        return unsafe { gen11_rps_irq_handler(ptr::addr_of_mut!((*media_gt).rps), iir as u32) };
    }
    if instance == OTHER_KCR_INSTANCE {
        // CONFIG_DRM_I915_PXP is disabled in this kernel configuration; the
        // upstream header's inline handler is correspondingly an empty body.
        return;
    }
    if instance == OTHER_GSC_INSTANCE {
        return unsafe { intel_gsc_irq_handler(gt, iir as u32) };
    }
    if instance == OTHER_GSC_HECI_2_INSTANCE {
        return unsafe {
            intel_gsc_proxy_irq_handler(ptr::addr_of_mut!((*gt).uc.gsc).cast(), iir as u32)
        };
    }
    WARN_ONCE!(
        true,
        "unhandled other interrupt instance=0x{:x}, iir=0x{:x}\n",
        instance,
        iir
    );
}

// upstream: intel_gt_irq.c pick_gt()
unsafe fn pick_gt(gt: *mut IntelGt, class: u8, instance: u8) -> *mut IntelGt {
    let media_gt = unsafe { (*(*gt).i915).media_gt };
    GEM_BUG_ON!(gt == media_gt);
    if media_gt.is_null() {
        return gt;
    }

    match class as i32 {
        VIDEO_DECODE_CLASS | VIDEO_ENHANCEMENT_CLASS => media_gt,
        OTHER_CLASS => {
            if instance == OTHER_GSC_HECI_2_INSTANCE {
                return media_gt;
            }
            if (instance == OTHER_GSC_INSTANCE || instance == OTHER_KCR_INSTANCE)
                && unsafe { HAS_ENGINE(media_gt, GSC0) }
            {
                return media_gt;
            }
            gt
        }
        _ => gt,
    }
}

// upstream: intel_gt_irq.c gen11_gt_identity_handler()
unsafe fn gen11_gt_identity_handler(mut gt: *mut IntelGt, identity: u32) {
    let class = ((identity & 0x0007_0000) >> 16) as u8;
    let instance = ((identity & 0x03f0_0000) >> 20) as u8;
    let intr = (identity & 0xffff) as u16;
    if intr == 0 {
        return;
    }

    gt = unsafe { pick_gt(gt, class, instance) };
    if class as i32 <= MAX_ENGINE_CLASS && instance as i32 <= MAX_ENGINE_INSTANCE {
        let engine = unsafe { (*gt).engine_class[class as usize][instance as usize] };
        if !engine.is_null() {
            return unsafe { engine_irq(engine, intr as u32) };
        }
    }
    if class as i32 == OTHER_CLASS {
        return unsafe { gen11_other_irq_handler(gt, instance, intr) };
    }
    WARN_ONCE!(
        true,
        "unknown interrupt class=0x{:x}, instance=0x{:x}, intr=0x{:x}\n",
        class,
        instance,
        intr
    );
}

// upstream: intel_gt_irq.c gen11_gt_bank_handler()
unsafe fn gen11_gt_bank_handler(gt: *mut IntelGt, bank: u32) {
    let regs = unsafe { raw_regs((*gt).uncore) };
    lockdep_assert_held!(unsafe { &mut *(*gt).irq_lock });
    let intr_dw = unsafe { raw_reg_read(regs, gen11_gt_intr_dw(bank)) };
    let mut pending = intr_dw;
    while pending != 0 {
        let bit = pending.trailing_zeros();
        let ident = unsafe { gen11_gt_engine_identity(gt, bank, bit) };
        unsafe { gen11_gt_identity_handler(gt, ident) };
        pending &= pending - 1;
    }
    // Clear must be after shared has been served for engine.
    unsafe { raw_reg_write(regs, gen11_gt_intr_dw(bank), intr_dw) };
}

// upstream: intel_gt_irq.c gen11_gt_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_gt_irq_handler(gt: *mut IntelGt, master_ctl: u32) {
    unsafe { spin_lock(&mut *(*gt).irq_lock) };
    for bank in 0..2 {
        if master_ctl & (1 << bank) != 0 {
            unsafe { gen11_gt_bank_handler(gt, bank) };
        }
    }
    unsafe { spin_unlock(&mut *(*gt).irq_lock) };
}

// upstream: intel_gt_irq.c gen11_gt_reset_one_iir()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_gt_reset_one_iir(gt: *mut IntelGt, bank: u32, bit: u32) -> bool {
    let regs = unsafe { raw_regs((*gt).uncore) };
    lockdep_assert_held!(unsafe { &mut *(*gt).irq_lock });
    let intr_dw = unsafe { raw_reg_read(regs, gen11_gt_intr_dw(bank)) };
    if intr_dw & (1 << bit) != 0 {
        // Service selector/shared identity before clearing the GT DW bit.
        unsafe { gen11_gt_engine_identity(gt, bank, bit) };
        unsafe { raw_reg_write(regs, gen11_gt_intr_dw(bank), 1 << bit) };
        return true;
    }
    false
}

// upstream: intel_gt_irq.c gen11_gt_irq_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_gt_irq_reset(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        intel_uncore_write(uncore, GEN11_RENDER_COPY_INTR_ENABLE, 0);
        intel_uncore_write(uncore, GEN11_VCS_VECS_INTR_ENABLE, 0);
        if CCS_MASK(gt) != 0 {
            intel_uncore_write(uncore, GEN12_CCS_RSVD_INTR_ENABLE, 0);
        }
        if has_heci_gsc((*gt).i915) || HAS_ENGINE(gt, GSC0) {
            intel_uncore_write(uncore, GEN11_GUNIT_CSME_INTR_ENABLE, 0);
        }

        intel_uncore_write(uncore, GEN11_RCS0_RSVD_INTR_MASK, u32::MAX);
        intel_uncore_write(uncore, GEN11_BCS_RSVD_INTR_MASK, u32::MAX);
        if HAS_ENGINE(gt, BCS1) || HAS_ENGINE(gt, BCS2) {
            intel_uncore_write(uncore, XEHPC_BCS1_BCS2_INTR_MASK, u32::MAX);
        }
        if HAS_ENGINE(gt, BCS3) || HAS_ENGINE(gt, BCS4) {
            intel_uncore_write(uncore, XEHPC_BCS3_BCS4_INTR_MASK, u32::MAX);
        }
        if HAS_ENGINE(gt, BCS5) || HAS_ENGINE(gt, BCS6) {
            intel_uncore_write(uncore, XEHPC_BCS5_BCS6_INTR_MASK, u32::MAX);
        }
        if HAS_ENGINE(gt, BCS7) || HAS_ENGINE(gt, BCS8) {
            intel_uncore_write(uncore, XEHPC_BCS7_BCS8_INTR_MASK, u32::MAX);
        }
        intel_uncore_write(uncore, GEN11_VCS0_VCS1_INTR_MASK, u32::MAX);
        intel_uncore_write(uncore, GEN11_VCS2_VCS3_INTR_MASK, u32::MAX);
        if HAS_ENGINE(gt, VCS4) || HAS_ENGINE(gt, VCS5) {
            intel_uncore_write(uncore, GEN12_VCS4_VCS5_INTR_MASK, u32::MAX);
        }
        if HAS_ENGINE(gt, VCS6) || HAS_ENGINE(gt, VCS7) {
            intel_uncore_write(uncore, GEN12_VCS6_VCS7_INTR_MASK, u32::MAX);
        }
        intel_uncore_write(uncore, GEN11_VECS0_VECS1_INTR_MASK, u32::MAX);
        if HAS_ENGINE(gt, VECS2) || HAS_ENGINE(gt, VECS3) {
            intel_uncore_write(uncore, GEN12_VECS2_VECS3_INTR_MASK, u32::MAX);
        }
        if HAS_ENGINE(gt, CCS0) || HAS_ENGINE(gt, CCS1) {
            intel_uncore_write(uncore, GEN12_CCS0_CCS1_INTR_MASK, u32::MAX);
        }
        if HAS_ENGINE(gt, CCS2) || HAS_ENGINE(gt, CCS3) {
            intel_uncore_write(uncore, GEN12_CCS2_CCS3_INTR_MASK, u32::MAX);
        }
        if has_heci_gsc((*gt).i915) || HAS_ENGINE(gt, GSC0) {
            intel_uncore_write(uncore, GEN11_GUNIT_CSME_INTR_MASK, u32::MAX);
        }

        intel_uncore_write(uncore, GEN11_GPM_WGBOXPERF_INTR_ENABLE, 0);
        intel_uncore_write(uncore, GEN11_GPM_WGBOXPERF_INTR_MASK, u32::MAX);
        intel_uncore_write(uncore, GEN11_GUC_SG_INTR_ENABLE, 0);
        intel_uncore_write(uncore, GEN11_GUC_SG_INTR_MASK, u32::MAX);
        intel_uncore_write(uncore, GEN11_CRYPTO_RSVD_INTR_ENABLE, 0);
        intel_uncore_write(uncore, GEN11_CRYPTO_RSVD_INTR_MASK, u32::MAX);
    }
}

// upstream: intel_gt_irq.c gen11_gt_irq_postinstall()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_gt_irq_postinstall(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    let uc = unsafe { ptr::addr_of_mut!((*gt).uc) };
    let mut irqs = GT_RENDER_USER_INTERRUPT;
    let guc_mask = if unsafe { crate::intel_uc_types_upstream::intel_uc_wants_guc(uc) } {
        GUC_INTR_GUC2HOST as u32
    } else {
        0
    };
    let mut gsc_mask = 0;
    let mut heci_mask = 0;
    if !unsafe { crate::intel_uc_types_upstream::intel_uc_wants_guc_submission(uc) } {
        irqs |= GT_CS_MASTER_ERROR_INTERRUPT
            | GT_CONTEXT_SWITCH_INTERRUPT
            | GT_WAIT_SEMAPHORE_INTERRUPT;
    }
    let dmask = (irqs << 16) | irqs;
    let smask = irqs << 16;

    unsafe {
        if HAS_ENGINE(gt, GSC0) {
            gsc_mask = irqs;
            heci_mask = 1 << 14;
        } else if has_heci_gsc((*gt).i915) {
            gsc_mask = (1 << 15) | (1 << 14);
        }
        assert_eq!(irqs & 0xffff_0000, 0);

        intel_uncore_write(uncore, GEN11_RENDER_COPY_INTR_ENABLE, dmask);
        intel_uncore_write(uncore, GEN11_VCS_VECS_INTR_ENABLE, dmask);
        if CCS_MASK(gt) != 0 {
            intel_uncore_write(uncore, GEN12_CCS_RSVD_INTR_ENABLE, smask);
        }
        if gsc_mask != 0 {
            intel_uncore_write(uncore, GEN11_GUNIT_CSME_INTR_ENABLE, gsc_mask | heci_mask);
        }

        intel_uncore_write(uncore, GEN11_RCS0_RSVD_INTR_MASK, !smask);
        intel_uncore_write(uncore, GEN11_BCS_RSVD_INTR_MASK, !smask);
        if HAS_ENGINE(gt, BCS1) || HAS_ENGINE(gt, BCS2) {
            intel_uncore_write(uncore, XEHPC_BCS1_BCS2_INTR_MASK, !dmask);
        }
        if HAS_ENGINE(gt, BCS3) || HAS_ENGINE(gt, BCS4) {
            intel_uncore_write(uncore, XEHPC_BCS3_BCS4_INTR_MASK, !dmask);
        }
        if HAS_ENGINE(gt, BCS5) || HAS_ENGINE(gt, BCS6) {
            intel_uncore_write(uncore, XEHPC_BCS5_BCS6_INTR_MASK, !dmask);
        }
        if HAS_ENGINE(gt, BCS7) || HAS_ENGINE(gt, BCS8) {
            intel_uncore_write(uncore, XEHPC_BCS7_BCS8_INTR_MASK, !dmask);
        }
        intel_uncore_write(uncore, GEN11_VCS0_VCS1_INTR_MASK, !dmask);
        intel_uncore_write(uncore, GEN11_VCS2_VCS3_INTR_MASK, !dmask);
        if HAS_ENGINE(gt, VCS4) || HAS_ENGINE(gt, VCS5) {
            intel_uncore_write(uncore, GEN12_VCS4_VCS5_INTR_MASK, !dmask);
        }
        if HAS_ENGINE(gt, VCS6) || HAS_ENGINE(gt, VCS7) {
            intel_uncore_write(uncore, GEN12_VCS6_VCS7_INTR_MASK, !dmask);
        }
        intel_uncore_write(uncore, GEN11_VECS0_VECS1_INTR_MASK, !dmask);
        if HAS_ENGINE(gt, VECS2) || HAS_ENGINE(gt, VECS3) {
            intel_uncore_write(uncore, GEN12_VECS2_VECS3_INTR_MASK, !dmask);
        }
        if HAS_ENGINE(gt, CCS0) || HAS_ENGINE(gt, CCS1) {
            intel_uncore_write(uncore, GEN12_CCS0_CCS1_INTR_MASK, !dmask);
        }
        if HAS_ENGINE(gt, CCS2) || HAS_ENGINE(gt, CCS3) {
            intel_uncore_write(uncore, GEN12_CCS2_CCS3_INTR_MASK, !dmask);
        }
        if gsc_mask != 0 {
            intel_uncore_write(uncore, GEN11_GUNIT_CSME_INTR_MASK, !gsc_mask);
        }
        if heci_mask != 0 {
            intel_uncore_write(
                uncore,
                GEN12_HECI2_RSVD_INTR_MASK,
                !REG_FIELD_PREP(ENGINE1_MASK, heci_mask),
            );
        }
        if guc_mask != 0 {
            let mask = if (*gt).type_ == GT_MEDIA {
                REG_FIELD_PREP(ENGINE0_MASK, guc_mask)
            } else {
                REG_FIELD_PREP(ENGINE1_MASK, guc_mask)
            };
            intel_uncore_write(
                uncore,
                GEN11_GUC_SG_INTR_ENABLE,
                REG_FIELD_PREP(ENGINE1_MASK, guc_mask),
            );
            intel_uncore_rmw(uncore, MTL_GUC_MGUC_INTR_MASK, mask, 0);
        }

        (*gt).pm_ier = 0;
        (*gt).pm_imr = !(*gt).pm_ier;
        intel_uncore_write(uncore, GEN11_GPM_WGBOXPERF_INTR_ENABLE, 0);
        intel_uncore_write(uncore, GEN11_GPM_WGBOXPERF_INTR_MASK, u32::MAX);
    }
}

// upstream: intel_gt_irq.c gen5_gt_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen5_gt_irq_handler(gt: *mut IntelGt, gt_iir: u32) {
    unsafe {
        if gt_iir & GT_RENDER_USER_INTERRUPT != 0 {
            engine_irq((*gt).engine_class[RENDER_CLASS as usize][0], gt_iir);
        }
        if gt_iir & ILK_BSD_USER_INTERRUPT != 0 {
            engine_irq((*gt).engine_class[VIDEO_DECODE_CLASS as usize][0], gt_iir);
        }
    }
}

#[repr(C)]
struct IntelL3Parity {
    remap_info: [*mut u32; 2],
    error_work: crate::intel_engine_cs_upstream::WorkStruct,
    which_slice: i32,
    _padding: u32,
}
const _: [(); 56] = [(); core::mem::size_of::<IntelL3Parity>()];

unsafe fn l3_parity(i915: *mut DrmI915Private) -> *mut IntelL3Parity {
    // The current i915 private overlay keeps this 56-byte C subrecord opaque;
    // `edram_size_mb` immediately follows it in the verified x86_64 layout.
    unsafe {
        ptr::addr_of_mut!((*i915).edram_size_mb)
            .cast::<u8>()
            .sub(core::mem::size_of::<IntelL3Parity>())
            .cast::<IntelL3Parity>()
    }
}

// upstream: intel_gt_irq.c gen7_parity_error_irq_handler()
unsafe fn gen7_parity_error_irq_handler(gt: *mut IntelGt, iir: u32) {
    let i915 = unsafe { (*gt).i915 };
    if !unsafe { has_l3_dpf(i915) } {
        return;
    }
    unsafe {
        spin_lock(&mut *(*gt).irq_lock);
        gen5_gt_disable_irq(gt, gt_parity_error(i915));
        spin_unlock(&mut *(*gt).irq_lock);
    }

    let parity = unsafe { l3_parity(i915) };
    if iir & GT_RENDER_L3_PARITY_ERROR_INTERRUPT_S1 != 0 {
        unsafe { (*parity).which_slice |= 1 << 1 };
    }
    if iir & GT_RENDER_L3_PARITY_ERROR_INTERRUPT != 0 {
        unsafe { (*parity).which_slice |= 1 << 0 };
    }
    unsafe {
        let work = ptr::addr_of_mut!((*parity).error_work);
        let _ = queue_work((*i915).unordered_wq, work);
    }
}

// upstream: intel_gt_irq.c gen6_gt_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_gt_irq_handler(gt: *mut IntelGt, gt_iir: u32) {
    unsafe {
        if gt_iir & GT_RENDER_USER_INTERRUPT != 0 {
            engine_irq((*gt).engine_class[RENDER_CLASS as usize][0], gt_iir);
        }
        if gt_iir & GT_BSD_USER_INTERRUPT != 0 {
            engine_irq(
                (*gt).engine_class[VIDEO_DECODE_CLASS as usize][0],
                gt_iir >> 12,
            );
        }
        if gt_iir & GT_BLT_USER_INTERRUPT != 0 {
            engine_irq(
                (*gt).engine_class[COPY_ENGINE_CLASS as usize][0],
                gt_iir >> 22,
            );
        }
        if gt_iir
            & (GT_BLT_CS_ERROR_INTERRUPT | GT_BSD_CS_ERROR_INTERRUPT | GT_CS_MASTER_ERROR_INTERRUPT)
            != 0
        {
            gt_dbg!(gt, "Command parser error, gt_iir 0x%08x\n", gt_iir);
        }
        if gt_iir & gt_parity_error((*gt).i915) != 0 {
            gen7_parity_error_irq_handler(gt, gt_iir);
        }
    }
}

// upstream: intel_gt_irq.c gen8_gt_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_gt_irq_handler(gt: *mut IntelGt, master_ctl: u32) {
    let regs = unsafe { raw_regs((*gt).uncore) };
    unsafe {
        if master_ctl & (GEN8_GT_RCS_IRQ | GEN8_GT_BCS_IRQ) != 0 {
            let iir = raw_reg_read(regs, gen8_gt_iir(0));
            if iir != 0 {
                engine_irq(
                    (*gt).engine_class[RENDER_CLASS as usize][0],
                    iir >> GEN8_RCS_IRQ_SHIFT,
                );
                engine_irq(
                    (*gt).engine_class[COPY_ENGINE_CLASS as usize][0],
                    iir >> GEN8_BCS_IRQ_SHIFT,
                );
                raw_reg_write(regs, gen8_gt_iir(0), iir);
            }
        }
        if master_ctl & (GEN8_GT_VCS0_IRQ | GEN8_GT_VCS1_IRQ) != 0 {
            let iir = raw_reg_read(regs, gen8_gt_iir(1));
            if iir != 0 {
                engine_irq(
                    (*gt).engine_class[VIDEO_DECODE_CLASS as usize][0],
                    iir >> GEN8_VCS0_IRQ_SHIFT,
                );
                engine_irq(
                    (*gt).engine_class[VIDEO_DECODE_CLASS as usize][1],
                    iir >> GEN8_VCS1_IRQ_SHIFT,
                );
                raw_reg_write(regs, gen8_gt_iir(1), iir);
            }
        }
        if master_ctl & GEN8_GT_VECS_IRQ != 0 {
            let iir = raw_reg_read(regs, gen8_gt_iir(3));
            if iir != 0 {
                engine_irq(
                    (*gt).engine_class[VIDEO_ENHANCEMENT_CLASS as usize][0],
                    iir >> GEN8_VECS_IRQ_SHIFT,
                );
                raw_reg_write(regs, gen8_gt_iir(3), iir);
            }
        }
        if master_ctl & (GEN8_GT_PM_IRQ | GEN8_GT_GUC_IRQ) != 0 {
            let iir = raw_reg_read(regs, gen8_gt_iir(2));
            if iir != 0 {
                gen6_rps_irq_handler(ptr::addr_of_mut!((*gt).rps), iir);
                guc_irq_handler(gt_to_guc(gt), (iir >> 16) as u16);
                raw_reg_write(regs, gen8_gt_iir(2), iir);
            }
        }
    }
}

// upstream: intel_gt_irq.c gen8_gt_irq_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_gt_irq_reset(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        gen2_irq_reset(uncore, gen8_gt_irq_regs(0));
        gen2_irq_reset(uncore, gen8_gt_irq_regs(1));
        gen2_irq_reset(uncore, gen8_gt_irq_regs(2));
        gen2_irq_reset(uncore, gen8_gt_irq_regs(3));
    }
}

// upstream: intel_gt_irq.c gen8_gt_irq_postinstall()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_gt_irq_postinstall(gt: *mut IntelGt) {
    let irqs = GT_CS_MASTER_ERROR_INTERRUPT
        | GT_RENDER_USER_INTERRUPT
        | GT_CONTEXT_SWITCH_INTERRUPT
        | GT_WAIT_SEMAPHORE_INTERRUPT;
    let gt_interrupts = [
        (irqs << GEN8_RCS_IRQ_SHIFT) | (irqs << GEN8_BCS_IRQ_SHIFT),
        (irqs << GEN8_VCS0_IRQ_SHIFT) | (irqs << GEN8_VCS1_IRQ_SHIFT),
        0,
        irqs << GEN8_VECS_IRQ_SHIFT,
    ];
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        (*gt).pm_ier = 0;
        (*gt).pm_imr = !(*gt).pm_ier;
        gen2_irq_init(
            uncore,
            gen8_gt_irq_regs(0),
            !gt_interrupts[0],
            gt_interrupts[0],
        );
        gen2_irq_init(
            uncore,
            gen8_gt_irq_regs(1),
            !gt_interrupts[1],
            gt_interrupts[1],
        );
        gen2_irq_init(uncore, gen8_gt_irq_regs(2), (*gt).pm_imr, (*gt).pm_ier);
        gen2_irq_init(
            uncore,
            gen8_gt_irq_regs(3),
            !gt_interrupts[3],
            gt_interrupts[3],
        );
    }
}

// upstream: intel_gt_irq.c gen5_gt_update_irq()
unsafe fn gen5_gt_update_irq(gt: *mut IntelGt, interrupt_mask: u32, enabled_irq_mask: u32) {
    lockdep_assert_held!(unsafe { &mut *(*gt).irq_lock });
    GEM_BUG_ON!(enabled_irq_mask & !interrupt_mask != 0);
    unsafe {
        (*gt).gt_imr &= !interrupt_mask;
        (*gt).gt_imr |= !enabled_irq_mask & interrupt_mask;
        intel_uncore_write((*gt).uncore, GTIMR, (*gt).gt_imr);
    }
}

// upstream: intel_gt_irq.c gen5_gt_enable_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen5_gt_enable_irq(gt: *mut IntelGt, mask: u32) {
    unsafe {
        gen5_gt_update_irq(gt, mask, mask);
        intel_uncore_posting_read_fw((*gt).uncore, GTIMR);
    }
}

// upstream: intel_gt_irq.c gen5_gt_disable_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen5_gt_disable_irq(gt: *mut IntelGt, mask: u32) {
    unsafe { gen5_gt_update_irq(gt, mask, 0) };
}

// upstream: intel_gt_irq.c gen5_gt_irq_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen5_gt_irq_reset(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        gen2_irq_reset(
            uncore,
            I915IrqRegs {
                imr: GTIMR,
                ier: GTIER,
                iir: GTIIR,
            },
        );
        if GRAPHICS_VER((*gt).i915) >= 6 {
            gen2_irq_reset(
                uncore,
                I915IrqRegs {
                    imr: GEN6_PMIMR,
                    ier: GEN6_PMIER,
                    iir: GEN6_PMIIR,
                },
            );
        }
    }
}

// upstream: intel_gt_irq.c gen5_gt_irq_postinstall()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen5_gt_irq_postinstall(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    let mut pm_irqs = 0;
    let mut gt_irqs = 0;
    unsafe {
        (*gt).gt_imr = u32::MAX;
        if has_l3_dpf(i915) {
            (*gt).gt_imr = !gt_parity_error(i915);
            gt_irqs |= gt_parity_error(i915);
        }
        gt_irqs |= GT_RENDER_USER_INTERRUPT;
        if GRAPHICS_VER(i915) == 5 {
            gt_irqs |= ILK_BSD_USER_INTERRUPT;
        } else {
            gt_irqs |= GT_BLT_USER_INTERRUPT | GT_BSD_USER_INTERRUPT;
        }

        gen2_irq_init(
            uncore,
            I915IrqRegs {
                imr: GTIMR,
                ier: GTIER,
                iir: GTIIR,
            },
            (*gt).gt_imr,
            gt_irqs,
        );

        if GRAPHICS_VER(i915) >= 6 {
            // RPS interrupts are enabled and disabled on demand with RPS.
            if HAS_ENGINE(gt, VECS0) {
                pm_irqs |= PM_VEBOX_USER_INTERRUPT;
                (*gt).pm_ier |= PM_VEBOX_USER_INTERRUPT;
            }
            (*gt).pm_imr = u32::MAX;
            gen2_irq_init(
                uncore,
                I915IrqRegs {
                    imr: GEN6_PMIMR,
                    ier: GEN6_PMIER,
                    iir: GEN6_PMIIR,
                },
                (*gt).pm_imr,
                pm_irqs,
            );
        }
    }
}
