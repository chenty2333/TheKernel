// SPDX-License-Identifier: MIT
// Copyright © 2016-2019 Intel Corporation.
// Source: Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_uc.c.
// GuC/HuC/GSC, reset, power, MMIO, and runtime-PM services remain owner APIs;
// this module preserves the source's uC policy and lifecycle sequencing.

#![allow(dead_code, non_snake_case, unsafe_code)]

use core::{ffi::c_char, ptr};

use crate::{
    i915_gem_object_api_upstream::{i915_gem_object_get, i915_gem_object_put},
    intel_engine_types_upstream::IntelEngineMask,
    intel_gsc_uc_types_upstream::{
        intel_gsc_uc_fini, intel_gsc_uc_flush_work, intel_gsc_uc_init, intel_gsc_uc_init_early,
        intel_gsc_uc_load_start, intel_gsc_uc_resume,
    },
    intel_gt_api_upstream::{guc_to_gt, uc_to_gt},
    intel_gt_types_upstream::IntelGt,
    intel_guc_ct_types_upstream::IntelGucCt,
    intel_guc_slpc_types_upstream::IntelGucSlpc,
    intel_guc_submission_types_upstream::{
        intel_guc_submission_disable, intel_guc_submission_enable, intel_guc_submission_flush_work,
        intel_guc_wait_for_pending_msg,
    },
    intel_guc_types_upstream::{IntelGuc, intel_guc_is_fw_running},
    intel_huc_types_upstream::{
        INTEL_HUC_AUTH_BY_GUC, intel_huc_auth, intel_huc_fini, intel_huc_fini_late, intel_huc_init,
        intel_huc_init_early, intel_huc_is_loaded_by_gsc, intel_huc_sanitize,
        intel_huc_update_auth_status,
    },
    intel_uc_fw_types_upstream::{INTEL_UC_FIRMWARE_ERROR, IntelUcFw},
    intel_uc_fw_upstream::intel_uc_fw_is_loadable,
    intel_uc_types_upstream::{
        IntelUc, IntelUcOps, intel_uc_supports_guc, intel_uc_supports_guc_submission,
        intel_uc_uses_gsc_uc, intel_uc_uses_guc, intel_uc_uses_guc_slpc,
        intel_uc_uses_guc_submission, intel_uc_wants_gsc_uc, intel_uc_wants_guc,
        intel_uc_wants_guc_slpc, intel_uc_wants_guc_submission, intel_uc_wants_huc,
    },
    intel_uncore_types_upstream::{
        IntelUncore, intel_uncore_read, intel_uncore_write, intel_uncore_write_and_verify,
    },
    intel_wopcm_types_upstream::{intel_wopcm_guc_base, intel_wopcm_guc_size},
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        config::{E2BIG, EINVAL, EIO, ENODEV, HZ},
        i915::{GRAPHICS_VER, IS_ALDERLAKE_S, IS_ROCKETLAKE, IS_SUBPLATFORM, IS_TIGERLAKE},
        i915_private::DrmI915Private,
        locks::{spin_lock_irq, spin_unlock_irq},
        memory::atomic_read,
        primitives::{fetch_and_zero, str_yes_no},
        print::DrmLogLevel,
    },
    linux_print::{CFormatArg, drm_log_at, format_message},
    wopcm::{
        DMA_GUC_WOPCM_OFFSET, GUC_WOPCM_OFFSET_MASK, GUC_WOPCM_OFFSET_VALID, GUC_WOPCM_SIZE,
        GUC_WOPCM_SIZE_LOCKED, GUC_WOPCM_SIZE_MASK, HUC_LOADING_AGENT_GUC,
    },
};

const ENABLE_GUC_SUBMISSION: i32 = 1 << 0;
const ENABLE_GUC_LOAD_HUC: i32 = 1 << 1;
const ENABLE_GUC_MASK: i32 = ENABLE_GUC_SUBMISSION | ENABLE_GUC_LOAD_HUC;
const INTEL_ALDERLAKE_S: u32 = 34;
const INTEL_SUBPLATFORM_RPL: u32 = 0;
const GUC_STATUS: I915RegT = I915RegT { reg: 0xc000 };
const GS_MIA_IN_RESET: u32 = 1;
const SOFT_SCRATCH_15: I915RegT = I915RegT { reg: 0xc1bc };

#[inline]
fn enabled_disabled(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}
const ENOEXEC: i32 = 8;
const ENOENT: i32 = 2;
const ESTALE: i32 = 116;
const EACCES: i32 = 13;
const EPERM: i32 = 1;
const HZ_DIV_5: isize = HZ as isize / 5;

