// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Linux 7.2.3 firmware state readout and ADLP/display-IP 13 sanitization.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_atomic_state_helper_upstream::__drm_atomic_helper_crtc_destroy_state,
    drm_atomic_types::DrmAtomicCommit,
    drm_atomic_uapi_upstream::drm_atomic_set_mode_for_crtc,
    drm_atomic_upstream::{
        drm_atomic_add_affected_connectors, drm_atomic_add_affected_planes, drm_atomic_commit,
        drm_atomic_commit_alloc, drm_atomic_commit_put, drm_atomic_get_crtc_state,
    },
    drm_connector_types::{DrmConnector, DrmConnectorState},
    drm_crtc_types::DrmCrtc,
    drm_encoder_types::DrmEncoder,
    drm_vblank_upstream::drm_crtc_vblank_reset,
    uapi_drm_consts::{DRM_MODE_DPMS_OFF, DRM_MODE_DPMS_ON},
};

use crate::{
    intel_atomic_upstream::intel_crtc_free_hw_state,
    intel_bw_upstream::intel_bw_update_hw_state,
    intel_cdclk_upstream::{intel_cdclk_update_hw_state, intel_crtc_min_cdclk},
    intel_cmtg_upstream::intel_cmtg_sanitize,
    intel_color_upstream::{
        intel_color_background_color_hw_to_drm, intel_color_commit_arm, intel_color_commit_noarm,
    },
    intel_connector_upstream::intel_connector_get_hw_state,
    intel_crtc_upstream::{intel_crtc_for_pipe, intel_crtc_state_reset, intel_crtc_vblank_on},
    intel_dbuf_bw_upstream::intel_dbuf_bw_update_hw_state,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_power_types_upstream::{IntelPowerDomainMask, POWER_DOMAIN_INIT},
    intel_display_power_upstream::{
        intel_display_power_get, intel_display_power_put, intel_display_power_sanitize_state,
    },
    intel_display_types_upstream::{
        IntelAtomicState, IntelConnector, IntelCrtc, IntelCrtcState, IntelEncoder, IntelPlane,
        IntelPlaneState,
    },
    intel_display_wa_upstream::DISPLAY_VER,
    intel_dmc_upstream::intel_dmc_enable_pipe,
    intel_dpll_mgr_upstream::{intel_dpll_readout_hw_state, intel_dpll_sanitize_state},
    intel_fifo_underrun_upstream::intel_init_fifo_underrun_reporting,
    intel_opregion_upstream::intel_opregion_notify_encoder,
    intel_pmdemand_upstream::{
        IntelPmdemandState, intel_pmdemand_init_pmdemand_params, intel_pmdemand_update_phys_mask,
        intel_pmdemand_update_port_clock,
    },
    intel_vblank_upstream::intel_crtc_update_active_timings,
    intel_wm_upstream::{intel_wm_get_hw_state, intel_wm_sanitize},
};

const ENODEV: i32 = tk_linux_kapi::config::ENODEV;
const ENOMEM: i32 = tk_linux_kapi::config::ENOMEM;

unsafe extern "C" {
    fn intel_set_plane_visible(
        crtc_state: *mut IntelCrtcState,
        plane_state: *mut IntelPlaneState,
        visible: bool,
    );
    fn intel_plane_fixup_bitmasks(crtc_state: *mut IntelCrtcState);
    fn intel_encoder_get_config(encoder: *mut IntelEncoder, crtc_state: *mut IntelCrtcState);
    fn intel_plane_disable_noatomic(crtc: *mut IntelCrtc, plane: *mut IntelPlane);
    fn intel_modeset_get_crtc_power_domains(
        crtc_state: *const IntelCrtcState,
        put_domains: *mut IntelPowerDomainMask,
    );
    fn intel_modeset_put_crtc_power_domains(
        crtc: *mut IntelCrtc,
        put_domains: *mut IntelPowerDomainMask,
    );
}

unsafe fn crtc_from_node(node: *mut tk_linux_kapi::types::ListHead) -> *mut IntelCrtc {
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(DrmCrtc, head))
            .cast::<IntelCrtc>()
    }
}

