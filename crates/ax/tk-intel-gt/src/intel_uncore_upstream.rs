// SPDX-License-Identifier: MIT
// Copyright © 2013 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/intel_uncore.c.
// The full MIT grant is retained in ../LICENSE-MIT.
//
// Layout note: `struct intel_uncore uncore` and `struct intel_uncore_mmio_debug
// mmio_debug` sit inside the opaque tail of `DrmI915Private`. Their offsets were
// taken from the oracle x86_64 wt-dev build (`offsetof(struct drm_i915_private,
// ...)`, compiled with the build's own i915 flags); see `I915_UNCORE_OFFSET`.

#![allow(unsafe_code, non_snake_case, non_upper_case_globals, dead_code)]

use core::{
    ffi::{c_char, c_void},
    ptr,
    sync::atomic::{Ordering, fence},
};

use crate::{
    intel_context_upstream::Hrtimer,
    intel_gt_api_upstream::intel_gt_set_wedged_async,
    intel_gt_types_upstream::{GT_MEDIA, IntelGt, IntelMmioRange},
    intel_uncore_types_upstream::{
        DrmDevice, FORCEWAKE_ALL, FORCEWAKE_GSC, FORCEWAKE_GT, FORCEWAKE_MEDIA,
        FORCEWAKE_MEDIA_VDBOX0, FORCEWAKE_MEDIA_VDBOX1, FORCEWAKE_MEDIA_VDBOX2,
        FORCEWAKE_MEDIA_VDBOX3, FORCEWAKE_MEDIA_VDBOX4, FORCEWAKE_MEDIA_VDBOX5,
        FORCEWAKE_MEDIA_VDBOX6, FORCEWAKE_MEDIA_VDBOX7, FORCEWAKE_MEDIA_VEBOX0,
        FORCEWAKE_MEDIA_VEBOX1, FORCEWAKE_MEDIA_VEBOX2, FORCEWAKE_MEDIA_VEBOX3,
        FORCEWAKE_RENDER, FW_DOMAIN_ID_COUNT, FW_DOMAIN_ID_GSC, FW_DOMAIN_ID_GT,
        FW_DOMAIN_ID_MEDIA, FW_DOMAIN_ID_MEDIA_VDBOX0, FW_DOMAIN_ID_MEDIA_VEBOX0,
        FW_DOMAIN_ID_RENDER, FW_REG_READ, FW_REG_WRITE, ForcewakeDomainId, ForcewakeDomains,
        IntelForcewakeRange, IntelRuntimePm, IntelUncore, IntelUncoreForcewakeDomain,
        IntelUncoreFuncs, IntelUncoreFwGet, IntelUncoreMmioDebug, UNCORE_HAS_DBG_UNCLAIMED,
        UNCORE_HAS_FIFO, UNCORE_HAS_FPGA_DBG_UNCLAIMED, UNCORE_HAS_FORCEWAKE,
        __raw_uncore_read8, __raw_uncore_read16, __raw_uncore_read32, __raw_uncore_read64,
        __raw_uncore_write8, __raw_uncore_write16, __raw_uncore_write32, __raw_uncore_write64,
        intel_uncore_has_fifo, intel_uncore_has_forcewake, intel_uncore_needs_flr_on_fini,
        intel_uncore_set_flr_on_fini, intel_wait_for_register_fw,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, IS_CHERRYVIEW, IS_DGFX, IS_GRAPHICS_VER,
            IS_BROADWELL, IS_HASWELL, IS_IVYBRIDGE, IS_VALLEYVIEW, MEDIA_VER, HAS_ENGINE, CCS_MASK,
            RCS_MASK,
        },
        locks::{spin_lock, spin_lock_irq, spin_lock_irqsave, spin_unlock, spin_unlock_irq,
            spin_unlock_irqrestore},
        memory::{atomic_read, kfree},
        primitives::udelay,
        timer::{NSEC_PER_MSEC, hrtimer_start_range_ns},
        wait::{cond_resched, wait_until},
    },
    linux_config::{EINVAL, EIO, ENODEV, ENOMEM, ETIMEDOUT},
    linux_i915_private::DrmI915Private,
};

/// `struct intel_uncore uncore` offset inside `struct drm_i915_private`
/// (x86_64, oracle wt-dev build, from `offsetof` of the i915 headers).
pub const I915_UNCORE_OFFSET: usize = 1840;
/// `struct intel_uncore_mmio_debug mmio_debug` offset inside `drm_i915_private`.
pub const I915_MMIO_DEBUG_OFFSET: usize = 2192;

const FORCEWAKE_ACK_TIMEOUT_MS: u64 = 50;
const GT_FIFO_TIMEOUT_MS: u64 = 10;
const TAINT_WARN: u32 = 9;
/// CONFIG_DRM_I915_DEBUG_RUNTIME_PM is not set in the oracle build.
const CONFIG_DRM_I915_DEBUG_RUNTIME_PM: bool = false;

// ---------------------------------------------------------------------------
// Register offsets from gt/intel_gt_regs.h, i915_reg.h, gt/intel_engine_regs.h
// (Linux 7.2.3). Only the ones this file touches are listed.
// ---------------------------------------------------------------------------

const FORCEWAKE_KERNEL: u32 = 1 << 0;
const FORCEWAKE_KERNEL_FALLBACK: u32 = 1 << 15;
const FORCEWAKE_MT_ENABLE: u32 = 1 << 5;

const GU_CNTL: I915RegT = I915RegT { reg: 0x101010 };
const GU_DEBUG: I915RegT = I915RegT { reg: 0x101018 };
const LMEM_INIT: u32 = 1 << 7;
const DRIVERFLR: u32 = 1 << 31;
const DRIVERFLR_STATUS: u32 = 1 << 31;
const FPGA_DBG: I915RegT = I915RegT { reg: 0x42300 };
const FPGA_DBG_RM_NOCLAIM: u32 = 1 << 31;
const CLAIM_ER: I915RegT = I915RegT {
    reg: 0x180000 + 0x2028,
};
const CLAIM_ER_CLR: u32 = 1 << 31;
const CLAIM_ER_OVERFLOW: u32 = 1 << 16;
const CLAIM_ER_CTR_MASK: u32 = 0xffff;

const ECOBUS: I915RegT = I915RegT { reg: 0xa180 };
const FORCEWAKE_MT: I915RegT = I915RegT { reg: 0xa188 };
const FORCEWAKE_GT_GEN9: I915RegT = I915RegT { reg: 0xa188 };
const FORCEWAKE: I915RegT = I915RegT { reg: 0xa18c };
const FORCEWAKE_MEDIA_GEN9: I915RegT = I915RegT { reg: 0xa270 };
const FORCEWAKE_RENDER_GEN9: I915RegT = I915RegT { reg: 0xa278 };
const FORCEWAKE_REQ_GSC: I915RegT = I915RegT { reg: 0xa618 };
const FORCEWAKE_VLV: I915RegT = I915RegT { reg: 0x1300b0 };
const FORCEWAKE_MEDIA_VLV: I915RegT = I915RegT { reg: 0x1300b8 };
const FORCEWAKE_MT_ACK: I915RegT = I915RegT { reg: 0x130040 };
const FORCEWAKE_ACK: I915RegT = I915RegT { reg: 0x130090 };
const FORCEWAKE_ACK_HSW: I915RegT = I915RegT { reg: 0x130044 };
const FORCEWAKE_ACK_GT_GEN9: I915RegT = I915RegT { reg: 0x130044 };
const FORCEWAKE_ACK_GT_MTL: I915RegT = I915RegT { reg: 0xdfc };
const FORCEWAKE_ACK_RENDER_GEN9: I915RegT = I915RegT { reg: 0xd84 };
const FORCEWAKE_ACK_MEDIA_GEN9: I915RegT = I915RegT { reg: 0xd88 };
const FORCEWAKE_ACK_GSC: I915RegT = I915RegT { reg: 0xdf8 };
const FORCEWAKE_ACK_VLV: I915RegT = I915RegT { reg: 0x1300b4 };
const FORCEWAKE_ACK_MEDIA_VLV: I915RegT = I915RegT { reg: 0x1300bc };
const GTFIFODBG: I915RegT = I915RegT { reg: 0x120000 };
const GTFIFOCTL: I915RegT = I915RegT { reg: 0x120008 };
const GT_FIFO_FREE_ENTRIES_MASK: u32 = 0x7f;
const GT_FIFO_NUM_RESERVED_ENTRIES: u32 = 20;
const GT_FIFO_CTL_BLOCK_ALL_POLICY_STALL: u32 = 1 << 12;
const GT_FIFO_CTL_RC6_POLICY_STALL: u32 = 1 << 11;
const GEN6_GT_THREAD_STATUS_REG: I915RegT = I915RegT { reg: 0x13805c };
const GEN6_GT_THREAD_STATUS_CORE_MASK: u32 = 0x7;

const fn forcewake_ack_media_vdbox_gen11(n: u32) -> I915RegT {
    I915RegT { reg: 0xd50 + n * 4 }
}
const fn forcewake_ack_media_vebox_gen11(n: u32) -> I915RegT {
    I915RegT { reg: 0xd70 + n * 4 }
}
const fn forcewake_media_vdbox_gen11(n: u32) -> I915RegT {
    I915RegT { reg: 0xa540 + n * 4 }
}
const fn forcewake_media_vebox_gen11(n: u32) -> I915RegT {
    I915RegT { reg: 0xa560 + n * 4 }
}

/// `RING_MI_MODE(RENDER_RING_BASE)` is provided by `intel_engine_regs_upstream`.
const RENDER_RING_BASE: u32 = 0x02000;

#[inline]
const fn masked_enable(value: u32) -> u32 {
    value | (value << 16)
}

#[inline]
const fn masked_disable(value: u32) -> u32 {
    value << 16
}

// ---------------------------------------------------------------------------
// MMIO and wait primitives. `readl`/`writel` are volatile accesses to a mapped
// BAR; the kernel `udelay`, `cond_resched` and `wait_until` helpers provide the
// timing behaviour of `wait_for_atomic()` and `wait_for()`.
// ---------------------------------------------------------------------------

#[inline]
unsafe fn readl(address: *const u32) -> u32 {
    unsafe { ptr::read_volatile(address) }
}

#[inline]
unsafe fn writel(value: u32, address: *mut u32) {
    fence(Ordering::SeqCst);
    unsafe { ptr::write_volatile(address, value) };
    fence(Ordering::SeqCst);
}

/// `wait_for_atomic(COND, ms)`: spin, 0 on success, -ETIMEDOUT otherwise.
#[inline]
fn wait_for_atomic_ms(mut condition: impl FnMut() -> bool, timeout_ms: u64) -> i32 {
    if wait_until(timeout_ms * 1_000_000, &mut condition, false) {
        -ETIMEDOUT
    } else {
        0
    }
}

/// `wait_for_atomic_us(COND, us)`.
#[inline]
fn wait_for_atomic_us_local(mut condition: impl FnMut() -> bool, timeout_us: u64) -> i32 {
    if wait_until(timeout_us * 1_000, &mut condition, false) {
        -ETIMEDOUT
    } else {
        0
    }
}

/// `wait_for(COND, ms)`: may sleep.
#[inline]
fn wait_for_ms(mut condition: impl FnMut() -> bool, timeout_ms: u64) -> i32 {
    if wait_until(timeout_ms * 1_000_000, &mut condition, true) {
        -ETIMEDOUT
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Small accessors shared by the translated functions.
// ---------------------------------------------------------------------------

#[inline]
unsafe fn uncore_i915(uncore: *const IntelUncore) -> *mut DrmI915Private {
    unsafe { (*uncore).i915 }
}

#[inline]
unsafe fn uncore_dev(uncore: *const IntelUncore) -> *const DrmDevice {
    unsafe { ptr::addr_of!((*uncore_i915(uncore)).drm).cast::<DrmDevice>() }
}

#[inline]
unsafe fn i915_const(i915: *const DrmI915Private) -> *const DrmI915Private {
    i915
}

#[inline]
unsafe fn i915_graphics_ver(i915: *mut DrmI915Private) -> u8 {
    unsafe { GRAPHICS_VER(i915_const(i915)) }
}

#[inline]
unsafe fn i915_graphics_ver_full(i915: *mut DrmI915Private) -> u16 {
    unsafe { GRAPHICS_VER_FULL(i915_const(i915)) }
}

/// `to_intel_uncore()`: `&to_i915(drm)->uncore`.
///
/// # Safety
/// `drm` must be the `drm` member of a live `DrmI915Private` (offset 0).
// upstream: intel_uncore.c to_intel_uncore()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn to_intel_uncore(drm: *mut DrmDevice) -> *mut IntelUncore {
    assert!(!drm.is_null());
    unsafe { drm.cast::<u8>().add(I915_UNCORE_OFFSET).cast::<IntelUncore>() }
}

/// Kernel-owned `struct intel_uncore_mmio_debug` helpers.
#[inline]
unsafe fn uncore_debug(uncore: *mut IntelUncore) -> *mut IntelUncoreMmioDebug {
    unsafe { (*uncore).debug }
}

// ---------------------------------------------------------------------------
// Forcewake domain register access (fw_set/fw_clear/fw_ack and the wait loops).
// ---------------------------------------------------------------------------

#[inline]
unsafe fn fw_ack(d: *const IntelUncoreForcewakeDomain) -> u32 {
    unsafe { readl((*d).reg_ack) }
}

#[inline]
unsafe fn fw_set(d: *const IntelUncoreForcewakeDomain, val: u32) {
    unsafe { writel(masked_enable(val), (*d).reg_set) };
}

#[inline]
unsafe fn fw_clear(d: *const IntelUncoreForcewakeDomain, val: u32) {
    unsafe { writel(masked_disable(val), (*d).reg_set) };
}

#[inline]
unsafe fn domain_i915(d: *const IntelUncoreForcewakeDomain) -> *mut DrmI915Private {
    unsafe { (*(*d).uncore).i915 }
}

fn forcewake_domain_name(id: ForcewakeDomainId) -> *const c_char {
    // Keep the names NUL-terminated for the C-facing `%s` consumers.
    const NAMES: [&[u8]; 16] = [
        b"render\0",
        b"gt\0",
        b"media\0",
        b"vdbox0\0",
        b"vdbox1\0",
        b"vdbox2\0",
        b"vdbox3\0",
        b"vdbox4\0",
        b"vdbox5\0",
        b"vdbox6\0",
        b"vdbox7\0",
        b"vebox0\0",
        b"vebox1\0",
        b"vebox2\0",
        b"vebox3\0",
        b"gsc\0",
    ];
    if (0..FW_DOMAIN_ID_COUNT).contains(&id) {
        NAMES[id as usize].as_ptr().cast()
    } else {
        b"unknown\0".as_ptr().cast()
    }
}

/// `intel_uncore_forcewake_domain_to_str()`.
// upstream: intel_uncore.c intel_uncore_forcewake_domain_to_str()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_domain_to_str(id: ForcewakeDomainId) -> *const c_char {
    if !(0..FW_DOMAIN_ID_COUNT).contains(&id) {
        // WARN_ON(id) in C: id 0 is the only in-range value that is silent.
        if id != 0 {
            drm_warn_msg("intel_uncore_forcewake_domain_to_str: unknown forcewake domain id");
        }
    }
    forcewake_domain_name(id)
}

fn drm_warn_msg(message: &str) {
    axlog::warn!("i915: {}", message);
}

#[inline]
unsafe fn __wait_for_ack(
    d: *const IntelUncoreForcewakeDomain,
    ack: u32,
    value: u32,
) -> i32 {
    wait_for_atomic_ms(
        || unsafe { fw_ack(d) } & ack == value,
        FORCEWAKE_ACK_TIMEOUT_MS,
    )
}

#[inline]
unsafe fn wait_ack_clear(d: *const IntelUncoreForcewakeDomain, ack: u32) -> i32 {
    unsafe { __wait_for_ack(d, ack, 0) }
}

#[inline]
unsafe fn wait_ack_set(d: *const IntelUncoreForcewakeDomain, ack: u32) -> i32 {
    unsafe { __wait_for_ack(d, ack, ack) }
}

