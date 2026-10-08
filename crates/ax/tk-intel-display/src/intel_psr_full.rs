// Copyright © 2014 Intel Corporation
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

//! Ordered translation of Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_psr.c`.
//! DRM/atomic/property, workqueue, MMIO, AUX/DPCD, firmware and debugfs services
//! remain explicit in [`PsrIo`]; no operation here silently emulates hardware.
#![allow(dead_code, clippy::too_many_arguments, non_snake_case)]

//! # Panel Self Refresh (PSR/SRD)
//!
//! Haswell's display controller introduced hardware Panel Self Refresh for
//! eDP 1.3 panels implementing a remote frame buffer (RFB).  The panel keeps
//! the last image and permits the display engine to stop continuously fetching
//! that image from memory.  Entry and exit are handled by display hardware,
//! including the required AUX messages; the software layer coordinates source
//! configuration with sink capabilities and suppresses re-entry while the
//! frontbuffer is dirty.
//!
//! PSR requires support on both the display source and sink.  Panel Replay
//! reuses the same broad RFB model for DP/eDP.  Panel Replay's selective-update
//! mode and PSR2 selective update are represented separately from PSR1 in the
//! state: `has_psr` alone means PSR1, PSR plus selective-update means PSR2,
//! PSR plus panel-replay means Panel Replay, and Panel Replay plus selective
//! update means Panel Replay Selective Update.
//!
//! Frontbuffer invalidation/flush callbacks bridge hardware tracking gaps.
//! A dirty frontbuffer makes the affected PSR pipe busy and exits PSR or forces
//! a full selective-fetch update.  The corresponding flush clears only the
//! matching pipe bits.  Re-enable is deferred to work context because the PSR
//! lock and atomic/display locks have different ordering constraints; teardown
//! synchronously cancels both immediate and delayed work.
//!
//! ## DC3CO
//!
//! Display generation 12 adds DC3 clock-off on top of PSR2.  The lower
//! DC3CO entry/exit overhead permits a clock-off interval during periodic page
//! flips which repeatedly wake PSR2 from deep sleep.  A flip enables DC3CO and
//! moves the delayed disable work out by six frame periods.  When that work
//! expires without another flip, it restores PSR2 idle-frame programming so
//! hardware can enter deep sleep.  Frontbuffer rendering does not enable DC3CO;
//! it follows the full invalidate/flush protocol instead.
//!
//! ## PSR exit masks
//!
//! On Haswell through Skylake, the EDP_PSR_DEBUG display-register-write mask
//! controls whether almost any display write (including software-firmware
//! writes) exits PSR.  Some registers have dedicated masks.  Generation 12 and
//! later effectively keep the old display-write mask set.
//!
//! On Skylake and later, PIPE_MISC's pipe-register-write mask gates pipe and
//! plane writes.  Primary-surface writes have a distinct mask on Haswell,
//! Broadwell, and Skylake+.  Sprite-enable masks exist on Haswell/Broadwell;
//! universal planes on Skylake and later do not have that mask.  Cursor
//! position is controlled by a dedicated mask on Haswell/Broadwell and is
//! included in the broader pipe-register-write mask on Skylake+.
//!
//! Vblank/vsync interrupt masks block PSR when the interrupt is both unmasked
//! in IMR and enabled in IER.  The vblank-to-pipe bit selects whether exit
//! generates an extra vblank before the first transmitted frame; polarity
//! differs on Haswell/Broadwell versus Skylake+.  With DC states, the extra
//! vblank occurs after link training, while without DC states it occurs
//! immediately after the PSR-exit trigger.  The transition register is double
//! buffered, so its value may not latch until that first vblank.  Standby mode
//! keeps the timing generator running and does not change periodic vblank
//! behavior.
//!
//! The pipe-register unmask bit has different behavior across platforms:
//! Broadwell needs it for post-exit vblank generation, while Haswell shows no
//! apparent effect.  These platform quirks remain in the source paths rather
//! than being folded into generic PSR policy.
//!
//! The remaining mask bits describe individual events (memory-up, hotplug,
//! low-power-signal entry, sprite enable, cursor movement and similar causes).
//! The implementation uses the broad masks only in the cases and generations
//! where the corresponding hardware bits exist.

use alloc::format;

