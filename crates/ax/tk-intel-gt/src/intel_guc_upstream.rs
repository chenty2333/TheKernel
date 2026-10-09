// SPDX-License-Identifier: MIT
// Copyright © 2008-2026 Intel Corporation.
// Source integration from Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc.c.

use core::{
    cmp::min,
    ffi::{c_char, c_ulong, c_void},
    ptr,
};

use crate::{
    guc_ads,
    guc_config::GUC_CTL_MAX_DWORDS as PARAM_DWORDS,
    guc_ct::{
        GUC_HXG_FAILURE_MSG_0_ERROR, GUC_HXG_FAILURE_MSG_0_HINT, GUC_HXG_MSG_0_ORIGIN,
        GUC_HXG_MSG_0_TYPE, GUC_HXG_REQUEST_MSG_0_ACTION, GUC_HXG_RESPONSE_MSG_0_DATA0,
        GUC_HXG_RETRY_MSG_0_REASON, HXG_ORIGIN_GUC, HXG_ORIGIN_HOST, HXG_TYPE_NO_RESPONSE_BUSY,
        HXG_TYPE_NO_RESPONSE_RETRY, HXG_TYPE_REQUEST, HXG_TYPE_RESPONSE_FAILURE,
        HXG_TYPE_RESPONSE_SUCCESS,
    },
    i915_gem_lmem_upstream::{i915_gem_object_create_lmem, i915_gem_object_is_lmem},
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_types_upstream::{
        I915_BO_ALLOC_CONTIGUOUS, I915_BO_ALLOC_CPU_CLEAR, I915_BO_ALLOC_PM_EARLY,
    },
    i915_gem_object_upstream::i915_gem_object_set_cache_coherency,
    i915_gem_pages_upstream::i915_gem_object_pin_map_unlocked,
    i915_gem_shmem_upstream::i915_gem_object_create_shmem,
    i915_vma_api_upstream::{
        i915_ggtt_pin, i915_ggtt_pin_bias, i915_vma_instance, i915_vma_make_unshrinkable,
        i915_vma_unpin_and_release,
    },
    i915_vma_types_upstream::I915Vma,
    intel_engine_cs_upstream::WorkStruct,
    intel_engine_types_upstream::{ALL_ENGINES, IntelEngineCs, IntelEngineMask},
    intel_gt_api_upstream::{gt_to_guc, guc_to_gt, intel_gt_coherent_map_type},
    intel_gt_types_upstream::{GT_MEDIA, IntelGt},
    intel_guc_actions_abi_types_upstream::{
        GUC_ACTION_HOST2GUC_SELF_CFG, HOST2GUC_SELF_CFG_REQUEST_MSG_1_KLV_KEY,
        HOST2GUC_SELF_CFG_REQUEST_MSG_1_KLV_LEN, HOST2GUC_SELF_CFG_REQUEST_MSG_2_VALUE32,
        HOST2GUC_SELF_CFG_REQUEST_MSG_3_VALUE64,
    },
    intel_guc_ct_types_upstream::IntelGucCt,
    intel_guc_fwif_types_upstream::{
        GUC_CTL_ADS, GUC_CTL_DEBUG, GUC_CTL_DEVID, GUC_CTL_DISABLE_SCHEDULER, GUC_CTL_ENABLE_SLPC,
        GUC_CTL_FEATURE, GUC_CTL_LOG_PARAMS, GUC_CTL_WA, GUC_LOG_BUF_ADDR_SHIFT,
        GUC_LOG_CAPTURE_SHIFT, GUC_LOG_CRASH_SHIFT, GUC_LOG_DEBUG_SHIFT, GUC_LOG_DISABLED,
        GUC_LOG_NOTIFY_ON_HALF_FULL, GUC_LOG_VALID, GUC_LOG_VERBOSITY_SHIFT,
        GUC_WA_CONTEXT_ISOLATION, GUC_WA_DUAL_QUEUE, GUC_WA_ENABLE_TSC_CHECK_ON_RC6,
        GUC_WA_HOLD_CCS_SWITCHOUT, GUC_WA_POLLCS, GUC_WA_PRE_PARSER, GUC_WA_RCS_CCS_SWITCHOUT,
        INTEL_GUC_RECV_MSG_CRASH_DUMP_POSTED, INTEL_GUC_RECV_MSG_EXCEPTION, SOFT_SCRATCH_COUNT,
    },
    intel_guc_log_types_upstream::{
        GUC_LOG_LEVEL_IS_VERBOSE, GUC_LOG_LEVEL_TO_VERBOSITY, GUC_LOG_SECTIONS_CAPTURE,
        GUC_LOG_SECTIONS_CRASH, GUC_LOG_SECTIONS_DEBUG, IntelGucLog,
    },
    intel_guc_rc_types_upstream::intel_guc_rc_init_early,
    intel_guc_slpc_upstream::{intel_guc_slpc_fini, intel_guc_slpc_init, intel_guc_slpc_is_used},
    intel_guc_submission_types_upstream::{
        intel_guc_submission_fini, intel_guc_submission_init, intel_guc_submission_init_early,
        intel_guc_submission_is_used,
    },
    intel_guc_types_upstream::{
        IntelGuc, intel_guc_is_fw_running, intel_guc_is_supported, intel_guc_is_wanted,
    },
    intel_uc_fw_types_upstream::{
        INTEL_UC_FIRMWARE_INIT_FAIL, INTEL_UC_FIRMWARE_LOADABLE, INTEL_UC_FW_TYPE_GUC,
    },
    intel_uc_fw_upstream::{
        intel_uc_fw_change_status, intel_uc_fw_dump, intel_uc_fw_fini, intel_uc_fw_init,
        intel_uc_fw_init_early,
    },
    intel_uncore_types_upstream::{
        FORCEWAKE_GT, FW_REG_READ, FW_REG_WRITE, intel_uncore_forcewake_for_reg,
        intel_uncore_forcewake_get, intel_uncore_forcewake_put, intel_uncore_posting_read,
        intel_uncore_read, intel_uncore_write, intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        config::{ENXIO, EPROTO, ETIMEDOUT},
        i915::{
            IS_DG2, IS_DG2_G11, IS_GFX_GT_IP_RANGE, graphics_ver, graphics_ver_full,
            i915_ggtt_offset, i915_mmio_reg_offset, i915_pci_revision,
            intel_engine_reset_needs_wa_22011802037,
        },
        iosys_map::{IosysMap, IosysMapAddr},
        locks::{spin_lock_init, spin_lock_irq, spin_unlock_irq},
        mutex::{mutex_init, mutex_lock, mutex_unlock},
        pm::{intel_runtime_pm_get, intel_runtime_pm_put},
        primitives::{jiffies, jiffies_to_msecs, ktime_get, wmb},
        registers::{I915_CACHE_NONE, I915_ERROR_CAPTURE, REG_FIELD_GET, REG_FIELD_PREP},
        workqueue::{INIT_WORK_C, flush_work, queue_work, system_dfl_wq},
    },
    linux_i915_private::DrmI915Private,
    linux_print::{DrmPrinter, drm_printer_write},
};