#[inline]
unsafe fn fw_domain_wait_ack_clear(d: *const IntelUncoreForcewakeDomain) {
    if unsafe { wait_ack_clear(d, FORCEWAKE_KERNEL) } == 0 {
        return;
    }

    let i915 = unsafe { domain_i915(d) };
    let id = unsafe { (*d).id };
    if unsafe { fw_ack(d) } == !0 {
        drm_err!(
            unsafe { uncore_dev((*d).uncore) },
            "%s: MMIO unreliable (forcewake register returns 0xFFFFFFFF)!\n",
            intel_uncore_forcewake_domain_to_str(id)
        );
        unsafe { intel_gt_set_wedged_async((*(*d).uncore).gt) };
    } else {
        drm_err!(
            unsafe { uncore_dev((*d).uncore) },
            "%s: timed out waiting for forcewake ack to clear.\n",
            intel_uncore_forcewake_domain_to_str(id)
        );
    }

    // add_taint_for_CI(): CI results are unreliable after this point.
    unsafe { add_taint_for_CI(i915, TAINT_WARN) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AckType {
    Clear = 0,
    Set = 1,
}

unsafe fn fw_domain_wait_ack_with_fallback(
    d: *const IntelUncoreForcewakeDomain,
    type_: AckType,
) -> i32 {
    let ack_bit = FORCEWAKE_KERNEL;
    let value = if type_ == AckType::Set { ack_bit } else { 0 };
    let mut pass: u32 = 1;
    let mut ack_detected;

    // HSDES #1604254524 (WaRsForcewakeAddDelayForAck): toggle the fallback bit
    // to kick the GT state machine so the original ack is delivered.
    loop {
        unsafe { wait_ack_clear(d, FORCEWAKE_KERNEL_FALLBACK) };

        unsafe { fw_set(d, FORCEWAKE_KERNEL_FALLBACK) };
        // Give gt some time to relax before the polling frenzy.
        udelay(10 * pass);
        unsafe { wait_ack_set(d, FORCEWAKE_KERNEL_FALLBACK) };

        ack_detected = (unsafe { fw_ack(d) } & ack_bit) == value;

        unsafe { fw_clear(d, FORCEWAKE_KERNEL_FALLBACK) };

        // `while (!ack_detected && pass++ < 10)`
        if ack_detected {
            break;
        }
        let old_pass = pass;
        pass += 1;
        if old_pass >= 10 {
            break;
        }
    }

    drm_dbg!(
        unsafe { uncore_dev((*d).uncore) },
        "%s had to use fallback to %s ack, 0x%x (passes %u)\n",
        intel_uncore_forcewake_domain_to_str(unsafe { (*d).id }),
        if type_ == AckType::Set { "set" } else { "clear" },
        unsafe { fw_ack(d) },
        pass
    );

    if ack_detected { 0 } else { -ETIMEDOUT }
}

#[inline]
unsafe fn fw_domain_wait_ack_clear_fallback(d: *const IntelUncoreForcewakeDomain) {
    if unsafe { wait_ack_clear(d, FORCEWAKE_KERNEL) } == 0 {
        return;
    }
    if unsafe { fw_domain_wait_ack_with_fallback(d, AckType::Clear) } != 0 {
        unsafe { fw_domain_wait_ack_clear(d) };
    }
}

#[inline]
unsafe fn fw_domain_get(d: *const IntelUncoreForcewakeDomain) {
    unsafe { fw_set(d, FORCEWAKE_KERNEL) };
}

#[inline]
unsafe fn fw_domain_wait_ack_set(d: *const IntelUncoreForcewakeDomain) {
    if unsafe { wait_ack_set(d, FORCEWAKE_KERNEL) } != 0 {
        drm_err!(
            unsafe { uncore_dev((*d).uncore) },
            "%s: timed out waiting for forcewake ack request.\n",
            intel_uncore_forcewake_domain_to_str(unsafe { (*d).id })
        );
        unsafe { add_taint_for_CI(domain_i915(d), TAINT_WARN) };
    }
}

#[inline]
unsafe fn fw_domain_wait_ack_set_fallback(d: *const IntelUncoreForcewakeDomain) {
    if unsafe { wait_ack_set(d, FORCEWAKE_KERNEL) } == 0 {
        return;
    }
    if unsafe { fw_domain_wait_ack_with_fallback(d, AckType::Set) } != 0 {
        unsafe { fw_domain_wait_ack_set(d) };
    }
}

#[inline]
unsafe fn fw_domain_put(d: *const IntelUncoreForcewakeDomain) {
    unsafe { fw_clear(d, FORCEWAKE_KERNEL) };
}

#[inline]
unsafe fn fw_domain_reset(d: *const IntelUncoreForcewakeDomain) {
    // We don't really know if the powerwell for the domain exists at this point
    // (engines may be fused off on ICL+), so do not wait for acks.
    // WaRsClearFWBitsAtReset
    let i915 = unsafe { domain_i915(d) };
    if unsafe { i915_graphics_ver(i915) } >= 12 {
        unsafe { fw_clear(d, 0xefff) };
    } else {
        unsafe { fw_clear(d, 0xffff) };
    }
}

/// Domain iteration with `for_each_fw_domain_masked()` semantics: walks the set
/// bits of `mask` and yields the registered domain for each (skipping holes).
struct DomainIter {
    uncore: *const IntelUncore,
    remaining: ForcewakeDomains,
}

impl Iterator for DomainIter {
    type Item = *mut IntelUncoreForcewakeDomain;

    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining != 0 {
            let bit = (self.remaining as u32).trailing_zeros() as usize;
            self.remaining &= self.remaining - 1;
            let domain = unsafe { (*self.uncore).fw_domain[bit] };
            if !domain.is_null() {
                return Some(domain);
            }
        }
        None
    }
}

#[inline]
fn for_each_fw_domain_masked(
    uncore: *const IntelUncore,
    mask: ForcewakeDomains,
) -> DomainIter {
    DomainIter {
        uncore,
        remaining: mask,
    }
}

#[inline]
fn for_each_fw_domain(uncore: *const IntelUncore) -> DomainIter {
    let mask = unsafe { (*uncore).fw_domains };
    for_each_fw_domain_masked(uncore, mask)
}

unsafe extern "C" fn fw_domains_get_normal(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    // GEM_BUG_ON(fw_domains & ~uncore->fw_domains)
    GEM_BUG_ON!(fw_domains & !unsafe { (*uncore).fw_domains } != 0);

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { fw_domain_wait_ack_clear(d) };
        unsafe { fw_domain_get(d) };
    }

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { fw_domain_wait_ack_set(d) };
    }

    unsafe { (*uncore).fw_domains_active |= fw_domains };
}

unsafe extern "C" fn fw_domains_get_with_fallback(
    uncore: *mut IntelUncore,
    fw_domains: ForcewakeDomains,
) {
    GEM_BUG_ON!(fw_domains & !unsafe { (*uncore).fw_domains } != 0);

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { fw_domain_wait_ack_clear_fallback(d) };
        unsafe { fw_domain_get(d) };
    }

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { fw_domain_wait_ack_set_fallback(d) };
    }

    unsafe { (*uncore).fw_domains_active |= fw_domains };
}

unsafe fn fw_domains_put(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    GEM_BUG_ON!(fw_domains & !unsafe { (*uncore).fw_domains } != 0);

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { fw_domain_put(d) };
    }

    unsafe { (*uncore).fw_domains_active &= !fw_domains };
}

unsafe fn fw_domains_reset(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    if fw_domains == 0 {
        return;
    }

    GEM_BUG_ON!(fw_domains & !unsafe { (*uncore).fw_domains } != 0);

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { fw_domain_reset(d) };
    }
}

#[inline]
unsafe fn gt_thread_status(uncore: *mut IntelUncore) -> u32 {
    let mut val = unsafe { __raw_uncore_read32(uncore, GEN6_GT_THREAD_STATUS_REG) };
    val &= GEN6_GT_THREAD_STATUS_CORE_MASK;
    val
}

unsafe fn gen6_gt_wait_for_thread_c0(uncore: *mut IntelUncore) {
    // w/a for a sporadic read returning 0 by waiting for the GT thread to wake.
    if wait_for_atomic_us_local(|| unsafe { gt_thread_status(uncore) } == 0, 5000) != 0 {
        drm_warn_msg("GT thread status wait timed out");
    }
}

unsafe extern "C" fn fw_domains_get_with_thread_status(
    uncore: *mut IntelUncore,
    fw_domains: ForcewakeDomains,
) {
    unsafe { fw_domains_get_normal(uncore, fw_domains) };

    // WaRsForcewakeWaitTC0:snb,ivb,hsw,bdw,vlv
    unsafe { gen6_gt_wait_for_thread_c0(uncore) };
}

unsafe fn gen6_check_for_fifo_debug(uncore: *mut IntelUncore) {
    let fifodbg = unsafe { __raw_uncore_read32(uncore, GTFIFODBG) };

    if fifodbg != 0 {
        drm_dbg!(
            unsafe { uncore_dev(uncore) },
            "GTFIFODBG = 0x08%x\n",
            fifodbg
        );
        unsafe { __raw_uncore_write32(uncore, GTFIFODBG, fifodbg) };
    }
}

unsafe extern "C" fn fw_domains_get_normal_fifo(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    unsafe { gen6_check_for_fifo_debug(uncore) };
    unsafe { fw_domains_get_normal(uncore, fw_domains) };
}

unsafe extern "C" fn fw_domains_get_with_thread_status_fifo(
    uncore: *mut IntelUncore,
    fw_domains: ForcewakeDomains,
) {
    unsafe { gen6_check_for_fifo_debug(uncore) };
    unsafe { fw_domains_get_with_thread_status(uncore, fw_domains) };
}

#[inline]
unsafe fn fifo_free_entries(uncore: *mut IntelUncore) -> u32 {
    let count = unsafe { __raw_uncore_read32(uncore, GTFIFOCTL) };
    count & GT_FIFO_FREE_ENTRIES_MASK
}

unsafe fn gen6_gt_wait_for_fifo_locked(uncore: *mut IntelUncore) {
    let mut n = if unsafe { IS_VALLEYVIEW(i915_const(uncore_i915(uncore))) } {
        // On VLV the FIFO is shared by SW and HW, so read FREE_ENTRIES every time.
        unsafe { fifo_free_entries(uncore) }
    } else {
        unsafe { (*uncore).fifo_count }
    };

    if n <= GT_FIFO_NUM_RESERVED_ENTRIES {
        let mut free = n;
        if wait_for_atomic_us_local(
            || {
                free = unsafe { fifo_free_entries(uncore) };
                free > GT_FIFO_NUM_RESERVED_ENTRIES
            },
            GT_FIFO_TIMEOUT_MS * 1000,
        ) != 0
        {
            drm_dbg!(
                unsafe { uncore_dev(uncore) },
                "GT_FIFO timeout, entries: %u\n",
                free
            );
            return;
        }
        n = free;
    }

    unsafe { (*uncore).fifo_count = n - 1 };
}
// upstream: intel_uncore.c gen8_shadowed_regs[]
static GEN8_SHADOWED_REGS: [IntelMmioRange; 5] = [
    IntelMmioRange { start: 0x2030, end: 0x2030 },
    IntelMmioRange { start: 0xA008, end: 0xA00C },
    IntelMmioRange { start: 0x12030, end: 0x12030 },
    IntelMmioRange { start: 0x1a030, end: 0x1a030 },
    IntelMmioRange { start: 0x22030, end: 0x22030 },
];

// upstream: intel_uncore.c gen11_shadowed_regs[]
static GEN11_SHADOWED_REGS: [IntelMmioRange; 24] = [
    IntelMmioRange { start: 0x2030, end: 0x2030 },
    IntelMmioRange { start: 0x2550, end: 0x2550 },
    IntelMmioRange { start: 0xA008, end: 0xA00C },
    IntelMmioRange { start: 0x22030, end: 0x22030 },
    IntelMmioRange { start: 0x22230, end: 0x22230 },
    IntelMmioRange { start: 0x22510, end: 0x22550 },
    IntelMmioRange { start: 0x1C0030, end: 0x1C0030 },
    IntelMmioRange { start: 0x1C0230, end: 0x1C0230 },
    IntelMmioRange { start: 0x1C0510, end: 0x1C0550 },
    IntelMmioRange { start: 0x1C4030, end: 0x1C4030 },
    IntelMmioRange { start: 0x1C4230, end: 0x1C4230 },
    IntelMmioRange { start: 0x1C4510, end: 0x1C4550 },
    IntelMmioRange { start: 0x1C8030, end: 0x1C8030 },
    IntelMmioRange { start: 0x1C8230, end: 0x1C8230 },
    IntelMmioRange { start: 0x1C8510, end: 0x1C8550 },
    IntelMmioRange { start: 0x1D0030, end: 0x1D0030 },
    IntelMmioRange { start: 0x1D0230, end: 0x1D0230 },
    IntelMmioRange { start: 0x1D0510, end: 0x1D0550 },
    IntelMmioRange { start: 0x1D4030, end: 0x1D4030 },
    IntelMmioRange { start: 0x1D4230, end: 0x1D4230 },
    IntelMmioRange { start: 0x1D4510, end: 0x1D4550 },
    IntelMmioRange { start: 0x1D8030, end: 0x1D8030 },
    IntelMmioRange { start: 0x1D8230, end: 0x1D8230 },
    IntelMmioRange { start: 0x1D8510, end: 0x1D8550 },
];

// upstream: intel_uncore.c gen12_shadowed_regs[]
static GEN12_SHADOWED_REGS: [IntelMmioRange; 35] = [
    IntelMmioRange { start: 0x2030, end: 0x2030 },
    IntelMmioRange { start: 0x2510, end: 0x2550 },
    IntelMmioRange { start: 0xA008, end: 0xA00C },
    IntelMmioRange { start: 0xA188, end: 0xA188 },
    IntelMmioRange { start: 0xA278, end: 0xA278 },
    IntelMmioRange { start: 0xA540, end: 0xA56C },
    IntelMmioRange { start: 0xC4C8, end: 0xC4C8 },
    IntelMmioRange { start: 0xC4D4, end: 0xC4D4 },
    IntelMmioRange { start: 0xC600, end: 0xC600 },
    IntelMmioRange { start: 0x22030, end: 0x22030 },
    IntelMmioRange { start: 0x22510, end: 0x22550 },
    IntelMmioRange { start: 0x1C0030, end: 0x1C0030 },
    IntelMmioRange { start: 0x1C0510, end: 0x1C0550 },
    IntelMmioRange { start: 0x1C4030, end: 0x1C4030 },
    IntelMmioRange { start: 0x1C4510, end: 0x1C4550 },
    IntelMmioRange { start: 0x1C8030, end: 0x1C8030 },
    IntelMmioRange { start: 0x1C8510, end: 0x1C8550 },
    IntelMmioRange { start: 0x1D0030, end: 0x1D0030 },
    IntelMmioRange { start: 0x1D0510, end: 0x1D0550 },
    IntelMmioRange { start: 0x1D4030, end: 0x1D4030 },
    IntelMmioRange { start: 0x1D4510, end: 0x1D4550 },
    IntelMmioRange { start: 0x1D8030, end: 0x1D8030 },
    IntelMmioRange { start: 0x1D8510, end: 0x1D8550 },
    IntelMmioRange { start: 0x1E0030, end: 0x1E0030 },
    IntelMmioRange { start: 0x1E0510, end: 0x1E0550 },
    IntelMmioRange { start: 0x1E4030, end: 0x1E4030 },
    IntelMmioRange { start: 0x1E4510, end: 0x1E4550 },
    IntelMmioRange { start: 0x1E8030, end: 0x1E8030 },
    IntelMmioRange { start: 0x1E8510, end: 0x1E8550 },
    IntelMmioRange { start: 0x1F0030, end: 0x1F0030 },
    IntelMmioRange { start: 0x1F0510, end: 0x1F0550 },
    IntelMmioRange { start: 0x1F4030, end: 0x1F4030 },
    IntelMmioRange { start: 0x1F4510, end: 0x1F4550 },
    IntelMmioRange { start: 0x1F8030, end: 0x1F8030 },
    IntelMmioRange { start: 0x1F8510, end: 0x1F8550 },
];

