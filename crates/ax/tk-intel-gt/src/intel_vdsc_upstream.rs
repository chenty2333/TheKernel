// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_vdsc.c intel_dsc_dp_pps_write()
// (Linux v7.2.3), with the DSC PPS header/payload helpers it calls from
// drivers/gpu/drm/display/drm_dsc_helper.c (drm_dsc_dp_pps_header_init and
// drm_dsc_pps_payload_pack) and the packed PPS layout of include/drm/display/drm_dsc.h.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::c_void,
    mem::{offset_of, size_of},
};

use crate::{
    i915_external_placeholder_types_upstream::DrmDscConfig,
    intel_display_types_upstream::{IntelCrtcState, IntelDigitalPort, IntelEncoder},
};

/// `DP_SDP_PPS` (drm_dp.h).
const DP_SDP_PPS: u8 = 0x10;
/// `DP_SDP_PPS_HEADER_PAYLOAD_BYTES_MINUS_1` (drm_dp.h).
const DP_SDP_PPS_HEADER_PAYLOAD_BYTES_MINUS_1: u8 = 0x7F;

const DSC_NUM_BUF_RANGES: usize = 15;
const DSC_RC_EDGE_FACTOR_CONST: u8 = 6;
const DSC_RC_TGT_OFFSET_HI_CONST: u8 = 3;
const DSC_RC_TGT_OFFSET_LO_CONST: u8 = 3;
const DSC_PPS_VERSION_MAJOR_SHIFT: u32 = 4;
const DSC_PPS_BPC_SHIFT: u32 = 4;
const DSC_PPS_MSB_SHIFT: u32 = 8;
const DSC_PPS_LSB_MASK: u32 = 0xFF << 0;
const DSC_PPS_BPP_HIGH_MASK: u32 = 0x3 << 8;
const DSC_PPS_VBR_EN_SHIFT: u32 = 2;
const DSC_PPS_SIMPLE422_SHIFT: u32 = 3;
const DSC_PPS_CONVERT_RGB_SHIFT: u32 = 4;
const DSC_PPS_BLOCK_PRED_EN_SHIFT: u32 = 5;
const DSC_PPS_INIT_XMIT_DELAY_HIGH_MASK: u32 = 0x3 << 8;
const DSC_PPS_SCALE_DEC_INT_HIGH_MASK: u32 = 0xF << 8;
const DSC_PPS_RC_TGT_OFFSET_HI_SHIFT: u32 = 4;
const DSC_PPS_RC_RANGE_MINQP_SHIFT: u32 = 11;
const DSC_PPS_RC_RANGE_MAXQP_SHIFT: u32 = 6;
const DSC_PPS_NATIVE_420_SHIFT: u32 = 1;

/// `struct dp_sdp_header` (drm_dp.h).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct DpSdpHeader {
    pub HB0: u8,
    pub HB1: u8,
    pub HB2: u8,
    pub HB3: u8,
}

/// `struct drm_dsc_picture_parameter_set` (drm_dsc.h), 128 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct DrmDscPictureParameterSet {
    pub dsc_version: u8,
    pub pps_identifier: u8,
    pub pps_reserved: u8,
    pub pps_3: u8,
    pub pps_4: u8,
    pub bits_per_pixel_low: u8,
    pub pic_height: u16,
    pub pic_width: u16,
    pub slice_height: u16,
    pub slice_width: u16,
    pub chunk_size: u16,
    pub initial_xmit_delay_high: u8,
    pub initial_xmit_delay_low: u8,
    pub initial_dec_delay: u16,
    pub pps20_reserved: u8,
    pub initial_scale_value: u8,
    pub scale_increment_interval: u16,
    pub scale_decrement_interval_high: u8,
    pub scale_decrement_interval_low: u8,
    pub pps26_reserved: u8,
    pub first_line_bpg_offset: u8,
    pub nfl_bpg_offset: u16,
    pub slice_bpg_offset: u16,
    pub initial_offset: u16,
    pub final_offset: u16,
    pub flatness_min_qp: u8,
    pub flatness_max_qp: u8,
    pub rc_model_size: u16,
    pub rc_edge_factor: u8,
    pub rc_quant_incr_limit0: u8,
    pub rc_quant_incr_limit1: u8,
    pub rc_tgt_offset: u8,
    pub rc_buf_thresh: [u8; DSC_NUM_BUF_RANGES - 1],
    pub rc_range_parameters: [u16; DSC_NUM_BUF_RANGES],
    pub native_422_420: u8,
    pub second_line_bpg_offset: u8,
    pub nsl_bpg_offset: u16,
    pub second_line_offset_adj: u16,
    pub pps_long_94_reserved: u32,
    pub pps_long_98_reserved: u32,
    pub pps_long_102_reserved: u32,
    pub pps_long_106_reserved: u32,
    pub pps_long_110_reserved: u32,
    pub pps_long_114_reserved: u32,
    pub pps_long_118_reserved: u32,
    pub pps_long_122_reserved: u32,
    pub pps_short_126_reserved: u16,
}

