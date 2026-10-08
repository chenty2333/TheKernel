// Copyright 2006 Dave Airlie <airlied@linux.ie>
// Copyright © 2006-2009 Intel Corporation
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice (including the next
// paragraph) shall be included in all copies or substantial portions of the
// Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.
//
// Authors:
// Eric Anholt <eric@anholt.net>
// Jesse Barnes <jesse.barnes@intel.com>
// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_hdmi.c.
//! Source-shaped HDMI policy, infoframe, DDC, HDCP and DSC helpers.
//!
//! MMIO, DDC/I2C, delays and DRM framework operations are narrow trait hooks.
//! This module owns the source decisions and the order in which those hooks
//! are called; callers own device lifetime, locking and power references.

#![allow(non_snake_case)]
extern crate alloc;

pub const HDMI_PACKET_TYPE_GENERAL_CONTROL: u8 = 0x03;
pub const HDMI_PACKET_TYPE_GAMUT_METADATA: u8 = 0x0a;
pub const HDMI_INFOFRAME_TYPE_VENDOR: u8 = 0x81;
pub const HDMI_INFOFRAME_TYPE_AVI: u8 = 0x82;
pub const HDMI_INFOFRAME_TYPE_SPD: u8 = 0x83;
pub const HDMI_INFOFRAME_TYPE_DRM: u8 = 0x87;
pub const DP_SDP_VSC: u8 = 0x07;
pub const DP_SDP_ADAPTIVE_SYNC: u8 = 0x22;
pub const DP_SDP_PPS: u8 = 0x10;
pub const VIDEO_DIP_DATA_SIZE: usize = 32;
pub const VIDEO_DIP_ASYNC_DATA_SIZE: usize = 36;
pub const VIDEO_DIP_GMP_DATA_SIZE: usize = 36;
pub const VIDEO_DIP_VSC_DATA_SIZE: usize = 36;
pub const VIDEO_DIP_PPS_DATA_SIZE: usize = 132;

pub const VIDEO_DIP_ENABLE: u32 = 1 << 31;
pub const VIDEO_DIP_PORT_MASK: u32 = 3 << 29;
pub const VIDEO_DIP_ENABLE_GCP: u32 = 1 << 25;
pub const VIDEO_DIP_ENABLE_AVI: u32 = 1 << 21;
pub const VIDEO_DIP_ENABLE_VENDOR: u32 = 2 << 21;
pub const VIDEO_DIP_ENABLE_GAMUT: u32 = 4 << 21;
pub const VIDEO_DIP_ENABLE_SPD: u32 = 8 << 21;
pub const VIDEO_DIP_SELECT_MASK: u32 = 3 << 19;
pub const VIDEO_DIP_SELECT_AVI: u32 = 0 << 19;
pub const VIDEO_DIP_SELECT_VENDOR: u32 = 1 << 19;
pub const VIDEO_DIP_SELECT_GAMUT: u32 = 2 << 19;
pub const VIDEO_DIP_SELECT_SPD: u32 = 3 << 19;
pub const VIDEO_DIP_FREQ_MASK: u32 = 3 << 16;
pub const VIDEO_DIP_FREQ_VSYNC: u32 = 1 << 16;
pub const VIDEO_DIP_ENABLE_DRM_GLK: u32 = 1 << 28;
pub const VDIP_ENABLE_PPS: u32 = 1 << 24;
pub const VIDEO_DIP_ENABLE_VSC_HSW: u32 = 1 << 20;
pub const VIDEO_DIP_ENABLE_GCP_HSW: u32 = 1 << 16;
pub const VIDEO_DIP_ENABLE_AVI_HSW: u32 = 1 << 12;
pub const VIDEO_DIP_ENABLE_VS_HSW: u32 = 1 << 8;
pub const VIDEO_DIP_ENABLE_GMP_HSW: u32 = 1 << 4;
pub const VIDEO_DIP_ENABLE_SPD_HSW: u32 = 1;
pub const VIDEO_DIP_ENABLE_AS_ADL: u32 = 1 << 23;
pub const VSC_DIP_HW_DATA_SW_HEA: u32 = 2 << 25;
pub const GCP_COLOR_INDICATION: u32 = 1 << 2;
pub const GCP_DEFAULT_PHASE_ENABLE: u32 = 1 << 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InfoframeFamily {
    G4x,
    IbexPeak,
    CougarPoint,
    Valleyview,
    Haswell,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InfoframeRegister {
    Control {
        family: InfoframeFamily,
        pipe: u8,
        transcoder: u8,
    },
    Data {
        family: InfoframeFamily,
        pipe: u8,
        transcoder: u8,
        packet_type: u8,
        dword: u16,
    },
    Gcp {
        family: InfoframeFamily,
        pipe: u8,
        transcoder: u8,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HdmiModeStatus {
    Ok,
    ClockLow,
    ClockHigh,
    ClockRange,
    No420,
    Bad,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Rgb,
    Ycbcr420,
    Ycbcr444,
    Ycbcr422,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HdmiPortClass {
    Combo,
    TypeC,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiMode {
    pub clock_khz: i32,
    pub hdisplay: i32,
    pub htotal: i32,
    pub hblank_start: i32,
    pub hblank_end: i32,
    pub hsync_start: i32,
    pub hsync_end: i32,
    pub flags: u32,
}
pub const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
pub const DRM_MODE_FLAG_DBLCLK: u32 = 1 << 12;
pub const DRM_MODE_FLAG_3D_MASK: u32 = 0x1f << 14;
pub const DRM_MODE_FLAG_3D_FRAME_PACKING: u32 = 1 << 14;

/// Backend owns raw MMIO address mapping, access ordering and diagnostics.
pub trait HdmiIo {
    fn register(&self, register: InfoframeRegister) -> u32;
    fn read32(&mut self, register: u32) -> u32;
    fn write32(&mut self, register: u32, value: u32);
    fn hdmi_port_enabled(&mut self) -> bool;
    fn transcoder_function_enabled(&mut self, transcoder: u8) -> bool;
    fn posting_read(&mut self, register: u32) {
        let _ = self.read32(register);
    }
    fn warning(&mut self, _condition: bool, _message: &'static str) {}
    fn debug(&mut self, _message: &'static str) {}
    fn pack_infoframe(
        &mut self,
        packet_type: u8,
        frame: &[u8],
        out: &mut [u8],
    ) -> Result<usize, ()>;
    fn write_infoframe(&mut self, packet_type: u8, bytes: &[u8], len: usize);
    fn read_infoframe(&mut self, packet_type: u8, bytes: &mut [u8]);
    fn unpack_infoframe(&mut self, packet_type: u8, packed: &[u8], out: &mut [u8]) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortPlatform {
    G4x,
    Cherryview,
    Broxton,
    Geminilake,
    Haswell,
    Display(u8),
    AlderLakeS,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockLimits {
    pub platform: PortPlatform,
    pub port: HdmiPortClass,
    pub source_limit_khz: i32,
    pub dp_dual_mode_limit_khz: Option<i32>,
    pub sink_limit_khz: Option<i32>,
    pub has_hdmi_sink: bool,
    pub respect_downstream_limits: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SinkCapabilities {
    pub has_hdmi_sink: bool,
    pub ycbcr420_allowed: bool,
    pub mode_is_420: bool,
    pub rgb_10bpc: bool,
    pub rgb_12bpc: bool,
    pub y420_10bpc: bool,
    pub y420_12bpc: bool,
    pub gmch: bool,
    pub display_version: u8,
}

// upstream: intel_hdmi.c intel_hdmi_is_frl()
pub fn intel_hdmi_is_frl(clock: u32, mut clock_matches: impl FnMut(u32, u32) -> bool) -> bool {
    [300_000, 600_000, 800_000, 1_000_000, 1_200_000]
        .into_iter()
        .any(|rate| clock_matches(clock, rate))
}

// upstream: intel_hdmi.c assert_hdmi_port_disabled()
pub fn assert_hdmi_port_disabled(io: &mut impl HdmiIo, port_register: u32, ddi: bool) -> bool {
    let mask = if ddi { 1 << 31 } else { 1 << 31 };
    let enabled = io.read32(port_register) & mask != 0;
    io.warning(enabled, "HDMI port enabled, expecting disabled");
    enabled
}

// upstream: intel_hdmi.c assert_hdmi_transcoder_func_disabled()
pub fn assert_hdmi_transcoder_func_disabled(io: &mut impl HdmiIo, func_register: u32) -> bool {
    let enabled = io.read32(func_register) & (1 << 31) != 0;
    io.warning(
        enabled,
        "HDMI transcoder function enabled, expecting disabled",
    );
    enabled
}

// upstream: intel_hdmi.c g4x_infoframe_index()
pub const fn g4x_infoframe_index(packet_type: u8) -> u32 {
    match packet_type {
        HDMI_PACKET_TYPE_GAMUT_METADATA => VIDEO_DIP_SELECT_GAMUT,
        HDMI_INFOFRAME_TYPE_AVI => VIDEO_DIP_SELECT_AVI,
        HDMI_INFOFRAME_TYPE_SPD => VIDEO_DIP_SELECT_SPD,
        HDMI_INFOFRAME_TYPE_VENDOR => VIDEO_DIP_SELECT_VENDOR,
        _ => 0,
    }
}

// upstream: intel_hdmi.c g4x_infoframe_enable()
pub const fn g4x_infoframe_enable(packet_type: u8) -> u32 {
    match packet_type {
        HDMI_PACKET_TYPE_GENERAL_CONTROL => VIDEO_DIP_ENABLE_GCP,
        HDMI_PACKET_TYPE_GAMUT_METADATA => VIDEO_DIP_ENABLE_GAMUT,
        DP_SDP_VSC | DP_SDP_ADAPTIVE_SYNC | HDMI_INFOFRAME_TYPE_DRM => 0,
        HDMI_INFOFRAME_TYPE_AVI => VIDEO_DIP_ENABLE_AVI,
        HDMI_INFOFRAME_TYPE_SPD => VIDEO_DIP_ENABLE_SPD,
        HDMI_INFOFRAME_TYPE_VENDOR => VIDEO_DIP_ENABLE_VENDOR,
        _ => 0,
    }
}

// upstream: intel_hdmi.c hsw_infoframe_enable()
pub const fn hsw_infoframe_enable(packet_type: u8) -> u32 {
    match packet_type {
        HDMI_PACKET_TYPE_GENERAL_CONTROL => VIDEO_DIP_ENABLE_GCP_HSW,
        HDMI_PACKET_TYPE_GAMUT_METADATA => VIDEO_DIP_ENABLE_GMP_HSW,
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

// upstream: intel_hdmi.c hsw_dip_data_reg()
pub fn hsw_dip_data_reg(
    io: &impl HdmiIo,
    transcoder: u8,
    packet_type: u8,
    dword: u16,
) -> Option<u32> {
    let register = InfoframeRegister::Data {
        family: InfoframeFamily::Haswell,
        pipe: 0,
        transcoder,
        packet_type,
        dword,
    };
    let reg = io.register(register);
    (reg != u32::MAX).then_some(reg)
}

// upstream: intel_hdmi.c hsw_dip_data_size()
pub const fn hsw_dip_data_size(display_version: u8, packet_type: u8) -> usize {
    match packet_type {
        DP_SDP_VSC => VIDEO_DIP_VSC_DATA_SIZE,
        DP_SDP_ADAPTIVE_SYNC => VIDEO_DIP_ASYNC_DATA_SIZE,
        DP_SDP_PPS => VIDEO_DIP_PPS_DATA_SIZE,
        HDMI_PACKET_TYPE_GAMUT_METADATA if display_version >= 11 => VIDEO_DIP_GMP_DATA_SIZE,
        _ => VIDEO_DIP_DATA_SIZE,
    }
}

#[derive(Clone, Copy)]
struct InfoframePath {
    family: InfoframeFamily,
    pipe: u8,
    transcoder: u8,
    packet_type: u8,
}
fn data_register(io: &impl HdmiIo, path: InfoframePath, dword: u16) -> u32 {
    io.register(InfoframeRegister::Data {
        family: path.family,
        pipe: path.pipe,
        transcoder: path.transcoder,
        packet_type: path.packet_type,
        dword,
    })
}
fn write_packet_words(
    io: &mut impl HdmiIo,
    path: InfoframePath,
    bytes: &[u8],
    len: usize,
    full_size: usize,
) {
    let len = len.min(bytes.len());
    let mut i = 0;
    while i < len {
        let mut word = [0u8; 4];
        let n = (len - i).min(4);
        word[..n].copy_from_slice(&bytes[i..i + n]);
        let reg = data_register(io, path, (i / 4) as u16);
        io.write32(reg, u32::from_le_bytes(word));
        i += 4;
    }
    while i < full_size {
        let reg = data_register(io, path, (i / 4) as u16);
        io.write32(reg, 0);
        i += 4;
    }
}
fn read_packet_words(io: &mut impl HdmiIo, path: InfoframePath, bytes: &mut [u8], len: usize) {
    let len = len.min(bytes.len());
    let mut i = 0;
    while i < len {
        let reg = data_register(io, path, (i / 4) as u16);
        let raw = io.read32(reg).to_le_bytes();
        let n = (len - i).min(4);
        bytes[i..i + n].copy_from_slice(&raw[..n]);
        i += 4;
    }
}

// upstream: intel_hdmi.c g4x_write_infoframe()
pub fn g4x_write_infoframe(
    io: &mut impl HdmiIo,
    packet_type: u8,
    _port: u8,
    frame: &[u8],
    len: usize,
) {
    let ctl = io.register(InfoframeRegister::Control {
        family: InfoframeFamily::G4x,
        pipe: 0,
        transcoder: 0,
    });
    let mut val = io.read32(ctl);
    io.warning(
        val & VIDEO_DIP_ENABLE == 0,
        "Writing DIP with CTL reg disabled",
    );
    val &= !(VIDEO_DIP_SELECT_MASK | 0xf);
    val |= g4x_infoframe_index(packet_type);
    val &= !g4x_infoframe_enable(packet_type);
    io.write32(ctl, val);
    write_packet_words(
        io,
        InfoframePath {
            family: InfoframeFamily::G4x,
            pipe: 0,
            transcoder: 0,
            packet_type,
        },
        frame,
        len,
        VIDEO_DIP_DATA_SIZE,
    );
    val |= g4x_infoframe_enable(packet_type);
    val &= !VIDEO_DIP_FREQ_MASK;
    val |= VIDEO_DIP_FREQ_VSYNC;
    io.write32(ctl, val);
    io.posting_read(ctl);
}

// upstream: intel_hdmi.c g4x_read_infoframe()
pub fn g4x_read_infoframe(io: &mut impl HdmiIo, packet_type: u8, frame: &mut [u8], len: usize) {
    let ctl = io.register(InfoframeRegister::Control {
        family: InfoframeFamily::G4x,
        pipe: 0,
        transcoder: 0,
    });
    let mut val = io.read32(ctl);
    val &= !(VIDEO_DIP_SELECT_MASK | 0xf);
    val |= g4x_infoframe_index(packet_type);
    io.write32(ctl, val);
    read_packet_words(
        io,
        InfoframePath {
            family: InfoframeFamily::G4x,
            pipe: 0,
            transcoder: 0,
            packet_type,
        },
        frame,
        len,
    );
}

// upstream: intel_hdmi.c g4x_infoframes_enabled()
pub fn g4x_infoframes_enabled(io: &mut impl HdmiIo, port: u8) -> u32 {
    let ctl = io.register(InfoframeRegister::Control {
        family: InfoframeFamily::G4x,
        pipe: 0,
        transcoder: 0,
    });
    let val = io.read32(ctl);
    if val & VIDEO_DIP_ENABLE == 0 || val & VIDEO_DIP_PORT_MASK != ((port as u32) << 29) {
        return 0;
    }
    val & (VIDEO_DIP_ENABLE_AVI | VIDEO_DIP_ENABLE_VENDOR | VIDEO_DIP_ENABLE_SPD)
}

fn legacy_write_infoframe(
    io: &mut impl HdmiIo,
    family: InfoframeFamily,
    pipe: u8,
    packet_type: u8,
    frame: &[u8],
    len: usize,
    keep_avi_enabled: bool,
) {
    let ctl = io.register(InfoframeRegister::Control {
        family,
        pipe,
        transcoder: 0,
    });
    let mut val = io.read32(ctl);
    io.warning(
        val & VIDEO_DIP_ENABLE == 0,
        "Writing DIP with CTL reg disabled",
    );
    val &= !(VIDEO_DIP_SELECT_MASK | 0xf);
    val |= g4x_infoframe_index(packet_type);
    if !(keep_avi_enabled && packet_type == HDMI_INFOFRAME_TYPE_AVI) {
        val &= !g4x_infoframe_enable(packet_type);
    }
    io.write32(ctl, val);
    write_packet_words(
        io,
        InfoframePath {
            family,
            pipe,
            transcoder: 0,
            packet_type,
        },
        frame,
        len,
        VIDEO_DIP_DATA_SIZE,
    );
    val |= g4x_infoframe_enable(packet_type);
    val &= !VIDEO_DIP_FREQ_MASK;
    val |= VIDEO_DIP_FREQ_VSYNC;
    io.write32(ctl, val);
    io.posting_read(ctl);
}
fn legacy_read_infoframe(
    io: &mut impl HdmiIo,
    family: InfoframeFamily,
    pipe: u8,
    packet_type: u8,
    frame: &mut [u8],
    len: usize,
) {
    let ctl = io.register(InfoframeRegister::Control {
        family,
        pipe,
        transcoder: 0,
    });
    let mut val = io.read32(ctl);
    val &= !(VIDEO_DIP_SELECT_MASK | 0xf);
    val |= g4x_infoframe_index(packet_type);
    io.write32(ctl, val);
    read_packet_words(
        io,
        InfoframePath {
            family,
            pipe,
            transcoder: 0,
            packet_type,
        },
        frame,
        len,
    );
}
fn legacy_enabled(
    io: &mut impl HdmiIo,
    family: InfoframeFamily,
    pipe: u8,
    port: u8,
    check_port: bool,
) -> u32 {
    let ctl = io.register(InfoframeRegister::Control {
        family,
        pipe,
        transcoder: 0,
    });
    let val = io.read32(ctl);
    if val & VIDEO_DIP_ENABLE == 0
        || (check_port && val & VIDEO_DIP_PORT_MASK != ((port as u32) << 29))
    {
        return 0;
    }
    val & (VIDEO_DIP_ENABLE_AVI
        | VIDEO_DIP_ENABLE_VENDOR
        | VIDEO_DIP_ENABLE_GAMUT
        | VIDEO_DIP_ENABLE_SPD
        | VIDEO_DIP_ENABLE_GCP)
}

// upstream: intel_hdmi.c ibx_write_infoframe()
pub fn ibx_write_infoframe(
    io: &mut impl HdmiIo,
    pipe: u8,
    packet_type: u8,
    frame: &[u8],
    len: usize,
) {
    legacy_write_infoframe(
        io,
        InfoframeFamily::IbexPeak,
        pipe,
        packet_type,
        frame,
        len,
        false,
    )
}
// upstream: intel_hdmi.c ibx_read_infoframe()
pub fn ibx_read_infoframe(
    io: &mut impl HdmiIo,
    pipe: u8,
    packet_type: u8,
    frame: &mut [u8],
    len: usize,
) {
    legacy_read_infoframe(io, InfoframeFamily::IbexPeak, pipe, packet_type, frame, len)
}
// upstream: intel_hdmi.c ibx_infoframes_enabled()
pub fn ibx_infoframes_enabled(io: &mut impl HdmiIo, pipe: u8, port: u8) -> u32 {
    legacy_enabled(io, InfoframeFamily::IbexPeak, pipe, port, true)
}
// upstream: intel_hdmi.c cpt_write_infoframe()
pub fn cpt_write_infoframe(
    io: &mut impl HdmiIo,
    pipe: u8,
    packet_type: u8,
    frame: &[u8],
    len: usize,
) {
    legacy_write_infoframe(
        io,
        InfoframeFamily::CougarPoint,
        pipe,
        packet_type,
        frame,
        len,
        true,
    )
}
// upstream: intel_hdmi.c cpt_read_infoframe()
pub fn cpt_read_infoframe(
    io: &mut impl HdmiIo,
    pipe: u8,
    packet_type: u8,
    frame: &mut [u8],
    len: usize,
) {
    legacy_read_infoframe(
        io,
        InfoframeFamily::CougarPoint,
        pipe,
        packet_type,
        frame,
        len,
    )
}
// upstream: intel_hdmi.c cpt_infoframes_enabled()
pub fn cpt_infoframes_enabled(io: &mut impl HdmiIo, pipe: u8) -> u32 {
    legacy_enabled(io, InfoframeFamily::CougarPoint, pipe, 0, false)
}
// upstream: intel_hdmi.c vlv_write_infoframe()
pub fn vlv_write_infoframe(
    io: &mut impl HdmiIo,
    pipe: u8,
    packet_type: u8,
    frame: &[u8],
    len: usize,
) {
    legacy_write_infoframe(
        io,
        InfoframeFamily::Valleyview,
        pipe,
        packet_type,
        frame,
        len,
        false,
    )
}
// upstream: intel_hdmi.c vlv_read_infoframe()
pub fn vlv_read_infoframe(
    io: &mut impl HdmiIo,
    pipe: u8,
    packet_type: u8,
    frame: &mut [u8],
    len: usize,
) {
    legacy_read_infoframe(
        io,
        InfoframeFamily::Valleyview,
        pipe,
        packet_type,
        frame,
        len,
    )
}
// upstream: intel_hdmi.c vlv_infoframes_enabled()
pub fn vlv_infoframes_enabled(io: &mut impl HdmiIo, pipe: u8, port: u8) -> u32 {
    legacy_enabled(io, InfoframeFamily::Valleyview, pipe, port, true)
}

// upstream: intel_hdmi.c hsw_write_infoframe()
pub fn hsw_write_infoframe(
    io: &mut impl HdmiIo,
    transcoder: u8,
    display_version: u8,
    has_psr: bool,
    has_panel_replay: bool,
    packet_type: u8,
    frame: &[u8],
    len: usize,
) {
    let ctl = io.register(InfoframeRegister::Control {
        family: InfoframeFamily::Haswell,
        pipe: 0,
        transcoder,
    });
    let size = hsw_dip_data_size(display_version, packet_type);
    let mut val = io.read32(ctl);
    io.warning(len > size, "Infoframe length exceeds DIP data size");
    val &= !hsw_infoframe_enable(packet_type);
    io.write32(ctl, val);
    write_packet_words(
        io,
        InfoframePath {
            family: InfoframeFamily::Haswell,
            pipe: 0,
            transcoder,
            packet_type,
        },
        frame,
        len,
        size,
    );
    if !((13..=14).contains(&display_version)
        && has_psr
        && !has_panel_replay
        && packet_type == DP_SDP_VSC)
    {
        val |= hsw_infoframe_enable(packet_type);
    }
    if packet_type == DP_SDP_VSC {
        val |= VSC_DIP_HW_DATA_SW_HEA;
    }
    io.write32(ctl, val);
    io.posting_read(ctl);
}

// upstream: intel_hdmi.c hsw_read_infoframe()
pub fn hsw_read_infoframe(
    io: &mut impl HdmiIo,
    transcoder: u8,
    packet_type: u8,
    frame: &mut [u8],
    len: usize,
) {
    read_packet_words(
        io,
        InfoframePath {
            family: InfoframeFamily::Haswell,
            pipe: 0,
            transcoder,
            packet_type,
        },
        frame,
        len,
    );
}

// upstream: intel_hdmi.c hsw_infoframes_enabled()
pub fn hsw_infoframes_enabled(
    io: &mut impl HdmiIo,
    transcoder: u8,
    display_version: u8,
    has_as_sdp: bool,
) -> u32 {
    let ctl = io.register(InfoframeRegister::Control {
        family: InfoframeFamily::Haswell,
        pipe: 0,
        transcoder,
    });
    let mut mask = VIDEO_DIP_ENABLE_VSC_HSW
        | VIDEO_DIP_ENABLE_AVI_HSW
        | VIDEO_DIP_ENABLE_GCP_HSW
        | VIDEO_DIP_ENABLE_VS_HSW
        | VIDEO_DIP_ENABLE_GMP_HSW
        | VIDEO_DIP_ENABLE_SPD_HSW;
    if display_version >= 10 {
        mask |= VIDEO_DIP_ENABLE_DRM_GLK;
    }
    if has_as_sdp {
        mask |= VIDEO_DIP_ENABLE_AS_ADL;
    }
    io.read32(ctl) & mask
}

const INFOFRAME_TYPE_TO_IDX: [u8; 8] = [
    HDMI_PACKET_TYPE_GENERAL_CONTROL,
    HDMI_PACKET_TYPE_GAMUT_METADATA,
    DP_SDP_VSC,
    DP_SDP_ADAPTIVE_SYNC,
    HDMI_INFOFRAME_TYPE_AVI,
    HDMI_INFOFRAME_TYPE_SPD,
    HDMI_INFOFRAME_TYPE_VENDOR,
    HDMI_INFOFRAME_TYPE_DRM,
];

// upstream: intel_hdmi.c intel_hdmi_infoframe_enable()
pub const fn intel_hdmi_infoframe_enable(packet_type: u8) -> u32 {
    let mut i = 0;
    while i < INFOFRAME_TYPE_TO_IDX.len() {
        if INFOFRAME_TYPE_TO_IDX[i] == packet_type {
            return 1 << i;
        }
        i += 1;
    }
    0
}

// upstream: intel_hdmi.c intel_hdmi_infoframes_enabled()
pub fn intel_hdmi_infoframes_enabled(enabled_hw_bits: u32, ddi: bool) -> u32 {
    let mut result = 0;
    let mut i = 0;
    while i < INFOFRAME_TYPE_TO_IDX.len() {
        let bit = if ddi {
            hsw_infoframe_enable(INFOFRAME_TYPE_TO_IDX[i])
        } else {
            g4x_infoframe_enable(INFOFRAME_TYPE_TO_IDX[i])
        };
        if enabled_hw_bits & bit != 0 {
            result |= 1 << i;
        }
        i += 1;
    }
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InfoframePayload {
    pub packet_type: u8,
    pub bytes: [u8; VIDEO_DIP_DATA_SIZE],
    pub len: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InfoframeResult {
    Skipped,
    Written,
    InvalidType,
    PackFailed,
    TooLong,
}

// upstream: intel_hdmi.c intel_write_infoframe()
pub fn intel_write_infoframe(
    io: &mut impl HdmiIo,
    enabled: u32,
    packet_type: u8,
    frame_type: u8,
    frame: &[u8],
) -> InfoframeResult {
    if enabled & intel_hdmi_infoframe_enable(packet_type) == 0 {
        return InfoframeResult::Skipped;
    }
    if frame_type != packet_type {
        io.warning(true, "Infoframe type differs from selected type");
        return InfoframeResult::InvalidType;
    }
    let mut buffer = [0u8; VIDEO_DIP_DATA_SIZE];
    let len = match io.pack_infoframe(packet_type, frame, &mut buffer[1..]) {
        Ok(len) if len <= VIDEO_DIP_DATA_SIZE - 1 => len,
        Ok(_) => return InfoframeResult::TooLong,
        Err(()) => {
            io.warning(true, "Infoframe pack failed");
            return InfoframeResult::PackFailed;
        }
    };
    let headers = [buffer[1], buffer[2], buffer[3]];
    buffer[..3].copy_from_slice(&headers);
    buffer[3] = 0;
    io.write_infoframe(packet_type, &buffer, len + 1);
    InfoframeResult::Written
}

// upstream: intel_hdmi.c intel_read_infoframe()
pub fn intel_read_infoframe(
    io: &mut impl HdmiIo,
    enabled: u32,
    packet_type: u8,
    out: &mut [u8],
) -> bool {
    if enabled & intel_hdmi_infoframe_enable(packet_type) == 0 {
        return false;
    }
    let mut raw = [0u8; VIDEO_DIP_DATA_SIZE];
    io.read_infoframe(packet_type, &mut raw);
    let headers = [raw[0], raw[1], raw[2]];
    raw[1..4].copy_from_slice(&headers);
    if !io.unpack_infoframe(packet_type, &raw[1..], out) {
        io.debug("HDMI infoframe unpack failed or type mismatched");
        return false;
    }
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InfoframeState {
    pub has_infoframe: bool,
    pub enabled: u32,
    pub output_format: OutputFormat,
    pub limited_color_range: bool,
    pub has_hdmi_infoframe: bool,
    pub display_version: u8,
    pub discrete_graphics: bool,
    pub hdr_metadata: bool,
    pub pipe_bpp: u8,
    pub gcp: u32,
    pub mode: HdmiMode,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InfoframeFrameworkError {
    Failed,
}
/// DRM's infoframe construction/checking API, kept as narrow framework hooks.
pub trait InfoframeFramework {
    fn avi_from_mode(&mut self, mode: HdmiMode) -> Result<(), InfoframeFrameworkError>;
    fn avi_colorimetry(&mut self);
    fn avi_set_colorspace(&mut self, format: OutputFormat);
    fn avi_quant_range(&mut self, limited: bool);
    fn avi_ycc_defaults(&mut self);
    fn avi_content_type(&mut self);
    fn avi_check(&mut self) -> Result<(), InfoframeFrameworkError>;
    fn spd_init(
        &mut self,
        vendor: &'static str,
        product: &'static str,
    ) -> Result<(), InfoframeFrameworkError>;
    fn spd_set_source_device_pc(&mut self);
    fn spd_check(&mut self) -> Result<(), InfoframeFrameworkError>;
    fn vendor_from_mode(&mut self, mode: HdmiMode) -> Result<(), InfoframeFrameworkError>;
    fn vendor_check(&mut self) -> Result<(), InfoframeFrameworkError>;
    fn drm_set_hdr_metadata(&mut self) -> Result<(), InfoframeFrameworkError>;
    fn drm_check(&mut self) -> Result<(), InfoframeFrameworkError>;
}

// upstream: intel_hdmi.c intel_hdmi_compute_avi_infoframe()
pub fn intel_hdmi_compute_avi_infoframe(
    state: &mut InfoframeState,
    fw: &mut impl InfoframeFramework,
    io: &mut impl HdmiIo,
) -> bool {
    if !state.has_infoframe {
        return true;
    }
    state.enabled |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_AVI);
    if fw.avi_from_mode(state.mode).is_err() {
        return false;
    }
    fw.avi_set_colorspace(state.output_format);
    fw.avi_colorimetry();
    io.warning(
        state.limited_color_range && state.output_format != OutputFormat::Rgb,
        "limited range requested for non-RGB HDMI output",
    );
    if state.output_format == OutputFormat::Rgb {
        fw.avi_quant_range(state.limited_color_range);
    } else {
        fw.avi_ycc_defaults();
    }
    fw.avi_content_type();
    fw.avi_check().is_ok()
}

// upstream: intel_hdmi.c intel_hdmi_compute_spd_infoframe()
pub fn intel_hdmi_compute_spd_infoframe(
    state: &mut InfoframeState,
    fw: &mut impl InfoframeFramework,
    io: &mut impl HdmiIo,
) -> bool {
    if !state.has_infoframe {
        return true;
    }
    state.enabled |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_SPD);
    let product = if state.discrete_graphics {
        "Discrete gfx"
    } else {
        "Integrated gfx"
    };
    if fw.spd_init("Intel", product).is_err() {
        io.warning(true, "SPD infoframe initialization failed");
        return false;
    }
    fw.spd_set_source_device_pc();
    fw.spd_check().is_ok()
}

// upstream: intel_hdmi.c intel_hdmi_compute_hdmi_infoframe()
pub fn intel_hdmi_compute_hdmi_infoframe(
    state: &mut InfoframeState,
    fw: &mut impl InfoframeFramework,
    io: &mut impl HdmiIo,
) -> bool {
    if !state.has_infoframe || !state.has_hdmi_infoframe {
        return true;
    }
    state.enabled |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_VENDOR);
    if fw.vendor_from_mode(state.mode).is_err() || fw.vendor_check().is_err() {
        io.warning(true, "HDMI vendor infoframe construction/check failed");
        return false;
    }
    true
}

// upstream: intel_hdmi.c intel_hdmi_compute_drm_infoframe()
pub fn intel_hdmi_compute_drm_infoframe(
    state: &mut InfoframeState,
    fw: &mut impl InfoframeFramework,
    io: &mut impl HdmiIo,
) -> bool {
    if state.display_version < 10 || !state.has_infoframe || !state.hdr_metadata {
        return true;
    }
    state.enabled |= intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_DRM);
    if fw.drm_set_hdr_metadata().is_err() || fw.drm_check().is_err() {
        io.warning(true, "DRM HDR infoframe construction/check failed");
        return false;
    }
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InfoframeSet {
    pub enabled: u32,
    pub port: u8,
    pub gcp: u32,
    pub avi: InfoframePayload,
    pub spd: InfoframePayload,
    pub vendor: InfoframePayload,
    pub drm: InfoframePayload,
}
fn set_packet(
    io: &mut impl HdmiIo,
    frames: &InfoframeSet,
    packet_type: u8,
    payload: &InfoframePayload,
) {
    let _ = intel_write_infoframe(
        io,
        frames.enabled,
        packet_type,
        payload.packet_type,
        &payload.bytes[..payload.len.min(VIDEO_DIP_DATA_SIZE - 1)],
    );
}
fn set_old_infoframes(
    io: &mut impl HdmiIo,
    family: InfoframeFamily,
    pipe: u8,
    port: u8,
    enable: bool,
    frames: &InfoframeSet,
    include_gamut: bool,
    set_avi_enable: bool,
    gcp_register: bool,
) {
    let ctl = io.register(InfoframeRegister::Control {
        family,
        pipe,
        transcoder: 0,
    });
    let port_enabled = io.hdmi_port_enabled();
    io.warning(
        port_enabled,
        "HDMI output must be disabled while changing infoframes",
    );
    let mut val = io.read32(ctl);
    val |= VIDEO_DIP_SELECT_AVI | VIDEO_DIP_FREQ_VSYNC;
    let clear = VIDEO_DIP_ENABLE
        | VIDEO_DIP_ENABLE_AVI
        | VIDEO_DIP_ENABLE_VENDOR
        | VIDEO_DIP_ENABLE_SPD
        | if gcp_register {
            VIDEO_DIP_ENABLE_GCP
        } else {
            0
        }
        | if include_gamut {
            VIDEO_DIP_ENABLE_GAMUT
        } else {
            0
        };
    if !enable {
        if val & VIDEO_DIP_ENABLE == 0 {
            return;
        }
        val &= !clear;
        io.write32(ctl, val);
        io.posting_read(ctl);
        return;
    }
    if family != InfoframeFamily::CougarPoint
        && ((port as u32) << 29) != (val & VIDEO_DIP_PORT_MASK)
    {
        if family == InfoframeFamily::G4x && val & VIDEO_DIP_ENABLE != 0 {
            io.debug("video DIP still enabled on another port");
            return;
        }
        io.warning(
            val & VIDEO_DIP_ENABLE != 0,
            "DIP already enabled on another port",
        );
        val &= !VIDEO_DIP_PORT_MASK;
        val |= (port as u32) << 29;
    }
    val |= VIDEO_DIP_ENABLE;
    val &= !(VIDEO_DIP_ENABLE_AVI
        | VIDEO_DIP_ENABLE_VENDOR
        | VIDEO_DIP_ENABLE_SPD
        | (if gcp_register {
            VIDEO_DIP_ENABLE_GCP
        } else {
            0
        })
        | (if include_gamut {
            VIDEO_DIP_ENABLE_GAMUT
        } else {
            0
        }));
    if set_avi_enable {
        val |= VIDEO_DIP_ENABLE_AVI;
    }
    if gcp_register
        && frames.enabled & intel_hdmi_infoframe_enable(HDMI_PACKET_TYPE_GENERAL_CONTROL) != 0
    {
        let gcp = io.register(InfoframeRegister::Gcp {
            family,
            pipe,
            transcoder: 0,
        });
        io.write32(gcp, frames.gcp);
        val |= VIDEO_DIP_ENABLE_GCP;
    }
    io.write32(ctl, val);
    io.posting_read(ctl);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_AVI, &frames.avi);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_SPD, &frames.spd);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_VENDOR, &frames.vendor);
}

// upstream: intel_hdmi.c g4x_set_infoframes()
pub fn g4x_set_infoframes(io: &mut impl HdmiIo, enable: bool, frames: &InfoframeSet) {
    set_old_infoframes(
        io,
        InfoframeFamily::G4x,
        0,
        frames.port,
        enable,
        frames,
        false,
        false,
        false,
    )
}

// upstream: intel_hdmi.c gcp_default_phase_possible()
pub fn gcp_default_phase_possible(pipe_bpp: u8, mode: HdmiMode) -> bool {
    let group = match pipe_bpp {
        30 => 4,
        36 => 2,
        48 => 1,
        _ => return false,
    };
    let group = group as i32;
    mode.hdisplay % group == 0
        && mode.htotal % group == 0
        && mode.hblank_start % group == 0
        && mode.hblank_end % group == 0
        && mode.hsync_start % group == 0
        && mode.hsync_end % group == 0
        && (mode.flags & DRM_MODE_FLAG_INTERLACE == 0 || (mode.htotal / 2) % group == 0)
}

// upstream: intel_hdmi.c intel_hdmi_set_gcp_infoframe()
pub fn intel_hdmi_set_gcp_infoframe(
    io: &mut impl HdmiIo,
    family: Option<InfoframeFamily>,
    pipe: u8,
    transcoder: u8,
    enabled: bool,
    value: u32,
) -> bool {
    if !enabled {
        return false;
    }
    let Some(family) = family else {
        return false;
    };
    let reg = io.register(InfoframeRegister::Gcp {
        family,
        pipe,
        transcoder,
    });
    io.write32(reg, value);
    true
}

// upstream: intel_hdmi.c intel_hdmi_read_gcp_infoframe()
pub fn intel_hdmi_read_gcp_infoframe(
    io: &mut impl HdmiIo,
    family: Option<InfoframeFamily>,
    pipe: u8,
    transcoder: u8,
    enabled: bool,
) -> Option<u32> {
    if !enabled {
        return None;
    }
    let family = family?;
    let reg = io.register(InfoframeRegister::Gcp {
        family,
        pipe,
        transcoder,
    });
    Some(io.read32(reg))
}

// upstream: intel_hdmi.c intel_hdmi_compute_gcp_infoframe()
pub fn intel_hdmi_compute_gcp_infoframe(state: &mut InfoframeState, g4x: bool) {
    if g4x || !state.has_infoframe {
        return;
    }
    state.enabled |= intel_hdmi_infoframe_enable(HDMI_PACKET_TYPE_GENERAL_CONTROL);
    if state.pipe_bpp > 24 {
        state.gcp |= GCP_COLOR_INDICATION;
    }
    if gcp_default_phase_possible(state.pipe_bpp, state.mode) {
        state.gcp |= GCP_DEFAULT_PHASE_ENABLE;
    }
}

// upstream: intel_hdmi.c ibx_set_infoframes()
pub fn ibx_set_infoframes(io: &mut impl HdmiIo, pipe: u8, enable: bool, frames: &InfoframeSet) {
    set_old_infoframes(
        io,
        InfoframeFamily::IbexPeak,
        pipe,
        frames.port,
        enable,
        frames,
        true,
        false,
        true,
    )
}
// upstream: intel_hdmi.c cpt_set_infoframes()
pub fn cpt_set_infoframes(io: &mut impl HdmiIo, pipe: u8, enable: bool, frames: &InfoframeSet) {
    set_old_infoframes(
        io,
        InfoframeFamily::CougarPoint,
        pipe,
        frames.port,
        enable,
        frames,
        true,
        true,
        true,
    )
}
// upstream: intel_hdmi.c vlv_set_infoframes()
pub fn vlv_set_infoframes(io: &mut impl HdmiIo, pipe: u8, enable: bool, frames: &InfoframeSet) {
    set_old_infoframes(
        io,
        InfoframeFamily::Valleyview,
        pipe,
        frames.port,
        enable,
        frames,
        true,
        false,
        true,
    )
}

// upstream: intel_hdmi.c intel_hdmi_fastset_infoframes()
pub fn intel_hdmi_fastset_infoframes(io: &mut impl HdmiIo, transcoder: u8, frames: &InfoframeSet) {
    let ctl = io.register(InfoframeRegister::Control {
        family: InfoframeFamily::Haswell,
        pipe: 0,
        transcoder,
    });
    let mut val = io.read32(ctl);
    if frames.enabled & intel_hdmi_infoframe_enable(HDMI_INFOFRAME_TYPE_DRM) == 0
        && val & VIDEO_DIP_ENABLE_DRM_GLK == 0
    {
        return;
    }
    val &= !VIDEO_DIP_ENABLE_DRM_GLK;
    io.write32(ctl, val);
    io.posting_read(ctl);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_DRM, &frames.drm);
}

// upstream: intel_hdmi.c hsw_set_infoframes()
pub fn hsw_set_infoframes(
    io: &mut impl HdmiIo,
    transcoder: u8,
    enable: bool,
    frames: &InfoframeSet,
) {
    let ctl = io.register(InfoframeRegister::Control {
        family: InfoframeFamily::Haswell,
        pipe: 0,
        transcoder,
    });
    let mut val = io.read32(ctl);
    let transcoder_enabled = io.transcoder_function_enabled(transcoder);
    io.warning(
        transcoder_enabled,
        "HDMI transcoder function must be disabled while changing infoframes",
    );
    val &= !(VIDEO_DIP_ENABLE_VSC_HSW
        | VIDEO_DIP_ENABLE_AVI_HSW
        | VIDEO_DIP_ENABLE_GCP_HSW
        | VIDEO_DIP_ENABLE_VS_HSW
        | VIDEO_DIP_ENABLE_GMP_HSW
        | VIDEO_DIP_ENABLE_SPD_HSW
        | VIDEO_DIP_ENABLE_DRM_GLK
        | VIDEO_DIP_ENABLE_AS_ADL);
    if !enable {
        io.write32(ctl, val);
        io.posting_read(ctl);
        return;
    }
    if frames.enabled & intel_hdmi_infoframe_enable(HDMI_PACKET_TYPE_GENERAL_CONTROL) != 0 {
        let gcp = io.register(InfoframeRegister::Gcp {
            family: InfoframeFamily::Haswell,
            pipe: 0,
            transcoder,
        });
        io.write32(gcp, frames.gcp);
        val |= VIDEO_DIP_ENABLE_GCP_HSW;
    }
    io.write32(ctl, val);
    io.posting_read(ctl);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_AVI, &frames.avi);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_SPD, &frames.spd);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_VENDOR, &frames.vendor);
    set_packet(io, frames, HDMI_INFOFRAME_TYPE_DRM, &frames.drm);
}

pub const DRM_HDCP_DDC_ADDR: u8 = 0x74;
pub const DRM_HDCP_AN_LEN: usize = 8;
pub const DRM_HDCP_BSTATUS_LEN: usize = 2;
pub const DRM_HDCP_KSV_LEN: usize = 5;
pub const DRM_HDCP_RI_LEN: usize = 2;
pub const DRM_HDCP_V_PRIME_PART_LEN: usize = 4;
pub const DRM_HDCP_V_PRIME_NUM_PARTS: usize = 5;
pub const DRM_HDCP_DDC_BKSV: u8 = 0x00;
pub const DRM_HDCP_DDC_RI_PRIME: u8 = 0x08;
pub const DRM_HDCP_DDC_AN: u8 = 0x18;
pub const DRM_HDCP_DDC_BCAPS: u8 = 0x40;
pub const DRM_HDCP_DDC_BCAPS_REPEATER_PRESENT: u8 = 1 << 6;
pub const DRM_HDCP_DDC_BCAPS_KSV_FIFO_READY: u8 = 1 << 5;
pub const DRM_HDCP_DDC_BSTATUS: u8 = 0x41;
pub const DRM_HDCP_DDC_KSV_FIFO: u8 = 0x43;
pub const HDCP_2_2_AKE_SEND_CERT: u8 = 3;
pub const HDCP_2_2_AKE_SEND_HPRIME: u8 = 7;
pub const HDCP_2_2_AKE_SEND_PAIRING_INFO: u8 = 8;
pub const HDCP_2_2_LC_SEND_LPRIME: u8 = 10;
pub const HDCP_2_2_REP_SEND_RECVID_LIST: u8 = 12;
pub const HDCP_2_2_REP_STREAM_READY: u8 = 17;
pub const HDCP_2_2_HDMI_REG_VER_OFFSET: u8 = 0x50;
pub const HDCP_2_2_HDMI_REG_WR_MSG_OFFSET: u8 = 0x60;
pub const HDCP_2_2_HDMI_REG_RXSTATUS_OFFSET: u8 = 0x70;
pub const HDCP_2_2_HDMI_REG_RD_MSG_OFFSET: u8 = 0x80;
pub const HDCP_2_2_HDMI_RXSTATUS_LEN: usize = 2;
pub const HDCP_2_2_HDMI_RXSTATUS_READY: u8 = 1 << 2;
pub const HDCP_2_2_HDMI_RXSTATUS_REAUTH_REQ: u8 = 1 << 3;
pub const HDCP_2_2_HDMI_SUPPORT_MASK: u8 = 1 << 2;
pub const HDCP_TOPOLOGY_CHANGE: i32 = 1;
pub const HDCP_STATUS_RI_MATCH: u32 = 1 << 19;
pub const HDCP_STATUS_ENC: u32 = 1 << 20;
pub const HDCP_REAUTH_REQUEST: i32 = 3;

/// Low-level I2C, wait and display-link operations used by the HDCP shim.
pub trait HdcpIo {
    /// Source performs a two-message offset-write plus read transfer at 0x74.
    fn hdcp_i2c_read(&mut self, address: u8, offset: u8, out: &mut [u8]) -> i32;
    /// Source performs one message containing offset followed by payload.
    fn hdcp_i2c_write(&mut self, address: u8, bytes: &[u8]) -> i32;
    fn gmbus_output_aksv(&mut self) -> i32;
    fn delay_us_range(&mut self, minimum: u32, maximum: u32);
    fn scanline(&mut self) -> u32;
    fn toggle_hdcp_signalling(&mut self, transcoder: u8, enable: bool) -> i32;
    fn wait_hdcp_status(&mut self, transcoder: u8, port: u8, mask: u32, timeout_ms: u32) -> i32;
    fn read_hdcp_status(&mut self, transcoder: u8, port: u8) -> u32;
    fn write_hdcp_rprime(&mut self, transcoder: u8, port: u8, value: u32);
    fn poll_rx_status(
        &mut self,
        offset: u8,
        interval_us: u32,
        timeout_us: u32,
    ) -> Result<[u8; 2], i32>;
}

pub trait DualModeTmdsIo {
    fn set_dual_mode_tmds_output(&mut self, adapter_type: u8, enable: bool);
}

// upstream: intel_hdmi.c intel_dp_dual_mode_set_tmds_output()
pub fn intel_dp_dual_mode_set_tmds_output(
    io: &mut impl DualModeTmdsIo,
    adapter_type: u8,
    enable: bool,
) {
    if adapter_type < 2 {
        return;
    }
    io.set_dual_mode_tmds_output(adapter_type, enable);
}

// upstream: intel_hdmi.c intel_hdmi_hdcp_read()
pub fn intel_hdmi_hdcp_read(io: &mut impl HdcpIo, offset: u32, buffer: &mut [u8]) -> i32 {
    let ret = io.hdcp_i2c_read(DRM_HDCP_DDC_ADDR, offset as u8, buffer);
    if ret == 2 {
        0
    } else if ret >= 0 {
        -5
    } else {
        ret
    }
}

// upstream: intel_hdmi.c intel_hdmi_hdcp_write()
pub fn intel_hdmi_hdcp_write(io: &mut impl HdcpIo, offset: u32, buffer: &[u8]) -> i32 {
    let mut write_buf = alloc::vec::Vec::with_capacity(buffer.len() + 1);
    write_buf.push(offset as u8);
    write_buf.extend_from_slice(buffer);
    let ret = io.hdcp_i2c_write(DRM_HDCP_DDC_ADDR, &write_buf);
    if ret == 1 {
        0
    } else if ret >= 0 {
        -5
    } else {
        ret
    }
}

// upstream: intel_hdmi.c intel_hdmi_hdcp_write_an_aksv()
pub fn intel_hdmi_hdcp_write_an_aksv(io: &mut impl HdcpIo, an: &[u8]) -> i32 {
    if an.len() < DRM_HDCP_AN_LEN {
        return -22;
    }
    let ret = intel_hdmi_hdcp_write(io, DRM_HDCP_DDC_AN as u32, &an[..DRM_HDCP_AN_LEN]);
    if ret != 0 {
        return ret;
    }
    let ret = io.gmbus_output_aksv();
    if ret < 0 { ret } else { 0 }
}

// upstream: intel_hdmi.c intel_hdmi_hdcp_read_bksv()
pub fn intel_hdmi_hdcp_read_bksv(io: &mut impl HdcpIo, bksv: &mut [u8]) -> i32 {
    if bksv.len() < DRM_HDCP_KSV_LEN {
        return -22;
    }
    intel_hdmi_hdcp_read(io, DRM_HDCP_DDC_BKSV as u32, &mut bksv[..DRM_HDCP_KSV_LEN])
}
// upstream: intel_hdmi.c intel_hdmi_hdcp_read_bstatus()
pub fn intel_hdmi_hdcp_read_bstatus(io: &mut impl HdcpIo, bstatus: &mut [u8]) -> i32 {
    if bstatus.len() < DRM_HDCP_BSTATUS_LEN {
        return -22;
    }
    intel_hdmi_hdcp_read(
        io,
        DRM_HDCP_DDC_BSTATUS as u32,
        &mut bstatus[..DRM_HDCP_BSTATUS_LEN],
    )
}
// upstream: intel_hdmi.c intel_hdmi_hdcp_repeater_present()
pub fn intel_hdmi_hdcp_repeater_present(io: &mut impl HdcpIo) -> Result<bool, i32> {
    let mut value = [0u8; 1];
    let ret = intel_hdmi_hdcp_read(io, DRM_HDCP_DDC_BCAPS as u32, &mut value);
    if ret != 0 {
        return Err(ret);
    }
    Ok(value[0] & DRM_HDCP_DDC_BCAPS_REPEATER_PRESENT != 0)
}
// upstream: intel_hdmi.c intel_hdmi_hdcp_read_ri_prime()
pub fn intel_hdmi_hdcp_read_ri_prime(io: &mut impl HdcpIo, ri_prime: &mut [u8]) -> i32 {
    if ri_prime.len() < DRM_HDCP_RI_LEN {
        return -22;
    }
    intel_hdmi_hdcp_read(
        io,
        DRM_HDCP_DDC_RI_PRIME as u32,
        &mut ri_prime[..DRM_HDCP_RI_LEN],
    )
}
// upstream: intel_hdmi.c intel_hdmi_hdcp_read_ksv_ready()
pub fn intel_hdmi_hdcp_read_ksv_ready(io: &mut impl HdcpIo) -> Result<bool, i32> {
    let mut value = [0u8; 1];
    let ret = intel_hdmi_hdcp_read(io, DRM_HDCP_DDC_BCAPS as u32, &mut value);
    if ret != 0 {
        return Err(ret);
    }
    Ok(value[0] & DRM_HDCP_DDC_BCAPS_KSV_FIFO_READY != 0)
}
// upstream: intel_hdmi.c intel_hdmi_hdcp_read_ksv_fifo()
pub fn intel_hdmi_hdcp_read_ksv_fifo(
    io: &mut impl HdcpIo,
    num_downstream: usize,
    fifo: &mut [u8],
) -> i32 {
    let size = num_downstream
        .saturating_mul(DRM_HDCP_KSV_LEN)
        .min(fifo.len());
    intel_hdmi_hdcp_read(io, DRM_HDCP_DDC_KSV_FIFO as u32, &mut fifo[..size])
}
// upstream: intel_hdmi.c intel_hdmi_hdcp_read_v_prime_part()
pub fn intel_hdmi_hdcp_read_v_prime_part(
    io: &mut impl HdcpIo,
    part_index: usize,
    part: &mut [u8],
) -> i32 {
    if part_index >= DRM_HDCP_V_PRIME_NUM_PARTS {
        return -22;
    }
    if part.len() < DRM_HDCP_V_PRIME_PART_LEN {
        return -22;
    }
    intel_hdmi_hdcp_read(
        io,
        (0x20 + part_index * 4) as u32,
        &mut part[..DRM_HDCP_V_PRIME_PART_LEN],
    )
}

// upstream: intel_hdmi.c kbl_repositioning_enc_en_signal()
pub fn kbl_repositioning_enc_en_signal(io: &mut impl HdcpIo, transcoder: u8) -> i32 {
    loop {
        let scanline = io.scanline();
        if scanline > 100 && scanline < 200 {
            break;
        }
        io.delay_us_range(25, 50);
    }
    let ret = io.toggle_hdcp_signalling(transcoder, false);
    if ret != 0 {
        return ret;
    }
    io.toggle_hdcp_signalling(transcoder, true)
}

// upstream: intel_hdmi.c intel_hdmi_hdcp_toggle_signalling()
pub fn intel_hdmi_hdcp_toggle_signalling(
    io: &mut impl HdcpIo,
    transcoder: u8,
    enable: bool,
    kabylake_wa: bool,
) -> i32 {
    if !enable {
        io.delay_us_range(6, 60);
    }
    let ret = io.toggle_hdcp_signalling(transcoder, enable);
    if ret != 0 {
        return ret;
    }
    if kabylake_wa && enable {
        return kbl_repositioning_enc_en_signal(io, transcoder);
    }
    0
}

// upstream: intel_hdmi.c intel_hdmi_hdcp_check_link_once()
pub fn intel_hdmi_hdcp_check_link_once(io: &mut impl HdcpIo, transcoder: u8, port: u8) -> bool {
    let mut ri = [0u8; DRM_HDCP_RI_LEN];
    if intel_hdmi_hdcp_read_ri_prime(io, &mut ri) != 0 {
        return false;
    }
    let value = u32::from_le_bytes([ri[0], ri[1], 0, 0]);
    io.write_hdcp_rprime(transcoder, port, value);
    if io.wait_hdcp_status(transcoder, port, HDCP_STATUS_RI_MATCH | HDCP_STATUS_ENC, 1) != 0 {
        let _status = io.read_hdcp_status(transcoder, port);
        return false;
    }
    true
}

// upstream: intel_hdmi.c intel_hdmi_hdcp_check_link()
pub fn intel_hdmi_hdcp_check_link(io: &mut impl HdcpIo, transcoder: u8, port: u8) -> bool {
    for _retry in 0..3 {
        if intel_hdmi_hdcp_check_link_once(io, transcoder, port) {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hdcp2MessageTimeout {
    pub message_id: u8,
    pub timeout_ms: u16,
}
const HDCP2_MESSAGE_TIMEOUTS: [Hdcp2MessageTimeout; 5] = [
    Hdcp2MessageTimeout {
        message_id: HDCP_2_2_AKE_SEND_CERT,
        timeout_ms: 100,
    },
    Hdcp2MessageTimeout {
        message_id: HDCP_2_2_AKE_SEND_PAIRING_INFO,
        timeout_ms: 200,
    },
    Hdcp2MessageTimeout {
        message_id: HDCP_2_2_LC_SEND_LPRIME,
        timeout_ms: 20,
    },
    Hdcp2MessageTimeout {
        message_id: HDCP_2_2_REP_SEND_RECVID_LIST,
        timeout_ms: 3000,
    },
    Hdcp2MessageTimeout {
        message_id: HDCP_2_2_REP_STREAM_READY,
        timeout_ms: 100,
    },
];

// upstream: intel_hdmi.c intel_hdmi_hdcp2_read_rx_status()
pub fn intel_hdmi_hdcp2_read_rx_status(io: &mut impl HdcpIo, out: &mut [u8; 2]) -> i32 {
    intel_hdmi_hdcp_read(io, 0x70, out)
}

// upstream: intel_hdmi.c get_hdcp2_msg_timeout()
pub const fn get_hdcp2_msg_timeout(msg_id: u8, is_paired: bool) -> i32 {
    if msg_id == HDCP_2_2_AKE_SEND_HPRIME {
        return if is_paired { 200 } else { 1000 };
    }
    let mut i = 0;
    while i < HDCP2_MESSAGE_TIMEOUTS.len() {
        if HDCP2_MESSAGE_TIMEOUTS[i].message_id == msg_id {
            return HDCP2_MESSAGE_TIMEOUTS[i].timeout_ms as i32;
        }
        i += 1;
    }
    -22
}

// upstream: intel_hdmi.c hdcp2_detect_msg_availability()
pub fn hdcp2_detect_msg_availability(
    io: &mut impl HdcpIo,
    msg_id: u8,
) -> Result<(bool, usize), i32> {
    let mut rx_status = [0u8; HDCP_2_2_HDMI_RXSTATUS_LEN];
    let ret = intel_hdmi_hdcp2_read_rx_status(io, &mut rx_status);
    if ret < 0 {
        return Err(ret);
    }
    let msg_size = (((rx_status[1] & 0x3) as usize) << 8) | rx_status[0] as usize;
    let ready = if msg_id == HDCP_2_2_REP_SEND_RECVID_LIST {
        rx_status[1] & HDCP_2_2_HDMI_RXSTATUS_READY != 0 && msg_size != 0
    } else {
        msg_size != 0
    };
    Ok((ready, msg_size))
}

// upstream: intel_hdmi.c intel_hdmi_hdcp2_wait_for_msg()
pub fn intel_hdmi_hdcp2_wait_for_msg(
    io: &mut impl HdcpIo,
    msg_id: u8,
    paired: bool,
) -> Result<usize, i32> {
    let timeout = get_hdcp2_msg_timeout(msg_id, paired);
    if timeout < 0 {
        return Err(timeout);
    }
    match io.poll_rx_status(
        HDCP_2_2_HDMI_REG_RXSTATUS_OFFSET,
        4000,
        (timeout as u32) * 1000,
    ) {
        Err(ret) => Err(ret),
        Ok(rx) => {
            let size = (((rx[1] & 0x3) as usize) << 8) | rx[0] as usize;
            let ready = if msg_id == HDCP_2_2_REP_SEND_RECVID_LIST {
                rx[1] & HDCP_2_2_HDMI_RXSTATUS_READY != 0 && size != 0
            } else {
                size != 0
            };
            if ready { Ok(size) } else { Err(-110) }
        }
    }
}

// upstream: intel_hdmi.c intel_hdmi_hdcp2_write_msg()
pub fn intel_hdmi_hdcp2_write_msg(io: &mut impl HdcpIo, bytes: &[u8]) -> i32 {
    intel_hdmi_hdcp_write(io, HDCP_2_2_HDMI_REG_WR_MSG_OFFSET as u32, bytes)
}

// upstream: intel_hdmi.c intel_hdmi_hdcp2_read_msg()
pub fn intel_hdmi_hdcp2_read_msg(
    io: &mut impl HdcpIo,
    msg_id: u8,
    paired: bool,
    out: &mut [u8],
) -> i32 {
    let size = match intel_hdmi_hdcp2_wait_for_msg(io, msg_id, paired) {
        Ok(size) => size,
        Err(ret) => return ret,
    };
    if size > out.len() {
        return -22;
    }
    intel_hdmi_hdcp_read(io, HDCP_2_2_HDMI_REG_RD_MSG_OFFSET as u32, &mut out[..size])
}

// upstream: intel_hdmi.c intel_hdmi_hdcp2_check_link()
pub fn intel_hdmi_hdcp2_check_link(io: &mut impl HdcpIo) -> i32 {
    let mut rx_status = [0u8; HDCP_2_2_HDMI_RXSTATUS_LEN];
    let ret = intel_hdmi_hdcp2_read_rx_status(io, &mut rx_status);
    if ret != 0 {
        return ret;
    }
    if rx_status[1] & HDCP_2_2_HDMI_RXSTATUS_REAUTH_REQ != 0 {
        HDCP_REAUTH_REQUEST
    } else if rx_status[1] & HDCP_2_2_HDMI_RXSTATUS_READY != 0 {
        HDCP_TOPOLOGY_CHANGE
    } else {
        ret
    }
}

// upstream: intel_hdmi.c intel_hdmi_hdcp2_get_capability()
pub fn intel_hdmi_hdcp2_get_capability(io: &mut impl HdcpIo) -> Result<bool, i32> {
    let mut version = [0u8; 1];
    let ret = intel_hdmi_hdcp_read(io, HDCP_2_2_HDMI_REG_VER_OFFSET as u32, &mut version);
    if ret != 0 {
        return Err(ret);
    }
    Ok(version[0] & HDCP_2_2_HDMI_SUPPORT_MASK != 0)
}

// upstream: intel_hdmi.c intel_hdmi_source_max_tmds_clock()
pub fn intel_hdmi_source_max_tmds_clock(
    display_version: u8,
    haswell: bool,
    alder_lake_s: bool,
    bios_limit_khz: i32,
) -> i32 {
    let mut limit = if display_version >= 13 || alder_lake_s {
        600_000
    } else if display_version >= 10 {
        594_000
    } else if display_version >= 8 || haswell {
        300_000
    } else if display_version >= 5 {
        225_000
    } else {
        165_000
    };
    if bios_limit_khz != 0 {
        limit = limit.min(bios_limit_khz);
    }
    limit
}

// upstream: intel_hdmi.c intel_has_hdmi_sink()
pub const fn intel_has_hdmi_sink(is_hdmi: bool, force_audio: i32, hdmi_audio_off_dvi: i32) -> bool {
    is_hdmi && force_audio != hdmi_audio_off_dvi
}

// upstream: intel_hdmi.c intel_hdmi_is_ycbcr420()
pub const fn intel_hdmi_is_ycbcr420(format: OutputFormat) -> bool {
    matches!(format, OutputFormat::Ycbcr420)
}

// upstream: intel_hdmi.c hdmi_port_clock_limit()
pub fn hdmi_port_clock_limit(limits: ClockLimits) -> i32 {
    let mut max_tmds_clock = limits.source_limit_khz;
    if limits.respect_downstream_limits {
        if let Some(limit) = limits.dp_dual_mode_limit_khz.filter(|v| *v != 0) {
            max_tmds_clock = max_tmds_clock.min(limit);
        }
        if let Some(limit) = limits.sink_limit_khz.filter(|v| *v != 0) {
            max_tmds_clock = max_tmds_clock.min(limit);
        } else if !limits.has_hdmi_sink {
            max_tmds_clock = max_tmds_clock.min(165_000);
        }
    }
    max_tmds_clock
}

// upstream: intel_hdmi.c hdmi_port_clock_valid()
pub fn hdmi_port_clock_valid(clock_khz: i32, limits: ClockLimits) -> HdmiModeStatus {
    if clock_khz < 25_000 {
        return HdmiModeStatus::ClockLow;
    }
    if clock_khz > hdmi_port_clock_limit(limits) {
        return HdmiModeStatus::ClockHigh;
    }
    let p = limits.platform;
    if matches!(p, PortPlatform::Geminilake) && (446_666..480_000).contains(&clock_khz) {
        return HdmiModeStatus::ClockRange;
    }
    if matches!(p, PortPlatform::Geminilake | PortPlatform::Broxton)
        && (223_333..240_000).contains(&clock_khz)
    {
        return HdmiModeStatus::ClockRange;
    }
    if matches!(p, PortPlatform::Cherryview) && (216_000..240_000).contains(&clock_khz) {
        return HdmiModeStatus::ClockRange;
    }
    if limits.port == HdmiPortClass::Combo && (500_000..533_200).contains(&clock_khz) {
        return HdmiModeStatus::ClockRange;
    }
    if limits.port == HdmiPortClass::TypeC && (500_000..532_800).contains(&clock_khz) {
        return HdmiModeStatus::ClockRange;
    }
    HdmiModeStatus::Ok
}

// upstream: intel_hdmi.c intel_hdmi_tmds_clock()
pub const fn intel_hdmi_tmds_clock(pixel_clock_khz: i32, bpc: i32, format: OutputFormat) -> i32 {
    let clock = if matches!(format, OutputFormat::Ycbcr420) {
        pixel_clock_khz / 2
    } else {
        pixel_clock_khz
    };
    (clock * bpc + 4) / 8
}

// upstream: intel_hdmi.c intel_hdmi_source_bpc_possible()
pub const fn intel_hdmi_source_bpc_possible(display_version: u8, has_gmch: bool, bpc: i32) -> bool {
    match bpc {
        12 => !has_gmch,
        10 => display_version >= 11,
        8 => true,
        _ => false,
    }
}

// upstream: intel_hdmi.c intel_hdmi_sink_bpc_possible()
pub const fn intel_hdmi_sink_bpc_possible(
    sink: SinkCapabilities,
    bpc: i32,
    format: OutputFormat,
) -> bool {
    match bpc {
        12 if !sink.has_hdmi_sink => false,
        12 => {
            if matches!(format, OutputFormat::Ycbcr420) {
                sink.y420_12bpc
            } else {
                sink.rgb_12bpc
            }
        }
        10 if !sink.has_hdmi_sink => false,
        10 => {
            if matches!(format, OutputFormat::Ycbcr420) {
                sink.y420_10bpc
            } else {
                sink.rgb_10bpc
            }
        }
        8 => true,
        _ => false,
    }
}

// upstream: intel_hdmi.c intel_hdmi_mode_clock_valid()
pub fn intel_hdmi_mode_clock_valid(
    clock_khz: i32,
    format: OutputFormat,
    sink: SinkCapabilities,
    limits: ClockLimits,
) -> HdmiModeStatus {
    let mut status = HdmiModeStatus::Ok;
    let mut bpc = 12;
    while bpc >= 8 {
        let tmds = intel_hdmi_tmds_clock(clock_khz, bpc, format);
        if intel_hdmi_source_bpc_possible(sink.display_version, sink.gmch, bpc)
            && intel_hdmi_sink_bpc_possible(sink, bpc, format)
        {
            status = hdmi_port_clock_valid(tmds, limits);
            if status == HdmiModeStatus::Ok {
                return HdmiModeStatus::Ok;
            }
        }
        bpc -= 2;
    }
    status
}

// upstream: intel_hdmi.c intel_hdmi_sink_format_valid()
pub const fn intel_hdmi_sink_format_valid(
    sink: SinkCapabilities,
    mode_is_420: bool,
    format: OutputFormat,
) -> HdmiModeStatus {
    match format {
        OutputFormat::Ycbcr420 if !sink.has_hdmi_sink || !sink.ycbcr420_allowed || !mode_is_420 => {
            HdmiModeStatus::No420
        }
        OutputFormat::Ycbcr420 | OutputFormat::Rgb => HdmiModeStatus::Ok,
        _ => HdmiModeStatus::Bad,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeRequest {
    pub mode: HdmiMode,
    pub adjusted_clock_khz: i32,
    pub max_dotclk_khz: i32,
    pub sink: SinkCapabilities,
    pub limits: ClockLimits,
    pub mode_is_420: bool,
    pub mode_is_420_only: bool,
    pub mode_is_420_also: bool,
    pub has_hdmi_sink: bool,
    pub interlace_allowed: bool,
}
pub trait ModeFramework {
    fn cpu_transcoder_mode_valid(&mut self, mode: HdmiMode) -> HdmiModeStatus;
    fn pfit_mode_valid(&mut self, mode: HdmiMode, format: OutputFormat) -> HdmiModeStatus;
    fn max_plane_size_valid(&mut self, mode: HdmiMode, planes: u8) -> HdmiModeStatus;
}

// upstream: intel_hdmi.c intel_hdmi_mode_valid_format()
pub fn intel_hdmi_mode_valid_format(
    req: ModeRequest,
    format: OutputFormat,
    fw: &mut impl ModeFramework,
) -> HdmiModeStatus {
    let status = intel_hdmi_sink_format_valid(req.sink, req.mode_is_420, format);
    if status != HdmiModeStatus::Ok {
        return status;
    }
    let status = fw.pfit_mode_valid(req.mode, format);
    if status != HdmiModeStatus::Ok {
        return status;
    }
    intel_hdmi_mode_clock_valid(req.adjusted_clock_khz, format, req.sink, req.limits)
}

// upstream: intel_hdmi.c intel_hdmi_mode_valid()
pub fn intel_hdmi_mode_valid(mut req: ModeRequest, fw: &mut impl ModeFramework) -> HdmiModeStatus {
    let mut status = fw.cpu_transcoder_mode_valid(req.mode);
    if status != HdmiModeStatus::Ok {
        return status;
    }
    if req.mode.flags & DRM_MODE_FLAG_3D_MASK == DRM_MODE_FLAG_3D_FRAME_PACKING {
        req.adjusted_clock_khz *= 2;
    }
    if req.adjusted_clock_khz > req.max_dotclk_khz {
        return HdmiModeStatus::ClockHigh;
    }
    if req.mode.flags & DRM_MODE_FLAG_DBLCLK != 0 {
        if !req.has_hdmi_sink {
            return HdmiModeStatus::ClockLow;
        }
        req.adjusted_clock_khz *= 2;
    }
    if req.adjusted_clock_khz > 600_000 {
        return HdmiModeStatus::ClockHigh;
    }
    if req.mode_is_420_only {
        status = intel_hdmi_mode_valid_format(req, OutputFormat::Ycbcr420, fw);
    } else {
        status = intel_hdmi_mode_valid_format(req, OutputFormat::Rgb, fw);
        if status != HdmiModeStatus::Ok && req.mode_is_420_also {
            status = intel_hdmi_mode_valid_format(req, OutputFormat::Ycbcr420, fw);
        }
    }
    if status != HdmiModeStatus::Ok {
        return status;
    }
    fw.max_plane_size_valid(req.mode, 1)
}

// upstream: intel_hdmi.c intel_hdmi_bpc_possible()
pub fn intel_hdmi_bpc_possible(
    connectors: &[SinkCapabilities],
    bpc: i32,
    format: OutputFormat,
    has_hdmi_sink: bool,
) -> bool {
    connectors.iter().all(|sink| {
        let mut sink = *sink;
        sink.has_hdmi_sink = has_hdmi_sink;
        intel_hdmi_sink_bpc_possible(sink, bpc, format)
    })
}

// upstream: intel_hdmi.c hdmi_bpc_possible()
pub fn hdmi_bpc_possible(
    sink: SinkCapabilities,
    bpc: i32,
    format: OutputFormat,
    mode: HdmiMode,
) -> bool {
    if !intel_hdmi_source_bpc_possible(sink.display_version, sink.gmch, bpc) {
        return false;
    }
    if matches!(format, OutputFormat::Ycbcr420)
        && bpc == 10
        && sink.display_version == 11
        && (mode.hblank_end - mode.hblank_start) % 8 == 2
    {
        return false;
    }
    intel_hdmi_sink_bpc_possible(sink, bpc, format)
}

// upstream: intel_hdmi.c intel_hdmi_compute_bpc()
pub fn intel_hdmi_compute_bpc(
    pipe_bpp: i32,
    clock_khz: i32,
    format: OutputFormat,
    sink: SinkCapabilities,
    mode: HdmiMode,
    limits: ClockLimits,
    respect_downstream_limits: bool,
) -> Option<i32> {
    let mut bpc = (pipe_bpp / 3).max(8);
    if !respect_downstream_limits {
        bpc = 8;
    }
    let limits = ClockLimits {
        respect_downstream_limits,
        ..limits
    };
    while bpc >= 8 {
        let tmds = intel_hdmi_tmds_clock(clock_khz, bpc, format);
        if hdmi_bpc_possible(sink, bpc, format, mode)
            && hdmi_port_clock_valid(tmds, limits) == HdmiModeStatus::Ok
        {
            return Some(bpc);
        }
        bpc -= 2;
    }
    None
}

// upstream: intel_hdmi.c intel_hdmi_compute_clock()
pub fn intel_hdmi_compute_clock(
    pipe_bpp: &mut i32,
    adjusted_mode: HdmiMode,
    format: OutputFormat,
    sink: SinkCapabilities,
    limits: ClockLimits,
    respect_downstream_limits: bool,
) -> Option<i32> {
    let mut clock = adjusted_mode.clock_khz;
    if adjusted_mode.flags & DRM_MODE_FLAG_DBLCLK != 0 {
        clock *= 2;
    }
    let bpc = intel_hdmi_compute_bpc(
        *pipe_bpp,
        clock,
        format,
        sink,
        adjusted_mode,
        limits,
        respect_downstream_limits,
    )?;
    let port_clock = intel_hdmi_tmds_clock(clock, bpc, format);
    *pipe_bpp = (*pipe_bpp).min(bpc * 3);
    Some(port_clock)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BroadcastRgb {
    Auto,
    Full,
    Limited,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForceAudio {
    Auto,
    On,
    OffDvi,
}

// upstream: intel_hdmi.c intel_hdmi_limited_color_range()
pub fn intel_hdmi_limited_color_range(
    format: OutputFormat,
    has_hdmi_sink: bool,
    broadcast_rgb: BroadcastRgb,
    default_is_limited: bool,
) -> bool {
    if format != OutputFormat::Rgb {
        return false;
    }
    match broadcast_rgb {
        BroadcastRgb::Auto => has_hdmi_sink && default_is_limited,
        BroadcastRgb::Full => false,
        BroadcastRgb::Limited => true,
    }
}

// upstream: intel_hdmi.c intel_hdmi_has_audio()
pub const fn intel_hdmi_has_audio(
    has_hdmi_sink: bool,
    force: ForceAudio,
    sink_has_audio: bool,
) -> bool {
    if !has_hdmi_sink {
        return false;
    }
    match force {
        ForceAudio::Auto => sink_has_audio,
        ForceAudio::On => true,
        ForceAudio::OffDvi => false,
    }
}

// upstream: intel_hdmi.c intel_hdmi_output_format()
pub const fn intel_hdmi_output_format(sink_format: OutputFormat) -> OutputFormat {
    sink_format
}

// upstream: intel_hdmi.c intel_hdmi_compute_output_format()
pub fn intel_hdmi_compute_output_format(
    format: OutputFormat,
    sink: SinkCapabilities,
    mode_is_420: bool,
    mode: HdmiMode,
    limits: ClockLimits,
    pipe_bpp: &mut i32,
    respect_downstream_limits: bool,
) -> Result<i32, i32> {
    if intel_hdmi_sink_format_valid(sink, mode_is_420, format) != HdmiModeStatus::Ok {
        return Err(-22);
    }
    intel_hdmi_compute_clock(
        pipe_bpp,
        mode,
        intel_hdmi_output_format(format),
        sink,
        limits,
        respect_downstream_limits,
    )
    .ok_or(-22)
}

// upstream: intel_hdmi.c intel_hdmi_compute_formats()
pub fn intel_hdmi_compute_formats(
    sink: SinkCapabilities,
    mode: HdmiMode,
    mode_is_420: bool,
    mode_is_420_only: bool,
    mode_is_420_also: bool,
    limits: ClockLimits,
    pipe_bpp: &mut i32,
    respect_downstream_limits: bool,
) -> Result<(OutputFormat, i32), i32> {
    if mode_is_420_only {
        match intel_hdmi_compute_output_format(
            OutputFormat::Ycbcr420,
            sink,
            mode_is_420,
            mode,
            limits,
            pipe_bpp,
            respect_downstream_limits,
        ) {
            Ok(clock) => Ok((OutputFormat::Ycbcr420, clock)),
            Err(_) if !respect_downstream_limits => intel_hdmi_compute_output_format(
                OutputFormat::Rgb,
                sink,
                mode_is_420,
                mode,
                limits,
                pipe_bpp,
                respect_downstream_limits,
            )
            .map(|clock| (OutputFormat::Rgb, clock)),
            Err(e) => Err(e),
        }
    } else {
        match intel_hdmi_compute_output_format(
            OutputFormat::Rgb,
            sink,
            mode_is_420,
            mode,
            limits,
            pipe_bpp,
            respect_downstream_limits,
        ) {
            Ok(clock) => Ok((OutputFormat::Rgb, clock)),
            Err(_) if mode_is_420_also => intel_hdmi_compute_output_format(
                OutputFormat::Ycbcr420,
                sink,
                mode_is_420,
                mode,
                limits,
                pipe_bpp,
                respect_downstream_limits,
            )
            .map(|clock| (OutputFormat::Ycbcr420, clock)),
            Err(e) => Err(e),
        }
    }
}

// upstream: intel_hdmi.c intel_hdmi_is_cloned()
pub const fn intel_hdmi_is_cloned(encoder_mask: u32) -> bool {
    encoder_mask != 0 && encoder_mask & (encoder_mask - 1) != 0
}

// upstream: intel_hdmi.c source_supports_scrambling()
pub const fn source_supports_scrambling(source_max_tmds_clock_khz: i32) -> bool {
    source_max_tmds_clock_khz > 340_000
}

// upstream: intel_hdmi.c intel_hdmi_compute_has_hdmi_sink()
pub const fn intel_hdmi_compute_has_hdmi_sink(has_hdmi_sink: bool, encoder_mask: u32) -> bool {
    has_hdmi_sink && !intel_hdmi_is_cloned(encoder_mask)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiComputeState {
    pub mode: HdmiMode,
    pub sink: SinkCapabilities,
    pub limits: ClockLimits,
    pub output_format: OutputFormat,
    pub pipe_bpp: i32,
    pub port_clock_khz: i32,
    pub pixel_multiplier: u8,
    pub lane_count: u8,
    pub picture_aspect_ratio: u8,
    pub has_infoframe: bool,
    pub has_audio: bool,
    pub limited_color_range: bool,
    pub hdmi_scrambling: bool,
    pub high_tmds_clock_ratio: bool,
    pub gcp: u32,
    pub infoframes_enabled: u32,
    pub g4x: bool,
    pub has_hdmi_infoframe: bool,
    pub display_version: u8,
    pub hdr_metadata: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiConnectorState {
    pub mode_is_420: bool,
    pub mode_is_420_only: bool,
    pub mode_is_420_also: bool,
    pub sink_has_audio: bool,
    pub force_audio: ForceAudio,
    pub broadcast_rgb: BroadcastRgb,
    pub default_rgb_limited: bool,
    pub has_scdc: bool,
    pub scdc_scrambling_supported: bool,
    pub scdc_low_rates: bool,
    pub aspect_ratio: u8,
    pub interlace_allowed: bool,
}
pub trait HdmiComputeFramework {
    fn link_bw_compute_pipe_bpp(&mut self, state: &mut HdmiComputeState) -> bool;
    fn audio_compute_config(&mut self, state: &mut HdmiComputeState) -> bool;
    fn pfit_compute_config(&mut self, state: &mut HdmiComputeState) -> i32;
    fn vrr_compute_config(&mut self, state: &mut HdmiComputeState);
    fn compute_avi_infoframe(&mut self, state: &mut HdmiComputeState) -> bool;
    fn compute_spd_infoframe(&mut self, state: &mut HdmiComputeState) -> bool;
    fn compute_hdmi_infoframe(&mut self, state: &mut HdmiComputeState) -> bool;
    fn compute_drm_infoframe(&mut self, state: &mut HdmiComputeState) -> bool;
    fn infoframe_error(&mut self, _kind: &'static str) {}
}

// upstream: intel_hdmi.c intel_hdmi_compute_config()
pub fn intel_hdmi_compute_config(
    state: &mut HdmiComputeState,
    connector: &HdmiConnectorState,
    fw: &mut impl HdmiComputeFramework,
) -> i32 {
    if state.mode.flags & (1 << 5) != 0 {
        return -22;
    } // DBLSCAN
    if !connector.interlace_allowed && state.mode.flags & DRM_MODE_FLAG_INTERLACE != 0 {
        return -22;
    }
    state.output_format = OutputFormat::Rgb;
    if state.sink.has_hdmi_sink {
        state.has_infoframe = true;
    }
    if state.mode.flags & DRM_MODE_FLAG_DBLCLK != 0 {
        state.pixel_multiplier = 2;
    }
    if !fw.link_bw_compute_pipe_bpp(state) {
        return -22;
    }
    state.has_audio = intel_hdmi_has_audio(
        state.sink.has_hdmi_sink,
        connector.force_audio,
        connector.sink_has_audio,
    ) && fw.audio_compute_config(state);
    let formatted = intel_hdmi_compute_formats(
        state.sink,
        state.mode,
        connector.mode_is_420,
        connector.mode_is_420_only,
        connector.mode_is_420_also,
        state.limits,
        &mut state.pipe_bpp,
        true,
    )
    .or_else(|_| {
        intel_hdmi_compute_formats(
            state.sink,
            state.mode,
            connector.mode_is_420,
            connector.mode_is_420_only,
            connector.mode_is_420_also,
            state.limits,
            &mut state.pipe_bpp,
            false,
        )
    });
    let (format, port_clock) = match formatted {
        Ok(x) => x,
        Err(e) => return e,
    };
    state.output_format = format;
    state.port_clock_khz = port_clock;
    let ret = fw.pfit_compute_config(state);
    if ret != 0 {
        return ret;
    }
    state.limited_color_range = intel_hdmi_limited_color_range(
        format,
        state.sink.has_hdmi_sink,
        connector.broadcast_rgb,
        connector.default_rgb_limited,
    );
    if connector.aspect_ratio != 0 {
        state.picture_aspect_ratio = connector.aspect_ratio;
    }
    state.lane_count = 4;
    if connector.scdc_scrambling_supported
        && source_supports_scrambling(state.limits.source_limit_khz)
    {
        if connector.scdc_low_rates {
            state.hdmi_scrambling = true;
        }
        if state.port_clock_khz > 340_000 {
            state.hdmi_scrambling = true;
            state.high_tmds_clock_ratio = true;
        }
    }
    fw.vrr_compute_config(state);
    let mut gcp_state = InfoframeState {
        has_infoframe: state.has_infoframe,
        enabled: state.infoframes_enabled,
        output_format: state.output_format,
        limited_color_range: state.limited_color_range,
        has_hdmi_infoframe: state.has_hdmi_infoframe,
        display_version: state.display_version,
        discrete_graphics: false,
        hdr_metadata: state.hdr_metadata,
        pipe_bpp: state.pipe_bpp.clamp(0, u8::MAX as i32) as u8,
        gcp: state.gcp,
        mode: state.mode,
    };
    intel_hdmi_compute_gcp_infoframe(&mut gcp_state, state.g4x);
    state.infoframes_enabled = gcp_state.enabled;
    state.gcp = gcp_state.gcp;
    if !fw.compute_avi_infoframe(state) {
        fw.infoframe_error("AVI");
        return -22;
    }
    if !fw.compute_spd_infoframe(state) {
        fw.infoframe_error("SPD");
        return -22;
    }
    if !fw.compute_hdmi_infoframe(state) {
        fw.infoframe_error("HDMI vendor");
        return -22;
    }
    if !fw.compute_drm_infoframe(state) {
        fw.infoframe_error("DRM");
        return -22;
    }
    0
}

// upstream: intel_hdmi.c intel_hdmi_encoder_shutdown()
pub fn intel_hdmi_encoder_shutdown(io: &mut impl DualModeTmdsIo, dual_mode_type: u8) {
    if dual_mode_type >= 2 {
        io.set_dual_mode_tmds_output(dual_mode_type, true);
    }
}

// Upstream framework-only callbacks intentionally omitted:
// `intel_hdmi_add_properties`, `intel_hdmi_connector_register`,
// `intel_hdmi_connector_unregister`, `intel_hdmi_connector_atomic_check`,
// `intel_hdmi_get_modes` and `intel_infoframe_init`.
// These only install DRM connector/property/callback tables or choose function
// pointers; the runtime policy and register callbacks they select are translated.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DualModeType {
    None,
    Unknown,
    Type1Dvi,
    Type2Dvi,
    Type2Hdmi,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DualModeState {
    pub kind: DualModeType,
    pub max_tmds_clock_khz: i32,
}
pub trait DualModeIo {
    fn detect_dual_mode(&mut self) -> DualModeType;
    fn dual_mode_max_tmds_clock(&mut self, kind: DualModeType) -> i32;
    fn vbt_supports_dual_mode(&self) -> bool;
    fn is_forced(&self) -> bool;
    fn display_version(&self) -> u8;
    fn haswell(&self) -> bool;
    fn native_hdmi_vbt_support(&self) -> bool;
    fn pch_type(&self) -> u8;
    fn debug(&mut self, _message: &'static str) {}
}

// upstream: intel_hdmi.c intel_hdmi_unset_edid()
pub fn intel_hdmi_unset_edid(state: &mut DualModeState, free_edid: impl FnOnce()) {
    state.kind = DualModeType::None;
    state.max_tmds_clock_khz = 0;
    free_edid();
}

// upstream: intel_hdmi.c intel_hdmi_dp_dual_mode_detect()
pub fn intel_hdmi_dp_dual_mode_detect(io: &mut impl DualModeIo, state: &mut DualModeState) {
    let mut kind = io.detect_dual_mode();
    if kind == DualModeType::Unknown {
        if !io.is_forced() && io.vbt_supports_dual_mode() {
            io.debug("Assuming DP dual-mode adaptor presence from VBT");
            kind = DualModeType::Type1Dvi;
        } else {
            kind = DualModeType::None;
        }
    }
    if kind == DualModeType::None {
        return;
    }
    state.kind = kind;
    state.max_tmds_clock_khz = io.dual_mode_max_tmds_clock(kind);
    if (io.display_version() >= 8 || io.haswell()) && !io.native_hdmi_vbt_support() {
        io.debug("Ignoring dual-mode TMDS limit for native HDMI port");
        state.max_tmds_clock_khz = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdidSnapshot {
    pub digital: bool,
    pub source_physical_address: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorStatus {
    Connected,
    Disconnected,
    Unknown,
}
pub trait HdmiDetectIo: DualModeIo {
    type PowerToken;
    fn display_device_enabled(&self) -> bool;
    fn driver_access_allowed(&self) -> bool;
    fn current_status(&self) -> ConnectorStatus;
    fn digital_port_connected(&mut self) -> bool;
    fn get_gmbus_power(&mut self) -> Self::PowerToken;
    fn put_gmbus_power(&mut self, token: Self::PowerToken);
    fn read_edid_ddc(&mut self) -> Option<EdidSnapshot>;
    fn gmbus_forced_bit(&self) -> bool;
    fn force_gpio_bit(&mut self, force: bool);
    fn update_connector_edid(&mut self, edid: Option<EdidSnapshot>);
    fn cec_set_physical_address(&mut self, address: u16);
    fn cec_invalidate_physical_address(&mut self);
}

// upstream: intel_hdmi.c intel_hdmi_set_edid()
pub fn intel_hdmi_set_edid(
    io: &mut impl HdmiDetectIo,
    state: &mut DualModeState,
    detect_edid: &mut Option<EdidSnapshot>,
) -> bool {
    let power = io.get_gmbus_power();
    let mut edid = io.read_edid_ddc();
    if edid.is_none() && !io.gmbus_forced_bit() {
        io.debug("HDMI GMBUS EDID read failed; retry with GPIO bit-banging");
        io.force_gpio_bit(true);
        edid = io.read_edid_ddc();
        io.force_gpio_bit(false);
    }
    io.update_connector_edid(edid);
    *detect_edid = edid;
    let connected = if edid.is_some_and(|e| e.digital) {
        intel_hdmi_dp_dual_mode_detect(io, state);
        true
    } else {
        false
    };
    let address = edid.map_or(0xffff, |e| e.source_physical_address);
    io.put_gmbus_power(power);
    io.cec_set_physical_address(address);
    connected
}

// upstream: intel_hdmi.c intel_hdmi_detect()
pub fn intel_hdmi_detect(
    io: &mut impl HdmiDetectIo,
    state: &mut DualModeState,
    detect_edid: &mut Option<EdidSnapshot>,
) -> ConnectorStatus {
    if !io.display_device_enabled() {
        return ConnectorStatus::Disconnected;
    }
    if !io.driver_access_allowed() {
        return io.current_status();
    }
    let power = io.get_gmbus_power();
    let mut status = ConnectorStatus::Disconnected;
    if io.display_version() < 11 || io.digital_port_connected() {
        intel_hdmi_unset_edid(state, || {
            *detect_edid = None;
        });
        if intel_hdmi_set_edid(io, state, detect_edid) {
            status = ConnectorStatus::Connected;
        }
    }
    io.put_gmbus_power(power);
    if status != ConnectorStatus::Connected {
        io.cec_invalidate_physical_address();
    }
    status
}

// upstream: intel_hdmi.c intel_hdmi_force()
pub fn intel_hdmi_force(
    io: &mut impl HdmiDetectIo,
    state: &mut DualModeState,
    detect_edid: &mut Option<EdidSnapshot>,
) {
    if !io.driver_access_allowed() {
        return;
    }
    intel_hdmi_unset_edid(state, || {
        *detect_edid = None;
    });
    if io.current_status() != ConnectorStatus::Connected {
        return;
    }
    let _ = intel_hdmi_set_edid(io, state, detect_edid);
}

pub trait ScdcIo {
    fn scrambling_supported(&self) -> bool;
    fn get_scrambling_status(&mut self) -> bool;
    fn set_high_tmds_clock_ratio(&mut self, enable: bool) -> bool;
    fn set_scrambling(&mut self, enable: bool) -> bool;
    fn delay_us_range(&mut self, minimum: u32, maximum: u32);
    fn debug(&mut self, _message: &'static str) {}
}

// upstream: intel_hdmi.c intel_hdmi_poll_for_scrambling_enable()
pub fn intel_hdmi_poll_for_scrambling_enable(io: &mut impl ScdcIo, hdmi_scrambling: bool) -> bool {
    if !hdmi_scrambling {
        return true;
    }
    for _ in 0..200 {
        if io.get_scrambling_status() {
            return true;
        }
        io.delay_us_range(1000, 1000);
    }
    io.debug("Timed out waiting for scrambling enable");
    false
}

// upstream: intel_hdmi.c intel_hdmi_handle_sink_scrambling()
pub fn intel_hdmi_handle_sink_scrambling(
    io: &mut impl ScdcIo,
    high_tmds_clock_ratio: bool,
    scrambling: bool,
) -> bool {
    if !io.scrambling_supported() {
        return true;
    }
    io.set_high_tmds_clock_ratio(high_tmds_clock_ratio) && io.set_scrambling(scrambling)
}

pub const GMBUS_PIN_DPD_CHV: u8 = 3;
pub const GMBUS_PIN_DPC: u8 = 4;
pub const GMBUS_PIN_DPB: u8 = 5;
pub const GMBUS_PIN_DPD: u8 = 6;
pub const GMBUS_PIN_1_BXT: u8 = 1;
pub const GMBUS_PIN_2_BXT: u8 = 2;
pub const GMBUS_PIN_3_BXT: u8 = 3;
pub const GMBUS_PIN_4_CNP: u8 = 4;
pub const GMBUS_PIN_9_TC1_ICP: u8 = 9;
pub const PORT_B: u8 = 1;
pub const PORT_C: u8 = 2;
pub const PORT_D: u8 = 3;
pub const PORT_F: u8 = 5;
pub const PHY_A: u8 = 0;
pub const PHY_B: u8 = 1;
pub const PHY_C: u8 = 2;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdcEncoder {
    pub port: u8,
    pub phy: u8,
    pub tc_index: u8,
}

// upstream: intel_hdmi.c chv_encoder_to_ddc_pin()
pub const fn chv_encoder_to_ddc_pin(port: u8) -> u8 {
    match port {
        PORT_B => GMBUS_PIN_DPB,
        PORT_C => GMBUS_PIN_DPC,
        PORT_D => GMBUS_PIN_DPD_CHV,
        _ => GMBUS_PIN_DPB,
    }
}
// upstream: intel_hdmi.c bxt_encoder_to_ddc_pin()
pub const fn bxt_encoder_to_ddc_pin(port: u8) -> u8 {
    match port {
        PORT_B => GMBUS_PIN_1_BXT,
        PORT_C => GMBUS_PIN_2_BXT,
        _ => GMBUS_PIN_1_BXT,
    }
}
// upstream: intel_hdmi.c cnp_encoder_to_ddc_pin()
pub const fn cnp_encoder_to_ddc_pin(port: u8) -> u8 {
    match port {
        PORT_B => GMBUS_PIN_1_BXT,
        PORT_C => GMBUS_PIN_2_BXT,
        PORT_D => GMBUS_PIN_4_CNP,
        PORT_F => GMBUS_PIN_3_BXT,
        _ => GMBUS_PIN_1_BXT,
    }
}
// upstream: intel_hdmi.c icl_encoder_to_ddc_pin()
pub const fn icl_encoder_to_ddc_pin(combo: bool, tc: bool, port: u8, tc_index: u8) -> u8 {
    if combo {
        GMBUS_PIN_1_BXT + port
    } else if tc {
        GMBUS_PIN_9_TC1_ICP + tc_index
    } else {
        GMBUS_PIN_2_BXT
    }
}
// upstream: intel_hdmi.c mcc_encoder_to_ddc_pin()
pub const fn mcc_encoder_to_ddc_pin(phy: u8) -> u8 {
    match phy {
        PHY_A => GMBUS_PIN_1_BXT,
        PHY_B => GMBUS_PIN_2_BXT,
        PHY_C => GMBUS_PIN_9_TC1_ICP,
        _ => GMBUS_PIN_1_BXT,
    }
}
// upstream: intel_hdmi.c rkl_encoder_to_ddc_pin()
pub const fn rkl_encoder_to_ddc_pin(phy: u8, pch_tgp: bool) -> u8 {
    if pch_tgp && phy >= PHY_C {
        GMBUS_PIN_9_TC1_ICP + (phy - PHY_C)
    } else {
        GMBUS_PIN_1_BXT + phy
    }
}
// upstream: intel_hdmi.c gen9bc_tgp_encoder_to_ddc_pin()
pub const fn gen9bc_tgp_encoder_to_ddc_pin(phy: u8, pch_tgp: bool) -> u8 {
    if pch_tgp && phy >= PHY_C {
        GMBUS_PIN_9_TC1_ICP + (phy - PHY_C)
    } else {
        GMBUS_PIN_1_BXT + phy
    }
}
// upstream: intel_hdmi.c dg1_encoder_to_ddc_pin()
pub const fn dg1_encoder_to_ddc_pin(phy: u8) -> u8 {
    phy + 1
}
// upstream: intel_hdmi.c adls_encoder_to_ddc_pin()
pub const fn adls_encoder_to_ddc_pin(phy: u8) -> u8 {
    if phy == PHY_A {
        GMBUS_PIN_1_BXT
    } else {
        GMBUS_PIN_9_TC1_ICP + (phy - PHY_B)
    }
}
// upstream: intel_hdmi.c g4x_encoder_to_ddc_pin()
pub const fn g4x_encoder_to_ddc_pin(port: u8) -> u8 {
    match port {
        PORT_B => GMBUS_PIN_DPB,
        PORT_C => GMBUS_PIN_DPC,
        PORT_D => GMBUS_PIN_DPD,
        _ => GMBUS_PIN_DPB,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdcPlatform {
    pub alder_lake_s: bool,
    pub display_version: u8,
    pub dg1_or_newer: bool,
    pub rocketlake: bool,
    pub gen9: bool,
    pub jasperlake_or_elkhartlake: bool,
    pub pch_tgp: bool,
    pub pch_cnp: bool,
    pub pch_icp_or_newer: bool,
    pub geminilake_or_broxton: bool,
    pub cherryview: bool,
}

// upstream: intel_hdmi.c intel_hdmi_default_ddc_pin()
pub const fn intel_hdmi_default_ddc_pin(platform: DdcPlatform, encoder: DdcEncoder) -> u8 {
    if platform.alder_lake_s {
        adls_encoder_to_ddc_pin(encoder.phy)
    } else if platform.dg1_or_newer {
        dg1_encoder_to_ddc_pin(encoder.phy)
    } else if platform.rocketlake {
        rkl_encoder_to_ddc_pin(encoder.phy, platform.pch_tgp)
    } else if platform.gen9 && platform.pch_tgp {
        gen9bc_tgp_encoder_to_ddc_pin(encoder.phy, platform.pch_tgp)
    } else if platform.jasperlake_or_elkhartlake && platform.pch_tgp {
        mcc_encoder_to_ddc_pin(encoder.phy)
    } else if platform.pch_icp_or_newer {
        icl_encoder_to_ddc_pin(
            encoder.phy == PHY_A || encoder.phy == PHY_B,
            encoder.phy >= PHY_C,
            encoder.port,
            encoder.tc_index,
        )
    } else if platform.pch_cnp {
        cnp_encoder_to_ddc_pin(encoder.port)
    } else if platform.geminilake_or_broxton {
        bxt_encoder_to_ddc_pin(encoder.port)
    } else if platform.cherryview {
        chv_encoder_to_ddc_pin(encoder.port)
    } else {
        g4x_encoder_to_ddc_pin(encoder.port)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdcAttachedEncoder {
    pub encoder_id: u32,
    pub is_digital_port: bool,
    pub attached: bool,
    pub ddc_pin: u8,
}

// upstream: intel_hdmi.c get_encoder_by_ddc_pin()
pub fn get_encoder_by_ddc_pin(
    current_encoder: u32,
    ddc_pin: u8,
    encoders: &[DdcAttachedEncoder],
) -> Option<u32> {
    for other in encoders {
        if other.encoder_id == current_encoder {
            continue;
        }
        if !other.is_digital_port {
            continue;
        }
        if other.attached && other.ddc_pin == ddc_pin {
            return Some(other.encoder_id);
        }
    }
    None
}

pub trait DdcIo {
    fn vbt_ddc_pin(&mut self) -> u8;
    fn valid_pin(&mut self, pin: u8) -> bool;
    fn log_invalid_pin(&mut self, _pin: u8) {}
    fn log_claimed_pin(&mut self, _pin: u8, _owner: u32) {}
    fn log_selected_pin(&mut self, _pin: u8, _from_vbt: bool) {}
}

// upstream: intel_hdmi.c intel_hdmi_ddc_pin()
pub fn intel_hdmi_ddc_pin(
    io: &mut impl DdcIo,
    current_encoder: u32,
    platform: DdcPlatform,
    encoder: DdcEncoder,
    encoders: &[DdcAttachedEncoder],
) -> u8 {
    let mut pin = io.vbt_ddc_pin();
    let from_vbt = pin != 0;
    if !from_vbt {
        pin = intel_hdmi_default_ddc_pin(platform, encoder);
    }
    if !io.valid_pin(pin) {
        io.log_invalid_pin(pin);
        return 0;
    }
    if let Some(owner) = get_encoder_by_ddc_pin(current_encoder, pin, encoders) {
        io.log_claimed_pin(pin, owner);
        return 0;
    }
    io.log_selected_pin(pin, from_vbt);
    pin
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiConnectorSetup {
    pub allow_interlace: bool,
    pub allow_stereo: bool,
    pub allow_ycbcr420: bool,
    pub polled_hpd: bool,
    pub ddi_hw_state: bool,
}
pub trait HdmiConnectorInitIo {
    fn display_version(&self) -> u8;
    fn port(&self) -> u8;
    fn max_lanes(&self) -> u8;
    fn has_ddi(&self) -> bool;
    fn ddc_pin(&mut self) -> u8;
    fn connector_init_with_ddc(&mut self, pin: u8);
    fn connector_helper_add(&mut self);
    fn configure_connector(&mut self, setup: HdmiConnectorSetup);
    fn add_hdmi_properties(&mut self);
    fn attach_encoder(&mut self);
    fn store_attached_connector(&mut self);
    fn hdcp_supported(&mut self) -> bool;
    fn hdcp_init(&mut self) -> i32;
    fn cec_register(&mut self) -> bool;
    fn debug(&mut self, _message: &'static str) {}
}

// upstream: intel_hdmi.c intel_hdmi_init_connector()
pub fn intel_hdmi_init_connector(io: &mut impl HdmiConnectorInitIo) -> bool {
    let display_version = io.display_version();
    let port = io.port();
    if display_version < 12 && port == 0 {
        return false;
    }
    if io.max_lanes() < 4 {
        io.debug("Not enough lanes for HDMI connector");
        return false;
    }
    let pin = io.ddc_pin();
    if pin == 0 {
        return false;
    }
    io.connector_init_with_ddc(pin);
    io.connector_helper_add();
    io.configure_connector(HdmiConnectorSetup {
        allow_interlace: display_version < 12,
        allow_stereo: true,
        allow_ycbcr420: display_version >= 10,
        polled_hpd: true,
        ddi_hw_state: io.has_ddi(),
    });
    io.add_hdmi_properties();
    io.attach_encoder();
    io.store_attached_connector();
    if io.hdcp_supported() && io.hdcp_init() != 0 {
        io.debug("HDCP init failed; skipping");
    }
    if !io.cec_register() {
        io.debug("CEC notifier registration failed");
    }
    true
}

// Upstream framework-only adapter wrappers intentionally omitted:
// `intel_hdmi_connector_register`/`intel_hdmi_connector_unregister` only
// forward connector lifecycle to DRM and unregister CEC; `intel_hdmi_get_modes`
// forwards to drm_edid_connector_add_modes; `intel_hdmi_connector_atomic_check`
// dispatches to DRM atomic helpers; `intel_hdmi_add_properties` attaches DRM
// properties; `intel_infoframe_init` only assigns function pointers for the selected engine.

// upstream: intel_hdmi.c intel_hdmi_dsc_get_slice_height()
pub const fn intel_hdmi_dsc_get_slice_height(vactive: i32) -> i32 {
    let mut slice_height = 96;
    while slice_height <= vactive {
        if vactive % slice_height == 0 {
            return slice_height;
        }
        slice_height += 2;
    }
    0
}

// upstream: intel_hdmi.c intel_hdmi_dsc_get_num_slices()
pub fn intel_hdmi_dsc_get_num_slices(
    pixel_clock_khz: i32,
    hdisplay: i32,
    output_format: OutputFormat,
    source_max_slices: i32,
    source_max_slice_width: i32,
    sink_max_slices: i32,
    sink_throughput_mhz: i32,
) -> i32 {
    const PEAK_PIXEL_RATE: i32 = 2_720_000;
    const THROUGHPUT_340_MHZ: i32 = 340_000;
    const THROUGHPUT_400_MHZ: i32 = 400_000;
    const MAX_HDMI_SLICE_WIDTH: i32 = 2720;
    if sink_throughput_mhz == 0 {
        return 0;
    }
    let kslice_adjust = if matches!(output_format, OutputFormat::Ycbcr444 | OutputFormat::Rgb) {
        10
    } else {
        5
    };
    let adjusted_clk_khz = (kslice_adjust * pixel_clock_khz + 9) / 10;
    let max_throughput = (if adjusted_clk_khz <= PEAK_PIXEL_RATE {
        THROUGHPUT_340_MHZ
    } else {
        THROUGHPUT_400_MHZ
    })
    .min(sink_throughput_mhz * 1000);
    let mut min_slices = (adjusted_clk_khz + max_throughput - 1) / max_throughput;
    let max_slice_width = MAX_HDMI_SLICE_WIDTH.min(source_max_slice_width);
    loop {
        let target_slices = if min_slices <= 1 && source_max_slices >= 1 && sink_max_slices >= 1 {
            1
        } else if min_slices <= 2 && source_max_slices >= 2 && sink_max_slices >= 2 {
            2
        } else if min_slices <= 4 && source_max_slices >= 4 && sink_max_slices >= 4 {
            4
        } else if min_slices <= 8 && source_max_slices >= 8 && sink_max_slices >= 8 {
            8
        } else if min_slices <= 12 && source_max_slices >= 12 && sink_max_slices >= 12 {
            12
        } else if min_slices <= 16 && source_max_slices >= 16 && sink_max_slices >= 16 {
            16
        } else {
            return 0;
        };
        let slice_width = (hdisplay + target_slices - 1) / target_slices;
        if slice_width < max_slice_width {
            return target_slices;
        }
        min_slices = target_slices + 1;
    }
}

// upstream: intel_hdmi.c intel_hdmi_dsc_get_bpp()
pub fn intel_hdmi_dsc_get_bpp(
    src_fractional_bpp: i32,
    slice_width: i32,
    num_slices: i32,
    output_format: OutputFormat,
    hdmi_all_bpp: bool,
    hdmi_max_chunk_bytes: i32,
) -> i32 {
    let (min_dsc_bpp, mut max_dsc_bpp) = match output_format {
        OutputFormat::Ycbcr420 => (6, 12),
        OutputFormat::Ycbcr444 | OutputFormat::Rgb => (8, 24),
        _ => (7, 16),
    };
    if !hdmi_all_bpp {
        max_dsc_bpp = max_dsc_bpp.min(12);
    }
    let fractional_bpp = if src_fractional_bpp == 0 {
        1
    } else {
        src_fractional_bpp
    };
    let bpp_decrement_x16 = (16 + fractional_bpp - 1) / fractional_bpp;
    let mut bpp_target_x16 = max_dsc_bpp * 16 - bpp_decrement_x16;
    while bpp_target_x16 > min_dsc_bpp * 16 {
        let bpp = (bpp_target_x16 + 15) / 16;
        let target_bytes = (num_slices * slice_width * bpp + 7) / 8;
        if target_bytes <= hdmi_max_chunk_bytes {
            return bpp_target_x16;
        }
        bpp_target_x16 -= bpp_decrement_x16;
    }
    0
}
