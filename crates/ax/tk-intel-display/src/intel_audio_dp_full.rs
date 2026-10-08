/*
 * Copyright © 2014 Intel Corporation
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice (including the next
 * paragraph) shall be included in all copies or substantial portions of the
 * Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 */

//! Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_audio.c` DP-audio path.
//! Kernel facilities are deliberately exposed through `IntelAudioDpHooks`;
//! this file performs no implicit MMIO, DRM, audio-core, HDA, ELD, or usercopy.
#![allow(dead_code, non_camel_case_types, clippy::too_many_arguments)]

pub const ELD_BYTES: usize = 84;
pub const AUDIO_TRANSCODER_COUNT: usize = 8;
pub const MAX_PORTS: usize = 16;

const AUD_CONFIG_N_VALUE_INDEX: u32 = 1 << 29;
const AUD_CONFIG_N_PROG_ENABLE: u32 = 1 << 28;
const AUD_CONFIG_UPPER_N_MASK: u32 = 0xff << 20;
const AUD_CONFIG_LOWER_N_MASK: u32 = 0xfff << 4;
const AUD_CONFIG_N_MASK: u32 = AUD_CONFIG_UPPER_N_MASK | AUD_CONFIG_LOWER_N_MASK;
const AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK: u32 = 0xf << 16;
const AUD_M_CTS_M_VALUE_INDEX: u32 = 1 << 21;
const AUD_M_CTS_M_PROG_ENABLE: u32 = 1 << 20;
const AUDIO_OUTPUT_ENABLE_BIT: u32 = 1 << 2;
const AUDIO_ELD_VALID_BIT: u32 = 1;
const AUD_ENABLE_SDP_SPLIT: u32 = 1 << 31;
const DACBE_DISABLE_MIN_HBLANK_FIX: u32 = 1 << 18;
const AUD_TS_CDCLK_M_EN: u32 = 1 << 31;
const AUD_PIN_BUF_ENABLE: u32 = 1 << 31;
const SKL_AUD_CODEC_WAKE_SIGNAL: u32 = 1 << 15;
const AUD_FREQ_TMODE_SHIFT: u32 = 14;
const AUD_FREQ_8T: u32 = 2 << AUD_FREQ_TMODE_SHIFT;
const AUD_FREQ_PULLCLKS_0: u32 = 0;
const AUD_FREQ_PULLCLKS_2: u32 = 2 << 11;
const AUD_FREQ_BCLK_96M: u32 = 1 << 4;
const AUD_FREQ_GEN12: u32 = AUD_FREQ_8T | AUD_FREQ_PULLCLKS_0 | AUD_FREQ_BCLK_96M;
const AUD_FREQ_TGL_BROKEN: u32 = AUD_FREQ_8T | AUD_FREQ_PULLCLKS_2 | AUD_FREQ_BCLK_96M;
const EDEADLK: i32 = -35;
const EEXIST: i32 = -17;
const ENOMEM: i32 = -12;
const ENODEV: i32 = -19;
const EINVAL: i32 = -22;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioRegister {
    HswAudCfg(usize),
    HswAudMCtsEnable(usize),
    HswAudPinEldCpVld,
    AudChickenbitReg3,
    AudConfigBe,
    AudDp20Ctrl(usize),
    AudTsCdclkM,
    AudTsCdclkN,
    AudFreqControl,
    AudPinBufferControl,
    HswAudChickenbit,
}

