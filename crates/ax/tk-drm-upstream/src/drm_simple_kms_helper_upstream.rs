// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_simple_kms_helper.c` (Virtio V0).
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::offset_of,
};

use tk_linux_kapi::assert::warn_on;

use crate::{
    drm_atomic_types::DrmAtomicCommit,
    drm_connector_types::DrmConnector,
    drm_crtc_types::{DrmCrtc, DrmCrtcFuncs, DrmCrtcState},
    drm_device_types::DrmDevice,
    drm_encoder_types::{DrmEncoder, DrmEncoderFuncs},
    drm_modes_types::{DrmDisplayMode, DrmModeStatus, MODE_OK},
    drm_modeset_helper_vtables_types::{DrmCrtcHelperFuncs, DrmPlaneHelperFuncs},
    drm_plane_types::{DRM_PLANE_TYPE_PRIMARY, DrmPlane, DrmPlaneFuncs, DrmPlaneState},
    drm_simple_kms_helper_types_upstream::{DrmSimpleDisplayPipe, DrmSimpleDisplayPipeFuncs},
    uapi_drm_consts::DRM_FORMAT_MOD_LINEAR,
};

const DRM_PLANE_NO_SCALING: i32 = 1 << 16;

static SIMPLE_ENCODER_FUNCS: DrmEncoderFuncs = DrmEncoderFuncs {
    reset: None,
    destroy: Some(crate::drm_encoder_upstream::drm_encoder_cleanup),
    late_register: None,
    early_unregister: None,
    debugfs_init: None,
};

static SIMPLE_CRTC_HELPER_FUNCS: DrmCrtcHelperFuncs = DrmCrtcHelperFuncs {
    dpms: None,
    prepare: None,
    commit: None,
    mode_valid: Some(simple_kms_crtc_mode_valid),
    mode_fixup: None,
    mode_set: None,
    mode_set_nofb: None,
    mode_set_base: None,
    disable: None,
    atomic_check: Some(simple_kms_crtc_check),
    atomic_begin: None,
    atomic_flush: Some(crate::drm_vblank_helper_upstream::drm_crtc_vblank_atomic_flush),
    atomic_enable: Some(simple_kms_crtc_enable),
    atomic_disable: Some(simple_kms_crtc_disable),
    get_scanout_position: None,
    handle_vblank_timeout: None,
};

