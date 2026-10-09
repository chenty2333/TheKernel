// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
// Source-order Rust translation of Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_rc6.c. Register programming and power/display
// interfaces below retain their hardware-facing owners; no successful MMIO,
// power, or display behavior is synthesized here.
#![allow(
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    unexpected_cfgs
)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo},
    i915_gem_region_upstream::i915_gem_object_create_region_at,
    intel_context_types_upstream::IntelRefTracker,
    intel_engine_api_upstream::HAS_ENGINE,
    intel_engine_regs_upstream::{IDLE_TIME_MASK, PWRCTX_MAXCNT},
    intel_engine_types_upstream::{_VCS, I915_MAX_VCS},
    intel_gt_api_upstream::gt_to_guc,
    intel_gt_types_upstream::{GT_MEDIA, IntelGt},
    intel_guc_rc_types_upstream::{intel_guc_rc_disable, intel_guc_rc_enable},
    intel_rc6_types_upstream::{
        INTEL_RC6_RES_MAX, INTEL_RC6_RES_RC6, INTEL_RC6_RES_RC6_LOCKED, INTEL_RC6_RES_RC6p,
        INTEL_RC6_RES_RC6pp, IntelRc6, IntelRc6ResType,
    },
    intel_sseu_types_upstream::SeqFile,
    intel_uncore_types_upstream::{
        FORCEWAKE_ALL, FW_REG_READ, IntelUncore, intel_uncore_forcewake_for_reg,
        intel_uncore_forcewake_get, intel_uncore_forcewake_get__locked, intel_uncore_forcewake_put,
        intel_uncore_forcewake_put__locked, intel_uncore_read, intel_uncore_read_fw,
        intel_uncore_write, intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        gem_memory::Resource,
        i915::{
            GRAPHICS_VER, INTEL_INFO, IP_VER, IS_BROADWELL, IS_CHERRYVIEW, IS_DG1, IS_GEN9_LP,
            IS_METEORLAKE, IS_SKYLAKE, IS_VALLEYVIEW,
        },
        i915_private::{DrmDevicePrefix, DrmI915Private},
        locks::spin_lock_irqsave,
        memory::kref_read,
        primitives::fetch_and_zero,
        registers::{BLT_RING_BASE, GEN6_BSD_RING_BASE, RENDER_RING_BASE, VEBOX_RING_BASE},
    },
    linux_config::{self, ENODEV, PAGE_SIZE},
    linux_locks::spin_unlock_irqrestore,
};

const fn mmio(reg: u32) -> I915RegT {
    I915RegT { reg }
}

// Register definitions from Linux 7.2.3 i915/GT headers used by this file.
const GEN6_RC_CONTROL: I915RegT = mmio(0x0a090);
const GEN6_RC_STATE: I915RegT = mmio(0x0a094);
const GEN6_RC1_WAKE_RATE_LIMIT: I915RegT = mmio(0x0a098);
const GEN6_RC6_WAKE_RATE_LIMIT: I915RegT = mmio(0x0a09c);
const GEN6_RC6PP_WAKE_RATE_LIMIT: I915RegT = mmio(0x0a0a0);
const GEN10_MEDIA_WAKE_RATE_LIMIT: I915RegT = mmio(0x0a0a0);
const GEN6_RC_EVALUATION_INTERVAL: I915RegT = mmio(0x0a0a8);
const GEN6_RC_IDLE_HYSTERSIS: I915RegT = mmio(0x0a0ac);
const GEN6_RC_SLEEP: I915RegT = mmio(0x0a0b0);
const GEN6_RC1E_THRESHOLD: I915RegT = mmio(0x0a0b4);
const GEN6_RC6_THRESHOLD: I915RegT = mmio(0x0a0b8);
const GEN6_RC6P_THRESHOLD: I915RegT = mmio(0x0a0bc);
const GEN6_RC6PP_THRESHOLD: I915RegT = mmio(0x0a0c0);
const GEN9_MEDIA_PG_IDLE_HYSTERESIS: I915RegT = mmio(0x0a0c4);
const GEN9_RENDER_PG_IDLE_HYSTERESIS: I915RegT = mmio(0x0a0c8);
const GEN9_PG_ENABLE: I915RegT = mmio(0x0a210);
const GEN8_PUSHBUS_CONTROL: I915RegT = mmio(0x0a248);
const GEN8_PUSHBUS_ENABLE: I915RegT = mmio(0x0a250);
const GEN8_PUSHBUS_SHIFT: I915RegT = mmio(0x0a25c);
const GEN6_GFXPAUSE: I915RegT = mmio(0x0a000);
const GEN8_MISC_CTRL0: I915RegT = mmio(0x0a180);
const RC6_LOCATION: I915RegT = mmio(0x00d40);
const RC6_CTX_BASE: I915RegT = mmio(0x00d48);
const GEN8_RC6_CTX_INFO: I915RegT = mmio(0x08504);
const VLV_PCBR: I915RegT = mmio(0x182120);
const VLV_COUNTER_CONTROL: I915RegT = mmio(0x138104);
const GEN6_GT_GFX_RC6_LOCKED: I915RegT = mmio(0x138104);
const GEN6_GT_GFX_RC6: I915RegT = mmio(0x138108);
const GEN6_GT_GFX_RC6P: I915RegT = mmio(0x13810c);
const GEN6_GT_GFX_RC6PP: I915RegT = mmio(0x138110);
const MTL_MEDIA_MC6: I915RegT = mmio(0x138048);
const GUC_MAX_IDLE_COUNT: I915RegT = mmio(0x00c3e4);
const GEN6_RC_CTL_RC6PP_ENABLE: u32 = 1 << 16;
const GEN6_RC_CTL_RC6P_ENABLE: u32 = 1 << 17;
const GEN6_RC_CTL_RC6_ENABLE: u32 = 1 << 18;
const GEN7_RC_CTL_TO_MODE: u32 = 1 << 28;
const GEN6_RC_CTL_EI_MODE_1: u32 = 1 << 27;
const GEN6_RC_CTL_HW_ENABLE: u32 = 1 << 31;
const VLV_RC_CTL_CTX_RST_PARALLEL: u32 = 1 << 24;
const GEN9_RENDER_PG_ENABLE: u32 = 1 << 0;
const GEN9_MEDIA_PG_ENABLE: u32 = 1 << 1;
const GEN11_MEDIA_SAMPLER_PG_ENABLE: u32 = 1 << 2;
const VLV_PCBR_ADDR_SHIFT: u32 = 12;
const VLV_COUNT_RANGE_HIGH: u32 = 1 << 15;
const VLV_MEDIA_RC0_COUNT_EN: u32 = 1 << 5;
const VLV_RENDER_RC0_COUNT_EN: u32 = 1 << 4;
const VLV_MEDIA_RC6_COUNT_EN: u32 = 1 << 1;
const VLV_RENDER_RC6_COUNT_EN: u32 = 1 << 0;
const RC6_CTX_IN_DRAM: u32 = 1 << 0;
const RC6_CTX_BASE_MASK: u32 = 0xffff_fff0;
const RC_SW_TARGET_STATE_SHIFT: u32 = 16;
const RC_SW_TARGET_STATE_MASK: u32 = 7 << RC_SW_TARGET_STATE_SHIFT;
const GEN6_PCODE_WRITE_RC6VIDS: u32 = 0x4;
const GEN6_PCODE_READ_RC6VIDS: u32 = 0x5;
const SZ_1K: u64 = 1024;
const I915_DSM_OFFSET: usize = 1704;
const U32_MAX: u64 = u32::MAX as u64;
const RC6_SUPPORTED_BIT: u8 = 1 << 0;
const RC6_ENABLED_BIT: u8 = 1 << 1;
const RC6_MANUAL_BIT: u8 = 1 << 2;
const RC6_WAKEREF_BIT: u8 = 1 << 3;
const RC6_BIOS_STATE_CAPTURED_BIT: u8 = 1 << 4;

