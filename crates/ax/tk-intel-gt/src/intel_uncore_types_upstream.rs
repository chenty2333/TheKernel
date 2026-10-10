// SPDX-License-Identifier: MIT
// Copyright © 2017 Intel Corporation.
// Source-order bindings from Linux v7.2.3 drivers/gpu/drm/i915/intel_uncore.h.

#![allow(non_snake_case)]

use core::ffi::{c_char, c_ulong, c_void};

use crate::{
    intel_context_upstream::Hrtimer,
    intel_engine_cs_upstream::Spinlock,
    intel_gt_types_upstream::{IntelGt, IntelMmioRange, PhysAddrT},
    intel_workarounds_types_upstream::I915RegT,
    linux::gem_memory::NotifierBlock,
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
pub struct DrmDevice {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct IntelRuntimePm {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct IntelUncoreMmioDebug {
    pub lock: Spinlock,
    pub unclaimed_mmio_check: i32,
    pub saved_mmio_check: i32,
    pub suspend_count: u32,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForcewakeDomainIdValue {
    Render      = 0,
    Gt          = 1,
    Media       = 2,
    MediaVdbox0 = 3,
    MediaVdbox1 = 4,
    MediaVdbox2 = 5,
    MediaVdbox3 = 6,
    MediaVdbox4 = 7,
    MediaVdbox5 = 8,
    MediaVdbox6 = 9,
    MediaVdbox7 = 10,
    MediaVebox0 = 11,
    MediaVebox1 = 12,
    MediaVebox2 = 13,
    MediaVebox3 = 14,
    Gsc         = 15,
    Count       = 16,
}

pub type ForcewakeDomainId = i32;
pub const FW_DOMAIN_ID_RENDER: ForcewakeDomainId = 0;
pub const FW_DOMAIN_ID_GT: ForcewakeDomainId = 1;
pub const FW_DOMAIN_ID_MEDIA: ForcewakeDomainId = 2;
pub const FW_DOMAIN_ID_MEDIA_VDBOX0: ForcewakeDomainId = 3;
pub const FW_DOMAIN_ID_MEDIA_VDBOX1: ForcewakeDomainId = 4;
pub const FW_DOMAIN_ID_MEDIA_VDBOX2: ForcewakeDomainId = 5;
pub const FW_DOMAIN_ID_MEDIA_VDBOX3: ForcewakeDomainId = 6;
pub const FW_DOMAIN_ID_MEDIA_VDBOX4: ForcewakeDomainId = 7;
pub const FW_DOMAIN_ID_MEDIA_VDBOX5: ForcewakeDomainId = 8;
pub const FW_DOMAIN_ID_MEDIA_VDBOX6: ForcewakeDomainId = 9;
pub const FW_DOMAIN_ID_MEDIA_VDBOX7: ForcewakeDomainId = 10;
pub const FW_DOMAIN_ID_MEDIA_VEBOX0: ForcewakeDomainId = 11;
pub const FW_DOMAIN_ID_MEDIA_VEBOX1: ForcewakeDomainId = 12;
pub const FW_DOMAIN_ID_MEDIA_VEBOX2: ForcewakeDomainId = 13;
pub const FW_DOMAIN_ID_MEDIA_VEBOX3: ForcewakeDomainId = 14;
pub const FW_DOMAIN_ID_GSC: ForcewakeDomainId = 15;
pub const FW_DOMAIN_ID_COUNT: ForcewakeDomainId = 16;

pub type ForcewakeDomains = i32;
pub const FORCEWAKE_RENDER: ForcewakeDomains = 1 << FW_DOMAIN_ID_RENDER;
pub const FORCEWAKE_GT: ForcewakeDomains = 1 << FW_DOMAIN_ID_GT;
pub const FORCEWAKE_MEDIA: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA;
pub const FORCEWAKE_MEDIA_VDBOX0: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX0;
pub const FORCEWAKE_MEDIA_VDBOX1: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX1;
pub const FORCEWAKE_MEDIA_VDBOX2: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX2;
pub const FORCEWAKE_MEDIA_VDBOX3: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX3;
pub const FORCEWAKE_MEDIA_VDBOX4: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX4;
pub const FORCEWAKE_MEDIA_VDBOX5: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX5;
pub const FORCEWAKE_MEDIA_VDBOX6: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX6;
pub const FORCEWAKE_MEDIA_VDBOX7: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VDBOX7;
pub const FORCEWAKE_MEDIA_VEBOX0: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VEBOX0;
pub const FORCEWAKE_MEDIA_VEBOX1: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VEBOX1;
pub const FORCEWAKE_MEDIA_VEBOX2: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VEBOX2;
pub const FORCEWAKE_MEDIA_VEBOX3: ForcewakeDomains = 1 << FW_DOMAIN_ID_MEDIA_VEBOX3;
pub const FORCEWAKE_GSC: ForcewakeDomains = 1 << FW_DOMAIN_ID_GSC;
pub const FORCEWAKE_ALL: ForcewakeDomains = (1 << FW_DOMAIN_ID_COUNT) - 1;

#[repr(C)]
pub struct IntelUncoreFwGet {
    pub force_wake_get: Option<unsafe extern "C" fn(*mut IntelUncore, ForcewakeDomains)>,
}

#[repr(C)]
pub struct IntelUncoreFuncs {
    pub read_fw_domains:
        Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT) -> ForcewakeDomains>,
    pub write_fw_domains:
        Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT) -> ForcewakeDomains>,
    pub mmio_readb: Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u8>,
    pub mmio_readw: Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u16>,
    pub mmio_readl: Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u32>,
    pub mmio_readq: Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT, bool) -> u64>,
    pub mmio_writeb: Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT, u8, bool)>,
    pub mmio_writew: Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT, u16, bool)>,
    pub mmio_writel: Option<unsafe extern "C" fn(*mut IntelUncore, I915RegT, u32, bool)>,
}

