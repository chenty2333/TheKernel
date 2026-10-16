// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_hdmi.c (Linux v7.2.3).
// ADL-P / PCH-ADP HDMI and DVI path. Older display generations are rejected.
#![allow(non_snake_case, dead_code, unsafe_code, clippy::missing_safety_doc)]

use core::{ffi::c_void, mem::size_of, ptr};

use tk_drm_upstream::{
    drm_atomic_state_helper_upstream::{
        __drm_atomic_helper_connector_destroy_state, __drm_atomic_helper_connector_duplicate_state,
        __drm_atomic_helper_connector_reset, drm_atomic_helper_connector_destroy_state,
    },
    drm_connector_types::{
        CONNECTOR_STATUS_CONNECTED, CONNECTOR_STATUS_DISCONNECTED, DRM_HDMI_BROADCAST_RGB_FULL,
        DRM_HDMI_BROADCAST_RGB_LIMITED, DRM_OUTPUT_COLOR_FORMAT_RGB444,
        DRM_OUTPUT_COLOR_FORMAT_YCBCR420, DRM_OUTPUT_COLOR_FORMAT_YCBCR422,
        DRM_OUTPUT_COLOR_FORMAT_YCBCR444, DrmConnector, DrmConnectorFuncs, DrmConnectorState,
        DrmConnectorStatus,
    },
    drm_edid_upstream::{
        DRM_EDID_HDMI_DC_30, DRM_EDID_HDMI_DC_36, DRM_EDID_YCBCR420_DC_30, DRM_EDID_YCBCR420_DC_36,
        drm_edid_connector_add_modes, drm_edid_connector_update, drm_edid_free, drm_edid_read,
    },
    drm_encoder_types::DrmEncoder,
    drm_hdmi_helper_upstream::drm_hdmi_compute_mode_clock,
    drm_hdmi_state_helper_upstream::drm_atomic_helper_connector_hdmi_check,
    drm_mode_object_upstream::drm_object_attach_property,
    drm_modes_types::{
        DrmDisplayMode, DrmModeStatus, MODE_CLOCK_HIGH, MODE_CLOCK_LOW, MODE_CLOCK_RANGE,
        MODE_ERROR, MODE_OK,
    },
    drm_modes_upstream::drm_mode_is_420,
    drm_modeset_helper_vtables_types::DrmConnectorHelperFuncs,
    drm_probe_helper_upstream::drm_helper_probe_single_connector_modes,
    drm_property_types::{DrmPropEnumList, DrmProperty},
    drm_property_upstream::drm_property_create_enum,
    drm_scdc_helper_upstream::{
        drm_scdc_get_scrambling_status, drm_scdc_set_high_tmds_clock_ratio, drm_scdc_set_scrambling,
    },
    hdmi_types::{
        HDMI_INFOFRAME_TYPE_AVI, HDMI_INFOFRAME_TYPE_DRM, HDMI_INFOFRAME_TYPE_SPD,
        HDMI_INFOFRAME_TYPE_VENDOR, HdmiInfoframe,
    },
    uapi_drm_consts::{
        DRM_MODE_CONNECTOR_HDMIA, DRM_MODE_FLAG_DBLCLK, DRM_MODE_FLAG_DBLSCAN,
        DRM_MODE_FLAG_INTERLACE,
    },
};
use tk_linux_kapi::{
    config::{EINVAL, GFP_KERNEL},
    memory::{kfree, kzalloc},
};

#[cfg(test)]
use crate::intel_display_limits_types_upstream::PORT_B;
use crate::{
    intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_write},
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PORT_A, PORT_C, PORT_TC1, PORT_TC6},
    intel_display_regs_upstream::{
        ADL_TVIDEO_DIP_AS_SDP_DATA, GCP_COLOR_INDICATION, GCP_DEFAULT_PHASE_ENABLE,
        GLK_TVIDEO_DIP_DRM_DATA, HSW_TVIDEO_DIP_AVI_DATA, HSW_TVIDEO_DIP_CTL, HSW_TVIDEO_DIP_GCP,
        HSW_TVIDEO_DIP_GMP_DATA, HSW_TVIDEO_DIP_SPD_DATA, HSW_TVIDEO_DIP_VS_DATA,
        HSW_TVIDEO_DIP_VSC_DATA, ICL_VIDEO_DIP_PPS_DATA, VDIP_ENABLE_PPS,
        VIDEO_DIP_ASYNC_DATA_SIZE, VIDEO_DIP_DATA_SIZE, VIDEO_DIP_ENABLE_AS_ADL,
        VIDEO_DIP_ENABLE_AVI_HSW, VIDEO_DIP_ENABLE_DRM_GLK, VIDEO_DIP_ENABLE_GCP_HSW,
        VIDEO_DIP_ENABLE_GMP_HSW, VIDEO_DIP_ENABLE_SPD_HSW, VIDEO_DIP_ENABLE_VS_HSW,
        VIDEO_DIP_ENABLE_VSC_HSW, VIDEO_DIP_GMP_DATA_SIZE, VIDEO_DIP_PPS_DATA_SIZE,
        VIDEO_DIP_VSC_DATA_SIZE, VSC_DIP_HW_DATA_SW_HEA,
    },
    intel_display_types_upstream::{
        HDMI_AUDIO_AUTO, HDMI_AUDIO_OFF, HDMI_AUDIO_OFF_DVI, HDMI_AUDIO_ON,
        INTEL_OUTPUT_FORMAT_RGB, INTEL_OUTPUT_FORMAT_YCBCR420, INTEL_OUTPUT_FORMAT_YCBCR444,
        IntelConnector, IntelCrtcState, IntelDigitalConnectorState, IntelDigitalPort, IntelEncoder,
    },
    intel_gmbus_upstream::{intel_gmbus_get_adapter, intel_gmbus_is_valid_pin},
    intel_workarounds_types_upstream::I915RegT,
};

const HDMI_DDC_PIN_FIRST_COMBO: u32 = 1;
const HDMI_DDC_PIN_FIRST_TC: u32 = 9;
const HDMI_DEFAULT_MAX_CLOCK_DVI_KHZ: u32 = 165_000;
const HDMI_MAX_CLOCK_KHZ: u32 = 600_000;
const HDMI_SCRAMBLING_THRESHOLD_KHZ: u32 = 340_000;
const HDMI_TC_PLL_GAP_START_KHZ: u32 = 500_000;
const HDMI_TC_PLL_GAP_END_KHZ: u32 = 532_800;
const HDMI_GCP_PACKET_TYPE: u32 = 0x03;
const HDMI_GAMUT_PACKET_TYPE: u32 = 0x0a;
const DP_SDP_VSC: u32 = 0x07;
const DP_SDP_PPS: u32 = 0x10;
const DP_SDP_ADAPTIVE_SYNC: u32 = 0x22;
const HDMI_INFOFRAME_TYPES: [u32; 8] = [
    HDMI_GCP_PACKET_TYPE,
    HDMI_GAMUT_PACKET_TYPE,
    DP_SDP_VSC,
    DP_SDP_ADAPTIVE_SYNC,
    HDMI_INFOFRAME_TYPE_AVI,
    HDMI_INFOFRAME_TYPE_SPD,
    HDMI_INFOFRAME_TYPE_VENDOR,
    HDMI_INFOFRAME_TYPE_DRM,
];

