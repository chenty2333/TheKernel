// SPDX-License-Identifier: MIT
// Copyright © 2019-2024 Intel Corporation.
// Fail-closed definitions for upstream code paths that only serve hardware
// TheKernel does not support (LMEM/TTM, migrate, GSC/PXP, MTL sa_media, gen6/7
// PPGTT, GMCH GGTT, ring submission, OA perf, debugfs). Each function names its
// Linux 7.2.3 source and the reason for its result. Callers that check error
// codes receive the Linux "unsupported" value (-ENODEV, false, ERR_PTR(-ENODEV),
// NULL); callers with no error channel panic, because on a supported platform
// the path is unreachable.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_int, c_void},
    mem::offset_of,
};

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_ww_upstream::I915GemWwCtx,
    i915_request_types_upstream::{I915Deps, I915Request},
    intel_context_types_upstream::IntelContext,
    intel_context_upstream::SgEntry as Scatterlist,
    intel_engine_cs_upstream::IntelEngineCs,
    intel_gsc_types_upstream::IntelGsc,
    intel_gsc_uc_types_upstream::IntelGscUc,
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::I915Ggtt,
    intel_migrate_types_upstream::IntelMigrate,
    intel_uc_fw_types_upstream::{INTEL_UC_FIRMWARE_NOT_SUPPORTED, IntelUcFw},
    linux::i915::{GRAPHICS_VER_FULL, HAS_ENGINE, IP_VER},
    linux::gem_memory::IntelMemoryRegion,
    linux_config::ENODEV,
};

// ERR_PTR(-ENODEV)
fn err_ptr_enodev<T>() -> *mut T {
    (-(ENODEV as isize)) as usize as *mut T
}

// GSC (HECI) hardware first appears with MTL (graphics IP 12.70). TheKernel's
// admitted platform is ADL-N, which has no GSC; the GSC interfaces are not
// translated and any reachable GSC path fails closed.
unsafe fn gsc_hw_present(i915: *mut crate::linux_i915_private::DrmI915Private) -> bool {
    unsafe { GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70) }
}

// upstream: intel_gsc.c container_of(gsc, struct intel_gt, gsc)
unsafe fn gsc_to_gt(gsc: *mut IntelGsc) -> *mut IntelGt {
    unsafe { gsc.cast::<u8>().sub(offset_of!(IntelGt, gsc)).cast::<IntelGt>() }
}

// upstream: intel_gsc_uc.c container_of(gsc, struct intel_gt, uc.gsc)
unsafe fn gsc_uc_to_gt(gsc: *mut IntelGscUc) -> *mut IntelGt {
    let uc = unsafe { gsc.cast::<u8>().sub(offset_of!(crate::intel_uc_types_upstream::IntelUc, gsc)) };
    unsafe { uc.cast::<u8>().sub(offset_of!(IntelGt, uc)).cast::<IntelGt>() }
}

// ---------------------------------------------------------------------------
// gt/intel_gt_debugfs.c
// ---------------------------------------------------------------------------

// upstream: intel_gt_debugfs.c intel_gt_debugfs_register()
//
// Only creates debugfs nodes. TheKernel has no debugfs, and no driver state
// depends on these nodes, so the registration is intentionally empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_debugfs_register(_gt: *mut IntelGt) {}

// ---------------------------------------------------------------------------
// gt/gen6_ppgtt.c
// ---------------------------------------------------------------------------

// upstream: gen6_ppgtt.c gen6_ppgtt_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_ppgtt_create(_gt: *mut IntelGt) -> *mut crate::intel_gtt_api_upstream::I915Ppgtt {
    err_ptr_enodev()
}

// upstream: gen6_ppgtt.c gen6_ppgtt_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_ppgtt_enable(_gt: *mut IntelGt) {
    panic!("gen6_ppgtt_enable: gen6 PPGTT hardware is not supported by TheKernel");
}

// upstream: gen6_ppgtt.c gen7_ppgtt_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen7_ppgtt_enable(_gt: *mut IntelGt) {
    panic!("gen7_ppgtt_enable: gen7 PPGTT hardware is not supported by TheKernel");
}

// ---------------------------------------------------------------------------
// gt/intel_ggtt_gmch.c
// ---------------------------------------------------------------------------

// upstream: intel_ggtt_gmch.c intel_ggtt_gmch_probe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ggtt_gmch_probe(_ggtt: *mut I915Ggtt) -> c_int {
    -ENODEV
}

