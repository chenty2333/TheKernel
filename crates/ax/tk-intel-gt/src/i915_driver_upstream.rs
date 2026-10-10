// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/i915_driver.c and the
// i915_probe_error() macro in drivers/gpu/drm/i915/i915_utils.h (which expands
// to drm_err(&(i915)->drm, ...)). Display and DRM-core calls go through the
// framework provider table (i915_probe_provider_upstream.rs); other owners'
// symbols are declared extern "C" below. The complete MIT grant is retained in
// ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, non_upper_case_globals, dead_code, unused_variables, unused_unsafe, clashing_extern_declarations)]

use core::ffi::{CStr, VaList, c_char, c_int, c_long, c_ulong, c_void};

use crate::{
    i915_probe_provider_upstream as provider,
    i915_probe_provider_upstream::framework,
    intel_gt_types_upstream::I915_MAX_GT,
    intel_engine_cs_upstream::Spinlock,
    linux::memory::{atomic_add},
    linux::pm::intel_runtime_pm_put,
    linux::locks::spin_lock_init,
    i915_utils_upstream::i915_vtd_active,
    intel_device_info_types_upstream::*,
    intel_device_info_upstream::*,
    intel_pciids_upstream::*,
    linux::i915::*,
    linux::primitives::str_yes_no,
    linux::memory::atomic_read,
    linux_i915_private::{DrmI915Private, I915Params},
    linux_print::{CFormatArg, DrmLogLevel, drm_log_at, drm_printer, format_message, drm_debug_enabled, drm_dbg_printer},
    i915_ioctl_upstream::DrmIoctlDesc,
    intel_gt_types_upstream::IntelGt,
    intel_uncore_types_upstream::{IntelUncore, IntelRuntimePm},
    intel_runtime_pm_upstream::IntelRuntimePmLayout,
};
/// Owned value for one printf conversion pulled from the C va_list.
type OwnedArg = alloc::boxed::Box<dyn CFormatArg>;

/// Reads the C string referenced by a `%s` argument, mapping NULL to
/// `(null)` as the Linux vsnprintf implementation does.
unsafe fn c_str_arg(ptr: *const c_char) -> alloc::string::String {
    if ptr.is_null() {
        return alloc::string::String::from("(null)");
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Walks a printf format and pulls one correctly typed argument per
/// conversion from the C variadic list, following the C default promotions
/// (`int`, `unsigned int`, `long`, `unsigned long`, pointers, strings).
unsafe fn collect_c_args(format: &[u8], args: &mut VaList<'_>) -> alloc::vec::Vec<OwnedArg> {
    let mut out: alloc::vec::Vec<OwnedArg> = alloc::vec::Vec::new();
    let mut i = 0;
    while i < format.len() {
        if format[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        if i < format.len() && format[i] == b'%' {
            i += 1;
            continue;
        }
        // flags, width, precision
        while i < format.len() && b"-+ #0123456789.*".contains(&format[i]) {
            if format[i] == b'*' {
                // Width or precision taken from the argument list.
                out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_int>() }));
            }
            i += 1;
        }
        // length modifiers
        let mut long_len = false;
        while i < format.len() && b"hlLqjzt".contains(&format[i]) {
            if format[i] == b'l' || format[i] == b'L' || format[i] == b'q' || format[i] == b'j' || format[i] == b'z' || format[i] == b't' {
                long_len = true;
            }
            i += 1;
        }
        if i >= format.len() {
            break;
        }
        let conv = format[i];
        i += 1;
        match conv {
            b'd' | b'i' => {
                if long_len {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_long>() }));
                } else {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_int>() }));
                }
            }
            b'u' | b'x' | b'X' | b'o' => {
                if long_len {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_ulong>() }));
                } else {
                    out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<u32>() }));
                }
            }
            b'c' => {
                out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<c_int>() }));
            }
            b's' => {
                let text = unsafe { c_str_arg(args.next_arg::<*const c_char>()) };
                out.push(alloc::boxed::Box::new(text));
            }
            b'p' => {
                out.push(alloc::boxed::Box::new(unsafe { args.next_arg::<*const c_void>() } as usize));
            }
            _ => {}
        }
    }
    out
}

/// Source `i915_probe_error(i915, fmt, ...)` from i915_utils.h: a
/// `drm_err(&(i915)->drm, fmt, ...)` probe-time error report.
///
/// # Safety
/// `fmt` must point to a NUL-terminated C format string and the variadic
/// arguments must match its conversions, as in the C call sites.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_probe_error(
    i915: *mut DrmI915Private,
    fmt: *const c_char,
    mut args: ...
) {
    let _ = i915;
    if fmt.is_null() {
        return;
    }
    let format_bytes = unsafe { CStr::from_ptr(fmt) }.to_bytes();
    let owned = unsafe { collect_c_args(format_bytes, &mut args) };
    let refs: alloc::vec::Vec<&dyn CFormatArg> = owned.iter().map(|a| a.as_ref() as &dyn CFormatArg).collect();
    let format = match core::str::from_utf8(format_bytes) {
        Ok(text) => text,
        Err(_) => return,
    };
    let message = format_message(format, &refs);
    drm_log_at(
        DrmLogLevel::Error,
        "i915 DRM error",
        file!(),
        line!(),
        &message,
    );
}

/// Helper declared by intel_gt_upstream.rs for gt probe failures. It has no
/// direct upstream counterpart; it logs through the same drm_err path as
/// `i915_probe_error`, with the gt name and return code as its fixed fields.
///
/// # Safety
/// `fmt` and `name` must be NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_probe_error(
    i915: *mut DrmI915Private,
    fmt: *const c_char,
    name: *const c_char,
    ret: i32,
) {
    let _ = i915;
    if fmt.is_null() {
        return;
    }
    let format = match unsafe { CStr::from_ptr(fmt) }.to_str() {
        Ok(text) => text,
        Err(_) => return,
    };
    let name_text = unsafe { c_str_arg(name) };
    let message = format_message(format, &[&name_text as &dyn CFormatArg, &ret as &dyn CFormatArg]);
    drm_log_at(
        DrmLogLevel::Error,
        "i915 DRM error",
        file!(),
        line!(),
        &message,
    );
}





use crate::i915_drm_client_upstream::{i915_drm_client_put};
use crate::i915_gem_context_upstream::{i915_gem_context_close};
use crate::i915_gem_core_upstream::{i915_gem_cleanup_early, i915_gem_drain_freed_objects, i915_gem_driver_register, i915_gem_driver_release, i915_gem_driver_remove, i915_gem_driver_unregister, i915_gem_init, i915_gem_init_early, i915_gem_open, i915_gem_runtime_suspend};
use crate::i915_gem_object_upstream::{i915_gem_flush_free_objects};
use crate::i915_gem_pm_upstream::{i915_gem_backup_suspend, i915_gem_freeze, i915_gem_freeze_late, i915_gem_resume, i915_gem_suspend, i915_gem_suspend_late};
use crate::i915_gem_wait_upstream::{i915_gem_fence_wait_priority_display};
use crate::i915_gpu_error_upstream::{i915_reset_error_state};
use crate::intel_device_info_upstream::{intel_device_info_driver_create, intel_device_info_print, intel_device_info_runtime_init, intel_device_info_runtime_init_early, intel_platform_name};
use crate::intel_ggtt_upstream::{i915_ggtt_driver_late_release, i915_ggtt_driver_release, i915_ggtt_enable_hw, i915_ggtt_init_hw, i915_ggtt_probe_hw, i915_ggtt_resume, i915_ggtt_suspend};
use crate::intel_gt_pm_upstream::{intel_gt_resume_early, intel_gt_runtime_resume, intel_gt_runtime_suspend};
use crate::intel_gt_upstream::{intel_gt_driver_late_release_all, intel_gt_driver_register, intel_gt_driver_unregister, intel_gt_info_print, intel_gt_init_mmio, intel_gt_probe_all, intel_gt_tiles_init, intel_root_gt_init_early};
use crate::intel_gtt_upstream::{setup_private_pat};
use crate::intel_reset_hw_upstream::{intel_gt_reset_all_engines};
use crate::intel_reset_upstream::{intel_gt_gpu_reset_clobbers_display};
use crate::intel_runtime_pm_upstream::{intel_runtime_pm_disable, intel_runtime_pm_driver_last_release, intel_runtime_pm_driver_release, intel_runtime_pm_enable, intel_runtime_pm_get, intel_runtime_pm_init_early};

/// `drm_info()` equivalent for the printk level used by this file.
macro_rules! drm_info {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let _ = $device;
        let __args: &[&dyn CFormatArg] = &[$(&($argument) as &dyn CFormatArg),*];
        let __message = format_message($format, __args);
        drm_log_at(DrmLogLevel::Info, "i915 DRM info", file!(), line!(), &__message);
    }};
}

/// `drm_printf(p, ...)` where `p` is a `*mut drm_printer`.
macro_rules! drm_printf_info {
    ($printer:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __args: &[&dyn CFormatArg] = &[$(&($argument) as &dyn CFormatArg),*];
        let __message = format_message($format, __args);
        crate::linux_print::drm_printer_write($printer as *mut drm_printer, &__message);
    }};
}

/// `INTEL_RPM_WAKELOCK_BIAS` (intel_runtime_pm.h): 1 << INTEL_RPM_WAKELOCK_SHIFT.
const INTEL_RPM_WAKELOCK_BIAS_I915: i32 = 1 << 16;

/// `disable_rpm_wakeref_asserts()` (intel_runtime_pm.h).
unsafe fn disable_rpm_wakeref_asserts<T>(rpm: *mut T) {
    unsafe { atomic_add(INTEL_RPM_WAKELOCK_BIAS_I915 + 1, &mut (*rpm.cast::<IntelRuntimePmLayout>()).wakeref_count) };
}

/// `enable_rpm_wakeref_asserts()` (intel_runtime_pm.h).
unsafe fn enable_rpm_wakeref_asserts<T>(rpm: *mut T) {
    unsafe { atomic_add(-(INTEL_RPM_WAKELOCK_BIAS_I915 + 1), &mut (*rpm.cast::<IntelRuntimePmLayout>()).wakeref_count) };
}

/// `atomic_read(&rpm->wakeref_count)`.
pub(crate) unsafe fn atomic_read_rpm_wakeref(rpm: *mut IntelRuntimePm) -> i32 {
    unsafe { atomic_read(&(*rpm.cast::<IntelRuntimePmLayout>()).wakeref_count) }
}

/// `&i915->uncore`.
unsafe fn uncore_of(i915: *mut DrmI915Private) -> *mut IntelUncore {
    unsafe { uncore_of(i915).cast() }
}

/// `INTEL_REVID(i915)` = `to_pci_dev(i915->drm.dev)->revision`.
unsafe fn intel_revid(i915: *mut DrmI915Private) -> u8 {
    unsafe { provider::pci_revision(to_pci_dev((*i915).drm.dev)) }
}

/// `HAS_PPGTT(i915)` = `INTEL_PPGTT(i915) != INTEL_PPGTT_NONE`.
unsafe fn has_ppgtt(i915: *mut DrmI915Private) -> bool {
    unsafe { (*i915).runtime.ppgtt_type != INTEL_PPGTT_NONE }
}

/// `HAS_RUNTIME_PM(i915)` = `INTEL_INFO(i915)->has_runtime_pm`.
unsafe fn has_runtime_pm_flag(i915: *mut DrmI915Private) -> bool {
    unsafe { (*((*i915).info as *const IntelDeviceInfo)).flag(DEV_INFO_FLAG_HAS_RUNTIME_PM) }
}

/// `IS_HASWELL_EARLY_SDV(i915)`.
unsafe fn is_haswell_early_sdv(i915: *mut DrmI915Private) -> bool {
    let p = i915.cast::<c_void>();
    unsafe { IS_HASWELL(p) && ((*i915).runtime.device_id & 0xFF00) == 0x0C00 }
}