const GUC_SEND_TRIGGER: u32 = 1;
const GEN11_GUC_HOST_INTERRUPT: I915RegT = I915RegT { reg: 0x1901f0 };
const MEDIA_GUC_HOST_INTERRUPT: I915RegT = I915RegT { reg: 0x190304 };
const GUC_SEND_INTERRUPT: I915RegT = I915RegT { reg: 0xc4c8 };
const GEN11_SOFT_SCRATCH_BASE: u32 = 0x190240;
const MEDIA_SOFT_SCRATCH_BASE: u32 = 0x190310;
const SOFT_SCRATCH_BASE: u32 = 0xc180;
const GEN11_GUC: u32 = 25;
const MTL_MGUC: u32 = 24;
const GEN8_GT_IIR2: I915RegT = I915RegT { reg: 0x44328 };
const GUC_STATUS: I915RegT = I915RegT { reg: 0xc000 };
const GS_BOOTROM_SHIFT: u32 = 1;
const GS_BOOTROM_MASK: u32 = 0x7f << GS_BOOTROM_SHIFT;
const GS_UKERNEL_SHIFT: u32 = 8;
const GS_UKERNEL_MASK: u32 = 0xff << GS_UKERNEL_SHIFT;
const GS_MIA_SHIFT: u32 = 16;
const GS_MIA_MASK: u32 = 0x7 << GS_MIA_SHIFT;
const GUC_LOG_LEVEL_NON_VERBOSE: u32 = 1;
const GUC_LOG_LEVEL_DEBUG: u32 = 2;
const GUC_LOG_LEVEL_MAX: u32 = 5;
const GUC_WA_ENABLE_GUC_PXP_CTL: u32 = 1 << 1;
const GUC_WA_ENABLE_SLPC: u32 = 1 << 2;
const GUC_WA_DISABLE_SCHEDULER: u32 = 1 << 14;
const GUC_ADS_ADDR_SHIFT: u32 = 1;
const INTEL_GUC_ACTION_AUTHENTICATE_HUC: u32 = 0x4000;
const INTEL_GUC_ACTION_CLIENT_SOFT_RESET: u32 = 0x5507;
const INTEL_GUC_ACTION_NOTIFY_CRASH_DUMP_POSTED: u32 = 0x8004;
const INTEL_GUC_ACTION_NOTIFY_EXCEPTION: u32 = 0x8005;
const ENOKEY: i32 = 126;
const GUC_GGTT_TOP: u64 = 0xfee0_0000;
const PIN_OFFSET_BIAS: u32 = 1 << 6;

const fn soft_scratch(index: u32) -> I915RegT {
    I915RegT {
        reg: SOFT_SCRATCH_BASE + index * 4,
    }
}

const fn gen11_soft_scratch(index: u32) -> I915RegT {
    I915RegT {
        reg: GEN11_SOFT_SCRATCH_BASE + index * 4,
    }
}

const fn media_soft_scratch(index: u32) -> I915RegT {
    I915RegT {
        reg: MEDIA_SOFT_SCRATCH_BASE + index * 4,
    }
}

// Lower-level owners outside intel_guc.c. These are real Linux/i915 services,
// not substitute implementations; their translation owners provide the ABI.
#[allow(improper_ctypes)]
unsafe extern "C" {
    fn intel_guc_ct_init_early(ct: *mut IntelGucCt);
    fn intel_guc_ct_init(ct: *mut IntelGucCt) -> i32;
    fn intel_guc_ct_fini(ct: *mut IntelGucCt);
    fn intel_guc_ct_enabled(ct: *const IntelGucCt) -> bool;
    fn intel_guc_ct_enable(ct: *mut IntelGucCt) -> i32;
    fn intel_guc_ct_disable(ct: *mut IntelGucCt);
    fn intel_guc_ct_event_handler(ct: *mut IntelGucCt);
    fn intel_guc_ct_sanitize(ct: *mut IntelGucCt);
    fn intel_guc_ct_send(
        ct: *mut IntelGucCt,
        action: *const u32,
        len: u32,
        response: *mut u32,
        response_len: u32,
        flags: u32,
    ) -> i32;
    fn intel_guc_log_init_early(log: *mut IntelGucLog);
    fn intel_guc_log_create(log: *mut IntelGucLog) -> i32;
    fn intel_guc_log_destroy(log: *mut IntelGucLog);
    fn intel_guc_ads_init_late(guc: *mut IntelGuc);
    fn intel_guc_ads_create(guc: *mut IntelGuc) -> i32;
    fn intel_guc_ads_destroy(guc: *mut IntelGuc);
    fn intel_guc_capture_init(guc: *mut IntelGuc) -> i32;
    fn intel_guc_capture_destroy(guc: *mut IntelGuc);
    fn intel_synchronize_irq(i915: *mut DrmI915Private);
    fn intel_gt_set_wedged(gt: *mut IntelGt);
}

unsafe fn intel_guc_is_ready(guc: *const IntelGuc) -> bool {
    unsafe { intel_guc_is_fw_running(guc) && intel_guc_ct_enabled(ptr::addr_of!((*guc).ct)) }
}

unsafe fn intel_guc_enable_msg(guc: *mut IntelGuc, mask: u32) {
    unsafe {
        spin_lock_irq(&mut (*guc).irq_lock);
        (*guc).msg_enabled_mask |= mask;
        spin_unlock_irq(&mut (*guc).irq_lock);
    }
}

unsafe fn intel_guc_send(guc: *mut IntelGuc, action: *const u32, len: u32) -> i32 {
    unsafe {
        intel_guc_ct_send(
            ptr::addr_of_mut!((*guc).ct),
            action,
            len,
            ptr::null_mut(),
            0,
            0,
        )
    }
}

unsafe fn intel_uc_fw_sanitize(fw: *mut crate::intel_uc_fw_types_upstream::IntelUcFw) {
    if unsafe { crate::intel_uc_fw_upstream::intel_uc_fw_is_loaded(fw) } {
        unsafe {
            intel_uc_fw_change_status(fw, INTEL_UC_FIRMWARE_LOADABLE);
        }
    }
}

// upstream: intel_guc.c intel_guc_notify(). The source function is also
// represented by the standalone GtIo helper in guc_fw.rs; this is its device
// ABI implementation for the upstream GT owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_notify(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    unsafe { intel_uncore_write((*gt).uncore, (*guc).notify_reg, GUC_SEND_TRIGGER) };
}

