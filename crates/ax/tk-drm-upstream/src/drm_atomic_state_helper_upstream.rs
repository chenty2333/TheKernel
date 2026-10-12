// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_atomic_state_helper.c` (batch B2a).
//!
//! State reset, duplicate, and destroy helpers for CRTCs, planes, connectors,
//! private objects, and bridges.
//!
//! Deviations:
//! - Bridges and writeback jobs are stubbed until their batches land.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::mem::size_of;

use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{GFP_KERNEL, ENOMEM, ERR_PTR};
use tk_linux_kapi::types::DmaFence;

use crate::drm_atomic_types::{DrmAtomicCommit, DrmBridgeState, DrmCrtcCommit, DrmPrivateObj, DrmPrivateState};
use crate::drm_connector_types::{DrmConnector, DrmConnectorState};
use crate::drm_connector_upstream::{drm_connector_get, drm_connector_put};
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState};
use crate::drm_device_types::DrmDevice;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_mode_object_upstream::drm_object_property_get_default_value;
use crate::drm_plane_types::{DrmPlane, DrmPlaneState};
use crate::drm_property_types::DrmPropertyBlob;
use crate::drm_vblank_upstream::{drm_crtc_vblank_reset, drm_dev_has_vblank};
use crate::uapi_drm_consts::DRM_MODE_ROTATE_0;

pub const DRM_BLEND_ALPHA_OPAQUE: u16 = 0xffff;
pub const DRM_MODE_BLEND_PREMULTI: u16 = 0;

unsafe extern "C" {
    fn drm_property_blob_get(blob: *mut DrmPropertyBlob) -> *mut DrmPropertyBlob;
    fn drm_property_blob_put(blob: *mut DrmPropertyBlob);
    fn drm_mode_object_get(obj: *mut crate::drm_mode_object_types::DrmModeObject);
    fn drm_mode_object_put(obj: *mut crate::drm_mode_object_types::DrmModeObject);
    fn dma_fence_put(fence: *mut DmaFence);
    fn __drm_crtc_commit_free(kref: *mut tk_linux_kapi::types::Kref);
    fn drm_atomic_get_old_connector_state(state: *const DrmAtomicCommit, connector: *const DrmConnector) -> *mut DrmConnectorState;
    fn drm_atomic_get_new_connector_state(state: *const DrmAtomicCommit, connector: *const DrmConnector) -> *mut DrmConnectorState;
    fn drm_atomic_get_new_crtc_state(state: *const DrmAtomicCommit, crtc: *const DrmCrtc) -> *mut DrmCrtcState;
    fn drm_writeback_cleanup_job(job: *mut c_void);
}

#[inline]
unsafe fn drm_framebuffer_get(fb: *mut DrmFramebuffer) {
    if !fb.is_null() {
        unsafe { drm_mode_object_get(core::ptr::addr_of_mut!((*fb).base)); }
    }
}

#[inline]
unsafe fn drm_framebuffer_put(fb: *mut DrmFramebuffer) {
    if !fb.is_null() {
        unsafe { drm_mode_object_put(core::ptr::addr_of_mut!((*fb).base)); }
    }
}