/// `drmm_add_action_or_reset()` (DRM provider).
unsafe fn drmm_add_action_or_reset(
    drm: *mut c_void,
    action: unsafe extern "C" fn(*mut c_void, *mut c_void),
    data: *mut c_void,
) -> c_int {
    unsafe { (framework().drmm_add_action_or_reset)(drm, action, data) }
}

/// `intel_clock_gating_init()` (display provider).
unsafe fn intel_display_device_probe(pdev: *mut c_void, parent: *const c_void) -> *mut c_void {
    unsafe { (framework().intel_display_device_probe)(pdev, parent) }
}

unsafe fn intel_clock_gating_init(dev: *mut c_void) {
    unsafe { (framework().intel_clock_gating_init)(dev) }
}


/// `CONFIG_DRM_I915_DEBUG_GEM` (linux/config.rs: disabled).
const CONFIG_DRM_I915_DEBUG_GEM: bool = false;

/// `IS_ERR()` / `PTR_ERR()` on a kernel pointer.
fn is_err_ptr(p: *mut c_void) -> bool {
    let v = p as isize;
    v < 0 && v >= -4095
}

unsafe extern "C" {
    pub fn intel_uncore_suspend(uncore: *mut IntelUncore);
    pub fn intel_uncore_runtime_resume(uncore: *mut IntelUncore);
    pub fn intel_uncore_unclaimed_mmio(uncore: *mut IntelUncore) -> bool;
    pub fn intel_uncore_arm_unclaimed_mmio_detection(uncore: *mut IntelUncore) -> bool;
    pub fn intel_uncore_init_mmio(uncore: *mut IntelUncore) -> c_int;
    pub fn intel_uncore_mmio_debug_init_early(i915: *mut DrmI915Private);
    pub fn assert_forcewakes_inactive(uncore: *mut IntelUncore);
}

unsafe extern "C" {
    pub fn alloc_workqueue(fmt: *const c_char, flags: u32, max_active: c_int, ...) -> *mut c_void;
    pub fn intel_uncore_fini_mmio(dev: *mut c_void, data: *mut c_void);
    pub fn pci_set_drvdata(pdev: *mut c_void, data: *mut c_void);
    pub fn pci_get_drvdata(pdev: *mut c_void) -> *mut c_void;
    pub fn pci_enable_device(pdev: *mut c_void) -> c_int;
    pub fn pci_disable_device(pdev: *mut c_void);
    pub fn dev_get_drvdata(dev: *mut c_void) -> *mut c_void;
    pub fn acpi_target_system_state() -> u32;
    pub fn intel_gt_support_legacy_fencing(gt: *mut IntelGt) -> bool;
    pub fn intel_irq_suspend(i915: *mut DrmI915Private);
    pub fn intel_irq_resume(i915: *mut DrmI915Private);
    pub fn intel_vgpu_active(i915: *mut DrmI915Private) -> bool;
    pub fn mutex_init(lock: *mut c_void);
    pub fn mutex_destroy(lock: *mut c_void);
    pub fn to_pci_dev(dev: *mut c_void) -> *mut c_void;
    pub fn aperture_remove_conflicting_pci_devices(pdev: *mut c_void, name: *const c_char) -> c_int;
    pub fn i915_edram_detect(i915: *mut DrmI915Private);
    pub fn i915_gmch_bar_setup(i915: *mut DrmI915Private);
    pub fn i915_gmch_bar_teardown(i915: *mut DrmI915Private);
    pub fn i915_gmch_bridge_setup(i915: *mut DrmI915Private) -> c_int;
    pub fn intel_region_ttm_device_fini(dev_priv: *mut DrmI915Private);
    pub fn intel_region_ttm_device_init(dev_priv: *mut DrmI915Private) -> c_int;
    pub fn vlv_iosf_sb_fini(i915: *mut DrmI915Private);
    pub fn vlv_iosf_sb_init(i915: *mut DrmI915Private);
    pub fn add_taint(flag: u32, lockdep_ok: i32);
    pub fn destroy_workqueue(wq: *mut c_void);
    pub fn dma_set_coherent_mask(dev: *mut c_void, mask: u64) -> c_int;
    pub fn dma_set_mask(dev: *mut c_void, mask: u64) -> c_int;
    pub fn dma_set_max_seg_size(dev: *mut c_void, size: u32);
    pub fn i915_memcpy_init_early(dev_priv: *mut DrmI915Private);
    pub fn i915_params_free(params: *mut c_void);
    pub fn intel_irq_fini(i915: *mut DrmI915Private);
    pub fn intel_irq_init(dev_priv: *mut DrmI915Private);
    pub fn intel_irq_install(dev_priv: *mut DrmI915Private) -> c_int;
    pub fn intel_irq_uninstall(dev_priv: *mut DrmI915Private);
    pub fn intel_memory_regions_driver_release(i915: *mut DrmI915Private);
    pub fn intel_memory_regions_hw_probe(i915: *mut DrmI915Private) -> c_int;
    pub fn intel_pcode_init(uncore: *mut c_void) -> c_int;
    pub fn intel_step_init(i915: *mut DrmI915Private);
    pub fn intel_vgpu_detect(dev_priv: *mut DrmI915Private);
    pub fn intel_vgpu_has_full_ppgtt(dev_priv: *mut DrmI915Private) -> bool;
    pub fn intel_vgpu_has_hwsp_emulation(dev_priv: *mut DrmI915Private) -> bool;
    pub fn intel_vgpu_register(i915: *mut DrmI915Private);
    pub fn pci_d3cold_disable(dev: *mut c_void);
    pub fn pci_d3cold_enable(dev: *mut c_void);
    pub fn pci_disable_msi(dev: *mut c_void);
    pub fn pci_enable_msi(dev: *mut c_void) -> c_int;
    pub fn pci_restore_state(dev: *mut c_void);
    pub fn pci_save_state(dev: *mut c_void) -> c_int;
    pub fn pci_set_master(dev: *mut c_void);
    pub fn pci_set_power_state(dev: *mut c_void, state: u32) -> c_int;
    pub fn pcie_find_root_port(dev: *mut c_void) -> *mut c_void;
    pub fn snb_pcode_write_p(uncore: *mut c_void, mbcmd: u32, p1: u32, p2: u32, val: u32) -> c_int;
    pub fn synchronize_rcu();
    pub fn vlv_resume_prepare(dev_priv: *mut DrmI915Private, rpm_resume: bool) -> c_int;
    pub fn vlv_suspend_cleanup(i915: *mut DrmI915Private);
    pub fn vlv_suspend_complete(dev_priv: *mut DrmI915Private) -> c_int;
    pub fn vlv_suspend_init(i915: *mut DrmI915Private) -> c_int;
}

unsafe fn drm_atomic_helper_shutdown(dev: *mut c_void) {
    unsafe { (framework().drm_atomic_helper_shutdown)(dev) }
}

unsafe fn drm_client_dev_resume(dev: *mut c_void) {
    unsafe { (framework().drm_client_dev_resume)(dev) }
}

unsafe fn drm_client_dev_suspend(dev: *mut c_void) {
    unsafe { (framework().drm_client_dev_suspend)(dev) }
}

unsafe fn drm_dev_register(dev: *mut c_void, flags: c_ulong) -> c_int {
    unsafe { (framework().drm_dev_register)(dev, flags) }
}

unsafe fn drm_dev_unplug(dev: *mut c_void) {
    unsafe { (framework().drm_dev_unplug)(dev) }
}

unsafe fn drm_dev_unregister(dev: *mut c_void) {
    unsafe { (framework().drm_dev_unregister)(dev) }
}

unsafe fn drm_kms_helper_poll_disable(dev: *mut c_void) {
    unsafe { (framework().drm_kms_helper_poll_disable)(dev) }
}

unsafe fn drm_kms_helper_poll_enable(dev: *mut c_void) {
    unsafe { (framework().drm_kms_helper_poll_enable)(dev) }
}

unsafe fn drm_mode_config_reset(dev: *mut c_void) {
    unsafe { (framework().drm_mode_config_reset)(dev) }
}

unsafe fn i915_debugfs_register(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_debugfs_register)(i915) }
}

unsafe fn i915_hwmon_register(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_hwmon_register)(i915) }
}

unsafe fn i915_hwmon_unregister(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_hwmon_unregister)(i915) }
}

unsafe fn i915_perf_fini(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_perf_fini)(i915) }
}

unsafe fn i915_perf_init(i915: *mut DrmI915Private) -> c_int {
    unsafe { (framework().i915_perf_init)(i915) }
}

unsafe fn i915_perf_register(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_perf_register)(i915) }
}

unsafe fn i915_perf_unregister(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_perf_unregister)(i915) }
}

unsafe fn i915_pmu_register(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_pmu_register)(i915) }
}

unsafe fn i915_pmu_unregister(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_pmu_unregister)(i915) }
}

unsafe fn i915_setup_sysfs(dev_priv: *mut DrmI915Private) {
    unsafe { (framework().i915_setup_sysfs)(dev_priv) }
}

unsafe fn i915_switcheroo_register(i915: *mut DrmI915Private) -> c_int {
    unsafe { (framework().i915_switcheroo_register)(i915) }
}

unsafe fn i915_switcheroo_unregister(i915: *mut DrmI915Private) {
    unsafe { (framework().i915_switcheroo_unregister)(i915) }
}

unsafe fn i915_teardown_sysfs(dev_priv: *mut DrmI915Private) {
    unsafe { (framework().i915_teardown_sysfs)(dev_priv) }
}

unsafe fn i9xx_display_sr_restore(display: *mut c_void) {
    unsafe { (framework().i9xx_display_sr_restore)(display) }
}

unsafe fn i9xx_display_sr_save(display: *mut c_void) {
    unsafe { (framework().i9xx_display_sr_save)(display) }
}

unsafe fn intel_bw_init_hw(display: *mut c_void) {
    unsafe { (framework().intel_bw_init_hw)(display) }
}

unsafe fn intel_clock_gating_hooks_init(drm: *mut c_void) {
    unsafe { (framework().intel_clock_gating_hooks_init)(drm) }
}

unsafe fn intel_display_device_info_runtime_init(display: *mut c_void) {
    unsafe { (framework().intel_display_device_info_runtime_init)(display) }
}

unsafe fn intel_display_device_present(display: *mut c_void) -> bool {
    unsafe { (framework().intel_display_device_present)(display) }
}

unsafe fn intel_display_device_remove(display: *mut c_void) {
    unsafe { (framework().intel_display_device_remove)(display) }
}

unsafe fn intel_display_driver_disable_user_access(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_disable_user_access)(display) }
}

unsafe fn intel_display_driver_early_probe(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_early_probe)(display) }
}

unsafe fn intel_display_driver_enable_user_access(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_enable_user_access)(display) }
}

unsafe fn intel_display_driver_init_hw(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_init_hw)(display) }
}

unsafe fn intel_display_driver_probe(display: *mut c_void) -> c_int {
    unsafe { (framework().intel_display_driver_probe)(display) }
}

unsafe fn intel_display_driver_probe_nogem(display: *mut c_void) -> c_int {
    unsafe { (framework().intel_display_driver_probe_nogem)(display) }
}

unsafe fn intel_display_driver_probe_noirq(display: *mut c_void) -> c_int {
    unsafe { (framework().intel_display_driver_probe_noirq)(display) }
}

unsafe fn intel_display_driver_register(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_register)(display) }
}

unsafe fn intel_display_driver_remove(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_remove)(display) }
}

unsafe fn intel_display_driver_remove_nogem(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_remove_nogem)(display) }
}

unsafe fn intel_display_driver_remove_noirq(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_remove_noirq)(display) }
}

unsafe fn intel_display_driver_resume(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_resume)(display) }
}

unsafe fn intel_display_driver_resume_access(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_resume_access)(display) }
}

unsafe fn intel_display_driver_suspend(display: *mut c_void) -> c_int {
    unsafe { (framework().intel_display_driver_suspend)(display) }
}

unsafe fn intel_display_driver_suspend_access(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_suspend_access)(display) }
}

unsafe fn intel_display_driver_unregister(display: *mut c_void) {
    unsafe { (framework().intel_display_driver_unregister)(display) }
}