// upstream: intel_guc.c guc_send_reg()
unsafe fn guc_send_reg(guc: *mut IntelGuc, index: u32) -> I915RegT {
    GEM_BUG_ON!(unsafe { (*guc).send_regs.base } == 0);
    GEM_BUG_ON!(unsafe { (*guc).send_regs.count } == 0);
    GEM_BUG_ON!(index >= unsafe { (*guc).send_regs.count });
    I915RegT {
        reg: unsafe { (*guc).send_regs.base } + 4 * index,
    }
}

// upstream: intel_guc.c intel_guc_init_send_regs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_init_send_regs(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    GEM_BUG_ON!(unsafe { (*guc).send_regs.base } == 0);
    GEM_BUG_ON!(unsafe { (*guc).send_regs.count } == 0);
    let mut domains = 0;
    for index in 0..unsafe { (*guc).send_regs.count } {
        let reg = unsafe { guc_send_reg(guc, index) };
        domains |= unsafe {
            intel_uncore_forcewake_for_reg((*gt).uncore, reg, FW_REG_READ | FW_REG_WRITE)
        };
    }
    unsafe { (*guc).send_regs.fw_domains = domains };
}

// upstream: intel_guc.c gen9_reset_guc_interrupts()
unsafe extern "C" fn gen9_reset_guc_interrupts(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    unsafe { crate::intel_runtime_pm_upstream::assert_rpm_raw_wakeref_held(rpm.cast()) };
    unsafe {
        spin_lock_irq(&mut *(*gt).irq_lock);
        crate::intel_gt_pm_irq_upstream::gen6_gt_pm_reset_iir(gt, (*gt).pm_guc_events);
        spin_unlock_irq(&mut *(*gt).irq_lock);
    }
}

// upstream: intel_guc.c gen9_enable_guc_interrupts()
unsafe extern "C" fn gen9_enable_guc_interrupts(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    unsafe { crate::intel_runtime_pm_upstream::assert_rpm_raw_wakeref_held(rpm.cast()) };
    unsafe {
        spin_lock_irq(&mut *(*gt).irq_lock);
        WARN_ON_ONCE!(intel_uncore_read((*gt).uncore, GEN8_GT_IIR2) & (*gt).pm_guc_events != 0);
        crate::intel_gt_pm_irq_upstream::gen6_gt_pm_enable_irq(gt, (*gt).pm_guc_events);
        spin_unlock_irq(&mut *(*gt).irq_lock);
        (*guc).interrupts.enabled = true;
    }
}

// upstream: intel_guc.c gen9_disable_guc_interrupts()
unsafe extern "C" fn gen9_disable_guc_interrupts(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    unsafe { crate::intel_runtime_pm_upstream::assert_rpm_raw_wakeref_held(rpm.cast()) };
    unsafe {
        (*guc).interrupts.enabled = false;
        spin_lock_irq(&mut *(*gt).irq_lock);
        crate::intel_gt_pm_irq_upstream::gen6_gt_pm_disable_irq(gt, (*gt).pm_guc_events);
        spin_unlock_irq(&mut *(*gt).irq_lock);
        intel_synchronize_irq(i915);
        gen9_reset_guc_interrupts(guc);
    }
}

// upstream: intel_guc.c __gen11_reset_guc_interrupts()
unsafe fn __gen11_reset_guc_interrupts(gt: *mut IntelGt) -> bool {
    let bit = if unsafe { (*gt).type_ == GT_MEDIA } {
        MTL_MGUC
    } else {
        GEN11_GUC
    };
    lockdep_assert_held!(unsafe { (*gt).irq_lock });
    unsafe { crate::intel_gt_irq_upstream::gen11_gt_reset_one_iir(gt, 0, bit) }
}

// upstream: intel_guc.c gen11_reset_guc_interrupts()
unsafe extern "C" fn gen11_reset_guc_interrupts(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    unsafe {
        spin_lock_irq(&mut *(*gt).irq_lock);
        let _ = __gen11_reset_guc_interrupts(gt);
        spin_unlock_irq(&mut *(*gt).irq_lock);
    }
}

// upstream: intel_guc.c gen11_enable_guc_interrupts()
unsafe extern "C" fn gen11_enable_guc_interrupts(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    unsafe {
        spin_lock_irq(&mut *(*gt).irq_lock);
        let _ = __gen11_reset_guc_interrupts(gt);
        spin_unlock_irq(&mut *(*gt).irq_lock);
        (*guc).interrupts.enabled = true;
    }
}

// upstream: intel_guc.c gen11_disable_guc_interrupts()
unsafe extern "C" fn gen11_disable_guc_interrupts(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    unsafe {
        (*guc).interrupts.enabled = false;
        intel_synchronize_irq((*gt).i915);
        gen11_reset_guc_interrupts(guc);
    }
}

// upstream: intel_guc.c guc_dead_worker_func()
unsafe extern "C" fn guc_dead_worker_func(work: *mut WorkStruct) {
    let guc = container_of!(work, IntelGuc, dead_guc_worker);
    let gt = unsafe { guc_to_gt(guc) };
    let last = unsafe { (*guc).last_dead_guc_jiffies };
    let delta = jiffies_to_msecs(jiffies().wrapping_sub(last));
    if delta < 500 {
        unsafe { intel_gt_set_wedged(gt) };
    } else {
        let reason = b"dead GuC\0";
        unsafe {
            crate::intel_reset_upstream::intel_gt_handle_error_format(
                gt,
                ALL_ENGINES,
                I915_ERROR_CAPTURE as c_ulong,
                reason.as_ptr().cast(),
                &[],
            );
            (*guc).last_dead_guc_jiffies = jiffies() as c_ulong;
        }
    }
}

// upstream: intel_guc.c intel_guc_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_init_early(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    unsafe {
        intel_uc_fw_init_early(ptr::addr_of_mut!((*guc).fw), INTEL_UC_FW_TYPE_GUC, true);
        intel_guc_ct_init_early(ptr::addr_of_mut!((*guc).ct));
        intel_guc_log_init_early(ptr::addr_of_mut!((*guc).log));
        intel_guc_submission_init_early(guc);
        crate::intel_guc_slpc_upstream::intel_guc_slpc_init_early(ptr::addr_of_mut!((*guc).slpc));
        intel_guc_rc_init_early(guc);
        INIT_WORK_C(&mut (*guc).dead_guc_worker, guc_dead_worker_func);
        mutex_init(&mut (*guc).send_mutex);
        spin_lock_init(&mut (*guc).irq_lock);
    }

    if unsafe { graphics_ver(i915) >= 11 } {
        unsafe {
            (*guc).interrupts.reset = Some(gen11_reset_guc_interrupts);
            (*guc).interrupts.enable = Some(gen11_enable_guc_interrupts);
            (*guc).interrupts.disable = Some(gen11_disable_guc_interrupts);
            if (*gt).type_ == GT_MEDIA {
                (*guc).notify_reg = MEDIA_GUC_HOST_INTERRUPT;
                (*guc).send_regs.base = i915_mmio_reg_offset(media_soft_scratch(0));
            } else {
                (*guc).notify_reg = GEN11_GUC_HOST_INTERRUPT;
                (*guc).send_regs.base = i915_mmio_reg_offset(gen11_soft_scratch(0));
            }
            (*guc).send_regs.count = 4;
        }
    } else {
        unsafe {
            (*guc).notify_reg = GUC_SEND_INTERRUPT;
            (*guc).interrupts.reset = Some(gen9_reset_guc_interrupts);
            (*guc).interrupts.enable = Some(gen9_enable_guc_interrupts);
            (*guc).interrupts.disable = Some(gen9_disable_guc_interrupts);
            (*guc).send_regs.base = i915_mmio_reg_offset(soft_scratch(0));
            (*guc).send_regs.count = crate::guc_ct::GUC_MAX_MMIO_MSG_LEN as u32;
            BUILD_BUG_ON!(crate::guc_ct::GUC_MAX_MMIO_MSG_LEN as u32 > SOFT_SCRATCH_COUNT);
        }
    }

    unsafe {
        intel_guc_enable_msg(
            guc,
            INTEL_GUC_RECV_MSG_EXCEPTION | INTEL_GUC_RECV_MSG_CRASH_DUMP_POSTED,
        );
    }
}

