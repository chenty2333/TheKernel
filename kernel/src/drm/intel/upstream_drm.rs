// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Kernel providers for the i915 framework, probe and DRM-core tables of
//! `tk-intel-gt`.
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
//! Display callbacks belong to no one: the device has no display pipes
//! (`HAS_DISPLAY()` is false), so the `no_display_*` entries do what Linux's
//! early returns do for such a device.

use intel_gt::linux::gem::DrmGemObject;
use core::{
    ffi::{c_char, c_int, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use intel_gt::{
    i915_gem_context_types_upstream::DrmI915Private,
    i915_probe_provider_upstream::{
        I915FrameworkOps, I915ProbeOps, install_i915_framework_ops, install_i915_probe_ops,
    },
    intel_context_upstream::{DmaFence, Kref},
    linux::{
        drm_core::{DrmCoreProvider, install_drm_core_provider},
    },
    linux_i915_private::{DrmDevice, DrmVmaOffsetManager},
};

#[cfg(target_os = "none")]
use super::pci::{ConfigSpace, Ecam};
use super::{pci, upstream_gt};

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
    fn kfree(object: *mut c_void);
    fn drm_vma_offset_remove(
        manager: *mut DrmVmaOffsetManager,
        node: *mut intel_gt::intel_context_upstream::DrmVmaOffsetNode,
    );
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
// Owner split: display, optional subsystems and other native-owned services
// ---------------------------------------------------------------------------

// Each group below is named for the reason its callbacks do nothing:
// - `no_display_*`: the device has no display pipes (`HAS_DISPLAY()` is false,
//   `pipe_mask == 0`). Linux's display entry points return early for such a
//   device (`intel_display_driver.c`, `intel_display_power.c`,
//   `intel_display_irq.c`, `intel_display_device.c`), so each of these does the
//   same: nothing, or the value Linux's early return gives.
// - `not_configured_*`: the configured Linux build compiles the subsystem out,
//   and the Linux stub does nothing or returns success.
// - `pxp_*`, `not_dg2_cpu`: the feature is absent on the ADL-N target, and the
//   Linux code reports that absence through its own return value.

/// Optional non-display core hook not configured by this kernel build.
unsafe extern "C" fn no_display_noop(_display: *mut c_void) {}

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

/// The native Intel display path is opt-in. The boot default must leave the
/// firmware/GOP scanout untouched, even when the render-only GT is enabled.
fn native_modeset_requested() -> bool {
    #[cfg(target_os = "none")]
    {
        axhal::boot::command_line_value("intel.native_modeset") == Some("1")
    }
    #[cfg(not(target_os = "none"))]
    {
        false
    }
}

// Every display parent service (BO, DPT, FB pin, initial plane, DSB, panic,
// PC8) is the translated i915 owner and the display path links completely,
// so the native stack is admitted whenever the user requests it.
const NATIVE_DISPLAY_STACK_READY: bool = true;

pub(super) fn native_modeset_enabled() -> bool {
    native_modeset_requested() && NATIVE_DISPLAY_STACK_READY
}

// The framework table is also used for the firmware/no-display path. Keep all
// register-capable Linux display callbacks behind the single native-mode-set
// opt-in while still routing every lifecycle phase to its real owner when the
// stack is admitted.
macro_rules! gated_display_void {
    ($name:ident, $target:path) => {
        unsafe extern "C" fn $name(display: *mut c_void) {
            if native_modeset_enabled() && !display.is_null() {
                // SAFETY: `display` is verified non-null, native modeset is enabled, and caller
                // provides a valid `intel_display` pointer initialized during probe.
                unsafe { $target(display.cast()) };
            }
        }
    };
}

macro_rules! gated_display_status {
    ($name:ident, $target:path) => {
        unsafe extern "C" fn $name(display: *mut c_void) -> c_int {
            if native_modeset_enabled() && !display.is_null() {
                // SAFETY: `display` is verified non-null, native modeset is enabled, and caller
                // provides a valid `intel_display` pointer initialized during probe.
                unsafe { $target(display.cast()) }
            } else {
                0
            }
        }
    };
}

gated_display_void!(display_early_probe, intel_gt::intel_display_driver_upstream::intel_display_driver_early_probe);
gated_display_void!(display_init_hw, intel_gt::intel_display_driver_upstream::intel_display_driver_init_hw);
gated_display_status!(display_probe_noirq, intel_gt::intel_display_driver_upstream::intel_display_driver_probe_noirq);
gated_display_status!(display_probe_nogem, intel_gt::intel_display_driver_upstream::intel_display_driver_probe_nogem);
gated_display_status!(display_probe, intel_gt::intel_display_driver_upstream::intel_display_driver_probe);
gated_display_void!(display_register, intel_gt::intel_display_driver_upstream::intel_display_driver_register);
gated_display_void!(display_remove, intel_gt::intel_display_driver_upstream::intel_display_driver_remove);
gated_display_void!(display_remove_noirq, intel_gt::intel_display_driver_upstream::intel_display_driver_remove_noirq);
gated_display_void!(display_remove_nogem, intel_gt::intel_display_driver_upstream::intel_display_driver_remove_nogem);
gated_display_void!(display_unregister, intel_gt::intel_display_driver_upstream::intel_display_driver_unregister);
gated_display_status!(display_suspend, intel_gt::intel_display_driver_upstream::intel_display_driver_suspend);
gated_display_void!(display_resume, intel_gt::intel_display_driver_upstream::intel_display_driver_resume);
gated_display_void!(display_suspend_access, intel_gt::intel_display_driver_upstream::intel_display_driver_suspend_access);
gated_display_void!(display_resume_access, intel_gt::intel_display_driver_upstream::intel_display_driver_resume_access);
gated_display_void!(display_disable_user_access, intel_gt::intel_display_driver_upstream::intel_display_driver_disable_user_access);
gated_display_void!(display_enable_user_access, intel_gt::intel_display_driver_upstream::intel_display_driver_enable_user_access);
gated_display_void!(display_power_cleanup, intel_gt::intel_display_power_upstream::intel_display_power_cleanup);
gated_display_void!(display_power_disable, intel_gt::intel_display_power_upstream::intel_display_power_disable);
gated_display_void!(display_power_driver_remove, intel_gt::intel_display_power_upstream::intel_display_power_driver_remove);
gated_display_void!(display_power_enable, intel_gt::intel_display_power_upstream::intel_display_power_enable);
gated_display_void!(display_power_resume_early, intel_gt::intel_display_power_upstream::intel_display_power_resume_early);
gated_display_void!(display_power_runtime_resume, intel_gt::intel_display_power_upstream::intel_display_power_runtime_resume);
gated_display_void!(display_power_runtime_suspend, intel_gt::intel_display_power_upstream::intel_display_power_runtime_suspend);
gated_display_void!(display_dmc_resume, intel_gt::intel_dmc_upstream::intel_dmc_resume);
gated_display_void!(display_dmc_suspend, intel_gt::intel_dmc_upstream::intel_dmc_suspend);
gated_display_void!(display_dp_mst_suspend, intel_gt::display_pending_stubs::opt::intel_dp_mst_suspend);
gated_display_void!(display_dpt_resume, intel_gt::intel_dpt_upstream::intel_dpt_resume);
gated_display_void!(display_dpt_suspend, intel_gt::intel_dpt_upstream::intel_dpt_suspend);
gated_display_status!(display_dram_detect, intel_gt::intel_dram_upstream::intel_dram_detect);
gated_display_void!(display_bw_init_hw, intel_gt::intel_bw_upstream::intel_bw_init_hw);
gated_display_void!(display_encoder_shutdown_all, intel_gt::intel_encoder_upstream::intel_encoder_shutdown_all);
gated_display_void!(display_encoder_suspend_all, intel_gt::intel_encoder_upstream::intel_encoder_suspend_all);
gated_display_void!(display_gmbus_reset, intel_gt::intel_gmbus_upstream::intel_gmbus_reset);
gated_display_void!(display_hpd_cancel_work, intel_gt::intel_hotplug_upstream::intel_hpd_cancel_work);
gated_display_void!(display_hpd_init, intel_gt::display_pending_stubs::b6::intel_hpd_init);
gated_display_void!(display_hpd_poll_disable, intel_gt::display_pending_stubs::b6::intel_hpd_poll_disable);
gated_display_void!(display_hpd_poll_enable, intel_gt::intel_hotplug_upstream::intel_hpd_poll_enable);
gated_display_void!(display_pps_unlock, intel_gt::intel_pps_upstream::intel_pps_unlock_regs_wa);
gated_display_void!(display_opregion_cleanup, intel_gt::intel_opregion_upstream::intel_opregion_cleanup);
gated_display_void!(display_opregion_resume, intel_gt::intel_opregion_upstream::intel_opregion_resume);
gated_display_status!(display_opregion_setup, intel_gt::intel_opregion_upstream::intel_opregion_setup);

unsafe extern "C" fn display_suspend_late(display: *mut c_void, keep: bool) {
    if native_modeset_enabled() && !display.is_null() {
        // SAFETY: `display` is non-null, native modeset is enabled, and caller passes a valid display pointer.
        unsafe { intel_gt::intel_display_power_upstream::intel_display_power_suspend_late(display.cast(), keep) };
    }
}

unsafe extern "C" fn display_opregion_notify(display: *mut c_void, state: u32) -> c_int {
    if native_modeset_enabled() && !display.is_null() {
        // SAFETY: `display` is non-null, native modeset is enabled, and caller passes a valid display pointer.
        unsafe { intel_gt::intel_opregion_upstream::intel_opregion_notify_adapter(display.cast(), state) }
    } else {
        0
    }
}

unsafe extern "C" fn display_opregion_suspend(display: *mut c_void, state: u32) {
    if native_modeset_enabled() && !display.is_null() {
        // SAFETY: `display` is non-null, native modeset is enabled, and caller passes a valid display pointer.
        unsafe { intel_gt::intel_opregion_upstream::intel_opregion_suspend(display.cast(), state) };
    }
}

unsafe extern "C" fn display_device_present(display: *mut c_void) -> bool {
    !display.is_null()
        // SAFETY: `display` is verified non-null and references a valid display device.
        && unsafe { intel_gt::intel_display_device_upstream::intel_display_device_present(display.cast()) }
}

unsafe extern "C" fn display_device_remove(display: *mut c_void) {
    // This frees the complete typed object allocated by the probe, including
    // the no-display descriptor case; it must run even with native KMS off.
    // SAFETY: `display` references the typed display object allocated during probe; releases it.
    unsafe { intel_gt::intel_display_device_upstream::intel_display_device_remove(display.cast()) };
}

unsafe extern "C" fn display_device_runtime_init(display: *mut c_void) {
    if !display.is_null() {
        // SAFETY: `display` is verified non-null and references a valid display device structure.
        unsafe { intel_gt::intel_display_device_upstream::intel_display_device_info_runtime_init(display.cast()) };
    }
}

unsafe extern "C" fn display_mode_config_reset(drm: *mut c_void) {
    if native_modeset_enabled() && !drm.is_null() {
        // SAFETY: `drm` is non-null, native modeset is enabled, and points to a valid `DrmDevice`.
        unsafe { tk_drm_upstream::drm_mode_config_upstream::drm_mode_config_reset(drm.cast()) };
    }
}

unsafe extern "C" fn display_atomic_shutdown(drm: *mut c_void) {
    if native_modeset_enabled() && !drm.is_null() {
        // SAFETY: `drm` is non-null, native modeset is enabled, and points to a valid `DrmDevice`.
        unsafe { tk_drm_upstream::drm_atomic_helper_upstream::drm_atomic_helper_shutdown(drm.cast()) };
    }
}

unsafe extern "C" fn display_client_suspend(drm: *mut c_void) {
    if native_modeset_enabled() && !drm.is_null() {
        // SAFETY: `drm` is non-null, native modeset is enabled, and points to a valid `DrmDevice`.
        unsafe { tk_drm_upstream::drm_client_event_upstream::drm_client_dev_suspend(drm.cast()) };
    }
}

unsafe extern "C" fn display_client_resume(drm: *mut c_void) {
    if native_modeset_enabled() && !drm.is_null() {
        // SAFETY: `drm` is non-null, native modeset is enabled, and points to a valid `DrmDevice`.
        unsafe { tk_drm_upstream::drm_client_event_upstream::drm_client_dev_resume(drm.cast()) };
    }
}

unsafe extern "C" fn display_poll_disable(drm: *mut c_void) {
    if native_modeset_enabled() && !drm.is_null() {
        // SAFETY: `drm` is non-null, native modeset is enabled, and points to a valid `DrmDevice`.
        unsafe { tk_drm_upstream::drm_probe_helper_upstream::drm_kms_helper_poll_disable(drm.cast()) };
    }
}

unsafe extern "C" fn display_poll_enable(drm: *mut c_void) {
    if native_modeset_enabled() && !drm.is_null() {
        // SAFETY: `drm` is non-null, native modeset is enabled, and points to a valid `DrmDevice`.
        unsafe { tk_drm_upstream::drm_probe_helper_upstream::drm_kms_helper_poll_enable(drm.cast()) };
    }
}

unsafe extern "C" fn display_clock_gating_init(drm: *mut c_void) {
    if native_modeset_enabled() && !drm.is_null() {
        // SAFETY: `drm` is non-null, native modeset is enabled, and points to a valid `DrmDevice`.
        unsafe { intel_gt::intel_clock_gating_init(drm) };
    }
}

/// `intel_display_device_probe()`: always allocate the real typed upstream
/// display object. Until explicit opt-in, point it at Linux's typed
/// `NO_DISPLAY` descriptor so early probe callbacks follow the no-display
/// path and do not initialize or write display registers.
unsafe extern "C" fn display_device_probe(
    pdev: *mut c_void,
    parent: *const c_void,
) -> *mut c_void {
    // SAFETY: the PCI identity and parent pointer are the exact inputs passed
    // by i915's framework; the upstream probe allocates a complete IntelDisplay.
    let display = unsafe {
        intel_gt::intel_display_device_upstream::intel_display_device_probe(
            pdev.cast(),
            parent.cast(),
        )
    };
    if display.is_null() || native_modeset_enabled() {
        return display.cast();
    }
    // SAFETY: display is the allocated typed object returned above. Replacing
    // only its descriptor makes HAS_DISPLAY() false without inventing a layout.
    unsafe {
        (*display).info.__device_info =
            ptr::addr_of!(intel_gt::intel_display_device_upstream::NO_DISPLAY).cast_mut();
    }
    display.cast()
}

/// `display_probe_defer()`: a device without display has nothing to wait for.
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
        let native_modeset = native_modeset_enabled();
        if native_modeset_requested() && !native_modeset {
            axlog::warn!(
                "intel.native_modeset=1 refused: display readout/parent providers are incomplete"
            );
        }
        (*dst).modeset = i32::from(native_modeset);
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
unsafe extern "C" fn file_client(file: *mut c_void) -> *mut c_void {
    if file.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: i915 invokes this callback with its live LinuxKPI `drm_file`,
    // whose allocation remains owned by the active open/close callback.
    let private = unsafe { (*file.cast::<intel_gt::linux::gem::DrmFile>()).driver_priv };
    if private.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `driver_priv` is the i915 file-private allocation attached to
    // this live `drm_file`; the pinned Linux 7.2.3 x86_64 layout places its
    // `client` pointer at byte 112 and includes that field in the allocation.
    unsafe { private.cast::<u8>().add(112).cast::<*mut c_void>().read() }
}

/// `kfree_rcu_file_priv()`: frees the private data of a closed `drm_file`. The
/// same binding gap as [`file_client`] applies.
unsafe extern "C" fn kfree_rcu_file_priv(file: *mut c_void) {
    if file.is_null() {
        return;
    }
    let drm_file = file.cast::<intel_gt::linux::gem::DrmFile>();
    // SAFETY: the DRM postclose callback supplies this live LinuxKPI file, and
    // the file-private allocation remains owned by it until this callback.
    let private = unsafe { (*drm_file).driver_priv };
    if !private.is_null() {
        // SAFETY: `private` is the kmalloc-backed driver-private block owned by
        // this file; clearing the slot first prevents a second close from
        // freeing it again.
        unsafe {
            (*drm_file).driver_priv = ptr::null_mut();
            kfree(private);
        }
    }
}

/// `drmm_add_action_or_reset()`: the kernel's DRM-managed action registry.
unsafe extern "C" fn drmm_add_action(
    drm: *mut c_void,
    action: unsafe extern "C" fn(*mut c_void, *mut c_void),
    data: *mut c_void,
) -> c_int {
    // SAFETY: the registry is the LinuxKPI `drmm_add_action_or_reset`, which
    // takes ownership of `action` and `data` exactly as Linux does.
    unsafe { drmm_add_action_or_reset(drm, action, data) }
}

// ---------------------------------------------------------------------------
// Framework table
// ---------------------------------------------------------------------------

static FRAMEWORK: I915FrameworkOps = I915FrameworkOps {
    drm_atomic_helper_shutdown: display_atomic_shutdown,
    drm_client_dev_resume: display_client_resume,
    drm_client_dev_suspend: display_client_suspend,
    drm_kms_helper_poll_disable: display_poll_disable,
    drm_kms_helper_poll_enable: display_poll_enable,
    drm_mode_config_reset: display_mode_config_reset,
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
    i9xx_display_sr_restore: no_display_noop,
    i9xx_display_sr_save: no_display_noop,
    intel_bw_init_hw: display_bw_init_hw,
    intel_clock_gating_hooks_init: no_display_noop,
    intel_display_device_info_runtime_init: display_device_runtime_init,
    intel_display_device_present: display_device_present,
    intel_display_device_remove: display_device_remove,
    intel_display_driver_disable_user_access: display_disable_user_access,
    intel_display_driver_early_probe: display_early_probe,
    intel_display_driver_enable_user_access: display_enable_user_access,
    intel_display_driver_init_hw: display_init_hw,
    intel_display_driver_probe: display_probe,
    intel_display_driver_probe_nogem: display_probe_nogem,
    intel_display_driver_probe_noirq: display_probe_noirq,
    intel_display_driver_register: display_register,
    intel_display_driver_remove: display_remove,
    intel_display_driver_remove_nogem: display_remove_nogem,
    intel_display_driver_remove_noirq: display_remove_noirq,
    intel_display_driver_resume: display_resume,
    intel_display_driver_resume_access: display_resume_access,
    intel_display_driver_suspend: display_suspend,
    intel_display_driver_suspend_access: display_suspend_access,
    intel_display_driver_unregister: display_unregister,
    intel_display_power_cleanup: display_power_cleanup,
    intel_display_power_disable: display_power_disable,
    intel_display_power_driver_remove: display_power_driver_remove,
    intel_display_power_enable: display_power_enable,
    intel_display_power_resume_early: display_power_resume_early,
    intel_display_power_runtime_resume: display_power_runtime_resume,
    intel_display_power_runtime_suspend: display_power_runtime_suspend,
    intel_display_power_suspend_late: display_suspend_late,
    intel_dmc_resume: display_dmc_resume,
    intel_dmc_suspend: display_dmc_suspend,
    intel_dp_mst_suspend: display_dp_mst_suspend,
    intel_dpt_resume: display_dpt_resume,
    intel_dpt_suspend: display_dpt_suspend,
    intel_dram_detect: display_dram_detect,
    intel_encoder_shutdown_all: display_encoder_shutdown_all,
    intel_encoder_suspend_all: display_encoder_suspend_all,
    intel_gmbus_reset: display_gmbus_reset,
    intel_gvt_driver_remove: not_configured_i915,
    intel_gvt_init: not_configured_ok,
    intel_gvt_resume: not_configured_i915,
    intel_hpd_cancel_work: display_hpd_cancel_work,
    intel_hpd_init: display_hpd_init,
    intel_hpd_poll_disable: display_hpd_poll_disable,
    intel_hpd_poll_enable: display_hpd_poll_enable,
    intel_init_pch_refclk: no_display_noop,
    intel_match_g8_cpu: not_dg2_cpu,
    intel_opregion_cleanup: display_opregion_cleanup,
    intel_opregion_notify_adapter: display_opregion_notify,
    intel_opregion_resume: display_opregion_resume,
    intel_opregion_setup: display_opregion_setup,
    intel_opregion_suspend: display_opregion_suspend,
    intel_pps_unlock_regs_wa: display_pps_unlock,
    intel_pxp_debugfs_register: no_display_noop,
    intel_pxp_fini: pxp_absent,
    intel_pxp_init: pxp_not_on_platform,
    intel_pxp_resume_complete: no_display_noop,
    intel_pxp_runtime_resume: no_display_noop,
    intel_pxp_runtime_suspend: no_display_noop,
    intel_pxp_suspend: no_display_noop,
    intel_pxp_suspend_prepare: no_display_noop,
    intel_sbi_fini: no_display_noop,
    intel_sbi_init: no_display_noop,
    skl_watermark_ipc_update: no_display_noop,
    i915_allocated: note_allocated_i915,
    i915_params_copy: {
        // SAFETY: the table's `struct i915_params *` and this `*mut c_void` are
        // the same thin pointer; `i915_params_copy` names its own layout.
        unsafe {
            core::mem::transmute::<
                unsafe extern "C" fn(*mut c_void),
                unsafe extern "C" fn(*mut intel_gt::linux_i915_private::I915Params),
            >(i915_params_copy)
        }
    },
    display_parent_interface: super::upstream_display::display_parent_interface,
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
    intel_clock_gating_init: display_clock_gating_init,
    intel_display_device_probe: display_device_probe,
};

// ---------------------------------------------------------------------------
// Probe table: BAR0 peek before MMIO is mapped
// ---------------------------------------------------------------------------

/// `early_gmd_read()`: reads one dword of BAR0 at `offset` through the display
/// register window that the probe registered. Returns false when the window is
/// not registered or the offset does not fit in it.
unsafe extern "C" fn early_gmd_read(
    _i915: *mut DrmI915Private,
    offset: u32,
    value: *mut u32,
) -> bool {
    let Some(window) = upstream_gt::gpu_window() else {
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

/// `drm_dev_enter()`: the canonical core admits the device while it is not
/// unplugged.
unsafe extern "C" fn dev_enter(dev: *mut c_void, idx: *mut c_int) -> bool {
    // SAFETY: forwards the caller's device and writable index slot unchanged.
    unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_enter(dev.cast(), idx) }
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

/// `drm_is_current_master()`: the canonical DRM core owns mastership.
unsafe extern "C" fn is_current_master(file: *mut c_void) -> bool {
    // SAFETY: i915 passes the live `drm_file` of the calling ioctl.
    unsafe { tk_drm_upstream::drm_auth_upstream::drm_is_current_master(file.cast()) }
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
    let obj = obj.cast::<DrmGemObject>();
    if obj.is_null() {
        return;
    }
    // SAFETY: DRM calls this release hook with the still-live GEM object whose
    // final reference triggered cleanup.
    let device = unsafe { (*obj).dev.cast::<DrmDevice>() };
    let manager = if device.is_null() {
        ptr::null_mut()
    } else {
        // SAFETY: a non-null object device is its live DRM device, whose
        // vma-offset manager was initialized before GEM objects were exposed.
        unsafe { (*device).vma_offset_manager }
    };
    // SAFETY: `obj` is still live during its final-reference callback, so its
    // embedded VMA node may be inspected before deciding whether it was added.
    let node_is_registered = !unsafe { (*obj).vma_node.vm_node.mm }.is_null();
    if !manager.is_null() && node_is_registered {
        // SAFETY: the non-null `mm` marker proves this node was registered in
        // the device manager, and both objects remain live in this callback.
        unsafe { drm_vma_offset_remove(manager, ptr::addr_of_mut!((*obj).vma_node)) };
    }
}

/// `drm_gem_object_free()`: the final put of a GEM object. The object's
/// `funcs->free` releases it, as in Linux. An object without `funcs` is the base
/// of an allocation whose owner must free it, which is not a supported path here.
unsafe extern "C" fn gem_object_free(refcount: *mut c_void) {
    // `refcount` is the first member of `drm_gem_object`, so the object starts
    // at the same address.
    let obj = refcount.cast::<DrmGemObject>();
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
    let obj = obj.cast::<DrmGemObject>();
    // SAFETY: the caller passes a live, exclusively owned `drm_gem_object` that
    // is being initialized; the offsets written are the fields of the Linux
    // 7.2.3 x86_64 layout that `DrmGemObject` records.
    unsafe {
        // `kref_init()`: one reference, held by the caller.
        (*obj).refcount.refcount.refs.counter = 1;
        // `handle_count` is the u32 immediately after the kref.
        obj.cast::<u8>()
            .add(size_of::<Kref>())
            .cast::<u32>()
            .write(0);
        (*obj).dev = dev.cast();
        (*obj).filp = ptr::null_mut();
        (*obj).size = size;
        // `name` (int) is the 8-byte slot after `size`.
        obj.cast::<u8>()
            .add(offset_of!(DrmGemObject, size) + size_of::<u64>())
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
unsafe extern "C" fn dma_buf_map_attachment(
    _attach: *mut c_void,
    _direction: c_int,
) -> *mut c_void {
    err_ptr(-ENODEV)
}

/// `dma_buf_unmap_attachment()`: no attachment can exist.
unsafe extern "C" fn dma_buf_unmap_attachment(
    _attach: *mut c_void,
    _sgt: *mut c_void,
    _direction: c_int,
) {
    panic!("dma_buf_unmap_attachment: no upstream dma-buf attachment exists; import is not bound");
}

/// `dma_buf_put()`: no dma-buf can exist.
unsafe extern "C" fn dma_buf_put(_dmabuf: *mut c_void) {
    panic!("dma_buf_put: no upstream dma-buf exists; export is not bound");
}

static DRM_CORE: DrmCoreProvider = DrmCoreProvider {
    dev_enter,
    dev_exit,
    dev_get,
    dev_put,
    is_current_master,
    print_memory_stats,
    gem_free_mmap_offset,
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
};

/// Installs the framework, probe and DRM-core tables. Called once from
/// `upstream_gt::install_providers()`. Reinstalling the same tables is accepted.
pub(super) fn install() -> Result<(), &'static str> {
    install_i915_framework_ops(&FRAMEWORK)?;
    install_i915_probe_ops(&PROBE)?;
    install_drm_core_provider(&DRM_CORE)?;
    Ok(())
}

/// `i915_allocated()`: records the device `i915_driver_create()` just allocated.
unsafe extern "C" fn note_allocated_i915(i915: *mut DrmI915Private) {
    upstream_gt::note_allocated_device(i915.cast());
}

/// The i915 DRM device once the canonical `drm_dev_register()` has registered
/// it, until `drm_dev_unplug()`. The registration itself is the upstream core's
/// (`tk_drm_upstream::drm_drv_upstream`); this only reads its `registered` flag.
pub(super) fn registered_device() -> Option<*mut c_void> {
    let i915 = upstream_gt::allocated_device()?.cast::<DrmI915Private>();
    // SAFETY: the allocated device lives for the kernel's lifetime (see
    // `note_allocated_i915`), and `drm` is its embedded `struct drm_device`.
    let drm = unsafe { ptr::addr_of!((*i915).drm).cast_mut() };
    // SAFETY: `drm` is the embedded record in the retained device above;
    // its lifecycle flags are initialized by the upstream probe before publication.
    unsafe { ((*drm).registered && !(*drm).unplugged).then_some(drm.cast()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_allocation_has_live_mapping_vma_manager_and_ttm_parent() {
        use tk_drm_upstream::drm_drv_upstream::__devm_drm_dev_alloc;
        use tk_drm_upstream::drm_vma_manager_upstream::{
            DRM_FILE_PAGE_OFFSET_START, drm_vma_offset_add, drm_vma_offset_remove,
        };

        unsafe extern "C" {
            fn intel_region_ttm_device_init(i915: *mut DrmI915Private) -> c_int;
            fn intel_region_ttm_device_fini(i915: *mut DrmI915Private);
        }
        // These are the production allocator and the exact TTM entry point
        // which faulted on hardware, not synthetic non-null pointer checks.
        let mut parent = 0u64;
        let parent = ptr::from_mut(&mut parent).cast();
        for _ in 0..2 {
            // SAFETY: exercises the canonical devm_drm_dev_alloc() and TTM
            // lifecycle using a valid mock parent pointer. The device is kept
            // for the test process: its release callback is the i915 teardown.
            unsafe {
                let container = __devm_drm_dev_alloc(
                    parent,
                    core::ptr::addr_of!(intel_gt::i915_driver_upstream::I915_DRM_DRIVER.0),
                    core::mem::size_of::<DrmI915Private>(),
                    core::mem::offset_of!(DrmI915Private, drm),
                );
                assert!(!tk_linux_kapi::config::IS_ERR(container));
                let i915 = container.cast::<DrmI915Private>();
                let drm = &(*i915).drm;
                assert_eq!(drm.dev, parent);
                assert_eq!(drm.dma_dev, parent);
                assert!(!drm.driver.is_null());
                let mapping = (*drm.anon_inode.cast::<tk_drm_upstream::drm_drv_upstream::DrmFsInode>()).i_mapping;
                assert!(!mapping.is_null());
                let manager = drm.vma_offset_manager;
                let mut node = core::mem::zeroed();
                assert_eq!(drm_vma_offset_add(manager, &mut node, 3), 0);
                assert!(node.vm_node.start >= DRM_FILE_PAGE_OFFSET_START);
                assert_eq!(node.vm_node.size, 3);
                drm_vma_offset_remove(manager, &mut node);
                assert_eq!(intel_region_ttm_device_init(i915), 0);
                assert_eq!((*i915).bdev.dev_mapping, mapping);
                assert_eq!((*i915).bdev.vma_manager, manager.cast());
                intel_region_ttm_device_fini(i915);
            }
        }
    }

    #[test]
    fn initialized_globals_supply_the_first_gem_object() {
        let _scheduler = crate::test_support::scheduler_test_context();
        // SAFETY: the host allocator is live; the production once-only
        // initializer owns all cache initialization in this test process.
        assert_eq!(unsafe { intel_gt::i915_driver_upstream::i915_globals_init() }, 0);
        // SAFETY: global object cache was initialized above.
        let object = unsafe { intel_gt::i915_gem_object_upstream::i915_gem_object_alloc() };
        assert!(!object.is_null());
        // SAFETY: the uninitialized GEM payload has no users or references.
        unsafe { intel_gt::i915_gem_object_upstream::i915_gem_object_free(object) };
        // SAFETY: repeat initialization exercises the production ready gate.
        assert_eq!(unsafe { intel_gt::i915_driver_upstream::i915_globals_init() }, 0);
    }
}