unsafe fn intel_display_power_cleanup(display: *mut c_void) {
    unsafe { (framework().intel_display_power_cleanup)(display) }
}

unsafe fn intel_display_power_disable(display: *mut c_void) {
    unsafe { (framework().intel_display_power_disable)(display) }
}

unsafe fn intel_display_power_driver_remove(display: *mut c_void) {
    unsafe { (framework().intel_display_power_driver_remove)(display) }
}

unsafe fn intel_display_power_enable(display: *mut c_void) {
    unsafe { (framework().intel_display_power_enable)(display) }
}

unsafe fn intel_display_power_resume_early(display: *mut c_void) {
    unsafe { (framework().intel_display_power_resume_early)(display) }
}

unsafe fn intel_display_power_runtime_resume(display: *mut c_void) {
    unsafe { (framework().intel_display_power_runtime_resume)(display) }
}

unsafe fn intel_display_power_runtime_suspend(display: *mut c_void) {
    unsafe { (framework().intel_display_power_runtime_suspend)(display) }
}

unsafe fn intel_display_power_suspend_late(display: *mut c_void, s2idle: bool) {
    unsafe { (framework().intel_display_power_suspend_late)(display, s2idle) }
}

unsafe fn intel_dmc_resume(display: *mut c_void) {
    unsafe { (framework().intel_dmc_resume)(display) }
}

unsafe fn intel_dmc_suspend(display: *mut c_void) {
    unsafe { (framework().intel_dmc_suspend)(display) }
}

unsafe fn intel_dp_mst_suspend(display: *mut c_void) {
    unsafe { (framework().intel_dp_mst_suspend)(display) }
}

unsafe fn intel_dpt_resume(display: *mut c_void) {
    unsafe { (framework().intel_dpt_resume)(display) }
}

unsafe fn intel_dpt_suspend(display: *mut c_void) {
    unsafe { (framework().intel_dpt_suspend)(display) }
}

unsafe fn intel_dram_detect(display: *mut c_void) -> c_int {
    unsafe { (framework().intel_dram_detect)(display) }
}

unsafe fn intel_encoder_shutdown_all(display: *mut c_void) {
    unsafe { (framework().intel_encoder_shutdown_all)(display) }
}

unsafe fn intel_encoder_suspend_all(display: *mut c_void) {
    unsafe { (framework().intel_encoder_suspend_all)(display) }
}

unsafe fn intel_gmbus_reset(display: *mut c_void) {
    unsafe { (framework().intel_gmbus_reset)(display) }
}

unsafe fn intel_gvt_driver_remove(dev_priv: *mut DrmI915Private) {
    unsafe { (framework().intel_gvt_driver_remove)(dev_priv) }
}

unsafe fn intel_gvt_init(dev_priv: *mut DrmI915Private) -> c_int {
    unsafe { (framework().intel_gvt_init)(dev_priv) }
}

unsafe fn intel_gvt_resume(dev_priv: *mut DrmI915Private) {
    unsafe { (framework().intel_gvt_resume)(dev_priv) }
}

unsafe fn intel_hpd_cancel_work(display: *mut c_void) {
    unsafe { (framework().intel_hpd_cancel_work)(display) }
}

unsafe fn intel_hpd_init(display: *mut c_void) {
    unsafe { (framework().intel_hpd_init)(display) }
}

unsafe fn intel_hpd_poll_disable(display: *mut c_void) {
    unsafe { (framework().intel_hpd_poll_disable)(display) }
}

unsafe fn intel_hpd_poll_enable(display: *mut c_void) {
    unsafe { (framework().intel_hpd_poll_enable)(display) }
}

unsafe fn intel_init_pch_refclk(display: *mut c_void) {
    unsafe { (framework().intel_init_pch_refclk)(display) }
}

unsafe fn intel_match_g8_cpu() -> bool {
    unsafe { (framework().intel_match_g8_cpu)() }
}

unsafe fn intel_opregion_cleanup(display: *mut c_void) {
    unsafe { (framework().intel_opregion_cleanup)(display) }
}

unsafe fn intel_opregion_notify_adapter(display: *mut c_void, state: u32) -> c_int {
    unsafe { (framework().intel_opregion_notify_adapter)(display, state) }
}

unsafe fn intel_opregion_resume(display: *mut c_void) {
    unsafe { (framework().intel_opregion_resume)(display) }
}

unsafe fn intel_opregion_setup(display: *mut c_void) -> c_int {
    unsafe { (framework().intel_opregion_setup)(display) }
}

unsafe fn intel_opregion_suspend(display: *mut c_void, state: u32) {
    unsafe { (framework().intel_opregion_suspend)(display, state) }
}

unsafe fn intel_pps_unlock_regs_wa(display: *mut c_void) {
    unsafe { (framework().intel_pps_unlock_regs_wa)(display) }
}

unsafe fn intel_pxp_debugfs_register(pxp: *mut c_void) {
    unsafe { (framework().intel_pxp_debugfs_register)(pxp) }
}

unsafe fn intel_pxp_fini(i915: *mut DrmI915Private) {
    unsafe { (framework().intel_pxp_fini)(i915) }
}

unsafe fn intel_pxp_init(i915: *mut DrmI915Private) -> c_int {
    unsafe { (framework().intel_pxp_init)(i915) }
}

unsafe fn intel_pxp_resume_complete(pxp: *mut c_void) {
    unsafe { (framework().intel_pxp_resume_complete)(pxp) }
}

unsafe fn intel_pxp_runtime_resume(pxp: *mut c_void) {
    unsafe { (framework().intel_pxp_runtime_resume)(pxp) }
}

unsafe fn intel_pxp_runtime_suspend(pxp: *mut c_void) {
    unsafe { (framework().intel_pxp_runtime_suspend)(pxp) }
}

unsafe fn intel_pxp_suspend(pxp: *mut c_void) {
    unsafe { (framework().intel_pxp_suspend)(pxp) }
}

unsafe fn intel_pxp_suspend_prepare(pxp: *mut c_void) {
    unsafe { (framework().intel_pxp_suspend_prepare)(pxp) }
}

unsafe fn intel_sbi_fini(display: *mut c_void) {
    unsafe { (framework().intel_sbi_fini)(display) }
}

unsafe fn intel_sbi_init(display: *mut c_void) {
    unsafe { (framework().intel_sbi_init)(display) }
}

unsafe fn skl_watermark_ipc_update(display: *mut c_void) {
    unsafe { (framework().skl_watermark_ipc_update)(display) }
}


// ---------------------------------------------------------------------------
// i915_driver.c: workqueues, pre-production detection, early/mmio/hw probe.
// ---------------------------------------------------------------------------

/// `WQ_UNBOUND`, `__WQ_ORDERED`, `WQ_PERCPU` (include/linux/workqueue.h).
const WQ_UNBOUND: u32 = 1 << 1;
const __WQ_ORDERED: u32 = 1 << 17;
const WQ_PERCPU: u32 = 1 << 5;

/// `TAINT_MACHINE_CHECK` (include/linux/panic.h).
const TAINT_MACHINE_CHECK: u32 = 4;
/// `LOCKDEP_STILL_OK` (enum lockdep_ok).
const LOCKDEP_STILL_OK_I915: i32 = 0;

/// `ENOMEM` (linux_config).
const I915_ENOMEM: i32 = crate::linux_config::ENOMEM;

/// `i915_workqueues_init()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private`.
unsafe fn i915_workqueues_init(dev_priv: *mut DrmI915Private) -> i32 {
    // The i915 workqueue is primarily used for batched retirement of
    // requests. All tasks on the workqueue acquire the dev mutex, so one
    // ordered instance is enough.
    unsafe {
        (*dev_priv).wq = alloc_workqueue(
            c"i915".as_ptr(),
            WQ_UNBOUND | __WQ_ORDERED,
            1,
        );
    }
    if unsafe { (*dev_priv).wq }.is_null() {
        drm_err!(dev_priv, "Failed to allocate workqueues.\n");
        return -I915_ENOMEM;
    }

    // The unordered i915 workqueue is used for work that need not run in
    // order.
    unsafe {
        (*dev_priv).unordered_wq = alloc_workqueue(c"i915-unordered".as_ptr(), WQ_PERCPU, 0);
    }
    if unsafe { (*dev_priv).unordered_wq }.is_null() {
        unsafe { destroy_workqueue((*dev_priv).wq) };
        drm_err!(dev_priv, "Failed to allocate workqueues.\n");
        return -I915_ENOMEM;
    }
    0
}

/// `i915_workqueues_cleanup()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private`.
unsafe fn i915_workqueues_cleanup(dev_priv: *mut DrmI915Private) {
    unsafe {
        destroy_workqueue((*dev_priv).unordered_wq);
        destroy_workqueue((*dev_priv).wq);
    }
}

/// `intel_detect_preproduction_hw()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private`.
unsafe fn intel_detect_preproduction_hw(dev_priv: *mut DrmI915Private) {
    let i = dev_priv.cast::<c_void>();
    let revid = unsafe { intel_revid(dev_priv) };
    let mut pre = false;
    pre |= unsafe { is_haswell_early_sdv(dev_priv) };
    pre |= unsafe { IS_SKYLAKE(i) } && revid < 0x6;
    pre |= unsafe { IS_BROXTON(i) } && revid < 0xA;
    pre |= unsafe { IS_KABYLAKE(i) } && revid < 0x1;
    pre |= unsafe { IS_GEMINILAKE(i) } && revid < 0x3;
    pre |= unsafe { IS_ICELAKE(i) } && revid < 0x7;
    pre |= unsafe { IS_TIGERLAKE(i) } && revid < 0x1;
    pre |= unsafe { IS_DG1(i) } && revid < 0x1;
    pre |= unsafe { IS_DG2_G10(i) } && revid < 0x8;
    pre |= unsafe { IS_DG2_G11(i) } && revid < 0x5;
    pre |= unsafe { IS_SUBPLATFORM(i, INTEL_DG2, INTEL_SUBPLATFORM_G12) } && revid < 0x1;

    if pre {
        drm_err!(
            dev_priv,
            "This is a pre-production stepping. It may not be fully functional.\n"
        );
        unsafe { add_taint(TAINT_MACHINE_CHECK, LOCKDEP_STILL_OK_I915) };
    }
}

/// `sanitize_gpu()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private` with its GT set up.
unsafe fn sanitize_gpu(i915: *mut DrmI915Private) {
    if !unsafe { intel_gt_gpu_reset_clobbers_display(to_gt(i915.cast::<c_void>())) } {
        for_each_gt_ptr(i915, |gt| {
            unsafe { intel_gt_reset_all_engines(gt) };
        });
    }
}

/// Iterates `for_each_gt(gt, i915, i)`: all GT slots that are populated.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`.
unsafe fn for_each_gt_ptr(i915: *mut DrmI915Private, mut f: impl FnMut(*mut IntelGt)) {
    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*i915).gt[id] };
        if !gt.is_null() {
            f(gt);
        }
    }
}