/// `struct drm_dsc_pps_infoframe` (drm_dsc.h).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct DrmDscPpsInfoframe {
    pub pps_header: DpSdpHeader,
    pub pps_payload: DrmDscPictureParameterSet,
}

const _: () = assert!(size_of::<DrmDscPictureParameterSet>() == 128);
const _: () = assert!(size_of::<DrmDscPpsInfoframe>() == 132);

/// `drm_dsc_dp_pps_header_init()`.
// upstream: drm_dsc_helper.c drm_dsc_dp_pps_header_init()
fn drm_dsc_dp_pps_header_init(pps_header: &mut DpSdpHeader) {
    *pps_header = DpSdpHeader {
        HB0: 0,
        HB1: DP_SDP_PPS,
        HB2: DP_SDP_PPS_HEADER_PAYLOAD_BYTES_MINUS_1,
        HB3: 0,
    };
}

/// `drm_dsc_pps_payload_pack()`.
// upstream: drm_dsc_helper.c drm_dsc_pps_payload_pack()
fn drm_dsc_pps_payload_pack(pps_payload: &mut DrmDscPictureParameterSet, dsc_cfg: &DrmDscConfig) {
    *pps_payload = unsafe { core::mem::zeroed() };
    let be16 = |v: u16| v.to_be();

    let bpp = dsc_cfg.bits_per_pixel as u32;
    pps_payload.dsc_version =
        dsc_cfg.dsc_version_minor | (dsc_cfg.dsc_version_major << DSC_PPS_VERSION_MAJOR_SHIFT);
    pps_payload.pps_3 = dsc_cfg.line_buf_depth | (dsc_cfg.bits_per_component << DSC_PPS_BPC_SHIFT);
    pps_payload.pps_4 = (((bpp & DSC_PPS_BPP_HIGH_MASK) >> DSC_PPS_MSB_SHIFT)
        | ((dsc_cfg.vbr_enable as u32) << DSC_PPS_VBR_EN_SHIFT)
        | ((dsc_cfg.simple_422 as u32) << DSC_PPS_SIMPLE422_SHIFT)
        | ((dsc_cfg.convert_rgb as u32) << DSC_PPS_CONVERT_RGB_SHIFT)
        | ((dsc_cfg.block_pred_enable as u32) << DSC_PPS_BLOCK_PRED_EN_SHIFT))
        as u8;
    pps_payload.bits_per_pixel_low = (bpp & DSC_PPS_LSB_MASK) as u8;
    pps_payload.pic_height = be16(dsc_cfg.pic_height);
    pps_payload.pic_width = be16(dsc_cfg.pic_width);
    pps_payload.slice_height = be16(dsc_cfg.slice_height);
    pps_payload.slice_width = be16(dsc_cfg.slice_width);
    pps_payload.chunk_size = be16(dsc_cfg.slice_chunk_size);

    let xmit = dsc_cfg.initial_xmit_delay as u32;
    pps_payload.initial_xmit_delay_high =
        ((xmit & DSC_PPS_INIT_XMIT_DELAY_HIGH_MASK) >> DSC_PPS_MSB_SHIFT) as u8;
    pps_payload.initial_xmit_delay_low = (xmit & DSC_PPS_LSB_MASK) as u8;
    pps_payload.initial_dec_delay = be16(dsc_cfg.initial_dec_delay);
    pps_payload.initial_scale_value = dsc_cfg.initial_scale_value;

    pps_payload.scale_increment_interval = be16(dsc_cfg.scale_increment_interval);
    let sdi = dsc_cfg.scale_decrement_interval as u32;
    pps_payload.scale_decrement_interval_high =
        ((sdi & DSC_PPS_SCALE_DEC_INT_HIGH_MASK) >> DSC_PPS_MSB_SHIFT) as u8;
    pps_payload.scale_decrement_interval_low = (sdi & DSC_PPS_LSB_MASK) as u8;

    pps_payload.first_line_bpg_offset = dsc_cfg.first_line_bpg_offset;
    pps_payload.nfl_bpg_offset = be16(dsc_cfg.nfl_bpg_offset);
    pps_payload.slice_bpg_offset = be16(dsc_cfg.slice_bpg_offset);
    pps_payload.initial_offset = be16(dsc_cfg.initial_offset);
    pps_payload.final_offset = be16(dsc_cfg.final_offset);
    pps_payload.flatness_min_qp = dsc_cfg.flatness_min_qp;
    pps_payload.flatness_max_qp = dsc_cfg.flatness_max_qp;
    pps_payload.rc_model_size = be16(dsc_cfg.rc_model_size);
    pps_payload.rc_edge_factor = DSC_RC_EDGE_FACTOR_CONST;
    pps_payload.rc_quant_incr_limit0 = dsc_cfg.rc_quant_incr_limit0;
    pps_payload.rc_quant_incr_limit1 = dsc_cfg.rc_quant_incr_limit1;
    pps_payload.rc_tgt_offset =
        DSC_RC_TGT_OFFSET_LO_CONST | (DSC_RC_TGT_OFFSET_HI_CONST << DSC_PPS_RC_TGT_OFFSET_HI_SHIFT);

    for i in 0..DSC_NUM_BUF_RANGES - 1 {
        pps_payload.rc_buf_thresh[i] = dsc_cfg.rc_buf_thresh[i] as u8;
    }
    for i in 0..DSC_NUM_BUF_RANGES {
        let range = &dsc_cfg.rc_range_params[i];
        pps_payload.rc_range_parameters[i] = be16(
            ((range.range_min_qp as u32) << DSC_PPS_RC_RANGE_MINQP_SHIFT
                | (range.range_max_qp as u32) << DSC_PPS_RC_RANGE_MAXQP_SHIFT
                | range.range_bpg_offset as u32) as u16,
        );
    }

    pps_payload.native_422_420 = (dsc_cfg.native_422 as u32
        | ((dsc_cfg.native_420 as u32) << DSC_PPS_NATIVE_420_SHIFT))
        as u8;
    pps_payload.second_line_bpg_offset = dsc_cfg.second_line_bpg_offset;
    pps_payload.nsl_bpg_offset = be16(dsc_cfg.nsl_bpg_offset);
    pps_payload.second_line_offset_adj = be16(dsc_cfg.second_line_offset_adj);
}