macro_rules! uc_gt_log {
    ($severity:expr, $gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __gt = $gt;
        let __id = unsafe { (*__gt).info.id };
        let __args: &[&dyn CFormatArg] = &[$(&($argument) as &dyn CFormatArg),*];
        let __message = format_message($format, __args);
        drm_log_at($severity, &alloc::format!("GT{__id}"), file!(), line!(), &__message);
    }};
}
macro_rules! uc_gt_info {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        uc_gt_log!(DrmLogLevel::Info, $gt, $format $(, $argument)*);
    }};
}
macro_rules! uc_gt_probe_error {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        uc_gt_log!(DrmLogLevel::Error, $gt, $format $(, $argument)*);
    }};
}
macro_rules! uc_guc_info {
    ($guc:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __guc = $guc;
        let __gt = unsafe { guc_to_gt(__guc) };
        uc_gt_log!(DrmLogLevel::Info, __gt, $format $(, $argument)*);
    }};
}

fn firmware_type_repr(ty: i32) -> &'static str {
    match ty {
        0 => "GuC",
        1 => "HuC",
        2 => "GSC",
        _ => "uC",
    }
}

unsafe fn firmware_path(path: *const c_char) -> alloc::string::String {
    if path.is_null() {
        return alloc::string::String::from("<none>");
    }
    unsafe { core::ffi::CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned()
}

unsafe fn firmware_status_to_error(status: i32) -> i32 {
    match status {
        -1 => -ENODEV,
        0 => -EACCES,
        1 => -EPERM,
        2 => -ESTALE,
        3 => -ENOENT,
        4 => -ENOEXEC,
        6 | 8 => -EIO,
        5 | 7 | 9 | 10 => 0,
        _ => -EINVAL,
    }
}

// upstream: gt/uc/intel_guc_ct.h intel_guc_ct_enabled()
#[inline]
#[unsafe(no_mangle)]
unsafe extern "C" fn intel_guc_ct_enabled(ct: *const IntelGucCt) -> bool {
    unsafe { (*ct).enabled }
}
#[inline]
unsafe fn intel_guc_is_ready(guc: *const IntelGuc) -> bool {
    unsafe { intel_guc_is_fw_running(guc) && intel_guc_ct_enabled(ptr::addr_of!((*guc).ct)) }
}

// These services receive pointers to opaque driver state; no Rust aggregate
// layout is passed by value across this C ABI boundary.
#[allow(improper_ctypes)]
unsafe extern "C" {
    fn intel_reset_guc(gt: *mut IntelGt) -> i32;
    fn intel_guc_init_early(guc: *mut IntelGuc);
    fn intel_guc_init_late(guc: *mut IntelGuc);
    fn intel_guc_init_send_regs(guc: *mut IntelGuc);
    fn intel_guc_init(guc: *mut IntelGuc) -> i32;
    fn intel_guc_fini(guc: *mut IntelGuc);
    fn intel_guc_sanitize(guc: *mut IntelGuc) -> i32;
    fn intel_guc_write_params(guc: *mut IntelGuc);
    fn intel_guc_fw_upload(guc: *mut IntelGuc) -> i32;
    fn intel_guc_ads_reset(guc: *mut IntelGuc);
    fn intel_guc_reset_interrupts(guc: *mut IntelGuc);
    fn intel_guc_enable_interrupts(guc: *mut IntelGuc);
    fn intel_guc_disable_interrupts(guc: *mut IntelGuc);
    fn intel_guc_ct_enable(ct: *mut IntelGucCt) -> i32;
    fn intel_guc_ct_disable(ct: *mut IntelGucCt);
    fn intel_guc_ct_event_handler(ct: *mut IntelGucCt);
    fn intel_guc_to_host_process_recv_msg(guc: *mut IntelGuc, payload: *const u32, len: u32) -> i32;
    fn intel_guc_submission_reset_prepare(guc: *mut IntelGuc);
    fn intel_guc_submission_reset(guc: *mut IntelGuc, stalled: IntelEngineMask);
    fn intel_guc_submission_reset_finish(guc: *mut IntelGuc);
    fn intel_guc_submission_cancel_requests(guc: *mut IntelGuc);
    fn intel_guc_slpc_enable(slpc: *mut IntelGucSlpc) -> i32;
    fn intel_guc_pm_intrmsk_enable(gt: *mut IntelGt);
    fn intel_guc_suspend(guc: *mut IntelGuc) -> i32;
    fn intel_guc_resume(guc: *mut IntelGuc) -> i32;
    fn intel_guc_tlb_invalidation_is_available(guc: *mut IntelGuc) -> bool;
    fn intel_guc_invalidate_tlb_engines(guc: *mut IntelGuc);
    fn intel_guc_invalidate_tlb_guc(guc: *mut IntelGuc);
    fn wake_up_all_tlb_invalidate(guc: *mut IntelGuc);
    fn intel_rps_raise_unslice(rps: *mut crate::intel_rps_types_upstream::IntelRps);
    fn intel_rps_lower_unslice(rps: *mut crate::intel_rps_types_upstream::IntelRps);
    fn i915_hwmon_power_max_disable(i915: *mut DrmI915Private, was_enabled: *mut bool);
    fn i915_hwmon_power_max_restore(i915: *mut DrmI915Private, was_enabled: bool);
}

static UC_OPS_OFF: IntelUcOps = IntelUcOps {
    sanitize: None,
    init_fw: None,
    fini_fw: None,
    init: None,
    fini: Some(__uc_fini),
    init_hw: Some(__uc_check_hw),
    fini_hw: None,
    resume_mappings: None,
};
static UC_OPS_ON: IntelUcOps = IntelUcOps {
    sanitize: Some(__uc_sanitize),
    init_fw: Some(__uc_fetch_firmwares),
    fini_fw: Some(__uc_cleanup_firmwares),
    init: Some(__uc_init),
    fini: Some(__uc_fini),
    init_hw: Some(__uc_init_hw),
    fini_hw: Some(__uc_fini_hw),
    resume_mappings: Some(__uc_resume_mappings),
};

// upstream: intel_uc.c uc_expand_default_options()
unsafe fn uc_expand_default_options(uc: *mut IntelUc) {
    let i915 = unsafe { (*uc_to_gt(uc)).i915 };
    if unsafe { (*i915).params.enable_guc } != -1 {
        return;
    }
    if unsafe { GRAPHICS_VER(i915) } < 12 {
        unsafe {
            (*i915).params.enable_guc = 0;
        }
        return;
    }
    if unsafe { IS_TIGERLAKE(i915) || IS_ROCKETLAKE(i915) } {
        unsafe {
            (*i915).params.enable_guc = 0;
        }
        return;
    }
    if unsafe { IS_ALDERLAKE_S(i915) }
        && !unsafe { IS_SUBPLATFORM(i915, INTEL_ALDERLAKE_S, INTEL_SUBPLATFORM_RPL) }
    {
        unsafe {
            (*i915).params.enable_guc = ENABLE_GUC_LOAD_HUC;
        }
        return;
    }
    unsafe {
        (*i915).params.enable_guc = ENABLE_GUC_LOAD_HUC | ENABLE_GUC_SUBMISSION;
    }
}

// upstream: intel_uc.c __intel_uc_reset_hw()
unsafe fn __intel_uc_reset_hw(uc: *mut IntelUc) -> i32 {
    let gt = unsafe { uc_to_gt(uc) };
    let ret = unsafe { intel_reset_guc(gt) };
    if ret != 0 {
        gt_err!(gt, "Failed to reset GuC, ret = %d\\n", ret);
        return ret;
    }
    let status = unsafe { intel_uncore_read((*gt).uncore, GUC_STATUS) };
    if status & GS_MIA_IN_RESET == 0 {
        uc_gt_log!(
            DrmLogLevel::Warn,
            gt,
            "GuC status: 0x%x, MIA core expected to be in reset\\n",
            status
        );
    }
    ret
}

// upstream: intel_uc.c __confirm_options()
unsafe fn __confirm_options(uc: *mut IntelUc) {
    let gt = unsafe { uc_to_gt(uc) };
    let i915 = unsafe { (*gt).i915 };
    gt_dbg!(
        gt,
        "enable_guc=%d (guc:%s submission:%s huc:%s slpc:%s)\\n",
        unsafe { (*i915).params.enable_guc },
        str_yes_no(unsafe { intel_uc_wants_guc(uc) }),
        str_yes_no(unsafe { intel_uc_wants_guc_submission(uc) }),
        str_yes_no(unsafe { intel_uc_wants_huc(uc) }),
        str_yes_no(unsafe { intel_uc_wants_guc_slpc(uc) }),
    );
    if unsafe { (*i915).params.enable_guc } == 0 {
        GEM_BUG_ON!(unsafe { intel_uc_wants_guc(uc) });
        GEM_BUG_ON!(unsafe { intel_uc_wants_guc_submission(uc) });
        GEM_BUG_ON!(unsafe { intel_uc_wants_huc(uc) });
        GEM_BUG_ON!(unsafe { intel_uc_wants_guc_slpc(uc) });
        return;
    }
    if !unsafe { intel_uc_supports_guc(uc) } {
        uc_gt_info!(
            gt,
            "Incompatible option enable_guc=%d - %s\\n",
            unsafe { (*i915).params.enable_guc },
            "GuC is not supported!"
        );
    }
    if unsafe { (*i915).params.enable_guc } & ENABLE_GUC_SUBMISSION != 0
        && !unsafe { intel_uc_supports_guc_submission(uc) }
    {
        uc_gt_info!(
            gt,
            "Incompatible option enable_guc=%d - %s\\n",
            unsafe { (*i915).params.enable_guc },
            "GuC submission is N/A"
        );
    }
    if unsafe { (*i915).params.enable_guc } & !ENABLE_GUC_MASK != 0 {
        uc_gt_info!(
            gt,
            "Incompatible option enable_guc=%d - %s\\n",
            unsafe { (*i915).params.enable_guc },
            "undocumented flag"
        );
    }
}

// upstream: intel_uc.c intel_uc_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_init_early(uc: *mut IntelUc) {
    unsafe {
        uc_expand_default_options(uc);
    }
    unsafe {
        intel_guc_init_early(ptr::addr_of_mut!((*uc).guc));
        intel_huc_init_early(ptr::addr_of_mut!((*uc).huc));
        intel_gsc_uc_init_early(ptr::addr_of_mut!((*uc).gsc));
        __confirm_options(uc);
        (*uc).ops = if intel_uc_wants_guc(uc) {
            &UC_OPS_ON
        } else {
            &UC_OPS_OFF
        };
    }
}

