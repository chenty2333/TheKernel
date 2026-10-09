// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Required callbacks from the kernel's PCI, IRQ, display, and DRM owners.
//!
//! These are not compatibility-success shims: the owner must install the
//! complete operations table before any translated reset or power path can
//! reach them. Missing installation fails closed instead of pretending that
//! PCI configuration, display reset, or user-visible events succeeded.

#![allow(unsafe_code, non_snake_case)]

use core::{
    ffi::{c_char, c_int, c_void},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

/// Kernel-owned operations required by the unconnected upstream GT path.
/// Pointers passed to the callbacks are the native PCI device, DRM device, or
/// display-owner identities retained in the corresponding i915 layout.
#[repr(C)]
pub struct I915PlatformOps {
    pub to_pci_dev: unsafe extern "C" fn(device: *mut c_void) -> *mut c_void,
    pub pci_read_config_byte:
        unsafe extern "C" fn(device: *mut c_void, offset: u32, value: *mut u8) -> c_int,
    pub pci_write_config_byte:
        unsafe extern "C" fn(device: *mut c_void, offset: u32, value: u8) -> c_int,
    pub pci_read_config_word:
        unsafe extern "C" fn(device: *mut c_void, offset: u32, value: *mut u16) -> c_int,
    pub irq_suspend: unsafe extern "C" fn(i915: *mut crate::linux_i915_private::DrmI915Private),
    pub irq_resume: unsafe extern "C" fn(i915: *mut crate::linux_i915_private::DrmI915Private),
    pub overlay_reset: unsafe extern "C" fn(display: *mut c_void),
    pub display_reset_test: unsafe extern "C" fn(display: *mut c_void) -> bool,
    pub display_reset_supported: unsafe extern "C" fn(display: *mut c_void) -> bool,
    pub display_reset_prepare: unsafe extern "C" fn(display: *mut c_void),
    pub display_reset_finish: unsafe extern "C" fn(display: *mut c_void, reset: bool),
    pub add_taint_for_ci:
        unsafe extern "C" fn(i915: *mut crate::linux_i915_private::DrmI915Private, flag: u32),
    pub kobject_uevent_env:
        unsafe extern "C" fn(kobj: *mut c_void, action: i32, envp: *const *const c_char) -> c_int,
    pub drm_dev_wedged_event:
        unsafe extern "C" fn(drm: *mut c_void, recovery: u32, data: *mut c_void),
}

static I915_PLATFORM_OPS: AtomicPtr<I915PlatformOps> = AtomicPtr::new(ptr::null_mut());

/// Install the unique kernel-owned platform interface before enabling any
/// upstream GT caller. The table must have static lifetime and remain
/// immutable while the feature is active.
pub fn install_i915_platform_ops(ops: &'static I915PlatformOps) -> Result<(), &'static str> {
    let candidate = (ops as *const I915PlatformOps).cast_mut();
    match I915_PLATFORM_OPS.compare_exchange(
        ptr::null_mut(),
        candidate,
        Ordering::Release,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(existing) if existing == candidate => Ok(()),
        Err(_) => Err("i915 platform operations are already installed"),
    }
}

fn ops() -> &'static I915PlatformOps {
    let ops = I915_PLATFORM_OPS.load(Ordering::Acquire);
    assert!(
        !ops.is_null(),
        "kernel PCI/IRQ/display/DRM owners must install i915 platform operations"
    );
    unsafe { &*ops }
}

/// `to_pci_dev()` is a container identity conversion in Linux. The TheKernel
/// provider receives the retained native PCI device identity directly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn to_pci_dev(device: *mut c_void) -> *mut c_void {
    assert!(!device.is_null());
    unsafe { (ops().to_pci_dev)(device) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_read_config_byte(
    device: *mut c_void,
    offset: u32,
    value: *mut u8,
) -> c_int {
    assert!(!device.is_null() && !value.is_null());
    unsafe { (ops().pci_read_config_byte)(device, offset, value) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_write_config_byte(
    device: *mut c_void,
    offset: u32,
    value: u8,
) -> c_int {
    assert!(!device.is_null());
    unsafe { (ops().pci_write_config_byte)(device, offset, value) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_read_config_word(
    device: *mut c_void,
    offset: u32,
    value: *mut u16,
) -> c_int {
    assert!(!device.is_null() && !value.is_null());
    unsafe { (ops().pci_read_config_word)(device, offset, value) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_irq_suspend(i915: *mut crate::linux_i915_private::DrmI915Private) {
    unsafe { (ops().irq_suspend)(i915) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_irq_resume(i915: *mut crate::linux_i915_private::DrmI915Private) {
    unsafe { (ops().irq_resume)(i915) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_overlay_reset(display: *mut c_void) {
    unsafe { (ops().overlay_reset)(display) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_reset_test(display: *mut c_void) -> bool {
    unsafe { (ops().display_reset_test)(display) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_reset_supported(display: *mut c_void) -> bool {
    unsafe { (ops().display_reset_supported)(display) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_reset_prepare(display: *mut c_void) {
    unsafe { (ops().display_reset_prepare)(display) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_reset_finish(display: *mut c_void, reset: bool) {
    unsafe { (ops().display_reset_finish)(display, reset) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn add_taint_for_CI(
    i915: *mut crate::linux_i915_private::DrmI915Private,
    flag: u32,
) {
    unsafe { (ops().add_taint_for_ci)(i915, flag) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kobject_uevent_env(
    kobj: *mut c_void,
    action: i32,
    envp: *const *const c_char,
) -> c_int {
    assert!(!kobj.is_null());
    unsafe { (ops().kobject_uevent_env)(kobj, action, envp) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_wedged_event(drm: *mut c_void, recovery: u32, data: *mut c_void) {
    assert!(!drm.is_null());
    unsafe { (ops().drm_dev_wedged_event)(drm, recovery, data) };
}