unsafe fn plane_from_node(node: *mut tk_linux_kapi::types::ListHead) -> *mut IntelPlane {
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(tk_drm_upstream::drm_plane_types::DrmPlane, head))
            .cast::<IntelPlane>()
    }
}

unsafe fn encoder_from_node(node: *mut tk_linux_kapi::types::ListHead) -> *mut IntelEncoder {
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(DrmEncoder, head))
            .cast::<IntelEncoder>()
    }
}

unsafe fn connector_from_node(node: *mut tk_linux_kapi::types::ListHead) -> *mut IntelConnector {
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(DrmConnector, head))
            .cast::<IntelConnector>()
    }
}

unsafe fn crtc_head(display: *mut IntelDisplay) -> *mut tk_linux_kapi::types::ListHead {
    unsafe { ptr::addr_of_mut!((*(*display).drm).mode_config.crtc_list) }
}

unsafe fn plane_head(display: *mut IntelDisplay) -> *mut tk_linux_kapi::types::ListHead {
    unsafe { ptr::addr_of_mut!((*(*display).drm).mode_config.plane_list) }
}

unsafe fn encoder_head(display: *mut IntelDisplay) -> *mut tk_linux_kapi::types::ListHead {
    unsafe { ptr::addr_of_mut!((*(*display).drm).mode_config.encoder_list) }
}

unsafe fn connector_head(display: *mut IntelDisplay) -> *mut tk_linux_kapi::types::ListHead {
    unsafe { ptr::addr_of_mut!((*(*display).drm).mode_config.connector_list) }
}

unsafe fn log_setup_error(display: *mut IntelDisplay, message: &'static str, error: i32) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    unsafe {
        tk_linux_kapi::print::drm_log_at(
            tk_linux_kapi::print::DrmLogLevel::Error,
            "i915",
            file!(),
            line!(),
            &alloc::format!("{message}: {error}"),
        )
    };
}

unsafe fn readout_planes(display: *mut IntelDisplay) {
    let head = unsafe { plane_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let plane = unsafe { plane_from_node(node) };
        let next = unsafe { (*node).next };
        let mut pipe = 0;
        let visible = if let Some(get_hw_state) = unsafe { (*plane).get_hw_state } {
            unsafe { get_hw_state(plane, &mut pipe) }
        } else {
            false
        };
        let crtc = unsafe { intel_crtc_for_pipe(display, pipe) };
        if !crtc.is_null() {
            let state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
            let plane_state = unsafe { (*plane).base.state.cast::<IntelPlaneState>() };
            if !state.is_null() && !plane_state.is_null() {
                unsafe { intel_set_plane_visible(state, plane_state, visible) };
            }
        }
        node = next;
    }

    let crtc_list = unsafe { crtc_head(display) };
    let mut node = unsafe { (*crtc_list).next };
    while !node.is_null() && node != crtc_list {
        let crtc = unsafe { crtc_from_node(node) };
        let next = unsafe { (*node).next };
        let state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
        if !state.is_null() {
            unsafe { intel_plane_fixup_bitmasks(state) };
        }
        node = next;
    }
}

unsafe fn readout_encoders(display: *mut IntelDisplay, pmdemand: *mut IntelPmdemandState) {
    let head = unsafe { encoder_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let encoder = unsafe { encoder_from_node(node) };
        let next = unsafe { (*node).next };
        let mut pipe = 0;
        let active = unsafe {
            (*encoder)
                .get_hw_state
                .is_some_and(|get_hw_state| get_hw_state(encoder, &mut pipe))
        };
        if active {
            let crtc = unsafe { intel_crtc_for_pipe(display, pipe) };
            if !crtc.is_null() {
                let crtc_state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
                unsafe { (*encoder).base.crtc = &mut (*crtc).base };
                if !crtc_state.is_null() {
                    unsafe { intel_encoder_get_config(encoder, crtc_state) };
                }
                unsafe { intel_pmdemand_update_phys_mask(display, encoder.cast(), pmdemand, true) };
            } else {
                unsafe {
                    (*encoder).base.crtc = ptr::null_mut();
                    intel_pmdemand_update_phys_mask(display, encoder.cast(), pmdemand, false);
                }
            }
        } else {
            unsafe {
                (*encoder).base.crtc = ptr::null_mut();
                intel_pmdemand_update_phys_mask(display, encoder.cast(), pmdemand, false);
            }
        }

        let crtc_state = if unsafe { (*encoder).base.crtc.is_null() } {
            ptr::null_mut()
        } else {
            unsafe { (*(*encoder).base.crtc).state.cast::<IntelCrtcState>() }
        };
        if let Some(sync_state) = unsafe { (*encoder).sync_state } {
            unsafe { sync_state(encoder, crtc_state) };
        }
        node = next;
    }
}