// upstream: intel_uc.c intel_uc_init_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_init_late(uc: *mut IntelUc) {
    unsafe {
        intel_guc_init_late(ptr::addr_of_mut!((*uc).guc));
        intel_gsc_uc_load_start(ptr::addr_of_mut!((*uc).gsc));
    }
}

// upstream: intel_uc.c intel_uc_driver_late_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_driver_late_release(uc: *mut IntelUc) {
    unsafe {
        intel_huc_fini_late(ptr::addr_of_mut!((*uc).huc));
    }
}

// upstream: intel_uc.c intel_uc_init_mmio()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_init_mmio(uc: *mut IntelUc) {
    unsafe {
        intel_guc_init_send_regs(ptr::addr_of_mut!((*uc).guc));
    }
}

// upstream: intel_uc.c __uc_capture_load_err_log()
unsafe extern "C" fn __uc_capture_load_err_log(uc: *mut IntelUc) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    if unsafe { !(*guc).log.vma.is_null() && (*uc).load_err_log.is_null() } {
        let obj = unsafe { (*(*guc).log.vma).obj };
        unsafe {
            (*uc).load_err_log = i915_gem_object_get(obj);
        }
    }
}

// upstream: intel_uc.c __uc_free_load_err_log()
unsafe extern "C" fn __uc_free_load_err_log(uc: *mut IntelUc) {
    let log = unsafe { fetch_and_zero(&mut (*uc).load_err_log) };
    if !log.is_null() {
        unsafe {
            i915_gem_object_put(log);
        }
    }
}

