// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display power wells, translated from `drivers/gpu/drm/i915/display/intel_display_power_well.c`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]

use core::ffi::{c_char, c_void};

use crate::display_pending_stubs::b3c::{intel_dmc_has_payload, intel_dmc_wl_get_noreg, intel_dmc_wl_put_noreg};
use crate::display_pending_stubs::b3d::{gen8_irq_power_well_post_enable, gen8_irq_power_well_pre_disable};
use crate::intel_de_upstream::{
    intel_de_read, intel_de_rmw, intel_de_wait_for_clear_ms, intel_de_wait_for_set_ms, intel_de_write,
};
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_upstream::intel_display_device_enabled;
use crate::intel_display_power_types_upstream::{
    I915PowerDomains, IntelDisplayPowerDomain, IntelPowerDomainMask,
};
use crate::intel_display_power_well_types_upstream::{
    I915PowerWell, I915PowerWellDesc, I915PowerWellId, I915PowerWellInstance,
    DISP_PW_ID_NONE, HSW_DISP_PW_GLOBAL, ICL_DISP_PW_3, SKL_DISP_DC_OFF, SKL_DISP_PW_1,
    SKL_DISP_PW_2, SKL_DISP_PW_MISC_IO, TGL_DISP_PW_TC_COLD_OFF,
};
use crate::intel_display_regs_upstream::{
    DC_STATE_DISABLE, DC_STATE_EN, DC_STATE_EN_DC3CO, DC_STATE_EN_UPTO_DC5,
    DC_STATE_EN_UPTO_DC5_DC6_MASK, DC_STATE_EN_UPTO_DC6, GEN8_CHICKEN_DCPR_1,
    HSW_PWR_WELL_CTL1, HSW_PWR_WELL_CTL2, HSW_PWR_WELL_CTL3, HSW_PWR_WELL_CTL4,
    HSW_PWR_WELL_CTL_REQ, HSW_PWR_WELL_CTL_STATE, ICL_PW_CTL_IDX_PW_1,
    ICL_PWR_WELL_CTL_AUX1, ICL_PWR_WELL_CTL_AUX2, ICL_PWR_WELL_CTL_AUX4,
    ICL_PWR_WELL_CTL_DDI1, ICL_PWR_WELL_CTL_DDI2, ICL_PWR_WELL_CTL_DDI4,
    SKL_FUSE_STATUS, SKL_PW_CTL_IDX_PW_1,
};
use crate::intel_display_reg_defs_upstream::mmio;
use crate::intel_parent_upstream::intel_parent_pcode_read;
use crate::intel_workarounds_types_upstream::I915RegT;

#[repr(C)]
pub struct I915PowerWellRegs {
    pub bios: I915RegT,
    pub driver: I915RegT,
    pub kvmr: I915RegT,
    pub debug: I915RegT,
}

#[repr(C)]
pub struct I915PowerWellOps {
    pub regs: *const I915PowerWellRegs,
    pub sync_hw: Option<unsafe extern "C" fn(*mut IntelDisplay, *mut I915PowerWell)>,
    pub enable: Option<unsafe extern "C" fn(*mut IntelDisplay, *mut I915PowerWell)>,
    pub disable: Option<unsafe extern "C" fn(*mut IntelDisplay, *mut I915PowerWell)>,
    pub is_enabled: Option<unsafe extern "C" fn(*mut IntelDisplay, *mut I915PowerWell) -> bool>,
}

unsafe impl Sync for I915PowerWellOps {}

#[repr(C)]
pub struct I915PowerWellInstanceList {
    pub list: *const I915PowerWellInstance,
    pub count: u8,
}

unsafe impl Sync for I915PowerWellInstanceList {}

#[repr(C)]
pub struct I915PowerWellDescList {
    pub list: *const I915PowerWellDesc,
    pub count: u8,
}

unsafe impl Sync for I915PowerWellDescList {}

#[repr(C)]
pub struct I915PowerDomainList {
    pub list: *const IntelDisplayPowerDomain,
    pub count: u8,
}

unsafe impl Sync for I915PowerDomainList {}

#[inline]
pub unsafe fn desc_irq_pipe_mask(desc: *const I915PowerWellDesc) -> u8 {
    unsafe { (*desc)._bitfields_16[0] & 0x0f }
}

#[inline]
pub unsafe fn desc_always_on(desc: *const I915PowerWellDesc) -> bool {
    unsafe { ((*desc)._bitfields_16[0] & 0x10) != 0 }
}