#[repr(C)]
pub struct IntelForcewakeRange {
    pub start: u32,
    pub end: u32,
    pub domains: ForcewakeDomains,
}

#[repr(C)]
pub struct IntelUncoreForcewakeDomain {
    pub uncore: *mut IntelUncore,
    pub id: ForcewakeDomainId,
    pub mask: ForcewakeDomains,
    pub wake_count: u32,
    pub active: bool,
    pub timer: Hrtimer,
    pub reg_set: *mut u32,
    pub reg_ack: *mut u32,
}

pub const UNCORE_HAS_FORCEWAKE: u32 = 1 << 0;
pub const UNCORE_HAS_FPGA_DBG_UNCLAIMED: u32 = 1 << 1;
pub const UNCORE_HAS_DBG_UNCLAIMED: u32 = 1 << 2;
pub const UNCORE_HAS_FIFO: u32 = 1 << 3;
pub const UNCORE_NEEDS_FLR_ON_FINI: u32 = 1 << 4;

#[repr(C)]
pub struct IntelUncore {
    pub regs: *mut c_void,
    pub i915: *mut DrmI915Private,
    pub gt: *mut IntelGt,
    pub rpm: *mut IntelRuntimePm,
    pub lock: Spinlock,
    pub gsi_offset: u32,
    pub flags: u32,
    pub fw_domains_table: *const IntelForcewakeRange,
    pub fw_domains_table_entries: u32,
    pub shadowed_reg_table: *const IntelMmioRange,
    pub shadowed_reg_table_entries: u32,
    pub pmic_bus_access_nb: NotifierBlock,
    pub fw_get_funcs: *const IntelUncoreFwGet,
    pub funcs: IntelUncoreFuncs,
    pub fifo_count: u32,
    pub fw_domains: ForcewakeDomains,
    pub fw_domains_active: ForcewakeDomains,
    pub fw_domains_timer: ForcewakeDomains,
    pub fw_domains_saved: ForcewakeDomains,
    pub fw_domain: [*mut IntelUncoreForcewakeDomain; FW_DOMAIN_ID_COUNT as usize],
    pub user_forcewake_count: u32,
    pub debug: *mut IntelUncoreMmioDebug,
}

