// SPDX-License-Identifier: MIT
// Copyright © 2016-2019 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_huc.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    ptr,
};

use crate::{
    intel_context_types_upstream::{I915SwFence, I915SwFenceNotify},
    intel_context_upstream::Hrtimer,
    intel_engine_types_upstream::{GSC0, I915_MAX_VCS, VCS0},
    intel_gt_api_upstream::{gt_is_root, gt_to_guc, huc_to_gt, intel_gt_perf_limit_reasons_reg},
    intel_gt_types_upstream::IntelGt,
    intel_guc_upstream::{intel_guc_allocate_vma, intel_guc_auth_huc, intel_guc_ggtt_offset},
    intel_huc_fw_upstream::intel_huc_fw_auth_via_gsccs,
    intel_huc_types_upstream::{
        INTEL_HUC_AUTH_BY_GSC, INTEL_HUC_AUTH_BY_GUC, INTEL_HUC_AUTH_MAX_MODES,
        INTEL_HUC_DELAYED_LOAD_ERROR, INTEL_HUC_WAITING_ON_GSC, INTEL_HUC_WAITING_ON_PXP, IntelHuc,
        IntelHucAuthStatus, IntelHucAuthenticationType, intel_huc_is_loaded_by_gsc,
        intel_huc_is_supported, intel_huc_is_wanted,
    },
    intel_uc_fw_types_upstream::{
        __intel_uc_fw_status, INTEL_UC_FIRMWARE_DISABLED, INTEL_UC_FIRMWARE_ERROR,
        INTEL_UC_FIRMWARE_INIT_FAIL, INTEL_UC_FIRMWARE_LOAD_FAIL, INTEL_UC_FIRMWARE_LOADABLE,
        INTEL_UC_FIRMWARE_MISSING, INTEL_UC_FIRMWARE_NOT_SUPPORTED, INTEL_UC_FIRMWARE_RUNNING,
        INTEL_UC_FW_TYPE_HUC, IntelUcFw,
    },
    intel_uc_fw_upstream::{
        intel_uc_fw_change_status, intel_uc_fw_dump, intel_uc_fw_fini, intel_uc_fw_init,
        intel_uc_fw_init_early, intel_uc_fw_is_loadable, intel_uc_fw_is_loaded,
    },
    intel_uncore_types_upstream::{__intel_wait_for_register, intel_uncore_read},
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        config::{EINVAL, EIO, ENODEV, ENOEXEC, ENOMEM, EOPNOTSUPP, ERR_PTR},
        fields::KtimeT,
        gem_memory::NotifierBlock,
        i915::{GRAPHICS_VER, HAS_ENGINE, HAS_GUC_DEPRIVILEGE, INTEL_INFO, IS_DG2},
        pm::{intel_runtime_pm_get, intel_runtime_pm_put},
        primitives::{ktime_get, ktime_to_ms, str_yes_no},
        timer::{
            CLOCK_MONOTONIC, HRTIMER_MODE_REL, HRTIMER_NORESTART, hrtimer_setup,
            hrtimer_start_range_ns,
        },
    },
    linux_print::DrmPrinter,
};

const ENOPKG: i32 = 65;
const EEXIST: i32 = 17;
const ENODEV_LOCAL: i32 = ENODEV;
const GSC_INIT_TIMEOUT_MS: u64 = 10_000;
const PXP_INIT_TIMEOUT_MS: u64 = 5_000;
const HUC_LOAD_RETRY_LIMIT: i32 = 3; // CONFIG_DRM_I915_DEBUG_GEM=n in this target.
const PXP43_HUC_AUTH_INOUT_SIZE: u32 = 4096;
const CONFIG_INTEL_MEI_PXP: bool = false;
const CONFIG_INTEL_MEI_GSC: bool = false;
const GEN11_HUC_KERNEL_LOAD_INFO: I915RegT = I915RegT { reg: 0xc1dc };
const HUC_LOAD_SUCCESSFUL: u32 = 1;
const HUC_STATUS2: I915RegT = I915RegT { reg: 0xd3b0 };
const HUC_FW_VERIFIED: u32 = 1 << 7;
const GUC_SHIM_CONTROL2: I915RegT = I915RegT { reg: 0xc068 };
const GSC_LOADS_HUC: u32 = 1 << 30;
const MTL_GSC_HECI1_BASE: u32 = 0x0011_6000;
const HECI1_FWSTS5_HUC_AUTH_DONE: u32 = 1 << 19;
const HECI_FWSTS5: u32 = 0xc68;

