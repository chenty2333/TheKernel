// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for Linux kernel API shims being landed in tk-linux-kapi
//! and DRM core by parallel batches. Replaced when those batches merge.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_void};
use tk_drm_upstream::drm_atomic_types::DrmAtomicCommit;
use tk_drm_upstream::drm_device_types::DrmDevice;
use tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx;

// upstream: drivers/gpu/vga/vga_switcheroo.c vga_switcheroo_client_probe_defer()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vga_switcheroo_client_probe_defer(_pdev: *mut c_void) -> bool {
    false
}

// upstream: drivers/firmware/dmi_scan.c dmi_check_system()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dmi_check_system(_list: *const c_void) -> i32 {
    0
}

// upstream: drivers/acpi/acpi_video.c acpi_video_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acpi_video_unregister() {}

// upstream: fs/debugfs/file.c debugfs_create_u32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn debugfs_create_u32(
    _name: *const c_char,
    _mode: u16,
    _parent: *mut c_void,
    _val: *mut u32,
) {}

// upstream: lib/pci_iomap.c pci_iomap_range()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_iomap_range(
    _pdev: *mut c_void,
    _bar: i32,
    _offset: u64,
    _maxlen: u64,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: lib/pci_iomap.c pci_iounmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_iounmap(_pdev: *mut c_void, _addr: *mut c_void) {}

// upstream: lib/iomap.c ioread32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ioread32(_addr: *const c_void) -> u32 {
    0
}

// upstream: drivers/gpu/drm/drm_privacy_screen.c drm_privacy_screen_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_privacy_screen_get(
    _dev: *mut c_void,
    _con_id: *const c_char,
) -> *mut c_void {
    core::ptr::null_mut()
}

// upstream: drivers/gpu/drm/drm_privacy_screen.c drm_privacy_screen_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_privacy_screen_put(_priv_: *mut c_void) {}

// upstream: drivers/gpu/drm/display/drm_dp_mst_topology.c drm_dp_mst_atomic_setup_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dp_mst_atomic_setup_commit(_state: *mut DrmAtomicCommit) -> i32 {
    0
}

// upstream: drivers/gpu/drm/clients/drm_client_setup.c drm_client_dev_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_unregister(_dev: *mut DrmDevice) {}

// upstream: drivers/gpu/drm/drm_probe_helper.c drm_kms_helper_poll_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_kms_helper_poll_init(_dev: *mut DrmDevice) {}

// upstream: drivers/gpu/drm/drm_probe_helper.c drm_kms_helper_poll_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_kms_helper_poll_fini(_dev: *mut DrmDevice) {}

// upstream: drivers/gpu/drm/drm_atomic_helper.c drm_atomic_helper_commit_duplicated_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_commit_duplicated_state(
    _state: *mut DrmAtomicCommit,
    _ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_atomic_helper.c drm_atomic_helper_disable_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_disable_all(
    _dev: *mut DrmDevice,
    _ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_atomic_helper.c drm_atomic_helper_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_duplicate_state(
    _dev: *mut DrmDevice,
    _ctx: *mut DrmModesetAcquireCtx,
) -> *mut DrmAtomicCommit {
    core::ptr::null_mut()
}

// upstream: include/drm/drm_atomic.h drm_atomic_commit_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_put(_state: *mut DrmAtomicCommit) {}

// upstream: arch/x86/include/asm/hypervisor.h hypervisor_is_type()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hypervisor_is_type(_type_: u32) -> bool {
    true
}


// upstream: drivers/gpu/drm/drm_atomic_helper.c drm_atomic_helper_shutdown()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_shutdown(_dev: *mut DrmDevice) {}

// Mock PCI dev pointer for ISA bridge scan in tests
static MOCK_ISA_BRIDGE: core::sync::atomic::AtomicPtr<crate::intel_quirks_upstream::PciDev> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub unsafe fn set_mock_isa_bridge(dev: *mut crate::intel_quirks_upstream::PciDev) {
    MOCK_ISA_BRIDGE.store(dev, core::sync::atomic::Ordering::SeqCst);
}

// upstream: drivers/pci/search.c pci_get_class()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_get_class(
    class: u32,
    from: *mut crate::intel_quirks_upstream::PciDev,
) -> *mut crate::intel_quirks_upstream::PciDev {
    if from.is_null() {
        let dev = MOCK_ISA_BRIDGE.load(core::sync::atomic::Ordering::SeqCst);
        if !dev.is_null() && (((*dev).class >> 8) == (class >> 8) || class == (0x0601 << 8)) {
            return dev;
        }
    }
    core::ptr::null_mut()
}

// upstream: drivers/pci/pci-driver.c pci_dev_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pci_dev_put(_dev: *mut crate::intel_quirks_upstream::PciDev) {}
