// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_parent.c

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::IntelInitialPlaneConfig,
};
use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_framebuffer_types::DrmFramebuffer,
    drm_plane_types::DrmPlaneState,
};

#[repr(C)]
pub struct IntelDisplayParentInterface {
    pub bo: *const IntelDisplayBoInterface,
    pub dpt: *const IntelDisplayDptInterface,
    pub dsb: *const IntelDisplayDsbInterface,
    pub fb_pin: *const IntelDisplayFbPinInterface,
    pub frontbuffer: *const IntelDisplayFrontbufferInterface,
    pub hdcp: *const IntelDisplayHdcpInterface,
    pub initial_plane: *const IntelDisplayInitialPlaneInterface,
    pub irq: *const IntelDisplayIrqInterface,
    pub panic: *const IntelDisplayPanicInterface,
    pub overlay: *const IntelDisplayOverlayInterface,
    pub pc8: *const IntelDisplayPc8Interface,
    pub pcode: *const IntelDisplayPcodeInterface,
    pub rpm: *const IntelDisplayRpmInterface,
    pub rps: *const IntelDisplayRpsInterface,
    pub stolen: *const IntelDisplayStolenInterface,
    pub vlv_iosf: *const IntelDisplayVlvIosfInterface,
    // Generic independent functions
    pub fence_priority_display: Option<unsafe extern "C" fn(fence: *mut c_void)>,
    pub has_auxccs: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> bool>,
    pub has_fenced_regions: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> bool>,
    pub vgpu_active: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> bool>,
}

#[repr(C)]
pub struct IntelDisplayBoInterface {
    pub is_tiled: Option<unsafe extern "C" fn(obj: *mut c_void) -> bool>,
    pub is_userptr: Option<unsafe extern "C" fn(obj: *mut c_void) -> bool>,
    pub is_shmem: Option<unsafe extern "C" fn(obj: *mut c_void) -> bool>,
    pub is_protected: Option<unsafe extern "C" fn(obj: *mut c_void) -> bool>,
    pub key_check: Option<unsafe extern "C" fn(obj: *mut c_void) -> i32>,
    pub fb_mmap: Option<unsafe extern "C" fn(obj: *mut c_void, vma: *mut c_void) -> i32>,
    pub read_from_page: Option<unsafe extern "C" fn(obj: *mut c_void, offset: u64, dst: *mut c_void, size: i32) -> i32>,
    pub describe: Option<unsafe extern "C" fn(m: *mut c_void, obj: *mut c_void)>,
    pub framebuffer_init: Option<unsafe extern "C" fn(obj: *mut c_void, mode_cmd: *mut c_void) -> i32>,
    pub framebuffer_fini: Option<unsafe extern "C" fn(obj: *mut c_void)>,
    pub framebuffer_lookup: Option<unsafe extern "C" fn(drm: *mut DrmDevice, filp: *mut c_void, user_mode_cmd: *const c_void) -> *mut c_void>,
    pub fbdev_create: Option<unsafe extern "C" fn(drm: *mut DrmDevice, size: i32) -> *mut c_void>,
    pub fbdev_destroy: Option<unsafe extern "C" fn(obj: *mut c_void)>,
    pub fbdev_fill_info: Option<unsafe extern "C" fn(obj: *mut c_void, info: *mut c_void, vma: *mut c_void) -> i32>,
    pub fbdev_pitch_align: Option<unsafe extern "C" fn(stride: u32) -> u32>,
}

#[repr(C)]
pub struct IntelDisplayDptInterface {
    pub create: Option<unsafe extern "C" fn(obj: *mut c_void, size: usize) -> *mut c_void>,
    pub destroy: Option<unsafe extern "C" fn(dpt: *mut c_void)>,
    pub suspend: Option<unsafe extern "C" fn(dpt: *mut c_void)>,
    pub resume: Option<unsafe extern "C" fn(dpt: *mut c_void)>,
}

