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

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use intel_gt::{
    i915_driver_upstream::{DrmDriverDesc, i915_drm_driver},
    i915_gem_context_types_upstream::DrmI915Private,
    i915_probe_provider_upstream::{
        I915FrameworkOps, I915ProbeOps, install_i915_framework_ops, install_i915_probe_ops,
    },
    intel_context_upstream::{DmaFence, DrmGemObjectBaseLayout, Kref},
    linux::{
        drm_core::{DrmCoreProvider, install_drm_core_provider},
        shmem,
    },
    linux_i915_private::{DrmDevicePrefix, DrmVmaOffsetManager, Inode},
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
    fn drm_mm_init(mm: *mut tk_linux_kapi::types::DrmMm, start: u64, size: u64);
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
// DRM device binding
// ---------------------------------------------------------------------------

/// The upstream `drm_device` attached to the kernel's native Intel DRM device.
/// TheKernel registers one primary device (`crate::drm::primary_device`); the
/// upstream i915 shares its file dispatcher and is never a second KMS device.
/// GT registration may precede primary KMS registration (including fallback).
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
    // GT registration is independent of KMS takeover. A failed native
    // fastboot may leave the firmware KMS primary to be registered later;
    // neither that ordering nor a headless machine invalidates this GT.
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
// - `no_display_*`: the device has no display pipes (`HAS_DISPLAY()` is false,
//   `pipe_mask == 0`). Linux's display entry points return early for such a
//   device (`intel_display_driver.c`, `intel_display_power.c`,
//   `intel_display_irq.c`, `intel_display_device.c`), so each of these does the
//   same: nothing, or the value Linux's early return gives.
// - `not_configured_*`: the configured Linux build compiles the subsystem out,
//   and the Linux stub does nothing or returns success.
// - `pxp_*`, `not_dg2_cpu`: the feature is absent on the ADL-N target, and the
//   Linux code reports that absence through its own return value.

/// No-display entry point with no work to do. Linux `intel_display_driver_*`,
/// `intel_display_power_*`, `intel_hpd_*`, `intel_dmc_*`, `intel_dpt_*`,
/// `intel_opregion_*`, `intel_pps_*`, `intel_pxp_*` and `intel_sbi_*` return
/// early when `!HAS_DISPLAY()`; there is no display state to set up or tear down.
unsafe extern "C" fn no_display_noop(_display: *mut c_void) {}

/// No-display entry point that returns success to the upstream caller, as Linux
/// `intel_display_driver_probe()` and its siblings do with `!HAS_DISPLAY()`.
unsafe extern "C" fn no_display_ok(_display: *mut c_void) -> c_int {
    0
}

/// `intel_display_device_present()` is `display && HAS_DISPLAY(display)`
/// (`intel_display_device.c`). This device has no display pipes, so it is false,
/// and the suspend and resume paths take their no-display branches.
unsafe extern "C" fn no_display_present(_display: *mut c_void) -> bool {
    false
}

/// `intel_opregion_suspend()`: TheKernel has no ACPI OpRegion.
unsafe extern "C" fn no_opregion_suspend(_display: *mut c_void, _state: u32) {}

/// `intel_opregion_notify_adapter()`: no OpRegion means no adapter to notify.
/// The upstream caller treats 0 as "nothing to do".
unsafe extern "C" fn no_opregion_notify(_display: *mut c_void, _state: u32) -> c_int {
    0
}

/// `intel_display_power_suspend_late()` with `!HAS_DISPLAY()`: no display wells
/// to keep powered.
unsafe extern "C" fn no_display_suspend_late(_display: *mut c_void, _keep: bool) {}

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

/// `intel_dram_detect()`: the DRAM parameters feed display watermarks and
/// bandwidth only. Without display there is nothing to read them for, so the
/// step succeeds. Linux `intel_dram_detect()` is display-side as well.
unsafe extern "C" fn no_display_dram_detect(_display: *mut c_void) -> c_int {
    0
}

/// The display object the upstream probe stores as `i915->display`, for a
/// device with no display pipes. Linux `intel_display_device.c` `no_display`
/// gives such a device a non-NULL display whose `info.pipe_mask` is 0, so
/// `HAS_DISPLAY()` is false (`intel_display_device.c:1767-1770`). The object is
/// all-zero: `pipe_mask` is 0 at any offset, and no plane, CRTC or DISPLAY_INFO
/// pointer is set. Every entry point that receives it is a `no_display_*` stub
/// that does not read it. The object is sized generously because the Linux
/// `struct intel_display` layout is not modelled here.
#[repr(C, align(16))]
struct NoDisplay([u64; 512]);
static mut NO_DISPLAY: NoDisplay = NoDisplay([0; 512]);

/// `intel_display_device_probe()`: the no-display object.
unsafe extern "C" fn no_display_device_probe(
    _pdev: *mut c_void,
    _parent: *const c_void,
) -> *mut c_void {
    // SAFETY: the object is only addressed, never read or written through here.
    ptr::addr_of_mut!(NO_DISPLAY).cast()
}

/// `display_parent_interface()`: the table upstream display code uses to reach
/// parent services. No display code runs here, so there is no reader.
unsafe extern "C" fn no_display_parent_interface() -> *const c_void {
    ptr::null()
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

/// The DRM device and its GEM resources have one kernel-lifetime owner, just
/// like the native PCI identity. The inode is the upstream-layout view; its
/// mapping is owned by the existing LinuxKPI anonymous file, not an opaque
/// token. The VMA manager uses the same drm_mm allocator as GEM mmap offsets.
#[repr(C)]
struct I915Allocation {
    i915: DrmI915Private,
    inode: Inode,
    vma: DrmVmaOffsetManager,
    driver: DrmDriverDesc,
    mapping_file: *mut shmem::File,
}

const DRM_FILE_PAGE_OFFSET_START: u64 = (u32::MAX as u64 >> 12) + 1;
const DRM_FILE_PAGE_OFFSET_SIZE: u64 = (u32::MAX as u64 >> 12) * 256;

/// Native `devm_drm_dev_alloc()` + the GEM portion of `drm_dev_init()`.
/// Zeroing i915 alone is insufficient: early TTM init dereferences the DRM
/// anonymous inode, and subsequent GEM users need a live mmap-offset manager.
unsafe extern "C" fn devm_drm_dev_alloc_i915(
    pdev: *mut c_void,
    err: *mut c_int,
) -> *mut DrmI915Private {
    if !err.is_null() {
        // SAFETY: the DRM allocation callback contract supplies `err` as NULL
        // or a writable errno slot; this branch checks the nullable case.
        unsafe { *err = -ENOMEM };
    }
    if pdev.is_null() {
        if !err.is_null() {
            // SAFETY: as above, a non-null `err` is the caller's writable
            // errno slot and is checked before storing.
            unsafe { *err = -EINVAL };
        }
        return ptr::null_mut();
    }
    let size = size_of::<I915Allocation>();
    // SAFETY: `kmalloc` with `__GFP_ZERO` returns `size` zeroed bytes or NULL;
    // the cast is to the allocation's own type.
    let allocation = unsafe { kmalloc(size, GFP_KERNEL | GFP_ZERO) }.cast::<I915Allocation>();
    if allocation.is_null() {
        return ptr::null_mut();
    }
    // Reuse the real page-cache owner. This file is retained with the device;
    // no pages are allocated until requested through its mapping.
    // SAFETY: the name is a static NUL-terminated string and the LinuxKPI
    // shmem helper accepts a zero-sized file to be grown on later faults.
    let file = unsafe { shmem::shmem_file_setup(c"drm".as_ptr(), 0, 0) };
    if tk_linux_kapi::config::IS_ERR(file) {
        if !err.is_null() {
            // SAFETY: a non-null `err` is the writable errno slot supplied to
            // this allocator callback, checked above before storing here.
            unsafe { *err = file as isize as c_int };
        }
        // SAFETY: `allocation` is still our live, unshared kmalloc block; the
        // failed shmem setup did not take ownership of it.
        unsafe { kfree(allocation.cast()) };
        return ptr::null_mut();
    }
    // SAFETY: `pdev` is the live PCI device supplied by i915's allocator
    // callback. All resources are exclusively owned and initialized in their
    // final addresses (drm_mm contains self-linked sentinels). Publish them
    // only after their owners exist; no GT hardware is accessed here.
    unsafe {
        ptr::write(ptr::addr_of_mut!((*allocation).driver), i915_drm_driver());
        (*allocation).mapping_file = file;
        (*allocation).inode.i_mapping = (*file).f_mapping.cast();
        let manager = ptr::addr_of_mut!((*allocation).vma);
        drm_mm_init(
            ptr::addr_of_mut!((*manager).vm_addr_space),
            DRM_FILE_PAGE_OFFSET_START,
            DRM_FILE_PAGE_OFFSET_SIZE,
        );
        (*allocation).i915.drm.initialize(
            pdev,
            ptr::addr_of_mut!((*allocation).driver).cast(),
            ptr::addr_of_mut!((*allocation).inode),
            manager,
        );
        if !err.is_null() {
            *err = 0;
        }
    }
    // The GT_IRQ power reference reaches the device's uncore before the probe
    // has registered it, from inside `i915_pci_probe()`.
    upstream_gt::note_allocated_device(allocation.cast());
    allocation.cast()
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
    drm_atomic_helper_shutdown: no_display_noop,
    drm_client_dev_resume: no_display_noop,
    drm_client_dev_suspend: no_display_noop,
    drm_dev_register,
    drm_dev_unplug,
    drm_dev_unregister,
    drm_kms_helper_poll_disable: no_display_noop,
    drm_kms_helper_poll_enable: no_display_noop,
    drm_mode_config_reset: no_display_noop,
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
    intel_bw_init_hw: no_display_noop,
    intel_clock_gating_hooks_init: no_display_noop,
    intel_display_device_info_runtime_init: no_display_noop,
    intel_display_device_present: no_display_present,
    intel_display_device_remove: no_display_noop,
    intel_display_driver_disable_user_access: no_display_noop,
    intel_display_driver_early_probe: no_display_noop,
    intel_display_driver_enable_user_access: no_display_noop,
    intel_display_driver_init_hw: no_display_noop,
    intel_display_driver_probe: no_display_ok,
    intel_display_driver_probe_nogem: no_display_ok,
    intel_display_driver_probe_noirq: no_display_ok,
    intel_display_driver_register: no_display_noop,
    intel_display_driver_remove: no_display_noop,
    intel_display_driver_remove_nogem: no_display_noop,
    intel_display_driver_remove_noirq: no_display_noop,
    intel_display_driver_resume: no_display_noop,
    intel_display_driver_resume_access: no_display_noop,
    intel_display_driver_suspend: no_display_ok,
    intel_display_driver_suspend_access: no_display_noop,
    intel_display_driver_unregister: no_display_noop,
    intel_display_power_cleanup: no_display_noop,
    intel_display_power_disable: no_display_noop,
    intel_display_power_driver_remove: no_display_noop,
    intel_display_power_enable: no_display_noop,
    intel_display_power_resume_early: no_display_noop,
    intel_display_power_runtime_resume: no_display_noop,
    intel_display_power_runtime_suspend: no_display_noop,
    intel_display_power_suspend_late: no_display_suspend_late,
    intel_dmc_resume: no_display_noop,
    intel_dmc_suspend: no_display_noop,
    intel_dp_mst_suspend: no_display_noop,
    intel_dpt_resume: no_display_noop,
    intel_dpt_suspend: no_display_noop,
    intel_dram_detect: no_display_dram_detect,
    intel_encoder_shutdown_all: no_display_noop,
    intel_encoder_suspend_all: no_display_noop,
    intel_gmbus_reset: no_display_noop,
    intel_gvt_driver_remove: not_configured_i915,
    intel_gvt_init: not_configured_ok,
    intel_gvt_resume: not_configured_i915,
    intel_hpd_cancel_work: no_display_noop,
    intel_hpd_init: no_display_noop,
    intel_hpd_poll_disable: no_display_noop,
    intel_hpd_poll_enable: no_display_noop,
    intel_init_pch_refclk: no_display_noop,
    intel_match_g8_cpu: not_dg2_cpu,
    intel_opregion_cleanup: no_display_noop,
    intel_opregion_notify_adapter: no_opregion_notify,
    intel_opregion_resume: no_display_noop,
    intel_opregion_setup: no_display_ok,
    intel_opregion_suspend: no_opregion_suspend,
    intel_pps_unlock_regs_wa: no_display_noop,
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
    devm_drm_dev_alloc_i915,
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
    intel_clock_gating_init: no_display_noop,
    intel_display_device_probe: no_display_device_probe,
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
unsafe extern "C" fn is_current_master(file: *mut c_void) -> bool {
    let Some(native) = super::upstream_file::native_file(file) else {
        return false;
    };
    // SAFETY: `native_file` returns a pointer only for an active file binding;
    // the binding keeps the native `DrmFile` borrowed for this callback.
    unsafe { (&*native.cast::<crate::drm::DrmFile>()).is_current_master() }
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
    if obj.is_null() {
        return;
    }
    // SAFETY: DRM calls this release hook with the still-live GEM object whose
    // final reference triggered cleanup.
    let device = unsafe { (*obj).dev.cast::<DrmDevicePrefix>() };
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

/// `drm_gem_handle_create()`: a GEM handle lives in the file's handle table,
/// and that table is the kernel `DrmFile`. No upstream file is bound to one yet,
/// so the call fails with `-ENODEV`.
unsafe extern "C" fn gem_handle_create(
    file: *mut c_void,
    obj: *mut c_void,
    handle: *mut u32,
) -> c_int {
    // SAFETY: Linux's GEM-handle callback provides a live object and writable
    // output slot; the bridge rejects null inputs and requires an active file
    // binding before it dereferences any of these pointers.
    unsafe { super::upstream_file::gem_handle_create(file, obj, handle) }
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
        obj.cast::<u8>()
            .add(size_of::<Kref>())
            .cast::<u32>()
            .write(0);
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

/// `sync_file_create()`: bind the upstream dma_fence to the native sync_file
/// and retain the LinuxKPI ABI wrapper until fd_install/fput.
unsafe extern "C" fn sync_file_create(fence: *mut c_void) -> *mut c_void {
    // SAFETY: i915 passes a live dma_fence reference to this callback; the
    // bridge obtains the ownership it needs before returning the sync_file.
    unsafe { super::upstream_sync::sync_file_create(fence) }
}

/// `sync_file_get_fence()`: return an owned upstream reference to the native
/// fence behind a sync_file descriptor.
unsafe extern "C" fn sync_file_get_fence(fd: c_int) -> *mut c_void {
    // SAFETY: `fd` is the descriptor requested by i915; the bridge resolves it
    // through the kernel file table and returns an owned fence or NULL.
    unsafe { super::upstream_sync::sync_file_get_fence(fd) }
}

/// Final `fput()` hook for a sync_file wrapper not transferred to fd_install.
unsafe extern "C" fn sync_file_release(file: *mut c_void) {
    // SAFETY: LinuxKPI calls this hook for the live sync_file wrapper created
    // by the paired bridge callback, which removes and releases that owner.
    unsafe { super::upstream_sync::sync_file_release(file) }
}

/// `drm_syncobj_find()`: resolve the upstream file/handle to the native
/// per-file syncobj and return one owned bridge reference.
unsafe extern "C" fn syncobj_find(file: *mut c_void, handle: u32) -> *mut c_void {
    // SAFETY: i915 supplies the active DRM file and handle; the bridge checks
    // its file binding/handle table before returning an owned syncobj ref.
    unsafe { super::upstream_sync::syncobj_find(file, handle) }
}

/// `drm_syncobj_create()`: create a native binary syncobj for i915 contexts.
unsafe extern "C" fn syncobj_create(
    out: *mut *mut c_void,
    flags: u32,
    fence: *mut c_void,
) -> c_int {
    // SAFETY: i915 provides a writable output slot and either NULL or a live
    // fence per the syncobj-create ABI; the bridge validates flags and inputs
    // before publishing the result.
    unsafe { super::upstream_sync::syncobj_create(out, flags, fence) }
}

/// `drm_syncobj_add_point()`: publish a native timeline point.
unsafe extern "C" fn syncobj_add_point(
    syncobj: *mut c_void,
    chain: *mut c_void,
    fence: *mut c_void,
    point: u64,
) {
    // SAFETY: i915 passes a live syncobj, timeline-chain slot and fence under
    // the syncobj ABI; the bridge validates bridge-owned objects before use.
    unsafe { super::upstream_sync::syncobj_add_point(syncobj, chain, fence, point) }
}

/// `drm_syncobj_replace_fence()`: publish a native binary completion edge.
unsafe extern "C" fn syncobj_replace_fence(syncobj: *mut c_void, fence: *mut c_void) {
    // SAFETY: i915 supplies the syncobj and replacement fence per the DRM
    // callback contract; the bridge resolves its owned syncobj before update.
    unsafe { super::upstream_sync::syncobj_replace_fence(syncobj, fence) }
}

/// `drm_syncobj_fence_get()`: obtain a Linux fence wrapper for native state.
unsafe extern "C" fn syncobj_fence_get(syncobj: *mut c_void) -> *mut c_void {
    // SAFETY: the callback receives the syncobj reference returned by the
    // bridge; the helper verifies the registered object and takes a fence ref.
    unsafe { super::upstream_sync::syncobj_fence_get(syncobj) }
}

/// `drm_syncobj_put()`: release one bridge reference.
unsafe extern "C" fn syncobj_put(syncobj: *mut c_void) {
    // SAFETY: the caller releases exactly one bridge-owned syncobj reference;
    // the helper's registry/refcount accounting makes the matching put.
    unsafe { super::upstream_sync::syncobj_put(syncobj) }
}

/// Mirror only explicitly registered i915 GEM aliases back into the native
/// reservation graph. All other Linux reservation objects remain untouched.
unsafe extern "C" fn resv_fence_add(resv: *mut c_void, fence: *mut c_void, usage: u32) {
    super::upstream_sync::upstream_reservation_fence_add(resv, fence.cast(), usage);
}

/// Resolve timeline point requests on native syncobj fences; return the
/// not-handled sentinel for ordinary upstream chain fences.
unsafe extern "C" fn syncobj_timeline_fence_find(fence: *mut *mut c_void, point: u64) -> c_int {
    // SAFETY: i915 supplies its writable fence slot to this lookup callback;
    // the bridge changes it only when it identifies its own native timeline
    // fence and leaves ordinary upstream fences to the caller.
    unsafe { super::upstream_sync::syncobj_timeline_fence_find(fence, point) }
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
    sync_file_release,
    syncobj_find,
    syncobj_create,
    syncobj_add_point,
    syncobj_replace_fence,
    syncobj_fence_get,
    syncobj_put,
    resv_fence_add,
    syncobj_timeline_fence_find,
};

/// Installs the framework, probe and DRM-core tables. Called once from
/// `upstream_gt::install_providers()`. Reinstalling the same tables is accepted.
pub(super) fn install() -> Result<(), &'static str> {
    install_i915_framework_ops(&FRAMEWORK)?;
    install_i915_probe_ops(&PROBE)?;
    install_drm_core_provider(&DRM_CORE)?;
    Ok(())
}

/// i915's published DRM device after a successful driver registration. This
/// is also the readiness gate for exposing upstream file/ioctl services.
pub(super) fn registered_device() -> Option<*mut c_void> {
    let dev = BOUND_DRM.load(Ordering::Acquire);
    (!dev.is_null() && !UNPLUGGED.load(Ordering::Acquire)).then_some(dev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_allocation_has_live_mapping_vma_manager_and_ttm_parent() {
        unsafe extern "C" {
            fn intel_region_ttm_device_init(i915: *mut DrmI915Private) -> c_int;
            fn intel_region_ttm_device_fini(i915: *mut DrmI915Private);
            fn drm_mm_takedown(mm: *mut tk_linux_kapi::types::DrmMm);
            fn drm_vma_offset_add(
                manager: *mut DrmVmaOffsetManager,
                node: *mut intel_gt::intel_context_upstream::DrmVmaOffsetNode,
                pages: c_ulong,
            ) -> c_int;
            fn drm_vma_offset_remove(
                manager: *mut DrmVmaOffsetManager,
                node: *mut intel_gt::intel_context_upstream::DrmVmaOffsetNode,
            );
        }
        // These are the production allocator and the exact TTM entry point
        // which faulted on hardware, not synthetic non-null pointer checks.
        let mut parent = 0u64;
        let parent = ptr::from_mut(&mut parent).cast();
        for _ in 0..2 {
            unsafe {
                let mut error = 123;
                let i915 = devm_drm_dev_alloc_i915(parent, &mut error);
                assert!(!i915.is_null());
                assert_eq!(error, 0);
                let allocation = i915.cast::<I915Allocation>();
                let drm = &(*i915).drm;
                assert_eq!(drm.dev, parent);
                assert_eq!(drm.dma_dev, parent);
                assert!(!drm.driver.is_null());
                let mapping = (*drm.anon_inode).i_mapping;
                assert_eq!(mapping, (*(*allocation).mapping_file).f_mapping.cast());
                assert!(!mapping.is_null());
                let mut node = core::mem::zeroed();
                assert_eq!(drm_vma_offset_add(drm.vma_offset_manager, &mut node, 3), 0);
                assert!(node.vm_node.start >= DRM_FILE_PAGE_OFFSET_START);
                assert_eq!(node.vm_node.size, 3);
                drm_vma_offset_remove(drm.vma_offset_manager, &mut node);
                assert_eq!(intel_region_ttm_device_init(i915), 0);
                assert_eq!((*i915).bdev.dev_mapping, mapping);
                assert_eq!((*i915).bdev.vma_manager, drm.vma_offset_manager.cast());
                intel_region_ttm_device_fini(i915);
                drm_mm_takedown(ptr::addr_of_mut!((*allocation).vma.vm_addr_space));
                shmem::fput_file((*allocation).mapping_file);
                kfree(allocation.cast());
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

    #[test]
    fn gt_registration_does_not_require_fastboot_primary() {
        let mut token = 0u64;
        let dev = ptr::from_mut(&mut token).cast::<c_void>();
        assert_eq!(unsafe { drm_dev_register(dev, 0) }, 0);
        assert_eq!(registered_device(), Some(dev));
        assert_eq!(unsafe { drm_dev_register(dev, 0) }, 0);
        unsafe { drm_dev_unregister(dev) };
        assert_eq!(registered_device(), None);
    }

    #[test]
    fn drm_allocation_rejects_missing_parent() {
        let mut error = 0;
        assert!(unsafe { devm_drm_dev_alloc_i915(ptr::null_mut(), &mut error) }.is_null());
        assert_eq!(error, -EINVAL);
    }
}