unsafe fn readout_connectors(display: *mut IntelDisplay) {
    let head = unsafe { connector_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let connector = unsafe { connector_from_node(node) };
        let next = unsafe { (*node).next };
        let active = unsafe { intel_connector_get_hw_state(connector) };
        let mut crtc_state: *mut IntelCrtcState = ptr::null_mut();
        if active {
            let encoder = unsafe { (*connector).encoder };
            if !encoder.is_null() {
                unsafe {
                    (*connector).base.dpms = DRM_MODE_DPMS_ON as i32;
                    (*connector).base.encoder = &mut (*encoder).base;
                }
                let crtc = unsafe { (*encoder).base.crtc };
                if !crtc.is_null() {
                    crtc_state = unsafe { (*crtc).state.cast::<IntelCrtcState>() };
                    if !crtc_state.is_null() && unsafe { (*crtc_state).hw.active } {
                        let connector_index = unsafe { (*connector).base.index };
                        let encoder_index = unsafe { (*encoder).base.index };
                        if connector_index < u32::BITS && encoder_index < u32::BITS {
                            unsafe {
                                (*crtc_state).uapi.connector_mask |= 1u32 << connector_index;
                                (*crtc_state).uapi.encoder_mask |= 1u32 << encoder_index;
                            }
                        }
                    }
                }
            } else {
                unsafe {
                    (*connector).base.dpms = DRM_MODE_DPMS_OFF as i32;
                    (*connector).base.encoder = ptr::null_mut();
                }
            }
        } else {
            unsafe {
                (*connector).base.dpms = DRM_MODE_DPMS_OFF as i32;
                (*connector).base.encoder = ptr::null_mut();
            }
        }
        if let Some(sync_state) = unsafe { (*connector).sync_state } {
            unsafe { sync_state(connector, crtc_state) };
        }
        node = next;
    }
}

unsafe fn get_encoder_power_domains(display: *mut IntelDisplay) {
    let head = unsafe { encoder_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let encoder = unsafe { encoder_from_node(node) };
        let next = unsafe { (*node).next };
        if let Some(get_power_domains) = unsafe { (*encoder).get_power_domains }
            && !unsafe { (*encoder).base.crtc.is_null() }
        {
            let state = unsafe { (*(*encoder).base.crtc).state.cast::<IntelCrtcState>() };
            if !state.is_null() {
                unsafe { get_power_domains(encoder, state) };
            }
        }
        node = next;
    }
}