// upstream: intel_uc.c intel_uc_driver_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_driver_remove(uc: *mut IntelUc) {
    unsafe {
        crate::intel_uc_types_upstream::intel_uc_fini_hw(uc);
        crate::intel_uc_types_upstream::intel_uc_fini(uc);
        __uc_free_load_err_log(uc);
    }
}

// upstream: intel_uc.c guc_clear_mmio_msg()
unsafe fn guc_clear_mmio_msg(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    unsafe {
        intel_uncore_write((*gt).uncore, SOFT_SCRATCH_15, 0);
    }
}

// upstream: intel_uc.c guc_get_mmio_msg()
unsafe fn guc_get_mmio_msg(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        spin_lock_irq(&mut (*guc).irq_lock);
    }
    let val = unsafe { intel_uncore_read(uncore, SOFT_SCRATCH_15) };
    unsafe {
        (*guc).mmio_msg |= val & (*guc).msg_enabled_mask;
    }
    unsafe {
        guc_clear_mmio_msg(guc);
        spin_unlock_irq(&mut (*guc).irq_lock);
    }
}

// upstream: intel_uc.c guc_handle_mmio_msg()
unsafe fn guc_handle_mmio_msg(guc: *mut IntelGuc) {
    GEM_BUG_ON!(!unsafe { intel_guc_ct_enabled(ptr::addr_of!((*guc).ct)) });
    unsafe {
        spin_lock_irq(&mut (*guc).irq_lock);
        if (*guc).mmio_msg != 0 {
            intel_guc_to_host_process_recv_msg(guc, ptr::addr_of!((*guc).mmio_msg), 1);
            (*guc).mmio_msg = 0;
        }
        spin_unlock_irq(&mut (*guc).irq_lock);
    }
}

// upstream: intel_uc.c guc_enable_communication()
unsafe fn guc_enable_communication(guc: *mut IntelGuc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let ct = unsafe { ptr::addr_of_mut!((*guc).ct) };
    GEM_BUG_ON!(unsafe { intel_guc_ct_enabled(ct) });
    let ret = unsafe { intel_guc_ct_enable(ct) };
    if ret != 0 {
        return ret;
    }
    unsafe {
        guc_get_mmio_msg(guc);
        guc_handle_mmio_msg(guc);
        intel_guc_enable_interrupts(guc);
    }
    let gt_irq_lock = unsafe { (*gt).irq_lock };
    unsafe {
        spin_lock_irq(&mut *gt_irq_lock);
        intel_guc_ct_event_handler(ct);
        spin_unlock_irq(&mut *gt_irq_lock);
    }
    guc_dbg!(guc, "communication enabled\\n");
    0
}

// upstream: intel_uc.c guc_disable_communication()
unsafe fn guc_disable_communication(guc: *mut IntelGuc) {
    unsafe {
        guc_clear_mmio_msg(guc);
        intel_guc_disable_interrupts(guc);
        intel_guc_ct_disable(ptr::addr_of_mut!((*guc).ct));
        guc_get_mmio_msg(guc);
    }
    guc_dbg!(guc, "communication disabled\\n");
}