// upstream: intel_uncore.c dg2_shadowed_regs[]
static DG2_SHADOWED_REGS: [IntelMmioRange; 36] = [
    IntelMmioRange { start: 0x2030, end: 0x2030 },
    IntelMmioRange { start: 0x2510, end: 0x2550 },
    IntelMmioRange { start: 0xA008, end: 0xA00C },
    IntelMmioRange { start: 0xA188, end: 0xA188 },
    IntelMmioRange { start: 0xA278, end: 0xA278 },
    IntelMmioRange { start: 0xA540, end: 0xA56C },
    IntelMmioRange { start: 0xC4C8, end: 0xC4C8 },
    IntelMmioRange { start: 0xC4E0, end: 0xC4E0 },
    IntelMmioRange { start: 0xC600, end: 0xC600 },
    IntelMmioRange { start: 0xC658, end: 0xC658 },
    IntelMmioRange { start: 0x22030, end: 0x22030 },
    IntelMmioRange { start: 0x22510, end: 0x22550 },
    IntelMmioRange { start: 0x1C0030, end: 0x1C0030 },
    IntelMmioRange { start: 0x1C0510, end: 0x1C0550 },
    IntelMmioRange { start: 0x1C4030, end: 0x1C4030 },
    IntelMmioRange { start: 0x1C4510, end: 0x1C4550 },
    IntelMmioRange { start: 0x1C8030, end: 0x1C8030 },
    IntelMmioRange { start: 0x1C8510, end: 0x1C8550 },
    IntelMmioRange { start: 0x1D0030, end: 0x1D0030 },
    IntelMmioRange { start: 0x1D0510, end: 0x1D0550 },
    IntelMmioRange { start: 0x1D4030, end: 0x1D4030 },
    IntelMmioRange { start: 0x1D4510, end: 0x1D4550 },
    IntelMmioRange { start: 0x1D8030, end: 0x1D8030 },
    IntelMmioRange { start: 0x1D8510, end: 0x1D8550 },
    IntelMmioRange { start: 0x1E0030, end: 0x1E0030 },
    IntelMmioRange { start: 0x1E0510, end: 0x1E0550 },
    IntelMmioRange { start: 0x1E4030, end: 0x1E4030 },
    IntelMmioRange { start: 0x1E4510, end: 0x1E4550 },
    IntelMmioRange { start: 0x1E8030, end: 0x1E8030 },
    IntelMmioRange { start: 0x1E8510, end: 0x1E8550 },
    IntelMmioRange { start: 0x1F0030, end: 0x1F0030 },
    IntelMmioRange { start: 0x1F0510, end: 0x1F0550 },
    IntelMmioRange { start: 0x1F4030, end: 0x1F4030 },
    IntelMmioRange { start: 0x1F4510, end: 0x1F4550 },
    IntelMmioRange { start: 0x1F8030, end: 0x1F8030 },
    IntelMmioRange { start: 0x1F8510, end: 0x1F8550 },
];

// upstream: intel_uncore.c mtl_shadowed_regs[]
static MTL_SHADOWED_REGS: [IntelMmioRange; 15] = [
    IntelMmioRange { start: 0x2030, end: 0x2030 },
    IntelMmioRange { start: 0x2510, end: 0x2550 },
    IntelMmioRange { start: 0xA008, end: 0xA00C },
    IntelMmioRange { start: 0xA188, end: 0xA188 },
    IntelMmioRange { start: 0xA278, end: 0xA278 },
    IntelMmioRange { start: 0xA540, end: 0xA56C },
    IntelMmioRange { start: 0xC050, end: 0xC050 },
    IntelMmioRange { start: 0xC340, end: 0xC340 },
    IntelMmioRange { start: 0xC4C8, end: 0xC4C8 },
    IntelMmioRange { start: 0xC4E0, end: 0xC4E0 },
    IntelMmioRange { start: 0xC600, end: 0xC600 },
    IntelMmioRange { start: 0xC658, end: 0xC658 },
    IntelMmioRange { start: 0xCFD4, end: 0xCFDC },
    IntelMmioRange { start: 0x22030, end: 0x22030 },
    IntelMmioRange { start: 0x22510, end: 0x22550 },
];

// upstream: intel_uncore.c xelpmp_shadowed_regs[]
static XELPMP_SHADOWED_REGS: [IntelMmioRange; 18] = [
    IntelMmioRange { start: 0x1C0030, end: 0x1C0030 },
    IntelMmioRange { start: 0x1C0510, end: 0x1C0550 },
    IntelMmioRange { start: 0x1C8030, end: 0x1C8030 },
    IntelMmioRange { start: 0x1C8510, end: 0x1C8550 },
    IntelMmioRange { start: 0x1D0030, end: 0x1D0030 },
    IntelMmioRange { start: 0x1D0510, end: 0x1D0550 },
    IntelMmioRange { start: 0x38A008, end: 0x38A00C },
    IntelMmioRange { start: 0x38A188, end: 0x38A188 },
    IntelMmioRange { start: 0x38A278, end: 0x38A278 },
    IntelMmioRange { start: 0x38A540, end: 0x38A56C },
    IntelMmioRange { start: 0x38A618, end: 0x38A618 },
    IntelMmioRange { start: 0x38C050, end: 0x38C050 },
    IntelMmioRange { start: 0x38C340, end: 0x38C340 },
    IntelMmioRange { start: 0x38C4C8, end: 0x38C4C8 },
    IntelMmioRange { start: 0x38C4E0, end: 0x38C4E4 },
    IntelMmioRange { start: 0x38C600, end: 0x38C600 },
    IntelMmioRange { start: 0x38C658, end: 0x38C658 },
    IntelMmioRange { start: 0x38CFD4, end: 0x38CFDC },
];

// upstream: intel_uncore.c __gen6_fw_ranges[]
static GEN6_FW_RANGES: [IntelForcewakeRange; 1] = [
    IntelForcewakeRange { start: 0x0, end: 0x3ffff, domains: FORCEWAKE_RENDER },
];

// upstream: intel_uncore.c __vlv_fw_ranges[]
static VLV_FW_RANGES: [IntelForcewakeRange; 7] = [
    IntelForcewakeRange { start: 0x2000, end: 0x3fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x5000, end: 0x7fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xb000, end: 0x11fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x12000, end: 0x13fff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x22000, end: 0x23fff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x2e000, end: 0x2ffff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x30000, end: 0x3ffff, domains: FORCEWAKE_MEDIA },
];

// upstream: intel_uncore.c __chv_fw_ranges[]
static CHV_FW_RANGES: [IntelForcewakeRange; 16] = [
    IntelForcewakeRange { start: 0x2000, end: 0x3fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x4000, end: 0x4fff, domains: FORCEWAKE_RENDER | FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x5200, end: 0x7fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8000, end: 0x82ff, domains: FORCEWAKE_RENDER | FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x8300, end: 0x84ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8500, end: 0x85ff, domains: FORCEWAKE_RENDER | FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x8800, end: 0x88ff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x9000, end: 0xafff, domains: FORCEWAKE_RENDER | FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0xb000, end: 0xb47f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xd000, end: 0xd7ff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0xe000, end: 0xe7ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xf000, end: 0xffff, domains: FORCEWAKE_RENDER | FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x12000, end: 0x13fff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x1a000, end: 0x1bfff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x1e800, end: 0x1e9ff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x30000, end: 0x37fff, domains: FORCEWAKE_MEDIA },
];

// upstream: intel_uncore.c __gen9_fw_ranges[]
static GEN9_FW_RANGES: [IntelForcewakeRange; 32] = [
    IntelForcewakeRange { start: 0x0, end: 0xaff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xb00, end: 0x1fff, domains: 0 },
    IntelForcewakeRange { start: 0x2000, end: 0x26ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x2700, end: 0x2fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x3000, end: 0x3fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x4000, end: 0x51ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x5200, end: 0x7fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8000, end: 0x812f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8130, end: 0x813f, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x8140, end: 0x815f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8160, end: 0x82ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8300, end: 0x84ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8500, end: 0x87ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8800, end: 0x89ff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x8a00, end: 0x8bff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8c00, end: 0x8cff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8d00, end: 0x93ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x9400, end: 0x97ff, domains: FORCEWAKE_RENDER | FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x9800, end: 0xafff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xb000, end: 0xb47f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xb480, end: 0xcfff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xd000, end: 0xd7ff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0xd800, end: 0xdfff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xe000, end: 0xe8ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xe900, end: 0x11fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x12000, end: 0x13fff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x14000, end: 0x19fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x1a000, end: 0x1e9ff, domains: FORCEWAKE_MEDIA },
    IntelForcewakeRange { start: 0x1ea00, end: 0x243ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24400, end: 0x247ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x24800, end: 0x2ffff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x30000, end: 0x3ffff, domains: FORCEWAKE_MEDIA },
];

// upstream: intel_uncore.c __gen11_fw_ranges[]
static GEN11_FW_RANGES: [IntelForcewakeRange; 35] = [
    IntelForcewakeRange { start: 0x0, end: 0x1fff, domains: 0 },
    IntelForcewakeRange { start: 0x2000, end: 0x26ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x2700, end: 0x2fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x3000, end: 0x3fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x4000, end: 0x51ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x5200, end: 0x7fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8000, end: 0x813f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8140, end: 0x815f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8160, end: 0x82ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8300, end: 0x84ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8500, end: 0x87ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8800, end: 0x8bff, domains: 0 },
    IntelForcewakeRange { start: 0x8c00, end: 0x8cff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8d00, end: 0x94cf, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x94d0, end: 0x955f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x9560, end: 0x95ff, domains: 0 },
    IntelForcewakeRange { start: 0x9600, end: 0xafff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xb000, end: 0xb47f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xb480, end: 0xdeff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xdf00, end: 0xe8ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xe900, end: 0x16dff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x16e00, end: 0x19fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x1a000, end: 0x23fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24000, end: 0x2407f, domains: 0 },
    IntelForcewakeRange { start: 0x24080, end: 0x2417f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24180, end: 0x242ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x24300, end: 0x243ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24400, end: 0x24fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x25000, end: 0x3ffff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x40000, end: 0x1bffff, domains: 0 },
    IntelForcewakeRange { start: 0x1c0000, end: 0x1c3fff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x1c4000, end: 0x1c7fff, domains: 0 },
    IntelForcewakeRange { start: 0x1c8000, end: 0x1cffff, domains: FORCEWAKE_MEDIA_VEBOX0 },
    IntelForcewakeRange { start: 0x1d0000, end: 0x1d3fff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x1d4000, end: 0x1dbfff, domains: 0 },
];

// upstream: intel_uncore.c __gen12_fw_ranges[]
static GEN12_FW_RANGES: [IntelForcewakeRange; 43] = [
    IntelForcewakeRange { start: 0x0, end: 0x1fff, domains: 0 },
    IntelForcewakeRange { start: 0x2000, end: 0x26ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x2700, end: 0x27ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x2800, end: 0x2aff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x2b00, end: 0x2fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x3000, end: 0x3fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x4000, end: 0x51ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x5200, end: 0x7fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8000, end: 0x813f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8140, end: 0x815f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8160, end: 0x81ff, domains: 0 },
    IntelForcewakeRange { start: 0x8200, end: 0x82ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8300, end: 0x84ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8500, end: 0x94cf, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x94d0, end: 0x955f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x9560, end: 0x97ff, domains: 0 },
    IntelForcewakeRange { start: 0x9800, end: 0xafff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xb000, end: 0xb3ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xb400, end: 0xcfff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xd000, end: 0xd7ff, domains: 0 },
    IntelForcewakeRange { start: 0xd800, end: 0xd8ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xd900, end: 0xdbff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xdc00, end: 0xefff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xf000, end: 0x147ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x14800, end: 0x1ffff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x20000, end: 0x20fff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x21000, end: 0x21fff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x22000, end: 0x23fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24000, end: 0x2417f, domains: 0 },
    IntelForcewakeRange { start: 0x24180, end: 0x249ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24a00, end: 0x251ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x25200, end: 0x255ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x25600, end: 0x2567f, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x25680, end: 0x259ff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x25a00, end: 0x25a7f, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x25a80, end: 0x2ffff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x30000, end: 0x3ffff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x40000, end: 0x1bffff, domains: 0 },
    IntelForcewakeRange { start: 0x1c0000, end: 0x1c3fff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x1c4000, end: 0x1c7fff, domains: 0 },
    IntelForcewakeRange { start: 0x1c8000, end: 0x1cbfff, domains: FORCEWAKE_MEDIA_VEBOX0 },
    IntelForcewakeRange { start: 0x1cc000, end: 0x1cffff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x1d0000, end: 0x1d3fff, domains: FORCEWAKE_MEDIA_VDBOX2 },
];

// upstream: intel_uncore.c __dg2_fw_ranges[]
static DG2_FW_RANGES: [IntelForcewakeRange; 54] = [
    IntelForcewakeRange { start: 0x0, end: 0x1fff, domains: 0 },
    IntelForcewakeRange { start: 0x2000, end: 0x26ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x2700, end: 0x4aff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x4b00, end: 0x51ff, domains: 0 },
    IntelForcewakeRange { start: 0x5200, end: 0x7fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8000, end: 0x813f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8140, end: 0x815f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8160, end: 0x81ff, domains: 0 },
    IntelForcewakeRange { start: 0x8200, end: 0x82ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8300, end: 0x84ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8500, end: 0x8cff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8d00, end: 0x8fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x9000, end: 0x94cf, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x94d0, end: 0x955f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x9560, end: 0x967f, domains: 0 },
    IntelForcewakeRange { start: 0x9680, end: 0x97ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x9800, end: 0xcfff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xd000, end: 0xd7ff, domains: 0 },
    IntelForcewakeRange { start: 0xd800, end: 0xd87f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xd880, end: 0xdbff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xdc00, end: 0xdcff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xdd00, end: 0xde7f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xde80, end: 0xe8ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xe900, end: 0xffff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x10000, end: 0x12fff, domains: 0 },
    IntelForcewakeRange { start: 0x13000, end: 0x131ff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x13200, end: 0x147ff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x14800, end: 0x14fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x15000, end: 0x16dff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x16e00, end: 0x21fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x22000, end: 0x23fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24000, end: 0x2417f, domains: 0 },
    IntelForcewakeRange { start: 0x24180, end: 0x249ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24a00, end: 0x251ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x25200, end: 0x25fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x26000, end: 0x2ffff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x30000, end: 0x3ffff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x40000, end: 0x1bffff, domains: 0 },
    IntelForcewakeRange { start: 0x1c0000, end: 0x1c3fff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x1c4000, end: 0x1c7fff, domains: FORCEWAKE_MEDIA_VDBOX1 },
    IntelForcewakeRange { start: 0x1c8000, end: 0x1cbfff, domains: FORCEWAKE_MEDIA_VEBOX0 },
    IntelForcewakeRange { start: 0x1cc000, end: 0x1ccfff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x1cd000, end: 0x1cdfff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x1ce000, end: 0x1cefff, domains: FORCEWAKE_MEDIA_VDBOX4 },
    IntelForcewakeRange { start: 0x1cf000, end: 0x1cffff, domains: FORCEWAKE_MEDIA_VDBOX6 },
    IntelForcewakeRange { start: 0x1d0000, end: 0x1d3fff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x1d4000, end: 0x1d7fff, domains: FORCEWAKE_MEDIA_VDBOX3 },
    IntelForcewakeRange { start: 0x1d8000, end: 0x1dffff, domains: FORCEWAKE_MEDIA_VEBOX1 },
    IntelForcewakeRange { start: 0x1e0000, end: 0x1e3fff, domains: FORCEWAKE_MEDIA_VDBOX4 },
    IntelForcewakeRange { start: 0x1e4000, end: 0x1e7fff, domains: FORCEWAKE_MEDIA_VDBOX5 },
    IntelForcewakeRange { start: 0x1e8000, end: 0x1effff, domains: FORCEWAKE_MEDIA_VEBOX2 },
    IntelForcewakeRange { start: 0x1f0000, end: 0x1f3fff, domains: FORCEWAKE_MEDIA_VDBOX6 },
    IntelForcewakeRange { start: 0x1f4000, end: 0x1f7fff, domains: FORCEWAKE_MEDIA_VDBOX7 },
    IntelForcewakeRange { start: 0x1f8000, end: 0x1fa0ff, domains: FORCEWAKE_MEDIA_VEBOX3 },
];

// upstream: intel_uncore.c __mtl_fw_ranges[]
static MTL_FW_RANGES: [IntelForcewakeRange; 32] = [
    IntelForcewakeRange { start: 0x0, end: 0xaff, domains: 0 },
    IntelForcewakeRange { start: 0xb00, end: 0xbff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xc00, end: 0xfff, domains: 0 },
    IntelForcewakeRange { start: 0x1000, end: 0x1fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x2000, end: 0x26ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x2700, end: 0x2fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x3000, end: 0x3fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x4000, end: 0x51ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x5200, end: 0x7fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8000, end: 0x813f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x8140, end: 0x817f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x8180, end: 0x81ff, domains: 0 },
    IntelForcewakeRange { start: 0x8200, end: 0x94cf, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x94d0, end: 0x955f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x9560, end: 0x967f, domains: 0 },
    IntelForcewakeRange { start: 0x9680, end: 0x97ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x9800, end: 0xcfff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xd000, end: 0xd7ff, domains: 0 },
    IntelForcewakeRange { start: 0xd800, end: 0xd87f, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xd880, end: 0xdbff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xdc00, end: 0xdcff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xdd00, end: 0xde7f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xde80, end: 0xe8ff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0xe900, end: 0xe9ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0xea00, end: 0x147ff, domains: 0 },
    IntelForcewakeRange { start: 0x14800, end: 0x19fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x1a000, end: 0x21fff, domains: FORCEWAKE_RENDER },
    IntelForcewakeRange { start: 0x22000, end: 0x23fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x24000, end: 0x2ffff, domains: 0 },
    IntelForcewakeRange { start: 0x30000, end: 0x3ffff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x40000, end: 0x1901ef, domains: 0 },
    IntelForcewakeRange { start: 0x1901f0, end: 0x1901f3, domains: FORCEWAKE_GT },
];

