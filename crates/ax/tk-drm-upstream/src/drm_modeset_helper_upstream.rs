// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_modeset_helper.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3. Plane, CRTC, probe, client
//! and atomic-helper callees are owned by other batches; they are declared
//! `extern "C"` here and resolved through `crate::pending_stubs` until
//! those files land.

use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of};

use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{EINVAL, IS_ERR, PTR_ERR};
use tk_linux_kapi::list::{list_move_tail, list_splice};
use tk_linux_kapi::locks::{spin_lock_irq_raw, spin_unlock_irq_raw};
use tk_linux_kapi::memory::kfree;
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};
use tk_linux_kapi::types::ListHead;

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_connector_types::DrmConnector;
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcFuncs};
use crate::drm_device_types::DrmDevice;
use crate::drm_fourcc_types::DrmFormatInfo;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_plane_types::{DrmPlane, DrmPlaneFuncs};
use crate::uapi_drm_mode_types::DrmModeFbCmd2;
use crate::uapi_drm_consts::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, DRM_MODE_CONNECTOR_DSI, DRM_MODE_CONNECTOR_LVDS, DRM_MODE_CONNECTOR_eDP};

// upstream: include/drm/drm_plane.h enum drm_plane_type: DRM_PLANE_TYPE_PRIMARY
const DRM_PLANE_TYPE_PRIMARY: u32 = 1;

// Plane, CRTC, probe, client and atomic-helper callees (see pending_stubs/).
unsafe extern "C" {
    fn drm_plane_helper_update_primary(
        plane: *mut DrmPlane,
        crtc: *mut DrmCrtc,
        fb: *mut DrmFramebuffer,
        crtc_x: i32,
        crtc_y: i32,
        crtc_w: u32,
        crtc_h: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
        ctx: *mut crate::drm_modeset_lock_types::DrmModesetAcquireCtx,
    ) -> i32;
    fn drm_plane_helper_disable_primary(
        plane: *mut DrmPlane,
        ctx: *mut crate::drm_modeset_lock_types::DrmModesetAcquireCtx,
    ) -> i32;
    fn drm_plane_helper_destroy(plane: *mut DrmPlane);
    fn __drm_universal_plane_alloc(
        dev: *mut DrmDevice,
        size: usize,
        offset: usize,
        possible_crtcs: u32,
        funcs: *const DrmPlaneFuncs,
        formats: *const u32,
        format_count: u32,
        format_modifiers: *const u64,
        plane_type: u32,
        name: *const c_char,
        ...
    ) -> *mut c_void;
    fn drm_plane_cleanup(plane: *mut DrmPlane);
    fn drm_crtc_init_with_planes(
        dev: *mut DrmDevice,
        crtc: *mut DrmCrtc,
        primary: *mut DrmPlane,
        cursor: *mut DrmPlane,
        funcs: *const DrmCrtcFuncs,
        name: *const c_char,
        ...
    ) -> i32;
    fn drm_kms_helper_poll_disable(dev: *mut DrmDevice);
    fn drm_kms_helper_poll_enable(dev: *mut DrmDevice);
    fn drm_client_dev_suspend(dev: *mut DrmDevice);
    fn drm_client_dev_resume(dev: *mut DrmDevice);
    fn drm_atomic_helper_suspend(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
    fn drm_atomic_helper_resume(dev: *mut DrmDevice, state: *mut DrmAtomicCommit) -> i32;
}

// upstream: drm_modeset_helper.c safe_modeset_formats[]
static SAFE_MODESET_FORMATS: [u32; 2] = [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888];

// upstream: drm_modeset_helper.c primary_plane_funcs (DRM_PLANE_NON_ATOMIC_FUNCS)
static PRIMARY_PLANE_FUNCS: DrmPlaneFuncs = DrmPlaneFuncs {
    update_plane: Some(drm_plane_helper_update_primary),
    disable_plane: Some(drm_plane_helper_disable_primary),
    destroy: Some(drm_plane_helper_destroy),
    reset: None,
    set_property: None,
    atomic_duplicate_state: None,
    atomic_destroy_state: None,
    atomic_set_property: None,
    atomic_get_property: None,
    late_register: None,
    early_unregister: None,
    atomic_print_state: None,
    format_mod_supported: None,
    format_mod_supported_async: None,
};

// upstream: drm_modeset_helper.c drm_helper_move_panel_connectors_to_head()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_helper_move_panel_connectors_to_head(dev: *mut DrmDevice) {
    let mut panel_list = ListHead {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
    };
    let pl = core::ptr::addr_of_mut!(panel_list);
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        (*pl).next = pl;
        (*pl).prev = pl;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let lock = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.connector_list_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_lock_irq_raw(lock) };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.connector_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // list_for_each_entry_safe(connector, tmp, &connector_list, head)
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let next = unsafe { (*link).next };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let connector = unsafe {
            link.cast::<u8>().sub(offset_of!(DrmConnector, head)) as *mut DrmConnector
        };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let ct = unsafe { (*connector).connector_type } as u32;
        if ct == DRM_MODE_CONNECTOR_LVDS || ct == DRM_MODE_CONNECTOR_eDP || ct == DRM_MODE_CONNECTOR_DSI {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { list_move_tail(core::ptr::addr_of_mut!((*connector).head), pl) };
        }
        link = next;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { list_splice(pl, head) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { spin_unlock_irq_raw(lock) };
}

