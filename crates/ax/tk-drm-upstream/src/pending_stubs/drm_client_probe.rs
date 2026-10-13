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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_restore(_dev: *mut DrmDevice, _restore: bool) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_dev_unregister(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_sysrq_register(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_sysrq_unregister(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_panic_register(_dev: *mut DrmDevice) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_panic_unregister(_dev: *mut DrmDevice) {}

// upstream: drm_probe_helper.c drm_crtc_mode_valid()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_mode_valid(
    crtc: *mut crate::drm_crtc_types::DrmCrtc,
    mode: *const crate::drm_modes_types::DrmDisplayMode,
) -> crate::drm_modes_types::DrmModeStatus {
    unsafe {
        if crtc.is_null() {
            return crate::drm_modes_types::MODE_OK;
        }
        let funcs = (*crtc).helper_private.cast::<crate::drm_modeset_helper_vtables_types::DrmCrtcHelperFuncs>();
        if funcs.is_null() {
            return crate::drm_modes_types::MODE_OK;
        }
        if let Some(mv) = (*funcs).mode_valid {
            return mv(crtc, mode as *mut _);
        }
        crate::drm_modes_types::MODE_OK
    }
}

// upstream: drm_probe_helper.c drm_encoder_mode_valid()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_encoder_mode_valid(
    encoder: *mut crate::drm_encoder_types::DrmEncoder,
    mode: *const crate::drm_modes_types::DrmDisplayMode,
) -> crate::drm_modes_types::DrmModeStatus {
    unsafe {
        if encoder.is_null() {
            return crate::drm_modes_types::MODE_OK;
        }
        let funcs = (*encoder).helper_private.cast::<crate::drm_modeset_helper_vtables_types::DrmEncoderHelperFuncs>();
        if funcs.is_null() {
            return crate::drm_modes_types::MODE_OK;
        }
        if let Some(mv) = (*funcs).mode_valid {
            return mv(encoder, mode as *mut _);
        }
        crate::drm_modes_types::MODE_OK
    }
}