// upstream: intel_guc.c intel_guc_init_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_init_late(guc: *mut IntelGuc) {
    unsafe { intel_guc_ads_init_late(guc) };
}

// C equivalents of the internal control-word helpers from intel_guc.c. The
// earlier GtIo-facing policy helpers in guc_config.rs remain the testable
// default-path variant; these operate on the upstream IntelGuc layout.
// upstream: intel_guc.c guc_ctl_debug_flags()
unsafe fn guc_ctl_debug_flags(guc: *mut IntelGuc) -> u32 {
    let level = unsafe { (*guc).log.level };
    if !GUC_LOG_LEVEL_IS_VERBOSE(level) {
        GUC_LOG_DISABLED
    } else {
        GUC_LOG_LEVEL_TO_VERBOSITY(level) << GUC_LOG_VERBOSITY_SHIFT
    }
}

// upstream: intel_guc.c guc_ctl_feature_flags()
unsafe fn guc_ctl_feature_flags(guc: *mut IntelGuc) -> u32 {
    let mut flags = 0;
    // The wt-dev kernel configuration has CONFIG_DRM_I915_PXP=n, so HAS_PXP
    // folds to false exactly as it does in the Linux preprocessor.
    let _pxp = false;
    if !unsafe { intel_guc_submission_is_used(guc) } {
        flags |= GUC_CTL_DISABLE_SCHEDULER;
    }
    if unsafe { intel_guc_slpc_is_used(guc) } {
        flags |= GUC_CTL_ENABLE_SLPC;
    }
    flags
}

// upstream: intel_guc.c guc_ctl_log_params_flags()
unsafe fn guc_ctl_log_params_flags(guc: *mut IntelGuc) -> u32 {
    let log = unsafe { ptr::addr_of!((*guc).log) };
    GEM_BUG_ON!(!unsafe { (*log).sizes_initialised });
    let offset =
        unsafe { intel_guc_ggtt_offset(guc, (*log).vma) } >> crate::linux::config::PAGE_SHIFT;
    let sizes = unsafe { &(*log).sizes };
    GUC_LOG_VALID
        | GUC_LOG_NOTIFY_ON_HALF_FULL
        | sizes[GUC_LOG_SECTIONS_DEBUG as usize].flag
        | sizes[GUC_LOG_SECTIONS_CAPTURE as usize].flag
        | ((sizes[GUC_LOG_SECTIONS_CRASH as usize].count as u32) << GUC_LOG_CRASH_SHIFT)
        | ((sizes[GUC_LOG_SECTIONS_DEBUG as usize].count as u32) << GUC_LOG_DEBUG_SHIFT)
        | ((sizes[GUC_LOG_SECTIONS_CAPTURE as usize].count as u32) << GUC_LOG_CAPTURE_SHIFT)
        | (offset << GUC_LOG_BUF_ADDR_SHIFT)
}

// upstream: intel_guc.c guc_ctl_ads_flags()
unsafe fn guc_ctl_ads_flags(guc: *mut IntelGuc) -> u32 {
    let ads =
        unsafe { intel_guc_ggtt_offset(guc, (*guc).ads_vma) } >> crate::linux::config::PAGE_SHIFT;
    ads << crate::intel_guc_fwif_types_upstream::GUC_ADS_ADDR_SHIFT
}

// upstream: intel_guc.c guc_ctl_wa_flags()
unsafe fn guc_ctl_wa_flags(guc: *mut IntelGuc) -> u32 {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let mut flags = 0;
    if unsafe { graphics_ver(i915) >= 11 }
        && unsafe { graphics_ver_full(i915) < crate::linux::i915::IP_VER(12, 55) }
    {
        flags |= GUC_WA_POLLCS;
    }
    if unsafe {
        crate::linux::i915::IS_GFX_GT_IP_STEP(
            gt,
            crate::linux::i915::IP_VER(12, 70),
            crate::linux::i915::STEP_A0,
            crate::linux::i915::STEP_B0,
        ) || IS_DG2(i915)
    } {
        flags |= GUC_WA_HOLD_CCS_SWITCHOUT;
    }
    if unsafe {
        IS_GFX_GT_IP_RANGE(
            gt,
            crate::linux::i915::IP_VER(12, 70),
            crate::linux::i915::IP_VER(12, 74),
        )
    } {
        flags |= GUC_WA_RCS_CCS_SWITCHOUT;
    }
    if unsafe { IS_DG2(i915) }
        || (unsafe { crate::intel_engine_api_upstream::CCS_MASK(gt) } != 0
            && unsafe { graphics_ver_full(i915) >= crate::linux::i915::IP_VER(12, 70) })
    {
        flags |= GUC_WA_DUAL_QUEUE;
    }
    if unsafe { intel_engine_reset_needs_wa_22011802037(gt) } {
        flags |= GUC_WA_PRE_PARSER;
    }
    if unsafe { IS_DG2_G11(i915) } {
        flags |= GUC_WA_CONTEXT_ISOLATION;
    }
    let fw = unsafe { (*guc).fw.file_selected.ver };
    let fw_version = (fw.major << 16) | (fw.minor << 8) | fw.patch;
    if fw_version >= ((70 << 16) | (7 << 8)) {
        flags |= GUC_WA_ENABLE_TSC_CHECK_ON_RC6;
    }
    flags
}

// upstream: intel_guc.c guc_ctl_devid()
unsafe fn guc_ctl_devid(guc: *mut IntelGuc, revision: u8) -> u32 {
    let i915 = unsafe { (*guc_to_gt(guc)).i915 };
    ((unsafe { (*i915).runtime.device_id } as u32) << 16) | revision as u32
}