/// Source: `include/drm/intel/display_parent_interface.h`.
/// The parent owns command-buffer storage and mappings until cleanup.
#[repr(C)]
pub struct IntelDisplayDsbInterface {
    pub ggtt_offset: Option<unsafe extern "C" fn(dsb_buf: *mut c_void) -> u32>,
    pub write: Option<unsafe extern "C" fn(dsb_buf: *mut c_void, idx: u32, val: u32)>,
    pub read: Option<unsafe extern "C" fn(dsb_buf: *mut c_void, idx: u32) -> u32>,
    pub fill:
        Option<unsafe extern "C" fn(dsb_buf: *mut c_void, idx: u32, val: u32, size: usize)>,
    pub create:
        Option<unsafe extern "C" fn(drm: *mut DrmDevice, size: usize) -> *mut c_void>,
    pub cleanup: Option<unsafe extern "C" fn(dsb_buf: *mut c_void)>,
    pub flush_map: Option<unsafe extern "C" fn(dsb_buf: *mut c_void)>,
}

/// Source: `include/drm/intel/display_parent_interface.h`.
/// The parent owns BIOS framebuffer backing and pin lifetime.
#[repr(C)]
pub struct IntelDisplayInitialPlaneInterface {
    pub alloc_obj: Option<
        unsafe extern "C" fn(
            drm: *mut DrmDevice,
            config: *mut IntelInitialPlaneConfig,
        ) -> *mut c_void,
    >,
    pub setup: Option<
        unsafe extern "C" fn(
            plane_state: *mut DrmPlaneState,
            config: *mut IntelInitialPlaneConfig,
            fb: *mut DrmFramebuffer,
            vma: *mut c_void,
        ) -> i32,
    >,
    pub config_fini: Option<unsafe extern "C" fn(config: *mut IntelInitialPlaneConfig)>,
}

const _: () = assert!(core::mem::size_of::<IntelDisplayDsbInterface>() == 56);
const _: () = assert!(core::mem::size_of::<IntelDisplayInitialPlaneInterface>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParentInterface, dsb) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParentInterface, initial_plane) == 48);
const _: () = assert!(
    core::mem::offset_of!(IntelDisplayParentInterface, fence_priority_display) == 128
);
const _: () = assert!(core::mem::size_of::<IntelDisplayParentInterface>() == 160);

#[repr(C)]
pub struct IntelDisplayFbPinInterface {
    pub ggtt_pin: Option<unsafe extern "C" fn(obj: *mut c_void, pin_params: *const c_void, out_ggtt_vma: *mut *mut c_void, out_offset: *mut u32, out_fence_id: *mut i32) -> i32>,
    pub ggtt_unpin: Option<unsafe extern "C" fn(ggtt_vma: *mut c_void, fence_id: i32)>,
    pub dpt_pin: Option<unsafe extern "C" fn(obj: *mut c_void, dpt: *mut c_void, pin_params: *const c_void, out_dpt_vma: *mut *mut c_void, out_ggtt_vma: *mut *mut c_void, out_offset: *mut u32) -> i32>,
    pub dpt_unpin: Option<unsafe extern "C" fn(dpt: *mut c_void, dpt_vma: *mut c_void, ggtt_vma: *mut c_void)>,
    pub reuse_vma: Option<unsafe extern "C" fn(old_ggtt_vma: *mut c_void, old_obj: *mut c_void, old_view: *const c_void, new_obj: *mut c_void, new_view: *const c_void, out_offset: *mut u32) -> *mut c_void>,
    pub get_map: Option<unsafe extern "C" fn(vma: *mut c_void, map: *mut c_void)>,
}

#[repr(C)]
pub struct IntelDisplayFrontbufferInterface {
    pub get: Option<unsafe extern "C" fn(obj: *mut c_void) -> *mut c_void>,
    pub r#ref: Option<unsafe extern "C" fn(front: *mut c_void)>,
    pub put: Option<unsafe extern "C" fn(front: *mut c_void)>,
    pub flush_for_display: Option<unsafe extern "C" fn(front: *mut c_void)>,
}