// Source inline flag queries and setter.
pub fn intel_uncore_has_forcewake(uncore: &IntelUncore) -> bool {
    uncore.flags & UNCORE_HAS_FORCEWAKE != 0
}
pub fn intel_uncore_has_fpga_dbg_unclaimed(uncore: &IntelUncore) -> bool {
    uncore.flags & UNCORE_HAS_FPGA_DBG_UNCLAIMED != 0
}
pub fn intel_uncore_has_dbg_unclaimed(uncore: &IntelUncore) -> bool {
    uncore.flags & UNCORE_HAS_DBG_UNCLAIMED != 0
}
pub fn intel_uncore_has_fifo(uncore: &IntelUncore) -> bool {
    uncore.flags & UNCORE_HAS_FIFO != 0
}
pub fn intel_uncore_needs_flr_on_fini(uncore: &IntelUncore) -> bool {
    uncore.flags & UNCORE_NEEDS_FLR_ON_FINI != 0
}
pub fn intel_uncore_set_flr_on_fini(uncore: &mut IntelUncore) -> bool {
    uncore.flags |= UNCORE_NEEDS_FLR_ON_FINI;
    true
}

pub const FW_REG_READ: u32 = 1;
pub const FW_REG_WRITE: u32 = 2;

unsafe extern "C" {
    pub fn intel_uncore_mmio_debug_init_early(i915: *mut DrmI915Private);
    pub fn intel_uncore_init_early(uncore: *mut IntelUncore, gt: *mut IntelGt);
    pub fn intel_uncore_setup_mmio(uncore: *mut IntelUncore, phys_addr: PhysAddrT) -> i32;
    pub fn intel_uncore_init_mmio(uncore: *mut IntelUncore) -> i32;
    pub fn intel_uncore_prune_engine_fw_domains(uncore: *mut IntelUncore, gt: *mut IntelGt);
    pub fn intel_uncore_unclaimed_mmio(uncore: *mut IntelUncore) -> bool;
    pub fn intel_uncore_arm_unclaimed_mmio_detection(uncore: *mut IntelUncore) -> bool;
    pub fn intel_uncore_fini_mmio(dev: *mut DrmDevice, data: *mut c_void);
    pub fn intel_uncore_suspend(uncore: *mut IntelUncore);
    pub fn intel_uncore_resume_early(uncore: *mut IntelUncore);
    pub fn intel_uncore_runtime_resume(uncore: *mut IntelUncore);
    pub fn assert_forcewakes_inactive(uncore: *mut IntelUncore);
    pub fn assert_forcewakes_active(uncore: *mut IntelUncore, domains: ForcewakeDomains);
    pub fn intel_uncore_forcewake_domain_to_str(id: ForcewakeDomainId) -> *const c_char;
    pub fn intel_uncore_forcewake_for_reg(
        uncore: *mut IntelUncore,
        reg: I915RegT,
        op: u32,
    ) -> ForcewakeDomains;
    pub fn intel_uncore_forcewake_get(uncore: *mut IntelUncore, domains: ForcewakeDomains);
    pub fn intel_uncore_forcewake_put(uncore: *mut IntelUncore, domains: ForcewakeDomains);
    pub fn intel_uncore_forcewake_put_delayed(uncore: *mut IntelUncore, domains: ForcewakeDomains);
    pub fn intel_uncore_forcewake_flush(uncore: *mut IntelUncore, domains: ForcewakeDomains);
    pub fn intel_uncore_forcewake_get__locked(uncore: *mut IntelUncore, domains: ForcewakeDomains);
    pub fn intel_uncore_forcewake_put__locked(uncore: *mut IntelUncore, domains: ForcewakeDomains);
    pub fn intel_uncore_forcewake_user_get(uncore: *mut IntelUncore);
    pub fn intel_uncore_forcewake_user_put(uncore: *mut IntelUncore);
    pub fn __intel_wait_for_register(
        uncore: *mut IntelUncore,
        reg: I915RegT,
        mask: u32,
        value: u32,
        fast_timeout_us: u32,
        slow_timeout_ms: u32,
        out_value: *mut u32,
    ) -> i32;
    pub fn __intel_wait_for_register_fw(
        uncore: *mut IntelUncore,
        reg: I915RegT,
        mask: u32,
        value: u32,
        fast_timeout_us: u32,
        slow_timeout_ms: u32,
        out_value: *mut u32,
    ) -> i32;
    pub fn to_intel_uncore(drm: *mut DrmDevice) -> *mut IntelUncore;
}