#[inline]
pub unsafe fn desc_fixed_enable_delay(desc: *const I915PowerWellDesc) -> bool {
    unsafe { ((*desc)._bitfields_16[0] & 0x20) != 0 }
}

#[inline]
pub unsafe fn desc_has_fuses(desc: *const I915PowerWellDesc) -> bool {
    unsafe { ((*desc)._bitfields_16[0] & 0x40) != 0 }
}

#[inline]
pub unsafe fn desc_is_tc_tbt(desc: *const I915PowerWellDesc) -> bool {
    unsafe { ((*desc)._bitfields_16[0] & 0x80) != 0 }
}

#[inline]
pub unsafe fn i915_power_well_instance(
    power_well: *const I915PowerWell,
) -> *const I915PowerWellInstance {
    unsafe {
        let instances = (*(*power_well).desc).instances as *const I915PowerWellInstanceList;
        (*instances).list.add((*power_well).instance_idx as usize)
    }
}

#[inline]
pub unsafe fn pw_hsw_idx(power_well: *const I915PowerWell) -> u32 {
    unsafe {
        let inst = i915_power_well_instance(power_well);
        let hsw = core::ptr::read(core::ptr::addr_of!((*inst)._anon_20_3.hsw));
        hsw.idx as u32
    }
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).info.__runtime_info.ip.ver }
    }
}

// upstream: intel_display_power_well.c lookup_power_well()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lookup_power_well(
    display: *mut IntelDisplay,
    id: I915PowerWellId,
) -> *mut I915PowerWell {
    if display.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let power_domains = core::ptr::addr_of_mut!((*display).power.domains);
        let count = (*power_domains).power_well_count;
        let wells = (*power_domains).power_wells;
        if wells.is_null() || count <= 0 {
            return core::ptr::null_mut();
        }
        for i in 0..count {
            let pw = wells.add(i as usize);
            let inst = i915_power_well_instance(pw);
            if (*inst).id == id {
                return pw;
            }
        }
        core::ptr::null_mut()
    }
}

// upstream: intel_display_power_well.c intel_power_well_is_enabled_cached()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_is_enabled_cached(
    power_well: *mut I915PowerWell,
) -> bool {
    if power_well.is_null() {
        false
    } else {
        unsafe { (*power_well).hw_enabled }
    }
}

// upstream: intel_display_power_well.c intel_power_well_is_always_on()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_is_always_on(
    power_well: *mut I915PowerWell,
) -> bool {
    if power_well.is_null() || unsafe { (*power_well).desc.is_null() } {
        false
    } else {
        unsafe { desc_always_on((*power_well).desc) }
    }
}

// upstream: intel_display_power_well.c intel_power_well_name()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_name(
    power_well: *mut I915PowerWell,
) -> *const c_char {
    if power_well.is_null() {
        core::ptr::null()
    } else {
        unsafe {
            let inst = i915_power_well_instance(power_well);
            (*inst).name
        }
    }
}

// upstream: intel_display_power_well.c intel_power_well_domains()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_domains(
    power_well: *mut I915PowerWell,
) -> *mut IntelPowerDomainMask {
    if power_well.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe { core::ptr::addr_of_mut!((*power_well).domains) }
    }
}

// upstream: intel_display_power_well.c intel_power_well_refcount()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_refcount(
    power_well: *mut I915PowerWell,
) -> i32 {
    if power_well.is_null() {
        0
    } else {
        unsafe { (*power_well).count }
    }
}

// upstream: intel_display_power_well.c intel_power_well_is_enabled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_is_enabled(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) -> bool {
    if display.is_null() || power_well.is_null() {
        return false;
    }
    unsafe {
        let desc = (*power_well).desc;
        if desc.is_null() || (*desc).ops.is_null() {
            return false;
        }
        let ops = (*desc).ops as *const I915PowerWellOps;
        if let Some(is_enabled_fn) = (*ops).is_enabled {
            is_enabled_fn(display, power_well)
        } else {
            false
        }
    }
}

// upstream: intel_display_power_well.c intel_display_power_well_is_enabled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_well_is_enabled(
    display: *mut IntelDisplay,
    power_well_id: I915PowerWellId,
) -> bool {
    let pw = unsafe { lookup_power_well(display, power_well_id) };
    if pw.is_null() {
        false
    } else {
        unsafe { intel_power_well_is_enabled(display, pw) }
    }
}