// upstream: intel_guc.c guc_init_params()
unsafe fn guc_init_params(guc: *mut IntelGuc, revision: u8) {
    BUILD_BUG_ON!(PARAM_DWORDS as u32 != SOFT_SCRATCH_COUNT - 2);
    let params = unsafe { &mut (*guc).params };
    params[GUC_CTL_LOG_PARAMS as usize] = unsafe { guc_ctl_log_params_flags(guc) };
    params[GUC_CTL_FEATURE as usize] = unsafe { guc_ctl_feature_flags(guc) };
    params[GUC_CTL_DEBUG as usize] = unsafe { guc_ctl_debug_flags(guc) };
    params[GUC_CTL_ADS as usize] = unsafe { guc_ctl_ads_flags(guc) };
    params[GUC_CTL_WA as usize] = unsafe { guc_ctl_wa_flags(guc) };
    params[GUC_CTL_DEVID as usize] = unsafe { guc_ctl_devid(guc, revision) };
    for (index, value) in params.iter().copied().enumerate() {
        guc_dbg!(guc, "param[%2d] = %#x\n", index as i32, value);
    }
}

// upstream: intel_guc.c intel_guc_write_params()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_write_params(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let uncore = unsafe { (*gt).uncore };
    unsafe { intel_uncore_forcewake_get(uncore, FORCEWAKE_GT) };
    unsafe { intel_uncore_write(uncore, soft_scratch(0), 0) };
    for index in 0..PARAM_DWORDS {
        let value = unsafe { (*guc).params[index] };
        unsafe { intel_uncore_write(uncore, soft_scratch(index as u32 + 1), value) };
    }
    unsafe { intel_uncore_forcewake_put(uncore, FORCEWAKE_GT) };
}

// upstream: intel_guc.c intel_guc_dump_time_info(). LinuxKPI currently exposes
// monotonic ktime rather than a distinct suspend-inclusive boottime clock.
pub unsafe extern "C" fn intel_guc_dump_time_info(guc: *mut IntelGuc, printer: *mut DrmPrinter) {
    let gt = unsafe { guc_to_gt(guc) };
    let mut stamp = 0;
    let i915 = unsafe { (*gt).i915 };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let wakeref = intel_runtime_pm_get(unsafe { &*rpm });
    if !wakeref.is_null() {
        stamp = unsafe { intel_uncore_read((*gt).uncore, I915RegT { reg: 0x0c3e8 }) };
        unsafe { intel_runtime_pm_put(&*rpm, wakeref) };
    }
    let timestamp = ktime_get() as u64;
    drm_printf!(
        printer,
        "Kernel timestamp: 0x%08llX [%llu]\n",
        timestamp,
        timestamp
    );
    drm_printf!(printer, "GuC timestamp: 0x%08X [%u]\n", stamp, stamp);
    drm_printf!(
        printer,
        "CS timestamp frequency: %u Hz, %u ns\n",
        unsafe { (*gt).clock_frequency },
        unsafe { (*gt).clock_period_ns }
    );
}

// upstream: intel_guc.c intel_guc_init(). The PCI config revision is needed
// by GuC firmware control word 5. Fail before allocating state when the kernel
// has not installed its PCI owner callback; never send a guessed revision.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_init(guc: *mut IntelGuc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let revision = match unsafe { i915_pci_revision(i915) } {
        Ok(revision) => revision,
        Err(error) => {
            guc_probe_error!(guc, "PCI revision reader unavailable (%d)\n", error);
            unsafe {
                intel_uc_fw_change_status(ptr::addr_of_mut!((*guc).fw), INTEL_UC_FIRMWARE_INIT_FAIL)
            };
            return error;
        }
    };

    let mut ret = unsafe { intel_uc_fw_init(ptr::addr_of_mut!((*guc).fw)) };
    if ret != 0 {
        return unsafe { guc_init_fail(guc, ret) };
    }
    ret = unsafe { intel_guc_log_create(ptr::addr_of_mut!((*guc).log)) };
    if ret != 0 {
        return unsafe { guc_init_unwind(guc, ret, GucInitUnwind::Firmware) };
    }
    ret = unsafe { intel_guc_capture_init(guc) };
    if ret != 0 {
        return unsafe { guc_init_unwind(guc, ret, GucInitUnwind::Log) };
    }
    ret = unsafe { intel_guc_ads_create(guc) };
    if ret != 0 {
        return unsafe { guc_init_unwind(guc, ret, GucInitUnwind::Capture) };
    }
    GEM_BUG_ON!(unsafe { (*guc).ads_vma.is_null() });
    ret = unsafe { intel_guc_ct_init(ptr::addr_of_mut!((*guc).ct)) };
    if ret != 0 {
        return unsafe { guc_init_unwind(guc, ret, GucInitUnwind::Ads) };
    }
    if unsafe { intel_guc_submission_is_used(guc) } {
        ret = unsafe { intel_guc_submission_init(guc) };
        if ret != 0 {
            return unsafe { guc_init_unwind(guc, ret, GucInitUnwind::Ct) };
        }
    }
    if unsafe { intel_guc_slpc_is_used(guc) } {
        ret = unsafe { intel_guc_slpc_init(ptr::addr_of_mut!((*guc).slpc)) };
        if ret != 0 {
            return unsafe { guc_init_unwind(guc, ret, GucInitUnwind::Submission) };
        }
    }

    unsafe { guc_init_params(guc, revision) };
    unsafe { intel_uc_fw_change_status(ptr::addr_of_mut!((*guc).fw), INTEL_UC_FIRMWARE_LOADABLE) };
    0
}

#[derive(Clone, Copy)]
enum GucInitUnwind {
    Firmware,
    Log,
    Capture,
    Ads,
    Ct,
    Submission,
}

unsafe fn guc_init_fail(guc: *mut IntelGuc, error: i32) -> i32 {
    unsafe {
        intel_uc_fw_change_status(ptr::addr_of_mut!((*guc).fw), INTEL_UC_FIRMWARE_INIT_FAIL);
    }
    guc_probe_error!(
        guc,
        "failed with %pe\n",
        crate::linux::config::ERR_PTR::<c_void>(error)
    );
    error
}

