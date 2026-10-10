// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Installed-provider table for the i915 probe path (Linux v7.2.3
// i915_driver.c / intel_device_info.c / i915_pci.c callees owned by the
// kernel: early GMD_ID peek, display driver, DRM core, PCI probe helpers that
// have no LinuxKPI owner). Calls fail closed when the table is not installed.
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::linux_i915_private::{DrmI915Private, I915Params};

/// Installed-provider table for display and DRM-core callbacks, plus the
/// i915 optional subsystems (debugfs/sysfs, hwmon, pmu, perf, switcheroo,
/// PXP, GVT) that i915_driver.c calls. Each entry has the C signature of its
/// upstream prototype; the table is installed by the display/DRM owner.
#[repr(C)]
pub struct I915FrameworkOps {
    pub drm_atomic_helper_shutdown: unsafe extern "C" fn(*mut c_void),
    pub drm_client_dev_resume: unsafe extern "C" fn(*mut c_void),
    pub drm_client_dev_suspend: unsafe extern "C" fn(*mut c_void),
    pub drm_dev_register: unsafe extern "C" fn(dev: *mut c_void, flags: c_ulong) -> c_int,
    pub drm_dev_unplug: unsafe extern "C" fn(*mut c_void),
    pub drm_dev_unregister: unsafe extern "C" fn(*mut c_void),
    pub drm_kms_helper_poll_disable: unsafe extern "C" fn(*mut c_void),
    pub drm_kms_helper_poll_enable: unsafe extern "C" fn(*mut c_void),
    pub drm_mode_config_reset: unsafe extern "C" fn(*mut c_void),
    pub i915_debugfs_register: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_hwmon_register: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_hwmon_unregister: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_perf_fini: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_perf_init: unsafe extern "C" fn(*mut DrmI915Private) -> c_int,
    pub i915_perf_register: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_perf_unregister: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_pmu_register: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_pmu_unregister: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_setup_sysfs: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_switcheroo_register: unsafe extern "C" fn(*mut DrmI915Private) -> c_int,
    pub i915_switcheroo_unregister: unsafe extern "C" fn(*mut DrmI915Private),
    pub i915_teardown_sysfs: unsafe extern "C" fn(*mut DrmI915Private),
    pub i9xx_display_sr_restore: unsafe extern "C" fn(*mut c_void),
    pub i9xx_display_sr_save: unsafe extern "C" fn(*mut c_void),
    pub intel_bw_init_hw: unsafe extern "C" fn(*mut c_void),
    pub intel_clock_gating_hooks_init: unsafe extern "C" fn(*mut c_void),
    pub intel_display_device_info_runtime_init: unsafe extern "C" fn(*mut c_void),
    pub intel_display_device_present: unsafe extern "C" fn(*mut c_void) -> bool,
    pub intel_display_device_remove: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_disable_user_access: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_early_probe: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_enable_user_access: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_init_hw: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_probe: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub intel_display_driver_probe_nogem: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub intel_display_driver_probe_noirq: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub intel_display_driver_register: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_remove: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_remove_nogem: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_remove_noirq: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_resume: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_resume_access: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_suspend: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub intel_display_driver_suspend_access: unsafe extern "C" fn(*mut c_void),
    pub intel_display_driver_unregister: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_cleanup: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_disable: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_driver_remove: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_enable: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_resume_early: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_runtime_resume: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_runtime_suspend: unsafe extern "C" fn(*mut c_void),
    pub intel_display_power_suspend_late: unsafe extern "C" fn(*mut c_void, bool),
    pub intel_dmc_resume: unsafe extern "C" fn(*mut c_void),
    pub intel_dmc_suspend: unsafe extern "C" fn(*mut c_void),
    pub intel_dp_mst_suspend: unsafe extern "C" fn(*mut c_void),
    pub intel_dpt_resume: unsafe extern "C" fn(*mut c_void),
    pub intel_dpt_suspend: unsafe extern "C" fn(*mut c_void),
    pub intel_dram_detect: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub intel_encoder_shutdown_all: unsafe extern "C" fn(*mut c_void),
    pub intel_encoder_suspend_all: unsafe extern "C" fn(*mut c_void),
    pub intel_gmbus_reset: unsafe extern "C" fn(*mut c_void),
    pub intel_gvt_driver_remove: unsafe extern "C" fn(*mut DrmI915Private),
    pub intel_gvt_init: unsafe extern "C" fn(*mut DrmI915Private) -> c_int,
    pub intel_gvt_resume: unsafe extern "C" fn(*mut DrmI915Private),
    pub intel_hpd_cancel_work: unsafe extern "C" fn(*mut c_void),
    pub intel_hpd_init: unsafe extern "C" fn(*mut c_void),
    pub intel_hpd_poll_disable: unsafe extern "C" fn(*mut c_void),
    pub intel_hpd_poll_enable: unsafe extern "C" fn(*mut c_void),
    pub intel_init_pch_refclk: unsafe extern "C" fn(*mut c_void),
    pub intel_match_g8_cpu: unsafe extern "C" fn() -> bool,
    pub intel_opregion_cleanup: unsafe extern "C" fn(*mut c_void),
    pub intel_opregion_notify_adapter: unsafe extern "C" fn(*mut c_void, u32) -> c_int,
    pub intel_opregion_resume: unsafe extern "C" fn(*mut c_void),
    pub intel_opregion_setup: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub intel_opregion_suspend: unsafe extern "C" fn(*mut c_void, u32),
    pub intel_pps_unlock_regs_wa: unsafe extern "C" fn(*mut c_void),
    pub intel_pxp_debugfs_register: unsafe extern "C" fn(*mut c_void),
    pub intel_pxp_fini: unsafe extern "C" fn(*mut DrmI915Private),
    pub intel_pxp_init: unsafe extern "C" fn(*mut DrmI915Private) -> c_int,
    pub intel_pxp_resume_complete: unsafe extern "C" fn(*mut c_void),
    pub intel_pxp_runtime_resume: unsafe extern "C" fn(*mut c_void),
    pub intel_pxp_runtime_suspend: unsafe extern "C" fn(*mut c_void),
    pub intel_pxp_suspend: unsafe extern "C" fn(*mut c_void),
    pub intel_pxp_suspend_prepare: unsafe extern "C" fn(*mut c_void),
    pub intel_sbi_fini: unsafe extern "C" fn(*mut c_void),
    pub intel_sbi_init: unsafe extern "C" fn(*mut c_void),
    pub skl_watermark_ipc_update: unsafe extern "C" fn(*mut c_void),
    pub devm_drm_dev_alloc_i915: unsafe extern "C" fn(pdev: *mut c_void, err: *mut c_int) -> *mut DrmI915Private,
    pub i915_params_copy: unsafe extern "C" fn(dst: *mut I915Params),
    pub display_parent_interface: unsafe extern "C" fn() -> *const c_void,
    pub display_probe_defer: unsafe extern "C" fn(pdev: *mut c_void) -> bool,
    pub modparam_force_probe: unsafe extern "C" fn() -> *const c_char,
    pub pci_msi_enabled: unsafe extern "C" fn(pdev: *mut c_void) -> bool,
    pub pci_func: unsafe extern "C" fn(pdev: *mut c_void) -> u32,
    pub dma_fence_is_i915: unsafe extern "C" fn(fence: *mut c_void) -> bool,
    pub drm_driver_name: unsafe extern "C" fn(driver: *mut c_void) -> *const c_char,
    pub file_client: unsafe extern "C" fn(file: *mut c_void) -> *mut c_void,
    pub pci_revision: unsafe extern "C" fn(pdev: *mut c_void) -> u8,
    pub kfree_rcu_file_priv: unsafe extern "C" fn(file: *mut c_void),
    pub pci_register_driver: unsafe extern "C" fn(drv: *const c_void) -> c_int,
    pub pci_unregister_driver: unsafe extern "C" fn(drv: *const c_void),
    pub drmm_add_action_or_reset: unsafe extern "C" fn(drm: *mut c_void, action: unsafe extern "C" fn(*mut c_void), data: *mut c_void) -> c_int,
    pub intel_clock_gating_init: unsafe extern "C" fn(dev: *mut c_void),
    pub intel_display_device_probe: unsafe extern "C" fn(pdev: *mut c_void, parent: *const c_void) -> *mut c_void,
}
unsafe impl Sync for I915FrameworkOps {}