pub unsafe fn intel_wait_for_register(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    mask: u32,
    value: u32,
    timeout_ms: u32,
) -> i32 {
    unsafe {
        __intel_wait_for_register(
            uncore,
            reg,
            mask,
            value,
            2,
            timeout_ms,
            core::ptr::null_mut(),
        )
    }
}

pub unsafe fn intel_wait_for_register_fw(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    mask: u32,
    value: u32,
    timeout_ms: u32,
    out_value: *mut u32,
) -> i32 {
    unsafe { __intel_wait_for_register_fw(uncore, reg, mask, value, 2, timeout_ms, out_value) }
}

pub const fn IS_GSI_REG(reg: u32) -> bool {
    reg < 0x40000
}

fn mmio_offset(uncore: &IntelUncore, reg: I915RegT) -> usize {
    let offset = reg.reg;
    offset.wrapping_add(if IS_GSI_REG(offset) {
        uncore.gsi_offset
    } else {
        0
    }) as usize
}

unsafe fn raw_addr(uncore: *const IntelUncore, reg: I915RegT) -> *mut u8 {
    let offset = mmio_offset(unsafe { &*uncore }, reg);
    unsafe { (*uncore).regs.cast::<u8>().add(offset) }
}

pub unsafe fn __raw_uncore_read8(uncore: *const IntelUncore, reg: I915RegT) -> u8 {
    unsafe { core::ptr::read_volatile(raw_addr(uncore, reg).cast::<u8>()) }
}
pub unsafe fn __raw_uncore_read16(uncore: *const IntelUncore, reg: I915RegT) -> u16 {
    unsafe { core::ptr::read_volatile(raw_addr(uncore, reg).cast::<u16>()) }
}
pub unsafe fn __raw_uncore_read32(uncore: *const IntelUncore, reg: I915RegT) -> u32 {
    unsafe { core::ptr::read_volatile(raw_addr(uncore, reg).cast::<u32>()) }
}
pub unsafe fn __raw_uncore_read64(uncore: *const IntelUncore, reg: I915RegT) -> u64 {
    // intel_uncore.h includes io-64-nonatomic-lo-hi.h: read low word first,
    // then high word, even on the configured x86_64 target.
    let addr = unsafe { raw_addr(uncore, reg) };
    let low = unsafe { core::ptr::read_volatile(addr.cast::<u32>()) };
    let high = unsafe { core::ptr::read_volatile(addr.add(4).cast::<u32>()) };
    low as u64 + ((high as u64) << 32)
}
pub unsafe fn __raw_uncore_write8(uncore: *const IntelUncore, reg: I915RegT, value: u8) {
    unsafe { core::ptr::write_volatile(raw_addr(uncore, reg).cast::<u8>(), value) };
}
pub unsafe fn __raw_uncore_write16(uncore: *const IntelUncore, reg: I915RegT, value: u16) {
    unsafe { core::ptr::write_volatile(raw_addr(uncore, reg).cast::<u16>(), value) };
}
pub unsafe fn __raw_uncore_write32(uncore: *const IntelUncore, reg: I915RegT, value: u32) {
    unsafe { core::ptr::write_volatile(raw_addr(uncore, reg).cast::<u32>(), value) };
}
pub unsafe fn __raw_uncore_write64(uncore: *const IntelUncore, reg: I915RegT, value: u64) {
    let addr = unsafe { raw_addr(uncore, reg) };
    unsafe { core::ptr::write_volatile(addr.cast::<u32>(), value as u32) };
    unsafe { core::ptr::write_volatile(addr.add(4).cast::<u32>(), (value >> 32) as u32) };
}