pub const I915_PSR_DEBUG_MODE_MASK: u32 = 0x0f;
pub const I915_PSR_DEBUG_DEFAULT: u32 = 0;
pub const I915_PSR_DEBUG_DISABLE: u32 = 1;
pub const I915_PSR_DEBUG_ENABLE: u32 = 2;
pub const I915_PSR_DEBUG_FORCE_PSR1: u32 = 3;
pub const I915_PSR_DEBUG_ENABLE_SEL_FETCH: u32 = 4;
pub const I915_PSR_DEBUG_IRQ: u32 = 0x10;
pub const I915_PSR_DEBUG_SU_REGION_ET_DISABLE: u32 = 0x20;
pub const I915_PSR_DEBUG_PANEL_REPLAY_DISABLE: u32 = 0x40;
pub const DP_MAX_RESYNC_FRAME_COUNT_MASK: u8 = 0x0f;
pub const PSR_EVENT_PSR2_WD_TIMER_EXPIRE: u32 = 1 << 17;
pub const PSR_EVENT_PSR2_DISABLED: u32 = 1 << 16;
pub const PSR_EVENT_SU_DIRTY_FIFO_UNDERRUN: u32 = 1 << 15;
pub const PSR_EVENT_SU_CRC_FIFO_UNDERRUN: u32 = 1 << 14;
pub const PSR_EVENT_GRAPHICS_RESET: u32 = 1 << 12;
pub const PSR_EVENT_PCH_INTERRUPT: u32 = 1 << 11;
pub const PSR_EVENT_MEMORY_UP: u32 = 1 << 10;
pub const PSR_EVENT_FRONT_BUFFER_MODIFY: u32 = 1 << 9;
pub const PSR_EVENT_WD_TIMER_EXPIRE: u32 = 1 << 8;
pub const PSR_EVENT_PIPE_REGISTERS_UPDATE: u32 = 1 << 6;
pub const PSR_EVENT_REGISTER_UPDATE: u32 = 1 << 5;
pub const PSR_EVENT_HDCP_ENABLE: u32 = 1 << 4;
pub const PSR_EVENT_KVMR_SESSION_ENABLE: u32 = 1 << 3;
pub const PSR_EVENT_VBI_ENABLE: u32 = 1 << 2;
pub const PSR_EVENT_LPSP_MODE_EXIT: u32 = 1 << 1;
pub const PSR_EVENT_PSR_DISABLE: u32 = 1;
pub const PSR_ERROR: u32 = 1 << 2;
pub const PSR_POST_EXIT: u32 = 1 << 1;
pub const PSR_PRE_ENTRY: u32 = 1;
pub const EDP_PSR_ENABLE: u32 = 1 << 31;
pub const EDP_PSR_RESTORE_PSR_ACTIVE_CTX_MASK: u32 = 1 << 29;
pub const EDP_PSR_LINK_STANDBY: u32 = 1 << 27;
pub const EDP_PSR_MAX_SLEEP_TIME_MASK: u32 = 0x01f0_0000;
pub const EDP_PSR_ENTRY_SETUP_FRAMES_MASK: u32 = 0x0003_0000;
pub const EDP_PSR_CRC_ENABLE: u32 = 1 << 10;
pub const EDP_PSR_IDLE_FRAMES_MASK: u32 = 0x0f;
pub const EDP_PSR_TP2_TP3_TIME_MASK: u32 = 3 << 8;
pub const EDP_PSR_TP4_TIME_0US: u32 = 3 << 6;
pub const EDP_PSR_TP1_TIME_MASK: u32 = 3 << 4;
pub const EDP_PSR_TP_SELECT_MASK: u32 = 1 << 11;
pub const DP_PSR_ENABLE: u8 = 1 << 0;
pub const DP_PSR_MAIN_LINK_ACTIVE: u8 = 1 << 1;
pub const DP_PSR_CRC_VERIFICATION: u8 = 1 << 2;
pub const DP_PSR_FRAME_CAPTURE: u8 = 1 << 3;
pub const DP_PSR_SU_REGION_SCANLINE_CAPTURE: u8 = 1 << 4;
pub const DP_PSR_IRQ_HPD_WITH_CRC_ERRORS: u8 = 1 << 5;
pub const DP_PSR_ENABLE_PSR2: u8 = 1 << 6;
pub const DP_PSR_ENABLE_SU_REGION_ET: u8 = 1 << 7;
pub const DP_PANEL_REPLAY_ENABLE: u8 = 1 << 0;
pub const DP_PANEL_REPLAY_VSC_SDP_CRC_EN: u8 = 1 << 1;
pub const DP_PANEL_REPLAY_UNRECOVERABLE_ERROR_EN: u8 = 1 << 3;
pub const DP_PANEL_REPLAY_RFB_STORAGE_ERROR_EN: u8 = 1 << 4;
pub const DP_PANEL_REPLAY_ACTIVE_FRAME_CRC_ERROR_EN: u8 = 1 << 5;
pub const DP_PANEL_REPLAY_SU_ENABLE: u8 = 1 << 6;
pub const DP_PANEL_REPLAY_ENABLE_SU_REGION_ET: u8 = 1 << 7;
pub const DP_PANEL_REPLAY_CRC_VERIFICATION: u8 = 1 << 1;
pub const DP_PANEL_REPLAY_SU_REGION_SCANLINE_CAPTURE: u8 = 1 << 7;
pub const DP_PSR2_SU_GRANULARITY_REQUIRED: u8 = 1 << 5;
pub const DP_PSR2_SU_Y_COORDINATE_REQUIRED: u8 = 1 << 4;
pub const DP_PANEL_REPLAY_SU_GRANULARITY_REQUIRED: u8 = 1 << 5;
pub const DP_PANEL_REPLAY_EARLY_TRANSPORT_SUPPORT: u8 = 1 << 2;
pub const DP_PANEL_REPLAY_ASYNC_VIDEO_TIMING_NOT_SUPPORTED: u8 = 1 << 3;
pub const DP_PANEL_REPLAY_LINK_OFF_SUPPORTED_AFTER_AS_SDP: u8 = 1 << 7;
pub const INTEL_DPCD_WA_PSR2_EARLYSCANLINE_SUPPORT: u32 = 0x3f0;
pub const EDP_PSR2_ENABLE: u32 = 1 << 31;
pub const EDP_PSR2_SU_TRACK_ENABLE: u32 = 1 << 30;
pub const EDP_PSR2_BLOCK_COUNT_MASK: u32 = 1 << 28;
pub const EDP_PSR2_SU_REGION_ET_ENABLE: u32 = 1 << 27;
pub const EDP_PSR2_Y_COORDINATE_ENABLE: u32 = 1 << 25;
pub const EDP_PSR2_IO_BUFFER_WAKE_MASK: u32 = 7 << 13;
pub const EDP_PSR2_IO_BUFFER_WAKE_OLD_MASK: u32 = 3 << 13;
pub const TGL_EDP_PSR2_IO_BUFFER_WAKE_MASK: u32 = 7 << 13;
pub const LNL_EDP_PSR2_IO_BUFFER_WAKE_MASK: u32 = 0x3f << 13;
pub const TGL_EDP_PSR2_FAST_WAKE_MASK: u32 = 7 << 10;
pub const EDP_PSR2_FAST_WAKE_MASK: u32 = 3 << 11;
pub const EDP_PSR2_TP2_TIME_MASK: u32 = 3 << 8;
pub const EDP_PSR2_FRAME_BEFORE_SU_MASK: u32 = 0xf << 4;
pub const EDP_PSR2_IDLE_FRAMES_MASK: u32 = 0xf;
pub const EDP_PSR2_STATUS_STATE_MASK: u32 = 0xf << 28;
pub const EDP_PSR2_STATUS_STATE_DEEP_SLEEP: u32 = 8 << 28;
pub const EDP_PSR_STATUS_STATE_MASK: u32 = 7 << 29;
pub const EDP_PSR2_MAN_TRACK_ENABLE: u32 = 1 << 31;
pub const EDP_PSR2_MAN_TRACK_START_MASK: u32 = 0x3ff << 21;
pub const EDP_PSR2_MAN_TRACK_END_MASK: u32 = 0x3ff << 11;
pub const ADLP_PSR2_MAN_TRACK_PARTIAL: u32 = 1 << 31;
pub const ADLP_PSR2_MAN_TRACK_START_MASK: u32 = 0x1fff << 16;
pub const ADLP_PSR2_MAN_TRACK_END_MASK: u32 = 0x1fff;
pub const PSR2_MAN_TRACK_SINGLE_FULL_FRAME: u32 = 1 << 3;
pub const PSR2_MAN_TRACK_CONTINUOUS_FULL_FRAME: u32 = 1 << 2;
pub const PSR2_MAN_TRACK_PARTIAL_FRAME: u32 = 1 << 1;
pub const ADLP_MAN_TRACK_SINGLE_FULL_FRAME: u32 = 1 << 14;
pub const ADLP_MAN_TRACK_CONTINUOUS_FULL_FRAME: u32 = 1 << 13;
pub const EDPP_PSR_DEBUG_MASK_MAX_SLEEP: u32 = 1 << 28;
pub const EDPP_PSR_DEBUG_MASK_LPSP: u32 = 1 << 27;
pub const EDPP_PSR_DEBUG_MASK_MEMUP: u32 = 1 << 26;
pub const EDPP_PSR_DEBUG_MASK_HPD: u32 = 1 << 25;
pub const EDPP_PSR_DEBUG_MASK_SPRITE_ENABLE: u32 = 1 << 21;
pub const EDPP_PSR_DEBUG_MASK_DISP_REG_WRITE: u32 = 1 << 16;
pub const PSR_AUX_CTL_ALLOWED_MASK: u32 = 0x0d1f_07ff;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Transcoder(pub u8);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsrReg {
    Control,
    HswSrdControl,
    Debug,
    HswSrdDebug,
    PerfCount,
    HswSrdPerfCount,
    Status,
    HswSrdStatus,
    Psr2Control,
    Psr2Status,
    TransDp2Control,
    ExitLine,
    ChickenDcpR1,
    ChickenPar1_1,
    ChickenTrans,
    MtlClockGateTrans,
    ClockGateMisc,
    LnlSffControl,
    PipeSourceSizeEarlyTpt,
    InterruptMask,
    TranscoderInterruptMask,
    InterruptIdentity,
    TranscoderInterruptIdentity,
    SuStatus(u8),
    AuxControl,
    HswSrdAuxControl,
    AuxData(u8),
    HswSrdAuxData(u8),
    Event,
    SourceStatus,
    ManualTrack,
    PipeSourceSize,
    PipeScanline,
    CursorSurfaceLive,
    Dsb,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsrWork {
    Psr,
    Dc3coDisable,
    Dc5Dc6Wa,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsrQueue {
    Unordered,
    DisplayPort,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsrDcState {
    UptoDc6,
    Dc3Co,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}
impl Rect {
    pub fn width(self) -> i32 {
        self.x2 - self.x1
    }
    pub fn height(self) -> i32 {
        self.y2 - self.y1
    }
    pub fn empty(self) -> bool {
        self.x1 >= self.x2 || self.y1 >= self.y2
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timing {
    pub pixel_clock_khz: u32,
    pub refresh_rate_hz: u32,
    pub htotal: u32,
    pub hdisplay: u32,
    pub hblank_start: u32,
    pub hblank_end: u32,
    pub crtc_clock_khz: u32,
    pub lane_count: u8,
    pub port_clock_khz: u32,
    pub vtotal: u32,
    pub vdisplay: u32,
    pub vblank_start: u32,
    pub vblank_end: u32,
    pub frame_time_us: u32,
    pub interlaced: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FirmwarePsrPolicy {
    pub enable_psr: bool,
    pub full_link: bool,
    pub idle_frames: u8,
    pub tp1_wakeup_time_us: u32,
    pub tp23_wakeup_time_us: u32,
    pub psr2_tp23_wakeup_time_us: i32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PsrCaps {
    pub sink_psr1: bool,
    pub sink_psr2: bool,
    pub sink_panel_replay: bool,
    pub source_psr: bool,
    pub source_psr2: bool,
    pub source_panel_replay: bool,
    pub selective_update: bool,
    pub su_granularity: u16,
    pub su_y_granularity: u16,
    pub max_su_area: u32,
    pub alpm: bool,
    pub alpm_aux_less: bool,
    pub dsc: bool,
    pub pr_dsc: bool,
    pub pr_dsc_support: u8,
    pub psr_dpcd_version: u8,
    pub pr_dpcd_support: u8,
    pub pr_dpcd_capability: u8,
    pub panel_replay_dpcd_support: u8,
    pub panel_replay_dpcd_capability: u8,
    pub dsc_max_bpp_x16: u16,
    pub async_video_timing: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PsrConfig {
    pub has_psr: bool,
    pub has_sel_update: bool,
    pub has_panel_replay: bool,
    pub link_off: bool,
    pub link_off_after_as_sdp: bool,
    pub disable_as_sdp: bool,
    pub dc3co: bool,
    pub transcoder: Transcoder,
    pub pipe: u8,
    pub pipe_src: (u16, u16),
    pub mode_width: u16,
    pub mode_height: u16,
    pub pipe_src_early_tpt: u32,
    pub manual_track_control: u32,
    pub su_x_granularity: u16,
    pub su_y_granularity: u16,
    pub idle_frames: u8,
    pub entry_setup_frames: u8,
    pub io_wake_lines: u8,
    pub fast_wake_lines: u8,
    pub aux_less_wake_lines: u8,
    pub exit_line: u16,
    pub frame_time_us: u32,
    pub min_guardband: u32,
    pub su_area: Rect,
    pub enable_psr2_sel_fetch: bool,
    pub enable_psr2_su_region_et: bool,
    pub req_psr2_sdp_prior_scanline: bool,
    pub active_non_psr_pipes: u8,
    pub pkg_c_latency_used: bool,
    pub crc_enabled: bool,
    pub wm_level_disabled: bool,
    pub infoframes_vsc_enabled: bool,
    pub active_planes: u32,
    pub pipe_bpp: u8,
    pub set_context_latency: i32,
    pub source_scanline_indication: bool,
    pub use_trans_push: bool,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PsrState {
    pub debug: u32,
    pub caps: PsrCaps,
    pub config: PsrConfig,
    pub enabled: bool,
    pub active: bool,
    pub panel_replay_enabled: bool,
    pub sel_update_enabled: bool,
    pub paused: bool,
    pub pause_counter: u32,
    pub sink_not_reliable: bool,
    pub irq_aux_error: bool,
    pub dc3co_enabled: bool,
    pub psr2_sel_fetch_enabled: bool,
    pub psr2_sel_fetch_cff_enabled: bool,
    pub su_region_et_enabled: bool,
    pub req_psr2_sdp_prior_scanline: bool,
    pub pkg_c_latency_used: bool,
    pub frontbuffer_bits: u32,
    pub busy_frontbuffer_bits: u32,
    pub transcoder: Transcoder,
    pub last_entry_attempt_ns: u64,
    pub last_exit_ns: u64,
    pub irq_mask: u32,
    pub panel_vbt_psr: bool,
    pub enable_panel_replay: bool,
    pub dmc_loaded: bool,
    pub link_ok: bool,
    pub active_non_psr_pipes: u8,
    pub pipe_active: bool,
    pub port: u8,
    pub generation: u8,
    pub lock_held: bool,
    pub work_pending: bool,
    pub no_psr_reason: Option<&'static str>,
    pub dc3co_exit_delay_ms: u32,
    pub wake_lines: u8,
    pub fast_wake_lines: u8,
    pub setup_frames: u8,
    pub sink_sync_latency: u8,
    pub status: u32,
    pub error_status: u32,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PsrDevice {
    pub display_ver: u8,
    pub transcoder: Transcoder,
    pub is_edp: bool,
    pub is_dp: bool,
    pub is_mst: bool,
    pub connector_id: u32,
    pub connector_is_edp: bool,
    pub crtc_has_edp: bool,
    pub panel_replay_param: bool,
    pub psr: PsrState,
    pub timing: Timing,
    pub hdisplay: u32,
    pub new_timing: Timing,
    pub active_planes: u32,
    pub pipe_count: u8,
    pub update_planes: u32,
    pub has_psr_hardware: bool,
    pub has_dp20: bool,
    pub port_a: bool,
    pub vbt_full_link: bool,
    pub enable_psr: bool,
    pub psr_safest_params: bool,
    pub tp1_wakeup_time_us: u32,
    pub tp23_wakeup_time_us: u32,
    pub psr2_tp23_wakeup_time_us: i32,
    pub source_tps3: bool,
    pub sink_tps3: bool,
    pub wa_22012278275: bool,
    pub haswell: bool,
    pub haswell_ult: bool,
    pub alderlake_p: bool,
    pub alderlake_p_early_step: bool,
    pub old_unsupported_phy: bool,
    pub tigerlake: bool,
    pub splitter_enabled: bool,
    pub wa_16029024088: bool,
    pub wa_16025596647: bool,
    pub wa_14014971492: bool,
    pub output_format_ycbcr420: bool,
    pub uhbr: bool,
    pub jasperlake: bool,
    pub elkhartlake: bool,
    pub disable_psr2_quirk: bool,
    pub force_psr1: bool,
    pub disable_psr2_wa: bool,
    pub has_psr_hw_tracking: bool,
    pub has_psr2_sel_fetch: bool,
    pub supports_trans_push: bool,
    pub stepping_3000_a0_b0: bool,
    pub has_dc3co: bool,
    pub allowed_dc3co: bool,
    pub dsc_enabled: bool,
    pub vrr_enabled: bool,
    pub hdcp_enabled: bool,
    pub hdcp_desired: bool,
    pub hdcp_value_undesired: bool,
    pub mode_changed: bool,
    pub atomic_committed: bool,
    pub underrun_wa: bool,
    pub dc5_dc6_blocked: bool,
    pub firmware_scanout: bool,
}

/// Hardware/framework boundary used by the source-order port.
pub trait PsrIo {
    fn mmio_read(&mut self, reg: PsrReg, transcoder: Transcoder) -> u32;
    fn mmio_write(&mut self, reg: PsrReg, transcoder: Transcoder, value: u32);
    fn mmio_write_fw(&mut self, reg: PsrReg, transcoder: Transcoder, value: u32);
    fn mmio_rmw(&mut self, reg: PsrReg, transcoder: Transcoder, clear: u32, set: u32) -> u32;
    fn aux_read(&mut self, address: u32, data: &mut [u8]) -> i32;
    fn aux_write(&mut self, address: u32, data: &[u8]) -> i32;
    fn wait_vblank(&mut self, pipe: u8, count: u8) -> bool;
    fn delay_us(&mut self, min: u32, max: u32);
    fn now_ns(&mut self) -> u64;
    fn queue_work(&mut self, queue: PsrQueue, work: PsrWork, delay_ms: u32);
    fn init_work(&mut self, work: PsrWork);
    fn init_mutex(&mut self);
    fn cancel_work_sync(&mut self, work: PsrWork);
    fn cancel_work(&mut self, work: PsrWork);
    fn set_target_dc_state(&mut self, state: PsrDcState);
    fn lock_psr(&mut self);
    fn lock_psr_interruptible(&mut self) -> Result<(), i32>;
    fn unlock_psr(&mut self);
    fn assert_psr_lock_held(&mut self);
    fn drm_log(&mut self, level: u8, message: &str, value: u32);
    fn atomic_property_changed(&mut self, connector: u32, property: u32, value: u64);
    fn add_debugfs_file(&mut self, name: &str, connector: u32);
    fn dmc_block(&mut self, transcoder: Transcoder, block: bool);
    fn set_frontbuffer_bits(&mut self, bits: u32, busy: bool);
    fn force_frame_change(&mut self, pipe: u8);
    fn program_dsb(&mut self, reg: PsrReg, transcoder: Transcoder, value: u32);
    fn firmware_psr_policy(&mut self, connector: u32) -> Option<FirmwarePsrPolicy>;
    fn debug_output(&mut self, text: &str);
    fn alpm_compute_params(&mut self, device: &mut PsrDevice) -> bool;
    fn as_sdp_transmission_time(&mut self) -> u8;
    fn alpm_enable_sink(&mut self, device: &PsrDevice, config: &PsrConfig);
    fn alpm_configure_source(&mut self, device: &PsrDevice, config: &PsrConfig);
    fn alpm_disable(&mut self, device: &PsrDevice);
    fn alpm_has_error(&mut self, device: &PsrDevice) -> bool;
    fn update_psr_phy_power_state(&mut self, device: &PsrDevice, enabled: bool);
    fn dmc_block_pkgc(&mut self, pipe: u8, blocked: bool);
    fn dmc_start_pkgc_exit_at_undelayed_vblank(&mut self, pipe: u8, enable: bool);
    fn enable_vrr_psr_frame_change(&mut self, device: &PsrDevice);
    fn add_affected_planes(&mut self, pipe: u8) -> i32;
    fn update_linked_plane_sel_fetch(&mut self, plane_id: u8, area: Rect) -> i32;
    fn dsc_su_et_parameters(&mut self, area_height: u32);
    fn fastset_force(&mut self) -> i32;
    fn vblank_power_reference(&mut self, enable: bool);
    fn runtime_pm_get(&mut self) -> u64;
    fn runtime_pm_put(&mut self, reference: u64);
    fn psr_aux_clock_divider(&mut self, device: &PsrDevice) -> u32;
    fn psr_aux_send_control(&mut self, device: &PsrDevice, message_size: u8, divider: u32) -> u32;
}

const fn mode_debug(debug: u32) -> u32 {
    debug & I915_PSR_DEBUG_MODE_MASK
}
const fn gen12(ver: u8) -> bool {
    ver >= 12
}
const fn gen8(ver: u8) -> bool {
    ver >= 8
}

// upstream: intel_psr.c intel_encoder_can_psr()
pub fn intel_encoder_can_psr(device: &PsrDevice) -> bool {
    (device.is_dp || device.is_mst)
        && (device.psr.caps.sink_psr1 && device.psr.caps.source_psr
            || device.psr.caps.sink_panel_replay && device.psr.caps.source_panel_replay)
}
// upstream: intel_psr.c intel_psr_needs_aux_io_power()
pub fn intel_psr_needs_aux_io_power(device: &PsrDevice, crtc_has_edp: bool) -> bool {
    crtc_has_edp && intel_encoder_can_psr(device)
}
// upstream: intel_psr.c psr_global_enabled()
pub fn psr_global_enabled(device: &PsrDevice) -> bool {
    match mode_debug(device.psr.debug) {
        I915_PSR_DEBUG_DEFAULT => !device.is_edp || device.psr.panel_vbt_psr,
        I915_PSR_DEBUG_DISABLE => false,
        _ => true,
    }
}
// upstream: intel_psr.c sel_update_global_enabled()
pub fn sel_update_global_enabled(device: &PsrDevice) -> bool {
    !matches!(
        mode_debug(device.psr.debug),
        I915_PSR_DEBUG_DISABLE | I915_PSR_DEBUG_FORCE_PSR1
    )
}
// upstream: intel_psr.c panel_replay_global_enabled()
pub fn panel_replay_global_enabled(device: &PsrDevice) -> bool {
    device.psr.debug & I915_PSR_DEBUG_PANEL_REPLAY_DISABLE == 0 && device.panel_replay_param
}
// upstream: intel_psr.c psr_irq_psr_error_bit_get()
pub fn psr_irq_psr_error_bit_get(device: &PsrDevice) -> u32 {
    if gen12(device.display_ver) {
        PSR_ERROR
    } else {
        PSR_ERROR << psr_transcoder_irq_shift(device.transcoder)
    }
}
// upstream: intel_psr.c psr_irq_post_exit_bit_get()
pub fn psr_irq_post_exit_bit_get(device: &PsrDevice) -> u32 {
    if gen12(device.display_ver) {
        PSR_POST_EXIT
    } else {
        PSR_POST_EXIT << psr_transcoder_irq_shift(device.transcoder)
    }
}
// upstream: intel_psr.c psr_irq_pre_entry_bit_get()
pub fn psr_irq_pre_entry_bit_get(device: &PsrDevice) -> u32 {
    if gen12(device.display_ver) {
        PSR_PRE_ENTRY
    } else {
        PSR_PRE_ENTRY << psr_transcoder_irq_shift(device.transcoder)
    }
}
// upstream: intel_psr.c psr_irq_mask_get()
pub fn psr_irq_mask_get(device: &PsrDevice) -> u32 {
    if gen12(device.display_ver) {
        0x7
    } else {
        0x7 << psr_transcoder_irq_shift(device.transcoder)
    }
}

/// Old fixed IMR/IIR pack one PSR triplet per transcoder. EDP is shift zero;
/// the other transcoder fields advance in bytes as in `_EDP_PSR_TRANS_SHIFT`.
fn psr_transcoder_irq_shift(transcoder: Transcoder) -> u32 {
    if transcoder.0 == 4 {
        0
    } else {
        u32::from(transcoder.0.saturating_add(1)) * 8
    }
}
// upstream: intel_psr.c psr_ctl_reg()
pub fn psr_ctl_reg(device: &PsrDevice) -> PsrReg {
    if gen8(device.display_ver) {
        PsrReg::Control
    } else {
        PsrReg::HswSrdControl
    }
}
// upstream: intel_psr.c psr_debug_reg()
pub fn psr_debug_reg(device: &PsrDevice) -> PsrReg {
    if gen8(device.display_ver) {
        PsrReg::Debug
    } else {
        PsrReg::HswSrdDebug
    }
}
// upstream: intel_psr.c psr_perf_cnt_reg()
pub fn psr_perf_cnt_reg(device: &PsrDevice) -> PsrReg {
    if gen8(device.display_ver) {
        PsrReg::PerfCount
    } else {
        PsrReg::HswSrdPerfCount
    }
}
// upstream: intel_psr.c psr_status_reg()
pub fn psr_status_reg(device: &PsrDevice) -> PsrReg {
    if gen8(device.display_ver) {
        PsrReg::Status
    } else {
        PsrReg::HswSrdStatus
    }
}
// upstream: intel_psr.c psr_imr_reg()
pub fn psr_imr_reg(device: &PsrDevice) -> PsrReg {
    if gen12(device.display_ver) {
        PsrReg::TranscoderInterruptMask
    } else {
        PsrReg::InterruptMask
    }
}
// upstream: intel_psr.c psr_iir_reg()
pub fn psr_iir_reg(device: &PsrDevice) -> PsrReg {
    if gen12(device.display_ver) {
        PsrReg::TranscoderInterruptIdentity
    } else {
        PsrReg::InterruptIdentity
    }
}
// upstream: intel_psr.c psr_aux_ctl_reg()
pub fn psr_aux_ctl_reg(device: &PsrDevice) -> PsrReg {
    if gen8(device.display_ver) {
        PsrReg::AuxControl
    } else {
        PsrReg::HswSrdAuxControl
    }
}
// upstream: intel_psr.c psr_aux_data_reg()
pub fn psr_aux_data_reg(device: &PsrDevice, index: u8) -> PsrReg {
    if gen8(device.display_ver) {
        PsrReg::AuxData(index)
    } else {
        PsrReg::HswSrdAuxData(index)
    }
}
// upstream: intel_psr.c psr_irq_control()
pub fn psr_irq_control<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    if device.psr.panel_replay_enabled {
        return;
    }
    let mut mask = psr_irq_psr_error_bit_get(device);
    if device.psr.debug & I915_PSR_DEBUG_IRQ != 0 {
        mask |= psr_irq_post_exit_bit_get(device) | psr_irq_pre_entry_bit_get(device);
    }
    device.psr.irq_mask = mask;
    io.mmio_rmw(
        PsrReg::InterruptMask,
        device.psr.transcoder,
        psr_irq_mask_get(device),
        !mask,
    );
}
// upstream: intel_psr.c psr_event_print()
pub fn psr_event_print<I: PsrIo>(io: &mut I, value: u32, sel_update_enabled: bool) {
    const EVENTS: &[(u32, &str, bool)] = &[
        (
            PSR_EVENT_PSR2_WD_TIMER_EXPIRE,
            "PSR2 watchdog timer expired",
            false,
        ),
        (PSR_EVENT_PSR2_DISABLED, "PSR2 disabled", true),
        (
            PSR_EVENT_SU_DIRTY_FIFO_UNDERRUN,
            "SU dirty FIFO underrun",
            false,
        ),
        (
            PSR_EVENT_SU_CRC_FIFO_UNDERRUN,
            "SU CRC FIFO underrun",
            false,
        ),
        (PSR_EVENT_GRAPHICS_RESET, "Graphics reset", false),
        (PSR_EVENT_PCH_INTERRUPT, "PCH interrupt", false),
        (PSR_EVENT_MEMORY_UP, "Memory up", false),
        (
            PSR_EVENT_FRONT_BUFFER_MODIFY,
            "Front buffer modification",
            false,
        ),
        (
            PSR_EVENT_WD_TIMER_EXPIRE,
            "PSR watchdog timer expired",
            false,
        ),
        (
            PSR_EVENT_PIPE_REGISTERS_UPDATE,
            "PIPE registers updated",
            false,
        ),
        (PSR_EVENT_REGISTER_UPDATE, "Register updated", false),
        (PSR_EVENT_HDCP_ENABLE, "HDCP enabled", false),
        (PSR_EVENT_KVMR_SESSION_ENABLE, "KVMR session enabled", false),
        (PSR_EVENT_VBI_ENABLE, "VBI enabled", false),
        (PSR_EVENT_LPSP_MODE_EXIT, "LPSP mode exited", false),
        (PSR_EVENT_PSR_DISABLE, "PSR disabled", true),
    ];
    io.drm_log(0, "PSR exit events", value);
    for &(bit, text, conditional) in EVENTS {
        if value & bit != 0
            && (!conditional || (bit == PSR_EVENT_PSR2_DISABLED) == sel_update_enabled)
        {
            io.drm_log(0, text, 0);
        }
    }
}
// upstream: intel_psr.c intel_psr_irq_handler()
pub fn intel_psr_irq_handler<I: PsrIo>(device: &mut PsrDevice, io: &mut I, psr_iir: u32) {
    let tr = device.psr.transcoder;
    let now = io.now_ns();
    if psr_iir & psr_irq_pre_entry_bit_get(device) != 0 {
        device.psr.last_entry_attempt_ns = now;
        io.drm_log(0, "PSR entry attempt in 2 vblanks", u32::from(tr.0));
    }
    if psr_iir & psr_irq_post_exit_bit_get(device) != 0 {
        device.psr.last_exit_ns = now;
        io.drm_log(0, "PSR exit completed", u32::from(tr.0));
        if device.display_ver >= 9 {
            let val = io.mmio_rmw(PsrReg::Event, tr, 0, 0);
            psr_event_print(io, val, device.psr.sel_update_enabled);
        }
    }
    if psr_iir & psr_irq_psr_error_bit_get(device) != 0 {
        io.drm_log(2, "PSR aux error", u32::from(tr.0));
        device.psr.irq_aux_error = true;
        io.mmio_rmw(
            PsrReg::InterruptMask,
            tr,
            0,
            psr_irq_psr_error_bit_get(device),
        );
        io.queue_work(PsrQueue::Unordered, PsrWork::Psr, 0);
    }
}
// upstream: intel_psr.c intel_dp_get_sink_sync_latency()
pub fn intel_dp_get_sink_sync_latency<I: PsrIo>(device: &PsrDevice, io: &mut I) -> u8 {
    let mut val = [8u8];
    if io.aux_read(0x2009, &mut val) == 1 {
        val[0] &= DP_MAX_RESYNC_FRAME_COUNT_MASK;
    } else {
        io.drm_log(
            0,
            "Unable to get sink synchronization latency, assuming 8 frames",
            0,
        );
    }
    val[0]
}
// upstream: intel_psr.c _psr_compute_su_granularity()
pub fn _psr_compute_su_granularity<I: PsrIo>(device: &mut PsrDevice, io: &mut I, required: bool) {
    let mut su_x_granularity = [0u8; 2];
    let mut su_y_granularity = 4u8;

    // Panels without a required granularity use the legacy SU values. The
    // source sends full lines, so x granularity has no PSR2 effect here.
    if !required {
        device.psr.caps.su_granularity = 4;
        device.psr.caps.su_y_granularity = 4;
        return;
    }

    // Read the little-endian X value first. A failed or zero read selects the
    // spec's default rather than retaining stale connector data.
    let x_read = io.aux_read(0x0225, &mut su_x_granularity);
    if x_read != su_x_granularity.len() as i32 {
        io.drm_log(
            0,
            "Unable to read selective update x granularity",
            x_read as u32,
        );
    }
    let mut x = if x_read == su_x_granularity.len() as i32 {
        u16::from_le_bytes(su_x_granularity)
    } else {
        0
    };
    if x == 0 {
        x = 4;
    }

    let mut y = [su_y_granularity];
    let y_read = io.aux_read(0x0227, &mut y);
    if y_read != 1 {
        io.drm_log(
            0,
            "Unable to read selective update y granularity",
            y_read as u32,
        );
        y[0] = 4;
    }
    // Unlike X, a successful zero Y field means one line per spec.
    if y[0] == 0 {
        y[0] = 1;
    }

    device.psr.caps.su_granularity = x;
    device.psr.caps.su_y_granularity = u16::from(y[0]);
}

// upstream: intel_psr.c compute_pr_dsc_support()
pub fn compute_pr_dsc_support(capability: u8) -> u8 {
    // DP_PANEL_REPLAY_DSC_DECODE_CAPABILITY_IN_PR_MASK is bits 2:1.
    match (capability >> 1) & 0x3 {
        0 => 2,     // INTEL_DP_PANEL_REPLAY_DSC_SELECTIVE_UPDATE
        1 => 1,     // INTEL_DP_PANEL_REPLAY_DSC_FULL_FRAME_ONLY
        2 | 3 => 0, // not supported or reserved
        _ => unreachable!(),
    }
}

// upstream: intel_psr.c panel_replay_dsc_support_str()
pub fn panel_replay_dsc_support_str(support: u8) -> &'static str {
    match support {
        0 => "not supported",
        1 => "full frame only",
        2 => "selective update",
        _ => "n/a",
    }
}
// upstream: intel_psr.c _panel_replay_compute_su_granularity()
pub fn _panel_replay_compute_su_granularity(device: &mut PsrDevice, caps: &[u8]) {
    let capability = caps.get(1).copied().unwrap_or(0);
    if capability & DP_PANEL_REPLAY_SU_GRANULARITY_REQUIRED == 0 {
        device.psr.caps.su_granularity = 4;
        device.psr.caps.su_y_granularity = 4;
        return;
    }

    let x_bytes = [
        caps.get(2).copied().unwrap_or(0),
        caps.get(3).copied().unwrap_or(0),
    ];
    let x = u16::from_le_bytes(x_bytes);
    let y = caps.get(4).copied().unwrap_or(0);
    device.psr.caps.su_granularity = if x == 0 { 4 } else { x };
    device.psr.caps.su_y_granularity = u16::from(if y == 0 { 1 } else { y });
}

// upstream: intel_psr.c _panel_replay_init_dpcd()
pub fn _panel_replay_init_dpcd<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    connector: u32,
    mst: bool,
    quirk_disable: bool,
) {
    // TODO: Enable Panel Replay on MST only after its multi-stream lifecycle is
    // implemented; do not read or advertise sink support on this path.
    if mst {
        return;
    }

    if device.is_edp && quirk_disable {
        io.drm_log(
            0,
            "Panel Replay support not currently available for this setup",
            connector,
        );
        return;
    }

    let mut panel_replay_dpcd = [0u8; 7];
    let read = io.aux_read(0x00b0, &mut panel_replay_dpcd);
    if read < 0 {
        return;
    }

    let support = panel_replay_dpcd[0];
    if support & 1 == 0 {
        return;
    }

    if device.is_edp && !device.psr.caps.alpm_aux_less {
        io.drm_log(
            0,
            "Panel doesn't support AUX-less ALPM, eDP Panel Replay not possible",
            0,
        );
        return;
    }

    if device.is_edp && support & DP_PANEL_REPLAY_EARLY_TRANSPORT_SUPPORT == 0 {
        io.drm_log(
            0,
            "Panel doesn't support early transport, eDP Panel Replay not possible",
            0,
        );
        return;
    }

    device.psr.caps.sink_panel_replay = true;
    device.psr.caps.pr_dpcd_support = support;
    device.psr.caps.pr_dpcd_capability = panel_replay_dpcd[1];
    if support & 2 != 0 {
        device.psr.caps.selective_update = true;
        _panel_replay_compute_su_granularity(device, &panel_replay_dpcd);
    }

    let dsc_support = compute_pr_dsc_support(panel_replay_dpcd[1]);
    device.psr.caps.pr_dsc_support = dsc_support;
    device.psr.caps.pr_dsc = dsc_support != 0;
    io.drm_log(
        0,
        if device.psr.caps.selective_update {
            "Panel replay selective_update is supported by panel (in DSC mode)"
        } else {
            "Panel replay is supported by panel (in DSC mode)"
        },
        0,
    );
    io.drm_log(0, panel_replay_dsc_support_str(dsc_support), 0);
}

// upstream: intel_psr.c _psr_init_dpcd()
pub fn _psr_init_dpcd<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    _connector: u32,
    dpcd: &mut [u8; 8],
    no_psr_quirk: bool,
    set_power_cap: bool,
    disable_psr2: bool,
) {
    let read = io.aux_read(0x0070, dpcd);
    if read < 0 || dpcd[0] == 0 {
        return;
    }

    io.drm_log(0, "eDP panel supports PSR version", u32::from(dpcd[0]));
    if no_psr_quirk {
        io.drm_log(0, "PSR support not currently available for this panel", 0);
        return;
    }
    if !set_power_cap {
        io.drm_log(
            0,
            "Panel lacks power state control, PSR cannot be enabled",
            0,
        );
        return;
    }

    device.psr.caps.sink_psr1 = true;
    device.psr.caps.psr_dpcd_version = dpcd[0];
    device.psr.sink_sync_latency = intel_dp_get_sink_sync_latency(device, io);
    if disable_psr2 || device.disable_psr2_quirk {
        return;
    }

    if device.display_ver >= 9 && dpcd[0] >= 3 {
        let y_coordinate_required = dpcd[1] & DP_PSR2_SU_Y_COORDINATE_REQUIRED != 0;
        // PSR DPCD 03h advertises Y-coordinate VSC support. i915 only enables
        // selective update if required by the sink and ALPM AUX wake is usable.
        device.psr.caps.selective_update = y_coordinate_required && device.psr.caps.alpm;
        io.drm_log(
            0,
            if device.psr.caps.selective_update {
                "PSR2 supported"
            } else {
                "PSR2 not supported"
            },
            0,
        );
    }

    if device.psr.caps.selective_update {
        let mut intel_wa_register_caps = [0u8; 1];
        let read = io.aux_read(
            INTEL_DPCD_WA_PSR2_EARLYSCANLINE_SUPPORT,
            &mut intel_wa_register_caps,
        );
        if read < 0 {
            return;
        }
        device.psr.status = u32::from(intel_wa_register_caps[0]);
        _psr_compute_su_granularity(device, io, dpcd[1] & DP_PSR2_SU_GRANULARITY_REQUIRED != 0);
    }
}

// upstream: intel_psr.c intel_psr_init_dpcd()
pub fn intel_psr_init_dpcd<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    connector: u32,
    dpcd: &mut [u8; 8],
    no_psr_quirk: bool,
    set_power_cap: bool,
    disable_psr2: bool,
    mst: bool,
    pr_quirk: bool,
) {
    _psr_init_dpcd(
        device,
        io,
        connector,
        dpcd,
        no_psr_quirk,
        set_power_cap,
        disable_psr2,
    );
    _panel_replay_init_dpcd(device, io, connector, mst, pr_quirk);
}
// upstream: intel_psr.c hsw_psr_setup_aux()
pub fn hsw_psr_setup_aux<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    let transcoder = device.psr.transcoder;

    // Native AUX write of DP_SET_POWER = D0. The message header is packed
    // little-endian into the display PSR AUX data registers.
    let aux_message = [
        (0x8u8 << 4) | ((0x0600u32 >> 16) as u8 & 0x0f),
        (0x0600u16 >> 8) as u8,
        0x0600u16 as u8,
        1 - 1,
        1, // DP_SET_POWER_D0
    ];
    for (index, chunk) in aux_message.chunks(4).enumerate() {
        let mut packed = 0u32;
        for (byte_index, byte) in chunk.iter().enumerate() {
            packed |= u32::from(*byte) << (byte_index * 8);
        }
        io.mmio_write(psr_aux_data_reg(device, index as u8), transcoder, packed);
    }

    let divider = io.psr_aux_clock_divider(device);
    let mut aux_control = io.psr_aux_send_control(device, aux_message.len() as u8, divider);

    // PSR AUX_CTL exposes only the timeout, message length, legacy precharge,
    // and 2x bit-clock fields from the regular DDI AUX send control word.
    aux_control &= PSR_AUX_CTL_ALLOWED_MASK;
    io.mmio_write(psr_aux_ctl_reg(device), transcoder, aux_control);
}

// upstream: intel_psr.c psr2_su_region_et_valid()
pub fn psr2_su_region_et_valid(
    device: &PsrDevice,
    panel_replay: bool,
    dpcd_capability: u8,
) -> bool {
    if device.display_ver < 20
        || !device.is_edp
        || device.psr.debug & I915_PSR_DEBUG_SU_REGION_ET_DISABLE != 0
    {
        return false;
    }

    if panel_replay {
        dpcd_capability & DP_PANEL_REPLAY_EARLY_TRANSPORT_SUPPORT != 0
    } else {
        // DP_PSR2_WITH_Y_COORD_ET_SUPPORTED is DPCD revision value 04h.
        dpcd_capability == 4
    }
}

// upstream: intel_psr.c _panel_replay_enable_sink()
pub fn _panel_replay_enable_sink<I: PsrIo>(_device: &PsrDevice, io: &mut I, config: &PsrConfig) {
    let mut panel_replay_config = [
        DP_PANEL_REPLAY_ENABLE
            | DP_PANEL_REPLAY_VSC_SDP_CRC_EN
            | DP_PANEL_REPLAY_UNRECOVERABLE_ERROR_EN
            | DP_PANEL_REPLAY_RFB_STORAGE_ERROR_EN
            | DP_PANEL_REPLAY_ACTIVE_FRAME_CRC_ERROR_EN,
        DP_PANEL_REPLAY_CRC_VERIFICATION,
    ];

    if config.has_sel_update {
        panel_replay_config[0] |= DP_PANEL_REPLAY_SU_ENABLE;
    }
    if config.enable_psr2_su_region_et {
        panel_replay_config[0] |= DP_PANEL_REPLAY_ENABLE_SU_REGION_ET;
    }
    if config.req_psr2_sdp_prior_scanline {
        panel_replay_config[1] |= DP_PANEL_REPLAY_SU_REGION_SCANLINE_CAPTURE;
    }

    // The sink programming order is CONFIG(0x1b0) before CONFIG3(0x11a).
    let _ = io.aux_write(0x01b0, &panel_replay_config);
    let panel_replay_config3 = io.as_sdp_transmission_time();
    let _ = io.aux_write(0x011a, &[panel_replay_config3]);
}

// upstream: intel_psr.c _psr_enable_sink()
pub fn _psr_enable_sink<I: PsrIo>(device: &PsrDevice, io: &mut I, config: &PsrConfig) {
    let mut value = 0u8;
    if config.has_sel_update {
        value |= DP_PSR_ENABLE_PSR2 | DP_PSR_IRQ_HPD_WITH_CRC_ERRORS;
    } else {
        if config.link_off {
            value |= DP_PSR_MAIN_LINK_ACTIVE;
        }
        if device.display_ver >= 8 {
            value |= DP_PSR_CRC_VERIFICATION;
        }
    }

    if config.req_psr2_sdp_prior_scanline {
        value |= DP_PSR_SU_REGION_SCANLINE_CAPTURE;
    }
    if config.enable_psr2_su_region_et {
        value |= DP_PSR_ENABLE_SU_REGION_ET;
    }
    if config.entry_setup_frames > 0 {
        value |= DP_PSR_FRAME_CAPTURE;
    }

    // First write setup/interrupt options with ENABLE clear, then arm the sink.
    let _ = io.aux_write(0x0170, &[value]);
    value |= DP_PSR_ENABLE;
    let _ = io.aux_write(0x0170, &[value]);
}

// upstream: intel_psr.c intel_psr_enable_sink()
pub fn intel_psr_enable_sink<I: PsrIo>(device: &PsrDevice, io: &mut I, config: &PsrConfig) {
    io.alpm_enable_sink(device, config);

    if config.has_panel_replay {
        _panel_replay_enable_sink(device, io, config);
    } else {
        _psr_enable_sink(device, io, config);
    }

    // eDP sink D0 is written after feature programming, before source enable.
    if device.is_edp {
        let _ = io.aux_write(0x0600, &[1]);
    }
}

// upstream: intel_psr.c intel_psr_panel_replay_enable_sink()
pub fn intel_psr_panel_replay_enable_sink<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    if device.psr.caps.sink_panel_replay
        && device.psr.caps.source_panel_replay
        && panel_replay_global_enabled(device)
    {
        io.aux_write(0x01b0, &[1]);
    }
}
// upstream: intel_psr.c intel_psr1_get_tp_time()
pub fn intel_psr1_get_tp_time(
    device: &PsrDevice,
    tp1_wakeup_time_us: u32,
    tp23_wakeup_time_us: u32,
    safest_params: bool,
    source_tps3: bool,
    sink_tps3: bool,
) -> u32 {
    const TP4_TIME_0US: u32 = EDP_PSR_TP4_TIME_0US;
    const TP1_500US: u32 = 0 << 4;
    const TP1_100US: u32 = 1 << 4;
    const TP1_2500US: u32 = 2 << 4;
    const TP1_0US: u32 = 3 << 4;
    const TP23_500US: u32 = 0 << 8;
    const TP23_100US: u32 = 1 << 8;
    const TP23_2500US: u32 = 2 << 8;
    const TP23_0US: u32 = 3 << 8;

    let mut value = 0;
    if device.display_ver >= 11 {
        value |= TP4_TIME_0US;
    }

    if safest_params {
        value |= TP1_2500US | TP23_2500US;
    } else {
        value |= if tp1_wakeup_time_us == 0 {
            TP1_0US
        } else if tp1_wakeup_time_us <= 100 {
            TP1_100US
        } else if tp1_wakeup_time_us <= 500 {
            TP1_500US
        } else {
            TP1_2500US
        };

        value |= if tp23_wakeup_time_us == 0 {
            TP23_0US
        } else if tp23_wakeup_time_us <= 100 {
            TP23_100US
        } else if tp23_wakeup_time_us <= 500 {
            TP23_500US
        } else {
            TP23_2500US
        };

        // Wa 0479: HSW/BDW must not skip both TP1 and TP2/TP3.
        if device.display_ver < 9 && tp1_wakeup_time_us == 0 && tp23_wakeup_time_us == 0 {
            value &= !EDP_PSR_TP2_TP3_TIME_MASK;
            value |= TP23_100US;
        }
    }

    if source_tps3 && sink_tps3 {
        value |= EDP_PSR_TP_SELECT_MASK;
    } else {
        value &= !EDP_PSR_TP_SELECT_MASK;
    }
    value
}

// upstream: intel_psr.c psr_compute_idle_frames()
pub fn psr_compute_idle_frames<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
    vbt_idle_frames: u8,
) -> u8 {
    // Keep six frames as the minimum: this covers known panels and a hardware
    // off-by-one issue. Also allow the sink's synchronization latency plus one.
    let mut idle_frames = 6u8.max(vbt_idle_frames);
    idle_frames = idle_frames.max(device.psr.sink_sync_latency.saturating_add(1));

    if idle_frames > 0x0f {
        io.drm_log(
            1,
            "PSR idle frame count exceeds register field",
            u32::from(idle_frames),
        );
        idle_frames = 0x0f;
    }
    idle_frames
}

// upstream: intel_psr.c is_dc5_dc6_blocked()
pub fn is_dc5_dc6_blocked(device: &PsrDevice, current_dc_state: u8, vblank_enabled: bool) -> bool {
    (current_dc_state != 5 && current_dc_state != 6)
        || device.psr.active_non_psr_pipes != 0
        || vblank_enabled
}

// upstream: intel_psr.c hsw_activate_psr1()
pub fn hsw_activate_psr1<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    let mut value = EDP_PSR_ENABLE;
    value |= u32::from(psr_compute_idle_frames(
        device,
        io,
        device.psr.config.idle_frames,
    ));

    if device.display_ver < 20 {
        value |= (0x1f << 20) & EDP_PSR_MAX_SLEEP_TIME_MASK;
    }
    if device.haswell {
        // Minimum link entry time of eight lines is the zero encoding.
        value |= 0;
    }
    if device.psr.config.link_off {
        value |= EDP_PSR_LINK_STANDBY;
    }
    value |= intel_psr1_get_tp_time(
        device,
        device.tp1_wakeup_time_us,
        device.tp23_wakeup_time_us,
        device.psr_safest_params,
        device.source_tps3,
        device.sink_tps3,
    );
    if device.display_ver >= 8 {
        value |= EDP_PSR_CRC_ENABLE;
    }
    if device.display_ver >= 20 {
        value |= (u32::from(device.psr.config.entry_setup_frames) << 16)
            & EDP_PSR_ENTRY_SETUP_FRAMES_MASK;
    }

    io.mmio_rmw(
        psr_ctl_reg(device),
        device.psr.transcoder,
        !EDP_PSR_RESTORE_PSR_ACTIVE_CTX_MASK,
        value,
    );

    // Wa_16025596647: DC5/DC6 blocking and package-C latency require the exit
    // trigger to be positioned at the start of an undelayed vblank.
    if (device.display_ver == 20 || device.stepping_3000_a0_b0)
        && is_dc5_dc6_blocked(device, 0, false)
        && device.psr.pkg_c_latency_used
    {
        io.dmc_start_pkgc_exit_at_undelayed_vblank(device.psr.config.pipe, true);
    }
}

// upstream: intel_psr.c intel_psr2_get_tp_time()
pub fn intel_psr2_get_tp_time(safest_params: bool, tp23_wakeup_time_us: i32) -> u32 {
    if safest_params {
        return 2 << 8; // EDP_PSR2_TP2_TIME_2500us
    }
    if (0..=50).contains(&tp23_wakeup_time_us) {
        3 << 8 // 50 us
    } else if tp23_wakeup_time_us <= 100 {
        1 << 8 // 100 us
    } else if tp23_wakeup_time_us <= 500 {
        0 << 8 // 500 us
    } else {
        2 << 8 // 2500 us
    }
}

// upstream: intel_psr.c psr2_block_count_lines()
pub fn psr2_block_count_lines(io_wake_lines: u8, fast_wake_lines: u8) -> i32 {
    if io_wake_lines < 9 && fast_wake_lines < 9 {
        8
    } else {
        12
    }
}
// upstream: intel_psr.c psr2_block_count()
pub fn psr2_block_count(device: &PsrDevice) -> i32 {
    psr2_block_count_lines(
        device.psr.config.io_wake_lines,
        device.psr.config.fast_wake_lines,
    ) / 4
}
// upstream: intel_psr.c frames_before_su_entry()
pub fn frames_before_su_entry(device: &PsrDevice) -> u8 {
    let mut frames = device.psr.sink_sync_latency.saturating_add(1).max(2);
    if device.psr.config.entry_setup_frames >= frames {
        frames = device.psr.config.entry_setup_frames + 1
    }
    frames
}
// upstream: intel_psr.c intel_psr_allow_pr_bw_optimization()
pub fn intel_psr_allow_pr_bw_optimization(
    device: &PsrDevice,
    tunnel_bw_alloc: bool,
    tunnel_pr_supported: bool,
) -> bool {
    !device.is_edp && tunnel_bw_alloc && tunnel_pr_supported
}
// upstream: intel_psr.c dg2_activate_panel_replay()
pub fn dg2_activate_panel_replay<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    let transcoder = device.psr.transcoder;
    let mut dp2_ctl_set = 1 << 0; // TRANS_DP2_PANEL_REPLAY_ENABLE
    let mut dp2_ctl_clear = 0;

    if device.is_edp && device.psr.sel_update_enabled {
        let mut psr2_control = 0;
        if device.psr.su_region_et_enabled {
            psr2_control |= EDP_PSR2_SU_REGION_ET_ENABLE;
        }
        if device.psr.req_psr2_sdp_prior_scanline {
            psr2_control |= 1 << 25; // EDP_PSR2_SU_SDP_SCANLINE
        }
        io.mmio_write(PsrReg::Psr2Control, transcoder, psr2_control);
    }

    if intel_psr_allow_pr_bw_optimization(
        device,
        device.psr.config.link_off_after_as_sdp,
        device.psr.caps.pr_dsc,
    ) {
        dp2_ctl_set |= 1 << 1; // TRANS_DP2_PR_TUNNELING_ENABLE
    } else {
        dp2_ctl_clear = 1 << 1;
    }

    let continuous_full_frame =
        man_trk_ctl_continuos_full_frame(device.display_ver, device.alderlake_p);
    io.mmio_rmw(PsrReg::ManualTrack, transcoder, 0, continuous_full_frame);
    io.mmio_rmw(
        PsrReg::TransDp2Control,
        transcoder,
        dp2_ctl_clear,
        dp2_ctl_set,
    );
}