// upstream: intel_uc.c __uc_fetch_firmwares()
unsafe extern "C" fn __uc_fetch_firmwares(uc: *mut IntelUc) {
    let gt = unsafe { uc_to_gt(uc) };
    GEM_BUG_ON!(!unsafe { intel_uc_wants_guc(uc) });
    let guc_fw = unsafe { ptr::addr_of_mut!((*uc).guc.fw) };
    let err = unsafe { crate::intel_uc_fw_upstream::intel_uc_fw_fetch(guc_fw) };
    if err != 0 {
        if unsafe { intel_uc_wants_huc(uc) } {
            gt_dbg!(gt, "Failed to fetch GuC fw (%d) disabling HuC\\n", err);
            unsafe {
                crate::intel_uc_fw_upstream::intel_uc_fw_change_status(
                    ptr::addr_of_mut!((*uc).huc.fw),
                    INTEL_UC_FIRMWARE_ERROR,
                );
            }
        }
        if unsafe { intel_uc_wants_gsc_uc(uc) } {
            gt_dbg!(gt, "Failed to fetch GuC fw (%d) disabling GSC\\n", err);
            unsafe {
                crate::intel_uc_fw_upstream::intel_uc_fw_change_status(
                    ptr::addr_of_mut!((*uc).gsc.fw),
                    INTEL_UC_FIRMWARE_ERROR,
                );
            }
        }
        return;
    }
    if unsafe { intel_uc_wants_huc(uc) } {
        unsafe {
            crate::intel_uc_fw_upstream::intel_uc_fw_fetch(ptr::addr_of_mut!((*uc).huc.fw));
        }
    }
    if unsafe { intel_uc_wants_gsc_uc(uc) } {
        unsafe {
            crate::intel_uc_fw_upstream::intel_uc_fw_fetch(ptr::addr_of_mut!((*uc).gsc.fw));
        }
    }
}

// upstream: intel_uc.c __uc_cleanup_firmwares()
unsafe extern "C" fn __uc_cleanup_firmwares(uc: *mut IntelUc) {
    unsafe {
        crate::intel_uc_fw_upstream::intel_uc_fw_cleanup_fetch(ptr::addr_of_mut!((*uc).gsc.fw));
        crate::intel_uc_fw_upstream::intel_uc_fw_cleanup_fetch(ptr::addr_of_mut!((*uc).huc.fw));
        crate::intel_uc_fw_upstream::intel_uc_fw_cleanup_fetch(ptr::addr_of_mut!((*uc).guc.fw));
    }
}

// upstream: intel_uc.c __uc_init()
unsafe extern "C" fn __uc_init(uc: *mut IntelUc) -> i32 {
    GEM_BUG_ON!(!unsafe { intel_uc_wants_guc(uc) });
    if !unsafe { intel_uc_uses_guc(uc) } {
        return 0;
    }
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    let ret = unsafe { intel_guc_init(guc) };
    if ret != 0 {
        return ret;
    }
    if unsafe { crate::intel_uc_types_upstream::intel_uc_uses_huc(uc) } {
        unsafe {
            intel_huc_init(ptr::addr_of_mut!((*uc).huc));
        }
    }
    if unsafe { intel_uc_uses_gsc_uc(uc) } {
        unsafe {
            intel_gsc_uc_init(ptr::addr_of_mut!((*uc).gsc));
        }
    }
    0
}

// upstream: intel_uc.c __uc_fini()
unsafe extern "C" fn __uc_fini(uc: *mut IntelUc) {
    unsafe {
        intel_gsc_uc_fini(ptr::addr_of_mut!((*uc).gsc));
        intel_huc_fini(ptr::addr_of_mut!((*uc).huc));
        intel_guc_fini(ptr::addr_of_mut!((*uc).guc));
    }
}

// upstream: intel_uc.c __uc_sanitize()
unsafe extern "C" fn __uc_sanitize(uc: *mut IntelUc) -> i32 {
    GEM_BUG_ON!(!unsafe { intel_uc_supports_guc(uc) });
    unsafe {
        intel_huc_sanitize(ptr::addr_of_mut!((*uc).huc));
        intel_guc_sanitize(ptr::addr_of_mut!((*uc).guc));
    }
    unsafe { __intel_uc_reset_hw(uc) }
}