#[repr(C)]
struct I915Dsm {
    stolen: Resource,
    reserved: Resource,
    usable_size: u64,
}
const _: [(); 136] = [(); size_of::<I915Dsm>()];
const _: [(); 64] = [(); offset_of!(I915Dsm, reserved)];

#[repr(C)]
struct SeqFilePrivateView {
    _before_private: [u8; 104],
    private: *mut c_void,
}
const _: [(); 112] = [(); size_of::<SeqFilePrivateView>()];
const _: [(); 104] = [(); offset_of!(SeqFilePrivateView, private)];

#[inline]
unsafe fn rc6_get(rc6: *const IntelRc6, bit: u8) -> bool {
    unsafe { (*rc6).state_bits & bit != 0 }
}
#[inline]
unsafe fn rc6_set(rc6: *mut IntelRc6, bit: u8, enabled: bool) {
    unsafe {
        if enabled {
            (*rc6).state_bits |= bit;
        } else {
            (*rc6).state_bits &= !bit;
        }
    }
}
#[inline]
unsafe fn dsm(i915: *mut DrmI915Private) -> *mut I915Dsm {
    unsafe { i915.cast::<u8>().add(I915_DSM_OFFSET).cast() }
}
#[inline]
unsafe fn has_rc6(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[3] & (1 << 4) != 0 }
}
#[inline]
unsafe fn has_rc6p(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[3] & (1 << 5) != 0 }
}
#[inline]
unsafe fn has_rc6pp(_i915: *mut DrmI915Private) -> bool {
    false
}
#[inline]
unsafe fn needs_rc6_ctx_corruption_wa(i915: *mut DrmI915Private) -> bool {
    unsafe { IS_BROADWELL(i915) || GRAPHICS_VER(i915) == 9 }
}
#[inline]
unsafe fn needs_disable_coarse_pg(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    unsafe { IS_SKYLAKE(i915) && !info.is_null() && ((*info).gt == 3 || (*info).gt == 4) }
}
#[inline]
unsafe fn is_mock_gt(gt: *const IntelGt) -> bool {
    if !linux_config::CONFIG_DRM_I915_SELFTEST {
        return false;
    }
    unsafe { (*gt).awake == linux_config::ERR_PTR::<IntelRefTracker>(-linux_config::ENODEV) }
}
#[inline]
fn range_end_overflows(start: u64, size: u64, max: u64) -> bool {
    start > max || size > max.wrapping_sub(start)
}
#[inline]
fn gen6_decode_rc6_vid(vid: u32) -> u32 {
    vid * 5 + 245
}
#[inline]
fn gen6_encode_rc6_vid(mv: u32) -> u32 {
    (mv - 245) / 5
}
#[inline]
fn mul_u64_u32_div(value: u64, mul: u32, divisor: u32) -> u64 {
    ((value as u128 * mul as u128) / divisor as u128) as u64
}
#[inline]
fn div_round_up_ull(value: u64, divisor: u64) -> u64 {
    value.wrapping_add(divisor - 1) / divisor
}
#[inline]
fn str_on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

unsafe extern "C" {
    fn intel_vgpu_active(i915: *mut DrmI915Private) -> bool;
    fn pm_runtime_get_sync(dev: *mut c_void) -> c_int;
    fn pm_runtime_put(dev: *mut c_void) -> c_int;
    fn vlv_clock_get_czclk(drm: *mut DrmDevicePrefix) -> c_int;
    fn snb_pcode_read(uncore: *mut IntelUncore, mbox: u32, val: *mut u32, val1: *mut u32) -> c_int;
    fn snb_pcode_write_timeout(
        uncore: *mut IntelUncore,
        mbox: u32,
        val: u32,
        timeout_ms: c_int,
    ) -> c_int;
    fn i915_gem_object_create_stolen(i915: *mut DrmI915Private, size: u64)
    -> *mut DrmI915GemObject;
    fn seq_printf(m: *mut SeqFile, format: *const c_char, ...) -> c_int;
}