#[repr(C)]
pub struct IntelDisplayHdcpInterface {
    pub gsc_msg_send: Option<unsafe extern "C" fn(gsc_context: *mut c_void, msg_in: *mut c_void, msg_in_len: usize, msg_out: *mut c_void, msg_out_len: usize) -> isize>,
    pub gsc_check_status: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> bool>,
    pub gsc_context_alloc: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> *mut c_void>,
    pub gsc_context_free: Option<unsafe extern "C" fn(gsc_context: *mut c_void)>,
}

#[repr(C)]
pub struct IntelDisplayIrqInterface {
    pub enabled: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> bool>,
    pub synchronize: Option<unsafe extern "C" fn(drm: *mut DrmDevice)>,
}

#[repr(C)]
pub struct IntelDisplayOverlayInterface {
    pub is_active: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> bool>,
    pub overlay_on: Option<unsafe extern "C" fn(drm: *mut DrmDevice, frontbuffer_bits: u32) -> i32>,
    pub overlay_continue: Option<unsafe extern "C" fn(drm: *mut DrmDevice, vma: *mut c_void, load_polyphase_filter: bool) -> i32>,
    pub overlay_off: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> i32>,
    pub recover_from_interrupt: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> i32>,
    pub release_old_vid: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> i32>,
    pub reset: Option<unsafe extern "C" fn(drm: *mut DrmDevice)>,
    pub pin_fb: Option<unsafe extern "C" fn(drm: *mut DrmDevice, obj: *mut c_void, offset: *mut u32) -> *mut c_void>,
    pub unpin_fb: Option<unsafe extern "C" fn(drm: *mut DrmDevice, vma: *mut c_void)>,
    pub obj_lookup: Option<unsafe extern "C" fn(drm: *mut DrmDevice, filp: *mut c_void, handle: u32) -> *mut c_void>,
    pub setup: Option<unsafe extern "C" fn(drm: *mut DrmDevice, needs_physical: bool) -> *mut c_void>,
    pub cleanup: Option<unsafe extern "C" fn(drm: *mut DrmDevice)>,
}

#[repr(C)]
pub struct IntelDisplayPanicInterface {
    pub alloc: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub setup: Option<unsafe extern "C" fn(panic: *mut c_void, sb: *mut c_void) -> i32>,
    pub finish: Option<unsafe extern "C" fn(panic: *mut c_void)>,
}

#[repr(C)]
pub struct IntelDisplayPc8Interface {
    pub block: Option<unsafe extern "C" fn(drm: *mut DrmDevice)>,
    pub unblock: Option<unsafe extern "C" fn(drm: *mut DrmDevice)>,
}

#[repr(C)]
pub struct IntelDisplayPcodeInterface {
    pub read: Option<unsafe extern "C" fn(drm: *mut DrmDevice, mbox: u32, val: *mut u32, val1: *mut u32) -> i32>,
    pub write: Option<unsafe extern "C" fn(drm: *mut DrmDevice, mbox: u32, val: u32, timeout_ms: i32) -> i32>,
    pub request: Option<unsafe extern "C" fn(drm: *mut DrmDevice, mbox: u32, request: u32, reply_mask: u32, reply: u32, timeout_base_ms: i32) -> i32>,
}

#[repr(C)]
pub struct IntelDisplayRpmInterface {
    pub get: Option<unsafe extern "C" fn(drm: *const DrmDevice) -> *mut c_void>,
    pub get_raw: Option<unsafe extern "C" fn(drm: *const DrmDevice) -> *mut c_void>,
    pub get_if_in_use: Option<unsafe extern "C" fn(drm: *const DrmDevice) -> *mut c_void>,
    pub get_noresume: Option<unsafe extern "C" fn(drm: *const DrmDevice) -> *mut c_void>,
    pub put: Option<unsafe extern "C" fn(drm: *const DrmDevice, wakeref: *mut c_void)>,
    pub put_raw: Option<unsafe extern "C" fn(drm: *const DrmDevice, wakeref: *mut c_void)>,
    pub put_unchecked: Option<unsafe extern "C" fn(drm: *const DrmDevice)>,
    pub suspended: Option<unsafe extern "C" fn(drm: *const DrmDevice) -> bool>,
    pub assert_held: Option<unsafe extern "C" fn(drm: *const DrmDevice)>,
    pub assert_block: Option<unsafe extern "C" fn(drm: *const DrmDevice)>,
    pub assert_unblock: Option<unsafe extern "C" fn(drm: *const DrmDevice)>,
}