// upstream: intel_psr.c hsw_activate_psr2()
pub fn hsw_activate_psr2<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    let transcoder = device.psr.transcoder;
    let mut value = EDP_PSR2_ENABLE;
    let mut psr_control = 0;

    // Wa_16025596647: when package-C latency has been selected while DC5/DC6
    // is blocked, PSR2 must not wait idle frames before entry.
    let idle_frames = if (device.display_ver == 20 || device.stepping_3000_a0_b0)
        && is_dc5_dc6_blocked(device, 0, false)
        && device.psr.pkg_c_latency_used
    {
        0
    } else {
        psr_compute_idle_frames(device, io, device.psr.config.idle_frames)
    };
    value |= u32::from(idle_frames) & EDP_PSR2_IDLE_FRAMES_MASK;

    if device.display_ver < 14 && !device.alderlake_p {
        value |= EDP_PSR2_SU_TRACK_ENABLE;
    }
    if (10..13).contains(&device.display_ver) {
        value |= EDP_PSR2_Y_COORDINATE_ENABLE;
    }

    value |= (u32::from(frames_before_su_entry(device)) << 4) & EDP_PSR2_FRAME_BEFORE_SU_MASK;
    value |= intel_psr2_get_tp_time(device.psr_safest_params, device.psr2_tp23_wakeup_time_us);

    if (12..20).contains(&device.display_ver) {
        if psr2_block_count(device) > 2 {
            value |= EDP_PSR2_BLOCK_COUNT_MASK;
        }
    }

    // Wa_22012278275: ADL-P remaps encoded IO/FAST wake counts. The firmware
    // defaults are retained, then translated through the documented table.
    if device.wa_22012278275 {
        const WAKE_REMAP: [u8; 8] = [2, 1, 0, 3, 6, 5, 4, 7];
        let io_index = usize::from(device.psr.config.io_wake_lines.saturating_sub(5).min(7));
        let fast_index = usize::from(device.psr.config.fast_wake_lines.saturating_sub(5).min(7));
        let io_lines = u32::from(WAKE_REMAP[io_index] + 5);
        let fast_lines = u32::from(WAKE_REMAP[fast_index] + 5);
        value |= ((io_lines - 5) << 13) & TGL_EDP_PSR2_IO_BUFFER_WAKE_MASK;
        value |= ((fast_lines - 5) << 10) & TGL_EDP_PSR2_FAST_WAKE_MASK;
    } else if device.display_ver >= 20 {
        value |= ((u32::from(device.psr.config.io_wake_lines.saturating_sub(5))) << 13)
            & LNL_EDP_PSR2_IO_BUFFER_WAKE_MASK;
    } else if device.display_ver >= 12 {
        value |= ((u32::from(device.psr.config.io_wake_lines.saturating_sub(5))) << 13)
            & TGL_EDP_PSR2_IO_BUFFER_WAKE_MASK;
        value |= ((u32::from(device.psr.config.fast_wake_lines.saturating_sub(5))) << 10)
            & TGL_EDP_PSR2_FAST_WAKE_MASK;
    } else if device.display_ver >= 9 {
        value |= ((8u32.saturating_sub(u32::from(device.psr.config.io_wake_lines))) << 13)
            & EDP_PSR2_IO_BUFFER_WAKE_OLD_MASK;
        value |= ((8u32.saturating_sub(u32::from(device.psr.config.fast_wake_lines))) << 11)
            & EDP_PSR2_FAST_WAKE_MASK;
    }

    if device.psr.req_psr2_sdp_prior_scanline {
        value |= EDP_PSR2_Y_COORDINATE_ENABLE;
    }
    if device.display_ver >= 20 {
        psr_control |= (u32::from(device.psr.config.entry_setup_frames) << 16)
            & EDP_PSR_ENTRY_SETUP_FRAMES_MASK;
    }

    if device.psr.psr2_sel_fetch_enabled {
        let manual_track = io.mmio_read(PsrReg::ManualTrack, transcoder);
        if manual_track & man_trk_ctl_enable_bit_get(device.display_ver, device.alderlake_p) == 0 {
            io.drm_log(
                2,
                "PSR2 selective fetch manual tracking is not enabled",
                manual_track,
            );
        }
    } else if device.has_psr2_sel_fetch {
        io.mmio_write(PsrReg::ManualTrack, transcoder, 0);
    }

    if device.psr.su_region_et_enabled {
        value |= EDP_PSR2_SU_REGION_ET_ENABLE;
    }

    // Program the shared PSR control first, then the PSR2 control register.
    io.mmio_write(psr_ctl_reg(device), transcoder, psr_control);
    io.mmio_write(PsrReg::Psr2Control, transcoder, value);
}