unsafe fn snb_pcode_write(uncore: *mut IntelUncore, mbox: u32, val: u32) -> c_int {
    unsafe { snb_pcode_write_timeout(uncore, mbox, val, 1) }
}

// upstream: intel_rc6.c rc6_to_gt()
unsafe fn rc6_to_gt(rc6: *mut IntelRc6) -> *mut IntelGt {
    unsafe { container_of!(rc6, IntelGt, rc6) }
}
// upstream: intel_rc6.c rc6_to_uncore()
unsafe fn rc6_to_uncore(rc6: *mut IntelRc6) -> *mut IntelUncore {
    unsafe { (*rc6_to_gt(rc6)).uncore }
}
// upstream: intel_rc6.c rc6_to_i915()
unsafe fn rc6_to_i915(rc6: *mut IntelRc6) -> *mut DrmI915Private {
    unsafe { (*rc6_to_gt(rc6)).i915 }
}

// upstream: intel_rc6.c gen11_rc6_enable()
unsafe fn gen11_rc6_enable(rc6: *mut IntelRc6) {
    let gt = unsafe { rc6_to_gt(rc6) };
    let uncore = unsafe { (*gt).uncore };
    if !unsafe { crate::intel_uc_types_upstream::intel_uc_uses_guc_rc(ptr::addr_of_mut!((*gt).uc)) }
    {
        unsafe {
            intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, (54 << 16) | 85);
            intel_uncore_write_fw(uncore, GEN10_MEDIA_WAKE_RATE_LIMIT, 150);
            intel_uncore_write_fw(uncore, GEN6_RC_EVALUATION_INTERVAL, 125000);
            intel_uncore_write_fw(uncore, GEN6_RC_IDLE_HYSTERSIS, 25);
        }
        let mut engine: *mut crate::intel_engine_cs_upstream::IntelEngineCs = ptr::null_mut();
        let mut id = 0;
        for_each_engine!(engine, id, gt, {
            unsafe {
                intel_uncore_write_fw(
                    uncore,
                    crate::intel_engine_regs_upstream::RING_MAX_IDLE((*engine).mmio_base),
                    10,
                );
            }
        });
        unsafe {
            intel_uncore_write_fw(uncore, GUC_MAX_IDLE_COUNT, 0xA);
            intel_uncore_write_fw(uncore, GEN6_RC_SLEEP, 0);
            intel_uncore_write_fw(uncore, GEN6_RC6_THRESHOLD, 50000);
        }
    }

    unsafe {
        intel_uncore_write_fw(uncore, GEN9_MEDIA_PG_IDLE_HYSTERESIS, 60);
        intel_uncore_write_fw(uncore, GEN9_RENDER_PG_IDLE_HYSTERESIS, 60);
        (*rc6).ctl_enable = if intel_guc_rc_enable(gt_to_guc(gt)) == 0 {
            GEN6_RC_CTL_RC6_ENABLE
        } else {
            GEN6_RC_CTL_HW_ENABLE | GEN6_RC_CTL_RC6_ENABLE | GEN6_RC_CTL_EI_MODE_1
        };
    }

    let i915 = unsafe { (*gt).i915 };
    let mut pg_enable =
        GEN9_RENDER_PG_ENABLE | GEN9_MEDIA_PG_ENABLE | GEN11_MEDIA_SAMPLER_PG_ENABLE;
    if unsafe { GRAPHICS_VER(i915) >= 12 && !IS_DG1(i915) } {
        for i in 0..I915_MAX_VCS {
            if unsafe { HAS_ENGINE(gt, _VCS(i as i32) as u32) } != 0 {
                pg_enable |= (1 << (3 + 2 * i)) | (1 << (4 + 2 * i));
            }
        }
    }
    unsafe { intel_uncore_write_fw(uncore, GEN9_PG_ENABLE, pg_enable) };
}

// upstream: intel_rc6.c gen9_rc6_enable()
unsafe fn gen9_rc6_enable(rc6: *mut IntelRc6) {
    let i915 = unsafe { rc6_to_i915(rc6) };
    let uncore = unsafe { rc6_to_uncore(rc6) };
    if unsafe { GRAPHICS_VER(i915) >= 11 } {
        unsafe {
            intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, (54 << 16) | 85);
            intel_uncore_write_fw(uncore, GEN10_MEDIA_WAKE_RATE_LIMIT, 150);
        }
    } else if unsafe { IS_SKYLAKE(i915) } {
        unsafe { intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, 108 << 16) };
    } else {
        unsafe { intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, 54 << 16) };
    }

    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC_EVALUATION_INTERVAL, 125000);
        intel_uncore_write_fw(uncore, GEN6_RC_IDLE_HYSTERSIS, 25);
    }
    let gt = unsafe { rc6_to_gt(rc6) };
    let mut engine: *mut crate::intel_engine_cs_upstream::IntelEngineCs = ptr::null_mut();
    let mut id = 0;
    for_each_engine!(engine, id, gt, {
        unsafe {
            intel_uncore_write_fw(
                uncore,
                crate::intel_engine_regs_upstream::RING_MAX_IDLE((*engine).mmio_base),
                10,
            )
        };
    });
    unsafe {
        intel_uncore_write_fw(uncore, GUC_MAX_IDLE_COUNT, 0xA);
        intel_uncore_write_fw(uncore, GEN6_RC_SLEEP, 0);
        intel_uncore_write_fw(uncore, GEN9_MEDIA_PG_IDLE_HYSTERESIS, 250);
        intel_uncore_write_fw(uncore, GEN9_RENDER_PG_IDLE_HYSTERESIS, 250);
        intel_uncore_write_fw(uncore, GEN6_RC6_THRESHOLD, 37500);
        (*rc6).ctl_enable = GEN6_RC_CTL_HW_ENABLE | GEN6_RC_CTL_RC6_ENABLE | GEN6_RC_CTL_EI_MODE_1;
    }
    if !unsafe { needs_disable_coarse_pg(i915) } {
        unsafe {
            intel_uncore_write_fw(
                uncore,
                GEN9_PG_ENABLE,
                GEN9_RENDER_PG_ENABLE | GEN9_MEDIA_PG_ENABLE,
            )
        };
    }
}