#[inline]
fn ddc_pin_for_port(port: i32) -> Option<u32> {
    match port {
        PORT_A..=PORT_C => Some(HDMI_DDC_PIN_FIRST_COMBO + port as u32),
        PORT_TC1..=PORT_TC6 => Some(HDMI_DDC_PIN_FIRST_TC + (port - PORT_TC1) as u32),
        _ => None,
    }
}

#[inline]
fn hsw_infoframe_enable(info_type: u32) -> u32 {
    match info_type {
        HDMI_GCP_PACKET_TYPE => VIDEO_DIP_ENABLE_GCP_HSW,
        HDMI_GAMUT_PACKET_TYPE => VIDEO_DIP_ENABLE_GMP_HSW,
        DP_SDP_VSC => VIDEO_DIP_ENABLE_VSC_HSW,
        DP_SDP_ADAPTIVE_SYNC => VIDEO_DIP_ENABLE_AS_ADL,
        DP_SDP_PPS => VDIP_ENABLE_PPS,
        HDMI_INFOFRAME_TYPE_AVI => VIDEO_DIP_ENABLE_AVI_HSW,
        HDMI_INFOFRAME_TYPE_SPD => VIDEO_DIP_ENABLE_SPD_HSW,
        HDMI_INFOFRAME_TYPE_VENDOR => VIDEO_DIP_ENABLE_VS_HSW,
        HDMI_INFOFRAME_TYPE_DRM => VIDEO_DIP_ENABLE_DRM_GLK,
        _ => 0,
    }
}

#[inline]
unsafe fn hsw_dip_data_reg(
    display: *mut IntelDisplay,
    transcoder: u32,
    info_type: u32,
    word: u32,
) -> Option<I915RegT> {
    Some(match info_type {
        HDMI_GAMUT_PACKET_TYPE => HSW_TVIDEO_DIP_GMP_DATA(display, transcoder, word),
        DP_SDP_VSC => HSW_TVIDEO_DIP_VSC_DATA(display, transcoder, word),
        DP_SDP_ADAPTIVE_SYNC => ADL_TVIDEO_DIP_AS_SDP_DATA(display, transcoder, word),
        DP_SDP_PPS => ICL_VIDEO_DIP_PPS_DATA(display, transcoder, word),
        HDMI_INFOFRAME_TYPE_AVI => HSW_TVIDEO_DIP_AVI_DATA(display, transcoder, word),
        HDMI_INFOFRAME_TYPE_SPD => HSW_TVIDEO_DIP_SPD_DATA(display, transcoder, word),
        HDMI_INFOFRAME_TYPE_VENDOR => HSW_TVIDEO_DIP_VS_DATA(display, transcoder, word),
        HDMI_INFOFRAME_TYPE_DRM => GLK_TVIDEO_DIP_DRM_DATA(display, transcoder, word),
        _ => return None,
    })
}

#[inline]
unsafe fn hsw_dip_data_size(display: *mut IntelDisplay, info_type: u32) -> usize {
    match info_type {
        DP_SDP_VSC => VIDEO_DIP_VSC_DATA_SIZE as usize,
        DP_SDP_ADAPTIVE_SYNC => VIDEO_DIP_ASYNC_DATA_SIZE as usize,
        DP_SDP_PPS => VIDEO_DIP_PPS_DATA_SIZE as usize,
        HDMI_GAMUT_PACKET_TYPE if (*display).info.__runtime_info.ip.ver >= 11 => {
            VIDEO_DIP_GMP_DATA_SIZE as usize
        }
        _ => VIDEO_DIP_DATA_SIZE as usize,
    }
}

