// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_rps.c.
// The full MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, unsafe_op_in_unsafe_fn, non_snake_case, dead_code)]

use core::{
    ffi::{c_long, c_ulong, c_void},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    i915_request_types_upstream::I915Request,
    intel_engine_cs_upstream::{IntelEngineCs, Mutex, Spinlock, TimerList, WorkStruct},
    intel_engine_types_upstream::{IntelEngineId, VECS0},
    intel_gt_clock_utils_upstream::{
        intel_gt_check_clock_frequency, intel_gt_ns_to_pm_interval, intel_gt_pm_interval_to_ns,
    },
    intel_gt_pm_irq_upstream::{
        gen6_gt_pm_disable_irq, gen6_gt_pm_enable_irq, gen6_gt_pm_mask_irq, gen6_gt_pm_reset_iir,
        gen6_gt_pm_unmask_irq,
    },
    intel_gt_types_upstream::{GT_MEDIA, IntelGt},
    intel_guc_slpc_types_upstream::IntelGucSlpc,
    intel_rps_types_upstream::{
        BETWEEN, HIGH_POWER, IntelIps, IntelRps, IntelRpsEi, IntelRpsFreqCaps, LOW_POWER,
    },
    intel_runtime_pm_upstream::{intel_runtime_pm_get_if_in_use, intel_runtime_pm_put_raw},
    intel_uc_types_upstream::{intel_uc_uses_guc_slpc, intel_uc_uses_guc_submission},
    intel_uncore_types_upstream::{
        FORCEWAKE_ALL, FORCEWAKE_MEDIA, IntelUncore, intel_uncore_forcewake_get,
        intel_uncore_forcewake_put, intel_uncore_posting_read, intel_uncore_posting_read16,
        intel_uncore_read, intel_uncore_read_fw, intel_uncore_read8, intel_uncore_read16,
        intel_uncore_rmw, intel_uncore_write, intel_uncore_write_fw, intel_uncore_write8,
        intel_uncore_write16,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        bits::*, i915::*, locks::*, memory::*, mutex::*, primitives::*, rcu::*, timer::*,
        workqueue::*,
    },
    linux_i915_private::DrmI915Private,
};

static mut MCHDEV_LOCK: Spinlock = unsafe { core::mem::zeroed() };
static mut IPS_MCHDEV: *mut DrmI915Private = ptr::null_mut();
static CHV_GPLL_WARNED: AtomicBool = AtomicBool::new(false);
static CHV_ODD_FREQ_WARNED: AtomicBool = AtomicBool::new(false);

const SLPC_POWER_PROFILES_POWER_SAVING: u32 = 1;
const GT_FREQUENCY_MULTIPLIER: u32 = 50;
const GEN9_FREQ_SCALER: u32 = 3;
const BUSY_MAX_EI: u32 = 20;
const NSEC_PER_MSEC: u64 = 1_000_000;

#[inline]
fn drm_warn_once(warned: &AtomicBool, device: *mut c_void, condition: bool) {
    if condition && !warned.swap(true, Ordering::Relaxed) {
        let _ = drm_WARN_ON!(device, true);
    }
}

const fn mmio(reg: u32) -> I915RegT {
    I915RegT { reg }
}
const fn genmask(high: u32, low: u32) -> u32 {
    (u32::MAX >> (31 - high)) & (u32::MAX << low)
}
#[inline]
fn field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}
#[inline]
const fn bit(n: u32) -> u32 {
    1u32 << n
}
#[inline]
fn ktime_sub(a: i64, b: i64) -> i64 {
    a.wrapping_sub(b)
}
#[inline]
fn ktime_to_ns(a: i64) -> i64 {
    a
}
#[inline]
fn div_u64(a: u64, b: u64) -> u64 {
    a / b
}
#[inline]
fn div64_u64(a: u64, b: u64) -> u64 {
    a / b
}
#[inline]
fn div_round_closest(a: i64, b: i64) -> i64 {
    if a >= 0 {
        (a + b / 2) / b
    } else {
        (a - b / 2) / b
    }
}

// RPS MMIO and sideband constants from the source GT, MCHBAR and PUNIT headers.
const GEN6_RPNSWREQ: I915RegT = mmio(0xa008);
const GEN6_RC_VIDEO_FREQ: I915RegT = mmio(0xa00c);
const GEN6_RP_DOWN_TIMEOUT: I915RegT = mmio(0xa010);
const GEN6_RP_INTERRUPT_LIMITS: I915RegT = mmio(0xa014);
const GEN6_RPSTAT1: I915RegT = mmio(0xa01c);
const GEN6_RP_CONTROL: I915RegT = mmio(0xa024);
const GEN6_RP_UP_THRESHOLD: I915RegT = mmio(0xa02c);
const GEN6_RP_DOWN_THRESHOLD: I915RegT = mmio(0xa030);
const GEN6_RP_CUR_UP_EI: I915RegT = mmio(0xa050);
const GEN6_RP_CUR_UP: I915RegT = mmio(0xa054);
const GEN6_RP_PREV_UP: I915RegT = mmio(0xa058);
const GEN6_RP_CUR_DOWN_EI: I915RegT = mmio(0xa05c);
const GEN6_RP_CUR_DOWN: I915RegT = mmio(0xa060);
const GEN6_RP_PREV_DOWN: I915RegT = mmio(0xa064);
const GEN6_RP_UP_EI: I915RegT = mmio(0xa068);
const GEN6_RP_DOWN_EI: I915RegT = mmio(0xa06c);
const GEN6_RP_IDLE_HYSTERSIS: I915RegT = mmio(0xa070);
const GEN6_PMINTRMSK: I915RegT = mmio(0xa168);
const GEN6_PMIER: I915RegT = mmio(0x4402c);
const GEN6_PMISR: I915RegT = mmio(0x44020);
const GEN6_PMIMR: I915RegT = mmio(0x44024);
const GEN6_PMIIR: I915RegT = mmio(0x44028);
const GEN8_GT_ISR_BASE: u32 = 0x44300;
const GEN8_GT_IMR_BASE: u32 = 0x44304;
const GEN8_GT_IIR_BASE: u32 = 0x44308;
const GEN8_GT_IER_BASE: u32 = 0x4430c;
const BXT_RP_STATE_CAP: I915RegT = mmio(0x138170);
const GEN6_RP_STATE_CAP: I915RegT = mmio(0x145998);
const GEN6_RP_STATE_LIMITS: I915RegT = mmio(0x145994);
const GEN6_GT_PERF_STATUS: I915RegT = mmio(0x145948);
const BXT_GT_PERF_STATUS: I915RegT = mmio(0x147070);
const GEN10_FREQ_INFO_REC: I915RegT = mmio(0x145ef0);
const GEN10_FREQ_INFO_REC_RPE_MASK: u32 = genmask(15, 8);
const RPE_MASK: u32 = genmask(15, 8);
const GEN6_CAGF_MASK: u32 = genmask(14, 8);
const HSW_CAGF_MASK: u32 = genmask(13, 7);
const GEN9_CAGF_MASK: u32 = genmask(31, 23);
const GEN12_RPSTAT1: I915RegT = mmio(0x1381b4);
const GEN12_CAGF_MASK: u32 = genmask(19, 11);
const MTL_MIRROR_TARGET_WP1: I915RegT = mmio(0xc60);
const MTL_CAGF_MASK: u32 = genmask(8, 0);
const MTL_RP_STATE_CAP: I915RegT = mmio(0x138000);
const MTL_MEDIAP_STATE_CAP: I915RegT = mmio(0x138020);
const MTL_GT_RPE_FREQUENCY: I915RegT = mmio(0x13800c);
const MTL_MPE_FREQUENCY: I915RegT = mmio(0x13802c);
const MTL_RP0_CAP_MASK: u32 = genmask(8, 0);
const MTL_RPN_CAP_MASK: u32 = genmask(24, 16);
const MTL_RPE_MASK: u32 = genmask(8, 0);
const GEN6_PM_RP_DOWN_TIMEOUT: u32 = bit(6);
const GEN6_PM_RP_UP_THRESHOLD: u32 = bit(5);
const GEN6_PM_RP_DOWN_THRESHOLD: u32 = bit(4);
const GEN6_PM_RP_UP_EI_EXPIRED: u32 = bit(2);
const GEN6_PM_RPS_EVENTS: u32 = GEN6_PM_RP_DOWN_TIMEOUT
    | GEN6_PM_RP_UP_THRESHOLD
    | GEN6_PM_RP_DOWN_THRESHOLD
    | GEN6_PM_RP_UP_EI_EXPIRED
    | bit(1);
const GEN8_PMINTR_DISABLE_REDIRECT_TO_GUC: u32 = bit(31);
const ARAT_EXPIRED_INTRMSK: u32 = bit(9);
const GEN6_RP_MEDIA_TURBO: u32 = bit(11);
const GEN6_RP_MEDIA_MODE_MASK: u32 = 3 << 9;
const GEN6_RP_MEDIA_HW_NORMAL_MODE: u32 = 2 << 9;
const GEN6_RP_MEDIA_SW_MODE: u32 = 0;
const GEN6_RP_MEDIA_IS_GFX: u32 = bit(8);
const GEN6_RP_ENABLE: u32 = bit(7);
const GEN6_RP_UP_BUSY_AVG: u32 = 2 << 3;
const GEN6_RP_DOWN_IDLE_AVG: u32 = 2;
const GEN6_RP_DOWN_IDLE_CONT: u32 = 1;
const GEN9_RPSWCTL_ENABLE: u32 = 2 << 9;
const GEN9_RPSWCTL_DISABLE: u32 = 0;
const GEN6_TURBO_DISABLE: u32 = bit(31);
const GEN9_SW_REQ_UNSLICE_RATIO_SHIFT: u32 = 23;
const GEN9_IGNORE_SLICE_RATIO: u32 = 0;
const GEN6_CURICONT_MASK: u32 = 0x00ff_ffff;
const GEN6_CURBSYTAVG_MASK: u32 = 0x00ff_ffff;
const GEN6_CURIAVG_MASK: u32 = 0x00ff_ffff;
const GEN6_RP_EI_MASK: u32 = 0x00ff_ffff;
const MTL_GEN11_GPM_WGBOX_PERF_INTR_ENABLE: I915RegT = mmio(0x19003c);
const GEN11_GPM_WGBOXPERF_INTR_ENABLE: I915RegT = mmio(0x19003c);
const GEN11_GPM_WGBOXPERF_INTR_MASK: I915RegT = mmio(0x1900ec);
const GEN11_GTPM: u32 = 16;
const GEN6_READ_OC_PARAMS: u32 = 0x0c;
const HSW_PCODE_DYNAMIC_DUTY_CYCLE_CONTROL: u32 = 0x1a;
const PXVFREQ_PX_MASK: u32 = 0x7f00_0000;
const PXVFREQ_PX_SHIFT: u32 = 24;
const GEN6_FREQUENCY_SHIFT: u32 = 25;
const HSW_FREQUENCY_SHIFT: u32 = 24;
const GEN9_FREQUENCY_SHIFT: u32 = 23;
const GEN6_OFFSET_SHIFT: u32 = 19;
const GEN6_AGGRESSIVE_TURBO: u32 = 0;
const MEMSWCTL: I915RegT = mmio(0x11170);
const MEMIHYST: I915RegT = mmio(0x1117c);
const MEMINTREN: I915RegT = mmio(0x11180);
const MEMINTRSTS: I915RegT = mmio(0x11184);
const MEMMODECTL: I915RegT = mmio(0x11190);
const RCBMAXAVG: I915RegT = mmio(0x1119c);
const RCBMINAVG: I915RegT = mmio(0x111a0);
const RCUPEI: I915RegT = mmio(0x111b0);
const RCDNEI: I915RegT = mmio(0x111b4);
const VIDSTART: I915RegT = mmio(0x111cc);
const MEMSTAT_ILK: I915RegT = mmio(0x111f8);
const PMMISC: I915RegT = mmio(0x11214);
const SDEW: I915RegT = mmio(0x1124c);
const CSIEW0: I915RegT = mmio(0x11250);
const CSIEW1: I915RegT = mmio(0x11254);
const CSIEW2: I915RegT = mmio(0x11258);
const CSIEC: I915RegT = mmio(0x112e0);
const DMIEC: I915RegT = mmio(0x112e4);
const DDREC: I915RegT = mmio(0x112e8);
const GFXEC: I915RegT = mmio(0x112f4);
const RCPREVBSYTUPAVG: I915RegT = mmio(0x113b8);
const RCPREVBSYTDNAVG: I915RegT = mmio(0x113bc);
const ECR: I915RegT = mmio(0x11600);
const OGW0: I915RegT = mmio(0x11608);
const OGW1: I915RegT = mmio(0x1160c);
const EG0: I915RegT = mmio(0x11610);
const EG1: I915RegT = mmio(0x11614);
const EG2: I915RegT = mmio(0x11618);
const EG3: I915RegT = mmio(0x1161c);
const EG4: I915RegT = mmio(0x11620);
const EG5: I915RegT = mmio(0x11624);
const EG6: I915RegT = mmio(0x11628);
const EG7: I915RegT = mmio(0x1162c);
const LCFUSE02: I915RegT = mmio(0x116c0);
const LCFUSE_HIV_MASK: u32 = 0xff;
const VLV_RENDER_C0_COUNT: I915RegT = mmio(0x138118);
const VLV_MEDIA_C0_COUNT: I915RegT = mmio(0x13811c);
const MEMCTL_CMD_STS: u32 = bit(12);
const MEMCTL_CMD_SHIFT: u32 = 13;
const MEMCTL_CMD_CHFREQ: u32 = 2;
const MEMCTL_FREQ_SHIFT: u32 = 8;
const MEMCTL_SFCAVM: u32 = bit(7);
const MEMINT_CX_SUPR_EN: u32 = bit(7);
const MEMINT_EVAL_CHG_EN: u32 = bit(4);
const MEMINT_EVAL_CHG: u32 = bit(4);
const MEMMODE_SWMODE_EN: u32 = bit(14);
const MEMMODE_FSTART_MASK: u32 = 0x0f00;
const MEMMODE_FSTART_SHIFT: u32 = 8;
const MEMMODE_FMAX_MASK: u32 = 0x00f0;
const MEMMODE_FMIN_MASK: u32 = 0x000f;
const MEMMODE_FMAX_SHIFT: u32 = 4;
const MCPPCE_EN: u32 = 1;
const TSE: u32 = 1;
const MEMSTAT_PSTATE_MASK: u32 = genmask(7, 3);
const HSW_FREQ_SHIFT: u32 = 24;
const PM_VEBOX_CS_ERROR_INTERRUPT: u32 = bit(12);
const PM_VEBOX_USER_INTERRUPT: u32 = bit(10);
const GEN6_PM_RP_UP_EI_EXPIRED_CONST: u32 = bit(2);
const GEN9_TURBO_DISABLE: u32 = bit(31);
const VLV_IOSF_SB_CCK: u32 = 1;
const VLV_IOSF_SB_NC: u32 = 7;
const VLV_IOSF_SB_PUNIT: u32 = 8;
const PUNIT_REG_GPU_LFM: u32 = 0xd3;
const PUNIT_REG_GPU_FREQ_REQ: u32 = 0xd4;
const PUNIT_REG_GPU_FREQ_STS: u32 = 0xd8;
const GPLLENABLE: u32 = bit(4);
const FB_GFX_FMAX_AT_VMAX_FUSE: u32 = 0x136;
const FB_GFX_FREQ_FUSE_MASK: u32 = 0xff;
const FB_GFX_FMAX_AT_VMAX_2SS4EU_FUSE_SHIFT: u32 = 24;
const FB_GFX_FMAX_AT_VMAX_2SS6EU_FUSE_SHIFT: u32 = 16;
const FB_GFX_FMAX_AT_VMAX_2SS8EU_FUSE_SHIFT: u32 = 8;
const FB_GFX_FMIN_AT_VMIN_FUSE: u32 = 0x137;
const FB_GFX_FMIN_AT_VMIN_FUSE_SHIFT: u32 = 8;
const PUNIT_GPU_DUTYCYCLE_REG: u32 = 0xdf;
const PUNIT_GPU_DUTYCYCLE_RPE_FREQ_SHIFT: u32 = 8;
const PUNIT_GPU_DUTYCYCLE_RPE_FREQ_MASK: u32 = 0xff;
const IOSF_NC_FB_GFX_FREQ_FUSE: u32 = 0x1c;
const FB_GFX_MAX_FREQ_FUSE_SHIFT: u32 = 3;
const FB_GFX_MAX_FREQ_FUSE_MASK: u32 = 0x7f8;
const FB_GFX_FGUARANTEED_FREQ_FUSE_SHIFT: u32 = 11;
const FB_GFX_FGUARANTEED_FREQ_FUSE_MASK: u32 = 0x7f800;
const IOSF_NC_FB_GFX_FMAX_FUSE_HI: u32 = 0x34;
const FB_FMAX_VMIN_FREQ_HI_MASK: u32 = 7;
const IOSF_NC_FB_GFX_FMAX_FUSE_LO: u32 = 0x30;
const FB_FMAX_VMIN_FREQ_LO_SHIFT: u32 = 27;
const FB_FMAX_VMIN_FREQ_LO_MASK: u32 = 0xf8000000;
const VLV_TURBO_SOC_OVERRIDE: u32 = 4;
const VLV_OVERRIDE_EN: u32 = 1;
const VLV_SOC_TDP_EN: u32 = 2;
const VLV_BIAS_CPU_125_SOC_875: u32 = 6 << 2;
const CHV_BIAS_CPU_50_SOC_50: u32 = 3 << 2;
const TSFS: I915RegT = mmio(0x11020);
const TR1: I915RegT = mmio(0x11006);
const TSFS_INTR_MASK: u32 = 0xff;
const TSFS_SLOPE_MASK: u32 = 0xff00;
const TSFS_SLOPE_SHIFT: u32 = 8;
const TSC1: I915RegT = mmio(0x11001);
const IOSF_NC_FB_GFX_FREQ_FUSE_CONST: u32 = 0x1c;