/// `i915_driver_early_probe()`: SW-only state, no device access.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private` with `info` set.
unsafe fn i915_driver_early_probe(dev_priv: *mut DrmI915Private) -> i32 {
    let display = unsafe { (*dev_priv).display };
    let mut ret: i32;

    unsafe { intel_device_info_runtime_init_early(dev_priv) };
    unsafe { intel_step_init(dev_priv) };
    unsafe { intel_uncore_mmio_debug_init_early(dev_priv) };
    unsafe { spin_lock_init(&mut *core::ptr::addr_of_mut!((*dev_priv).gpu_error.lock).cast::<Spinlock>()) };
    unsafe { intel_sbi_init(display) };
    unsafe { vlv_iosf_sb_init(dev_priv) };
    unsafe { mutex_init(core::ptr::addr_of_mut!((*dev_priv).sb_lock).cast::<c_void>()) };
    unsafe { i915_memcpy_init_early(dev_priv) };
    unsafe { intel_runtime_pm_init_early(core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast()) };

    ret = unsafe { i915_workqueues_init(dev_priv) };
    if ret < 0 {
        return ret;
    }

    ret = unsafe { vlv_suspend_init(dev_priv) };
    if ret < 0 {
        unsafe { i915_workqueues_cleanup(dev_priv) };
        return ret;
    }

    ret = unsafe { intel_region_ttm_device_init(dev_priv) };
    if ret != 0 {
        unsafe { vlv_suspend_cleanup(dev_priv) };
        unsafe { i915_workqueues_cleanup(dev_priv) };
        return ret;
    }

    ret = unsafe { intel_root_gt_init_early(dev_priv) };
    if ret < 0 {
        unsafe { intel_region_ttm_device_fini(dev_priv) };
        unsafe { vlv_suspend_cleanup(dev_priv) };
        unsafe { i915_workqueues_cleanup(dev_priv) };
        return ret;
    }

    unsafe { i915_gem_init_early(dev_priv) };
    unsafe { intel_irq_init(dev_priv) };
    unsafe { intel_display_driver_early_probe(display) };
    unsafe { intel_clock_gating_hooks_init((*dev_priv).drm.dev) };
    unsafe { intel_detect_preproduction_hw(dev_priv) };
    0
}

/// `i915_driver_late_release()`: undoes `i915_driver_early_probe()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private`.
unsafe fn i915_driver_late_release(dev_priv: *mut DrmI915Private) {
    let display = unsafe { (*dev_priv).display };
    unsafe { intel_irq_fini(dev_priv) };
    unsafe { intel_display_power_cleanup(display) };
    unsafe { i915_gem_cleanup_early(dev_priv) };
    unsafe { intel_gt_driver_late_release_all(dev_priv) };
    unsafe { intel_region_ttm_device_fini(dev_priv) };
    unsafe { vlv_suspend_cleanup(dev_priv) };
    unsafe { i915_workqueues_cleanup(dev_priv) };
    unsafe { mutex_destroy(core::ptr::addr_of_mut!((*dev_priv).sb_lock).cast()) };
    unsafe { vlv_iosf_sb_fini(dev_priv) };
    unsafe { intel_sbi_fini(display) };
    unsafe { i915_params_free(core::ptr::addr_of_mut!((*dev_priv).params).cast()) };
    unsafe { intel_display_device_remove(display) };
    unsafe { (*dev_priv).display = core::ptr::null_mut() };
}

/// `i915_driver_mmio_probe()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private` after early probe.
unsafe fn i915_driver_mmio_probe(dev_priv: *mut DrmI915Private) -> i32 {
    let display = unsafe { (*dev_priv).display };

    let mut ret = unsafe { i915_gmch_bridge_setup(dev_priv) };
    if ret < 0 {
        return ret;
    }

    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if gt.is_null() {
            continue;
        }
        ret = unsafe { intel_uncore_init_mmio(core::ptr::addr_of_mut!((*gt).uncore).cast()) };
        if ret != 0 {
            return ret;
        }
        ret = unsafe {
            drmm_add_action_or_reset(
                core::ptr::addr_of_mut!((*dev_priv).drm).cast(),
                intel_uncore_fini_mmio,
                (*gt).uncore.cast(),
            )
        };
        if ret != 0 {
            return ret;
        }
    }

    // Try to make sure MCHBAR is enabled before poking at it.
    unsafe { i915_gmch_bar_setup(dev_priv) };
    unsafe { intel_device_info_runtime_init(dev_priv) };
    unsafe { intel_display_device_info_runtime_init(display) };

    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if gt.is_null() {
            continue;
        }
        ret = unsafe { intel_gt_init_mmio(gt) };
        if ret != 0 {
            unsafe { i915_gmch_bar_teardown(dev_priv) };
            return ret;
        }
    }

    // As early as possible, scrub existing GPU state before clobbering.
    unsafe { sanitize_gpu(dev_priv) };
    0
}

/// `i915_driver_mmio_release()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private`.
unsafe fn i915_driver_mmio_release(dev_priv: *mut DrmI915Private) {
    unsafe { i915_gmch_bar_teardown(dev_priv) };
}

/// `i915_set_dma_info()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private` with `info` set.
unsafe fn i915_set_dma_info(i915: *mut DrmI915Private) -> i32 {
    let info = unsafe { (*i915).info.cast::<IntelDeviceInfo>() };
    let mut mask_size = unsafe { (*info).dma_mask_size };
    let dev = unsafe { (*i915).drm.dev };

    // GEM_BUG_ON(!mask_size)
    assert!(mask_size != 0, "GEM_BUG_ON(!mask_size)");

    // We don't have a max segment size, so set it to the max so the sg
    // debugging layer doesn't complain.
    unsafe { dma_set_max_seg_size(dev, u32::MAX) };

    let mut ret = unsafe { dma_set_mask(dev, dma_bit_mask(mask_size)) };
    if ret != 0 {
        drm_err!(i915, "Can't set DMA mask/consistent mask ({})\n", ret);
        return ret;
    }

    // overlay on gen2 is broken and can't address above 1G
    if unsafe { GRAPHICS_VER(i915.cast::<c_void>()) } == 2 {
        mask_size = 30;
    }

    // 965GM sometimes incorrectly writes to the HWS using 32bit addressing
    // above 4GB, so use the full 32-bit mask there.
    if unsafe { IS_I965G(i915.cast::<c_void>()) || IS_I965GM(i915.cast::<c_void>()) } {
        mask_size = 32;
    }

    ret = unsafe { dma_set_coherent_mask(dev, dma_bit_mask(mask_size)) };
    if ret != 0 {
        drm_err!(i915, "Can't set DMA mask/consistent mask ({})\n", ret);
        return ret;
    }
    0
}

/// `DMA_BIT_MASK(n)`.
const fn dma_bit_mask(n: u32) -> u64 {
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// `i915_enable_g8()`: Wa_14022698537:dg2.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`.
unsafe fn i915_enable_g8(i915: *mut DrmI915Private) {
    let p = i915.cast::<c_void>();
    if unsafe { IS_DG2(p) } {
        if unsafe { IS_SUBPLATFORM(p, INTEL_DG2, INTEL_SUBPLATFORM_D) } && !unsafe { intel_match_g8_cpu() } {
            return;
        }
        unsafe {
            snb_pcode_write_p(
                uncore_of(i915).cast(),
                PCODE_POWER_SETUP,
                POWER_SETUP_SUBCOMMAND_G8_ENABLE,
                0,
                0,
            )
        };
    }
}

/// `PCODE_POWER_SETUP` and `POWER_SETUP_SUBCOMMAND_G8_ENABLE`
/// (intel_pcode_regs.h).
const PCODE_POWER_SETUP: u32 = 0x27;
const POWER_SETUP_SUBCOMMAND_G8_ENABLE: u32 = 0x2;

/// `i915_pcode_init()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private` with its GTs probed.
unsafe fn i915_pcode_init(i915: *mut DrmI915Private) -> i32 {
    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*i915).gt[id] };
        if gt.is_null() {
            continue;
        }
        let ret = unsafe { intel_pcode_init((*gt).uncore.cast()) };
        if ret != 0 {
            gt_err!(gt, "intel_pcode_init failed {}\n", ret);
            return ret;
        }
    }
    unsafe { i915_enable_g8(i915) };
    0
}

/// `i915_driver_hw_probe()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private` after mmio probe.
unsafe fn i915_driver_hw_probe(dev_priv: *mut DrmI915Private) -> i32 {
    let p = dev_priv.cast::<c_void>();
    let display = unsafe { (*dev_priv).display };
    let pdev = unsafe { to_pci_dev((*dev_priv).drm.dev) };
    let mut ret: i32;

    if unsafe { has_ppgtt(dev_priv) } {
        if unsafe { intel_vgpu_active(dev_priv) } && !unsafe { intel_vgpu_has_full_ppgtt(dev_priv) } {
            drm_err!(dev_priv, "incompatible vGPU found, support for isolated ppGTT required\n");
            return -crate::linux_config::ENXIO;
        }
    }

    if unsafe { HAS_EXECLISTS(p) } {
        if unsafe { intel_vgpu_active(dev_priv) } && !unsafe { intel_vgpu_has_hwsp_emulation(dev_priv) } {
            drm_err!(dev_priv, "old vGPU host found, support for HWSP emulation required\n");
            return -crate::linux_config::ENXIO;
        }
    }

    unsafe { i915_edram_detect(dev_priv) };

    ret = unsafe { i915_set_dma_info(dev_priv) };
    if ret != 0 {
        return ret;
    }

    ret = unsafe { i915_perf_init(dev_priv) };
    if ret != 0 {
        return ret;
    }

    ret = unsafe { i915_ggtt_probe_hw(dev_priv) };
    if ret != 0 {
        unsafe { i915_perf_fini(dev_priv) };
        return ret;
    }

    let driver_name = unsafe { driver_name_of((*dev_priv).drm.driver) };
    ret = unsafe { aperture_remove_conflicting_pci_devices(pdev, driver_name) };
    if ret != 0 {
        unsafe { i915_ggtt_driver_release(dev_priv) };
        unsafe { i915_gem_drain_freed_objects(dev_priv) };
        unsafe { i915_ggtt_driver_late_release(dev_priv) };
        unsafe { i915_perf_fini(dev_priv) };
        return ret;
    }

    // err_ggtt tail, shared by the three steps below.
    macro_rules! ggtt_unwind {
        () => {{
            unsafe { i915_ggtt_driver_release(dev_priv) };
            unsafe { i915_gem_drain_freed_objects(dev_priv) };
            unsafe { i915_ggtt_driver_late_release(dev_priv) };
            unsafe { i915_perf_fini(dev_priv) };
            return ret;
        }};
    }

    ret = unsafe { i915_ggtt_init_hw(dev_priv) };
    if ret != 0 {
        ggtt_unwind!();
    }

    ret = unsafe { intel_gt_tiles_init(dev_priv) };
    if ret != 0 {
        ggtt_unwind!();
    }

    ret = unsafe { intel_memory_regions_hw_probe(dev_priv) };
    if ret != 0 {
        ggtt_unwind!();
    }

    ret = unsafe { i915_ggtt_enable_hw(dev_priv) };
    if ret != 0 {
        drm_err!(dev_priv, "failed to enable GGTT\n");
        // err_mem_regions
        unsafe { intel_memory_regions_driver_release(dev_priv) };
        ggtt_unwind!();
    }

    unsafe { pci_set_master(pdev) };
    if unsafe { GRAPHICS_VER(p) } >= 5 {
        if unsafe { pci_enable_msi(pdev) } < 0 {
            drm_dbg!(dev_priv, "can't enable MSI");
        }
    }

    unsafe { intel_opregion_setup(display) };

    ret = unsafe { i915_pcode_init(dev_priv) };
    if ret != 0 {
        // err_opregion
        unsafe { intel_opregion_cleanup(display) };
        unsafe { i915_driver_hw_probe_msi_release(pdev) };
        unsafe { intel_memory_regions_driver_release(dev_priv) };
        ggtt_unwind!();
    }

    ret = unsafe { intel_dram_detect(display) };
    if ret != 0 {
        // err_opregion
        unsafe { intel_opregion_cleanup(display) };
        unsafe { i915_driver_hw_probe_msi_release(pdev) };
        unsafe { intel_memory_regions_driver_release(dev_priv) };
        ggtt_unwind!();
    }

    unsafe { intel_bw_init_hw(display) };
    0
}

/// `if (pdev->msi_enabled) pci_disable_msi(pdev);` (the `err_opregion`
/// unwind edge), with the `msi_enabled` bit read through the PCI provider.
///
/// # Safety
/// `pdev` must be a live PCI device.
unsafe fn i915_driver_hw_probe_msi_release(pdev: *mut c_void) {
    if unsafe { provider::pci_msi_enabled(pdev) } {
        unsafe { pci_disable_msi(pdev) };
    }
}

/// `driver->name` of a `struct drm_driver` (the PCI aperture owner name).
///
/// # Safety
/// `driver` must be a live `struct drm_driver` pointer.
unsafe fn driver_name_of(driver: *mut c_void) -> *const c_char {
    unsafe { provider::drm_driver_name(driver) }
}

