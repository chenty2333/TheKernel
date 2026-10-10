// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Linux core APIs used by the i915 probe and IRQ paths, implemented for this
//! kernel. Device, PCI and DMA hardware state belongs to the kernel, so those
//! calls go through `PciCoreOps`; the driver-data registry and the taint and
//! ACPI-state records are kept here because they are pure Linux core state.
//!
//! The provider slot fails closed: calling any `PciCoreOps` entry before the
//! kernel installs the table panics, instead of reporting success.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use alloc::collections::BTreeMap;
use core::{
    ffi::{c_char, c_int, c_void},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::linux::kernel_services::ProviderSlot;

/// Kernel-owned PCI, DMA and aperture operations. `dev` is the native device
/// identity (a `struct pci_dev *` as returned by `to_pci_dev()`), and DMA
/// calls take the `struct device *` identity used for the device's DMA window.
#[repr(C)]
pub struct PciCoreOps {
    /// `pci_enable_device()`: 0 or a negative errno.
    pub enable_device: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub disable_device: unsafe extern "C" fn(dev: *mut c_void),
    pub set_master: unsafe extern "C" fn(dev: *mut c_void),
    /// `pci_enable_msi()`: 0 or a negative errno.
    pub enable_msi: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub disable_msi: unsafe extern "C" fn(dev: *mut c_void),
    /// `pci_save_state()`: 0 or a negative errno.
    pub save_state: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub restore_state: unsafe extern "C" fn(dev: *mut c_void),
    /// `pci_set_power_state()`: `state` is the `pci_power_t` value.
    pub set_power_state: unsafe extern "C" fn(dev: *mut c_void, state: u32) -> c_int,
    pub d3cold_enable: unsafe extern "C" fn(dev: *mut c_void),
    pub d3cold_disable: unsafe extern "C" fn(dev: *mut c_void),
    /// `pcie_find_root_port()`: referenced-free `struct pci_dev *`, or NULL.
    pub find_root_port: unsafe extern "C" fn(dev: *mut c_void) -> *mut c_void,
    /// `pci_resource_flags(dev, bar)`.
    pub resource_flags: unsafe extern "C" fn(dev: *mut c_void, bar: c_int) -> u64,
    /// `dma_set_mask()`: 0, or -EIO when the device cannot address `mask`.
    pub dma_set_mask: unsafe extern "C" fn(dev: *mut c_void, mask: u64) -> c_int,
    /// `dma_set_coherent_mask()`: 0, or -EIO.
    pub dma_set_coherent_mask: unsafe extern "C" fn(dev: *mut c_void, mask: u64) -> c_int,
    /// `dma_set_max_seg_size()`.
    pub dma_set_max_seg_size: unsafe extern "C" fn(dev: *mut c_void, size: u32),
    /// `aperture_remove_conflicting_pci_devices()`: take over the firmware
    /// framebuffer that the PCI device's BARs overlap. The kernel owns the
    /// console and must release the firmware framebuffer for this device, so
    /// this is never a no-op. Returns 0 or a negative errno.
    pub aperture_remove_conflicting:
        unsafe extern "C" fn(dev: *mut c_void, name: *const c_char) -> c_int,
}

pub static PCI_CORE: ProviderSlot<PciCoreOps> = ProviderSlot::new();

/// Install the kernel PCI/DMA/aperture table. The table must be `'static`.
pub fn install_pci_core_ops(ops: &'static PciCoreOps) -> Result<(), &'static str> {
    PCI_CORE.install(ops)
}

fn pci_core() -> &'static PciCoreOps {
    PCI_CORE.require("PCI core")
}

unsafe extern "C" {
    fn to_pci_dev(dev: *mut c_void) -> *mut c_void;
}

/// Driver-data registry, keyed by the native PCI device identity. Linux keeps
/// one `drvdata` slot per `struct device`; here `dev_get_drvdata()` and
/// `pci_get_drvdata()` on the same device must reach the same slot, so both
/// normalize the device through `to_pci_dev()`.
static DRVDATA: spin::Mutex<BTreeMap<usize, usize>> = spin::Mutex::new(BTreeMap::new());

unsafe fn drvdata_key(dev: *mut c_void) -> usize {
    assert!(!dev.is_null(), "drvdata on a NULL device");
    unsafe { to_pci_dev(dev) as usize }
}

unsafe fn get_drvdata(dev: *mut c_void) -> *mut c_void {
    let key = unsafe { drvdata_key(dev) };
    DRVDATA
        .lock()
        .get(&key)
        .map_or(core::ptr::null_mut(), |data| *data as *mut c_void)
}

unsafe fn set_drvdata(dev: *mut c_void, data: *mut c_void) {
    let key = unsafe { drvdata_key(dev) };
    let mut registry = DRVDATA.lock();
    if data.is_null() {
        registry.remove(&key);
    } else {
        registry.insert(key, data as usize);
    }
}

/// Linux `pci_get_drvdata()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_get_drvdata(pdev: *mut c_void) -> *mut c_void {
    unsafe { get_drvdata(pdev) }
}