unsafe fn sanitize_encoders(display: *mut IntelDisplay, pmdemand: *mut IntelPmdemandState) {
    let head = unsafe { encoder_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let encoder = unsafe { encoder_from_node(node) };
        let next = unsafe { (*node).next };
        let crtc = unsafe { (*encoder).base.crtc.cast::<IntelCrtc>() };
        let crtc_state = if crtc.is_null() {
            ptr::null_mut()
        } else {
            unsafe { (*crtc).base.state.cast::<IntelCrtcState>() }
        };
        let active_pipe = !crtc_state.is_null() && unsafe { (*crtc_state).hw.active };
        let mut connector: *mut IntelConnector = ptr::null_mut();
        let connectors = unsafe { connector_head(display) };
        let mut connector_node = unsafe { (*connectors).next };
        while !connector_node.is_null() && connector_node != connectors {
            let candidate = unsafe { connector_from_node(connector_node) };
            if unsafe { (*candidate).base.encoder == &mut (*encoder).base } {
                connector = candidate;
                break;
            }
            connector_node = unsafe { (*connector_node).next };
        }

        if !connector.is_null() && !active_pipe {
            unsafe { intel_pmdemand_update_phys_mask(display, encoder.cast(), pmdemand, false) };
            if !crtc_state.is_null() {
                let conn_state = unsafe { (*connector).base.state };
                if !conn_state.is_null() {
                    let saved = unsafe { (*conn_state).best_encoder };
                    unsafe { (*conn_state).best_encoder = &mut (*encoder).base };
                    if let Some(disable) = unsafe { (*encoder).disable } {
                        unsafe { disable(ptr::null_mut(), encoder, crtc_state, conn_state) };
                    }
                    if let Some(post_disable) = unsafe { (*encoder).post_disable } {
                        unsafe { post_disable(ptr::null_mut(), encoder, crtc_state, conn_state) };
                    }
                    unsafe { (*conn_state).best_encoder = saved };
                }
            }
            unsafe {
                (*encoder).base.crtc = ptr::null_mut();
                (*connector).base.dpms = DRM_MODE_DPMS_OFF as i32;
                (*connector).base.encoder = ptr::null_mut();
            }
        }
        unsafe { intel_opregion_notify_encoder(encoder, !connector.is_null() && active_pipe) };
        if unsafe { (*encoder).base.crtc.is_null() }
            && let Some(is_clock_enabled) = unsafe { (*encoder).is_clock_enabled }
            && unsafe { is_clock_enabled(encoder) }
            && let Some(disable_clock) = unsafe { (*encoder).disable_clock }
        {
            // ADL-P DDI ports must not retain an ungated clock after firmware
            // left the encoder disabled.
            unsafe { disable_clock(encoder) };
        }
        node = next;
    }
}

unsafe fn replace_blob(
    slot: *mut *mut tk_drm_upstream::drm_property_types::DrmPropertyBlob,
    blob: *mut tk_drm_upstream::drm_property_types::DrmPropertyBlob,
) {
    unsafe { tk_drm_upstream::drm_property_upstream::drm_property_replace_blob(slot, blob) };
}

unsafe fn copy_hw_to_uapi(state: *mut IntelCrtcState, display: *mut IntelDisplay) {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return;
    }
    unsafe {
        (*state).uapi.enable = (*state).hw.enable;
        (*state).uapi.active = (*state).hw.active;
        let ret = drm_atomic_set_mode_for_crtc(&mut (*state).uapi, &(*state).hw.mode);
        if ret != 0 {
            (*state).hw.active = false;
            (*state).hw.enable = false;
            (*state).uapi.active = false;
            (*state).uapi.enable = false;
            (*state).inherited = false;
            return;
        }
        ptr::copy_nonoverlapping(
            ptr::addr_of!((*state).hw.adjusted_mode),
            ptr::addr_of_mut!((*state).uapi.adjusted_mode),
            1,
        );
        (*state).uapi.scaling_filter = (*state).hw.scaling_filter;
        (*state).uapi.sharpness_strength = (*state).hw.sharpness_strength;
        (*state).uapi.background_color =
            intel_color_background_color_hw_to_drm((*state).hw.background_color);
        let device_info = if display.is_null() {
            ptr::null_mut()
        } else {
            (*display).info.__device_info
        };
        if !device_info.is_null() && (*device_info).color.degamma_lut_size != 0 {
            replace_blob(
                ptr::addr_of_mut!((*state).hw.degamma_lut),
                (*state).pre_csc_lut,
            );
            replace_blob(
                ptr::addr_of_mut!((*state).hw.gamma_lut),
                (*state).post_csc_lut,
            );
        } else {
            replace_blob(ptr::addr_of_mut!((*state).hw.degamma_lut), ptr::null_mut());
            replace_blob(
                ptr::addr_of_mut!((*state).hw.gamma_lut),
                if (*state).post_csc_lut.is_null() {
                    (*state).pre_csc_lut
                } else {
                    (*state).post_csc_lut
                },
            );
        }
        replace_blob(
            ptr::addr_of_mut!((*state).uapi.degamma_lut),
            (*state).hw.degamma_lut,
        );
        replace_blob(
            ptr::addr_of_mut!((*state).uapi.gamma_lut),
            (*state).hw.gamma_lut,
        );
        replace_blob(ptr::addr_of_mut!((*state).uapi.ctm), (*state).hw.ctm);
    }
}