unsafe fn guc_init_unwind(guc: *mut IntelGuc, error: i32, unwind: GucInitUnwind) -> i32 {
    unsafe {
        if matches!(unwind, GucInitUnwind::Submission) {
            intel_guc_submission_fini(guc);
        }
        if matches!(unwind, GucInitUnwind::Submission | GucInitUnwind::Ct) {
            intel_guc_ct_fini(ptr::addr_of_mut!((*guc).ct));
        }
        if matches!(
            unwind,
            GucInitUnwind::Submission | GucInitUnwind::Ct | GucInitUnwind::Ads
        ) {
            intel_guc_ads_destroy(guc);
        }
        if matches!(
            unwind,
            GucInitUnwind::Submission
                | GucInitUnwind::Ct
                | GucInitUnwind::Ads
                | GucInitUnwind::Capture
        ) {
            intel_guc_capture_destroy(guc);
        }
        if matches!(
            unwind,
            GucInitUnwind::Submission
                | GucInitUnwind::Ct
                | GucInitUnwind::Ads
                | GucInitUnwind::Capture
                | GucInitUnwind::Log
        ) {
            intel_guc_log_destroy(ptr::addr_of_mut!((*guc).log));
        }
        if matches!(
            unwind,
            GucInitUnwind::Submission
                | GucInitUnwind::Ct
                | GucInitUnwind::Ads
                | GucInitUnwind::Capture
                | GucInitUnwind::Log
                | GucInitUnwind::Firmware
        ) {
            intel_uc_fw_fini(ptr::addr_of_mut!((*guc).fw));
        }
    }
    unsafe { guc_init_fail(guc, error) }
}

// upstream: intel_guc.c intel_guc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_fini(guc: *mut IntelGuc) {
    if !unsafe { crate::intel_uc_fw_upstream::intel_uc_fw_is_loadable(ptr::addr_of!((*guc).fw)) } {
        return;
    }
    unsafe { flush_work(ptr::addr_of_mut!((*guc).dead_guc_worker)) };
    if unsafe { intel_guc_slpc_is_used(guc) } {
        unsafe { intel_guc_slpc_fini(ptr::addr_of_mut!((*guc).slpc)) };
    }
    if unsafe { intel_guc_submission_is_used(guc) } {
        unsafe { intel_guc_submission_fini(guc) };
    }
    unsafe {
        intel_guc_ct_fini(ptr::addr_of_mut!((*guc).ct));
        intel_guc_ads_destroy(guc);
        intel_guc_capture_destroy(guc);
        intel_guc_log_destroy(ptr::addr_of_mut!((*guc).log));
        intel_uc_fw_fini(ptr::addr_of_mut!((*guc).fw));
    }
}

// upstream: intel_guc.c intel_guc_send_mmio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_send_mmio(
    guc: *mut IntelGuc,
    request: *const u32,
    len: u32,
    response_buf: *mut u32,
    response_buf_size: u32,
) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let uncore = unsafe { (*gt).uncore };
    GEM_BUG_ON!(len == 0);
    GEM_BUG_ON!(len > unsafe { (*guc).send_regs.count });
    let first = unsafe { ptr::read(request) };
    GEM_BUG_ON!(REG_FIELD_GET(GUC_HXG_MSG_0_ORIGIN, first) != HXG_ORIGIN_HOST);
    GEM_BUG_ON!(REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, first) != HXG_TYPE_REQUEST);

    unsafe { mutex_lock(ptr::addr_of_mut!((*guc).send_mutex)) };
    unsafe { intel_uncore_forcewake_get(uncore, (*guc).send_regs.fw_domains) };
    let ret = loop {
        for index in 0..len {
            let word = unsafe { ptr::read(request.add(index as usize)) };
            let reg = unsafe { guc_send_reg(guc, index) };
            unsafe { intel_uncore_write(uncore, reg, word) };
        }
        let last = unsafe { guc_send_reg(guc, len - 1) };
        unsafe { intel_uncore_posting_read(uncore, last) };
        unsafe { intel_guc_notify(guc) };

        let mut header = 0;
        let mut result = unsafe {
            crate::intel_uncore_types_upstream::__intel_wait_for_register_fw(
                uncore,
                guc_send_reg(guc, 0),
                GUC_HXG_MSG_0_ORIGIN,
                REG_FIELD_PREP(GUC_HXG_MSG_0_ORIGIN, HXG_ORIGIN_GUC),
                10,
                10,
                &mut header,
            )
        };
        if result != 0 {
            guc_err!(guc, "mmio request %#x: no reply %x\n", first, header);
            break result;
        }

        let response_type = REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, header);
        if response_type == HXG_TYPE_NO_RESPONSE_BUSY {
            let timed_out = wait_for!(
                {
                    header = unsafe { intel_uncore_read(uncore, guc_send_reg(guc, 0)) };
                    REG_FIELD_GET(GUC_HXG_MSG_0_ORIGIN, header) != HXG_ORIGIN_GUC
                        || REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, header) != HXG_TYPE_NO_RESPONSE_BUSY
                },
                1000
            );
            if timed_out {
                guc_err!(guc, "mmio request %#x: no reply %x\n", first, header);
                result = -ETIMEDOUT;
                break result;
            }
            if REG_FIELD_GET(GUC_HXG_MSG_0_ORIGIN, header) != HXG_ORIGIN_GUC {
                guc_err!(
                    guc,
                    "mmio request %#x: unexpected reply %#x\n",
                    first,
                    header
                );
                result = -EPROTO;
                break result;
            }
        }

        match REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, header) {
            HXG_TYPE_NO_RESPONSE_RETRY => {
                let reason = REG_FIELD_GET(GUC_HXG_RETRY_MSG_0_REASON, header);
                guc_dbg!(
                    guc,
                    "mmio request %#x: retrying, reason %u\n",
                    first,
                    reason
                );
                continue;
            }
            HXG_TYPE_RESPONSE_FAILURE => {
                let hint = REG_FIELD_GET(GUC_HXG_FAILURE_MSG_0_HINT, header);
                let error = REG_FIELD_GET(GUC_HXG_FAILURE_MSG_0_ERROR, header);
                guc_err!(guc, "mmio request %#x: failure %x/%u\n", first, error, hint);
                break -ENXIO;
            }
            HXG_TYPE_RESPONSE_SUCCESS => {
                if !response_buf.is_null() {
                    let count = min(response_buf_size, unsafe { (*guc).send_regs.count });
                    GEM_BUG_ON!(count == 0);
                    unsafe { ptr::write(response_buf, header) };
                    for index in 1..count {
                        let word = unsafe { intel_uncore_read(uncore, guc_send_reg(guc, index)) };
                        unsafe { ptr::write(response_buf.add(index as usize), word) };
                    }
                    break count as i32;
                }
                break REG_FIELD_GET(GUC_HXG_RESPONSE_MSG_0_DATA0, header) as i32;
            }
            _ => {
                guc_err!(
                    guc,
                    "mmio request %#x: unexpected reply %#x\n",
                    first,
                    header
                );
                break -EPROTO;
            }
        }
    };
    unsafe {
        intel_uncore_forcewake_put(uncore, (*guc).send_regs.fw_domains);
        mutex_unlock(ptr::addr_of_mut!((*guc).send_mutex));
    }
    ret
}