/// Safe representation of an i915 encoder class relevant to audio routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputType {
    DisplayPort,
    DisplayPortMst,
    Hdmi,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecGeneration {
    G4x,
    IbexPeak,
    HaswellOrNewer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelEncoder {
    pub id: i32,
    pub name: &'static str,
    pub port: usize,
    pub output: OutputType,
    pub current_crtc: Option<CrtcInfo>,
    pub current_state: Option<BoxlessCrtcState>,
}

/// The state snapshot held by an encoder's legacy `base.crtc->config` pointer.
/// It intentionally contains only fields consumed by this translation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoxlessCrtcState {
    pub has_audio: bool,
    pub is_dp_encoder: bool,
    pub is_dp_mst: bool,
    pub cpu_transcoder: usize,
    pub sdp_split_enable: bool,
    pub port_clock: u32,
    pub lane_count: u32,
    pub mode: DisplayMode,
    pub dsc: DscState,
    pub eld: [u8; ELD_BYTES],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayMode {
    pub crtc_hdisplay: u32,
    pub crtc_htotal: u32,
    pub crtc_clock: u32,
    pub hdisplay: u32,
    pub htotal: u32,
    pub clock: u32,
    pub vdisplay: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DscState {
    pub compression_enable: bool,
    pub compressed_bpp_x16: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrtcInfo {
    pub id: i32,
    pub name: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Connector {
    pub id: i32,
    pub name: &'static str,
    pub eld: [u8; ELD_BYTES],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrmConnectorState {
    pub connector: Connector,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelCrtcState {
    pub uapi_crtc: Option<CrtcInfo>,
    pub has_audio: bool,
    pub is_dp_encoder: bool,
    pub is_dp_mst: bool,
    pub sdp_split_enable: bool,
    pub cpu_transcoder: usize,
    pub port_clock: u32,
    pub lane_count: u32,
    pub mode: DisplayMode,
    pub dsc: DscState,
    pub eld: [u8; ELD_BYTES],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelAudioState {
    pub encoder: Option<IntelEncoder>,
    pub eld: [u8; ELD_BYTES],
}

impl Default for IntelAudioState {
    fn default() -> Self {
        Self {
            encoder: None,
            eld: [0; ELD_BYTES],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioPlatform {
    pub g4x: bool,
    pub valleyview: bool,
    pub cherryview: bool,
    pub pch_cpt: bool,
    pub pch_ibx: bool,
    pub haswell: bool,
    pub broadwell: bool,
    pub geminilake: bool,
    pub tigerlake: bool,
    pub rocketlake: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelAudioPrivate {
    pub funcs: Option<CodecGeneration>,
    pub component_bound: bool,
    pub component_registered: bool,
    pub lpe_platdev: bool,
    pub power_refcount: i32,
    pub freq_cntrl: u32,
    pub sample_rate: [i32; MAX_PORTS],
    pub state: [IntelAudioState; AUDIO_TRANSCODER_COUNT],
}

impl Default for IntelAudioPrivate {
    fn default() -> Self {
        Self {
            funcs: None,
            component_bound: false,
            component_registered: false,
            lpe_platdev: false,
            power_refcount: 0,
            freq_cntrl: 0,
            sample_rate: [0; MAX_PORTS],
            state: [IntelAudioState::default(); AUDIO_TRANSCODER_COUNT],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelAudioDisplay {
    pub drm_id: usize,
    pub display_ver: u8,
    pub has_ddi: bool,
    pub has_dp20: bool,
    pub platform: AudioPlatform,
    pub cdclk_ref: i32,
    pub cdclk: i32,
    pub audio: IntelAudioPrivate,
}

impl Default for IntelAudioDisplay {
    fn default() -> Self {
        Self {
            drm_id: 0,
            display_ver: 0,
            has_ddi: false,
            has_dp20: false,
            platform: AudioPlatform::default(),
            cdclk_ref: 0,
            cdclk: 0,
            audio: IntelAudioPrivate::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioComponent {
    pub ops_installed: bool,
    pub device_attached: bool,
    pub audio_ops_present: bool,
    pub pin_eld_notify_present: bool,
}

impl Default for AudioComponent {
    fn default() -> Self {
        Self {
            ops_installed: false,
            device_attached: false,
            audio_ops_present: false,
            pin_eld_notify_present: false,
        }
    }
}

/// Explicit adapter surface for kernel services. `read32`/`write32` are the
/// only route to MMIO; lock, DRM, audio-core/HDA, ELD, and usercopy operations
/// are individually visible at call sites and may fail closed in an adapter.
pub trait IntelAudioDpHooks {
    fn read32(&mut self, register: AudioRegister) -> u32;
    fn write32(&mut self, register: AudioRegister, value: u32);

    fn rmw(&mut self, register: AudioRegister, clear: u32, set: u32) {
        let old = self.read32(register);
        self.write32(register, (old & !clear) | set);
    }

    fn drm_debug(&mut self, drm_id: usize, message: &str);
    fn drm_warn(&mut self, drm_id: usize, message: &str);
    fn drm_error(&mut self, drm_id: usize, message: &str);
    fn mutex_lock(&mut self, drm_id: usize);
    fn mutex_unlock(&mut self, drm_id: usize);
    fn modeset_lock_all(&mut self, drm_id: usize);
    fn modeset_unlock_all(&mut self, drm_id: usize);
    fn wait_vblank(&mut self, crtc: CrtcInfo);
    fn display_wa_14020863754(&mut self, drm_id: usize) -> bool;

    fn connector_eld_lock(&mut self, connector_id: i32);
    fn connector_eld_unlock(&mut self, connector_id: i32);
    fn drm_av_sync_delay(&mut self, connector: &Connector, mode: &DisplayMode) -> i32;
    fn drm_eld_size(&mut self, eld: &[u8; ELD_BYTES]) -> usize;
    fn copy_eld_to_user(&mut self, source: &[u8], destination: &mut [u8], count: usize);

    fn hsw_hdmi_audio_config_update(
        &mut self,
        display: &mut IntelAudioDisplay,
        encoder: &IntelEncoder,
        crtc_state: &IntelCrtcState,
    );
    fn legacy_codec_enable(
        &mut self,
        generation: CodecGeneration,
        encoder: &IntelEncoder,
        crtc_state: &IntelCrtcState,
        conn_state: &DrmConnectorState,
    );
    fn legacy_codec_disable(
        &mut self,
        generation: CodecGeneration,
        encoder: &IntelEncoder,
        old_crtc_state: &IntelCrtcState,
        old_conn_state: &DrmConnectorState,
    );
    fn legacy_codec_get_config(
        &mut self,
        generation: CodecGeneration,
        encoder: &IntelEncoder,
        crtc_state: &mut IntelCrtcState,
    );
    fn audio_ops_has_pin_eld_notify(&mut self) -> bool;
    fn pin_eld_notify(&mut self, port: i32, cpu_transcoder: i32);
    fn lpe_audio_notify(
        &mut self,
        cpu_transcoder: i32,
        port: i32,
        eld: Option<&[u8; ELD_BYTES]>,
        port_clock: u32,
        is_dp_encoder: bool,
    );

    fn audio_power_get(&mut self, drm_id: usize) -> u64;
    fn audio_power_put(&mut self, drm_id: usize, cookie: u64);
    fn sleep_range_us(&mut self, minimum: u32, maximum: u32);
    fn ddi_supported(&mut self, display: &IntelAudioDisplay) -> bool;

    fn audio_atomic_modeset_lock(&mut self, crtc: CrtcInfo) -> i32;
    fn audio_atomic_get_cdclk_state(&mut self) -> Result<(), i32>;
    fn audio_atomic_force_min_cdclk(&mut self, minimum: i32);
    fn audio_atomic_commit(&mut self) -> i32;
    fn audio_atomic_commit_alloc(&mut self, drm_id: usize) -> Option<u64>;
    fn audio_atomic_commit_set_internal(&mut self, commit: u64);
    fn audio_atomic_commit_clear(&mut self, commit: u64);
    fn audio_atomic_commit_put(&mut self, commit: u64);
    fn audio_modeset_acquire_init(&mut self) -> u64;
    fn audio_modeset_backoff(&mut self, acquire_ctx: u64);
    fn audio_modeset_drop_locks(&mut self, acquire_ctx: u64);
    fn audio_modeset_acquire_fini(&mut self, acquire_ctx: u64);
    fn first_crtc(&mut self, drm_id: usize) -> Option<CrtcInfo>;

    fn device_link_add_stateless(&mut self, hda_device: usize, display_device: usize) -> bool;
    fn device_link_remove(&mut self, hda_device: usize, display_device: usize);
    fn component_add_typed_audio(&mut self, drm_id: usize) -> i32;
    fn component_del_audio(&mut self, drm_id: usize);
    fn intel_lpe_audio_init(&mut self, drm_id: usize) -> i32;
    fn intel_lpe_audio_teardown(&mut self, drm_id: usize);
}

/// Linux `intel_de_write()`-ordered register update.
fn intel_de_rmw<H: IntelAudioDpHooks>(
    hooks: &mut H,
    register: AudioRegister,
    clear: u32,
    set: u32,
) {
    hooks.rmw(register, clear, set);
}

fn transcoder_output_enable(transcoder: usize) -> u32 {
    AUDIO_OUTPUT_ENABLE_BIT << (transcoder * 4)
}

fn transcoder_eld_valid(transcoder: usize) -> u32 {
    AUDIO_ELD_VALID_BIT << (transcoder * 4)
}

/// upstream: intel_audio.c hsw_dp_audio_config_update()
pub fn hsw_dp_audio_config_update<H: IntelAudioDpHooks>(
    hooks: &mut H,
    _display: &mut IntelAudioDisplay,
    _encoder: &IntelEncoder,
    crtc_state: &IntelCrtcState,
) {
    let transcoder = crtc_state.cpu_transcoder;
    // Enable time stamps. Let HW calculate Maud/Naud values.
    intel_de_rmw(
        hooks,
        AudioRegister::HswAudCfg(transcoder),
        AUD_CONFIG_N_VALUE_INDEX
            | AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK
            | AUD_CONFIG_UPPER_N_MASK
            | AUD_CONFIG_LOWER_N_MASK
            | AUD_CONFIG_N_PROG_ENABLE,
        AUD_CONFIG_N_VALUE_INDEX,
    );
}

/// upstream: intel_audio.c hsw_audio_config_update()
pub fn hsw_audio_config_update<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    encoder: &IntelEncoder,
    crtc_state: &IntelCrtcState,
) {
    if crtc_state.is_dp_encoder {
        hsw_dp_audio_config_update(hooks, display, encoder, crtc_state);
    } else {
        // The HDMI-only implementation remains an explicit sibling-module hook.
        hooks.hsw_hdmi_audio_config_update(display, encoder, crtc_state);
    }
}

/// upstream: intel_audio.c intel_audio_sdp_split_update()
pub fn intel_audio_sdp_split_update<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    crtc_state: &IntelCrtcState,
    enable: bool,
) {
    if !display.has_dp20 {
        return;
    }
    let split = enable && crtc_state.sdp_split_enable;
    intel_de_rmw(
        hooks,
        AudioRegister::AudDp20Ctrl(crtc_state.cpu_transcoder),
        AUD_ENABLE_SDP_SPLIT,
        if split { AUD_ENABLE_SDP_SPLIT } else { 0 },
    );
}

/// upstream: intel_audio.c hsw_audio_codec_disable()
pub fn hsw_audio_codec_disable<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    _encoder: &IntelEncoder,
    old_crtc_state: &IntelCrtcState,
    _old_conn_state: &DrmConnectorState,
) {
    let Some(crtc) = old_crtc_state.uapi_crtc else {
        hooks.drm_warn(display.drm_id, "audio disable without CRTC");
        return;
    };
    let cpu_transcoder = old_crtc_state.cpu_transcoder;
    hooks.mutex_lock(display.drm_id);

    // Disable timestamps.
    intel_de_rmw(
        hooks,
        AudioRegister::HswAudCfg(cpu_transcoder),
        AUD_CONFIG_N_VALUE_INDEX | AUD_CONFIG_UPPER_N_MASK | AUD_CONFIG_LOWER_N_MASK,
        AUD_CONFIG_N_PROG_ENABLE
            | if old_crtc_state.is_dp_encoder {
                AUD_CONFIG_N_VALUE_INDEX
            } else {
                0
            },
    );

    // Invalidate ELD, wait two vblanks, and disable audio presence detect.
    intel_de_rmw(
        hooks,
        AudioRegister::HswAudPinEldCpVld,
        transcoder_eld_valid(cpu_transcoder),
        0,
    );
    hooks.wait_vblank(crtc);
    hooks.wait_vblank(crtc);
    intel_de_rmw(
        hooks,
        AudioRegister::HswAudPinEldCpVld,
        transcoder_output_enable(cpu_transcoder),
        0,
    );

    // WA_14020863754: a Min Hblank corner case can hang audio.
    if hooks.display_wa_14020863754(display.drm_id) {
        intel_de_rmw(
            hooks,
            AudioRegister::AudChickenbitReg3,
            DACBE_DISABLE_MIN_HBLANK_FIX,
            0,
        );
    }
    intel_audio_sdp_split_update(hooks, display, old_crtc_state, false);
    hooks.mutex_unlock(display.drm_id);
}

/// upstream: intel_audio.c calc_hblank_early_prog()
pub fn calc_hblank_early_prog<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &IntelAudioDisplay,
    _encoder: &IntelEncoder,
    crtc_state: &IntelCrtcState,
) -> u32 {
    let h_active = crtc_state.mode.crtc_hdisplay;
    let h_total = crtc_state.mode.crtc_htotal;
    let pixel_clk = crtc_state.mode.crtc_clock;
    let vdsc_bppx16 = crtc_state.dsc.compressed_bpp_x16;
    let cdclk = display.cdclk as u32;
    let fec_coeff = 972_261_u64;
    let link_clk = crtc_state.port_clock;
    let lanes = crtc_state.lane_count;

    hooks.drm_debug(
        display.drm_id,
        "calc_hblank_early_prog: h_active/link_clk/lanes/bpp_x16/cdclk captured",
    );
    if link_clk == 0 || pixel_clk == 0 || lanes == 0 || vdsc_bppx16 == 0 || cdclk == 0 {
        hooks.drm_warn(display.drm_id, "invalid DP DSC audio clock or lane parameters");
        return 0;
    }

    let blank = h_total.saturating_sub(h_active);
    let link_clks_available = blank
        .saturating_mul(link_clk)
        .checked_div(pixel_clk)
        .unwrap_or(0)
        .saturating_sub(28);
    let link_clks_required = div_round_up_u64(192_000_u64 * u64::from(h_total),
                                             1_000_u64 * u64::from(pixel_clk))
        * u64::from(48 / lanes + 2);

    let hblank_delta = if u64::from(link_clks_available) > link_clks_required {
        32_u64
    } else {
        div_round_up_u64(
            5 * (u64::from(link_clk) + u64::from(cdclk)) * u64::from(pixel_clk),
            u64::from(link_clk) * u64::from(cdclk),
        )
    };

    let tu_data = (u64::from(pixel_clk) * u64::from(vdsc_bppx16) * 8 * 1_000_000)
        / (u64::from(link_clk) * u64::from(lanes) * 16 * fec_coeff);
    let tu_line = (u64::from(h_active) * u64::from(link_clk) * fec_coeff)
        / (64 * u64::from(pixel_clk) * 1_000_000);
    let link_clks_active = tu_line.saturating_sub(1) * 64 + tu_data;
    let fec_deskew = div_round_up_u64(link_clks_active, 250);
    let hblank_rise = (link_clks_active + 6 * fec_deskew + 4) * u64::from(pixel_clk)
        / u64::from(link_clk);
    u64::from(h_active)
        .saturating_sub(hblank_rise)
        .saturating_add(hblank_delta)
        .min(u64::from(u32::MAX)) as u32
}

/// upstream: intel_audio.c calc_samples_room()
pub fn calc_samples_room(crtc_state: &IntelCrtcState) -> u32 {
    let h_active = crtc_state.mode.hdisplay;
    let h_total = crtc_state.mode.htotal;
    let pixel_clk = crtc_state.mode.clock;
    let link_clk = crtc_state.port_clock;
    let lanes = crtc_state.lane_count;
    if pixel_clk == 0 || lanes == 0 {
        return 0;
    }
    let numerator = u64::from(h_total.saturating_sub(h_active)) * u64::from(link_clk);
    let numerator = numerator.saturating_sub(12 * u64::from(pixel_clk));
    let denominator = u64::from(pixel_clk) * u64::from(48 / lanes + 2);
    if denominator == 0 {
        0
    } else {
        (numerator / denominator).min(u64::from(u32::MAX)) as u32
    }
}

/// upstream: intel_audio.c enable_audio_dsc_wa()
pub fn enable_audio_dsc_wa<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &IntelAudioDisplay,
    encoder: &IntelEncoder,
    crtc_state: &IntelCrtcState,
) {
    if display.display_ver < 11 {
        return;
    }
    let transcoder = crtc_state.cpu_transcoder;
    let mut value = hooks.read32(AudioRegister::AudConfigBe);
    if display.display_ver == 11 {
        value |= 1_u32 << (20 - transcoder);
    } else {
        value |= 1_u32 << (24 + transcoder);
    }

    if crtc_state.dsc.compression_enable
        && crtc_state.mode.hdisplay >= 3840
        && crtc_state.mode.vdisplay >= 2160
    {
        value &= !(0x7 << (3 + transcoder * 6));
        let hblank_early = calc_hblank_early_prog(hooks, display, encoder, crtc_state);
        let count = if hblank_early < 32 {
            2 // HBLANK_START_COUNT_32
        } else if hblank_early < 64 {
            3 // HBLANK_START_COUNT_64
        } else if hblank_early < 96 {
            4 // HBLANK_START_COUNT_96
        } else {
            5 // HBLANK_START_COUNT_128
        };
        value |= (count & 0x7) << (3 + transcoder * 6);

        value &= !(0x3 << (transcoder * 6));
        let samples_room = calc_samples_room(crtc_state);
        // Zero means all samples available in the buffer.
        value |= if samples_room < 3 {
            (samples_room & 0x3) << (transcoder * 6)
        } else {
            0
        };
    }
    hooks.write32(AudioRegister::AudConfigBe, value);
}

/// upstream: intel_audio.c hsw_audio_codec_enable()
pub fn hsw_audio_codec_enable<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    encoder: &IntelEncoder,
    crtc_state: &IntelCrtcState,
    _conn_state: &DrmConnectorState,
) {
    let Some(crtc) = crtc_state.uapi_crtc else {
        hooks.drm_warn(display.drm_id, "audio enable without CRTC");
        return;
    };
    let cpu_transcoder = crtc_state.cpu_transcoder;
    hooks.mutex_lock(display.drm_id);

    // Enable audio WA for 4K DSC DP usecases.
    if crtc_state.is_dp_encoder {
        enable_audio_dsc_wa(hooks, display, encoder, crtc_state);
    }
    intel_audio_sdp_split_update(hooks, display, crtc_state, true);

    // WA_14020863754: implement the Min Hblank audio workaround.
    if hooks.display_wa_14020863754(display.drm_id) {
        intel_de_rmw(
            hooks,
            AudioRegister::AudChickenbitReg3,
            0,
            DACBE_DISABLE_MIN_HBLANK_FIX,
        );
    }

    // Enable audio presence detect and invalidate ELD after one vblank.
    intel_de_rmw(
        hooks,
        AudioRegister::HswAudPinEldCpVld,
        0,
        transcoder_output_enable(cpu_transcoder),
    );
    hooks.wait_vblank(crtc);
    intel_de_rmw(
        hooks,
        AudioRegister::HswAudPinEldCpVld,
        transcoder_eld_valid(cpu_transcoder),
        0,
    );

    // The audio component conveys ELD; HSW does not use the hardware ELD buffer.
    hsw_audio_config_update(hooks, display, encoder, crtc_state);
    hooks.mutex_unlock(display.drm_id);
}

/// upstream: intel_audio.c intel_audio_compute_config()
pub fn intel_audio_compute_config<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    _encoder: &IntelEncoder,
    crtc_state: &mut IntelCrtcState,
    conn_state: &DrmConnectorState,
) -> bool {
    let connector = &conn_state.connector;
    hooks.connector_eld_lock(connector.id);
    if connector.eld[0] == 0 {
        hooks.drm_debug(display.drm_id, "Bogus ELD on connector");
        hooks.connector_eld_unlock(connector.id);
        return false;
    }

    crtc_state.eld = connector.eld;
    crtc_state.eld[6] = (hooks.drm_av_sync_delay(connector, &crtc_state.mode) / 2) as u8;
    hooks.connector_eld_unlock(connector.id);
    true
}

fn audio_state_index(state: &IntelCrtcState, drm_id: usize, hooks: &mut impl IntelAudioDpHooks) -> Option<usize> {
    if state.cpu_transcoder >= AUDIO_TRANSCODER_COUNT {
        hooks.drm_warn(drm_id, "CPU transcoder out of audio-state range");
        None
    } else {
        Some(state.cpu_transcoder)
    }
}

/// upstream: intel_audio.c intel_audio_codec_enable()
pub fn intel_audio_codec_enable<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    encoder: &IntelEncoder,
    crtc_state: &IntelCrtcState,
    conn_state: &DrmConnectorState,
) {
    if !crtc_state.has_audio {
        return;
    }
    hooks.drm_debug(display.drm_id, "enable codec after link training; ELD size queried by DRM hook");

    if let Some(generation) = display.audio.funcs {
        if generation == CodecGeneration::HaswellOrNewer {
            hsw_audio_codec_enable(hooks, display, encoder, crtc_state, conn_state);
        } else {
            hooks.legacy_codec_enable(generation, encoder, crtc_state, conn_state);
        }
    }

    let Some(index) = audio_state_index(crtc_state, display.drm_id, hooks) else {
        return;
    };
    hooks.mutex_lock(display.drm_id);
    display.audio.state[index].encoder = Some(*encoder);
    display.audio.state[index].eld = crtc_state.eld;
    hooks.mutex_unlock(display.drm_id);

    if display.audio.component_bound && hooks.audio_ops_has_pin_eld_notify() {
        let cpu_transcoder = if crtc_state.is_dp_mst {
            crtc_state.cpu_transcoder as i32
        } else {
            -1
        };
        hooks.pin_eld_notify(encoder.port as i32, cpu_transcoder);
    }
    hooks.lpe_audio_notify(
        if crtc_state.is_dp_mst {
            crtc_state.cpu_transcoder as i32
        } else {
            -1
        },
        encoder.port as i32,
        Some(&crtc_state.eld),
        crtc_state.port_clock,
        crtc_state.is_dp_encoder,
    );
}

/// upstream: intel_audio.c intel_audio_codec_disable()
pub fn intel_audio_codec_disable<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    encoder: &IntelEncoder,
    old_crtc_state: &IntelCrtcState,
    old_conn_state: &DrmConnectorState,
) {
    if !old_crtc_state.has_audio {
        return;
    }
    hooks.drm_debug(display.drm_id, "disable codec before disabling transcoder or port");

    if let Some(generation) = display.audio.funcs {
        if generation == CodecGeneration::HaswellOrNewer {
            hsw_audio_codec_disable(hooks, display, encoder, old_crtc_state, old_conn_state);
        } else {
            hooks.legacy_codec_disable(generation, encoder, old_crtc_state, old_conn_state);
        }
    }

    let Some(index) = audio_state_index(old_crtc_state, display.drm_id, hooks) else {
        return;
    };
    hooks.mutex_lock(display.drm_id);
    display.audio.state[index].encoder = None;
    display.audio.state[index].eld = [0; ELD_BYTES];
    hooks.mutex_unlock(display.drm_id);

    if display.audio.component_bound && hooks.audio_ops_has_pin_eld_notify() {
        let cpu_transcoder = if old_crtc_state.is_dp_mst {
            old_crtc_state.cpu_transcoder as i32
        } else {
            -1
        };
        hooks.pin_eld_notify(encoder.port as i32, cpu_transcoder);
    }
    hooks.lpe_audio_notify(
        if old_crtc_state.is_dp_mst {
            old_crtc_state.cpu_transcoder as i32
        } else {
            -1
        },
        encoder.port as i32,
        None,
        0,
        false,
    );
}

/// upstream: intel_audio.c intel_acomp_get_config()
pub fn intel_acomp_get_config<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    _encoder: &IntelEncoder,
    crtc_state: &mut IntelCrtcState,
) {
    let Some(index) = audio_state_index(crtc_state, display.drm_id, hooks) else {
        return;
    };
    hooks.mutex_lock(display.drm_id);
    if display.audio.state[index].encoder.is_some() {
        crtc_state.eld = display.audio.state[index].eld;
    }
    hooks.mutex_unlock(display.drm_id);
}

/// upstream: intel_audio.c intel_audio_codec_get_config()
pub fn intel_audio_codec_get_config<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    encoder: &IntelEncoder,
    crtc_state: &mut IntelCrtcState,
) {
    if !crtc_state.has_audio {
        return;
    }
    if let Some(generation) = display.audio.funcs {
        if generation == CodecGeneration::HaswellOrNewer {
            intel_acomp_get_config(hooks, display, encoder, crtc_state);
        } else {
            hooks.legacy_codec_get_config(generation, encoder, crtc_state);
        }
    }
}

/// upstream: intel_audio.c intel_audio_hooks_init()
pub fn intel_audio_hooks_init(display: &mut IntelAudioDisplay) {
    let platform = display.platform;
    display.audio.funcs = if platform.g4x {
        Some(CodecGeneration::G4x)
    } else if platform.valleyview || platform.cherryview || platform.pch_cpt || platform.pch_ibx {
        Some(CodecGeneration::IbexPeak)
    } else if platform.haswell || display.display_ver >= 8 {
        Some(CodecGeneration::HaswellOrNewer)
    } else {
        None
    };
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudTsCdclkMN {
    pub m: u8,
    pub n: u16,
}

/// upstream: intel_audio.c intel_audio_cdclk_change_pre()
pub fn intel_audio_cdclk_change_pre<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &IntelAudioDisplay,
) {
    if display.display_ver >= 13 {
        intel_de_rmw(hooks, AudioRegister::AudTsCdclkM, AUD_TS_CDCLK_M_EN, 0);
    }
}

/// upstream: intel_audio.c get_aud_ts_cdclk_m_n()
pub fn get_aud_ts_cdclk_m_n(refclk: i32, cdclk: i32) -> AudTsCdclkMN {
    let _ = refclk; // Retained in the upstream signature; the BSpec ratio ignores it.
    let m = 60_u8;
    let n = ((cdclk * i32::from(m)) / 24_000) as u16;
    AudTsCdclkMN { m, n }
}

/// upstream: intel_audio.c intel_audio_cdclk_change_post()
pub fn intel_audio_cdclk_change_post<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &IntelAudioDisplay,
) {
    if display.display_ver >= 13 {
        let aud_ts = get_aud_ts_cdclk_m_n(display.cdclk_ref, display.cdclk);
        hooks.write32(AudioRegister::AudTsCdclkN, u32::from(aud_ts.n));
        hooks.write32(
            AudioRegister::AudTsCdclkM,
            u32::from(aud_ts.m) | AUD_TS_CDCLK_M_EN,
        );
        hooks.drm_debug(display.drm_id, "aud_ts_cdclk M/N programmed");
    }
}

/// upstream: intel_audio.c glk_force_audio_cdclk_commit()
pub fn glk_force_audio_cdclk_commit<H: IntelAudioDpHooks>(
    hooks: &mut H,
    _display: &IntelAudioDisplay,
    crtc: CrtcInfo,
    enable: bool,
) -> i32 {
    // Hold at least one CRTC lock for the global state.
    let ret = hooks.audio_atomic_modeset_lock(crtc);
    if ret != 0 {
        return ret;
    }
    if let Err(err) = hooks.audio_atomic_get_cdclk_state() {
        return err;
    }
    hooks.audio_atomic_force_min_cdclk(if enable { 2 * 96_000 } else { 0 });
    hooks.audio_atomic_commit()
}

/// upstream: intel_audio.c glk_force_audio_cdclk()
pub fn glk_force_audio_cdclk<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &IntelAudioDisplay,
    enable: bool,
) {
    let Some(crtc) = hooks.first_crtc(display.drm_id) else {
        return;
    };
    let acquire_ctx = hooks.audio_modeset_acquire_init();
    let Some(commit) = hooks.audio_atomic_commit_alloc(display.drm_id) else {
        hooks.drm_warn(display.drm_id, "audio CDCLK atomic commit allocation failed");
        return;
    };
    hooks.audio_atomic_commit_set_internal(commit);

    loop {
        let ret = glk_force_audio_cdclk_commit(hooks, display, crtc, enable);
        if ret == EDEADLK {
            hooks.audio_atomic_commit_clear(commit);
            hooks.audio_modeset_backoff(acquire_ctx);
            continue;
        }
        if ret != 0 {
            hooks.drm_warn(display.drm_id, "audio CDCLK force commit failed");
        }
        break;
    }

    hooks.audio_atomic_commit_put(commit);
    hooks.audio_modeset_drop_locks(acquire_ctx);
    hooks.audio_modeset_acquire_fini(acquire_ctx);
}

/// upstream: intel_audio.c intel_audio_min_cdclk()
pub fn intel_audio_min_cdclk(display: &IntelAudioDisplay, crtc_state: &IntelCrtcState) -> i32 {
    if !crtc_state.has_audio {
        return 0;
    }
    let mut min_cdclk = 0;

    if crtc_state.is_dp_encoder && crtc_state.port_clock >= 540_000 && crtc_state.lane_count == 4 {
        if display.display_ver == 10 {
            min_cdclk = min_cdclk.max(316_800); // Display WA #1145: GLK.
        } else if display.display_ver == 9 || display.platform.broadwell {
            min_cdclk = min_cdclk.max(432_000); // Display WA #1144: SKL/BXT.
        }
    }

    // BCLK is 96MHz; CDCLK must be at least twice BCLK.
    if display.display_ver >= 9 {
        min_cdclk = min_cdclk.max(2 * 96_000);
    }

    // Valleyview/Cherryview DP audio requires CDCLK >= the DP link frequency.
    if (display.platform.valleyview || display.platform.cherryview) && crtc_state.is_dp_encoder {
        min_cdclk = min_cdclk.max(crtc_state.port_clock as i32);
    }
    min_cdclk
}

/// upstream: intel_audio.c intel_audio_component_get_power()
pub fn intel_audio_component_get_power<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
) -> u64 {
    let cookie = hooks.audio_power_get(display.drm_id);
    display.audio.power_refcount += 1;
    if display.audio.power_refcount == 1 {
        if display.display_ver >= 9 {
            hooks.write32(AudioRegister::AudFreqControl, display.audio.freq_cntrl);
            hooks.drm_debug(display.drm_id, "restored AUD_FREQ_CNTRL");
        }
        // Force CDCLK to 2*BCLK while audio power is held on Gemini Lake.
        if display.platform.geminilake {
            glk_force_audio_cdclk(hooks, display, true);
        }
        if display.display_ver >= 10 {
            intel_de_rmw(hooks, AudioRegister::AudPinBufferControl, 0, AUD_PIN_BUF_ENABLE);
        }
    }
    cookie
}

/// upstream: intel_audio.c intel_audio_component_put_power()
pub fn intel_audio_component_put_power<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    cookie: u64,
) {
    display.audio.power_refcount -= 1;
    if display.audio.power_refcount == 0 && display.platform.geminilake {
        glk_force_audio_cdclk(hooks, display, false);
    }
    hooks.audio_power_put(display.drm_id, cookie);
}

/// upstream: intel_audio.c intel_audio_component_codec_wake_override()
pub fn intel_audio_component_codec_wake_override<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    enable: bool,
) {
    if display.display_ver < 9 {
        return;
    }
    let cookie = intel_audio_component_get_power(hooks, display);
    // Override the controller's internal codec-wake generation logic.
    intel_de_rmw(
        hooks,
        AudioRegister::HswAudChickenbit,
        SKL_AUD_CODEC_WAKE_SIGNAL,
        0,
    );
    hooks.sleep_range_us(1_000, 1_500);
    if enable {
        intel_de_rmw(
            hooks,
            AudioRegister::HswAudChickenbit,
            0,
            SKL_AUD_CODEC_WAKE_SIGNAL,
        );
        hooks.sleep_range_us(1_000, 1_500);
    }
    intel_audio_component_put_power(hooks, display, cookie);
}

/// upstream: intel_audio.c intel_audio_component_get_cdclk_freq()
pub fn intel_audio_component_get_cdclk_freq<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &IntelAudioDisplay,
) -> Result<i32, i32> {
    if !hooks.ddi_supported(display) {
        hooks.drm_warn(display.drm_id, "audio CDCLK query requires DDI");
        return Err(ENODEV);
    }
    Ok(display.cdclk)
}

