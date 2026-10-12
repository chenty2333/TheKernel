// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_file.c event reservation and delivery (B2e file batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_file_upstream.rs when the drm_file batch lands.
//!
//! Symbols: drm_event_cancel_free, drm_event_reserve_init, drm_event_reserve_init_locked, drm_send_event_timestamp_locked.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::{DrmFile, DrmPendingEvent};
use core::sync::atomic::Ordering;
use crate::uapi_drm_types::DrmEvent;
use crate::pending_stubs::test_records::{DELIVERED, DELIVERED_COUNT, RECORD_SLOTS};

// upstream: drivers/gpu/drm/drm_file.c drm_event_cancel_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_event_cancel_free(_dev: *mut DrmDevice, _p: *mut DrmPendingEvent) {}

// upstream: drivers/gpu/drm/drm_file.c drm_event_reserve_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_event_reserve_init(
    _dev: *mut DrmDevice,
    _file_priv: *mut DrmFile,
    _p: *mut DrmPendingEvent,
    _e: *mut DrmEvent,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_file.c drm_event_reserve_init_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_event_reserve_init_locked(
    _dev: *mut DrmDevice,
    _file_priv: *mut DrmFile,
    _p: *mut DrmPendingEvent,
    _e: *mut DrmEvent,
) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_file.c drm_send_event_timestamp_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_send_event_timestamp_locked(_dev: *mut DrmDevice, p: *mut DrmPendingEvent, _now: i64) {
    let n = DELIVERED_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < RECORD_SLOTS {
        DELIVERED[n].store(p as u64, Ordering::SeqCst);
    }
}
