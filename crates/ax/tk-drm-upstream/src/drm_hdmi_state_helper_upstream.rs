// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/display/drm_hdmi_state_helper.c` (batch B2e).
//!
//! Implements HDMI connector state calculation, format and BPC selection,
//! infoframe generation (AVI, SPD, HDR, HDMI-VS), connector mode validation,
//! and infoframes update/clear routines.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::cmp::{max, min};
use core::ffi::{c_char, c_void};

use tk_linux_kapi::config::{EINVAL, EOPNOTSUPP, IS_ERR, PTR_ERR};
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_connector_types::{
    CONNECTOR_STATUS_CONNECTED, CONNECTOR_STATUS_DISCONNECTED,
    DrmConnector, DrmConnectorHdmiInfoframe, DrmConnectorInfoframeFuncs, DrmConnectorState,
    DrmConnectorStatus, DrmDisplayInfo, DrmHdmiBroadcastRgb, DrmOutputColorFormat,
    DRM_HDMI_BROADCAST_RGB_AUTO, DRM_HDMI_BROADCAST_RGB_FULL, DRM_HDMI_BROADCAST_RGB_LIMITED,
    DRM_OUTPUT_COLOR_FORMAT_RGB444, DRM_OUTPUT_COLOR_FORMAT_YCBCR420,
    DRM_OUTPUT_COLOR_FORMAT_YCBCR422, DRM_OUTPUT_COLOR_FORMAT_YCBCR444,
};
use crate::drm_connector_upstream::drm_hdmi_connector_get_output_format_name;
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState};
use crate::drm_edid_types::Edid;
use crate::drm_edid_upstream::{
    drm_default_rgb_quant_range, drm_edid_connector_update, drm_edid_free, drm_edid_read,
    drm_hdmi_avi_infoframe_from_display_mode, drm_hdmi_avi_infoframe_quant_range,
    drm_hdmi_vendor_infoframe_from_display_mode, drm_match_cea_mode, DrmEdid,
    DRM_EDID_HDMI_DC_30, DRM_EDID_HDMI_DC_36, DRM_EDID_YCBCR420_DC_30,
    DRM_EDID_YCBCR420_DC_36, DRM_EDID_YCBCR420_DC_48,
};
use crate::drm_hdmi_helper_upstream::{
    drm_hdmi_avi_infoframe_bars, drm_hdmi_avi_infoframe_colorimetry,
    drm_hdmi_compute_mode_clock, drm_hdmi_infoframe_set_hdr_metadata,
};
use crate::drm_modes_types::{
    DrmDisplayMode, DrmModeStatus, MODE_CLOCK_HIGH, MODE_ERROR, MODE_OK,
};
use crate::drm_modes_upstream::{drm_mode_is_420, drm_mode_is_420_only, drm_mode_vrefresh};
use crate::hdmi_types::*;
use crate::hdmi_upstream::*;

macro_rules! drm_dbg_kms {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {
        tk_linux_kapi::drm_dbg!($device, $format $(, $argument)*)
    };
}

macro_rules! drm_warn {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {
        tk_linux_kapi::drm_dbg!($device, $format $(, $argument)*)
    };
}

// External symbol from batch B2a pending stubs
unsafe extern "C" {
    fn drm_atomic_get_crtc_state(state: *mut DrmAtomicCommit, crtc: *mut DrmCrtc) -> *mut DrmCrtcState;
}

// Inlined atomic state getters (mirrors include/drm/drm_atomic.h)
#[inline]
unsafe fn drm_atomic_get_new_crtc_state_inline(
    state: *mut DrmAtomicCommit,
    crtc: *mut DrmCrtc,
) -> *mut DrmCrtcState {
    if state.is_null() || crtc.is_null() {
        return core::ptr::null_mut();
    }
    let idx = unsafe { (*crtc).index as usize };
    unsafe { (*(*state).crtcs.add(idx)).new_state }
}

#[inline]
unsafe fn drm_atomic_get_new_connector_state_inline(
    state: *mut DrmAtomicCommit,
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if state.is_null() || connector.is_null() {
        return core::ptr::null_mut();
    }
    let idx = unsafe { (*connector).index as i32 };
    if idx >= unsafe { (*state).num_connector } {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).connectors.add(idx as usize)).new_state }
}