// upstream: intel_rc6.c gen8_rc6_enable()
unsafe fn gen8_rc6_enable(rc6: *mut IntelRc6) {
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let gt = unsafe { rc6_to_gt(rc6) };
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, 40 << 16);
        intel_uncore_write_fw(uncore, GEN6_RC_EVALUATION_INTERVAL, 125000);
        intel_uncore_write_fw(uncore, GEN6_RC_IDLE_HYSTERSIS, 25);
    }
    let mut engine: *mut crate::intel_engine_cs_upstream::IntelEngineCs = ptr::null_mut();
    let mut id = 0;
    for_each_engine!(engine, id, gt, {
        unsafe {
            intel_uncore_write_fw(
                uncore,
                crate::intel_engine_regs_upstream::RING_MAX_IDLE((*engine).mmio_base),
                10,
            )
        };
    });
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC_SLEEP, 0);
        intel_uncore_write_fw(uncore, GEN6_RC6_THRESHOLD, 625);
        (*rc6).ctl_enable = GEN6_RC_CTL_HW_ENABLE | GEN7_RC_CTL_TO_MODE | GEN6_RC_CTL_RC6_ENABLE;
    }
}

// upstream: intel_rc6.c gen6_rc6_enable()
unsafe fn gen6_rc6_enable(rc6: *mut IntelRc6) {
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let i915 = unsafe { rc6_to_i915(rc6) };
    let gt = unsafe { rc6_to_gt(rc6) };
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC1_WAKE_RATE_LIMIT, 1000 << 16);
        intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, (40 << 16) | 30);
        intel_uncore_write_fw(uncore, GEN6_RC6PP_WAKE_RATE_LIMIT, 30);
        intel_uncore_write_fw(uncore, GEN6_RC_EVALUATION_INTERVAL, 125000);
        intel_uncore_write_fw(uncore, GEN6_RC_IDLE_HYSTERSIS, 25);
    }
    let mut engine: *mut crate::intel_engine_cs_upstream::IntelEngineCs = ptr::null_mut();
    let mut id = 0;
    for_each_engine!(engine, id, gt, {
        unsafe {
            intel_uncore_write_fw(
                uncore,
                crate::intel_engine_regs_upstream::RING_MAX_IDLE((*engine).mmio_base),
                10,
            )
        };
    });
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC_SLEEP, 0);
        intel_uncore_write_fw(uncore, GEN6_RC1E_THRESHOLD, 1000);
        intel_uncore_write_fw(uncore, GEN6_RC6_THRESHOLD, 50000);
        intel_uncore_write_fw(uncore, GEN6_RC6P_THRESHOLD, 150000);
        intel_uncore_write_fw(uncore, GEN6_RC6PP_THRESHOLD, 64000);
    }

    let mut rc6_mask = GEN6_RC_CTL_RC6_ENABLE;
    if unsafe { has_rc6p(i915) } {
        rc6_mask |= GEN6_RC_CTL_RC6P_ENABLE;
    }
    if unsafe { has_rc6pp(i915) } {
        rc6_mask |= GEN6_RC_CTL_RC6PP_ENABLE;
    }
    unsafe {
        (*rc6).ctl_enable = rc6_mask | GEN6_RC_CTL_EI_MODE_1 | GEN6_RC_CTL_HW_ENABLE;
    }

    let mut rc6vids = 0u32;
    let ret = unsafe {
        snb_pcode_read(
            uncore,
            GEN6_PCODE_READ_RC6VIDS,
            &mut rc6vids,
            ptr::null_mut(),
        )
    };
    if unsafe { GRAPHICS_VER(i915) == 6 && ret != 0 } {
        drm_dbg!(&(*i915).drm, "Couldn't check for BIOS workaround\n");
    } else if unsafe { GRAPHICS_VER(i915) == 6 && gen6_decode_rc6_vid(rc6vids & 0xff) < 450 } {
        drm_dbg!(
            &(*i915).drm,
            "You should update your BIOS. Correcting minimum rc6 voltage (%dmV->%dmV)\n",
            gen6_decode_rc6_vid(rc6vids & 0xff),
            450
        );
        rc6vids &= 0xffff00;
        rc6vids |= gen6_encode_rc6_vid(450);
        let ret = unsafe { snb_pcode_write(uncore, GEN6_PCODE_WRITE_RC6VIDS, rc6vids) };
        if ret != 0 {
            drm_err!(&(*i915).drm, "Couldn't fix incorrect rc6 voltage\n");
        }
    }
}

// upstream: intel_rc6.c chv_rc6_init()
unsafe fn chv_rc6_init(rc6: *mut IntelRc6) -> c_int {
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let i915 = unsafe { rc6_to_i915(rc6) };
    let pcbr = unsafe { intel_uncore_read(uncore, VLV_PCBR) };
    if pcbr >> VLV_PCBR_ADDR_SHIFT == 0 {
        drm_dbg!(&(*i915).drm, "BIOS didn't set up PCBR, fixing up\n");
        let paddr = unsafe {
            (*dsm(i915))
                .stolen
                .end
                .wrapping_add(1)
                .wrapping_sub(32 * SZ_1K)
        };
        GEM_BUG_ON!(paddr > U32_MAX);
        let pctx_paddr = paddr & !4095;
        unsafe { intel_uncore_write(uncore, VLV_PCBR, pctx_paddr as u32) };
    }
    0
}