#[inline]
fn reg_gen8_gt_isr(n: u32) -> I915RegT {
    mmio(GEN8_GT_ISR_BASE + 0x10 * n)
}
#[inline]
fn reg_gen8_gt_imr(n: u32) -> I915RegT {
    mmio(GEN8_GT_IMR_BASE + 0x10 * n)
}
#[inline]
fn reg_gen8_gt_iir(n: u32) -> I915RegT {
    mmio(GEN8_GT_IIR_BASE + 0x10 * n)
}
#[inline]
fn reg_gen8_gt_ier(n: u32) -> I915RegT {
    mmio(GEN8_GT_IER_BASE + 0x10 * n)
}
#[inline]
fn pxvfreq(i: u32) -> I915RegT {
    mmio(0x11110 + 4 * i)
}
#[inline]
fn pew(i: u32) -> I915RegT {
    mmio(0x1125c + 4 * i)
}
#[inline]
fn dew(i: u32) -> I915RegT {
    mmio(0x11270 + 4 * i)
}
#[inline]
fn pxw(i: u32) -> I915RegT {
    mmio(0x11664 + 4 * i)
}
#[inline]
fn pxwl(i: u32) -> I915RegT {
    mmio(0x11680 + 8 * i)
}

unsafe fn is_mobile(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[0] & 1 != 0 }
}

unsafe fn is_gen9_bc(i915: *mut DrmI915Private) -> bool {
    GRAPHICS_VER(i915) == 9 && !unsafe { IS_GEN9_LP(i915) }
}

unsafe fn has_rps(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[3] & (1 << 6) != 0 }
}

unsafe fn is_ironlake_mobile(i915: *mut DrmI915Private) -> bool {
    GRAPHICS_VER(i915) == 5 && unsafe { is_mobile(i915) }
}

// upstream: intel_rps.c rps_to_gt()
unsafe fn rps_to_gt(rps: *mut IntelRps) -> *mut IntelGt {
    container_of!(rps, IntelGt, rps)
}
// upstream: intel_rps.c rps_to_i915()
unsafe fn rps_to_i915(rps: *mut IntelRps) -> *mut DrmI915Private {
    unsafe { (*rps_to_gt(rps)).i915 }
}
// upstream: intel_rps.c rps_to_uncore()
unsafe fn rps_to_uncore(rps: *mut IntelRps) -> *mut IntelUncore {
    unsafe { (*rps_to_gt(rps)).uncore }
}
// upstream: intel_rps.c rps_to_slpc()
unsafe fn rps_to_slpc(rps: *mut IntelRps) -> *mut IntelGucSlpc {
    let gt = unsafe { rps_to_gt(rps) };
    unsafe { ptr::addr_of_mut!((*gt).uc.guc.slpc) }
}
// upstream: intel_rps.c rps_uses_slpc()
unsafe fn rps_uses_slpc(rps: *mut IntelRps) -> bool {
    let gt = unsafe { rps_to_gt(rps) };
    unsafe { intel_uc_uses_guc_slpc(ptr::addr_of_mut!((*gt).uc)) }
}

#[inline]
unsafe fn rd(u: *mut IntelUncore, r: I915RegT) -> u32 {
    unsafe { intel_uncore_read(u, r) }
}
#[inline]
unsafe fn rd_fw(u: *const IntelUncore, r: I915RegT) -> u32 {
    unsafe { intel_uncore_read_fw(u, r) }
}
#[inline]
unsafe fn wr(u: *mut IntelUncore, r: I915RegT, v: u32) {
    unsafe { intel_uncore_write(u, r, v) }
}
#[inline]
unsafe fn wr_fw(u: *const IntelUncore, r: I915RegT, v: u32) {
    unsafe { intel_uncore_write_fw(u, r, v) }
}
#[inline]
unsafe fn wr16(u: *mut IntelUncore, r: I915RegT, v: u16) {
    unsafe { intel_uncore_write16(u, r, v) }
}
#[inline]
unsafe fn rd16(u: *mut IntelUncore, r: I915RegT) -> u16 {
    unsafe { intel_uncore_read16(u, r) }
}
#[inline]
unsafe fn fw_get(u: *mut IntelUncore, d: i32) {
    unsafe { intel_uncore_forcewake_get(u, d) }
}
#[inline]
unsafe fn fw_put(u: *mut IntelUncore, d: i32) {
    unsafe { intel_uncore_forcewake_put(u, d) }
}
#[inline]
unsafe fn rps_is_enabled(r: *const IntelRps) -> bool {
    unsafe { (*r).flags & (1 << 0) != 0 }
}
#[inline]
unsafe fn rps_is_active(r: *const IntelRps) -> bool {
    unsafe { (*r).flags & (1 << 1) != 0 }
}
#[inline]
unsafe fn rps_has_interrupts(r: *const IntelRps) -> bool {
    unsafe { (*r).flags & (1 << 2) != 0 }
}
#[inline]
unsafe fn rps_uses_timer(r: *const IntelRps) -> bool {
    unsafe { (*r).flags & (1 << 3) != 0 }
}
#[inline]
unsafe fn rps_set_enabled(r: *mut IntelRps) {
    unsafe { (*r).flags |= 1 << 0 }
}
#[inline]
unsafe fn rps_clear_enabled(r: *mut IntelRps) {
    unsafe { (*r).flags &= !(1 << 0) }
}
#[inline]
unsafe fn rps_set_active(r: *mut IntelRps) {
    unsafe { (*r).flags |= 1 << 1 }
}
#[inline]
unsafe fn rps_clear_active(r: *mut IntelRps) -> bool {
    let was = unsafe { rps_is_active(r) };
    unsafe { (*r).flags &= !(1 << 1) };
    was
}
#[inline]
unsafe fn rps_set_interrupts(r: *mut IntelRps) {
    unsafe { (*r).flags |= 1 << 2 }
}
#[inline]
unsafe fn rps_clear_interrupts(r: *mut IntelRps) {
    unsafe { (*r).flags &= !(1 << 2) }
}
#[inline]
unsafe fn rps_set_timer(r: *mut IntelRps) {
    unsafe { (*r).flags |= 1 << 3 }
}
#[inline]
unsafe fn rps_clear_timer(r: *mut IntelRps) {
    unsafe { (*r).flags &= !(1 << 3) }
}
#[inline]
unsafe fn mutex_lock_ptr(m: *mut Mutex) {
    unsafe { mutex_lock(m) }
}
#[inline]
unsafe fn mutex_unlock_ptr(m: *mut Mutex) {
    unsafe { mutex_unlock(m) }
}
#[inline]
unsafe fn qwork(queue: *mut c_void, work: *mut WorkStruct) {
    unsafe {
        let _ = queue_work(queue, work);
    }
}

/// Linux `intel_engine_cs_irq()` is an inline dispatch through the engine callback.
#[inline]
unsafe fn intel_engine_cs_irq(engine: *mut IntelEngineCs, iir: u16) {
    if iir != 0 {
        if let Some(handler) = unsafe { (*engine).irq_handler } {
            unsafe { handler(engine, iir) };
        }
    }
}

// upstream: intel_rps.c rps_pm_sanitize_mask()
unsafe fn rps_pm_sanitize_mask(rps: *mut IntelRps, mask: u32) -> u32 {
    mask & !unsafe { (*rps).pm_intrmsk_mbz }
}
// upstream: intel_rps.c set()
unsafe fn set(uncore: *mut IntelUncore, reg: I915RegT, val: u32) {
    unsafe { wr_fw(uncore, reg, val) }
}
// upstream: intel_rps.c rps_timer()
unsafe extern "C" fn rps_timer(timer: *mut TimerList) {
    let rps = unsafe { container_of!(timer, IntelRps, timer) };
    let gt = unsafe { rps_to_gt(rps) };
    let mut engine: *mut IntelEngineCs = ptr::null_mut();
    let mut timestamp = 0i64;
    let mut max_busy = [0i64; 3];
    for id in 0..unsafe { (*gt).info.num_engines } {
        engine = unsafe { (*gt).engine[id as usize] };
        if engine.is_null() {
            continue;
        }
        let mut now = timestamp;
        let dt = unsafe {
            crate::intel_engine_cs_upstream::intel_engine_get_busy_time(engine, &mut now)
        };
        let last = unsafe { (*engine).stats.rps };
        unsafe { (*engine).stats.rps = dt };
        timestamp = now;
        let mut busy = ktime_to_ns(ktime_sub(dt, last));
        for slot in &mut max_busy {
            if busy > *slot {
                core::mem::swap(&mut busy, slot);
            }
        }
    }
    let last = unsafe { (*rps).pm_timestamp };
    unsafe { (*rps).pm_timestamp = timestamp };
    if unsafe { rps_is_active(rps) } {
        let dt = ktime_sub(timestamp, last);
        let mut busy = max_busy[0];
        for i in 1..3 {
            if max_busy[i] == 0 {
                break;
            }
            busy += max_busy[i] / (1 << i);
        }
        if (100 * busy > unsafe { (*rps).power.up_threshold as i64 } * dt)
            && unsafe { (*rps).cur_freq < (*rps).max_freq_softlimit }
        {
            unsafe {
                (*rps).pm_iir |= GEN6_PM_RP_UP_THRESHOLD;
                (*rps).pm_interval = 1;
                qwork((*(*gt).i915).unordered_wq, ptr::addr_of_mut!((*rps).work));
            }
        } else if (100 * busy < unsafe { (*rps).power.down_threshold as i64 } * dt)
            && unsafe { (*rps).cur_freq > (*rps).min_freq_softlimit }
        {
            unsafe {
                (*rps).pm_iir |= GEN6_PM_RP_DOWN_THRESHOLD;
                (*rps).pm_interval = 1;
                qwork((*(*gt).i915).unordered_wq, ptr::addr_of_mut!((*rps).work));
            }
        } else {
            unsafe {
                (*rps).last_adj = 0;
            }
        }
        let interval = unsafe { (*rps).pm_interval };
        unsafe {
            let _ = mod_timer(
                ptr::addr_of_mut!((*rps).timer),
                (jiffies() + msecs_to_jiffies(interval) as u64) as c_ulong,
            );
            (*rps).pm_interval = (interval.saturating_mul(2)).min(BUSY_MAX_EI);
        }
    }
}
// upstream: intel_rps.c rps_start_timer()
unsafe fn rps_start_timer(rps: *mut IntelRps) {
    unsafe {
        (*rps).pm_timestamp = ktime_sub(ktime_get(), (*rps).pm_timestamp);
        (*rps).pm_interval = 1;
        let _ = mod_timer(ptr::addr_of_mut!((*rps).timer), (jiffies() + 1) as c_ulong);
    }
}
// upstream: intel_rps.c rps_stop_timer()
unsafe fn rps_stop_timer(rps: *mut IntelRps) {
    unsafe {
        let _ = timer_delete_sync(ptr::addr_of_mut!((*rps).timer));
        (*rps).pm_timestamp = ktime_sub(ktime_get(), (*rps).pm_timestamp);
        let _ = cancel_work_sync(ptr::addr_of_mut!((*rps).work));
    }
}
// upstream: intel_rps.c rps_pm_mask()
unsafe fn rps_pm_mask(rps: *mut IntelRps, val: u8) -> u32 {
    let mut mask = 0;
    unsafe {
        if val > (*rps).min_freq_softlimit {
            mask |= GEN6_PM_RP_UP_EI_EXPIRED | GEN6_PM_RP_DOWN_THRESHOLD | GEN6_PM_RP_DOWN_TIMEOUT;
        }
        if val < (*rps).max_freq_softlimit {
            mask |= GEN6_PM_RP_UP_EI_EXPIRED | GEN6_PM_RP_UP_THRESHOLD;
        }
        mask &= (*rps).pm_events;
    }
    unsafe { rps_pm_sanitize_mask(rps, !mask) }
}
// upstream: intel_rps.c rps_reset_ei()
unsafe fn rps_reset_ei(rps: *mut IntelRps) {
    unsafe { ptr::write_bytes(ptr::addr_of_mut!((*rps).ei), 0, 1) }
}
// upstream: intel_rps.c rps_enable_interrupts()
unsafe fn rps_enable_interrupts(rps: *mut IntelRps) {
    let gt = unsafe { rps_to_gt(rps) };
    unsafe {
        rps_reset_ei(rps);
        spin_lock_irq_raw((*gt).irq_lock);
        gen6_gt_pm_enable_irq(gt, (*rps).pm_events);
        spin_unlock_irq_raw((*gt).irq_lock);
        wr(
            (*gt).uncore,
            GEN6_PMINTRMSK,
            rps_pm_mask(rps, (*rps).last_freq),
        );
    }
}
// upstream: intel_rps.c gen6_rps_reset_interrupts()
unsafe fn gen6_rps_reset_interrupts(rps: *mut IntelRps) {
    unsafe { gen6_gt_pm_reset_iir(rps_to_gt(rps), GEN6_PM_RPS_EVENTS) }
}
// upstream: intel_rps.c gen11_rps_reset_interrupts()
unsafe fn gen11_rps_reset_interrupts(rps: *mut IntelRps) {
    let gt = unsafe { rps_to_gt(rps) };
    unsafe { while crate::intel_gt_irq_upstream::gen11_gt_reset_one_iir(gt, 0, GEN11_GTPM) {} }
}
// upstream: intel_rps.c rps_reset_interrupts()
unsafe fn rps_reset_interrupts(rps: *mut IntelRps) {
    let gt = unsafe { rps_to_gt(rps) };
    unsafe {
        spin_lock_irq_raw((*gt).irq_lock);
        if GRAPHICS_VER((*gt).i915) >= 11 {
            gen11_rps_reset_interrupts(rps)
        } else {
            gen6_rps_reset_interrupts(rps)
        }
        (*rps).pm_iir = 0;
        spin_unlock_irq_raw((*gt).irq_lock);
    }
}
// upstream: intel_rps.c rps_disable_interrupts()
unsafe fn rps_disable_interrupts(rps: *mut IntelRps) {
    let gt = unsafe { rps_to_gt(rps) };
    unsafe {
        wr((*gt).uncore, GEN6_PMINTRMSK, rps_pm_sanitize_mask(rps, !0));
        spin_lock_irq_raw((*gt).irq_lock);
        gen6_gt_pm_disable_irq(gt, GEN6_PM_RPS_EVENTS);
        spin_unlock_irq_raw((*gt).irq_lock);
        intel_synchronize_irq((*gt).i915);
        let _ = cancel_work_sync(ptr::addr_of_mut!((*rps).work));
        rps_reset_interrupts(rps);
    }
}