#[inline]
pub unsafe fn drm_crtc_commit_put(commit: *mut DrmCrtcCommit) {
    if !commit.is_null() {
        unsafe {
            tk_linux_kapi::memory::kref_put(core::ptr::addr_of_mut!((*commit).r#ref), __drm_crtc_commit_free);
        }
    }
}

#[inline]
pub unsafe fn drm_atomic_crtc_effectively_active(state: *const DrmCrtcState) -> bool {
    unsafe { (*state).active || (*state).self_refresh_active }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_crtc_state_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_crtc_state_reset(
    crtc_state: *mut DrmCrtcState,
    crtc: *mut DrmCrtc,
) {
    unsafe {
        (*crtc_state).crtc = crtc;
        (*crtc_state).background_color = 0xffff_0000_0000_0000; // DRM_ARGB64_PREP(0xffff, 0, 0, 0)
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_crtc_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_crtc_reset(
    crtc: *mut DrmCrtc,
    crtc_state: *mut DrmCrtcState,
) {
    unsafe {
        if !crtc_state.is_null() {
            __drm_atomic_helper_crtc_state_reset(crtc_state, crtc);
        }
        if drm_dev_has_vblank((*crtc).dev) {
            drm_crtc_vblank_reset(crtc);
        }
        (*crtc).state = crtc_state;
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_crtc_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_crtc_reset(crtc: *mut DrmCrtc) {
    unsafe {
        let crtc_state = tk_linux_kapi::memory::kzalloc(size_of::<DrmCrtcState>(), GFP_KERNEL) as *mut DrmCrtcState;
        if !(*crtc).state.is_null() {
            if let Some(destroy) = (*(*crtc).funcs).atomic_destroy_state {
                destroy(crtc, (*crtc).state);
            }
        }
        __drm_atomic_helper_crtc_reset(crtc, crtc_state);
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_crtc_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_crtc_duplicate_state(
    crtc: *mut DrmCrtc,
    state: *mut DrmCrtcState,
) {
    unsafe {
        core::ptr::copy_nonoverlapping((*crtc).state, state, 1);

        if !(*state).mode_blob.is_null() {
            drm_property_blob_get((*state).mode_blob);
        }
        if !(*state).degamma_lut.is_null() {
            drm_property_blob_get((*state).degamma_lut);
        }
        if !(*state).ctm.is_null() {
            drm_property_blob_get((*state).ctm);
        }
        if !(*state).gamma_lut.is_null() {
            drm_property_blob_get((*state).gamma_lut);
        }
        (*state).set_mode_changed(false);
        (*state).set_active_changed(false);
        (*state).set_planes_changed(false);
        (*state).set_connectors_changed(false);
        (*state).set_color_mgmt_changed(false);
        (*state).set_zpos_changed(false);
        (*state).commit = core::ptr::null_mut();
        (*state).event = core::ptr::null_mut();
        (*state).async_flip = false;

        /* Self refresh should be canceled when a new update is available */
        (*state).active = drm_atomic_crtc_effectively_active(state);
        (*state).self_refresh_active = false;
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_crtc_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_crtc_duplicate_state(
    crtc: *mut DrmCrtc,
) -> *mut DrmCrtcState {
    unsafe {
        if warn_on((*crtc).state.is_null()) {
            return core::ptr::null_mut();
        }
        let state = tk_linux_kapi::memory::kmalloc(size_of::<DrmCrtcState>(), GFP_KERNEL) as *mut DrmCrtcState;
        if !state.is_null() {
            __drm_atomic_helper_crtc_duplicate_state(crtc, state);
        }
        state
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_crtc_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_crtc_destroy_state(state: *mut DrmCrtcState) {
    unsafe {
        if !(*state).commit.is_null() {
            if !(*state).event.is_null() && (*(*state).commit).abort_completion {
                drm_crtc_commit_put((*state).commit);
            }

            tk_linux_kapi::memory::kfree((*(*state).commit).event);
            (*(*state).commit).event = core::ptr::null_mut();

            drm_crtc_commit_put((*state).commit);
        }

        drm_property_blob_put((*state).mode_blob);
        drm_property_blob_put((*state).degamma_lut);
        drm_property_blob_put((*state).ctm);
        drm_property_blob_put((*state).gamma_lut);
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_crtc_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_crtc_destroy_state(
    _crtc: *mut DrmCrtc,
    state: *mut DrmCrtcState,
) {
    unsafe {
        __drm_atomic_helper_crtc_destroy_state(state);
        tk_linux_kapi::memory::kfree(state);
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_plane_state_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_plane_state_reset(
    plane_state: *mut DrmPlaneState,
    plane: *mut DrmPlane,
) {
    unsafe {
        let mut val: u64 = 0;

        (*plane_state).plane = plane;
        (*plane_state).rotation = DRM_MODE_ROTATE_0;

        (*plane_state).alpha = DRM_BLEND_ALPHA_OPAQUE;
        (*plane_state).pixel_blend_mode = DRM_MODE_BLEND_PREMULTI;

        if !(*plane).color_encoding_property.is_null() {
            if drm_object_property_get_default_value(
                core::ptr::addr_of_mut!((*plane).base),
                (*plane).color_encoding_property,
                &mut val,
            ) == 0 {
                (*plane_state).color_encoding = val as u32;
            }
        }

        if !(*plane).color_range_property.is_null() {
            if drm_object_property_get_default_value(
                core::ptr::addr_of_mut!((*plane).base),
                (*plane).color_range_property,
                &mut val,
            ) == 0 {
                (*plane_state).color_range = val as u32;
            }
        }

        if !(*plane).color_pipeline_property.is_null() {
            /* default is always NULL, i.e., bypass */
            (*plane_state).color_pipeline = core::ptr::null_mut();
        }

        if !(*plane).zpos_property.is_null() {
            if drm_object_property_get_default_value(
                core::ptr::addr_of_mut!((*plane).base),
                (*plane).zpos_property,
                &mut val,
            ) == 0 {
                (*plane_state).zpos = val as u32;
                (*plane_state).normalized_zpos = val as u32;
            }
        }

        if !(*plane).hotspot_x_property.is_null() {
            if drm_object_property_get_default_value(
                core::ptr::addr_of_mut!((*plane).base),
                (*plane).hotspot_x_property,
                &mut val,
            ) == 0 {
                (*plane_state).hotspot_x = val as i32;
            }
        }

        if !(*plane).hotspot_y_property.is_null() {
            if drm_object_property_get_default_value(
                core::ptr::addr_of_mut!((*plane).base),
                (*plane).hotspot_y_property,
                &mut val,
            ) == 0 {
                (*plane_state).hotspot_y = val as i32;
            }
        }
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_plane_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_plane_reset(
    plane: *mut DrmPlane,
    plane_state: *mut DrmPlaneState,
) {
    unsafe {
        if !plane_state.is_null() {
            __drm_atomic_helper_plane_state_reset(plane_state, plane);
        }
        (*plane).state = plane_state;
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_plane_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_plane_reset(plane: *mut DrmPlane) {
    unsafe {
        if !(*plane).state.is_null() {
            __drm_atomic_helper_plane_destroy_state((*plane).state);
        }

        tk_linux_kapi::memory::kfree((*plane).state);
        let plane_state = tk_linux_kapi::memory::kzalloc(size_of::<DrmPlaneState>(), GFP_KERNEL) as *mut DrmPlaneState;
        (*plane).state = plane_state;
        if !plane_state.is_null() {
            __drm_atomic_helper_plane_reset(plane, plane_state);
        }
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_plane_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_plane_duplicate_state(
    plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) {
    unsafe {
        core::ptr::copy_nonoverlapping((*plane).state, state, 1);

        if !(*state).fb.is_null() {
            drm_framebuffer_get((*state).fb);
        }

        (*state).fence = core::ptr::null_mut();
        (*state).commit = core::ptr::null_mut();
        (*state).fb_damage_clips = core::ptr::null_mut();
        (*state).set_color_mgmt_changed(false);
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_plane_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_plane_duplicate_state(
    plane: *mut DrmPlane,
) -> *mut DrmPlaneState {
    unsafe {
        if warn_on((*plane).state.is_null()) {
            return core::ptr::null_mut();
        }

        let state = tk_linux_kapi::memory::kmalloc(size_of::<DrmPlaneState>(), GFP_KERNEL) as *mut DrmPlaneState;
        if !state.is_null() {
            __drm_atomic_helper_plane_duplicate_state(plane, state);
        }
        state
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_plane_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_plane_destroy_state(state: *mut DrmPlaneState) {
    unsafe {
        if !(*state).fb.is_null() {
            drm_framebuffer_put((*state).fb);
        }

        if !(*state).fence.is_null() {
            dma_fence_put((*state).fence);
        }

        if !(*state).commit.is_null() {
            drm_crtc_commit_put((*state).commit);
        }

        drm_property_blob_put((*state).fb_damage_clips);
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_plane_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_plane_destroy_state(
    _plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) {
    unsafe {
        __drm_atomic_helper_plane_destroy_state(state);
        tk_linux_kapi::memory::kfree(state);
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_connector_state_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_connector_state_reset(
    conn_state: *mut DrmConnectorState,
    connector: *mut DrmConnector,
) {
    unsafe {
        (*conn_state).connector = connector;
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_connector_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_connector_reset(
    connector: *mut DrmConnector,
    conn_state: *mut DrmConnectorState,
) {
    unsafe {
        if !conn_state.is_null() {
            __drm_atomic_helper_connector_state_reset(conn_state, connector);
        }
        (*connector).state = conn_state;
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_connector_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_reset(connector: *mut DrmConnector) {
    unsafe {
        let conn_state = tk_linux_kapi::memory::kzalloc(size_of::<DrmConnectorState>(), GFP_KERNEL) as *mut DrmConnectorState;

        if !(*connector).state.is_null() {
            __drm_atomic_helper_connector_destroy_state((*connector).state);
        }

        tk_linux_kapi::memory::kfree((*connector).state);
        __drm_atomic_helper_connector_reset(connector, conn_state);
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_connector_tv_margins_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_tv_margins_reset(connector: *mut DrmConnector) {
    unsafe {
        let cmdline = core::ptr::addr_of_mut!((*connector).cmdline_mode);
        let state = (*connector).state;

        (*state).tv.margins.left = (*cmdline).tv_margins.left;
        (*state).tv.margins.right = (*cmdline).tv_margins.right;
        (*state).tv.margins.top = (*cmdline).tv_margins.top;
        (*state).tv.margins.bottom = (*cmdline).tv_margins.bottom;
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_connector_tv_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_tv_reset(connector: *mut DrmConnector) {
    unsafe {
        let dev = (*connector).dev;
        let cmdline = core::ptr::addr_of_mut!((*connector).cmdline_mode);
        let state = (*connector).state;
        let mut val: u64 = 0;

        let mut prop = (*dev).mode_config.tv_mode_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.mode = val as u32;
            }
        }

        if (*cmdline).tv_mode_specified {
            (*state).tv.mode = (*cmdline).tv_mode;
        }

        prop = (*dev).mode_config.tv_select_subconnector_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.select_subconnector = val as u32;
            }
        }

        prop = (*dev).mode_config.tv_subconnector_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.subconnector = val as u32;
            }
        }

        prop = (*dev).mode_config.tv_brightness_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.brightness = val as u32;
            }
        }

        prop = (*dev).mode_config.tv_contrast_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.contrast = val as u32;
            }
        }

        prop = (*dev).mode_config.tv_flicker_reduction_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.flicker_reduction = val as u32;
            }
        }

        prop = (*dev).mode_config.tv_overscan_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.overscan = val as u32;
            }
        }

        prop = (*dev).mode_config.tv_saturation_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.saturation = val as u32;
            }
        }

        prop = (*dev).mode_config.tv_hue_property;
        if !prop.is_null() {
            if drm_object_property_get_default_value(core::ptr::addr_of_mut!((*connector).base), prop, &mut val) == 0 {
                (*state).tv.hue = val as u32;
            }
        }

        drm_atomic_helper_connector_tv_margins_reset(connector);
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_connector_tv_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_tv_check(
    connector: *mut DrmConnector,
    state: *mut DrmAtomicCommit,
) -> i32 {
    unsafe {
        let old_conn_state = drm_atomic_get_old_connector_state(state, connector);
        let new_conn_state = drm_atomic_get_new_connector_state(state, connector);

        let crtc = (*new_conn_state).crtc;
        if crtc.is_null() {
            return 0;
        }

        let crtc_state = drm_atomic_get_new_crtc_state(state, crtc);
        if crtc_state.is_null() {
            return -tk_linux_kapi::config::EINVAL;
        }

        if (*old_conn_state).tv.mode != (*new_conn_state).tv.mode {
            (*crtc_state).set_mode_changed(true);
        }

        if (*old_conn_state).tv.margins.left != (*new_conn_state).tv.margins.left
            || (*old_conn_state).tv.margins.right != (*new_conn_state).tv.margins.right
            || (*old_conn_state).tv.margins.top != (*new_conn_state).tv.margins.top
            || (*old_conn_state).tv.margins.bottom != (*new_conn_state).tv.margins.bottom
            || (*old_conn_state).tv.mode != (*new_conn_state).tv.mode
            || (*old_conn_state).tv.brightness != (*new_conn_state).tv.brightness
            || (*old_conn_state).tv.contrast != (*new_conn_state).tv.contrast
            || (*old_conn_state).tv.flicker_reduction != (*new_conn_state).tv.flicker_reduction
            || (*old_conn_state).tv.overscan != (*new_conn_state).tv.overscan
            || (*old_conn_state).tv.saturation != (*new_conn_state).tv.saturation
            || (*old_conn_state).tv.hue != (*new_conn_state).tv.hue
        {
            (*crtc_state).set_connectors_changed(true);
        }

        0
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_connector_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_connector_duplicate_state(
    connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
) {
    unsafe {
        core::ptr::copy_nonoverlapping((*connector).state, state, 1);
        if !(*state).crtc.is_null() {
            drm_connector_get(connector);
        }
        (*state).commit = core::ptr::null_mut();

        if !(*state).hdr_output_metadata.is_null() {
            drm_property_blob_get((*state).hdr_output_metadata);
        }

        /* Don't copy over a writeback job, they are used only once */
        (*state).writeback_job = core::ptr::null_mut();
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_connector_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_duplicate_state(
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    unsafe {
        if warn_on((*connector).state.is_null()) {
            return core::ptr::null_mut();
        }

        let state = tk_linux_kapi::memory::kmalloc(size_of::<DrmConnectorState>(), GFP_KERNEL) as *mut DrmConnectorState;
        if !state.is_null() {
            __drm_atomic_helper_connector_duplicate_state(connector, state);
        }

        state
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_connector_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_connector_destroy_state(state: *mut DrmConnectorState) {
    unsafe {
        if !(*state).crtc.is_null() {
            drm_connector_put((*state).connector);
        }

        if !(*state).commit.is_null() {
            drm_crtc_commit_put((*state).commit);
        }

        if !(*state).writeback_job.is_null() {
            drm_writeback_cleanup_job((*state).writeback_job);
        }

        drm_property_blob_put((*state).hdr_output_metadata);
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_connector_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_destroy_state(
    _connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
) {
    unsafe {
        __drm_atomic_helper_connector_destroy_state(state);
        tk_linux_kapi::memory::kfree(state);
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_private_obj_create_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_private_obj_create_state(
    obj: *mut DrmPrivateObj,
    state: *mut DrmPrivateState,
) {
    unsafe {
        if !state.is_null() {
            (*state).obj = obj;
        }
        (*obj).state = state;
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_private_obj_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_private_obj_duplicate_state(
    obj: *mut DrmPrivateObj,
    state: *mut DrmPrivateState,
) {
    unsafe {
        core::ptr::copy_nonoverlapping((*obj).state, state, 1);
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_bridge_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_bridge_duplicate_state(
    bridge: *mut c_void,
    state: *mut DrmBridgeState,
) {
    unsafe {
        __drm_atomic_helper_private_obj_duplicate_state(bridge as *mut DrmPrivateObj, core::ptr::addr_of_mut!((*state).base));
        (*state).bridge = bridge;
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_bridge_duplicate_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_bridge_duplicate_state(
    bridge: *mut c_void,
) -> *mut DrmBridgeState {
    unsafe {
        let p_obj = bridge as *mut DrmPrivateObj;
        if warn_on((*p_obj).state.is_null()) {
            return core::ptr::null_mut();
        }

        let new_state = tk_linux_kapi::memory::kzalloc(size_of::<DrmBridgeState>(), GFP_KERNEL) as *mut DrmBridgeState;
        if !new_state.is_null() {
            __drm_atomic_helper_bridge_duplicate_state(bridge, new_state);
        }

        new_state
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_bridge_destroy_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_bridge_destroy_state(
    _bridge: *mut c_void,
    state: *mut DrmBridgeState,
) {
    unsafe {
        tk_linux_kapi::memory::kfree(state);
    }
}

// upstream: drm_atomic_state_helper.c __drm_atomic_helper_bridge_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_bridge_reset(
    bridge: *mut c_void,
    state: *mut DrmBridgeState,
) {
    unsafe {
        core::ptr::write_bytes(state, 0, 1);
        __drm_atomic_helper_private_obj_create_state(bridge as *mut DrmPrivateObj, core::ptr::addr_of_mut!((*state).base));
        (*state).bridge = bridge;
    }
}

// upstream: drm_atomic_state_helper.c drm_atomic_helper_bridge_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_bridge_reset(
    bridge: *mut c_void,
) -> *mut DrmBridgeState {
    unsafe {
        let bridge_state = tk_linux_kapi::memory::kzalloc(size_of::<DrmBridgeState>(), GFP_KERNEL) as *mut DrmBridgeState;
        if bridge_state.is_null() {
            return ERR_PTR(-ENOMEM);
        }

        __drm_atomic_helper_bridge_reset(bridge, bridge_state);
        bridge_state
    }
}
