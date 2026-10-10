// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Kernel-side owners for the upstream i915 GT path (`intel-upstream-gt`).
//!
//! The translated GT code in `tk-intel-gt` declares C symbols it expects the
//! kernel to define. This module is where those definitions live, together
//! with the installation of the kernel-owned platform callbacks.
//!
//! Every symbol here either implements the Linux behaviour for the
//! configuration TheKernel builds, or fails closed. Fail-closed means a
//! `panic!` naming the unsupported owner, never a silent success. Where the
//! upstream caller already has a "not supported" branch, the function returns
//! the value that branch expects.
//!
//! Nothing in this file is reachable from the default build: the module
//! declaration in `mod.rs` is gated on the `intel-upstream-gt` feature.

use alloc::boxed::Box;
use core::{
    ffi::{c_char, c_int, c_void},
    ptr,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use intel_gt::linux_platform::{I915PlatformOps, install_i915_platform_ops};

use super::pci;
#[cfg(target_os = "none")]
use super::pci::{ConfigSpace, ConfigWriteSpace};

/// Linux errno values used by the platform callbacks.
const EIO: c_int = 5;
const EINVAL: c_int = 22;
const EOPNOTSUPP: c_int = 95;
const ENODEV: c_int = 19;

/// PCI STATUS register (word at 0x06). Its bits are write-one-to-clear, so a
/// byte or word read-modify-write of the adjacent COMMAND word must never
/// echo it back.
const PCI_STATUS: u16 = 0x06;

/// Native PCI identity retained in the upstream `drm.dev` field. The GT
/// owner passes this pointer to `to_pci_dev()` and the config accessors; the
/// kernel creates it once per device with [`native_pci_device`].
#[repr(C)]
pub(super) struct NativePciDevice {
    bdf: pci::Bdf,
}

/// Leak one native PCI identity for the lifetime of the kernel. There is one
/// GT-capable device on the supported platforms, so this runs once at probe.
#[allow(dead_code)] // called by the probe owner when it constructs the i915 layout
pub(super) fn native_pci_device(bdf: pci::Bdf) -> *mut c_void {
    *GT_PCI_IDENTITY.lock() = Some(bdf);
    Box::into_raw(Box::new(NativePciDevice { bdf })).cast()
}

/// The single GT-capable PCI function the probe owner created. The PCI
/// revision reader reads its configuration space; TheKernel supports one GT
/// device per boot, so there is no per-device lookup.
static GT_PCI_IDENTITY: spin::Mutex<Option<pci::Bdf>> = spin::Mutex::new(None);

fn bdf_of(device: *mut c_void) -> Option<pci::Bdf> {
    if device.is_null() {
        return None;
    }
    // SAFETY: the only producer of these pointers is `native_pci_device`,
    // which leaks a live `NativePciDevice` for the kernel's lifetime.
    Some(unsafe { (*device.cast::<NativePciDevice>()).bdf })
}

/// Configuration-space access through the kernel's ECAM window, mapped on
/// first use. `None` means the window is unavailable or the access is outside
/// it. Host builds have no ECAM and always report `None`.
#[cfg(target_os = "none")]
fn with_config<R>(f: impl FnOnce(&mut pci::Ecam) -> Option<R>) -> Option<R> {
    static ECAM: axsync::Mutex<Option<pci::Ecam>> = axsync::Mutex::new(None);
    let mut guard = ECAM.lock();
    if guard.is_none() {
        *guard = pci::Ecam::platform();
    }
    f(guard.as_mut()?)
}

#[cfg(target_os = "none")]
fn config_read_u32(bdf: pci::Bdf, offset: u16) -> Option<u32> {
    with_config(|ecam| ecam.read_u32(bdf, offset))
}

#[cfg(target_os = "none")]
fn config_read_u16(bdf: pci::Bdf, offset: u16) -> Option<u16> {
    with_config(|ecam| ecam.read_u16(bdf, offset))
}

#[cfg(target_os = "none")]
fn config_write_u16(bdf: pci::Bdf, offset: u16, value: u16) -> Option<()> {
    with_config(|ecam| ecam.write_u16(bdf, offset, value).ok())
}

#[cfg(not(target_os = "none"))]
fn config_read_u32(_bdf: pci::Bdf, _offset: u16) -> Option<u32> {
    None
}

#[cfg(not(target_os = "none"))]
fn config_read_u16(_bdf: pci::Bdf, _offset: u16) -> Option<u16> {
    None
}

#[cfg(not(target_os = "none"))]
fn config_write_u16(_bdf: pci::Bdf, _offset: u16, _value: u16) -> Option<()> {
    None
}

/// The upstream i915 device whose GT interrupt path the display IRQ handler
/// may delegate to. Stage two registers it once the probe owns the layout.
/// Nothing reads it until `irq.rs` gains the `gen11_irq_handler` branch, which
/// needs `tk-intel-gt` to export that handler and its hooks trait.
static UPSTREAM_I915: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Register the upstream device. Re-registering the same pointer is accepted;
/// a different device is refused.
#[allow(dead_code)] // consumed by the stage-two IRQ branch
pub(super) fn register_upstream_i915(i915: *mut c_void) -> Result<(), &'static str> {
    if i915.is_null() {
        return Err("null upstream i915 device");
    }
    match UPSTREAM_I915.compare_exchange(
        ptr::null_mut(),
        i915,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(existing) if existing == i915 => Ok(()),
        Err(_) => Err("a different upstream i915 device is already registered"),
    }
}