// upstream: intel_rc6.c vlv_rc6_init()
unsafe fn vlv_rc6_init(rc6: *mut IntelRc6) -> c_int {
    let i915 = unsafe { rc6_to_i915(rc6) };
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let pctx_size = 24 * SZ_1K;
    let pcbr = unsafe { intel_uncore_read(uncore, VLV_PCBR) };
    let pctx = if pcbr != 0 {
        let pcbr_offset = (pcbr as u64 & !4095).wrapping_sub(unsafe { (*dsm(i915)).stolen.start });
        unsafe {
            crate::i915_gem_region_upstream::i915_gem_object_create_region_at(
                (*i915).mm.stolen_region,
                pcbr_offset,
                pctx_size,
                0,
            )
        }
    } else {
        drm_dbg!(&(*i915).drm, "BIOS didn't set up PCBR, fixing up\n");
        let pctx = unsafe { i915_gem_object_create_stolen(i915, pctx_size) };
        if linux_config::IS_ERR(pctx) {
            drm_dbg!(
                &(*i915).drm,
                "not enough stolen space for PCTX, disabling\n"
            );
            return linux_config::PTR_ERR(pctx);
        }
        let stolen = unsafe { (*pctx).backing.stolen };
        GEM_BUG_ON!(range_end_overflows(
            unsafe { (*dsm(i915)).stolen.start },
            unsafe { (*stolen).start },
            U32_MAX,
        ));
        let pctx_paddr = unsafe { (*dsm(i915)).stolen.start.wrapping_add((*stolen).start) };
        unsafe { intel_uncore_write(uncore, VLV_PCBR, pctx_paddr as u32) };
        pctx
    };
    if linux_config::IS_ERR(pctx) {
        return linux_config::PTR_ERR(pctx);
    }
    unsafe { (*rc6).pctx = pctx };
    0
}

// upstream: intel_rc6.c chv_rc6_enable()
unsafe fn chv_rc6_enable(rc6: *mut IntelRc6) {
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let gt = unsafe { rc6_to_gt(rc6) };
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, 40 << 16);
        intel_uncore_write_fw(uncore, GEN6_RC_EVALUATION_INTERVAL, 125000);
        intel_uncore_write_fw(uncore, GEN6_RC_IDLE_HYSTERSIS, 25);
    }
    let mut engine: *mut crate::intel_engine_cs_upstream::IntelEngineCs = ptr::null_mut();
    for_each_engine!(engine, id, gt, {
        unsafe {
            intel_uncore_write_fw(
                uncore,
                crate::intel_engine_regs_upstream::RING_MAX_IDLE((*engine).mmio_base),
                10,
            )
        };
    });
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC_SLEEP, 0);
        intel_uncore_write_fw(uncore, GEN6_RC6_THRESHOLD, 0x186);
        intel_uncore_write_fw(
            uncore,
            VLV_COUNTER_CONTROL,
            REG_MASKED_FIELD_ENABLE!(
                VLV_COUNT_RANGE_HIGH | VLV_MEDIA_RC6_COUNT_EN | VLV_RENDER_RC6_COUNT_EN
            ),
        );
        (*rc6).ctl_enable = GEN7_RC_CTL_TO_MODE;
    }
}

// upstream: intel_rc6.c vlv_rc6_enable()
unsafe fn vlv_rc6_enable(rc6: *mut IntelRc6) {
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let gt = unsafe { rc6_to_gt(rc6) };
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC6_WAKE_RATE_LIMIT, 0x00280000);
        intel_uncore_write_fw(uncore, GEN6_RC_EVALUATION_INTERVAL, 125000);
        intel_uncore_write_fw(uncore, GEN6_RC_IDLE_HYSTERSIS, 25);
    }
    let mut engine: *mut crate::intel_engine_cs_upstream::IntelEngineCs = ptr::null_mut();
    for_each_engine!(engine, id, gt, {
        unsafe {
            intel_uncore_write_fw(
                uncore,
                crate::intel_engine_regs_upstream::RING_MAX_IDLE((*engine).mmio_base),
                10,
            )
        };
    });
    unsafe {
        intel_uncore_write_fw(uncore, GEN6_RC6_THRESHOLD, 0x557);
        intel_uncore_write_fw(
            uncore,
            VLV_COUNTER_CONTROL,
            REG_MASKED_FIELD_ENABLE!(
                VLV_COUNT_RANGE_HIGH
                    | VLV_MEDIA_RC0_COUNT_EN
                    | VLV_RENDER_RC0_COUNT_EN
                    | VLV_MEDIA_RC6_COUNT_EN
                    | VLV_RENDER_RC6_COUNT_EN
            ),
        );
        (*rc6).ctl_enable = GEN7_RC_CTL_TO_MODE | VLV_RC_CTL_CTX_RST_PARALLEL;
    }
}

// upstream: intel_rc6.c intel_check_bios_c6_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_check_bios_c6_setup(rc6: *mut IntelRc6) -> bool {
    if !unsafe { rc6_get(rc6, RC6_BIOS_STATE_CAPTURED_BIT) } {
        let uncore = unsafe { rc6_to_uncore(rc6) };
        with_intel_runtime_pm!(unsafe { (*uncore).rpm }, wakeref, {
            unsafe { (*rc6).bios_rc_state = intel_uncore_read(uncore, GEN6_RC_STATE) };
        });
        unsafe { rc6_set(rc6, RC6_BIOS_STATE_CAPTURED_BIT, true) };
    }
    unsafe { (*rc6).bios_rc_state & RC_SW_TARGET_STATE_MASK != 0 }
}