// Linux APIs without a LinuxKPI implementation yet. These are source-level
// kernel service boundaries, not local fallback behavior.
unsafe extern "C" {
    fn bus_register_notifier(
        bus: *const crate::intel_huc_types_upstream::BusType,
        nb: *mut NotifierBlock,
    ) -> c_int;
    fn bus_unregister_notifier(
        bus: *const crate::intel_huc_types_upstream::BusType,
        nb: *mut NotifierBlock,
    );
    fn hrtimer_cancel(timer: *mut Hrtimer) -> c_int;
    fn intel_rps_read_actual_frequency(rps: *mut crate::intel_rps_types_upstream::IntelRps) -> u32;
    fn intel_rps_get_requested_frequency(
        rps: *mut crate::intel_rps_types_upstream::IntelRps,
    ) -> u32;
}

macro_rules! huc_gt_log {
    ($macro:ident, $huc:expr, $fmt:expr $(, $arg:expr)* $(,)?) => {{
        let __gt = unsafe { huc_to_gt($huc) };
        $macro!(__gt, $fmt $(, $arg)*);
    }};
}
macro_rules! huc_err { ($huc:expr, $fmt:expr $(, $arg:expr)* $(,)?) => { huc_gt_log!(gt_err, $huc, $fmt $(, $arg)*) }; }
macro_rules! huc_info { ($huc:expr, $fmt:expr $(, $arg:expr)* $(,)?) => { huc_gt_log!(gt_notice, $huc, $fmt $(, $arg)*) }; }
macro_rules! huc_notice { ($huc:expr, $fmt:expr $(, $arg:expr)* $(,)?) => { huc_gt_log!(gt_notice, $huc, $fmt $(, $arg)*) }; }
macro_rules! huc_dbg { ($huc:expr, $fmt:expr $(, $arg:expr)* $(,)?) => { huc_gt_log!(gt_dbg, $huc, $fmt $(, $arg)*) }; }
macro_rules! huc_warn { ($huc:expr, $fmt:expr $(, $arg:expr)* $(,)?) => {{
    let __gt = unsafe { huc_to_gt($huc) };
    let __id = unsafe { (*__gt).info.id };
    let __args: &[&dyn crate::linux_print::CFormatArg] = &[
        $(&($arg) as &dyn crate::linux_print::CFormatArg),*
    ];
    let __message = crate::linux_print::format_message($fmt, __args);
    crate::linux_print::drm_log_at(
        crate::linux_print::DrmLogLevel::Warn,
        &alloc::format!("GT{__id}"),
        file!(),
        line!(),
        &__message,
    );
}}; }
macro_rules! huc_probe_error { ($huc:expr, $fmt:expr $(, $arg:expr)* $(,)?) => {{
    let __gt = unsafe { huc_to_gt($huc) };
    gt_err!(__gt, concat!("HuC probe: ", $fmt) $(, $arg)*);
}}; }

#[inline]
unsafe fn cancel_timer(timer: *mut Hrtimer) {
    let _ = unsafe { hrtimer_cancel(timer) };
}

#[inline]
unsafe fn start_timer(timer: *mut Hrtimer, timeout_ms: u64) {
    unsafe {
        hrtimer_start_range_ns(
            timer,
            (timeout_ms * 1_000_000) as KtimeT,
            0,
            HRTIMER_MODE_REL,
        )
    };
}

#[inline]
unsafe fn delayed_fence(huc: *mut IntelHuc) -> *mut I915SwFence {
    unsafe { ptr::addr_of_mut!((*huc).delayed_load.fence) }
}

#[inline]
unsafe fn huc_status_index(auth_type: IntelHucAuthenticationType) -> usize {
    assert!((0..INTEL_HUC_AUTH_MAX_MODES).contains(&auth_type));
    auth_type as usize
}

#[inline]
unsafe fn fw_sanitize(fw: *mut IntelUcFw) {
    if unsafe { intel_uc_fw_is_loaded(fw) } {
        unsafe { intel_uc_fw_change_status(fw, INTEL_UC_FIRMWARE_LOADABLE) };
    }
}