// upstream: intel_psr.c transcoder_has_psr2()
pub fn transcoder_has_psr2(display_ver: u8, transcoder: Transcoder, alderlake_p: bool) -> bool {
    if alderlake_p || display_ver >= 14 {
        transcoder.0 <= 1
    } else if display_ver >= 12 {
        transcoder.0 == 0
    } else if display_ver >= 9 {
        transcoder.0 == 3
    } else {
        false
    }
}
// upstream: intel_psr.c intel_get_frame_time_us()
pub fn intel_get_frame_time_us(timing: Timing, active: bool) -> u32 {
    if !active {
        return 0;
    }
    if timing.refresh_rate_hz != 0 {
        return 1_000_000u32.div_ceil(timing.refresh_rate_hz);
    }
    timing.frame_time_us
}
// upstream: intel_psr.c psr2_program_idle_frames()
pub fn psr2_program_idle_frames<I: PsrIo>(device: &PsrDevice, io: &mut I, idle_frames: u32) {
    io.mmio_rmw(
        PsrReg::Status,
        device.psr.transcoder,
        0xf0,
        idle_frames << 4,
    );
}
// upstream: intel_psr.c tgl_psr2_enable_dc3co()
pub fn tgl_psr2_enable_dc3co<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    psr2_program_idle_frames(device, io, 0);
    io.set_target_dc_state(PsrDcState::Dc3Co);
    device.psr.dc3co_enabled = true;
    device.psr.config.dc3co = true;
}
// upstream: intel_psr.c tgl_psr2_disable_dc3co()
pub fn tgl_psr2_disable_dc3co<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    io.set_target_dc_state(PsrDcState::UptoDc6);
    let idle_frames = psr_compute_idle_frames(device, io, device.psr.config.idle_frames);
    psr2_program_idle_frames(device, io, u32::from(idle_frames));
    device.psr.dc3co_enabled = false;
    device.psr.config.dc3co = false;
}
// upstream: intel_psr.c tgl_dc3co_disable_work()
pub fn tgl_dc3co_disable_work<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    delayed_work_pending: bool,
) {
    io.lock_psr();
    if !delayed_work_pending {
        tgl_psr2_disable_dc3co(device, io);
    }
    io.unlock_psr();
}
// upstream: intel_psr.c tgl_disallow_dc3co_on_psr2_exit()
pub fn tgl_disallow_dc3co_on_psr2_exit<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    if device.psr.config.exit_line == 0 {
        return;
    }
    io.cancel_work(PsrWork::Dc3coDisable);
    tgl_psr2_disable_dc3co(device, io);
}
// upstream: intel_psr.c dc3co_is_pipe_port_compatible()
pub fn dc3co_is_pipe_port_compatible(
    device: &PsrDevice,
    pipe: u8,
    port: u8,
    alderlake_p: bool,
) -> bool {
    if alderlake_p || device.display_ver >= 14 {
        pipe <= 1 && port <= 1
    } else {
        pipe == 0 && port == 0
    }
}
// upstream: intel_psr.c tgl_dc3co_exitline_compute_config()
pub fn tgl_dc3co_exitline_compute_config(
    device: &mut PsrDevice,
    allowed: bool,
    pipe: u8,
    port: u8,
    alderlake_p: bool,
    wa: bool,
    scanlines_200us: u32,
) {
    device.psr.config.exit_line = 0; /* Source currently returns unconditionally: BSpec 49196 activation order remains disabled. */
    return;
    #[allow(unreachable_code)]
    {
        if !allowed
            || !dc3co_is_pipe_port_compatible(device, pipe, port, alderlake_p)
            || wa
            || device.psr.config.has_sel_update
        {
            return;
        }
        let exit = scanlines_200us + 1;
        if exit < device.timing.vdisplay {
            device.psr.config.exit_line = (device.timing.vdisplay - exit) as u16;
        }
    }
}
// upstream: intel_psr.c intel_psr2_sel_fetch_config_valid()
pub fn intel_psr2_sel_fetch_config_valid(device: &mut PsrDevice, parameter_enabled: bool) -> bool {
    if !parameter_enabled && device.psr.debug != I915_PSR_DEBUG_ENABLE_SEL_FETCH {
        return false;
    }
    device.psr.config.enable_psr2_sel_fetch = true;
    true
}

// upstream: intel_psr.c psr2_granularity_check()
pub fn psr2_granularity_check(
    device: &mut PsrDevice,
    width: u32,
    height: u32,
    sink_w_granularity: u16,
    sink_y_granularity: u16,
    sel_fetch: bool,
    dsc_slice_height: u16,
) -> bool {
    let display_ver = device.display_ver;
    let sink_y = sink_y_granularity;
    let sink_w = if device.psr.config.has_panel_replay && sink_w_granularity == u16::MAX {
        width
    } else {
        u32::from(sink_w_granularity)
    };

    // PSR2 HW sends full lines; width must still be divisible by the sink's
    // advertised horizontal granularity.
    if sink_w == 0 || width % sink_w != 0 {
        return false;
    }
    if sink_y == 0 || height % u32::from(sink_y) != 0 {
        return false;
    }

    // Without selective fetch, hardware tracking can only align to four lines.
    if !sel_fetch {
        return sink_y == 4;
    }

    // ADL-P/MTL and later support the sink's one-line granularity. Older SW
    // tracking rounds small requirements to four and accepts larger multiples.
    let y_granularity = if device.alderlake_p || display_ver >= 14 {
        sink_y
    } else if sink_y <= 2 {
        4
    } else if sink_y % 4 == 0 {
        sink_y
    } else {
        0
    };

    if y_granularity == 0 || height % u32::from(y_granularity) != 0 {
        return false;
    }
    if device.dsc_enabled && dsc_slice_height != 0 && dsc_slice_height % y_granularity != 0 {
        return false;
    }

    device.psr.config.su_y_granularity = y_granularity;
    true
}

// upstream: intel_psr.c apply_scanline_indication_wa()
pub fn apply_scanline_indication_wa(
    device: &mut PsrDevice,
    early_scanline_support: u8,
    edp_dpcd_version: u8,
) -> bool {
    if edp_dpcd_version >= 0x15 {
        return true;
    }

    match early_scanline_support & 0x3 {
        0 => {
            // Sink requires fallback to PSR1.
            device.psr.config.req_psr2_sdp_prior_scanline = false;
            false
        }
        1 => true, // PSR2 with early-scanline SDP supported.
        2 => {
            // PSR2 supported, but without early-scanline SDP.
            device.psr.config.req_psr2_sdp_prior_scanline = false;
            true
        }
        _ => false, // reserved encoding
    }
}

// upstream: intel_psr.c _compute_psr2_sdp_prior_scanline_indication()
pub fn _compute_psr2_sdp_prior_scanline_indication(
    device: &mut PsrDevice,
    hblank_total: u32,
    crtc_clock: u32,
    lane_count: u8,
    port_clock: u32,
    edp_dpcd_version: u8,
    wa_caps: u8,
) -> bool {
    if crtc_clock == 0 || lane_count == 0 || port_clock == 0 {
        return false;
    }
    let hblank_ns = 1_000_000u64 * u64::from(hblank_total) / u64::from(crtc_clock);
    let req_ns =
        u64::from((60 / u32::from(lane_count) + 11) * 1000) / (u64::from(port_clock) / 1000);
    if hblank_ns.saturating_sub(req_ns) > 100 {
        return true;
    }
    if device.display_ver < 14 || edp_dpcd_version < 0x14 {
        return false;
    }
    device.psr.config.req_psr2_sdp_prior_scanline = true;
    device.psr.config.source_scanline_indication = true;
    apply_scanline_indication_wa(device, wa_caps & 0x3, edp_dpcd_version)
}
// upstream: intel_psr.c intel_psr_entry_setup_frames()
pub fn intel_psr_entry_setup_frames(
    device: &mut PsrDevice,
    setup_time_us: i32,
    line_time_ns: u32,
    vblank_lines: u32,
) -> i32 {
    if setup_time_us < 0 {
        device.psr.no_psr_reason = Some("PSR setup time unavailable");
        return -62;
    }
    if line_time_ns == 0 {
        device.psr.no_psr_reason = Some("PSR setup time unavailable");
        return -62;
    }

    let setup_time_ns = u64::from(setup_time_us as u32) * 1_000;
    let setup_scanlines = setup_time_ns.div_ceil(u64::from(line_time_ns)) as u32;
    let available_vblank = vblank_lines.saturating_sub(1);
    if setup_scanlines > available_vblank {
        if device.display_ver >= 20 {
            // Lunar Lake can capture the PSR entry setup over one frame (up to
            // three are supported by hardware; this path chooses the first).
            device.psr.config.entry_setup_frames = 1;
            return 1;
        }
        device.psr.no_psr_reason = Some("PSR setup timing not met");
        return -62;
    }

    device.psr.config.entry_setup_frames = 0;
    0
}

// upstream: intel_psr.c _intel_psr_min_set_context_latency()
pub fn _intel_psr_min_set_context_latency(
    device: &PsrDevice,
    needs_panel_replay: bool,
    needs_sel_update: bool,
    vrr_possible: bool,
) -> i32 {
    if !device.psr.config.has_psr {
        return 0;
    }
    if vrr_possible && (13..=14).contains(&device.display_ver) {
        return 1;
    }
    if device.display_ver < 20 {
        return 0;
    }
    if needs_sel_update {
        return 0;
    }
    if device.display_ver < 30 && device.is_edp {
        return 0;
    }
    if device.display_ver >= 30 && needs_panel_replay {
        return 0;
    }
    1
}
// upstream: intel_psr.c _wake_lines_fit_into_vblank()
pub fn _wake_lines_fit_into_vblank(device: &PsrDevice, mut vblank: i32, wake_lines: i32) -> bool {
    if device.psr.config.source_scanline_indication {
        vblank -= 1
    }
    vblank >= wake_lines
}
// upstream: intel_psr.c wake_lines_fit_into_vblank()
pub fn wake_lines_fit_into_vblank(
    device: &PsrDevice,
    aux_less: bool,
    needs_pr: bool,
    needs_su: bool,
) -> bool {
    let mut vblank = device
        .timing
        .vblank_end
        .saturating_sub(device.timing.vblank_start) as i32;
    vblank -= _intel_psr_min_set_context_latency(device, needs_pr, needs_su, device.vrr_enabled);
    let wake = if aux_less {
        i32::from(device.psr.config.aux_less_wake_lines)
    } else if device.display_ver < 20 {
        psr2_block_count_lines(
            device.psr.config.io_wake_lines,
            device.psr.config.fast_wake_lines,
        )
    } else {
        i32::from(device.psr.config.io_wake_lines)
    };
    _wake_lines_fit_into_vblank(device, vblank, wake)
}
// upstream: intel_psr.c alpm_config_valid()
pub fn alpm_config_valid<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    aux_less: bool,
    needs_panel_replay: bool,
    needs_sel_update: bool,
) -> bool {
    if !io.alpm_compute_params(device) {
        io.drm_log(
            0,
            "PSR2/Panel Replay not enabled, unable to use long enough wake times",
            0,
        );
        return false;
    }

    if !wake_lines_fit_into_vblank(device, aux_less, needs_panel_replay, needs_sel_update) {
        io.drm_log(0, "PSR2/Panel Replay not enabled, too short vblank time", 0);
        return false;
    }

    true
}

// upstream: intel_psr.c intel_psr2_config_valid()
pub fn intel_psr2_config_valid<I: PsrIo>(device: &mut PsrDevice, io: &mut I) -> bool {
    let display_ver = device.display_ver;
    let crtc_hdisplay = u32::from(device.psr.config.mode_width);
    let crtc_vdisplay = u32::from(device.psr.config.mode_height);
    let pipe_bpp = device.psr.config.pipe_bpp;

    if !device.psr.caps.selective_update || device.force_psr1 {
        return false;
    }

    // Jasper Lake and Elkhart Lake PHYs implement only eDP 1.3 PSR.
    if device.jasperlake || device.elkhartlake {
        io.drm_log(0, "PSR2 not supported by phy", 0);
        return false;
    }
    if device.disable_psr2_wa {
        io.drm_log(0, "PSR2 is defeatured for this platform", 0);
        return false;
    }
    if device.alderlake_p && device.alderlake_p_early_step {
        io.drm_log(0, "PSR2 not completely functional in this stepping", 0);
        return false;
    }
    if !transcoder_has_psr2(display_ver, device.transcoder, device.alderlake_p) {
        io.drm_log(
            0,
            "PSR2 not supported by this transcoder",
            u32::from(device.transcoder.0),
        );
        return false;
    }

    // A DSC-required mode wins over PSR2 on platforms which cannot combine
    // DSC and selective update. ADL-P is the documented exception.
    if device.dsc_enabled && display_ver < 14 && !device.alderlake_p {
        io.drm_log(0, "PSR2 cannot be enabled since DSC is enabled", 0);
        return false;
    }

    let (max_hdisplay, max_vdisplay, max_pipe_bpp) = if display_ver >= 20 {
        (crtc_hdisplay, crtc_vdisplay, pipe_bpp)
    } else if (12..=14).contains(&display_ver) {
        (5120, 3200, 30)
    } else if (10..=11).contains(&display_ver) {
        (4096, 2304, 24)
    } else if display_ver == 9 {
        (3640, 2304, 24)
    } else {
        (0, 0, 0)
    };

    if pipe_bpp > max_pipe_bpp {
        io.drm_log(
            0,
            "PSR2 not enabled: pipe bpp exceeds maximum",
            u32::from(pipe_bpp),
        );
        return false;
    }
    if device.vrr_enabled && device.alderlake_p && device.alderlake_p_early_step {
        io.drm_log(
            0,
            "PSR2 not enabled: incompatible hardware stepping and VRR",
            0,
        );
        return false;
    }

    if !alpm_config_valid(device, io, false, false, true) {
        return false;
    }

    if !device.psr.config.enable_psr2_sel_fetch
        && (crtc_hdisplay > max_hdisplay || crtc_vdisplay > max_vdisplay)
    {
        io.drm_log(
            0,
            "PSR2 not enabled: mode exceeds maximum resolution",
            crtc_hdisplay,
        );
        return false;
    }

    // The current upstream function computes no DC3CO exit line: BSpec 49196
    // keeps this feature disabled pending the changed activation sequence.
    let dc3co_allowed = device.has_dc3co && device.allowed_dc3co;
    let pipe = device.psr.config.pipe;
    let port = device.psr.port;
    let alderlake_p = device.alderlake_p;
    tgl_dc3co_exitline_compute_config(device, dc3co_allowed, pipe, port, alderlake_p, false, 0);
    true
}

// upstream: intel_psr.c intel_sel_update_config_valid()
pub fn intel_sel_update_config_valid<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    sdp_indication_valid: bool,
    granularity_valid: bool,
) -> bool {
    // If selective fetch is exposed, either the requested fetch configuration
    // must be valid or this platform must retain hardware tracking as fallback.
    if device.has_psr2_sel_fetch
        && !intel_psr2_sel_fetch_config_valid(device, device.psr.config.enable_psr2_sel_fetch)
        && !device.has_psr_hw_tracking
    {
        io.drm_log(
            0,
            "Selective update not enabled: selective fetch invalid and no HW tracking",
            0,
        );
        goto_unsupported(device);
        return false;
    }

    if !sel_update_global_enabled(device) {
        io.drm_log(0, "Selective update disabled by flag", 0);
        goto_unsupported(device);
        return false;
    }

    if !device.psr.config.has_panel_replay && !intel_psr2_config_valid(device, io) {
        goto_unsupported(device);
        return false;
    }

    if !sdp_indication_valid {
        io.drm_log(
            0,
            "Selective update SDP indication does not fit in hblank",
            0,
        );
        goto_unsupported(device);
        return false;
    }

    if device.psr.config.has_panel_replay {
        if device.display_ver < 14 {
            goto_unsupported(device);
            return false;
        }
        if !device.psr.caps.selective_update {
            goto_unsupported(device);
            return false;
        }
        if device.dsc_enabled && device.psr.caps.pr_dsc_support != 2 {
            io.drm_log(
                0,
                "Selective update with Panel Replay not supported with DSC",
                0,
            );
            goto_unsupported(device);
            return false;
        }
    }

    if device.psr.config.crc_enabled {
        io.drm_log(
            0,
            "Selective update not enabled because it inhibits pipe CRC calculation",
            0,
        );
        goto_unsupported(device);
        return false;
    }

    if !granularity_valid {
        io.drm_log(
            0,
            "Selective update not enabled: SU granularity incompatible",
            0,
        );
        goto_unsupported(device);
        return false;
    }

    device.psr.config.enable_psr2_su_region_et = psr2_su_region_et_valid(
        device,
        device.psr.config.has_panel_replay,
        if device.psr.config.has_panel_replay {
            device.psr.caps.pr_dpcd_support
        } else {
            device.psr.caps.psr_dpcd_version
        },
    );
    true
}

fn goto_unsupported(device: &mut PsrDevice) {
    device.psr.config.enable_psr2_sel_fetch = false;
    device.psr.config.has_sel_update = false;
}

// upstream: intel_psr.c _psr_compute_config()
pub fn _psr_compute_config(
    device: &mut PsrDevice,
    enable_psr: bool,
    entry_setup_us: i32,
    line_time_ns: u32,
    vblank_lines: u32,
) -> bool {
    if !(device.psr.caps.sink_psr1 && device.psr.caps.source_psr) || !enable_psr {
        return false;
    }

    // VRR with PSR is unreliable on the supported source generations.
    if device.vrr_enabled {
        return false;
    }

    let entry_setup_frames =
        intel_psr_entry_setup_frames(device, entry_setup_us, line_time_ns, vblank_lines);
    if entry_setup_frames >= 0 {
        device.psr.config.entry_setup_frames = entry_setup_frames as u8;
        return true;
    }

    device.psr.no_psr_reason = Some("PSR setup timing not met");
    false
}

// upstream: intel_psr.c compute_link_off_after_as_sdp_when_pr_active()
pub fn compute_link_off_after_as_sdp_when_pr_active(capability: u8) -> bool {
    capability & DP_PANEL_REPLAY_LINK_OFF_SUPPORTED_AFTER_AS_SDP != 0
}

// upstream: intel_psr.c compute_disable_as_sdp_when_pr_active()
pub fn compute_disable_as_sdp_when_pr_active(capability: u8) -> bool {
    capability & DP_PANEL_REPLAY_ASYNC_VIDEO_TIMING_NOT_SUPPORTED == 0
}

// upstream: intel_psr.c _panel_replay_compute_config()
pub fn _panel_replay_compute_config<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    enabled: bool,
    crc_enabled: bool,
    pipe: u8,
    uhbr: bool,
    hdcp_desired: bool,
    hdcp_value_undesired: bool,
) -> bool {
    if !intel_encoder_can_psr(device) && !device.psr.caps.sink_panel_replay {
        return false;
    }
    if !device.psr.caps.sink_panel_replay {
        return false;
    }
    if !panel_replay_global_enabled(device) {
        io.drm_log(0, "Panel Replay disabled by flag", 0);
        return false;
    }
    if crc_enabled {
        io.drm_log(
            0,
            "Panel Replay not enabled because it inhibits pipe CRC",
            0,
        );
        return false;
    }
    if device.dsc_enabled && device.psr.caps.pr_dsc_support == 0 {
        io.drm_log(
            0,
            "Panel Replay not enabled with DSC: sink decoder unsupported",
            0,
        );
        return false;
    }

    // The capability's Adaptive-Sync bits determine link-off and AS-SDP
    // behavior while Panel Replay is active.
    device.psr.config.link_off_after_as_sdp =
        compute_link_off_after_as_sdp_when_pr_active(device.psr.caps.pr_dpcd_capability);
    device.psr.config.disable_as_sdp =
        compute_disable_as_sdp_when_pr_active(device.psr.caps.pr_dpcd_capability);

    // External DP Panel Replay requires no eDP-only ALPM and can proceed now.
    if !device.is_edp {
        return true;
    }

    // Remaining restrictions apply to embedded DisplayPort.
    if pipe != 0 && pipe != 1 {
        return false;
    }
    if uhbr {
        io.drm_log(0, "Panel Replay is not supported with 128b/132b", 0);
        return false;
    }
    if hdcp_desired || (device.hdcp_enabled && hdcp_value_undesired) {
        io.drm_log(0, "Panel Replay is not supported with HDCP", 0);
        return false;
    }
    if !alpm_config_valid(device, io, true, true, false) {
        return false;
    }
    let _ = enabled;
    true
}

