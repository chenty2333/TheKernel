// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Host checks for the generated layouts. The compile-time asserts in each
//! `*_types.rs` module already pin every size and offset; these run-time
//! checks keep the headline numbers visible in the test log and pin the
//! x86_64 offsets that the i915 port reads directly from `struct drm_device`.

use core::mem::{offset_of, size_of};

use crate::{
    drm_atomic_types::DrmAtomicCommit,
    drm_connector_types::{DrmConnector, DrmConnectorState},
    drm_crtc_types::{DrmCrtc, DrmCrtcState},
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    drm_framebuffer_types::DrmFramebuffer,
    drm_mode_object_types::DrmModeObject,
    drm_modes_types::DrmDisplayMode,
    drm_plane_types::{DrmPlane, DrmPlaneState},
};

/// Oracle DWARF sizes (linux-7.2.3-oracle, THP enabled).
#[test]
fn headline_struct_sizes_match_oracle() {
    assert_eq!(size_of::<DrmModeObject>(), 32);
    assert_eq!(size_of::<DrmDevice>(), 1560);
    assert_eq!(size_of::<DrmFile>(), 368);
    assert_eq!(size_of::<DrmConnector>(), 2248);
    assert_eq!(size_of::<DrmCrtc>(), 1648);
    assert_eq!(size_of::<DrmPlane>(), 1352);
    assert_eq!(size_of::<DrmFramebuffer>(), 192);
    assert_eq!(size_of::<DrmDisplayMode>(), 120);
    assert_eq!(size_of::<DrmCrtcState>(), 344);
    assert_eq!(size_of::<DrmPlaneState>(), 184);
    assert_eq!(size_of::<DrmConnectorState>(), 440);
    assert_eq!(size_of::<DrmAtomicCommit>(), 128);
}

/// Offsets read by the i915 port through `struct drm_device` (THP enabled).
/// `huge_mnt` is present in this configuration, which moves every member
/// after `driver_features` by 8 bytes relative to the THP-off layout.
#[test]
fn drm_device_i915_offsets_match_oracle() {
    assert_eq!(offset_of!(DrmDevice, driver), 56);
    assert_eq!(offset_of!(DrmDevice, unplugged), 124);
    assert_eq!(offset_of!(DrmDevice, anon_inode), 128);
    assert_eq!(offset_of!(DrmDevice, huge_mnt), 112);
    assert_eq!(offset_of!(DrmDevice, vma_offset_manager), 1496);
    assert_eq!(offset_of!(DrmDevice, switch_power_state), 1512);
}

/// Smoke test: the crate's type modules and the no_std build link together.
#[test]
fn crate_builds_without_std_surface() {
    let uninit = core::mem::MaybeUninit::<DrmModeObject>::uninit();
    assert_eq!(core::mem::size_of_val(&uninit), 32);
}