// upstream: intel_uncore.c __xelpmp_fw_ranges[]
static XELPMP_FW_RANGES: [IntelForcewakeRange; 25] = [
    IntelForcewakeRange { start: 0x0, end: 0x115fff, domains: 0 },
    IntelForcewakeRange { start: 0x116000, end: 0x11ffff, domains: FORCEWAKE_GSC },
    IntelForcewakeRange { start: 0x120000, end: 0x1bffff, domains: 0 },
    IntelForcewakeRange { start: 0x1c0000, end: 0x1c7fff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x1c8000, end: 0x1cbfff, domains: FORCEWAKE_MEDIA_VEBOX0 },
    IntelForcewakeRange { start: 0x1cc000, end: 0x1cffff, domains: FORCEWAKE_MEDIA_VDBOX0 },
    IntelForcewakeRange { start: 0x1d0000, end: 0x1d7fff, domains: FORCEWAKE_MEDIA_VDBOX2 },
    IntelForcewakeRange { start: 0x1d8000, end: 0x1da0ff, domains: FORCEWAKE_MEDIA_VEBOX1 },
    IntelForcewakeRange { start: 0x1da100, end: 0x380aff, domains: 0 },
    IntelForcewakeRange { start: 0x380b00, end: 0x380bff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x380c00, end: 0x380fff, domains: 0 },
    IntelForcewakeRange { start: 0x381000, end: 0x38817f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x388180, end: 0x3882ff, domains: 0 },
    IntelForcewakeRange { start: 0x388300, end: 0x38955f, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x389560, end: 0x389fff, domains: 0 },
    IntelForcewakeRange { start: 0x38a000, end: 0x38cfff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x38d000, end: 0x38d11f, domains: 0 },
    IntelForcewakeRange { start: 0x38d120, end: 0x391fff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x392000, end: 0x392fff, domains: 0 },
    IntelForcewakeRange { start: 0x393000, end: 0x3931ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x393200, end: 0x39323f, domains: FORCEWAKE_ALL },
    IntelForcewakeRange { start: 0x393240, end: 0x3933ff, domains: FORCEWAKE_GT },
    IntelForcewakeRange { start: 0x393400, end: 0x3934ff, domains: FORCEWAKE_ALL },
    IntelForcewakeRange { start: 0x393500, end: 0x393c7f, domains: 0 },
    IntelForcewakeRange { start: 0x393c80, end: 0x393dff, domains: FORCEWAKE_GT },
];


// ---------------------------------------------------------------------------
// Release timer and the forcewake reset path.
// ---------------------------------------------------------------------------

const HRTIMER_NORESTART: i32 = 0;
const HRTIMER_RESTART: i32 = 1;

#[inline]
unsafe fn fw_domain_arm_timer(d: *mut IntelUncoreForcewakeDomain) {
    let uncore = unsafe { (*d).uncore };
    // GEM_BUG_ON(d->uncore->fw_domains_timer & d->mask)
    GEM_BUG_ON!(unsafe { (*uncore).fw_domains_timer } & unsafe { (*d).mask } != 0);
    unsafe { (*uncore).fw_domains_timer |= (*d).mask };
    unsafe { (*d).wake_count += 1 };
    unsafe {
        hrtimer_start_range_ns(
            ptr::addr_of_mut!((*d).timer),
            NSEC_PER_MSEC as i64,
            NSEC_PER_MSEC,
            crate::linux::timer::HRTIMER_MODE_REL,
        )
    };
}

/// `intel_uncore_fw_release_timer()`: the hrtimer callback for a domain.
///
/// # Safety
/// `timer` must be the `timer` member of a live `IntelUncoreForcewakeDomain`.
// upstream: intel_uncore.c intel_uncore_fw_release_timer()
unsafe extern "C" fn intel_uncore_fw_release_timer(timer: *mut Hrtimer) -> i32 {
    let domain = unsafe {
        timer
            .cast::<u8>()
            .sub(core::mem::offset_of!(IntelUncoreForcewakeDomain, timer))
            .cast::<IntelUncoreForcewakeDomain>()
    };
    let uncore = unsafe { (*domain).uncore };

    unsafe { assert_rpm_device_not_suspended((*uncore).rpm) };

    // xchg(&domain->active, false)
    let was_active = unsafe { ptr::replace(ptr::addr_of_mut!((*domain).active), false) };
    if was_active {
        return HRTIMER_RESTART;
    }

    let mut irqflags = 0;
    unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };

    unsafe { (*uncore).fw_domains_timer &= !(*domain).mask };

    GEM_BUG_ON!(unsafe { (*domain).wake_count } == 0);
    unsafe { (*domain).wake_count -= 1 };
    if unsafe { (*domain).wake_count } == 0 {
        unsafe { fw_domains_put(uncore, (*domain).mask) };
    }

    unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };

    HRTIMER_NORESTART
}

/// `hrtimer_active()` for a forcewake domain. The uncore's `fw_domains_timer`
/// bit is set when the release timer is armed and cleared when the release
/// path runs, so it tracks the same pending state the C helper reports.
#[inline]
unsafe fn fw_domain_timer_pending(uncore: *const IntelUncore, mask: ForcewakeDomains) -> bool {
    (unsafe { (*uncore).fw_domains_timer }) & mask != 0
}

/// Forcewake reset: drain pending release timers, drop all references and
/// reprogram the domains. Callers must hold the PUNIT->PMIC bus.
// upstream: intel_uncore.c intel_uncore_forcewake_reset()
unsafe fn intel_uncore_forcewake_reset(uncore: *mut IntelUncore) -> ForcewakeDomains {
    let mut irqflags = 0;
    let mut retry_count = 100;
    let mut active_domains: ForcewakeDomains;

    unsafe { iosf_mbi_assert_punit_acquired() };

    // Hold uncore.lock across reset to prevent any register access with
    // forcewake not set correctly. Wait until all pending timers are run.
    loop {
        active_domains = 0;

        for d in for_each_fw_domain(uncore) {
            // smp_store_mb(domain->active, false)
            unsafe { ptr::write_volatile(ptr::addr_of_mut!((*d).active), false) };
            fence(Ordering::SeqCst);
            if unsafe { hrtimer_cancel(ptr::addr_of_mut!((*d).timer)) } == 0 {
                continue;
            }

            unsafe { intel_uncore_fw_release_timer(ptr::addr_of_mut!((*d).timer)) };
        }

        unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };

        for d in for_each_fw_domain(uncore) {
            if unsafe { fw_domain_timer_pending(uncore, (*d).mask) } {
                active_domains |= unsafe { (*d).mask };
            }
        }

        if active_domains == 0 {
            break;
        }

        retry_count -= 1;
        if retry_count == 0 {
            drm_err!(
                unsafe { uncore_dev(uncore) },
                "Timed out waiting for forcewake timers to finish\n"
            );
            break;
        }

        unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };
        cond_resched();
    }

    if active_domains != 0 {
        drm_warn_msg("forcewake timers still active after reset");
    }

    let fw = unsafe { (*uncore).fw_domains_active };
    if fw != 0 {
        unsafe { fw_domains_put(uncore, fw) };
    }

    unsafe { fw_domains_reset(uncore, (*uncore).fw_domains) };
    unsafe { assert_forcewakes_inactive(uncore) };

    unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };

    fw // track the lost user forcewake domains
}

// ---------------------------------------------------------------------------
// Unclaimed MMIO detection.
// ---------------------------------------------------------------------------

unsafe fn fpga_check_for_unclaimed_mmio(uncore: *mut IntelUncore) -> bool {
    let dbg = unsafe { __raw_uncore_read32(uncore, FPGA_DBG) };
    if dbg & FPGA_DBG_RM_NOCLAIM == 0 {
        return false;
    }

    // Lost MMIO BAR access reads back as 0xFFFFFFFF everywhere; the FPGA_DBG
    // unused bits are the canary for that.
    if dbg == !0 {
        drm_err!(
            unsafe { uncore_dev(uncore) },
            "Lost access to MMIO BAR; all registers now read back as 0xFFFFFFFF!\n"
        );
    }

    unsafe { __raw_uncore_write32(uncore, FPGA_DBG, FPGA_DBG_RM_NOCLAIM) };

    true
}

unsafe fn vlv_check_for_unclaimed_mmio(uncore: *mut IntelUncore) -> bool {
    let cer = unsafe { __raw_uncore_read32(uncore, CLAIM_ER) };
    if cer & (CLAIM_ER_OVERFLOW | CLAIM_ER_CTR_MASK) == 0 {
        return false;
    }

    unsafe { __raw_uncore_write32(uncore, CLAIM_ER, CLAIM_ER_CLR) };

    true
}

/// `check_for_unclaimed_mmio()`. The caller holds `uncore->debug->lock`.
unsafe fn check_for_unclaimed_mmio(uncore: *mut IntelUncore) -> bool {
    let mut ret = false;

    if unsafe { (*(*uncore).debug).suspend_count } != 0 {
        return false;
    }

    if intel_uncore_has_fpga_dbg_unclaimed(uncore) {
        ret |= unsafe { fpga_check_for_unclaimed_mmio(uncore) };
    }

    if intel_uncore_has_dbg_unclaimed(uncore) {
        ret |= unsafe { vlv_check_for_unclaimed_mmio(uncore) };
    }

    ret
}

#[inline]
unsafe fn intel_uncore_has_fpga_dbg_unclaimed(uncore: *const IntelUncore) -> bool {
    unsafe { (*uncore).flags & UNCORE_HAS_FPGA_DBG_UNCLAIMED != 0 }
}

#[inline]
unsafe fn intel_uncore_has_dbg_unclaimed(uncore: *const IntelUncore) -> bool {
    unsafe { (*uncore).flags & UNCORE_HAS_DBG_UNCLAIMED != 0 }
}

#[inline]
unsafe fn mmio_debug_params(uncore: *const IntelUncore) -> i32 {
    unsafe { (*uncore_i915(uncore)).params.mmio_debug }
}

#[inline]
unsafe fn set_mmio_debug_param(uncore: *const IntelUncore, value: i32) {
    unsafe { (*uncore_i915(uncore)).params.mmio_debug = value };
}

unsafe fn mmio_debug_suspend(uncore: *mut IntelUncore) {
    let debug = unsafe { uncore_debug(uncore) };
    if debug.is_null() {
        return;
    }

    unsafe { spin_lock(&mut (*debug).lock) };

    // Save and disable mmio debugging for the user bypass.
    let count = unsafe { (*debug).suspend_count };
    unsafe { (*debug).suspend_count = count.wrapping_add(1) };
    if count == 0 {
        unsafe {
            (*debug).saved_mmio_check = (*debug).unclaimed_mmio_check;
            (*debug).unclaimed_mmio_check = 0;
        }
    }

    unsafe { spin_unlock(&mut (*debug).lock) };
}

unsafe fn mmio_debug_resume(uncore: *mut IntelUncore) {
    let debug = unsafe { uncore_debug(uncore) };
    if debug.is_null() {
        return;
    }

    unsafe { spin_lock(&mut (*debug).lock) };

    unsafe { (*debug).suspend_count = (*debug).suspend_count.wrapping_sub(1) };
    if unsafe { (*debug).suspend_count } == 0 {
        unsafe { (*debug).unclaimed_mmio_check = (*debug).saved_mmio_check };
    }

    if unsafe { check_for_unclaimed_mmio(uncore) } {
        drm_notice!(
            unsafe { uncore_dev(uncore) },
            "Invalid mmio detected during user access\n"
        );
    }

    unsafe { spin_unlock(&mut (*debug).lock) };
}

/// `__unclaimed_reg_debug()`.
unsafe fn unclaimed_reg_debug_report(uncore: *mut IntelUncore, reg: I915RegT, read: bool) {
    if unsafe { check_for_unclaimed_mmio(uncore) } {
        drm_warn!(
            unsafe { uncore_dev(uncore) },
            "Unclaimed %s register 0x%x\n",
            if read { "read from" } else { "write to" },
            reg.reg
        );
        // Only report the first N failures.
        unsafe { set_mmio_debug_param(uncore, mmio_debug_params(uncore) - 1) };
    }
}

/// `__unclaimed_previous_reg_debug()`.
unsafe fn unclaimed_previous_reg_debug(uncore: *mut IntelUncore, reg: I915RegT, read: bool) {
    if unsafe { check_for_unclaimed_mmio(uncore) } {
        drm_dbg!(
            unsafe { uncore_dev(uncore) },
            "Unclaimed access detected before %s register 0x%x\n",
            if read { "read from" } else { "write to" },
            reg.reg
        );
    }
}

/// `unclaimed_reg_debug_header()`: returns whether the footer must run.
unsafe fn unclaimed_reg_debug_header(uncore: *mut IntelUncore, reg: I915RegT, read: bool) -> bool {
    if unsafe { mmio_debug_params(uncore) } == 0 || unsafe { uncore_debug(uncore) }.is_null() {
        return false;
    }

    // Interrupts are disabled and re-enabled around uncore->lock usage.
    unsafe { spin_lock(&mut (*uncore_debug(uncore)).lock) };

    unsafe { unclaimed_previous_reg_debug(uncore, reg, read) };

    true
}

/// `unclaimed_reg_debug_footer()`.
unsafe fn unclaimed_reg_debug_footer(uncore: *mut IntelUncore, reg: I915RegT, read: bool) {
    unsafe { unclaimed_reg_debug_report(uncore, reg, read) };
    unsafe { spin_unlock(&mut (*uncore_debug(uncore)).lock) };
}

/// `intel_uncore_mmio_debug_init_early()`.
// upstream: intel_uncore.c intel_uncore_mmio_debug_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_mmio_debug_init_early(i915: *mut DrmI915Private) {
    assert!(!i915.is_null());
    let debug = unsafe { i915.cast::<u8>().add(I915_MMIO_DEBUG_OFFSET).cast::<IntelUncoreMmioDebug>() };
    unsafe {
        spin_lock_init_raw(ptr::addr_of_mut!((*debug).lock));
        (*debug).unclaimed_mmio_check = 1;
        // i915->uncore.debug = &i915->mmio_debug;
        let uncore = i915.cast::<u8>().add(I915_UNCORE_OFFSET).cast::<IntelUncore>();
        (*uncore).debug = debug;
    }
}

#[inline]
unsafe fn spin_lock_init_raw(lock: *mut crate::intel_engine_cs_upstream::Spinlock) {
    unsafe { crate::linux::locks::spin_lock_init(&mut *lock) };
}

// ---------------------------------------------------------------------------
// Forcewake early sanitize, suspend and resume.
// ---------------------------------------------------------------------------

unsafe fn forcewake_early_sanitize(uncore: *mut IntelUncore, restore_forcewake: ForcewakeDomains) {
    GEM_BUG_ON!(!unsafe { intel_uncore_has_forcewake(&*uncore) });

    // WaDisableShadowRegForCpd:chv
    if unsafe { IS_CHERRYVIEW(i915_const(uncore_i915(uncore))) } {
        let v = unsafe { __raw_uncore_read32(uncore, GTFIFOCTL) }
            | GT_FIFO_CTL_BLOCK_ALL_POLICY_STALL
            | GT_FIFO_CTL_RC6_POLICY_STALL;
        unsafe { __raw_uncore_write32(uncore, GTFIFOCTL, v) };
    }

    if unsafe { intel_uncore_has_fifo(&*uncore) } {
        unsafe { gen6_check_for_fifo_debug(uncore) };
    }

    unsafe { iosf_mbi_punit_acquire() };
    unsafe { intel_uncore_forcewake_reset(uncore) };
    if restore_forcewake != 0 {
        let mut flags = 0;
        unsafe { spin_lock_irq(&mut (*uncore).lock) };
        unsafe { fw_domains_get(uncore, restore_forcewake) };

        if unsafe { intel_uncore_has_fifo(&*uncore) } {
            unsafe { (*uncore).fifo_count = fifo_free_entries(uncore) };
        }
        unsafe { spin_unlock_irq(&mut (*uncore).lock) };
        let _ = &mut flags;
    }
    unsafe { iosf_mbi_punit_release() };
}

