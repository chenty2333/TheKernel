// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_rps.c

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_regs_upstream::DE_PCU_EVENT;
use crate::intel_display_types_upstream::IntelAtomicState;
use crate::intel_parent_upstream::{
    intel_parent_rps_available, intel_parent_rps_boost_if_not_started,
    intel_parent_rps_ilk_irq_handler, intel_parent_rps_mark_interactive,
};
use tk_drm_upstream::drm_crtc_types::DrmCrtc;
use tk_linux_kapi::types::{DmaFence, WaitQueueEntry, WaitQueueHead};

unsafe extern "C" {
    fn drm_crtc_vblank_get(crtc: *mut DrmCrtc) -> i32;
    fn drm_crtc_vblank_put(crtc: *mut DrmCrtc);
    fn drm_crtc_vblank_waitqueue(crtc: *mut DrmCrtc) -> *mut WaitQueueHead;
    fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_put(fence: *mut DmaFence);
    fn add_wait_queue(wq: *mut WaitQueueHead, entry: *mut WaitQueueEntry);
    fn ilk_enable_display_irq(display: *mut IntelDisplay, mask: u32);
    fn ilk_disable_display_irq(display: *mut IntelDisplay, mask: u32);
    fn __drm_to_display(drm: *mut tk_drm_upstream::drm_device_types::DrmDevice) -> *mut IntelDisplay;
}

#[repr(C)]
struct WaitRpsBoost {
    wait: WaitQueueEntry,
    crtc: *mut DrmCrtc,
    fence: *mut DmaFence,
}

unsafe extern "C" fn do_rps_boost(
    entry: *mut WaitQueueEntry,
    _mode: u32,
    _flags: i32,
    _key: *mut c_void,
) -> i32 {
    let wait = entry.cast::<WaitRpsBoost>();
    let display = __drm_to_display((*(*wait).crtc).dev);

    intel_parent_rps_boost_if_not_started(display, (*wait).fence.cast());
    dma_fence_put((*wait).fence);
    drm_crtc_vblank_put((*wait).crtc);

    tk_linux_kapi::list::list_del(&mut (*wait).wait.entry);
    tk_linux_kapi::memory::kfree(wait);
    1
}

#[inline]
unsafe fn display_ver(display: *mut IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        (*display).info.__runtime_info.ip.ver
    }
}

// upstream: intel_display_rps.c intel_display_rps_boost_after_vblank()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rps_boost_after_vblank(
    crtc: *mut DrmCrtc,
    fence: *mut DmaFence,
) {
    let display = __drm_to_display((*crtc).dev);

    if !intel_parent_rps_available(display) {
        return;
    }

    if display_ver(display) < 6 {
        return;
    }

    if drm_crtc_vblank_get(crtc) != 0 {
        return;
    }

    let wait = tk_linux_kapi::memory::kmalloc(
        core::mem::size_of::<WaitRpsBoost>(),
        tk_linux_kapi::config::GFP_KERNEL,
    )
    .cast::<WaitRpsBoost>();

    if wait.is_null() {
        drm_crtc_vblank_put(crtc);
        return;
    }

    (*wait).fence = dma_fence_get(fence);
    (*wait).crtc = crtc;
    (*wait).wait.func = Some(do_rps_boost);
    (*wait).wait.flags = 0;
    tk_linux_kapi::list::INIT_LIST_HEAD(&mut (*wait).wait.entry);

    add_wait_queue(drm_crtc_vblank_waitqueue(crtc), &mut (*wait).wait);
}

// upstream: intel_display_rps.c intel_display_rps_mark_interactive()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_rps_mark_interactive(
    display: *mut IntelDisplay,
    state: *mut IntelAtomicState,
    interactive: bool,
) {
    if !intel_parent_rps_available(display) {
        return;
    }

    if (*state).rps_interactive == interactive {
        return;
    }

    intel_parent_rps_mark_interactive(display, interactive);
    (*state).rps_interactive = interactive;
}

// upstream: intel_display_rps.c ilk_display_rps_enable()
// Collides with kernel/src/drm/intel/upstream_gt.rs: exported without no_mangle
pub unsafe extern "C" fn ilk_display_rps_enable(display: *mut IntelDisplay) {
    tk_linux_kapi::locks::spin_lock(&mut (*display).irq.lock);
    ilk_enable_display_irq(display, DE_PCU_EVENT);
    tk_linux_kapi::locks::spin_unlock(&mut (*display).irq.lock);
}

// upstream: intel_display_rps.c ilk_display_rps_disable()
// Collides with kernel/src/drm/intel/upstream_gt.rs: exported without no_mangle
pub unsafe extern "C" fn ilk_display_rps_disable(display: *mut IntelDisplay) {
    tk_linux_kapi::locks::spin_lock(&mut (*display).irq.lock);
    ilk_disable_display_irq(display, DE_PCU_EVENT);
    tk_linux_kapi::locks::spin_unlock(&mut (*display).irq.lock);
}

// upstream: intel_display_rps.c ilk_display_rps_irq_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_display_rps_irq_handler(display: *mut IntelDisplay) {
    intel_parent_rps_ilk_irq_handler(display);
}