// upstream: intel_huc.c sw_fence_dummy_notify()
unsafe extern "C" fn sw_fence_dummy_notify(
    _sf: *mut I915SwFence,
    _state: I915SwFenceNotify,
) -> i32 {
    0
}

// upstream: intel_huc.c __delayed_huc_load_complete()
unsafe fn __delayed_huc_load_complete(huc: *mut IntelHuc) {
    let fence = unsafe { delayed_fence(huc) };
    if !unsafe { crate::linux::sw_fence::i915_sw_fence_done(&*fence) } {
        unsafe { crate::i915_sw_fence_upstream::i915_sw_fence_complete(fence) };
    }
}

// upstream: intel_huc.c delayed_huc_load_complete()
unsafe fn delayed_huc_load_complete(huc: *mut IntelHuc) {
    unsafe { cancel_timer(ptr::addr_of_mut!((*huc).delayed_load.timer)) };
    unsafe { __delayed_huc_load_complete(huc) };
}

// upstream: intel_huc.c __gsc_init_error()
unsafe fn __gsc_init_error(huc: *mut IntelHuc) {
    unsafe { (*huc).delayed_load.status = INTEL_HUC_DELAYED_LOAD_ERROR };
    unsafe { __delayed_huc_load_complete(huc) };
}

// upstream: intel_huc.c gsc_init_error()
unsafe fn gsc_init_error(huc: *mut IntelHuc) {
    unsafe { cancel_timer(ptr::addr_of_mut!((*huc).delayed_load.timer)) };
    unsafe { __gsc_init_error(huc) };
}

// upstream: intel_huc.c gsc_init_done()
unsafe fn gsc_init_done(huc: *mut IntelHuc) {
    unsafe { cancel_timer(ptr::addr_of_mut!((*huc).delayed_load.timer)) };
    unsafe { (*huc).delayed_load.status = INTEL_HUC_WAITING_ON_PXP };
    let fence = unsafe { delayed_fence(huc) };
    if !unsafe { crate::linux::sw_fence::i915_sw_fence_done(&*fence) } {
        unsafe {
            start_timer(
                ptr::addr_of_mut!((*huc).delayed_load.timer),
                PXP_INIT_TIMEOUT_MS,
            )
        };
    }
}

// upstream: intel_huc.c huc_delayed_load_timer_callback()
unsafe extern "C" fn huc_delayed_load_timer_callback(timer: *mut Hrtimer) -> i32 {
    let huc = unsafe {
        timer
            .cast::<u8>()
            .sub(
                core::mem::offset_of!(IntelHuc, delayed_load)
                    + core::mem::offset_of!(
                        crate::intel_huc_types_upstream::IntelHucDelayedLoad,
                        timer
                    ),
            )
            .cast::<IntelHuc>()
    };
    if !unsafe { intel_huc_is_authenticated(huc, INTEL_HUC_AUTH_BY_GSC) } {
        match unsafe { (*huc).delayed_load.status } {
            INTEL_HUC_WAITING_ON_GSC => huc_notice!(huc, "timed out waiting for MEI GSC\n"),
            INTEL_HUC_WAITING_ON_PXP => huc_notice!(huc, "timed out waiting for MEI PXP\n"),
            status => MISSING_CASE!(status),
        }
        unsafe { __gsc_init_error(huc) };
    }
    HRTIMER_NORESTART
}

// upstream: intel_huc.c huc_delayed_load_start()
unsafe fn huc_delayed_load_start(huc: *mut IntelHuc) {
    GEM_BUG_ON!(unsafe { intel_huc_is_authenticated(huc, INTEL_HUC_AUTH_BY_GSC) });
    let delay = match unsafe { (*huc).delayed_load.status } {
        INTEL_HUC_WAITING_ON_GSC => GSC_INIT_TIMEOUT_MS,
        INTEL_HUC_WAITING_ON_PXP => PXP_INIT_TIMEOUT_MS,
        _ => {
            unsafe { gsc_init_error(huc) };
            return;
        }
    };
    let fence = unsafe { delayed_fence(huc) };
    GEM_BUG_ON!(!unsafe { crate::linux::sw_fence::i915_sw_fence_done(&*fence) });
    unsafe {
        crate::i915_sw_fence_upstream::i915_sw_fence_fini(fence);
        crate::i915_sw_fence_upstream::i915_sw_fence_reinit(fence);
        crate::i915_sw_fence_upstream::i915_sw_fence_await(fence);
        crate::i915_sw_fence_upstream::i915_sw_fence_commit(fence);
        start_timer(ptr::addr_of_mut!((*huc).delayed_load.timer), delay);
    }
}