/// `intel_uncore_suspend()`.
// upstream: intel_uncore.c intel_uncore_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_suspend(uncore: *mut IntelUncore) {
    if !unsafe { intel_uncore_has_forcewake(&*uncore) } {
        return;
    }

    unsafe { iosf_mbi_punit_acquire() };
    unsafe {
        iosf_mbi_unregister_pmic_bus_access_notifier_unlocked(ptr::addr_of_mut!(
            (*uncore).pmic_bus_access_nb
        ))
    };
    let saved = unsafe { intel_uncore_forcewake_reset(uncore) };
    unsafe { (*uncore).fw_domains_saved = saved };
    unsafe { iosf_mbi_punit_release() };
}

/// `intel_uncore_resume_early()`.
// upstream: intel_uncore.c intel_uncore_resume_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_resume_early(uncore: *mut IntelUncore) {
    if unsafe { intel_uncore_unclaimed_mmio(uncore) } {
        drm_dbg!(
            unsafe { uncore_dev(uncore) },
            "unclaimed mmio detected on resume, clearing\n"
        );
    }

    if !unsafe { intel_uncore_has_forcewake(&*uncore) } {
        return;
    }

    let restore_forcewake = unsafe { fetch_and_zero_domains(&mut (*uncore).fw_domains_saved) };
    unsafe { forcewake_early_sanitize(uncore, restore_forcewake) };

    unsafe {
        iosf_mbi_register_pmic_bus_access_notifier(ptr::addr_of_mut!((*uncore).pmic_bus_access_nb))
    };
}

#[inline]
unsafe fn fetch_and_zero_domains(place: *mut ForcewakeDomains) -> ForcewakeDomains {
    let value = unsafe { *place };
    unsafe { *place = 0 };
    value
}

/// `intel_uncore_runtime_resume()`.
// upstream: intel_uncore.c intel_uncore_runtime_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_runtime_resume(uncore: *mut IntelUncore) {
    if !unsafe { intel_uncore_has_forcewake(&*uncore) } {
        return;
    }

    unsafe {
        iosf_mbi_register_pmic_bus_access_notifier(ptr::addr_of_mut!((*uncore).pmic_bus_access_nb))
    };
}

// ---------------------------------------------------------------------------
// Public forcewake reference API.
// ---------------------------------------------------------------------------

/// `__intel_uncore_forcewake_get()`. Caller holds `uncore->lock`.
unsafe fn __intel_uncore_forcewake_get(uncore: *mut IntelUncore, mut fw_domains: ForcewakeDomains) {
    fw_domains &= unsafe { (*uncore).fw_domains };

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        let count = unsafe { (*d).wake_count };
        unsafe { (*d).wake_count = count.wrapping_add(1) };
        if count != 0 {
            fw_domains &= !unsafe { (*d).mask };
            unsafe { (*d).active = true };
        }
    }

    if fw_domains != 0 {
        unsafe { fw_domains_get(uncore, fw_domains) };
    }
}

/// `fw_domains_get()`: dispatch through the chip-specific acquire callback.
unsafe fn fw_domains_get(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    let funcs = unsafe { (*uncore).fw_get_funcs };
    let force_wake_get = unsafe { (*funcs).force_wake_get }.expect("force_wake_get is unset");
    unsafe { force_wake_get(uncore, fw_domains) };
}

/// `intel_uncore_forcewake_get()`.
// upstream: intel_uncore.c intel_uncore_forcewake_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_get(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    unsafe { assert_rpm_wakelock_held((*uncore).rpm) };

    let mut irqflags = 0;
    unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };
    unsafe { __intel_uncore_forcewake_get(uncore, fw_domains) };
    unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };
}

/// `intel_uncore_forcewake_user_get()`.
// upstream: intel_uncore.c intel_uncore_forcewake_user_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_user_get(uncore: *mut IntelUncore) {
    unsafe { spin_lock_irq(&mut (*uncore).lock) };
    let count = unsafe { (*uncore).user_forcewake_count };
    unsafe { (*uncore).user_forcewake_count = count.wrapping_add(1) };
    if count == 0 {
        unsafe { intel_uncore_forcewake_get__locked(uncore, FORCEWAKE_ALL) };
        unsafe { mmio_debug_suspend(uncore) };
    }
    unsafe { spin_unlock_irq(&mut (*uncore).lock) };
}

/// `intel_uncore_forcewake_user_put()`.
// upstream: intel_uncore.c intel_uncore_forcewake_user_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_user_put(uncore: *mut IntelUncore) {
    unsafe { spin_lock_irq(&mut (*uncore).lock) };
    unsafe { (*uncore).user_forcewake_count = (*uncore).user_forcewake_count.wrapping_sub(1) };
    if unsafe { (*uncore).user_forcewake_count } == 0 {
        unsafe { mmio_debug_resume(uncore) };
        unsafe { intel_uncore_forcewake_put__locked(uncore, FORCEWAKE_ALL) };
    }
    unsafe { spin_unlock_irq(&mut (*uncore).lock) };
}

/// `intel_uncore_forcewake_get__locked()`.
// upstream: intel_uncore.c intel_uncore_forcewake_get__locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_get__locked(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    unsafe { __intel_uncore_forcewake_get(uncore, fw_domains) };
}

/// `__intel_uncore_forcewake_put()`.
unsafe fn __intel_uncore_forcewake_put(
    uncore: *mut IntelUncore,
    mut fw_domains: ForcewakeDomains,
    delayed: bool,
) {
    fw_domains &= unsafe { (*uncore).fw_domains };

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        GEM_BUG_ON!(unsafe { (*d).wake_count } == 0);

        unsafe { (*d).wake_count -= 1 };
        if unsafe { (*d).wake_count } != 0 {
            unsafe { (*d).active = true };
            continue;
        }

        if delayed && unsafe { (*uncore).fw_domains_timer } & unsafe { (*d).mask } == 0 {
            unsafe { fw_domain_arm_timer(d) };
        } else {
            unsafe { fw_domains_put(uncore, (*d).mask) };
        }
    }
}

/// `intel_uncore_forcewake_put()`.
// upstream: intel_uncore.c intel_uncore_forcewake_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_put(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    let mut irqflags = 0;
    unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };
    unsafe { __intel_uncore_forcewake_put(uncore, fw_domains, false) };
    unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };
}

/// `intel_uncore_forcewake_put_delayed()`.
// upstream: intel_uncore.c intel_uncore_forcewake_put_delayed()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_put_delayed(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    let mut irqflags = 0;
    unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };
    unsafe { __intel_uncore_forcewake_put(uncore, fw_domains, true) };
    unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };
}

/// `intel_uncore_forcewake_flush()`: run any pending delayed releases now.
// upstream: intel_uncore.c intel_uncore_forcewake_flush()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_flush(uncore: *mut IntelUncore, mut fw_domains: ForcewakeDomains) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    fw_domains &= unsafe { (*uncore).fw_domains };
    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { ptr::write_volatile(ptr::addr_of_mut!((*d).active), false) };
        if unsafe { hrtimer_cancel(ptr::addr_of_mut!((*d).timer)) } != 0 {
            unsafe { intel_uncore_fw_release_timer(ptr::addr_of_mut!((*d).timer)) };
        }
    }
}

/// `intel_uncore_forcewake_put__locked()`.
// upstream: intel_uncore.c intel_uncore_forcewake_put__locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_put__locked(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    unsafe { __intel_uncore_forcewake_put(uncore, fw_domains, false) };
}

/// `assert_forcewakes_inactive()`.
// upstream: intel_uncore.c assert_forcewakes_inactive()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn assert_forcewakes_inactive(uncore: *mut IntelUncore) {
    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    let active = unsafe { (*uncore).fw_domains_active };
    if active != 0 {
        drm_warn_msg("Expected all fw_domains to be inactive, but some are still on");
    }
}

/// `assert_forcewakes_active()`. Only does work with CONFIG_DRM_I915_DEBUG_RUNTIME_PM.
// upstream: intel_uncore.c assert_forcewakes_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn assert_forcewakes_active(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    if !CONFIG_DRM_I915_DEBUG_RUNTIME_PM {
        return;
    }

    if unsafe { (*uncore).fw_get_funcs.is_null() } {
        return;
    }

    let mut irqflags = 0;
    unsafe { spin_lock_irq(&mut (*uncore).lock) };

    unsafe { assert_rpm_wakelock_held((*uncore).rpm) };

    let fw_domains = fw_domains & unsafe { (*uncore).fw_domains };
    if fw_domains & !unsafe { (*uncore).fw_domains_active } != 0 {
        drm_warn_msg("Expected fw_domains to be active, but some are off");
    }

    // Check that the caller has an explicit wakeref and we don't mistake it for
    // the auto wakeref.
    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        let actual = unsafe { ptr::read_volatile(ptr::addr_of!((*d).wake_count)) };
        let mut expect = 1;

        if unsafe { (*uncore).fw_domains_timer } & unsafe { (*d).mask } != 0 {
            expect += 1; // pending automatic release
        }

        if actual < expect {
            drm_warn_msg("Expected domain to be held awake by caller");
            break;
        }
    }

    unsafe { spin_unlock_irq(&mut (*uncore).lock) };
    let _ = &mut irqflags;
}

// ---------------------------------------------------------------------------
// Forcewake table lookup and the MMIO accessor families.
// ---------------------------------------------------------------------------

#[inline]
const fn needs_force_wake(reg: u32) -> bool {
    // Fast paths for the really cool registers. The second range includes media
    // domains (and the GSC starting from Xe_LPM+).
    reg < 0x40000 || reg >= 0x116000
}

#[inline]
fn fw_range_cmp(offset: u32, entry: &IntelForcewakeRange) -> i32 {
    if offset < entry.start {
        -1
    } else if offset > entry.end {
        1
    } else {
        0
    }
}

#[inline]
fn mmio_range_cmp(key: u32, range: &IntelMmioRange) -> i32 {
    if key < range.start {
        -1
    } else if key > range.end {
        1
    } else {
        0
    }
}

/// `BSEARCH()`: binary search over a sorted table; returns the matching entry.
unsafe fn bsearch<'a, T>(
    key: u32,
    base: *const T,
    num: u32,
    cmp: fn(u32, &T) -> i32,
) -> Option<&'a T> {
    let mut start: u32 = 0;
    let mut end: u32 = num;
    while start < end {
        let mid = start + (end - start) / 2;
        let entry = unsafe { &*base.add(mid as usize) };
        let ret = cmp(key, entry);
        if ret < 0 {
            end = mid;
        } else if ret > 0 {
            start = mid + 1;
        } else {
            return Some(entry);
        }
    }
    None
}

#[inline]
unsafe fn gsi_adjust(uncore: *const IntelUncore, offset: u32) -> u32 {
    if crate::intel_uncore_types_upstream::IS_GSI_REG(offset) {
        offset.wrapping_add(unsafe { (*uncore).gsi_offset })
    } else {
        offset
    }
}

/// `find_fw_domain()`.
unsafe fn find_fw_domain(uncore: *mut IntelUncore, offset: u32) -> ForcewakeDomains {
    let offset = unsafe { gsi_adjust(uncore, offset) };

    let entry = unsafe {
        bsearch(
            offset,
            (*uncore).fw_domains_table,
            (*uncore).fw_domains_table_entries,
            fw_range_cmp,
        )
    };
    let Some(entry) = entry else {
        return 0;
    };

    // The domain list depends on the SKU in gen11+, so FORCEWAKE_ALL is
    // translated here to the list of available domains.
    if entry.domains == FORCEWAKE_ALL {
        return unsafe { (*uncore).fw_domains };
    }

    if entry.domains & !unsafe { (*uncore).fw_domains } != 0 {
        drm_warn_msg("Uninitialized forcewake domain(s) accessed");
    }

    entry.domains
}

/// `is_shadowed()`.
unsafe fn is_shadowed(uncore: *mut IntelUncore, offset: u32) -> bool {
    if unsafe { (*uncore).shadowed_reg_table.is_null() } {
        drm_warn_msg("shadowed register table is not initialized");
        return false;
    }

    let offset = unsafe { gsi_adjust(uncore, offset) };

    unsafe {
        bsearch(
            offset,
            (*uncore).shadowed_reg_table,
            (*uncore).shadowed_reg_table_entries,
            mmio_range_cmp,
        )
    }
    .is_some()
}

/// `gen6_reg_write_fw_domains()`.
unsafe extern "C" fn gen6_reg_write_fw_domains(_uncore: *mut IntelUncore, _reg: I915RegT) -> ForcewakeDomains {
    FORCEWAKE_RENDER
}

#[inline]
unsafe fn fwtable_reg_read_fw_domains_offset(uncore: *mut IntelUncore, offset: u32) -> ForcewakeDomains {
    if needs_force_wake(offset) {
        unsafe { find_fw_domain(uncore, offset) }
    } else {
        0
    }
}

#[inline]
unsafe fn fwtable_reg_write_fw_domains_offset(uncore: *mut IntelUncore, offset: u32) -> ForcewakeDomains {
    if needs_force_wake(offset) && !unsafe { is_shadowed(uncore, offset) } {
        unsafe { find_fw_domain(uncore, offset) }
    } else {
        0
    }
}

/// `fwtable_reg_read_fw_domains()`.
unsafe extern "C" fn fwtable_reg_read_fw_domains(uncore: *mut IntelUncore, reg: I915RegT) -> ForcewakeDomains {
    unsafe { fwtable_reg_read_fw_domains_offset(uncore, reg.reg) }
}

/// `fwtable_reg_write_fw_domains()`.
unsafe extern "C" fn fwtable_reg_write_fw_domains(uncore: *mut IntelUncore, reg: I915RegT) -> ForcewakeDomains {
    unsafe { fwtable_reg_write_fw_domains_offset(uncore, reg.reg) }
}

/// `ilk_dummy_write()`: WaIssueDummyWriteToWakeupFromRC6:ilk. MI_MODE is masked,
/// so writing 0 is harmless.
unsafe fn ilk_dummy_write(uncore: *mut IntelUncore) {
    unsafe {
        __raw_uncore_write32(
            uncore,
            crate::intel_engine_regs_upstream::RING_MI_MODE(RENDER_RING_BASE),
            0,
        )
    };
}

// The read and write families below mirror the C macro expansions. The
// `trace_i915_reg_rw()` calls are omitted: the tracepoint is only compiled in
// with CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS, which this build leaves off, and
// the generic tracepoint is disabled.

macro_rules! def_raw_read {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c vgpu_read / gen2_read family
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> $ty {
            unsafe { $raw(uncore, reg) }
        }
    };
}

macro_rules! def_gen5_read {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c gen5_read family (WaIssueDummyWrite first)
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> $ty {
            GEM_BUG_ON!(!unsafe { rpm_wakelock_held_check(uncore) });
            unsafe { ilk_dummy_write(uncore) };
            unsafe { $raw(uncore, reg) }
        }
    };
}

macro_rules! def_gen2_read {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c gen2_read family
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> $ty {
            GEM_BUG_ON!(!unsafe { rpm_wakelock_held_check(uncore) });
            unsafe { $raw(uncore, reg) }
        }
    };
}

macro_rules! def_fwtable_read {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c fwtable_read family (GEN6_READ_HEADER/FOOTER)
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> $ty {
            let offset = reg.reg;
            let mut irqflags = 0;
            unsafe { rpm_wakelock_assert(uncore) };
            unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };
            let unclaimed_reg_debug = unsafe { unclaimed_reg_debug_header(uncore, reg, true) };

            let fw_engine = unsafe { fwtable_reg_read_fw_domains_offset(uncore, offset) };
            if fw_engine != 0 {
                unsafe { force_wake_auto(uncore, fw_engine) };
            }
            let val = unsafe { $raw(uncore, reg) };

            if unclaimed_reg_debug {
                unsafe { unclaimed_reg_debug_footer(uncore, reg, true) };
            }
            unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };
            val
        }
    };
}

macro_rules! def_raw_write {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c vgpu_write family
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, val: $ty, _trace: bool) {
            unsafe { $raw(uncore, reg, val) };
        }
    };
}

macro_rules! def_gen5_write {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c gen5_write family
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, val: $ty, _trace: bool) {
            GEM_BUG_ON!(!unsafe { rpm_wakelock_held_check(uncore) });
            unsafe { ilk_dummy_write(uncore) };
            unsafe { $raw(uncore, reg, val) };
        }
    };
}

macro_rules! def_gen2_write {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c gen2_write family
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, val: $ty, _trace: bool) {
            GEM_BUG_ON!(!unsafe { rpm_wakelock_held_check(uncore) });
            unsafe { $raw(uncore, reg, val) };
        }
    };
}