// ---------------------------------------------------------------------------
// i915 platform callbacks (`linux/platform.rs`)
// ---------------------------------------------------------------------------

/// `to_pci_dev()` is a container-identity conversion in Linux; the native
/// identity already is the PCI device.
unsafe extern "C" fn platform_to_pci_dev(device: *mut c_void) -> *mut c_void {
    device
}

unsafe extern "C" fn platform_pci_read_config_byte(
    device: *mut c_void,
    offset: u32,
    value: *mut u8,
) -> c_int {
    let (Some(bdf), Ok(offset)) = (bdf_of(device), u16::try_from(offset)) else {
        return -EINVAL;
    };
    let Some(dword) = config_read_u32(bdf, offset & !3) else {
        return -EIO;
    };
    // SAFETY: the caller passes a valid byte destination (checked by the
    // platform wrapper before dispatch).
    unsafe { *value = (dword >> (8 * u32::from(offset & 3))) as u8 };
    0
}

unsafe extern "C" fn platform_pci_read_config_word(
    device: *mut c_void,
    offset: u32,
    value: *mut u16,
) -> c_int {
    let (Some(bdf), Ok(offset)) = (bdf_of(device), u16::try_from(offset)) else {
        return -EINVAL;
    };
    if offset & 1 != 0 {
        return -EINVAL;
    }
    let Some(word) = config_read_u16(bdf, offset) else {
        return -EIO;
    };
    // SAFETY: valid destination per the platform wrapper contract.
    unsafe { *value = word };
    0
}

unsafe extern "C" fn platform_pci_write_config_byte(
    device: *mut c_void,
    offset: u32,
    value: u8,
) -> c_int {
    let (Some(bdf), Ok(offset)) = (bdf_of(device), u16::try_from(offset)) else {
        return -EINVAL;
    };
    let word_offset = offset & !1;
    // A byte write to STATUS would be a read-modify-write that echoes its
    // write-one-to-clear bits. No translated caller needs that, so refuse it.
    if word_offset == PCI_STATUS {
        return -EOPNOTSUPP;
    }
    let shift = 8 * u32::from(offset & 1);
    let Some(word) = config_read_u16(bdf, word_offset) else {
        return -EIO;
    };
    let merged = (word & !(0xff << shift)) | (u16::from(value) << shift);
    match config_write_u16(bdf, word_offset, merged) {
        Some(()) => 0,
        None => -EIO,
    }
}

/// System suspend/resume are not supported by TheKernel's GT owner. The
/// generic parameter lets the static table take the owner's pointer type
/// without naming the `tk-intel-gt` private layout.
unsafe extern "C" fn platform_irq_suspend<T>(_i915: *mut T) {
    panic!("intel_irq_suspend: system suspend of the upstream GT path is not supported");
}

unsafe extern "C" fn platform_irq_resume<T>(_i915: *mut T) {
    panic!("intel_irq_resume: system resume of the upstream GT path is not supported");
}