// upstream: intel_psr.c intel_psr_needs_wa_18037818876()
pub fn intel_psr_needs_wa_18037818876(device: &PsrDevice) -> bool {
    device.display_ver == 20
        && device.psr.config.entry_setup_frames > 0
        && !device.psr.config.has_sel_update
}
// upstream: intel_psr.c intel_psr_set_non_psr_pipes()
pub fn intel_psr_set_non_psr_pipes(
    device: &mut PsrDevice,
    wa_enabled: bool,
    active_pipes: u8,
    pipe: u8,
) {
    if !wa_enabled || device.psr.config.has_panel_replay {
        return;
    }
    device.psr.config.pipe = pipe;
    device.pipe_count = (active_pipes & !(1 << pipe)).count_ones() as u8;
}
// upstream: intel_psr.c intel_psr_compute_config()
pub fn intel_psr_compute_config<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    enable_psr: bool,
    setup_us: i32,
    line_time_ns: u32,
    vblank_lines: u32,
    sdp_indication_valid: bool,
    granularity_valid: bool,
) {
    if !psr_global_enabled(device) {
        io.drm_log(0, "PSR disabled by flag", 0);
        return;
    }
    if device.psr.sink_not_reliable {
        io.drm_log(0, "PSR sink implementation is not reliable", 0);
        return;
    }
    if device.timing.interlaced {
        io.drm_log(0, "PSR condition failed: Interlaced mode enabled", 0);
        return;
    }

    // PSR/Panel Replay are transcoder-level features; joiner pipes remain
    // excluded until their source/sink routing can be tracked correctly.
    if device.pipe_count > 1 {
        io.drm_log(
            0,
            "PSR disabled due to joiner",
            u32::from(device.pipe_count),
        );
        return;
    }

    // Preserve sink DSC support for state verification before evaluating PR.
    let pr_enabled = panel_replay_global_enabled(device);
    let crc_enabled = device.psr.config.crc_enabled;
    let pipe = device.psr.config.pipe;
    let uhbr = device.uhbr;
    let hdcp_desired = device.hdcp_desired;
    let hdcp_value_undesired = device.hdcp_value_undesired;
    device.psr.config.has_panel_replay = _panel_replay_compute_config(
        device,
        io,
        pr_enabled,
        crc_enabled,
        pipe,
        uhbr,
        hdcp_desired,
        hdcp_value_undesired,
    );

    device.psr.config.has_psr = if device.psr.config.has_panel_replay {
        true
    } else {
        _psr_compute_config(device, enable_psr, setup_us, line_time_ns, vblank_lines)
    };
    if !device.psr.config.has_psr {
        return;
    }

    device.psr.config.has_sel_update =
        intel_sel_update_config_valid(device, io, sdp_indication_valid, granularity_valid);
}

// upstream: intel_psr.c intel_psr_get_config()
pub fn intel_psr_get_config<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    has_dp_digital_port: bool,
) {
    if !has_dp_digital_port {
        return;
    }
    if !(intel_encoder_can_psr(device) || device.psr.caps.sink_panel_replay) {
        return;
    }

    io.lock_psr();
    if !device.psr.enabled {
        io.unlock_psr();
        return;
    }

    if device.psr.panel_replay_enabled {
        device.psr.config.has_psr = true;
        device.psr.config.has_panel_replay = true;
    } else {
        // PSR/PSR2_CTL is toggled by frontbuffer tracking and cannot be used to
        // reconstruct the atomic feature state; the software state is canonical.
        device.psr.config.has_psr = true;
    }

    device.psr.config.has_sel_update = device.psr.sel_update_enabled;
    device.psr.config.infoframes_vsc_enabled = true;
    if !device.psr.sel_update_enabled {
        io.unlock_psr();
        return;
    }

    if device.has_psr2_sel_fetch {
        let manual_track = io.mmio_read(PsrReg::ManualTrack, device.psr.transcoder);
        if manual_track & man_trk_ctl_enable_bit_get(device.display_ver, device.alderlake_p) != 0 {
            device.psr.config.enable_psr2_sel_fetch = true;
        }
    }

    device.psr.config.enable_psr2_su_region_et = device.psr.su_region_et_enabled;
    if device.display_ver >= 12 {
        let exit_line = io.mmio_read(PsrReg::ExitLine, device.psr.transcoder);
        device.psr.config.exit_line = (exit_line & 0xffff) as u16;
    }

    io.unlock_psr();
}

// upstream: intel_psr.c intel_psr_activate()
pub fn intel_psr_activate<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    let transcoder = device.psr.transcoder;
    io.assert_psr_lock_held();

    if transcoder_has_psr2(device.display_ver, transcoder, device.alderlake_p)
        && io.mmio_read(PsrReg::Psr2Control, transcoder) & EDP_PSR2_ENABLE != 0
    {
        io.drm_log(2, "PSR2 control already enabled before activation", 0);
    }
    if io.mmio_read(psr_ctl_reg(device), transcoder) & EDP_PSR_ENABLE != 0 {
        io.drm_log(2, "PSR1 control already enabled before activation", 0);
    }
    if device.psr.active {
        io.drm_log(2, "PSR active state already set before activation", 0);
    }
    if !device.psr.enabled {
        io.drm_log(2, "PSR must be enabled before activation", 0);
        return;
    }

    // PSR1, PSR2 and Panel Replay are mutually exclusive.
    if device.psr.panel_replay_enabled {
        dg2_activate_panel_replay(device, io);
    } else if device.psr.sel_update_enabled {
        hsw_activate_psr2(device, io);
    } else {
        hsw_activate_psr1(device, io);
    }

    device.psr.active = true;
    device.psr.no_psr_reason = None;
}

// upstream: intel_psr.c wm_optimization_wa()
pub fn wm_optimization_wa<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
    wm_level_disabled: bool,
    vblank_start: u32,
    vdisplay: u32,
) {
    let mut activate = false;

    // Wa_14015648006: apply on display versions 11 through 14 when the
    // watermark level has been disabled.
    if (11..=14).contains(&device.display_ver) && wm_level_disabled {
        activate = true;
    }

    // Wa_16013835468: display 12 requires latency reporting removed when the
    // programmed vblank starts after the active display.
    if device.display_ver == 12 && vblank_start != vdisplay {
        activate = true;
    }

    let latency_reporting_removed = 1u32 << device.psr.config.pipe;
    if activate {
        io.mmio_rmw(
            PsrReg::ChickenDcpR1,
            device.psr.transcoder,
            0,
            latency_reporting_removed,
        );
    } else {
        io.mmio_rmw(
            PsrReg::ChickenDcpR1,
            device.psr.transcoder,
            latency_reporting_removed,
            0,
        );
    }
}

// upstream: intel_psr.c intel_psr_enable_source()
pub fn intel_psr_enable_source<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    let transcoder = device.psr.transcoder;

    // Only HSW/BDW have PSR AUX programming registers. SKL and later use
    // hardcoded AUX transaction values.
    if device.display_ver < 9 {
        hsw_psr_setup_aux(device, io);
    }

    let mut mask = 0;
    // Mask HPD on pre-Lunar Lake sources and for eDP Panel Replay. Newer DP
    // Panel Replay has no applicable bits; eDP PR retains the full mask set.
    if device.display_ver < 20 || device.is_edp {
        mask |= EDPP_PSR_DEBUG_MASK_HPD;
    }

    if device.is_edp {
        mask |= EDPP_PSR_DEBUG_MASK_MEMUP;

        // Leave LPSP unmasked on affected HSW non-ULT systems to avoid
        // external-display flicker/runtime-PM coupling; mask on other systems.
        if device.display_ver >= 8 || device.haswell_ult {
            mask |= EDPP_PSR_DEBUG_MASK_LPSP;
        }
        if device.display_ver < 20 {
            mask |= EDPP_PSR_DEBUG_MASK_MAX_SLEEP;
        }

        // HSW/BDW have no independent pipe-register-write mask. Preserve the
        // CURSURFLIVE tracking sequence on display versions 9 and 10.
        if (9..=10).contains(&device.display_ver) {
            mask |= EDPP_PSR_DEBUG_MASK_DISP_REG_WRITE;
        }
        if device.haswell {
            mask |= EDPP_PSR_DEBUG_MASK_SPRITE_ENABLE;
        }
    }

    io.mmio_write(psr_debug_reg(device), transcoder, mask);
    psr_irq_control(device, io);

    // Program DC3CO exit line before the remaining source workarounds.
    if device.psr.config.exit_line != 0 {
        io.mmio_rmw(
            PsrReg::ExitLine,
            transcoder,
            0x0000_ffff,
            (u32::from(device.psr.config.exit_line) << 8) | (1 << 31),
        );
    }

    if device.has_psr_hw_tracking && device.has_psr2_sel_fetch {
        const IGNORE_PSR2_HW_TRACKING: u32 = 1 << 0;
        io.mmio_rmw(
            PsrReg::ChickenPar1_1,
            transcoder,
            IGNORE_PSR2_HW_TRACKING,
            if device.psr.psr2_sel_fetch_enabled {
                IGNORE_PSR2_HW_TRACKING
            } else {
                0
            },
        );
    }

    // Wa_16013835468 / Wa_14015648006 update the latency-reporting bit.
    wm_optimization_wa(
        device,
        io,
        device.psr.config.wm_level_disabled,
        device.timing.vblank_start,
        device.timing.vdisplay,
    );

    if device.psr.sel_update_enabled {
        if device.display_ver == 9 {
            const PSR2_VSC_ENABLE_PROG_HEADER: u32 = 1 << 0;
            const PSR2_ADD_VERTICAL_LINE_COUNT: u32 = 1 << 1;
            io.mmio_rmw(
                PsrReg::ChickenTrans,
                transcoder,
                0,
                PSR2_VSC_ENABLE_PROG_HEADER | PSR2_ADD_VERTICAL_LINE_COUNT,
            );
        }

        // Wa_16014451276:adlp,mtl[a0,b0], 1-based X granularity.
        if !device.psr.panel_replay_enabled && (device.alderlake_p_early_step || device.alderlake_p)
        {
            io.mmio_rmw(PsrReg::ChickenTrans, transcoder, 0, 1 << 0);
        }

        // Wa_16012604467:adlp,mtl[a0,b0], DMASC clock-gating workaround.
        if !device.psr.panel_replay_enabled && device.alderlake_p_early_step {
            io.mmio_rmw(PsrReg::MtlClockGateTrans, transcoder, 0, 1 << 0);
        } else if device.alderlake_p {
            io.mmio_rmw(PsrReg::ClockGateMisc, transcoder, 0, 1 << 0);
        }
    }

    // Wa_16025596647: package-C is blocked while PSR is active on these
    // versions/steppings, except for Panel Replay.
    if (device.display_ver == 20 || device.stepping_3000_a0_b0) && !device.psr.panel_replay_enabled
    {
        io.dmc_block_pkgc(device.psr.config.pipe, true);
    }

    io.alpm_configure_source(device, &device.psr.config);

    if device.supports_trans_push && device.psr.config.has_psr {
        io.enable_vrr_psr_frame_change(device);
    }
}

// upstream: intel_psr.c psr_interrupt_error_check()
pub fn psr_interrupt_error_check<I: PsrIo>(device: &mut PsrDevice, io: &mut I) -> bool {
    if device.psr.panel_replay_enabled {
        return true;
    }

    // A PSR error IIR may remain set across a driver reload even after generic
    // IRQ preinstall/uninstall clears. Enabling PSR with it set can freeze the
    // first frame, so make the sink permanently unreliable for this session.
    let mut error = io.mmio_read(psr_iir_reg(device), device.psr.transcoder);
    error &= psr_irq_psr_error_bit_get(device);
    if error != 0 {
        device.psr.sink_not_reliable = true;
        io.drm_log(0, "PSR interruption error set, not enabling PSR", error);
        return false;
    }

    true
}

// upstream: intel_psr.c intel_psr_enable_locked()
pub fn intel_psr_enable_locked<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    config: PsrConfig,
) -> bool {
    if device.psr.enabled {
        io.drm_log(2, "PSR enable requested while already enabled", 0);
        return false;
    }

    // Copy all state needed by IRQ, workqueue and frontbuffer paths before any
    // sink/source enable operation. The mutex remains held through activation.
    device.psr.config = config;
    device.psr.sel_update_enabled = config.has_sel_update;
    device.psr.panel_replay_enabled = config.has_panel_replay;
    device.psr.busy_frontbuffer_bits = 0;
    device.psr.frontbuffer_bits = 0;
    device.psr.config.pipe = config.pipe;
    device.psr.transcoder = config.transcoder;

    let frame_time_us = intel_get_frame_time_us(device.timing, device.psr.pipe_active);
    device.psr.dc3co_exit_delay_ms = frame_time_us.saturating_mul(6) / 1000;
    device.psr.config.exit_line = config.exit_line;
    device.psr.psr2_sel_fetch_enabled = config.enable_psr2_sel_fetch;
    device.psr.su_region_et_enabled = config.enable_psr2_su_region_et;
    device.psr.psr2_sel_fetch_cff_enabled = false;
    device.psr.req_psr2_sdp_prior_scanline = config.req_psr2_sdp_prior_scanline;
    device.psr.active_non_psr_pipes = config.active_non_psr_pipes;
    device.psr.pkg_c_latency_used = config.pkg_c_latency_used;
    device.psr.wake_lines = config.io_wake_lines;
    device.psr.fast_wake_lines = config.fast_wake_lines;
    device.psr.setup_frames = config.entry_setup_frames;

    if !psr_interrupt_error_check(device, io) {
        return false;
    }

    io.drm_log(
        0,
        if device.psr.panel_replay_enabled {
            "Enabling Panel Replay"
        } else if device.psr.sel_update_enabled {
            "Enabling PSR2"
        } else {
            "Enabling PSR1"
        },
        0,
    );

    // Sink configuration first (ALPM plus PR/PSR options), then eDP PHY power,
    // then source MMIO/workarounds. The sink's PR enable bit was already
    // written by the dedicated Panel Replay setup path where applicable.
    intel_psr_enable_sink(device, io, &config);
    if device.is_edp {
        io.update_psr_phy_power_state(device, true);
    }
    intel_psr_enable_source(device, io);

    device.psr.enabled = true;
    device.psr.pause_counter = 0;
    device.psr.paused = false;

    // `link_ok` stays sticky until the first short-pulse interrupt; panels can
    // report a transient bad link immediately after PSR begins.
    device.psr.link_ok = true;

    intel_psr_activate(device, io);
    true
}

// upstream: intel_psr.c intel_psr_exit()
pub fn intel_psr_exit<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    let display_ver = device.display_ver;
    let transcoder = device.psr.transcoder;

    if !device.psr.active {
        if transcoder_has_psr2(display_ver, transcoder, device.alderlake_p) {
            let value = io.mmio_read(PsrReg::Psr2Control, transcoder);
            if value & EDP_PSR2_ENABLE != 0 {
                io.drm_log(2, "PSR2 enable bit set while software inactive", value);
            }
        }
        let value = io.mmio_read(psr_ctl_reg(device), transcoder);
        if value & EDP_PSR_ENABLE != 0 {
            io.drm_log(2, "PSR enable bit set while software inactive", value);
        }
        return;
    }

    if device.psr.panel_replay_enabled {
        const TRANS_DP2_PANEL_REPLAY_ENABLE: u32 = 1 << 0;
        io.mmio_rmw(
            PsrReg::TransDp2Control,
            transcoder,
            TRANS_DP2_PANEL_REPLAY_ENABLE,
            0,
        );
    } else if device.psr.sel_update_enabled {
        tgl_disallow_dc3co_on_psr2_exit(device, io);
        let previous = io.mmio_rmw(PsrReg::Psr2Control, transcoder, EDP_PSR2_ENABLE, 0);
        if previous & EDP_PSR2_ENABLE == 0 {
            io.drm_log(
                2,
                "PSR2 enable bit was unexpectedly clear on exit",
                previous,
            );
        }
    } else {
        if (display_ver == 20 || device.stepping_3000_a0_b0) && device.psr.pkg_c_latency_used {
            io.dmc_start_pkgc_exit_at_undelayed_vblank(device.psr.config.pipe, false);
        }
        let previous = io.mmio_rmw(psr_ctl_reg(device), transcoder, EDP_PSR_ENABLE, 0);
        if previous & EDP_PSR_ENABLE == 0 {
            io.drm_log(2, "PSR enable bit was unexpectedly clear on exit", previous);
        }
    }

    device.psr.active = false;
}

// upstream: intel_psr.c intel_psr_wait_exit_locked()
pub fn intel_psr_wait_exit_locked<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
    timeout_ms: u32,
) -> bool {
    let (register, state_mask) =
        if device.is_edp && (device.psr.sel_update_enabled || device.psr.panel_replay_enabled) {
            (PsrReg::Psr2Status, EDP_PSR2_STATUS_STATE_MASK)
        } else {
            (psr_status_reg(device), EDP_PSR_STATUS_STATE_MASK)
        };

    // Wait for state machine IDLE. Keep the timeout in milliseconds like
    // intel_de_wait_for_clear_ms and retain 100 us polling granularity.
    let polls = timeout_ms.saturating_mul(10);
    for _ in 0..polls {
        if io.mmio_read(register, device.psr.transcoder) & state_mask == 0 {
            return true;
        }
        io.delay_us(100, 100);
    }

    io.drm_log(2, "Timed out waiting PSR idle state", timeout_ms);
    false
}

// upstream: intel_psr.c intel_psr_disable_locked()
pub fn intel_psr_disable_locked<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    io.assert_psr_lock_held();
    if !device.psr.enabled {
        return;
    }

    io.drm_log(
        0,
        if device.psr.panel_replay_enabled {
            "Disabling Panel Replay"
        } else if device.psr.sel_update_enabled {
            "Disabling PSR2"
        } else {
            "Disabling PSR1"
        },
        0,
    );

    intel_psr_exit(device, io);
    let _idle = intel_psr_wait_exit_locked(device, io, 2_000);

    // Clear Wa_16013835468 / Wa_14015648006 after the source exits PSR.
    if device.display_ver >= 11 {
        let latency_reporting_removed = 1u32 << device.psr.config.pipe;
        io.mmio_rmw(
            PsrReg::ChickenDcpR1,
            device.psr.transcoder,
            latency_reporting_removed,
            0,
        );
    }

    if device.psr.sel_update_enabled {
        // Wa_16012604467:adlp,mtl[a0,b0]. Restore the DMASC clock gating bit
        // that was disabled for selective update at source enable.
        if !device.psr.panel_replay_enabled && device.alderlake_p_early_step {
            io.mmio_rmw(PsrReg::MtlClockGateTrans, device.psr.transcoder, 1, 0);
        } else if device.alderlake_p {
            io.mmio_rmw(PsrReg::ClockGateMisc, device.psr.transcoder, 1, 0);
        }
    }

    if device.is_edp {
        io.update_psr_phy_power_state(device, false);
    }
    if device.psr.panel_replay_enabled && device.is_edp {
        io.alpm_disable(device);
    }

    // PSR's sink bit is disabled after source exit; Panel Replay's configured
    // sink state is managed by its dedicated enable/disable path.
    if !device.psr.panel_replay_enabled {
        let _ = io.aux_write(0x0170, &[0]);
        if device.psr.sel_update_enabled {
            let _ = io.aux_write(0x0116, &[0]);
        }
    }

    if (device.display_ver == 20 || device.stepping_3000_a0_b0) && !device.psr.panel_replay_enabled
    {
        io.dmc_block_pkgc(device.psr.config.pipe, false);
    }

    device.psr.enabled = false;
    device.psr.active = false;
    device.psr.panel_replay_enabled = false;
    device.psr.sel_update_enabled = false;
    device.psr.psr2_sel_fetch_enabled = false;
    device.psr.su_region_et_enabled = false;
    device.psr.psr2_sel_fetch_cff_enabled = false;
    device.psr.active_non_psr_pipes = 0;
    device.psr.pkg_c_latency_used = false;
    device.psr.config.has_psr = false;
    device.psr.config.has_sel_update = false;
    device.psr.config.has_panel_replay = false;
}

// upstream: intel_psr.c intel_psr_disable()
pub fn intel_psr_disable<I: PsrIo>(device: &mut PsrDevice, io: &mut I, old_state_has_psr: bool) {
    if !old_state_has_psr || !intel_encoder_can_psr(device) && !device.psr.caps.sink_panel_replay {
        return;
    }
    io.lock_psr();
    intel_psr_disable_locked(device, io);
    device.psr.link_ok = false;
    io.unlock_psr();
    io.cancel_work_sync(PsrWork::Psr);
    io.cancel_work_sync(PsrWork::Dc3coDisable);
}
// upstream: intel_psr.c intel_psr_pause()
pub fn intel_psr_pause<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    if !intel_encoder_can_psr(device) && !device.psr.caps.sink_panel_replay {
        return;
    }

    io.lock_psr();
    if !device.psr.enabled {
        io.unlock_psr();
        return;
    }

    let first_pause = device.psr.pause_counter == 0;
    device.psr.pause_counter = device.psr.pause_counter.saturating_add(1);
    device.psr.paused = true;
    if first_pause {
        intel_psr_exit(device, io);
        let _idle = intel_psr_wait_exit_locked(device, io, 2_000);
    }
    io.unlock_psr();

    io.cancel_work_sync(PsrWork::Psr);
    io.cancel_work_sync(PsrWork::Dc3coDisable);
}