// upstream: intel_huc.c gsc_notifier()
unsafe extern "C" fn gsc_notifier(
    nb: *mut NotifierBlock,
    action: c_ulong,
    data: *mut c_void,
) -> c_int {
    let dev = data;
    let huc = unsafe {
        nb.cast::<u8>()
            .sub(
                core::mem::offset_of!(IntelHuc, delayed_load)
                    + core::mem::offset_of!(
                        crate::intel_huc_types_upstream::IntelHucDelayedLoad,
                        nb
                    ),
            )
            .cast::<IntelHuc>()
    };
    let gt = unsafe { huc_to_gt(huc) };
    let intf = unsafe { ptr::addr_of_mut!((*gt).gsc.intf[0]) };
    let aux_dev = unsafe { (*intf).adev };
    // `mei_aux_device::aux_dev` is the first member, and
    // `auxiliary_device::dev` is likewise the first member.
    if aux_dev.is_null() || aux_dev.cast::<c_void>() != dev {
        return 0;
    }
    match action as i32 {
        BUS_NOTIFY_BOUND_DRIVER => unsafe { gsc_init_done(huc) },
        BUS_NOTIFY_DRIVER_NOT_BOUND | BUS_NOTIFY_UNBIND_DRIVER => {
            huc_info!(huc, "MEI driver not bound, disabling load\n");
            unsafe { gsc_init_error(huc) };
        }
        _ => {}
    }
    0
}

const BUS_NOTIFY_BOUND_DRIVER: i32 = 4;
const BUS_NOTIFY_DRIVER_NOT_BOUND: i32 = 2;
const BUS_NOTIFY_UNBIND_DRIVER: i32 = 5;

// upstream: intel_huc.c intel_huc_register_gsc_notifier()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_register_gsc_notifier(
    huc: *mut IntelHuc,
    bus: *const crate::intel_huc_types_upstream::BusType,
) {
    if !unsafe { intel_huc_is_loaded_by_gsc(huc) } {
        return;
    }
    unsafe { (*huc).delayed_load.nb.notifier_call = Some(gsc_notifier) };
    let ret = unsafe { bus_register_notifier(bus, ptr::addr_of_mut!((*huc).delayed_load.nb)) };
    if ret != 0 {
        huc_err!(
            huc,
            "failed to register GSC notifier %pe\n",
            ERR_PTR::<c_void>(ret)
        );
        unsafe { (*huc).delayed_load.nb.notifier_call = None };
        unsafe { gsc_init_error(huc) };
    }
}

// upstream: intel_huc.c intel_huc_unregister_gsc_notifier()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_unregister_gsc_notifier(
    huc: *mut IntelHuc,
    bus: *const crate::intel_huc_types_upstream::BusType,
) {
    if unsafe { (*huc).delayed_load.nb.notifier_call.is_none() } {
        return;
    }
    unsafe { delayed_huc_load_complete(huc) };
    unsafe { bus_unregister_notifier(bus, ptr::addr_of_mut!((*huc).delayed_load.nb)) };
    unsafe { (*huc).delayed_load.nb.notifier_call = None };
}

// upstream: intel_huc.c delayed_huc_load_init()
unsafe fn delayed_huc_load_init(huc: *mut IntelHuc) {
    unsafe {
        crate::i915_sw_fence_upstream::i915_sw_fence_init(
            delayed_fence(huc),
            Some(sw_fence_dummy_notify),
        );
        crate::i915_sw_fence_upstream::i915_sw_fence_commit(delayed_fence(huc));
        hrtimer_setup(
            ptr::addr_of_mut!((*huc).delayed_load.timer),
            Some(huc_delayed_load_timer_callback),
            CLOCK_MONOTONIC,
            HRTIMER_MODE_REL,
        );
    }
}

// upstream: intel_huc.c delayed_huc_load_fini()
unsafe fn delayed_huc_load_fini(huc: *mut IntelHuc) {
    unsafe { delayed_huc_load_complete(huc) };
    unsafe { crate::i915_sw_fence_upstream::i915_sw_fence_fini(delayed_fence(huc)) };
}

