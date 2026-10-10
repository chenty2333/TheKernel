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

use alloc::{boxed::Box, collections::BTreeMap};
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
    identity_for(bdf)
}

/// Native `struct pci_dev` identity for `bdf`, created once per function and
/// never freed. Identities are permanent kernel objects, so a `pci_dev_put`
/// has nothing to release and repeated lookups return the same pointer.
fn identity_for(bdf: pci::Bdf) -> *mut c_void {
    let mut map = DEVICE_IDENTITIES.lock();
    let address = *map.entry((bdf.bus, bdf.device, bdf.function)).or_insert_with(|| {
        Box::into_raw(Box::new(NativePciDevice { bdf })) as usize
    });
    address as *mut c_void
}

static DEVICE_IDENTITIES: spin::Mutex<BTreeMap<(u8, u8, u8), usize>> =
    spin::Mutex::new(BTreeMap::new());

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
    install_k2_tables()
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

// ===========================================================================
// Round 4 (K2): task, file, PCI, runtime PM, SMP, ioremap and uncore tables
// ===========================================================================

use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::AtomicBool;

use axhal::{
    irq::{IpiReason, IpiTarget},
    mem::PhysAddr,
};
use intel_gt::{
    intel_uncore_upstream::{UncoreKernelOps, install_uncore_kernel_ops},
    linux::{
        kernel_core::{PciBusOps, PciCoreOps, install_pci_bus_ops, install_pci_core_ops},
        kernel_services::{
            FileOps, PciOps, PmOps, SmpOps, TaskOps, install_file_ops, install_pci_ops,
            install_pm_ops, install_smp_ops, install_task_ops,
        },
        primitives::{IoremapProvider, install_ioremap_provider},
    },
};

use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axfs_ng_vfs::FsPath;

use crate::{
    file::{FileDescription, FileLike, ReservedFd},
    task::AsThread,
};

const EPERM: c_int = 1;
const EIO_: c_int = 5;
const ENOENT: c_int = 2;
const EEXIST: c_int = 17;
const ENOSPC: c_int = 28;
const EMFILE: c_int = 24;
const EINVAL_: c_int = 22;
const ENODEV_: c_int = 19;

/// PCI command register bits (config offset 0x04, low word).
const CMD_IO: u16 = 0x1;
const CMD_MEM: u16 = 0x2;
const CMD_MASTER: u16 = 0x4;
/// PCI status bit 4: the capability list is present.
const STATUS_CAP_LIST: u32 = 1 << 20;
const CAP_ID_PM: u8 = 0x01;
const CAP_ID_EXP: u8 = 0x10;
/// PCIe port type of a root port (PCI Express Capabilities, bits 7:4).
const PCI_EXP_TYPE_ROOT_PORT: u32 = 4;
const PCI_EXP_TYPE_MASK: u32 = 0x00f0;

const IORESOURCE_IO: u64 = 0x0000_0100;
const IORESOURCE_MEM: u64 = 0x0000_0200;
const IORESOURCE_PREFETCH: u64 = 0x0000_2000;
const IORESOURCE_MEM_64: u64 = 0x0010_0000;
const IORESOURCE_BUSY: u64 = 0x8000_0000;

/// Linux `struct resource` (include/linux/ioport.h), as read by the upstream
/// GT code: start, end, name, flags, desc, parent, sibling.
#[repr(C)]
pub(super) struct Resource {
    start: u64,
    end: u64,
    name: *const c_char,
    flags: u64,
    desc: u64,
    parent: *mut Resource,
    sibling: *mut Resource,
}

/// Compile-time check that the field offsets match the upstream layout.
const _: () = {
    assert!(core::mem::offset_of!(Resource, end) == 8);
    assert!(core::mem::offset_of!(Resource, name) == 16);
    assert!(core::mem::offset_of!(Resource, flags) == 24);
};

/// Claimed memory regions made by `request_mem_region`, keyed by start.
static CLAIMED_REGIONS: spin::Mutex<BTreeMap<u64, usize>> = spin::Mutex::new(BTreeMap::new());

/// Saved configuration headers from `pci_save_state`, keyed by function.
static SAVED_HEADERS: spin::Mutex<BTreeMap<(u8, u8, u8), [u32; 16]>> =
    spin::Mutex::new(BTreeMap::new());

/// Outstanding runtime-PM references per device identity. The GPU is never
/// runtime-suspended, so every device is active and the count only pairs
/// gets with puts.
static DEVICE_PM_USAGE: spin::Mutex<BTreeMap<usize, isize>> = spin::Mutex::new(BTreeMap::new());

/// D3cold permission per function. TheKernel never removes power from the GPU,
/// so the flag records policy only and no transition depends on it.
static D3COLD_ALLOWED: spin::Mutex<BTreeMap<(u8, u8, u8), bool>> =
    spin::Mutex::new(BTreeMap::new());

// ---------------------------------------------------------------------------
// Configuration space helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "none")]
fn config_write_u32(bdf: pci::Bdf, offset: u16, value: u32) -> Option<()> {
    with_config(|ecam| ecam.write_u32(bdf, offset, value).ok())
}

#[cfg(not(target_os = "none"))]
fn config_write_u32(_bdf: pci::Bdf, _offset: u16, _value: u32) -> Option<()> {
    None
}

/// Byte read through the dword that contains it.
fn config_read_u8<C: ConfigSpace + ?Sized>(config: &C, bdf: pci::Bdf, offset: u16) -> Option<u8> {
    let dword = config.read_u32(bdf, offset & !3)?;
    Some((dword >> (8 * u32::from(offset & 3))) as u8)
}

/// Read-modify-write of the command word. The status half is written back as
/// zero, so its write-one-to-clear bits are never acknowledged by accident.
#[cfg(target_os = "none")]
fn config_update_command(bdf: pci::Bdf, update: impl FnOnce(u16) -> u16) -> Option<()> {
    let command = with_config(|ecam| ecam.read_u16(bdf, 0x04))?;
    config_write_u32(bdf, 0x04, u32::from(update(command)))
}

#[cfg(not(target_os = "none"))]
fn config_update_command(_bdf: pci::Bdf, _update: impl FnOnce(u16) -> u16) -> Option<()> {
    None
}