// Source-owned display-parent operations are real Linux i915 callbacks.
unsafe extern "C" {
    fn intel_synchronize_irq(i915: *mut DrmI915Private);
    fn ilk_display_rps_enable(display: *mut c_void);
    fn ilk_display_rps_disable(display: *mut c_void);
    fn vlv_iosf_sb_get(drm: *mut c_void, domains: core::ffi::c_ulong);
    fn vlv_iosf_sb_put(drm: *mut c_void, domains: core::ffi::c_ulong);
    fn vlv_iosf_sb_read(drm: *mut c_void, unit: u32, reg: u32) -> u32;
    fn vlv_iosf_sb_write(drm: *mut c_void, unit: u32, reg: u32, val: u32) -> i32;
    fn vlv_clock_get_gpll(drm: *mut c_void) -> u16;
    fn vlv_clock_get_czclk(drm: *mut c_void) -> u32;
    fn ilk_fsb_freq(i915: *mut DrmI915Private) -> u32;
    fn ilk_mem_freq(i915: *mut DrmI915Private) -> u32;
    fn snb_pcode_read(uncore: *mut IntelUncore, mbox: u32, val: *mut u32, val1: *mut u32) -> i32;
    fn snb_pcode_write_timeout(
        uncore: *mut IntelUncore,
        mbox: u32,
        val: u32,
        val1: u32,
        timeout: u32,
    ) -> i32;
    fn intel_vgpu_active(i915: *mut DrmI915Private) -> bool;
}
const IOSF_SB_PUNIT: u32 = 8;
const IOSF_SB_NC: u32 = 7;
const IOSF_SB_CCK: u32 = 1;