// upstream: intel_huc.c intel_huc_sanitize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_sanitize(huc: *mut IntelHuc) -> i32 {
    unsafe { delayed_huc_load_complete(huc) };
    unsafe { fw_sanitize(ptr::addr_of_mut!((*huc).fw)) };
    0
}

// upstream: intel_huc.c vcs_supported()
unsafe fn vcs_supported(gt: *mut IntelGt) -> bool {
    GEM_BUG_ON!(!unsafe { gt_is_root(gt) } && unsafe { (*gt).info.engine_mask } == 0);
    let mask = if unsafe { gt_is_root(gt) } {
        unsafe { (*INTEL_INFO((*gt).i915)).platform_engine_mask }
    } else {
        unsafe { (*gt).info.engine_mask }
    };
    let bits = (1u32 << I915_MAX_VCS) - 1;
    ((mask >> VCS0) & bits) != 0
}

// upstream: intel_huc.c intel_huc_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_init_early(huc: *mut IntelHuc) {
    let gt = unsafe { huc_to_gt(huc) };
    let i915 = unsafe { (*gt).i915 };
    unsafe { intel_uc_fw_init_early(ptr::addr_of_mut!((*huc).fw), INTEL_UC_FW_TYPE_HUC, true) };
    unsafe { delayed_huc_load_init(huc) };
    if !unsafe { vcs_supported(gt) } {
        unsafe {
            intel_uc_fw_change_status(
                ptr::addr_of_mut!((*huc).fw),
                INTEL_UC_FIRMWARE_NOT_SUPPORTED,
            )
        };
        return;
    }
    let guc_status = if unsafe { GRAPHICS_VER(i915) } >= 11 {
        IntelHucAuthStatus {
            reg: GEN11_HUC_KERNEL_LOAD_INFO,
            mask: HUC_LOAD_SUCCESSFUL,
            value: HUC_LOAD_SUCCESSFUL,
        }
    } else {
        IntelHucAuthStatus {
            reg: HUC_STATUS2,
            mask: HUC_FW_VERIFIED,
            value: HUC_FW_VERIFIED,
        }
    };
    unsafe { (*huc).status[INTEL_HUC_AUTH_BY_GUC as usize] = guc_status };
    let gsc_status = if unsafe { IS_DG2(i915) } {
        IntelHucAuthStatus {
            reg: GEN11_HUC_KERNEL_LOAD_INFO,
            mask: HUC_LOAD_SUCCESSFUL,
            value: HUC_LOAD_SUCCESSFUL,
        }
    } else {
        IntelHucAuthStatus {
            reg: I915RegT {
                reg: MTL_GSC_HECI1_BASE + HECI_FWSTS5,
            },
            mask: HECI1_FWSTS5_HUC_AUTH_DONE,
            value: HECI1_FWSTS5_HUC_AUTH_DONE,
        }
    };
    unsafe { (*huc).status[INTEL_HUC_AUTH_BY_GSC as usize] = gsc_status };
}

// upstream: intel_huc.c intel_huc_fini_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_fini_late(huc: *mut IntelHuc) {
    unsafe { delayed_huc_load_fini(huc) };
}

// upstream: intel_huc.c check_huc_loading_mode()
unsafe fn check_huc_loading_mode(huc: *mut IntelHuc) -> i32 {
    let gt = unsafe { huc_to_gt(huc) };
    let i915 = unsafe { (*gt).i915 };
    let gsc_enabled = unsafe { (*huc).fw.has_gsc_headers };
    if unsafe { HAS_GUC_DEPRIVILEGE(i915) } {
        unsafe {
            (*huc).loaded_via_gsc =
                intel_uncore_read((*gt).uncore, GUC_SHIM_CONTROL2) & GSC_LOADS_HUC != 0
        };
    }
    if unsafe { (*huc).loaded_via_gsc } && !gsc_enabled {
        huc_err!(
            huc,
            "HW requires a GSC-enabled blob, but we found a legacy one\n"
        );
        return -ENOEXEC;
    }
    if !unsafe { (*huc).loaded_via_gsc }
        && gsc_enabled
        && unsafe { (*huc).fw.dma_start_offset } == 0
    {
        huc_err!(
            huc,
            "HW in DMA mode, but we have an incompatible GSC-enabled blob\n"
        );
        return -ENOEXEC;
    }
    if unsafe { (*huc).loaded_via_gsc } {
        if unsafe { IS_DG2(i915) } {
            if !CONFIG_INTEL_MEI_PXP || !CONFIG_INTEL_MEI_GSC {
                huc_info!(huc, "can't load due to missing mei modules\n");
                return -EIO;
            }
        } else if !unsafe { HAS_ENGINE(gt, GSC0) } {
            huc_info!(huc, "can't load due to missing GSCCS\n");
            return -EIO;
        }
    }
    huc_dbg!(
        huc,
        "loaded by GSC = %s\n",
        str_yes_no(unsafe { (*huc).loaded_via_gsc })
    );
    0
}