unsafe fn readout_crtcs_and_account(display: *mut IntelDisplay, pmdemand: *mut IntelPmdemandState) {
    let head = unsafe { crtc_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let crtc = unsafe { crtc_from_node(node) };
        let next = unsafe { (*node).next };
        let state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
        if !state.is_null() {
            unsafe {
                __drm_atomic_helper_crtc_destroy_state(&mut (*state).uapi);
                intel_crtc_free_hw_state(state);
                intel_crtc_state_reset(state, crtc);
            }
            let funcs = unsafe { (*display).modeset.funcs };
            let readout = if funcs.is_null() {
                None
            } else {
                unsafe { (*funcs).get_pipe_config }
            };
            if let Some(readout) = readout {
                let _ = unsafe { readout(crtc, state) };
            }
            unsafe {
                (*state).hw.enable = (*state).hw.active;
                (*crtc).base.enabled = (*state).hw.enable;
                (*crtc).active = (*state).hw.active;
            }
        }
        node = next;
    }

    unsafe { readout_planes(display) };
    unsafe { readout_encoders(display, pmdemand) };
    unsafe { intel_dpll_readout_hw_state(display) };
    unsafe { readout_connectors(display) };

    node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let crtc = unsafe { crtc_from_node(node) };
        let next = unsafe { (*node).next };
        let state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
        if !state.is_null() {
            unsafe {
                (*state).inherited = true;
                if (*state).hw.active {
                    intel_crtc_update_active_timings(state, (*state).vrr.enable);
                    copy_hw_to_uapi(state, display);
                    if !(*state).hw.active {
                        let crtc = (*state).uapi.crtc.cast::<IntelCrtc>();
                        if !crtc.is_null() {
                            (*crtc).active = false;
                            (*crtc).base.enabled = false;
                        }
                    }
                }
            }
            let plane_head = unsafe { plane_head(display) };
            let mut plane_node = unsafe { (*plane_head).next };
            while !plane_node.is_null() && plane_node != plane_head {
                let plane = unsafe { plane_from_node(plane_node) };
                let plane_next = unsafe { (*plane_node).next };
                let plane_state = unsafe { (*plane).base.state.cast::<IntelPlaneState>() };
                let plane_index = unsafe { (*plane).base.index };
                if !plane_state.is_null()
                    && plane_index < u32::BITS
                    && unsafe { (*state).uapi.plane_mask & (1u32 << plane_index) != 0 }
                    && unsafe { (*plane_state).uapi.visible }
                {
                    let id = unsafe { (*plane).id as usize };
                    if id < unsafe { (*state).data_rate.len() } {
                        unsafe { (*state).data_rate[id] = (*state).pixel_rate.saturating_mul(4) };
                        if unsafe { (*plane).min_cdclk.is_some() } {
                            unsafe {
                                (*state).plane_min_cdclk[id] =
                                    (((*state).pixel_rate.saturating_add(1) / 2)
                                        .min(i32::MAX as u32))
                                        as i32;
                            }
                        }
                    }
                }
                plane_node = plane_next;
            }
            unsafe { (*state).min_cdclk = intel_crtc_min_cdclk(state) };
            unsafe {
                intel_pmdemand_update_port_clock(
                    display,
                    pmdemand,
                    (*crtc).pipe,
                    (*state).port_clock,
                )
            };
        }
        node = next;
    }

    // Linux reads the shared bandwidth, DBUF, and CDCLK state only after all
    // per-pipe timing and plane-derived rates have been populated.
    unsafe {
        if DISPLAY_VER(display) >= 9 {
            intel_wm_get_hw_state(display);
        }
        intel_bw_update_hw_state(display);
        intel_dbuf_bw_update_hw_state(display);
        intel_cdclk_update_hw_state(display);
        intel_pmdemand_init_pmdemand_params(display, pmdemand);
    }
}