// upstream: intel_rps.c gen5_rps_init()
unsafe fn gen5_rps_init(rps: *mut IntelRps) {
    let i915 = unsafe { rps_to_i915(rps) };
    let uncore = unsafe { rps_to_uncore(rps) };
    let fsb = unsafe { ilk_fsb_freq(i915) };
    let mem = unsafe { ilk_mem_freq(i915) };
    let c_m = if fsb <= 3_200_000 {
        0
    } else if fsb <= 4_800_000 {
        1
    } else {
        2
    };
    let params = [
        (1, 1333, 301, 28664),
        (1, 1067, 294, 24460),
        (1, 800, 294, 25192),
        (0, 1333, 276, 27605),
        (0, 1067, 276, 27605),
        (0, 800, 231, 23784),
    ];
    for (i, t, m, c) in params {
        if i == c_m && t == ((mem + 500) / 1000) as i32 {
            unsafe {
                (*rps).ips.m = m;
                (*rps).ips.c = c;
            }
            break;
        }
    }
    let mode = unsafe { rd(uncore, MEMMODECTL) };
    let max = ((mode & 0xf0) >> 4) as u8;
    let min = (mode & 0xf) as u8;
    let start = ((mode & 0xf00) >> 8) as u8;
    drm_dbg!(
        unsafe { ptr::addr_of_mut!((*i915).drm) },
        "fmax: {}, fmin: {}, fstart: {}\n",
        max,
        min,
        start
    );
    unsafe {
        (*rps).min_freq = max;
        (*rps).efficient_freq = start;
        (*rps).max_freq = min;
    }
}
// upstream: intel_rps.c __ips_chipset_val()
unsafe fn __ips_chipset_val(ips: *mut IntelIps) -> c_ulong {
    let uncore = unsafe { rps_to_uncore(container_of!(ips, IntelRps, ips)) };
    let now = jiffies_to_msecs(jiffies());
    let dt = now.wrapping_sub(unsafe { (*ips).last_time1 });
    if dt <= 10 {
        return unsafe { (*ips).chipset_power };
    }
    let total =
        unsafe { rd(uncore, DMIEC) as u64 + rd(uncore, DDREC) as u64 + rd(uncore, CSIEC) as u64 };
    let delta = total.wrapping_sub(unsafe { (*ips).last_count1 });
    let result =
        ((unsafe { (*ips).m as u64 } * delta / dt + unsafe { (*ips).c as u64 }) / 10) as c_ulong;
    unsafe {
        (*ips).last_count1 = total;
        (*ips).last_time1 = now;
        (*ips).chipset_power = result;
    }
    result
}
// upstream: intel_rps.c ips_mch_val()
unsafe fn ips_mch_val(uncore: *mut IntelUncore) -> u32 {
    let tsfs = unsafe { rd(uncore, TSFS) };
    let x = unsafe { intel_uncore_read8(uncore, TR1) } as u32;
    let b = tsfs & TSFS_INTR_MASK;
    let m = (tsfs & TSFS_SLOPE_MASK) >> TSFS_SLOPE_SHIFT;
    (m * x / 127).wrapping_sub(b)
}
// upstream: intel_rps.c _pxvid_to_vd()
fn _pxvid_to_vd(mut pxvid: u8) -> i32 {
    if pxvid == 0 {
        return 0;
    }
    if (8..31).contains(&pxvid) {
        pxvid = 31;
    }
    (pxvid as i32 + 2) * 125
}
// upstream: intel_rps.c pvid_to_extvid()
unsafe fn pvid_to_extvid(i915: *mut DrmI915Private, pxvid: u8) -> u32 {
    let vd = _pxvid_to_vd(pxvid);
    if unsafe { is_mobile(i915) } {
        (vd - 1125).max(0) as u32
    } else {
        vd as u32
    }
}
// upstream: intel_rps.c __gen5_ips_update()
unsafe fn __gen5_ips_update(ips: *mut IntelIps) {
    let uncore = unsafe { rps_to_uncore(container_of!(ips, IntelRps, ips)) };
    let now = ktime_get_raw_fast_ns();
    let dt = now.wrapping_sub(unsafe { (*ips).last_time2 }) / NSEC_PER_MSEC;
    if dt <= 10 {
        return;
    }
    let count = unsafe { rd(uncore, GFXEC) };
    let delta = count.wrapping_sub(unsafe { (*ips).last_count2 as u32 });
    unsafe {
        (*ips).last_count2 = count as u64;
        (*ips).last_time2 = now;
        (*ips).gfx_power = (delta as u64 * 1181 / (dt * 10)) as c_ulong;
    }
}
// upstream: intel_rps.c gen5_rps_update()
unsafe fn gen5_rps_update(rps: *mut IntelRps) {
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        __gen5_ips_update(ptr::addr_of_mut!((*rps).ips));
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
    }
}
// upstream: intel_rps.c gen5_invert_freq()
unsafe fn gen5_invert_freq(rps: *mut IntelRps, val: u32) -> u32 {
    unsafe { (*rps).min_freq as u32 + ((*rps).max_freq as u32 - val) }
}
// upstream: intel_rps.c __gen5_rps_set()
unsafe fn __gen5_rps_set(rps: *mut IntelRps, mut val: u8) -> i32 {
    let uncore = unsafe { rps_to_uncore(rps) };
    unsafe {
        lockdep_assert_held!(ptr::addr_of_mut!(MCHDEV_LOCK));
    }
    let mut control = unsafe { rd16(uncore, MEMSWCTL) };
    if control & (MEMCTL_CMD_STS as u16) != 0 {
        drm_dbg!(
            unsafe { ptr::addr_of_mut!((*rps_to_i915(rps)).drm) },
            "gpu busy, RCS change rejected\n"
        );
        return -16;
    }
    val = unsafe { gen5_invert_freq(rps, val as u32) } as u8;
    control = ((MEMCTL_CMD_CHFREQ << MEMCTL_CMD_SHIFT)
        | ((val as u32) << MEMCTL_FREQ_SHIFT)
        | MEMCTL_SFCAVM) as u16;
    unsafe {
        wr16(uncore, MEMSWCTL, control);
        intel_uncore_posting_read16(uncore, MEMSWCTL);
        wr16(uncore, MEMSWCTL, control | MEMCTL_CMD_STS as u16);
    }
    0
}
// upstream: intel_rps.c gen5_rps_set()
unsafe fn gen5_rps_set(rps: *mut IntelRps, val: u8) -> i32 {
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        let err = __gen5_rps_set(rps, val);
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        err
    }
}
// upstream: intel_rps.c intel_pxfreq()
fn intel_pxfreq(v: u32) -> u64 {
    let div = ((v & 0x3f0000) >> 16) as u64;
    let post = ((v & 0x3000) >> 12) as u32;
    let pre = (v & 7) as u32;
    if pre == 0 {
        return 0;
    }
    div * 133333 / ((pre << post) as u64)
}
// upstream: intel_rps.c init_emon()
unsafe fn init_emon(uncore: *mut IntelUncore) -> u32 {
    let mut weights = [0u8; 16];
    unsafe {
        wr(uncore, ECR, 0);
        intel_uncore_posting_read(uncore, ECR);
        wr(uncore, SDEW, 0x15040d00);
        wr(uncore, CSIEW0, 0x007f0000);
        wr(uncore, CSIEW1, 0x1e220004);
        wr(uncore, CSIEW2, 0x04000004);
    }
    for i in 0..5 {
        unsafe { wr(uncore, pew(i), 0) }
    }
    for i in 0..3 {
        unsafe { wr(uncore, dew(i), 0) }
    }
    for i in 0..16 {
        let px = unsafe { rd(uncore, pxvfreq(i)) };
        let freq = intel_pxfreq(px);
        let vid = ((px & PXVFREQ_PX_MASK) >> PXVFREQ_PX_SHIFT) as u64;
        weights[i as usize] = (vid * vid * freq / 1000 * 255 / (127 * 127 * 900)) as u8;
    }
    weights[14] = 0;
    weights[15] = 0;
    for i in 0..4 {
        let v = (weights[(i * 4) as usize] as u32) << 24
            | (weights[(i * 4 + 1) as usize] as u32) << 16
            | (weights[(i * 4 + 2) as usize] as u32) << 8
            | weights[(i * 4 + 3) as usize] as u32;
        unsafe { wr(uncore, pxw(i), v) }
    }
    unsafe {
        wr(uncore, OGW0, 0);
        wr(uncore, OGW1, 0);
        wr(uncore, EG0, 0x7f00);
        wr(uncore, EG1, 0xe);
        wr(uncore, EG2, 0xe0000);
        wr(uncore, EG3, 0x68000300);
        wr(uncore, EG4, 0x42000000);
        wr(uncore, EG5, 0x00140031);
        wr(uncore, EG6, 0);
        wr(uncore, EG7, 0);
    }
    for i in 0..8 {
        unsafe { wr(uncore, pxwl(i), 0) }
    }
    unsafe {
        wr(uncore, ECR, 0x80000019);
        rd(uncore, LCFUSE02) & LCFUSE_HIV_MASK
    }
}
// upstream: intel_rps.c gen5_rps_enable()
unsafe fn gen5_rps_enable(rps: *mut IntelRps) -> bool {
    let i915 = unsafe { rps_to_i915(rps) };
    let display = unsafe { (*i915).display.cast::<c_void>() };
    let u = unsafe { rps_to_uncore(rps) };
    spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
    let mode = unsafe { rd(u, MEMMODECTL) };
    unsafe {
        wr16(u, PMMISC, rd16(u, PMMISC) | MCPPCE_EN as u16);
        wr16(u, TSC1, rd16(u, TSC1) | TSE as u16);
        wr(u, RCUPEI, 100000);
        wr(u, RCDNEI, 100000);
        wr(u, RCBMAXAVG, 90000);
        wr(u, RCBMINAVG, 80000);
        wr(u, MEMIHYST, 1);
    }
    let start = ((mode & MEMMODE_FSTART_MASK) >> MEMMODE_FSTART_SHIFT) as u8;
    let vid =
        ((unsafe { rd(u, pxvfreq(start as u32)) } & PXVFREQ_PX_MASK) >> PXVFREQ_PX_SHIFT) as u16;
    unsafe {
        wr(u, MEMINTREN, MEMINT_CX_SUPR_EN | MEMINT_EVAL_CHG_EN);
        wr(u, VIDSTART, vid as u32);
        intel_uncore_posting_read(u, VIDSTART);
        wr(u, MEMMODECTL, mode | MEMMODE_SWMODE_EN);
    }
    if unsafe { rd(u, MEMSWCTL) & MEMCTL_CMD_STS } != 0 {
        drm_err!(
            ptr::addr_of_mut!((*(*u).i915).drm),
            "stuck trying to change perf mode\n"
        );
    }
    unsafe {
        __gen5_rps_set(rps, (*rps).cur_freq);
        (*rps).ips.last_count1 = (rd(u, DMIEC) + rd(u, DDREC) + rd(u, CSIEC)) as u64;
        (*rps).ips.last_time1 = jiffies_to_msecs(jiffies());
        (*rps).ips.last_count2 = rd(u, GFXEC) as u64;
        (*rps).ips.last_time2 = ktime_get_raw_fast_ns();
        ilk_display_rps_enable(display);
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        (*rps).ips.corr = init_emon(u) as u8;
    }
    true
}
// upstream: intel_rps.c gen5_rps_disable()
unsafe fn gen5_rps_disable(rps: *mut IntelRps) {
    let i915 = unsafe { rps_to_i915(rps) };
    let u = unsafe { rps_to_uncore(rps) };
    spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
    unsafe {
        ilk_display_rps_disable((*i915).display.cast());
        let status = rd16(u, MEMSWCTL);
        intel_uncore_rmw(u, MEMINTREN, MEMINT_EVAL_CHG_EN, 0);
        wr(u, MEMINTRSTS, MEMINT_EVAL_CHG);
        __gen5_rps_set(rps, (*rps).idle_freq);
        wr16(u, MEMSWCTL, status | MEMCTL_CMD_STS as u16);
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
    }
}
// upstream: intel_rps.c rps_limits()
unsafe fn rps_limits(rps: *mut IntelRps, val: u8) -> u32 {
    let i915 = unsafe { rps_to_i915(rps) };
    let mut limits;
    if GRAPHICS_VER(i915) >= 9 {
        limits = (unsafe { (*rps).max_freq_softlimit as u32 }) << 23;
        if val <= unsafe { (*rps).min_freq_softlimit } {
            limits |= (unsafe { (*rps).min_freq_softlimit as u32 }) << 14;
        }
    } else {
        limits = (unsafe { (*rps).max_freq_softlimit as u32 }) << 24;
        if val <= unsafe { (*rps).min_freq_softlimit } {
            limits |= (unsafe { (*rps).min_freq_softlimit as u32 }) << 16;
        }
    }
    limits
}
// upstream: intel_rps.c rps_set_power()
unsafe fn rps_set_power(rps: *mut IntelRps, new_power: i32) {
    let gt = unsafe { rps_to_gt(rps) };
    let u = unsafe { (*gt).uncore };
    let (up, down) = match new_power {
        LOW_POWER => (16000, 32000),
        BETWEEN => (13000, 32000),
        HIGH_POWER => (10000, 32000),
        _ => (0, 0),
    };
    if new_power == unsafe { (*rps).power.mode } {
        return;
    }
    if unsafe { IS_VALLEYVIEW((*gt).i915) } {
        unsafe { (*rps).power.mode = new_power };
        return;
    }
    unsafe {
        set(
            u,
            GEN6_RP_UP_EI,
            intel_gt_ns_to_pm_interval(gt, (up * 1000) as u64) as u32,
        );
        set(
            u,
            GEN6_RP_UP_THRESHOLD,
            intel_gt_ns_to_pm_interval(gt, (up * (*rps).power.up_threshold as i32 * 10) as u64)
                as u32,
        );
        set(
            u,
            GEN6_RP_DOWN_EI,
            intel_gt_ns_to_pm_interval(gt, (down * 1000) as u64) as u32,
        );
        set(
            u,
            GEN6_RP_DOWN_THRESHOLD,
            intel_gt_ns_to_pm_interval(gt, (down * (*rps).power.down_threshold as i32 * 10) as u64)
                as u32,
        );
        set(
            u,
            GEN6_RP_CONTROL,
            (if GRAPHICS_VER((*gt).i915) > 9 {
                0
            } else {
                GEN6_RP_MEDIA_TURBO
            }) | GEN6_RP_MEDIA_HW_NORMAL_MODE
                | GEN6_RP_MEDIA_IS_GFX
                | GEN6_RP_ENABLE
                | GEN6_RP_UP_BUSY_AVG
                | GEN6_RP_DOWN_IDLE_AVG,
        );
        (*rps).power.mode = new_power;
    }
}
// upstream: intel_rps.c gen6_rps_set_thresholds()
unsafe fn gen6_rps_set_thresholds(rps: *mut IntelRps, val: u8) {
    let mut power = unsafe { (*rps).power.mode };
    let eff = unsafe { (*rps).efficient_freq };
    let cur = unsafe { (*rps).cur_freq };
    match power {
        LOW_POWER => {
            if val > eff.saturating_add(1) && val > cur {
                power = BETWEEN
            }
        }
        BETWEEN => {
            if val <= eff && val < cur {
                power = LOW_POWER
            } else if val >= unsafe { (*rps).rp0_freq } && val > cur {
                power = HIGH_POWER
            }
        }
        HIGH_POWER => {
            if val < ((unsafe { (*rps).rp1_freq as u16 + (*rps).rp0_freq as u16 } / 2) as u8)
                && val < cur
            {
                power = BETWEEN
            }
        }
        _ => {}
    }
    if val <= unsafe { (*rps).min_freq_softlimit } {
        power = LOW_POWER;
    }
    if val >= unsafe { (*rps).max_freq_softlimit } {
        power = HIGH_POWER;
    }
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).power.mutex));
        if (*rps).power.interactive != 0 {
            power = HIGH_POWER;
        }
        rps_set_power(rps, power);
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).power.mutex));
    }
}
// upstream: intel_rps.c intel_rps_mark_interactive()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_mark_interactive(rps: *mut IntelRps, interactive: bool) {
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).power.mutex));
        if interactive {
            let old = (*rps).power.interactive;
            (*rps).power.interactive = old.wrapping_add(1);
            if old == 0 && rps_is_active(rps) {
                rps_set_power(rps, HIGH_POWER);
            }
        } else {
            assert!((*rps).power.interactive != 0);
            (*rps).power.interactive -= 1;
        }
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).power.mutex));
    }
}
// upstream: intel_rps.c gen6_rps_set()
unsafe fn gen6_rps_set(rps: *mut IntelRps, val: u8) -> i32 {
    let u = unsafe { rps_to_uncore(rps) };
    let i = unsafe { rps_to_i915(rps) };
    let swreq = if GRAPHICS_VER(i) >= 9 {
        (val as u32) << GEN9_FREQUENCY_SHIFT
    } else if unsafe { IS_HASWELL(i) || IS_BROADWELL(i) } {
        (val as u32) << HSW_FREQUENCY_SHIFT
    } else {
        ((val as u32) << GEN6_FREQUENCY_SHIFT) | GEN6_AGGRESSIVE_TURBO
    };
    unsafe {
        set(u, GEN6_RPNSWREQ, swreq);
    }
    0
}
// upstream: intel_rps.c vlv_rps_set()
unsafe fn vlv_rps_set(rps: *mut IntelRps, val: u8) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    let drm = unsafe { ptr::addr_of_mut!((*i).drm) }.cast::<c_void>();
    unsafe {
        vlv_iosf_sb_get(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
        let e = vlv_iosf_sb_write(drm, IOSF_SB_PUNIT, PUNIT_REG_GPU_FREQ_REQ, val as u32);
        vlv_iosf_sb_put(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
        e
    }
}
// upstream: intel_rps.c rps_set()
unsafe fn rps_set(rps: *mut IntelRps, val: u8, update: bool) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    if val == unsafe { (*rps).last_freq } {
        return 0;
    }
    let err = if unsafe { IS_VALLEYVIEW(i) || IS_CHERRYVIEW(i) } {
        unsafe { vlv_rps_set(rps, val) }
    } else if GRAPHICS_VER(i) >= 6 {
        unsafe { gen6_rps_set(rps, val) }
    } else {
        unsafe { gen5_rps_set(rps, val) }
    };
    if err != 0 {
        return err;
    }
    if update && GRAPHICS_VER(i) >= 6 {
        unsafe { gen6_rps_set_thresholds(rps, val) };
    }
    unsafe {
        (*rps).last_freq = val;
    }
    0
}
// upstream: intel_rps.c intel_rps_unpark()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_unpark(rps: *mut IntelRps) {
    if !unsafe { rps_is_enabled(rps) } {
        return;
    }
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
        rps_set_active(rps);
        let val = (*rps)
            .cur_freq
            .clamp((*rps).min_freq_softlimit, (*rps).max_freq_softlimit);
        let _ = intel_rps_set(rps, val);
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
        (*rps).pm_iir = 0;
        if rps_has_interrupts(rps) {
            rps_enable_interrupts(rps)
        }
        if rps_uses_timer(rps) {
            rps_start_timer(rps)
        }
        if GRAPHICS_VER(rps_to_i915(rps)) == 5 {
            gen5_rps_update(rps)
        }
    }
}
// upstream: intel_rps.c intel_rps_park()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_park(rps: *mut IntelRps) {
    if !unsafe { rps_is_enabled(rps) } || !unsafe { rps_clear_active(rps) } {
        return;
    }
    unsafe {
        if rps_uses_timer(rps) {
            rps_stop_timer(rps)
        }
        if rps_has_interrupts(rps) {
            rps_disable_interrupts(rps)
        }
        if (*rps).last_freq <= (*rps).idle_freq {
            return;
        }
        let u = rps_to_uncore(rps);
        fw_get(u, FORCEWAKE_MEDIA);
        let _ = rps_set(rps, (*rps).idle_freq, false);
        fw_put(u, FORCEWAKE_MEDIA);
        let mut adj = (*rps).last_adj;
        if adj < 0 {
            adj *= 2
        } else {
            adj = -2;
        }
        (*rps).last_adj = adj;
        (*rps).cur_freq = ((*rps).cur_freq as i32 + adj).max((*rps).min_freq as i32) as u8;
        if (*rps).cur_freq < (*rps).efficient_freq {
            (*rps).cur_freq = (*rps).efficient_freq;
            (*rps).last_adj = 0;
        }
    }
}
// upstream: intel_rps.c intel_rps_get_boost_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_boost_frequency(rps: *mut IntelRps) -> u32 {
    unsafe {
        if rps_uses_slpc(rps) {
            (*rps_to_slpc(rps)).boost_freq
        } else {
            intel_gpu_freq(rps, (*rps).boost_freq as i32) as u32
        }
    }
}
// upstream: intel_rps.c rps_set_boost_freq()
unsafe fn rps_set_boost_freq(rps: *mut IntelRps, val: u32) -> i32 {
    let val = unsafe { intel_freq_opcode(rps, val as i32) };
    if val < unsafe { (*rps).min_freq as i32 } || val > unsafe { (*rps).max_freq as i32 } {
        return -22;
    }
    let mut boost = false;
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
        if val as u8 != (*rps).boost_freq {
            (*rps).boost_freq = val as u8;
            boost = atomic_read(&(*rps).num_waiters) != 0;
        }
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
        if boost {
            qwork(
                (*(*rps_to_gt(rps)).i915).unordered_wq,
                ptr::addr_of_mut!((*rps).work),
            );
        }
    }
    0
}
// upstream: intel_rps.c intel_rps_set_boost_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_set_boost_frequency(rps: *mut IntelRps, freq: u32) -> i32 {
    unsafe {
        if rps_uses_slpc(rps) {
            crate::intel_guc_slpc_upstream::intel_guc_slpc_set_boost_freq(rps_to_slpc(rps), freq)
        } else {
            rps_set_boost_freq(rps, freq)
        }
    }
}
// upstream: intel_rps.c intel_rps_dec_waiters()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_dec_waiters(rps: *mut IntelRps) {
    unsafe {
        if rps_uses_slpc(rps) {
            let slpc = rps_to_slpc(rps);
            if (*slpc).power_profile == SLPC_POWER_PROFILES_POWER_SAVING {
                return;
            }
            crate::intel_guc_slpc_upstream::intel_guc_slpc_dec_waiters(slpc);
        } else {
            atomic_dec(&mut (*rps).num_waiters);
        }
    }
}
// upstream: intel_rps.c intel_rps_boost()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_boost(rq: *mut I915Request) {
    if unsafe {
        crate::linux::requests::i915_request_signaled(rq)
            || ((*rq).fence.flags & (1 << crate::linux::requests::I915_FENCE_FLAG_BOOST)) != 0
    } {
        return;
    }
    if unsafe {
        test_bit(
            crate::intel_context_types_upstream::CONTEXT_LOW_LATENCY,
            &(*(*rq).context).flags,
        )
    } {
        return;
    }
    if unsafe {
        test_and_set_bit(
            crate::linux::requests::I915_FENCE_FLAG_BOOST,
            &mut (*rq).fence.flags,
        )
    } {
        return;
    }
    let engine = unsafe { (*rq).engine };
    let rps = unsafe { ptr::addr_of_mut!((*(*engine).gt).rps) };
    unsafe {
        if rps_uses_slpc(rps) {
            let slpc = rps_to_slpc(rps);
            if (*slpc).power_profile == SLPC_POWER_PROFILES_POWER_SAVING {
                return;
            }
            if atomic_fetch_inc(&mut (*slpc).num_waiters) == 0
                && (*slpc).min_freq_softlimit < (*slpc).boost_freq
            {
                qwork(
                    (*(*rps_to_gt(rps)).i915).unordered_wq,
                    ptr::addr_of_mut!((*slpc).boost_work),
                );
            }
            return;
        }
        if atomic_fetch_inc(&mut (*rps).num_waiters) != 0 || !rps_is_active(rps) {
            return;
        }
        if (*rps).cur_freq < (*rps).boost_freq {
            qwork(
                (*(*rps_to_gt(rps)).i915).unordered_wq,
                ptr::addr_of_mut!((*rps).work),
            );
        }
        (*rps).boosts = (*rps).boosts.wrapping_add(1);
    }
}
// upstream: intel_rps.c intel_rps_set()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_set(rps: *mut IntelRps, val: u8) -> i32 {
    unsafe {
        assert!(val <= (*rps).max_freq && val >= (*rps).min_freq);
        if rps_is_active(rps) {
            let err = rps_set(rps, val, true);
            if err != 0 {
                return err;
            }
            if rps_has_interrupts(rps) {
                let u = rps_to_uncore(rps);
                set(u, GEN6_RP_INTERRUPT_LIMITS, rps_limits(rps, val));
                set(u, GEN6_PMINTRMSK, rps_pm_mask(rps, val));
            }
        }
        (*rps).cur_freq = val;
    }
    0
}
// upstream: intel_rps.c intel_rps_read_state_cap()
unsafe fn intel_rps_read_state_cap(rps: *mut IntelRps) -> u32 {
    let i = unsafe { rps_to_i915(rps) };
    let u = unsafe { rps_to_uncore(rps) };
    unsafe {
        rd(
            u,
            if IS_GEN9_LP(i) {
                BXT_RP_STATE_CAP
            } else {
                GEN6_RP_STATE_CAP
            },
        )
    }
}
// upstream: intel_rps.c mtl_get_freq_caps()
unsafe fn mtl_get_freq_caps(rps: *mut IntelRps, caps: *mut IntelRpsFreqCaps) {
    let u = unsafe { rps_to_uncore(rps) };
    let gt = unsafe { rps_to_gt(rps) };
    let state = unsafe {
        rd(
            u,
            if (*gt).type_ == GT_MEDIA {
                MTL_MEDIAP_STATE_CAP
            } else {
                MTL_RP_STATE_CAP
            },
        )
    };
    let rpe = unsafe {
        rd(
            u,
            if (*gt).type_ == GT_MEDIA {
                MTL_MPE_FREQUENCY
            } else {
                MTL_GT_RPE_FREQUENCY
            },
        )
    };
    unsafe {
        (*caps).rp0_freq = field_get(MTL_RP0_CAP_MASK, state) as u8;
        (*caps).min_freq = field_get(MTL_RPN_CAP_MASK, state) as u8;
        (*caps).rp1_freq = field_get(MTL_RPE_MASK, rpe) as u8;
    }
}
// upstream: intel_rps.c __gen6_rps_get_freq_caps()
unsafe fn __gen6_rps_get_freq_caps(rps: *mut IntelRps, caps: *mut IntelRpsFreqCaps) {
    let i = unsafe { rps_to_i915(rps) };
    let v = unsafe { intel_rps_read_state_cap(rps) };
    unsafe {
        if IS_GEN9_LP(i) {
            (*caps).rp0_freq = ((v >> 16) & 0xff) as u8;
            (*caps).rp1_freq = ((v >> 8) & 0xff) as u8;
            (*caps).min_freq = (v & 0xff) as u8;
        } else {
            (*caps).rp0_freq = (v & 0xff) as u8;
            (*caps).rp1_freq = if GRAPHICS_VER(i) >= 10 {
                field_get(RPE_MASK, rd((*rps_to_gt(rps)).uncore, GEN10_FREQ_INFO_REC)) as u8
            } else {
                ((v >> 8) & 0xff) as u8
            };
            (*caps).min_freq = ((v >> 16) & 0xff) as u8;
        }
        if is_gen9_bc(i) || GRAPHICS_VER(i) >= 11 {
            (*caps).rp0_freq = (*caps).rp0_freq.wrapping_mul(GEN9_FREQ_SCALER as u8);
            (*caps).rp1_freq = (*caps).rp1_freq.wrapping_mul(GEN9_FREQ_SCALER as u8);
            (*caps).min_freq = (*caps).min_freq.wrapping_mul(GEN9_FREQ_SCALER as u8);
        }
    }
}
// upstream: intel_rps.c gen6_rps_get_freq_caps()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_rps_get_freq_caps(rps: *mut IntelRps, caps: *mut IntelRpsFreqCaps) {
    if unsafe { GRAPHICS_VER_FULL(rps_to_i915(rps)) >= IP_VER(12, 70) } {
        unsafe { mtl_get_freq_caps(rps, caps) }
    } else {
        unsafe { __gen6_rps_get_freq_caps(rps, caps) }
    }
}
// upstream: intel_rps.c gen6_rps_init()
unsafe fn gen6_rps_init(rps: *mut IntelRps) {
    let i = unsafe { rps_to_i915(rps) };
    let mut caps = IntelRpsFreqCaps {
        rp0_freq: 0,
        rp1_freq: 0,
        min_freq: 0,
    };
    unsafe {
        gen6_rps_get_freq_caps(rps, &mut caps);
        (*rps).rp0_freq = caps.rp0_freq;
        (*rps).rp1_freq = caps.rp1_freq;
        (*rps).min_freq = caps.min_freq;
        (*rps).max_freq = (*rps).rp0_freq;
        (*rps).efficient_freq = (*rps).rp1_freq;
    }
    if unsafe { IS_HASWELL(i) || IS_BROADWELL(i) || is_gen9_bc(i) || GRAPHICS_VER(i) >= 11 } {
        let mut ddcc = 0;
        let mult = if unsafe { is_gen9_bc(i) || GRAPHICS_VER(i) >= 11 } {
            GEN9_FREQ_SCALER
        } else {
            1
        };
        if unsafe {
            snb_pcode_read(
                (*rps_to_gt(rps)).uncore,
                HSW_PCODE_DYNAMIC_DUTY_CYCLE_CONTROL,
                &mut ddcc,
                ptr::null_mut(),
            )
        } == 0
        {
            unsafe {
                (*rps).efficient_freq = (((ddcc >> 8) & 0xff) * mult)
                    .clamp((*rps).min_freq as u32, (*rps).max_freq as u32)
                    as u8;
            }
        }
    }
}
// upstream: intel_rps.c rps_reset()
unsafe fn rps_reset(rps: *mut IntelRps) -> bool {
    let i = unsafe { rps_to_i915(rps) };
    unsafe {
        (*rps).power.mode = -1;
        (*rps).last_freq = u8::MAX;
    }
    if unsafe { rps_set(rps, (*rps).min_freq, true) } != 0 {
        drm_err!(
            ptr::addr_of_mut!((*i).drm),
            "Failed to reset RPS to initial values\n"
        );
        return false;
    }
    unsafe {
        (*rps).cur_freq = (*rps).min_freq;
    }
    true
}
// upstream: intel_rps.c gen9_rps_enable()
unsafe fn gen9_rps_enable(rps: *mut IntelRps) -> bool {
    let gt = unsafe { rps_to_gt(rps) };
    if GRAPHICS_VER((*gt).i915) == 9 {
        unsafe {
            wr_fw(
                (*gt).uncore,
                GEN6_RC_VIDEO_FREQ,
                ((*rps).rp1_freq as u32) << GEN9_FREQUENCY_SHIFT,
            );
        }
    }
    unsafe {
        wr_fw((*gt).uncore, GEN6_RP_IDLE_HYSTERSIS, 10);
        (*rps).pm_events = GEN6_PM_RP_UP_THRESHOLD | GEN6_PM_RP_DOWN_THRESHOLD;
        rps_reset(rps)
    }
}
// upstream: intel_rps.c gen8_rps_enable()
unsafe fn gen8_rps_enable(rps: *mut IntelRps) -> bool {
    let u = unsafe { rps_to_uncore(rps) };
    unsafe {
        wr_fw(
            u,
            GEN6_RC_VIDEO_FREQ,
            ((*rps).rp1_freq as u32) << HSW_FREQUENCY_SHIFT,
        );
        wr_fw(u, GEN6_RP_IDLE_HYSTERSIS, 10);
        (*rps).pm_events = GEN6_PM_RP_UP_THRESHOLD | GEN6_PM_RP_DOWN_THRESHOLD;
        rps_reset(rps)
    }
}
// upstream: intel_rps.c gen6_rps_enable()
unsafe fn gen6_rps_enable(rps: *mut IntelRps) -> bool {
    let u = unsafe { rps_to_uncore(rps) };
    unsafe {
        wr_fw(u, GEN6_RP_DOWN_TIMEOUT, 50000);
        wr_fw(u, GEN6_RP_IDLE_HYSTERSIS, 10);
        (*rps).pm_events =
            GEN6_PM_RP_UP_THRESHOLD | GEN6_PM_RP_DOWN_THRESHOLD | GEN6_PM_RP_DOWN_TIMEOUT;
        rps_reset(rps)
    }
}
// upstream: intel_rps.c chv_rps_max_freq()
unsafe fn chv_rps_max_freq(rps: *mut IntelRps) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    let gt = unsafe { rps_to_gt(rps) };
    let mut v = unsafe {
        vlv_iosf_sb_read(
            ptr::addr_of_mut!((*i).drm).cast(),
            IOSF_SB_PUNIT,
            FB_GFX_FMAX_AT_VMAX_FUSE,
        )
    };
    v >>= match unsafe { (*gt).info.sseu.eu_total } {
        8 => 24,
        12 => 16,
        _ => 8,
    };
    (v & FB_GFX_FREQ_FUSE_MASK) as i32
}
// upstream: intel_rps.c chv_rps_rpe_freq()
unsafe fn chv_rps_rpe_freq(rps: *mut IntelRps) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    ((unsafe {
        vlv_iosf_sb_read(
            ptr::addr_of_mut!((*i).drm).cast(),
            IOSF_SB_PUNIT,
            PUNIT_GPU_DUTYCYCLE_REG,
        )
    } >> PUNIT_GPU_DUTYCYCLE_RPE_FREQ_SHIFT)
        & PUNIT_GPU_DUTYCYCLE_RPE_FREQ_MASK) as i32
}
// upstream: intel_rps.c chv_rps_guar_freq()
unsafe fn chv_rps_guar_freq(rps: *mut IntelRps) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    (unsafe {
        vlv_iosf_sb_read(
            ptr::addr_of_mut!((*i).drm).cast(),
            IOSF_SB_PUNIT,
            FB_GFX_FMAX_AT_VMAX_FUSE,
        )
    } & FB_GFX_FREQ_FUSE_MASK) as i32
}
// upstream: intel_rps.c chv_rps_min_freq()
unsafe fn chv_rps_min_freq(rps: *mut IntelRps) -> u32 {
    let i = unsafe { rps_to_i915(rps) };
    (unsafe {
        vlv_iosf_sb_read(
            ptr::addr_of_mut!((*i).drm).cast(),
            IOSF_SB_PUNIT,
            FB_GFX_FMIN_AT_VMIN_FUSE,
        )
    } >> FB_GFX_FMIN_AT_VMIN_FUSE_SHIFT)
        & FB_GFX_FREQ_FUSE_MASK
}
// upstream: intel_rps.c chv_rps_enable()
unsafe fn chv_rps_enable(rps: *mut IntelRps) -> bool {
    let u = unsafe { rps_to_uncore(rps) };
    let i = unsafe { rps_to_i915(rps) };
    unsafe {
        wr_fw(u, GEN6_RP_DOWN_TIMEOUT, 1_000_000);
        wr_fw(u, GEN6_RP_UP_THRESHOLD, 59400);
        wr_fw(u, GEN6_RP_DOWN_THRESHOLD, 245000);
        wr_fw(u, GEN6_RP_UP_EI, 66000);
        wr_fw(u, GEN6_RP_DOWN_EI, 350000);
        wr_fw(u, GEN6_RP_IDLE_HYSTERSIS, 10);
        wr_fw(
            u,
            GEN6_RP_CONTROL,
            GEN6_RP_MEDIA_HW_NORMAL_MODE
                | GEN6_RP_MEDIA_IS_GFX
                | GEN6_RP_ENABLE
                | GEN6_RP_UP_BUSY_AVG
                | GEN6_RP_DOWN_IDLE_AVG,
        );
        (*rps).pm_events =
            GEN6_PM_RP_UP_THRESHOLD | GEN6_PM_RP_DOWN_THRESHOLD | GEN6_PM_RP_DOWN_TIMEOUT;
        let drm = ptr::addr_of_mut!((*i).drm).cast();
        vlv_iosf_sb_get(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
        let v = VLV_OVERRIDE_EN | VLV_SOC_TDP_EN | CHV_BIAS_CPU_50_SOC_50;
        let _ = vlv_iosf_sb_write(drm, IOSF_SB_PUNIT, VLV_TURBO_SOC_OVERRIDE, v);
        let sts = vlv_iosf_sb_read(drm, IOSF_SB_PUNIT, PUNIT_REG_GPU_FREQ_STS);
        vlv_iosf_sb_put(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
        drm_warn_once(&CHV_GPLL_WARNED, drm, sts & GPLLENABLE == 0);
        drm_dbg!(drm, "GPLL enabled? {}\n", str_yes_no(sts & GPLLENABLE != 0));
        drm_dbg!(drm, "GPU status: 0x{:08x}\n", sts);
        rps_reset(rps)
    }
}
// upstream: intel_rps.c vlv_rps_guar_freq()
unsafe fn vlv_rps_guar_freq(rps: *mut IntelRps) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    ((unsafe {
        vlv_iosf_sb_read(
            ptr::addr_of_mut!((*i).drm).cast(),
            IOSF_SB_NC,
            IOSF_NC_FB_GFX_FREQ_FUSE,
        )
    } & FB_GFX_FGUARANTEED_FREQ_FUSE_MASK)
        >> FB_GFX_FGUARANTEED_FREQ_FUSE_SHIFT) as i32
}
// upstream: intel_rps.c vlv_rps_max_freq()
unsafe fn vlv_rps_max_freq(rps: *mut IntelRps) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    (((unsafe {
        vlv_iosf_sb_read(
            ptr::addr_of_mut!((*i).drm).cast(),
            IOSF_SB_NC,
            IOSF_NC_FB_GFX_FREQ_FUSE,
        )
    } & FB_GFX_MAX_FREQ_FUSE_MASK)
        >> FB_GFX_MAX_FREQ_FUSE_SHIFT)
        .min(0xea)) as i32
}
// upstream: intel_rps.c vlv_rps_rpe_freq()
unsafe fn vlv_rps_rpe_freq(rps: *mut IntelRps) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    let drm = unsafe { ptr::addr_of_mut!((*i).drm) }.cast();
    let lo = unsafe { vlv_iosf_sb_read(drm, IOSF_SB_NC, IOSF_NC_FB_GFX_FMAX_FUSE_LO) };
    let hi = unsafe { vlv_iosf_sb_read(drm, IOSF_SB_NC, IOSF_NC_FB_GFX_FMAX_FUSE_HI) };
    (((lo & FB_FMAX_VMIN_FREQ_LO_MASK) >> FB_FMAX_VMIN_FREQ_LO_SHIFT)
        | ((hi & FB_FMAX_VMIN_FREQ_HI_MASK) << 5)) as i32
}
// upstream: intel_rps.c vlv_rps_min_freq()
unsafe fn vlv_rps_min_freq(rps: *mut IntelRps) -> u32 {
    let i = unsafe { rps_to_i915(rps) };
    unsafe {
        vlv_iosf_sb_read(
            ptr::addr_of_mut!((*i).drm).cast(),
            IOSF_SB_PUNIT,
            PUNIT_REG_GPU_LFM,
        ) & 0xff
    }
    .max(0xc0)
}
// upstream: intel_rps.c vlv_rps_enable()
unsafe fn vlv_rps_enable(rps: *mut IntelRps) -> bool {
    let u = unsafe { rps_to_uncore(rps) };
    let i = unsafe { rps_to_i915(rps) };
    unsafe {
        wr_fw(u, GEN6_RP_DOWN_TIMEOUT, 1_000_000);
        wr_fw(u, GEN6_RP_UP_THRESHOLD, 59400);
        wr_fw(u, GEN6_RP_DOWN_THRESHOLD, 245000);
        wr_fw(u, GEN6_RP_UP_EI, 66000);
        wr_fw(u, GEN6_RP_DOWN_EI, 350000);
        wr_fw(u, GEN6_RP_IDLE_HYSTERSIS, 10);
        wr_fw(
            u,
            GEN6_RP_CONTROL,
            GEN6_RP_MEDIA_TURBO
                | GEN6_RP_MEDIA_HW_NORMAL_MODE
                | GEN6_RP_MEDIA_IS_GFX
                | GEN6_RP_ENABLE
                | GEN6_RP_UP_BUSY_AVG
                | GEN6_RP_DOWN_IDLE_CONT,
        );
        (*rps).pm_events = GEN6_PM_RP_UP_EI_EXPIRED;
        let drm = ptr::addr_of_mut!((*i).drm).cast();
        vlv_iosf_sb_get(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
        let v = VLV_OVERRIDE_EN | VLV_SOC_TDP_EN | VLV_BIAS_CPU_125_SOC_875;
        let _ = vlv_iosf_sb_write(drm, IOSF_SB_PUNIT, VLV_TURBO_SOC_OVERRIDE, v);
        let sts = vlv_iosf_sb_read(drm, IOSF_SB_PUNIT, PUNIT_REG_GPU_FREQ_STS);
        vlv_iosf_sb_put(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
        drm_warn_once(&CHV_GPLL_WARNED, drm, sts & GPLLENABLE == 0);
        drm_dbg!(drm, "GPLL enabled? {}\n", str_yes_no(sts & GPLLENABLE != 0));
        drm_dbg!(drm, "GPU status: 0x{:08x}\n", sts);
        rps_reset(rps)
    }
}
// upstream: intel_rps.c __ips_gfx_val()
unsafe fn __ips_gfx_val(ips: *mut IntelIps) -> c_ulong {
    let rps = unsafe { container_of!(ips, IntelRps, ips) };
    let u = unsafe { rps_to_uncore(rps) };
    let px = (unsafe { rd(u, pxvfreq((*rps).cur_freq as u32)) } >> 24) & 0x7f;
    let ext = unsafe { pvid_to_extvid(rps_to_i915(rps), px as u8) } as u64;
    let t = unsafe { ips_mch_val(u) } as u64;
    let corr = if t > 80 {
        t * 2349 + 135940
    } else if t >= 50 {
        t * 964 + 29317
    } else {
        t * 301 + 1004
    };
    let corr = (corr * 150142 * ext / 10000).wrapping_sub(78642);
    let corr2 = corr / 100000 * unsafe { (*ips).corr as u64 };
    let state2 = (corr2 * ext / 10000) / 100;
    unsafe {
        __gen5_ips_update(ips);
        (*ips).gfx_power + state2 as c_ulong
    }
}
// upstream: intel_rps.c has_busy_stats()
unsafe fn has_busy_stats(rps: *mut IntelRps) -> bool {
    let gt = unsafe { rps_to_gt(rps) };
    for engine in unsafe { (*gt).engine.iter().take((*gt).info.num_engines as usize) } {
        if !engine.is_null()
            && !unsafe { crate::intel_engine_cs_upstream::intel_engine_supports_stats(*engine) }
        {
            return false;
        }
    }
    true
}
// upstream: intel_rps.c intel_rps_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_enable(rps: *mut IntelRps) {
    let i = unsafe { rps_to_i915(rps) };
    let u = unsafe { rps_to_uncore(rps) };
    if !unsafe { has_rps(i) || rps_uses_slpc(rps) } {
        return;
    }
    unsafe {
        intel_gt_check_clock_frequency(rps_to_gt(rps));
        fw_get(u, FORCEWAKE_ALL);
    }
    let enabled = unsafe {
        if (*rps).max_freq <= (*rps).min_freq {
            false
        } else if IS_CHERRYVIEW(i) {
            chv_rps_enable(rps)
        } else if IS_VALLEYVIEW(i) {
            vlv_rps_enable(rps)
        } else if GRAPHICS_VER(i) >= 9 {
            gen9_rps_enable(rps)
        } else if GRAPHICS_VER(i) >= 8 {
            gen8_rps_enable(rps)
        } else if GRAPHICS_VER(i) >= 6 {
            gen6_rps_enable(rps)
        } else if is_ironlake_mobile(i) {
            gen5_rps_enable(rps)
        } else {
            false
        }
    };
    unsafe {
        fw_put(u, FORCEWAKE_ALL);
    }
    if !enabled {
        return;
    }
    unsafe {
        assert!((*rps).max_freq >= (*rps).min_freq);
        assert!((*rps).idle_freq <= (*rps).max_freq);
        assert!(
            (*rps).efficient_freq >= (*rps).min_freq && (*rps).efficient_freq <= (*rps).max_freq
        );
        if has_busy_stats(rps) {
            rps_set_timer(rps)
        } else if (6..=11).contains(&GRAPHICS_VER(i)) {
            rps_set_interrupts(rps)
        }
        rps_set_enabled(rps);
    }
}
// upstream: intel_rps.c gen6_rps_disable()
unsafe fn gen6_rps_disable(rps: *mut IntelRps) {
    unsafe { set(rps_to_uncore(rps), GEN6_RP_CONTROL, 0) }
}
// upstream: intel_rps.c intel_rps_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_disable(rps: *mut IntelRps) {
    if !unsafe { rps_is_enabled(rps) } {
        return;
    }
    let i = unsafe { rps_to_i915(rps) };
    unsafe {
        rps_clear_enabled(rps);
        rps_clear_interrupts(rps);
        rps_clear_timer(rps);
        if GRAPHICS_VER(i) >= 6 {
            gen6_rps_disable(rps)
        } else if is_ironlake_mobile(i) {
            gen5_rps_disable(rps)
        }
    }
}
// upstream: intel_rps.c byt_gpu_freq()
unsafe fn byt_gpu_freq(rps: *mut IntelRps, val: i32) -> i32 {
    div_round_closest(
        unsafe { (*rps).gpll_ref_freq as i64 * (val as i64 - 0xb7) },
        1000,
    ) as i32
}
// upstream: intel_rps.c byt_freq_opcode()
unsafe fn byt_freq_opcode(rps: *mut IntelRps, val: i32) -> i32 {
    let ref_freq = unsafe { (*rps).gpll_ref_freq as i64 };
    (div_round_closest(1000 * val as i64, ref_freq) + 0xb7) as i32
}
// upstream: intel_rps.c chv_gpu_freq()
unsafe fn chv_gpu_freq(rps: *mut IntelRps, val: i32) -> i32 {
    div_round_closest(unsafe { (*rps).gpll_ref_freq as i64 * val as i64 }, 4000) as i32
}
// upstream: intel_rps.c chv_freq_opcode()
unsafe fn chv_freq_opcode(rps: *mut IntelRps, val: i32) -> i32 {
    (div_round_closest(2000 * val as i64, unsafe { (*rps).gpll_ref_freq as i64 }) * 2) as i32
}
// upstream: intel_rps.c intel_gpu_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gpu_freq(rps: *mut IntelRps, val: i32) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    if GRAPHICS_VER(i) >= 9 {
        div_round_closest(
            val as i64 * GT_FREQUENCY_MULTIPLIER as i64,
            GEN9_FREQ_SCALER as i64,
        ) as i32
    } else if unsafe { IS_CHERRYVIEW(i) } {
        unsafe { chv_gpu_freq(rps, val) }
    } else if unsafe { IS_VALLEYVIEW(i) } {
        unsafe { byt_gpu_freq(rps, val) }
    } else if GRAPHICS_VER(i) >= 6 {
        val * GT_FREQUENCY_MULTIPLIER as i32
    } else {
        val
    }
}
// upstream: intel_rps.c intel_freq_opcode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_freq_opcode(rps: *mut IntelRps, val: i32) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    if GRAPHICS_VER(i) >= 9 {
        div_round_closest(
            val as i64 * GEN9_FREQ_SCALER as i64,
            GT_FREQUENCY_MULTIPLIER as i64,
        ) as i32
    } else if unsafe { IS_CHERRYVIEW(i) } {
        unsafe { chv_freq_opcode(rps, val) }
    } else if unsafe { IS_VALLEYVIEW(i) } {
        unsafe { byt_freq_opcode(rps, val) }
    } else if GRAPHICS_VER(i) >= 6 {
        div_round_closest(val as i64, GT_FREQUENCY_MULTIPLIER as i64) as i32
    } else {
        val
    }
}
// upstream: intel_rps.c vlv_init_gpll_ref_freq()
unsafe fn vlv_init_gpll_ref_freq(rps: *mut IntelRps) {
    let i = unsafe { rps_to_i915(rps) };
    let f = unsafe { vlv_clock_get_gpll(ptr::addr_of_mut!((*i).drm).cast()) };
    unsafe {
        (*rps).gpll_ref_freq = f;
    }
}
// upstream: intel_rps.c vlv_rps_init()
unsafe fn vlv_rps_init(rps: *mut IntelRps) {
    let i = unsafe { rps_to_i915(rps) };
    let drm = unsafe { ptr::addr_of_mut!((*i).drm) }.cast();
    unsafe {
        vlv_init_gpll_ref_freq(rps);
        vlv_iosf_sb_get(drm, (bit(IOSF_SB_PUNIT) | bit(IOSF_SB_NC) | bit(IOSF_SB_CCK)) as core::ffi::c_ulong);
        (*rps).max_freq = vlv_rps_max_freq(rps) as u8;
        (*rps).rp0_freq = (*rps).max_freq;
        (*rps).efficient_freq = vlv_rps_rpe_freq(rps) as u8;
        (*rps).rp1_freq = vlv_rps_guar_freq(rps) as u8;
        (*rps).min_freq = vlv_rps_min_freq(rps) as u8;
        vlv_iosf_sb_put(drm, (bit(IOSF_SB_PUNIT) | bit(IOSF_SB_NC) | bit(IOSF_SB_CCK)) as core::ffi::c_ulong);
    }
}
// upstream: intel_rps.c chv_rps_init()
unsafe fn chv_rps_init(rps: *mut IntelRps) {
    let i = unsafe { rps_to_i915(rps) };
    let drm = unsafe { ptr::addr_of_mut!((*i).drm) }.cast();
    unsafe {
        vlv_init_gpll_ref_freq(rps);
        vlv_iosf_sb_get(drm, (bit(IOSF_SB_PUNIT) | bit(IOSF_SB_NC) | bit(IOSF_SB_CCK)) as core::ffi::c_ulong);
        (*rps).max_freq = chv_rps_max_freq(rps) as u8;
        (*rps).rp0_freq = (*rps).max_freq;
        (*rps).efficient_freq = chv_rps_rpe_freq(rps) as u8;
        (*rps).rp1_freq = chv_rps_guar_freq(rps) as u8;
        (*rps).min_freq = chv_rps_min_freq(rps) as u8;
        vlv_iosf_sb_put(drm, (bit(IOSF_SB_PUNIT) | bit(IOSF_SB_NC) | bit(IOSF_SB_CCK)) as core::ffi::c_ulong);
        let odd =
            ((*rps).max_freq | (*rps).efficient_freq | (*rps).rp1_freq | (*rps).min_freq) & 1 != 0;
        drm_warn_once(&CHV_ODD_FREQ_WARNED, drm, odd);
    }
}
// upstream: intel_rps.c vlv_c0_read()
unsafe fn vlv_c0_read(u: *mut IntelUncore, ei: *mut IntelRpsEi) {
    unsafe {
        (*ei).ktime = ktime_get_raw_fast_ns() as i64;
        (*ei).render_c0 = rd(u, VLV_RENDER_C0_COUNT);
        (*ei).media_c0 = rd(u, VLV_MEDIA_C0_COUNT);
    }
}
// upstream: intel_rps.c vlv_wa_c0_ei()
unsafe fn vlv_wa_c0_ei(rps: *mut IntelRps, pm_iir: u32) -> u32 {
    if pm_iir & GEN6_PM_RP_UP_EI_EXPIRED == 0 {
        return 0;
    }
    let i = unsafe { rps_to_i915(rps) };
    let u = unsafe { rps_to_uncore(rps) };
    let prev = unsafe { ptr::addr_of!((*rps).ei) };
    let mut now = IntelRpsEi {
        ktime: 0,
        render_c0: 0,
        media_c0: 0,
    };
    unsafe {
        vlv_c0_read(u, &mut now);
    }
    let mut events = 0;
    if unsafe { (*prev).ktime != 0 } {
        let time = (now.ktime - unsafe { (*prev).ktime }) / 1000
            * unsafe { vlv_clock_get_czclk(ptr::addr_of_mut!((*i).drm).cast()) } as i64;
        let render = now.render_c0.wrapping_sub(unsafe { (*prev).render_c0 });
        let media = now.media_c0.wrapping_sub(unsafe { (*prev).media_c0 });
        let c0 = render.max(media) as i64 * 1000 * 100 << 8;
        if c0 > time * unsafe { (*rps).power.up_threshold as i64 } {
            events = GEN6_PM_RP_UP_THRESHOLD
        } else if c0 < time * unsafe { (*rps).power.down_threshold as i64 } {
            events = GEN6_PM_RP_DOWN_THRESHOLD;
        }
    }
    unsafe {
        (*rps).ei = now;
    }
    events
}
// upstream: intel_rps.c rps_work()
unsafe extern "C" fn rps_work(work: *mut WorkStruct) {
    let rps = unsafe { container_of!(work, IntelRps, work) };
    let gt = unsafe { rps_to_gt(rps) };
    let mut client_boost = false;
    let mut pm_iir = 0;
    unsafe {
        spin_lock_irq_raw((*gt).irq_lock);
        pm_iir = core::mem::replace(&mut (*rps).pm_iir, 0) & (*rps).pm_events;
        client_boost = atomic_read(&(*rps).num_waiters) != 0;
        spin_unlock_irq_raw((*gt).irq_lock);
    }
    if pm_iir == 0 && !client_boost {
        unsafe {
            spin_lock_irq_raw((*gt).irq_lock);
            gen6_gt_pm_unmask_irq(gt, (*rps).pm_events);
            spin_unlock_irq_raw((*gt).irq_lock);
        }
        return;
    }
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
        if !rps_is_active(rps) {
            mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
            return;
        }
    }
    unsafe {
        pm_iir |= vlv_wa_c0_ei(rps, pm_iir);
    }
    let mut adj = unsafe { (*rps).last_adj };
    let mut new_freq = unsafe { (*rps).cur_freq as i32 };
    let min = unsafe { (*rps).min_freq_softlimit as i32 };
    let max = unsafe {
        if client_boost {
            (*rps).max_freq as i32
        } else {
            (*rps).max_freq_softlimit as i32
        }
    };
    if client_boost && new_freq < unsafe { (*rps).boost_freq as i32 } {
        new_freq = unsafe { (*rps).boost_freq as i32 };
        adj = 0;
    } else if pm_iir & GEN6_PM_RP_UP_THRESHOLD != 0 {
        if adj > 0 {
            adj *= 2
        } else {
            adj = if unsafe { IS_CHERRYVIEW((*gt).i915) } {
                2
            } else {
                1
            };
        }
        if new_freq >= unsafe { (*rps).max_freq_softlimit as i32 } {
            adj = 0;
        }
    } else if client_boost {
        adj = 0;
    } else if pm_iir & GEN6_PM_RP_DOWN_TIMEOUT != 0 {
        if unsafe { (*rps).cur_freq > (*rps).efficient_freq } {
            new_freq = unsafe { (*rps).efficient_freq as i32 }
        } else if unsafe { (*rps).cur_freq > (*rps).min_freq_softlimit } {
            new_freq = unsafe { (*rps).min_freq_softlimit as i32 }
        }
        adj = 0;
    } else if pm_iir & GEN6_PM_RP_DOWN_THRESHOLD != 0 {
        if adj < 0 {
            adj *= 2
        } else {
            adj = if unsafe { IS_CHERRYVIEW((*gt).i915) } {
                -2
            } else {
                -1
            };
        }
        if new_freq <= unsafe { (*rps).min_freq_softlimit as i32 } {
            adj = 0;
        }
    } else {
        adj = 0;
    }
    new_freq = (new_freq + adj).clamp(min, max);
    if unsafe { intel_rps_set(rps, new_freq as u8) } != 0 {
        drm_dbg!(
            unsafe { ptr::addr_of_mut!((*(*gt).i915).drm) },
            "Failed to set new GPU frequency\n"
        );
        adj = 0;
    }
    unsafe {
        (*rps).last_adj = adj;
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
        spin_lock_irq_raw((*gt).irq_lock);
        gen6_gt_pm_unmask_irq(gt, (*rps).pm_events);
        spin_unlock_irq_raw((*gt).irq_lock);
    }
}
// upstream: intel_rps.c gen11_rps_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_rps_irq_handler(rps: *mut IntelRps, pm_iir: u32) {
    let gt = unsafe { rps_to_gt(rps) };
    let events = unsafe { (*rps).pm_events & pm_iir };
    if events == 0 {
        return;
    }
    unsafe {
        gen6_gt_pm_mask_irq(gt, events);
        (*rps).pm_iir |= events;
        qwork((*(*gt).i915).unordered_wq, ptr::addr_of_mut!((*rps).work));
    }
}
// upstream: intel_rps.c gen6_rps_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_rps_irq_handler(rps: *mut IntelRps, pm_iir: u32) {
    let gt = unsafe { rps_to_gt(rps) };
    let events = unsafe { pm_iir & (*rps).pm_events };
    if events != 0 {
        unsafe {
            spin_lock_irq_raw((*gt).irq_lock);
            gen6_gt_pm_mask_irq(gt, events);
            (*rps).pm_iir |= events;
            qwork((*(*gt).i915).unordered_wq, ptr::addr_of_mut!((*rps).work));
            spin_unlock_irq_raw((*gt).irq_lock);
        }
    }
    if GRAPHICS_VER(unsafe { (*gt).i915 }) >= 8 {
        return;
    }
    if pm_iir & PM_VEBOX_USER_INTERRUPT != 0 {
        unsafe {
            intel_engine_cs_irq((*gt).engine[VECS0 as usize], (pm_iir >> 10) as u16);
        }
    }
    if pm_iir & PM_VEBOX_CS_ERROR_INTERRUPT != 0 {
        drm_dbg!(
            unsafe { ptr::addr_of_mut!((*(*gt).i915).drm) },
            "Command parser error, pm_iir 0x{:08x}\n",
            pm_iir
        );
    }
}
// upstream: intel_rps.c gen5_rps_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen5_rps_irq_handler(rps: *mut IntelRps) {
    let u = unsafe { rps_to_uncore(rps) };
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        wr16(u, MEMINTRSTS, rd16(u, MEMINTRSTS));
        wr16(u, MEMINTRSTS, MEMINT_EVAL_CHG as u16);
    }
    let up = unsafe { rd(u, RCPREVBSYTUPAVG) };
    let down = unsafe { rd(u, RCPREVBSYTDNAVG) };
    let max = unsafe { rd(u, RCBMAXAVG) };
    let min = unsafe { rd(u, RCBMINAVG) };
    let mut f = unsafe { (*rps).cur_freq };
    if up > max {
        f = f.saturating_add(1)
    } else if down < min {
        f = f.saturating_sub(1)
    }
    f = f.clamp(unsafe { (*rps).min_freq_softlimit }, unsafe {
        (*rps).max_freq_softlimit
    });
    if f != unsafe { (*rps).cur_freq } && unsafe { __gen5_rps_set(rps, f) } == 0 {
        unsafe {
            (*rps).cur_freq = f;
        }
    }
    unsafe {
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
    }
}
// upstream: intel_rps.c intel_rps_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_init_early(rps: *mut IntelRps) {
    unsafe {
        mutex_init(ptr::addr_of_mut!((*rps).lock));
        mutex_init(ptr::addr_of_mut!((*rps).power.mutex));
        let _ = timer_setup(ptr::addr_of_mut!((*rps).timer), rps_timer, 0);
        INIT_WORK_C(&mut (*rps).work, rps_work);
        atomic_set(&mut (*rps).num_waiters, 0);
    }
}
// upstream: intel_rps.c intel_rps_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_init(rps: *mut IntelRps) {
    let i = unsafe { rps_to_i915(rps) };
    if unsafe { rps_uses_slpc(rps) } {
        return;
    }
    unsafe {
        if IS_CHERRYVIEW(i) {
            chv_rps_init(rps)
        } else if IS_VALLEYVIEW(i) {
            vlv_rps_init(rps)
        } else if GRAPHICS_VER(i) >= 6 {
            gen6_rps_init(rps)
        } else if is_ironlake_mobile(i) {
            gen5_rps_init(rps)
        } else {
        }
    }
    unsafe {
        (*rps).max_freq_softlimit = (*rps).max_freq;
        (*rps_to_gt(rps)).defaults.max_freq = (*rps).max_freq_softlimit as u32;
        (*rps).min_freq_softlimit = (*rps).min_freq;
        (*rps_to_gt(rps)).defaults.min_freq = (*rps).min_freq_softlimit as u32;
    }
    if GRAPHICS_VER(i) == 6 || unsafe { IS_IVYBRIDGE(i) || IS_HASWELL(i) } {
        let mut params = 0;
        unsafe {
            let _ = snb_pcode_read(
                (*rps_to_gt(rps)).uncore,
                GEN6_READ_OC_PARAMS,
                &mut params,
                ptr::null_mut(),
            );
        }
        if params & bit(31) != 0 {
            unsafe {
                (*rps).max_freq = (params & 0xff) as u8;
            }
        }
    }
    unsafe {
        (*rps).power.up_threshold = 95;
        (*rps_to_gt(rps)).defaults.rps_up_threshold = 95;
        (*rps).power.down_threshold = 85;
        (*rps_to_gt(rps)).defaults.rps_down_threshold = 85;
        (*rps).boost_freq = (*rps).max_freq;
        (*rps).idle_freq = (*rps).min_freq;
        (*rps).cur_freq = (*rps).efficient_freq;
        (*rps).pm_intrmsk_mbz = 0;
        if GRAPHICS_VER(i) <= 7 {
            (*rps).pm_intrmsk_mbz |= GEN6_PM_RP_UP_EI_EXPIRED;
        }
        if (8..11).contains(&GRAPHICS_VER(i)) {
            (*rps).pm_intrmsk_mbz |= GEN8_PMINTR_DISABLE_REDIRECT_TO_GUC;
        }
        if intel_uc_uses_guc_submission(ptr::addr_of_mut!((*rps_to_gt(rps)).uc)) {
            (*rps).pm_intrmsk_mbz |= ARAT_EXPIRED_INTRMSK;
        }
    }
}
// upstream: intel_rps.c intel_rps_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_sanitize(rps: *mut IntelRps) {
    if unsafe { rps_uses_slpc(rps) } {
        return;
    }
    if GRAPHICS_VER(unsafe { rps_to_i915(rps) }) >= 6 {
        unsafe { rps_disable_interrupts(rps) }
    }
}
// upstream: intel_rps.c intel_rps_read_rpstat()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_read_rpstat(rps: *mut IntelRps) -> u32 {
    let i = unsafe { rps_to_i915(rps) };
    unsafe {
        rd(
            (*rps_to_gt(rps)).uncore,
            if GRAPHICS_VER(i) >= 12 {
                GEN12_RPSTAT1
            } else {
                GEN6_RPSTAT1
            },
        )
    }
}
// upstream: intel_rps.c intel_rps_get_cagf()
unsafe fn intel_rps_get_cagf(rps: *mut IntelRps, rpstat: u32) -> u32 {
    let i = unsafe { rps_to_i915(rps) };
    if GRAPHICS_VER_FULL(i) >= IP_VER(12, 70) {
        field_get(MTL_CAGF_MASK, rpstat)
    } else if GRAPHICS_VER(i) >= 12 {
        field_get(GEN12_CAGF_MASK, rpstat)
    } else if unsafe { IS_VALLEYVIEW(i) || IS_CHERRYVIEW(i) } {
        field_get(RPE_MASK, rpstat)
    } else if GRAPHICS_VER(i) >= 9 {
        field_get(GEN9_CAGF_MASK, rpstat)
    } else if unsafe { IS_HASWELL(i) || IS_BROADWELL(i) } {
        field_get(HSW_CAGF_MASK, rpstat)
    } else if GRAPHICS_VER(i) >= 6 {
        field_get(GEN6_CAGF_MASK, rpstat)
    } else {
        unsafe { gen5_invert_freq(rps, field_get(MEMSTAT_PSTATE_MASK, rpstat)) }
    }
}
// upstream: intel_rps.c __read_cagf()
unsafe fn __read_cagf(rps: *mut IntelRps, take_fw: bool) -> u32 {
    let i = unsafe { rps_to_i915(rps) };
    let u = unsafe { rps_to_uncore(rps) };
    let mut reg = I915RegT { reg: 0 };
    let mut freq = 0;
    if GRAPHICS_VER_FULL(i) >= IP_VER(12, 70) {
        reg = MTL_MIRROR_TARGET_WP1;
    } else if GRAPHICS_VER(i) >= 12 {
        reg = GEN12_RPSTAT1;
    } else if unsafe { IS_VALLEYVIEW(i) || IS_CHERRYVIEW(i) } {
        let drm = unsafe { ptr::addr_of_mut!((*i).drm) }.cast();
        unsafe {
            vlv_iosf_sb_get(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
            freq = vlv_iosf_sb_read(drm, IOSF_SB_PUNIT, PUNIT_REG_GPU_FREQ_STS);
            vlv_iosf_sb_put(drm, (bit(IOSF_SB_PUNIT)) as core::ffi::c_ulong);
        }
    } else if GRAPHICS_VER(i) >= 6 {
        reg = GEN6_RPSTAT1;
    } else {
        reg = MEMSTAT_ILK;
    }
    if reg.reg != 0 {
        freq = unsafe { if take_fw { rd(u, reg) } else { rd_fw(u, reg) } };
    }
    unsafe { intel_rps_get_cagf(rps, freq) }
}
// upstream: intel_rps.c read_cagf()
unsafe fn read_cagf(rps: *mut IntelRps) -> u32 {
    unsafe { __read_cagf(rps, true) }
}
// upstream: intel_rps.c intel_rps_read_actual_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_read_actual_frequency(rps: *mut IntelRps) -> u32 {
    let u = unsafe { rps_to_uncore(rps) };
    let rpm = unsafe { (*u).rpm };
    let wakeref = unsafe { intel_runtime_pm_get_if_in_use(rpm) };
    if wakeref.is_null() {
        0
    } else {
        let f = unsafe { intel_gpu_freq(rps, read_cagf(rps) as i32) as u32 };
        unsafe {
            intel_runtime_pm_put_raw(rpm, wakeref);
        }
        f
    }
}
// upstream: intel_rps.c intel_rps_read_actual_frequency_fw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_read_actual_frequency_fw(rps: *mut IntelRps) -> u32 {
    unsafe { intel_gpu_freq(rps, __read_cagf(rps, false) as i32) as u32 }
}
// upstream: intel_rps.c intel_rps_read_punit_req()
unsafe fn intel_rps_read_punit_req(rps: *mut IntelRps) -> u32 {
    let u = unsafe { rps_to_uncore(rps) };
    let rpm = unsafe { (*u).rpm };
    let wakeref = unsafe { intel_runtime_pm_get_if_in_use(rpm) };
    if wakeref.is_null() {
        0
    } else {
        let v = unsafe { rd(u, GEN6_RPNSWREQ) };
        unsafe {
            intel_runtime_pm_put_raw(rpm, wakeref);
        }
        v
    }
}
// upstream: intel_rps.c intel_rps_get_req()
unsafe fn intel_rps_get_req(pureq: u32) -> u32 {
    pureq >> GEN9_SW_REQ_UNSLICE_RATIO_SHIFT
}
// upstream: intel_rps.c intel_rps_read_punit_req_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_read_punit_req_frequency(rps: *mut IntelRps) -> u32 {
    unsafe { intel_gpu_freq(rps, intel_rps_get_req(intel_rps_read_punit_req(rps)) as i32) as u32 }
}
// upstream: intel_rps.c intel_rps_get_requested_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_requested_frequency(rps: *mut IntelRps) -> u32 {
    unsafe {
        if rps_uses_slpc(rps) {
            intel_rps_read_punit_req_frequency(rps)
        } else {
            intel_gpu_freq(rps, (*rps).cur_freq as i32) as u32
        }
    }
}
// upstream: intel_rps.c intel_rps_get_max_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_max_frequency(rps: *mut IntelRps) -> u32 {
    unsafe {
        if rps_uses_slpc(rps) {
            (*rps_to_slpc(rps)).max_freq_softlimit
        } else {
            intel_gpu_freq(rps, (*rps).max_freq_softlimit as i32) as u32
        }
    }
}
// upstream: intel_rps.c intel_rps_get_max_raw_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_max_raw_freq(rps: *mut IntelRps) -> u32 {
    if unsafe { rps_uses_slpc(rps) } {
        return unsafe { (((*rps_to_slpc(rps)).rp0_freq as u32 + 25) / 50) };
    }
    let f = unsafe { (*rps).max_freq as u32 };
    if GRAPHICS_VER(unsafe { rps_to_i915(rps) }) >= 9 {
        f / GEN9_FREQ_SCALER
    } else {
        f
    }
}
// upstream: intel_rps.c intel_rps_get_rp0_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_rp0_frequency(rps: *mut IntelRps) -> u32 {
    unsafe {
        if rps_uses_slpc(rps) {
            (*rps_to_slpc(rps)).rp0_freq
        } else {
            intel_gpu_freq(rps, (*rps).rp0_freq as i32) as u32
        }
    }
}
// upstream: intel_rps.c intel_rps_get_rp1_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_rp1_frequency(rps: *mut IntelRps) -> u32 {
    unsafe {
        if rps_uses_slpc(rps) {
            (*rps_to_slpc(rps)).rp1_freq
        } else {
            intel_gpu_freq(rps, (*rps).rp1_freq as i32) as u32
        }
    }
}
// upstream: intel_rps.c intel_rps_get_rpn_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_rpn_frequency(rps: *mut IntelRps) -> u32 {
    unsafe {
        if rps_uses_slpc(rps) {
            (*rps_to_slpc(rps)).min_freq
        } else {
            intel_gpu_freq(rps, (*rps).min_freq as i32) as u32
        }
    }
}
// upstream: intel_rps.c rps_frequency_dump()
unsafe fn rps_frequency_dump(rps: *mut IntelRps, p: *mut crate::linux::fields::DrmPrinter) {
    let gt = unsafe { rps_to_gt(rps) };
    let i = unsafe { (*gt).i915 };
    let u = unsafe { (*gt).uncore };
    let mut caps = IntelRpsFreqCaps {
        rp0_freq: 0,
        rp1_freq: 0,
        min_freq: 0,
    };
    unsafe {
        let limits = rd(u, GEN6_RP_STATE_LIMITS);
        gen6_rps_get_freq_caps(rps, &mut caps);
        let perf = rd(
            u,
            if IS_GEN9_LP(i) {
                BXT_GT_PERF_STATUS
            } else {
                GEN6_GT_PERF_STATUS
            },
        );
        fw_get(u, FORCEWAKE_ALL);
        let mut req = rd(u, GEN6_RPNSWREQ);
        if GRAPHICS_VER(i) >= 9 {
            req >>= 23
        } else {
            req &= !GEN6_TURBO_DISABLE;
            req >>= if IS_HASWELL(i) || IS_BROADWELL(i) {
                24
            } else {
                25
            };
        }
        req = intel_gpu_freq(rps, req as i32) as u32;
        let rpmodectl = rd(u, GEN6_RP_CONTROL);
        let up = rd(u, GEN6_RP_UP_THRESHOLD);
        let down = rd(u, GEN6_RP_DOWN_THRESHOLD);
        let rpstat = intel_rps_read_rpstat(rps);
        let cur_up_ei = rd(u, GEN6_RP_CUR_UP_EI) & GEN6_CURICONT_MASK;
        let cur_up = rd(u, GEN6_RP_CUR_UP) & GEN6_CURBSYTAVG_MASK;
        let prev_up = rd(u, GEN6_RP_PREV_UP) & GEN6_CURBSYTAVG_MASK;
        let cur_down_ei = rd(u, GEN6_RP_CUR_DOWN_EI) & GEN6_CURIAVG_MASK;
        let cur_down = rd(u, GEN6_RP_CUR_DOWN) & GEN6_CURBSYTAVG_MASK;
        let prev_down = rd(u, GEN6_RP_PREV_DOWN) & GEN6_CURBSYTAVG_MASK;
        let up_ei = rd(u, GEN6_RP_UP_EI);
        let down_ei = rd(u, GEN6_RP_DOWN_EI);
        let cagf = intel_rps_read_actual_frequency(rps);
        fw_put(u, FORCEWAKE_ALL);
        let (ier, imr, isr, iir) = if GRAPHICS_VER(i) >= 11 {
            (
                rd(u, GEN11_GPM_WGBOXPERF_INTR_ENABLE),
                rd(u, GEN11_GPM_WGBOXPERF_INTR_MASK),
                0,
                0,
            )
        } else if GRAPHICS_VER(i) >= 8 {
            (
                rd(u, reg_gen8_gt_ier(2)),
                rd(u, reg_gen8_gt_imr(2)),
                rd(u, reg_gen8_gt_isr(2)),
                rd(u, reg_gen8_gt_iir(2)),
            )
        } else {
            (
                rd(u, GEN6_PMIER),
                rd(u, GEN6_PMIMR),
                rd(u, GEN6_PMISR),
                rd(u, GEN6_PMIIR),
            )
        };
        let pm_mask = rd(u, GEN6_PMINTRMSK);
        drm_printf!(
            p,
            "Video Turbo Mode: %s\n",
            str_yes_no(rpmodectl & GEN6_RP_MEDIA_TURBO != 0)
        );
        drm_printf!(
            p,
            "HW control enabled: %s\n",
            str_yes_no(rpmodectl & GEN6_RP_ENABLE != 0)
        );
        drm_printf!(
            p,
            "SW control enabled: %s\n",
            str_yes_no((rpmodectl & GEN6_RP_MEDIA_MODE_MASK) == GEN6_RP_MEDIA_SW_MODE)
        );
        drm_printf!(
            p,
            "PM IER=0x%08x IMR=0x%08x, MASK=0x%08x\n",
            ier,
            imr,
            pm_mask
        );
        if GRAPHICS_VER(i) <= 10 {
            drm_printf!(p, "PM ISR=0x%08x IIR=0x%08x\n", isr, iir);
        }
        drm_printf!(p, "pm_intrmsk_mbz: 0x%08x\n", (*rps).pm_intrmsk_mbz);
        drm_printf!(p, "GT_PERF_STATUS: 0x%08x\n", perf);
        drm_printf!(
            p,
            "Render p-state ratio: %d\n",
            (perf
                & (if GRAPHICS_VER(i) >= 9 {
                    0x1ff00
                } else {
                    0xff00
                }))
                >> 8
        );
        drm_printf!(p, "Render p-state VID: %d\n", perf & 0xff);
        drm_printf!(p, "Render p-state limit: %d\n", limits & 0xff);
        drm_printf!(p, "RPSTAT1: 0x%08x\n", rpstat);
        drm_printf!(p, "RPMODECTL: 0x%08x\n", rpmodectl);
        drm_printf!(p, "RPINCLIMIT: 0x%08x\n", up);
        drm_printf!(p, "RPDECLIMIT: 0x%08x\n", down);
        drm_printf!(p, "RPNSWREQ: %dMHz\n", req);
        drm_printf!(p, "CAGF: %dMHz\n", cagf);
        drm_printf!(
            p,
            "RP CUR UP EI: %d (%lldns)\n",
            cur_up_ei,
            intel_gt_pm_interval_to_ns(gt, cur_up_ei as u64)
        );
        drm_printf!(
            p,
            "RP CUR UP: %d (%lldns)\n",
            cur_up,
            intel_gt_pm_interval_to_ns(gt, cur_up as u64)
        );
        drm_printf!(
            p,
            "RP PREV UP: %d (%lldns)\n",
            prev_up,
            intel_gt_pm_interval_to_ns(gt, prev_up as u64)
        );
        drm_printf!(p, "Up threshold: %d%%\n", (*rps).power.up_threshold);
        drm_printf!(
            p,
            "RP UP EI: %d (%lldns)\n",
            up_ei,
            intel_gt_pm_interval_to_ns(gt, up_ei as u64)
        );
        drm_printf!(
            p,
            "RP UP THRESHOLD: %d (%lldns)\n",
            up,
            intel_gt_pm_interval_to_ns(gt, up as u64)
        );
        drm_printf!(
            p,
            "RP CUR DOWN EI: %d (%lldns)\n",
            cur_down_ei,
            intel_gt_pm_interval_to_ns(gt, cur_down_ei as u64)
        );
        drm_printf!(
            p,
            "RP CUR DOWN: %d (%lldns)\n",
            cur_down,
            intel_gt_pm_interval_to_ns(gt, cur_down as u64)
        );
        drm_printf!(
            p,
            "RP PREV DOWN: %d (%lldns)\n",
            prev_down,
            intel_gt_pm_interval_to_ns(gt, prev_down as u64)
        );
        drm_printf!(p, "Down threshold: %d%%\n", (*rps).power.down_threshold);
        drm_printf!(
            p,
            "RP DOWN EI: %d (%lldns)\n",
            down_ei,
            intel_gt_pm_interval_to_ns(gt, down_ei as u64)
        );
        drm_printf!(
            p,
            "RP DOWN THRESHOLD: %d (%lldns)\n",
            down,
            intel_gt_pm_interval_to_ns(gt, down as u64)
        );
        drm_printf!(
            p,
            "Lowest (RPN) frequency: %dMHz\n",
            intel_gpu_freq(rps, caps.min_freq as i32)
        );
        drm_printf!(
            p,
            "Nominal (RP1) frequency: %dMHz\n",
            intel_gpu_freq(rps, caps.rp1_freq as i32)
        );
        drm_printf!(
            p,
            "Max non-overclocked (RP0) frequency: %dMHz\n",
            intel_gpu_freq(rps, caps.rp0_freq as i32)
        );
        drm_printf!(
            p,
            "Max overclocked frequency: %dMHz\n",
            intel_gpu_freq(rps, (*rps).max_freq as i32)
        );
        drm_printf!(
            p,
            "Current freq: %d MHz\n",
            intel_gpu_freq(rps, (*rps).cur_freq as i32)
        );
        drm_printf!(p, "Actual freq: %d MHz\n", cagf);
        drm_printf!(
            p,
            "Idle freq: %d MHz\n",
            intel_gpu_freq(rps, (*rps).idle_freq as i32)
        );
        drm_printf!(
            p,
            "Min freq: %d MHz\n",
            intel_gpu_freq(rps, (*rps).min_freq as i32)
        );
        drm_printf!(
            p,
            "Boost freq: %d MHz\n",
            intel_gpu_freq(rps, (*rps).boost_freq as i32)
        );
        drm_printf!(
            p,
            "Max freq: %d MHz\n",
            intel_gpu_freq(rps, (*rps).max_freq as i32)
        );
        drm_printf!(
            p,
            "efficient (RPe) frequency: %d MHz\n",
            intel_gpu_freq(rps, (*rps).efficient_freq as i32)
        );
    }
}
// upstream: intel_rps.c slpc_frequency_dump()
unsafe fn slpc_frequency_dump(rps: *mut IntelRps, p: *mut crate::linux::fields::DrmPrinter) {
    let gt = unsafe { rps_to_gt(rps) };
    let u = unsafe { (*gt).uncore };
    let mut caps = IntelRpsFreqCaps {
        rp0_freq: 0,
        rp1_freq: 0,
        min_freq: 0,
    };
    unsafe {
        gen6_rps_get_freq_caps(rps, &mut caps);
        let mask = rd(u, GEN6_PMINTRMSK);
        drm_printf!(p, "PM MASK=0x%08x\n", mask);
        drm_printf!(p, "pm_intrmsk_mbz: 0x%08x\n", (*rps).pm_intrmsk_mbz);
        drm_printf!(p, "RPSTAT1: 0x%08x\n", intel_rps_read_rpstat(rps));
        drm_printf!(
            p,
            "RPNSWREQ: %dMHz\n",
            intel_rps_get_requested_frequency(rps)
        );
        drm_printf!(
            p,
            "Lowest (RPN) frequency: %dMHz\n",
            intel_gpu_freq(rps, caps.min_freq as i32)
        );
        drm_printf!(
            p,
            "Nominal (RP1) frequency: %dMHz\n",
            intel_gpu_freq(rps, caps.rp1_freq as i32)
        );
        drm_printf!(
            p,
            "Max non-overclocked (RP0) frequency: %dMHz\n",
            intel_gpu_freq(rps, caps.rp0_freq as i32)
        );
        drm_printf!(
            p,
            "Current freq: %d MHz\n",
            intel_rps_get_requested_frequency(rps)
        );
        drm_printf!(
            p,
            "Actual freq: %d MHz\n",
            intel_rps_read_actual_frequency(rps)
        );
        drm_printf!(p, "Min freq: %d MHz\n", intel_rps_get_min_frequency(rps));
        drm_printf!(
            p,
            "Boost freq: %d MHz\n",
            intel_rps_get_boost_frequency(rps)
        );
        drm_printf!(p, "Max freq: %d MHz\n", intel_rps_get_max_frequency(rps));
        drm_printf!(
            p,
            "efficient (RPe) frequency: %d MHz\n",
            intel_gpu_freq(rps, caps.rp1_freq as i32)
        );
    }
}
// upstream: intel_rps.c gen6_rps_frequency_dump()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_rps_frequency_dump(
    rps: *mut IntelRps,
    p: *mut crate::linux::fields::DrmPrinter,
) {
    unsafe {
        if rps_uses_slpc(rps) {
            slpc_frequency_dump(rps, p)
        } else {
            rps_frequency_dump(rps, p)
        }
    }
}
// upstream: intel_rps.c set_max_freq()
unsafe fn set_max_freq(rps: *mut IntelRps, val: u32) -> i32 {
    let i = unsafe { rps_to_i915(rps) };
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
    let mut val = unsafe { intel_freq_opcode(rps, val as i32) };
    if val < unsafe { (*rps).min_freq as i32 }
        || val > unsafe { (*rps).max_freq as i32 }
        || val < unsafe { (*rps).min_freq_softlimit as i32 }
    {
        unsafe {
            mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
        }
        return -22;
    }
    if val > unsafe { (*rps).rp0_freq as i32 } {
        drm_dbg!(
            unsafe { ptr::addr_of_mut!((*i).drm) },
            "User requested overclocking to %d\n",
            unsafe { intel_gpu_freq(rps, val) }
        );
    }
    unsafe {
        (*rps).max_freq_softlimit = val as u8;
        val = ((*rps).cur_freq as i32).clamp(
            (*rps).min_freq_softlimit as i32,
            (*rps).max_freq_softlimit as i32,
        );
        let _ = intel_rps_set(rps, val as u8);
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
    0
}
// upstream: intel_rps.c intel_rps_set_max_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_set_max_frequency(rps: *mut IntelRps, val: u32) -> i32 {
    unsafe {
        if rps_uses_slpc(rps) {
            crate::intel_guc_slpc_upstream::intel_guc_slpc_set_max_freq(rps_to_slpc(rps), val)
        } else {
            set_max_freq(rps, val)
        }
    }
}
// upstream: intel_rps.c intel_rps_get_min_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_min_frequency(rps: *mut IntelRps) -> u32 {
    unsafe {
        if rps_uses_slpc(rps) {
            (*rps_to_slpc(rps)).min_freq_softlimit
        } else {
            intel_gpu_freq(rps, (*rps).min_freq_softlimit as i32) as u32
        }
    }
}
// upstream: intel_rps.c intel_rps_get_min_raw_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_min_raw_freq(rps: *mut IntelRps) -> u32 {
    if unsafe { rps_uses_slpc(rps) } {
        return unsafe { ((*rps_to_slpc(rps)).min_freq as u32 + 25) / 50 };
    }
    let f = unsafe { (*rps).min_freq as u32 };
    if GRAPHICS_VER(unsafe { rps_to_i915(rps) }) >= 9 {
        f / GEN9_FREQ_SCALER
    } else {
        f
    }
}
// upstream: intel_rps.c set_min_freq()
unsafe fn set_min_freq(rps: *mut IntelRps, val: u32) -> i32 {
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
    let mut val = unsafe { intel_freq_opcode(rps, val as i32) };
    if val < unsafe { (*rps).min_freq as i32 }
        || val > unsafe { (*rps).max_freq as i32 }
        || val > unsafe { (*rps).max_freq_softlimit as i32 }
    {
        unsafe {
            mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
        }
        return -22;
    }
    unsafe {
        (*rps).min_freq_softlimit = val as u8;
        val = ((*rps).cur_freq as i32).clamp(
            (*rps).min_freq_softlimit as i32,
            (*rps).max_freq_softlimit as i32,
        );
        let _ = intel_rps_set(rps, val as u8);
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
    0
}
// upstream: intel_rps.c intel_rps_set_min_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_set_min_frequency(rps: *mut IntelRps, val: u32) -> i32 {
    unsafe {
        if rps_uses_slpc(rps) {
            crate::intel_guc_slpc_upstream::intel_guc_slpc_set_min_freq(rps_to_slpc(rps), val)
        } else {
            set_min_freq(rps, val)
        }
    }
}
// upstream: intel_rps.c intel_rps_get_up_threshold()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_up_threshold(rps: *mut IntelRps) -> u8 {
    unsafe { (*rps).power.up_threshold }
}
// upstream: intel_rps.c rps_set_threshold()
unsafe fn rps_set_threshold(rps: *mut IntelRps, threshold: *mut u8, val: u8) -> i32 {
    if val > 100 {
        return -22;
    }
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
    if unsafe { *threshold == val } {
        unsafe {
            mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
        }
        return 0;
    }
    unsafe {
        *threshold = val;
        (*rps).last_freq = u8::MAX;
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).power.mutex));
        (*rps).power.mode = -1;
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).power.mutex));
        let f = (*rps)
            .cur_freq
            .clamp((*rps).min_freq_softlimit, (*rps).max_freq_softlimit);
        let _ = intel_rps_set(rps, f);
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
    0
}
// upstream: intel_rps.c intel_rps_set_up_threshold()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_set_up_threshold(rps: *mut IntelRps, val: u8) -> i32 {
    unsafe { rps_set_threshold(rps, ptr::addr_of_mut!((*rps).power.up_threshold), val) }
}
// upstream: intel_rps.c intel_rps_get_down_threshold()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_get_down_threshold(rps: *mut IntelRps) -> u8 {
    unsafe { (*rps).power.down_threshold }
}
// upstream: intel_rps.c intel_rps_set_down_threshold()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_set_down_threshold(rps: *mut IntelRps, val: u8) -> i32 {
    unsafe { rps_set_threshold(rps, ptr::addr_of_mut!((*rps).power.down_threshold), val) }
}
// upstream: intel_rps.c intel_rps_set_manual()
unsafe fn intel_rps_set_manual(rps: *mut IntelRps, enable: bool) {
    unsafe {
        wr(
            rps_to_uncore(rps),
            GEN6_RP_CONTROL,
            if enable {
                GEN9_RPSWCTL_ENABLE
            } else {
                GEN9_RPSWCTL_DISABLE
            },
        )
    }
}
// upstream: intel_rps.c intel_rps_raise_unslice()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_raise_unslice(rps: *mut IntelRps) {
    let u = unsafe { rps_to_uncore(rps) };
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
        if rps_uses_slpc(rps) {
            let mut c = IntelRpsFreqCaps {
                rp0_freq: 0,
                rp1_freq: 0,
                min_freq: 0,
            };
            gen6_rps_get_freq_caps(rps, &mut c);
            intel_rps_set_manual(rps, true);
            wr(
                u,
                GEN6_RPNSWREQ,
                ((c.rp0_freq as u32) << GEN9_SW_REQ_UNSLICE_RATIO_SHIFT) | GEN9_IGNORE_SLICE_RATIO,
            );
            intel_rps_set_manual(rps, false);
        } else {
            let _ = intel_rps_set(rps, (*rps).rp0_freq);
        }
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
}
// upstream: intel_rps.c intel_rps_lower_unslice()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_lower_unslice(rps: *mut IntelRps) {
    let u = unsafe { rps_to_uncore(rps) };
    unsafe {
        mutex_lock_ptr(ptr::addr_of_mut!((*rps).lock));
        if rps_uses_slpc(rps) {
            let mut c = IntelRpsFreqCaps {
                rp0_freq: 0,
                rp1_freq: 0,
                min_freq: 0,
            };
            gen6_rps_get_freq_caps(rps, &mut c);
            intel_rps_set_manual(rps, true);
            wr(
                u,
                GEN6_RPNSWREQ,
                ((c.min_freq as u32) << GEN9_SW_REQ_UNSLICE_RATIO_SHIFT) | GEN9_IGNORE_SLICE_RATIO,
            );
            intel_rps_set_manual(rps, false);
        } else {
            let _ = intel_rps_set(rps, (*rps).min_freq);
        }
        mutex_unlock_ptr(ptr::addr_of_mut!((*rps).lock));
    }
}
// upstream: intel_rps.c rps_read_mmio()
unsafe fn rps_read_mmio(rps: *mut IntelRps, reg: I915RegT) -> u32 {
    let gt = unsafe { rps_to_gt(rps) };
    let rpm = unsafe { (*(*gt).uncore).rpm };
    let wakeref = crate::linux_pm::intel_runtime_pm_get(rpm);
    let value = unsafe { rd((*gt).uncore, reg) };
    crate::linux_pm::intel_runtime_pm_put(rpm, wakeref);
    value
}
// upstream: intel_rps.c rps_read_mask_mmio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rps_read_mask_mmio(rps: *mut IntelRps, reg: I915RegT, mask: u32) -> bool {
    unsafe { rps_read_mmio(rps, reg) & mask != 0 }
}
// upstream: intel_rps.c ips_ping_for_i915_load()
unsafe fn ips_ping_for_i915_load() {
    unsafe extern "C" {
        fn __symbol_get(name: *const i8) -> *mut c_void;
        fn __symbol_put(name: *const i8);
    }
    let name = b"ips_link_to_i915_driver\0";
    let f = unsafe { __symbol_get(name.as_ptr().cast()) };
    if !f.is_null() {
        let link: unsafe extern "C" fn() = unsafe { core::mem::transmute(f) };
        unsafe {
            link();
            __symbol_put(name.as_ptr().cast());
        }
    }
}
// upstream: intel_rps.c intel_rps_driver_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_driver_register(rps: *mut IntelRps) {
    let gt = unsafe { rps_to_gt(rps) };
    if GRAPHICS_VER(unsafe { (*gt).i915 }) == 5 {
        unsafe {
            assert!(IPS_MCHDEV.is_null());
            rcu_assign_pointer!(&raw mut IPS_MCHDEV, (*gt).i915);
            ips_ping_for_i915_load();
        }
    }
}
// upstream: intel_rps.c intel_rps_driver_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rps_driver_unregister(rps: *mut IntelRps) {
    unsafe {
        if IPS_MCHDEV == rps_to_i915(rps) {
            rcu_assign_pointer!(&raw mut IPS_MCHDEV, ptr::null_mut::<DrmI915Private>());
        }
    }
}
// upstream: intel_rps.c mchdev_get()
unsafe fn mchdev_get() -> *mut DrmI915Private {
    unsafe {
        rcu_read_lock();
        let i = rcu_dereference!(IPS_MCHDEV);
        let got = !i.is_null()
            && crate::linux_memory::kref_get_unless_zero(
                &mut *((ptr::addr_of_mut!((*i).drm).cast::<u8>().add(8))
                    .cast::<crate::intel_context_upstream::Kref>()),
            );
        rcu_read_unlock();
        if got { i } else { ptr::null_mut() }
    }
}
// upstream: intel_rps.c i915_read_mch_val()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_read_mch_val() -> c_ulong {
    let i = unsafe { mchdev_get() };
    if i.is_null() {
        return 0;
    }
    let rpm = unsafe { ptr::addr_of_mut!((*i).runtime_pm) };
    let wakeref = crate::linux_pm::intel_runtime_pm_get(rpm);
    let gt = unsafe { (*i).gt[0] };
    let ips = unsafe { ptr::addr_of_mut!((*gt).rps.ips) };
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        let chipset = __ips_chipset_val(ips);
        let graphics = __ips_gfx_val(ips);
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        crate::linux_pm::intel_runtime_pm_put(rpm, wakeref);
        crate::linux::gem::drm_dev_put(ptr::addr_of_mut!((*i).drm).cast());
        chipset + graphics
    }
}
// upstream: intel_rps.c i915_gpu_raise()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gpu_raise() -> bool {
    let i = unsafe { mchdev_get() };
    if i.is_null() {
        return false;
    }
    let r = unsafe { ptr::addr_of_mut!((*(*i).gt[0]).rps) };
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        if (*r).max_freq_softlimit < (*r).max_freq {
            (*r).max_freq_softlimit += 1;
        }
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        crate::linux::gem::drm_dev_put(ptr::addr_of_mut!((*i).drm).cast());
    }
    true
}
// upstream: intel_rps.c i915_gpu_lower()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gpu_lower() -> bool {
    let i = unsafe { mchdev_get() };
    if i.is_null() {
        return false;
    }
    let r = unsafe { ptr::addr_of_mut!((*(*i).gt[0]).rps) };
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        if (*r).max_freq_softlimit > (*r).min_freq {
            (*r).max_freq_softlimit -= 1;
        }
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        crate::linux::gem::drm_dev_put(ptr::addr_of_mut!((*i).drm).cast());
    }
    true
}
// upstream: intel_rps.c i915_gpu_busy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gpu_busy() -> bool {
    let i = unsafe { mchdev_get() };
    if i.is_null() {
        return false;
    }
    let gt = unsafe { (*i).gt[0] };
    let awake = unsafe { !(*gt).awake.is_null() };
    unsafe {
        crate::linux::gem::drm_dev_put(ptr::addr_of_mut!((*i).drm).cast());
        awake
    }
}
// upstream: intel_rps.c i915_gpu_turbo_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gpu_turbo_disable() -> bool {
    let i = unsafe { mchdev_get() };
    if i.is_null() {
        return false;
    }
    let r = unsafe { ptr::addr_of_mut!((*(*i).gt[0]).rps) };
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        (*r).max_freq_softlimit = (*r).min_freq;
        let ret = __gen5_rps_set(r, (*r).min_freq) == 0;
        spin_unlock_irq_raw(ptr::addr_of_mut!(MCHDEV_LOCK));
        crate::linux::gem::drm_dev_put(ptr::addr_of_mut!((*i).drm).cast());
        ret
    }
}
// upstream: intel_rps.c boost_if_not_started()
unsafe extern "C" fn boost_if_not_started(fence: *mut crate::intel_context_upstream::DmaFence) {
    if unsafe {
        !((*fence).ops == ptr::addr_of!(crate::i915_request_upstream::i915_fence_ops).cast())
    } {
        return;
    }
    let rq = unsafe { crate::i915_request_upstream::to_request(fence) };
    if !unsafe { crate::linux::requests::i915_request_started(rq) } {
        unsafe { intel_rps_boost(rq) }
    }
}
// upstream: intel_rps.c mark_interactive()
unsafe extern "C" fn mark_interactive(drm: *mut c_void, interactive: bool) {
    let i = unsafe {
        container_of!(
            drm.cast::<crate::linux::gem::DrmDevice>(),
            DrmI915Private,
            drm
        )
    };
    unsafe { intel_rps_mark_interactive(ptr::addr_of_mut!((*(*i).gt[0]).rps), interactive) }
}
// upstream: intel_rps.c ilk_irq_handler()
unsafe extern "C" fn ilk_irq_handler(drm: *mut c_void) {
    let i = unsafe {
        container_of!(
            drm.cast::<crate::linux::gem::DrmDevice>(),
            DrmI915Private,
            drm
        )
    };
    unsafe {
        gen5_rps_irq_handler(ptr::addr_of_mut!((*(*i).gt[0]).rps));
    }
}

#[repr(C)]
pub struct IntelDisplayRpsInterface {
    pub boost_if_not_started:
        Option<unsafe extern "C" fn(*mut crate::intel_context_upstream::DmaFence)>,
    pub mark_interactive: Option<unsafe extern "C" fn(*mut c_void, bool)>,
    pub ilk_irq_handler: Option<unsafe extern "C" fn(*mut c_void)>,
}
#[unsafe(no_mangle)]
pub static i915_display_rps_interface: IntelDisplayRpsInterface = IntelDisplayRpsInterface {
    boost_if_not_started: Some(boost_if_not_started),
    mark_interactive: Some(mark_interactive),
    ilk_irq_handler: Some(ilk_irq_handler),
};