static I915_FRAMEWORK_OPS: AtomicPtr<I915FrameworkOps> = AtomicPtr::new(ptr::null_mut());

/// Installs the unique display/DRM framework table. Installation is
/// idempotent for the same table and fails for any other one.
pub fn install_i915_framework_ops(ops: &'static I915FrameworkOps) -> Result<(), &'static str> {
    let candidate = (ops as *const I915FrameworkOps).cast_mut();
    match I915_FRAMEWORK_OPS.compare_exchange(ptr::null_mut(), candidate, Ordering::Release, Ordering::Acquire) {
        Ok(_) => Ok(()),
        Err(existing) if existing == candidate => Ok(()),
        Err(_) => Err("i915 framework operations are already installed"),
    }
}

/// Fail-closed accessor: panics when the owner has not installed the table.
pub fn framework() -> &'static I915FrameworkOps {
    let ops = I915_FRAMEWORK_OPS.load(Ordering::Acquire);
    assert!(
        !ops.is_null(),
        "display/DRM owner must install the i915 framework operations before the probe path runs"
    );
    unsafe { &*ops }
}

/// Manual provider entries used by the probe translation.
pub unsafe fn devm_drm_dev_alloc_i915(pdev: *mut c_void, err: *mut c_int) -> *mut DrmI915Private {
    unsafe { (framework().devm_drm_dev_alloc_i915)(pdev, err) }
}
pub unsafe fn i915_params_copy(dst: *mut I915Params) {
    unsafe { (framework().i915_params_copy)(dst) }
}
pub fn display_parent_interface() -> *const c_void {
    unsafe { (framework().display_parent_interface)() }
}
pub unsafe fn display_probe_defer(pdev: *mut c_void) -> bool {
    unsafe { (framework().display_probe_defer)(pdev) }
}
pub fn modparam_force_probe() -> *const c_char {
    unsafe { (framework().modparam_force_probe)() }
}
pub unsafe fn pci_msi_enabled(pdev: *mut c_void) -> bool {
    unsafe { (framework().pci_msi_enabled)(pdev) }
}
pub unsafe fn pci_func(pdev: *mut c_void) -> u32 {
    unsafe { (framework().pci_func)(pdev) }
}
pub unsafe fn pci_revision(pdev: *mut c_void) -> u8 {
    unsafe { (framework().pci_revision)(pdev) }
}
pub unsafe fn dma_fence_is_i915(fence: *mut c_void) -> bool {
    unsafe { (framework().dma_fence_is_i915)(fence) }
}
pub unsafe fn drm_driver_name(driver: *mut c_void) -> *const c_char {
    unsafe { (framework().drm_driver_name)(driver) }
}
pub unsafe fn file_client(file: *mut c_void) -> *mut c_void {
    unsafe { (framework().file_client)(file) }
}
pub unsafe fn kfree_rcu_file_priv(file: *mut c_void) {
    unsafe { (framework().kfree_rcu_file_priv)(file) }
}
pub unsafe fn pci_register_driver(drv: *const c_void) -> c_int {
    unsafe { (framework().pci_register_driver)(drv) }
}
pub unsafe fn pci_unregister_driver(drv: *const c_void) {
    unsafe { (framework().pci_unregister_driver)(drv) }
}