/// Linux: `intel_overlay_reset()` returns at once when the device has no
/// overlay. The overlay is absent on every supported platform (see
/// [`intel_overlay_available`]), so there is nothing to reset.
unsafe extern "C" fn platform_overlay_reset(_display: *mut c_void) {}

/// The module parameter that forces a display reset test does not exist in
/// TheKernel; its default, `false`, is what Linux uses.
unsafe extern "C" fn platform_display_reset_test(_display: *mut c_void) -> bool {
    false
}

/// TheKernel's display owner has no reset-with-modeset path. Upstream checks
/// this first and skips `prepare`/`finish` when it is `false`, which is the
/// supported route.
unsafe extern "C" fn platform_display_reset_supported(_display: *mut c_void) -> bool {
    false
}

unsafe extern "C" fn platform_display_reset_prepare(_display: *mut c_void) {
    panic!("intel_display_reset_prepare: TheKernel has no display reset; upstream must check intel_display_reset_supported() first");
}

unsafe extern "C" fn platform_display_reset_finish(_display: *mut c_void, _reset: bool) {
    panic!("intel_display_reset_finish: TheKernel has no display reset; upstream must check intel_display_reset_supported() first");
}

/// CI taint flags have no kernel taint mask in TheKernel. Record the event so
/// a CI run still shows it.
unsafe extern "C" fn platform_add_taint_for_ci<T>(_i915: *mut T, flag: u32) {
    axlog::warn!("i915: CI taint flag {flag:#x} requested; TheKernel has no taint mask");
}

/// Kobject uevents have no delivery path in TheKernel. Log the action and
/// report success, as a kernel without uevent listeners does.
unsafe extern "C" fn platform_kobject_uevent_env(
    _kobj: *mut c_void,
    action: i32,
    _envp: *const *const c_char,
) -> c_int {
    axlog::info!("i915: uevent action {action} has no listener in TheKernel");
    0
}

/// DRM wedged events have no delivery path in TheKernel. Recovery is driven
/// by the GT owner's own state, so the event is logged only.
unsafe extern "C" fn platform_drm_dev_wedged_event(
    _drm: *mut c_void,
    recovery: u32,
    _data: *mut c_void,
) {
    axlog::warn!("i915: device wedged (recovery {recovery:#x}); no uevent delivery");
}

static PLATFORM_OPS: I915PlatformOps = I915PlatformOps {
    to_pci_dev: platform_to_pci_dev,
    pci_read_config_byte: platform_pci_read_config_byte,
    pci_write_config_byte: platform_pci_write_config_byte,
    pci_read_config_word: platform_pci_read_config_word,
    irq_suspend: platform_irq_suspend,
    irq_resume: platform_irq_resume,
    overlay_reset: platform_overlay_reset,
    display_reset_test: platform_display_reset_test,
    display_reset_supported: platform_display_reset_supported,
    display_reset_prepare: platform_display_reset_prepare,
    display_reset_finish: platform_display_reset_finish,
    add_taint_for_ci: platform_add_taint_for_ci,
    kobject_uevent_env: platform_kobject_uevent_env,
    drm_dev_wedged_event: platform_drm_dev_wedged_event,
};

/// Acknowledged all-CPU invalidation of shared kernel mappings, for the
/// upstream WC iomap and vmap paths. The kernel's own owner provides the
/// rendezvous; this returns once every CPU has acknowledged it.
unsafe extern "C" fn kernel_map_tlb_sync() {
    drop(crate::mm::tlb::synchronize_kernel_map_tlb());
}

/// Install every kernel-owned provider this module can supply. Called once
/// from `gt::init_at_boot()`. Reinstalling the same table is accepted.
pub(super) fn install_providers() -> Result<(), &'static str> {
    install_i915_platform_ops(&PLATFORM_OPS)?;
    axmm::install_kernel_map_tlb_sync(kernel_map_tlb_sync)
        .map_err(|_| "kernel-map TLB synchronizer already installed with another owner")?;
    intel_gt::linux_pm::install_runtime_pm_ops(&RUNTIME_PM_OPS)?;
    intel_gt::linux::signal::install_signal_pending_state(kernel_signal_pending_state)?;
    let reader: intel_gt::linux::i915::I915PciRevisionReader =
        // SAFETY: both types are `unsafe extern "C" fn` pointers of identical
        // ABI; the reader ignores its `i915` argument (single GT device).
        unsafe { core::mem::transmute(kernel_pci_revision as unsafe extern "C" fn(*mut c_void, *mut u8) -> i32) };
    intel_gt::linux::i915::install_pci_revision_reader(Some(reader));
    Ok(())
}