// upstream: intel_ggtt_gmch.c intel_ggtt_gmch_enable_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ggtt_gmch_enable_hw(_i915: *mut crate::linux_i915_private::DrmI915Private) -> c_int {
    -ENODEV
}

// upstream: intel_ggtt_gmch.c intel_ggtt_gmch_flush()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ggtt_gmch_flush() {
    panic!("intel_ggtt_gmch_flush: GMCH GGTT hardware is not supported by TheKernel");
}

// ---------------------------------------------------------------------------
// gt/intel_gsc.c
// ---------------------------------------------------------------------------

// upstream: intel_gsc.c intel_gsc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_init(_gsc: *mut IntelGsc, i915: *mut crate::linux_i915_private::DrmI915Private) {
    if !unsafe { gsc_hw_present(i915) } {
        return;
    }
    panic!("intel_gsc_init: GSC (HECI) hardware is not supported by TheKernel");
}

// upstream: intel_gsc.c intel_gsc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_fini(gsc: *mut IntelGsc) {
    let i915 = unsafe { (*gsc_to_gt(gsc)).i915 };
    if !unsafe { gsc_hw_present(i915) } {
        return;
    }
    panic!("intel_gsc_fini: GSC (HECI) hardware is not supported by TheKernel");
}

// upstream: intel_gsc.c intel_gsc_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_irq_handler(_gt: *mut IntelGt, _iir: u32) {
    panic!("intel_gsc_irq_handler: GSC interrupts are not supported by TheKernel");
}

// ---------------------------------------------------------------------------
// gt/intel_migrate.c
// ---------------------------------------------------------------------------

// upstream: intel_migrate.c intel_migrate_init()
//
// Migration contexts exist only for LMEM copy/clear. Without LMEM the caller
// (intel_gt_probe) ignores the result and `context` stays NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_migrate_init(_migrate: *mut IntelMigrate, _gt: *mut IntelGt) -> i32 {
    -ENODEV
}

// upstream: intel_migrate.c intel_migrate_create_context()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_migrate_create_context(_migrate: *mut IntelMigrate) -> *mut IntelContext {
    err_ptr_enodev()
}

// upstream: intel_migrate.c intel_migrate_fini()
//
// With no migration context created there is nothing to release; anything else
// would be an unsupported LMEM teardown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_migrate_fini(migrate: *mut IntelMigrate) {
    if !unsafe { (*migrate).context.is_null() } {
        panic!("intel_migrate_fini: LMEM migration (unsupported by TheKernel) was initialized");
    }
}

// upstream: intel_migrate.c intel_migrate_copy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_migrate_copy(
    _migrate: *mut IntelMigrate,
    _ww: *mut I915GemWwCtx,
    _deps: *const I915Deps,
    _src: *mut Scatterlist,
    _src_pat_index: u32,
    _src_is_lmem: bool,
    _dst: *mut Scatterlist,
    _dst_pat_index: u32,
    _dst_is_lmem: bool,
    _out: *mut *mut I915Request,
) -> i32 {
    -ENODEV
}

// upstream: intel_migrate.c intel_context_migrate_copy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_context_migrate_copy(
    _ce: *mut IntelContext,
    _deps: *const I915Deps,
    _src: *mut Scatterlist,
    _src_pat_index: u32,
    _src_is_lmem: bool,
    _dst: *mut Scatterlist,
    _dst_pat_index: u32,
    _dst_is_lmem: bool,
    _out: *mut *mut I915Request,
) -> i32 {
    -ENODEV
}

// upstream: intel_migrate.c intel_migrate_clear()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_migrate_clear(
    _migrate: *mut IntelMigrate,
    _ww: *mut I915GemWwCtx,
    _deps: *const I915Deps,
    _sg: *mut Scatterlist,
    _pat_index: u32,
    _is_lmem: bool,
    _value: u32,
    _out: *mut *mut I915Request,
) -> i32 {
    -ENODEV
}

// upstream: intel_migrate.c intel_context_migrate_clear()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_context_migrate_clear(
    _ce: *mut IntelContext,
    _deps: *const I915Deps,
    _sg: *mut Scatterlist,
    _pat_index: u32,
    _is_lmem: bool,
    _value: u32,
    _out: *mut *mut I915Request,
) -> i32 {
    -ENODEV
}

// ---------------------------------------------------------------------------
// gt/intel_region_lmem.c, gem/i915_gem_ttm_pm.c
// ---------------------------------------------------------------------------