// upstream: intel_uc.c uc_init_wopcm()
unsafe fn uc_init_wopcm(uc: *mut IntelUc) -> i32 {
    let gt = unsafe { uc_to_gt(uc) };
    let uncore = unsafe { (*gt).uncore };
    let wopcm = unsafe { &(*gt).wopcm };
    let base = intel_wopcm_guc_base(wopcm);
    let size = intel_wopcm_guc_size(wopcm);
    let huc_agent = if unsafe { crate::intel_uc_types_upstream::intel_uc_uses_huc(uc) } {
        HUC_LOADING_AGENT_GUC
    } else {
        0
    };
    if base == 0 || size == 0 {
        uc_gt_probe_error!(gt, "Unsuccessful WOPCM partitioning\\n");
        return -E2BIG;
    }
    GEM_BUG_ON!(!unsafe { intel_uc_supports_guc(uc) });
    GEM_BUG_ON!(base & GUC_WOPCM_OFFSET_MASK == 0);
    GEM_BUG_ON!(base & !GUC_WOPCM_OFFSET_MASK != 0);
    GEM_BUG_ON!(size & GUC_WOPCM_SIZE_MASK == 0);
    GEM_BUG_ON!(size & !GUC_WOPCM_SIZE_MASK != 0);

    let mask = GUC_WOPCM_SIZE_MASK | GUC_WOPCM_SIZE_LOCKED;
    let mut err = unsafe {
        intel_uncore_write_and_verify(
            uncore,
            I915RegT {
                reg: GUC_WOPCM_SIZE,
            },
            size,
            mask,
            size | GUC_WOPCM_SIZE_LOCKED,
        )
    };
    if err != 0 {
        return unsafe { uc_init_wopcm_error(gt, uncore, err) };
    }

    let mask = GUC_WOPCM_OFFSET_MASK | GUC_WOPCM_OFFSET_VALID | huc_agent;
    err = unsafe {
        intel_uncore_write_and_verify(
            uncore,
            I915RegT {
                reg: DMA_GUC_WOPCM_OFFSET,
            },
            base | huc_agent,
            mask,
            base | huc_agent | GUC_WOPCM_OFFSET_VALID,
        )
    };
    if err != 0 {
        return unsafe { uc_init_wopcm_error(gt, uncore, err) };
    }
    0
}

unsafe fn uc_init_wopcm_error(gt: *mut IntelGt, uncore: *mut IntelUncore, err: i32) -> i32 {
    uc_gt_probe_error!(gt, "Failed to init uC WOPCM registers!\\n");
    let offset = unsafe {
        intel_uncore_read(
            uncore,
            I915RegT {
                reg: DMA_GUC_WOPCM_OFFSET,
            },
        )
    };
    let size = unsafe {
        intel_uncore_read(
            uncore,
            I915RegT {
                reg: GUC_WOPCM_SIZE,
            },
        )
    };
    uc_gt_probe_error!(
        gt,
        "%s(%#x)=%#x\\n",
        "DMA_GUC_WOPCM_OFFSET",
        DMA_GUC_WOPCM_OFFSET,
        offset
    );
    uc_gt_probe_error!(gt, "%s(%#x)=%#x\\n", "GUC_WOPCM_SIZE", GUC_WOPCM_SIZE, size);
    err
}

// upstream: intel_uc.c uc_is_wopcm_locked()
unsafe fn uc_is_wopcm_locked(uc: *mut IntelUc) -> bool {
    let gt = unsafe { uc_to_gt(uc) };
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        (intel_uncore_read(
            uncore,
            I915RegT {
                reg: GUC_WOPCM_SIZE,
            },
        ) & GUC_WOPCM_SIZE_LOCKED)
            != 0
            || (intel_uncore_read(
                uncore,
                I915RegT {
                    reg: DMA_GUC_WOPCM_OFFSET,
                },
            ) & GUC_WOPCM_OFFSET_VALID)
                != 0
    }
}

// upstream: intel_uc.c __uc_check_hw()
unsafe extern "C" fn __uc_check_hw(uc: *mut IntelUc) -> i32 {
    if unsafe { (*uc).fw_table_invalid } {
        return -EIO;
    }
    if !unsafe { intel_uc_supports_guc(uc) } {
        return 0;
    }
    if unsafe { uc_is_wopcm_locked(uc) } {
        return -EIO;
    }
    0
}

