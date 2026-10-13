// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/display/drm_hdmi_helper.c` (batch B2e).
//!
//! Provides HDMI helper functions for HDR metadata extraction, AVI infoframe
//! colorimetry/bars/content type calculation, TMDS character rate computation,
//! and Audio Clock Regeneration (ACR) N and CTS lookup/calculation.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

use tk_linux_kapi::config::EINVAL;
use tk_linux_kapi::primitives::div_round_closest_ull;

use crate::drm_connector_types::{
    DrmConnector, DrmConnectorState, DrmOutputColorFormat, DRM_OUTPUT_COLOR_FORMAT_RGB444,
    DRM_OUTPUT_COLOR_FORMAT_YCBCR420, DRM_OUTPUT_COLOR_FORMAT_YCBCR422,
};
use crate::drm_connector_types::{
    DRM_MODE_COLORIMETRY_BT2020_CYCC, DRM_MODE_COLORIMETRY_BT2020_RGB,
    DRM_MODE_COLORIMETRY_BT2020_YCC, DRM_MODE_COLORIMETRY_BT709_YCC,
    DRM_MODE_COLORIMETRY_NO_DATA, DRM_MODE_COLORIMETRY_OPRGB, DRM_MODE_COLORIMETRY_OPYCC_601,
    DRM_MODE_COLORIMETRY_SMPTE_170M_YCC, DRM_MODE_COLORIMETRY_SYCC_601,
    DRM_MODE_COLORIMETRY_XVYCC_601, DRM_MODE_COLORIMETRY_XVYCC_709,
};
use crate::drm_edid_upstream::drm_match_cea_mode;
use crate::drm_modes_types::DrmDisplayMode;
use crate::hdmi_types::*;
use crate::hdmi_upstream::hdmi_drm_infoframe_init;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::HdrOutputMetadata;