/// `intel_dsc_dp_pps_write()`: send the DSC PPS SDP through the port's
/// infoframe writer.
// upstream: intel_vdsc.c intel_dsc_dp_pps_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dsc_dp_pps_write(
    encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
) {
    unsafe {
        let dig_port = encoder
            .cast::<u8>()
            .sub(offset_of!(IntelDigitalPort, base))
            .cast::<IntelDigitalPort>();
        let vdsc_cfg = &(*crtc_state).dsc.config;

        if !(*crtc_state).dsc.compression_enable {
            return;
        }

        let mut dp_dsc_pps_sdp = core::mem::zeroed::<DrmDscPpsInfoframe>();
        let mut header = dp_dsc_pps_sdp.pps_header;
        drm_dsc_dp_pps_header_init(&mut header);
        dp_dsc_pps_sdp.pps_header = header;
        let mut payload = dp_dsc_pps_sdp.pps_payload;
        drm_dsc_pps_payload_pack(&mut payload, vdsc_cfg);
        dp_dsc_pps_sdp.pps_payload = payload;

        let write = (*dig_port).write_infoframe.expect("write_infoframe");
        write(
            encoder,
            crtc_state,
            DP_SDP_PPS as u32,
            (&mut dp_dsc_pps_sdp as *mut DrmDscPpsInfoframe).cast::<c_void>(),
            size_of::<DrmDscPpsInfoframe>() as isize,
        );
    }
}