// upstream: intel_psr.c intel_psr_resume()
pub fn intel_psr_resume<I: PsrIo>(device: &mut PsrDevice, io: &mut I, _balanced: bool) {
    if !intel_encoder_can_psr(device) && !device.psr.caps.sink_panel_replay {
        return;
    }

    io.lock_psr();
    if !device.psr.enabled {
        io.unlock_psr();
        return;
    }

    if device.psr.pause_counter == 0 {
        io.drm_log(1, "Unbalanced PSR pause/resume!", 0);
        io.unlock_psr();
        return;
    }

    device.psr.pause_counter -= 1;
    if device.psr.pause_counter == 0 {
        device.psr.paused = false;
        intel_psr_activate(device, io);
    }
    io.unlock_psr();
}

// upstream: intel_psr.c intel_psr_needs_vblank_notification()
pub fn intel_psr_needs_vblank_notification(device: &PsrDevice) -> bool {
    device.is_edp
        && ((device.psr.caps.sink_panel_replay && device.psr.caps.source_panel_replay)
            || ((device.display_ver == 20 || device.stepping_3000_a0_b0)
                && device.psr.caps.sink_psr1
                && device.psr.caps.source_psr))
}
// upstream: intel_psr.c intel_psr_trigger_frame_change_event()
pub fn intel_psr_trigger_frame_change_event<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    if !device.psr.config.has_psr || intel_psr_use_trans_push(device) {
        return;
    }
    io.program_dsb(PsrReg::CursorSurfaceLive, device.psr.transcoder, 0);
}
// upstream: intel_psr.c intel_psr_min_set_context_latency()
pub fn intel_psr_min_set_context_latency(device: &PsrDevice) -> i32 {
    _intel_psr_min_set_context_latency(
        device,
        device.psr.config.has_panel_replay,
        device.psr.config.has_sel_update,
        device.vrr_enabled,
    )
}
// upstream: intel_psr.c man_trk_ctl_enable_bit_get()
pub fn man_trk_ctl_enable_bit_get(display_ver: u8, alderlake_p: bool) -> u32 {
    if alderlake_p || display_ver >= 14 {
        0
    } else {
        EDP_PSR2_MAN_TRACK_ENABLE
    }
}

// upstream: intel_psr.c man_trk_ctl_single_full_frame_bit_get()
pub fn man_trk_ctl_single_full_frame_bit_get(display_ver: u8, alderlake_p: bool) -> u32 {
    if alderlake_p || display_ver >= 14 {
        ADLP_MAN_TRACK_SINGLE_FULL_FRAME
    } else {
        PSR2_MAN_TRACK_SINGLE_FULL_FRAME
    }
}

// upstream: intel_psr.c man_trk_ctl_partial_frame_bit_get()
pub fn man_trk_ctl_partial_frame_bit_get(display_ver: u8, alderlake_p: bool) -> u32 {
    if alderlake_p || display_ver >= 14 {
        ADLP_PSR2_MAN_TRACK_PARTIAL
    } else {
        PSR2_MAN_TRACK_PARTIAL_FRAME
    }
}

// upstream: intel_psr.c man_trk_ctl_continuos_full_frame()
pub fn man_trk_ctl_continuos_full_frame(display_ver: u8, alderlake_p: bool) -> u32 {
    if alderlake_p || display_ver >= 14 {
        ADLP_MAN_TRACK_CONTINUOUS_FULL_FRAME
    } else {
        PSR2_MAN_TRACK_CONTINUOUS_FULL_FRAME
    }
}

// upstream: intel_psr.c intel_psr_force_update()
pub fn intel_psr_force_update<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    io.force_frame_change(device.psr.config.pipe);
}
// upstream: intel_psr.c intel_psr2_program_trans_man_trk_ctl()
pub fn intel_psr2_program_trans_man_trk_ctl<I: PsrIo>(device: &PsrDevice, io: &mut I, dsb: bool) {
    if !device.psr.config.enable_psr2_sel_fetch {
        return;
    }

    if !dsb {
        io.assert_psr_lock_held();
    }

    // Before display 20 the CFF bit is part of the same manual-tracking
    // control word; do not overwrite the full-frame configuration while it is
    // armed for frontbuffer invalidation.
    if device.display_ver < 20 && device.psr.psr2_sel_fetch_cff_enabled {
        return;
    }

    let control = device.psr.config.manual_track_control;
    if dsb {
        io.program_dsb(PsrReg::ManualTrack, device.psr.transcoder, control);
    } else {
        io.mmio_write(PsrReg::ManualTrack, device.psr.transcoder, control);
    }

    if !device.psr.config.enable_psr2_su_region_et {
        return;
    }

    let early_transport_size = device.psr.config.pipe_src_early_tpt;
    if dsb {
        io.program_dsb(
            PsrReg::PipeSourceSizeEarlyTpt,
            device.psr.transcoder,
            early_transport_size,
        );
    } else {
        io.mmio_write(
            PsrReg::PipeSourceSizeEarlyTpt,
            device.psr.transcoder,
            early_transport_size,
        );
    }

    if !device.dsc_enabled {
        return;
    }

    io.dsc_su_et_parameters(device.psr.config.su_area.height().max(0) as u32);
}

// upstream: intel_psr.c psr2_man_trk_ctl_calc()
pub fn psr2_man_trk_ctl_calc(device: &mut PsrDevice, full_update: bool) -> u32 {
    let display_ver = device.display_ver;
    let alderlake_p = device.alderlake_p;
    let mut value = man_trk_ctl_enable_bit_get(display_ver, alderlake_p)
        | man_trk_ctl_partial_frame_bit_get(display_ver, alderlake_p);
    let su_area = device.psr.config.su_area;

    // The partial-frame bit is needed even for a full update. A full update
    // adds the continuous-full-frame selection and does not program a region.
    if full_update {
        value |= man_trk_ctl_continuos_full_frame(display_ver, alderlake_p);
        return value;
    }

    if su_area.y1 == -1 {
        return value;
    }

    if alderlake_p || display_ver >= 14 {
        value |= ((su_area.y1 as u32) << 16) & ADLP_PSR2_MAN_TRACK_START_MASK;
        value |= (su_area.y2.saturating_sub(1) as u32) & ADLP_PSR2_MAN_TRACK_END_MASK;
    } else {
        if su_area.y1 % 4 != 0 || su_area.y2 % 4 != 0 {
            return 0;
        }
        let start = (su_area.y1 / 4 + 1) as u32;
        let end = (su_area.y2 / 4 + 1) as u32;
        value |= (start << 21) & EDP_PSR2_MAN_TRACK_START_MASK;
        value |= (end << 11) & EDP_PSR2_MAN_TRACK_END_MASK;
    }

    value
}

// upstream: intel_psr.c psr2_pipe_srcsz_early_tpt_calc()
pub fn psr2_pipe_srcsz_early_tpt_calc(config: &PsrConfig, full_update: bool) -> u32 {
    if !config.enable_psr2_su_region_et || full_update {
        return 0;
    }

    let width = config.su_area.width();
    let height = config.su_area.height();
    if width <= 0 || height <= 0 {
        return 0;
    }

    // PIPE_SRCSZ_ERLY_TPT stores size minus one in 13-bit width/height fields.
    ((width as u32 - 1) & 0x1fff) | (((height as u32 - 1) & 0x1fff) << 16)
}

// upstream: intel_psr.c clip_area_update()
pub fn clip_area_update(overlap: &mut Rect, damage: &mut Rect, display: &Rect) {
    damage.x1 = damage.x1.max(display.x1);
    damage.y1 = damage.y1.max(display.y1);
    damage.x2 = damage.x2.min(display.x2);
    damage.y2 = damage.y2.min(display.y2);
    if damage.empty() {
        return;
    }
    if overlap.y1 < 0 {
        overlap.y1 = damage.y1;
        overlap.y2 = damage.y2
    } else {
        overlap.y1 = overlap.y1.min(damage.y1);
        overlap.y2 = overlap.y2.max(damage.y2)
    }
}
// upstream: intel_psr.c intel_psr2_sel_fetch_pipe_alignment()
pub fn intel_psr2_sel_fetch_pipe_alignment(device: &mut PsrDevice, dsc_slice_height: u16) -> bool {
    let y_alignment = if device.dsc_enabled && (device.alderlake_p || device.display_ver >= 14) {
        dsc_slice_height
    } else {
        device.psr.config.su_y_granularity
    };
    let y_alignment = i32::from(y_alignment.max(1));
    let mut changed = false;
    let area = &mut device.psr.config.su_area;

    if area.y1 % y_alignment != 0 {
        area.y1 -= area.y1 % y_alignment;
        changed = true;
    }
    if area.y2 % y_alignment != 0 {
        area.y2 = ((area.y2 / y_alignment) + 1) * y_alignment;
        changed = true;
    }

    changed
}

// upstream: intel_psr.c intel_psr2_sel_fetch_et_alignment()
pub fn intel_psr2_sel_fetch_et_alignment(
    config: &mut PsrConfig,
    display_area: &Rect,
    cursor: &Rect,
    cursor_visible: bool,
) -> bool {
    if !config.enable_psr2_su_region_et || !cursor_visible {
        return false;
    }

    let mut intersection = config.su_area;
    intersection.x1 = intersection.x1.max(cursor.x1);
    intersection.y1 = intersection.y1.max(cursor.y1);
    intersection.x2 = intersection.x2.min(cursor.x2);
    intersection.y2 = intersection.y2.min(cursor.y2);
    if intersection.empty() {
        return false;
    }

    // The source marks the cursor as covered on any intersection, then expands
    // the pipe region to include the complete, display-clipped cursor bounds.
    let mut cursor_damage = *cursor;
    clip_area_update(&mut config.su_area, &mut cursor_damage, display_area);
    true
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneUpdate {
    pub id: u8,
    pub pipe: u8,
    pub old_dst: Rect,
    pub new_dst: Rect,
    pub old_visible: bool,
    pub new_visible: bool,
    pub old_alpha: u16,
    pub new_alpha: u16,
    pub damage: Option<Rect>,
    pub source_origin: (i32, i32),
    pub cursor: bool,
    pub negative_position: bool,
    pub scaled: bool,
    pub rotated: bool,
    pub linked_plane: Option<u8>,
    pub old_sel_fetch: Rect,
    pub new_sel_fetch: Rect,
}

// upstream: intel_psr.c psr2_sel_fetch_plane_state_supported()
pub fn psr2_sel_fetch_plane_state_supported(plane: &PlaneUpdate) -> bool {
    // TODO upstream: negative X/Y, per-plane scaling and rotation do not have a
    // reliable selective-fetch mapping yet. These properties can change without
    // a modeset, so the check is repeated for every atomic commit.
    plane.new_dst.y1 >= 0
        && plane.new_dst.x1 >= 0
        && !plane.negative_position
        && !plane.scaled
        && !plane.rotated
}

// upstream: intel_psr.c psr2_sel_fetch_pipe_state_supported()
pub fn psr2_sel_fetch_pipe_state_supported(has_scaler: bool, async_flip_planes: u32) -> bool {
    !has_scaler && async_flip_planes == 0
}
// upstream: intel_psr.c intel_psr_apply_pr_link_on_su_wa()
pub fn intel_psr_apply_pr_link_on_su_wa(
    config: &mut PsrConfig,
    display_ver: u8,
    ycbcr420: bool,
    uhbr: bool,
    hdisplay: u32,
    panel_replay: bool,
    sel_update: bool,
) {
    if config.su_area.y1 != 0 || config.su_area.y2 != 0 {
        return;
    }
    let limit = match (ycbcr420, uhbr) {
        (true, true) => 1230,
        (true, false) => 546,
        (false, true) => 615,
        (false, false) => 273,
    };
    if display_ver == 30 && hdisplay >= limit && panel_replay && sel_update {
        config.su_area.y2 += 1
    }
}
// upstream: intel_psr.c intel_psr_apply_su_area_workarounds()
pub fn intel_psr_apply_su_area_workarounds(
    device: &mut PsrDevice,
    splitter: bool,
    wa_force_top: bool,
    display_ver: u8,
    hdisplay: u32,
    ycbcr420: bool,
    uhbr: bool,
) {
    // Wa_14014971492: splitter modes on TGL, ADL-P and early MTL require the
    // selective region to start at the top of the frame (except Panel Replay).
    let wa_14014971492 = device.wa_14014971492
        && (device.tigerlake || device.alderlake_p || device.display_ver >= 14);
    if !device.psr.config.has_panel_replay && splitter && wa_14014971492 {
        device.psr.config.su_area.y1 = 0;
    }

    // Wa_16029024088: include line zero when prior-scanline indication is
    // requested but Region Early Transport is not active.
    if wa_force_top
        && device.psr.config.req_psr2_sdp_prior_scanline
        && !device.psr.config.enable_psr2_su_region_et
    {
        device.psr.config.su_area.y1 = 0;
    }

    // Wa_14019834836 is only present on display version 30 and can extend PR
    // Selective Update by one line to preserve the link after AS-SDP.
    if display_ver == 30 {
        let panel_replay = device.psr.config.has_panel_replay;
        let sel_update = device.psr.config.has_sel_update;
        intel_psr_apply_pr_link_on_su_wa(
            &mut device.psr.config,
            display_ver,
            ycbcr420,
            uhbr,
            hdisplay,
            panel_replay,
            sel_update,
        );
    }
}

// upstream: intel_psr.c intel_psr2_sel_fetch_update()
pub fn intel_psr2_sel_fetch_update<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    planes: &mut [PlaneUpdate],
    has_scaler: bool,
    async_flip_planes: u32,
    dsc_slice_height: u16,
) -> i32 {
    if !device.psr.config.enable_psr2_sel_fetch {
        return 0;
    }

    device.update_planes = 0;
    let display_area = Rect {
        x1: 0,
        y1: 0,
        x2: i32::from(device.psr.config.mode_width),
        y2: i32::from(device.psr.config.mode_height),
    };
    let mut full_update = false;

    // Scaling or asynchronous flips cannot be represented by selective fetch;
    // use a full update but continue through the common register-programming
    // tail below.
    if !psr2_sel_fetch_pipe_state_supported(has_scaler, async_flip_planes) {
        full_update = true;
    }

    let mut su_area = Rect {
        x1: 0,
        y1: -1,
        x2: display_area.x2,
        y2: -1,
    };
    device.psr.config.su_area = su_area;

    if !full_update {
        // First pass computes the pipe-wide damaged Y range. Each plane's own
        // selector is programmed in the later pass from this shared region.
        for plane in planes.iter() {
            if plane.pipe != device.psr.config.pipe {
                continue;
            }
            if !plane.new_visible && !plane.old_visible {
                continue;
            }
            if !psr2_sel_fetch_plane_state_supported(plane) {
                full_update = true;
                break;
            }

            let mut damaged_area = Rect {
                x1: 0,
                y1: -1,
                x2: i32::MAX,
                y2: -1,
            };

            // A visibility change or movement redraws both the old and new
            // locations. Clip each one to the active display before expanding
            // the shared SU Y interval.
            if plane.new_visible != plane.old_visible || plane.new_dst != plane.old_dst {
                if plane.old_visible {
                    damaged_area.y1 = plane.old_dst.y1;
                    damaged_area.y2 = plane.old_dst.y2;
                    clip_area_update(&mut su_area, &mut damaged_area, &display_area);
                }
                if plane.new_visible {
                    damaged_area.y1 = plane.new_dst.y1;
                    damaged_area.y2 = plane.new_dst.y2;
                    clip_area_update(&mut su_area, &mut damaged_area, &display_area);
                }
                continue;
            }

            // Alpha changes affect the full plane area even if its geometry is
            // unchanged.
            if plane.new_alpha != plane.old_alpha {
                damaged_area.y1 = plane.new_dst.y1;
                damaged_area.y2 = plane.new_dst.y2;
                clip_area_update(&mut su_area, &mut damaged_area, &display_area);
                continue;
            }

            let Some(mut damaged_area) = plane.damage else {
                continue;
            };

            // DRM damage is in source coordinates after fixed-point source
            // bounds were rounded to integer pixels upstream. Translate it to
            // CRTC coordinates using destination minus source origin.
            damaged_area.y1 += plane.new_dst.y1 - plane.source_origin.1;
            damaged_area.y2 += plane.new_dst.y1 - plane.source_origin.1;
            damaged_area.x1 += plane.new_dst.x1 - plane.source_origin.0;
            damaged_area.x2 += plane.new_dst.x1 - plane.source_origin.0;
            clip_area_update(&mut su_area, &mut damaged_area, &display_area);
        }

        // No plane supplied useful damage. The C implementation reports this
        // once and falls back instead of programming an empty selective region.
        if su_area.y1 == -1 {
            io.drm_log(
                1,
                "Selective fetch area calculation failed",
                u32::from(device.psr.config.pipe),
            );
            full_update = true;
        }
    }

    if !full_update {
        device.psr.config.su_area = su_area;

        // Wa_14014971492, Wa_16029024088, and Wa_14019834836 adjust the region
        // only after the union of all plane damage is known.
        let splitter_enabled = device.splitter_enabled;
        let wa_force_top = device.wa_16029024088;
        let display_ver = device.display_ver;
        let hdisplay = display_area.x2 as u32;
        let ycbcr420 = device.output_format_ycbcr420;
        let uhbr = device.uhbr;
        intel_psr_apply_su_area_workarounds(
            device,
            splitter_enabled,
            wa_force_top,
            display_ver,
            hdisplay,
            ycbcr420,
            uhbr,
        );

        let ret = io.add_affected_planes(device.psr.config.pipe);
        if ret != 0 {
            return ret;
        }

        loop {
            let mut cursor_in_su_area = false;

            // Early transport requires the region to cover a cursor entirely
            // whenever that cursor intersects the region. Expand using the
            // display-clipped cursor bounds, then align Y to sink/DSC needs.
            let pipe = device.psr.config.pipe;
            for cursor in planes
                .iter()
                .filter(|plane| plane.pipe == pipe && plane.cursor && plane.new_visible)
            {
                cursor_in_su_area |= intel_psr2_sel_fetch_et_alignment(
                    &mut device.psr.config,
                    &display_area,
                    &cursor.new_dst,
                    true,
                );
            }

            let su_area_changed = intel_psr2_sel_fetch_pipe_alignment(device, dsc_slice_height);
            if cursor_in_su_area || !su_area_changed {
                break;
            }
        }

        // Second pass intersects the final pipe damage area with each visible
        // plane and produces that plane's local selective-fetch Y range.
        for plane in planes.iter_mut() {
            if plane.pipe != device.psr.config.pipe || !plane.new_visible {
                continue;
            }

            let mut intersection = device.psr.config.su_area;
            intersection.x1 = intersection.x1.max(plane.new_dst.x1);
            intersection.y1 = intersection.y1.max(plane.new_dst.y1);
            intersection.x2 = intersection.x2.min(plane.new_dst.x2);
            intersection.y2 = intersection.y2.min(plane.new_dst.y2);

            if intersection.empty() {
                plane.new_sel_fetch.y1 = -1;
                plane.new_sel_fetch.y2 = -1;
                if plane.old_sel_fetch.height() > 0 && plane.id < 32 {
                    device.update_planes |= 1 << plane.id;
                }
                continue;
            }

            if !psr2_sel_fetch_plane_state_supported(plane) {
                full_update = true;
                break;
            }

            plane.new_sel_fetch.y1 = intersection.y1 - plane.new_dst.y1;
            plane.new_sel_fetch.y2 = intersection.y2 - plane.new_dst.y1;
            device.update_planes |= 1 << plane.id;

            // Planar UV fetch uses the same local Y interval for its linked Y
            // plane. Failure to obtain the linked atomic state propagates.
            if let Some(linked_plane) = plane.linked_plane {
                let ret = io.update_linked_plane_sel_fetch(
                    linked_plane,
                    Rect {
                        x1: 0,
                        y1: plane.new_sel_fetch.y1,
                        x2: 0,
                        y2: plane.new_sel_fetch.y2,
                    },
                );
                if ret != 0 {
                    return ret;
                }
                if linked_plane < 32 {
                    device.update_planes |= 1 << linked_plane;
                }
            }
        }
    }

    if full_update {
        let mut full_damage = display_area;
        clip_area_update(
            &mut device.psr.config.su_area,
            &mut full_damage,
            &display_area,
        );
    }

    let manual_track_control = psr2_man_trk_ctl_calc(device, full_update);
    device.psr.config.manual_track_control = manual_track_control;
    device.psr.config.pipe_src_early_tpt =
        psr2_pipe_srcsz_early_tpt_calc(&device.psr.config, full_update);
    device.psr.config.pipe_src = (
        (device.psr.config.pipe_src_early_tpt & 0xffff) as u16,
        (manual_track_control & 0xffff) as u16,
    );
    0
}