macro_rules! drm_dbg_kms {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {
        tk_linux_kapi::drm_dbg!($device, $format $(, $argument)*)
    };
}

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c is_eotf_supported()
#[inline]
fn is_eotf_supported(output_eotf: u8, sink_eotf: u8) -> bool {
    (sink_eotf & (1 << output_eotf)) != 0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c drm_hdmi_infoframe_set_hdr_metadata()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_hdmi_infoframe_set_hdr_metadata(
    frame: *mut HdmiDrmInfoframe,
    conn_state: *const DrmConnectorState,
) -> i32 {
    if frame.is_null() || conn_state.is_null() {
        return -EINVAL;
    }

    let conn_state_ref = unsafe { &*conn_state };
    let connector = conn_state_ref.connector;
    if connector.is_null() || conn_state_ref.hdr_output_metadata.is_null() {
        return -EINVAL;
    }

    let blob = unsafe { &*conn_state_ref.hdr_output_metadata };
    let hdr_metadata = blob.data as *const HdrOutputMetadata;
    if hdr_metadata.is_null() {
        return -EINVAL;
    }

    let hdr_meta_ref = unsafe { &*hdr_metadata };
    let type1 = unsafe { &hdr_meta_ref._anon_4.hdmi_metadata_type1 };
    let sink_type1 = unsafe { &(*connector).display_info.hdr_sink_metadata._anon_4.hdmi_type1 };

    if !is_eotf_supported(type1.eotf, sink_type1.eotf) {
        unsafe {
            drm_dbg_kms!(
                (*connector).dev as *const c_void,
                "Unknown EOTF {}\n",
                type1.eotf
            );
        }
    }

    let err = unsafe { hdmi_drm_infoframe_init(frame) };
    if err < 0 {
        return err;
    }

    let frame_ref = unsafe { &mut *frame };
    frame_ref.eotf = type1.eotf as u32;
    frame_ref.metadata_type = type1.metadata_type as u32;

    for i in 0..3 {
        frame_ref.display_primaries[i].x = type1.display_primaries[i].x;
        frame_ref.display_primaries[i].y = type1.display_primaries[i].y;
    }

    frame_ref.white_point.x = type1.white_point.x;
    frame_ref.white_point.y = type1.white_point.y;

    frame_ref.max_display_mastering_luminance = type1.max_display_mastering_luminance;
    frame_ref.min_display_mastering_luminance = type1.min_display_mastering_luminance;
    frame_ref.max_fall = type1.max_fall;
    frame_ref.max_cll = type1.max_cll;

    0
}

// HDMI Colorspace Spec Definitions
const FULL_COLORIMETRY_MASK: u32 = 0x1ff;
const NORMAL_COLORIMETRY_MASK: u32 = 0x3;
const EXTENDED_COLORIMETRY_MASK: u32 = 0x7;

const fn C(x: u32) -> u32 {
    x
}
const fn EC(x: u32) -> u32 {
    x << 2
}
const fn ACE(x: u32) -> u32 {
    x << 5
}

const HDMI_COLORIMETRY_NO_DATA: u32 = 0x0;
const HDMI_COLORIMETRY_SMPTE_170M_YCC: u32 = C(1) | EC(0) | ACE(0);
const HDMI_COLORIMETRY_BT709_YCC: u32 = C(2) | EC(0) | ACE(0);
const HDMI_COLORIMETRY_XVYCC_601: u32 = C(3) | EC(0) | ACE(0);
const HDMI_COLORIMETRY_XVYCC_709: u32 = C(3) | EC(1) | ACE(0);
const HDMI_COLORIMETRY_SYCC_601: u32 = C(3) | EC(2) | ACE(0);
const HDMI_COLORIMETRY_OPYCC_601: u32 = C(3) | EC(3) | ACE(0);
const HDMI_COLORIMETRY_OPRGB: u32 = C(3) | EC(4) | ACE(0);
const HDMI_COLORIMETRY_BT2020_CYCC: u32 = C(3) | EC(5) | ACE(0);
const HDMI_COLORIMETRY_BT2020_RGB: u32 = C(3) | EC(6) | ACE(0);
const HDMI_COLORIMETRY_BT2020_YCC: u32 = C(3) | EC(6) | ACE(0);

const HDMI_COLORIMETRY_VAL: [u32; 11] = [
    HDMI_COLORIMETRY_NO_DATA,         // DRM_MODE_COLORIMETRY_NO_DATA (0)
    HDMI_COLORIMETRY_SMPTE_170M_YCC,  // DRM_MODE_COLORIMETRY_SMPTE_170M_YCC (1)
    HDMI_COLORIMETRY_BT709_YCC,       // DRM_MODE_COLORIMETRY_BT709_YCC (2)
    HDMI_COLORIMETRY_XVYCC_601,       // DRM_MODE_COLORIMETRY_XVYCC_601 (3)
    HDMI_COLORIMETRY_XVYCC_709,       // DRM_MODE_COLORIMETRY_XVYCC_709 (4)
    HDMI_COLORIMETRY_SYCC_601,        // DRM_MODE_COLORIMETRY_SYCC_601 (5)
    HDMI_COLORIMETRY_OPYCC_601,       // DRM_MODE_COLORIMETRY_OPYCC_601 (6)
    HDMI_COLORIMETRY_OPRGB,           // DRM_MODE_COLORIMETRY_OPRGB (7)
    HDMI_COLORIMETRY_BT2020_CYCC,     // DRM_MODE_COLORIMETRY_BT2020_CYCC (8)
    HDMI_COLORIMETRY_BT2020_RGB,      // DRM_MODE_COLORIMETRY_BT2020_RGB (9)
    HDMI_COLORIMETRY_BT2020_YCC,      // DRM_MODE_COLORIMETRY_BT2020_YCC (10)
];

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c drm_hdmi_avi_infoframe_colorimetry()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_hdmi_avi_infoframe_colorimetry(
    frame: *mut HdmiAviInfoframe,
    conn_state: *const DrmConnectorState,
) {
    if frame.is_null() || conn_state.is_null() {
        return;
    }
    let conn_state_ref = unsafe { &*conn_state };
    let colorimetry_index = (conn_state_ref.colorspace & FULL_COLORIMETRY_MASK) as usize;

    let colorimetry_val = if colorimetry_index >= HDMI_COLORIMETRY_VAL.len() {
        HDMI_COLORIMETRY_NO_DATA
    } else {
        HDMI_COLORIMETRY_VAL[colorimetry_index]
    };

    let frame_ref = unsafe { &mut *frame };
    frame_ref.colorimetry = colorimetry_val & NORMAL_COLORIMETRY_MASK;
    frame_ref.extended_colorimetry = (colorimetry_val >> 2) & EXTENDED_COLORIMETRY_MASK;
}

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c drm_hdmi_avi_infoframe_bars()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_hdmi_avi_infoframe_bars(
    frame: *mut HdmiAviInfoframe,
    conn_state: *const DrmConnectorState,
) {
    if frame.is_null() || conn_state.is_null() {
        return;
    }
    let conn_state_ref = unsafe { &*conn_state };
    let frame_ref = unsafe { &mut *frame };
    frame_ref.right_bar = conn_state_ref.tv.margins.right as u16;
    frame_ref.left_bar = conn_state_ref.tv.margins.left as u16;
    frame_ref.top_bar = conn_state_ref.tv.margins.top as u16;
    frame_ref.bottom_bar = conn_state_ref.tv.margins.bottom as u16;
}

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c drm_hdmi_avi_infoframe_content_type()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_hdmi_avi_infoframe_content_type(
    frame: *mut HdmiAviInfoframe,
    conn_state: *const DrmConnectorState,
) {
    if frame.is_null() || conn_state.is_null() {
        return;
    }
    let conn_state_ref = unsafe { &*conn_state };
    let frame_ref = unsafe { &mut *frame };

    match conn_state_ref.content_type {
        DRM_MODE_CONTENT_TYPE_GRAPHICS => {
            frame_ref.content_type = HDMI_CONTENT_TYPE_GRAPHICS;
        }
        DRM_MODE_CONTENT_TYPE_CINEMA => {
            frame_ref.content_type = HDMI_CONTENT_TYPE_CINEMA;
        }
        DRM_MODE_CONTENT_TYPE_GAME => {
            frame_ref.content_type = HDMI_CONTENT_TYPE_GAME;
        }
        DRM_MODE_CONTENT_TYPE_PHOTO => {
            frame_ref.content_type = HDMI_CONTENT_TYPE_PHOTO;
        }
        _ => {
            frame_ref.content_type = HDMI_CONTENT_TYPE_GRAPHICS;
        }
    }

    frame_ref.itc = conn_state_ref.content_type != DRM_MODE_CONTENT_TYPE_NO_DATA;
}

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c drm_hdmi_compute_mode_clock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_hdmi_compute_mode_clock(
    mode: *const DrmDisplayMode,
    bpc: u32,
    fmt: DrmOutputColorFormat,
) -> u64 {
    if mode.is_null() {
        return 0;
    }
    let mode_ref = unsafe { &*mode };
    let mut clock: u64 = (mode_ref.clock as u64) * 1000;
    let vic = unsafe { drm_match_cea_mode(mode) };

    if vic == 1 && bpc != 8 {
        return 0;
    }

    let mut eff_bpc = bpc;
    if fmt == DRM_OUTPUT_COLOR_FORMAT_YCBCR422 {
        if bpc > 12 {
            return 0;
        }
        eff_bpc = 8;
    }

    if fmt == DRM_OUTPUT_COLOR_FORMAT_YCBCR420 {
        clock /= 2;
    }

    if (mode_ref.flags & DRM_MODE_FLAG_DBLCLK) != 0 {
        clock *= 2;
    }

    div_round_closest_ull(clock * (eff_bpc as u64), 8)
}

// ============================================================================
// Audio Clock Regeneration (ACR) Table and Helpers
// ============================================================================

struct DrmHdmiAcrNCtsEntry {
    n: u32,
    cts: u32,
}

struct DrmHdmiAcrData {
    tmds_clock_khz: u64,
    n_cts_32k: DrmHdmiAcrNCtsEntry,
    n_cts_44k1: DrmHdmiAcrNCtsEntry,
    n_cts_48k: DrmHdmiAcrNCtsEntry,
}

const HDMI_ACR_N_CTS: [DrmHdmiAcrData; 15] = [
    DrmHdmiAcrData {
        /* "Other" entry */
        tmds_clock_khz: 0,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 0 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 0 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 0 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 25175,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4576, cts: 28125 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 7007, cts: 31250 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6864, cts: 28125 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 25200,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 25200 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 28000 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 25200 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 27000,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 27000 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 30000 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 27000 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 27027,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 27027 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 30030 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 27027 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 54000,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 54000 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 60000 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 54000 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 54054,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 54054 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 60060 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 54054 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 74176,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 11648, cts: 210937 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 17836, cts: 234375 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 11648, cts: 140625 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 74250,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 74250 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 82500 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 74250 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 148352,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 11648, cts: 421875 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 8918, cts: 234375 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 5824, cts: 140625 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 148500,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 4096, cts: 148500 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 6272, cts: 165000 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 148500 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 296703,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 5824, cts: 421875 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 4459, cts: 234375 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 5824, cts: 281250 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 297000,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 3072, cts: 222750 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 4704, cts: 247500 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 5120, cts: 247500 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 593407,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 5824, cts: 843750 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 8918, cts: 937500 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 5824, cts: 562500 },
    },
    DrmHdmiAcrData {
        tmds_clock_khz: 594000,
        n_cts_32k: DrmHdmiAcrNCtsEntry { n: 3072, cts: 445500 },
        n_cts_44k1: DrmHdmiAcrNCtsEntry { n: 9408, cts: 990000 },
        n_cts_48k: DrmHdmiAcrNCtsEntry { n: 6144, cts: 594000 },
    },
];

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c drm_hdmi_acr_find_tmds_entry()
fn drm_hdmi_acr_find_tmds_entry(tmds_clock_khz: u64) -> usize {
    for i in 1..HDMI_ACR_N_CTS.len() {
        if HDMI_ACR_N_CTS[i].tmds_clock_khz == tmds_clock_khz {
            return i;
        }
    }
    0
}