// upstream: intel_guc.c intel_guc_crash_process_msg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_crash_process_msg(guc: *mut IntelGuc, action: u32) -> i32 {
    if action == INTEL_GUC_ACTION_NOTIFY_CRASH_DUMP_POSTED {
        guc_err!(guc, "Crash dump notification\n");
    } else if action == INTEL_GUC_ACTION_NOTIFY_EXCEPTION {
        guc_err!(guc, "Exception notification\n");
    } else {
        guc_err!(guc, "Unknown crash notification: 0x%04X\n", action);
    }
    unsafe { queue_work(system_dfl_wq, ptr::addr_of_mut!((*guc).dead_guc_worker)) };
    0
}

// upstream: intel_guc.c intel_guc_to_host_process_recv_msg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_to_host_process_recv_msg(
    guc: *mut IntelGuc,
    payload: *const u32,
    len: u32,
) -> i32 {
    if len == 0 {
        return -EPROTO;
    }
    let msg = unsafe { ptr::read(payload) } & unsafe { (*guc).msg_enabled_mask };
    if msg & INTEL_GUC_RECV_MSG_CRASH_DUMP_POSTED != 0 {
        guc_err!(guc, "Received early crash dump notification!\n");
    }
    if msg & INTEL_GUC_RECV_MSG_EXCEPTION != 0 {
        guc_err!(guc, "Received early exception notification!\n");
    }
    if msg & (INTEL_GUC_RECV_MSG_CRASH_DUMP_POSTED | INTEL_GUC_RECV_MSG_EXCEPTION) != 0 {
        unsafe { queue_work(system_dfl_wq, ptr::addr_of_mut!((*guc).dead_guc_worker)) };
    }
    0
}

// Header inline entry points used by intel_uc.c and the local suspend path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_reset_interrupts(guc: *mut IntelGuc) {
    if let Some(reset) = unsafe { (*guc).interrupts.reset } {
        unsafe { reset(guc) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_enable_interrupts(guc: *mut IntelGuc) {
    if let Some(enable) = unsafe { (*guc).interrupts.enable } {
        unsafe { enable(guc) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_disable_interrupts(guc: *mut IntelGuc) {
    if let Some(disable) = unsafe { (*guc).interrupts.disable } {
        unsafe { disable(guc) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_sanitize(guc: *mut IntelGuc) -> i32 {
    unsafe {
        intel_uc_fw_sanitize(ptr::addr_of_mut!((*guc).fw));
        intel_guc_disable_interrupts(guc);
        intel_guc_ct_sanitize(ptr::addr_of_mut!((*guc).ct));
        (*guc).mmio_msg = 0;
    }
    0
}

// upstream: intel_guc.c intel_guc_auth_huc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_auth_huc(guc: *mut IntelGuc, rsa_offset: u32) -> i32 {
    let action = [INTEL_GUC_ACTION_AUTHENTICATE_HUC, rsa_offset];
    unsafe { intel_guc_send(guc, action.as_ptr(), action.len() as u32) }
}

// upstream: intel_guc.c intel_guc_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_suspend(guc: *mut IntelGuc) -> i32 {
    if !unsafe { intel_guc_is_ready(guc) } {
        return 0;
    }
    if unsafe { intel_guc_submission_is_used(guc) } {
        unsafe { flush_work(ptr::addr_of_mut!((*guc).dead_guc_worker)) };
        let action = [INTEL_GUC_ACTION_CLIENT_SOFT_RESET];
        let err = unsafe { intel_guc_send_mmio(guc, action.as_ptr(), 1, ptr::null_mut(), 0) };
        if err != 0 {
            guc_err!(
                guc,
                "suspend: RESET_CLIENT action failed with %pe\n",
                crate::linux::config::ERR_PTR::<c_void>(err)
            );
        }
    }
    unsafe { intel_guc_sanitize(guc) };
    0
}

// upstream: intel_guc.c intel_guc_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_resume(_guc: *mut IntelGuc) -> i32 {
    0
}

// upstream: intel_guc.c intel_guc_allocate_vma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_allocate_vma(guc: *mut IntelGuc, size: u32) -> *mut I915Vma {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let obj = if unsafe { crate::linux::i915::HAS_LMEM(i915) } {
        unsafe {
            i915_gem_object_create_lmem(
                i915,
                size as u64,
                (I915_BO_ALLOC_CPU_CLEAR | I915_BO_ALLOC_CONTIGUOUS | I915_BO_ALLOC_PM_EARLY)
                    as u32,
            )
        }
    } else {
        unsafe { i915_gem_object_create_shmem(i915, size as u64) }
    };
    if crate::linux::config::IS_ERR(obj) {
        return obj.cast::<I915Vma>();
    }
    if unsafe { crate::intel_gt_api_upstream::intel_gt_needs_wa_22016122933(gt) } {
        unsafe { i915_gem_object_set_cache_coherency(obj, I915_CACHE_NONE) };
    }
    let vma = unsafe { i915_vma_instance(obj, &mut (*(*gt).ggtt).vm, ptr::null()) };
    if crate::linux::config::IS_ERR(vma) {
        unsafe { i915_gem_object_put(obj) };
        return vma;
    }
    let flags = PIN_OFFSET_BIAS | unsafe { i915_ggtt_pin_bias(vma) };
    let ret = unsafe { i915_ggtt_pin(vma, ptr::null_mut(), 0, flags) };
    if ret != 0 {
        unsafe { i915_gem_object_put(obj) };
        return crate::linux::config::ERR_PTR(ret);
    }
    unsafe { i915_vma_make_unshrinkable(vma) }
}

// upstream: intel_guc.c intel_guc_allocate_and_map_vma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_allocate_and_map_vma(
    guc: *mut IntelGuc,
    size: u32,
    out_vma: *mut *mut I915Vma,
    out_vaddr: *mut *mut c_void,
) -> i32 {
    let vma = unsafe { intel_guc_allocate_vma(guc, size) };
    if crate::linux::config::IS_ERR(vma) {
        return crate::linux::config::PTR_ERR(vma);
    }
    let gt = unsafe { guc_to_gt(guc) };
    let obj = unsafe { (*vma).obj };
    let map_type = unsafe { intel_gt_coherent_map_type(gt, obj, true) };
    let vaddr = unsafe { i915_gem_object_pin_map_unlocked(obj, map_type) };
    if crate::linux::config::IS_ERR(vaddr) {
        let mut vma = vma;
        unsafe { i915_vma_unpin_and_release(&mut vma, 0) };
        return crate::linux::config::PTR_ERR(vaddr);
    }
    unsafe {
        ptr::write(out_vma, vma);
        ptr::write(out_vaddr, vaddr);
    }
    0
}

// upstream: intel_guc.c __guc_action_self_cfg()
unsafe fn __guc_action_self_cfg(guc: *mut IntelGuc, key: u16, len: u16, value: u64) -> i32 {
    let request = [
        REG_FIELD_PREP(GUC_HXG_MSG_0_ORIGIN, HXG_ORIGIN_HOST)
            | REG_FIELD_PREP(GUC_HXG_MSG_0_TYPE, HXG_TYPE_REQUEST)
            | REG_FIELD_PREP(GUC_HXG_REQUEST_MSG_0_ACTION, GUC_ACTION_HOST2GUC_SELF_CFG),
        REG_FIELD_PREP(HOST2GUC_SELF_CFG_REQUEST_MSG_1_KLV_KEY, key as u32)
            | REG_FIELD_PREP(HOST2GUC_SELF_CFG_REQUEST_MSG_1_KLV_LEN, len as u32),
        REG_FIELD_PREP(HOST2GUC_SELF_CFG_REQUEST_MSG_2_VALUE32, value as u32),
        REG_FIELD_PREP(
            HOST2GUC_SELF_CFG_REQUEST_MSG_3_VALUE64,
            (value >> 32) as u32,
        ),
    ];
    GEM_BUG_ON!(len > 2);
    GEM_BUG_ON!(len == 1 && value >> 32 != 0);
    let ret = unsafe {
        intel_guc_send_mmio(
            guc,
            request.as_ptr(),
            request.len() as u32,
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return ret;
    }
    if ret > 1 {
        return -EPROTO;
    }
    if ret == 0 {
        return -ENOKEY;
    }
    0
}

// upstream: intel_guc.c __guc_self_cfg()
unsafe fn __guc_self_cfg(guc: *mut IntelGuc, key: u16, len: u16, value: u64) -> i32 {
    let err = unsafe { __guc_action_self_cfg(guc, key, len, value) };
    if err != 0 {
        guc_probe_error!(
            guc,
            "Unsuccessful self-config (%pe) key %#hx value %#llx\n",
            crate::linux::config::ERR_PTR::<c_void>(err),
            key,
            value,
        );
    }
    err
}

// upstream: intel_guc.c intel_guc_self_cfg32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_self_cfg32(guc: *mut IntelGuc, key: u16, value: u32) -> i32 {
    unsafe { __guc_self_cfg(guc, key, 1, value as u64) }
}

// upstream: intel_guc.c intel_guc_self_cfg64()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_self_cfg64(guc: *mut IntelGuc, key: u16, value: u64) -> i32 {
    unsafe { __guc_self_cfg(guc, key, 2, value) }
}

// upstream: intel_guc.c intel_guc_load_status()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_load_status(guc: *mut IntelGuc, printer: *mut DrmPrinter) {
    if !unsafe { intel_guc_is_supported(guc) } {
        drm_printf!(printer, "GuC not supported\n");
        return;
    }
    if !unsafe { intel_guc_is_wanted(guc) } {
        drm_printf!(printer, "GuC disabled\n");
        return;
    }
    unsafe { intel_uc_fw_dump(ptr::addr_of!((*guc).fw), printer) };
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm) };
    let wakeref = intel_runtime_pm_get(unsafe { &*rpm });
    if !wakeref.is_null() {
        let uncore = unsafe { (*gt).uncore };
        let status = unsafe { intel_uncore_read(uncore, GUC_STATUS) };
        drm_printf!(printer, "GuC status 0x%08x:\n", status);
        drm_printf!(
            printer,
            "\tBootrom status = 0x%x\n",
            (status & GS_BOOTROM_MASK) >> GS_BOOTROM_SHIFT
        );
        drm_printf!(
            printer,
            "\tuKernel status = 0x%x\n",
            (status & GS_UKERNEL_MASK) >> GS_UKERNEL_SHIFT
        );
        drm_printf!(
            printer,
            "\tMIA Core status = 0x%x\n",
            (status & GS_MIA_MASK) >> GS_MIA_SHIFT
        );
        drm_printer_write(printer, "Scratch registers:\n");
        for index in 0..16 {
            let value = unsafe { intel_uncore_read(uncore, soft_scratch(index)) };
            drm_printf!(printer, "\t%2d: \t0x%x\n", index as i32, value);
        }
        unsafe { intel_runtime_pm_put(&*rpm, wakeref) };
    }
}