/// upstream: intel_audio.c find_audio_state()
pub fn find_audio_state<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &IntelAudioDisplay,
    port: usize,
    mut cpu_transcoder: i32,
) -> Option<usize> {
    // MST: only the explicitly supplied CPU transcoder can match.
    if cpu_transcoder >= 0 {
        if cpu_transcoder as usize >= display.audio.state.len() {
            hooks.drm_warn(display.drm_id, "audio transcoder is out of state-array bounds");
            return None;
        }
        let index = cpu_transcoder as usize;
        let encoder = display.audio.state[index].encoder;
        if let Some(encoder) = encoder {
            if encoder.port == port && encoder.output == OutputType::DisplayPortMst {
                return Some(index);
            }
        }
    }

    // Non-MST: a positive transcoder argument is invalid.
    if cpu_transcoder > 0 {
        return None;
    }
    for index in 0..display.audio.state.len() {
        cpu_transcoder = index as i32;
        let encoder = display.audio.state[index].encoder;
        if let Some(encoder) = encoder {
            if encoder.port == port && encoder.output != OutputType::DisplayPortMst {
                return Some(index);
            }
        }
    }
    let _ = cpu_transcoder;
    None
}

/// upstream: intel_audio.c intel_audio_component_sync_audio_rate()
pub fn intel_audio_component_sync_audio_rate<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    port: usize,
    cpu_transcoder: i32,
    rate: i32,
) -> i32 {
    if !hooks.ddi_supported(display) {
        return 0;
    }
    let cookie = intel_audio_component_get_power(hooks, display);
    hooks.mutex_lock(display.drm_id);

    let state_index = find_audio_state(hooks, display, port, cpu_transcoder);
    let result = if let Some(index) = state_index {
        if port >= MAX_PORTS {
            ENODEV
        } else {
            let Some(encoder) = display.audio.state[index].encoder else {
                hooks.mutex_unlock(display.drm_id);
                intel_audio_component_put_power(hooks, display, cookie);
                return ENODEV;
            };
            display.audio.sample_rate[port] = rate;
            // The legacy CRTC config remains the source for the active encoder.
            if let Some(crtc_state) = encoder.current_state {
                hsw_audio_config_update(hooks, display, &encoder, &IntelCrtcState {
                    uapi_crtc: encoder.current_crtc,
                    has_audio: crtc_state.has_audio,
                    is_dp_encoder: crtc_state.is_dp_encoder,
                    is_dp_mst: crtc_state.is_dp_mst,
                    sdp_split_enable: crtc_state.sdp_split_enable,
                    cpu_transcoder: crtc_state.cpu_transcoder,
                    port_clock: crtc_state.port_clock,
                    lane_count: crtc_state.lane_count,
                    mode: crtc_state.mode,
                    dsc: crtc_state.dsc,
                    eld: crtc_state.eld,
                });
                0
            } else {
                ENODEV
            }
        }
    } else {
        hooks.drm_debug(display.drm_id, "audio rate sync rejected for unmatched port/transcoder");
        ENODEV
    };

    hooks.mutex_unlock(display.drm_id);
    intel_audio_component_put_power(hooks, display, cookie);
    result
}

