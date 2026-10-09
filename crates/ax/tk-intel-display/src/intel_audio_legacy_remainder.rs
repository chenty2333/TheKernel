// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation
// Translated from Linux 7.2.3 drivers/gpu/drm/i915/display/intel_audio.c.
// The complete MIT grant is retained in LICENSES/Intel-i915-DP-Audio-MIT.txt.
//! Source-ordered legacy HDMI audio helpers omitted from the DP-focused module.
//! MMIO, mutexes, ELD sizing, and vblank waiting are explicit hooks.
#![allow(dead_code, clippy::too_many_arguments)]

pub const ELD_BYTES: usize = 84;
const G4X_ELD_VALID: u32 = 1 << 14;
const G4X_ELD_BUFFER_SIZE_MASK: u32 = 0x3e00;
const G4X_ELD_ADDRESS_MASK: u32 = 0x01e0;
const IBX_ELD_BUFFER_SIZE_MASK: u32 = 0x7c00;
const IBX_ELD_ADDRESS_MASK: u32 = 0x03e0;
const AUD_CONFIG_N_VALUE_INDEX: u32 = 1 << 29;
const AUD_CONFIG_N_PROG_ENABLE: u32 = 1 << 28;
const AUD_CONFIG_UPPER_N_MASK: u32 = 0xff << 20;
const AUD_CONFIG_LOWER_N_MASK: u32 = 0xfff << 4;
const AUD_CONFIG_N_MASK: u32 = AUD_CONFIG_UPPER_N_MASK | AUD_CONFIG_LOWER_N_MASK;
const AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK: u32 = 0xf << 16;
const AUD_M_CTS_M_VALUE_INDEX: u32 = 1 << 21;
const AUD_M_CTS_M_PROG_ENABLE: u32 = 1 << 20;
const VLV_DISPLAY_BASE: u32 = 0x180000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioPlatform {
    pub display_ver: u8,
    pub valleyview: bool,
    pub cherryview: bool,
    pub pch_cpt: bool,
    pub pch_ibx: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioCrtcState {
    pub pipe: usize,
    pub port: usize,
    pub cpu_transcoder: usize,
    pub port_clock: u32,
    pub crtc_clock: u32,
    pub pipe_bpp: u8,
    pub audio_sample_rate: u32,
    pub is_dp_encoder: bool,
    pub eld: [u8; ELD_BYTES],
}

impl Default for AudioCrtcState {
    fn default() -> Self {
        Self {
            pipe: 0,
            port: 0,
            cpu_transcoder: 0,
            port_clock: 0,
            crtc_clock: 0,
            pipe_bpp: 0,
            audio_sample_rate: 0,
            is_dp_encoder: false,
            eld: [0; ELD_BYTES],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioConnectorState {
    pub eld: [u8; ELD_BYTES],
}

impl Default for AudioConnectorState {
    fn default() -> Self {
        Self {
            eld: [0; ELD_BYTES],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IbXAudioRegisters {
    pub hdmiw_hdmiedid: u32,
    pub aud_config: u32,
    pub aud_cntl_st: u32,
    pub aud_cntrl_st2: u32,
}

pub trait LegacyAudioIo {
    fn read32(&mut self, register: u32) -> u32;
    fn write32(&mut self, register: u32, value: u32);
    fn wait_next_vblank(&mut self, pipe: usize);
    fn eld_size(&mut self, eld: &[u8; ELD_BYTES]) -> usize;
    fn mutex_lock(&mut self);
    fn mutex_unlock(&mut self);
    fn debug(&mut self, message: &'static str, value: u32);
    fn warn(&mut self, message: &'static str, value: u32);
}

fn intel_de_rmw(io: &mut impl LegacyAudioIo, register: u32, clear: u32, set: u32) {
    let old = io.read32(register);
    io.write32(register, (old & !clear) | set);
}

#[derive(Clone, Copy)]
struct HdmiNcts {
    sample_rate: u32,
    clock: u32,
    n: u32,
    _cts: u32,
}

const TMDS_297M: u32 = 297_000;
const TMDS_296M: u32 = 296_703;
const TMDS_594M: u32 = 594_000;
const TMDS_593M: u32 = 593_407;
const TMDS_371M: u32 = 371_250;
const TMDS_370M: u32 = 370_878;
const TMDS_445_5M: u32 = 445_500;
const TMDS_445M: u32 = 445_054;

const HDMI_NCTS_24BPP: &[HdmiNcts] = &[
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_296M,
        n: 5824,
        _cts: 421875,
    },
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_297M,
        n: 3072,
        _cts: 222750,
    },
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_593M,
        n: 5824,
        _cts: 843750,
    },
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_594M,
        n: 3072,
        _cts: 445500,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_296M,
        n: 4459,
        _cts: 234375,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_297M,
        n: 4704,
        _cts: 247500,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_593M,
        n: 8918,
        _cts: 937500,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_594M,
        n: 9408,
        _cts: 990000,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_296M,
        n: 8918,
        _cts: 234375,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_297M,
        n: 9408,
        _cts: 247500,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_593M,
        n: 17836,
        _cts: 937500,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_594M,
        n: 18816,
        _cts: 990000,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_296M,
        n: 17836,
        _cts: 234375,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_297M,
        n: 18816,
        _cts: 247500,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_593M,
        n: 35672,
        _cts: 937500,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_594M,
        n: 37632,
        _cts: 990000,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_296M,
        n: 5824,
        _cts: 281250,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_297M,
        n: 5120,
        _cts: 247500,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_593M,
        n: 5824,
        _cts: 562500,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_594M,
        n: 6144,
        _cts: 594000,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_296M,
        n: 11648,
        _cts: 281250,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_297M,
        n: 10240,
        _cts: 247500,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_593M,
        n: 11648,
        _cts: 562500,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_594M,
        n: 12288,
        _cts: 594000,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_296M,
        n: 23296,
        _cts: 281250,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_297M,
        n: 20480,
        _cts: 247500,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_593M,
        n: 23296,
        _cts: 562500,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_594M,
        n: 24576,
        _cts: 594000,
    },
];

const HDMI_NCTS_30BPP: &[HdmiNcts] = &[
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_370M,
        n: 5824,
        _cts: 527344,
    },
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_371M,
        n: 6144,
        _cts: 556875,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_370M,
        n: 8918,
        _cts: 585938,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_371M,
        n: 4704,
        _cts: 309375,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_370M,
        n: 17836,
        _cts: 585938,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_371M,
        n: 9408,
        _cts: 309375,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_370M,
        n: 35672,
        _cts: 585938,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_371M,
        n: 18816,
        _cts: 309375,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_370M,
        n: 11648,
        _cts: 703125,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_371M,
        n: 5120,
        _cts: 309375,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_370M,
        n: 23296,
        _cts: 703125,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_371M,
        n: 10240,
        _cts: 309375,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_370M,
        n: 46592,
        _cts: 703125,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_371M,
        n: 20480,
        _cts: 309375,
    },
];

const HDMI_NCTS_36BPP: &[HdmiNcts] = &[
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_445M,
        n: 5824,
        _cts: 632813,
    },
    HdmiNcts {
        sample_rate: 32_000,
        clock: TMDS_445_5M,
        n: 4096,
        _cts: 445500,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_445M,
        n: 8918,
        _cts: 703125,
    },
    HdmiNcts {
        sample_rate: 44_100,
        clock: TMDS_445_5M,
        n: 4704,
        _cts: 371250,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_445M,
        n: 17836,
        _cts: 703125,
    },
    HdmiNcts {
        sample_rate: 88_200,
        clock: TMDS_445_5M,
        n: 9408,
        _cts: 371250,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_445M,
        n: 35672,
        _cts: 703125,
    },
    HdmiNcts {
        sample_rate: 176_400,
        clock: TMDS_445_5M,
        n: 18816,
        _cts: 371250,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_445M,
        n: 5824,
        _cts: 421875,
    },
    HdmiNcts {
        sample_rate: 48_000,
        clock: TMDS_445_5M,
        n: 5120,
        _cts: 371250,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_445M,
        n: 11648,
        _cts: 421875,
    },
    HdmiNcts {
        sample_rate: 96_000,
        clock: TMDS_445_5M,
        n: 10240,
        _cts: 371250,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_445M,
        n: 23296,
        _cts: 421875,
    },
    HdmiNcts {
        sample_rate: 192_000,
        clock: TMDS_445_5M,
        n: 20480,
        _cts: 371250,
    },
];

// upstream: intel_audio.c audio_config_hdmi_pixel_clock()
pub fn audio_config_hdmi_pixel_clock(
    io: &mut impl LegacyAudioIo,
    display_ver: u8,
    crtc_clock: u32,
) -> u32 {
    const CLOCKS: [(u32, u32); 14] = [
        (25_175, 0 << 16),
        (25_200, 1 << 16),
        (27_000, 2 << 16),
        (27_027, 3 << 16),
        (54_000, 4 << 16),
        (54_054, 5 << 16),
        (74_176, 6 << 16),
        (74_250, 7 << 16),
        (148_352, 8 << 16),
        (148_500, 9 << 16),
        (296_703, 10 << 16),
        (297_000, 11 << 16),
        (593_407, 12 << 16),
        (594_000, 13 << 16),
    ];
    let mut i = CLOCKS
        .iter()
        .position(|(clock, _)| *clock == crtc_clock)
        .unwrap_or(CLOCKS.len());
    if display_ver < 12 && crtc_clock > 148_500 {
        i = CLOCKS.len();
    }
    if i == CLOCKS.len() {
        io.debug(
            "HDMI audio pixel clock setting not found; using default",
            crtc_clock,
        );
        i = 1;
    }
    io.debug("Configuring HDMI audio pixel clock", crtc_clock);
    io.debug("HDMI audio pixel clock config", CLOCKS[i].1);
    CLOCKS[i].1
}

// upstream: intel_audio.c audio_config_hdmi_get_n()
pub fn audio_config_hdmi_get_n(crtc: &AudioCrtcState, rate: u32) -> u32 {
    let table = match crtc.pipe_bpp {
        36 => HDMI_NCTS_36BPP,
        30 => HDMI_NCTS_30BPP,
        _ => HDMI_NCTS_24BPP,
    };
    table
        .iter()
        .find(|row| row.sample_rate == rate && row.clock == crtc.port_clock)
        .map(|row| row.n)
        .unwrap_or(0)
}

// upstream: intel_audio.c g4x_eld_buffer_size()
pub fn g4x_eld_buffer_size(io: &mut impl LegacyAudioIo) -> usize {
    ((io.read32(0x620b4) & G4X_ELD_BUFFER_SIZE_MASK) >> 9) as usize
}

// upstream: intel_audio.c g4x_audio_codec_get_config()
pub fn g4x_audio_codec_get_config(io: &mut impl LegacyAudioIo, crtc: &mut AudioCrtcState) {
    if io.read32(0x620b4) & G4X_ELD_VALID == 0 {
        return;
    }
    intel_de_rmw(io, 0x620b4, G4X_ELD_ADDRESS_MASK, 0);
    let length = core::cmp::min(ELD_BYTES / 4, g4x_eld_buffer_size(io));
    for index in 0..length {
        let value = io.read32(0x6210c);
        crtc.eld[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
    }
}

// upstream: intel_audio.c g4x_audio_codec_disable()
pub fn g4x_audio_codec_disable(io: &mut impl LegacyAudioIo, crtc: &AudioCrtcState) {
    intel_de_rmw(io, 0x620b4, G4X_ELD_VALID, 0);
    io.wait_next_vblank(crtc.pipe);
    io.wait_next_vblank(crtc.pipe);
}

// upstream: intel_audio.c g4x_audio_codec_enable()
pub fn g4x_audio_codec_enable(
    io: &mut impl LegacyAudioIo,
    crtc: &AudioCrtcState,
    _connector: &AudioConnectorState,
) {
    io.wait_next_vblank(crtc.pipe);
    intel_de_rmw(io, 0x620b4, G4X_ELD_VALID | G4X_ELD_ADDRESS_MASK, 0);
    let eld_size = g4x_eld_buffer_size(io);
    let mut index = 0;
    let length = core::cmp::min(io.eld_size(&crtc.eld) / 4, eld_size);
    while index < length {
        let value = u32::from_ne_bytes(crtc.eld[index * 4..index * 4 + 4].try_into().unwrap());
        io.write32(0x6210c, value);
        index += 1;
    }
    while index < eld_size {
        io.write32(0x6210c, 0);
        index += 1;
    }
    if io.read32(0x620b4) & G4X_ELD_ADDRESS_MASK != 0 {
        let value = io.read32(0x620b4);
        io.warn("G4X ELD address did not reset", value);
    }
    intel_de_rmw(io, 0x620b4, 0, G4X_ELD_VALID);
}

// upstream: intel_audio.c hsw_hdmi_audio_config_update()
pub fn hsw_hdmi_audio_config_update(
    io: &mut impl LegacyAudioIo,
    display: AudioPlatform,
    crtc: &AudioCrtcState,
) {
    let transcoder = crtc.cpu_transcoder;
    let aud_cfg = 0x65000 + transcoder as u32 * 0x100;
    let mut value = io.read32(aud_cfg);
    value &= !AUD_CONFIG_N_VALUE_INDEX;
    value &= !AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK;
    value &= !AUD_CONFIG_N_PROG_ENABLE;
    value |= audio_config_hdmi_pixel_clock(io, display.display_ver, crtc.crtc_clock);

    let n = audio_config_hdmi_get_n(crtc, crtc.audio_sample_rate);
    if n != 0 {
        io.debug("Using HDMI audio N", n);
        value &= !AUD_CONFIG_N_MASK;
        value |= ((n >> 12) & 0xff) << 20;
        value |= (n & 0xfff) << 4;
        value |= AUD_CONFIG_N_PROG_ENABLE;
    } else {
        io.debug("Using automatic HDMI audio N", crtc.audio_sample_rate);
    }
    io.write32(aud_cfg, value);

    let m_cts = 0x65028 + transcoder as u32 * 0x100;
    let mut value = io.read32(m_cts);
    value &= !AUD_M_CTS_M_PROG_ENABLE;
    value &= !AUD_M_CTS_M_VALUE_INDEX;
    io.write32(m_cts, value);
}

// upstream: intel_audio.c ibx_audio_regs_init()
pub fn ibx_audio_regs_init(display: AudioPlatform, pipe: usize) -> Option<IbXAudioRegisters> {
    if pipe > 1 {
        return None;
    }
    match (
        display.valleyview || display.cherryview,
        display.pch_cpt,
        display.pch_ibx,
    ) {
        (true, ..) => Some(IbXAudioRegisters {
            hdmiw_hdmiedid: VLV_DISPLAY_BASE + 0x62050 + pipe as u32 * 0x100,
            aud_config: VLV_DISPLAY_BASE + 0x62000 + pipe as u32 * 0x100,
            aud_cntl_st: VLV_DISPLAY_BASE + 0x620b4 + pipe as u32 * 0x100,
            aud_cntrl_st2: VLV_DISPLAY_BASE + 0x620c0,
        }),
        (false, true, _) => Some(IbXAudioRegisters {
            hdmiw_hdmiedid: 0xe5050 + pipe as u32 * 0x100,
            aud_config: 0xe5000 + pipe as u32 * 0x100,
            aud_cntl_st: 0xe50b4 + pipe as u32 * 0x100,
            aud_cntrl_st2: 0xe50c0,
        }),
        (false, false, true) => Some(IbXAudioRegisters {
            hdmiw_hdmiedid: 0xe2050 + pipe as u32 * 0x100,
            aud_config: 0xe2000 + pipe as u32 * 0x100,
            aud_cntl_st: 0xe20b4 + pipe as u32 * 0x100,
            aud_cntrl_st2: 0xe20c0,
        }),
        _ => None,
    }
}

fn ibx_eld_valid(port: usize) -> u32 {
    1u32 << ((port - 1) * 4)
}

// upstream: intel_audio.c ibx_audio_codec_disable()
pub fn ibx_audio_codec_disable(
    io: &mut impl LegacyAudioIo,
    display: AudioPlatform,
    crtc: &AudioCrtcState,
    _connector: &AudioConnectorState,
) {
    if crtc.port == 0 {
        io.warn("IBX audio codec disable called for port A", 0);
        return;
    }
    let Some(regs) = ibx_audio_regs_init(display, crtc.pipe) else {
        return;
    };
    io.mutex_lock();
    intel_de_rmw(
        io,
        regs.aud_config,
        AUD_CONFIG_N_VALUE_INDEX | AUD_CONFIG_UPPER_N_MASK | AUD_CONFIG_LOWER_N_MASK,
        AUD_CONFIG_N_PROG_ENABLE
            | if crtc.is_dp_encoder {
                AUD_CONFIG_N_VALUE_INDEX
            } else {
                0
            },
    );
    intel_de_rmw(io, regs.aud_cntrl_st2, ibx_eld_valid(crtc.port), 0);
    io.mutex_unlock();
    io.wait_next_vblank(crtc.pipe);
    io.wait_next_vblank(crtc.pipe);
}

// upstream: intel_audio.c ibx_audio_codec_enable()
pub fn ibx_audio_codec_enable(
    io: &mut impl LegacyAudioIo,
    display: AudioPlatform,
    crtc: &AudioCrtcState,
    _connector: &AudioConnectorState,
) {
    if crtc.port == 0 {
        io.warn("IBX audio codec enable called for port A", 0);
        return;
    }
    io.wait_next_vblank(crtc.pipe);
    let Some(regs) = ibx_audio_regs_init(display, crtc.pipe) else {
        return;
    };
    io.mutex_lock();
    let set = if crtc.is_dp_encoder {
        AUD_CONFIG_N_VALUE_INDEX
    } else {
        audio_config_hdmi_pixel_clock(io, display.display_ver, crtc.crtc_clock)
    };
    intel_de_rmw(io, regs.aud_cntrl_st2, ibx_eld_valid(crtc.port), 0);
    intel_de_rmw(
        io,
        regs.aud_config,
        AUD_CONFIG_N_VALUE_INDEX | AUD_CONFIG_N_PROG_ENABLE | AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK,
        set,
    );
    io.mutex_unlock();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct QuietIo;
    impl LegacyAudioIo for QuietIo {
        fn read32(&mut self, _register: u32) -> u32 {
            0
        }
        fn write32(&mut self, _register: u32, _value: u32) {}
        fn wait_next_vblank(&mut self, _pipe: usize) {}
        fn eld_size(&mut self, eld: &[u8; ELD_BYTES]) -> usize {
            eld.len()
        }
        fn mutex_lock(&mut self) {}
        fn mutex_unlock(&mut self) {}
        fn debug(&mut self, _message: &'static str, _value: u32) {}
        fn warn(&mut self, _message: &'static str, _value: u32) {}
    }

    #[test]
    fn hdmi_audio_ncts_uses_matching_deep_color_table() {
        let mut crtc = AudioCrtcState {
            port_clock: TMDS_594M,
            pipe_bpp: 24,
            ..AudioCrtcState::default()
        };
        assert_eq!(audio_config_hdmi_get_n(&crtc, 48_000), 6144);
        crtc.pipe_bpp = 30;
        crtc.port_clock = TMDS_371M;
        assert_eq!(audio_config_hdmi_get_n(&crtc, 48_000), 5120);
        crtc.pipe_bpp = 36;
        crtc.port_clock = TMDS_445_5M;
        assert_eq!(audio_config_hdmi_get_n(&crtc, 48_000), 5120);
    }

    #[test]
    fn hdmi_pixel_clock_and_legacy_pipe_maps_follow_source_tables() {
        let mut io = QuietIo;
        assert_eq!(audio_config_hdmi_pixel_clock(&mut io, 13, 148_500), 9 << 16);
        assert_eq!(audio_config_hdmi_pixel_clock(&mut io, 11, 297_000), 1 << 16);
        assert_eq!(
            ibx_audio_regs_init(
                AudioPlatform {
                    valleyview: true,
                    ..AudioPlatform::default()
                },
                1,
            )
            .unwrap()
            .hdmiw_hdmiedid,
            VLV_DISPLAY_BASE + 0x62150
        );
        assert_eq!(
            ibx_audio_regs_init(
                AudioPlatform {
                    pch_ibx: true,
                    ..AudioPlatform::default()
                },
                0,
            )
            .unwrap()
            .aud_config,
            0xe2000
        );
        assert!(ibx_audio_regs_init(AudioPlatform::default(), 2).is_none());
    }
}