// upstream: intel_huc.c intel_huc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_init(huc: *mut IntelHuc) -> i32 {
    let gt = unsafe { huc_to_gt(huc) };
    let mut err = unsafe { check_huc_loading_mode(huc) };
    if err != 0 {
        return unsafe { huc_init_fail(huc, err) };
    }
    if unsafe { HAS_ENGINE(gt, GSC0) } {
        let vma = unsafe { intel_guc_allocate_vma(gt_to_guc(gt), PXP43_HUC_AUTH_INOUT_SIZE * 2) };
        if crate::linux_config::IS_ERR(vma) {
            err = crate::linux_config::PTR_ERR(vma);
            huc_info!(huc, "Failed to allocate heci pkt\n");
            return unsafe { huc_init_fail(huc, err) };
        }
        unsafe { (*huc).heci_pkt = vma };
    }
    err = unsafe { intel_uc_fw_init(ptr::addr_of_mut!((*huc).fw)) };
    if err != 0 {
        if !unsafe { (*huc).heci_pkt.is_null() } {
            unsafe {
                crate::i915_vma_api_upstream::i915_vma_unpin_and_release(
                    ptr::addr_of_mut!((*huc).heci_pkt),
                    0,
                )
            };
        }
        return unsafe { huc_init_fail(huc, err) };
    }
    unsafe { intel_uc_fw_change_status(ptr::addr_of_mut!((*huc).fw), INTEL_UC_FIRMWARE_LOADABLE) };
    0
}

unsafe fn huc_init_fail(huc: *mut IntelHuc, err: i32) -> i32 {
    unsafe {
        intel_uc_fw_change_status(
            ptr::addr_of_mut!((*huc).fw),
            crate::intel_uc_fw_types_upstream::INTEL_UC_FIRMWARE_INIT_FAIL,
        )
    };
    huc_info!(huc, "initialization failed %pe\n", ERR_PTR::<c_void>(err));
    err
}

// upstream: intel_huc.c intel_huc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_fini(huc: *mut IntelHuc) {
    if !unsafe { (*huc).heci_pkt.is_null() } {
        unsafe {
            crate::i915_vma_api_upstream::i915_vma_unpin_and_release(
                ptr::addr_of_mut!((*huc).heci_pkt),
                0,
            )
        };
    }
    if unsafe { intel_uc_fw_is_loadable(ptr::addr_of!((*huc).fw)) } {
        unsafe { intel_uc_fw_fini(ptr::addr_of_mut!((*huc).fw)) };
    }
}

// upstream: intel_huc.c auth_mode_string()
unsafe fn auth_mode_string(
    huc: *mut IntelHuc,
    auth_type: IntelHucAuthenticationType,
) -> &'static str {
    if unsafe { (*huc).fw.has_gsc_headers } && auth_type == INTEL_HUC_AUTH_BY_GUC {
        "clear media"
    } else {
        "all workloads"
    }
}

