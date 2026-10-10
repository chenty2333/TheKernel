// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Kernel providers for the i915 framework, probe and DRM-core tables of
//! `tk-intel-gt` (`intel-upstream-gt`).
//!
//! [`install`] installs three tables:
//!
//! - `I915FrameworkOps`: the display owner split, DRM device binding, PCI
//!   identity queries, the parameter defaults, and the optional subsystems
//!   that the configured TheKernel build does not compile in.
//! - `I915ProbeOps`: the BAR0 register peek used before the probe maps MMIO.
//! - `DrmCoreProvider`: the DRM object entry points. Object calls that need a
//!   kernel `DrmFile`, GEM handle, dma-buf, sync_file or syncobj fail closed
//!   (see the comment on each). They are reachable only through ioctl or
//!   export paths that the ioctl owner has not bound yet.
//!
//! Display callbacks belong to the native display owner
//! (`kernel/src/drm/intel/{fastboot,native_*,hpd,gmbus,dmc,pipe}.rs`), which is
//! initialized before the upstream probe runs. Upstream display code is never
//! run, so those callbacks record the owner split and return the value the
//! upstream caller expects from a display that is already initialized.
//!
//! Nothing here is reachable from the default build: the module declaration in
//! `mod.rs` is gated on the `intel-upstream-gt` feature.

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use intel_gt::{
    i915_gem_context_types_upstream::DrmI915Private,
    i915_probe_provider_upstream::{
        I915FrameworkOps, I915ProbeOps, install_i915_framework_ops, install_i915_probe_ops,
    },
    intel_context_upstream::{DmaFence, DrmGemObjectBaseLayout, Kref},
    linux::drm_core::{DrmCoreProvider, install_drm_core_provider},
};

use super::{pci, upstream_gt};

#[cfg(target_os = "none")]
use super::pci::{ConfigSpace, Ecam};

const ENODEV: c_int = 19;
const EBUSY: c_int = 16;
const EINVAL: c_int = 22;
const ENOMEM: c_int = 12;
const GFP_KERNEL: u32 = 0x0cc0;
/// `__GFP_ZERO` (include/linux/gfp_types.h); honoured by `kmalloc`.
const GFP_ZERO: u32 = 0x0100;

unsafe extern "C" {
    /// Linux `kmalloc()`, exported under its C name by `tk-intel-gt`.
    fn kmalloc(size: usize, flags: u32) -> *mut c_void;
    /// Linux `dma_resv_init()`, exported under its C name by `tk-intel-gt`.
    fn dma_resv_init(resv: *mut c_void);
    /// Linux `drmm_add_action_or_reset()`, exported by `tk-intel-gt`.
    fn drmm_add_action_or_reset(
        drm: *mut c_void,
        action: unsafe extern "C" fn(*mut c_void, *mut c_void),
        data: *mut c_void,
    ) -> c_int;
}

/// `ERR_PTR(errno)`.
#[inline]
fn err_ptr(errno: c_int) -> *mut c_void {
    (errno as isize) as *mut c_void
}

// ---------------------------------------------------------------------------
// DRM device binding
// ---------------------------------------------------------------------------

/// The upstream `drm_device` attached to the kernel's native Intel DRM device.
/// TheKernel registers one primary device (`crate::drm::primary_device`); the
/// upstream i915 is attached to it and is never registered as a second device.
static BOUND_DRM: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
/// Set by `drm_dev_unplug()`. `drm_dev_enter()` then reports the device gone.
static UNPLUGGED: AtomicBool = AtomicBool::new(false);

/// `drm_dev_register()`: bind the upstream device to the native primary device.
/// The ioctl dispatch stays with the native device; this makes the upstream
/// `drm_device` identity known to the DRM core callbacks.
unsafe extern "C" fn drm_dev_register(dev: *mut c_void, _flags: c_ulong) -> c_int {
    if dev.is_null() {
        return -EINVAL;
    }
    if crate::drm::primary_device().is_none() {
        axlog::error!("upstream i915: no native primary DRM device to attach to");
        return -ENODEV;
    }
    match BOUND_DRM.compare_exchange(ptr::null_mut(), dev, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            UNPLUGGED.store(false, Ordering::Release);
            0
        }
        Err(existing) if existing == dev => 0,
        Err(_) => -EBUSY,
    }
}