// upstream: intel_psr.c intel_psr2_panic_force_full_update()
pub fn intel_psr2_panic_force_full_update<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    let value = man_trk_ctl_enable_bit_get(device.display_ver, device.alderlake_p)
        | man_trk_ctl_partial_frame_bit_get(device.display_ver, device.alderlake_p)
        | man_trk_ctl_continuos_full_frame(device.display_ver, device.alderlake_p);

    // Panic path writes through the forcewake-safe firmware MMIO interface.
    if device.display_ver >= 20 {
        io.mmio_write_fw(
            PsrReg::LnlSffControl,
            device.psr.transcoder,
            ADLP_MAN_TRACK_SINGLE_FULL_FRAME,
        );
    } else {
        io.mmio_write_fw(PsrReg::ManualTrack, device.psr.transcoder, value);
    }

    if device.psr.config.enable_psr2_su_region_et {
        io.mmio_write_fw(PsrReg::PipeSourceSizeEarlyTpt, device.psr.transcoder, 0);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrontbufferOrigin {
    Render,
    Flip,
    CursorUpdate,
    Other,
}

// upstream: intel_psr.c intel_psr_pre_plane_update()
pub fn intel_psr_pre_plane_update<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    new_config: &PsrConfig,
    needs_modeset: bool,
    update_m_n: bool,
    update_lrr: bool,
    active_planes: u32,
    wm_level_disabled: bool,
) {
    if !device.has_psr_hardware {
        return;
    }
    io.lock_psr();
    if !new_config.has_psr {
        device.psr.no_psr_reason = Some("PSR not enabled in new atomic state");
    }
    if device.psr.enabled
        && (needs_modeset
            || update_m_n
            || update_lrr
            || !new_config.has_psr
            || active_planes == 0
            || new_config.has_sel_update != device.psr.sel_update_enabled
            || new_config.has_panel_replay != device.psr.panel_replay_enabled
            || new_config.enable_psr2_su_region_et != device.psr.su_region_et_enabled
            || device.display_ver < 11 && wm_level_disabled)
    {
        intel_psr_disable_locked(device, io)
    } else if device.psr.enabled && wm_level_disabled {
        wm_optimization_wa(
            device,
            io,
            true,
            device.timing.vblank_start,
            device.timing.vdisplay,
        )
    }
    io.unlock_psr();
}
// upstream: intel_psr.c verify_panel_replay_dsc_state()
pub fn verify_panel_replay_dsc_state(device: &PsrDevice, dsc_support: u8) -> bool {
    !device.psr.config.has_panel_replay || !device.dsc_enabled || dsc_support != 0
}
// upstream: intel_psr.c intel_psr_post_plane_update()
pub fn intel_psr_post_plane_update<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    config: PsrConfig,
    active_planes: u32,
    wm_level_disabled: bool,
    crc_enabled: bool,
) {
    if !device.has_psr_hardware || !config.has_psr {
        return;
    }
    if !verify_panel_replay_dsc_state(device, u8::from(device.psr.caps.pr_dsc)) {
        io.drm_log(2, "Panel Replay state selected with unsupported DSC", 0);
    }
    io.lock_psr();
    let mut keep_disabled = device.psr.sink_not_reliable;
    if active_planes == 0 {
        device.psr.no_psr_reason = Some("All planes inactive");
        keep_disabled = true;
    }
    if device.display_ver < 11 && wm_level_disabled {
        device.psr.no_psr_reason = Some("Workaround #1136 for skl, bxt");
        keep_disabled = true;
    }
    if !device.psr.enabled && !keep_disabled {
        let _enabled = intel_psr_enable_locked(device, io, config);
    } else if device.psr.enabled && !wm_level_disabled {
        wm_optimization_wa(
            device,
            io,
            false,
            device.timing.vblank_start,
            device.timing.vdisplay,
        )
    }
    if crc_enabled && device.psr.enabled {
        intel_psr_force_update(device, io)
    }
    device.psr.busy_frontbuffer_bits = 0;
    io.unlock_psr();
}
// upstream: intel_psr.c _psr2_ready_for_pipe_update_locked()
pub fn _psr2_ready_for_pipe_update_locked<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
    dsb: bool,
) -> bool {
    wait_status_clear(device, io, PsrReg::Status, 1 << 4, 50, dsb)
}
// upstream: intel_psr.c _psr1_ready_for_pipe_update_locked()
pub fn _psr1_ready_for_pipe_update_locked<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
    dsb: bool,
) -> bool {
    wait_status_clear(device, io, psr_status_reg(device), 0x7, 50, dsb)
}
fn wait_status_clear<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
    reg: PsrReg,
    mask: u32,
    timeout_ms: u32,
    dsb: bool,
) -> bool {
    if dsb {
        return true;
    }
    for _ in 0..timeout_ms * 1000 {
        if io.mmio_read(reg, device.psr.transcoder) & mask == 0 {
            return true;
        }
        io.delay_us(100, 100)
    }
    false
}
// upstream: intel_psr.c intel_psr_wait_for_idle_locked()
pub fn intel_psr_wait_for_idle_locked<I: PsrIo>(device: &PsrDevice, io: &mut I) -> bool {
    if !device.psr.config.has_psr || !device.psr.enabled || device.psr.panel_replay_enabled {
        return true;
    }
    let idle = if device.psr.sel_update_enabled {
        _psr2_ready_for_pipe_update_locked(device, io, false)
    } else {
        _psr1_ready_for_pipe_update_locked(device, io, false)
    };
    if !idle {
        io.drm_log(2, "PSR wait timed out, atomic update may fail", 0)
    }
    idle
}
// upstream: intel_psr.c intel_psr_wait_for_idle_dsb()
pub fn intel_psr_wait_for_idle_dsb<I: PsrIo>(device: &PsrDevice, io: &mut I, dsb: bool) {
    if !device.psr.config.has_psr || device.psr.config.has_panel_replay {
        return;
    }
    if device.psr.config.has_sel_update {
        let _ = _psr2_ready_for_pipe_update_locked(device, io, dsb);
    } else {
        let _ = _psr1_ready_for_pipe_update_locked(device, io, dsb);
    }
}
// upstream: intel_psr.c __psr_wait_for_idle_locked()
pub fn __psr_wait_for_idle_locked<I: PsrIo>(device: &mut PsrDevice, io: &mut I) -> bool {
    if !device.psr.enabled {
        return false;
    }
    io.unlock_psr();
    let ok = intel_psr_wait_exit_locked(device, io, 50);
    io.lock_psr();
    ok && device.psr.enabled && !device.psr.paused
}
// upstream: intel_psr.c intel_psr_fastset_force()
pub fn intel_psr_fastset_force<I: PsrIo>(device: &mut PsrDevice, io: &mut I) -> i32 {
    // The framework hook performs the upstream internal atomic commit: allocate
    // an internal commit, acquire interruptible modeset locks, mark connected
    // eDP CRTC state mode_changed, commit, and back off/retry on -EDEADLK.
    let error = io.fastset_force();
    if error == 0 {
        device.mode_changed = true;
        device.atomic_committed = true;
    }
    error
}

// upstream: intel_psr.c intel_psr_debug_set()
pub fn intel_psr_debug_set<I: PsrIo>(device: &mut PsrDevice, io: &mut I, val: u64) -> i32 {
    let val = val as u32;
    let mode = val & I915_PSR_DEBUG_MODE_MASK;
    let disable_bits =
        val & (I915_PSR_DEBUG_SU_REGION_ET_DISABLE | I915_PSR_DEBUG_PANEL_REPLAY_DISABLE);
    let valid_mask = I915_PSR_DEBUG_IRQ
        | I915_PSR_DEBUG_SU_REGION_ET_DISABLE
        | I915_PSR_DEBUG_PANEL_REPLAY_DISABLE
        | I915_PSR_DEBUG_MODE_MASK;

    if val & !valid_mask != 0 || mode > I915_PSR_DEBUG_ENABLE_SEL_FETCH {
        io.drm_log(0, "Invalid PSR debug mask", val);
        return -22; // -EINVAL
    }

    if let Err(error) = io.lock_psr_interruptible() {
        return error;
    }

    let old_mode = device.psr.debug & I915_PSR_DEBUG_MODE_MASK;
    let old_disable_bits = device.psr.debug
        & (I915_PSR_DEBUG_SU_REGION_ET_DISABLE | I915_PSR_DEBUG_PANEL_REPLAY_DISABLE);
    device.psr.debug = val;

    // If source is already enabled, apply IRQ-mask changes immediately. For a
    // disabled source, enable_source programs the debug/IRQ state later.
    if device.psr.enabled {
        psr_irq_control(device, io);
    }
    io.unlock_psr();

    if old_mode != mode || old_disable_bits != disable_bits {
        intel_psr_fastset_force(device, io)
    } else {
        0
    }
}

// upstream: intel_psr.c intel_psr_handle_irq()
pub fn intel_psr_handle_irq<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    intel_psr_disable_locked(device, io);
    device.psr.sink_not_reliable = true;
    let _ = io.aux_write(0x0600, &[1]);
}
// upstream: intel_psr.c intel_psr_work()
pub fn intel_psr_work<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    io.lock_psr();
    if !device.psr.enabled {
        io.unlock_psr();
        return;
    }
    if device.psr.irq_aux_error {
        intel_psr_handle_irq(device, io);
        io.unlock_psr();
        return;
    }
    if device.psr.paused {
        io.unlock_psr();
        return;
    }
    if !__psr_wait_for_idle_locked(device, io) {
        io.unlock_psr();
        return;
    }
    if device.psr.busy_frontbuffer_bits != 0 || device.psr.active {
        io.unlock_psr();
        return;
    }
    intel_psr_activate(device, io);
    io.unlock_psr();
}
// upstream: intel_psr.c intel_psr_configure_full_frame_update()
pub fn intel_psr_configure_full_frame_update<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    if !device.psr.sel_update_enabled {
        return;
    }
    if device.display_ver >= 20 {
        io.mmio_write(PsrReg::ManualTrack, device.psr.transcoder, 1 << 4)
    } else {
        let v = man_trk_ctl_enable_bit_get(device.display_ver, device.alderlake_p)
            | man_trk_ctl_partial_frame_bit_get(device.display_ver, device.alderlake_p)
            | man_trk_ctl_single_full_frame_bit_get(device.display_ver, device.alderlake_p)
            | man_trk_ctl_continuos_full_frame(device.display_ver, device.alderlake_p);
        io.mmio_write(PsrReg::ManualTrack, device.psr.transcoder, v)
    }
}
// upstream: intel_psr.c _psr_invalidate_handle()
pub fn _psr_invalidate_handle<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    if device.display_ver < 20 && device.psr.sel_update_enabled {
        intel_psr_configure_full_frame_update(device, io);
        intel_psr_force_update(device, io)
    } else {
        let mut d = device.clone();
        intel_psr_exit(&mut d, io)
    }
}
// upstream: intel_psr.c intel_psr_invalidate()
pub fn intel_psr_invalidate<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    frontbuffer_bits: u32,
    pipe_mask: u32,
    origin: FrontbufferOrigin,
) {
    if origin == FrontbufferOrigin::Flip {
        return;
    }
    let bits = frontbuffer_bits & pipe_mask;
    io.lock_psr();
    if device.psr.enabled {
        device.psr.busy_frontbuffer_bits |= bits;
        if bits != 0 {
            _psr_invalidate_handle(device, io)
        }
    }
    io.unlock_psr();
}
// upstream: intel_psr.c tgl_dc3co_flush_locked()
pub fn tgl_dc3co_flush_locked<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    frontbuffer_bits: u32,
    pipe_mask: u32,
) {
    if device.psr.config.exit_line == 0
        || !device.psr.sel_update_enabled
        || !device.psr.active
        || frontbuffer_bits & pipe_mask == 0
    {
        return;
    }
    tgl_psr2_enable_dc3co(device, io);
    io.queue_work(
        PsrQueue::Unordered,
        PsrWork::Dc3coDisable,
        device.psr.config.frame_time_us.saturating_mul(6) / 1000,
    );
}
// upstream: intel_psr.c _psr_flush_handle()
pub fn _psr_flush_handle<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    if device.display_ver >= 20 {
        intel_psr_exit(device, io)
    } else if device.psr.sel_update_enabled {
        intel_psr_configure_full_frame_update(device, io);
        intel_psr_force_update(device, io)
    } else {
        intel_psr_force_update(device, io)
    }
    if !device.psr.active && device.psr.busy_frontbuffer_bits == 0 {
        io.queue_work(PsrQueue::Unordered, PsrWork::Psr, 0)
    }
}
// upstream: intel_psr.c intel_psr_flush()
pub fn intel_psr_flush<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    frontbuffer_bits: u32,
    pipe_mask: u32,
    origin: FrontbufferOrigin,
) {
    let bits = frontbuffer_bits & pipe_mask;
    io.lock_psr();
    if !device.psr.enabled {
        io.unlock_psr();
        return;
    }
    device.psr.busy_frontbuffer_bits &= !bits;
    if device.psr.paused {
        io.unlock_psr();
        return;
    }
    if origin == FrontbufferOrigin::Flip
        || origin == FrontbufferOrigin::CursorUpdate && !device.psr.sel_update_enabled
    {
        tgl_dc3co_flush_locked(device, io, frontbuffer_bits, pipe_mask);
        io.unlock_psr();
        return;
    }
    if bits != 0 {
        _psr_flush_handle(device, io)
    }
    io.unlock_psr();
}
// upstream: intel_psr.c intel_psr_init()
pub fn intel_psr_init<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    if !(device.has_psr_hardware || device.has_dp20) {
        return;
    }

    // HSW PSR is physically tied to port A. BDW/GEN9/GEN11 have per-
    // transcoder registers, but HW validation only covers eDP; keep the
    // pre-display-12 instance constrained to port A.
    if device.display_ver < 12 && !device.port_a {
        io.drm_log(
            0,
            "PSR condition failed: Port not supported",
            u32::from(device.psr.port),
        );
        return;
    }

    if let Some(policy) = io.firmware_psr_policy(device.connector_id) {
        device.psr.panel_vbt_psr = policy.enable_psr;
        device.vbt_full_link = policy.full_link;
        device.psr.config.idle_frames = policy.idle_frames;
        device.tp1_wakeup_time_us = policy.tp1_wakeup_time_us;
        device.tp23_wakeup_time_us = policy.tp23_wakeup_time_us;
        device.psr2_tp23_wakeup_time_us = policy.psr2_tp23_wakeup_time_us;
    }

    if (device.has_dp20 && !device.is_edp) || device.display_ver >= 20 {
        device.psr.caps.source_panel_replay = true;
    }
    if device.has_psr_hardware && device.is_edp {
        device.psr.caps.source_psr = true;
    }

    // Before display 12, honor the firmware full-link/standby choice. Newer
    // platforms use the source's fixed defaults.
    if device.display_ver < 12 {
        device.psr.config.link_off = device.vbt_full_link;
    }

    io.init_work(PsrWork::Psr);
    io.init_work(PsrWork::Dc3coDisable);
    io.init_mutex();
    device.psr.work_pending = false;
    device.psr.enabled = false;
    device.psr.active = false;
    device.psr.link_ok = false;
    device.psr.busy_frontbuffer_bits = 0;
}

// upstream: intel_psr.c psr_get_status_and_error_status()
pub fn psr_get_status_and_error_status<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
) -> Result<(u8, u8), i32> {
    let (status_reg, error_reg) = if device.psr.panel_replay_enabled {
        (0x2022, 0x2020)
    } else {
        (0x2008, 0x2006)
    };
    let mut s = [0u8];
    let mut e = [0u8];
    let r = io.aux_read(status_reg, &mut s);
    if r != 1 {
        return Err(r);
    }
    let r = io.aux_read(error_reg, &mut e);
    if r != 1 {
        return Err(r);
    }
    Ok((s[0] & 0x7, e[0]))
}
// upstream: intel_psr.c psr_alpm_check()
pub fn psr_alpm_check<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    if !device.psr.sel_update_enabled {
        return;
    }

    if io.alpm_has_error(device) {
        intel_psr_disable_locked(device, io);
        device.psr.sink_not_reliable = true;
    }
}

// upstream: intel_psr.c psr_capability_changed_check()
pub fn psr_capability_changed_check<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    let mut esi = [0u8; 1];
    let read = io.aux_read(0x2007, &mut esi); // DP_PSR_ESI
    if read != 1 {
        io.drm_log(2, "Error reading DP_PSR_ESI", read as u32);
        return;
    }

    if esi[0] & 1 != 0 {
        intel_psr_disable_locked(device, io);
        device.psr.sink_not_reliable = true;
        io.drm_log(0, "Sink PSR capability changed, disabling PSR", 0);

        // DP_PSR_ESI is write-one-to-clear; echo the observed byte.
        let _ = io.aux_write(0x2007, &esi);
    }
}

// upstream: intel_psr.c intel_psr_short_pulse()
pub fn intel_psr_short_pulse<I: PsrIo>(device: &mut PsrDevice, io: &mut I) {
    if !intel_encoder_can_psr(device) && !device.psr.caps.sink_panel_replay {
        return;
    }

    io.lock_psr();
    device.psr.link_ok = false;
    if !device.psr.enabled {
        io.unlock_psr();
        return;
    }

    let (status, error_status) = match psr_get_status_and_error_status(device, io) {
        Ok(status_and_error) => status_and_error,
        Err(error) => {
            io.drm_log(2, "Error reading PSR status or error status", error as u32);
            io.unlock_psr();
            return;
        }
    };

    // Common bit definitions are shared by PSR and Panel Replay sinks.
    const DP_PSR_LINK_CRC_ERROR: u8 = 1 << 0;
    const DP_PSR_RFB_STORAGE_ERROR: u8 = 1 << 1;
    const DP_PSR_VSC_SDP_UNCORRECTABLE_ERROR: u8 = 1 << 2;
    const DP_PSR_SINK_INTERNAL_ERROR: u8 = 7;
    const COMMON_ERRORS: u8 =
        DP_PSR_RFB_STORAGE_ERROR | DP_PSR_VSC_SDP_UNCORRECTABLE_ERROR | DP_PSR_LINK_CRC_ERROR;

    if (!device.psr.panel_replay_enabled && status == DP_PSR_SINK_INTERNAL_ERROR)
        || error_status & COMMON_ERRORS != 0
    {
        intel_psr_disable_locked(device, io);
        device.psr.sink_not_reliable = true;
    }

    if !device.psr.panel_replay_enabled && status == DP_PSR_SINK_INTERNAL_ERROR && error_status == 0
    {
        io.drm_log(0, "PSR sink internal error, disabling PSR", 0);
    }
    if error_status & DP_PSR_RFB_STORAGE_ERROR != 0 {
        io.drm_log(0, "PSR RFB storage error, disabling PSR", 0);
    }
    if error_status & DP_PSR_VSC_SDP_UNCORRECTABLE_ERROR != 0 {
        io.drm_log(0, "PSR VSC SDP uncorrectable error, disabling PSR", 0);
    }
    if error_status & DP_PSR_LINK_CRC_ERROR != 0 {
        io.drm_log(0, "PSR Link CRC error, disabling PSR", 0);
    }

    if error_status & !COMMON_ERRORS != 0 {
        io.drm_log(
            2,
            "PSR_ERROR_STATUS unhandled errors",
            u32::from(error_status & !COMMON_ERRORS),
        );
    }

    // Clear the error register after logging, then check ALPM and capability
    // changes for PSR (Panel Replay has its own status path).
    let _ = io.aux_write(0x2006, &[error_status]); // DP_PSR_ERROR_STATUS
    if !device.psr.panel_replay_enabled {
        psr_alpm_check(device, io);
        psr_capability_changed_check(device, io);
    }

    io.unlock_psr();
}

// upstream: intel_psr.c intel_psr_enabled()
pub fn intel_psr_enabled<I: PsrIo>(device: &PsrDevice, io: &mut I) -> bool {
    if !(device.psr.caps.sink_psr1 && device.psr.caps.source_psr) {
        return false;
    }

    io.lock_psr();
    let enabled = device.psr.enabled;
    io.unlock_psr();
    enabled
}