// upstream: intel_display_power_well.c intel_power_well_sync_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_sync_hw(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    if display.is_null() || power_well.is_null() {
        return;
    }
    unsafe {
        let desc = (*power_well).desc;
        if desc.is_null() || (*desc).ops.is_null() {
            return;
        }
        let ops = (*desc).ops as *const I915PowerWellOps;
        if let Some(sync_fn) = (*ops).sync_hw {
            sync_fn(display, power_well);
        }
        (*power_well).hw_enabled = intel_power_well_is_enabled(display, power_well);
    }
}

// upstream: intel_display_power_well.c intel_power_well_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_enable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    if display.is_null() || power_well.is_null() {
        return;
    }
    unsafe {
        let desc = (*power_well).desc;
        if desc.is_null() || (*desc).ops.is_null() {
            return;
        }
        let ops = (*desc).ops as *const I915PowerWellOps;
        if let Some(enable_fn) = (*ops).enable {
            enable_fn(display, power_well);
        }
        (*power_well).hw_enabled = true;
    }
}

// upstream: intel_display_power_well.c intel_power_well_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_disable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    if display.is_null() || power_well.is_null() {
        return;
    }
    unsafe {
        let desc = (*power_well).desc;
        if desc.is_null() || (*desc).ops.is_null() {
            return;
        }
        let ops = (*desc).ops as *const I915PowerWellOps;
        if let Some(disable_fn) = (*ops).disable {
            disable_fn(display, power_well);
        }
        (*power_well).hw_enabled = false;
    }
}

// upstream: intel_display_power_well.c intel_power_well_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_get(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    if display.is_null() || power_well.is_null() {
        return;
    }
    unsafe {
        let count = (*power_well).count;
        (*power_well).count += 1;
        if count == 0 {
            intel_power_well_enable(display, power_well);
        }
    }
}

// upstream: intel_display_power_well.c intel_power_well_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_power_well_put(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    if display.is_null() || power_well.is_null() {
        return;
    }
    unsafe {
        if (*power_well).count > 0 {
            (*power_well).count -= 1;
            if (*power_well).count == 0 {
                intel_power_well_disable(display, power_well);
            }
        }
    }
}

// --- HSW / ICL / TGL / XELPD power well ops ---

static HSW_POWER_WELL_REGS: I915PowerWellRegs = I915PowerWellRegs {
    bios: HSW_PWR_WELL_CTL1,
    driver: HSW_PWR_WELL_CTL2,
    kvmr: HSW_PWR_WELL_CTL3,
    debug: HSW_PWR_WELL_CTL4,
};

static ICL_AUX_POWER_WELL_REGS: I915PowerWellRegs = I915PowerWellRegs {
    bios: ICL_PWR_WELL_CTL_AUX1,
    driver: ICL_PWR_WELL_CTL_AUX2,
    kvmr: mmio(0),
    debug: ICL_PWR_WELL_CTL_AUX4,
};

static ICL_DDI_POWER_WELL_REGS: I915PowerWellRegs = I915PowerWellRegs {
    bios: ICL_PWR_WELL_CTL_DDI1,
    driver: ICL_PWR_WELL_CTL_DDI2,
    kvmr: mmio(0),
    debug: ICL_PWR_WELL_CTL_DDI4,
};

unsafe fn hsw_power_well_requesters(
    display: *mut IntelDisplay,
    regs: *const I915PowerWellRegs,
    pw_idx: u32,
) -> u32 {
    let req_mask = HSW_PWR_WELL_CTL_REQ(pw_idx);
    unsafe {
        let mut ret = if (intel_de_read(display, (*regs).bios) & req_mask) != 0 { 1 } else { 0 };
        if (intel_de_read(display, (*regs).driver) & req_mask) != 0 { ret |= 2; }
        if (*regs).kvmr.reg != 0 && (intel_de_read(display, (*regs).kvmr) & req_mask) != 0 { ret |= 4; }
        if (*regs).debug.reg != 0 && (intel_de_read(display, (*regs).debug) & req_mask) != 0 { ret |= 8; }
        ret
    }
}

