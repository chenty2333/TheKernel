// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_client.c and drm_probe_helper.c (client/probe batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_client_upstream.rs and drm_probe_helper_upstream.rs when they land.
//!
//! Symbols: drm_client_dev_resume, drm_client_dev_suspend, drm_kms_helper_poll_disable, drm_kms_helper_poll_enable.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_device_types::DrmDevice;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_resume(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_suspend(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_kms_helper_poll_disable(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_kms_helper_poll_enable(_dev: *mut DrmDevice) {}