/// upstream: intel_audio.c intel_audio_component_get_eld()
pub fn intel_audio_component_get_eld<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    port: usize,
    cpu_transcoder: i32,
    enabled: &mut bool,
    user_buffer: &mut [u8],
    max_bytes: i32,
) -> i32 {
    hooks.mutex_lock(display.drm_id);
    let Some(index) = find_audio_state(hooks, display, port, cpu_transcoder) else {
        hooks.drm_debug(display.drm_id, "ELD query rejected for unmatched port/transcoder");
        hooks.mutex_unlock(display.drm_id);
        return EINVAL;
    };

    let audio_state = display.audio.state[index];
    *enabled = audio_state.encoder.is_some();
    let ret = if *enabled {
        let size = hooks.drm_eld_size(&audio_state.eld);
        let count = usize::try_from(max_bytes.max(0)).unwrap_or(0).min(size);
        let copy_count = count.min(user_buffer.len());
        hooks.copy_eld_to_user(
            &audio_state.eld[..size.min(ELD_BYTES)],
            user_buffer,
            copy_count,
        );
        size as i32
    } else {
        0
    };
    hooks.mutex_unlock(display.drm_id);
    ret
}

/// upstream: intel_audio.c intel_audio_component_bind()
pub fn intel_audio_component_bind<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    hda_device: usize,
    display_device: usize,
    component: &mut AudioComponent,
) -> i32 {
    if component.ops_installed || component.device_attached {
        hooks.drm_warn(display.drm_id, "audio component already has ops or a device");
        return EEXIST;
    }
    if !hooks.device_link_add_stateless(hda_device, display_device) {
        hooks.drm_warn(display.drm_id, "failed to add HDA/display device link");
        return ENOMEM;
    }

    hooks.modeset_lock_all(display.drm_id);
    component.ops_installed = true;
    component.device_attached = true;
    display.audio.sample_rate = [0; MAX_PORTS];
    display.audio.component_bound = true;
    hooks.modeset_unlock_all(display.drm_id);
    0
}