// upstream: intel_rc6.c bxt_check_bios_rc6_setup()
unsafe fn bxt_check_bios_rc6_setup(rc6: *mut IntelRc6) -> bool {
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let i915 = unsafe { rc6_to_i915(rc6) };
    let mut enable_rc6 = true;
    let rc_ctl = unsafe { intel_uncore_read(uncore, GEN6_RC_CONTROL) };
    let mut rc_sw_target = unsafe { intel_uncore_read(uncore, GEN6_RC_STATE) };
    rc_sw_target = (rc_sw_target & RC_SW_TARGET_STATE_MASK) >> RC_SW_TARGET_STATE_SHIFT;
    drm_dbg!(
        &(*i915).drm,
        "BIOS enabled RC states: HW_CTRL %s HW_RC6 %s SW_TARGET_STATE %x\n",
        str_on_off(rc_ctl & GEN6_RC_CTL_HW_ENABLE != 0),
        str_on_off(rc_ctl & GEN6_RC_CTL_RC6_ENABLE != 0),
        rc_sw_target
    );

    if unsafe { intel_uncore_read(uncore, RC6_LOCATION) } & RC6_CTX_IN_DRAM == 0 {
        drm_dbg!(&(*i915).drm, "RC6 Base location not set properly.\n");
        enable_rc6 = false;
    }
    let rc6_ctx_base = unsafe { intel_uncore_read(uncore, RC6_CTX_BASE) } & RC6_CTX_BASE_MASK;
    let reserved = unsafe { &(*dsm(i915)).reserved };
    if (rc6_ctx_base as u64) < reserved.start
        || (rc6_ctx_base as u64).wrapping_add(PAGE_SIZE as u64) >= reserved.end
    {
        drm_dbg!(&(*i915).drm, "RC6 Base address not as expected.\n");
        enable_rc6 = false;
    }

    if !((unsafe { intel_uncore_read(uncore, PWRCTX_MAXCNT(RENDER_RING_BASE)) } & IDLE_TIME_MASK)
        > 1
        && (unsafe { intel_uncore_read(uncore, PWRCTX_MAXCNT(GEN6_BSD_RING_BASE)) }
            & IDLE_TIME_MASK)
            > 1
        && (unsafe { intel_uncore_read(uncore, PWRCTX_MAXCNT(BLT_RING_BASE)) } & IDLE_TIME_MASK)
            > 1
        && (unsafe { intel_uncore_read(uncore, PWRCTX_MAXCNT(VEBOX_RING_BASE)) } & IDLE_TIME_MASK)
            > 1)
    {
        drm_dbg!(&(*i915).drm, "Engine Idle wait time not set properly.\n");
        enable_rc6 = false;
    }
    if unsafe { intel_uncore_read(uncore, GEN8_PUSHBUS_CONTROL) } == 0
        || unsafe { intel_uncore_read(uncore, GEN8_PUSHBUS_ENABLE) } == 0
        || unsafe { intel_uncore_read(uncore, GEN8_PUSHBUS_SHIFT) } == 0
    {
        drm_dbg!(&(*i915).drm, "Pushbus not setup properly.\n");
        enable_rc6 = false;
    }
    if unsafe { intel_uncore_read(uncore, GEN6_GFXPAUSE) } == 0 {
        drm_dbg!(&(*i915).drm, "GFX pause not setup properly.\n");
        enable_rc6 = false;
    }
    if unsafe { intel_uncore_read(uncore, GEN8_MISC_CTRL0) } == 0 {
        drm_dbg!(&(*i915).drm, "GPM control not setup properly.\n");
        enable_rc6 = false;
    }
    enable_rc6
}

// upstream: intel_rc6.c rc6_supported()
unsafe fn rc6_supported(rc6: *mut IntelRc6) -> bool {
    let i915 = unsafe { rc6_to_i915(rc6) };
    let gt = unsafe { rc6_to_gt(rc6) };
    if !unsafe { has_rc6(i915) } {
        return false;
    }
    if unsafe { intel_vgpu_active(i915) } || unsafe { is_mock_gt(gt) } {
        return false;
    }
    if unsafe { IS_GEN9_LP(i915) && !bxt_check_bios_rc6_setup(rc6) } {
        drm_notice!(&(*i915).drm, "RC6 and powersaving disabled by BIOS\n");
        return false;
    }
    if unsafe { IS_METEORLAKE((*gt).i915) && !intel_check_bios_c6_setup(rc6) } {
        drm_notice!(&(*i915).drm, "C6 disabled by BIOS\n");
        return false;
    }
    if crate::IS_MEDIA_GT_IP_STEP!(
        gt,
        IP_VER(13, 0),
        crate::linux::i915::STEP_A0,
        crate::linux::i915::STEP_B0
    ) {
        drm_notice!(&(*i915).drm, "Media RC6 disabled on A step\n");
        return false;
    }
    true
}

// upstream: intel_rc6.c rpm_get()
unsafe fn rpm_get(rc6: *mut IntelRc6) {
    GEM_BUG_ON!(unsafe { rc6_get(rc6, RC6_WAKEREF_BIT) });
    let i915 = unsafe { rc6_to_i915(rc6) };
    let _ = unsafe { pm_runtime_get_sync((*i915).drm.dev) };
    unsafe { rc6_set(rc6, RC6_WAKEREF_BIT, true) };
}

// upstream: intel_rc6.c rpm_put()
unsafe fn rpm_put(rc6: *mut IntelRc6) {
    GEM_BUG_ON!(!unsafe { rc6_get(rc6, RC6_WAKEREF_BIT) });
    let i915 = unsafe { rc6_to_i915(rc6) };
    let _ = unsafe { pm_runtime_put((*i915).drm.dev) };
    unsafe { rc6_set(rc6, RC6_WAKEREF_BIT, false) };
}

// upstream: intel_rc6.c pctx_corrupted()
unsafe fn pctx_corrupted(rc6: *mut IntelRc6) -> bool {
    let i915 = unsafe { rc6_to_i915(rc6) };
    if !unsafe { needs_rc6_ctx_corruption_wa(i915) } {
        return false;
    }
    if unsafe { intel_uncore_read(rc6_to_uncore(rc6), GEN8_RC6_CTX_INFO) } != 0 {
        return false;
    }
    drm_notice!(
        &(*i915).drm,
        "RC6 context corruption, disabling runtime power management\n"
    );
    true
}

// upstream: intel_rc6.c __intel_rc6_disable()
unsafe fn __intel_rc6_disable(rc6: *mut IntelRc6) {
    let i915 = unsafe { rc6_to_i915(rc6) };
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let gt = unsafe { rc6_to_gt(rc6) };
    unsafe {
        intel_guc_rc_disable(gt_to_guc(gt));
        intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL);
        if GRAPHICS_VER(i915) >= 9 {
            intel_uncore_write_fw(uncore, GEN9_PG_ENABLE, 0);
        }
        intel_uncore_write_fw(uncore, GEN6_RC_CONTROL, 0);
        intel_uncore_write_fw(uncore, GEN6_RC_STATE, 0);
        intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL);
    }
}