pub unsafe fn intel_uncore_read8(uncore: *mut IntelUncore, reg: I915RegT) -> u8 {
    unsafe { ((*uncore).funcs.mmio_readb.unwrap())(uncore, reg, true) }
}
pub unsafe fn intel_uncore_read16(uncore: *mut IntelUncore, reg: I915RegT) -> u16 {
    unsafe { ((*uncore).funcs.mmio_readw.unwrap())(uncore, reg, true) }
}
pub unsafe fn intel_uncore_read(uncore: *mut IntelUncore, reg: I915RegT) -> u32 {
    unsafe { ((*uncore).funcs.mmio_readl.unwrap())(uncore, reg, true) }
}
pub unsafe fn intel_uncore_read16_notrace(uncore: *mut IntelUncore, reg: I915RegT) -> u16 {
    unsafe { ((*uncore).funcs.mmio_readw.unwrap())(uncore, reg, false) }
}
pub unsafe fn intel_uncore_read_notrace(uncore: *mut IntelUncore, reg: I915RegT) -> u32 {
    unsafe { ((*uncore).funcs.mmio_readl.unwrap())(uncore, reg, false) }
}
pub unsafe fn intel_uncore_write8(uncore: *mut IntelUncore, reg: I915RegT, value: u8) {
    unsafe { ((*uncore).funcs.mmio_writeb.unwrap())(uncore, reg, value, true) };
}
pub unsafe fn intel_uncore_write16(uncore: *mut IntelUncore, reg: I915RegT, value: u16) {
    unsafe { ((*uncore).funcs.mmio_writew.unwrap())(uncore, reg, value, true) };
}
pub unsafe fn intel_uncore_write(uncore: *mut IntelUncore, reg: I915RegT, value: u32) {
    unsafe { ((*uncore).funcs.mmio_writel.unwrap())(uncore, reg, value, true) };
}
pub unsafe fn intel_uncore_write_notrace(uncore: *mut IntelUncore, reg: I915RegT, value: u32) {
    unsafe { ((*uncore).funcs.mmio_writel.unwrap())(uncore, reg, value, false) };
}
pub unsafe fn intel_uncore_read64(uncore: *mut IntelUncore, reg: I915RegT) -> u64 {
    unsafe { ((*uncore).funcs.mmio_readq.unwrap())(uncore, reg, true) }
}

pub unsafe fn intel_uncore_posting_read(uncore: *mut IntelUncore, reg: I915RegT) {
    let _ = unsafe { intel_uncore_read_notrace(uncore, reg) };
}
pub unsafe fn intel_uncore_posting_read16(uncore: *mut IntelUncore, reg: I915RegT) {
    let _ = unsafe { intel_uncore_read16_notrace(uncore, reg) };
}
pub unsafe fn intel_uncore_read_fw(uncore: *const IntelUncore, reg: I915RegT) -> u32 {
    unsafe { __raw_uncore_read32(uncore, reg) }
}
pub unsafe fn intel_uncore_write_fw(uncore: *const IntelUncore, reg: I915RegT, value: u32) {
    unsafe { __raw_uncore_write32(uncore, reg, value) };
}
pub unsafe fn intel_uncore_write64_fw(uncore: *const IntelUncore, reg: I915RegT, value: u64) {
    unsafe { __raw_uncore_write64(uncore, reg, value) };
}
pub unsafe fn intel_uncore_posting_read_fw(uncore: *const IntelUncore, reg: I915RegT) {
    let _ = unsafe { intel_uncore_read_fw(uncore, reg) };
}