// ---------------------------------------------------------------------------
// Task signal state (`linux/signal.rs`)
// ---------------------------------------------------------------------------

const TASK_INTERRUPTIBLE: c_int = 0x0001;
const TASK_WAKEKILL: c_int = 0x0100;

/// Linux `signal_pending_state()`: an interruptible sleep is interrupted by any
/// deliverable signal; a `TASK_WAKEKILL` sleep only by a fatal one. Only the
/// calling task can be asked (upstream waits never name another task), so any
/// other pointer fails closed.
unsafe extern "C" fn kernel_signal_pending_state(state: c_int, task: *mut c_void) -> bool {
    use crate::task::AsThread;

    if state & (TASK_INTERRUPTIBLE | TASK_WAKEKILL) == 0 {
        return false;
    }
    let current = axtask::current();
    assert!(
        ptr::eq(alloc::sync::Arc::as_ptr(&current.clone()).cast::<c_void>(), task.cast_const()),
        "signal_pending_state: upstream GT may only query the current task"
    );
    // A kernel thread has no signal state, so nothing can be pending on it.
    let Some(thread) = current.try_as_thread() else {
        return false;
    };
    if !crate::task::has_pending_syscall_signal(thread) {
        return false;
    }
    state & TASK_INTERRUPTIBLE != 0 || crate::task::has_pending_fatal_signal(thread)
}

// ---------------------------------------------------------------------------
// Runtime PM (`linux/pm.rs`)
// ---------------------------------------------------------------------------

/// Outstanding runtime-PM references on the GT device. TheKernel never
/// runtime-suspends the GPU, so a reference only has to pair with its put;
/// the device stays powered for as long as any reference exists and always.
static RUNTIME_PM_DEPTH: AtomicUsize = AtomicUsize::new(0);
/// Opaque, non-null wakeref cookie. Linux only hands it back to the put.
static RUNTIME_PM_COOKIE: u8 = 0;

unsafe fn kernel_runtime_get(rpm: *mut c_void) -> intel_gt::intel_context_upstream::IntelWakerefHandle {
    assert!(!rpm.is_null(), "runtime PM get on a null device");
    RUNTIME_PM_DEPTH.fetch_add(1, Ordering::AcqRel);
    ptr::addr_of!(RUNTIME_PM_COOKIE).cast_mut().cast()
}

unsafe fn kernel_runtime_put(
    rpm: *mut c_void,
    wakeref: intel_gt::intel_context_upstream::IntelWakerefHandle,
) {
    assert!(!rpm.is_null() && !wakeref.is_null(), "runtime PM put with a null handle");
    RUNTIME_PM_DEPTH
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| depth.checked_sub(1))
        .unwrap_or_else(|_| panic!("runtime PM put without a matching get"));
}

static RUNTIME_PM_OPS: intel_gt::linux_pm::RuntimePmOps = intel_gt::linux_pm::RuntimePmOps {
    runtime_get: kernel_runtime_get,
    runtime_put: kernel_runtime_put,
};

// ---------------------------------------------------------------------------
// PCI revision (`linux/i915.rs`)
// ---------------------------------------------------------------------------

/// PCI config-space REVISION_ID (byte 0x08 of the class/revision dword).
const PCI_REVISION_ID: u16 = 0x08;

/// Linux `INTEL_REVID` source: the revision byte of the GT's own PCI function.
unsafe extern "C" fn kernel_pci_revision(_i915: *mut c_void, revision: *mut u8) -> c_int {
    assert!(!revision.is_null(), "PCI revision read into a null buffer");
    let Some(bdf) = *GT_PCI_IDENTITY.lock() else {
        return -ENODEV;
    };
    let Some(dword) = config_read_u32(bdf, PCI_REVISION_ID & !0x3) else {
        return -ENODEV;
    };
    let shift = u32::from(PCI_REVISION_ID & 0x3) * 8;
    unsafe { *revision = (dword >> shift) as u8 };
    0
}

