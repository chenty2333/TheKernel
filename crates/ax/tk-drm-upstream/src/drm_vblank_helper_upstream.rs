// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_vblank_helper.c` (Virtio V0).
#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_linux_kapi::locks::{spin_lock_irq, spin_unlock_irq};

use crate::{
    drm_atomic_types::DrmAtomicCommit, drm_atomic_upstream::drm_atomic_get_new_crtc_state,
    drm_crtc_types::DrmCrtc,
};

// upstream: drivers/gpu/drm/drm_vblank.c
unsafe extern "C" {
    fn drm_crtc_vblank_get(crtc: *mut DrmCrtc) -> i32;
    fn drm_crtc_arm_vblank_event(
        crtc: *mut DrmCrtc,
        event: *mut crate::drm_vblank_types::DrmPendingVblankEvent,
    );
    fn drm_crtc_send_vblank_event(
        crtc: *mut DrmCrtc,
        event: *mut crate::drm_vblank_types::DrmPendingVblankEvent,
    );
    fn drm_crtc_vblank_on(crtc: *mut DrmCrtc);
    fn drm_crtc_vblank_off(crtc: *mut DrmCrtc);
    fn drm_crtc_vblank_start_timer(crtc: *mut DrmCrtc) -> i32;
    fn drm_crtc_vblank_cancel_timer(crtc: *mut DrmCrtc);
    fn drm_crtc_vblank_get_vblank_timeout(crtc: *mut DrmCrtc, timestamp: *mut i64);
}

// upstream: drivers/gpu/drm/drm_vblank_helper.c drm_crtc_vblank_atomic_flush()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_vblank_atomic_flush(
    crtc: *mut DrmCrtc,
    state: *mut DrmAtomicCommit,
) {
    unsafe {
        let dev = (*crtc).dev;
        let crtc_state = drm_atomic_get_new_crtc_state(state, crtc);
        let event = (*crtc_state).event;
        spin_lock_irq(&mut (*dev).event_lock);
        (*crtc_state).event = core::ptr::null_mut();
        if !event.is_null() {
            if drm_crtc_vblank_get(crtc) == 0 {
                drm_crtc_arm_vblank_event(crtc, event.cast());
            } else {
                drm_crtc_send_vblank_event(crtc, event.cast());
            }
        }
        spin_unlock_irq(&mut (*dev).event_lock);
    }
}

// upstream: drivers/gpu/drm/drm_vblank_helper.c drm_crtc_vblank_atomic_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_vblank_atomic_enable(
    crtc: *mut DrmCrtc,
    _state: *mut DrmAtomicCommit,
) {
    unsafe { drm_crtc_vblank_on(crtc) };
}

// upstream: drivers/gpu/drm/drm_vblank_helper.c drm_crtc_vblank_atomic_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_vblank_atomic_disable(
    crtc: *mut DrmCrtc,
    _state: *mut DrmAtomicCommit,
) {
    unsafe { drm_crtc_vblank_off(crtc) };
}

// upstream: drivers/gpu/drm/drm_vblank_helper.c drm_crtc_vblank_helper_enable_vblank_timer()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_vblank_helper_enable_vblank_timer(crtc: *mut DrmCrtc) -> i32 {
    unsafe { drm_crtc_vblank_start_timer(crtc) }
}

// upstream: drivers/gpu/drm/drm_vblank_helper.c drm_crtc_vblank_helper_disable_vblank_timer()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_vblank_helper_disable_vblank_timer(crtc: *mut DrmCrtc) {
    unsafe { drm_crtc_vblank_cancel_timer(crtc) };
}

// upstream: drivers/gpu/drm/drm_vblank_helper.c drm_crtc_vblank_helper_get_vblank_timestamp_from_timer()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_vblank_helper_get_vblank_timestamp_from_timer(
    crtc: *mut DrmCrtc,
    _max_error: *mut i32,
    vblank_time: *mut i64,
    _in_vblank_irq: bool,
) -> bool {
    unsafe { drm_crtc_vblank_get_vblank_timeout(crtc, vblank_time) };
    true
}