// upstream: intel_rc6.c rc6_res_reg_init()
unsafe fn rc6_res_reg_init(rc6: *mut IntelRc6) {
    let mut res_reg = [mmio(0); INTEL_RC6_RES_MAX as usize];
    match unsafe { (*rc6_to_gt(rc6)).type_ } {
        GT_MEDIA => res_reg[INTEL_RC6_RES_RC6 as usize] = MTL_MEDIA_MC6,
        _ => {
            res_reg[INTEL_RC6_RES_RC6_LOCKED as usize] = GEN6_GT_GFX_RC6_LOCKED;
            res_reg[INTEL_RC6_RES_RC6 as usize] = GEN6_GT_GFX_RC6;
            res_reg[INTEL_RC6_RES_RC6p as usize] = GEN6_GT_GFX_RC6P;
            res_reg[INTEL_RC6_RES_RC6pp as usize] = GEN6_GT_GFX_RC6PP;
        }
    }
    unsafe { (*rc6).res_reg = res_reg };
}

// upstream: intel_rc6.c intel_rc6_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_init(rc6: *mut IntelRc6) {
    let i915 = unsafe { rc6_to_i915(rc6) };
    unsafe { rpm_get(rc6) };
    if !unsafe { rc6_supported(rc6) } {
        return;
    }
    unsafe { rc6_res_reg_init(rc6) };
    let err = if unsafe { IS_CHERRYVIEW(i915) } {
        unsafe { chv_rc6_init(rc6) }
    } else if unsafe { IS_VALLEYVIEW(i915) } {
        unsafe { vlv_rc6_init(rc6) }
    } else {
        0
    };
    unsafe { __intel_rc6_disable(rc6) };
    unsafe { rc6_set(rc6, RC6_SUPPORTED_BIT, err == 0) };
}

// upstream: intel_rc6.c intel_rc6_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_sanitize(rc6: *mut IntelRc6) {
    unsafe { (*rc6).prev_hw_residency = [0; INTEL_RC6_RES_MAX as usize] };
    if unsafe { rc6_get(rc6, RC6_ENABLED_BIT) } {
        unsafe {
            rpm_get(rc6);
            rc6_set(rc6, RC6_ENABLED_BIT, false);
        }
    }
    if unsafe { rc6_get(rc6, RC6_SUPPORTED_BIT) } {
        unsafe { __intel_rc6_disable(rc6) };
    }
}

// upstream: intel_rc6.c intel_rc6_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_enable(rc6: *mut IntelRc6) {
    if !unsafe { rc6_get(rc6, RC6_SUPPORTED_BIT) } {
        return;
    }
    GEM_BUG_ON!(unsafe { rc6_get(rc6, RC6_ENABLED_BIT) });
    let i915 = unsafe { rc6_to_i915(rc6) };
    let uncore = unsafe { rc6_to_uncore(rc6) };
    unsafe { intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL) };
    if unsafe { IS_CHERRYVIEW(i915) } {
        unsafe { chv_rc6_enable(rc6) };
    } else if unsafe { IS_VALLEYVIEW(i915) } {
        unsafe { vlv_rc6_enable(rc6) };
    } else if unsafe { GRAPHICS_VER(i915) >= 11 } {
        unsafe { gen11_rc6_enable(rc6) };
    } else if unsafe { GRAPHICS_VER(i915) >= 9 } {
        unsafe { gen9_rc6_enable(rc6) };
    } else if unsafe { IS_BROADWELL(i915) } {
        unsafe { gen8_rc6_enable(rc6) };
    } else if unsafe { GRAPHICS_VER(i915) >= 6 } {
        unsafe { gen6_rc6_enable(rc6) };
    }
    unsafe {
        rc6_set(
            rc6,
            RC6_MANUAL_BIT,
            (*rc6).ctl_enable & GEN6_RC_CTL_RC6_ENABLE != 0,
        );
        if needs_rc6_ctx_corruption_wa(i915) {
            (*rc6).ctl_enable = 0;
        }
        intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL);
    }
    if unsafe { pctx_corrupted(rc6) } {
        return;
    }
    unsafe {
        rpm_put(rc6);
        rc6_set(rc6, RC6_ENABLED_BIT, true);
    }
}

// upstream: intel_rc6.c intel_rc6_unpark()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_unpark(rc6: *mut IntelRc6) {
    if !unsafe { rc6_get(rc6, RC6_ENABLED_BIT) } {
        return;
    }
    unsafe { intel_uncore_write_fw(rc6_to_uncore(rc6), GEN6_RC_CONTROL, (*rc6).ctl_enable) };
}

// upstream: intel_rc6.c intel_rc6_park()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_park(rc6: *mut IntelRc6) {
    if !unsafe { rc6_get(rc6, RC6_ENABLED_BIT) } {
        return;
    }
    if unsafe { pctx_corrupted(rc6) } {
        unsafe { intel_rc6_disable(rc6) };
        return;
    }
    if !unsafe { rc6_get(rc6, RC6_MANUAL_BIT) } {
        return;
    }
    let i915 = unsafe { rc6_to_i915(rc6) };
    unsafe { intel_uncore_write_fw(rc6_to_uncore(rc6), GEN6_RC_CONTROL, GEN6_RC_CTL_RC6_ENABLE) };
    let target = if unsafe { has_rc6pp(i915) } {
        0x6
    } else if unsafe { has_rc6p(i915) } {
        0x5
    } else {
        0x4
    };
    unsafe {
        intel_uncore_write_fw(
            rc6_to_uncore(rc6),
            GEN6_RC_STATE,
            target << RC_SW_TARGET_STATE_SHIFT,
        )
    };
}

// upstream: intel_rc6.c intel_rc6_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_disable(rc6: *mut IntelRc6) {
    if !unsafe { rc6_get(rc6, RC6_ENABLED_BIT) } {
        return;
    }
    unsafe {
        rpm_get(rc6);
        rc6_set(rc6, RC6_ENABLED_BIT, false);
        __intel_rc6_disable(rc6);
    }
}

