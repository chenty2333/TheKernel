// SPDX-License-Identifier: MIT
// Copyright © 2008-2018 Intel Corporation.
//! Linux 7.2.3 gt/intel_reset.c Gen6+ hardware-domain dependency functions,
//! source ordered. The port's supported GTs are Gen12; legacy PCI reset
//! implementations for Gen2-5 are outside its device admission set.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::ffi::c_int;

use crate::{
    intel_engine_regs_upstream::*,
    intel_engine_types_upstream::{
        _VECS, ALL_ENGINES, GSC0, IntelEngineCs, VIDEO_DECODE_CLASS as VDC,
        VIDEO_ENHANCEMENT_CLASS as VEC,
    },
    intel_gt_types_upstream::IntelGt,
    intel_uncore_types_upstream::{
        __intel_wait_for_register_fw, FORCEWAKE_ALL, intel_uncore_forcewake_get,
        intel_uncore_forcewake_put, intel_uncore_read, intel_uncore_read_fw, intel_uncore_rmw,
        intel_uncore_rmw_fw, intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{GRAPHICS_VER, GRAPHICS_VER_FULL, IP_VER, IS_DG2, MEDIA_VER_FULL},
        locks::{spin_lock_irqsave_raw, spin_unlock_irqrestore_raw},
        primitives::udelay,
    },
    linux_config::{EINVAL, ENODEV, ETIMEDOUT},
};
const VIDEO_DECODE_CLASS: u8 = VDC as u8;
const VIDEO_ENHANCEMENT_CLASS: u8 = VEC as u8;
unsafe fn engines(gt: *mut IntelGt, mask: u32) -> impl Iterator<Item = *mut IntelEngineCs> {
    unsafe { (*gt).engine }
        .into_iter()
        .filter(move |&p| !p.is_null() && unsafe { (*p).mask & mask != 0 })
}
fn reg(value: u32) -> I915RegT {
    I915RegT { reg: value }
}
// upstream: intel_reset.c gen6_hw_domain_reset()
unsafe fn gen6_hw_domain_reset(gt: *mut IntelGt, mask: u32) -> c_int {
    let uncore = (*gt).uncore;
    let mut loops = if GRAPHICS_VER_FULL((*gt).i915) < IP_VER(12, 70) {
        2
    } else {
        1
    };
    let mut err;
    loop {
        intel_uncore_write_fw(uncore, reg(0x941c), mask);
        err = __intel_wait_for_register_fw(
            uncore,
            reg(0x941c),
            mask,
            0,
            2000,
            0,
            core::ptr::null_mut(),
        );
        if err != 0 {
            break;
        }
        loops -= 1;
        if loops == 0 {
            break;
        }
    }
    if err != 0 {
        axlog::error!("Wait for {mask:08x} engines reset failed");
    }
    udelay(50);
    err
}
// upstream: intel_reset.c __gen6_reset_engines()
unsafe fn __gen6_reset_engines(gt: *mut IntelGt, mask: u32, _retry: u32) -> c_int {
    let mut hw_mask;
    if mask == ALL_ENGINES {
        hw_mask = 1;
    } else {
        hw_mask = 0;
        for engine in engines(gt, mask) {
            hw_mask |= (*engine).reset_domain;
        }
    }
    gen6_hw_domain_reset(gt, hw_mask)
}
// upstream: intel_reset.c gen6_reset_engines()
unsafe fn gen6_reset_engines(gt: *mut IntelGt, mask: u32, retry: u32) -> c_int {
    let mut flags = 0;
    spin_lock_irqsave_raw(&mut (*(*gt).uncore).lock, &mut flags);
    let ret = __gen6_reset_engines(gt, mask, retry);
    spin_unlock_irqrestore_raw(&mut (*(*gt).uncore).lock, flags);
    ret
}
// upstream: intel_reset.c find_sfc_paired_vecs_engine()
unsafe fn find_sfc_paired_vecs_engine(engine: *mut IntelEngineCs) -> *mut IntelEngineCs {
    GEM_BUG_ON!((*engine).class != VIDEO_DECODE_CLASS);
    (*(*engine).gt).engine[_VECS((*engine).instance as i32 / 2) as usize]
}
struct SfcLock {
    lock: I915RegT,
    ack: I915RegT,
    usage: I915RegT,
    lock_bit: u32,
    ack_bit: u32,
    usage_bit: u32,
    reset_bit: u32,
}
// upstream: intel_reset.c get_sfc_forced_lock_data()
unsafe fn get_sfc_forced_lock_data(engine: *mut IntelEngineCs) -> SfcLock {
    let b = (*engine).mmio_base;
    match (*engine).class {
        VIDEO_ENHANCEMENT_CLASS => SfcLock {
            lock: GEN11_VECS_SFC_FORCED_LOCK(b),
            ack: GEN11_VECS_SFC_LOCK_ACK(b),
            usage: GEN11_VECS_SFC_USAGE(b),
            lock_bit: GEN11_VECS_SFC_FORCED_LOCK_BIT,
            ack_bit: GEN11_VECS_SFC_LOCK_ACK_BIT,
            usage_bit: GEN11_VECS_SFC_USAGE_BIT,
            reset_bit: (1 << 17) << (*engine).instance,
        },
        _ => {
            if (*engine).class != VIDEO_DECODE_CLASS {
                axlog::warn!("unknown SFC engine class {}", (*engine).class);
            }
            SfcLock {
                lock: GEN11_VCS_SFC_FORCED_LOCK(b),
                ack: GEN11_VCS_SFC_LOCK_STATUS(b),
                usage: GEN11_VCS_SFC_LOCK_STATUS(b),
                lock_bit: GEN11_VCS_SFC_FORCED_LOCK_BIT,
                ack_bit: GEN11_VCS_SFC_LOCK_ACK_BIT,
                usage_bit: GEN11_VCS_SFC_USAGE_BIT,
                reset_bit: (1 << 17) << ((*engine).instance >> 1),
            }
        }
    }
}
// upstream: intel_reset.c gen11_lock_sfc()
unsafe fn gen11_lock_sfc(
    engine: *mut IntelEngineCs,
    reset_mask: &mut u32,
    unlock_mask: &mut u32,
) -> c_int {
    let uncore = (*engine).uncore;
    let access = (*(*engine).gt).info.vdbox_sfc_access;
    let mut other = false;
    match (*engine).class {
        VIDEO_DECODE_CLASS => {
            if (1 << (*engine).instance) & access as u32 == 0 {
                return 0;
            }
        }
        VIDEO_ENHANCEMENT_CLASS => {}
        _ => return 0,
    }
    let mut lock = get_sfc_forced_lock_data(engine);
    if intel_uncore_read_fw(uncore, lock.usage) & lock.usage_bit == 0 {
        if (*engine).class != VIDEO_DECODE_CLASS || GRAPHICS_VER((*engine).i915) != 12 {
            return 0;
        }
        if intel_uncore_read_fw(uncore, GEN12_HCP_SFC_LOCK_STATUS((*engine).mmio_base))
            & GEN12_HCP_SFC_USAGE_BIT
            == 0
        {
            return 0;
        }
        let paired = find_sfc_paired_vecs_engine(engine);
        lock = get_sfc_forced_lock_data(paired);
        other = true;
        *unlock_mask |= (*paired).mask;
    } else {
        *unlock_mask |= (*engine).mask;
    }
    intel_uncore_rmw_fw(uncore, lock.lock, 0, lock.lock_bit);
    let ret = __intel_wait_for_register_fw(
        uncore,
        lock.ack,
        lock.ack_bit,
        lock.ack_bit,
        1000,
        0,
        core::ptr::null_mut(),
    );
    let obtained = intel_uncore_read_fw(uncore, lock.usage) & lock.usage_bit != 0;
    if obtained == other {
        return 0;
    }
    if ret != 0 {
        axlog::debug!("Wait for SFC forced lock ack failed");
        return ret;
    }
    *reset_mask |= lock.reset_bit;
    0
}
// upstream: intel_reset.c gen11_unlock_sfc()
unsafe fn gen11_unlock_sfc(engine: *mut IntelEngineCs) {
    let access = (*(*engine).gt).info.vdbox_sfc_access;
    if (*engine).class != VIDEO_DECODE_CLASS && (*engine).class != VIDEO_ENHANCEMENT_CLASS {
        return;
    }
    if (*engine).class == VIDEO_DECODE_CLASS && (1 << (*engine).instance) & access as u32 == 0 {
        return;
    }
    let lock = get_sfc_forced_lock_data(engine);
    intel_uncore_rmw_fw((*engine).uncore, lock.lock, lock.lock_bit, 0);
}
// upstream: intel_reset.c __gen11_reset_engines()
unsafe fn __gen11_reset_engines(gt: *mut IntelGt, mask: u32, _retry: u32) -> c_int {
    let mut reset_mask;
    let mut unlock = 0;
    let mut ret = 0;
    if mask == ALL_ENGINES {
        reset_mask = 1;
    } else {
        reset_mask = 0;
        for engine in engines(gt, mask) {
            reset_mask |= (*engine).reset_domain;
            ret = gen11_lock_sfc(engine, &mut reset_mask, &mut unlock);
            if ret != 0 {
                break;
            }
        }
    }
    if ret == 0 {
        ret = gen6_hw_domain_reset(gt, reset_mask);
    }
    for engine in engines(gt, unlock) {
        gen11_unlock_sfc(engine);
    }
    ret
}
// upstream: intel_reset.c gen8_engine_reset_prepare()
unsafe fn gen8_engine_reset_prepare(engine: *mut IntelEngineCs) -> c_int {
    let uncore = (*engine).uncore;
    let r = RING_RESET_CTL((*engine).mmio_base);
    let ack = intel_uncore_read_fw(uncore, r);
    let (request, mask, ack) = if ack & 4 != 0 {
        (4, 4, 0)
    } else if ack & 2 == 0 {
        (1, 2, 2)
    } else {
        return 0;
    };
    intel_uncore_write_fw(uncore, r, (request << 16) | request);
    let ret = __intel_wait_for_register_fw(uncore, r, mask, ack, 700, 0, core::ptr::null_mut());
    if ret != 0 {
        axlog::error!(
            "engine reset request timed out: request={request:08x} ctl={:08x}",
            intel_uncore_read_fw(uncore, r)
        );
    }
    ret
}
// upstream: intel_reset.c gen8_engine_reset_cancel()
unsafe fn gen8_engine_reset_cancel(engine: *mut IntelEngineCs) {
    intel_uncore_write_fw(
        (*engine).uncore,
        RING_RESET_CTL((*engine).mmio_base),
        1 << 16,
    );
}
// upstream: intel_reset.c gen8_reset_engines()
unsafe fn gen8_reset_engines(gt: *mut IntelGt, mask: u32, retry: u32) -> c_int {
    let mut flags = 0;
    spin_lock_irqsave_raw(&mut (*(*gt).uncore).lock, &mut flags);
    let mut ret = 0;
    let non_ready = retry >= 1;
    for engine in engines(gt, mask) {
        ret = gen8_engine_reset_prepare(engine);
        if ret != 0 && !non_ready {
            break;
        }
    }
    if ret == 0 || non_ready {
        if IS_DG2((*gt).i915) && mask == ALL_ENGINES {
            __gen11_reset_engines(gt, (*gt).info.engine_mask, 0);
        }
        ret = if GRAPHICS_VER((*gt).i915) >= 11 {
            __gen11_reset_engines(gt, mask, retry)
        } else {
            __gen6_reset_engines(gt, mask, retry)
        };
    }
    for engine in engines(gt, mask) {
        gen8_engine_reset_cancel(engine);
    }
    spin_unlock_irqrestore_raw(&mut (*(*gt).uncore).lock, flags);
    ret
}
type Reset = unsafe fn(*mut IntelGt, u32, u32) -> c_int;
// Target admission specialization of intel_get_gpu_reset(): the port only
// admits Gen12. Keep legacy Gen6-7 dispatch available to its dependency tests;
// Gen2-5 PCI config reset is deliberately not represented as a fake callback.
unsafe fn intel_get_gpu_reset(gt: *mut IntelGt) -> Option<Reset> {
    if GRAPHICS_VER((*gt).i915) >= 8 {
        Some(gen8_reset_engines)
    } else if GRAPHICS_VER((*gt).i915) >= 6 {
        Some(gen6_reset_engines)
    } else {
        None
    }
}
// upstream: intel_reset.c __reset_guc()
unsafe fn __reset_guc(gt: *mut IntelGt) -> c_int {
    gen6_hw_domain_reset(
        gt,
        if GRAPHICS_VER((*gt).i915) >= 11 {
            1 << 3
        } else {
            1 << 5
        },
    )
}
// upstream: intel_reset.c needs_wa_14015076503()
unsafe fn needs_wa_14015076503(gt: *mut IntelGt, mask: u32) -> bool {
    if MEDIA_VER_FULL((*gt).i915) != IP_VER(13, 0)
        || (*gt).info.engine_mask & (1 << GSC0 as u32) == 0
    {
        return false;
    }
    if mask & (1 << GSC0 as u32) == 0 {
        return false;
    }
    intel_uncore_read((*gt).uncore, reg(0x116c40)) & (1 << 9) != 0 // intel_gsc_uc_fw_init_done()
}
// upstream: intel_reset.c wa_14015076503_start()
unsafe fn wa_14015076503_start(gt: *mut IntelGt, mut mask: u32, first: bool) -> u32 {
    if !needs_wa_14015076503(gt, mask) {
        return mask;
    }
    if mask == ALL_ENGINES
        && first
        && crate::intel_engine_cs_upstream::intel_engine_is_idle((*gt).engine[GSC0 as usize])
    {
        __reset_guc(gt);
        mask = (*gt).info.engine_mask & !(1 << GSC0 as u32);
    } else {
        intel_uncore_rmw((*gt).uncore, reg(0x117c4c), 0, 1);
        intel_uncore_rmw((*gt).uncore, reg(0x117004), 1 << 4, 1 << 2);
        axtask::sleep(core::time::Duration::from_millis(200));
    }
    mask
}
// upstream: intel_reset.c wa_14015076503_end()
unsafe fn wa_14015076503_end(gt: *mut IntelGt, mask: u32) {
    if !needs_wa_14015076503(gt, mask) {
        return;
    }
    intel_uncore_rmw((*gt).uncore, reg(0x117c4c), 1, 0);
}
// upstream: intel_reset.c __intel_gt_reset()
unsafe fn __intel_gt_reset(gt: *mut IntelGt, mask: u32) -> c_int {
    let retries = if mask == ALL_ENGINES { 3 } else { 1 };
    let mut ret = -ETIMEDOUT;
    let Some(reset) = intel_get_gpu_reset(gt) else {
        return -ENODEV;
    };
    intel_uncore_forcewake_get((*gt).uncore, FORCEWAKE_ALL);
    for retry in 0..retries {
        if ret != -ETIMEDOUT {
            break;
        }
        let reset_mask = wa_14015076503_start(gt, mask, retry == 0);
        ret = reset(gt, reset_mask, retry);
        wa_14015076503_end(gt, reset_mask);
    }
    intel_uncore_forcewake_put((*gt).uncore, FORCEWAKE_ALL);
    ret
}
// upstream: intel_reset.c intel_gt_reset_all_engines()
pub unsafe fn intel_gt_reset_all_engines(gt: *mut IntelGt) -> c_int {
    __intel_gt_reset(gt, ALL_ENGINES)
}
// upstream: intel_reset.c intel_gt_reset_engine()
pub unsafe fn intel_gt_reset_engine(engine: *mut IntelEngineCs) -> c_int {
    __intel_gt_reset((*engine).gt, (*engine).mask)
}