// ---------------------------------------------------------------------------
// Display symbols declared by the upstream GT path
// ---------------------------------------------------------------------------

/// The primary plane's largest stride is the most the display stack can scan
/// out. Dumb buffers are linear, so that is the bound `DRM_IOCTL_MODE_CREATE_DUMB`
/// may accept. The supported platforms always have a display.
// upstream: display/intel_display.c intel_dumb_fb_max_stride()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dumb_fb_max_stride(
    _dev: *mut c_void,
    _pixel_format: u32,
    _modifier: u64,
) -> u32 {
    super::fb::MAX_STRIDE
}

// upstream: display/intel_display.c intel_has_pending_fb_unpin()
/// TheKernel's flips are synchronous: a framebuffer is not unpinned after the
/// commit returns, so no fence is held across a flip. The deferred cursor
/// page release is not a fence, and the fence allocator does not wait for it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_pending_fb_unpin(_display: *mut c_void) -> bool {
    false
}

/// `enum intel_display_power_domain` value of `POWER_DOMAIN_GT_IRQ` in Linux
/// 7.2.3 `intel_display_power.h` (sequential enum, no explicit values). It is
/// the only display domain the GT path takes: `__gt_unpark()` holds it while
/// the GT is awake so DC states cannot delay GT interrupts.
const POWER_DOMAIN_GT_IRQ: i32 = 72;

/// Display register window the power-domain owner programs. Stage two sets it
/// together with the upstream device; until then GT power references refuse.
static DISPLAY_WINDOW: spin::Mutex<Option<super::regs::RegisterWindow>> = spin::Mutex::new(None);

#[allow(dead_code)] // set by the stage-two probe owner
pub(super) fn register_display_window(window: super::regs::RegisterWindow) {
    *DISPLAY_WINDOW.lock() = Some(window);
}

fn gt_irq_domain(domain: i32) -> intel_display::power_map::PowerDomain {
    assert!(
        domain == POWER_DOMAIN_GT_IRQ,
        "display power domain {domain}: the upstream GT path only takes POWER_DOMAIN_GT_IRQ"
    );
    intel_display::power_map::PowerDomain::GtIrq
}

/// Take a reference on the display `GT_IRQ` domain through the kernel's single
/// power-domain owner (`POWER`), so the GT shares the display refcount instead
/// of writing power-well registers itself. The returned wakeref is an opaque
/// non-null cookie; Linux only compares it and hands it back to the put.
// upstream: display/intel_display_power.c intel_display_power_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_power_get(_display: *mut c_void, domain: i32) -> *mut c_void {
    let domain = gt_irq_domain(domain);
    let window = DISPLAY_WINDOW
        .lock()
        .expect("intel_display_power_get: display register window is not registered");
    let mut power = super::POWER.lock();
    let state = power
        .as_mut()
        .expect("intel_display_power_get: display power owner is not initialized");
    state
        .get_domain(&window, domain)
        .unwrap_or_else(|e| panic!("intel_display_power_get(GT_IRQ): {e:?}"));
    ptr::NonNull::<u8>::dangling().as_ptr().cast()
}

/// Linux defers the final put of an async release by `delay_ms` so a quick
/// re-unpark does not toggle DC states. The kernel owner has no delayed-put
/// worker, so the reference is dropped immediately: the same final refcount,
/// only an earlier possible DC entry.
// upstream: display/intel_display_power.c __intel_display_power_put_async()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_display_power_put_async(
    _display: *mut c_void,
    domain: i32,
    wakeref: *mut c_void,
    _delay_ms: i32,
) {
    assert!(!wakeref.is_null(), "__intel_display_power_put_async: null wakeref");
    let domain = gt_irq_domain(domain);
    let window = DISPLAY_WINDOW
        .lock()
        .expect("__intel_display_power_put_async: display register window is not registered");
    let mut power = super::POWER.lock();
    let state = power
        .as_mut()
        .expect("__intel_display_power_put_async: display power owner is not initialized");
    state
        .put_domain(&window, domain)
        .unwrap_or_else(|e| panic!("__intel_display_power_put_async(GT_IRQ): {e:?}"));
}