unsafe fn update_connector_atomic_state(display: *mut IntelDisplay) {
    let head = unsafe { connector_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let connector = unsafe { connector_from_node(node) };
        let next = unsafe { (*node).next };
        let conn_state = unsafe { (*connector).base.state };
        if !conn_state.is_null() {
            if !unsafe { (*conn_state).crtc.is_null() } {
                unsafe {
                    tk_drm_upstream::drm_mode_object_upstream::drm_mode_object_put(
                        (&mut (*connector).base.base) as *mut _ as *mut _,
                    );
                }
            }
            let encoder = unsafe { (*connector).base.encoder.cast::<IntelEncoder>() };
            if !encoder.is_null() && !unsafe { (*encoder).base.crtc.is_null() } {
                let crtc = unsafe { (*encoder).base.crtc.cast::<IntelCrtc>() };
                let state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
                if !state.is_null() {
                    unsafe {
                        (*conn_state).best_encoder = &mut (*encoder).base;
                        (*conn_state).crtc = &mut (*crtc).base;
                        tk_drm_upstream::drm_mode_object_upstream::drm_mode_object_get(
                            (&mut (*connector).base.base) as *mut _ as *mut _,
                        );
                        let pipe_bpp = if (*state).pipe_bpp == 0 {
                            24
                        } else {
                            (*state).pipe_bpp
                        };
                        (*conn_state).max_bpc = (pipe_bpp.max(0) / 3) as u8;
                    }
                } else {
                    unsafe {
                        (*conn_state).best_encoder = ptr::null_mut();
                        (*conn_state).crtc = ptr::null_mut();
                    }
                }
            } else {
                unsafe {
                    (*conn_state).best_encoder = ptr::null_mut();
                    (*conn_state).crtc = ptr::null_mut();
                }
            }
        }
        node = next;
    }
}

unsafe fn encoder_for_crtc(display: *mut IntelDisplay, crtc: *mut IntelCrtc) -> bool {
    let head = unsafe { encoder_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let encoder = unsafe { encoder_from_node(node) };
        if unsafe { (*encoder).base.crtc == &mut (*crtc).base } {
            return true;
        }
        node = unsafe { (*node).next };
    }
    false
}

unsafe fn disable_unconnected_crtc(
    display: *mut IntelDisplay,
    crtc: *mut IntelCrtc,
    ctx: *mut tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx,
) -> i32 {
    let drm = unsafe { (*display).drm };
    let state = unsafe { drm_atomic_commit_alloc(drm) };
    if state.is_null() {
        return -ENOMEM;
    }
    unsafe {
        (*state).acquire_ctx = ctx;
        (*state.cast::<IntelAtomicState>()).internal = true;
    }
    let crtc_state = unsafe { drm_atomic_get_crtc_state(state, &mut (*crtc).base) };
    if crtc_state.is_null() || tk_linux_kapi::config::IS_ERR(crtc_state) {
        let error = if crtc_state.is_null() {
            -ENOMEM
        } else {
            tk_linux_kapi::config::PTR_ERR(crtc_state)
        };
        unsafe { drm_atomic_commit_put(state) };
        return error;
    }
    unsafe {
        (*crtc_state).enable = false;
        (*crtc_state).active = false;
    }
    let mut ret = unsafe { drm_atomic_add_affected_connectors(state, &mut (*crtc).base) };
    if ret == 0 {
        ret = unsafe { drm_atomic_add_affected_planes(state, &mut (*crtc).base) };
    }
    if ret == 0 {
        let funcs = unsafe { (*display).modeset.funcs };
        let disable = if funcs.is_null() {
            None
        } else {
            unsafe { (*funcs).crtc_disable }
        };
        if let Some(disable) = disable {
            unsafe { disable(state.cast(), crtc) };
            ret = unsafe { drm_atomic_commit(state) };
        } else {
            ret = -ENODEV;
        }
    }
    unsafe { drm_atomic_commit_put(state) };
    if ret == 0 {
        unsafe {
            (*crtc).active = false;
            (*crtc).base.enabled = false;
        }
    }
    ret
}

