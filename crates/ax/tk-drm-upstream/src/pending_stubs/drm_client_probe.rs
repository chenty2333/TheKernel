// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Re-export paths for B2c functions retained by earlier port batches. The
//! implementations live in their Linux-source modules, not in this pending
//! namespace.

#![allow(non_snake_case, dead_code, unsafe_code)]

pub use crate::drm_client_event_upstream::{
    drm_client_dev_restore, drm_client_dev_resume, drm_client_dev_suspend,
    drm_client_dev_unregister,
};
pub use crate::drm_probe_helper_upstream::{
    drm_crtc_mode_valid, drm_encoder_mode_valid, drm_kms_helper_poll_disable,
    drm_kms_helper_poll_enable,
};

// These optional panic/sysrq facilities are outside B2c's enabled paths.

use crate::drm_device_types::DrmDevice;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_sysrq_register(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_sysrq_unregister(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_panic_register(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_panic_unregister(_dev: *mut DrmDevice) {}