// upstream: drivers/gpu/drm/display/drm_hdmi_helper.c drm_hdmi_acr_get_n_cts()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_hdmi_acr_get_n_cts(
    tmds_char_rate: u64,
    sample_rate: u32,
    out_n: *mut u32,
    out_cts: *mut u32,
) {
    if out_n.is_null() || out_cts.is_null() || sample_rate == 0 {
        return;
    }

    let tmds_clock_khz = div_round_closest_ull(tmds_char_rate, 1000);
    let tmds_idx = drm_hdmi_acr_find_tmds_entry(tmds_clock_khz);

    let (entry, mult) = if sample_rate % 48000 == 0 {
        (&HDMI_ACR_N_CTS[tmds_idx].n_cts_48k, sample_rate / 48000)
    } else if sample_rate % 44100 == 0 {
        (&HDMI_ACR_N_CTS[tmds_idx].n_cts_44k1, sample_rate / 44100)
    } else if sample_rate % 32000 == 0 {
        (&HDMI_ACR_N_CTS[tmds_idx].n_cts_32k, sample_rate / 32000)
    } else {
        return;
    };

    let n = entry.n * mult;
    let mut cts = entry.cts;

    if cts == 0 {
        cts = div_round_closest_ull(tmds_char_rate * (n as u64), (128 * sample_rate) as u64) as u32;
    }

    unsafe {
        *out_n = n;
        *out_cts = cts;
    }
}