// upstream: intel_uc.c print_fw_ver()
unsafe fn print_fw_ver(gt: *mut IntelGt, fw: *mut IntelUcFw) {
    let path = unsafe { firmware_path((*fw).file_selected.path) };
    uc_gt_info!(
        gt,
        "%s firmware %s version %u.%u.%u\\n",
        firmware_type_repr(unsafe { (*fw).r#type }),
        path,
        unsafe { (*fw).file_selected.ver.major },
        unsafe { (*fw).file_selected.ver.minor },
        unsafe { (*fw).file_selected.ver.patch },
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UcInitCleanup {
    None,
    Rps,
    Capture,
    Submission,
}

// upstream: intel_uc.c __uc_init_hw()
unsafe extern "C" fn __uc_init_hw(uc: *mut IntelUc) -> i32 {
    let gt = unsafe { uc_to_gt(uc) };
    let i915 = unsafe { (*gt).i915 };
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    let huc = unsafe { ptr::addr_of_mut!((*uc).huc) };
    let mut pl1en = false;
    GEM_BUG_ON!(!unsafe { intel_uc_supports_guc(uc) });
    GEM_BUG_ON!(!unsafe { intel_uc_wants_guc(uc) });
    unsafe {
        print_fw_ver(gt, ptr::addr_of_mut!((*guc).fw));
    }
    if unsafe { crate::intel_uc_types_upstream::intel_uc_uses_huc(uc) } {
        unsafe {
            print_fw_ver(gt, ptr::addr_of_mut!((*huc).fw));
        }
    }

    let mut ret = 0;
    let mut cleanup = UcInitCleanup::None;
    let mut initialized = false;
    'initialize: {
        if !unsafe { intel_uc_fw_is_loadable(ptr::addr_of!((*guc).fw)) } {
            let blocked = unsafe { __uc_check_hw(uc) } != 0
                || unsafe { (*ptr::addr_of!((*guc).fw)).user_overridden }
                || unsafe { intel_uc_wants_guc_submission(uc) };
            if blocked {
                let status = unsafe { (*guc).fw.status.status };
                ret = unsafe { firmware_status_to_error(status) };
            }
            break 'initialize;
        }

        ret = unsafe { uc_init_wopcm(uc) };
        if ret != 0 {
            break 'initialize;
        }

        unsafe {
            intel_guc_reset_interrupts(guc);
        }
        let mut attempts = if unsafe { GRAPHICS_VER(i915) == 9 } {
            3
        } else {
            1
        };
        unsafe {
            i915_hwmon_power_max_disable(i915, &mut pl1en);
        }
        let rps = unsafe { ptr::addr_of_mut!((*gt).rps) };
        unsafe {
            intel_rps_raise_unslice(rps);
        }
        cleanup = UcInitCleanup::Rps;
        while attempts != 0 {
            attempts -= 1;
            ret = unsafe { __uc_sanitize(uc) };
            if ret != 0 {
                break 'initialize;
            }
            unsafe {
                crate::intel_huc_fw_upstream::intel_huc_fw_upload(huc);
            }
            unsafe {
                intel_guc_ads_reset(guc);
                intel_guc_write_params(guc);
            }
            ret = unsafe { intel_guc_fw_upload(guc) };
            if ret == 0 {
                break;
            }
            gt_dbg!(
                gt,
                "GuC fw load failed (%d) will reset and retry %d more time(s)\\n",
                ret,
                attempts
            );
        }
        if ret != 0 {
            cleanup = UcInitCleanup::Capture;
            break 'initialize;
        }
        ret = unsafe { guc_enable_communication(guc) };
        if ret != 0 {
            cleanup = UcInitCleanup::Capture;
            break 'initialize;
        }
        if unsafe { intel_huc_is_loaded_by_gsc(huc) } {
            unsafe {
                intel_huc_update_auth_status(huc);
            }
        } else {
            unsafe {
                intel_huc_auth(huc, INTEL_HUC_AUTH_BY_GUC);
            }
        }
        if unsafe { intel_uc_uses_guc_submission(uc) } {
            ret = unsafe { intel_guc_submission_enable(guc) };
            if ret != 0 {
                cleanup = UcInitCleanup::Capture;
                break 'initialize;
            }
        }
        if unsafe { intel_uc_uses_guc_slpc(uc) } {
            ret = unsafe { intel_guc_slpc_enable(ptr::addr_of_mut!((*guc).slpc)) };
            if ret != 0 {
                cleanup = UcInitCleanup::Submission;
                break 'initialize;
            }
        } else {
            unsafe {
                intel_rps_lower_unslice(rps);
            }
        }
        unsafe {
            i915_hwmon_power_max_restore(i915, pl1en);
        }
        uc_guc_info!(
            guc,
            "submission %s\\n",
            enabled_disabled(unsafe { intel_uc_uses_guc_submission(uc) })
        );
        uc_guc_info!(
            guc,
            "SLPC %s\\n",
            enabled_disabled(unsafe { intel_uc_uses_guc_slpc(uc) })
        );
        initialized = true;
    }

    if initialized {
        return 0;
    }
    if cleanup == UcInitCleanup::Submission {
        unsafe {
            intel_guc_submission_disable(guc);
        }
        cleanup = UcInitCleanup::Capture;
    }
    if cleanup == UcInitCleanup::Capture {
        unsafe {
            __uc_capture_load_err_log(uc);
        }
    }
    if matches!(cleanup, UcInitCleanup::Rps | UcInitCleanup::Capture) {
        let rps = unsafe { ptr::addr_of_mut!((*gt).rps) };
        unsafe {
            intel_rps_lower_unslice(rps);
            i915_hwmon_power_max_restore(i915, pl1en);
        }
    }
    unsafe {
        __uc_sanitize(uc);
    }
    if ret == 0 {
        gt_notice!(gt, "GuC is uninitialized\\n");
        return 0;
    }
    uc_gt_probe_error!(gt, "GuC initialization failed %d\\n", ret);
    -EIO
}

// upstream: intel_uc.c __uc_fini_hw()
unsafe extern "C" fn __uc_fini_hw(uc: *mut IntelUc) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    if !unsafe { intel_guc_is_fw_running(guc) } {
        return;
    }
    if unsafe { intel_uc_uses_guc_submission(uc) } {
        unsafe {
            intel_guc_submission_disable(guc);
        }
    }
    unsafe {
        __uc_sanitize(uc);
    }
}

// upstream: intel_uc.c intel_uc_reset_prepare()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_reset_prepare(uc: *mut IntelUc) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    unsafe {
        (*uc).reset_in_progress = true;
    }
    if !unsafe { intel_uc_supports_guc(uc) } {
        return;
    }
    if unsafe { intel_guc_is_ready(guc) } && unsafe { intel_uc_uses_guc_submission(uc) } {
        unsafe {
            intel_guc_submission_reset_prepare(guc);
        }
    }
    unsafe {
        __uc_sanitize(uc);
    }
}