/// upstream: intel_audio.c intel_audio_component_unbind()
pub fn intel_audio_component_unbind<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
    hda_device: usize,
    display_device: usize,
    component: &mut AudioComponent,
) {
    hooks.modeset_lock_all(display.drm_id);
    component.ops_installed = false;
    component.device_attached = false;
    display.audio.component_bound = false;
    hooks.modeset_unlock_all(display.drm_id);

    hooks.device_link_remove(hda_device, display_device);
    if display.audio.power_refcount != 0 {
        hooks.drm_error(display.drm_id, "audio power refcount nonzero after unbind");
    }
}

/// upstream: intel_audio.c intel_audio_component_init()
pub fn intel_audio_component_init<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
) {
    if display.display_ver >= 9 {
        let aud_freq_init = hooks.read32(AudioRegister::AudFreqControl);
        let mut aud_freq = if display.display_ver >= 12 {
            AUD_FREQ_GEN12
        } else {
            aud_freq_init
        };
        // Use BIOS value on TGL/RKL unless it is the known broken value.
        if (display.platform.tigerlake || display.platform.rocketlake)
            && aud_freq_init != AUD_FREQ_TGL_BROKEN
        {
            aud_freq = aud_freq_init;
        }
        hooks.drm_debug(display.drm_id, "selected AUD_FREQ_CNTRL from display generation/BIOS");
        display.audio.freq_cntrl = aud_freq;
    }

    // Initialize audio timestamps for the current CDCLK.
    intel_audio_cdclk_change_post(hooks, display);
}