pub unsafe fn intel_uncore_rmw(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    clear: u32,
    set: u32,
) -> u32 {
    let old = unsafe { intel_uncore_read(uncore, reg) };
    unsafe { intel_uncore_write(uncore, reg, (old & !clear) | set) };
    old
}

pub unsafe fn intel_uncore_rmw_fw(uncore: *mut IntelUncore, reg: I915RegT, clear: u32, set: u32) {
    let old = unsafe { intel_uncore_read_fw(uncore, reg) };
    let value = (old & !clear) | set;
    if value != old {
        unsafe { intel_uncore_write_fw(uncore, reg, value) };
    }
}

pub unsafe fn intel_uncore_read64_2x32(
    uncore: *mut IntelUncore,
    lower_reg: I915RegT,
    upper_reg: I915RegT,
) -> u64 {
    let mut domains = unsafe { intel_uncore_forcewake_for_reg(uncore, lower_reg, FW_REG_READ) };
    domains |= unsafe { intel_uncore_forcewake_for_reg(uncore, upper_reg, FW_REG_READ) };
    let mut flags = 0 as c_ulong;
    unsafe { crate::linux_locks::spin_lock_irqsave(&mut (*uncore).lock, &mut flags) };
    unsafe { intel_uncore_forcewake_get__locked(uncore, domains) };
    let mut upper = unsafe { intel_uncore_read_fw(uncore, upper_reg) };
    let mut lower;
    let mut old_upper;
    let mut loop_count = 0;
    loop {
        old_upper = upper;
        lower = unsafe { intel_uncore_read_fw(uncore, lower_reg) };
        upper = unsafe { intel_uncore_read_fw(uncore, upper_reg) };
        if upper == old_upper || loop_count >= 2 {
            break;
        }
        loop_count += 1;
    }
    unsafe { intel_uncore_forcewake_put__locked(uncore, domains) };
    unsafe { crate::linux_locks::spin_unlock_irqrestore(&mut (*uncore).lock, flags) };
    ((upper as u64) << 32) | lower as u64
}

pub unsafe fn intel_uncore_write_and_verify(
    uncore: *mut IntelUncore,
    reg: I915RegT,
    value: u32,
    mask: u32,
    expected_value: u32,
) -> i32 {
    unsafe { intel_uncore_write(uncore, reg, value) };
    let reg_value = unsafe { intel_uncore_read(uncore, reg) };
    if reg_value & mask != expected_value {
        -crate::linux_config::EINVAL
    } else {
        0
    }
}

pub unsafe fn intel_uncore_regs(uncore: *mut IntelUncore) -> *mut c_void {
    unsafe { (*uncore).regs }
}

pub unsafe fn raw_reg_read(base: *const u8, reg: I915RegT) -> u32 {
    unsafe { core::ptr::read_volatile(base.add(reg.reg as usize).cast::<u32>()) }
}
pub unsafe fn raw_reg_write(base: *mut u8, reg: I915RegT, value: u32) {
    unsafe { core::ptr::write_volatile(base.add(reg.reg as usize).cast::<u32>(), value) };
}

pub struct IntelForcewakeDomainIter<'a> {
    uncore: &'a IntelUncore,
    remaining: ForcewakeDomains,
}

impl Iterator for IntelForcewakeDomainIter<'_> {
    type Item = *mut IntelUncoreForcewakeDomain;
    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining != 0 {
            let bit = (self.remaining as u32).trailing_zeros() as usize;
            self.remaining &= self.remaining - 1;
            let domain = self.uncore.fw_domain[bit];
            if !domain.is_null() {
                return Some(domain);
            }
        }
        None
    }
}

pub fn for_each_fw_domain_masked(
    uncore: &IntelUncore,
    mask: ForcewakeDomains,
) -> IntelForcewakeDomainIter<'_> {
    IntelForcewakeDomainIter {
        uncore,
        remaining: mask,
    }
}