/// `i915_driver_hw_remove()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private`.
unsafe fn i915_driver_hw_remove(dev_priv: *mut DrmI915Private) {
    let display = unsafe { (*dev_priv).display };
    let pdev = unsafe { to_pci_dev((*dev_priv).drm.dev) };

    unsafe { i915_perf_fini(dev_priv) };
    unsafe { intel_opregion_cleanup(display) };
    unsafe { i915_driver_hw_probe_msi_release(pdev) };
}

/// `i915_driver_register()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private` with display and GTs set up.
unsafe fn i915_driver_register(dev_priv: *mut DrmI915Private) -> i32 {
    let display = unsafe { (*dev_priv).display };

    unsafe { i915_gem_driver_register(dev_priv) };
    unsafe { i915_pmu_register(dev_priv) };
    unsafe { intel_vgpu_register(dev_priv) };

    let ret = unsafe { drm_dev_register(core::ptr::addr_of_mut!((*dev_priv).drm).cast(), 0) };
    if ret != 0 {
        i915_probe_error(
            dev_priv,
            c"Failed to register driver for userspace access!\n".as_ptr(),
        );
        unsafe { drm_dev_unregister(core::ptr::addr_of_mut!((*dev_priv).drm).cast()) };
        unsafe { i915_pmu_unregister(dev_priv) };
        unsafe { i915_gem_driver_unregister(dev_priv) };
        return ret;
    }

    unsafe { i915_debugfs_register(dev_priv) };
    unsafe { i915_setup_sysfs(dev_priv) };
    unsafe { i915_perf_register(dev_priv) };

    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_gt_driver_register(gt) };
        }
    }

    unsafe { intel_pxp_debugfs_register((*dev_priv).pxp) };
    unsafe { i915_hwmon_register(dev_priv) };
    unsafe { intel_display_driver_register(display) };
    unsafe { intel_display_power_enable(display) };
    unsafe {
        intel_runtime_pm_enable(core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast())
    };

    if unsafe { i915_switcheroo_register(dev_priv) } != 0 {
        drm_err!(dev_priv, "Failed to register vga switcheroo!\n");
    }
    0
}

/// `i915_driver_unregister()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private` that was registered.
unsafe fn i915_driver_unregister(dev_priv: *mut DrmI915Private) {
    let display = unsafe { (*dev_priv).display };

    unsafe { i915_switcheroo_unregister(dev_priv) };
    unsafe {
        intel_runtime_pm_disable(core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast())
    };
    unsafe { intel_display_power_disable(display) };
    unsafe { intel_display_driver_unregister(display) };
    unsafe { intel_pxp_fini(dev_priv) };

    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_gt_driver_unregister(gt) };
        }
    }

    unsafe { i915_hwmon_unregister(dev_priv) };
    unsafe { i915_perf_unregister(dev_priv) };
    unsafe { i915_pmu_unregister(dev_priv) };
    unsafe { i915_teardown_sysfs(dev_priv) };
    unsafe { drm_dev_unplug(core::ptr::addr_of_mut!((*dev_priv).drm).cast()) };
    unsafe { i915_gem_driver_unregister(dev_priv) };
}

/// `i915_print_iommu_status()`.
///
/// # Safety
/// `p` must be a live DRM printer.
pub unsafe fn i915_print_iommu_status(i915: *mut DrmI915Private, p: *mut drm_printer) {
    let active = unsafe { i915_vtd_active(i915) };
    drm_printf_info!(p, "iommu: %s\n", str_enabled_disabled(active));
}

/// `str_enabled_disabled()` (linux/string_helpers.h).
fn str_enabled_disabled(enable: bool) -> &'static str {
    if enable { "enabled" } else { "disabled" }
}

/// `intel_subplatform()` from i915_drv.h.
///
/// # Safety
/// `info` must be a live runtime-info record.
unsafe fn intel_subplatform(info: *const IntelRuntimeInfo, platform: u32) -> u32 {
    let pi = (platform / PLATFORM_MASK_PBITS_I915) as usize;
    unsafe { (*info).platform_mask[pi] & INTEL_SUBPLATFORM_MASK }
}

/// `BITS_PER_TYPE(u32) - INTEL_SUBPLATFORM_BITS` (`__platform_mask_index`).
const PLATFORM_MASK_PBITS_I915: u32 = 32 - INTEL_SUBPLATFORM_BITS;

/// `i915_welcome_messages()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private` after registration.
unsafe fn i915_welcome_messages(dev_priv: *mut DrmI915Private) {
    let p_dev = core::ptr::addr_of_mut!((*dev_priv).drm).cast::<c_void>();
    if unsafe { drm_debug_enabled(DRM_UT_DRIVER) } {
        let mut p = unsafe { drm_dbg_printer(&mut (*dev_priv).drm, DRM_UT_DRIVER, c"device info:".as_ptr()) };
        let info = unsafe { (*dev_priv).info.cast::<IntelDeviceInfo>() };
        let runtime = unsafe { core::ptr::addr_of!((*dev_priv).runtime) };
        let pp: *mut drm_printer = &mut p;

        let platform = unsafe { (*info).platform };
        drm_printf_info!(
            pp,
            "pciid=0x%04x rev=0x%02x platform=%s (subplatform=0x%x) gen=%i\n",
            unsafe { (*dev_priv).runtime.device_id } as u32,
            unsafe { intel_revid(dev_priv) } as u32,
            intel_platform_name(platform),
            unsafe { intel_subplatform(runtime, platform) },
            unsafe { GRAPHICS_VER(dev_priv.cast::<c_void>()) } as i32
        );
        unsafe { intel_device_info_print(info, runtime, pp) };
        unsafe { i915_print_iommu_status(dev_priv, pp) };
        for id in 0..I915_MAX_GT {
            let gt = unsafe { (*dev_priv).gt[id] };
            if !gt.is_null() {
                unsafe { intel_gt_info_print(core::ptr::addr_of!((*gt).info).cast(), pp) };
            }
        }
    }

    if CONFIG_DRM_I915_DEBUG {
        drm_info!(p_dev, "DRM_I915_DEBUG enabled\n");
    }
    if CONFIG_DRM_I915_DEBUG_GEM {
        drm_info!(p_dev, "DRM_I915_DEBUG_GEM enabled\n");
    }
    if CONFIG_DRM_I915_DEBUG_RUNTIME_PM {
        drm_info!(p_dev, "DRM_I915_DEBUG_RUNTIME_PM enabled\n");
    }
}

/// `CONFIG_DRM_I915_DEBUG` and `CONFIG_DRM_I915_DEBUG_RUNTIME_PM`: not set in
/// the configured TheKernel build (linux/config.rs does not enable them).
const CONFIG_DRM_I915_DEBUG: bool = false;
const CONFIG_DRM_I915_DEBUG_RUNTIME_PM: bool = false;
/// `DRM_UT_DRIVER` (drm_print.h).
const DRM_UT_DRIVER: u32 = 1 << 2;

/// `fence_priority_display()`: display parent-interface callback.
///
/// # Safety
/// `fence` must be a live `struct dma_fence`.
pub unsafe fn fence_priority_display(fence: *mut c_void) {
    if unsafe { provider::dma_fence_is_i915(fence) } {
        unsafe { i915_gem_fence_wait_priority_display(fence.cast()) };
    }
}

/// `has_auxccs()`: display parent-interface callback.
///
/// # Safety
/// `drm` must be a live `struct drm_device`.
pub unsafe fn has_auxccs(drm: *mut c_void) -> bool {
    let i915 = unsafe { to_i915(drm) };
    let p = i915.cast::<c_void>();
    (unsafe { IS_GRAPHICS_VER(p, 9, 12) }) && !(unsafe { HAS_FLAT_CCS(p) })
}

/// `has_fenced_regions()`: display parent-interface callback.
///
/// # Safety
/// `drm` must be a live `struct drm_device`.
pub unsafe fn has_fenced_regions(drm: *mut c_void) -> bool {
    let gt = unsafe { to_gt(to_i915(drm).cast::<c_void>()) };
    unsafe { intel_gt_support_legacy_fencing(gt) }
}

/// `vgpu_active()`: display parent-interface callback.
///
/// # Safety
/// `drm` must be a live `struct drm_device`.
pub unsafe fn vgpu_active(drm: *mut c_void) -> bool {
    unsafe { intel_vgpu_active(to_i915(drm)) }
}

/// `i915_driver_parent_interface()`: the display parent-interface table. The
/// table holds pointers to display-owned interface records and the callbacks
/// above, so it is installed by the display provider (`display_parent_interface`).
pub fn i915_driver_parent_interface() -> *const c_void {
    provider::display_parent_interface()
}

/// `i915_driver_create()`: allocates the DRM-embedded private object and
/// sets up the device-info and display state.
///
/// # Safety
/// `pdev` must be a live PCI device; `info` must come from the pciidlist.
unsafe fn i915_driver_create(
    pdev: *mut c_void,
    device: u16,
    match_info: *const IntelDeviceInfo,
) -> Result<*mut DrmI915Private, i32> {
    let mut err: i32 = 0;
    let i915 = unsafe { provider::devm_drm_dev_alloc_i915(pdev, &mut err) };
    if i915.is_null() {
        return Err(err);
    }

    unsafe { pci_set_drvdata(pdev, core::ptr::addr_of_mut!((*i915).drm).cast()) };
    unsafe { provider::i915_params_copy(core::ptr::addr_of_mut!((*i915).params)) };
    unsafe { intel_device_info_driver_create(i915, device, match_info) };

    let display = unsafe { intel_display_device_probe(pdev, i915_driver_parent_interface()) };
    if is_err_ptr(display) {
        return Err(-(display as isize) as i32);
    }
    unsafe { (*i915).display = display };
    Ok(i915)
}