#[repr(C)]
pub struct IntelDisplayRpsInterface {
    pub boost_if_not_started: Option<unsafe extern "C" fn(fence: *mut c_void)>,
    pub mark_interactive: Option<unsafe extern "C" fn(drm: *mut DrmDevice, interactive: bool)>,
    pub ilk_irq_handler: Option<unsafe extern "C" fn(drm: *mut DrmDevice)>,
}

#[repr(C)]
pub struct IntelDisplayStolenInterface {
    pub insert_node_in_range: Option<unsafe extern "C" fn(node: *mut c_void, size: u64, align: u32, start: u64, end: u64) -> i32>,
    pub insert_node: Option<unsafe extern "C" fn(node: *mut c_void, size: u64, align: u32) -> i32>,
    pub remove_node: Option<unsafe extern "C" fn(node: *mut c_void)>,
    pub initialized: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> bool>,
    pub node_allocated: Option<unsafe extern "C" fn(node: *const c_void) -> bool>,
    pub node_offset: Option<unsafe extern "C" fn(node: *const c_void) -> u64>,
    pub area_address: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> u64>,
    pub area_size: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> u64>,
    pub node_address: Option<unsafe extern "C" fn(node: *const c_void) -> u64>,
    pub node_size: Option<unsafe extern "C" fn(node: *const c_void) -> u64>,
    pub node_alloc: Option<unsafe extern "C" fn(drm: *mut DrmDevice) -> *mut c_void>,
    pub node_free: Option<unsafe extern "C" fn(node: *const c_void)>,
}

#[repr(C)]
pub struct IntelDisplayVlvIosfInterface {
    pub get: Option<unsafe extern "C" fn(drm: *mut DrmDevice, unit_mask: usize)>,
    pub put: Option<unsafe extern "C" fn(drm: *mut DrmDevice, unit_mask: usize)>,
    pub read: Option<unsafe extern "C" fn(drm: *mut DrmDevice, unit: i32, addr: u32) -> u32>,
    pub write: Option<unsafe extern "C" fn(drm: *mut DrmDevice, unit: i32, addr: u32, val: u32) -> i32>,
}

#[inline]
pub unsafe fn intel_display_parent(display: *mut IntelDisplay) -> *const IntelDisplayParentInterface {
    if display.is_null() {
        core::ptr::null()
    } else {
        (*display).parent.cast()
    }
}