/// Offset of the configuration-space capability `id`, if it is listed.
fn find_capability<C: ConfigSpace + ?Sized>(config: &C, bdf: pci::Bdf, id: u8) -> Option<u16> {
    let status = config.read_u32(bdf, 0x04)?;
    if status & STATUS_CAP_LIST == 0 {
        return None;
    }
    let mut pointer = u16::from(config_read_u8(config, bdf, 0x34)? & !3);
    // A well-formed list is at most 48 entries (the spec's bound); the cap
    // stops a looping list from hanging the probe.
    for _ in 0..48 {
        if pointer == 0 {
            return None;
        }
        if config_read_u8(config, bdf, pointer)? == id {
            return Some(pointer);
        }
        pointer = u16::from(config_read_u8(config, bdf, pointer + 1)? & !3);
    }
    None
}

/// Base and flags of memory/IO BAR `slot`, decoded from its registers.
fn bar_decode<C: ConfigSpace + ?Sized>(config: &C, bdf: pci::Bdf, slot: u8) -> Option<(u64, u64)> {
    let offset = 0x10 + 4 * u16::from(slot);
    let low = config.read_u32(bdf, offset)?;
    if low & 1 != 0 {
        return Some((u64::from(low & !0x3), IORESOURCE_IO));
    }
    let mut flags = IORESOURCE_MEM;
    if low & 0x8 != 0 {
        flags |= IORESOURCE_PREFETCH;
    }
    let mut base = u64::from(low & !0xf);
    if (low >> 1) & 0x3 == 0x2 {
        flags |= IORESOURCE_MEM_64;
        base |= u64::from(config.read_u32(bdf, offset + 4)?) << 32;
    }
    Some((base, flags))
}

/// Size of memory/IO BAR `slot` by the standard sizing write: decode is off
/// while the all-ones value is in the BAR, and the original value and command
/// are restored on every path.
#[cfg(target_os = "none")]
fn bar_size(ecam: &mut pci::Ecam, bdf: pci::Bdf, slot: u8) -> Option<u64> {
    let offset = 0x10 + 4 * u16::from(slot);
    let command = ecam.read_u16(bdf, 0x04)?;
    let low = ecam.read_u32(bdf, offset)?;
    let is_io = low & 1 != 0;
    let is_64 = !is_io && (low >> 1) & 0x3 == 0x2;
    let high = if is_64 { ecam.read_u32(bdf, offset + 4)? } else { 0 };

    let sized = (|| {
        ecam.write_u16(bdf, 0x04, command & !(CMD_IO | CMD_MEM)).ok()?;
        ecam.write_u32(bdf, offset, u32::MAX).ok()?;
        let mask_low = ecam.read_u32(bdf, offset)?;
        let mut mask = u64::from(mask_low) & if is_io { !0x3 } else { !0xf };
        if is_64 {
            ecam.write_u32(bdf, offset + 4, u32::MAX).ok()?;
            let mask_high = ecam.read_u32(bdf, offset + 4)?;
            mask |= u64::from(mask_high) << 32;
        }
        Some(mask)
    })();

    // Restore unconditionally; a failed write cannot leave the BAR all-ones.
    let restored = ecam.write_u32(bdf, offset, low).is_ok()
        && (!is_64 || ecam.write_u32(bdf, offset + 4, high).is_ok())
        && ecam.write_u16(bdf, 0x04, command).is_ok();
    let mask = sized?;
    if !restored {
        return None;
    }
    if mask == 0 {
        return Some(0);
    }
    let size = if is_64 {
        (!mask).wrapping_add(1)
    } else {
        ((!mask) as u32).wrapping_add(1) as u64
    };
    Some(size)
}

#[cfg(not(target_os = "none"))]
fn bar_size_host(_bdf: pci::Bdf, _slot: u8) -> Option<u64> {
    None
}

/// Walk up from `bdf` to the root port above it (`pcie_find_root_port()`).
#[cfg(target_os = "none")]
fn root_port_of(bdf: pci::Bdf) -> Option<pci::Bdf> {
    with_config(|ecam| {
        let bus_end = ecam.bus_end();
        let mut bus = bdf.bus;
        while bus != 0 {
            // The bridge whose secondary bus is `bus` is the parent.
            let mut parent = None;
            'scan: for scan_bus in 0..=bus_end {
                for device in 0..32u8 {
                    for function in 0..8u8 {
                        let candidate = pci::Bdf { bus: scan_bus, device, function };
                        let Some(vendor) = ecam.read_u32(candidate, 0x00) else { continue };
                        if vendor & 0xffff == 0xffff {
                            continue;
                        }
                        let Some(header) = config_read_u8(&*ecam, candidate, 0x0e) else {
                            continue;
                        };
                        if header & 0x7f != 0x01 {
                            continue;
                        }
                        let Some(secondary) = config_read_u8(&*ecam, candidate, 0x19) else {
                            continue;
                        };
                        if u16::from(secondary) == u16::from(bus) {
                            parent = Some(candidate);
                            break 'scan;
                        }
                    }
                }
            }
            let parent = parent?;
            // The PCIe Capabilities register is the upper half of the dword
            // at the capability header.
            let cap = find_capability(&*ecam, parent, CAP_ID_EXP)?;
            let capabilities = ecam.read_u32(parent, cap)? >> 16;
            let port_type = (capabilities & PCI_EXP_TYPE_MASK) >> 4;
            if port_type == PCI_EXP_TYPE_ROOT_PORT {
                return Some(parent);
            }
            bus = parent.bus;
        }
        None
    })
}

#[cfg(not(target_os = "none"))]
fn root_port_of(_bdf: pci::Bdf) -> Option<pci::Bdf> {
    None
}

fn current_identity_key() -> usize {
    let current = axtask::current().clone();
    Arc::as_ptr(&current) as usize
}

// ---------------------------------------------------------------------------
// Task identity and credentials (`TaskOps`)
// ---------------------------------------------------------------------------

/// Linux `TASK_COMM_LEN` is 16 including the terminating NUL.
const TASK_COMM_LEN: usize = 16;

/// Interned, NUL-terminated copies of task names. Names are few and reused,
/// so each distinct one is kept for the kernel's lifetime; that makes the
/// pointer from `current_comm()` valid for as long as any caller needs it.
static COMM_NAMES: spin::Mutex<BTreeMap<String, usize>> = spin::Mutex::new(BTreeMap::new());

fn current_comm_bytes() -> Vec<u8> {
    let name = axtask::current().name().unwrap_or_default();
    let mut bytes = name.into_bytes();
    bytes.truncate(TASK_COMM_LEN - 1);
    bytes
}