macro_rules! def_gen6_write {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c gen6_write family (GEN6_WRITE_HEADER/FOOTER)
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, val: $ty, _trace: bool) {
            let offset = reg.reg;
            let mut irqflags = 0;
            unsafe { rpm_wakelock_assert(uncore) };
            unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };
            let unclaimed_reg_debug = unsafe { unclaimed_reg_debug_header(uncore, reg, false) };

            if needs_force_wake(offset) {
                unsafe { gen6_gt_wait_for_fifo_locked(uncore) };
            }
            unsafe { $raw(uncore, reg, val) };

            if unclaimed_reg_debug {
                unsafe { unclaimed_reg_debug_footer(uncore, reg, false) };
            }
            unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };
        }
    };
}

macro_rules! def_fwtable_write {
    ($name:ident, $ty:ty, $raw:ident) => {
        // upstream: intel_uncore.c fwtable_write family (GEN6_WRITE_HEADER/FOOTER)
        unsafe extern "C" fn $name(uncore: *mut IntelUncore, reg: I915RegT, val: $ty, _trace: bool) {
            let offset = reg.reg;
            let mut irqflags = 0;
            unsafe { rpm_wakelock_assert(uncore) };
            unsafe { spin_lock_irqsave(&mut (*uncore).lock, &mut irqflags) };
            let unclaimed_reg_debug = unsafe { unclaimed_reg_debug_header(uncore, reg, false) };

            let fw_engine = unsafe { fwtable_reg_write_fw_domains_offset(uncore, offset) };
            if fw_engine != 0 {
                unsafe { force_wake_auto(uncore, fw_engine) };
            }
            unsafe { $raw(uncore, reg, val) };

            if unclaimed_reg_debug {
                unsafe { unclaimed_reg_debug_footer(uncore, reg, false) };
            }
            unsafe { spin_unlock_irqrestore(&mut (*uncore).lock, irqflags) };
        }
    };
}

/// `assert_rpm_wakelock_held(uncore->rpm)`.
#[inline]
unsafe fn rpm_wakelock_assert(uncore: *mut IntelUncore) {
    unsafe { assert_rpm_wakelock_held((*uncore).rpm) };
}

#[inline]
unsafe fn rpm_wakelock_held_check(uncore: *mut IntelUncore) -> bool {
    unsafe { rpm_wakelock_assert(uncore) };
    true
}

/// `___force_wake_auto()`.
unsafe fn force_wake_auto_inner(uncore: *mut IntelUncore, fw_domains: ForcewakeDomains) {
    GEM_BUG_ON!(fw_domains & !unsafe { (*uncore).fw_domains } != 0);

    for d in for_each_fw_domain_masked(uncore, fw_domains) {
        unsafe { fw_domain_arm_timer(d) };
    }
    unsafe { fw_domains_get(uncore, fw_domains) };
}

/// `__force_wake_auto()`: turn on all requested but inactive supported domains.
#[inline]
unsafe fn force_wake_auto(uncore: *mut IntelUncore, mut fw_domains: ForcewakeDomains) {
    GEM_BUG_ON!(fw_domains == 0);

    // Turn on all requested but inactive supported forcewake domains.
    fw_domains &= unsafe { (*uncore).fw_domains };
    fw_domains &= !unsafe { (*uncore).fw_domains_active };
    if fw_domains != 0 {
        unsafe { force_wake_auto_inner(uncore, fw_domains) };
    }
}

// Read families (u8/u16/u32/u64 as the C macros generate them).
def_raw_read!(vgpu_read8, u8, __raw_uncore_read8);
def_raw_read!(vgpu_read16, u16, __raw_uncore_read16);
def_raw_read!(vgpu_read32, u32, __raw_uncore_read32);
def_raw_read!(vgpu_read64, u64, __raw_uncore_read64);

def_gen5_read!(gen5_read8, u8, __raw_uncore_read8);
def_gen5_read!(gen5_read16, u16, __raw_uncore_read16);
def_gen5_read!(gen5_read32, u32, __raw_uncore_read32);
def_gen5_read!(gen5_read64, u64, __raw_uncore_read64);

def_gen2_read!(gen2_read8, u8, __raw_uncore_read8);
def_gen2_read!(gen2_read16, u16, __raw_uncore_read16);
def_gen2_read!(gen2_read32, u32, __raw_uncore_read32);
def_gen2_read!(gen2_read64, u64, __raw_uncore_read64);

def_fwtable_read!(fwtable_read8, u8, __raw_uncore_read8);
def_fwtable_read!(fwtable_read16, u16, __raw_uncore_read16);
def_fwtable_read!(fwtable_read32, u32, __raw_uncore_read32);
def_fwtable_read!(fwtable_read64, u64, __raw_uncore_read64);

// Write families.
def_gen2_write!(gen2_write8, u8, __raw_uncore_write8);
def_gen2_write!(gen2_write16, u16, __raw_uncore_write16);
def_gen2_write!(gen2_write32, u32, __raw_uncore_write32);

def_gen5_write!(gen5_write8, u8, __raw_uncore_write8);
def_gen5_write!(gen5_write16, u16, __raw_uncore_write16);
def_gen5_write!(gen5_write32, u32, __raw_uncore_write32);

def_gen6_write!(gen6_write8, u8, __raw_uncore_write8);
def_gen6_write!(gen6_write16, u16, __raw_uncore_write16);
def_gen6_write!(gen6_write32, u32, __raw_uncore_write32);

def_fwtable_write!(fwtable_write8, u8, __raw_uncore_write8);
def_fwtable_write!(fwtable_write16, u16, __raw_uncore_write16);
def_fwtable_write!(fwtable_write32, u32, __raw_uncore_write32);

def_raw_write!(vgpu_write8, u8, __raw_uncore_write8);
def_raw_write!(vgpu_write16, u16, __raw_uncore_write16);
def_raw_write!(vgpu_write32, u32, __raw_uncore_write32);

/// Function-pointer tuples for the `uncore->funcs` accessor families.
mod vfuncs {
    use super::*;

    pub type Readers = (
        unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u8,
        unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u16,
        unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u32,
        unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u64,
    );
    pub type Writers = (
        unsafe extern "C" fn(*mut IntelUncore, I915RegT, u8, bool),
        unsafe extern "C" fn(*mut IntelUncore, I915RegT, u16, bool),
        unsafe extern "C" fn(*mut IntelUncore, I915RegT, u32, bool),
    );

    pub const VGPU: Readers = (vgpu_read8, vgpu_read16, vgpu_read32, vgpu_read64);
    pub const GEN5: Readers = (gen5_read8, gen5_read16, gen5_read32, gen5_read64);
    pub const GEN2: Readers = (gen2_read8, gen2_read16, gen2_read32, gen2_read64);
    pub const FWTABLE_READ: Readers = (fwtable_read8, fwtable_read16, fwtable_read32, fwtable_read64);

    pub const VGPU_WRITE: Writers = (vgpu_write8, vgpu_write16, vgpu_write32);
    pub const GEN5_WRITE: Writers = (gen5_write8, gen5_write16, gen5_write32);
    pub const GEN2_WRITE: Writers = (gen2_write8, gen2_write16, gen2_write32);
    pub const GEN6_WRITE: Writers = (gen6_write8, gen6_write16, gen6_write32);
    pub const FWTABLE_WRITE: Writers = (fwtable_write8, fwtable_write16, fwtable_write32);
}

// ---------------------------------------------------------------------------
// Forcewake domain setup and teardown.
// ---------------------------------------------------------------------------

/// `__fw_domain_init()`.
// upstream: intel_uncore.c __fw_domain_init()
unsafe fn fw_domain_init_inner(
    uncore: *mut IntelUncore,
    domain_id: ForcewakeDomainId,
    reg_set: I915RegT,
    reg_ack: I915RegT,
) -> i32 {
    GEM_BUG_ON!(!(0..FW_DOMAIN_ID_COUNT).contains(&domain_id));
    GEM_BUG_ON!(!unsafe { (*uncore).fw_domain[domain_id as usize] }.is_null());

    let d: *mut IntelUncoreForcewakeDomain = crate::linux::memory::kzalloc_obj::<
        IntelUncoreForcewakeDomain,
    >();
    if d.is_null() {
        return -ENOMEM;
    }

    if reg_set.reg == 0 {
        drm_warn_msg("forcewake set register is not valid");
    }
    if reg_ack.reg == 0 {
        drm_warn_msg("forcewake ack register is not valid");
    }

    unsafe {
        (*d).uncore = uncore;
        (*d).wake_count = 0;
        let gsi = (*uncore).gsi_offset as usize;
        let regs = (*uncore).regs.cast::<u8>();
        (*d).reg_set = regs.add(reg_set.reg as usize + gsi).cast::<u32>();
        (*d).reg_ack = regs.add(reg_ack.reg as usize + gsi).cast::<u32>();
        (*d).id = domain_id;
        (*d).mask = 1_i32 << domain_id;
        hrtimer_setup(
            ptr::addr_of_mut!((*d).timer),
            Some(intel_uncore_fw_release_timer),
            crate::linux::timer::CLOCK_MONOTONIC,
            crate::linux::timer::HRTIMER_MODE_REL,
        );
        (*uncore).fw_domains |= 1_i32 << domain_id;
        fw_domain_reset(d);
        (*uncore).fw_domain[domain_id as usize] = d;
    }

    0
}

/// `fw_domain_fini()`.
// upstream: intel_uncore.c fw_domain_fini()
unsafe fn fw_domain_fini(uncore: *mut IntelUncore, domain_id: ForcewakeDomainId) {
    GEM_BUG_ON!(!(0..FW_DOMAIN_ID_COUNT).contains(&domain_id));

    // fetch_and_zero(&uncore->fw_domain[domain_id])
    let slot = unsafe { ptr::addr_of_mut!((*uncore).fw_domain[domain_id as usize]) };
    let d = unsafe { ptr::replace(slot, ptr::null_mut()) };
    if d.is_null() {
        return;
    }

    unsafe { (*uncore).fw_domains &= !(1_i32 << domain_id) };
    if unsafe { (*d).wake_count } != 0 {
        drm_warn_msg("forcewake domain finalized while still referenced");
    }
    if unsafe { hrtimer_cancel(ptr::addr_of_mut!((*d).timer)) } != 0 {
        drm_warn_msg("forcewake release timer still pending at fini");
    }
    unsafe { kfree(d) };
}

/// `intel_uncore_fw_domains_fini()`.
// upstream: intel_uncore.c intel_uncore_fw_domains_fini()
unsafe fn intel_uncore_fw_domains_fini(uncore: *mut IntelUncore) {
    for d in for_each_fw_domain(uncore) {
        let id = unsafe { (*d).id };
        unsafe { fw_domain_fini(uncore, id) };
    }
}

// upstream: intel_uncore.c uncore_get_fallback / uncore_get_normal / ...
static UNCORE_GET_FALLBACK: IntelUncoreFwGet = IntelUncoreFwGet {
    force_wake_get: Some(fw_domains_get_with_fallback),
};
static UNCORE_GET_NORMAL: IntelUncoreFwGet = IntelUncoreFwGet {
    force_wake_get: Some(fw_domains_get_normal),
};
static UNCORE_GET_THREAD_STATUS: IntelUncoreFwGet = IntelUncoreFwGet {
    force_wake_get: Some(fw_domains_get_with_thread_status),
};
static UNCORE_GET_NORMAL_FIFO: IntelUncoreFwGet = IntelUncoreFwGet {
    force_wake_get: Some(fw_domains_get_normal_fifo),
};
static UNCORE_GET_THREAD_STATUS_FIFO: IntelUncoreFwGet = IntelUncoreFwGet {
    force_wake_get: Some(fw_domains_get_with_thread_status_fifo),
};

/// `intel_uncore_fw_domains_init()`.
// upstream: intel_uncore.c intel_uncore_fw_domains_init()
unsafe fn intel_uncore_fw_domains_init(uncore: *mut IntelUncore) -> i32 {
    let i915 = unsafe { (*uncore).i915 };
    let mut ret: i32 = 0;

    GEM_BUG_ON!(!unsafe { intel_uncore_has_forcewake(&*uncore) });

    // fw_domain_init(uncore, id, set, ack): `(ret ?: (ret = __fw_domain_init(...)))`
    macro_rules! fw_domain_init {
        ($id:expr, $set:expr, $ack:expr) => {
            if ret == 0 {
                ret = unsafe { fw_domain_init_inner(uncore, $id, $set, $ack) };
            }
        };
    }

    'out: {
        if unsafe { i915_graphics_ver(i915) } >= 11 {
            // We'll prune the domains of missing engines later.
            let emask = unsafe { (*(*uncore).gt).info.engine_mask };
            unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_FALLBACK };

            if unsafe { i915_graphics_ver_full(i915) } >= crate::linux::i915::IP_VER(12, 70) {
                fw_domain_init!(
                    FW_DOMAIN_ID_GT,
                    FORCEWAKE_GT_GEN9,
                    FORCEWAKE_ACK_GT_MTL
                );
            } else {
                fw_domain_init!(
                    FW_DOMAIN_ID_GT,
                    FORCEWAKE_GT_GEN9,
                    FORCEWAKE_ACK_GT_GEN9
                );
            }

            let gt = unsafe { (*uncore).gt };
            if unsafe { RCS_MASK(gt) } != 0 || unsafe { CCS_MASK(gt) } != 0 {
                fw_domain_init!(
                    FW_DOMAIN_ID_RENDER,
                    FORCEWAKE_RENDER_GEN9,
                    FORCEWAKE_ACK_RENDER_GEN9
                );
            }

            for i in 0..crate::linux_config::I915_MAX_VCS as u32 {
                if emask & (1u32 << (crate::intel_engine_types_upstream::VCS0 as u32 + i)) == 0 {
                    continue;
                }
                fw_domain_init!(
                    FW_DOMAIN_ID_MEDIA_VDBOX0 + i as i32,
                    forcewake_media_vdbox_gen11(i),
                    forcewake_ack_media_vdbox_gen11(i)
                );
            }

            for i in 0..crate::linux_config::I915_MAX_VECS as u32 {
                if emask & (1u32 << (crate::intel_engine_types_upstream::VECS0 as u32 + i)) == 0 {
                    continue;
                }
                fw_domain_init!(
                    FW_DOMAIN_ID_MEDIA_VEBOX0 + i as i32,
                    forcewake_media_vebox_gen11(i),
                    forcewake_ack_media_vebox_gen11(i)
                );
            }

            if unsafe { (*gt).type_ } == GT_MEDIA {
                fw_domain_init!(FW_DOMAIN_ID_GSC, FORCEWAKE_REQ_GSC, FORCEWAKE_ACK_GSC);
            }
        } else if unsafe { IS_GRAPHICS_VER(i915 as *const DrmI915Private, 9, 10) } {
            unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_FALLBACK };
            fw_domain_init!(
                FW_DOMAIN_ID_RENDER,
                FORCEWAKE_RENDER_GEN9,
                FORCEWAKE_ACK_RENDER_GEN9
            );
            fw_domain_init!(FW_DOMAIN_ID_GT, FORCEWAKE_GT_GEN9, FORCEWAKE_ACK_GT_GEN9);
            fw_domain_init!(
                FW_DOMAIN_ID_MEDIA,
                FORCEWAKE_MEDIA_GEN9,
                FORCEWAKE_ACK_MEDIA_GEN9
            );
        } else if unsafe { IS_VALLEYVIEW(i915 as *const DrmI915Private) }
            || unsafe { IS_CHERRYVIEW(i915 as *const DrmI915Private) }
        {
            if unsafe { intel_uncore_has_fifo(&*uncore) } {
                unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_NORMAL_FIFO };
            } else {
                unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_NORMAL };
            }
            fw_domain_init!(FW_DOMAIN_ID_RENDER, FORCEWAKE_VLV, FORCEWAKE_ACK_VLV);
            fw_domain_init!(
                FW_DOMAIN_ID_MEDIA,
                FORCEWAKE_MEDIA_VLV,
                FORCEWAKE_ACK_MEDIA_VLV
            );
        } else if unsafe { IS_HASWELL(i915 as *const DrmI915Private) }
            || unsafe { IS_BROADWELL(i915 as *const DrmI915Private) }
        {
            if unsafe { intel_uncore_has_fifo(&*uncore) } {
                unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_THREAD_STATUS_FIFO };
            } else {
                unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_THREAD_STATUS };
            }
            fw_domain_init!(FW_DOMAIN_ID_RENDER, FORCEWAKE_MT, FORCEWAKE_ACK_HSW);
        } else if unsafe { IS_IVYBRIDGE(i915 as *const DrmI915Private) } {
            // IVB configs may use multi-threaded forcewake. If the BIOS has not
            // configured MT forcewake and the device is in RC6, ECOBUS reads
            // back zero, which the test below interprets as MT being disabled.
            unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_THREAD_STATUS_FIFO };

            // Init first for ECOBUS access; reset the gen6 fw registers first.
            unsafe { __raw_uncore_write32(uncore, FORCEWAKE, 0) };
            let _ = unsafe { __raw_uncore_read32(uncore, ECOBUS) };

            ret = unsafe {
                fw_domain_init_inner(
                    uncore,
                    FW_DOMAIN_ID_RENDER,
                    FORCEWAKE_MT,
                    FORCEWAKE_MT_ACK,
                )
            };
            if ret != 0 {
                break 'out;
            }

            unsafe { spin_lock_irq(&mut (*uncore).lock) };
            unsafe { fw_domains_get_with_thread_status(uncore, FORCEWAKE_RENDER) };
            let ecobus = unsafe { __raw_uncore_read32(uncore, ECOBUS) };
            unsafe { fw_domains_put(uncore, FORCEWAKE_RENDER) };
            unsafe { spin_unlock_irq(&mut (*uncore).lock) };

            if ecobus & FORCEWAKE_MT_ENABLE == 0 {
                drm_notice!(
                    unsafe { ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>() },
                    "No MT forcewake available on Ivybridge, this can result in issues\n"
                );
                drm_notice!(
                    unsafe { ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>() },
                    "when using vblank-synced partial screen updates.\n"
                );
                unsafe { fw_domain_fini(uncore, FW_DOMAIN_ID_RENDER) };
                fw_domain_init!(FW_DOMAIN_ID_RENDER, FORCEWAKE, FORCEWAKE_ACK);
            }
        } else if unsafe { i915_graphics_ver(i915) } == 6 {
            unsafe { (*uncore).fw_get_funcs = &UNCORE_GET_THREAD_STATUS_FIFO };
            fw_domain_init!(FW_DOMAIN_ID_RENDER, FORCEWAKE, FORCEWAKE_ACK);
        }

        // All future platforms are expected to require complex power gating.
        if ret == 0 && unsafe { (*uncore).fw_domains } == 0 {
            drm_warn_msg("no forcewake domains on a forcewake platform");
        }
    }

    if ret != 0 {
        unsafe { intel_uncore_fw_domains_fini(uncore) };
    }
    ret
}