// dpt
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_dpt_create(
    display: *mut IntelDisplay,
    obj: *mut c_void,
    size: usize,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).dpt.is_null() {
        if let Some(f) = (*(*p).dpt).create {
            return f(obj, size);
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_dpt_destroy(display: *mut IntelDisplay, dpt: *mut c_void) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).dpt.is_null() {
        if let Some(f) = (*(*p).dpt).destroy {
            f(dpt);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_dpt_suspend(display: *mut IntelDisplay, dpt: *mut c_void) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).dpt.is_null() {
        if let Some(f) = (*(*p).dpt).suspend {
            f(dpt);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_dpt_resume(display: *mut IntelDisplay, dpt: *mut c_void) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).dpt.is_null() {
        if let Some(f) = (*(*p).dpt).resume {
            f(dpt);
        }
    }
}

// fb_pin
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_fb_pin_ggtt_pin(
    display: *mut IntelDisplay,
    obj: *mut c_void,
    pin_params: *const c_void,
    out_ggtt_vma: *mut *mut c_void,
    out_offset: *mut u32,
    out_fence_id: *mut i32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).fb_pin.is_null() {
        if let Some(f) = (*(*p).fb_pin).ggtt_pin {
            return f(obj, pin_params, out_ggtt_vma, out_offset, out_fence_id);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_fb_pin_ggtt_unpin(
    display: *mut IntelDisplay,
    ggtt_vma: *mut c_void,
    fence_id: i32,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).fb_pin.is_null() {
        if let Some(f) = (*(*p).fb_pin).ggtt_unpin {
            f(ggtt_vma, fence_id);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_fb_pin_dpt_pin(
    display: *mut IntelDisplay,
    obj: *mut c_void,
    dpt: *mut c_void,
    pin_params: *const c_void,
    out_dpt_vma: *mut *mut c_void,
    out_ggtt_vma: *mut *mut c_void,
    out_offset: *mut u32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).fb_pin.is_null() {
        if let Some(f) = (*(*p).fb_pin).dpt_pin {
            return f(obj, dpt, pin_params, out_dpt_vma, out_ggtt_vma, out_offset);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_fb_pin_dpt_unpin(
    display: *mut IntelDisplay,
    dpt: *mut c_void,
    dpt_vma: *mut c_void,
    ggtt_vma: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).fb_pin.is_null() {
        if let Some(f) = (*(*p).fb_pin).dpt_unpin {
            f(dpt, dpt_vma, ggtt_vma);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_fb_pin_reuse_vma(
    display: *mut IntelDisplay,
    old_ggtt_vma: *mut c_void,
    old_obj: *mut c_void,
    old_view: *const c_void,
    new_obj: *mut c_void,
    new_view: *const c_void,
    out_offset: *mut u32,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).fb_pin.is_null() {
        if let Some(f) = (*(*p).fb_pin).reuse_vma {
            return f(old_ggtt_vma, old_obj, old_view, new_obj, new_view, out_offset);
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_fb_pin_get_map(
    display: *mut IntelDisplay,
    vma: *mut c_void,
    map: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).fb_pin.is_null() {
        if let Some(f) = (*(*p).fb_pin).get_map {
            f(vma, map);
        }
    }
}

// frontbuffer
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_frontbuffer_get(
    display: *mut IntelDisplay,
    obj: *mut c_void,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).frontbuffer.is_null() {
        if let Some(f) = (*(*p).frontbuffer).get {
            return f(obj);
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_frontbuffer_ref(
    display: *mut IntelDisplay,
    front: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).frontbuffer.is_null() {
        if let Some(f) = (*(*p).frontbuffer).r#ref {
            f(front);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_frontbuffer_put(
    display: *mut IntelDisplay,
    front: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).frontbuffer.is_null() {
        if let Some(f) = (*(*p).frontbuffer).put {
            f(front);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_frontbuffer_flush_for_display(
    display: *mut IntelDisplay,
    front: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).frontbuffer.is_null() {
        if let Some(f) = (*(*p).frontbuffer).flush_for_display {
            f(front);
        }
    }
}

// hdcp
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_hdcp_gsc_msg_send(
    display: *mut IntelDisplay,
    gsc_context: *mut c_void,
    msg_in: *mut c_void,
    msg_in_len: usize,
    msg_out: *mut c_void,
    msg_out_len: usize,
) -> isize {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).hdcp.is_null() {
        if let Some(f) = (*(*p).hdcp).gsc_msg_send {
            return f(gsc_context, msg_in, msg_in_len, msg_out, msg_out_len);
        }
    }
    -crate::linux_config::ENODEV as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_hdcp_gsc_check_status(
    display: *mut IntelDisplay,
) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).hdcp.is_null() {
        if let Some(f) = (*(*p).hdcp).gsc_check_status {
            return f((*display).drm);
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_hdcp_gsc_context_alloc(
    display: *mut IntelDisplay,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).hdcp.is_null() {
        if let Some(f) = (*(*p).hdcp).gsc_context_alloc {
            return f((*display).drm);
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_hdcp_gsc_context_free(
    display: *mut IntelDisplay,
    gsc_context: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).hdcp.is_null() {
        if let Some(f) = (*(*p).hdcp).gsc_context_free {
            f(gsc_context);
        }
    }
}

// irq
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_irq_enabled(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).irq.is_null() {
        if let Some(f) = (*(*p).irq).enabled {
            return f((*display).drm);
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_irq_synchronize(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).irq.is_null() {
        if let Some(f) = (*(*p).irq).synchronize {
            f((*display).drm);
        }
    }
}

// overlay
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_is_active(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).is_active {
            return f((*display).drm);
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_on(
    display: *mut IntelDisplay,
    frontbuffer_bits: u32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).overlay_on {
            return f((*display).drm, frontbuffer_bits);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_continue(
    display: *mut IntelDisplay,
    vma: *mut c_void,
    load_polyphase_filter: bool,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).overlay_continue {
            return f((*display).drm, vma, load_polyphase_filter);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_off(display: *mut IntelDisplay) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).overlay_off {
            return f((*display).drm);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_recover_from_interrupt(
    display: *mut IntelDisplay,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).recover_from_interrupt {
            return f((*display).drm);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_release_old_vid(
    display: *mut IntelDisplay,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).release_old_vid {
            return f((*display).drm);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_reset(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).reset {
            f((*display).drm);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_pin_fb(
    display: *mut IntelDisplay,
    obj: *mut c_void,
    offset: *mut u32,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).pin_fb {
            return f((*display).drm, obj, offset);
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_unpin_fb(
    display: *mut IntelDisplay,
    vma: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).unpin_fb {
            f((*display).drm, vma);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_obj_lookup(
    display: *mut IntelDisplay,
    filp: *mut c_void,
    handle: u32,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).obj_lookup {
            return f((*display).drm, filp, handle);
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_setup(
    display: *mut IntelDisplay,
    needs_physical: bool,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).setup {
            return f((*display).drm, needs_physical);
        }
    }
    (-crate::linux_config::ENODEV as isize) as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_overlay_cleanup(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).overlay.is_null() {
        if let Some(f) = (*(*p).overlay).cleanup {
            f((*display).drm);
        }
    }
}

// panic
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_panic_alloc(
    display: *mut IntelDisplay,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).panic.is_null() {
        if let Some(f) = (*(*p).panic).alloc {
            return f();
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_panic_setup(
    display: *mut IntelDisplay,
    panic: *mut c_void,
    sb: *mut c_void,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).panic.is_null() {
        if let Some(f) = (*(*p).panic).setup {
            return f(panic, sb);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_panic_finish(
    display: *mut IntelDisplay,
    panic: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).panic.is_null() {
        if let Some(f) = (*(*p).panic).finish {
            f(panic);
        }
    }
}

// pc8
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_pc8_block(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).pc8.is_null() {
        if let Some(f) = (*(*p).pc8).block {
            f((*display).drm);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_pc8_unblock(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).pc8.is_null() {
        if let Some(f) = (*(*p).pc8).unblock {
            f((*display).drm);
        }
    }
}

// pcode
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_pcode_read(
    display: *mut IntelDisplay,
    mbox: u32,
    val: *mut u32,
    val1: *mut u32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).pcode.is_null() {
        if let Some(f) = (*(*p).pcode).read {
            return f((*display).drm, mbox, val, val1);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_pcode_write_timeout(
    display: *mut IntelDisplay,
    mbox: u32,
    val: u32,
    timeout_ms: i32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).pcode.is_null() {
        if let Some(f) = (*(*p).pcode).write {
            return f((*display).drm, mbox, val, timeout_ms);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_pcode_write(
    display: *mut IntelDisplay,
    mbox: u32,
    val: u32,
) -> i32 {
    intel_parent_pcode_write_timeout(display, mbox, val, 1)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_pcode_request(
    display: *mut IntelDisplay,
    mbox: u32,
    request: u32,
    reply_mask: u32,
    reply: u32,
    timeout_base_ms: i32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).pcode.is_null() {
        if let Some(f) = (*(*p).pcode).request {
            return f((*display).drm, mbox, request, reply_mask, reply, timeout_base_ms);
        }
    }
    -crate::linux_config::ENODEV
}

// rps
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_rps_available(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    !p.is_null() && !(*p).rps.is_null()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_rps_boost_if_not_started(
    display: *mut IntelDisplay,
    fence: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rps.is_null() {
        if let Some(f) = (*(*p).rps).boost_if_not_started {
            f(fence);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_rps_mark_interactive(
    display: *mut IntelDisplay,
    interactive: bool,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rps.is_null() {
        if let Some(f) = (*(*p).rps).mark_interactive {
            f((*display).drm, interactive);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_rps_ilk_irq_handler(display: *mut IntelDisplay) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).rps.is_null() {
        if let Some(f) = (*(*p).rps).ilk_irq_handler {
            f((*display).drm);
        }
    }
}

// stolen
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_insert_node_in_range(
    display: *mut IntelDisplay,
    node: *mut c_void,
    size: u64,
    align: u32,
    start: u64,
    end: u64,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).insert_node_in_range {
            return f(node, size, align, start, end);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_insert_node(
    display: *mut IntelDisplay,
    node: *mut c_void,
    size: u64,
    align: u32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).insert_node {
            return f(node, size, align);
        }
    }
    -crate::linux_config::ENODEV
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_remove_node(
    display: *mut IntelDisplay,
    node: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).remove_node {
            f(node);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_initialized(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).initialized {
            return f((*display).drm);
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_node_allocated(
    display: *mut IntelDisplay,
    node: *const c_void,
) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).node_allocated {
            return f(node);
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_node_offset(
    display: *mut IntelDisplay,
    node: *mut c_void,
) -> u32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).node_offset {
            return f(node) as u32;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_area_address(display: *mut IntelDisplay) -> u64 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).area_address {
            return f((*display).drm);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_area_size(display: *mut IntelDisplay) -> u64 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).area_size {
            return f((*display).drm);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_node_address(
    display: *mut IntelDisplay,
    node: *mut c_void,
) -> u64 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).node_address {
            return f(node);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_node_size(
    display: *mut IntelDisplay,
    node: *const c_void,
) -> u64 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).node_size {
            return f(node);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_node_alloc(
    display: *mut IntelDisplay,
) -> *mut c_void {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).node_alloc {
            return f((*display).drm);
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_stolen_node_free(
    display: *mut IntelDisplay,
    node: *const c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).stolen.is_null() {
        if let Some(f) = (*(*p).stolen).node_free {
            f(node);
        }
    }
}

// vlv iosf
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_vlv_iosf_get(
    display: *mut IntelDisplay,
    unit_mask: usize,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).vlv_iosf.is_null() {
        if let Some(f) = (*(*p).vlv_iosf).get {
            f((*display).drm, unit_mask);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_vlv_iosf_put(
    display: *mut IntelDisplay,
    unit_mask: usize,
) {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).vlv_iosf.is_null() {
        if let Some(f) = (*(*p).vlv_iosf).put {
            f((*display).drm, unit_mask);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_vlv_iosf_read(
    display: *mut IntelDisplay,
    unit: i32,
    addr: u32,
) -> u32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).vlv_iosf.is_null() {
        if let Some(f) = (*(*p).vlv_iosf).read {
            return f((*display).drm, unit, addr);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_vlv_iosf_write(
    display: *mut IntelDisplay,
    unit: i32,
    addr: u32,
    val: u32,
) -> i32 {
    let p = intel_display_parent(display);
    if !p.is_null() && !(*p).vlv_iosf.is_null() {
        if let Some(f) = (*(*p).vlv_iosf).write {
            return f((*display).drm, unit, addr, val);
        }
    }
    -crate::linux_config::EINVAL
}

// generic
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_fence_priority_display(
    display: *mut IntelDisplay,
    fence: *mut c_void,
) {
    let p = intel_display_parent(display);
    if !p.is_null() {
        if let Some(f) = (*p).fence_priority_display {
            f(fence);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_has_auxccs(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() {
        if let Some(f) = (*p).has_auxccs {
            return f((*display).drm);
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_has_fenced_regions(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() {
        if let Some(f) = (*p).has_fenced_regions {
            return f((*display).drm);
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_parent_vgpu_active(display: *mut IntelDisplay) -> bool {
    let p = intel_display_parent(display);
    if !p.is_null() {
        if let Some(f) = (*p).vgpu_active {
            return f((*display).drm);
        }
    }
    false
}