#[inline]
unsafe fn drm_atomic_get_old_connector_state_inline(
    state: *mut DrmAtomicCommit,
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if state.is_null() || connector.is_null() {
        return core::ptr::null_mut();
    }
    let idx = unsafe { (*connector).index as i32 };
    if idx >= unsafe { (*state).num_connector } {
        return core::ptr::null_mut();
    }
    unsafe { (*(*state).connectors.add(idx as usize)).old_state }
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c __drm_atomic_helper_connector_hdmi_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_atomic_helper_connector_hdmi_reset(
    connector: *mut DrmConnector,
    new_conn_state: *mut DrmConnectorState,
) {
    if connector.is_null() || new_conn_state.is_null() {
        return;
    }
    let max_bpc = unsafe { (*connector).max_bpc as u8 };
    let state_ref = unsafe { &mut *new_conn_state };
    state_ref.max_bpc = max_bpc;
    state_ref.max_requested_bpc = max_bpc;
    state_ref.hdmi.broadcast_rgb = DRM_HDMI_BROADCAST_RGB_AUTO;
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c output_color_format_to_hdmi_colorspace()
fn output_color_format_to_hdmi_colorspace(
    connector: &DrmConnector,
    fmt: DrmOutputColorFormat,
) -> HdmiColorspace {
    match fmt {
        DRM_OUTPUT_COLOR_FORMAT_YCBCR420 => HDMI_COLORSPACE_YUV420,
        DRM_OUTPUT_COLOR_FORMAT_YCBCR422 => HDMI_COLORSPACE_YUV422,
        DRM_OUTPUT_COLOR_FORMAT_YCBCR444 => HDMI_COLORSPACE_YUV444,
        _ => {
            if fmt != DRM_OUTPUT_COLOR_FORMAT_RGB444 {
                unsafe {
                    drm_warn!(
                        connector.dev as *const c_void,
                        "Unsupported output color format. Defaulting to RGB."
                    );
                }
            }
            HDMI_COLORSPACE_RGB
        }
    }
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c connector_state_get_mode()
unsafe fn connector_state_get_mode(
    conn_state: *const DrmConnectorState,
) -> *const DrmDisplayMode {
    if conn_state.is_null() {
        return core::ptr::null();
    }
    let state = unsafe { (*conn_state).state };
    if state.is_null() {
        return core::ptr::null();
    }
    let crtc = unsafe { (*conn_state).crtc };
    if crtc.is_null() {
        return core::ptr::null();
    }
    let crtc_state = unsafe { drm_atomic_get_new_crtc_state_inline(state, crtc) };
    if crtc_state.is_null() {
        return core::ptr::null();
    }
    unsafe { core::ptr::addr_of!((*crtc_state).mode) }
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_is_limited_range()
unsafe fn hdmi_is_limited_range(
    connector: *const DrmConnector,
    conn_state: *const DrmConnectorState,
) -> bool {
    let conn_state_ref = unsafe { &*conn_state };
    let info = unsafe { &(*connector).display_info };
    let mode = unsafe { connector_state_get_mode(conn_state) };

    if conn_state_ref.hdmi.output_format != DRM_OUTPUT_COLOR_FORMAT_RGB444 {
        return true;
    }

    if conn_state_ref.hdmi.broadcast_rgb == DRM_HDMI_BROADCAST_RGB_FULL {
        return false;
    }

    if conn_state_ref.hdmi.broadcast_rgb == DRM_HDMI_BROADCAST_RGB_LIMITED {
        return true;
    }

    if !info.is_hdmi {
        return false;
    }

    unsafe { drm_default_rgb_quant_range(mode) == HDMI_QUANTIZATION_RANGE_LIMITED }
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c sink_supports_format_bpc()
unsafe fn sink_supports_format_bpc(
    connector: *const DrmConnector,
    info: *const DrmDisplayInfo,
    mode: *const DrmDisplayMode,
    format: DrmOutputColorFormat,
    bpc: u32,
) -> bool {
    let dev = unsafe { (*connector).dev as *const c_void };
    let vic = unsafe { drm_match_cea_mode(mode) };

    if vic == 1 && bpc != 8 {
        unsafe { drm_dbg_kms!(dev, "VIC1 requires a bpc of 8, got {}\n", bpc) };
        return false;
    }

    if unsafe { !(*info).is_hdmi }
        && (format != DRM_OUTPUT_COLOR_FORMAT_RGB444 || bpc != 8)
    {
        unsafe { drm_dbg_kms!(dev, "DVI Monitors require an RGB output at 8 bpc\n") };
        return false;
    }

    let supported_formats = unsafe { (*connector).hdmi.supported_formats };
    if (supported_formats & (1 << format)) == 0 {
        unsafe {
            let fmt_name = drm_hdmi_connector_get_output_format_name(format);
            let s = core::ffi::CStr::from_ptr(fmt_name).to_str().unwrap_or("unknown");
            drm_dbg_kms!(dev, "{} format unsupported by the connector.\n", s);
        }
        return false;
    }

    if unsafe { drm_mode_is_420_only(info, mode) }
        && format != DRM_OUTPUT_COLOR_FORMAT_YCBCR420
    {
        unsafe { drm_dbg_kms!(dev, "Mode can be only supported in YUV420 format.\n") };
        return false;
    }

    match format {
        DRM_OUTPUT_COLOR_FORMAT_RGB444 => {
            unsafe { drm_dbg_kms!(dev, "RGB Format, checking the constraints.\n") };

            if (unsafe { (*info).color_formats } & (1 << DRM_OUTPUT_COLOR_FORMAT_RGB444)) == 0 {
                unsafe { drm_warn!(dev, "HDMI Sink doesn't support RGB, something's wrong.\n") };
            }

            if bpc == 10 && (unsafe { (*info).edid_hdmi_rgb444_dc_modes } & DRM_EDID_HDMI_DC_30) == 0 {
                unsafe { drm_dbg_kms!(dev, "10 BPC but sink doesn't support Deep Color 30.\n") };
                return false;
            }

            if bpc == 12 && (unsafe { (*info).edid_hdmi_rgb444_dc_modes } & DRM_EDID_HDMI_DC_36) == 0 {
                unsafe { drm_dbg_kms!(dev, "12 BPC but sink doesn't support Deep Color 36.\n") };
                return false;
            }

            unsafe { drm_dbg_kms!(dev, "RGB format supported in that configuration.\n") };
            true
        }
        DRM_OUTPUT_COLOR_FORMAT_YCBCR420 => {
            unsafe { drm_dbg_kms!(dev, "YUV420 format, checking the constraints.\n") };

            if (unsafe { (*info).color_formats } & (1 << DRM_OUTPUT_COLOR_FORMAT_YCBCR420)) == 0 {
                unsafe { drm_dbg_kms!(dev, "Sink doesn't support YUV420.\n") };
                return false;
            }

            if unsafe { !drm_mode_is_420(info, mode) } {
                unsafe { drm_dbg_kms!(dev, "Mode cannot be supported in YUV420 format.\n") };
                return false;
            }

            let y420_dc = unsafe { (*info).hdmi.y420_dc_modes };
            if bpc == 10 && (y420_dc & DRM_EDID_YCBCR420_DC_30) == 0 {
                unsafe { drm_dbg_kms!(dev, "10 BPC but sink doesn't support Deep Color 30.\n") };
                return false;
            }

            if bpc == 12 && (y420_dc & DRM_EDID_YCBCR420_DC_36) == 0 {
                unsafe { drm_dbg_kms!(dev, "12 BPC but sink doesn't support Deep Color 36.\n") };
                return false;
            }

            if bpc == 16 && (y420_dc & DRM_EDID_YCBCR420_DC_48) == 0 {
                unsafe { drm_dbg_kms!(dev, "16 BPC but sink doesn't support Deep Color 48.\n") };
                return false;
            }

            unsafe { drm_dbg_kms!(dev, "YUV420 format supported in that configuration.\n") };
            true
        }
        DRM_OUTPUT_COLOR_FORMAT_YCBCR422 => {
            unsafe { drm_dbg_kms!(dev, "YUV422 format, checking the constraints.\n") };

            if (unsafe { (*info).color_formats } & (1 << DRM_OUTPUT_COLOR_FORMAT_YCBCR422)) == 0 {
                unsafe { drm_dbg_kms!(dev, "Sink doesn't support YUV422.\n") };
                return false;
            }

            if bpc > 12 {
                unsafe { drm_dbg_kms!(dev, "YUV422 only supports 12 bpc or lower.\n") };
                return false;
            }

            unsafe { drm_dbg_kms!(dev, "YUV422 format supported in that configuration.\n") };
            true
        }
        DRM_OUTPUT_COLOR_FORMAT_YCBCR444 => {
            unsafe { drm_dbg_kms!(dev, "YUV444 format, checking the constraints.\n") };

            if (unsafe { (*info).color_formats } & (1 << DRM_OUTPUT_COLOR_FORMAT_YCBCR444)) == 0 {
                unsafe { drm_dbg_kms!(dev, "Sink doesn't support YUV444.\n") };
                return false;
            }

            if bpc == 10 && (unsafe { (*info).edid_hdmi_ycbcr444_dc_modes } & DRM_EDID_HDMI_DC_30) == 0 {
                unsafe { drm_dbg_kms!(dev, "10 BPC but sink doesn't support Deep Color 30.\n") };
                return false;
            }

            if bpc == 12 && (unsafe { (*info).edid_hdmi_ycbcr444_dc_modes } & DRM_EDID_HDMI_DC_36) == 0 {
                unsafe { drm_dbg_kms!(dev, "12 BPC but sink doesn't support Deep Color 36.\n") };
                return false;
            }

            unsafe { drm_dbg_kms!(dev, "YUV444 format supported in that configuration.\n") };
            true
        }
        _ => {
            unsafe { drm_dbg_kms!(dev, "Unsupported pixel format.\n") };
            false
        }
    }
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_clock_valid()
unsafe fn hdmi_clock_valid(
    connector: *const DrmConnector,
    mode: *const DrmDisplayMode,
    clock: u64,
) -> DrmModeStatus {
    let funcs = unsafe { (*connector).hdmi.funcs };
    let info = unsafe { &(*connector).display_info };

    if info.max_tmds_clock != 0 && clock > (info.max_tmds_clock as u64) * 1000 {
        return MODE_CLOCK_HIGH;
    }

    if !funcs.is_null() {
        if let Some(tmds_valid) = unsafe { (*funcs).tmds_char_rate_valid } {
            let status = unsafe {
                tmds_valid(
                    connector as *mut DrmConnector,
                    mode as *mut DrmDisplayMode,
                    clock,
                )
            };
            if status != MODE_OK {
                return status;
            }
        }
    }

    MODE_OK
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_compute_clock()
unsafe fn hdmi_compute_clock(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
    mode: *const DrmDisplayMode,
    bpc: u32,
    fmt: DrmOutputColorFormat,
) -> i32 {
    let clock = unsafe { drm_hdmi_compute_mode_clock(mode, bpc, fmt) };
    if clock == 0 {
        return -EINVAL;
    }

    let status = unsafe { hdmi_clock_valid(connector, mode, clock) };
    if status != MODE_OK {
        return -EINVAL;
    }

    unsafe { (*conn_state).hdmi.tmds_char_rate = clock };
    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_try_format_bpc()
unsafe fn hdmi_try_format_bpc(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
    mode: *const DrmDisplayMode,
    bpc: u32,
    fmt: DrmOutputColorFormat,
) -> bool {
    let info = unsafe { &(*connector).display_info };
    let dev = unsafe { (*connector).dev as *const c_void };

    unsafe {
        let fmt_name = drm_hdmi_connector_get_output_format_name(fmt);
        let s = core::ffi::CStr::from_ptr(fmt_name).to_str().unwrap_or("unknown");
        drm_dbg_kms!(dev, "Trying {} output format with {} bpc\n", s, bpc);
    }

    if unsafe { !sink_supports_format_bpc(connector, info, mode, fmt, bpc) } {
        unsafe {
            let fmt_name = drm_hdmi_connector_get_output_format_name(fmt);
            let s = core::ffi::CStr::from_ptr(fmt_name).to_str().unwrap_or("unknown");
            drm_dbg_kms!(dev, "{} output format not supported with {} bpc\n", s, bpc);
        }
        return false;
    }

    let ret = unsafe { hdmi_compute_clock(connector, conn_state, mode, bpc, fmt) };
    if ret != 0 {
        unsafe {
            let fmt_name = drm_hdmi_connector_get_output_format_name(fmt);
            let s = core::ffi::CStr::from_ptr(fmt_name).to_str().unwrap_or("unknown");
            drm_dbg_kms!(dev, "Couldn't compute clock for {} output format and {} bpc\n", s, bpc);
        }
        return false;
    }

    unsafe {
        let fmt_name = drm_hdmi_connector_get_output_format_name(fmt);
        let s = core::ffi::CStr::from_ptr(fmt_name).to_str().unwrap_or("unknown");
        drm_dbg_kms!(
            dev,
            "{} output format supported with {} bpc (TMDS char rate: {} Hz)\n",
            s,
            bpc,
            (*conn_state).hdmi.tmds_char_rate
        );
    }

    true
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_compute_format_bpc()
unsafe fn hdmi_compute_format_bpc(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
    mode: *const DrmDisplayMode,
    max_bpc: u32,
    fmt: DrmOutputColorFormat,
) -> i32 {
    let dev = unsafe { (*connector).dev as *const c_void };
    let mut bpc = max_bpc;

    while bpc >= 8 {
        let ret = unsafe { hdmi_try_format_bpc(connector, conn_state, mode, bpc, fmt) };
        if ret {
            unsafe {
                (*conn_state).hdmi.output_bpc = bpc;
                (*conn_state).hdmi.output_format = fmt;

                let fmt_name = drm_hdmi_connector_get_output_format_name((*conn_state).hdmi.output_format);
                let s = core::ffi::CStr::from_ptr(fmt_name).to_str().unwrap_or("unknown");
                drm_dbg_kms!(
                    dev,
                    "Mode {}x{} @ {}Hz: Found configuration: bpc: {}, fmt: {}, clock: {}\n",
                    (*mode).hdisplay,
                    (*mode).vdisplay,
                    drm_mode_vrefresh(mode),
                    (*conn_state).hdmi.output_bpc,
                    s,
                    (*conn_state).hdmi.tmds_char_rate
                );
            }
            return 0;
        }
        if bpc < 2 {
            break;
        }
        bpc -= 2;
    }

    unsafe {
        let fmt_name = drm_hdmi_connector_get_output_format_name(fmt);
        let s = core::ffi::CStr::from_ptr(fmt_name).to_str().unwrap_or("unknown");
        drm_dbg_kms!(dev, "Failed. {} output format not supported for any bpc count.\n", s);
    }

    -EINVAL
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_compute_config()
unsafe fn hdmi_compute_config(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
    mode: *const DrmDisplayMode,
) -> i32 {
    let raw_max = unsafe { (*conn_state).max_bpc as u32 };
    let conn_max = unsafe { (*connector).max_bpc as u32 };
    let max_bpc = min(max(raw_max, 8), conn_max);

    let mut ret = unsafe {
        hdmi_compute_format_bpc(
            connector,
            conn_state,
            mode,
            max_bpc,
            DRM_OUTPUT_COLOR_FORMAT_RGB444,
        )
    };

    if ret != 0 {
        if unsafe { (*connector).ycbcr_420_allowed } {
            ret = unsafe {
                hdmi_compute_format_bpc(
                    connector,
                    conn_state,
                    mode,
                    max_bpc,
                    DRM_OUTPUT_COLOR_FORMAT_YCBCR420,
                )
            };
            if ret != 0 {
                unsafe { drm_dbg_kms!((*connector).dev as *const c_void, "YUV420 output format doesn't work.\n") };
            }
        } else {
            unsafe { drm_dbg_kms!((*connector).dev as *const c_void, "YUV420 output format not allowed for connector.\n") };
            ret = -EINVAL;
        }
    }

    ret
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_generate_avi_infoframe()
unsafe fn hdmi_generate_avi_infoframe(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    let mode = unsafe { connector_state_get_mode(conn_state) };
    let infoframe = unsafe { core::ptr::addr_of_mut!((*conn_state).hdmi.infoframes.avi) };
    let frame = unsafe { core::ptr::addr_of_mut!((*infoframe).data.avi) as *mut HdmiAviInfoframe };
    let is_lim_range = unsafe { (*conn_state).hdmi.is_limited_range };
    let rgb_quant_range = if is_lim_range {
        HDMI_QUANTIZATION_RANGE_LIMITED
    } else {
        HDMI_QUANTIZATION_RANGE_FULL
    };

    unsafe { (*infoframe).set = false };

    let ret = unsafe {
        drm_hdmi_avi_infoframe_from_display_mode(
            frame,
            connector as *mut DrmConnector,
            mode,
        )
    };
    if ret != 0 {
        return ret;
    }

    unsafe {
        (*frame).colorspace = output_color_format_to_hdmi_colorspace(
            &*connector,
            (*conn_state).hdmi.output_format,
        );

        drm_hdmi_avi_infoframe_quant_range(frame, connector as *mut DrmConnector, mode, rgb_quant_range);
        drm_hdmi_avi_infoframe_colorimetry(frame, conn_state);
        drm_hdmi_avi_infoframe_bars(frame, conn_state);

        (*infoframe).set = true;
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_generate_spd_infoframe()
unsafe fn hdmi_generate_spd_infoframe(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    let infoframe = unsafe { core::ptr::addr_of_mut!((*conn_state).hdmi.infoframes.spd) };
    let frame = unsafe { core::ptr::addr_of_mut!((*infoframe).data.spd) as *mut HdmiSpdInfoframe };

    unsafe { (*infoframe).set = false };

    let funcs = unsafe { (*connector).hdmi.funcs };
    if funcs.is_null() || unsafe { (*funcs).spd.write_infoframe.is_none() } {
        return 0;
    }

    let vendor_ptr = unsafe { (*connector).hdmi.vendor.as_ptr() as *const c_char };
    let prod_ptr = unsafe { (*connector).hdmi.product.as_ptr() as *const c_char };
    let ret = unsafe { hdmi_spd_infoframe_init(frame, vendor_ptr, prod_ptr) };
    if ret != 0 {
        return ret;
    }

    unsafe {
        (*frame).sdi = HDMI_SPD_SDI_PC;
        (*infoframe).set = true;
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_generate_hdr_infoframe()
unsafe fn hdmi_generate_hdr_infoframe(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    let infoframe = unsafe { core::ptr::addr_of_mut!((*conn_state).hdmi.infoframes.hdr_drm) };
    let frame = unsafe { core::ptr::addr_of_mut!((*infoframe).data.drm) as *mut HdmiDrmInfoframe };

    unsafe { (*infoframe).set = false };

    let funcs = unsafe { (*connector).hdmi.funcs };
    if funcs.is_null() || unsafe { (*funcs).hdr_drm.write_infoframe.is_none() } {
        return 0;
    }

    if unsafe { (*connector).max_bpc } < 10 {
        return 0;
    }

    if unsafe { (*conn_state).hdr_output_metadata.is_null() } {
        return 0;
    }

    let ret = unsafe { drm_hdmi_infoframe_set_hdr_metadata(frame, conn_state) };
    if ret != 0 {
        return ret;
    }

    unsafe { (*infoframe).set = true };
    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_generate_hdmi_vendor_infoframe()
unsafe fn hdmi_generate_hdmi_vendor_infoframe(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    let info = unsafe { &(*connector).display_info };
    let mode = unsafe { connector_state_get_mode(conn_state) };
    let infoframe = unsafe { core::ptr::addr_of_mut!((*conn_state).hdmi.infoframes.hdmi) };
    let frame = unsafe { core::ptr::addr_of_mut!((*infoframe).data) as *mut HdmiVendorInfoframe };

    unsafe { (*infoframe).set = false };

    if !info.has_hdmi_infoframe {
        return 0;
    }

    let ret = unsafe {
        drm_hdmi_vendor_infoframe_from_display_mode(
            frame,
            connector as *mut DrmConnector,
            mode,
        )
    };
    if ret != 0 {
        return ret;
    }

    unsafe { (*infoframe).set = true };
    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c hdmi_generate_infoframes()
unsafe fn hdmi_generate_infoframes(
    connector: *const DrmConnector,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    let info = unsafe { &(*connector).display_info };
    if !info.is_hdmi {
        return 0;
    }

    let mut ret = unsafe { hdmi_generate_avi_infoframe(connector, conn_state) };
    if ret != 0 {
        return ret;
    }

    ret = unsafe { hdmi_generate_spd_infoframe(connector, conn_state) };
    if ret != 0 {
        return ret;
    }

    ret = unsafe { hdmi_generate_hdr_infoframe(connector, conn_state) };
    if ret != 0 {
        return ret;
    }

    ret = unsafe { hdmi_generate_hdmi_vendor_infoframe(connector, conn_state) };
    if ret != 0 {
        return ret;
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_atomic_helper_connector_hdmi_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_hdmi_check(
    connector: *mut DrmConnector,
    state: *mut DrmAtomicCommit,
) -> i32 {
    if connector.is_null() || state.is_null() {
        return -EINVAL;
    }

    let old_conn_state = unsafe { drm_atomic_get_old_connector_state_inline(state, connector) };
    let new_conn_state = unsafe { drm_atomic_get_new_connector_state_inline(state, connector) };
    if old_conn_state.is_null() || new_conn_state.is_null() {
        return 0;
    }

    if unsafe { (*new_conn_state).crtc.is_null() || (*new_conn_state).best_encoder.is_null() } {
        return 0;
    }

    let mode = unsafe { connector_state_get_mode(new_conn_state) };
    let mut ret = unsafe { hdmi_compute_config(connector, new_conn_state, mode) };
    if ret != 0 {
        return ret;
    }

    unsafe {
        (*new_conn_state).hdmi.is_limited_range = hdmi_is_limited_range(connector, new_conn_state);
    }

    ret = unsafe { hdmi_generate_infoframes(connector, new_conn_state) };
    if ret != 0 {
        return ret;
    }

    let old_broadcast = unsafe { (*old_conn_state).hdmi.broadcast_rgb };
    let new_broadcast = unsafe { (*new_conn_state).hdmi.broadcast_rgb };
    let old_bpc = unsafe { (*old_conn_state).hdmi.output_bpc };
    let new_bpc = unsafe { (*new_conn_state).hdmi.output_bpc };
    let old_fmt = unsafe { (*old_conn_state).hdmi.output_format };
    let new_fmt = unsafe { (*new_conn_state).hdmi.output_format };

    if old_broadcast != new_broadcast || old_bpc != new_bpc || old_fmt != new_fmt {
        let crtc = unsafe { (*new_conn_state).crtc };
        let crtc_state = unsafe { drm_atomic_get_crtc_state(state, crtc) };
        if IS_ERR(crtc_state as *const c_void) {
            return PTR_ERR(crtc_state as *const c_void);
        }

        // crtc_state->mode_changed = true; (bit 1 in _bitfields_10)
        unsafe { (*crtc_state)._bitfields_10[0] |= 1 << 1 };
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_hdmi_connector_mode_valid()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_hdmi_connector_mode_valid(
    connector: *mut DrmConnector,
    mode: *const DrmDisplayMode,
) -> DrmModeStatus {
    if connector.is_null() || mode.is_null() {
        return MODE_ERROR;
    }

    let clock = unsafe { drm_hdmi_compute_mode_clock(mode, 8, DRM_OUTPUT_COLOR_FORMAT_RGB444) };
    if clock == 0 {
        return MODE_ERROR;
    }

    unsafe { hdmi_clock_valid(connector, mode, clock) }
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c clear_infoframe()
unsafe fn clear_infoframe(
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorInfoframeFuncs,
    info_type: &str,
) -> i32 {
    let dev = unsafe { (*connector).dev as *const c_void };
    unsafe { drm_dbg_kms!(dev, "Clearing {} InfoFrame\n", info_type) };

    if funcs.is_null() {
        return -EOPNOTSUPP;
    }
    let clear_fn = unsafe { (*funcs).clear_infoframe };
    let Some(clear_fn) = clear_fn else {
        unsafe { drm_dbg_kms!(dev, "Function not implemented, bailing.\n") };
        return -EOPNOTSUPP;
    };

    let ret = unsafe { clear_fn(connector) };
    if ret != 0 {
        unsafe { drm_dbg_kms!(dev, "Call failed: {}\n", ret) };
        return ret;
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c write_infoframe()
unsafe fn write_infoframe(
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorInfoframeFuncs,
    info_type: &str,
    new_frame: *mut DrmConnectorHdmiInfoframe,
) -> i32 {
    let dev = unsafe { (*connector).dev as *const c_void };
    let mut buffer = [0u8; HDMI_INFOFRAME_HEADER_SIZE + HDMI_MAX_INFOFRAME_SIZE];

    unsafe { drm_dbg_kms!(dev, "Writing {} InfoFrame\n", info_type) };

    if funcs.is_null() {
        return -EOPNOTSUPP;
    }
    let write_fn = unsafe { (*funcs).write_infoframe };
    let Some(write_fn) = write_fn else {
        unsafe { drm_dbg_kms!(dev, "Function not implemented, bailing.\n") };
        return -EOPNOTSUPP;
    };

    let len = unsafe {
        hdmi_infoframe_pack(
            core::ptr::addr_of_mut!((*new_frame).data),
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len(),
        )
    };
    if len < 0 {
        return len as i32;
    }

    let ret = unsafe { write_fn(connector, buffer.as_mut_ptr(), len as usize) };
    if ret != 0 {
        unsafe { drm_dbg_kms!(dev, "Call failed: {}\n", ret) };
        return ret;
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c write_or_clear_infoframe()
unsafe fn write_or_clear_infoframe(
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorInfoframeFuncs,
    info_type: &str,
    old_frame: *mut DrmConnectorHdmiInfoframe,
    new_frame: *mut DrmConnectorHdmiInfoframe,
) -> i32 {
    if unsafe { (*new_frame).set } {
        return unsafe { write_infoframe(connector, funcs, info_type, new_frame) };
    }

    if unsafe { (*old_frame).set && !(*new_frame).set } {
        return unsafe { clear_infoframe(connector, funcs, info_type) };
    }

    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_atomic_helper_connector_hdmi_update_infoframes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_hdmi_update_infoframes(
    connector: *mut DrmConnector,
    state: *mut DrmAtomicCommit,
) -> i32 {
    if connector.is_null() || state.is_null() {
        return -EINVAL;
    }

    let funcs = unsafe { (*connector).hdmi.funcs };
    let old_conn_state = unsafe { drm_atomic_get_old_connector_state_inline(state, connector) };
    let new_conn_state = unsafe { drm_atomic_get_new_connector_state_inline(state, connector) };
    let info = unsafe { &(*connector).display_info };

    if !info.is_hdmi {
        return 0;
    }

    if funcs.is_null() {
        unsafe { drm_dbg_kms!((*connector).dev as *const c_void, "Function not implemented, bailing.\n") };
        return -EINVAL;
    }

    let lock_ptr = unsafe { core::ptr::addr_of_mut!((*connector).hdmi.infoframes.lock) };
    unsafe { mutex_lock(lock_ptr) };

    let mut ret = 0;
    if !old_conn_state.is_null() && !new_conn_state.is_null() {
        ret = unsafe {
            write_or_clear_infoframe(
                connector,
                core::ptr::addr_of!((*funcs).avi),
                "AVI",
                core::ptr::addr_of_mut!((*old_conn_state).hdmi.infoframes.avi),
                core::ptr::addr_of_mut!((*new_conn_state).hdmi.infoframes.avi),
            )
        };
        if ret != 0 {
            unsafe { mutex_unlock(lock_ptr) };
            return ret;
        }

        if unsafe { (*connector).hdmi.infoframes.audio.set } {
            ret = unsafe {
                write_infoframe(
                    connector,
                    core::ptr::addr_of!((*funcs).audio),
                    "Audio",
                    core::ptr::addr_of_mut!((*connector).hdmi.infoframes.audio),
                )
            };
            if ret != 0 {
                unsafe { mutex_unlock(lock_ptr) };
                return ret;
            }
        }

        ret = unsafe {
            write_or_clear_infoframe(
                connector,
                core::ptr::addr_of!((*funcs).hdr_drm),
                "HDR DRM",
                core::ptr::addr_of_mut!((*old_conn_state).hdmi.infoframes.hdr_drm),
                core::ptr::addr_of_mut!((*new_conn_state).hdmi.infoframes.hdr_drm),
            )
        };
        if ret != 0 {
            unsafe { mutex_unlock(lock_ptr) };
            return ret;
        }

        ret = unsafe {
            write_or_clear_infoframe(
                connector,
                core::ptr::addr_of!((*funcs).spd),
                "SPD",
                core::ptr::addr_of_mut!((*old_conn_state).hdmi.infoframes.spd),
                core::ptr::addr_of_mut!((*new_conn_state).hdmi.infoframes.spd),
            )
        };
        if ret != 0 {
            unsafe { mutex_unlock(lock_ptr) };
            return ret;
        }

        if info.has_hdmi_infoframe {
            ret = unsafe {
                write_or_clear_infoframe(
                    connector,
                    core::ptr::addr_of!((*funcs).hdmi),
                    "HDMI-VS",
                    core::ptr::addr_of_mut!((*old_conn_state).hdmi.infoframes.hdmi),
                    core::ptr::addr_of_mut!((*new_conn_state).hdmi.infoframes.hdmi),
                )
            };
            if ret != 0 {
                unsafe { mutex_unlock(lock_ptr) };
                return ret;
            }
        }
    }

    unsafe { mutex_unlock(lock_ptr) };
    ret
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_atomic_helper_connector_hdmi_update_audio_infoframe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_hdmi_update_audio_infoframe(
    connector: *mut DrmConnector,
    frame: *mut HdmiAudioInfoframe,
) -> i32 {
    if connector.is_null() || frame.is_null() {
        return -EINVAL;
    }

    let funcs = unsafe { (*connector).hdmi.funcs };
    let infoframe = unsafe { core::ptr::addr_of_mut!((*connector).hdmi.infoframes.audio) };
    let info = unsafe { &(*connector).display_info };

    if !info.is_hdmi {
        return 0;
    }

    if funcs.is_null() || unsafe { (*funcs).audio.write_infoframe.is_none() } {
        unsafe { drm_dbg_kms!((*connector).dev as *const c_void, "Function not implemented, bailing.\n") };
        return -EINVAL;
    }

    let lock_ptr = unsafe { core::ptr::addr_of_mut!((*connector).hdmi.infoframes.lock) };
    unsafe { mutex_lock(lock_ptr) };

    unsafe {
        let dest = core::ptr::addr_of_mut!((*infoframe).data.audio) as *mut HdmiAudioInfoframe;
        core::ptr::copy_nonoverlapping(frame, dest, 1);
        (*infoframe).set = true;
    }

    let ret = unsafe {
        write_infoframe(
            connector,
            core::ptr::addr_of!((*funcs).audio),
            "Audio",
            infoframe,
        )
    };

    unsafe { mutex_unlock(lock_ptr) };
    ret
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_atomic_helper_connector_hdmi_clear_audio_infoframe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_hdmi_clear_audio_infoframe(
    connector: *mut DrmConnector,
) -> i32 {
    if connector.is_null() {
        return -EINVAL;
    }

    let funcs = unsafe { (*connector).hdmi.funcs };
    let infoframe = unsafe { core::ptr::addr_of_mut!((*connector).hdmi.infoframes.audio) };
    let info = unsafe { &(*connector).display_info };

    if !info.is_hdmi {
        return 0;
    }

    if funcs.is_null() || unsafe { (*funcs).audio.write_infoframe.is_none() } {
        unsafe { drm_dbg_kms!((*connector).dev as *const c_void, "Function not implemented, bailing.\n") };
        return -EINVAL;
    }

    let lock_ptr = unsafe { core::ptr::addr_of_mut!((*connector).hdmi.infoframes.lock) };
    unsafe { mutex_lock(lock_ptr) };

    unsafe { (*infoframe).set = false };

    let ret = unsafe {
        clear_infoframe(
            connector,
            core::ptr::addr_of!((*funcs).audio),
            "Audio",
        )
    };

    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!((*infoframe).data),
            0,
            1,
        );
        mutex_unlock(lock_ptr);
    }

    ret
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_atomic_helper_connector_hdmi_update()
unsafe fn drm_atomic_helper_connector_hdmi_update(
    connector: *mut DrmConnector,
    status: DrmConnectorStatus,
) {
    if connector.is_null() {
        return;
    }

    if status == CONNECTOR_STATUS_DISCONNECTED {
        unsafe {
            drm_edid_connector_update(connector, core::ptr::null());
            crate::drm_connector_upstream::drm_connector_cec_phys_addr_invalidate(connector);
        }
        return;
    }

    let funcs = unsafe { (*connector).hdmi.funcs };
    let drm_edid = if !funcs.is_null() && unsafe { (*funcs).read_edid.is_some() } {
        let read_fn = unsafe { (*funcs).read_edid.unwrap() };
        unsafe { read_fn(connector) as *const DrmEdid }
    } else {
        unsafe { drm_edid_read(connector) }
    };

    unsafe {
        drm_edid_connector_update(connector, drm_edid);
        drm_edid_free(drm_edid);
    }

    if status == CONNECTOR_STATUS_CONNECTED {
        unsafe {
            crate::drm_connector_upstream::drm_connector_cec_phys_addr_set(connector);
        }
    }
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_atomic_helper_connector_hdmi_hotplug()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_hdmi_hotplug(
    connector: *mut DrmConnector,
    status: DrmConnectorStatus,
) {
    unsafe { drm_atomic_helper_connector_hdmi_update(connector, status) };
}

// upstream: drivers/gpu/drm/display/drm_hdmi_state_helper.c drm_atomic_helper_connector_hdmi_force()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_helper_connector_hdmi_force(
    connector: *mut DrmConnector,
) {
    if connector.is_null() {
        return;
    }
    let status = unsafe { (*connector).status };
    unsafe { drm_atomic_helper_connector_hdmi_update(connector, status) };
}