// upstream: intel_region_lmem.c intel_gt_setup_lmem()
//
// intel_gt.c treats -ENODEV from this call as "no LMEM region" and continues.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_setup_lmem(_gt: *mut IntelGt) -> *mut IntelMemoryRegion {
    err_ptr_enodev()
}

// upstream: gem/i915_gem_ttm_pm.c i915_ttm_backup_region()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ttm_backup_region(_region: *mut IntelMemoryRegion, _flags: u32) -> c_int {
    -ENODEV
}

// upstream: gem/i915_gem_ttm_pm.c i915_ttm_restore_region()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ttm_restore_region(_region: *mut IntelMemoryRegion, _flags: u32) -> c_int {
    -ENODEV
}

// upstream: gem/i915_gem_ttm_pm.c i915_ttm_recover_region()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ttm_recover_region(_region: *mut IntelMemoryRegion) {
    panic!("i915_ttm_recover_region: TTM-backed LMEM is not supported by TheKernel");
}

// ---------------------------------------------------------------------------
// gt/intel_ring_submission.c, gt/intel_sa_media.c
// ---------------------------------------------------------------------------

// upstream: intel_ring_submission.c intel_ring_submission_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ring_submission_setup(_engine: *mut IntelEngineCs) -> i32 {
    -ENODEV
}

// upstream: intel_sa_media.c intel_sa_mediagt_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_sa_mediagt_setup(_gt: *mut IntelGt, _phys: u64, _gsi_offset: u32) -> i32 {
    -ENODEV
}

// ---------------------------------------------------------------------------
// gt/uc/intel_gsc_fw.c, intel_gsc_proxy.c, intel_gsc_uc.c, intel_gsc_uc_heci_cmd_submit.c
// ---------------------------------------------------------------------------

// upstream: gt/uc/intel_gsc_fw.c intel_gsc_fw_get_binary_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_fw_get_binary_info(_uc_fw: *mut IntelUcFw, _data: *const u8, _size: usize) -> i32 {
    -ENODEV
}

// upstream: gt/uc/intel_gsc_proxy.c intel_gsc_proxy_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_proxy_irq_handler(_gsc: *mut c_void, _iir: u32) {
    panic!("intel_gsc_proxy_irq_handler: GSC proxy is not supported by TheKernel");
}