unsafe fn hsw_wait_for_power_well_enable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
    _timeout_expected: bool,
) {
    let ops = unsafe { (*(*power_well).desc).ops as *const I915PowerWellOps };
    let regs = unsafe { (*ops).regs };
    if regs.is_null() { return; }
    let pw_idx = unsafe { pw_hsw_idx(power_well) };
    let timeout: u32 = unsafe {
        let to = (*(*power_well).desc).enable_timeout;
        if to != 0 { to as u32 } else { 1 }
    };
    unsafe {
        intel_de_wait_for_set_ms(display, (*regs).driver, HSW_PWR_WELL_CTL_STATE(pw_idx), timeout);
    }
}

unsafe fn hsw_wait_for_power_well_disable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    let ops = unsafe { (*(*power_well).desc).ops as *const I915PowerWellOps };
    let regs = unsafe { (*ops).regs };
    if regs.is_null() { return; }
    let pw_idx = unsafe { pw_hsw_idx(power_well) };
    let reqs = unsafe { hsw_power_well_requesters(display, regs, pw_idx) };
    let timeout: u32 = if reqs != 0 { 0 } else { 1 };
    unsafe {
        intel_de_wait_for_clear_ms(display, (*regs).driver, HSW_PWR_WELL_CTL_STATE(pw_idx), timeout);
    }
}

unsafe fn gen9_wait_for_power_well_fuses(display: *mut IntelDisplay, pg: u32) {
    let mask = 1u32 << (27 - pg);
    unsafe {
        intel_de_wait_for_set_ms(display, SKL_FUSE_STATUS, mask, 1);
    }
}

unsafe fn pw_idx_to_pg(display: *mut IntelDisplay, pw_idx: i32) -> u32 {
    let pw1_idx = if unsafe { display_ver(display) } >= 11 {
        ICL_PW_CTL_IDX_PW_1 as i32
    } else {
        SKL_PW_CTL_IDX_PW_1 as i32
    };
    (pw_idx - pw1_idx + 1) as u32
}

unsafe extern "C" fn hsw_power_well_enable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    let ops = unsafe { (*(*power_well).desc).ops as *const I915PowerWellOps };
    let regs = unsafe { (*ops).regs };
    if regs.is_null() { return; }
    let pw_idx = unsafe { pw_hsw_idx(power_well) };

    unsafe {
        if desc_has_fuses((*power_well).desc) {
            let pg = pw_idx_to_pg(display, pw_idx as i32);
            let is_adlp = (*display).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0;
            if is_adlp && pg == 1 {
                intel_de_rmw(display, GEN8_CHICKEN_DCPR_1, 0, 1 << 15);
            }
            if pg == 1 {
                gen9_wait_for_power_well_fuses(display, 0);
            }
        }

        intel_de_rmw(display, (*regs).driver, 0, HSW_PWR_WELL_CTL_REQ(pw_idx));
        hsw_wait_for_power_well_enable(display, power_well, false);

        if desc_has_fuses((*power_well).desc) {
            let pg = pw_idx_to_pg(display, pw_idx as i32);
            gen9_wait_for_power_well_fuses(display, pg);
        }

        let irq_mask = desc_irq_pipe_mask((*power_well).desc);
        if irq_mask != 0 {
            gen8_irq_power_well_post_enable(display, irq_mask);
        }
    }
}

unsafe extern "C" fn hsw_power_well_disable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    let ops = unsafe { (*(*power_well).desc).ops as *const I915PowerWellOps };
    let regs = unsafe { (*ops).regs };
    if regs.is_null() { return; }
    let pw_idx = unsafe { pw_hsw_idx(power_well) };

    unsafe {
        let irq_mask = desc_irq_pipe_mask((*power_well).desc);
        if irq_mask != 0 {
            gen8_irq_power_well_pre_disable(display, irq_mask);
        }

        intel_de_rmw(display, (*regs).driver, HSW_PWR_WELL_CTL_REQ(pw_idx), 0);
        hsw_wait_for_power_well_disable(display, power_well);
    }
}

unsafe extern "C" fn hsw_power_well_sync_hw(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    let ops = unsafe { (*(*power_well).desc).ops as *const I915PowerWellOps };
    let regs = unsafe { (*ops).regs };
    if regs.is_null() { return; }
    let pw_idx = unsafe { pw_hsw_idx(power_well) };
    let mask = HSW_PWR_WELL_CTL_REQ(pw_idx);
    unsafe {
        let bios_req = intel_de_read(display, (*regs).bios);
        if (bios_req & mask) != 0 {
            let drv_req = intel_de_read(display, (*regs).driver);
            if (drv_req & mask) == 0 {
                intel_de_write(display, (*regs).driver, drv_req | mask);
            }
            intel_de_write(display, (*regs).bios, bios_req & !mask);
        }
    }
}