// upstream: intel_huc.c intel_huc_wait_for_auth_complete()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_wait_for_auth_complete(
    huc: *mut IntelHuc,
    auth_type: IntelHucAuthenticationType,
) -> i32 {
    let gt = unsafe { huc_to_gt(huc) };
    let uncore = unsafe { (*gt).uncore };
    let index = unsafe { huc_status_index(auth_type) };
    let status = unsafe { (*huc).status[index] };
    let rps = unsafe { ptr::addr_of_mut!((*gt).rps) };
    let before_freq = unsafe { intel_rps_read_actual_frequency(rps) };
    let before = ktime_get();
    let mut ret = 0;
    let mut count = 0;
    while count < HUC_LOAD_RETRY_LIMIT {
        ret = unsafe {
            __intel_wait_for_register(
                uncore,
                status.reg,
                status.mask,
                status.value,
                2,
                1000,
                ptr::null_mut(),
            )
        };
        if ret == 0 {
            break;
        }
        huc_dbg!(
            huc,
            "auth still in progress, count = %d, freq = %dMHz, status = 0x%08X\n",
            count,
            unsafe { intel_rps_read_actual_frequency(rps) },
            status.reg.reg
        );
        count += 1;
    }
    let delta_ms = ktime_to_ms(ktime_get() - before);
    if delta_ms > 50 {
        huc_warn!(
            huc,
            "excessive auth time: %lldms! [status = 0x%08X, count = %d, ret = %d]\n",
            delta_ms,
            status.reg.reg,
            count,
            ret
        );
        huc_warn!(
            huc,
            "excessive auth time: [freq = %dMHz -> %dMHz vs %dMHz, perf_limit_reasons = 0x%08X]\n",
            before_freq,
            unsafe { intel_rps_read_actual_frequency(rps) },
            unsafe { intel_rps_get_requested_frequency(rps) },
            unsafe { intel_uncore_read(uncore, intel_gt_perf_limit_reasons_reg(gt)) }
        );
    } else {
        huc_dbg!(
            huc,
            "auth took %lldms, freq = %dMHz -> %dMHz vs %dMHz, status = 0x%08X, count = %d, ret = \
             %d\n",
            delta_ms,
            before_freq,
            unsafe { intel_rps_read_actual_frequency(rps) },
            unsafe { intel_rps_get_requested_frequency(rps) },
            status.reg.reg,
            count,
            ret
        );
    }
    unsafe { delayed_huc_load_complete(huc) };
    if ret != 0 {
        huc_err!(
            huc,
            "firmware not verified for %s: %pe\n",
            unsafe { auth_mode_string(huc, auth_type) },
            ERR_PTR::<c_void>(ret)
        );
        unsafe {
            intel_uc_fw_change_status(ptr::addr_of_mut!((*huc).fw), INTEL_UC_FIRMWARE_LOAD_FAIL)
        };
        return ret;
    }
    unsafe { intel_uc_fw_change_status(ptr::addr_of_mut!((*huc).fw), INTEL_UC_FIRMWARE_RUNNING) };
    huc_info!(huc, "authenticated for %s\n", unsafe {
        auth_mode_string(huc, auth_type)
    });
    0
}

// upstream: intel_huc.c intel_huc_auth()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_auth(
    huc: *mut IntelHuc,
    auth_type: IntelHucAuthenticationType,
) -> i32 {
    let gt = unsafe { huc_to_gt(huc) };
    let guc = unsafe { gt_to_guc(gt) };
    if !unsafe { intel_uc_fw_is_loaded(ptr::addr_of!((*huc).fw)) } {
        return -ENOEXEC;
    }
    if unsafe { intel_huc_is_loaded_by_gsc(huc) } {
        return -ENODEV_LOCAL;
    }
    if unsafe { intel_huc_is_authenticated(huc, auth_type) } {
        return -EEXIST;
    }
    let ret = match auth_type {
        INTEL_HUC_AUTH_BY_GUC => unsafe {
            intel_guc_auth_huc(guc, intel_guc_ggtt_offset(guc, (*huc).fw.rsa_data))
        },
        INTEL_HUC_AUTH_BY_GSC => unsafe { intel_huc_fw_auth_via_gsccs(huc) },
        _ => {
            MISSING_CASE!(auth_type);
            -EINVAL
        }
    };
    if ret == 0 {
        let wait_ret = unsafe { intel_huc_wait_for_auth_complete(huc, auth_type) };
        if wait_ret == 0 {
            return 0;
        }
        huc_probe_error!(
            huc,
            "%s authentication failed %pe\n",
            unsafe { auth_mode_string(huc, auth_type) },
            ERR_PTR::<c_void>(wait_ret)
        );
        return wait_ret;
    }
    huc_probe_error!(
        huc,
        "%s authentication failed %pe\n",
        unsafe { auth_mode_string(huc, auth_type) },
        ERR_PTR::<c_void>(ret)
    );
    ret
}