// The ranges and shadow tables generated from intel_uncore.c follow.

// ---------------------------------------------------------------------------
// Kernel-owned services: IOSF MBI (CONFIG_IOSF_MBI=y in the oracle build) and
// the display device info query used by HAS_FPGA_DBG_UNCLAIMED().
// ---------------------------------------------------------------------------

/// Kernel-owned operations used by the uncore translation. The owner installs
/// the complete table before any upstream GT caller can reach uncore init.
///
/// `punit_*` model `iosf_mbi_punit_acquire()/release()/assert_punit_acquired()`
/// from `asm/iosf_mbi.c`, a mutex serialising PUNIT access. The PMIC bus
/// notifier chain is `iosf_mbi_register/unregister_pmic_bus_access_notifier*`.
/// `display_has_fpga_dbg` answers `HAS_FPGA_DBG_UNCLAIMED(display)`.
#[repr(C)]
pub struct UncoreKernelOps {
    pub punit_acquire: unsafe extern "C" fn(),
    pub punit_release: unsafe extern "C" fn(),
    pub punit_assert_acquired: unsafe extern "C" fn(),
    pub register_pmic_bus_access_notifier:
        unsafe extern "C" fn(nb: *mut crate::linux::gem_memory::NotifierBlock) -> i32,
    pub unregister_pmic_bus_access_notifier_unlocked:
        unsafe extern "C" fn(nb: *mut crate::linux::gem_memory::NotifierBlock) -> i32,
    pub display_has_fpga_dbg: unsafe extern "C" fn(display: *mut c_void) -> bool,
}

static UNCORE_KERNEL_OPS: core::sync::atomic::AtomicPtr<UncoreKernelOps> =
    core::sync::atomic::AtomicPtr::new(ptr::null_mut());

/// Install the kernel-owned uncore services. Fails if a different table is
/// already installed.
pub fn install_uncore_kernel_ops(ops: &'static UncoreKernelOps) -> Result<(), &'static str> {
    let candidate = (ops as *const UncoreKernelOps).cast_mut();
    match UNCORE_KERNEL_OPS.compare_exchange(
        ptr::null_mut(),
        candidate,
        Ordering::Release,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(existing) if existing == candidate => Ok(()),
        Err(_) => Err("i915 uncore kernel operations are already installed"),
    }
}

/// Fail closed: upstream uncore code must not run before the owner installs
/// the kernel services.
fn uncore_kernel_ops() -> &'static UncoreKernelOps {
    let ops = UNCORE_KERNEL_OPS.load(Ordering::Acquire);
    assert!(
        !ops.is_null(),
        "kernel must install UncoreKernelOps before the upstream uncore path"
    );
    unsafe { &*ops }
}

#[inline]
fn iosf_mbi_punit_acquire() {
    unsafe { (uncore_kernel_ops().punit_acquire)() };
}

#[inline]
fn iosf_mbi_punit_release() {
    unsafe { (uncore_kernel_ops().punit_release)() };
}

#[inline]
fn iosf_mbi_assert_punit_acquired() {
    unsafe { (uncore_kernel_ops().punit_assert_acquired)() };
}

#[inline]
unsafe fn iosf_mbi_register_pmic_bus_access_notifier(nb: *mut crate::linux::gem_memory::NotifierBlock) -> i32 {
    unsafe { (uncore_kernel_ops().register_pmic_bus_access_notifier)(nb) }
}

#[inline]
unsafe fn iosf_mbi_unregister_pmic_bus_access_notifier_unlocked(
    nb: *mut crate::linux::gem_memory::NotifierBlock,
) -> i32 {
    unsafe { (uncore_kernel_ops().unregister_pmic_bus_access_notifier_unlocked)(nb) }
}

#[inline]
unsafe fn has_fpga_dbg_unclaimed_display(display: *mut c_void) -> bool {
    unsafe { (uncore_kernel_ops().display_has_fpga_dbg)(display) }
}

const MBI_PMIC_BUS_ACCESS_BEGIN: u64 = 1;
const MBI_PMIC_BUS_ACCESS_END: u64 = 2;
const NOTIFY_OK: i32 = 1;

// Linux kernel and LinuxKPI symbols that are owned elsewhere (see owners.tsv)
// and are called by this translation.
unsafe extern "C" {
    fn add_taint_for_CI(i915: *mut DrmI915Private, flag: u32);
    fn hrtimer_cancel(timer: *mut Hrtimer) -> i32;
    fn hrtimer_setup(
        timer: *mut Hrtimer,
        function: Option<unsafe extern "C" fn(*mut Hrtimer) -> i32>,
        clock_id: i32,
        mode: i32,
    );
    fn ioremap(base: u64, size: u64) -> *mut c_void;
    fn iounmap(addr: *mut c_void);
    fn intel_vgpu_active(i915: *mut DrmI915Private) -> bool;
}

/// Linux `drmm_add_action_or_reset()`. Not owned by any translated unit yet.
unsafe extern "C" {
    fn drmm_add_action_or_reset(
        drm: *mut DrmDevice,
        action: unsafe extern "C" fn(*mut DrmDevice, *mut c_void),
        data: *mut c_void,
    ) -> i32;
}

/// `i915_pmic_bus_access_notifier()`.
// upstream: intel_uncore.c i915_pmic_bus_access_notifier()
unsafe extern "C" fn i915_pmic_bus_access_notifier(
    nb: *mut crate::linux::gem_memory::NotifierBlock,
    action: u64,
    _data: *mut c_void,
) -> i32 {
    // container_of(nb, struct intel_uncore, pmic_bus_access_nb)
    let uncore = unsafe {
        nb.cast::<u8>()
            .sub(core::mem::offset_of!(IntelUncore, pmic_bus_access_nb))
            .cast::<IntelUncore>()
    };

    match action {
        MBI_PMIC_BUS_ACCESS_BEGIN => {
            // The notifier is unregistered during runtime suspend, so the access
            // is safe without an RPM wakeref: disable the wakeref asserts for it.
            unsafe { disable_rpm_wakeref_asserts((*uncore).rpm) };
            unsafe { intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL) };
            unsafe { enable_rpm_wakeref_asserts((*uncore).rpm) };
        }
        MBI_PMIC_BUS_ACCESS_END => {
            unsafe { intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL) };
        }
        _ => {}
    }

    NOTIFY_OK
}

use crate::intel_runtime_pm_upstream::{
    assert_rpm_device_not_suspended, assert_rpm_wakelock_held, disable_rpm_wakeref_asserts,
    enable_rpm_wakeref_asserts,
};

/// `uncore_unmap_mmio()`: drm-managed action.
unsafe extern "C" fn uncore_unmap_mmio(_drm: *mut DrmDevice, regs: *mut c_void) {
    unsafe { iounmap(regs) };
}

/// `intel_uncore_setup_mmio()`.
// upstream: intel_uncore.c intel_uncore_setup_mmio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_setup_mmio(uncore: *mut IntelUncore, phys_addr: u64) -> i32 {
    let i915 = unsafe { (*uncore).i915 };

    // Before gen4 the registers and the GTT are behind different BARs; from gen4
    // on they share a BAR. Meteor Lake and newer use the 4MB register range.
    let mmio_size: u64 = if unsafe { IS_DGFX(i915 as *const DrmI915Private) }
        || unsafe { i915_graphics_ver_full(i915) } >= crate::linux::i915::IP_VER(12, 70)
    {
        4 * 1024 * 1024
    } else if unsafe { i915_graphics_ver(i915) } >= 5 {
        2 * 1024 * 1024
    } else {
        512 * 1024
    };

    let regs = unsafe { ioremap(phys_addr, mmio_size) };
    unsafe { (*uncore).regs = regs };
    if regs.is_null() {
        drm_err!(
            unsafe { ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>() },
            "failed to map registers\n"
        );
        return -EIO;
    }

    unsafe {
        drmm_add_action_or_reset(
            ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>(),
            uncore_unmap_mmio,
            regs,
        )
    }
}

/// `intel_uncore_init_early()`.
// upstream: intel_uncore.c intel_uncore_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_init_early(uncore: *mut IntelUncore, gt: *mut IntelGt) {
    unsafe {
        crate::linux::locks::spin_lock_init(&mut (*uncore).lock);
        (*uncore).i915 = (*gt).i915;
        (*uncore).gt = gt;
        (*uncore).rpm = ptr::addr_of_mut!((*(*gt).i915).runtime_pm).cast::<IntelRuntimePm>();
    }
}

/// `uncore_raw_init()`.
unsafe fn uncore_raw_init(uncore: *mut IntelUncore) {
    GEM_BUG_ON!(unsafe { intel_uncore_has_forcewake(&*uncore) });

    let i915 = unsafe { (*uncore).i915 };
    let funcs: &mut IntelUncoreFuncs = unsafe { &mut (*uncore).funcs };
    if unsafe { intel_vgpu_active(i915) } {
        assign_raw_write(funcs, &vfuncs::VGPU_WRITE);
        assign_raw_read(funcs, &vfuncs::VGPU);
    } else if unsafe { i915_graphics_ver(i915) } == 5 {
        assign_raw_write(funcs, &vfuncs::GEN5_WRITE);
        assign_raw_read(funcs, &vfuncs::GEN5);
    } else {
        assign_raw_write(funcs, &vfuncs::GEN2_WRITE);
        assign_raw_read(funcs, &vfuncs::GEN2);
    }
}

/// ASSIGN_RAW_WRITE_MMIO_VFUNCS
#[inline]
fn assign_raw_write(funcs: &mut IntelUncoreFuncs, w: &vfuncs::Writers) {
    funcs.mmio_writeb = Some(w.0);
    funcs.mmio_writew = Some(w.1);
    funcs.mmio_writel = Some(w.2);
}

/// ASSIGN_RAW_READ_MMIO_VFUNCS
#[inline]
fn assign_raw_read(funcs: &mut IntelUncoreFuncs, r: &vfuncs::Readers) {
    funcs.mmio_readb = Some(r.0);
    funcs.mmio_readw = Some(r.1);
    funcs.mmio_readl = Some(r.2);
    funcs.mmio_readq = Some(r.3);
}

/// ASSIGN_WRITE_MMIO_VFUNCS (raw writes plus the forcewake-domain resolver).
#[inline]
fn assign_write(
    funcs: &mut IntelUncoreFuncs,
    w: &vfuncs::Writers,
    write_fw_domains: unsafe extern "C" fn(*mut IntelUncore, I915RegT) -> ForcewakeDomains,
) {
    assign_raw_write(funcs, w);
    funcs.write_fw_domains = Some(write_fw_domains);
}

/// ASSIGN_READ_MMIO_VFUNCS (raw reads plus the forcewake-domain resolver).
#[inline]
fn assign_read(
    funcs: &mut IntelUncoreFuncs,
    r: &vfuncs::Readers,
    read_fw_domains: unsafe extern "C" fn(*mut IntelUncore, I915RegT) -> ForcewakeDomains,
) {
    assign_raw_read(funcs, r);
    funcs.read_fw_domains = Some(read_fw_domains);
}

#[inline]
fn assign_fw_domains_table(uncore: &mut IntelUncore, table: &[IntelForcewakeRange]) {
    uncore.fw_domains_table = table.as_ptr();
    uncore.fw_domains_table_entries = table.len() as u32;
}

#[inline]
fn assign_shadow_table(uncore: &mut IntelUncore, table: &[IntelMmioRange]) {
    uncore.shadowed_reg_table = table.as_ptr();
    uncore.shadowed_reg_table_entries = table.len() as u32;
}