unsafe extern "C" fn hsw_power_well_enabled(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) -> bool {
    let ops = unsafe { (*(*power_well).desc).ops as *const I915PowerWellOps };
    let regs = unsafe { (*ops).regs };
    if regs.is_null() { return false; }
    let pw_idx = unsafe { pw_hsw_idx(power_well) };
    let mask = HSW_PWR_WELL_CTL_REQ(pw_idx) | HSW_PWR_WELL_CTL_STATE(pw_idx);
    unsafe {
        let val = intel_de_read(display, (*regs).driver);
        (val & mask) == mask
    }
}

unsafe extern "C" fn icl_aux_power_well_enable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    hsw_power_well_enable(display, power_well);
}

unsafe extern "C" fn icl_aux_power_well_disable(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    hsw_power_well_disable(display, power_well);
}

unsafe extern "C" fn gen9_dc_off_power_well_enable(
    display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) {
    gen9_disable_dc_states(display);
}

unsafe extern "C" fn gen9_dc_off_power_well_disable(
    display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) {
    if !unsafe { intel_dmc_has_payload(display) } {
        return;
    }
    unsafe {
        let target = (*display).power.domains.target_dc_state;
        if target == DC_STATE_EN_UPTO_DC6 {
            skl_enable_dc6(display);
        } else if target == DC_STATE_EN_UPTO_DC5 {
            gen9_enable_dc5(display);
        }
    }
}

unsafe extern "C" fn gen9_dc_off_power_well_enabled(
    display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) -> bool {
    unsafe {
        let val = intel_de_read(display, DC_STATE_EN);
        (val & DC_STATE_EN_DC3CO) == 0 && (val & DC_STATE_EN_UPTO_DC5_DC6_MASK) == 0
    }
}

unsafe fn tgl_tc_cold_request(display: *mut IntelDisplay, block: bool) {
    let mut tries: u8 = 0;
    loop {
        let mut low_val: u32 = if block { 0 } else { 1 };
        let mut high_val: u32 = 0;
        let ret = unsafe {
            intel_parent_pcode_read(display, 0x26 /* TGL_PCODE_TCCOLD */, &mut low_val, &mut high_val)
        };
        if ret == 0 {
            if block && (low_val & 1) != 0 {
                // pcode exit failed
            } else {
                break;
            }
        }
        tries += 1;
        if tries == 3 {
            break;
        }
    }
}

unsafe extern "C" fn tgl_tc_cold_off_power_well_enable(
    display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) {
    unsafe { tgl_tc_cold_request(display, true); }
}

unsafe extern "C" fn tgl_tc_cold_off_power_well_disable(
    display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) {
    unsafe { tgl_tc_cold_request(display, false); }
}

unsafe extern "C" fn tgl_tc_cold_off_power_well_sync_hw(
    display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) {
    if unsafe { intel_power_well_refcount(power_well) } > 0 {
        unsafe { tgl_tc_cold_off_power_well_enable(display, power_well); }
    } else {
        unsafe { tgl_tc_cold_off_power_well_disable(display, power_well); }
    }
}

unsafe extern "C" fn tgl_tc_cold_off_power_well_is_enabled(
    _display: *mut IntelDisplay,
    power_well: *mut I915PowerWell,
) -> bool {
    unsafe { intel_power_well_refcount(power_well) > 0 }
}

unsafe extern "C" fn i9xx_power_well_sync_hw_noop(
    _display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) {}

unsafe extern "C" fn i9xx_always_on_power_well_noop(
    _display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) {}

unsafe extern "C" fn i9xx_always_on_power_well_enabled(
    _display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) -> bool {
    true
}

unsafe extern "C" fn non_display_13_power_well_disabled(
    _display: *mut IntelDisplay,
    _power_well: *mut I915PowerWell,
) -> bool {
    false
}

// --- Exported power well ops structs ---

pub static i9xx_always_on_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(i9xx_always_on_power_well_enabled),
};

pub static hsw_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::addr_of!(HSW_POWER_WELL_REGS),
    sync_hw: Some(hsw_power_well_sync_hw),
    enable: Some(hsw_power_well_enable),
    disable: Some(hsw_power_well_disable),
    is_enabled: Some(hsw_power_well_enabled),
};