// `intel_gsc_uc_is_loadable`-style guard used by every GSC uC entry point:
// a GSC firmware that is not loadable (NOT_SUPPORTED on ADL-N) has nothing
// to start, flush, suspend, resume or tear down.
unsafe fn gsc_uc_not_loadable(gsc: *mut IntelGscUc) -> bool {
    !unsafe { crate::intel_uc_fw_upstream::intel_uc_fw_is_loadable(core::ptr::addr_of!((*gsc).fw)) }
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_init_early(gsc: *mut IntelGscUc) {
    let gt = unsafe { gsc_uc_to_gt(gsc) };
    unsafe {
        crate::intel_uc_fw_upstream::intel_uc_fw_init_early(
            core::ptr::addr_of_mut!((*gsc).fw),
            crate::intel_uc_fw_types_upstream::INTEL_UC_FW_TYPE_GSC,
            false,
        );
    }
    // gsc_engine_supported(): the GSC0 engine must exist on this device.
    if !unsafe { HAS_ENGINE(gt, crate::intel_engine_types_upstream::GSC0) } {
        unsafe {
            crate::intel_uc_fw_upstream::intel_uc_fw_change_status(
                core::ptr::addr_of_mut!((*gsc).fw),
                INTEL_UC_FIRMWARE_NOT_SUPPORTED,
            );
        }
        return;
    }
    panic!("intel_gsc_uc_init_early: GSC engine present but GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_init(gsc: *mut IntelGscUc) -> i32 {
    if unsafe { gsc_uc_not_loadable(gsc) } {
        return -ENODEV;
    }
    panic!("intel_gsc_uc_init: GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_fini(gsc: *mut IntelGscUc) {
    if unsafe { gsc_uc_not_loadable(gsc) } {
        return;
    }
    panic!("intel_gsc_uc_fini: GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_suspend(gsc: *mut IntelGscUc) {
    if unsafe { gsc_uc_not_loadable(gsc) } {
        return;
    }
    panic!("intel_gsc_uc_suspend: GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_resume(gsc: *mut IntelGscUc) {
    if unsafe { gsc_uc_not_loadable(gsc) } {
        return;
    }
    panic!("intel_gsc_uc_resume: GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_flush_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_flush_work(gsc: *mut IntelGscUc) {
    if unsafe { gsc_uc_not_loadable(gsc) } {
        return;
    }
    panic!("intel_gsc_uc_flush_work: GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_load_start()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_load_start(gsc: *mut IntelGscUc) {
    if unsafe { gsc_uc_not_loadable(gsc) } {
        return;
    }
    panic!("intel_gsc_uc_load_start: GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc.c intel_gsc_uc_load_status()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_load_status(gsc: *mut IntelGscUc, _printer: *mut c_void) {
    if unsafe { gsc_uc_not_loadable(gsc) } {
        return;
    }
    panic!("intel_gsc_uc_load_status: GSC uC is not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc_heci_cmd_submit.c intel_gsc_uc_heci_cmd_emit_mtl_header()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_heci_cmd_emit_mtl_header(
    _header: *mut c_void,
    _heci_client_id: u8,
    _message_size: u32,
    _host_session_id: u64,
) {
    panic!("intel_gsc_uc_heci_cmd_emit_mtl_header: GSC HECI commands are not supported by TheKernel");
}

// upstream: gt/uc/intel_gsc_uc_heci_cmd_submit.c intel_gsc_uc_heci_cmd_submit_packet()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gsc_uc_heci_cmd_submit_packet(
    _gsc: *mut IntelGscUc,
    _addr_in: u64,
    _size_in: u32,
    _addr_out: u64,
    _size_out: u32,
) -> i32 {
    -ENODEV
}

// ---------------------------------------------------------------------------
// pxp/intel_pxp.c, pxp/intel_pxp_huc.c (PXP requires GSC; unsupported)
// ---------------------------------------------------------------------------

// upstream: pxp/intel_pxp.c intel_pxp_get_readiness_status()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pxp_get_readiness_status(_pxp: *mut c_void, _timeout_ms: c_int) -> c_int {
    -ENODEV
}

// upstream: pxp/intel_pxp.c intel_pxp_is_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pxp_is_active(_pxp: *const c_void) -> bool {
    false
}

// upstream: pxp/intel_pxp.c intel_pxp_is_enabled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pxp_is_enabled(_pxp: *const c_void) -> bool {
    false
}

// upstream: pxp/intel_pxp.c intel_pxp_key_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pxp_key_check(_object: *mut c_void, _assign: bool) -> c_int {
    -ENODEV
}

// upstream: pxp/intel_pxp.c intel_pxp_start()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pxp_start(_pxp: *mut c_void) -> c_int {
    -ENODEV
}

// upstream: pxp/intel_pxp_huc.c intel_pxp_huc_load_and_auth()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pxp_huc_load_and_auth(_pxp: *mut c_void) -> i32 {
    -ENODEV
}

// ---------------------------------------------------------------------------
// i915_perf.c (OA). TheKernel exposes no OA stream, so no perf state exists:
// no metrics idr entries, no exclusive stream, no OA timestamp frequency.
// ---------------------------------------------------------------------------

// upstream: i915_perf.c i915_oa_config_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_oa_config_release(_ref_: *mut c_void) {
    panic!("i915_oa_config_release: no OA config can exist without the OA perf interface");
}

// upstream: i915_perf.c i915_oa_init_reg_state()
//
// Linux returns unless the context is a render context and an exclusive OA
// stream is open. TheKernel never opens an OA stream, so both checks end here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_oa_init_reg_state(_ce: *const IntelContext, _engine: *const IntelEngineCs) {}

// upstream: i915_perf.c i915_perf_get_oa_config()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_perf_get_oa_config(_perf: *mut c_void, _metrics_set: c_int) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: i915_perf.c i915_perf_ioctl_version()
//
// I915_PARAM_PERF_REVISION reports 0 when no OA perf interface exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_perf_ioctl_version(_i915: *mut crate::linux_i915_private::DrmI915Private) -> c_int {
    0
}

// upstream: i915_perf.c i915_perf_oa_timestamp_frequency()
//
// No OA timestamp source exists; 0 is the unsupported value reported to userspace.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_perf_oa_timestamp_frequency(_i915: *mut crate::linux_i915_private::DrmI915Private) -> u32 {
    0
}

// ---------------------------------------------------------------------------
// Header-level helpers whose upstream definitions are declared only or are
// spelled differently in the Rust callers.
// ---------------------------------------------------------------------------

// upstream: i915_vma.h i915_vma_unlink_ctx() — declared in 7.2.3, never defined.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_unlink_ctx(_vma: *mut c_void) {
    panic!("i915_vma_unlink_ctx: has no definition in Linux 7.2.3");
}