unsafe extern "C" fn hsw_write_infoframe(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    info_type: u32,
    frame: *mut c_void,
    len: isize,
) {
    if encoder.is_null() || crtc_state.is_null() || frame.is_null() || len < 0 {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let data_size = hsw_dip_data_size(display, info_type);
    if len as usize > data_size
        || hsw_dip_data_reg(display, (*crtc_state).cpu_transcoder as u32, info_type, 0).is_none()
    {
        return;
    }

    let ctl = HSW_TVIDEO_DIP_CTL(display, (*crtc_state).cpu_transcoder as u32);
    let mut ctl_val = intel_de_read(display, ctl);
    ctl_val &= !hsw_infoframe_enable(info_type);
    intel_de_write(display, ctl, ctl_val);

    let bytes = frame.cast::<u8>();
    let words = (len as usize).div_ceil(4);
    for word in 0..words {
        let mut packed = [0u8; 4];
        let start = word * 4;
        let count = (len as usize - start).min(4);
        ptr::copy_nonoverlapping(bytes.add(start), packed.as_mut_ptr(), count);
        if let Some(reg) = hsw_dip_data_reg(
            display,
            (*crtc_state).cpu_transcoder as u32,
            info_type,
            word as u32,
        ) {
            intel_de_write(display, reg, u32::from_le_bytes(packed));
        } else {
            return;
        }
    }
    for word in words..(data_size / 4) {
        if let Some(reg) = hsw_dip_data_reg(
            display,
            (*crtc_state).cpu_transcoder as u32,
            info_type,
            word as u32,
        ) {
            intel_de_write(display, reg, 0);
        } else {
            return;
        }
    }

    let workaround_suppresses_vsc = (*display).info.__runtime_info.ip.ver >= 13
        && (*display).info.__runtime_info.ip.ver <= 14
        && (*crtc_state).has_psr
        && !(*crtc_state).has_panel_replay
        && info_type == DP_SDP_VSC;
    if !workaround_suppresses_vsc {
        ctl_val |= hsw_infoframe_enable(info_type);
    }
    if info_type == DP_SDP_VSC {
        ctl_val |= VSC_DIP_HW_DATA_SW_HEA;
    }
    intel_de_write(display, ctl, ctl_val);
    intel_de_posting_read(display, ctl);
}

unsafe extern "C" fn hsw_read_infoframe(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    info_type: u32,
    frame: *mut c_void,
    len: isize,
) {
    if encoder.is_null() || crtc_state.is_null() || frame.is_null() || len < 0 {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null()
        || hsw_dip_data_reg(display, (*crtc_state).cpu_transcoder as u32, info_type, 0).is_none()
    {
        return;
    }
    let bytes = frame.cast::<u8>();
    for word in 0..(len as usize).div_ceil(4) {
        let Some(reg) = hsw_dip_data_reg(
            display,
            (*crtc_state).cpu_transcoder as u32,
            info_type,
            word as u32,
        ) else {
            return;
        };
        let packed = intel_de_read(display, reg).to_le_bytes();
        let start = word * 4;
        let count = (len as usize - start).min(4);
        ptr::copy_nonoverlapping(packed.as_ptr(), bytes.add(start), count);
    }
}

unsafe extern "C" fn hsw_infoframes_enabled(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
) -> u32 {
    if encoder.is_null() || crtc_state.is_null() {
        return 0;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return 0;
    }
    let ctl = HSW_TVIDEO_DIP_CTL(display, (*crtc_state).cpu_transcoder as u32);
    let mut mask = VIDEO_DIP_ENABLE_VSC_HSW
        | VIDEO_DIP_ENABLE_AVI_HSW
        | VIDEO_DIP_ENABLE_GCP_HSW
        | VIDEO_DIP_ENABLE_VS_HSW
        | VIDEO_DIP_ENABLE_GMP_HSW
        | VIDEO_DIP_ENABLE_SPD_HSW
        | VIDEO_DIP_ENABLE_DRM_GLK
        | VIDEO_DIP_ENABLE_AS_ADL;
    if (*display).info.__runtime_info.ip.ver < 10 {
        mask &= !VIDEO_DIP_ENABLE_DRM_GLK;
    }
    intel_de_read(display, ctl) & mask
}

unsafe extern "C" fn hsw_set_infoframes(
    encoder: *mut IntelEncoder,
    enable: bool,
    crtc_state: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) {
    if encoder.is_null() || crtc_state.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let ctl = HSW_TVIDEO_DIP_CTL(display, (*crtc_state).cpu_transcoder as u32);
    let mask = VIDEO_DIP_ENABLE_VSC_HSW
        | VIDEO_DIP_ENABLE_AVI_HSW
        | VIDEO_DIP_ENABLE_GCP_HSW
        | VIDEO_DIP_ENABLE_VS_HSW
        | VIDEO_DIP_ENABLE_GMP_HSW
        | VIDEO_DIP_ENABLE_SPD_HSW
        | VIDEO_DIP_ENABLE_DRM_GLK
        | VIDEO_DIP_ENABLE_AS_ADL;
    let mut ctl_val = intel_de_read(display, ctl) & !mask;
    if !enable {
        intel_de_write(display, ctl, ctl_val);
        intel_de_posting_read(display, ctl);
        return;
    }
    if (*crtc_state).infoframes.enable & intel_hdmi_infoframe_enable(HDMI_GCP_PACKET_TYPE) != 0 {
        let gcp = HSW_TVIDEO_DIP_GCP(display, (*crtc_state).cpu_transcoder as u32);
        intel_de_write(display, gcp, (*crtc_state).infoframes.gcp);
        ctl_val |= VIDEO_DIP_ENABLE_GCP_HSW;
    }
    intel_de_write(display, ctl, ctl_val);
    intel_de_posting_read(display, ctl);
    intel_write_infoframe(
        encoder,
        crtc_state,
        HDMI_INFOFRAME_TYPE_AVI,
        ptr::addr_of!((*crtc_state).infoframes.avi),
    );
    intel_write_infoframe(
        encoder,
        crtc_state,
        HDMI_INFOFRAME_TYPE_SPD,
        ptr::addr_of!((*crtc_state).infoframes.spd),
    );
    intel_write_infoframe(
        encoder,
        crtc_state,
        HDMI_INFOFRAME_TYPE_VENDOR,
        ptr::addr_of!((*crtc_state).infoframes.hdmi),
    );
    intel_write_infoframe(
        encoder,
        crtc_state,
        HDMI_INFOFRAME_TYPE_DRM,
        ptr::addr_of!((*crtc_state).infoframes.drm),
    );
    let _ = conn_state;
}

unsafe fn intel_write_infoframe(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    info_type: u32,
    frame: *const HdmiInfoframe,
) {
    if encoder.is_null()
        || crtc_state.is_null()
        || frame.is_null()
        || (*crtc_state).infoframes.enable & intel_hdmi_infoframe_enable(info_type) == 0
        || ptr::read_unaligned(frame.cast::<u32>()) != info_type
    {
        return;
    }
    let mut buffer = [0u8; VIDEO_DIP_DATA_SIZE as usize];
    let len = tk_drm_upstream::hdmi_upstream::hdmi_infoframe_pack_only(
        frame,
        buffer.as_mut_ptr().add(1).cast::<c_void>(),
        buffer.len() - 1,
    );
    if len < 0 {
        return;
    }
    let Some(dip_len) = dip_insert_hole(&mut buffer, len as usize) else {
        return;
    };
    let port = encoder.cast::<IntelDigitalPort>();
    if let Some(write) = (*port).write_infoframe {
        write(
            encoder,
            crtc_state,
            info_type,
            buffer.as_mut_ptr().cast::<c_void>(),
            dip_len as isize,
        );
    }
}

#[inline]
fn dip_insert_hole(buffer: &mut [u8], packed_len: usize) -> Option<usize> {
    if packed_len < 3 || packed_len + 1 > buffer.len() {
        return None;
    }
    buffer.copy_within(1..4, 0);
    buffer[3] = 0;
    Some(packed_len + 1)
}

#[inline]
fn dip_remove_hole(buffer: &mut [u8]) -> bool {
    if buffer.len() < 4 {
        return false;
    }
    buffer.copy_within(0..3, 1);
    true
}

#[inline]
unsafe fn connector_from_encoder(encoder: *mut IntelEncoder) -> *mut IntelConnector {
    if encoder.is_null() {
        return ptr::null_mut();
    }
    let port = encoder.cast::<IntelDigitalPort>();
    (*port).hdmi.attached_connector
}

#[inline]
unsafe fn display_from_encoder(encoder: *mut IntelEncoder) -> *mut IntelDisplay {
    if encoder.is_null() || (*encoder).base.dev.is_null() {
        return ptr::null_mut();
    }
    __drm_to_display((*encoder).base.dev)
}

#[inline]
unsafe fn intel_connector(connector: *mut DrmConnector) -> *mut IntelConnector {
    connector.cast::<IntelConnector>()
}

#[inline]
unsafe fn digital_state(state: *mut DrmConnectorState) -> *mut IntelDigitalConnectorState {
    state.cast::<IntelDigitalConnectorState>()
}

unsafe extern "C" fn connector_destroy(connector: *mut DrmConnector) {
    if !connector.is_null() {
        tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(connector);
    }
}

unsafe extern "C" fn connector_reset(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let state = kzalloc(size_of::<IntelDigitalConnectorState>(), GFP_KERNEL)
        .cast::<IntelDigitalConnectorState>();
    if state.is_null() {
        return;
    }
    if !(*connector).state.is_null() {
        __drm_atomic_helper_connector_destroy_state((*connector).state);
        kfree((*connector).state.cast::<c_void>());
    }
    __drm_atomic_helper_connector_reset(connector, ptr::addr_of_mut!((*state).base));
    tk_drm_upstream::drm_hdmi_state_helper_upstream::__drm_atomic_helper_connector_hdmi_reset(
        connector,
        ptr::addr_of_mut!((*state).base),
    );
    (*state).force_audio = HDMI_AUDIO_AUTO;
    (*state).broadcast_rgb = (*state).base.hdmi.broadcast_rgb as i32;
}

unsafe extern "C" fn connector_duplicate_state(
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if connector.is_null() || (*connector).state.is_null() {
        return ptr::null_mut();
    }
    let state = kzalloc(size_of::<IntelDigitalConnectorState>(), GFP_KERNEL)
        .cast::<IntelDigitalConnectorState>();
    if state.is_null() {
        return ptr::null_mut();
    }
    __drm_atomic_helper_connector_duplicate_state(connector, ptr::addr_of_mut!((*state).base));
    let old = (*connector).state.cast::<IntelDigitalConnectorState>();
    (*state).force_audio = (*old).force_audio;
    (*state).broadcast_rgb = (*old).broadcast_rgb;
    ptr::addr_of_mut!((*state).base)
}

unsafe extern "C" fn connector_detect(
    connector: *mut DrmConnector,
    _force: bool,
) -> DrmConnectorStatus {
    if connector.is_null() || (*connector).ddc.is_null() {
        return CONNECTOR_STATUS_DISCONNECTED;
    }
    let edid = drm_edid_read(connector);
    if edid.is_null() {
        let _ = drm_edid_connector_update(connector, ptr::null());
        return CONNECTOR_STATUS_DISCONNECTED;
    }
    let _ = drm_edid_connector_update(connector, edid);
    drm_edid_free(edid);
    CONNECTOR_STATUS_CONNECTED
}

unsafe extern "C" fn connector_get_modes(connector: *mut DrmConnector) -> i32 {
    if connector.is_null() || (*connector).ddc.is_null() {
        return 0;
    }
    let edid = drm_edid_read(connector);
    let count = if edid.is_null() {
        let _ = drm_edid_connector_update(connector, ptr::null());
        0
    } else {
        let ret = drm_edid_connector_update(connector, edid);
        let count = if ret < 0 {
            0
        } else {
            drm_edid_connector_add_modes(connector)
        };
        drm_edid_free(edid);
        count
    };
    count
}

unsafe extern "C" fn connector_best_encoder(connector: *mut DrmConnector) -> *mut DrmEncoder {
    if connector.is_null() {
        return ptr::null_mut();
    }
    let intel = intel_connector(connector);
    if (*intel).encoder.is_null() {
        ptr::null_mut()
    } else {
        ptr::addr_of_mut!((*(*intel).encoder).base)
    }
}

unsafe extern "C" fn connector_atomic_check(
    connector: *mut DrmConnector,
    state: *mut tk_drm_upstream::drm_atomic_types::DrmAtomicCommit,
) -> i32 {
    drm_atomic_helper_connector_hdmi_check(connector, state)
}

unsafe extern "C" fn connector_atomic_set_property(
    connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
    property: *mut DrmProperty,
    value: u64,
) -> i32 {
    if connector.is_null() || state.is_null() || property.is_null() {
        return -EINVAL;
    }
    let display = __drm_to_display((*connector).dev);
    if display.is_null() || property != (*display).properties.force_audio {
        return -EINVAL;
    }
    (*digital_state(state)).force_audio = value as i32;
    0
}

unsafe extern "C" fn connector_atomic_get_property(
    connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
    property: *mut DrmProperty,
    value: *mut u64,
) -> i32 {
    if connector.is_null() || state.is_null() || property.is_null() || value.is_null() {
        return -EINVAL;
    }
    let display = __drm_to_display((*connector).dev);
    if display.is_null() || property != (*display).properties.force_audio {
        return -EINVAL;
    }
    *value = (*digital_state(state)).force_audio as i64 as u64;
    0
}

unsafe fn attach_force_audio_property(
    connector: *mut DrmConnector,
    display: *mut IntelDisplay,
) -> i32 {
    static FORCE_DVI: &[u8] = b"force-dvi\0";
    static OFF: &[u8] = b"off\0";
    static AUTO: &[u8] = b"auto\0";
    static ON: &[u8] = b"on\0";
    let names = [
        DrmPropEnumList {
            r#type: HDMI_AUDIO_OFF_DVI,
            name: FORCE_DVI.as_ptr().cast_mut().cast(),
        },
        DrmPropEnumList {
            r#type: HDMI_AUDIO_OFF,
            name: OFF.as_ptr().cast_mut().cast(),
        },
        DrmPropEnumList {
            r#type: HDMI_AUDIO_AUTO,
            name: AUTO.as_ptr().cast_mut().cast(),
        },
        DrmPropEnumList {
            r#type: HDMI_AUDIO_ON,
            name: ON.as_ptr().cast_mut().cast(),
        },
    ];
    let mut property = (*display).properties.force_audio;
    if property.is_null() {
        property = drm_property_create_enum(
            (*display).drm,
            0,
            c"audio".as_ptr(),
            names.as_ptr(),
            names.len() as i32,
        );
        if property.is_null() {
            return -tk_linux_kapi::config::ENOMEM;
        }
        (*display).properties.force_audio = property;
    }
    drm_object_attach_property(ptr::addr_of_mut!((*connector).base), property, 0);
    0
}

unsafe extern "C" fn connector_mode_valid(
    connector: *mut DrmConnector,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    if connector.is_null() || mode.is_null() {
        return MODE_ERROR;
    }
    if (*mode).flags & DRM_MODE_FLAG_DBLSCAN != 0 {
        return MODE_ERROR;
    }
    let intel = intel_connector(connector);
    let encoder = (*intel).encoder;
    let is_hdmi = (*connector).display_info.is_hdmi;
    let info = &(*connector).display_info;
    let has_420 = (*connector).ycbcr_420_allowed && drm_mode_is_420(info, mode);
    let base_clock = ((*mode).clock.max(0) as i64)
        * if (*mode).flags & DRM_MODE_FLAG_DBLCLK != 0 {
            2
        } else {
            1
        };
    if base_clock > HDMI_MAX_CLOCK_KHZ as i64 {
        return MODE_CLOCK_HIGH;
    }
    let base_clock = base_clock.min(i32::MAX as i64) as i32;
    let mut failure = MODE_CLOCK_HIGH;

    for format in [INTEL_OUTPUT_FORMAT_RGB, INTEL_OUTPUT_FORMAT_YCBCR420] {
        if format == INTEL_OUTPUT_FORMAT_YCBCR420 && (!is_hdmi || !has_420) {
            continue;
        }
        for bpc in [12, 10, 8] {
            let depth_modes = if format == INTEL_OUTPUT_FORMAT_YCBCR420 {
                info.hdmi.y420_dc_modes
            } else {
                info.edid_hdmi_rgb444_dc_modes
            };
            let depth_ok = match (format, bpc) {
                (_, 8) => true,
                (INTEL_OUTPUT_FORMAT_YCBCR420, 10) => depth_modes & DRM_EDID_YCBCR420_DC_30 != 0,
                (INTEL_OUTPUT_FORMAT_YCBCR420, 12) => depth_modes & DRM_EDID_YCBCR420_DC_36 != 0,
                (_, 10) => depth_modes & DRM_EDID_HDMI_DC_30 != 0,
                (_, 12) => depth_modes & DRM_EDID_HDMI_DC_36 != 0,
                _ => false,
            };
            if !depth_ok || (!is_hdmi && bpc != 8) {
                continue;
            }
            let tmds = intel_hdmi_tmds_clock(base_clock, bpc, format);
            if tmds < 25_000 {
                failure = MODE_CLOCK_LOW;
                continue;
            }
            let max_sink_clock = if info.max_tmds_clock != 0 {
                info.max_tmds_clock
            } else if !is_hdmi {
                HDMI_DEFAULT_MAX_CLOCK_DVI_KHZ as i32
            } else {
                HDMI_MAX_CLOCK_KHZ as i32
            };
            if tmds > HDMI_MAX_CLOCK_KHZ as i32 || tmds > max_sink_clock {
                continue;
            }
            if tmds > HDMI_SCRAMBLING_THRESHOLD_KHZ as i32 && !info.hdmi.scdc.scrambling.supported {
                continue;
            }
            if !encoder.is_null() {
                let port = (*encoder).port;
                let gap_end = if port >= PORT_TC1 {
                    HDMI_TC_PLL_GAP_END_KHZ
                } else {
                    533_200
                };
                if tmds > HDMI_TC_PLL_GAP_START_KHZ as i32 && tmds < gap_end as i32 {
                    failure = MODE_CLOCK_RANGE;
                    continue;
                }
            }
            return MODE_OK;
        }
    }
    failure
}

static HDMI_CONNECTOR_FUNCS: DrmConnectorFuncs = DrmConnectorFuncs {
    dpms: None,
    reset: Some(connector_reset),
    detect: Some(connector_detect),
    force: None,
    fill_modes: Some(drm_helper_probe_single_connector_modes),
    set_property: None,
    late_register: None,
    early_unregister: None,
    destroy: Some(connector_destroy),
    atomic_duplicate_state: Some(connector_duplicate_state),
    atomic_destroy_state: Some(drm_atomic_helper_connector_destroy_state),
    atomic_set_property: Some(connector_atomic_set_property),
    atomic_get_property: Some(connector_atomic_get_property),
    atomic_print_state: None,
    oob_hotplug_event: None,
    debugfs_init: None,
};

static HDMI_CONNECTOR_HELPERS: DrmConnectorHelperFuncs = DrmConnectorHelperFuncs {
    get_modes: Some(connector_get_modes),
    detect_ctx: None,
    mode_valid: Some(connector_mode_valid),
    mode_valid_ctx: None,
    best_encoder: Some(connector_best_encoder),
    atomic_best_encoder: None,
    atomic_check: Some(connector_atomic_check),
    atomic_commit: None,
    prepare_writeback_job: None,
    cleanup_writeback_job: None,
    enable_hpd: None,
    disable_hpd: None,
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_is_frl(clock: u32) -> bool {
    // HDMI FRL rate clocks used by the DPLL helpers. ADL-P does not itself
    // enable FRL output, but consumers use this predicate to classify rates.
    matches!(clock, 300_000 | 600_000 | 800_000 | 1_000_000 | 1_200_000)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_infoframe_enable(info_type: u32) -> u32 {
    HDMI_INFOFRAME_TYPES
        .iter()
        .position(|&candidate| candidate == info_type)
        .map_or(0, |idx| 1u32 << idx)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_infoframes_enabled(
    encoder: *mut IntelEncoder,
    crtc_state: *const IntelCrtcState,
) -> u32 {
    if encoder.is_null() || crtc_state.is_null() {
        return 0;
    }
    let port = encoder.cast::<IntelDigitalPort>();
    let Some(read_enabled) = (*port).infoframes_enabled else {
        return 0;
    };
    let hw_enable = read_enabled(encoder, crtc_state.cast_mut());
    let mut enabled = 0;
    for (index, &info_type) in HDMI_INFOFRAME_TYPES.iter().enumerate() {
        if hw_enable & hsw_infoframe_enable(info_type) != 0 {
            enabled |= 1u32 << index;
        }
    }
    enabled
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_infoframe_init(dig_port: *mut IntelDigitalPort) {
    if !dig_port.is_null() {
        (*dig_port).write_infoframe = Some(hsw_write_infoframe);
        (*dig_port).read_infoframe = Some(hsw_read_infoframe);
        (*dig_port).set_infoframes = Some(hsw_set_infoframes);
        (*dig_port).infoframes_enabled = Some(infoframes_enabled_callback);
    }
}

unsafe extern "C" fn infoframes_enabled_callback(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
) -> u32 {
    hsw_infoframes_enabled(encoder, crtc_state)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_compute_has_hdmi_sink(
    encoder: *mut IntelEncoder,
    _crtc_state: *const IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> bool {
    if encoder.is_null() || conn_state.is_null() {
        return false;
    }
    let connector = connector_from_encoder(encoder);
    if connector.is_null() {
        return false;
    }
    (*connector).base.display_info.is_hdmi
        && (*conn_state.cast::<IntelDigitalConnectorState>()).force_audio != HDMI_AUDIO_OFF_DVI
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_tmds_clock(clock: i32, bpc: i32, sink_format: u32) -> i32 {
    if clock <= 0 || bpc < 8 {
        return 0;
    }
    let pixel_clock = if sink_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
        clock / 2
    } else {
        clock
    };
    ((pixel_clock as i64 * bpc as i64 + 4) / 8).min(i32::MAX as i64) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_bpc_possible(
    crtc_state: *const IntelCrtcState,
    bpc: i32,
    has_hdmi_sink: bool,
) -> bool {
    if crtc_state.is_null() || !(8..=12).contains(&bpc) || bpc % 2 != 0 {
        return false;
    }
    if !has_hdmi_sink {
        return bpc == 8;
    }
    let atomic = (*crtc_state).uapi.state;
    if atomic.is_null() || (*atomic).connectors.is_null() {
        return bpc == 8;
    }
    for index in 0..(*atomic).num_connector.max(0) as usize {
        let conn_state = (*(*atomic).connectors.add(index)).new_state;
        if conn_state.is_null() || (*conn_state).crtc != (*crtc_state).uapi.crtc {
            continue;
        }
        let connector = (*conn_state).connector.cast::<IntelConnector>();
        if connector.is_null() {
            continue;
        }
        let info = &(*connector).base.display_info;
        let supported = if (*crtc_state).sink_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
            info.hdmi.y420_dc_modes
        } else {
            info.edid_hdmi_rgb444_dc_modes
        };
        return match bpc {
            8 => true,
            10 if (*crtc_state).sink_format == INTEL_OUTPUT_FORMAT_YCBCR420 => {
                supported & DRM_EDID_YCBCR420_DC_30 != 0
            }
            10 => supported & DRM_EDID_HDMI_DC_30 != 0,
            12 if (*crtc_state).sink_format == INTEL_OUTPUT_FORMAT_YCBCR420 => {
                supported & DRM_EDID_YCBCR420_DC_36 != 0
            }
            12 => supported & DRM_EDID_HDMI_DC_36 != 0,
            _ => false,
        };
    }
    bpc == 8
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_limited_color_range(
    _crtc_state: *const IntelCrtcState,
    conn_state: *const DrmConnectorState,
) -> bool {
    if conn_state.is_null() {
        return false;
    }
    let state = &*conn_state;
    if state.hdmi.broadcast_rgb == DRM_HDMI_BROADCAST_RGB_LIMITED {
        true
    } else if state.hdmi.broadcast_rgb == DRM_HDMI_BROADCAST_RGB_FULL {
        false
    } else {
        state.hdmi.is_limited_range
    }
}

unsafe fn copy_infoframe(dst: *mut HdmiInfoframe, src: *const HdmiInfoframe, set: bool) {
    if set {
        ptr::copy_nonoverlapping(src, dst, 1);
    } else {
        ptr::write_bytes(dst.cast::<u8>(), 0, size_of::<HdmiInfoframe>());
    }
}

unsafe fn compute_infoframes(
    crtc: *mut IntelCrtcState,
    connector: *mut IntelConnector,
    conn_state: *const DrmConnectorState,
) {
    let state = &*conn_state;
    let frames = &state.hdmi.infoframes;
    let info = ptr::addr_of_mut!((*crtc).infoframes);
    (*info).enable = 0;
    (*info).gcp = 0;
    copy_infoframe(
        ptr::addr_of_mut!((*info).avi),
        &frames.avi.data,
        frames.avi.set,
    );
    copy_infoframe(
        ptr::addr_of_mut!((*info).spd),
        &frames.spd.data,
        frames.spd.set,
    );
    copy_infoframe(
        ptr::addr_of_mut!((*info).hdmi),
        &frames.hdmi.data,
        frames.hdmi.set,
    );
    copy_infoframe(
        ptr::addr_of_mut!((*info).drm),
        &frames.hdr_drm.data,
        frames.hdr_drm.set,
    );

    if (*crtc).has_infoframe {
        (*info).enable |= intel_hdmi_infoframe_enable(HDMI_GCP_PACKET_TYPE);
        if (*crtc).pipe_bpp > 24 {
            (*info).gcp |= GCP_COLOR_INDICATION;
        }
        let packing = match (*crtc).pipe_bpp {
            30 => 4u16,
            36 => 2u16,
            48 => 1u16,
            _ => 0,
        };
        if packing != 0 {
            let mode = &(*crtc).hw.adjusted_mode;
            let aligned = mode.crtc_hdisplay % packing == 0
                && mode.crtc_htotal % packing == 0
                && mode.crtc_hblank_start % packing == 0
                && mode.crtc_hblank_end % packing == 0
                && mode.crtc_hsync_start % packing == 0
                && mode.crtc_hsync_end % packing == 0
                && (mode.flags & DRM_MODE_FLAG_INTERLACE == 0
                    || (mode.crtc_htotal / 2) % packing == 0);
            if aligned {
                (*info).gcp |= GCP_DEFAULT_PHASE_ENABLE;
            }
        }
        if frames.avi.set {
            (*info).enable |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_AVI);
        }
        if frames.spd.set {
            (*info).enable |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_SPD);
        }
        if frames.hdmi.set {
            (*info).enable |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_VENDOR);
        }
        if frames.hdr_drm.set {
            (*info).enable |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_DRM);
        }
    }
    let _ = connector;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_compute_config(
    encoder: *mut IntelEncoder,
    pipe_config: *mut IntelCrtcState,
    conn_state: *mut DrmConnectorState,
) -> i32 {
    if encoder.is_null() || pipe_config.is_null() || conn_state.is_null() {
        return -EINVAL;
    }
    let connector = connector_from_encoder(encoder);
    let display = display_from_encoder(encoder);
    if connector.is_null() || display.is_null() || (*display).info.__runtime_info.ip.ver != 13 {
        return -EINVAL;
    }
    let mode = ptr::addr_of_mut!((*pipe_config).hw.adjusted_mode);
    let flags = (*mode).flags;
    if flags & DRM_MODE_FLAG_DBLSCAN != 0
        || (!(*connector).base.interlace_allowed && flags & DRM_MODE_FLAG_INTERLACE != 0)
    {
        return -EINVAL;
    }

    let has_hdmi_sink = intel_hdmi_compute_has_hdmi_sink(encoder, pipe_config, conn_state);
    (*pipe_config).has_hdmi_sink = has_hdmi_sink;
    (*pipe_config).has_infoframe = has_hdmi_sink;
    (*pipe_config).output_format = INTEL_OUTPUT_FORMAT_RGB;
    (*pipe_config).sink_format = INTEL_OUTPUT_FORMAT_RGB;
    (*pipe_config).lane_count = 4;
    (*pipe_config).pixel_multiplier = if flags & DRM_MODE_FLAG_DBLCLK != 0 {
        2
    } else {
        1
    };

    let digital = digital_state(conn_state);
    let requested_bpc = (*conn_state).hdmi.output_bpc as i32;
    let bpc = if has_hdmi_sink && (8..=12).contains(&requested_bpc) && requested_bpc % 2 == 0 {
        requested_bpc
    } else {
        8
    };
    let format = (*conn_state).hdmi.output_format;
    let (drm_format, intel_format) = match format {
        DRM_OUTPUT_COLOR_FORMAT_YCBCR420 => (
            DRM_OUTPUT_COLOR_FORMAT_YCBCR420,
            INTEL_OUTPUT_FORMAT_YCBCR420,
        ),
        DRM_OUTPUT_COLOR_FORMAT_YCBCR444 => (
            DRM_OUTPUT_COLOR_FORMAT_YCBCR444,
            INTEL_OUTPUT_FORMAT_YCBCR444,
        ),
        DRM_OUTPUT_COLOR_FORMAT_YCBCR422 => (
            DRM_OUTPUT_COLOR_FORMAT_YCBCR422,
            INTEL_OUTPUT_FORMAT_YCBCR444,
        ),
        _ => (DRM_OUTPUT_COLOR_FORMAT_RGB444, INTEL_OUTPUT_FORMAT_RGB),
    };
    if !has_hdmi_sink && drm_format != DRM_OUTPUT_COLOR_FORMAT_RGB444 {
        return -EINVAL;
    }
    let color_depth = if drm_format == DRM_OUTPUT_COLOR_FORMAT_YCBCR422 {
        8
    } else {
        bpc
    };
    let sink_depth_modes = if intel_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
        (*connector).base.display_info.hdmi.y420_dc_modes
    } else {
        (*connector).base.display_info.edid_hdmi_rgb444_dc_modes
    };
    let supports_10bpc = if intel_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
        sink_depth_modes & DRM_EDID_YCBCR420_DC_30 != 0
    } else {
        sink_depth_modes & DRM_EDID_HDMI_DC_30 != 0
    };
    let supports_12bpc = if intel_format == INTEL_OUTPUT_FORMAT_YCBCR420 {
        sink_depth_modes & DRM_EDID_YCBCR420_DC_36 != 0
    } else {
        sink_depth_modes & DRM_EDID_HDMI_DC_36 != 0
    };
    if color_depth == 10 && !supports_10bpc {
        return -EINVAL;
    }
    if color_depth == 12 && !supports_12bpc {
        return -EINVAL;
    }

    let calculated_hz = drm_hdmi_compute_mode_clock(mode, color_depth as u32, drm_format);
    if calculated_hz == 0 {
        return -EINVAL;
    }
    let chosen_hz = if (*conn_state).hdmi.tmds_char_rate != 0 {
        (*conn_state).hdmi.tmds_char_rate
    } else {
        calculated_hz
    };
    let tmds_clock = ((chosen_hz + 500) / 1000).min(i32::MAX as u64) as i32;
    if tmds_clock < 25_000 || tmds_clock > HDMI_MAX_CLOCK_KHZ as i32 {
        return -EINVAL;
    }
    let info = &(*connector).base.display_info;
    let max_sink_clock = if info.max_tmds_clock != 0 {
        info.max_tmds_clock as i32
    } else if !has_hdmi_sink {
        HDMI_DEFAULT_MAX_CLOCK_DVI_KHZ as i32
    } else {
        HDMI_MAX_CLOCK_KHZ as i32
    };
    if tmds_clock > max_sink_clock {
        return -EINVAL;
    }
    if tmds_clock > HDMI_SCRAMBLING_THRESHOLD_KHZ as i32 && !info.hdmi.scdc.scrambling.supported {
        return -EINVAL;
    }
    if tmds_clock > HDMI_TC_PLL_GAP_START_KHZ as i32 {
        let gap_end = if (*encoder).port >= PORT_TC1 {
            HDMI_TC_PLL_GAP_END_KHZ
        } else {
            533_200
        };
        if tmds_clock < gap_end as i32 {
            return -EINVAL;
        }
    }

    (*pipe_config).sink_format = intel_format;
    (*pipe_config).output_format = intel_format;
    (*pipe_config).port_clock = tmds_clock;
    (*pipe_config).pipe_bpp = if (*pipe_config).pipe_bpp <= 0 {
        bpc * 3
    } else {
        (*pipe_config).pipe_bpp.min(bpc * 3)
    };
    (*pipe_config).max_pipe_bpp = (*pipe_config).pipe_bpp;
    (*pipe_config).max_link_bpp_x16 = bpc * 3 * 16;
    (*pipe_config).limited_color_range = intel_hdmi_limited_color_range(pipe_config, conn_state);
    (*pipe_config).hdmi_scrambling = has_hdmi_sink
        && info.hdmi.scdc.scrambling.supported
        && (tmds_clock > HDMI_SCRAMBLING_THRESHOLD_KHZ as i32
            || info.hdmi.scdc.scrambling.low_rates);
    (*pipe_config).hdmi_high_tmds_clock_ratio =
        (*pipe_config).hdmi_scrambling && tmds_clock > HDMI_SCRAMBLING_THRESHOLD_KHZ as i32;
    let force_audio = (*digital).force_audio;
    (*pipe_config).has_audio = has_hdmi_sink
        && force_audio != HDMI_AUDIO_OFF
        && (force_audio == HDMI_AUDIO_ON || (*connector).base.display_info.has_audio);
    compute_infoframes(pipe_config, connector, conn_state);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_handle_sink_scrambling(
    encoder: *mut IntelEncoder,
    drm_connector: *mut DrmConnector,
    high_tmds_clock_ratio: bool,
    scrambling: bool,
) -> bool {
    if encoder.is_null() || drm_connector.is_null() {
        return false;
    }
    let supported = (*drm_connector).display_info.hdmi.scdc.scrambling.supported;
    if !supported {
        return true;
    }
    drm_scdc_set_high_tmds_clock_ratio(drm_connector, high_tmds_clock_ratio)
        && drm_scdc_set_scrambling(drm_connector, scrambling)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_poll_for_scrambling_enable(
    crtc_state: *const IntelCrtcState,
    connector: *mut DrmConnector,
) {
    if crtc_state.is_null() || connector.is_null() || !(*crtc_state).hdmi_scrambling {
        return;
    }
    let deadline = tk_linux_kapi::clock::now_ns().saturating_add(200_000_000);
    while tk_linux_kapi::clock::now_ns() < deadline {
        if drm_scdc_get_scrambling_status(connector) {
            return;
        }
        tk_linux_kapi::wait::msleep(1);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_encoder_shutdown(encoder: *mut IntelEncoder) {
    let connector = connector_from_encoder(encoder);
    if connector.is_null() {
        return;
    }
    let _ = intel_hdmi_handle_sink_scrambling(encoder, &mut (*connector).base, false, false);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_fastset_infoframes(
    encoder: *mut IntelEncoder,
    crtc_state: *const IntelCrtcState,
    _conn_state: *const DrmConnectorState,
) {
    if encoder.is_null() || crtc_state.is_null() {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let ctl = HSW_TVIDEO_DIP_CTL(display, (*crtc_state).cpu_transcoder as u32);
    let mut ctl_val = intel_de_read(display, ctl);
    if (*crtc_state).infoframes.enable & intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_DRM) == 0
        && ctl_val & VIDEO_DIP_ENABLE_DRM_GLK == 0
    {
        return;
    }
    ctl_val &= !VIDEO_DIP_ENABLE_DRM_GLK;
    intel_de_write(display, ctl, ctl_val);
    intel_de_posting_read(display, ctl);
    intel_write_infoframe(
        encoder,
        crtc_state.cast_mut(),
        HDMI_INFOFRAME_TYPE_DRM,
        ptr::addr_of!((*crtc_state).infoframes.drm),
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_read_gcp_infoframe(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
) {
    if encoder.is_null() || crtc_state.is_null() {
        return;
    }
    if (*crtc_state).infoframes.enable & intel_hdmi_infoframe_enable(HDMI_GCP_PACKET_TYPE) == 0 {
        return;
    }
    let display = display_from_encoder(encoder);
    if display.is_null() {
        return;
    }
    let reg = HSW_TVIDEO_DIP_GCP(display, (*crtc_state).cpu_transcoder as u32);
    (*crtc_state).infoframes.gcp = intel_de_read(display, reg);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_read_infoframe(
    encoder: *mut IntelEncoder,
    crtc_state: *const IntelCrtcState,
    info_type: u32,
    frame: *mut HdmiInfoframe,
) {
    if encoder.is_null() || crtc_state.is_null() || frame.is_null() {
        return;
    }
    let port = encoder.cast::<IntelDigitalPort>();
    if (*crtc_state).infoframes.enable & intel_hdmi_infoframe_enable(info_type) == 0 {
        return;
    }
    let Some(read) = (*port).read_infoframe else {
        return;
    };
    let mut buffer = [0u8; VIDEO_DIP_DATA_SIZE as usize];
    read(
        encoder,
        crtc_state.cast_mut(),
        info_type,
        buffer.as_mut_ptr().cast::<c_void>(),
        buffer.len() as isize,
    );
    if !dip_remove_hole(&mut buffer) {
        return;
    }
    ptr::write_bytes(frame.cast::<u8>(), 0, size_of::<HdmiInfoframe>());
    let ret = tk_drm_upstream::hdmi_upstream::hdmi_infoframe_unpack(
        frame,
        buffer.as_ptr().add(1).cast::<c_void>(),
        buffer.len() - 1,
    );
    if ret != 0 || ptr::read_unaligned(frame.cast::<u32>()) != info_type {
        ptr::write_bytes(frame.cast::<u8>(), 0, size_of::<HdmiInfoframe>());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hdmi_init_connector(
    dig_port: *mut IntelDigitalPort,
    intel_conn: *mut IntelConnector,
) -> bool {
    if dig_port.is_null() || intel_conn.is_null() {
        return false;
    }
    let encoder = ptr::addr_of_mut!((*dig_port).base);
    if (*dig_port).max_lanes < 4 || (*encoder).base.dev.is_null() {
        return false;
    }
    let display = __drm_to_display((*encoder).base.dev);
    if display.is_null()
        || (*display).info.__runtime_info.ip.ver != 13
        || (*display).pch_type != crate::intel_pch_types_upstream::PCH_ADP
    {
        return false;
    }
    let Some(pin) = ddc_pin_for_port((*encoder).port) else {
        return false;
    };
    if !intel_gmbus_is_valid_pin(display, pin) {
        return false;
    }
    let ddc = intel_gmbus_get_adapter(display, pin);
    if ddc.is_null() {
        return false;
    }
    let connector = ptr::addr_of_mut!((*intel_conn).base);
    (*connector).max_bpc = 12;
    let dev = (*encoder).base.dev;
    let ret = tk_drm_upstream::drm_connector_upstream::drm_connector_init_with_ddc(
        dev,
        connector,
        &HDMI_CONNECTOR_FUNCS,
        DRM_MODE_CONNECTOR_HDMIA as i32,
        ddc,
    );
    if ret != 0 {
        return false;
    }
    if (*connector).state.is_null() {
        tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(connector);
        return false;
    }
    (*connector).helper_private = ptr::addr_of!(HDMI_CONNECTOR_HELPERS).cast_mut();
    (*connector).stereo_allowed = true;
    (*connector).ycbcr_420_allowed = true;
    (*intel_conn).polled = 1 << 2; // DRM_CONNECTOR_POLL_HPD
    (*connector).polled = (*intel_conn).polled;
    if tk_drm_upstream::drm_connector_upstream::drm_connector_attach_encoder(
        connector,
        &mut (*encoder).base,
    ) != 0
    {
        tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(connector);
        return false;
    }
    let ret = tk_drm_upstream::drm_connector_upstream::drm_connector_attach_max_bpc_property(
        connector, 8, 12,
    );
    if ret != 0 {
        tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(connector);
        return false;
    }
    let ret = tk_drm_upstream::drm_connector_upstream::drm_connector_attach_broadcast_rgb_property(
        connector,
    );
    if ret != 0 {
        tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(connector);
        return false;
    }
    let ret = tk_drm_upstream::drm_connector_upstream::drm_connector_attach_content_type_property(
        connector,
    );
    if ret != 0 {
        tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(connector);
        return false;
    }
    let ret = attach_force_audio_property(connector, display);
    if ret != 0 {
        tk_drm_upstream::drm_connector_upstream::drm_connector_cleanup(connector);
        return false;
    }
    if (*display).info.__runtime_info.ip.ver >= 10 {
        tk_drm_upstream::drm_connector_upstream::drm_connector_attach_hdr_output_metadata_property(
            connector,
        );
    }
    (*connector).display_info.color_formats |= 1;
    (*intel_conn).encoder = encoder;
    (*dig_port).hdmi.attached_connector = intel_conn;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adlp_ddc_pin_map_distinguishes_combo_and_type_c() {
        assert_eq!(ddc_pin_for_port(PORT_A), Some(1));
        assert_eq!(ddc_pin_for_port(PORT_B), Some(2));
        assert_eq!(ddc_pin_for_port(PORT_C), Some(3));
        assert_eq!(ddc_pin_for_port(PORT_TC1), Some(9));
        assert_eq!(ddc_pin_for_port(PORT_TC6), Some(14));
        assert_eq!(ddc_pin_for_port(-1), None);
        assert_eq!(ddc_pin_for_port(9), None);
    }

    #[test]
    fn tmds_clock_applies_deep_color_and_y420_scaling() {
        unsafe {
            assert_eq!(
                intel_hdmi_tmds_clock(148_500, 8, INTEL_OUTPUT_FORMAT_RGB),
                148_500
            );
            assert_eq!(
                intel_hdmi_tmds_clock(148_500, 10, INTEL_OUTPUT_FORMAT_RGB),
                185_625
            );
            assert_eq!(
                intel_hdmi_tmds_clock(148_500, 12, INTEL_OUTPUT_FORMAT_YCBCR420),
                111_375
            );
        }
    }

    #[test]
    fn dip_frame_hole_round_trips_and_enable_masks_are_separate() {
        let mut buffer = [0u8; 8];
        buffer[1..4].copy_from_slice(&[HDMI_INFOFRAME_TYPE_AVI as u8, 2, 13]);
        assert_eq!(dip_insert_hole(&mut buffer, 3), Some(4));
        assert_eq!(&buffer[..4], &[HDMI_INFOFRAME_TYPE_AVI as u8, 2, 13, 0]);
        assert!(dip_remove_hole(&mut buffer));
        assert_eq!(&buffer[1..4], &[HDMI_INFOFRAME_TYPE_AVI as u8, 2, 13]);

        unsafe {
            assert_eq!(intel_hdmi_infoframe_enable(HDMI_GCP_PACKET_TYPE), 1 << 0);
            assert_eq!(intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_AVI), 1 << 4);
            assert_eq!(
                hsw_infoframe_enable(HDMI_INFOFRAME_TYPE_AVI),
                VIDEO_DIP_ENABLE_AVI_HSW
            );
            assert_ne!(
                intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_AVI),
                hsw_infoframe_enable(HDMI_INFOFRAME_TYPE_AVI)
            );
        }
    }
}

/// `intel_dp_dual_mode_set_tmds_output()`: enable/disable TMDS output on a
/// DP dual-mode (DP++) adaptor.
// upstream: intel_hdmi.c intel_dp_dual_mode_set_tmds_output()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_dual_mode_set_tmds_output(
    hdmi: *mut crate::intel_display_types_upstream::IntelHdmi,
    enable: bool,
) {
    use tk_drm_upstream::drm_dp_dual_mode_helper_upstream::{
        DRM_DP_DUAL_MODE_TYPE2_DVI, drm_dp_dual_mode_set_tmds_output,
    };
    unsafe {
        let connector = (*hdmi).attached_connector;
        let dev = (*connector).base.dev;
        let dual_mode_type = (*hdmi).dp_dual_mode.r#type;
        if dual_mode_type < DRM_DP_DUAL_MODE_TYPE2_DVI {
            return;
        }
        let ddc = (*connector).base.ddc;
        drm_dp_dual_mode_set_tmds_output(dev, dual_mode_type, ddc.cast(), enable);
    }
}