pub static gen9_dc_off_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(gen9_dc_off_power_well_enable),
    disable: Some(gen9_dc_off_power_well_disable),
    is_enabled: Some(gen9_dc_off_power_well_enabled),
};

pub static icl_aux_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::addr_of!(ICL_AUX_POWER_WELL_REGS),
    sync_hw: Some(hsw_power_well_sync_hw),
    enable: Some(icl_aux_power_well_enable),
    disable: Some(icl_aux_power_well_disable),
    is_enabled: Some(hsw_power_well_enabled),
};

pub static icl_ddi_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::addr_of!(ICL_DDI_POWER_WELL_REGS),
    sync_hw: Some(hsw_power_well_sync_hw),
    enable: Some(hsw_power_well_enable),
    disable: Some(hsw_power_well_disable),
    is_enabled: Some(hsw_power_well_enabled),
};

pub static tgl_tc_cold_off_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(tgl_tc_cold_off_power_well_sync_hw),
    enable: Some(tgl_tc_cold_off_power_well_enable),
    disable: Some(tgl_tc_cold_off_power_well_disable),
    is_enabled: Some(tgl_tc_cold_off_power_well_is_enabled),
};

// Platform stubs (non-Display-13)
pub static chv_pipe_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static chv_dpio_cmn_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static i830_pipes_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static bxt_dpio_cmn_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static vlv_display_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static vlv_dpio_cmn_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static vlv_dpio_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static xelpdp_aux_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

pub static xe2lpd_pica_power_well_ops: I915PowerWellOps = I915PowerWellOps {
    regs: core::ptr::null(),
    sync_hw: Some(i9xx_power_well_sync_hw_noop),
    enable: Some(i9xx_always_on_power_well_noop),
    disable: Some(i9xx_always_on_power_well_noop),
    is_enabled: Some(non_display_13_power_well_disabled),
};

// --- DC state functions ---

// upstream: intel_display_power_well.c gen9_set_dc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen9_set_dc_state(display: *mut IntelDisplay, state: u32) {
    if display.is_null() || !unsafe { intel_display_device_enabled(display) } {
        return;
    }
    unsafe {
        let mask = DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC5_DC6_MASK;
        let mut val = intel_de_read(display, DC_STATE_EN);
        val &= !mask;
        val |= state;
        intel_de_write(display, DC_STATE_EN, val);
        (*display).power.domains.dc_state = state;
    }
}

// upstream: intel_display_power_well.c gen9_sanitize_dc_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen9_sanitize_dc_state(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        let val = intel_de_read(display, DC_STATE_EN);
        let mask = DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC5_DC6_MASK;
        (*display).power.domains.dc_state = val & mask;
    }
}

// upstream: intel_display_power_well.c gen9_disable_dc_states()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen9_disable_dc_states(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        if intel_display_device_enabled(display) {
            intel_dmc_wl_get_noreg(display);
            gen9_set_dc_state(display, DC_STATE_DISABLE);
            intel_dmc_wl_put_noreg(display);
        } else {
            gen9_set_dc_state(display, DC_STATE_DISABLE);
        }
    }
}

// upstream: intel_display_power_well.c gen9_enable_dc5()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen9_enable_dc5(display: *mut IntelDisplay) {
    unsafe { gen9_set_dc_state(display, DC_STATE_EN_UPTO_DC5); }
}

// upstream: intel_display_power_well.c skl_enable_dc6()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_enable_dc6(display: *mut IntelDisplay) {
    unsafe { gen9_set_dc_state(display, DC_STATE_EN_UPTO_DC6); }
}

// upstream: intel_display_power_well.c bxt_enable_dc9()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bxt_enable_dc9(_display: *mut IntelDisplay) {
    // skipped: not display 13
}

// upstream: intel_display_power_well.c bxt_disable_dc9()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bxt_disable_dc9(_display: *mut IntelDisplay) {
    // skipped: not display 13
}

// upstream: intel_display_power_well.c chv_phy_powergate_lanes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chv_phy_powergate_lanes(
    _encoder: *mut c_void,
    _override_val: bool,
    _mask: u32,
) {
    // skipped: not display 13
}

// upstream: intel_display_power_well.c chv_phy_powergate_ch()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chv_phy_powergate_ch(
    _display: *mut IntelDisplay,
    _phy: u32,
    _ch: u32,
    _override_val: bool,
) -> bool {
    // skipped: not display 13
    false
}