/// Linux `pci_set_drvdata()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_set_drvdata(pdev: *mut c_void, data: *mut c_void) {
    unsafe { set_drvdata(pdev, data) }
}

/// Linux `dev_get_drvdata()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dev_get_drvdata(dev: *mut c_void) -> *mut c_void {
    unsafe { get_drvdata(dev) }
}

/// Linux `pci_enable_device()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_enable_device(pdev: *mut c_void) -> c_int {
    unsafe { (pci_core().enable_device)(pdev) }
}

/// Linux `pci_disable_device()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_disable_device(pdev: *mut c_void) {
    unsafe { (pci_core().disable_device)(pdev) }
}

/// Linux `pci_set_master()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_set_master(dev: *mut c_void) {
    unsafe { (pci_core().set_master)(dev) }
}

/// Linux `pci_enable_msi()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_enable_msi(dev: *mut c_void) -> c_int {
    unsafe { (pci_core().enable_msi)(dev) }
}

/// Linux `pci_disable_msi()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_disable_msi(dev: *mut c_void) {
    unsafe { (pci_core().disable_msi)(dev) }
}

/// Linux `pci_save_state()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_save_state(dev: *mut c_void) -> c_int {
    unsafe { (pci_core().save_state)(dev) }
}

/// Linux `pci_restore_state()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_restore_state(dev: *mut c_void) {
    unsafe { (pci_core().restore_state)(dev) }
}

/// Linux `pci_set_power_state()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_set_power_state(dev: *mut c_void, state: u32) -> c_int {
    unsafe { (pci_core().set_power_state)(dev, state) }
}

/// Linux `pci_d3cold_enable()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_d3cold_enable(dev: *mut c_void) {
    unsafe { (pci_core().d3cold_enable)(dev) }
}

/// Linux `pci_d3cold_disable()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_d3cold_disable(dev: *mut c_void) {
    unsafe { (pci_core().d3cold_disable)(dev) }
}

/// Linux `pcie_find_root_port()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pcie_find_root_port(dev: *mut c_void) -> *mut c_void {
    unsafe { (pci_core().find_root_port)(dev) }
}

/// Linux `pci_resource_flags()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_resource_flags(pdev: *mut c_void, bar: c_int) -> u64 {
    unsafe { (pci_core().resource_flags)(pdev, bar) }
}

/// Linux `dma_set_mask()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_set_mask(dev: *mut c_void, mask: u64) -> c_int {
    unsafe { (pci_core().dma_set_mask)(dev, mask) }
}

/// Linux `dma_set_coherent_mask()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_set_coherent_mask(dev: *mut c_void, mask: u64) -> c_int {
    unsafe { (pci_core().dma_set_coherent_mask)(dev, mask) }
}

/// Linux `dma_set_max_seg_size()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_set_max_seg_size(dev: *mut c_void, size: u32) {
    unsafe { (pci_core().dma_set_max_seg_size)(dev, size) }
}

/// Linux `aperture_remove_conflicting_pci_devices()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aperture_remove_conflicting_pci_devices(
    pdev: *mut c_void,
    name: *const c_char,
) -> c_int {
    unsafe { (pci_core().aperture_remove_conflicting)(pdev, name) }
}