// upstream: intel_huc.c intel_huc_is_authenticated()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_is_authenticated(
    huc: *mut IntelHuc,
    auth_type: IntelHucAuthenticationType,
) -> bool {
    let gt = unsafe { huc_to_gt(huc) };
    let index = unsafe { huc_status_index(auth_type) };
    let status = unsafe { (*huc).status[index] };
    let rpm = unsafe { (*(*gt).uncore).rpm };
    let wakeref = intel_runtime_pm_get(rpm);
    let value = unsafe { intel_uncore_read((*gt).uncore, status.reg) };
    intel_runtime_pm_put(rpm, wakeref);
    value & status.mask == status.value
}

// upstream: intel_huc.c huc_is_fully_authenticated()
unsafe fn huc_is_fully_authenticated(huc: *mut IntelHuc) -> bool {
    let fw = unsafe { ptr::addr_of!((*huc).fw) };
    if !unsafe { (*fw).has_gsc_headers } {
        unsafe { intel_huc_is_authenticated(huc, INTEL_HUC_AUTH_BY_GUC) }
    } else if unsafe { intel_huc_is_loaded_by_gsc(huc) || HAS_ENGINE(huc_to_gt(huc), GSC0) } {
        unsafe { intel_huc_is_authenticated(huc, INTEL_HUC_AUTH_BY_GSC) }
    } else {
        false
    }
}

// upstream: intel_huc.c intel_huc_check_status()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_check_status(huc: *mut IntelHuc) -> i32 {
    let fw = unsafe { ptr::addr_of!((*huc).fw) };
    match unsafe { __intel_uc_fw_status(fw) } {
        INTEL_UC_FIRMWARE_NOT_SUPPORTED => return -ENODEV,
        INTEL_UC_FIRMWARE_DISABLED => return -EOPNOTSUPP,
        INTEL_UC_FIRMWARE_MISSING => return -ENOPKG,
        INTEL_UC_FIRMWARE_ERROR => return -ENOEXEC,
        INTEL_UC_FIRMWARE_INIT_FAIL => return -ENOMEM,
        INTEL_UC_FIRMWARE_LOAD_FAIL => return -EIO,
        _ => {}
    }
    if unsafe { huc_is_fully_authenticated(huc) } {
        1
    } else if unsafe {
        (*fw).has_gsc_headers
            && !intel_huc_is_loaded_by_gsc(huc)
            && intel_huc_is_authenticated(huc, INTEL_HUC_AUTH_BY_GUC)
    } {
        2
    } else {
        0
    }
}

// upstream: intel_huc.c huc_has_delayed_load()
unsafe fn huc_has_delayed_load(huc: *mut IntelHuc) -> bool {
    unsafe {
        intel_huc_is_loaded_by_gsc(huc)
            && (*huc).delayed_load.status != INTEL_HUC_DELAYED_LOAD_ERROR
    }
}

// upstream: intel_huc.c intel_huc_update_auth_status()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_update_auth_status(huc: *mut IntelHuc) {
    let fw = unsafe { ptr::addr_of_mut!((*huc).fw) };
    if !unsafe { intel_uc_fw_is_loadable(fw) } || !unsafe { (*fw).has_gsc_headers } {
        return;
    }
    if unsafe { huc_is_fully_authenticated(huc) } {
        unsafe { intel_uc_fw_change_status(fw, INTEL_UC_FIRMWARE_RUNNING) };
    } else if unsafe { huc_has_delayed_load(huc) } {
        unsafe { huc_delayed_load_start(huc) };
    }
}

// upstream: intel_huc.c intel_huc_load_status()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_load_status(huc: *mut IntelHuc, printer: *mut DrmPrinter) {
    let gt = unsafe { huc_to_gt(huc) };
    if !unsafe { intel_huc_is_supported(huc) } {
        drm_printf!(printer, "HuC not supported\n");
        return;
    }
    if !unsafe { intel_huc_is_wanted(huc) } {
        drm_printf!(printer, "HuC disabled\n");
        return;
    }
    unsafe { intel_uc_fw_dump(ptr::addr_of!((*huc).fw), printer) };
    let rpm = unsafe { (*(*gt).uncore).rpm };
    let wakeref = intel_runtime_pm_get(rpm);
    let reg = unsafe { (*huc).status[INTEL_HUC_AUTH_BY_GUC as usize].reg };
    let status = unsafe { intel_uncore_read((*gt).uncore, reg) };
    drm_printf!(printer, "HuC status: 0x%08x\n", status);
    intel_runtime_pm_put(rpm, wakeref);
}