/// `i915_driver_probe()`: the PCI probe chain. `device` is `pdev->device` and
/// `info` is the matched device info.
///
/// # Safety
/// `pdev` must be a live PCI device accepted by `i915_pci_probe`.
pub unsafe fn i915_driver_probe(pdev: *mut c_void, device: u16, info: &IntelDeviceInfo) -> i32 {
    let mut ret = unsafe { pci_enable_device(pdev) };
    if ret != 0 {
        axlog::error!("Failed to enable graphics device: {}", ret);
        return ret;
    }

    let i915 = match unsafe { i915_driver_create(pdev, device, info) } {
        Ok(i915) => i915,
        Err(err) => {
            unsafe { pci_disable_device(pdev) };
            return err;
        }
    };
    let display = unsafe { (*i915).display };

    ret = unsafe { i915_driver_early_probe(i915) };
    if ret < 0 {
        return unsafe { out_pci_disable(pdev, i915, ret) };
    }

    unsafe { disable_rpm_wakeref_asserts(core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<c_void>()) };
    unsafe { intel_vgpu_detect(i915) };

    ret = unsafe { intel_gt_probe_all(i915) };
    if ret < 0 {
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    ret = unsafe { i915_driver_mmio_probe(i915) };
    if ret < 0 {
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    ret = unsafe { i915_driver_hw_probe(i915) };
    if ret < 0 {
        // out_cleanup_mmio
        unsafe { i915_driver_mmio_release(i915) };
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    ret = unsafe { intel_gvt_init(i915) };
    if ret != 0 {
        // out_cleanup_hw
        unsafe { out_cleanup_hw(i915) };
        unsafe { i915_driver_mmio_release(i915) };
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    ret = unsafe { intel_display_driver_probe_noirq(display) };
    if ret < 0 {
        // out_cleanup_gvt
        unsafe { intel_gvt_driver_remove(i915) };
        unsafe { out_cleanup_hw(i915) };
        unsafe { i915_driver_mmio_release(i915) };
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    ret = unsafe { intel_irq_install(i915) };
    if ret != 0 {
        // out_cleanup_modeset
        unsafe { intel_display_driver_remove_nogem(display) };
        unsafe { intel_gvt_driver_remove(i915) };
        unsafe { out_cleanup_hw(i915) };
        unsafe { i915_driver_mmio_release(i915) };
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    ret = unsafe { intel_display_driver_probe_nogem(display) };
    if ret != 0 {
        // out_cleanup_irq
        unsafe { intel_irq_uninstall(i915) };
        unsafe { intel_display_driver_remove_nogem(display) };
        unsafe { intel_gvt_driver_remove(i915) };
        unsafe { out_cleanup_hw(i915) };
        unsafe { i915_driver_mmio_release(i915) };
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    ret = unsafe { i915_gem_init(i915) };
    if ret != 0 {
        // out_cleanup_modeset2
        unsafe { intel_display_driver_remove(display) };
        unsafe { intel_irq_uninstall(i915) };
        unsafe { intel_display_driver_remove_noirq(display) };
        unsafe { intel_display_driver_remove_nogem(display) };
        unsafe { intel_gvt_driver_remove(i915) };
        unsafe { out_cleanup_hw(i915) };
        unsafe { i915_driver_mmio_release(i915) };
        return unsafe { out_runtime_pm_put(pdev, i915, ret) };
    }

    let pxp_ret = unsafe { intel_pxp_init(i915) };
    if pxp_ret != 0 && pxp_ret != -crate::linux_config::ENODEV {
        drm_dbg!(
            core::ptr::addr_of_mut!((*i915).drm).cast::<c_void>(),
            "pxp init failed with {}\n",
            pxp_ret
        );
    }

    ret = unsafe { intel_display_driver_probe(display) };
    if ret != 0 {
        return unsafe { out_cleanup_gem(pdev, i915, display, ret) };
    }

    ret = unsafe { i915_driver_register(i915) };
    if ret != 0 {
        return unsafe { out_cleanup_gem(pdev, i915, display, ret) };
    }

    unsafe { enable_rpm_wakeref_asserts(core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<c_void>()) };
    unsafe { i915_welcome_messages(i915) };
    unsafe { (*i915).do_release = true };
    0
}

/// `out_cleanup_gem:` ... `out_cleanup_hw:` tail of `i915_driver_probe()`
/// after `i915_gem_init()` has succeeded (the chain falls through to the
/// labels in source order).
///
/// # Safety
/// The live `i915` must have completed `i915_gem_init()`.
unsafe fn out_cleanup_gem(
    pdev: *mut c_void,
    i915: *mut DrmI915Private,
    display: *mut c_void,
    ret: i32,
) -> i32 {
    unsafe { intel_pxp_fini(i915) };
    unsafe { i915_gem_suspend(i915) };
    unsafe { i915_gem_driver_remove(i915) };
    unsafe { i915_gem_driver_release(i915) };
    // out_cleanup_modeset2
    unsafe { intel_display_driver_remove(display) };
    unsafe { intel_irq_uninstall(i915) };
    unsafe { intel_display_driver_remove_noirq(display) };
    // out_cleanup_modeset
    unsafe { intel_display_driver_remove_nogem(display) };
    // out_cleanup_gvt
    unsafe { intel_gvt_driver_remove(i915) };
    // out_cleanup_hw
    unsafe { out_cleanup_hw(i915) };
    // out_cleanup_mmio
    unsafe { i915_driver_mmio_release(i915) };
    unsafe { out_runtime_pm_put(pdev, i915, ret) }
}

/// `out_cleanup_hw:` block: hardware teardown after `i915_driver_hw_probe()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`.
unsafe fn out_cleanup_hw(i915: *mut DrmI915Private) {
    unsafe { i915_driver_hw_remove(i915) };
    unsafe { intel_memory_regions_driver_release(i915) };
    unsafe { i915_ggtt_driver_release(i915) };
    unsafe { i915_gem_drain_freed_objects(i915) };
    unsafe { i915_ggtt_driver_late_release(i915) };
}

/// `out_runtime_pm_put:` ... `out_pci_disable:` tail of `i915_driver_probe()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private` whose early probe has run.
unsafe fn out_runtime_pm_put(pdev: *mut c_void, i915: *mut DrmI915Private, ret: i32) -> i32 {
    unsafe { enable_rpm_wakeref_asserts(core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<c_void>()) };
    unsafe { i915_driver_late_release(i915) };
    unsafe { out_pci_disable(pdev, i915, ret) }
}

/// `out_pci_disable:` tail: disables the PCI device and reports the error.
///
/// # Safety
/// `pdev` must be a live PCI device and `i915` a live private object.
unsafe fn out_pci_disable(pdev: *mut c_void, i915: *mut DrmI915Private, ret: i32) -> i32 {
    unsafe { pci_disable_device(pdev) };
    i915_probe_error(
        i915,
        c"Device initialization failed (%d)\n".as_ptr(),
        ret,
    );
    ret
}

/// `i915_driver_remove()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private` that was probed successfully.
pub unsafe fn i915_driver_remove(i915: *mut DrmI915Private) {
    let display = unsafe { (*i915).display };
    let rpm = core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<IntelRuntimePm>();

    let wakeref = unsafe { intel_runtime_pm_get(rpm) };
    unsafe { i915_driver_unregister(i915) };
    unsafe { synchronize_rcu() };
    unsafe { i915_gem_suspend(i915) };
    unsafe { intel_gvt_driver_remove(i915) };
    unsafe { intel_display_driver_remove(display) };
    unsafe { intel_irq_uninstall(i915) };
    unsafe { intel_hpd_cancel_work(display) };
    unsafe { intel_display_driver_remove_noirq(display) };
    unsafe { i915_reset_error_state(i915) };
    unsafe { i915_gem_driver_remove(i915) };
    unsafe { intel_display_driver_remove_nogem(display) };
    unsafe { i915_driver_hw_remove(i915) };
    unsafe { intel_runtime_pm_put(rpm, wakeref) };
}

/// `i915_driver_release()`: DRM release callback.
///
/// # Safety
/// `dev` must be the live `struct drm_device` of an i915 instance.
pub unsafe fn i915_driver_release(dev: *mut c_void) {
    let dev_priv = unsafe { to_i915(dev) };
    let rpm = core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast::<IntelRuntimePm>();

    if unsafe { (*dev_priv).do_release } == false {
        return;
    }

    let wakeref = unsafe { intel_runtime_pm_get(rpm) };
    unsafe { i915_gem_driver_release(dev_priv) };
    unsafe { intel_memory_regions_driver_release(dev_priv) };
    unsafe { i915_ggtt_driver_release(dev_priv) };
    unsafe { i915_gem_drain_freed_objects(dev_priv) };
    unsafe { i915_ggtt_driver_late_release(dev_priv) };
    unsafe { i915_driver_mmio_release(dev_priv) };
    unsafe { intel_runtime_pm_put(rpm, wakeref) };
    unsafe { intel_runtime_pm_driver_release(rpm) };
    unsafe { i915_driver_late_release(dev_priv) };
}

/// `i915_driver_open()`: DRM open callback.
///
/// # Safety
/// `dev` and `file` must be live DRM objects.
pub unsafe fn i915_driver_open(dev: *mut c_void, file: *mut c_void) -> i32 {
    let i915 = unsafe { to_i915(dev) };
    let ret = unsafe { i915_gem_open(i915, file) };
    if ret != 0 {
        return ret;
    }
    0
}

/// `i915_driver_postclose()`: DRM postclose callback.
///
/// # Safety
/// `dev` and `file` must be live DRM objects.
pub unsafe fn i915_driver_postclose(dev: *mut c_void, file: *mut c_void) {
    let client = unsafe { provider::file_client(file) };
    unsafe { i915_gem_context_close(file.cast()) };
    unsafe { i915_drm_client_put(client.cast()) };
    unsafe { provider::kfree_rcu_file_priv(file) };
    unsafe { i915_gem_flush_free_objects(to_i915(dev)) };
}

/// `i915_driver_shutdown()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private` that was probed successfully.
pub unsafe fn i915_driver_shutdown(i915: *mut DrmI915Private) {
    let display = unsafe { (*i915).display };
    let rpm = core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<c_void>();
    let drm = core::ptr::addr_of_mut!((*i915).drm).cast::<c_void>();

    unsafe { disable_rpm_wakeref_asserts(rpm) };
    unsafe { intel_runtime_pm_disable(rpm.cast()) };
    unsafe { intel_display_power_disable(display) };
    unsafe { drm_client_dev_suspend(drm) };
    if unsafe { intel_display_device_present(display) } {
        unsafe { drm_kms_helper_poll_disable(drm) };
        unsafe { intel_display_driver_disable_user_access(display) };
        unsafe { drm_atomic_helper_shutdown(drm) };
    }
    unsafe { intel_dp_mst_suspend(display) };
    unsafe { intel_irq_suspend(i915) };
    unsafe { intel_hpd_cancel_work(display) };
    if unsafe { intel_display_device_present(display) } {
        unsafe { intel_display_driver_suspend_access(display) };
    }
    unsafe { intel_encoder_suspend_all(display) };
    unsafe { intel_encoder_shutdown_all(display) };
    unsafe { intel_dmc_suspend(display) };
    unsafe { i915_gem_suspend(i915) };
    unsafe { intel_display_power_driver_remove(display) };
    unsafe { enable_rpm_wakeref_asserts(rpm) };
    unsafe { intel_runtime_pm_driver_last_release(rpm.cast()) };
}

/// `DRM_SWITCH_POWER_OFF` (enum switch_power_state, drm_device.h).
const DRM_SWITCH_POWER_OFF: i32 = 1;
/// `PCI_D0`, `PCI_D1`, `PCI_D3hot`, `PCI_D3cold` (enum pci_power_t).
const PCI_D0: u32 = 0;
const PCI_D1: u32 = 1;
const PCI_D3HOT: u32 = 3;
const PCI_D3COLD: u32 = 4;
/// `PM_EVENT_FREEZE`, `PM_EVENT_SUSPEND` (linux/pm.h).
const PM_EVENT_FREEZE: i32 = 0x0001;
const PM_EVENT_SUSPEND: i32 = 0x0002;
/// `ACPI_STATE_S3` (acpi/actypes.h).
const ACPI_STATE_S3: u32 = 3;

/// `kdev_to_i915(kdev)` = `to_i915(dev_get_drvdata(kdev))`.
///
/// # Safety
/// `kdev` must be the live struct device bound to i915.
unsafe fn kdev_to_i915(kdev: *mut c_void) -> *mut DrmI915Private {
    unsafe { to_i915(dev_get_drvdata(kdev)) }
}

/// `suspend_to_idle()`: CONFIG_ACPI_SLEEP is enabled in the configured build.
///
/// # Safety
/// Calls the ACPI target-state service; no device state is accessed.
unsafe fn suspend_to_idle(_dev_priv: *mut DrmI915Private) -> bool {
    let state = unsafe { acpi_target_system_state() };
    state < ACPI_STATE_S3
}

/// `i915_drm_complete()`.
///
/// # Safety
/// `dev` must be the live i915 DRM device.
unsafe fn i915_drm_complete(dev: *mut c_void) {
    let i915 = unsafe { to_i915(dev) };
    unsafe { intel_pxp_resume_complete((*i915).pxp) };
}

/// `i915_drm_prepare()`.
///
/// # Safety
/// `dev` must be the live i915 DRM device.
unsafe fn i915_drm_prepare(dev: *mut c_void) -> i32 {
    let i915 = unsafe { to_i915(dev) };
    unsafe { intel_pxp_suspend_prepare((*i915).pxp) };
    unsafe { i915_gem_backup_suspend(i915) }
}

/// `i915_drm_suspend()`.
///
/// # Safety
/// `dev` must be the live i915 DRM device.
unsafe fn i915_drm_suspend(dev: *mut c_void) -> i32 {
    let dev_priv = unsafe { to_i915(dev) };
    let display = unsafe { (*dev_priv).display };
    let rpm = core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast::<c_void>();

    unsafe { disable_rpm_wakeref_asserts(rpm) };
    unsafe { intel_display_power_disable(display) };
    unsafe { drm_client_dev_suspend(dev) };
    if unsafe { intel_display_device_present(display) } {
        unsafe { drm_kms_helper_poll_disable(dev) };
        unsafe { intel_display_driver_disable_user_access(display) };
    }
    unsafe { intel_display_driver_suspend(display) };
    unsafe { intel_irq_suspend(dev_priv) };
    unsafe { intel_hpd_cancel_work(display) };
    if unsafe { intel_display_device_present(display) } {
        unsafe { intel_display_driver_suspend_access(display) };
    }
    unsafe { intel_encoder_suspend_all(display) };
    unsafe { intel_dpt_suspend(display) };
    unsafe { i915_ggtt_suspend((*to_gt(dev_priv.cast::<c_void>())).ggtt.cast()) };
    unsafe { i9xx_display_sr_save(display) };
    let opregion_target_state = if unsafe { suspend_to_idle(dev_priv) } { PCI_D1 } else { PCI_D3COLD };
    unsafe { intel_opregion_suspend(display, opregion_target_state) };
    unsafe { (*dev_priv).suspend_count += 1 };
    unsafe { intel_dmc_suspend(display) };
    unsafe { enable_rpm_wakeref_asserts(rpm) };
    unsafe { i915_gem_drain_freed_objects(dev_priv) };
    0
}

/// `i915_drm_suspend_late()`.
///
/// # Safety
/// `dev` must be the live i915 DRM device.
unsafe fn i915_drm_suspend_late(dev: *mut c_void, hibernation: bool) -> i32 {
    let dev_priv = unsafe { to_i915(dev) };
    let display = unsafe { (*dev_priv).display };
    let rpm = core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast::<IntelRuntimePm>();
    let s2idle = !hibernation && unsafe { suspend_to_idle(dev_priv) };

    unsafe { disable_rpm_wakeref_asserts(rpm.cast::<c_void>()) };
    unsafe { intel_pxp_suspend((*dev_priv).pxp) };
    unsafe { i915_gem_suspend_late(dev_priv) };
    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_uncore_suspend((*gt).uncore) };
        }
    }
    unsafe { intel_display_power_suspend_late(display, s2idle) };

    let ret = unsafe { vlv_suspend_complete(dev_priv) };
    if ret != 0 {
        drm_err!(dev_priv, "Suspend complete failed: {}\n", ret);
        unsafe { intel_display_power_resume_early(display) };
    }

    unsafe { enable_rpm_wakeref_asserts(rpm.cast::<c_void>()) };
    if unsafe { (*uncore_of(dev_priv)).user_forcewake_count } == 0 {
        unsafe { intel_runtime_pm_driver_release(rpm) };
    }
    ret
}

/// `i915_drm_suspend_noirq()`.
///
/// # Safety
/// `dev` must be the live i915 DRM device.
unsafe fn i915_drm_suspend_noirq(dev: *mut c_void, hibernation: bool) -> i32 {
    let dev_priv = unsafe { to_i915(dev) };
    let pdev = unsafe { to_pci_dev((*dev_priv).drm.dev) };
    if hibernation && unsafe { GRAPHICS_VER(dev_priv.cast::<c_void>()) } < 6 {
        unsafe { pci_save_state(pdev) };
    }
    0
}

/// `i915_driver_suspend_switcheroo()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`.
pub unsafe fn i915_driver_suspend_switcheroo(i915: *mut DrmI915Private, event: i32) -> i32 {
    let pdev = unsafe { to_pci_dev((*i915).drm.dev) };
    let drm = core::ptr::addr_of_mut!((*i915).drm).cast::<c_void>();

    if event != PM_EVENT_SUSPEND && event != PM_EVENT_FREEZE {
        axlog::warn!("i915_driver_suspend_switcheroo: unexpected PM event {}", event);
        return -crate::linux_config::EINVAL;
    }

    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }

    let mut error = unsafe { i915_drm_suspend(drm) };
    if error != 0 {
        return error;
    }

    error = unsafe { i915_drm_suspend_late(drm, false) };
    if error != 0 {
        return error;
    }

    unsafe { pci_save_state(pdev) };
    unsafe { pci_set_power_state(pdev, PCI_D3HOT) };
    0
}

/// `i915_drm_resume()`.
///
/// # Safety
/// `dev` must be the live i915 DRM device.
unsafe fn i915_drm_resume(dev: *mut c_void) -> i32 {
    let dev_priv = unsafe { to_i915(dev) };
    let display = unsafe { (*dev_priv).display };
    let rpm = core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast::<c_void>();
    let p = dev_priv.cast::<c_void>();

    unsafe { disable_rpm_wakeref_asserts(rpm) };
    let mut ret = unsafe { i915_pcode_init(dev_priv) };
    if ret != 0 {
        return ret;
    }

    unsafe { sanitize_gpu(dev_priv) };

    ret = unsafe { i915_ggtt_enable_hw(dev_priv) };
    if ret != 0 {
        drm_err!(dev_priv, "failed to re-enable GGTT\n");
    }

    unsafe { i915_ggtt_resume((*to_gt(p)).ggtt.cast()) };

    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() && unsafe { GRAPHICS_VER((*gt).i915.cast::<c_void>()) } >= 8 {
            unsafe { setup_private_pat(gt) };
        }
    }

    unsafe { intel_dpt_resume(display) };
    unsafe { intel_dmc_resume(display) };
    unsafe { i9xx_display_sr_restore(display) };
    unsafe { intel_gmbus_reset(display) };
    unsafe { intel_pps_unlock_regs_wa(display) };
    unsafe { intel_init_pch_refclk(display) };
    unsafe { intel_irq_resume(dev_priv) };
    if unsafe { intel_display_device_present(display) } {
        unsafe { drm_mode_config_reset(dev) };
    }
    unsafe { i915_gem_resume(dev_priv) };
    unsafe { intel_display_driver_init_hw(display) };
    unsafe { intel_clock_gating_init(core::ptr::addr_of_mut!((*dev_priv).drm).cast()) };
    if unsafe { intel_display_device_present(display) } {
        unsafe { intel_display_driver_resume_access(display) };
    }
    unsafe { intel_hpd_init(display) };
    unsafe { intel_display_driver_resume(display) };
    if unsafe { intel_display_device_present(display) } {
        unsafe { intel_display_driver_enable_user_access(display) };
        unsafe { drm_kms_helper_poll_enable(dev) };
    }
    unsafe { intel_hpd_poll_disable(display) };
    unsafe { intel_opregion_resume(display) };
    unsafe { drm_client_dev_resume(dev) };
    unsafe { intel_display_power_enable(display) };
    unsafe { intel_gvt_resume(dev_priv) };
    unsafe { enable_rpm_wakeref_asserts(rpm) };
    0
}

/// `i915_drm_resume_early()`.
///
/// # Safety
/// `dev` must be the live i915 DRM device.
unsafe fn i915_drm_resume_early(dev: *mut c_void) -> i32 {
    let dev_priv = unsafe { to_i915(dev) };
    let display = unsafe { (*dev_priv).display };
    let rpm = core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast::<c_void>();

    unsafe { disable_rpm_wakeref_asserts(rpm) };
    let ret = unsafe { vlv_resume_prepare(dev_priv, false) };
    if ret != 0 {
        drm_err!(
            dev_priv,
            "Resume prepare failed: {}, continuing anyway\n",
            ret
        );
    }

    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_gt_resume_early(gt) };
        }
    }

    unsafe { intel_display_power_resume_early(display) };
    unsafe { enable_rpm_wakeref_asserts(rpm) };
    ret
}