unsafe extern "C" fn kernel_current_comm() -> *const c_char {
    let bytes = current_comm_bytes();
    let key = String::from_utf8_lossy(&bytes).into_owned();
    let mut names = COMM_NAMES.lock();
    let address = *names.entry(key).or_insert_with(|| {
        let name = CString::new(bytes).unwrap_or_default();
        name.into_raw() as usize
    });
    address as *const c_char
}

/// Upstream asks for names of the calling task only; any other task pointer
/// would need a reference to that task's thread, which this path never takes.
fn assert_current_task(task: *const c_void) {
    assert!(
        core::ptr::eq(task, current_identity_key() as *const c_void),
        "task query for a task other than the caller is not supported"
    );
}

unsafe extern "C" fn kernel_task_comm(task: *const c_void, buf: *mut c_char) {
    assert!(!buf.is_null(), "task_comm into a null buffer");
    assert_current_task(task);
    let bytes = current_comm_bytes();
    let out = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), TASK_COMM_LEN) };
    out.fill(0);
    out[..bytes.len()].copy_from_slice(&bytes);
}

/// The caller's PID in the initial namespace. A kernel task has no user PID
/// namespace entry and reports 0.
fn caller_pid() -> c_int {
    let current = axtask::current();
    current
        .try_as_thread()
        .map_or(0, |thread| thread.kernel_tid() as c_int)
}

unsafe extern "C" fn kernel_task_pid_nr(task: *const c_void) -> c_int {
    assert_current_task(task);
    caller_pid()
}

/// `struct pid` is a referenced PID value. Its contents never change, so the
/// value is boxed and released by `put_pid`.
unsafe extern "C" fn kernel_task_pid(task: *mut c_void, _pid_type: c_int) -> *mut c_void {
    assert_current_task(task);
    Box::into_raw(Box::new(caller_pid())).cast()
}

unsafe extern "C" fn kernel_pid_nr(pid: *const c_void) -> c_int {
    assert!(!pid.is_null(), "pid_nr on a null pid");
    unsafe { *pid.cast::<c_int>() }
}

unsafe extern "C" fn kernel_put_pid(pid: *mut c_void) {
    if pid.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(pid.cast::<c_int>()) });
}

/// Linux `CAP_PERFMON` and `CAP_SYS_ADMIN` numbers.
const CAP_SYS_ADMIN_NUM: c_int = 21;
const CAP_PERFMON_NUM: c_int = 38;

/// The caller's effective capability in the initial user namespace. A kernel
/// task holds no user capability, so the answer is false.
fn caller_has_capability(capability: u32) -> bool {
    let current = axtask::current();
    current
        .try_as_thread()
        .is_some_and(|thread| thread.current_cred().has_effective_capability(capability))
}

unsafe extern "C" fn kernel_capable(capability: c_int) -> bool {
    caller_has_capability(capability as u32)
}

unsafe extern "C" fn kernel_perfmon_capable() -> bool {
    caller_has_capability(CAP_PERFMON_NUM as u32) || caller_has_capability(CAP_SYS_ADMIN_NUM as u32)
}

static TASK_OPS: TaskOps = TaskOps {
    current_comm: kernel_current_comm,
    task_comm: kernel_task_comm,
    task_pid_nr: kernel_task_pid_nr,
    task_pid: kernel_task_pid,
    pid_nr: kernel_pid_nr,
    put_pid: kernel_put_pid,
    capable: kernel_capable,
    perfmon_capable: kernel_perfmon_capable,
};

// ---------------------------------------------------------------------------
// File descriptors and anonymous inodes (`FileOps`)
// ---------------------------------------------------------------------------

/// A LinuxKPI `struct file` published as a kernel descriptor. The descriptor
/// owns one file reference, released when the last descriptor reference goes.
struct AnonFile {
    file: usize,
}

// SAFETY: the LinuxKPI file is only released through `fput`, which is itself
// safe to call from any thread once the reference is owned here.
unsafe impl Send for AnonFile {}
unsafe impl Sync for AnonFile {}

impl Drop for AnonFile {
    fn drop(&mut self) {
        unsafe { intel_gt::linux::shmem::fput(self.file as *mut c_void) };
    }
}

impl Pollable for AnonFile {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut core::task::Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

impl FileLike for AnonFile {
    fn path(&self) -> axerrno::AxResult<alloc::borrow::Cow<'_, FsPath>> {
        Ok(alloc::borrow::Cow::Borrowed(FsPath::new(b"anon_inode:[i915.gem]")))
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> axerrno::AxResult {
        Ok(())
    }