/// `uncore_media_forcewake_init()`.
unsafe fn uncore_media_forcewake_init(uncore: *mut IntelUncore) -> i32 {
    let i915 = unsafe { (*uncore).i915 };
    let media_ver = unsafe { MEDIA_VER(i915 as *const DrmI915Private) };
    if media_ver >= 13 {
        let u = unsafe { &mut *uncore };
        assign_fw_domains_table(u, &XELPMP_FW_RANGES);
        assign_shadow_table(u, &XELPMP_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else {
        MISSING_CASE!(media_ver);
        return -ENODEV;
    }

    0
}

/// `uncore_forcewake_init()`.
unsafe fn uncore_forcewake_init(uncore: *mut IntelUncore) -> i32 {
    let i915 = unsafe { (*uncore).i915 };

    GEM_BUG_ON!(!unsafe { intel_uncore_has_forcewake(&*uncore) });

    let ret = unsafe { intel_uncore_fw_domains_init(uncore) };
    if ret != 0 {
        return ret;
    }

    unsafe { forcewake_early_sanitize(uncore, 0) };

    {
        let u = unsafe { &mut *uncore };
        assign_read(&mut u.funcs, &vfuncs::FWTABLE_READ, fwtable_reg_read_fw_domains);
    }

    if unsafe { (*(*uncore).gt).type_ } == GT_MEDIA {
        return unsafe { uncore_media_forcewake_init(uncore) };
    }

    let ver_full = unsafe { i915_graphics_ver_full(i915) };
    let ver = unsafe { i915_graphics_ver(i915) };
    let i915c = i915 as *const DrmI915Private;
    let u = unsafe { &mut *uncore };

    if ver_full >= crate::linux::i915::IP_VER(12, 70) {
        assign_fw_domains_table(u, &MTL_FW_RANGES);
        assign_shadow_table(u, &MTL_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else if ver_full >= crate::linux::i915::IP_VER(12, 55) {
        assign_fw_domains_table(u, &DG2_FW_RANGES);
        assign_shadow_table(u, &DG2_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else if ver >= 12 {
        assign_fw_domains_table(u, &GEN12_FW_RANGES);
        assign_shadow_table(u, &GEN12_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else if ver == 11 {
        assign_fw_domains_table(u, &GEN11_FW_RANGES);
        assign_shadow_table(u, &GEN11_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else if unsafe { IS_GRAPHICS_VER(i915c, 9, 10) } {
        assign_fw_domains_table(u, &GEN9_FW_RANGES);
        assign_shadow_table(u, &GEN8_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else if unsafe { IS_CHERRYVIEW(i915c) } {
        assign_fw_domains_table(u, &CHV_FW_RANGES);
        assign_shadow_table(u, &GEN8_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else if ver == 8 {
        assign_fw_domains_table(u, &GEN6_FW_RANGES);
        assign_shadow_table(u, &GEN8_SHADOWED_REGS);
        assign_write(&mut u.funcs, &vfuncs::FWTABLE_WRITE, fwtable_reg_write_fw_domains);
    } else if unsafe { IS_VALLEYVIEW(i915c) } {
        assign_fw_domains_table(u, &VLV_FW_RANGES);
        assign_write(&mut u.funcs, &vfuncs::GEN6_WRITE, gen6_reg_write_fw_domains);
    } else if unsafe { IS_GRAPHICS_VER(i915c, 6, 7) } {
        assign_fw_domains_table(u, &GEN6_FW_RANGES);
        assign_write(&mut u.funcs, &vfuncs::GEN6_WRITE, gen6_reg_write_fw_domains);
    }

    unsafe {
        (*uncore).pmic_bus_access_nb.notifier_call = Some(i915_pmic_bus_access_notifier);
        iosf_mbi_register_pmic_bus_access_notifier(ptr::addr_of_mut!((*uncore).pmic_bus_access_nb));
    }

    0
}

/// `sanity_check_mmio_access()`.
unsafe fn sanity_check_mmio_access(uncore: *mut IntelUncore) -> i32 {
    let i915 = unsafe { (*uncore).i915 };
    if unsafe { i915_graphics_ver(i915) } < 8 {
        return 0;
    }

    // Use the primary GT's forcewake register as the guinea pig: it is masked, so
    // its upper 16 bits never read back as 1s when device access works.
    // If MMIO is not working, wait up to 2 seconds for it to recover.
    if wait_for_ms(
        || unsafe { __raw_uncore_read32(uncore, FORCEWAKE_MT) } != !0,
        2000,
    ) == -ETIMEDOUT
    {
        drm_err!(
            unsafe { ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>() },
            "Device is non-operational; MMIO access returns 0xFFFFFFFF!\n"
        );
        return -EIO;
    }

    0
}

/// `intel_uncore_init_mmio()`.
// upstream: intel_uncore.c intel_uncore_init_mmio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_init_mmio(uncore: *mut IntelUncore) -> i32 {
    let i915 = unsafe { (*uncore).i915 };

    let mut ret = unsafe { sanity_check_mmio_access(uncore) };
    if ret != 0 {
        return ret;
    }

    // The boot firmware initializes local memory and assesses its health. If
    // memory training fails, the punit keeps the GT powered down and we must not
    // continue with driver initialization.
    if unsafe { IS_DGFX(i915 as *const DrmI915Private) }
        && unsafe { __raw_uncore_read32(uncore, GU_CNTL) } & LMEM_INIT == 0
    {
        drm_err!(
            unsafe { ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>() },
            "LMEM not initialized by firmware\n"
        );
        return -ENODEV;
    }

    if unsafe { i915_graphics_ver(i915) } > 5 && !unsafe { intel_vgpu_active(i915) } {
        unsafe { (*uncore).flags |= UNCORE_HAS_FORCEWAKE };
    }

    if !unsafe { intel_uncore_has_forcewake(&*uncore) } {
        unsafe { uncore_raw_init(uncore) };
    } else {
        ret = unsafe { uncore_forcewake_init(uncore) };
        if ret != 0 {
            return ret;
        }
    }

    // Make sure the fw funcs are set if and only if we have fw.
    let has_fw = unsafe { intel_uncore_has_forcewake(&*uncore) };
    GEM_BUG_ON!(has_fw != unsafe { !(*uncore).fw_get_funcs.is_null() });
    GEM_BUG_ON!(has_fw != unsafe { (*uncore).funcs.read_fw_domains.is_some() });
    GEM_BUG_ON!(has_fw != unsafe { (*uncore).funcs.write_fw_domains.is_some() });

    let display = unsafe { (*i915).display };
    if unsafe { has_fpga_dbg_unclaimed_display(display) } {
        unsafe { (*uncore).flags |= UNCORE_HAS_FPGA_DBG_UNCLAIMED };
    }

    let i915c = i915 as *const DrmI915Private;
    if unsafe { IS_VALLEYVIEW(i915c) } || unsafe { IS_CHERRYVIEW(i915c) } {
        unsafe { (*uncore).flags |= UNCORE_HAS_DBG_UNCLAIMED };
    }

    if unsafe { IS_GRAPHICS_VER(i915c, 6, 7) } {
        unsafe { (*uncore).flags |= UNCORE_HAS_FIFO };
    }

    // Clear out unclaimed reg detection bit.
    if unsafe { intel_uncore_unclaimed_mmio(uncore) } {
        drm_dbg!(
            unsafe { ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>() },
            "unclaimed mmio detected on uncore init, clearing\n"
        );
    }

    0
}

/// `intel_uncore_prune_engine_fw_domains()`: some engines may be fused off after
/// the domains were initialized; drop the domains that reference none.
// upstream: intel_uncore.c intel_uncore_prune_engine_fw_domains()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_prune_engine_fw_domains(uncore: *mut IntelUncore, gt: *mut IntelGt) {
    let fw_domains = unsafe { (*uncore).fw_domains };
    let i915 = unsafe { (*uncore).i915 };

    if !unsafe { intel_uncore_has_forcewake(&*uncore) } || unsafe { i915_graphics_ver(i915) } < 11 {
        return;
    }

    for i in 0..crate::linux_config::I915_MAX_VCS as u32 {
        let domain_id = FW_DOMAIN_ID_MEDIA_VDBOX0 + i as i32;
        if unsafe { HAS_ENGINE(gt, crate::intel_engine_types_upstream::VCS0 + i as i32) } {
            continue;
        }

        // Starting with XeHP, the power well for an even-numbered VDBOX is also
        // used for shared units within the media slice (such as SFC), so keep the
        // domain if any other engine in the slice is present.
        if unsafe { i915_graphics_ver_full(i915) } >= crate::linux::i915::IP_VER(12, 55) && i % 2 == 0
        {
            if (i + 1) < crate::linux_config::I915_MAX_VCS as u32
                && unsafe { HAS_ENGINE(gt, crate::intel_engine_types_upstream::VCS0 + (i + 1) as i32) }
            {
                continue;
            }
            if unsafe { HAS_ENGINE(gt, crate::intel_engine_types_upstream::VECS0 + (i / 2) as i32) } {
                continue;
            }
        }

        if fw_domains & (1_i32 << domain_id) != 0 {
            unsafe { fw_domain_fini(uncore, domain_id) };
        }
    }

    for i in 0..crate::linux_config::I915_MAX_VECS as u32 {
        let domain_id = FW_DOMAIN_ID_MEDIA_VEBOX0 + i as i32;
        if unsafe { HAS_ENGINE(gt, crate::intel_engine_types_upstream::VECS0 + i as i32) } {
            continue;
        }

        if fw_domains & (1_i32 << domain_id) != 0 {
            unsafe { fw_domain_fini(uncore, domain_id) };
        }
    }

    if fw_domains & (1_i32 << FW_DOMAIN_ID_GSC) != 0
        && !unsafe { HAS_ENGINE(gt, crate::intel_engine_types_upstream::GSC0) }
    {
        unsafe { fw_domain_fini(uncore, FW_DOMAIN_ID_GSC) };
    }

}

/// `driver_initiated_flr()`: the driver-initiated FLR, run as the last action
/// before releasing the HW during the driver release flow.
// upstream: intel_uncore.c driver_initiated_flr()
unsafe fn driver_initiated_flr(uncore: *mut IntelUncore) {
    let i915 = unsafe { (*uncore).i915 };
    let drm = unsafe { ptr::addr_of_mut!((*i915).drm).cast::<DrmDevice>() };

    drm_dbg!(drm, "Triggering Driver-FLR\n");

    // The spec recommends a 3 second FLR reset timeout; extend it to 9 seconds.
    let flr_timeout_ms: u32 = 9000;

    // Make sure any pending FLR request has cleared, and clear the sticky status.
    let mut ret = unsafe {
        intel_wait_for_register_fw(uncore, GU_CNTL, DRIVERFLR, 0, flr_timeout_ms, ptr::null_mut())
    };
    if ret != 0 {
        drm_err!(drm, "Failed to wait for Driver-FLR bit to clear! %d\n", ret);
        return;
    }
    unsafe { crate::intel_uncore_types_upstream::intel_uncore_write_fw(uncore, GU_DEBUG, DRIVERFLR_STATUS) };

    // Trigger the actual Driver-FLR.
    unsafe { crate::intel_uncore_types_upstream::intel_uncore_rmw_fw(uncore, GU_CNTL, 0, DRIVERFLR) };

    // Wait for hardware teardown to complete.
    ret = unsafe {
        intel_wait_for_register_fw(uncore, GU_CNTL, DRIVERFLR, 0, flr_timeout_ms, ptr::null_mut())
    };
    if ret != 0 {
        drm_err!(drm, "Driver-FLR-teardown wait completion failed! %d\n", ret);
        return;
    }

    // Wait for hardware/firmware re-init to complete.
    ret = unsafe {
        intel_wait_for_register_fw(
            uncore,
            GU_DEBUG,
            DRIVERFLR_STATUS,
            DRIVERFLR_STATUS,
            flr_timeout_ms,
            ptr::null_mut(),
        )
    };
    if ret != 0 {
        drm_err!(drm, "Driver-FLR-reinit wait completion failed! %d\n", ret);
        return;
    }

    // Clear sticky completion status.
    unsafe { crate::intel_uncore_types_upstream::intel_uncore_write_fw(uncore, GU_DEBUG, DRIVERFLR_STATUS) };
}

/// `intel_uncore_fini_mmio()`: drm-managed action.
// upstream: intel_uncore.c intel_uncore_fini_mmio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_fini_mmio(_dev: *mut DrmDevice, data: *mut c_void) {
    let uncore = data.cast::<IntelUncore>();

    if unsafe { intel_uncore_has_forcewake(&*uncore) } {
        iosf_mbi_punit_acquire();
        unsafe {
            iosf_mbi_unregister_pmic_bus_access_notifier_unlocked(ptr::addr_of_mut!(
                (*uncore).pmic_bus_access_nb
            ))
        };
        unsafe { intel_uncore_forcewake_reset(uncore) };
        unsafe { intel_uncore_fw_domains_fini(uncore) };
        iosf_mbi_punit_release();
    }

    if unsafe { intel_uncore_needs_flr_on_fini(&*uncore) } {
        unsafe { driver_initiated_flr(uncore) };
    }
}

/// `__intel_wait_for_register_fw()`: caller holds forcewake.
// upstream: intel_uncore.c __intel_wait_for_register_fw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_wait_for_register_fw(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    mask: u32,
    value: u32,
    fast_timeout_us: u32,
    slow_timeout_ms: u32,
    out_value: *mut u32,
) -> i32 {
    let mut reg_value: u32 = 0;

    // Catch any overuse of this function.
    GEM_BUG_ON!(fast_timeout_us > 20000);
    GEM_BUG_ON!(fast_timeout_us == 0 && slow_timeout_ms == 0);

    let mut done = || {
        reg_value = unsafe { crate::intel_uncore_types_upstream::intel_uncore_read_fw(uncore, reg) };
        reg_value & mask == value
    };

    let mut ret = -ETIMEDOUT;
    if fast_timeout_us != 0 && fast_timeout_us <= 20000 {
        ret = wait_for_atomic_us_local(&mut done, fast_timeout_us as u64);
    }
    if ret != 0 && slow_timeout_ms != 0 {
        ret = wait_for_ms(&mut done, slow_timeout_ms as u64);
    }

    if !out_value.is_null() {
        unsafe { *out_value = reg_value };
    }
    ret
}

/// `__intel_wait_for_register()`: takes forcewake itself.
// upstream: intel_uncore.c __intel_wait_for_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_wait_for_register(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    mask: u32,
    value: u32,
    fast_timeout_us: u32,
    slow_timeout_ms: u32,
    out_value: *mut u32,
) -> i32 {
    let fw = unsafe { intel_uncore_forcewake_for_reg(uncore, reg, FW_REG_READ) };
    let mut reg_value: u32 = 0;

    unsafe { spin_lock_irq(&mut (*uncore).lock) };
    unsafe { intel_uncore_forcewake_get__locked(uncore, fw) };
    let mut ret = unsafe {
        __intel_wait_for_register_fw(uncore, reg, mask, value, fast_timeout_us, 0, &mut reg_value)
    };
    unsafe { intel_uncore_forcewake_put__locked(uncore, fw) };
    unsafe { spin_unlock_irq(&mut (*uncore).lock) };

    if ret != 0 && slow_timeout_ms != 0 {
        // __wait_for(reg_value = intel_uncore_read_notrace(uncore, reg), ...,
        //            slow_timeout_ms * 1000, 10, 1000): may sleep.
        let mut poll = || {
            reg_value = unsafe { crate::intel_uncore_types_upstream::intel_uncore_read_notrace(uncore, reg) };
            reg_value & mask == value
        };
        let timed_out = wait_until(slow_timeout_ms as u64 * 1_000_000, &mut poll, true);
        ret = if timed_out { -ETIMEDOUT } else { 0 };
    }

    // Trace the final value only (trace_i915_reg_rw is a no-op here).
    if !out_value.is_null() {
        unsafe { *out_value = reg_value };
    }
    ret
}

/// `intel_uncore_unclaimed_mmio()`.
// upstream: intel_uncore.c intel_uncore_unclaimed_mmio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_unclaimed_mmio(uncore: *mut IntelUncore) -> bool {
    let debug = unsafe { uncore_debug(uncore) };
    if debug.is_null() {
        return false;
    }

    unsafe { spin_lock_irq(&mut (*debug).lock) };
    let ret = unsafe { check_for_unclaimed_mmio(uncore) };
    unsafe { spin_unlock_irq(&mut (*debug).lock) };
    ret
}

/// `intel_uncore_arm_unclaimed_mmio_detection()`.
// upstream: intel_uncore.c intel_uncore_arm_unclaimed_mmio_detection()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_arm_unclaimed_mmio_detection(uncore: *mut IntelUncore) -> bool {
    let mut ret = false;
    let debug = unsafe { uncore_debug(uncore) };
    if debug.is_null() {
        drm_warn_msg("unclaimed mmio detection armed without mmio debug state");
        return false;
    }

    unsafe { spin_lock_irq(&mut (*debug).lock) };

    if unsafe { (*debug).unclaimed_mmio_check } > 0 {
        if unsafe { check_for_unclaimed_mmio(uncore) } {
            if unsafe { mmio_debug_params(uncore) } == 0 {
                drm_dbg!(
                    unsafe { uncore_dev(uncore) },
                    "Unclaimed register detected, enabling oneshot unclaimed register reporting. Please use i915.mmio_debug=N for more information.\n"
                );
                unsafe { set_mmio_debug_param(uncore, mmio_debug_params(uncore) + 1) };
            }
            unsafe { (*debug).unclaimed_mmio_check -= 1 };
            ret = true;
        }
    }

    unsafe { spin_unlock_irq(&mut (*debug).lock) };
    ret
}

/// `intel_uncore_forcewake_for_reg()`.
// upstream: intel_uncore.c intel_uncore_forcewake_for_reg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uncore_forcewake_for_reg(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    op: u32,
) -> ForcewakeDomains {
    let mut fw_domains: ForcewakeDomains = 0;
    if op == 0 {
        drm_warn_msg("forcewake_for_reg called without an access mode");
    }

    if !unsafe { intel_uncore_has_forcewake(&*uncore) } {
        return 0;
    }

    if op & FW_REG_READ != 0 {
        let read_fw_domains = unsafe { (*uncore).funcs.read_fw_domains }
            .expect("forcewake read-domain callback is uninitialized");
        fw_domains = unsafe { read_fw_domains(uncore, reg) };
    }
    if op & FW_REG_WRITE != 0 {
        let write_fw_domains = unsafe { (*uncore).funcs.write_fw_domains }
            .expect("forcewake write-domain callback is uninitialized");
        fw_domains |= unsafe { write_fw_domains(uncore, reg) };
    }

    if fw_domains & !unsafe { (*uncore).fw_domains } != 0 {
        drm_warn_msg("register callback returned unavailable forcewake domains");
    }

    fw_domains
}

/// `intel_mmio_bar()` from include/drm/intel/pci_config.h (static inline there).
// upstream: include/drm/intel/pci_config.h intel_mmio_bar()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mmio_bar(graphics_ver: i32) -> i32 {
    const GEN2_MMADR_BAR: i32 = 1;
    const GEN3_MMADR_BAR: i32 = 0;
    const GEN4_GTTMMADR_BAR: i32 = 0;
    match graphics_ver {
        2 => GEN2_MMADR_BAR,
        3 => GEN3_MMADR_BAR,
        _ => GEN4_GTTMMADR_BAR,
    }
}