// upstream: intel_uc.c intel_uc_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_reset(uc: *mut IntelUc, stalled: IntelEngineMask) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    if unsafe { intel_uc_uses_guc_submission(uc) } {
        unsafe {
            intel_guc_submission_reset(guc, stalled);
        }
    }
}

// upstream: intel_uc.c intel_uc_reset_finish()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_reset_finish(uc: *mut IntelUc) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    unsafe {
        (*uc).reset_in_progress = false;
    }
    if unsafe { intel_uc_uses_guc_submission(uc) } {
        unsafe {
            intel_guc_submission_reset_finish(guc);
        }
    }
}

// upstream: intel_uc.c intel_uc_cancel_requests()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_cancel_requests(uc: *mut IntelUc) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    if unsafe { intel_uc_uses_guc_submission(uc) } {
        unsafe {
            intel_guc_submission_cancel_requests(guc);
        }
    }
}

// upstream: intel_uc.c intel_uc_runtime_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_runtime_suspend(uc: *mut IntelUc) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    if !unsafe { intel_guc_is_ready(guc) } {
        unsafe {
            (*guc).interrupts.enabled = false;
        }
        return;
    }
    unsafe {
        intel_guc_wait_for_pending_msg(
            guc,
            ptr::addr_of_mut!((*guc).outstanding_submission_g2h),
            false,
            HZ_DIV_5,
        );
    }
    GEM_WARN_ON!(unsafe { atomic_read(&(*guc).outstanding_submission_g2h) } != 0);
    unsafe {
        guc_disable_communication(guc);
    }
}

// upstream: intel_uc.c intel_uc_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_suspend(uc: *mut IntelUc) {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    let gt = unsafe { uc_to_gt(uc) };
    unsafe {
        intel_gsc_uc_flush_work(ptr::addr_of_mut!((*uc).gsc));
        wake_up_all_tlb_invalidate(guc);
    }
    if !unsafe { intel_guc_is_ready(guc) } {
        unsafe {
            (*guc).interrupts.enabled = false;
        }
        return;
    }
    unsafe {
        intel_guc_submission_flush_work(guc);
    }
    let rpm = unsafe { ptr::addr_of_mut!((*(*gt).i915).runtime_pm) };
    let wakeref = unsafe { crate::linux_pm::intel_runtime_pm_get(&*rpm) };
    if !wakeref.is_null() {
        let err = unsafe { intel_guc_suspend(guc) };
        if err != 0 {
            guc_dbg!(guc, "Failed to suspend, %d\\n", err);
        }
        unsafe { crate::linux_pm::intel_runtime_pm_put(&*rpm, wakeref) };
    }
}

// upstream: intel_uc.c __uc_resume_mappings()
unsafe extern "C" fn __uc_resume_mappings(uc: *mut IntelUc) {
    unsafe {
        crate::intel_uc_fw_upstream::intel_uc_fw_resume_mapping(ptr::addr_of_mut!((*uc).guc.fw));
        crate::intel_uc_fw_upstream::intel_uc_fw_resume_mapping(ptr::addr_of_mut!((*uc).huc.fw));
    }
}

// upstream: intel_uc.c __uc_resume()
unsafe fn __uc_resume(uc: *mut IntelUc, enable_communication: bool) -> i32 {
    let guc = unsafe { ptr::addr_of_mut!((*uc).guc) };
    let gt = unsafe { guc_to_gt(guc) };
    if !unsafe { intel_guc_is_fw_running(guc) } {
        return 0;
    }
    GEM_BUG_ON!(enable_communication == unsafe { intel_guc_ct_enabled(ptr::addr_of!((*guc).ct)) });
    if enable_communication {
        unsafe {
            guc_enable_communication(guc);
        }
    }
    if enable_communication && unsafe { intel_uc_uses_guc_slpc(uc) } {
        unsafe {
            intel_guc_pm_intrmsk_enable(gt);
        }
    }
    let err = unsafe { intel_guc_resume(guc) };
    if err != 0 {
        guc_dbg!(guc, "Failed to resume, %d\\n", err);
        return err;
    }
    unsafe {
        intel_gsc_uc_resume(ptr::addr_of_mut!((*uc).gsc));
    }
    if unsafe { intel_guc_tlb_invalidation_is_available(guc) } {
        unsafe {
            intel_guc_invalidate_tlb_engines(guc);
            intel_guc_invalidate_tlb_guc(guc);
        }
    }
    0
}

// upstream: intel_uc.c intel_uc_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_resume(uc: *mut IntelUc) -> i32 {
    unsafe { __uc_resume(uc, false) }
}

// upstream: intel_uc.c intel_uc_runtime_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_uc_runtime_resume(uc: *mut IntelUc) -> i32 {
    unsafe { __uc_resume(uc, true) }
}