// upstream: intel_guc.c intel_guc_write_barrier()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_write_barrier(guc: *const IntelGuc) {
    let guc = guc.cast_mut();
    let gt = unsafe { guc_to_gt(guc) };
    let ct_vma = unsafe { (*guc).ct.vma };
    if unsafe { i915_gem_object_is_lmem((*ct_vma).obj) } {
        GEM_BUG_ON!(unsafe { (*guc).send_regs.fw_domains } != 0);
        unsafe {
            intel_uncore_write_fw((*gt).uncore, gen11_soft_scratch(0), 0);
        }
    } else {
        wmb();
    }
}

// upstream: intel_guc_ads.c intel_guc_engine_usage_offset().
pub unsafe fn intel_guc_engine_usage_offset(guc: *mut IntelGuc) -> u32 {
    let base = unsafe { intel_guc_ggtt_offset(guc, (*guc).ads_vma) };
    guc_ads::engine_usage_offset(base).expect("valid page-aligned GuC ADS GGTT address")
}

// upstream: intel_guc_ads.c intel_guc_engine_usage_record_map().
pub unsafe fn intel_guc_engine_usage_record_map(engine: &IntelEngineCs) -> IosysMap {
    let guc = unsafe { gt_to_guc(engine.gt) };
    let guc_class = guc_ads::engine_class_to_guc_class(engine.class)
        .expect("registered engine class has a GuC class");
    let mask = engine.logical_mask;
    assert_ne!(mask, 0, "engine logical mask must be nonzero");
    let logical_index = (u32::BITS - 1 - mask.leading_zeros()) as u8;
    let offset = guc_ads::engine_usage_record_offset(guc_class, logical_index)
        .expect("engine usage record index is in the ADS array");
    let map = unsafe { &(*guc).ads_map };
    let mut result = IosysMap {
        addr: IosysMapAddr {
            vaddr: core::ptr::null_mut(),
        },
        is_iomem: map.is_iomem,
    };
    unsafe {
        if map.is_iomem {
            let base = map.addr.vaddr_iomem.cast::<u8>();
            result.addr.vaddr_iomem = base.wrapping_add(offset).cast::<c_void>();
        } else {
            let base = map.addr.vaddr.cast::<u8>();
            result.addr.vaddr = base.wrapping_add(offset).cast::<c_void>();
        }
    }
    result
}

// upstream: intel_guc.h intel_guc_ggtt_offset()
pub unsafe fn intel_guc_ggtt_offset(_guc: *mut IntelGuc, vma: *const I915Vma) -> u32 {
    const GUC_GGTT_TOP: u64 = 0xFEE0_0000;
    let offset = unsafe { i915_ggtt_offset(vma) };
    GEM_BUG_ON!(offset < unsafe { i915_ggtt_pin_bias(vma) });
    let start = offset as u64;
    let size = unsafe { (*vma).size };
    GEM_BUG_ON!(start >= GUC_GGTT_TOP || size > GUC_GGTT_TOP - start);
    offset
}
