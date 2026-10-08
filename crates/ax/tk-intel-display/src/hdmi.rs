// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_hdmi.c:
// hsw_infoframes_enabled, hsw_infoframe_enable, hsw_dip_data_reg,
// hsw_read_infoframe, intel_read_infoframe, intel_hdmi_read_gcp_infoframe
// (display13 HDMI packet branches).
// Copyright 2006 Dave Airlie <airlied@linux.ie>
// Copyright © 2006-2009 Intel Corporation.
// intel_display_regs.h selected fields: Copyright © 2025 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. HDMI packet writes are typed and caller-owned.
use crate::{
    Error,
    device::Platform,
    display::{Pipe, ReadoutIo, Timings},
    hdmi_packet::{Avi, Drm, Infoframe, Spd, hdmi_infoframe_unpack},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameType {
    Avi,
    Spd,
    Vendor,
    Drm,
}
impl FrameType {
    pub const fn packet_type(self) -> u8 {
        match self {
            Self::Avi => 0x82,
            Self::Spd => 0x83,
            Self::Vendor => 0x81,
            Self::Drm => 0x87,
        }
    }
    pub const fn enable_mask(self) -> u32 {
        match self {
            Self::Avi => 1 << 12,
            Self::Spd => 1,
            Self::Vendor => 1 << 8,
            Self::Drm => 1 << 28,
        }
    }
    pub const fn data_base(self) -> u32 {
        match self {
            Self::Avi => 0x60220,
            Self::Spd => 0x602a0,
            Self::Vendor => 0x60260,
            Self::Drm => 0x60440,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawInfoframe {
    pub raw: [u8; 32],
    pub kind: FrameType,
}
impl RawInfoframe {
    /// HSW DIP has an ECC/reserved hole at byte3. Remove that hole, NOT the
    /// checksum at byte4; preserve the source memmove+offset result exactly.
    pub fn packet(self) -> [u8; 31] {
        let mut packet = [0; 31];
        packet[..3].copy_from_slice(&self.raw[..3]);
        packet[3..].copy_from_slice(&self.raw[4..]);
        packet
    }
    pub fn unpack(self) -> Result<Infoframe, Error> {
        let packet = self.packet();
        if packet[0] != self.kind.packet_type() {
            return Err(Error::InvalidHeader);
        }
        hdmi_infoframe_unpack(&packet)
    }
}
pub fn hsw_infoframes_enabled(raw: u32) -> u32 {
    // HAS_AS_SDP is true on display13. PPS is not in this source helper's mask;
    // full raw control is retained separately so admission can still reject it.
    raw & ((1 << 20) | (1 << 16) | (1 << 12) | (1 << 8) | (1 << 4) | 1 | (1 << 28) | (1 << 23))
}
/// Convert source hardware enable bits to the i915 software DIP indices.
pub fn intel_hdmi_infoframes_enabled(raw: u32) -> u32 {
    let enabled = hsw_infoframes_enabled(raw);
    let mut out = 0;
    for (index, bit) in [16, 4, 20, 23, 12, 0, 8, 28].into_iter().enumerate() {
        if enabled & (1 << bit) != 0 {
            out |= 1 << index;
        }
    }
    out
}
pub fn hsw_read_infoframe(
    io: &impl ReadoutIo,
    pipe: Pipe,
    kind: FrameType,
) -> Result<RawInfoframe, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let mut raw = [0; 32];
    for (n, word) in raw.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        word.copy_from_slice(
            &io.read32(pipe.transcoder_register(kind.data_base() + n as u32 * 4))?
                .to_le_bytes(),
        );
    }
    Ok(RawInfoframe { raw, kind })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiBpcRequest {
    pub pipe_bpp: u8,
    pub pixel_clock_khz: u32,
    pub display_version: u8,
    pub has_gmch: bool,
    pub format: HdmiOutputFormat,
    pub respect_downstream_limits: bool,
    pub source_limit_khz: u32,
    pub dp_dual_mode_limit_khz: Option<u32>,
    pub sink_limit_khz: Option<u32>,
    pub has_hdmi_sink: bool,
    pub sink: HdmiSinkBpc,
    pub combo_phy: bool,
    pub tc_phy: bool,
    pub y420_hblank_width: u32,
}

/// Try deep color from the pipe target down to the HDMI 8-bpc floor.
// upstream: intel_hdmi.c intel_hdmi_compute_bpc()
pub fn intel_hdmi_compute_bpc(request: HdmiBpcRequest) -> Option<u8> {
    let mut bpc = (request.pipe_bpp / 3).max(8);
    if !request.respect_downstream_limits {
        bpc = 8;
    }
    let limit = hdmi_port_clock_limit(
        request.source_limit_khz,
        request.respect_downstream_limits,
        request.dp_dual_mode_limit_khz,
        request.sink_limit_khz,
        request.has_hdmi_sink,
    );
    while bpc >= 8 {
        let tmds = intel_hdmi_tmds_clock(request.pixel_clock_khz, bpc, request.format);
        let source_ok =
            intel_hdmi_source_bpc_possible(request.display_version, request.has_gmch, bpc);
        let sink_ok = intel_hdmi_sink_bpc_possible(request.sink, bpc, request.format);
        let wa_ok = !(request.format == HdmiOutputFormat::Ycbcr420
            && bpc == 10
            && request.display_version == 11
            && request.y420_hblank_width % 8 == 2);
        if source_ok
            && sink_ok
            && wa_ok
            && hdmi_port_clock_valid(tmds, limit, request.combo_phy, request.tc_phy)
                == HdmiModeStatus::Ok
        {
            return Some(bpc);
        }
        if bpc < 10 {
            break;
        }
        bpc -= 2;
    }
    None
}

/// Source BPC ceilings selected by the display version and the ADL-S 600-MHz SKU.
// upstream: intel_hdmi.c intel_hdmi_source_max_tmds_clock()
pub fn intel_hdmi_source_max_tmds_clock(
    display_version: u8,
    platform: Platform,
    vbt_max_tmds_clock: Option<u32>,
) -> u32 {
    let mut max_clock = if display_version >= 13 || platform == Platform::AlderLakeS {
        600_000
    } else if display_version >= 10 {
        594_000
    } else if display_version >= 8 {
        300_000
    } else if display_version >= 5 {
        225_000
    } else {
        165_000
    };
    if let Some(vbt_max) = vbt_max_tmds_clock.filter(|clock| *clock != 0) {
        max_clock = max_clock.min(vbt_max);
    }
    max_clock
}

/// HDMI TMDS character rate, including YCbCr 4:2:0 and deep-color scaling.
// upstream: intel_hdmi.c intel_hdmi_tmds_clock()
pub fn intel_hdmi_tmds_clock(pixel_clock_khz: u32, bpc: u8, format: HdmiOutputFormat) -> u32 {
    let clock = if format == HdmiOutputFormat::Ycbcr420 {
        pixel_clock_khz / 2
    } else {
        pixel_clock_khz
    };
    (clock * u32::from(bpc) + 4) / 8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HdmiModeStatus {
    Ok,
    ClockLow,
    ClockHigh,
    ClockRange,
}

/// Apply source, dual-mode-adapter, and sink TMDS limits in i915 order.
// upstream: intel_hdmi.c hdmi_port_clock_limit()
pub fn hdmi_port_clock_limit(
    source_limit: u32,
    respect_downstream_limits: bool,
    dp_dual_mode_limit: Option<u32>,
    sink_limit: Option<u32>,
    has_hdmi_sink: bool,
) -> u32 {
    if !respect_downstream_limits {
        return source_limit;
    }
    let mut limit = source_limit;
    if let Some(value) = dp_dual_mode_limit.filter(|value| *value != 0) {
        limit = limit.min(value);
    }
    if let Some(value) = sink_limit.filter(|value| *value != 0) {
        limit = limit.min(value);
    } else if !has_hdmi_sink {
        limit = limit.min(165_000);
    }
    limit
}

/// Validate the display-12/13 clock floor/cap and combo/TC PLL hole.
// upstream: intel_hdmi.c hdmi_port_clock_valid()
pub fn hdmi_port_clock_valid(
    clock_khz: u32,
    limit_khz: u32,
    is_combo_phy: bool,
    is_tc_phy: bool,
) -> HdmiModeStatus {
    if clock_khz < 25_000 {
        HdmiModeStatus::ClockLow
    } else if clock_khz > limit_khz {
        HdmiModeStatus::ClockHigh
    } else if (is_combo_phy && clock_khz > 500_000 && clock_khz < 533_200)
        || (is_tc_phy && clock_khz > 500_000 && clock_khz < 532_800)
    {
        HdmiModeStatus::ClockRange
    } else {
        HdmiModeStatus::Ok
    }
}

/// Whether the source hardware can generate this HDMI component depth.
// upstream: intel_hdmi.c intel_hdmi_source_bpc_possible()
pub const fn intel_hdmi_source_bpc_possible(display_version: u8, has_gmch: bool, bpc: u8) -> bool {
    match bpc {
        12 => !has_gmch,
        10 => display_version >= 11,
        8 => true,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiSinkBpc {
    pub has_hdmi_sink: bool,
    pub y420_dc_modes: u8,
    pub rgb444_dc_modes: u8,
}

/// Check the sink's EDID deep-color bits for RGB or YCbCr 4:2:0 output.
// upstream: intel_hdmi.c intel_hdmi_sink_bpc_possible()
pub const fn intel_hdmi_sink_bpc_possible(
    sink: HdmiSinkBpc,
    bpc: u8,
    format: HdmiOutputFormat,
) -> bool {
    let is_420 = matches!(format, HdmiOutputFormat::Ycbcr420);
    match bpc {
        12 if !sink.has_hdmi_sink => false,
        12 if is_420 => sink.y420_dc_modes & (1 << 1) != 0,
        12 => sink.rgb444_dc_modes & (1 << 5) != 0,
        10 if !sink.has_hdmi_sink => false,
        10 if is_420 => sink.y420_dc_modes & (1 << 0) != 0,
        10 => sink.rgb444_dc_modes & (1 << 4) != 0,
        8 => true,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HdmiOutputFormat {
    Rgb,
    Ycbcr422,
    Ycbcr444,
    Ycbcr420,
}

/// Apply the format/range portion of i915 AVI generation to the DRM-computed
/// baseline frame. Connector colorimetry/content-type helpers remain caller
/// inputs because they belong to the generic DRM connector layer.
pub fn apply_avi_output_policy(
    mut frame: Avi,
    output_format: HdmiOutputFormat,
    limited_color_range: bool,
) -> Avi {
    frame.colorspace = match output_format {
        HdmiOutputFormat::Rgb => 0,
        HdmiOutputFormat::Ycbcr422 => 1,
        HdmiOutputFormat::Ycbcr444 => 2,
        HdmiOutputFormat::Ycbcr420 => 3,
    };
    if output_format == HdmiOutputFormat::Rgb {
        frame.quantization_range = if limited_color_range { 1 } else { 2 };
    } else {
        frame.quantization_range = 0;
        frame.ycc_quantization_range = 1;
    }
    frame
}

/// Build Intel's SPD packet after the caller has admitted HDMI infoframes.
// upstream: intel_hdmi.c intel_hdmi_compute_spd_infoframe()
pub fn intel_hdmi_compute_spd_infoframe(
    has_infoframe: bool,
    discrete_graphics: bool,
) -> Option<Spd> {
    if !has_infoframe {
        return None;
    }
    let mut vendor = [0; 8];
    vendor[..5].copy_from_slice(b"Intel");
    let product_text = if discrete_graphics {
        b"Discrete gfx".as_slice()
    } else {
        b"Integrated gfx".as_slice()
    };
    let mut product = [0; 16];
    product[..product_text.len()].copy_from_slice(product_text);
    Some(Spd {
        vendor,
        product,
        sdi: 9,
    }) // HDMI_SPD_SDI_PC
}

/// Preserve the source's version/infoframe/HDR-metadata gates for DRM packets.
// upstream: intel_hdmi.c intel_hdmi_compute_drm_infoframe()
pub fn intel_hdmi_compute_drm_infoframe(
    display_version: u8,
    has_infoframe: bool,
    metadata: Option<Drm>,
) -> Option<Drm> {
    if display_version < 10 || !has_infoframe {
        return None;
    }
    metadata
}

const INFOFRAME_TYPE_TO_IDX: [u8; 8] = [0x03, 0x0a, 0x07, 0x22, 0x82, 0x83, 0x81, 0x87];

/// Map a packet type to its `intel_crtc_state.infoframes.enable` slot.
// upstream: intel_hdmi.c intel_hdmi_infoframe_enable()
pub fn intel_hdmi_infoframe_enable(packet_type: u8) -> u32 {
    INFOFRAME_TYPE_TO_IDX
        .iter()
        .position(|candidate| *candidate == packet_type)
        .map(|index| 1 << index)
        .unwrap_or(0)
}

const GCP_COLOR_INDICATION: u32 = 1 << 2;
const GCP_DEFAULT_PHASE_ENABLE: u32 = 1 << 1;
const HSW_INFOFRAME_ENABLE_MASK: u32 =
    (1 << 20) | (1 << 12) | (1 << 16) | (1 << 8) | (1 << 4) | 1 | (1 << 28) | (1 << 23);

/// Calculate whether HDMI deep-color packing can keep pixel phase zero.
// upstream: intel_hdmi.c gcp_default_phase_possible()
pub fn gcp_default_phase_possible(pipe_bpp: u8, mode: &Timings) -> bool {
    let pixels_per_group = match pipe_bpp {
        30 => 4,
        36 => 2,
        48 => 1,
        _ => return false,
    };
    mode.hdisplay % pixels_per_group == 0
        && mode.htotal % pixels_per_group == 0
        && mode.hblank_start % pixels_per_group == 0
        && mode.hblank_end % pixels_per_group == 0
        && mode.hsync_start % pixels_per_group == 0
        && mode.hsync_end % pixels_per_group == 0
        && (!mode.interlaced || (mode.htotal / 2) % pixels_per_group == 0)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcpState {
    pub enabled: bool,
    pub value: u32,
}

/// Compute the source GCP color indication and default-phase fields.
// upstream: intel_hdmi.c intel_hdmi_compute_gcp_infoframe()
pub fn intel_hdmi_compute_gcp_infoframe(
    g4x: bool,
    has_infoframe: bool,
    pipe_bpp: u8,
    mode: &Timings,
    mut state: GcpState,
) -> GcpState {
    if g4x || !has_infoframe {
        return state;
    }
    state.enabled = true;
    if pipe_bpp > 24 {
        state.value |= GCP_COLOR_INDICATION;
    }
    if gcp_default_phase_possible(pipe_bpp, mode) {
        state.value |= GCP_DEFAULT_PHASE_ENABLE;
    }
    state
}

/// Program/read the HSW GCP payload register selected by `cpu_transcoder`.
// upstream: intel_hdmi.c intel_hdmi_set_gcp_infoframe()
pub fn intel_hdmi_set_gcp_infoframe(
    io: &impl ReadoutIo,
    pipe: Pipe,
    state: GcpState,
) -> Result<bool, Error> {
    if !state.enabled {
        return Ok(false);
    }
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    io.write32(pipe.transcoder_register(0x60210), state.value)?;
    Ok(true)
}

/// Fastset the HSW DRM/HDR packet only when either desired or hardware state
/// says it is present; clear its enable before rewriting the data words.
// upstream: intel_hdmi.c intel_hdmi_fastset_infoframes()
pub fn intel_hdmi_fastset_infoframes(
    io: &impl ReadoutIo,
    pipe: Pipe,
    drm_enabled: bool,
    packet: Option<[u8; 32]>,
) -> Result<(), Error> {
    if !io.pipe_powered(pipe) || (drm_enabled && packet.is_none()) {
        return Err(Error::Refused);
    }
    let ctl = pipe.transcoder_register(0x60200);
    let mut value = io.read32(ctl)?;
    if !drm_enabled && value & FrameType::Drm.enable_mask() == 0 {
        return Ok(());
    }
    value &= !FrameType::Drm.enable_mask();
    io.write32(ctl, value)?;
    let _ = io.read32(ctl)?;
    if let Some(packet) = packet {
        hsw_write_infoframe(io, pipe, FrameType::Drm, &packet)?;
    }
    Ok(())
}

/// HSW frame set used by `hsw_set_infoframes()` in source ordering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HswInfoframeSet {
    pub enabled: bool,
    pub gcp: Option<u32>,
    pub avi: Option<[u8; 32]>,
    pub spd: Option<[u8; 32]>,
    pub vendor: Option<[u8; 32]>,
    pub drm: Option<[u8; 32]>,
}

/// Select HSW DIP packets, write GCP, then AVI/SPD/vendor/DRM in i915 order.
// upstream: intel_hdmi.c hsw_set_infoframes()
pub fn hsw_set_infoframes(
    io: &impl ReadoutIo,
    pipe: Pipe,
    frames: HswInfoframeSet,
) -> Result<(), Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let ctl = pipe.transcoder_register(0x60200);
    let mut value = io.read32(ctl)? & !HSW_INFOFRAME_ENABLE_MASK;
    if !frames.enabled {
        io.write32(ctl, value)?;
        let _ = io.read32(ctl)?;
        return Ok(());
    }
    if let Some(gcp) = frames.gcp {
        let _ = intel_hdmi_set_gcp_infoframe(
            io,
            pipe,
            GcpState {
                enabled: true,
                value: gcp,
            },
        )?;
        value |= 1 << 16;
    }
    io.write32(ctl, value)?;
    let _ = io.read32(ctl)?;
    for (kind, packet) in [
        (FrameType::Avi, frames.avi),
        (FrameType::Spd, frames.spd),
        (FrameType::Vendor, frames.vendor),
        (FrameType::Drm, frames.drm),
    ] {
        if let Some(raw) = packet {
            hsw_write_infoframe(io, pipe, kind, &raw)?;
        }
    }
    Ok(())
}

/// Pack the generic infoframe bytes into the HSW DIP buffer's ECC hole layout.
/// `hdmi_infoframe_pack_only()` supplies the checksum/header packet; this
/// reproduces `intel_write_infoframe()`'s memmove and zero byte at DW0 byte 3.
// upstream: intel_hdmi.c intel_write_infoframe()
pub fn infoframe_packet_to_dip_data(packet: &[u8]) -> Result<[u8; 32], Error> {
    if !(3..=31).contains(&packet.len()) {
        return Err(Error::Refused);
    }
    let mut raw = [0; 32];
    raw[..3].copy_from_slice(&packet[..3]);
    raw[4..packet.len() + 1].copy_from_slice(&packet[3..]);
    raw[3] = 0;
    Ok(raw)
}

/// Write one prepacked HDMI packet through the HSW transcoder DIP aperture.
/// All 32 bytes are written so the hardware ECC state is deterministic, just
/// as the source writes the remaining data words as zero. The caller owns the
/// surrounding modeset/port state and decides whether this packet is enabled.
// upstream: intel_hdmi.c hsw_write_infoframe()
pub fn hsw_write_infoframe(
    io: &impl ReadoutIo,
    pipe: Pipe,
    kind: FrameType,
    raw_dip_data: &[u8; 32],
) -> Result<(), Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let ctl = pipe.transcoder_register(0x60200);
    let mut value = io.read32(ctl)? & !kind.enable_mask();
    io.write32(ctl, value)?;
    for (index, bytes) in raw_dip_data.chunks_exact(4).enumerate() {
        let offset = kind.data_base() + index as u32 * 4;
        let word = u32::from_le_bytes(bytes.try_into().map_err(|_| Error::Truncated)?);
        io.write32(pipe.transcoder_register(offset), word)?;
    }
    value |= kind.enable_mask();
    io.write32(ctl, value)?;
    let _ = io.read32(ctl)?; // intel_de_posting_read()
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiReadout {
    pub control: u32,
    /// Filtered hardware bits; unlike the software DIP-index bitmap.
    pub enable: u32,
    pub enabled_packets: u32,
    pub gcp: Option<u32>,
    pub frames: [Option<RawInfoframe>; 4],
}
pub fn read_hdmi_state(io: &impl ReadoutIo, pipe: Pipe) -> Result<HdmiReadout, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let control = io.read32(pipe.transcoder_register(0x60200))?;
    let enable = hsw_infoframes_enabled(control);
    let gcp = if enable & (1 << 16) != 0 {
        Some(io.read32(pipe.transcoder_register(0x60210))?)
    } else {
        None
    };
    // Same HDMI get_config order as intel_ddi_get_config: AVI/SPD/VS/DRM.
    let mut frames = [None; 4];
    for (kind, slot) in [
        FrameType::Avi,
        FrameType::Spd,
        FrameType::Vendor,
        FrameType::Drm,
    ]
    .into_iter()
    .zip(frames.iter_mut())
    {
        if enable & kind.enable_mask() != 0 {
            *slot = Some(hsw_read_infoframe(io, pipe, kind)?);
        }
    }
    Ok(HdmiReadout {
        control,
        enable,
        enabled_packets: intel_hdmi_infoframes_enabled(control),
        gcp,
        frames,
    })
}

#[cfg(test)]
mod write_tests {
    extern crate std;
    use std::{cell::RefCell, collections::BTreeMap, vec::Vec};

    use super::*;
    use crate::RegisterIo;

    #[derive(Default)]
    struct Mock {
        registers: RefCell<BTreeMap<u32, u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
        powered: bool,
    }
    impl RegisterIo for Mock {
        fn read32(&self, offset: u32) -> Result<u32, Error> {
            Ok(*self.registers.borrow().get(&offset).unwrap_or(&0))
        }
        fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.registers.borrow_mut().insert(offset, value);
            self.writes.borrow_mut().push((offset, value));
            Ok(())
        }
    }
    impl ReadoutIo for Mock {
        fn pipe_powered(&self, _pipe: Pipe) -> bool {
            self.powered
        }
    }

    fn mode() -> Timings {
        Timings {
            hdisplay: 1920,
            htotal: 2200,
            hblank_start: 1920,
            hblank_end: 280,
            hsync_start: 2008,
            hsync_end: 2052,
            vdisplay: 1080,
            vtotal: 1125,
            vblank_start: 1080,
            vblank_end: 45,
            vsync_start: 1084,
            vsync_end: 1089,
            set_context_latency: 0,
            interlaced: false,
        }
    }

    #[test]
    fn hdmi_bpc_fallback_and_display12_pll_holes_follow_source_order() {
        let sink = HdmiSinkBpc {
            has_hdmi_sink: true,
            y420_dc_modes: 0,
            rgb444_dc_modes: (1 << 4) | (1 << 5),
        };
        let request = HdmiBpcRequest {
            pipe_bpp: 36,
            pixel_clock_khz: 148_500,
            display_version: 13,
            has_gmch: false,
            format: HdmiOutputFormat::Rgb,
            respect_downstream_limits: true,
            source_limit_khz: 600_000,
            dp_dual_mode_limit_khz: None,
            sink_limit_khz: None,
            has_hdmi_sink: true,
            sink,
            combo_phy: true,
            tc_phy: false,
            y420_hblank_width: 280,
        };
        assert_eq!(intel_hdmi_compute_bpc(request), Some(12));
        let constrained = HdmiBpcRequest {
            pixel_clock_khz: 500_000,
            ..request
        };
        assert_eq!(intel_hdmi_compute_bpc(constrained), Some(8));
        assert_eq!(
            hdmi_port_clock_valid(25_000, 600_000, true, false),
            HdmiModeStatus::Ok
        );
        assert_eq!(
            hdmi_port_clock_valid(533_199, 600_000, true, false),
            HdmiModeStatus::ClockRange
        );
        assert_eq!(
            hdmi_port_clock_valid(532_800, 600_000, false, true),
            HdmiModeStatus::Ok
        );
        assert_eq!(
            hdmi_port_clock_limit(600_000, true, None, None, false),
            165_000
        );
    }

    #[test]
    fn source_tmds_clock_and_source_sink_bpc_gates_match_i915() {
        assert_eq!(
            intel_hdmi_source_max_tmds_clock(12, Platform::TigerLake, None),
            594_000
        );
        assert_eq!(
            intel_hdmi_source_max_tmds_clock(12, Platform::AlderLakeS, Some(590_000)),
            590_000
        );
        assert_eq!(
            intel_hdmi_source_max_tmds_clock(13, Platform::AlderLakeN, Some(0)),
            600_000
        );
        assert_eq!(
            intel_hdmi_tmds_clock(148_500, 8, HdmiOutputFormat::Rgb),
            148_500
        );
        assert_eq!(
            intel_hdmi_tmds_clock(297_000, 10, HdmiOutputFormat::Ycbcr420),
            185_625
        );
        assert!(!intel_hdmi_source_bpc_possible(12, true, 12));
        assert!(intel_hdmi_source_bpc_possible(13, false, 12));
        assert!(intel_hdmi_source_bpc_possible(12, false, 10));
        let sink = HdmiSinkBpc {
            has_hdmi_sink: true,
            y420_dc_modes: 0b11,
            rgb444_dc_modes: (1 << 4) | (1 << 5),
        };
        assert!(intel_hdmi_sink_bpc_possible(
            sink,
            12,
            HdmiOutputFormat::Ycbcr420
        ));
        assert!(intel_hdmi_sink_bpc_possible(
            sink,
            10,
            HdmiOutputFormat::Rgb
        ));
        assert!(!intel_hdmi_sink_bpc_possible(
            HdmiSinkBpc {
                has_hdmi_sink: false,
                ..sink
            },
            10,
            HdmiOutputFormat::Rgb
        ));
    }

    #[test]
    fn avi_output_policy_matches_rgb_and_ycc_source_branches() {
        let baseline = Avi {
            colorspace: 7,
            scan_mode: 2,
            colorimetry: 1,
            picture_aspect: 2,
            active_aspect: 8,
            itc: false,
            extended_colorimetry: 0,
            quantization_range: 0,
            nups: 0,
            video_code: 16,
            ycc_quantization_range: 0,
            content_type: 0,
            pixel_repeat: 0,
            top_bar: 0,
            bottom_bar: 0,
            left_bar: 0,
            right_bar: 0,
        };
        let rgb = apply_avi_output_policy(baseline, HdmiOutputFormat::Rgb, true);
        assert_eq!(
            (
                rgb.colorspace,
                rgb.quantization_range,
                rgb.ycc_quantization_range
            ),
            (0, 1, 0)
        );
        let yuv = apply_avi_output_policy(baseline, HdmiOutputFormat::Ycbcr420, true);
        assert_eq!(
            (
                yuv.colorspace,
                yuv.quantization_range,
                yuv.ycc_quantization_range
            ),
            (3, 0, 1)
        );
        assert_eq!(yuv.video_code, baseline.video_code);
    }

    #[test]
    fn source_infoframe_type_index_matches_all_slots() {
        for (index, packet) in INFOFRAME_TYPE_TO_IDX.into_iter().enumerate() {
            assert_eq!(intel_hdmi_infoframe_enable(packet), 1 << index);
        }
        assert_eq!(intel_hdmi_infoframe_enable(0xff), 0);
    }

    #[test]
    fn spd_and_drm_compute_keep_source_enable_gates_and_defaults() {
        assert_eq!(intel_hdmi_compute_spd_infoframe(false, false), None);
        let spd = intel_hdmi_compute_spd_infoframe(true, false).unwrap();
        assert_eq!(&spd.vendor[..5], b"Intel");
        assert_eq!(&spd.product[..14], b"Integrated gfx");
        assert_eq!(spd.sdi, 9);
        let metadata = Drm {
            eotf: 2,
            metadata_type: 0,
            display_primaries: [[1, 2]; 3],
            white_point: [3, 4],
            max_display_mastering_luminance: 5,
            min_display_mastering_luminance: 6,
            max_cll: 7,
            max_fall: 8,
        };
        assert_eq!(
            intel_hdmi_compute_drm_infoframe(9, true, Some(metadata)),
            None
        );
        assert_eq!(
            intel_hdmi_compute_drm_infoframe(13, false, Some(metadata)),
            None
        );
        assert_eq!(
            intel_hdmi_compute_drm_infoframe(13, true, Some(metadata)),
            Some(metadata)
        );
        assert_eq!(intel_hdmi_compute_drm_infoframe(13, true, None), None);
    }

    #[test]
    fn gcp_default_phase_obeys_every_horizontal_boundary_and_bpp_case() {
        assert!(gcp_default_phase_possible(30, &mode()));
        assert!(gcp_default_phase_possible(36, &mode()));
        assert!(!gcp_default_phase_possible(24, &mode()));
        let mut interlaced = mode();
        interlaced.interlaced = true;
        interlaced.htotal = 2204;
        assert!(!gcp_default_phase_possible(30, &interlaced));
        assert_eq!(
            intel_hdmi_compute_gcp_infoframe(false, true, 30, &mode(), GcpState::default()),
            GcpState {
                enabled: true,
                value: GCP_COLOR_INDICATION | GCP_DEFAULT_PHASE_ENABLE
            },
        );
        assert_eq!(
            intel_hdmi_compute_gcp_infoframe(true, true, 30, &mode(), GcpState::default()),
            GcpState::default()
        );
    }

    #[test]
    fn hsw_set_infoframes_clears_then_enables_packets_in_order() {
        let mock = Mock {
            powered: true,
            ..Mock::default()
        };
        let pipe = Pipe::A;
        let avi = infoframe_packet_to_dip_data(&[0x82, 2, 13, 1, 2, 3]).unwrap();
        let spd = infoframe_packet_to_dip_data(&[0x83, 1, 25, 4, 5, 6]).unwrap();
        hsw_set_infoframes(
            &mock,
            pipe,
            HswInfoframeSet {
                enabled: true,
                gcp: Some(7),
                avi: Some(avi),
                spd: Some(spd),
                ..HswInfoframeSet::default()
            },
        )
        .unwrap();
        let writes = mock.writes.borrow();
        assert_eq!(writes[0], (pipe.transcoder_register(0x60210), 7));
        assert_eq!(writes[1].0, pipe.transcoder_register(0x60200));
        assert_eq!(writes[3].0, pipe.transcoder_register(0x60220));
        assert_eq!(writes[13].0, pipe.transcoder_register(0x602a0));
        assert_eq!(writes.last().unwrap().0, pipe.transcoder_register(0x60200));
    }

    #[test]
    fn fastset_hdr_clears_enabled_packet_before_rewriting_or_removal() {
        let pipe = Pipe::A;
        let drm = FrameType::Drm.enable_mask();
        let mock = Mock {
            powered: true,
            ..Mock::default()
        };
        mock.registers
            .borrow_mut()
            .insert(pipe.transcoder_register(0x60200), drm | 0x123);
        intel_hdmi_fastset_infoframes(&mock, pipe, false, None).unwrap();
        assert_eq!(
            mock.writes.borrow().as_slice(),
            &[(pipe.transcoder_register(0x60200), 0x123)]
        );
        mock.writes.borrow_mut().clear();
        intel_hdmi_fastset_infoframes(&mock, pipe, false, None).unwrap();
        assert!(mock.writes.borrow().is_empty());
    }

    #[test]
    fn packet_hole_and_hsw_write_order_match_source() {
        let packet = [0x82, 2, 13, 0x7a, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        let raw = infoframe_packet_to_dip_data(&packet).unwrap();
        assert_eq!(&raw[..4], &[0x82, 2, 13, 0]);
        assert_eq!(&raw[4..4 + packet.len() - 3], &packet[3..]);
        assert!(raw[4 + packet.len() - 3..].iter().all(|b| *b == 0));
        let mock = Mock {
            powered: true,
            ..Mock::default()
        };
        let pipe = Pipe::A;
        mock.registers
            .borrow_mut()
            .insert(pipe.transcoder_register(0x60200), 0xabc0_0123);
        hsw_write_infoframe(&mock, pipe, FrameType::Avi, &raw).unwrap();
        let writes = mock.writes.borrow();
        assert_eq!(writes.len(), 10); // disable, eight data writes, enable
        assert_eq!(
            writes[0],
            (pipe.transcoder_register(0x60200), 0xabc0_0123 & !(1 << 12))
        );
        assert_eq!(
            writes[1],
            (
                pipe.transcoder_register(0x60220),
                u32::from_le_bytes([0x82, 2, 13, 0])
            )
        );
        assert_eq!(writes[8], (pipe.transcoder_register(0x6023c), 0));
        assert_eq!(
            writes[9],
            (pipe.transcoder_register(0x60200), 0xabc0_0123 | (1 << 12))
        );
    }

    #[test]
    fn packet_write_refuses_when_pipe_power_is_not_owned() {
        let mock = Mock::default();
        assert_eq!(
            hsw_write_infoframe(&mock, Pipe::A, FrameType::Avi, &[0; 32]),
            Err(Error::Refused)
        );
        assert!(mock.writes.borrow().is_empty());
    }
}