// upstream: intel_rc6.c intel_rc6_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_fini(rc6: *mut IntelRc6) {
    unsafe { intel_rc6_disable(rc6) };
    let i915 = unsafe { rc6_to_i915(rc6) };
    let uncore = unsafe { rc6_to_uncore(rc6) };
    if unsafe { IS_METEORLAKE(i915) && rc6_get(rc6, RC6_BIOS_STATE_CAPTURED_BIT) } {
        unsafe { intel_uncore_write_fw(uncore, GEN6_RC_STATE, (*rc6).bios_rc_state) };
    }
    let pctx = unsafe { fetch_and_zero(&mut (*rc6).pctx) };
    if !pctx.is_null() {
        unsafe { i915_gem_object_put(pctx) };
    }
    if unsafe { rc6_get(rc6, RC6_WAKEREF_BIT) } {
        unsafe { rpm_put(rc6) };
    }
}

// upstream: intel_rc6.c vlv_residency_raw()
unsafe fn vlv_residency_raw(uncore: *mut IntelUncore, reg: I915RegT) -> u64 {
    let mut loop_count = 2;
    let mut lower = 0u32;
    let mut upper;
    let mut tmp;
    lockdep_assert_held!(unsafe { &(*uncore).lock });
    unsafe {
        intel_uncore_write_fw(
            uncore,
            VLV_COUNTER_CONTROL,
            REG_MASKED_FIELD_ENABLE!(VLV_COUNT_RANGE_HIGH),
        );
        upper = intel_uncore_read_fw(uncore, reg);
    }
    loop {
        tmp = upper;
        unsafe {
            intel_uncore_write_fw(
                uncore,
                VLV_COUNTER_CONTROL,
                REG_MASKED_FIELD_DISABLE!(VLV_COUNT_RANGE_HIGH),
            );
            lower = intel_uncore_read_fw(uncore, reg);
            intel_uncore_write_fw(
                uncore,
                VLV_COUNTER_CONTROL,
                REG_MASKED_FIELD_ENABLE!(VLV_COUNT_RANGE_HIGH),
            );
            upper = intel_uncore_read_fw(uncore, reg);
        }
        if upper == tmp {
            break;
        }
        loop_count -= 1;
        if loop_count == 0 {
            break;
        }
    }
    lower as u64 | ((upper as u64) << 8)
}

// upstream: intel_rc6.c intel_rc6_residency_ns()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_residency_ns(rc6: *mut IntelRc6, id: IntelRc6ResType) -> u64 {
    if !unsafe { rc6_get(rc6, RC6_SUPPORTED_BIT) } {
        return 0;
    }
    let i915 = unsafe { rc6_to_i915(rc6) };
    let uncore = unsafe { rc6_to_uncore(rc6) };
    let reg = unsafe { (*rc6).res_reg[id as usize] };
    let mut fw_domains = unsafe { intel_uncore_forcewake_for_reg(uncore, reg, FW_REG_READ) };
    let mut flags: c_ulong = 0;
    let mut mul: u32;
    let mut div: u32;
    let mut overflow_hw: u64;
    let mut time_hw: u64;
    let prev_hw;

    unsafe {
        spin_lock_irqsave(&mut (*uncore).lock, &mut flags);
        intel_uncore_forcewake_get__locked(uncore, fw_domains);
    }
    if unsafe { IS_VALLEYVIEW(i915) || IS_CHERRYVIEW(i915) } {
        mul = 1_000_000;
        div = unsafe { vlv_clock_get_czclk((&mut (*i915).drm) as *mut DrmDevicePrefix) as u32 };
        overflow_hw = 1u64 << 40;
        time_hw = unsafe { vlv_residency_raw(uncore, reg) };
    } else {
        if unsafe { IS_GEN9_LP(i915) } {
            mul = 10000;
            div = 12;
        } else {
            mul = 1280;
            div = 1;
        }
        overflow_hw = 1u64 << 32;
        time_hw = unsafe { intel_uncore_read_fw(uncore, reg) as u64 };
    }
    prev_hw = unsafe { (*rc6).prev_hw_residency[id as usize] };
    unsafe { (*rc6).prev_hw_residency[id as usize] = time_hw };
    if time_hw >= prev_hw {
        time_hw -= prev_hw;
    } else {
        time_hw = time_hw.wrapping_add(overflow_hw).wrapping_sub(prev_hw);
    }
    time_hw = time_hw.wrapping_add(unsafe { (*rc6).cur_residency[id as usize] });
    unsafe { (*rc6).cur_residency[id as usize] = time_hw };
    unsafe {
        intel_uncore_forcewake_put__locked(uncore, fw_domains);
        spin_unlock_irqrestore(&mut (*uncore).lock, flags);
    }
    mul_u64_u32_div(time_hw, mul, div)
}

// upstream: intel_rc6.c intel_rc6_residency_us()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_residency_us(rc6: *mut IntelRc6, id: IntelRc6ResType) -> u64 {
    div_round_up_ull(unsafe { intel_rc6_residency_ns(rc6, id) }, 1000)
}

// upstream: intel_rc6.c intel_rc6_print_residency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_rc6_print_residency(
    m: *mut SeqFile,
    title: *const c_char,
    id: IntelRc6ResType,
) {
    let gt = unsafe { (*m.cast::<SeqFilePrivateView>()).private.cast::<IntelGt>() };
    let uncore = unsafe { (*gt).uncore };
    let reg = unsafe { (*gt).rc6.res_reg[id as usize] };
    with_intel_runtime_pm!(unsafe { (*uncore).rpm }, wakeref, {
        let value = unsafe { intel_uncore_read(uncore, reg) };
        let residency = unsafe { intel_rc6_residency_us(ptr::addr_of_mut!((*gt).rc6), id) };
        let _ = unsafe {
            seq_printf(
                m,
                b"%s %u (%llu us)\n\0".as_ptr().cast(),
                title,
                value,
                residency,
            )
        };
    });
}
