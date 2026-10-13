// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for Linux kernel API shims being landed in tk-linux-kapi
//! and DRM core by parallel batches. Replaced when those batches merge.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_void};
use tk_drm_upstream::drm_atomic_types::DrmAtomicCommit;
use tk_drm_upstream::drm_device_types::DrmDevice;
use tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx;

// upstream: lib/iomap.c ioread32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ioread32(_addr: *const c_void) -> u32 {
    0
}

// upstream: include/drm/drm_privacy_screen_consumer.h drm_privacy_screen_get()
// Degraded stub matches Linux !CONFIG_DRM_PRIVACY_SCREEN returning ERR_PTR(-ENODEV).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_privacy_screen_get(
    _dev: *mut c_void,
    _con_id: *const c_char,
) -> *mut c_void {
    tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENODEV)
}

// upstream: drivers/gpu/drm/drm_privacy_screen.c drm_privacy_screen_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_privacy_screen_put(_priv_: *mut c_void) {}

// upstream: drivers/gpu/drm/display/drm_dp_mst_topology.c drm_dp_mst_atomic_setup_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dp_mst_atomic_setup_commit(_state: *mut DrmAtomicCommit) -> i32 {
    0
}

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
// Degraded stub returns ERR_PTR(-ENOMEM), matching Linux failure when state allocation fails.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_duplicate_state(
    _dev: *mut DrmDevice,
    _ctx: *mut DrmModesetAcquireCtx,
) -> *mut DrmAtomicCommit {
    tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENOMEM)
}

// upstream: include/drm/drm_atomic.h drm_atomic_commit_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_commit_put(_state: *mut DrmAtomicCommit) {}

const X86_HYPER_NATIVE: u32 = 0;

// upstream: arch/x86/include/asm/hypervisor.h hypervisor_is_type()
// Degraded stub matches Linux bare-metal semantics (x86_hyper_type == X86_HYPER_NATIVE).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hypervisor_is_type(type_: u32) -> bool {
    type_ == X86_HYPER_NATIVE
}


// upstream: drivers/gpu/drm/drm_atomic_helper.c drm_atomic_helper_shutdown()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_shutdown(_dev: *mut DrmDevice) {}

// Defined by tk-linux-kapi / tk-drm-upstream since kapi-display and BI merged;
// re-exported so existing call paths keep resolving.
pub use tk_linux_kapi::acpi::acpi_video_unregister;
pub use tk_linux_kapi::debugfs::debugfs_create_u32;
pub use tk_linux_kapi::acpi::dmi_check_system;
pub use tk_drm_upstream::pending_stubs::drm_client_probe::drm_client_dev_unregister;
pub use tk_linux_kapi::pci::pci_iomap_range;
pub use tk_linux_kapi::pci::pci_iounmap;
pub use tk_linux_kapi::vga::vga_switcheroo_client_probe_defer;