/// Kernel IRQ registration. `irq` is the device's interrupt vector (the value
/// `crate::linux::dma::device_irq()` reports), and `dev_id` is the cookie that
/// the handler receives and that `free_irq` matches.
#[repr(C)]
pub struct IrqCoreOps {
    /// `request_irq()`: 0 or a negative errno. `handler` is called with
    /// `(irq, dev_id)` in hard-IRQ context and returns an `irqreturn_t`.
    pub request_irq: unsafe extern "C" fn(
        irq: usize,
        handler: unsafe extern "C" fn(c_int, *mut c_void) -> c_int,
        dev_id: *mut c_void,
    ) -> c_int,
    /// `free_irq()`: unregisters the handler installed with `dev_id`.
    pub free_irq: unsafe extern "C" fn(irq: usize, dev_id: *mut c_void),
}

pub static IRQ_CORE: ProviderSlot<IrqCoreOps> = ProviderSlot::new();

/// Install the kernel IRQ registration table. The table must be `'static`.
pub fn install_irq_core_ops(ops: &'static IrqCoreOps) -> Result<(), &'static str> {
    IRQ_CORE.install(ops)
}

/// Kernel taint bits recorded by `add_taint()`.
static TAINT_FLAGS: AtomicU64 = AtomicU64::new(0);

/// Linux `add_taint()`. Lockdep is compiled out (`CONFIG_LOCKDEP` is off), so
/// `lockdep_ok` has no effect beyond the record. The taint is recorded in
/// `TAINT_FLAGS` and logged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn add_taint(flag: u32, lockdep_ok: i32) {
    let _ = lockdep_ok;
    let bit = 1u64 << (flag & 63);
    let previous = TAINT_FLAGS.fetch_or(bit, Ordering::AcqRel);
    if previous & bit == 0 {
        axlog::warn!("kernel tainted: flag {}", flag);
    }
}

/// Linux `acpi_target_system_state()`. TheKernel's only suspend path is the
/// device runtime-PM path, which never enters an ACPI sleep state, so the
/// system target is always `ACPI_STATE_S0` (0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_target_system_state() -> u32 {
    0
}

/// Driver-managed action registry: `drmm_add_action_or_reset()` records
/// `(drm, action, data)`, and `drmm_release_all()` runs them in reverse order
/// as Linux does at DRM device release.
static DRMM_ACTIONS: spin::Mutex<alloc::vec::Vec<(usize, usize, usize)>> =
    spin::Mutex::new(alloc::vec::Vec::new());

/// Linux `drmm_add_action_or_reset()`. If the action cannot be recorded, it
/// runs immediately and the error is returned, as in Linux.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_add_action_or_reset(
    drm: *mut c_void,
    action: unsafe extern "C" fn(*mut c_void, *mut c_void),
    data: *mut c_void,
) -> c_int {
    assert!(!drm.is_null(), "drmm_add_action on a NULL drm device");
    let mut actions = DRMM_ACTIONS.lock();
    if actions.try_reserve(1).is_err() {
        drop(actions);
        unsafe { action(drm, data) };
        return -(crate::linux_config::ENOMEM as c_int);
    }
    actions.push((drm as usize, action as usize, data as usize));
    0
}

/// Run and remove the actions registered for `drm`, newest first. Called from
/// the DRM device release path together with `drmm_release_all()`.
pub unsafe fn drmm_run_actions(drm: *mut c_void) {
    let mut pending = {
        let mut actions = DRMM_ACTIONS.lock();
        let mut mine = alloc::vec::Vec::new();
        let mut index = 0;
        while index < actions.len() {
            if actions[index].0 == drm as usize {
                mine.push(actions.remove(index));
            } else {
                index += 1;
            }
        }
        mine
    };
    while let Some((_, action, data)) = pending.pop() {
        let action: unsafe extern "C" fn(*mut c_void, *mut c_void) =
            unsafe { core::mem::transmute(action) };
        unsafe { action(drm, data as *mut c_void) };
    }
}

/// Linux `synchronize_rcu()`: wait for a full grace period by advancing the
/// epoch once the current readers have left.
#[unsafe(export_name = "synchronize_rcu")]
pub extern "C" fn synchronize_rcu() {
    crate::linux::rcu::cond_synchronize_rcu(crate::linux::rcu::get_state_synchronize_rcu());
}