unsafe fn sanitize_crtcs(
    display: *mut IntelDisplay,
    ctx: *mut tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx,
) {
    let head = unsafe { crtc_head(display) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let crtc = unsafe { crtc_from_node(node) };
        let next = unsafe { (*node).next };
        let state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
        if !state.is_null() && unsafe { (*state).hw.active } {
            let planes = unsafe { plane_head(display) };
            let mut plane_node = unsafe { (*planes).next };
            while !plane_node.is_null() && plane_node != planes {
                let plane = unsafe { plane_from_node(plane_node) };
                let plane_next = unsafe { (*plane_node).next };
                let plane_state = unsafe { (*plane).base.state.cast::<IntelPlaneState>() };
                let plane_index = unsafe { (*plane).base.index };
                if !plane_state.is_null()
                    && plane_index < u32::BITS
                    && unsafe { (*state).uapi.plane_mask & (1u32 << plane_index) != 0 }
                    && unsafe { (*plane_state).uapi.visible }
                    && unsafe {
                        (*plane).base.r#type
                            != tk_drm_upstream::drm_plane_types::DRM_PLANE_TYPE_PRIMARY
                    }
                {
                    unsafe { intel_plane_disable_noatomic(crtc, plane) };
                }
                plane_node = plane_next;
            }
            unsafe {
                intel_color_commit_noarm(ptr::null_mut(), state);
                intel_color_commit_arm(ptr::null_mut(), state);
            }
            // A pipe without a live output is firmware residue. The CRTC
            // driver's vtable performs the real hardware disable atomically.
            if !unsafe { encoder_for_crtc(display, crtc) } {
                let ret = unsafe { disable_unconnected_crtc(display, crtc, ctx) };
                if ret != 0 {
                    unsafe { log_setup_error(display, "failed to disable orphan CRTC", ret) };
                }
            }
        }
        node = next;
    }
}

/// Read firmware state, inherit a complete UAPI state, and sanitize stray
/// planes/CRTCs before the initial atomic commit. The supplied modeset context
/// is already locked by the caller, as on both initial probe and resume.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_modeset_setup_hw_state(
    display: *mut IntelDisplay,
    ctx: *mut tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx,
) {
    if display.is_null()
        || unsafe { (*display).drm.is_null() }
        || !unsafe { intel_display_device_present(display) }
    {
        return;
    }
    let funcs = unsafe { (*display).modeset.funcs };
    if funcs.is_null() || unsafe { (*funcs).get_pipe_config.is_none() } {
        unsafe { log_setup_error(display, "missing CRTC hardware readout callback", -ENODEV) };
        return;
    }

    let wakeref = unsafe { intel_display_power_get(display, POWER_DOMAIN_INIT) };
    let pmdemand = unsafe { (*display).pmdemand.obj.state.cast::<IntelPmdemandState>() };
    unsafe { readout_crtcs_and_account(display, pmdemand) };

    // DISPLAY_VER 13 ADL-P has no legacy PCH path. Its CMTG and DDI/DPLL state
    // still need the same post-readout sanitization used by Linux.
    unsafe { get_encoder_power_domains(display) };
    unsafe { intel_cmtg_sanitize(display) };
    let crtc_list = unsafe { crtc_head(display) };
    let mut node = unsafe { (*crtc_list).next };
    while !node.is_null() && node != crtc_list {
        let crtc = unsafe { crtc_from_node(node) };
        let next = unsafe { (*node).next };
        let state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
        if !state.is_null() {
            unsafe {
                intel_init_fifo_underrun_reporting(display, crtc, !(*state).hw.active);
                drm_crtc_vblank_reset(&mut (*crtc).base);
                if (*state).hw.active {
                    intel_dmc_enable_pipe(state);
                    intel_crtc_vblank_on(state);
                }
            }
        }
        node = next;
    }
    unsafe {
        sanitize_encoders(display, pmdemand);
        update_connector_atomic_state(display);
        sanitize_crtcs(display, ctx);
        intel_dpll_sanitize_state(display);
        intel_wm_sanitize(display);
        let crtc_list = crtc_head(display);
        let mut node = (*crtc_list).next;
        while !node.is_null() && node != crtc_list {
            let crtc = crtc_from_node(node);
            let next = (*node).next;
            let state = (*crtc).base.state.cast::<IntelCrtcState>();
            if !state.is_null() {
                let mut put_domains = IntelPowerDomainMask { bits: [0; 2] };
                intel_modeset_get_crtc_power_domains(state, &mut put_domains);
                if put_domains.bits.iter().any(|bits| *bits != 0) {
                    intel_modeset_put_crtc_power_domains(crtc, &mut put_domains);
                }
            }
            node = next;
        }
        intel_display_power_put(display, POWER_DOMAIN_INIT, wakeref);
        intel_display_power_sanitize_state(display);
    }
}