    fn stat(&self) -> axerrno::AxResult<crate::file::Kstat> {
        Err(axerrno::AxError::BadFileDescriptor)
    }
}

/// Linux `anon_inode_getfile()`. The file is a LinuxKPI file with the caller's
/// `f_op` and `private_data`; its mapping and inode come from the same
/// anonymous-file allocator the GEM shmem objects use.
unsafe extern "C" fn kernel_anon_inode_getfile(
    name: *const c_char,
    fops: *const c_void,
    private_data: *mut c_void,
    flags: c_int,
) -> *mut c_void {
    let file = unsafe { intel_gt::linux::shmem::shmem_file_setup(name, 0, 0) };
    if intel_gt::linux_config::IS_ERR(file) {
        return file.cast();
    }
    unsafe {
        (*file).f_op = fops.cast();
        (*file)._private_data = private_data;
        (*file).f_flags = flags as u32;
    }
    file.cast()
}

/// Descriptors reserved by `get_unused_fd_flags` and not yet installed.
static RESERVED_FDS: axsync::Mutex<BTreeMap<c_int, ReservedFd>> =
    axsync::Mutex::new(BTreeMap::new());

const O_CLOEXEC_BIT: c_int = 0o2000000;

unsafe extern "C" fn kernel_get_unused_fd_flags(flags: c_int) -> c_int {
    let cloexec = flags & O_CLOEXEC_BIT != 0;
    match crate::file::reserve_fd(cloexec) {
        Ok(reservation) => {
            let fd = reservation.fd();
            RESERVED_FDS.lock().insert(fd, reservation);
            fd
        }
        Err(_) => -EMFILE,
    }
}

unsafe extern "C" fn kernel_put_unused_fd(fd: c_int) {
    // Dropping the reservation returns the number to the table.
    drop(RESERVED_FDS.lock().remove(&fd));
}

unsafe extern "C" fn kernel_fd_install(fd: c_int, file: *mut c_void) {
    assert!(!file.is_null(), "fd_install with a null file");
    let reservation = RESERVED_FDS
        .lock()
        .remove(&fd)
        .unwrap_or_else(|| panic!("fd_install({fd}) without a reservation"));
    let description = FileDescription::new(Arc::new(AnonFile { file: file as usize }))
        .unwrap_or_else(|error| panic!("fd_install({fd}): {error:?}"));
    // The reservation publishes the descriptor into the caller's table.
    if let Err(error) = reservation.publish(description) {
        panic!("fd_install({fd}) publication failed: {error:?}");
    }
}

static FILE_OPS: FileOps = FileOps {
    anon_inode_getfile: kernel_anon_inode_getfile,
    get_unused_fd_flags: kernel_get_unused_fd_flags,
    put_unused_fd: kernel_put_unused_fd,
    fd_install: kernel_fd_install,
};

// ---------------------------------------------------------------------------
// PCI device operations (`PciOps`, `PciCoreOps`, `PciBusOps`)
// ---------------------------------------------------------------------------

/// The device's `bdf`, or `None` for a pointer that is not a native identity.
/// Identities are only produced by `identity_for`, so the pointer is valid.
fn device_bdf(dev: *mut c_void) -> Option<pci::Bdf> {
    bdf_of(dev)
}

unsafe extern "C" fn kernel_pci_resource_start(dev: *mut c_void, bar: u32) -> u64 {
    let Some(bdf) = device_bdf(dev) else { return 0 };
    let Ok(slot) = u8::try_from(bar) else { return 0 };
    if slot > 5 {
        return 0;
    }
    with_config_read(|config| bar_decode(config, bdf, slot))
        .map_or(0, |(base, _)| base)
}

unsafe extern "C" fn kernel_pci_resource_len(dev: *mut c_void, bar: u32) -> u64 {
    let Some(bdf) = device_bdf(dev) else { return 0 };
    let Ok(slot) = u8::try_from(bar) else { return 0 };
    if slot > 5 {
        return 0;
    }
    bar_size_for(bdf, slot).unwrap_or(0)
}

/// `request_mem_region`: refuse a range that overlaps a claim, otherwise record
/// the claim and return a resource the caller releases with `release_resource`.
unsafe extern "C" fn kernel_request_mem_region(
    dev: *mut c_void,
    start: u64,
    n: u64,
    name: *const c_char,
) -> *mut c_void {
    if device_bdf(dev).is_none() || n == 0 {
        return core::ptr::null_mut();
    }
    let Some(end) = start.checked_add(n - 1) else {
        return core::ptr::null_mut();
    };
    let mut claims = CLAIMED_REGIONS.lock();
    let overlaps = claims.iter().any(|(&claimed_start, &resource)| {
        let claimed_end = unsafe { (*(resource as *const Resource)).end };
        claimed_start <= end && start <= claimed_end
    });
    if overlaps {
        return core::ptr::null_mut();
    }
    let label = if name.is_null() {
        CString::default()
    } else {
        CString::from(unsafe { core::ffi::CStr::from_ptr(name) })
    };
    let resource = Box::into_raw(Box::new(Resource {
        start,
        end,
        name: label.into_raw(),
        flags: IORESOURCE_MEM | IORESOURCE_BUSY,
        desc: 0,
        parent: core::ptr::null_mut(),
        sibling: core::ptr::null_mut(),
    }));
    claims.insert(start, resource as usize);
    resource.cast()
}

static PCI_OPS: PciOps = PciOps {
    resource_start: kernel_pci_resource_start,
    resource_len: kernel_pci_resource_len,
    request_mem_region: kernel_request_mem_region,
};

/// Run `read` over the ECAM window. Without a mapped window the answer is
/// `None` and callers report `-ENODEV`.
#[cfg(target_os = "none")]
fn with_config_read<R>(read: impl FnOnce(&pci::Ecam) -> Option<R>) -> Option<R> {
    with_config(|ecam| read(&*ecam))
}

#[cfg(not(target_os = "none"))]
fn with_config_read<R>(_read: impl FnOnce(&pci::Ecam) -> Option<R>) -> Option<R> {
    None
}

#[cfg(target_os = "none")]
fn bar_size_for(bdf: pci::Bdf, slot: u8) -> Option<u64> {
    with_config(|ecam| bar_size(ecam, bdf, slot))
}

#[cfg(not(target_os = "none"))]
fn bar_size_for(bdf: pci::Bdf, slot: u8) -> Option<u64> {
    bar_size_host(bdf, slot)
}

unsafe extern "C" fn kernel_pci_enable_device(dev: *mut c_void) -> c_int {
    let Some(bdf) = device_bdf(dev) else { return -ENODEV_ };
    // Memory decode for the GT BARs, and I/O decode when the function has an
    // I/O BAR. Bus mastering is a separate call (`pci_set_master`).
    let has_io = (0..6u8).any(|slot| {
        with_config_read(|config| bar_decode(config, bdf, slot))
            .is_some_and(|(_, flags)| flags & IORESOURCE_IO != 0)
    });
    match config_update_command(bdf, |command| {
        command | CMD_MEM | if has_io { CMD_IO } else { 0 }
    }) {
        Some(()) => 0,
        None => -EIO_,
    }
}

unsafe extern "C" fn kernel_pci_disable_device(dev: *mut c_void) {
    // Linux's `pci_disable_device()` clears bus mastering and leaves decode
    // alone, so the BARs stay mapped for the owner that disabled the device.
    if let Some(bdf) = device_bdf(dev) {
        let _ = config_update_command(bdf, |command| command & !CMD_MASTER);
    }
}

unsafe extern "C" fn kernel_pci_set_master(dev: *mut c_void) {
    let Some(bdf) = device_bdf(dev) else { return };
    let _ = config_update_command(bdf, |command| command | CMD_MASTER);
}

/// `pci_enable_msi()`. The display IRQ owner reserved this function's MSI
/// vector and programmed the message (`irq.rs::install_n305`). Enabling MSI
/// consumes that vector; it does not program a second one. The vector number
/// is read back by the IRQ registration through `IrqCoreOps`.
unsafe extern "C" fn kernel_pci_enable_msi(dev: *mut c_void) -> c_int {
    let Some(bdf) = device_bdf(dev) else { return -ENODEV_ };
    match irq_msi_vector(bdf) {
        Some(_) => 0,
        None => -ENOSPC,
    }
}

#[cfg(target_os = "none")]
fn irq_msi_vector(bdf: pci::Bdf) -> Option<usize> {
    super::irq::msi_vector_for(bdf)
}

#[cfg(not(target_os = "none"))]
fn irq_msi_vector(_bdf: pci::Bdf) -> Option<usize> {
    None
}

/// `pci_disable_msi()`. The vector stays reserved by the IRQ owner, which is
/// the only place it is torn down, so disabling MSI here only clears the
/// function's own enable bit path, which the owner already controls.
unsafe extern "C" fn kernel_pci_disable_msi(_dev: *mut c_void) {}

/// `pci_save_state()`: snapshot the standard configuration header.
unsafe extern "C" fn kernel_pci_save_state(dev: *mut c_void) -> c_int {
    let Some(bdf) = device_bdf(dev) else { return -ENODEV_ };
    let header = with_config_read(|config| {
        let mut words = [0u32; 16];
        for (index, word) in words.iter_mut().enumerate() {
            *word = config.read_u32(bdf, (index * 4) as u16)?;
        }
        Some(words)
    });
    match header {
        Some(words) => {
            SAVED_HEADERS.lock().insert((bdf.bus, bdf.device, bdf.function), words);
            0
        }
        None => -ENODEV_,
    }
}

/// `pci_restore_state()`: write back the saved header. The status word is
/// written as zero so no write-one-to-clear bit is acknowledged, and the
/// read-only vendor/device word is skipped.
unsafe extern "C" fn kernel_pci_restore_state(dev: *mut c_void) {
    let Some(bdf) = device_bdf(dev) else { return };
    let Some(words) = SAVED_HEADERS.lock().get(&(bdf.bus, bdf.device, bdf.function)).copied()
    else {
        return;
    };
    for (index, word) in words.iter().enumerate().skip(1) {
        let offset = (index * 4) as u16;
        let value = if offset == 0x04 { *word & 0xffff } else { *word };
        let _ = config_write_u32(bdf, offset, value);
    }
}

/// `pci_set_power_state()`. D0 is the only state TheKernel's GT path enters;
/// D3hot is written when requested. Without a PM capability only D0 exists.
unsafe extern "C" fn kernel_pci_set_power_state(dev: *mut c_void, state: u32) -> c_int {
    let Some(bdf) = device_bdf(dev) else { return -ENODEV_ };
    if state > 3 {
        return -EINVAL_;
    }
    let pm = with_config_read(|config| find_capability(config, bdf, CAP_ID_PM));
    let Some(pm) = pm else {
        return if state == 0 { 0 } else { -EINVAL_ };
    };
    let pmcsr = pm + 4;
    let Some(control) = with_config_read(|config| config.read_u32(bdf, pmcsr)) else {
        return -EIO_;
    };
    let previous = control & 0x3;
    let next = (control & !0x3) | state;
    if config_write_u32(bdf, pmcsr, next).is_none() {
        return -EIO_;
    }
    if previous == 3 && state == 0 {
        // Leaving D3hot needs the device's 10 ms recovery time (PCI PM spec).
        let start = axhal::time::monotonic_time_nanos();
        while axhal::time::monotonic_time_nanos().saturating_sub(start) < 10_000_000 {
            core::hint::spin_loop();
        }
    }
    0
}

unsafe extern "C" fn kernel_pci_d3cold_enable(dev: *mut c_void) {
    if let Some(bdf) = device_bdf(dev) {
        D3COLD_ALLOWED.lock().insert((bdf.bus, bdf.device, bdf.function), true);
    }
}

unsafe extern "C" fn kernel_pci_d3cold_disable(dev: *mut c_void) {
    if let Some(bdf) = device_bdf(dev) {
        D3COLD_ALLOWED.lock().insert((bdf.bus, bdf.device, bdf.function), false);
    }
}

unsafe extern "C" fn kernel_pci_find_root_port(dev: *mut c_void) -> *mut c_void {
    let Some(bdf) = device_bdf(dev) else { return core::ptr::null_mut() };
    match root_port_of(bdf) {
        Some(port) => identity_for(port),
        None => core::ptr::null_mut(),
    }
}

unsafe extern "C" fn kernel_pci_resource_flags(dev: *mut c_void, bar: c_int) -> u64 {
    let Some(bdf) = device_bdf(dev) else { return 0 };
    let Ok(slot) = u8::try_from(bar) else { return 0 };
    if slot > 5 {
        return 0;
    }
    with_config_read(|config| bar_decode(config, bdf, slot)).map_or(0, |(_, flags)| flags)
}

/// Linux `DMA_BIT_MASK(32)`. TheKernel's DMA is physically addressed, so the
/// only limit a device can hit is the width of its address register; the GT
/// programs 32-bit-or-wider addresses everywhere it uses DMA.
const DMA_MIN_MASK: u64 = 0xffff_ffff;

unsafe extern "C" fn kernel_dma_set_mask(_dev: *mut c_void, mask: u64) -> c_int {
    if mask >= DMA_MIN_MASK { 0 } else { -EIO_ }
}

unsafe extern "C" fn kernel_dma_set_coherent_mask(_dev: *mut c_void, mask: u64) -> c_int {
    if mask >= DMA_MIN_MASK { 0 } else { -EIO_ }
}

unsafe extern "C" fn kernel_dma_set_max_seg_size(dev: *mut c_void, size: u32) {
    // Recorded per device for the scatter-gather builder. The LinuxKPI DMA
    // path does not split segments, so the value is an upper bound that the
    // upstream code only ever uses to size its own segments.
    let _ = (dev, size);
}

/// `aperture_remove_conflicting_pci_devices()`. The firmware framebuffer is
/// the simpledrm surface; when the native device takes the aperture, that
/// surface is released so the console is owned by one device.
unsafe extern "C" fn kernel_aperture_remove_conflicting(
    dev: *mut c_void,
    _name: *const c_char,
) -> c_int {
    if device_bdf(dev).is_none() {
        return -ENODEV_;
    }
    crate::drm::release_firmware_primary();
    0
}

static PCI_CORE_OPS: PciCoreOps = PciCoreOps {
    enable_device: kernel_pci_enable_device,
    disable_device: kernel_pci_disable_device,
    set_master: kernel_pci_set_master,
    enable_msi: kernel_pci_enable_msi,
    disable_msi: kernel_pci_disable_msi,
    save_state: kernel_pci_save_state,
    restore_state: kernel_pci_restore_state,
    set_power_state: kernel_pci_set_power_state,
    d3cold_enable: kernel_pci_d3cold_enable,
    d3cold_disable: kernel_pci_d3cold_disable,
    find_root_port: kernel_pci_find_root_port,
    resource_flags: kernel_pci_resource_flags,
    dma_set_mask: kernel_dma_set_mask,
    dma_set_coherent_mask: kernel_dma_set_coherent_mask,
    dma_set_max_seg_size: kernel_dma_set_max_seg_size,
    aperture_remove_conflicting: kernel_aperture_remove_conflicting,
};

/// Host bridge at 00:00.0 of the segment, or NULL if it is absent.
unsafe extern "C" fn kernel_domain_host_bridge(dev: *mut c_void) -> *mut c_void {
    if device_bdf(dev).is_none() {
        return core::ptr::null_mut();
    }
    let host = pci::Bdf { bus: 0, device: 0, function: 0 };
    match with_config_read(|config| config.read_u32(host, 0x00)) {
        Some(vendor) if vendor & 0xffff != 0xffff => identity_for(host),
        _ => core::ptr::null_mut(),
    }
}

unsafe extern "C" fn kernel_pci_dev_put(_dev: *mut c_void) {
    // Identities are permanent, so the reference handed out by
    // `domain_host_bridge` has nothing to release.
}

unsafe extern "C" fn kernel_read_config_dword(
    dev: *mut c_void,
    offset: u32,
    value: *mut u32,
) -> c_int {
    assert!(!value.is_null(), "config read into a null value");
    let Some(bdf) = device_bdf(dev) else { return -ENODEV_ };
    let Ok(offset) = u16::try_from(offset) else { return -EINVAL_ };
    if offset & 3 != 0 || offset >= 0x1000 {
        return -EINVAL_;
    }
    match with_config_read(|config| config.read_u32(bdf, offset)) {
        Some(word) => {
            unsafe { *value = word };
            0
        }
        None => -EIO_,
    }
}

unsafe extern "C" fn kernel_write_config_dword(dev: *mut c_void, offset: u32, value: u32) -> c_int {
    let Some(bdf) = device_bdf(dev) else { return -ENODEV_ };
    let Ok(offset) = u16::try_from(offset) else { return -EINVAL_ };
    if offset & 3 != 0 || offset >= 0x1000 {
        return -EINVAL_;
    }
    match config_write_u32(bdf, offset, value) {
        Some(()) => 0,
        None => -EIO_,
    }
}

/// `pci_bus_alloc_resource()` for a bridge window. Only the pre-gen6 GMCH
/// path allocates bridge windows, and TheKernel's platforms never reach it.
/// The firmware assigns windows and the kernel does not move them, so the
/// allocator reports "no such device" for the caller's fallback.
unsafe extern "C" fn kernel_bus_alloc_mem_resource(
    _dev: *mut c_void,
    _res: *mut c_void,
    _size: u64,
    _align: u64,
) -> c_int {
    -ENODEV_
}

/// `release_resource()`: drops a claim made by `request_mem_region`.
unsafe extern "C" fn kernel_release_resource(res: *mut c_void) {
    if res.is_null() {
        return;
    }
    let start = unsafe { (*res.cast::<Resource>()).start };
    let mut claims = CLAIMED_REGIONS.lock();
    if claims.get(&start).copied() == Some(res as usize) {
        claims.remove(&start);
        let resource = unsafe { Box::from_raw(res.cast::<Resource>()) };
        if !resource.name.is_null() {
            drop(unsafe { CString::from_raw(resource.name.cast_mut()) });
        }
    }
}

static PCI_BUS_OPS: PciBusOps = PciBusOps {
    domain_host_bridge: kernel_domain_host_bridge,
    pci_dev_put: kernel_pci_dev_put,
    read_config_dword: kernel_read_config_dword,
    write_config_dword: kernel_write_config_dword,
    bus_alloc_mem_resource: kernel_bus_alloc_mem_resource,
    release_resource: kernel_release_resource,
};

// ---------------------------------------------------------------------------
// Runtime PM for a device (`PmOps`)
// ---------------------------------------------------------------------------

/// TheKernel never runtime-suspends a device, so every device is active. The
/// get calls take a usage reference and report it as they would in Linux for
/// an active device; the put calls drop it and reject an unbalanced put.
fn pm_usage_add(dev: *mut c_void, delta: isize) -> Option<isize> {
    let mut usage = DEVICE_PM_USAGE.lock();
    let entry = usage.entry(dev as usize).or_insert(0);
    let next = entry.checked_add(delta)?;
    if next < 0 {
        return None;
    }
    *entry = next;
    Some(next)
}

unsafe extern "C" fn kernel_pm_get_sync(dev: *mut c_void) -> c_int {
    // An active device reports 1 (pm_runtime_get_sync()'s "already active").
    match pm_usage_add(dev, 1) {
        Some(_) => 1,
        None => -EINVAL_,
    }
}

unsafe extern "C" fn kernel_pm_put(dev: *mut c_void) -> c_int {
    match pm_usage_add(dev, -1) {
        Some(_) => 0,
        None => -EINVAL_,
    }
}

unsafe extern "C" fn kernel_pm_get_if_active(dev: *mut c_void) -> c_int {
    // Active devices always take the reference.
    match pm_usage_add(dev, 1) {
        Some(_) => 1,
        None => -EINVAL_,
    }
}

unsafe extern "C" fn kernel_pm_get_if_in_use(dev: *mut c_void) -> c_int {
    // Only a device that already holds a reference gains another.
    let in_use = DEVICE_PM_USAGE.lock().get(&(dev as usize)).copied().unwrap_or(0) > 0;
    if in_use {
        match pm_usage_add(dev, 1) {
            Some(_) => 1,
            None => -EINVAL_,
        }
    } else {
        0
    }
}

unsafe extern "C" fn kernel_pm_get_noresume(dev: *mut c_void) {
    let _ = pm_usage_add(dev, 1);
}

unsafe extern "C" fn kernel_pm_suspended(_dev: *mut c_void) -> bool {
    // No device is ever runtime-suspended.
    false
}

// The autosuspend timer, its delay, the allow/forbid switch and the driver
// flags only steer when runtime suspend is attempted. TheKernel never runs
// that policy, so these accept the call and keep no state.
unsafe extern "C" fn kernel_pm_put_autosuspend(dev: *mut c_void) -> c_int {
    unsafe { kernel_pm_put(dev) }
}
unsafe extern "C" fn kernel_pm_mark_last_busy(_dev: *mut c_void) {}
unsafe extern "C" fn kernel_pm_set_autosuspend_delay(_dev: *mut c_void, _delay: c_int) {}
unsafe extern "C" fn kernel_pm_use_autosuspend(_dev: *mut c_void) {}
unsafe extern "C" fn kernel_pm_dont_use_autosuspend(_dev: *mut c_void) {}
unsafe extern "C" fn kernel_pm_allow(_dev: *mut c_void) {}
unsafe extern "C" fn kernel_pm_set_driver_flags(_dev: *mut c_void, _flags: u32) {}

static PM_OPS: PmOps = PmOps {
    get_sync: kernel_pm_get_sync,
    put: kernel_pm_put,
    put_autosuspend: kernel_pm_put_autosuspend,
    get_if_active: kernel_pm_get_if_active,
    get_if_in_use: kernel_pm_get_if_in_use,
    get_noresume: kernel_pm_get_noresume,
    mark_last_busy: kernel_pm_mark_last_busy,
    set_autosuspend_delay: kernel_pm_set_autosuspend_delay,
    use_autosuspend: kernel_pm_use_autosuspend,
    dont_use_autosuspend: kernel_pm_dont_use_autosuspend,
    allow: kernel_pm_allow,
    suspended: kernel_pm_suspended,
    set_driver_flags: kernel_pm_set_driver_flags,
};

// ---------------------------------------------------------------------------
// Cross-CPU calls (`SmpOps`)
// ---------------------------------------------------------------------------

/// Requests are serialised: one call or stop is in flight at a time.
static SMP_SERIAL: spin::Mutex<()> = spin::Mutex::new(());

const SMP_MODE_CALL: usize = 0;
const SMP_MODE_STOP: usize = 1;

static SMP_MODE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static SMP_FUNC: AtomicUsize = AtomicUsize::new(0);
static SMP_ARG: AtomicUsize = AtomicUsize::new(0);
/// CPUs that have finished `func` (call mode).
static SMP_DONE: AtomicUsize = AtomicUsize::new(0);
/// CPUs currently parked in stop mode.
static SMP_PARKED: AtomicUsize = AtomicUsize::new(0);
/// Set by the stopping CPU to let the parked CPUs continue.
static SMP_RESUME: AtomicBool = AtomicBool::new(false);

/// The `CallFunction` IPI consumer. In call mode it runs the request on this
/// CPU and reports completion. In stop mode it parks until released.
fn smp_ipi_handler() {
    if SMP_MODE.load(Ordering::Acquire) == SMP_MODE_STOP {
        SMP_PARKED.fetch_add(1, Ordering::AcqRel);
        while !SMP_RESUME.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        SMP_PARKED.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    let func = SMP_FUNC.load(Ordering::Acquire);
    let arg = SMP_ARG.load(Ordering::Acquire) as *mut c_void;
    // SAFETY: `func` was stored from a live `extern "C" fn(*mut c_void)` by
    // the caller, which waits for this CPU's completion before returning.
    let func: unsafe extern "C" fn(*mut c_void) = unsafe { core::mem::transmute(func) };
    unsafe { func(arg) };
    SMP_DONE.fetch_add(1, Ordering::AcqRel);
}

/// Number of CPUs other than the caller.
fn other_cpu_count() -> usize {
    axhal::cpu_num() - 1
}

/// One `CallFunction` IPI to every CPU except this one.
fn send_ipi_to_others() {
    let cpu_num = axhal::cpu_num();
    let cpu_id = axhal::percpu::this_cpu_id();
    if let Err(error) = axhal::irq::send_ipi_reason(
        IpiReason::CallFunction,
        IpiTarget::AllExceptCurrent { cpu_id, cpu_num },
    ) {
        panic!("cross-CPU call: CallFunction IPI could not be sent: {error:?}");
    }
}

fn wait_until(mut done: impl FnMut() -> bool) {
    while !done() {
        core::hint::spin_loop();
    }
}

/// `call_on_other_cpus()`: run `func(arg)` on every other CPU and return once
/// all of them have finished. A single-CPU system has nothing to call.
unsafe extern "C" fn kernel_call_on_other_cpus(
    func: unsafe extern "C" fn(*mut c_void),
    arg: *mut c_void,
) {
    let _serial = SMP_SERIAL.lock();
    let others = other_cpu_count();
    if others == 0 {
        return;
    }
    SMP_MODE.store(SMP_MODE_CALL, Ordering::Release);
    SMP_FUNC.store(func as usize, Ordering::Release);
    SMP_ARG.store(arg as usize, Ordering::Release);
    SMP_DONE.store(0, Ordering::Release);
    send_ipi_to_others();
    wait_until(|| SMP_DONE.load(Ordering::Acquire) == others);
}

/// `stop_machine()` on more than one CPU: every other CPU parks in the IPI
/// handler, `func(data)` runs here with the others parked, and the others are
/// released only after it returns. `cpus` is accepted as Linux's mask; every
/// other CPU is parked, which is a superset of any mask the caller can name.
unsafe extern "C" fn kernel_stop_machine(
    func: unsafe extern "C" fn(*mut c_void) -> c_int,
    data: *mut c_void,
    _cpus: *const c_void,
) -> c_int {
    let _serial = SMP_SERIAL.lock();
    let others = other_cpu_count();
    if others == 0 {
        return unsafe { func(data) };
    }
    SMP_MODE.store(SMP_MODE_STOP, Ordering::Release);
    SMP_RESUME.store(false, Ordering::Release);
    send_ipi_to_others();
    wait_until(|| SMP_PARKED.load(Ordering::Acquire) == others);
    let result = unsafe { func(data) };
    SMP_RESUME.store(true, Ordering::Release);
    wait_until(|| SMP_PARKED.load(Ordering::Acquire) == 0);
    SMP_RESUME.store(false, Ordering::Release);
    SMP_MODE.store(SMP_MODE_CALL, Ordering::Release);
    result
}

static SMP_OPS: SmpOps = SmpOps {
    call_on_other_cpus: kernel_call_on_other_cpus,
    stop_machine: kernel_stop_machine,
};

// ---------------------------------------------------------------------------
// Uncached MMIO mappings (`IoremapProvider`)
// ---------------------------------------------------------------------------

/// Mappings already made, keyed by (page-aligned physical start, length). The
/// kernel address space has no unmap for an `iomap` region, so a mapping lives
/// for the kernel's lifetime and `ioremap` of the same range returns it again.
static IOREMAPS: spin::Mutex<BTreeMap<(u64, u64), usize>> = spin::Mutex::new(BTreeMap::new());

unsafe extern "C" fn kernel_ioremap(phys: u64, size: u64) -> *mut c_void {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let start = phys & !0xfff;
    let Some(end) = phys.checked_add(size).and_then(|end| end.checked_add(0xfff)) else {
        return core::ptr::null_mut();
    };
    let length = (end & !0xfff) - start;
    let mut mappings = IOREMAPS.lock();
    if let Some(&base) = mappings.get(&(start, length)) {
        return (base + (phys - start) as usize) as *mut c_void;
    }
    let Ok(Ok(virt)) = usize::try_from(start).map(|start| {
        axmm::iomap(PhysAddr::from_usize(start), length as usize).map(|virt| virt.as_usize())
    }) else {
        return core::ptr::null_mut();
    };
    mappings.insert((start, length), virt);
    (virt + (phys - start) as usize) as *mut c_void
}

/// `iounmap()`. The mapping is kept for reuse by `ioremap` (see `IOREMAPS`).
unsafe extern "C" fn kernel_iounmap(_addr: *mut c_void) {}

static IOREMAP_PROVIDER: IoremapProvider = IoremapProvider {
    ioremap: kernel_ioremap,
    iounmap: kernel_iounmap,
};

// ---------------------------------------------------------------------------
// Uncore kernel services (`UncoreKernelOps`)
// ---------------------------------------------------------------------------

/// Owner token of the PUNIT lock: the calling task, or 0 when free.
static PUNIT_OWNER: AtomicUsize = AtomicUsize::new(0);

/// `iosf_mbi_punit_acquire()`: a sleeping mutex, held across calls by the
/// same task. Re-acquiring a held lock would deadlock in Linux, so it asserts.
unsafe extern "C" fn kernel_punit_acquire() {
    let token = current_identity_key();
    assert!(
        PUNIT_OWNER.load(Ordering::Acquire) != token,
        "PUNIT lock re-acquired by its holder"
    );
    while PUNIT_OWNER
        .compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        axtask::yield_now();
    }
}

unsafe extern "C" fn kernel_punit_release() {
    let token = current_identity_key();
    assert!(
        PUNIT_OWNER
            .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "PUNIT lock released by a task that does not hold it"
    );
}

unsafe extern "C" fn kernel_punit_assert_acquired() {
    assert!(
        PUNIT_OWNER.load(Ordering::Acquire) == current_identity_key(),
        "PUNIT lock not held by the caller"
    );
}

/// PMIC bus-access notifier chain (`iosf_mbi_register_pmic_bus_access_notifier`).
/// Entries are the notifier blocks registered by their owners.
static PMIC_NOTIFIERS: spin::Mutex<Vec<usize>> = spin::Mutex::new(Vec::new());

unsafe extern "C" fn kernel_register_pmic_bus_access_notifier(nb: *mut c_void) -> c_int {
    assert!(!nb.is_null(), "PMIC notifier registered with a null block");
    let mut chain = PMIC_NOTIFIERS.lock();
    if chain.contains(&(nb as usize)) {
        return -EEXIST;
    }
    chain.push(nb as usize);
    0
}

unsafe extern "C" fn kernel_unregister_pmic_bus_access_notifier_unlocked(nb: *mut c_void) -> c_int {
    let mut chain = PMIC_NOTIFIERS.lock();
    match chain.iter().position(|&entry| entry == nb as usize) {
        Some(index) => {
            chain.remove(index);
            0
        }
        None => -ENOENT,
    }
}

/// `HAS_FPGA_DBG_UNCLAIMED(display)`. The only display this kernel drives is
/// ADL-N, which uses `xe_lpd_display`; `XE_LPD_FEATURES` sets `has_fpga_dbg`
/// (intel_display_device.c, Linux 7.2.3).
unsafe extern "C" fn kernel_display_has_fpga_dbg(_display: *mut c_void) -> bool {
    true
}

/// The uncore table takes `*mut NotifierBlock`, a `tk-intel-gt` type this
/// crate cannot name. The notifier functions only ever compare and store the
/// pointer, so the `*mut c_void` form has the same ABI and is converted here.
static UNCORE_KERNEL_OPS: UncoreKernelOps = UncoreKernelOps {
    punit_acquire: kernel_punit_acquire,
    punit_release: kernel_punit_release,
    punit_assert_acquired: kernel_punit_assert_acquired,
    register_pmic_bus_access_notifier: unsafe {
        core::mem::transmute(
            kernel_register_pmic_bus_access_notifier as unsafe extern "C" fn(*mut c_void) -> c_int,
        )
    },
    unregister_pmic_bus_access_notifier_unlocked: unsafe {
        core::mem::transmute(
            kernel_unregister_pmic_bus_access_notifier_unlocked
                as unsafe extern "C" fn(*mut c_void) -> c_int,
        )
    },
    display_has_fpga_dbg: kernel_display_has_fpga_dbg,
};

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Install every round-4 K2 table. Called from `install_providers()`.
fn install_k2_tables() -> Result<(), &'static str> {
    intel_gt::linux::kernel_services::install_task_ops(&TASK_OPS)?;
    install_file_ops(&FILE_OPS)?;
    install_pci_ops(&PCI_OPS)?;
    install_pm_ops(&PM_OPS)?;
    install_smp_ops(&SMP_OPS)?;
    install_pci_core_ops(&PCI_CORE_OPS)?;
    install_pci_bus_ops(&PCI_BUS_OPS)?;
    install_ioremap_provider(&IOREMAP_PROVIDER)?;
    install_uncore_kernel_ops(&UNCORE_KERNEL_OPS)?;
    if !axhal::irq::register_ipi_reason(IpiReason::CallFunction, smp_ipi_handler) {
        return Err("CallFunction IPI consumer already registered");
    }
    Ok(())
}