// upstream: intel_psr.c intel_psr_link_ok()
pub fn intel_psr_link_ok<I: PsrIo>(device: &PsrDevice, io: &mut I) -> bool {
    if (!intel_encoder_can_psr(device) && !device.psr.caps.sink_panel_replay) || !device.is_edp {
        return false;
    }
    io.lock_psr();
    let ok = device.psr.link_ok;
    io.unlock_psr();
    ok
}
// upstream: intel_psr.c intel_psr_lock()
pub fn intel_psr_lock<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    if !device.psr.config.has_psr {
        return;
    }
    io.lock_psr();
}

// upstream: intel_psr.c intel_psr_unlock()
pub fn intel_psr_unlock<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    if !device.psr.config.has_psr {
        return;
    }
    io.unlock_psr();
}

// upstream: intel_psr.c intel_psr_apply_underrun_on_idle_wa_locked()
pub fn intel_psr_apply_underrun_on_idle_wa_locked<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    dc5_dc6_blocked: bool,
) {
    if !device.psr.active || !device.psr.pkg_c_latency_used {
        return;
    }

    if device.psr.sel_update_enabled {
        let idle_frames = if dc5_dc6_blocked {
            0
        } else {
            psr_compute_idle_frames(device, io, device.psr.config.idle_frames)
        };
        psr2_program_idle_frames(device, io, u32::from(idle_frames));
    } else {
        io.dmc_start_pkgc_exit_at_undelayed_vblank(device.psr.config.pipe, dc5_dc6_blocked);
    }
}

// upstream: intel_psr.c psr_dc5_dc6_wa_work()
pub fn psr_dc5_dc6_wa_work<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    current_dc_state: u8,
    vblank_enabled: bool,
) {
    io.lock_psr();
    if device.psr.enabled && !device.psr.panel_replay_enabled && !device.psr.pkg_c_latency_used {
        let dc5_dc6_blocked = is_dc5_dc6_blocked(device, current_dc_state, vblank_enabled);
        intel_psr_apply_underrun_on_idle_wa_locked(device, io, dc5_dc6_blocked);
    }
    io.unlock_psr();
}

// upstream: intel_psr.c intel_psr_notify_dc5_dc6()
pub fn intel_psr_notify_dc5_dc6<I: PsrIo>(device: &PsrDevice, io: &mut I, wa_enabled: bool) {
    if wa_enabled {
        io.queue_work(PsrQueue::Unordered, PsrWork::Dc5Dc6Wa, 0)
    }
    let _ = device;
}
// upstream: intel_psr.c intel_psr_dc5_dc6_wa_init()
pub fn intel_psr_dc5_dc6_wa_init<I: PsrIo>(_device: &PsrDevice, io: &mut I, wa_enabled: bool) {
    if !wa_enabled {
        return;
    }
    io.init_work(PsrWork::Dc5Dc6Wa);
}

// upstream: intel_psr.c intel_psr_notify_pipe_change()
pub fn intel_psr_notify_pipe_change<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    wa_enabled: bool,
    pipe: u8,
    enable: bool,
) {
    if !wa_enabled {
        return;
    }

    io.lock_psr();
    if !device.psr.enabled || device.psr.panel_replay_enabled {
        io.unlock_psr();
        return;
    }

    let old_active_non_psr_pipes = device.psr.active_non_psr_pipes;
    let new_active_non_psr_pipes = if enable {
        old_active_non_psr_pipes | (1 << pipe)
    } else {
        old_active_non_psr_pipes & !(1 << pipe)
    };

    if new_active_non_psr_pipes == old_active_non_psr_pipes {
        io.unlock_psr();
        return;
    }

    if (enable && old_active_non_psr_pipes != 0)
        || (!enable && old_active_non_psr_pipes == 0)
        || !device.psr.pkg_c_latency_used
    {
        device.psr.active_non_psr_pipes = new_active_non_psr_pipes;
        io.unlock_psr();
        return;
    }

    device.psr.active_non_psr_pipes = new_active_non_psr_pipes;
    let dc5_dc6_blocked = is_dc5_dc6_blocked(device, 6, false);
    intel_psr_apply_underrun_on_idle_wa_locked(device, io, dc5_dc6_blocked);
    io.unlock_psr();
}

// upstream: intel_psr.c intel_psr_notify_vblank_enable_disable()
pub fn intel_psr_notify_vblank_enable_disable<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    enable: bool,
) {
    io.lock_psr();

    if device.psr.caps.sink_panel_replay {
        if enable {
            io.vblank_power_reference(true);
        } else {
            io.vblank_power_reference(false);
        }
    }

    if device.psr.enabled && !device.psr.panel_replay_enabled && device.psr.pkg_c_latency_used {
        let dc5_dc6_blocked = is_dc5_dc6_blocked(device, if enable { 4 } else { 6 }, enable);
        intel_psr_apply_underrun_on_idle_wa_locked(device, io, dc5_dc6_blocked);
    }

    io.unlock_psr();
}

// upstream: intel_psr.c psr_source_status()
pub fn psr_source_status<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    const PSR2_STATE: &[&str] = &[
        "IDLE",
        "CAPTURE",
        "CAPTURE_FS",
        "SLEEP",
        "BUFON_FW",
        "ML_UP",
        "SU_STANDBY",
        "FAST_SLEEP",
        "DEEP_SLEEP",
        "BUF_ON",
        "TG_ON",
    ];
    const PSR1_STATE: &[&str] = &[
        "IDLE",
        "SRDONACK",
        "SRDENT",
        "BUFOFF",
        "BUFON",
        "AUXACK",
        "SRDOFFACK",
        "SRDENT_ON",
    ];

    let use_psr2_status = (device.is_edp || device.display_ver >= 30)
        && (device.psr.sel_update_enabled || device.psr.panel_replay_enabled);
    let (register, state) = if use_psr2_status {
        let value = io.mmio_read(PsrReg::Psr2Status, device.psr.transcoder);
        let state = ((value & EDP_PSR2_STATUS_STATE_MASK) >> 28) as usize;
        (value, PSR2_STATE.get(state).copied().unwrap_or("unknown"))
    } else {
        let value = io.mmio_read(psr_status_reg(device), device.psr.transcoder);
        let state = ((value & EDP_PSR_STATUS_STATE_MASK) >> 29) as usize;
        (value, PSR1_STATE.get(state).copied().unwrap_or("unknown"))
    };

    io.debug_output(&format!(
        "Source PSR/PanelReplay status: {state} [0x{register:08x}]\n"
    ));
}

// upstream: intel_psr.c intel_psr_sink_capability()
pub fn intel_psr_sink_capability<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    let psr_supported = device.psr.caps.sink_psr1;
    let panel_replay_supported = device.psr.caps.sink_panel_replay;
    io.debug_output(&format!("Sink support: PSR = {psr_supported}"));

    if psr_supported {
        io.debug_output(&format!(" [0x{:02x}]", device.psr.caps.psr_dpcd_version));
    }
    if device.psr.caps.psr_dpcd_version == 4 {
        io.debug_output(" (Early Transport)");
    }

    io.debug_output(&format!(
        ", Panel Replay = {panel_replay_supported}, Panel Replay Selective Update = {}, Panel \
         Replay DSC support = {}",
        device.psr.caps.selective_update,
        panel_replay_dsc_support_str(device.psr.caps.pr_dsc_support),
    ));
    if device.psr.caps.pr_dpcd_support & DP_PANEL_REPLAY_EARLY_TRANSPORT_SUPPORT != 0 {
        io.debug_output(" (Early Transport)");
    }
    io.debug_output("\n");
}

// upstream: intel_psr.c intel_psr_print_mode()
pub fn intel_psr_print_mode<I: PsrIo>(device: &PsrDevice, io: &mut I) {
    let mode = if device.psr.panel_replay_enabled && device.psr.sel_update_enabled {
        "Panel Replay Selective Update"
    } else if device.psr.panel_replay_enabled {
        "Panel Replay"
    } else if device.psr.sel_update_enabled {
        "PSR2"
    } else if device.psr.enabled {
        "PSR1"
    } else {
        ""
    };
    let enabled_status = if device.psr.enabled {
        " enabled"
    } else {
        "disabled"
    };
    let early_transport = if device.psr.su_region_et_enabled {
        " (Early Transport)"
    } else {
        ""
    };

    io.debug_output(&format!(
        "PSR mode: {mode}{enabled_status}{early_transport}\n"
    ));
    if let Some(reason) = device.psr.no_psr_reason {
        io.debug_output(&format!("  {reason}\n"));
    }
}

// upstream: intel_psr.c intel_psr_status()
pub fn intel_psr_status<I: PsrIo>(device: &PsrDevice, io: &mut I, _connector: u32) {
    intel_psr_sink_capability(device, io);
    if !device.psr.caps.sink_psr1 && !device.psr.caps.sink_panel_replay {
        return;
    }

    let runtime_pm_reference = io.runtime_pm_get();
    io.lock_psr();
    intel_psr_print_mode(device, io);

    if !device.psr.enabled {
        io.debug_output(&format!(
            "PSR sink not reliable: {}\n",
            device.psr.sink_not_reliable
        ));
        io.unlock_psr();
        io.runtime_pm_put(runtime_pm_reference);
        return;
    }

    let transcoder = device.psr.transcoder;
    let (control_register, control_value, psr2_control_value) = if device.psr.panel_replay_enabled {
        let value = io.mmio_read(PsrReg::TransDp2Control, transcoder);
        let psr2 = if device.is_edp {
            io.mmio_read(PsrReg::Psr2Control, transcoder)
        } else {
            0
        };
        (PsrReg::TransDp2Control, value, psr2)
    } else if device.psr.sel_update_enabled {
        let value = io.mmio_read(PsrReg::Psr2Control, transcoder);
        (PsrReg::Psr2Control, value, 0)
    } else {
        let value = io.mmio_read(psr_ctl_reg(device), transcoder);
        (psr_ctl_reg(device), value, 0)
    };

    let enabled = if device.psr.panel_replay_enabled {
        control_value & (1 << 0) != 0
    } else if device.psr.sel_update_enabled {
        control_value & EDP_PSR2_ENABLE != 0
    } else {
        control_value & EDP_PSR_ENABLE != 0
    };
    let _ = control_register;
    io.debug_output(&format!(
        "Source PSR/PanelReplay ctl: {} [0x{control_value:08x}]\n",
        if enabled { "enabled" } else { "disabled" }
    ));
    if device.psr.panel_replay_enabled && device.is_edp {
        io.debug_output(&format!("PSR2_CTL: 0x{psr2_control_value:08x}\n"));
    }

    psr_source_status(device, io);
    io.debug_output(&format!(
        "Busy frontbuffer bits: 0x{:08x}\n",
        device.psr.busy_frontbuffer_bits
    ));

    // Perf count resets on SKL+ when the display enters a DC state.
    let performance_counter = io.mmio_read(psr_perf_cnt_reg(device), transcoder);
    io.debug_output(&format!(
        "Performance counter: {}\n",
        performance_counter & 0x00ff_ffff
    ));

    if device.psr.debug & I915_PSR_DEBUG_IRQ != 0 {
        io.debug_output(&format!(
            "Last attempted entry at: {}\nLast exit at: {}\n",
            device.psr.last_entry_attempt_ns, device.psr.last_exit_ns,
        ));
    }

    if device.psr.sel_update_enabled {
        // PSR2_SU_STATUS is tied off on display 13 and removed on Xe2_LPD.
        if device.display_ver < 13 {
            let mut status_words = [0u32; 3];
            for first_frame in (0..8).step_by(3) {
                let index = (first_frame / 3) as u8;
                status_words[index as usize] = io.mmio_read(PsrReg::SuStatus(index), transcoder);
            }
            io.debug_output("Frame:\tPSR2 SU blocks:\n");
            for frame in 0..8usize {
                let word = status_words[frame / 3];
                let shift = (frame % 3) * 10;
                let blocks = (word >> shift) & 0x3ff;
                io.debug_output(&format!("{frame}\t{blocks}\n"));
            }
        }
        io.debug_output(&format!(
            "PSR2 selective fetch: {}\n",
            device.psr.psr2_sel_fetch_enabled
        ));
    }

    io.unlock_psr();
    io.runtime_pm_put(runtime_pm_reference);
}

// upstream: intel_psr.c i915_edp_psr_status_show()
pub fn i915_edp_psr_status_show<I: PsrIo>(
    device: &PsrDevice,
    io: &mut I,
    has_psr: bool,
    connector: u32,
) -> i32 {
    if !has_psr {
        return -19;
    }
    intel_psr_status(device, io, connector);
    0
}
// upstream: intel_psr.c i915_edp_psr_debug_set()
pub fn i915_edp_psr_debug_set<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    val: u64,
    has_psr: bool,
) -> i32 {
    if !has_psr {
        return -19;
    }
    io.drm_log(0, "Setting PSR debug", val as u32);
    intel_psr_debug_set(device, io, val)
}
// upstream: intel_psr.c i915_edp_psr_debug_get()
pub fn i915_edp_psr_debug_get(device: &PsrDevice, has_psr: bool) -> Result<u64, i32> {
    if !has_psr {
        Err(-19)
    } else {
        Ok(u64::from(device.psr.debug))
    }
}
// upstream: intel_psr.c intel_psr_debugfs_register()
pub fn intel_psr_debugfs_register<I: PsrIo>(io: &mut I, display: u32) {
    io.add_debugfs_file("i915_edp_psr_debug", display);
    io.add_debugfs_file("i915_edp_psr_status", display);
}
// upstream: intel_psr.c psr_mode_str()
pub fn psr_mode_str(device: &PsrDevice) -> &'static str {
    if device.psr.panel_replay_enabled {
        "PANEL-REPLAY"
    } else if device.psr.enabled {
        "PSR"
    } else {
        "unknown"
    }
}
// upstream: intel_psr.c i915_psr_sink_status_show()
pub fn i915_psr_sink_status_show<I: PsrIo>(device: &PsrDevice, io: &mut I, connected: bool) -> i32 {
    const SINK_STATUS: &[&str] = &[
        "inactive",
        "transition to active, capture and display",
        "active, display from RFB",
        "active, capture and display on sink device timings",
        "transition to inactive, capture and display, timing re-sync",
        "reserved",
        "reserved",
        "sink internal error",
    ];
    const DP_PSR_LINK_CRC_ERROR: u8 = 1 << 0;
    const DP_PSR_RFB_STORAGE_ERROR: u8 = 1 << 1;
    const DP_PSR_VSC_SDP_UNCORRECTABLE_ERROR: u8 = 1 << 2;

    if !(device.psr.caps.sink_psr1 && device.psr.caps.source_psr)
        && !(device.psr.caps.sink_panel_replay && device.psr.caps.source_panel_replay)
    {
        io.debug_output("PSR/Panel-Replay Unsupported\n");
        return -19; // -ENODEV
    }
    if !connected {
        return -19;
    }

    let (status, error_status) = match psr_get_status_and_error_status(device, io) {
        Ok(status) => status,
        Err(error) => return error,
    };
    let status_string = SINK_STATUS
        .get(usize::from(status & 0x7))
        .copied()
        .unwrap_or("unknown");

    io.debug_output(&format!(
        "Sink {} status: 0x{:x} [{status_string}]\n",
        psr_mode_str(device),
        status & 0x7,
    ));
    io.debug_output(&format!(
        "Sink {} error status: 0x{:x}",
        psr_mode_str(device),
        error_status,
    ));

    if error_status
        & (DP_PSR_RFB_STORAGE_ERROR | DP_PSR_VSC_SDP_UNCORRECTABLE_ERROR | DP_PSR_LINK_CRC_ERROR)
        != 0
    {
        io.debug_output(":\n");
    } else {
        io.debug_output("\n");
    }
    if error_status & DP_PSR_RFB_STORAGE_ERROR != 0 {
        io.debug_output(&format!("\t{} RFB storage error\n", psr_mode_str(device)));
    }
    if error_status & DP_PSR_VSC_SDP_UNCORRECTABLE_ERROR != 0 {
        io.debug_output(&format!(
            "\t{} VSC SDP uncorrectable error\n",
            psr_mode_str(device)
        ));
    }
    if error_status & DP_PSR_LINK_CRC_ERROR != 0 {
        io.debug_output(&format!("\t{} Link CRC error\n", psr_mode_str(device)));
    }
    0
}

// upstream: intel_psr.c i915_psr_status_show()
pub fn i915_psr_status_show<I: PsrIo>(device: &PsrDevice, io: &mut I, connector: u32) -> i32 {
    intel_psr_status(device, io, connector);
    0
}
// upstream: intel_psr.c intel_psr_connector_debugfs_add()
pub fn intel_psr_connector_debugfs_add<I: PsrIo>(
    io: &mut I,
    connector: u32,
    connector_type: u8,
    has_psr: bool,
    has_dp20: bool,
) {
    if connector_type != 0 && connector_type != 1 {
        return;
    }
    io.add_debugfs_file("i915_psr_sink_status", connector);
    if has_psr || has_dp20 {
        io.add_debugfs_file("i915_psr_status", connector)
    }
}
// upstream: intel_psr.c intel_psr_needs_alpm()
pub fn intel_psr_needs_alpm(
    device: &PsrDevice,
    has_sel_update: bool,
    has_panel_replay: bool,
) -> bool {
    device.is_edp && (has_sel_update || has_panel_replay)
}
// upstream: intel_psr.c intel_psr_needs_alpm_aux_less()
pub fn intel_psr_needs_alpm_aux_less(device: &PsrDevice, has_panel_replay: bool) -> bool {
    device.is_edp && has_panel_replay
}
// upstream: intel_psr.c intel_psr_compute_config_late()
pub fn intel_psr_compute_config_late<I: PsrIo>(
    device: &mut PsrDevice,
    io: &mut I,
    wake_lines: i32,
    vblank: i32,
    wa_18037818876: bool,
    active_pipes: u8,
) {
    let aux_less = intel_psr_needs_alpm_aux_less(device, device.psr.config.has_panel_replay);
    let needs_alpm = intel_psr_needs_alpm(
        device,
        device.psr.config.has_sel_update,
        device.psr.config.has_panel_replay,
    );
    let wake_lines = if aux_less || needs_alpm {
        wake_lines
    } else {
        0
    };

    // Guardband is final now; if wake time no longer fits, disable PR/SU
    // features. Do not recompute PSR/PSR2 fallback here: the source TODO leaves
    // this fallback for a later config pass that can use the actual guardband.
    if wake_lines != 0 && !_wake_lines_fit_into_vblank(device, vblank, wake_lines) {
        io.drm_log(
            0,
            "Adjusting PSR/PR mode: vblank too short for wake lines",
            wake_lines as u32,
        );

        if device.psr.config.has_panel_replay {
            device.psr.config.has_panel_replay = false;
            device.psr.config.has_psr = false;
        }
        device.psr.config.has_sel_update = false;
        device.psr.config.enable_psr2_su_region_et = false;
        device.psr.config.enable_psr2_sel_fetch = false;
    }

    // Wa_18037818876: avoid PSR entry setup hang on the affected platform.
    if wa_18037818876 && intel_psr_needs_wa_18037818876(device) {
        device.psr.config.has_psr = false;
        io.drm_log(0, "PSR disabled to workaround PSR FSM hang issue", 0);
    }

    intel_psr_set_non_psr_pipes(
        device,
        device.wa_16025596647,
        active_pipes,
        device.psr.config.pipe,
    );
}

// upstream: intel_psr.c intel_psr_min_guardband()
pub fn intel_psr_min_guardband(
    device: &PsrDevice,
    has_panel_replay: bool,
    has_sel_update: bool,
    set_context_latency: i32,
) -> i32 {
    if !device.is_edp {
        return 0;
    }
    let wake = if has_panel_replay {
        device.psr.config.aux_less_wake_lines
    } else if has_sel_update {
        if device.display_ver < 20 {
            psr2_block_count_lines(
                device.psr.config.io_wake_lines,
                device.psr.config.fast_wake_lines,
            ) as u8
        } else {
            device.psr.config.io_wake_lines
        }
    } else {
        return 0;
    };
    i32::from(wake) + set_context_latency + i32::from(device.psr.config.source_scanline_indication)
}
// upstream: intel_psr.c intel_psr_use_trans_push()
pub fn intel_psr_use_trans_push(device: &PsrDevice) -> bool {
    device.supports_trans_push && device.psr.config.has_psr
}
pub fn intel_psr_use_trans_push_config(config: &PsrConfig) -> bool {
    config.has_psr && config.use_trans_push
}
// upstream: intel_psr.c intel_psr_pr_async_video_timing_supported()
pub fn intel_psr_pr_async_video_timing_supported(
    device: &PsrDevice,
    pr_support: u8,
    pr_capability: u8,
) -> bool {
    pr_support & 1 != 0 && pr_capability & DP_PANEL_REPLAY_ASYNC_VIDEO_TIMING_NOT_SUPPORTED == 0
}