/// upstream: intel_audio.c intel_audio_component_register()
pub fn intel_audio_component_register<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
) {
    let ret = hooks.component_add_typed_audio(display.drm_id);
    if ret < 0 {
        hooks.drm_error(display.drm_id, "failed to add audio component; continuing reduced");
        return;
    }
    display.audio.component_registered = true;
}

/// upstream: intel_audio.c intel_audio_component_cleanup()
pub fn intel_audio_component_cleanup<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
) {
    if !display.audio.component_registered {
        return;
    }
    hooks.component_del_audio(display.drm_id);
    display.audio.component_registered = false;
}

/// upstream: intel_audio.c intel_audio_init()
pub fn intel_audio_init<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
) {
    if hooks.intel_lpe_audio_init(display.drm_id) < 0 {
        display.audio.lpe_platdev = false;
        intel_audio_component_init(hooks, display);
    } else {
        display.audio.lpe_platdev = true;
    }
}

/// upstream: intel_audio.c intel_audio_register()
pub fn intel_audio_register<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
) {
    if !display.audio.lpe_platdev {
        intel_audio_component_register(hooks, display);
    }
}

/// upstream: intel_audio.c intel_audio_deinit()
pub fn intel_audio_deinit<H: IntelAudioDpHooks>(
    hooks: &mut H,
    display: &mut IntelAudioDisplay,
) {
    if display.audio.lpe_platdev {
        hooks.intel_lpe_audio_teardown(display.drm_id);
    } else {
        intel_audio_component_cleanup(hooks, display);
    }
}

fn div_round_up_u64(dividend: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return 0;
    }
    dividend / divisor + u64::from(dividend % divisor != 0)
}