/// Kernel-owned callbacks used by the i915 probe translation.
#[repr(C)]
pub struct I915ProbeOps {
    /// Reads one 32-bit register through BAR0 before MMIO is mapped
    /// (`pci_iomap_range(pdev, 0, offset, 4)` + `ioread32()`). Returns false
    /// when the mapping fails.
    pub early_gmd_read:
        unsafe extern "C" fn(i915: *mut DrmI915Private, offset: u32, value: *mut u32) -> bool,
}

static I915_PROBE_OPS: AtomicPtr<I915ProbeOps> = AtomicPtr::new(ptr::null_mut());

/// Installs the unique kernel-owned probe operations table. It must have
/// static lifetime and remain unchanged while the feature is active.
pub fn install_i915_probe_ops(ops: &'static I915ProbeOps) -> Result<(), &'static str> {
    let candidate = (ops as *const I915ProbeOps).cast_mut();
    match I915_PROBE_OPS.compare_exchange(
        ptr::null_mut(),
        candidate,
        Ordering::Release,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(existing) if existing == candidate => Ok(()),
        Err(_) => Err("i915 probe operations are already installed"),
    }
}

fn ops() -> &'static I915ProbeOps {
    let ops = I915_PROBE_OPS.load(Ordering::Acquire);
    assert!(
        !ops.is_null(),
        "kernel owners must install i915 probe operations before the probe path runs"
    );
    unsafe { &*ops }
}

/// Source `ip_ver_read()` register access; see `I915ProbeOps::early_gmd_read`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`; `value` must be writable.
pub unsafe fn early_gmd_read(i915: *mut DrmI915Private, offset: u32, value: *mut u32) -> bool {
    assert!(!i915.is_null() && !value.is_null());
    unsafe { (ops().early_gmd_read)(i915, offset, value) }
}

#[allow(dead_code)]
fn _provider_is_sized(_: *mut c_void) {}