pub fn for_each_fw_domain(uncore: &IntelUncore) -> IntelForcewakeDomainIter<'_> {
    for_each_fw_domain_masked(uncore, uncore.fw_domains)
}

// x86_64 Linux 7.2.3 layout: LOCKDEP=n and target spinlock/notifier/hrtimer
// layouts are imported from their canonical LinuxKPI owner declarations.
const _: [(); 16] = [(); core::mem::size_of::<IntelUncoreMmioDebug>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelUncoreMmioDebug, lock)];
const _: [(); 4] = [(); core::mem::offset_of!(IntelUncoreMmioDebug, unclaimed_mmio_check)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelUncoreMmioDebug, saved_mmio_check)];
const _: [(); 12] = [(); core::mem::offset_of!(IntelUncoreMmioDebug, suspend_count)];
const _: [(); 8] = [(); core::mem::size_of::<IntelUncoreFwGet>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelUncoreFwGet, force_wake_get)];
const _: [(); 72] = [(); core::mem::size_of::<IntelUncoreFuncs>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelUncoreFuncs, read_fw_domains)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelUncoreFuncs, write_fw_domains)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelUncoreFuncs, mmio_readb)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelUncoreFuncs, mmio_readw)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelUncoreFuncs, mmio_readl)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelUncoreFuncs, mmio_readq)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelUncoreFuncs, mmio_writeb)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelUncoreFuncs, mmio_writew)];
const _: [(); 64] = [(); core::mem::offset_of!(IntelUncoreFuncs, mmio_writel)];
const _: [(); 12] = [(); core::mem::size_of::<IntelForcewakeRange>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelForcewakeRange, start)];
const _: [(); 4] = [(); core::mem::offset_of!(IntelForcewakeRange, end)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelForcewakeRange, domains)];
const _: [(); 120] = [(); core::mem::size_of::<IntelUncoreForcewakeDomain>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelUncoreForcewakeDomain>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, uncore)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, id)];
const _: [(); 12] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, mask)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, wake_count)];
const _: [(); 20] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, active)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, timer)];
const _: [(); 104] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, reg_set)];
const _: [(); 112] = [(); core::mem::offset_of!(IntelUncoreForcewakeDomain, reg_ack)];
const _: [(); 352] = [(); core::mem::size_of::<IntelUncore>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelUncore>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelUncore, regs)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelUncore, i915)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelUncore, gt)];
const _: [(); 24] = [(); core::mem::offset_of!(IntelUncore, rpm)];
const _: [(); 32] = [(); core::mem::offset_of!(IntelUncore, lock)];
const _: [(); 36] = [(); core::mem::offset_of!(IntelUncore, gsi_offset)];
const _: [(); 40] = [(); core::mem::offset_of!(IntelUncore, flags)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelUncore, fw_domains_table)];
const _: [(); 56] = [(); core::mem::offset_of!(IntelUncore, fw_domains_table_entries)];
const _: [(); 64] = [(); core::mem::offset_of!(IntelUncore, shadowed_reg_table)];
const _: [(); 72] = [(); core::mem::offset_of!(IntelUncore, shadowed_reg_table_entries)];
const _: [(); 80] = [(); core::mem::offset_of!(IntelUncore, pmic_bus_access_nb)];
const _: [(); 104] = [(); core::mem::offset_of!(IntelUncore, fw_get_funcs)];
const _: [(); 112] = [(); core::mem::offset_of!(IntelUncore, funcs)];
const _: [(); 184] = [(); core::mem::offset_of!(IntelUncore, fifo_count)];
const _: [(); 188] = [(); core::mem::offset_of!(IntelUncore, fw_domains)];
const _: [(); 208] = [(); core::mem::offset_of!(IntelUncore, fw_domain)];
const _: [(); 336] = [(); core::mem::offset_of!(IntelUncore, user_forcewake_count)];
const _: [(); 344] = [(); core::mem::offset_of!(IntelUncore, debug)];