/// `drm_dev_unregister()`: detach the upstream device from the native device.
unsafe extern "C" fn drm_dev_unregister(dev: *mut c_void) {
    let _ = BOUND_DRM.compare_exchange(dev, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
}

/// `drm_dev_unplug()`: the device is gone for new `drm_dev_enter()` callers.
unsafe extern "C" fn drm_dev_unplug(_dev: *mut c_void) {
    UNPLUGGED.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Owner split: display, optional subsystems and other native-owned services
// ---------------------------------------------------------------------------

// Each group below is named for the reason its callbacks do nothing:
// - `owned_by_native_*`: the native owner already performs the step on the
//   display path, so the upstream call has nothing left to do.
// - `not_configured_*`: the configured Linux build compiles the subsystem out,
//   and the Linux stub does nothing or returns success.
// - `pxp_*`, `not_dg2_cpu`: the feature is absent on the ADL-N target, and the
//   Linux code reports that absence through its own return value.

/// Native display owner: power domains, HPD, GMBUS, DMC, PPS, PCH SBI, DPT,
/// encoder and pipe state, clock gating, watermark and bandwidth setup, and the
/// display user-access and lifecycle steps. These run on the native path in
/// `kernel/src/drm/intel/`, which is initialized before this callback runs.
unsafe extern "C" fn owned_by_native_display(_display: *mut c_void) {}

/// Native display owner, for callbacks that return success to the upstream
/// caller (display probe, display suspend, OpRegion setup).
unsafe extern "C" fn owned_by_native_display_ok(_display: *mut c_void) -> c_int {
    0
}

/// Native display owner: the panel is present on the native path.
unsafe extern "C" fn native_display_present(_display: *mut c_void) -> bool {
    true
}

/// `intel_opregion_suspend()`: TheKernel has no ACPI OpRegion, and the native
/// owner suspends its own state.
unsafe extern "C" fn no_opregion_suspend(_display: *mut c_void, _state: u32) {}

/// `intel_opregion_notify_adapter()`: no OpRegion means no adapter to notify.
/// The upstream caller treats 0 as "nothing to do".
unsafe extern "C" fn no_opregion_notify(_display: *mut c_void, _state: u32) -> c_int {
    0
}

/// `intel_display_power_suspend_late()`: the native power owner handles late
/// suspend, and it makes the keep-powered decision on its own path.
unsafe extern "C" fn owned_by_native_display_suspend_late(_display: *mut c_void, _keep: bool) {}

/// `intel_match_g8_cpu()`: selects a DG2 workaround for specific CPU steppings.
/// ADL-N is not DG2, so the workaround is never selected.
unsafe extern "C" fn not_dg2_cpu() -> bool {
    false
}

/// Debugfs, sysfs, hwmon, PMU, perf register/unregister, switcheroo and GVT
/// hooks. CONFIG_DEBUG_FS, CONFIG_HWMON, CONFIG_PERF_EVENTS,
/// CONFIG_VGA_SWITCHEROO and CONFIG_DRM_I915_GVT are not configured, so the
/// Linux versions of these hooks are empty.
unsafe extern "C" fn not_configured_i915(_i915: *mut DrmI915Private) {}

/// `i915_perf_init()`, `i915_switcheroo_register()`, `intel_gvt_init()`: the
/// Linux stubs for the unconfigured subsystems return 0.
unsafe extern "C" fn not_configured_ok(_i915: *mut DrmI915Private) -> c_int {
    0
}

/// `intel_pxp_init()`: PXP and GSC are outside the supported ADL-N path. Linux
/// returns `-ENODEV` when no GT provides PXP or a tee link, and the probe
/// continues without PXP.
unsafe extern "C" fn pxp_not_on_platform(_i915: *mut DrmI915Private) -> c_int {
    -ENODEV
}

/// `intel_pxp_fini()` and the PXP runtime and suspend hooks. Without PXP there
/// is no PXP state to tear down or to suspend and resume.
unsafe extern "C" fn pxp_absent(_i915: *mut DrmI915Private) {}

/// `intel_dram_detect()`: the native display path reads the DRAM parameters it
/// needs for watermark and bandwidth setup, so the upstream detect step
/// succeeds.
unsafe extern "C" fn native_dram_detect(_display: *mut c_void) -> c_int {
    0
}

/// The display object the upstream probe stores as `i915->display`. The native
/// display owner is the only display in this kernel, and every upstream display
/// callback that receives this pointer is native-owned and does not
/// dereference it. The token is a static address: non-null, and not an
/// `ERR_PTR` value.
static NATIVE_DISPLAY_OWNER: u8 = 0;

/// `intel_display_device_probe()`: returns the native display token.
unsafe extern "C" fn native_display_device_probe(
    _pdev: *mut c_void,
    _parent: *const c_void,
) -> *mut c_void {
    ptr::addr_of!(NATIVE_DISPLAY_OWNER).cast_mut().cast()
}

/// `display_parent_interface()`: the table that upstream display code would use
/// to reach parent services. The native owner never runs upstream display code,
/// so there is no reader, and NULL is what the table reports.
unsafe extern "C" fn no_display_parent_interface() -> *const c_void {
    ptr::null()
}

/// `display_probe_defer()`: the native display is brought up with the GT probe
/// on the rootfs callback, so the upstream probe never has to defer.
unsafe extern "C" fn display_never_defers(_pdev: *mut c_void) -> bool {
    false
}

// ---------------------------------------------------------------------------
// PCI identity
// ---------------------------------------------------------------------------

/// Reads through the ECAM window, mapped on first use. `None` means the window
/// is not available. Host builds have no ECAM.
#[cfg(target_os = "none")]
fn with_ecam<R>(f: impl FnOnce(&mut Ecam) -> Option<R>) -> Option<R> {
    static ECAM: axsync::Mutex<Option<Ecam>> = axsync::Mutex::new(None);
    let mut guard = ECAM.lock();
    if guard.is_none() {
        *guard = Ecam::platform();
    }
    f(guard.as_mut()?)
}

#[cfg(target_os = "none")]
fn config_u8(bdf: pci::Bdf, offset: u16) -> Option<u8> {
    with_ecam(|ecam| ecam.read_u8(bdf, offset))
}

#[cfg(target_os = "none")]
fn config_u16(bdf: pci::Bdf, offset: u16) -> Option<u16> {
    with_ecam(|ecam| ecam.read_u16(bdf, offset))
}

#[cfg(not(target_os = "none"))]
fn config_u8(_bdf: pci::Bdf, _offset: u16) -> Option<u8> {
    None
}

#[cfg(not(target_os = "none"))]
fn config_u16(_bdf: pci::Bdf, _offset: u16) -> Option<u16> {
    None
}

/// The native PCI identity behind a `pdev` pointer. Every `pdev` the upstream
/// probe passes was created by `upstream_gt::native_pci_device()`.
fn bdf_of(pdev: *mut c_void) -> pci::Bdf {
    upstream_gt::bdf_of(pdev).expect("upstream PCI call: pdev is not a native PCI device")
}

/// Walks the capability list for MSI (ID 0x05) and reads its enable bit. This
/// is the hardware state that Linux keeps as `pdev->msi_enabled`.
fn msi_enabled_in_hardware(bdf: pci::Bdf) -> Option<bool> {
    const STATUS: u16 = 0x06;
    const CAP_LIST: u16 = 1 << 4;
    const CAP_POINTER: u16 = 0x34;
    const CAP_MSI: u8 = 0x05;
    const MSI_ENABLE: u16 = 1;

    if config_u16(bdf, STATUS)? & CAP_LIST == 0 {
        return Some(false);
    }
    let mut pointer = config_u8(bdf, CAP_POINTER)? & !3;
    // A conventional capability list has at most 48 entries.
    for _ in 0..48 {
        if pointer == 0 {
            break;
        }
        let id = config_u8(bdf, u16::from(pointer))?;
        if id == CAP_MSI {
            return Some(config_u16(bdf, u16::from(pointer) + 2)? & MSI_ENABLE != 0);
        }
        pointer = config_u8(bdf, u16::from(pointer) + 1)? & !3;
    }
    Some(false)
}

/// `pci_msi_enabled()`: whether the device's MSI is enabled. An unreadable
/// config space means the platform has no ECAM, which the upstream code cannot
/// recover from, so it stops here.
unsafe extern "C" fn pci_msi_enabled(pdev: *mut c_void) -> bool {
    msi_enabled_in_hardware(bdf_of(pdev)).expect("pci_msi_enabled: ECAM config space unavailable")
}

/// `pci_func()`: the PCI function number of `pdev`.
unsafe extern "C" fn pci_func(pdev: *mut c_void) -> u32 {
    u32::from(bdf_of(pdev).function)
}

/// `pci_revision()`: the revision ID at config offset 0x08.
unsafe extern "C" fn pci_revision(pdev: *mut c_void) -> u8 {
    config_u8(bdf_of(pdev), 0x08).expect("pci_revision: ECAM config space unavailable")
}

/// The PCI driver recorded by `pci_register_driver()`. TheKernel's native boot
/// calls `i915_pci_probe()` directly and does not walk a PCI driver table, so
/// registration records the driver pointer and reports success. A second,
/// different driver is refused.
static REGISTERED_PCI_DRIVER: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// `pci_register_driver()`.
unsafe extern "C" fn pci_register_driver(drv: *const c_void) -> c_int {
    if drv.is_null() {
        return -EINVAL;
    }
    let drv = drv.cast_mut();
    match REGISTERED_PCI_DRIVER.compare_exchange(
        ptr::null_mut(),
        drv,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => 0,
        Err(existing) if existing == drv => 0,
        Err(_) => -EBUSY,
    }
}

/// `pci_unregister_driver()`: forget the recorded driver pointer.
unsafe extern "C" fn pci_unregister_driver(drv: *const c_void) {
    let _ = REGISTERED_PCI_DRIVER.compare_exchange(
        drv.cast_mut(),
        ptr::null_mut(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

// ---------------------------------------------------------------------------
// Driver, parameters, fences and file helpers
// ---------------------------------------------------------------------------

/// The name of `i915_drm_driver`, as `driver->name`.
static I915_DRIVER_NAME: &[u8] = b"i915\0";

/// `drm_driver_name()`.
unsafe extern "C" fn drm_driver_name(_driver: *mut c_void) -> *const c_char {
    I915_DRIVER_NAME.as_ptr().cast()
}

/// The `i915.force_probe` default. Kconfig leaves `DRM_I915_FORCE_PROBE` empty,
/// so the string is "" and no device is forced.
static EMPTY_FORCE_PROBE: &[u8] = b"\0";

/// `modparam_force_probe()`.
unsafe extern "C" fn modparam_force_probe() -> *const c_char {
    EMPTY_FORCE_PROBE.as_ptr().cast()
}

/// `devm_drm_dev_alloc_i915()`: allocate the zeroed `drm_i915_private` and point
/// its DRM device at the native PCI identity, as `devm_drm_dev_alloc()` does
/// with the parent device.
unsafe extern "C" fn devm_drm_dev_alloc_i915(pdev: *mut c_void, err: *mut c_int) -> *mut DrmI915Private {
    let size = size_of::<DrmI915Private>();
    // SAFETY: `kmalloc` with `__GFP_ZERO` returns `size` zeroed bytes or NULL;
    // the cast is to the allocation's own type.
    let i915 = unsafe { kmalloc(size, GFP_KERNEL | GFP_ZERO) }.cast::<DrmI915Private>();
    if i915.is_null() {
        if !err.is_null() {
            // SAFETY: the caller passes a writable `int` for the error code.
            unsafe { *err = -ENOMEM };
        }
        return ptr::null_mut();
    }
    // SAFETY: `i915` is a fresh, zeroed, exclusively owned allocation of
    // `DrmI915Private`, and `pdev` is the native PCI identity the caller owns.
    unsafe {
        (*i915).drm.dev = pdev;
        (*i915).drm.dma_dev = pdev;
    }
    i915
}

/// Field-for-field view of `struct i915_params` (`i915_params.h`). The owning
/// module is private to `tk-intel-gt`, so the layout is named here.
#[repr(C)]
struct I915ParamsLayout {
    modeset: i32,
    enable_guc: i32,
    guc_log_level: i32,
    guc_firmware_path: *mut c_char,
    huc_firmware_path: *mut c_char,
    gsc_firmware_path: *mut c_char,
    memtest: bool,
    mmio_debug: i32,
    reset: u32,
    force_probe: *mut c_char,
    request_timeout_ms: u32,
    lmem_size: u32,
    lmem_bar_size: u32,
    enable_hangcheck: bool,
    error_capture: bool,
    enable_gvt: bool,
    enable_debug_only_api: bool,
}

/// `i915_params_copy()`: copy the module-parameter defaults. TheKernel sets no
/// module parameters, so the values are the `i915_params.h` defaults for this
/// Kconfig. `force_probe` is duplicated, as `_param_dup_charp()` does, so that
/// `i915_params_free()` can release it.
unsafe extern "C" fn i915_params_copy(dst: *mut c_void) {
    let dst = dst.cast::<I915ParamsLayout>();
    // SAFETY: the duplicate is a fresh one-byte zeroed allocation, so the empty
    // string is NUL-terminated.
    let force_probe = unsafe { kmalloc(1, GFP_KERNEL | GFP_ZERO) }.cast::<c_char>();
    if force_probe.is_null() {
        panic!("i915_params_copy: out of memory duplicating force_probe");
    }
    // SAFETY: `dst` is the caller's `struct i915_params`, which the probe owns
    // and has not yet read; every field is written in full.
    unsafe {
        (*dst).modeset = -1;
        (*dst).enable_guc = -1;
        (*dst).guc_log_level = -1;
        (*dst).guc_firmware_path = ptr::null_mut();
        (*dst).huc_firmware_path = ptr::null_mut();
        (*dst).gsc_firmware_path = ptr::null_mut();
        (*dst).memtest = false;
        (*dst).mmio_debug = 0;
        (*dst).reset = 3;
        (*dst).force_probe = force_probe;
        (*dst).request_timeout_ms = 20_000;
        (*dst).lmem_size = 0;
        (*dst).lmem_bar_size = 0;
        (*dst).enable_hangcheck = true;
        (*dst).error_capture = true;
        (*dst).enable_gvt = false;
        (*dst).enable_debug_only_api = false;
    }
}

/// `dma_fence_is_i915()`: whether `fence` was created with the i915 fence ops.
unsafe extern "C" fn dma_fence_is_i915(fence: *mut c_void) -> bool {
    if fence.is_null() {
        return false;
    }
    let fence = fence.cast::<DmaFence>();
    // SAFETY: the caller passes a live `struct dma_fence`; only `ops` is read.
    unsafe { (*fence).ops == ptr::addr_of!(intel_gt::i915_request_upstream::i915_fence_ops).cast() }
}

/// `file_client()`: the upstream `drm_file` private data. The upstream
/// `drm_file` is opaque here, and the kernel's `DrmFile` is bound to it only
/// when the ioctl owner routes requests. The postclose path that calls this is
/// therefore unreachable until that binding exists, so it panics if reached.
unsafe extern "C" fn file_client(_file: *mut c_void) -> *mut c_void {
    panic!("file_client: upstream drm_file is not bound to a kernel DrmFile yet");
}

/// `kfree_rcu_file_priv()`: frees the private data of a closed `drm_file`. The
/// same binding gap as [`file_client`] applies.
unsafe extern "C" fn kfree_rcu_file_priv(_file: *mut c_void) {
    panic!("kfree_rcu_file_priv: upstream drm_file is not bound to a kernel DrmFile yet");
}

/// `drmm_add_action_or_reset()`: the kernel's DRM-managed action registry. The
/// table declares a one-argument action, but Linux calls the action with
/// `(drm, data)`, which is the signature the registry uses.
unsafe extern "C" fn drmm_add_action(
    drm: *mut c_void,
    action: unsafe extern "C" fn(*mut c_void),
    data: *mut c_void,
) -> c_int {
    // SAFETY: both are thin `extern "C"` function pointers with the same calling
    // convention; the registry invokes the action with `(drm, data)`, the Linux
    // prototype, so the two-argument view is the one it calls.
    let action: unsafe extern "C" fn(*mut c_void, *mut c_void) =
        unsafe { core::mem::transmute(action) };
    // SAFETY: the registry is the kernel's `drmm_add_action_or_reset`, which takes
    // ownership of `action` and `data` exactly as Linux does.
    unsafe { drmm_add_action_or_reset(drm, action, data) }
}

// ---------------------------------------------------------------------------
// Framework table
// ---------------------------------------------------------------------------

static FRAMEWORK: I915FrameworkOps = I915FrameworkOps {
    drm_atomic_helper_shutdown: owned_by_native_display,
    drm_client_dev_resume: owned_by_native_display,
    drm_client_dev_suspend: owned_by_native_display,
    drm_dev_register,
    drm_dev_unplug,
    drm_dev_unregister,
    drm_kms_helper_poll_disable: owned_by_native_display,
    drm_kms_helper_poll_enable: owned_by_native_display,
    drm_mode_config_reset: owned_by_native_display,
    i915_debugfs_register: not_configured_i915,
    i915_hwmon_register: not_configured_i915,
    i915_hwmon_unregister: not_configured_i915,
    i915_perf_fini: not_configured_i915,
    i915_perf_init: not_configured_ok,
    i915_perf_register: not_configured_i915,
    i915_perf_unregister: not_configured_i915,
    i915_pmu_register: not_configured_i915,
    i915_pmu_unregister: not_configured_i915,
    i915_setup_sysfs: not_configured_i915,
    i915_switcheroo_register: not_configured_ok,
    i915_switcheroo_unregister: not_configured_i915,
    i915_teardown_sysfs: not_configured_i915,
    i9xx_display_sr_restore: owned_by_native_display,
    i9xx_display_sr_save: owned_by_native_display,
    intel_bw_init_hw: owned_by_native_display,
    intel_clock_gating_hooks_init: owned_by_native_display,
    intel_display_device_info_runtime_init: owned_by_native_display,
    intel_display_device_present: native_display_present,
    intel_display_device_remove: owned_by_native_display,
    intel_display_driver_disable_user_access: owned_by_native_display,
    intel_display_driver_early_probe: owned_by_native_display,
    intel_display_driver_enable_user_access: owned_by_native_display,
    intel_display_driver_init_hw: owned_by_native_display,
    intel_display_driver_probe: owned_by_native_display_ok,
    intel_display_driver_probe_nogem: owned_by_native_display_ok,
    intel_display_driver_probe_noirq: owned_by_native_display_ok,
    intel_display_driver_register: owned_by_native_display,
    intel_display_driver_remove: owned_by_native_display,
    intel_display_driver_remove_nogem: owned_by_native_display,
    intel_display_driver_remove_noirq: owned_by_native_display,
    intel_display_driver_resume: owned_by_native_display,
    intel_display_driver_resume_access: owned_by_native_display,
    intel_display_driver_suspend: owned_by_native_display_ok,
    intel_display_driver_suspend_access: owned_by_native_display,
    intel_display_driver_unregister: owned_by_native_display,
    intel_display_power_cleanup: owned_by_native_display,
    intel_display_power_disable: owned_by_native_display,
    intel_display_power_driver_remove: owned_by_native_display,
    intel_display_power_enable: owned_by_native_display,
    intel_display_power_resume_early: owned_by_native_display,
    intel_display_power_runtime_resume: owned_by_native_display,
    intel_display_power_runtime_suspend: owned_by_native_display,
    intel_display_power_suspend_late: owned_by_native_display_suspend_late,
    intel_dmc_resume: owned_by_native_display,
    intel_dmc_suspend: owned_by_native_display,
    intel_dp_mst_suspend: owned_by_native_display,
    intel_dpt_resume: owned_by_native_display,
    intel_dpt_suspend: owned_by_native_display,
    intel_dram_detect: native_dram_detect,
    intel_encoder_shutdown_all: owned_by_native_display,
    intel_encoder_suspend_all: owned_by_native_display,
    intel_gmbus_reset: owned_by_native_display,
    intel_gvt_driver_remove: not_configured_i915,
    intel_gvt_init: not_configured_ok,
    intel_gvt_resume: not_configured_i915,
    intel_hpd_cancel_work: owned_by_native_display,
    intel_hpd_init: owned_by_native_display,
    intel_hpd_poll_disable: owned_by_native_display,
    intel_hpd_poll_enable: owned_by_native_display,
    intel_init_pch_refclk: owned_by_native_display,
    intel_match_g8_cpu: not_dg2_cpu,
    intel_opregion_cleanup: owned_by_native_display,
    intel_opregion_notify_adapter: no_opregion_notify,
    intel_opregion_resume: owned_by_native_display,
    intel_opregion_setup: owned_by_native_display_ok,
    intel_opregion_suspend: no_opregion_suspend,
    intel_pps_unlock_regs_wa: owned_by_native_display,
    intel_pxp_debugfs_register: owned_by_native_display,
    intel_pxp_fini: pxp_absent,
    intel_pxp_init: pxp_not_on_platform,
    intel_pxp_resume_complete: owned_by_native_display,
    intel_pxp_runtime_resume: owned_by_native_display,
    intel_pxp_runtime_suspend: owned_by_native_display,
    intel_pxp_suspend: owned_by_native_display,
    intel_pxp_suspend_prepare: owned_by_native_display,
    intel_sbi_fini: owned_by_native_display,
    intel_sbi_init: owned_by_native_display,
    skl_watermark_ipc_update: owned_by_native_display,
    devm_drm_dev_alloc_i915,
    i915_params_copy: {
        // SAFETY: the table's `struct i915_params *` and this `*mut c_void` are
        // the same thin pointer; `i915_params_copy` names its own layout.
        unsafe { core::mem::transmute(i915_params_copy as unsafe extern "C" fn(*mut c_void)) }
    },
    display_parent_interface: no_display_parent_interface,
    display_probe_defer: display_never_defers,
    modparam_force_probe,
    pci_msi_enabled,
    pci_func,
    dma_fence_is_i915,
    drm_driver_name,
    file_client,
    pci_revision,
    kfree_rcu_file_priv,
    pci_register_driver,
    pci_unregister_driver,
    drmm_add_action_or_reset: drmm_add_action,
    intel_clock_gating_init: owned_by_native_display,
    intel_display_device_probe: native_display_device_probe,
};

// ---------------------------------------------------------------------------
// Probe table: BAR0 peek before MMIO is mapped
// ---------------------------------------------------------------------------

/// `early_gmd_read()`: reads one dword of BAR0 at `offset` through the display
/// register window that the probe registered. Returns false when the window is
/// not registered or the offset does not fit in it.
unsafe extern "C" fn early_gmd_read(_i915: *mut DrmI915Private, offset: u32, value: *mut u32) -> bool {
    let Some(window) = upstream_gt::display_window() else {
        return false;
    };
    let offset = offset as usize;
    if value.is_null() || offset.checked_add(4).is_none_or(|end| end > window.len()) {
        return false;
    }
    let address = window.base() + offset;
    // SAFETY: `[address, address + 4)` lies inside the registered register
    // window, which is mapped device MMIO; the bounds are checked above.
    unsafe { *value = ptr::read_volatile(address as *const u32) };
    true
}

static PROBE: I915ProbeOps = I915ProbeOps { early_gmd_read };

// ---------------------------------------------------------------------------
// DRM core table
// ---------------------------------------------------------------------------

/// `drm_dev_enter()`: the device is admitted while it is not unplugged.
unsafe extern "C" fn dev_enter(dev: *mut c_void, idx: *mut c_int) -> bool {
    if dev.is_null() || UNPLUGGED.load(Ordering::Acquire) {
        return false;
    }
    if !idx.is_null() {
        // SAFETY: the caller passes a writable index slot, as in Linux.
        unsafe { *idx = 0 };
    }
    true
}

/// `drm_dev_exit()`: pairs with [`dev_enter`]. Unplug does not wait for SRCU
/// readers, because the kernel never frees the native device, so no reader can
/// hold memory that is about to go away.
unsafe extern "C" fn dev_exit(_idx: c_int) {}

/// `drm_dev_get()`: the upstream device lives as long as the kernel, so the
/// reference is the device pointer itself.
unsafe extern "C" fn dev_get(dev: *mut c_void) -> *mut c_void {
    dev
}

/// `drm_dev_put()`: pairs with [`dev_get`]. The device is never freed.
unsafe extern "C" fn dev_put(_dev: *mut c_void) {}

/// `drm_is_current_master()`: no upstream `drm_file` is bound to a kernel
/// `DrmFile`, so no file can be shown to be master. Privileged paths refuse.
unsafe extern "C" fn is_current_master(_file: *mut c_void) -> bool {
    false
}

/// `drm_print_memory_stats()`: fdinfo output. TheKernel exposes no fdinfo, so
/// the kernel never creates a printer for it and there is nothing to print.
unsafe extern "C" fn print_memory_stats(
    _printer: *mut c_void,
    _stats: *const c_void,
    _supported: c_int,
    _region: *const c_char,
) {
}

/// `drm_gem_free_mmap_offset()`: mmap offsets are created only by the mmap path,
/// which is not bound yet. A node with start 0 was never allocated and has
/// nothing to release.
unsafe extern "C" fn gem_free_mmap_offset(obj: *mut c_void) {
    let obj = obj.cast::<DrmGemObjectBaseLayout>();
    // SAFETY: the caller passes a live `drm_gem_object`; only `vma_node` is read.
    if unsafe { (*obj).vma_node.vm_node.start } != 0 {
        panic!("drm_gem_free_mmap_offset: mmap offset was allocated outside the bound DRM core");
    }
}

/// `drm_gem_handle_create()`: a GEM handle lives in the file's handle table,
/// and that table is the kernel `DrmFile`. No upstream file is bound to one yet,
/// so the call fails with `-ENODEV`.
unsafe extern "C" fn gem_handle_create(_file: *mut c_void, _obj: *mut c_void, _handle: *mut u32) -> c_int {
    -ENODEV
}

/// `drm_gem_object_free()`: the final put of a GEM object. The object's
/// `funcs->free` releases it, as in Linux. An object without `funcs` is the base
/// of an allocation whose owner must free it, which is not a supported path here.
unsafe extern "C" fn gem_object_free(refcount: *mut c_void) {
    // `refcount` is the first member of `drm_gem_object`, so the object starts
    // at the same address.
    let obj = refcount.cast::<DrmGemObjectBaseLayout>();
    // SAFETY: the caller passes the `refcount` of a live object whose final put
    // has happened; `funcs` is read, not written.
    let funcs = unsafe { (*obj).funcs };
    if funcs.is_null() {
        panic!("drm_gem_object_free: object without funcs->free is not supported");
    }
    // SAFETY: `struct drm_gem_object_funcs` begins with the `free` member.
    let free = unsafe { *funcs.cast::<Option<unsafe extern "C" fn(*mut c_void)>>() };
    match free {
        // SAFETY: `free` is the object's own release callback, and `obj` is the
        // object it releases.
        Some(free) => unsafe { free(obj.cast()) },
        None => panic!("drm_gem_object_free: object without funcs->free is not supported"),
    }
}

/// `drm_gem_private_object_init()`: initializes a private GEM object's base
/// record, as Linux does. The reservation is initialized through the
/// `dma_resv_init` symbol that `tk-intel-gt` exports.
unsafe extern "C" fn gem_private_object_init(dev: *mut c_void, obj: *mut c_void, size: usize) {
    let obj = obj.cast::<DrmGemObjectBaseLayout>();
    // SAFETY: the caller passes a live, exclusively owned `drm_gem_object` that
    // is being initialized; the offsets written are the fields of the Linux
    // 7.2.3 x86_64 layout that `DrmGemObjectBaseLayout` records.
    unsafe {
        // `kref_init()`: one reference, held by the caller.
        (*obj).refcount.refcount.refs.counter = 1;
        // `handle_count` is the u32 immediately after the kref.
        obj.cast::<u8>().add(size_of::<Kref>()).cast::<u32>().write(0);
        (*obj).dev = dev;
        (*obj).filp = ptr::null_mut();
        (*obj).size = size as u64;
        // `name` (int) is the 8-byte slot after `size`.
        obj.cast::<u8>()
            .add(offset_of!(DrmGemObjectBaseLayout, size) + size_of::<u64>())
            .cast::<u32>()
            .write(0);
        (*obj).dma_buf = ptr::null_mut();
        (*obj).import_attach = ptr::null_mut();
        // `drm_vma_node_reset()`: an unallocated node is all zero.
        ptr::write_bytes(ptr::addr_of_mut!((*obj).vma_node), 0, 1);
        (*obj).resv = ptr::addr_of_mut!((*obj)._resv).cast();
        dma_resv_init(ptr::addr_of_mut!((*obj)._resv).cast());
    }
}

/// `drm_gem_dmabuf_export()`: the kernel's dma-buf (`DmaBufFile`) wraps a kernel
/// `GemObject`, and no upstream object is bound to one yet.
unsafe extern "C" fn gem_dmabuf_export(_dev: *mut c_void, _info: *mut c_void) -> *mut c_void {
    err_ptr(-ENODEV)
}

/// `drm_gem_dmabuf_release()`: export never succeeds here, so no dma-buf can
/// reach its release.
unsafe extern "C" fn gem_dmabuf_release(_dmabuf: *mut c_void) {
    panic!("drm_gem_dmabuf_release: no upstream dma-buf exists; export is not bound");
}

/// `drm_gem_prime_mmap()`: only exported dma-bufs are mmapped, so this cannot be
/// reached before export succeeds.
unsafe extern "C" fn gem_prime_mmap(_obj: *mut c_void, _vma: *mut c_void) -> c_int {
    -ENODEV
}

/// `drm_gem_unmap_dma_buf()`: import never succeeds here.
unsafe extern "C" fn gem_unmap_dma_buf(_attach: *mut c_void, _sgt: *mut c_void, _direction: c_int) {
    panic!("drm_gem_unmap_dma_buf: no upstream dma-buf attachment exists; import is not bound");
}

/// `drm_prime_gem_destroy()`: only objects with an import attachment reach this,
/// and none exist.
unsafe extern "C" fn prime_gem_destroy(_obj: *mut c_void, _sg: *mut c_void) {
    panic!("drm_prime_gem_destroy: no upstream imported object exists; import is not bound");
}

/// `dma_buf_attach()`: import is not bound.
unsafe extern "C" fn dma_buf_attach(_dmabuf: *mut c_void, _dev: *mut c_void) -> *mut c_void {
    err_ptr(-ENODEV)
}

/// `dma_buf_detach()`: no attachment can exist.
unsafe extern "C" fn dma_buf_detach(_dmabuf: *mut c_void, _attach: *mut c_void) {
    panic!("dma_buf_detach: no upstream dma-buf attachment exists; import is not bound");
}

/// `dma_buf_map_attachment()`: no attachment can exist.
unsafe extern "C" fn dma_buf_map_attachment(_attach: *mut c_void, _direction: c_int) -> *mut c_void {
    err_ptr(-ENODEV)
}

/// `dma_buf_unmap_attachment()`: no attachment can exist.
unsafe extern "C" fn dma_buf_unmap_attachment(_attach: *mut c_void, _sgt: *mut c_void, _direction: c_int) {
    panic!("dma_buf_unmap_attachment: no upstream dma-buf attachment exists; import is not bound");
}

/// `dma_buf_put()`: no dma-buf can exist.
unsafe extern "C" fn dma_buf_put(_dmabuf: *mut c_void) {
    panic!("dma_buf_put: no upstream dma-buf exists; export is not bound");
}

/// `sync_file_create()`: a sync_file wraps a kernel fence and an fd. The upstream
/// fence has no kernel `Fence` bound, so NULL reports the failure.
unsafe extern "C" fn sync_file_create(_fence: *mut c_void) -> *mut c_void {
    ptr::null_mut()
}

/// `sync_file_get_fence()`: the fence behind an fd. No upstream sync_file is
/// bound, so no fd resolves to one.
unsafe extern "C" fn sync_file_get_fence(_fd: c_int) -> *mut c_void {
    ptr::null_mut()
}

/// `drm_syncobj_find()`: a syncobj handle in the file's table. No upstream file
/// is bound, so no handle resolves.
unsafe extern "C" fn syncobj_find(_file: *mut c_void, _handle: u32) -> *mut c_void {
    ptr::null_mut()
}

/// `drm_syncobj_create()`: a syncobj lives in the kernel's syncobj table, and no
/// upstream object can be bound to it yet.
unsafe extern "C" fn syncobj_create(_out: *mut *mut c_void, _flags: u32, _fence: *mut c_void) -> c_int {
    -ENODEV
}

/// `drm_syncobj_add_point()`: no syncobj can be created.
unsafe extern "C" fn syncobj_add_point(_syncobj: *mut c_void, _chain: *mut c_void, _fence: *mut c_void, _point: u64) {
    panic!("drm_syncobj_add_point: no upstream syncobj exists; creation is not bound");
}

/// `drm_syncobj_replace_fence()`: no syncobj can be created.
unsafe extern "C" fn syncobj_replace_fence(_syncobj: *mut c_void, _fence: *mut c_void) {
    panic!("drm_syncobj_replace_fence: no upstream syncobj exists; creation is not bound");
}

/// `drm_syncobj_fence_get()`: no syncobj can be created.
unsafe extern "C" fn syncobj_fence_get(_syncobj: *mut c_void) -> *mut c_void {
    ptr::null_mut()
}

/// `drm_syncobj_put()`: no syncobj can be created.
unsafe extern "C" fn syncobj_put(_syncobj: *mut c_void) {
    panic!("drm_syncobj_put: no upstream syncobj exists; creation is not bound");
}

static DRM_CORE: DrmCoreProvider = DrmCoreProvider {
    dev_enter,
    dev_exit,
    dev_get,
    dev_put,
    is_current_master,
    print_memory_stats,
    gem_free_mmap_offset,
    gem_handle_create,
    gem_object_free,
    gem_private_object_init,
    gem_dmabuf_export,
    gem_dmabuf_release,
    gem_prime_mmap,
    gem_unmap_dma_buf,
    prime_gem_destroy,
    dma_buf_attach,
    dma_buf_detach,
    dma_buf_map_attachment,
    dma_buf_unmap_attachment,
    dma_buf_put,
    sync_file_create,
    sync_file_get_fence,
    syncobj_find,
    syncobj_create,
    syncobj_add_point,
    syncobj_replace_fence,
    syncobj_fence_get,
    syncobj_put,
};

/// Installs the framework, probe and DRM-core tables. Called once from
/// `upstream_gt::install_providers()`. Reinstalling the same tables is accepted.
pub(super) fn install() -> Result<(), &'static str> {
    install_i915_framework_ops(&FRAMEWORK)?;
    install_i915_probe_ops(&PROBE)?;
    install_drm_core_provider(&DRM_CORE)?;
    Ok(())
}