static SIMPLE_CRTC_FUNCS: DrmCrtcFuncs = DrmCrtcFuncs {
    reset: Some(simple_kms_crtc_reset),
    cursor_set: None,
    cursor_set2: None,
    cursor_move: None,
    gamma_set: None,
    destroy: Some(crate::drm_crtc_upstream::drm_crtc_cleanup),
    set_config: Some(crate::drm_atomic_helper_upstream::drm_atomic_helper_set_config),
    page_flip: Some(crate::drm_atomic_helper_upstream::drm_atomic_helper_page_flip),
    page_flip_target: None,
    set_property: None,
    atomic_duplicate_state: Some(simple_kms_crtc_duplicate_state),
    atomic_destroy_state: Some(simple_kms_crtc_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    late_register: None,
    early_unregister: None,
    set_crc_source: None,
    verify_crc_source: None,
    get_crc_sources: None,
    atomic_print_state: None,
    get_vblank_counter: None,
    enable_vblank: Some(simple_kms_crtc_enable_vblank),
    disable_vblank: Some(simple_kms_crtc_disable_vblank),
    get_vblank_timestamp: Some(
        crate::drm_vblank_helper_upstream::drm_crtc_vblank_helper_get_vblank_timestamp_from_timer,
    ),
};

static SIMPLE_PLANE_HELPER_FUNCS: DrmPlaneHelperFuncs = DrmPlaneHelperFuncs {
    prepare_fb: Some(simple_kms_plane_prepare_fb),
    cleanup_fb: Some(simple_kms_plane_cleanup_fb),
    begin_fb_access: Some(simple_kms_plane_begin_fb_access),
    end_fb_access: Some(simple_kms_plane_end_fb_access),
    atomic_check: Some(simple_kms_plane_atomic_check),
    atomic_update: Some(simple_kms_plane_atomic_update),
    atomic_enable: None,
    atomic_disable: None,
    atomic_async_check: None,
    atomic_async_update: None,
    get_scanout_buffer: None,
    panic_flush: None,
};

static SIMPLE_PLANE_FUNCS: DrmPlaneFuncs = DrmPlaneFuncs {
    update_plane: Some(crate::drm_atomic_helper_upstream::drm_atomic_helper_update_plane),
    disable_plane: Some(crate::drm_atomic_helper_upstream::drm_atomic_helper_disable_plane),
    destroy: Some(crate::drm_plane_upstream::drm_plane_cleanup),
    reset: Some(simple_kms_plane_reset),
    set_property: None,
    atomic_duplicate_state: Some(simple_kms_plane_duplicate_state),
    atomic_destroy_state: Some(simple_kms_plane_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    late_register: None,
    early_unregister: None,
    atomic_print_state: None,
    format_mod_supported: Some(simple_kms_format_mod_supported),
    format_mod_supported_async: None,
};

#[inline]
unsafe fn pipe_from_crtc(crtc: *mut DrmCrtc) -> *mut DrmSimpleDisplayPipe {
    crtc.cast()
}

#[inline]
unsafe fn pipe_from_plane(plane: *mut DrmPlane) -> *mut DrmSimpleDisplayPipe {
    plane
        .cast::<u8>()
        .wrapping_sub(offset_of!(DrmSimpleDisplayPipe, plane))
        .cast()
}

#[inline]
unsafe fn pipe_funcs(
    pipe: *mut DrmSimpleDisplayPipe,
) -> Option<&'static DrmSimpleDisplayPipeFuncs> {
    unsafe { (*pipe).funcs.as_ref() }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_mode_valid()
unsafe extern "C" fn simple_kms_crtc_mode_valid(
    crtc: *mut DrmCrtc,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    unsafe {
        let pipe = pipe_from_crtc(crtc);
        pipe_funcs(pipe)
            .and_then(|funcs| funcs.mode_valid)
            .map_or(MODE_OK, |callback| callback(pipe, mode))
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_check()
unsafe extern "C" fn simple_kms_crtc_check(crtc: *mut DrmCrtc, state: *mut DrmAtomicCommit) -> i32 {
    unsafe {
        let crtc_state = crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, crtc);
        if (*crtc_state).enable {
            let ret = crate::drm_atomic_helper_upstream::drm_atomic_helper_check_crtc_primary_plane(
                crtc_state,
            );
            if ret != 0 {
                return ret;
            }
        }
        crate::drm_atomic_upstream::drm_atomic_add_affected_planes(state, crtc)
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_enable()
unsafe extern "C" fn simple_kms_crtc_enable(crtc: *mut DrmCrtc, _state: *mut DrmAtomicCommit) {
    unsafe {
        let pipe = pipe_from_crtc(crtc);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.enable) {
            callback(pipe, (*crtc).state, (*pipe).plane.state);
        }
        crate::drm_vblank_upstream::drm_crtc_vblank_on(crtc);
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_disable()
unsafe extern "C" fn simple_kms_crtc_disable(crtc: *mut DrmCrtc, _state: *mut DrmAtomicCommit) {
    unsafe {
        crate::drm_vblank_upstream::drm_crtc_vblank_off(crtc);
        let pipe = pipe_from_crtc(crtc);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.disable) {
            callback(pipe);
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_reset()
unsafe extern "C" fn simple_kms_crtc_reset(crtc: *mut DrmCrtc) {
    unsafe {
        let pipe = pipe_from_crtc(crtc);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.reset_crtc) {
            callback(pipe);
        } else {
            crate::drm_atomic_state_helper_upstream::drm_atomic_helper_crtc_reset(crtc);
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_duplicate_state()
unsafe extern "C" fn simple_kms_crtc_duplicate_state(crtc: *mut DrmCrtc) -> *mut DrmCrtcState {
    unsafe {
        let pipe = pipe_from_crtc(crtc);
        pipe_funcs(pipe)
            .and_then(|funcs| funcs.duplicate_crtc_state)
            .map_or_else(
                || {
                    crate::drm_atomic_state_helper_upstream::drm_atomic_helper_crtc_duplicate_state(
                        crtc,
                    )
                },
                |callback| callback(pipe),
            )
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_destroy_state()
unsafe extern "C" fn simple_kms_crtc_destroy_state(crtc: *mut DrmCrtc, state: *mut DrmCrtcState) {
    unsafe {
        let pipe = pipe_from_crtc(crtc);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.destroy_crtc_state) {
            callback(pipe, state);
        } else {
            crate::drm_atomic_state_helper_upstream::drm_atomic_helper_crtc_destroy_state(
                crtc, state,
            );
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_enable_vblank()
unsafe extern "C" fn simple_kms_crtc_enable_vblank(crtc: *mut DrmCrtc) -> i32 {
    unsafe {
        let pipe = pipe_from_crtc(crtc);
        pipe_funcs(pipe)
            .and_then(|funcs| funcs.enable_vblank)
            .map_or(0, |callback| callback(pipe))
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_crtc_disable_vblank()
unsafe extern "C" fn simple_kms_crtc_disable_vblank(crtc: *mut DrmCrtc) {
    unsafe {
        let pipe = pipe_from_crtc(crtc);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.disable_vblank) {
            callback(pipe);
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_atomic_check()
unsafe extern "C" fn simple_kms_plane_atomic_check(
    plane: *mut DrmPlane,
    state: *mut DrmAtomicCommit,
) -> i32 {
    unsafe {
        let plane_state = crate::drm_atomic_upstream::drm_atomic_get_new_plane_state(state, plane);
        let pipe = pipe_from_plane(plane);
        let crtc_state =
            crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, &mut (*pipe).crtc);
        let ret = crate::drm_atomic_helper_upstream::drm_atomic_helper_check_plane_state(
            plane_state,
            crtc_state,
            DRM_PLANE_NO_SCALING,
            DRM_PLANE_NO_SCALING,
            false,
            false,
        );
        if ret != 0 || !(*plane_state).visible {
            return ret;
        }
        pipe_funcs(pipe)
            .and_then(|funcs| funcs.check)
            .map_or(0, |callback| callback(pipe, plane_state, crtc_state))
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_atomic_update()
unsafe extern "C" fn simple_kms_plane_atomic_update(
    plane: *mut DrmPlane,
    state: *mut DrmAtomicCommit,
) {
    unsafe {
        let pipe = pipe_from_plane(plane);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.update) {
            callback(
                pipe,
                crate::drm_atomic_upstream::drm_atomic_get_old_plane_state(state, plane),
            );
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_prepare_fb()
unsafe extern "C" fn simple_kms_plane_prepare_fb(
    plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) -> i32 {
    unsafe {
        let pipe = pipe_from_plane(plane);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.prepare_fb) {
            return callback(pipe, state);
        }
        if warn_on((*(*plane).dev).driver_features & DRIVER_GEM == 0) {
            return 0;
        }
        warn_on(pipe_funcs(pipe).is_some_and(|funcs| funcs.cleanup_fb.is_some()));
        crate::drm_gem_atomic_helper_upstream::drm_gem_plane_helper_prepare_fb(plane, state)
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_cleanup_fb()
unsafe extern "C" fn simple_kms_plane_cleanup_fb(plane: *mut DrmPlane, state: *mut DrmPlaneState) {
    unsafe {
        let pipe = pipe_from_plane(plane);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.cleanup_fb) {
            callback(pipe, state);
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_begin_fb_access()
unsafe extern "C" fn simple_kms_plane_begin_fb_access(
    plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) -> i32 {
    unsafe {
        let pipe = pipe_from_plane(plane);
        pipe_funcs(pipe)
            .and_then(|funcs| funcs.begin_fb_access)
            .map_or(0, |callback| callback(pipe, state))
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_end_fb_access()
unsafe extern "C" fn simple_kms_plane_end_fb_access(
    plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) {
    unsafe {
        let pipe = pipe_from_plane(plane);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.end_fb_access) {
            callback(pipe, state);
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_format_mod_supported()
unsafe extern "C" fn simple_kms_format_mod_supported(
    _plane: *mut DrmPlane,
    _format: u32,
    modifier: u64,
) -> bool {
    modifier == DRM_FORMAT_MOD_LINEAR
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_reset()
unsafe extern "C" fn simple_kms_plane_reset(plane: *mut DrmPlane) {
    unsafe {
        let pipe = pipe_from_plane(plane);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.reset_plane) {
            callback(pipe);
        } else {
            crate::drm_atomic_state_helper_upstream::drm_atomic_helper_plane_reset(plane);
        }
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_duplicate_state()
unsafe extern "C" fn simple_kms_plane_duplicate_state(plane: *mut DrmPlane) -> *mut DrmPlaneState {
    unsafe {
        let pipe = pipe_from_plane(plane);
        pipe_funcs(pipe)
            .and_then(|funcs| funcs.duplicate_plane_state)
            .map_or_else(
                || crate::drm_atomic_state_helper_upstream::drm_atomic_helper_plane_duplicate_state(plane),
                |callback| callback(pipe),
            )
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_kms_plane_destroy_state()
unsafe extern "C" fn simple_kms_plane_destroy_state(
    plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) {
    unsafe {
        let pipe = pipe_from_plane(plane);
        if let Some(callback) = pipe_funcs(pipe).and_then(|funcs| funcs.destroy_plane_state) {
            callback(pipe, state);
        } else {
            crate::drm_atomic_state_helper_upstream::drm_atomic_helper_plane_destroy_state(
                plane, state,
            );
        }
    }
}

const DRIVER_GEM: u32 = 1 << 3;

// upstream: drm_simple_kms_helper.c drm_simple_encoder_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_simple_encoder_init(
    dev: *mut DrmDevice,
    encoder: *mut DrmEncoder,
    encoder_type: c_int,
) -> i32 {
    unsafe {
        crate::drm_encoder_upstream::drm_encoder_init(
            dev,
            encoder,
            &SIMPLE_ENCODER_FUNCS,
            encoder_type,
            core::ptr::null(),
        )
    }
}

// upstream: drm_simple_kms_helper.c __drmm_simple_encoder_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drmm_simple_encoder_alloc(
    dev: *mut DrmDevice,
    size: usize,
    offset: usize,
    encoder_type: c_int,
) -> *mut c_void {
    unsafe {
        crate::drm_encoder_upstream::__drmm_encoder_alloc(
            dev,
            size,
            offset,
            core::ptr::null(),
            encoder_type,
            core::ptr::null(),
        )
    }
}

// upstream: drm_simple_kms_helper.c drm_simple_display_pipe_attach_bridge()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_simple_display_pipe_attach_bridge(
    pipe: *mut DrmSimpleDisplayPipe,
    bridge: *mut c_void,
) -> i32 {
    unsafe extern "C" {
        fn drm_bridge_attach(
            encoder: *mut DrmEncoder,
            bridge: *mut c_void,
            previous: *mut c_void,
            flags: u32,
        ) -> i32;
    }
    unsafe { drm_bridge_attach(&mut (*pipe).encoder, bridge, core::ptr::null_mut(), 0) }
}

// upstream: drm_simple_kms_helper.c drm_simple_display_pipe_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_simple_display_pipe_init(
    dev: *mut DrmDevice,
    pipe: *mut DrmSimpleDisplayPipe,
    funcs: *const DrmSimpleDisplayPipeFuncs,
    formats: *const u32,
    format_count: u32,
    format_modifiers: *const u64,
    connector: *mut DrmConnector,
) -> i32 {
    unsafe {
        (*pipe).connector = connector;
        (*pipe).funcs = funcs;
        (*pipe).plane.helper_private =
            &SIMPLE_PLANE_HELPER_FUNCS as *const DrmPlaneHelperFuncs as *mut DrmPlaneHelperFuncs;
        let ret = crate::drm_plane_upstream::drm_universal_plane_init(
            dev,
            &mut (*pipe).plane,
            0,
            &SIMPLE_PLANE_FUNCS,
            formats,
            format_count,
            format_modifiers,
            DRM_PLANE_TYPE_PRIMARY,
            core::ptr::null(),
        );
        if ret != 0 {
            return ret;
        }

        (*pipe).crtc.helper_private =
            &SIMPLE_CRTC_HELPER_FUNCS as *const DrmCrtcHelperFuncs as *mut DrmCrtcHelperFuncs;
        let ret = crate::drm_crtc_upstream::drm_crtc_init_with_planes(
            dev,
            &mut (*pipe).crtc,
            &mut (*pipe).plane,
            core::ptr::null_mut(),
            &SIMPLE_CRTC_FUNCS,
            core::ptr::null(),
        );
        if ret != 0 {
            return ret;
        }

        (*pipe).encoder.possible_crtcs = crate::drm_crtc_upstream::drm_crtc_mask(&(*pipe).crtc);
        let ret = drm_simple_encoder_init(
            dev,
            &mut (*pipe).encoder,
            crate::uapi_drm_consts::DRM_MODE_ENCODER_NONE as i32,
        );
        if ret != 0 || connector.is_null() {
            return ret;
        }
        crate::drm_connector_upstream::drm_connector_attach_encoder(connector, &mut (*pipe).encoder)
    }
}

#[cfg(test)]
mod tests {
    use core::ptr;

    use super::*;

    #[test]
    fn simple_crtc_wires_timer_vblank_and_atomic_flip_events() {
        assert!(SIMPLE_CRTC_FUNCS.enable_vblank.is_some());
        assert!(SIMPLE_CRTC_FUNCS.disable_vblank.is_some());
        assert!(SIMPLE_CRTC_FUNCS.get_vblank_timestamp.is_some());
        assert!(SIMPLE_CRTC_HELPER_FUNCS.atomic_flush.is_some());
    }

    unsafe extern "C" fn custom_prepare_fb(
        pipe: *mut DrmSimpleDisplayPipe,
        state: *mut DrmPlaneState,
    ) -> i32 {
        assert!(!pipe.is_null());
        assert!(!state.is_null());
        37
    }

    #[test]
    fn simple_plane_prepare_dispatches_to_driver_callback() {
        let funcs = DrmSimpleDisplayPipeFuncs {
            mode_valid: None,
            enable: None,
            disable: None,
            check: None,
            update: None,
            prepare_fb: Some(custom_prepare_fb),
            cleanup_fb: None,
            begin_fb_access: None,
            end_fb_access: None,
            enable_vblank: None,
            disable_vblank: None,
            reset_crtc: None,
            duplicate_crtc_state: None,
            destroy_crtc_state: None,
            reset_plane: None,
            duplicate_plane_state: None,
            destroy_plane_state: None,
        };
        let mut pipe: DrmSimpleDisplayPipe = unsafe { core::mem::zeroed() };
        pipe.funcs = &funcs;
        let mut state: DrmPlaneState = unsafe { core::mem::zeroed() };
        assert_eq!(
            unsafe { simple_kms_plane_prepare_fb(&mut pipe.plane, &mut state) },
            37
        );
    }

    #[test]
    fn simple_plane_only_accepts_linear_modifiers() {
        assert!(unsafe {
            simple_kms_format_mod_supported(ptr::null_mut(), 0, DRM_FORMAT_MOD_LINEAR)
        });
        assert!(!unsafe { simple_kms_format_mod_supported(ptr::null_mut(), 0, 1) });
    }
}