// upstream: display/intel_frontbuffer.c intel_frontbuffer_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_init(_front: *mut c_void, _drm: *mut c_void) {
    panic!("intel_frontbuffer_init: TheKernel's scanout owner does not track upstream frontbuffers");
}

// upstream: display/intel_frontbuffer.c intel_frontbuffer_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_fini(_front: *mut c_void) {
    panic!("intel_frontbuffer_fini: no frontbuffer exists because intel_frontbuffer_init fails closed");
}

// upstream: display/intel_frontbuffer.c __intel_frontbuffer_flush()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_frontbuffer_flush(_front: *mut c_void, _origin: c_int, _bits: u32) {
    panic!("__intel_frontbuffer_flush: no frontbuffer exists because intel_frontbuffer_init fails closed");
}

// upstream: display/intel_frontbuffer.c __intel_frontbuffer_invalidate()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_frontbuffer_invalidate(
    _front: *mut c_void,
    _origin: c_int,
    _bits: u32,
) {
    panic!("__intel_frontbuffer_invalidate: no frontbuffer exists because intel_frontbuffer_init fails closed");
}

// upstream: display/intel_overlay.c intel_overlay_available()
/// Overlay hardware is absent on ADL-N and every other supported platform, so
/// the translated `display->overlay` is always NULL and this returns `false`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_overlay_available(_display: *mut c_void) -> bool {
    false
}

// upstream: display/intel_display_rps.c ilk_display_rps_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_display_rps_enable(_display: *mut c_void) {
    panic!("ilk_display_rps_enable: Ironlake DE_PCU RPS is outside TheKernel's Gen12+ matrix");
}

// upstream: display/intel_display_rps.c ilk_display_rps_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_display_rps_disable(_display: *mut c_void) {
    panic!("ilk_display_rps_disable: Ironlake DE_PCU RPS is outside TheKernel's Gen12+ matrix");
}

// upstream: display/vlv_clock.c vlv_clock_get_czclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_clock_get_czclk(_drm: *mut c_void) -> u32 {
    panic!("vlv_clock_get_czclk: Valleyview/Cherryview are not supported by TheKernel");
}

// upstream: display/vlv_clock.c vlv_clock_get_gpll()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlv_clock_get_gpll(_drm: *mut c_void) -> u16 {
    panic!("vlv_clock_get_gpll: Valleyview/Cherryview are not supported by TheKernel");
}

// ---------------------------------------------------------------------------
// Configuration-off Linux behaviour
// ---------------------------------------------------------------------------

// upstream: gt/intel_gt_sysfs.c intel_gt_sysfs_register()
/// TheKernel has no sysfs or kobject tree, so the GT's sysfs directory is not
/// created. This is the sysfs case the translation rules allow to be empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_sysfs_register(_gt: *mut c_void) {}

// upstream: gt/intel_gt_sysfs.c intel_gt_sysfs_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_sysfs_unregister(_gt: *mut c_void) {}

// upstream: i915_hwmon.h i915_hwmon_power_max_disable() (CONFIG_HWMON off)
/// With `CONFIG_HWMON` off, Linux's header provides an empty inline. The
/// caller's `was_enabled` is left as the caller set it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_hwmon_power_max_disable(_i915: *mut c_void, _was_enabled: *mut bool) {}

// upstream: i915_hwmon.h i915_hwmon_power_max_restore() (CONFIG_HWMON off)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_hwmon_power_max_restore(_i915: *mut c_void, _was_enabled: bool) {}

// upstream: i915_pmu.h i915_pmu_gt_parked() (CONFIG_PERF_EVENTS off)
/// With `CONFIG_PERF_EVENTS` off, Linux's header provides an empty inline.
/// TheKernel registers no PMU, so `pmu->registered` is never set and the
/// upstream function would return at its first check.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_pmu_gt_parked(_gt: *mut c_void) {}

// upstream: i915_pmu.h i915_pmu_gt_unparked() (CONFIG_PERF_EVENTS off)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_pmu_gt_unparked(_gt: *mut c_void) {}