// upstream: drm_modeset_helper.c drm_helper_mode_fill_fb_struct()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_helper_mode_fill_fb_struct(
    dev: *mut DrmDevice,
    fb: *mut DrmFramebuffer,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { &mut *fb };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mode_cmd = unsafe { &*mode_cmd };

    fb.dev = dev;
    fb.format = info as *mut DrmFormatInfo;
    fb.width = mode_cmd.width;
    fb.height = mode_cmd.height;
    for i in 0..4 {
        fb.pitches[i] = mode_cmd.pitches[i];
        fb.offsets[i] = mode_cmd.offsets[i];
    }
    fb.modifier = mode_cmd.modifier[0];
    fb.flags = mode_cmd.flags as i32;
}

// upstream: drm_modeset_helper.c drm_crtc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_init(dev: *mut DrmDevice, crtc: *mut DrmCrtc, funcs: *const DrmCrtcFuncs) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let primary = unsafe {
        __drm_universal_plane_alloc(
            dev,
            size_of::<DrmPlane>(),
            0,
            0,
            &PRIMARY_PLANE_FUNCS,
            SAFE_MODESET_FORMATS.as_ptr(),
            SAFE_MODESET_FORMATS.len() as u32,
            core::ptr::null(),
            DRM_PLANE_TYPE_PRIMARY,
            core::ptr::null(),
        )
    } as *mut DrmPlane;
    if IS_ERR(primary) {
        return PTR_ERR(primary);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*primary).format_default = true };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe {
        drm_crtc_init_with_planes(dev, crtc, primary, core::ptr::null_mut(), funcs, core::ptr::null())
    };
    if ret != 0 {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            drm_plane_cleanup(primary);
            kfree(primary);
        }
        return ret;
    }

    0
}

// upstream: drm_modeset_helper.c drm_mode_config_helper_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_config_helper_suspend(dev: *mut DrmDevice) -> i32 {
    if dev.is_null() {
        return 0;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { (*dev).mode_config.poll_enabled } {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_kms_helper_poll_disable(dev) };
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_client_dev_suspend(dev) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let state = unsafe { drm_atomic_helper_suspend(dev) };
    if IS_ERR(state) {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_client_dev_resume(dev) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*dev).mode_config.poll_enabled } {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_kms_helper_poll_enable(dev) };
        }
        return PTR_ERR(state);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*dev).mode_config.suspend_state = state };
    0
}

// upstream: drm_modeset_helper.c drm_mode_config_helper_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_config_helper_resume(dev: *mut DrmDevice) -> i32 {
    if dev.is_null() {
        return 0;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if warn_on(unsafe { (*dev).mode_config.suspend_state.is_null() }) {
        return -EINVAL;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let state = unsafe { (*dev).mode_config.suspend_state };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe { drm_atomic_helper_resume(dev, state) };
    if ret != 0 {
        // DRM_ERROR("Failed to resume (%d)\n", ret)
        drm_log_at(
            DrmLogLevel::Error,
            "drm",
            file!(),
            line!(),
            &alloc::format!("*ERROR* Failed to resume ({})", ret),
        );
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*dev).mode_config.suspend_state = core::ptr::null_mut() };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_client_dev_resume(dev) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { (*dev).mode_config.poll_enabled } {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_kms_helper_poll_enable(dev) };
    }
    ret
}