/// `i915_driver_resume_switcheroo()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`.
pub unsafe fn i915_driver_resume_switcheroo(i915: *mut DrmI915Private) -> i32 {
    let pdev = unsafe { to_pci_dev((*i915).drm.dev) };
    let drm = core::ptr::addr_of_mut!((*i915).drm).cast::<c_void>();

    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }

    let mut ret = unsafe { pci_set_power_state(pdev, PCI_D0) };
    if ret != 0 {
        return ret;
    }

    unsafe { pci_restore_state(pdev) };

    ret = unsafe { i915_drm_resume_early(drm) };
    if ret != 0 {
        return ret;
    }

    unsafe { i915_drm_resume(drm) }
}

/// Returns `i915` for a `struct device` callback, or `None` when the device is
/// not bound (`!i915` in the C PM callbacks).
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe fn pm_i915(kdev: *mut c_void) -> *mut DrmI915Private {
    unsafe { kdev_to_i915(kdev) }
}

/// `i915_pm_prepare()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_prepare(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if i915.is_null() {
        axlog::error!("DRM not initialized, aborting suspend.");
        return -crate::linux_config::ENODEV;
    }
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_prepare(core::ptr::addr_of_mut!((*i915).drm).cast()) }
}

/// `i915_pm_suspend()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_suspend(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if i915.is_null() {
        axlog::error!("DRM not initialized, aborting suspend.");
        return -crate::linux_config::ENODEV;
    }
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_suspend(core::ptr::addr_of_mut!((*i915).drm).cast()) }
}

/// `i915_pm_suspend_late()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_suspend_late(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_suspend_late(core::ptr::addr_of_mut!((*i915).drm).cast(), false) }
}

/// `i915_pm_suspend_noirq()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_suspend_noirq(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_suspend_noirq(core::ptr::addr_of_mut!((*i915).drm).cast(), false) }
}

/// `i915_pm_poweroff_late()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_poweroff_late(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_suspend_late(core::ptr::addr_of_mut!((*i915).drm).cast(), true) }
}

/// `i915_pm_poweroff_noirq()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_poweroff_noirq(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_suspend_noirq(core::ptr::addr_of_mut!((*i915).drm).cast(), true) }
}

/// `i915_pm_resume_early()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_resume_early(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_resume_early(core::ptr::addr_of_mut!((*i915).drm).cast()) }
}

/// `i915_pm_resume()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_resume(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return 0;
    }
    unsafe { i915_drm_resume(core::ptr::addr_of_mut!((*i915).drm).cast()) }
}

/// `i915_pm_complete()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_complete(kdev: *mut c_void) {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } == DRM_SWITCH_POWER_OFF {
        return;
    }
    unsafe { i915_drm_complete(core::ptr::addr_of_mut!((*i915).drm).cast()) };
}

/// `i915_pm_freeze()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_freeze(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } != DRM_SWITCH_POWER_OFF {
        let ret = unsafe { i915_drm_suspend(core::ptr::addr_of_mut!((*i915).drm).cast()) };
        if ret != 0 {
            return ret;
        }
    }
    let ret = unsafe { i915_gem_freeze(i915) };
    if ret != 0 {
        return ret;
    }
    0
}

/// `i915_pm_freeze_late()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_freeze_late(kdev: *mut c_void) -> i32 {
    let i915 = unsafe { pm_i915(kdev) };
    if unsafe { (*i915).drm.switch_power_state } != DRM_SWITCH_POWER_OFF {
        let ret = unsafe { i915_drm_suspend_late(core::ptr::addr_of_mut!((*i915).drm).cast(), true) };
        if ret != 0 {
            return ret;
        }
    }
    let ret = unsafe { i915_gem_freeze_late(i915) };
    if ret != 0 {
        return ret;
    }
    0
}

/// `i915_pm_thaw_early()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_thaw_early(kdev: *mut c_void) -> i32 {
    unsafe { i915_pm_resume_early(kdev) }
}

/// `i915_pm_thaw()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_thaw(kdev: *mut c_void) -> i32 {
    unsafe { i915_pm_resume(kdev) }
}

/// `i915_pm_restore_early()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_restore_early(kdev: *mut c_void) -> i32 {
    unsafe { i915_pm_resume_early(kdev) }
}

/// `i915_pm_restore()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_restore(kdev: *mut c_void) -> i32 {
    unsafe { i915_pm_resume(kdev) }
}

/// `i915_pm_runtime_suspend()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_runtime_suspend(kdev: *mut c_void) -> i32 {
    let dev_priv = unsafe { kdev_to_i915(kdev) };
    let display = unsafe { (*dev_priv).display };
    let rpm = core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast::<IntelRuntimePm>();
    let pdev = unsafe { to_pci_dev((*dev_priv).drm.dev) };
    let drm = core::ptr::addr_of_mut!((*dev_priv).drm).cast::<c_void>();

    if !unsafe { has_runtime_pm_flag(dev_priv) } {
        axlog::warn!("i915_pm_runtime_suspend: runtime PM is not supported");
        return -crate::linux_config::ENODEV;
    }
    drm_dbg!(drm, "Suspending device\n");
    unsafe { disable_rpm_wakeref_asserts(rpm.cast::<c_void>()) };
    unsafe { i915_gem_runtime_suspend(dev_priv) };
    unsafe { intel_pxp_runtime_suspend((*dev_priv).pxp) };
    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_gt_runtime_suspend(gt) };
        }
    }
    unsafe { intel_irq_suspend(dev_priv) };
    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_uncore_suspend((*gt).uncore) };
        }
    }
    unsafe { intel_display_power_runtime_suspend(display) };

    let ret = unsafe { vlv_suspend_complete(dev_priv) };
    if ret != 0 {
        drm_err!(dev_priv, "Runtime suspend failed, disabling it ({})\n", ret);
        unsafe { intel_uncore_runtime_resume(uncore_of(dev_priv).cast()) };
        unsafe { intel_irq_resume(dev_priv) };
        for id in 0..I915_MAX_GT {
            let gt = unsafe { (*dev_priv).gt[id] };
            if !gt.is_null() {
                unsafe { intel_gt_runtime_resume(gt) };
            }
        }
        unsafe { enable_rpm_wakeref_asserts(rpm.cast::<c_void>()) };
        return ret;
    }

    unsafe { enable_rpm_wakeref_asserts(rpm.cast::<c_void>()) };
    unsafe { intel_runtime_pm_driver_release(rpm) };

    if unsafe { intel_uncore_arm_unclaimed_mmio_detection(uncore_of(dev_priv).cast()) } {
        drm_err!(dev_priv, "Unclaimed access detected prior to suspending\n");
    }

    let root_pdev = unsafe { pcie_find_root_port(pdev) };
    if !root_pdev.is_null() {
        unsafe { pci_d3cold_disable(root_pdev) };
    }

    if unsafe { IS_BROADWELL(dev_priv.cast::<c_void>()) } {
        unsafe { intel_opregion_notify_adapter(display, PCI_D3HOT) };
    } else {
        unsafe { intel_opregion_notify_adapter(display, PCI_D1) };
    }

    unsafe { assert_forcewakes_inactive(uncore_of(dev_priv).cast()) };
    let p = dev_priv.cast::<c_void>();
    if !unsafe { IS_VALLEYVIEW(p) } && !unsafe { IS_CHERRYVIEW(p) } {
        unsafe { intel_hpd_poll_enable(display) };
    }

    drm_dbg!(drm, "Device suspended\n");
    0
}

/// `i915_pm_runtime_resume()`.
///
/// # Safety
/// `kdev` must be a live struct device.
unsafe extern "C" fn i915_pm_runtime_resume(kdev: *mut c_void) -> i32 {
    let dev_priv = unsafe { kdev_to_i915(kdev) };
    let display = unsafe { (*dev_priv).display };
    let rpm = core::ptr::addr_of_mut!((*dev_priv).runtime_pm).cast::<IntelRuntimePm>();
    let pdev = unsafe { to_pci_dev((*dev_priv).drm.dev) };
    let drm = core::ptr::addr_of_mut!((*dev_priv).drm).cast::<c_void>();
    let p = dev_priv.cast::<c_void>();

    if !unsafe { has_runtime_pm_flag(dev_priv) } {
        axlog::warn!("i915_pm_runtime_resume: runtime PM is not supported");
        return -crate::linux_config::ENODEV;
    }
    drm_dbg!(drm, "Resuming device\n");
    if unsafe { atomic_read_rpm_wakeref(rpm) } != 0 {
        axlog::warn!("i915_pm_runtime_resume: wakeref_count not zero");
    }

    unsafe { disable_rpm_wakeref_asserts(rpm.cast::<c_void>()) };
    unsafe { intel_opregion_notify_adapter(display, PCI_D0) };
    let root_pdev = unsafe { pcie_find_root_port(pdev) };
    if !root_pdev.is_null() {
        unsafe { pci_d3cold_enable(root_pdev) };
    }

    if unsafe { intel_uncore_unclaimed_mmio(uncore_of(dev_priv).cast()) } {
        drm_dbg!(drm, "Unclaimed access during suspend, bios?\n");
    }

    unsafe { intel_display_power_runtime_resume(display) };
    let ret = unsafe { vlv_resume_prepare(dev_priv, true) };
    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_uncore_runtime_resume((*gt).uncore) };
        }
    }
    unsafe { intel_irq_resume(dev_priv) };
    for id in 0..I915_MAX_GT {
        let gt = unsafe { (*dev_priv).gt[id] };
        if !gt.is_null() {
            unsafe { intel_gt_runtime_resume(gt) };
        }
    }
    unsafe { intel_pxp_runtime_resume((*dev_priv).pxp) };

    if !unsafe { IS_VALLEYVIEW(p) } && !unsafe { IS_CHERRYVIEW(p) } {
        unsafe { intel_hpd_init(display) };
        unsafe { intel_hpd_poll_disable(display) };
    }

    unsafe { skl_watermark_ipc_update(display) };
    unsafe { enable_rpm_wakeref_asserts(rpm.cast::<c_void>()) };

    if ret != 0 {
        drm_err!(dev_priv, "Runtime resume failed, disabling it ({})\n", ret);
    } else {
        drm_dbg!(drm, "Device resumed\n");
    }
    ret
}

/// `dev_pm_ops i915_pm_ops` (i915_driver.c). Installed on the PCI driver as
/// `.driver.pm`.
#[repr(C)]
pub struct DevPmOps {
    pub prepare: unsafe extern "C" fn(*mut c_void) -> i32,
    pub suspend: unsafe extern "C" fn(*mut c_void) -> i32,
    pub suspend_late: unsafe extern "C" fn(*mut c_void) -> i32,
    pub suspend_noirq: unsafe extern "C" fn(*mut c_void) -> i32,
    pub resume_early: unsafe extern "C" fn(*mut c_void) -> i32,
    pub resume: unsafe extern "C" fn(*mut c_void) -> i32,
    pub complete: unsafe extern "C" fn(*mut c_void),
    pub freeze: unsafe extern "C" fn(*mut c_void) -> i32,
    pub freeze_late: unsafe extern "C" fn(*mut c_void) -> i32,
    pub thaw_early: unsafe extern "C" fn(*mut c_void) -> i32,
    pub thaw: unsafe extern "C" fn(*mut c_void) -> i32,
    pub poweroff: unsafe extern "C" fn(*mut c_void) -> i32,
    pub poweroff_late: unsafe extern "C" fn(*mut c_void) -> i32,
    pub poweroff_noirq: unsafe extern "C" fn(*mut c_void) -> i32,
    pub restore_early: unsafe extern "C" fn(*mut c_void) -> i32,
    pub restore: unsafe extern "C" fn(*mut c_void) -> i32,
    pub runtime_suspend: unsafe extern "C" fn(*mut c_void) -> i32,
    pub runtime_resume: unsafe extern "C" fn(*mut c_void) -> i32,
}
unsafe impl Sync for DevPmOps {}

pub static i915_pm_ops: DevPmOps = DevPmOps {
    prepare: i915_pm_prepare,
    suspend: i915_pm_suspend,
    suspend_late: i915_pm_suspend_late,
    suspend_noirq: i915_pm_suspend_noirq,
    resume_early: i915_pm_resume_early,
    resume: i915_pm_resume,
    complete: i915_pm_complete,
    freeze: i915_pm_freeze,
    freeze_late: i915_pm_freeze_late,
    thaw_early: i915_pm_thaw_early,
    thaw: i915_pm_thaw,
    poweroff: i915_pm_suspend,
    poweroff_late: i915_pm_poweroff_late,
    poweroff_noirq: i915_pm_poweroff_noirq,
    restore_early: i915_pm_restore_early,
    restore: i915_pm_restore,
    runtime_suspend: i915_pm_runtime_suspend,
    runtime_resume: i915_pm_runtime_resume,
};

/// `DRIVER_MAJOR`, `DRIVER_MINOR`, `DRIVER_PATCHLEVEL` (i915_driver.c).
pub const DRIVER_MAJOR: u32 = 1;
pub const DRIVER_MINOR: u32 = 6;
pub const DRIVER_PATCHLEVEL: u32 = 0;

/// `DRIVER_NAME` and `DRIVER_DESC` (i915_drv.h).
pub const DRIVER_NAME: &str = "i915";
pub const DRIVER_DESC: &str = "Intel Graphics";

/// `static const struct drm_driver i915_drm_driver` (i915_driver.c). The
/// DRM-core callbacks are installed through the DRM provider; this record keeps
/// the feature set, identity and the Rust-side callbacks.
pub struct DrmDriverDesc {
    pub name: &'static str,
    pub desc: &'static str,
    pub major: u32,
    pub minor: u32,
    pub patchlevel: u32,
    pub release: unsafe fn(*mut c_void),
    pub open: unsafe fn(*mut c_void, *mut c_void) -> i32,
    pub postclose: unsafe fn(*mut c_void, *mut c_void),
    pub ioctls: &'static [crate::i915_ioctl_upstream::DrmIoctlDesc],
}

/// `DRIVER_GEM | DRIVER_RENDER | DRIVER_MODESET | DRIVER_ATOMIC |
/// DRIVER_SYNCOBJ | DRIVER_SYNCOBJ_TIMELINE`, recorded for the DRM provider.
pub const I915_DRM_DRIVER_FEATURES: u32 = DRIVER_GEM_FLAG
    | DRIVER_RENDER_FLAG
    | DRIVER_MODESET_FLAG
    | DRIVER_ATOMIC_FLAG
    | DRIVER_SYNCOBJ_FLAG
    | DRIVER_SYNCOBJ_TIMELINE_FLAG;
const DRIVER_GEM_FLAG: u32 = 1 << 0;
const DRIVER_RENDER_FLAG: u32 = 1 << 3;
const DRIVER_MODESET_FLAG: u32 = 1 << 1;
const DRIVER_ATOMIC_FLAG: u32 = 1 << 4;
const DRIVER_SYNCOBJ_FLAG: u32 = 1 << 5;
const DRIVER_SYNCOBJ_TIMELINE_FLAG: u32 = 1 << 6;

/// `i915_driver_release` / `i915_driver_open` / `i915_driver_postclose` as
/// the DRM-driver callbacks (wrapped to the `unsafe fn` shape above).
unsafe fn drm_release_cb(dev: *mut c_void) {
    unsafe { i915_driver_release(dev) };
}
unsafe fn drm_open_cb(dev: *mut c_void, file: *mut c_void) -> i32 {
    unsafe { i915_driver_open(dev, file) }
}
unsafe fn drm_postclose_cb(dev: *mut c_void, file: *mut c_void) {
    unsafe { i915_driver_postclose(dev, file) };
}

/// `i915_drm_driver` (i915_driver.c).
pub fn i915_drm_driver() -> DrmDriverDesc {
    DrmDriverDesc {
        name: DRIVER_NAME,
        desc: DRIVER_DESC,
        major: DRIVER_MAJOR,
        minor: DRIVER_MINOR,
        patchlevel: DRIVER_PATCHLEVEL,
        release: drm_release_cb,
        open: drm_open_cb,
        postclose: drm_postclose_cb,
        ioctls: crate::i915_ioctl_upstream::I915_IOCTLS,
    }
}

/// `i915_gem_reject_pin_ioctl()`: DRM_I915_GEM_PIN/UNPIN are rejected.
///
/// # Safety
/// Takes no device state; arguments are ignored as in the C version.
pub unsafe fn i915_gem_reject_pin_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    file: *mut c_void,
) -> c_int {
    let _ = (dev, data, file);
    -crate::linux_config::ENODEV
}
