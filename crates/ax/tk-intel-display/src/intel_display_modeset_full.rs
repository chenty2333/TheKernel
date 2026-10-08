// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Linux 7.2.3 i915 `intel_display.c` modeset/atomic paths for display 12/13.
//!
//! Register, DRM-core, and subsystem primitives remain explicit backend hooks;
//! sequencing, state transitions, retry policy, and error propagation live here.
extern crate alloc;

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModesetError {
    Invalid,
    NoDevice,
    Again,
    Deadlock,
    OutOfMemory,
    Backend(i32),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timing {
    pub hdisplay: u32,
    pub hsync_start: u32,
    pub hsync_end: u32,
    pub htotal: u32,
    pub hblank_start: u32,
    pub hblank_end: u32,
    pub vdisplay: u32,
    pub vblank_start: u32,
    pub vblank_end: u32,
    pub vsync_start: u32,
    pub vsync_end: u32,
    pub vtotal: u32,
    pub clock_khz: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayMode {
    pub timing: Timing,
    pub vscan: u8,
    pub hskew: bool,
    pub csync: bool,
    pub ncsync: bool,
    pub pcsync: bool,
    pub bcast: bool,
    pub pixmux: bool,
    pub clkdiv2: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeStatus {
    Ok,
    NoVscan,
    HorizontalIllegal,
    VerticalIllegal,
    HorizontalSync,
    ClockHigh,
    Bad,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdbEntry {
    pub start: u16,
    pub end: u16,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipeConfigCompareData {
    /// Same-order scalar checks expanded from PIPE_CONF_CHECK_* in the source.
    pub always_equal: [u64; 48],
    /// Checks guarded by `if (!fastset)` in the upstream comparator.
    pub full_modeset_only: [u64; 48],
    /// DP M/N fields are conditionally omitted for double-buffered M/N fastsets.
    pub dp_m_n: [u32; 5],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkMn {
    pub tu: u8,
    pub data_m: u32,
    pub data_n: u32,
    pub link_m: u32,
    pub link_n: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpVscSdp {
    pub pixelformat: u8,
    pub colorimetry: u8,
    pub bpc: u8,
    pub dynamic_range: u8,
    pub content_type: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpAsSdp {
    pub sdp_type: u8,
    pub revision: u8,
    pub length: u8,
    pub vtotal: u16,
    pub target_rr: u16,
    pub duration_incr_ms: u8,
    pub duration_decr_ms: u8,
    pub target_rr_divider: u8,
    pub mode: u8,
    pub coasting_vtotal: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipeMiscReadout {
    pub yuv420: bool,
    pub yuv420_full_blend: bool,
    pub yuv: bool,
    pub bpc: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipeMiscWriteout {
    pub bpc: u8,
    pub dither: bool,
    pub yuv: bool,
    pub yuv420: bool,
    pub yuv420_full_blend: bool,
    pub hdr_precision: bool,
    pub pixel_rounding_trunc: bool,
    pub psr_mask_sprite: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JoinerHwFlags {
    pub uncompressed_primary: bool,
    pub uncompressed_secondary: bool,
    pub big_enabled: bool,
    pub big_primary: bool,
    pub ultra_enabled: bool,
    pub ultra_primary: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimingField { HDisplay, HTotal, HBlankStart, HBlankEnd, HSyncStart, HSyncEnd,
    VDisplay, VTotal, VBlankStart, VBlankEnd, VSyncStart, VSyncEnd, ContextLatency,
    MinHblank, FrameStartDelay, Linetime, IpsLinetime, PixelMultiplier }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat { #[default] Rgb, Ycbcr444, Ycbcr420 }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EncoderKind { Ddi, Dp, Hdmi, Edp, DpMst, #[default] Other }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FormatInfo {
    pub pixel_format: u32,
    pub plane_count: u8,
}

impl Default for PipeConfigCompareData {
    fn default() -> Self {
        Self { always_equal: [0; 48], full_modeset_only: [0; 48], dp_m_n: [0; 5] }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipeState {
    pub pipe: u8,
    pub hw_enable: bool,
    pub hw_active: bool,
    pub uapi_enable: bool,
    pub uapi_active: bool,
    pub uapi_async_flip: bool,
    pub uapi_scaling_filter: u8,
    pub uapi_sharpness_strength: u16,
    pub uapi_connectors_changed: bool,
    pub needs_modeset: bool,
    pub needs_fastset: bool,
    pub mode_changed: bool,
    pub inherited: bool,
    pub is_joiner_secondary: bool,
    pub joiner_pipes: u8,
    pub port_sync_slave: bool,
    pub port_sync_master: bool,
    pub port_sync_mode: bool,
    pub master_transcoder: u8,
    pub sync_slaves_mask: u8,
    pub mst_slave: bool,
    pub mst_master_transcoder: u8,
    pub cpu_transcoder: u8,
    pub is_edp_transcoder: bool,
    pub has_pch_encoder: bool,
    pub has_dp_encoder: bool,
    pub transcoder_is_dsi: bool,
    pub has_intel_dpll: bool,
    pub dpll_hw_state: u64,
    pub pipe_active: bool,
    pub scaler_state: u64,
    pub crc_enabled: bool,
    pub wm_state: u64,
    pub has_dsc: bool,
    pub double_wide: bool,
    pub dither: bool,
    pub is_yuv_output: bool,
    pub uapi_plane_mask: u32,
    pub enabled_planes: u32,
    pub glk_scaler_wa: bool,
    pub hsw_workaround_pipe: u8,
    pub pixel_multiplier: u8,
    pub lane_count: u8,
    pub lane_lat_optim_mask: u32,
    pub min_hblank: u32,
    pub output_types: u32,
    pub framestart_delay: u32,
    pub msa_timing_delay: u32,
    pub pipe_bpp: u8,
    pub max_pipe_bpp: u8,
    pub max_requested_bpc: u8,
    pub dither_force_disable: bool,
    pub port_clock: u32,
    pub dp_m_n: LinkMn,
    pub dp_m2_n2: LinkMn,
    pub fdi_m_n: LinkMn,
    pub min_voltage_level: u8,
    pub output_format: OutputFormat,
    pub sink_format: OutputFormat,
    pub has_hdmi_sink: bool,
    pub hdmi_scrambling: bool,
    pub hdmi_high_tmds_clock_ratio: bool,
    pub enhanced_framing: bool,
    pub fec_enable: bool,
    pub update_pipe: bool,
    pub update_m_n: bool,
    pub update_lrr: bool,
    pub old_vrr_in_range: bool,
    pub vrr_in_range: bool,
    pub vrr_enabled: bool,
    pub vrr_enabling: bool,
    pub vrr_disabling: bool,
    pub vrr_flipline: i32,
    pub vrr_vmin: i32,
    pub vrr_vmax: i32,
    pub vrr_guardband: i32,
    pub vrr_pipeline_full: i32,
    pub vrr_vsync_start: i32,
    pub vrr_vsync_end: i32,
    pub cmrr_enabled: bool,
    pub cmrr_m: u64,
    pub cmrr_n: u64,
    pub set_context_latency: i32,
    pub old_set_context_latency: i32,
    pub old_timing: Timing,
    pub new_timing: Timing,
    pub pipe_src_width: u32,
    pub pipe_src_height: u32,
    pub pipe_src_x: i32,
    pub pipe_src_y: i32,
    pub splitter_enabled: bool,
    pub splitter_link_count: u8,
    pub splitter_pixel_overlap: u16,
    pub lvds_dual_link: bool,
    pub is_lvds_output: bool,
    pub psr_min_set_context_latency: i32,
    pub is_interlaced: bool,
    pub is_sdvo_output: bool,
    pub active_planes: u32,
    pub hdr_plane_mask: u32,
    pub cursor_plane_mask: u32,
    pub old_active_planes: u32,
    pub update_planes: u32,
    pub scaled_planes: u32,
    pub nv12_planes: u32,
    pub c8_planes: u32,
    pub fb_bits: u32,
    pub async_flip_planes: u32,
    pub old_async_flip_planes: u32,
    pub do_async_flip: bool,
    pub use_dsb: bool,
    pub use_flipq: bool,
    pub has_dsb_commit: bool,
    pub event_pending: bool,
    pub inherited_first_fastset: bool,
    pub color_update: bool,
    pub preload_luts: bool,
    pub update_mn: bool,
    pub update_lrr_timing: bool,
    pub has_ips: bool,
    pub has_dsb: bool,
    pub eld_changed: bool,
    pub has_lobf: bool,
    pub pch_pfit_enabled: bool,
    pub scaler_ecc_wa: bool,
    pub has_audio: bool,
    pub update_wm_post: bool,
    pub wm_need_postvbl_update: bool,
    pub pixel_rate: u32,
    pub crtc_clock: u32,
    pub linetime: u16,
    pub ips_linetime: u16,
    pub scaler_update_needed: bool,
    pub scalers_setup_needed: bool,
    pub ips_config_needed: bool,
    pub psr2_update_needed: bool,
    pub dpt_configure: bool,
    pub fbc_enabled: bool,
    pub overlay_active: bool,
    pub color_mgmt_changed: bool,
    pub encoder_mask: u32,
    pub lobf_disabling: bool,
    pub audio_disabling: bool,
    pub drrs_active: bool,
    pub ips_pre_update_wait: bool,
    pub fbc_pre_update_wait: bool,
    pub async_flip_vtd_wa: bool,
    pub nv12_wa: bool,
    pub scalerclk_wa: bool,
    pub cursorclk_wa: bool,
    pub disable_cxsr: bool,
    pub update_wm_pre: bool,
    pub async_flip_toggle_wait: bool,
    pub vrr_dcb_enabled: bool,
    pub has_psr: bool,
    pub plane_color_changed: bool,
    pub has_dsb_color: bool,
    pub uses_chained_dsb: bool,
    pub uses_gosub_dsb: bool,
    pub psr_use_trans_push: bool,
    pub modeset_power_domains: u32,
    pub enabled_power_domains: u32,
    pub pipe_power_domain: u32,
    pub transcoder_power_domain: u32,
    pub panel_fitter_power_domain: u32,
    pub encoder_power_domains: u32,
    pub audio_power_domain: u32,
    pub display_core_power_domain: u32,
    pub dsc_power_domain: u32,
    pub pch_pfit_force_thru: bool,
    pub pfit_dst_width: u32,
    pub pfit_dst_height: u32,
    pub ddb: DdbEntry,
    pub old_ddb: DdbEntry,
    pub pipe_mode: Timing,
    pub adjusted_mode: Timing,
    pub uapi_mode: Timing,
    pub hw_mode: Timing,
    pub hw_adjusted_mode: Timing,
    pub hw_scaling_filter: u8,
    pub hw_sharpness_strength: u16,
    pub uapi_color_mgmt_changed: bool,
    pub hw_color_luts: [u64; 3],
    pub uapi_color_luts: [u64; 3],
    pub hw_background_color: u32,
    pub uapi_background_color: u32,
    pub compare_data: PipeConfigCompareData,
    pub vrr_always_use_vrr_tg: bool,
    pub is_dsi_output: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipeTransition {
    pub old: PipeState,
    pub new: PipeState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneTransition {
    pub pipe: u8,
    pub id: u8,
    /// DRM plane-mask bit; unlike `id`, these bits identify DRM objects.
    pub uapi_plane_mask_bit: u32,
    pub frontbuffer_bit: u32,
    pub old_visible: bool,
    pub old_async_flip: bool,
    pub new_async_flip: bool,
    pub need_async_flip_toggle_wa: bool,
    pub async_flip_capable: bool,
    pub old_fb_exists: bool,
    pub new_fb_exists: bool,
    pub new_visible: bool,
    pub is_y_plane: bool,
    pub old_format: u32,
    pub new_format: u32,
    pub old_modifier: u64,
    pub new_modifier: u64,
    pub old_mapping_stride: u32,
    pub new_mapping_stride: u32,
    pub old_rotation: u32,
    pub new_rotation: u32,
    pub old_aux_dist: u32,
    pub new_aux_dist: u32,
    pub old_src_rect: [i32; 4],
    pub new_src_rect: [i32; 4],
    pub old_dst_rect: [i32; 4],
    pub new_dst_rect: [i32; 4],
    pub old_alpha: u16,
    pub new_alpha: u16,
    pub old_pixel_blend_mode: u8,
    pub new_pixel_blend_mode: u8,
    pub old_color_encoding: u8,
    pub new_color_encoding: u8,
    pub old_color_range: u8,
    pub new_color_range: u8,
    pub old_decrypt: bool,
    pub new_decrypt: bool,
    pub fence_pending: bool,
    pub clear_color_plane: bool,
    pub clear_color_value: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EncoderTransition {
    pub encoder_id: u8,
    pub old_crtc: Option<u8>,
    pub new_crtc: Option<u8>,
    pub hook_mask: u16,
    pub output_type: u32,
    pub edid_bpc: u8,
    pub max_bpc: u8,
    pub max_requested_bpc: u8,
    pub aux_channel: u8,
    pub tbt_alt_mode: bool,
    pub tbt_aux_power_domain: u32,
    pub legacy_aux_power_domain: u32,
    pub kind: EncoderKind,
    pub port: u8,
    pub has_best_encoder: bool,
    pub encoder_type_bit: u32,
    pub cloneable_types: u32,
    pub possible_crtcs_mask: u8,
    pub possible_clones_mask: u32,
    pub has_get_config_hook: bool,
    pub dedicated_external: bool,
}

pub const ENCODER_HOOK_PRE_PLL: u16 = 1 << 0;
pub const ENCODER_HOOK_PRE_ENABLE: u16 = 1 << 1;
pub const ENCODER_HOOK_ENABLE: u16 = 1 << 2;
pub const ENCODER_HOOK_DISABLE: u16 = 1 << 3;
pub const ENCODER_HOOK_POST_DISABLE: u16 = 1 << 4;
pub const ENCODER_HOOK_POST_PLL_DISABLE: u16 = 1 << 5;
pub const ENCODER_HOOK_UPDATE_PIPE: u16 = 1 << 6;
pub const ENCODER_HOOK_AUDIO_ENABLE: u16 = 1 << 7;
pub const ENCODER_HOOK_AUDIO_DISABLE: u16 = 1 << 8;
pub const ENCODER_HOOK_COMPUTE_CONFIG: u16 = 1 << 9;
pub const ENCODER_HOOK_COMPUTE_CONFIG_LATE: u16 = 1 << 10;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AtomicState {
    /// Entries are in DRM atomic-state iteration order; reverse loops use its reverse.
    pub pipes: Vec<PipeTransition>,
    pub planes: Vec<PlaneTransition>,
    pub encoders: Vec<EncoderTransition>,
    pub modeset: bool,
    pub internal: bool,
    pub nonblock: bool,
    pub legacy_cursor_update: bool,
    pub wakeref: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayCaps {
    pub display_version: u8,
    pub display_version_x100: u16,
    pub has_display: bool,
    pub haswell: bool,
    pub broadwell: bool,
    pub has_double_buffered_m_n: bool,
    pub has_dpt: bool,
    pub has_dpll_manager: bool,
    pub has_double_buffered_lut: bool,
    pub has_ips: bool,
    pub has_gmch: bool,
    pub has_dsb: bool,
    pub supports_flipq: bool,
    pub has_lrr: bool,
    pub i830: bool,
    pub has_ddi: bool,
    pub has_vrr: bool,
    pub has_pch_split: bool,
    pub cherryview: bool,
    pub g4x: bool,
    pub valleyview: bool,
    pub gen5_to7: bool,
    pub dg2: bool,
    pub alder_lake_s: bool,
    pub alder_lake_p: bool,
    pub tiger_lake: bool,
    pub dgfx: bool,
    pub wa_14010547955: bool,
    pub pipe_mask: u8,
    pub port_mask: u16,
    pub edp_transcoder_bit: u8,
    pub dsi_transcoder_mask: u8,
    pub has_ultrajoiner: bool,
    pub has_uncompressed_joiner: bool,
    pub has_bigjoiner: bool,
    pub cdclk_max_dotclock: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    DriverAccess,
    VrrCheckModeset,
    FrameworkCheckModeset,
    AsyncFlipUapiCheck,
    JoinerAddAffectedCrtcs,
    FdiAddAffectedCrtcs,
    CopyUapiToHwNoModeset,
    PrepareClearedState,
    AllocateCrtcScratchState,
    FreeCrtcHwState,
    ClearDpTunnelStreamBandwidth,
    FreeCrtcScratchState,
    ConfigurePipe,
    ConfigurePipeLate,
    CopyJoinerNoModeset,
    CheckJoiner,
    JoinerAdjustPipeSrc,
    DpllRelease,
    DpllGet,
    ColorCheck,
    CheckDigitalPortConflicts,
    PlaneAtomicCheck,
    ComputeMinCdclk,
    ComputeGlobalWatermarks,
    WatermarkCompute,
    ScalerUpdate,
    ScalerSetup,
    IpsCompute,
    Psr2SelFetchUpdate,
    BandwidthAtomicCheck,
    CdclkAtomicCheck,
    HaswellModeSetPlanesWorkaround,
    PmdemandAtomicCheck,
    CrtcAtomicCheck,
    FbcAtomicCheck,
    AsyncFlipHardwareCheck,
    ColorAssertLuts,
    DumpCrtcState,
    LinkBandwidthInit,
    LinkBandwidthReduceLimit,
    LinkBandwidthAtomicCheck,
    PreparePlanes,
    SetupCommit,
    SetupGlobalStateCommit,
    SwapFrameworkState,
    SwapGlobalState,
    SwapDpllState,
    TrackFramebuffers,
    DsbPrepare,
    PrepareDsb,
    DsbDmcHalt,
    DsbDmcUnhalt,
    DsbVblankEvade,
    DsbFrameChange,
    DsbWaitVblanks,
    DsbVrrPush,
    DsbPsrWaitIdle,
    DsbChain,
    DsbGosub,
    DsbInterrupt,
    DsbCleanup,
    ColorPrepareCommit,
    ColorWaitCommit,
    ColorCleanupCommit,
    FenceWaitPlane,
    FencePutPlane,
    TrackFramebufferPlane,
    ReadPlaneClearColor,
    CleanupPlanes,
    CommitCleanupDone,
    AtomicCommitPut,
    FenceWait,
    DisplayTdFlush,
    PreparePlaneClearColors,
    FbcPrepareDirtyRect,
    DsbFinish,
    WaitAtomicDependencies,
    WaitMstDependencies,
    WaitGlobalStateDependencies,
    AcquireDcOffPower,
    GetCrtcPowerDomains,
    AcquirePowerDomain,
    ReleasePowerDomain,
    PrePlaneUpdate,
    IpsPreUpdate,
    PsrPrePlaneUpdate,
    AlpmLobfDisable,
    VrrDisable,
    ResetVrrDcb,
    AudioDisable,
    DrrsDeactivate,
    FbcPreUpdate,
    AsyncFlipVtdWaEnable,
    Nv12WaEnable,
    ScalerClockGatingWaEnable,
    CursorClockGatingWaEnable,
    DisableCxsr,
    InitialWatermarksCheck,
    UpdateIntermediateWatermarks,
    AsyncFlipDisableWorkaround,
    DisablePlanes,
    FlushVblankWork,
    DisablePipeCrc,
    PsrPipeChange,
    DisableCrtc,
    MarkPipeInactive,
    MarkPipeActive,
    DisableFbc,
    InitialWatermarks,
    AllocateDpTunnelBandwidth,
    SetCrtcConfig,
    DpllClockCompute,
    ComputePipePixelRate,
    ComputeVrrGuardband,
    IlkFdiComputeConfig,
    PmdemandPrePlaneUpdate,
    UpdateLegacyModesetState,
    SetCdclkPrePlaneUpdate,
    VerifyDisabledModeset,
    SagvPrePlaneUpdate,
    SendDisabledPipeEvent,
    EncodersUpdatePrepare,
    DbufPrePlaneUpdate,
    EnableFlipDone,
    EnableCrtc,
    DpkgcProgramLatency,
    WaitVblankWorkers,
    WaitFlipDone,
    DisableFlipDone,
    DsbWaitCommit,
    VrrCheckPushSent,
    DisableFlipq,
    EnableCpuFifoUnderrunReporting,
    EnablePchFifoUnderrunReporting,
    OptimizeWatermarks,
    DbufPostPlaneUpdate,
    PostPlaneUpdate,
    PutCrtcPowerDomains,
    VerifyCrtc,
    ReadoutPipePowerDomain,
    ReadoutPanelFitterPowerDomain,
    ReadoutPowerDomainsPutAll,
    DscGetConfig,
    VrrGetConfig,
    ColorGetConfig,
    SkylakeScalerGetConfig,
    IlkPfitGetConfig,
    IpsGetConfig,
    ReadHswLinetime,
    PostPlaneUpdateAfterReadout,
    FrontbufferFlip,
    FbcPostUpdate,
    AsyncFlipVtdWaDisable,
    Nv12WaDisable,
    ScalerClockGatingWaDisable,
    CursorClockGatingWaDisable,
    ColorPostUpdate,
    AudioEnable,
    UnmaskScalerEcc,
    AlpmLobfEnable,
    PsrPostPlaneUpdate,
    HswIpsPostUpdate,
    DrrsActivate,
    TransferDsbOwnership,
    CheckCpuFifoUnderruns,
    CheckPchFifoUnderruns,
    VerifyPlanes,
    VerifyPlaneEnabled,
    VerifyPlaneDisabled,
    SagvPostPlaneUpdate,
    SetCdclkPostPlaneUpdate,
    PmdemandPostPlaneUpdate,
    CommitHardwareDone,
    CommitGlobalStateDone,
    ArmUnclaimedMmioDetection,
    DelayedDcOffPowerPut,
    DelayedDcOffPowerPut17ms,
    RuntimePowerPut,
    QueueCleanupWork,
    AllocateAtomicState,
    AcquireModesetLocks,
    AddAffectedPlanes,
    AddDpTunnelState,
    AddMstTopologyState,
    UnlockPpsRegisters,
    DdiInitEncoder,
    EncoderGetConfigHook,
    EncoderCountMismatch,
    DisableOverlay,
    ComputeEncoderPossibleCrtcs,
    ComputeEncoderPossibleClones,
    InitPchRefclk,
    MovePanelConnectorsHead,
    ForceColorManagementUpdate,
    AddAffectedConnectors,
    ClearAtomicState,
    ModesetBackoff,
    CommitState,
    PutAtomicState,
    DropModesetLocks,
    RuntimePowerGet,
    UnpreparePlanes,
    QueueModesetWork,
    QueueFlipWork,
    FlushModesetWork,
    AtomicCommitGet,
    SetPipeSourceSize,
    EnableSkylakePfit,
    EnableIlkPfit,
    DisableIlkPfit,
    SetHswLinetimeWatermark,
    SetTranscoderM1N1,
    SetTranscoderM2N2,
    SetTranscoderTimings,
    WriteContextLatency,
    WriteVsyncShift,
    WriteTranscoderHtotal,
    WriteTranscoderHblank,
    WriteTranscoderHsync,
    WriteTranscoderVtotal,
    WriteTranscoderVblank,
    WriteTranscoderVsync,
    WriteEdpVtotal,
    WriteMinHblank,
    WriteDataM,
    WriteDataN,
    WriteLinkM,
    WriteLinkN,
    SetVrrFixedRefreshTimings,
    EnableVrrTranscoder,
    SetTranscoderMultiplier,
    SetFrameStartDelay,
    SetTransconf,
    SetTranscoderLrrTimings,
    ColorCommitArm,
    SetPipeMisc,
    ProgramPsr2Tracking,
    UpdateWatermarks,
    DetachScalers,
    LoadColorLuts,
    EnableVrr,
    UpdateActiveTimings,
    IncrementVrrFlipCount,
    ArmFifoUnderrun,
    ConfigureDpt,
    PreloadColorLuts,
    EncoderUpdatePipe,
    SetIclPipeChicken,
    SetVrrTranscoderTimings,
    UpdateFbc,
    ColorCommitNoarm,
    PlanesUpdateNoarm,
    FlipqEnable,
    PrepareVblankEvent,
    FlipqAdd,
    DsbCommit,
    PipeUpdateStart,
    PlanesUpdateArm,
    PipeUpdateEnd,
    DisableColorScaler,
    DisableColorLuts,
    SetPipeCrcEnabled,
    WaitNextVblank,
    EnableDmcPipe,
    EncoderPrePllHook,
    EncoderPreEnableHook,
    EncoderEnableHook,
    EncoderDisableHook,
    EncoderPostDisableHook,
    EncoderPostPllDisableHook,
    EncoderUpdatePipeHook,
    EncoderAudioEnableHook,
    EncoderAudioDisableHook,
    EncoderOutputTypeHook,
    EncoderComputeConfigHook,
    EncoderComputeConfigLateHook,
    InitializeAdjustedMode,
    OpRegionEnable,
    OpRegionDisable,
    EncodersPrePllEnable,
    EnableDpll,
    EncodersPreEnable,
    EnableDsc,
    EnableUncompressedJoiner,
    ColorModeset,
    PipeScalerClockGatingEnable,
    PipeScalerClockGatingDisable,
    EncodersEnable,
    EncodersDisable,
    EncodersPostDisable,
    DisableDpll,
    EncodersPostPllDisable,
    DisableDmcPipe,
    DisableTranscoder,
    TranscoderEnabled,
    PipeOffWaitTimedOut,
    NeedsScanlineWait,
    WaitPipeOff100ms,
    WarnPipeOffTimeout,
    WaitScanlineMoving,
    WaitScanlineStopped,
    WarnTranscoderAlreadyEnabled,
    ClearTranscoderFecStall,
    DisablePlaneArm,
    SkylakeWmPlaneDisableNoatomic,
    DisableIps,
    EnablePlaneFlipDone,
    DisablePlaneFlipDone,
    AsyncFlipToggleDisable,
}

/// Framework/register actions are hooks; ordering and policy stay in translated code.
pub trait ModesetOps {
    fn action(&mut self, action: Action, pipe: Option<u8>) -> Result<(), ModesetError>;
    fn action_value(&mut self, action: Action, pipe: u8, value: u32) -> Result<(), ModesetError>;
    fn action_pair(&mut self, action: Action, pipe: u8, first: u32, second: u32) -> Result<(), ModesetError>;
    fn write_transconf(&mut self, pipe: u8, enable: bool, interlaced: bool,
        dither: bool, yuv_output: bool) -> Result<(), ModesetError>;
    fn update_transcoder_config(&mut self, pipe: u8, enable: Option<bool>,
        clear_double_wide: bool, dsc_pixel_count_x4: Option<bool>) -> Result<(), ModesetError>;
    fn dp_link_symbol_clock(&mut self, link_clock: u32) -> u32;
    fn dp_link_symbol_size(&mut self, link_freq: u32) -> u32;
    fn dp_effective_data_rate(&mut self, pixel_clock: u32, bpp_x16: u16, overhead_ppm: u32) -> u32;
    fn dp_max_dprx_data_rate(&mut self, link_clock: u32, lanes: u8) -> u32;
    fn read_link_m_n(&mut self, pipe: u8, transcoder: u8, index: u8) -> Result<LinkMn, ModesetError>;
    fn read_timing_field(&mut self, pipe: u8, transcoder: u8, field: TimingField) -> Result<u32, ModesetError>;
    fn read_pipe_src_size(&mut self, pipe: u8) -> Result<(u32, u32), ModesetError>;
    fn read_pipe_chicken(&mut self, pipe: u8) -> Result<u32, ModesetError>;
    fn write_pipe_chicken(&mut self, pipe: u8, value: u32) -> Result<(), ModesetError>;
    fn read_transcoder_interlaced(&mut self, pipe: u8, hsw_mask: bool) -> Result<bool, ModesetError>;
    fn read_pipe_misc(&mut self, pipe: u8) -> Result<PipeMiscReadout, ModesetError>;
    fn hsw_get_transcoder_state(&mut self, state: &mut PipeState) -> Result<bool, ModesetError>;
    fn plane_can_async_flip(&mut self, plane: u8, format: u32, modifier: u64) -> bool;
    fn write_pipe_misc(&mut self, pipe: u8, value: PipeMiscWriteout) -> Result<(), ModesetError>;
    fn read_joiner_hw_flags(&mut self, pipe: u8) -> Result<JoinerHwFlags, ModesetError>;
    fn plane_action(&mut self, action: Action, pipe: u8, plane: u8) -> Result<(), ModesetError>;
    fn encoder_action(&mut self, action: Action, pipe: u8, encoder: u8) -> Result<(), ModesetError>;
    fn prepare_dsb(&mut self, pipe: u8, size: u32) -> Result<bool, ModesetError>;
    fn read_plane_clear_color(&mut self, pipe: u8, plane: u8) -> Result<Option<u64>, ModesetError>;
    fn action_condition(&mut self, action: Action, pipe: u8) -> Result<bool, ModesetError>;
    fn framework_check_modeset(&mut self, state: &mut AtomicState) -> Result<(), ModesetError>;
    fn initial_fastset_check_failed(&mut self, pipe: u8) -> bool;
    fn link_m_n_equal(&mut self, old: &PipeState, new: &PipeState) -> bool;
    fn reduce_link_bpp_limit(&mut self, failed_pipe: u8) -> bool;
    fn cdclk_khz(&mut self, pipe: u8) -> Result<u32, ModesetError>;
    fn promote_external_modeset_dependency(&mut self, state: &AtomicState, index: usize) -> bool {
        let new = &state.pipes[index].new;
        (new.mst_slave && intel_cpu_transcoders_need_modeset(state, pipe_bit(new.mst_master_transcoder))) ||
            (new.port_sync_mode && intel_cpu_transcoders_need_modeset(state,
                new.sync_slaves_mask | if new.master_transcoder != u8::MAX { pipe_bit(new.master_transcoder) } else { 0 })) ||
            (new.joiner_pipes != 0 && intel_pipes_need_modeset(state, new.joiner_pipes))
    }
    fn mst_crtc_needs_modeset(&mut self, _state: &AtomicState, _index: usize) -> bool { false }
    fn ddb_overlaps(&mut self, candidate: DdbEntry, entries: &[DdbEntry], pipe: u8) -> bool {
        entries.iter().enumerate().any(|(idx, e)| {
            idx != usize::from(pipe) && e.enabled && candidate.enabled &&
                candidate.start < e.end && e.start < candidate.end
        })
    }
}

fn pipe_bit(pipe: u8) -> u8 { 1u8.checked_shl(u32::from(pipe)).unwrap_or(0) }
fn joined_mask(state: &PipeState) -> u8 {
    pipe_bit(state.pipe) | state.joiner_pipes
}
fn needs_modeset(state: &PipeState) -> bool { state.needs_modeset }
fn needs_fastset(state: &PipeState) -> bool { state.needs_fastset }
// upstream: intel_display.c is_hdr_mode()
pub fn is_hdr_mode(state: &PipeState) -> bool {
    state.active_planes & !(state.hdr_plane_mask | state.cursor_plane_mask) == 0
}

// upstream: intel_display.c is_trans_port_sync_slave()
pub fn is_trans_port_sync_slave(state: &PipeState) -> bool {
    state.master_transcoder != u8::MAX
}

// upstream: intel_display.c is_trans_port_sync_master()
pub fn is_trans_port_sync_master(state: &PipeState) -> bool {
    state.sync_slaves_mask != 0
}

// upstream: intel_display.c is_trans_port_sync_mode()
pub fn is_trans_port_sync_mode(state: &PipeState) -> bool {
    is_trans_port_sync_master(state) || is_trans_port_sync_slave(state)
}

// upstream: intel_display.c joiner_primary_pipe()
pub fn joiner_primary_pipe(state: &PipeState) -> u8 {
    if state.joiner_pipes == 0 { u8::MAX } else { state.joiner_pipes.trailing_zeros() as u8 }
}

// upstream: intel_display.c is_bigjoiner()
pub fn is_bigjoiner(state: &PipeState) -> bool {
    state.joiner_pipes.count_ones() >= 2
}

// upstream: intel_display.c bigjoiner_primary_pipes()
pub fn bigjoiner_primary_pipes(state: &PipeState) -> u8 {
    if !is_bigjoiner(state) { return 0; }
    state.joiner_pipes & (0b0101_0101u16.checked_shl(u32::from(joiner_primary_pipe(state))).unwrap_or(0) as u8)
}

// upstream: intel_display.c bigjoiner_secondary_pipes()
pub fn bigjoiner_secondary_pipes(state: &PipeState) -> u8 {
    if !is_bigjoiner(state) { return 0; }
    state.joiner_pipes & (0b1010_1010u16.checked_shl(u32::from(joiner_primary_pipe(state))).unwrap_or(0) as u8)
}

// upstream: intel_display.c intel_crtc_is_bigjoiner_primary()
pub fn intel_crtc_is_bigjoiner_primary(state: &PipeState) -> bool {
    is_bigjoiner(state) && bigjoiner_primary_pipes(state) & pipe_bit(state.pipe) != 0
}

// upstream: intel_display.c intel_crtc_is_bigjoiner_secondary()
pub fn intel_crtc_is_bigjoiner_secondary(state: &PipeState) -> bool {
    is_bigjoiner(state) && bigjoiner_secondary_pipes(state) & pipe_bit(state.pipe) != 0
}

// upstream: intel_display.c _intel_modeset_primary_pipes()
pub fn intel_modeset_primary_pipes(state: &PipeState) -> u8 {
    if !is_bigjoiner(state) { pipe_bit(state.pipe) } else { bigjoiner_primary_pipes(state) }
}

// upstream: intel_display.c _intel_modeset_secondary_pipes()
pub fn intel_modeset_secondary_pipes(state: &PipeState) -> u8 {
    bigjoiner_secondary_pipes(state)
}

// upstream: intel_display.c intel_crtc_is_ultrajoiner()
pub fn intel_crtc_is_ultrajoiner(state: &PipeState) -> bool {
    intel_crtc_joined_pipe_mask(state).count_ones() >= 4
}

// upstream: intel_display.c ultrajoiner_primary_pipes()
pub fn ultrajoiner_primary_pipes(state: &PipeState) -> u8 {
    if !intel_crtc_is_ultrajoiner(state) { return 0; }
    state.joiner_pipes & (0b0001_0001u16.checked_shl(u32::from(joiner_primary_pipe(state))).unwrap_or(0) as u8)
}

// upstream: intel_display.c intel_crtc_is_ultrajoiner_primary()
pub fn intel_crtc_is_ultrajoiner_primary(state: &PipeState) -> bool {
    intel_crtc_is_ultrajoiner(state) && ultrajoiner_primary_pipes(state) & pipe_bit(state.pipe) != 0
}

// upstream: intel_display.c ultrajoiner_enable_pipes()
pub fn ultrajoiner_enable_pipes(state: &PipeState) -> u8 {
    if !intel_crtc_is_ultrajoiner(state) { return 0; }
    state.joiner_pipes & (0b0111_0111u16.checked_shl(u32::from(joiner_primary_pipe(state))).unwrap_or(0) as u8)
}

// upstream: intel_display.c intel_crtc_ultrajoiner_enable_needed()
pub fn intel_crtc_ultrajoiner_enable_needed(state: &PipeState) -> bool {
    intel_crtc_is_ultrajoiner(state) && ultrajoiner_enable_pipes(state) & pipe_bit(state.pipe) != 0
}

// upstream: intel_display.c intel_crtc_joiner_secondary_pipes()
pub fn intel_crtc_joiner_secondary_pipes(state: &PipeState) -> u8 {
    if state.joiner_pipes == 0 { 0 } else { state.joiner_pipes & !pipe_bit(joiner_primary_pipe(state)) }
}

// upstream: intel_display.c intel_crtc_is_joiner_secondary()
pub fn intel_crtc_is_joiner_secondary(state: &PipeState) -> bool {
    state.joiner_pipes != 0 && state.pipe != joiner_primary_pipe(state)
}

// upstream: intel_display.c intel_crtc_is_joiner_primary()
pub fn intel_crtc_is_joiner_primary(state: &PipeState) -> bool {
    state.joiner_pipes != 0 && state.pipe == joiner_primary_pipe(state)
}

// upstream: intel_display.c intel_crtc_num_joined_pipes()
pub fn intel_crtc_num_joined_pipes(state: &PipeState) -> u8 {
    intel_crtc_joined_pipe_mask(state).count_ones() as u8
}

// upstream: intel_display.c intel_crtc_joined_pipe_mask()
pub fn intel_crtc_joined_pipe_mask(state: &PipeState) -> u8 {
    pipe_bit(state.pipe) | state.joiner_pipes
}

// upstream: intel_display.c intel_primary_crtc()
pub fn intel_primary_crtc_pipe(state: &PipeState) -> u8 {
    if intel_crtc_is_joiner_secondary(state) { joiner_primary_pipe(state) } else { state.pipe }
}

// The following predicates retain upstream's enable/disable edge semantics.

// upstream: intel_display.c intel_wait_for_pipe_off()
pub fn intel_wait_for_pipe_off<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if caps.display_version >= 4 {
        ops.action(Action::WaitPipeOff100ms, Some(state.pipe))?;
        if ops.action_condition(Action::PipeOffWaitTimedOut, state.pipe)? {
            ops.action(Action::WarnPipeOffTimeout, Some(state.pipe))?;
        }
    } else {
        ops.action(Action::WaitScanlineStopped, Some(state.pipe))?;
    }
    Ok(())
}

// upstream: intel_display.c intel_enable_transcoder()
pub fn intel_enable_transcoder<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::VerifyPlanes, Some(state.pipe))?;
    if ops.action_condition(Action::TranscoderEnabled, state.pipe)? {
        if !caps.i830 { ops.action(Action::WarnTranscoderAlreadyEnabled, Some(state.pipe))?; }
        return Ok(());
    }
    let dsc_x4 = caps.display_version >= 13 && state.has_dsc;
    ops.update_transcoder_config(state.pipe, Some(true), false,
        if dsc_x4 { Some(true) } else { None })?;
    if ops.action_condition(Action::NeedsScanlineWait, state.pipe)? {
        ops.action(Action::WaitScanlineMoving, Some(state.pipe))?;
    }
    Ok(())
}

// upstream: intel_display.c intel_disable_transcoder()
pub fn intel_disable_transcoder<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::VerifyPlanes, Some(state.pipe))?;
    if !ops.action_condition(Action::TranscoderEnabled, state.pipe)? { return Ok(()); }
    let keep_pipe_enabled = caps.i830;
    let dsc_x4 = caps.display_version >= 13 && state.has_dsc;
    ops.update_transcoder_config(
        state.pipe,
        if keep_pipe_enabled { None } else { Some(false) },
        state.double_wide,
        if dsc_x4 { Some(false) } else { None },
    )?;
    if caps.display_version >= 12 {
        ops.action(Action::ClearTranscoderFecStall, Some(state.pipe))?;
    }
    if !keep_pipe_enabled { intel_wait_for_pipe_off(caps, state, ops)?; }
    Ok(())
}

// upstream: intel_display.c intel_plane_fb_max_stride()
pub fn intel_plane_fb_max_stride<F>(
    caps: DisplayCaps,
    info: Option<&FormatInfo>,
    modifier: u64,
    mut primary_plane_max_stride: F,
) -> u32
where
    F: FnMut(u8, &FormatInfo, u64, u32) -> u32,
{
    let Some(info) = info else { return 0; };
    let Some(pipe) = (0..8).find(|pipe| caps.pipe_mask & pipe_bit(*pipe) != 0) else {
        return 0;
    };

    // The upstream query deliberately uses the primary plane of the first
    // available CRTC, which is assumed to have the largest supported stride.
    primary_plane_max_stride(pipe, info, modifier, 1 /* DRM_MODE_ROTATE_0 */)
}

// upstream: intel_display.c intel_dumb_fb_max_stride()
pub fn intel_dumb_fb_max_stride<F>(
    caps: DisplayCaps,
    info: Option<&FormatInfo>,
    modifier: u64,
    primary_plane_max_stride: F,
) -> u32
where
    F: FnMut(u8, &FormatInfo, u64, u32) -> u32,
{
    if !caps.has_display { return 0; }
    intel_plane_fb_max_stride(caps, info, modifier, primary_plane_max_stride)
}

// upstream: intel_display.c intel_set_plane_visible()
pub fn intel_set_plane_visible(
    crtc_state: &mut PipeState,
    plane_state: &mut PlaneTransition,
    visible: bool,
) {
    plane_state.new_visible = visible;
    if visible {
        crtc_state.uapi_plane_mask |= plane_state.uapi_plane_mask_bit;
    } else {
        crtc_state.uapi_plane_mask &= !plane_state.uapi_plane_mask_bit;
    }
}

// upstream: intel_display.c intel_plane_fixup_bitmasks()
pub fn intel_plane_fixup_bitmasks(crtc_state: &mut PipeState, planes: &[PlaneTransition]) {
    crtc_state.enabled_planes = 0;
    crtc_state.active_planes = 0;
    for plane in planes {
        if plane.pipe != crtc_state.pipe ||
            crtc_state.uapi_plane_mask & plane.uapi_plane_mask_bit == 0
        {
            continue;
        }
        let bit = 1u32.checked_shl(u32::from(plane.id)).unwrap_or(0);
        crtc_state.enabled_planes |= bit;
        crtc_state.active_planes |= bit;
    }
}

// upstream: intel_display.c intel_plane_disable_noatomic()
pub fn intel_plane_disable_noatomic<O: ModesetOps>(
    crtc_state: &mut PipeState,
    plane_state: &mut PlaneTransition,
    planes: &[PlaneTransition],
    caps: DisplayCaps,
    ops: &mut O,
) -> Result<(), ModesetError> {
    if plane_state.pipe != crtc_state.pipe { return Err(ModesetError::Invalid); }

    // intel_plane_set_invisible() drops per-plane derived masks before the
    // UAPI mask is updated and the CRTC bitmasks are rebuilt.
    let bit = 1u32.checked_shl(u32::from(plane_state.id)).unwrap_or(0);
    crtc_state.active_planes &= !bit;
    crtc_state.scaled_planes &= !bit;
    crtc_state.nv12_planes &= !bit;
    crtc_state.c8_planes &= !bit;
    crtc_state.async_flip_planes &= !bit;
    intel_set_plane_visible(crtc_state, plane_state, false);
    intel_plane_fixup_bitmasks(crtc_state, planes);

    ops.plane_action(Action::SkylakeWmPlaneDisableNoatomic, crtc_state.pipe, plane_state.id)?;

    let cursor_bit = 1u32 << 7; // PLANE_CURSOR
    if crtc_state.active_planes & !cursor_bit == 0 && crtc_state.has_ips &&
        ops.action_condition(Action::DisableIps, crtc_state.pipe)?
    {
        ops.action(Action::DisableIps, Some(crtc_state.pipe))?;
        crtc_state.has_ips = false;
        ops.action(Action::WaitNextVblank, Some(crtc_state.pipe))?;
    }

    if caps.has_gmch && ops.action_condition(Action::DisableCxsr, crtc_state.pipe)? {
        ops.action(Action::DisableCxsr, Some(crtc_state.pipe))?;
        ops.action(Action::WaitNextVblank, Some(crtc_state.pipe))?;
    }

    // Gen2 underrun-reporting workaround is intentionally outside the
    // display 12/13 contract; the common arm-and-vblank sequence is retained.
    ops.plane_action(Action::DisablePlaneArm, crtc_state.pipe, plane_state.id)?;
    ops.action(Action::WaitNextVblank, Some(crtc_state.pipe))?;
    Ok(())
}

// upstream: intel_display.c intel_plane_fence_y_offset()
pub fn intel_plane_fence_y_offset<F>(
    plane_state: &PlaneTransition,
    color_plane0_offset: u32,
    mut adjust_aligned_offset: F,
) -> u32
where
    F: FnMut(&mut i32, &mut i32, &PlaneTransition, u8, u32, u32) -> u32,
{
    let (mut x, mut y) = (0, 0);
    let _offset = adjust_aligned_offset(
        &mut x, &mut y, plane_state, 0, color_plane0_offset, 0,
    );
    y as u32
}

// upstream: intel_display.c icl_set_pipe_chicken()
pub fn icl_set_pipe_chicken<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    const PER_PIXEL_ALPHA_BYPASS_EN: u32 = 1 << 7;
    const DG2_RENDER_CCSTAG_4_3_EN: u32 = 1 << 12;
    const PIXEL_ROUNDING_TRUNC_FB_PASSTHRU: u32 = 1 << 15;
    const UNDERRUN_RECOVERY_BIT: u32 = 1 << 30;
    let mut value = ops.read_pipe_chicken(state.pipe)?;
    value |= PER_PIXEL_ALPHA_BYPASS_EN | PIXEL_ROUNDING_TRUNC_FB_PASSTHRU;
    if caps.dg2 {
        value &= !UNDERRUN_RECOVERY_BIT;
    } else if caps.display_version >= 13 && caps.display_version < 30 {
        value |= UNDERRUN_RECOVERY_BIT;
    }
    if caps.wa_14010547955 { value |= DG2_RENDER_CCSTAG_4_3_EN; }
    ops.write_pipe_chicken(state.pipe, value)
}
// upstream: intel_display.c intel_get_crtc_new_encoder()
pub fn intel_get_crtc_new_encoder<O: ModesetOps>(
    state: &AtomicState, crtc: &PipeState, ops: &mut O,
) -> Result<Option<u8>, ModesetError> {
    let primary_pipe = intel_primary_crtc_pipe(crtc);
    let mut result = None;
    let mut count = 0u32;
    for encoder in state.encoders.iter().filter(|e| e.new_crtc == Some(primary_pipe)) {
        result = Some(encoder.encoder_id);
        count += 1;
    }
    if count != 1 { ops.action(Action::EncoderCountMismatch, Some(primary_pipe))?; }
    Ok(result)
}

// upstream: intel_display.c intel_crtc_dpms_overlay_disable()
pub fn intel_crtc_dpms_overlay_disable<O: ModesetOps>(
    state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.overlay_active { ops.action(Action::DisableOverlay, Some(state.pipe))?; }
    Ok(())
}

// upstream: intel_display.c intel_encoders_audio_enable()
pub fn intel_encoders_audio_enable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.new_crtc == Some(pipe) && encoder.hook_mask & ENCODER_HOOK_AUDIO_ENABLE != 0 {
            encoder_hook(ops, Action::EncoderAudioEnableHook, pipe, encoder)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_encoders_audio_disable()
pub fn intel_encoders_audio_disable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.old_crtc == Some(pipe) && encoder.hook_mask & ENCODER_HOOK_AUDIO_DISABLE != 0 {
            encoder_hook(ops, Action::EncoderAudioDisableHook, pipe, encoder)?;
        }
    }
    Ok(())
}

const DATA_LINK_M_N_MASK: u32 = 0x00ff_ffff;
const DATA_LINK_N_MAX: u32 = 0x0080_0000;

// upstream: intel_display.c planes_enabling()
pub fn planes_enabling(old: &PipeState, new: &PipeState) -> bool {
    new.hw_active && (old.active_planes == 0 || new.needs_modeset) && new.active_planes != 0
}

// upstream: intel_display.c planes_disabling()
pub fn planes_disabling(old: &PipeState, new: &PipeState) -> bool {
    old.hw_active && old.active_planes != 0 && (new.active_planes == 0 || new.needs_modeset)
}

// upstream: intel_display.c vrr_params_changed()
pub fn vrr_params_changed(old: &PipeState, new: &PipeState) -> bool {
    old.vrr_flipline != new.vrr_flipline || old.vrr_vmin != new.vrr_vmin ||
        old.vrr_vmax != new.vrr_vmax || old.vrr_guardband != new.vrr_guardband ||
        old.vrr_pipeline_full != new.vrr_pipeline_full ||
        old.vrr_vsync_start != new.vrr_vsync_start ||
        old.vrr_vsync_end != new.vrr_vsync_end
}

// upstream: intel_display.c cmrr_params_changed()
pub fn cmrr_params_changed(old: &PipeState, new: &PipeState) -> bool {
    old.cmrr_m != new.cmrr_m || old.cmrr_n != new.cmrr_n
}

// upstream: intel_display.c intel_crtc_vrr_enabling()
pub fn intel_crtc_vrr_enabling(old: &PipeState, new: &PipeState) -> bool {
    new.hw_active && (((!old.vrr_enabled || new.needs_modeset) && new.vrr_enabled) ||
        (new.vrr_enabled && (new.update_m_n || new.update_lrr || vrr_params_changed(old, new))))
}

// upstream: intel_display.c intel_crtc_vrr_disabling()
pub fn intel_crtc_vrr_disabling(old: &PipeState, new: &PipeState) -> bool {
    old.hw_active && ((old.vrr_enabled && (!new.vrr_enabled || new.needs_modeset)) ||
        (old.vrr_enabled && (new.update_m_n || new.update_lrr || vrr_params_changed(old, new))))
}

// upstream: intel_display.c audio_enabling()
pub fn audio_enabling(old: &PipeState, new: &PipeState) -> bool {
    new.hw_active && (((!old.has_audio || new.needs_modeset) && new.has_audio) ||
        (new.has_audio && new.eld_changed))
}

// upstream: intel_display.c audio_disabling()
pub fn audio_disabling(old: &PipeState, new: &PipeState) -> bool {
    old.hw_active && ((old.has_audio && (!new.has_audio || new.needs_modeset)) ||
        (old.has_audio && new.eld_changed))
}

// upstream: intel_display.c intel_crtc_lobf_enabling()
pub fn intel_crtc_lobf_enabling(old: &PipeState, new: &PipeState) -> bool {
    new.hw_active && (((!old.has_lobf || new.needs_modeset) && new.has_lobf) ||
        (new.has_lobf && (new.update_lrr || new.update_m_n)))
}

// upstream: intel_display.c intel_crtc_lobf_disabling()
pub fn intel_crtc_lobf_disabling(old: &PipeState, new: &PipeState) -> bool {
    old.hw_active && ((old.has_lobf && (!new.has_lobf || new.needs_modeset)) ||
        (old.has_lobf && (new.update_lrr || new.update_m_n)))
}

// upstream: intel_display.c intel_post_plane_update()
pub fn intel_post_plane_update<O: ModesetOps>(
    state: &AtomicState, tr: &PipeTransition, ops: &mut O,
) -> Result<(), ModesetError> {
    let old = &tr.old;
    let new = &tr.new;
    ops.action_value(Action::FrontbufferFlip, new.pipe, new.fb_bits)?;
    if new.update_wm_post && new.hw_active {
        ops.action(Action::UpdateWatermarks, Some(new.pipe))?;
    }
    ops.action(Action::FbcPostUpdate, Some(new.pipe))?;
    if old.async_flip_vtd_wa && !new.async_flip_vtd_wa {
        ops.action(Action::AsyncFlipVtdWaDisable, Some(new.pipe))?;
    }
    if old.nv12_wa && !new.nv12_wa {
        ops.action(Action::Nv12WaDisable, Some(new.pipe))?;
    }
    if old.scalerclk_wa && !new.scalerclk_wa {
        ops.action(Action::ScalerClockGatingWaDisable, Some(new.pipe))?;
    }
    if old.cursorclk_wa && !new.cursorclk_wa {
        ops.action(Action::CursorClockGatingWaDisable, Some(new.pipe))?;
    }
    if new.color_update { ops.action(Action::ColorPostUpdate, Some(new.pipe))?; }
    if audio_enabling(old, new) { intel_encoders_audio_enable(state, new.pipe, ops)?; }
    if new.scaler_ecc_wa && old.pch_pfit_enabled != new.pch_pfit_enabled {
        ops.action(Action::UnmaskScalerEcc, Some(new.pipe))?;
    }
    if intel_crtc_lobf_enabling(old, new) {
        ops.action(Action::AlpmLobfEnable, Some(new.pipe))?;
    }
    ops.action(Action::PsrPostPlaneUpdate, Some(new.pipe))
}

// upstream: intel_display.c intel_post_plane_update_after_readout()
pub fn intel_post_plane_update_after_readout<O: ModesetOps>(
    tr: &PipeTransition, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::HswIpsPostUpdate, Some(tr.new.pipe))?;
    ops.action(Action::DrrsActivate, Some(tr.new.pipe))
}

// upstream: intel_display.c intel_crtc_enable_flip_done()
pub fn intel_crtc_enable_flip_done<O: ModesetOps>(
    state: &AtomicState, crtc: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    for plane in &state.planes {
        if plane.pipe == crtc.pipe && crtc.update_planes & pipe_bit(plane.id) as u32 != 0 {
            ops.plane_action(Action::EnablePlaneFlipDone, plane.pipe, plane.id)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_crtc_disable_flip_done()
pub fn intel_crtc_disable_flip_done<O: ModesetOps>(
    state: &AtomicState, crtc: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    for plane in &state.planes {
        if plane.pipe == crtc.pipe && crtc.update_planes & pipe_bit(plane.id) as u32 != 0 {
            ops.plane_action(Action::DisablePlaneFlipDone, plane.pipe, plane.id)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_crtc_async_flip_disable_wa()
pub fn intel_crtc_async_flip_disable_wa<O: ModesetOps>(
    state: &AtomicState, tr: &PipeTransition, ops: &mut O,
) -> Result<(), ModesetError> {
    let disable = tr.old.async_flip_planes & !tr.new.async_flip_planes;
    let mut need_vbl_wait = false;
    for plane in &state.planes {
        let bit = pipe_bit(plane.id) as u32;
        if plane.need_async_flip_toggle_wa && plane.pipe == tr.new.pipe && disable & bit != 0 {
            ops.plane_action(Action::AsyncFlipToggleDisable, plane.pipe, plane.id)?;
            need_vbl_wait = true;
        }
    }
    if need_vbl_wait { ops.action(Action::WaitNextVblank, Some(tr.new.pipe))?; }
    Ok(())
}

// upstream: intel_display.c intel_pre_plane_update()
pub fn intel_pre_plane_update<O: ModesetOps>(
    state: &AtomicState, tr: &PipeTransition, ops: &mut O,
) -> Result<(), ModesetError> {
    let old = &tr.old;
    let new = &tr.new;
    if intel_crtc_lobf_disabling(old, new) {
        ops.action(Action::AlpmLobfDisable, Some(new.pipe))?;
    }
    ops.action(Action::PsrPrePlaneUpdate, Some(new.pipe))?;
    if new.vrr_disabling {
        ops.action(Action::VrrDisable, Some(new.pipe))?;
        ops.action(Action::ResetVrrDcb, Some(new.pipe))?;
        ops.action(Action::UpdateActiveTimings, Some(old.pipe))?;
    }
    if audio_disabling(old, new) { intel_encoders_audio_disable(state, new.pipe, ops)?; }
    if old.drrs_active { ops.action(Action::DrrsDeactivate, Some(old.pipe))?; }
    if new.ips_pre_update_wait && ops.action_condition(Action::IpsPreUpdate, new.pipe)? {
        ops.action(Action::WaitNextVblank, Some(new.pipe))?;
    }
    if new.fbc_pre_update_wait && ops.action_condition(Action::FbcPreUpdate, new.pipe)? {
        ops.action(Action::WaitNextVblank, Some(new.pipe))?;
    }
    if !old.async_flip_vtd_wa && new.async_flip_vtd_wa {
        ops.action(Action::AsyncFlipVtdWaEnable, Some(new.pipe))?;
    }
    if !old.nv12_wa && new.nv12_wa {
        ops.action(Action::Nv12WaEnable, Some(new.pipe))?;
    }
    if !old.scalerclk_wa && new.scalerclk_wa {
        ops.action(Action::ScalerClockGatingWaEnable, Some(new.pipe))?;
    }
    if !old.cursorclk_wa && new.cursorclk_wa {
        ops.action(Action::CursorClockGatingWaEnable, Some(new.pipe))?;
    }
    if old.hw_active && new.disable_cxsr {
        if ops.action_condition(Action::DisableCxsr, new.pipe)? {
            ops.action(Action::WaitNextVblank, Some(new.pipe))?;
        }
    }
    if !needs_modeset(new) {
        let initial = ops.action_condition(Action::InitialWatermarksCheck, new.pipe)?;
        if !initial && new.update_wm_pre {
            ops.action(Action::UpdateIntermediateWatermarks, Some(new.pipe))?;
        }
    }
    if old.async_flip_planes & !new.async_flip_planes != 0 {
        intel_crtc_async_flip_disable_wa(state, tr, ops)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_crtc_disable_planes()
pub fn intel_crtc_disable_planes<O: ModesetOps>(
    state: &AtomicState, tr: &PipeTransition, ops: &mut O,
) -> Result<(), ModesetError> {
    let update_mask = tr.new.update_planes;
    intel_crtc_dpms_overlay_disable(&tr.new, ops)?;
    let mut fb_bits = 0u32;
    for plane in &state.planes {
        if plane.pipe != tr.old.pipe || update_mask & (pipe_bit(plane.id) as u32) == 0 { continue; }
        ops.plane_action(Action::DisablePlaneArm, plane.pipe, plane.id)?;
        if plane.old_visible { fb_bits |= plane.frontbuffer_bit; }
    }
    ops.action_value(Action::FrontbufferFlip, tr.new.pipe, fb_bits)?;
    Ok(())
}

// upstream: intel_display.c intel_encoders_update_prepare()
pub fn intel_encoders_update_prepare<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, _ops: &mut O,
) {
    if !caps.has_dpll_manager { return; }
    for tr in &mut state.pipes {
        if needs_modeset(&tr.new) { continue; }
        tr.new.has_intel_dpll = tr.old.has_intel_dpll;
        tr.new.dpll_hw_state = tr.old.dpll_hw_state;
    }
}

// upstream: intel_display.c intel_encoders_pre_pll_enable()
pub fn intel_encoders_pre_pll_enable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.new_crtc == Some(pipe) && encoder.hook_mask & ENCODER_HOOK_PRE_PLL != 0 {
            encoder_hook(ops, Action::EncoderPrePllHook, pipe, encoder)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_encoders_pre_enable()
pub fn intel_encoders_pre_enable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.new_crtc == Some(pipe) && encoder.hook_mask & ENCODER_HOOK_PRE_ENABLE != 0 {
            encoder_hook(ops, Action::EncoderPreEnableHook, pipe, encoder)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_encoders_enable()
pub fn intel_encoders_enable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.new_crtc != Some(pipe) { continue; }
        if encoder.hook_mask & ENCODER_HOOK_ENABLE != 0 {
            encoder_hook(ops, Action::EncoderEnableHook, pipe, encoder)?;
        }
        encoder_hook(ops, Action::OpRegionEnable, pipe, encoder)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_encoders_disable()
pub fn intel_encoders_disable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.old_crtc != Some(pipe) { continue; }
        encoder_hook(ops, Action::OpRegionDisable, pipe, encoder)?;
        if encoder.hook_mask & ENCODER_HOOK_DISABLE != 0 {
            encoder_hook(ops, Action::EncoderDisableHook, pipe, encoder)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_encoders_post_disable()
pub fn intel_encoders_post_disable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.old_crtc == Some(pipe) && encoder.hook_mask & ENCODER_HOOK_POST_DISABLE != 0 {
            encoder_hook(ops, Action::EncoderPostDisableHook, pipe, encoder)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_encoders_post_pll_disable()
pub fn intel_encoders_post_pll_disable<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.old_crtc == Some(pipe) && encoder.hook_mask & ENCODER_HOOK_POST_PLL_DISABLE != 0 {
            encoder_hook(ops, Action::EncoderPostPllDisableHook, pipe, encoder)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_encoders_update_pipe()
pub fn intel_encoders_update_pipe<O: ModesetOps>(
    state: &AtomicState, pipe: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for encoder in &state.encoders {
        if encoder.new_crtc == Some(pipe) && encoder.hook_mask & ENCODER_HOOK_UPDATE_PIPE != 0 {
            encoder_hook(ops, Action::EncoderUpdatePipeHook, pipe, encoder)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c hsw_set_linetime_wm()
pub fn hsw_set_linetime_wm<O: ModesetOps>(
    state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action_pair(Action::SetHswLinetimeWatermark, state.pipe,
        u32::from(state.linetime), u32::from(state.ips_linetime))
}

// upstream: intel_display.c hsw_set_frame_start_delay()
pub fn hsw_set_frame_start_delay<O: ModesetOps>(
    state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action_value(Action::SetFrameStartDelay, state.pipe,
        state.framestart_delay.saturating_sub(1))
}

// upstream: intel_display.c hsw_configure_cpu_transcoder()
pub fn hsw_configure_cpu_transcoder<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.has_pch_encoder {
        intel_cpu_transcoder_set_m1_n1(state, &state.fdi_m_n, ops)?;
    } else if state.has_dp_encoder {
        intel_cpu_transcoder_set_m1_n1(state, &state.dp_m_n, ops)?;
        intel_cpu_transcoder_set_m2_n2(caps, state, 0xfe, ops)?;
    }
    intel_set_transcoder_timings(caps, state, ops)?;
    if !state.is_edp_transcoder {
        ops.action(Action::SetTranscoderMultiplier, Some(state.pipe))?;
    }
    hsw_set_frame_start_delay(state, ops)?;
    hsw_set_transconf(caps, state, ops)
}

// upstream: intel_display.c hsw_crtc_enable()
pub fn hsw_crtc_enable<O: ModesetOps>(
    state: &AtomicState, master: &PipeState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    if master.pipe_active {
        ops.action(Action::VerifyCrtc, Some(master.pipe))?;
        return Ok(());
    }
    let mask = joined_mask(master);
    for member in &state.pipes {
        if mask & pipe_bit(member.new.pipe) != 0 && member.new.hw_enable {
            ops.action(Action::EnableDmcPipe, Some(member.new.pipe))?;
        }
    }
    intel_encoders_pre_pll_enable(state, master.pipe, ops)?;
    if master.has_intel_dpll { ops.action(Action::EnableDpll, Some(master.pipe))?; }
    intel_encoders_pre_enable(state, master.pipe, ops)?;
    for member in &state.pipes {
        let pipe = member.new.pipe;
        let crtc = &member.new;
        if mask & pipe_bit(pipe) == 0 || !crtc.hw_enable { continue; }
        if crtc.has_dsc { ops.action(Action::EnableDsc, Some(pipe))?; }
        if caps.has_uncompressed_joiner {
            ops.action(Action::EnableUncompressedJoiner, Some(pipe))?;
        }
        intel_set_pipe_src_size(crtc, ops)?;
        if caps.display_version >= 9 || caps.broadwell {
            bdw_set_pipe_misc(caps, crtc, ops)?;
        }
    }
    if !master.transcoder_is_dsi {
        hsw_configure_cpu_transcoder(caps, master, ops)?;
    }
    for member in &state.pipes {
        let pipe = member.new.pipe;
        let crtc = &member.new;
        if mask & pipe_bit(pipe) == 0 || !crtc.hw_enable { continue; }
        ops.action(Action::MarkPipeActive, Some(pipe))?;
        if crtc.glk_scaler_wa {
            ops.action(Action::PipeScalerClockGatingEnable, Some(pipe))?;
        }
        if caps.display_version >= 9 {
            ops.action(Action::EnableSkylakePfit, Some(pipe))?;
        } else {
            ops.action(Action::EnableIlkPfit, Some(pipe))?;
        }
        ops.action(Action::ColorModeset, Some(pipe))?;
        hsw_set_linetime_wm(crtc, ops)?;
        if caps.display_version >= 11 {
            icl_set_pipe_chicken(caps, crtc, ops)?;
        }
        ops.action(Action::InitialWatermarks, Some(pipe))?;
    }
    intel_encoders_enable(state, master.pipe, ops)?;
    for member in &state.pipes {
        let pipe = member.new.pipe;
        let crtc = &member.new;
        if mask & pipe_bit(pipe) == 0 || !crtc.hw_enable { continue; }
        if crtc.glk_scaler_wa {
            ops.action(Action::WaitNextVblank, Some(pipe))?;
            ops.action(Action::PipeScalerClockGatingDisable, Some(pipe))?;
        }
        if caps.haswell && crtc.hsw_workaround_pipe != u8::MAX {
            ops.action(Action::WaitNextVblank, Some(crtc.hsw_workaround_pipe))?;
            ops.action(Action::WaitNextVblank, Some(crtc.hsw_workaround_pipe))?;
        }
    }
    Ok(())
}

// upstream: intel_display.c hsw_crtc_disable()
pub fn hsw_crtc_disable<O: ModesetOps>(
    state: &AtomicState, old: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    intel_encoders_disable(state, old.pipe, ops)?;
    intel_encoders_post_disable(state, old.pipe, ops)?;
    if old.has_intel_dpll { ops.action(Action::DisableDpll, Some(old.pipe))?; }
    intel_encoders_post_pll_disable(state, old.pipe, ops)?;
    for member in &state.pipes {
        if joined_mask(old) & pipe_bit(member.old.pipe) != 0 {
            ops.action(Action::DisableDmcPipe, Some(member.old.pipe))?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_phy_is_combo()
pub fn intel_phy_is_combo(caps: DisplayCaps, phy: i8) -> bool {
    const PHY_NONE: i8 = -1;
    const PHY_B: i8 = 1;
    const PHY_E: i8 = 4;
    if phy == PHY_NONE || caps.dg2 { return false; }
    if caps.alder_lake_s { return phy <= PHY_E; }
    if caps.alder_lake_p || caps.display_version == 11 || caps.display_version == 12 {
        return phy <= PHY_B;
    }
    false
}

// upstream: intel_display.c intel_phy_is_tc()
pub fn intel_phy_is_tc(caps: DisplayCaps, phy: i8) -> bool {
    const PHY_D: i8 = 3;
    const PHY_F: i8 = 5;
    const PHY_I: i8 = 8;
    if caps.dgfx { return false; }
    if caps.display_version >= 13 { return (PHY_F..=PHY_I).contains(&phy); }
    if caps.tiger_lake { return (PHY_D..=PHY_I).contains(&phy); }
    false
}

const PHY_A: i8 = 0;
const PHY_B: i8 = 1;
const PHY_D: i8 = 3;
const PHY_F: i8 = 5;
const PHY_NONE: i8 = -1;
const PORT_NONE: i8 = -1;
const PORT_A: i8 = 0;
const PORT_D: i8 = 3;
const PORT_TC1: i8 = PORT_D;
const PORT_TC5: i8 = 7;
const TC_PORT_NONE: i8 = -1;
const TC_PORT_1: i8 = 0;

// upstream: intel_display.c intel_port_to_phy()
pub fn intel_port_to_phy(caps: DisplayCaps, port: i8) -> i8 {
    if port == PORT_NONE { return PHY_NONE; }
    if caps.display_version >= 13 && port >= PORT_TC5 {
        PHY_D + port - PORT_TC5
    } else if caps.display_version >= 13 && port >= PORT_TC1 {
        PHY_F + port - PORT_TC1
    } else if caps.alder_lake_s && port >= PORT_TC1 {
        PHY_B + port - PORT_TC1
    } else {
        PHY_A + port - PORT_A
    }
}

// upstream: intel_display.c intel_port_to_tc()
pub fn intel_port_to_tc(caps: DisplayCaps, port: i8) -> i8 {
    let first_port = if caps.display_version >= 12 { PORT_TC1 } else { 2 };
    if port < first_port { return TC_PORT_NONE; }
    TC_PORT_1 + port - first_port
}

// upstream: intel_display.c intel_encoder_to_phy()
pub fn intel_encoder_to_phy(caps: DisplayCaps, encoder: &EncoderTransition) -> i8 {
    intel_port_to_phy(caps, encoder.port as i8)
}

// upstream: intel_display.c intel_encoder_is_combo()
pub fn intel_encoder_is_combo(caps: DisplayCaps, encoder: &EncoderTransition) -> bool {
    intel_phy_is_combo(caps, intel_encoder_to_phy(caps, encoder))
}

// upstream: intel_display.c intel_encoder_is_tc()
pub fn intel_encoder_is_tc(caps: DisplayCaps, encoder: &EncoderTransition) -> bool {
    !encoder.dedicated_external && intel_phy_is_tc(caps, intel_encoder_to_phy(caps, encoder))
}

// upstream: intel_display.c intel_encoder_to_tc()
pub fn intel_encoder_to_tc(caps: DisplayCaps, encoder: &EncoderTransition) -> i8 {
    let phy = intel_encoder_to_phy(caps, encoder);
    if !intel_phy_is_tc(caps, phy) { return TC_PORT_NONE; }
    intel_port_to_tc(caps, encoder.port as i8)
}

// upstream: intel_display.c intel_aux_power_domain()
pub fn intel_aux_power_domain(encoder: &EncoderTransition) -> u32 {
    if encoder.tbt_alt_mode { encoder.tbt_aux_power_domain }
    else { encoder.legacy_aux_power_domain }
}
// upstream: intel_display.c get_crtc_power_domains()
pub fn get_crtc_power_domains(caps: DisplayCaps, state: &PipeState) -> u32 {
    if !state.hw_active { return 0; }
    let mut mask = state.pipe_power_domain | state.transcoder_power_domain;
    if state.pch_pfit_enabled || state.pch_pfit_force_thru {
        mask |= state.panel_fitter_power_domain;
    }
    mask |= state.encoder_power_domains;
    if caps.has_ddi && state.has_audio { mask |= state.audio_power_domain; }
    if state.has_intel_dpll { mask |= state.display_core_power_domain; }
    if state.has_dsc { mask |= state.dsc_power_domain; }
    mask
}

// upstream: intel_display.c intel_modeset_get_crtc_power_domains()
pub fn intel_modeset_get_crtc_power_domains<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, old_domains: &mut u32, ops: &mut O,
) -> Result<(), ModesetError> {
    let domains = get_crtc_power_domains(caps, state);
    let new_domains = domains & !state.enabled_power_domains;
    *old_domains = state.enabled_power_domains & !domains;
    let mut pending = new_domains;
    while pending != 0 {
        let bit = pending.trailing_zeros();
        let domain = 1u32 << bit;
        ops.action_value(Action::AcquirePowerDomain, state.pipe, domain)?;
        pending &= !domain;
    }
    state.enabled_power_domains |= new_domains;
    Ok(())
}

// upstream: intel_display.c intel_modeset_put_crtc_power_domains()
pub fn intel_modeset_put_crtc_power_domains<O: ModesetOps>(
    state: &mut PipeState, domains: u32, ops: &mut O,
) -> Result<(), ModesetError> {
    let mut pending = domains;
    while pending != 0 {
        let bit = pending.trailing_zeros();
        let domain = 1u32 << bit;
        ops.action_value(Action::ReleasePowerDomain, state.pipe, domain)?;
        pending &= !domain;
    }
    state.enabled_power_domains &= !domains;
    Ok(())
}

// upstream: intel_display.c intel_mode_from_crtc_timings()
pub fn intel_mode_from_crtc_timings(_mode: &mut Timing, timings: &Timing) {
    *_mode = *timings;
}

// upstream: intel_display.c intel_crtc_compute_pixel_rate()
pub fn intel_crtc_compute_pixel_rate(caps: DisplayCaps, state: &mut PipeState) {
    let rate = state.pipe_mode.clock_khz;
    if caps.has_gmch || !state.pch_pfit_enabled {
        state.pixel_rate = rate;
        return;
    }
    let src_w = state.pipe_src_width;
    let src_h = state.pipe_src_height;
    let dst_w = src_w.min(state.pfit_dst_width);
    let dst_h = src_h.min(state.pfit_dst_height);
    let dst_pixels = u64::from(dst_w) * u64::from(dst_h);
    if dst_pixels == 0 {
        state.pixel_rate = u32::MAX;
    } else {
        let src_pixels = u64::from(src_w) * u64::from(src_h);
        state.pixel_rate = (u64::from(rate) * src_pixels).div_ceil(dst_pixels).min(u64::from(u32::MAX)) as u32;
    }
}

// upstream: intel_display.c intel_joiner_adjust_timings()
pub fn intel_joiner_adjust_timings(state: &PipeState, mode: &mut Timing) {
    let n = u32::from(intel_crtc_num_joined_pipes(state));
    if n == 1 { return; }
    mode.clock_khz /= n;
    mode.hdisplay /= n;
    mode.hblank_start /= n;
    mode.hblank_end /= n;
    mode.hsync_start /= n;
    mode.hsync_end /= n;
    mode.htotal /= n;
}

// upstream: intel_display.c intel_splitter_adjust_timings()
pub fn intel_splitter_adjust_timings(state: &PipeState, mode: &mut Timing) {
    if !state.splitter_enabled { return; }
    let overlap = u32::from(state.splitter_pixel_overlap);
    let links = u32::from(state.splitter_link_count);
    if links == 0 { return; }
    mode.hdisplay = mode.hdisplay.saturating_sub(overlap).saturating_mul(links);
    mode.hblank_start = mode.hblank_start.saturating_sub(overlap).saturating_mul(links);
    mode.hblank_end = mode.hblank_end.saturating_sub(overlap).saturating_mul(links);
    mode.hsync_start = mode.hsync_start.saturating_sub(overlap).saturating_mul(links);
    mode.hsync_end = mode.hsync_end.saturating_sub(overlap).saturating_mul(links);
    mode.htotal = mode.htotal.saturating_sub(overlap).saturating_mul(links);
    mode.clock_khz = mode.clock_khz.saturating_mul(links);
}

// upstream: intel_display.c intel_crtc_readout_derived_state()
pub fn intel_crtc_readout_derived_state(caps: DisplayCaps, state: &mut PipeState) {
    let mut pipe_mode = state.adjusted_mode;
    intel_splitter_adjust_timings(state, &mut pipe_mode);
    let mut adjusted = state.adjusted_mode;
    intel_mode_from_crtc_timings(&mut adjusted, &pipe_mode);
    state.adjusted_mode = adjusted;
    let mut mode = pipe_mode;
    intel_mode_from_crtc_timings(&mut mode, &pipe_mode);
    mode.hdisplay = state.pipe_src_width.saturating_mul(u32::from(intel_crtc_num_joined_pipes(state)));
    mode.vdisplay = state.pipe_src_height;
    intel_joiner_adjust_timings(state, &mut pipe_mode);
    let mut pipe_crtc_mode = pipe_mode;
    intel_mode_from_crtc_timings(&mut pipe_crtc_mode, &pipe_mode);
    state.hw_mode = mode;
    state.pipe_mode = pipe_crtc_mode;
    intel_crtc_compute_pixel_rate(caps, state);
}

// upstream: intel_display.c intel_encoder_get_config()
pub fn intel_encoder_get_config<O: ModesetOps>(
    state: &mut AtomicState, crtc_index: usize, encoder_id: u8, caps: DisplayCaps,
    ops: &mut O,
) -> Result<(), ModesetError> {
    let crtc = state.pipes.get(crtc_index).ok_or(ModesetError::Invalid)?.new;
    ops.encoder_action(Action::EncoderGetConfigHook, crtc.pipe, encoder_id)?;
    intel_crtc_readout_derived_state(caps, &mut state.pipes[crtc_index].new);
    Ok(())
}
// upstream: intel_display.c intel_joiner_compute_pipe_src()
pub fn intel_joiner_compute_pipe_src(state: &mut PipeState) {
    let n = u32::from(intel_crtc_num_joined_pipes(state));
    if n == 1 { return; }
    state.pipe_src_width /= n;
}

// upstream: intel_display.c intel_crtc_compute_pipe_src()
pub fn intel_crtc_compute_pipe_src(state: &mut PipeState) -> Result<(), ModesetError> {
    intel_joiner_compute_pipe_src(state);
    if state.pipe_src_width & 1 != 0 && (state.double_wide || state.lvds_dual_link) {
        return Err(ModesetError::Invalid);
    }
    Ok(())
}

// upstream: intel_display.c intel_crtc_compute_pipe_mode()
pub fn intel_crtc_compute_pipe_mode(
    caps: DisplayCaps, state: &mut PipeState,
) -> Result<(), ModesetError> {
    let mut mode = state.adjusted_mode;
    intel_splitter_adjust_timings(state, &mut mode);
    intel_joiner_adjust_timings(state, &mut mode);
    let crtc_mode = mode;
    intel_mode_from_crtc_timings(&mut mode, &crtc_mode);
    if mode.clock_khz > caps.cdclk_max_dotclock { return Err(ModesetError::Invalid); }
    state.pipe_mode = mode;
    Ok(())
}

// upstream: intel_display.c intel_crtc_set_context_latency()
pub fn intel_crtc_set_context_latency(caps: DisplayCaps, state: &PipeState) -> i32 {
    if !caps.has_dsb { return 0; }
    state.psr_min_set_context_latency.max(0)
}

// upstream: intel_display.c intel_crtc_compute_set_context_latency()
pub fn intel_crtc_compute_set_context_latency(
    caps: DisplayCaps, state: &mut PipeState,
) -> Result<(), ModesetError> {
    let latency = intel_crtc_set_context_latency(caps, state);
    let max_delay = state.adjusted_mode.vblank_end
        .saturating_sub(state.adjusted_mode.vblank_start).saturating_sub(1);
    if u32::try_from(latency).unwrap_or(u32::MAX) > max_delay {
        return Err(ModesetError::Invalid);
    }
    state.set_context_latency = latency;
    state.adjusted_mode.vblank_start = state.adjusted_mode.vblank_start.saturating_add(latency as u32);
    Ok(())
}

// upstream: intel_display.c intel_crtc_compute_config()
pub fn intel_crtc_compute_config<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::DpllClockCompute, Some(state.pipe))?;
    intel_crtc_compute_set_context_latency(caps, state)?;
    intel_crtc_compute_pipe_src(state)?;
    intel_crtc_compute_pipe_mode(caps, state)?;
    intel_crtc_compute_pixel_rate(caps, state);
    if state.has_pch_encoder {
        ops.action(Action::IlkFdiComputeConfig, Some(state.pipe))?;
    } else {
        ops.action(Action::ComputeVrrGuardband, Some(state.pipe))?;
    }
    Ok(())
}

// upstream: intel_display.c intel_reduce_m_n_ratio()
pub fn intel_reduce_m_n_ratio(num: &mut u32, den: &mut u32) {
    while *num > DATA_LINK_M_N_MASK || *den > DATA_LINK_M_N_MASK {
        *num >>= 1;
        *den >>= 1;
    }
}

// upstream: intel_display.c compute_m_n()
pub fn compute_m_n(m: u32, n: u32, constant_n: u32) -> Result<(u32, u32), ModesetError> {
    if n == 0 { return Err(ModesetError::Invalid); }
    let mut ret_n = if constant_n != 0 {
        constant_n
    } else {
        n.checked_next_power_of_two().unwrap_or(DATA_LINK_N_MAX).min(DATA_LINK_N_MAX)
    };
    let mut ret_m = ((u64::from(m) * u64::from(ret_n)) / u64::from(n)) as u32;
    intel_reduce_m_n_ratio(&mut ret_m, &mut ret_n);
    Ok((ret_m, ret_n))
}

// upstream: intel_display.c intel_link_compute_m_n()
pub fn intel_link_compute_m_n<O: ModesetOps>(
    ops: &mut O, bpp_x16: u16, lanes: u8, pixel_clock: u32,
    link_clock: u32, bw_overhead_ppm: u32,
) -> Result<LinkMn, ModesetError> {
    if lanes == 0 || link_clock == 0 { return Err(ModesetError::Invalid); }
    let symbol_clock = ops.dp_link_symbol_clock(link_clock);
    let data_m = ops.dp_effective_data_rate(pixel_clock, bpp_x16, bw_overhead_ppm);
    let data_n = ops.dp_max_dprx_data_rate(link_clock, lanes);
    let (data_m, data_n) = compute_m_n(data_m, data_n, 0x0800_0000)?;
    let (link_m, link_n) = compute_m_n(pixel_clock, symbol_clock, 0x0008_0000)?;
    Ok(LinkMn { tu: 64, data_m, data_n, link_m, link_n })
}

// upstream: intel_display.c intel_zero_m_n()
pub fn intel_zero_m_n() -> LinkMn { LinkMn { tu: 1, ..LinkMn::default() } }

// upstream: intel_display.c intel_set_m_n()
pub fn intel_set_m_n<O: ModesetOps>(
    state: &PipeState, m_n: &LinkMn, ops: &mut O,
) -> Result<(), ModesetError> {
    let tu_size = u32::from(m_n.tu.saturating_sub(1) & 0x3f) << 25;
    ops.action_value(Action::WriteDataM, state.pipe, tu_size | m_n.data_m)?;
    ops.action_value(Action::WriteDataN, state.pipe, m_n.data_n)?;
    ops.action_value(Action::WriteLinkM, state.pipe, m_n.link_m)?;
    // LINK_N is last on BDW+ because it arms the double-buffered M/N update.
    ops.action_value(Action::WriteLinkN, state.pipe, m_n.link_n)
}

// upstream: intel_display.c intel_cpu_transcoder_has_m2_n2()
pub fn intel_cpu_transcoder_has_m2_n2(caps: DisplayCaps, transcoder: u8, edp_transcoder: u8) -> bool {
    if caps.haswell { return transcoder == edp_transcoder; }
    caps.gen5_to7 || caps.cherryview
}

// upstream: intel_display.c intel_cpu_transcoder_set_m1_n1()
pub fn intel_cpu_transcoder_set_m1_n1<O: ModesetOps>(
    state: &PipeState, m_n: &LinkMn, ops: &mut O,
) -> Result<(), ModesetError> {
    intel_set_m_n(state, m_n, ops)
}

// upstream: intel_display.c intel_cpu_transcoder_set_m2_n2()
pub fn intel_cpu_transcoder_set_m2_n2<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, edp_transcoder: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    if !intel_cpu_transcoder_has_m2_n2(caps, state.cpu_transcoder, edp_transcoder) { return Ok(()); }
    intel_set_m_n(state, &state.dp_m2_n2, ops)
}

// upstream: intel_display.c transcoder_has_vrr()
pub fn transcoder_has_vrr(caps: DisplayCaps, crtc_state: &PipeState) -> bool {
    caps.has_vrr && !crtc_state.transcoder_is_dsi
}

// upstream: intel_display.c intel_set_transcoder_timings()
pub fn intel_set_transcoder_timings<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.transcoder_is_dsi { ops.action(Action::VerifyCrtc, Some(state.pipe))?; }
    let m = state.adjusted_mode;
    let vdisplay = m.vdisplay;
    let mut vtotal = m.vtotal;
    let mut vblank_start = m.vblank_start;
    let mut vblank_end = m.vblank_end;
    let mut vsync_shift = 0i64;
    if state.is_interlaced {
        vtotal = vtotal.saturating_sub(1);
        vblank_end = vblank_end.saturating_sub(1);
        vsync_shift = if state.is_sdvo_output {
            i64::from(m.htotal.saturating_sub(1) / 2)
        } else {
            i64::from(m.hsync_start) - i64::from(m.htotal / 2)
        };
        if vsync_shift < 0 { vsync_shift += i64::from(m.htotal); }
    }
    if caps.display_version >= 13 {
        ops.action_value(Action::WriteContextLatency, state.pipe, state.set_context_latency as u32)?;
        vblank_start = 1;
    } else if caps.display_version == 12 {
        vblank_start = vdisplay.saturating_add(state.set_context_latency as u32);
    }
    if caps.display_version >= 4 && caps.display_version < 35 {
        ops.action_value(Action::WriteVsyncShift, state.pipe, vsync_shift as u32)?;
    }
    ops.action_pair(Action::WriteTranscoderHtotal, state.pipe,
        m.hdisplay.saturating_sub(1), m.htotal.saturating_sub(1))?;
    ops.action_pair(Action::WriteTranscoderHblank, state.pipe,
        m.hblank_start.saturating_sub(1), m.hblank_end.saturating_sub(1))?;
    ops.action_pair(Action::WriteTranscoderHsync, state.pipe,
        m.hsync_start.saturating_sub(1), m.hsync_end.saturating_sub(1))?;
    if state.vrr_always_use_vrr_tg { vtotal = 1; }
    ops.action_pair(Action::WriteTranscoderVtotal, state.pipe,
        vdisplay.saturating_sub(1), vtotal.saturating_sub(1))?;
    ops.action_pair(Action::WriteTranscoderVblank, state.pipe,
        vblank_start.saturating_sub(1), vblank_end.saturating_sub(1))?;
    ops.action_pair(Action::WriteTranscoderVsync, state.pipe,
        m.vsync_start.saturating_sub(1), m.vsync_end.saturating_sub(1))?;
    if caps.haswell && state.is_edp_transcoder && (state.pipe == 1 || state.pipe == 2) {
        ops.action_pair(Action::WriteEdpVtotal, state.pipe,
            vdisplay.saturating_sub(1), vtotal.saturating_sub(1))?;
    }
    if caps.display_version >= 30 {
        ops.action_value(Action::WriteMinHblank, state.pipe, state.min_hblank)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_set_transcoder_timings_lrr()
pub fn intel_set_transcoder_timings_lrr<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.transcoder_is_dsi { ops.action(Action::VerifyCrtc, Some(state.pipe))?; }
    let m = state.adjusted_mode;
    let vdisplay = m.vdisplay;
    let mut vtotal = m.vtotal;
    let mut vblank_start = m.vblank_start;
    let mut vblank_end = m.vblank_end;
    if state.is_interlaced {
        vtotal = vtotal.saturating_sub(1);
        vblank_end = vblank_end.saturating_sub(1);
    }
    if caps.display_version >= 13 {
        ops.action_value(Action::WriteContextLatency, state.pipe, state.set_context_latency as u32)?;
        vblank_start = 1;
    } else if caps.display_version == 12 {
        vblank_start = vdisplay.saturating_add(state.set_context_latency as u32);
    }
    ops.action_pair(Action::WriteTranscoderVblank, state.pipe,
        vblank_start.saturating_sub(1), vblank_end.saturating_sub(1))?;
    if state.vrr_always_use_vrr_tg { vtotal = 1; }
    ops.action_pair(Action::WriteTranscoderVtotal, state.pipe,
        vdisplay.saturating_sub(1), vtotal.saturating_sub(1))?;
    ops.action(Action::SetVrrFixedRefreshTimings, Some(state.pipe))?;
    ops.action(Action::EnableVrrTranscoder, Some(state.pipe))
}

// upstream: intel_display.c intel_set_pipe_src_size()
pub fn intel_set_pipe_src_size<O: ModesetOps>(
    state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action_pair(Action::SetPipeSourceSize, state.pipe,
        state.pipe_src_width.saturating_sub(1), state.pipe_src_height.saturating_sub(1))
}

// upstream: intel_display.c intel_pipe_is_interlaced()
pub fn intel_pipe_is_interlaced<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<bool, ModesetError> {
    if caps.display_version == 2 || caps.display_version >= 35 { return Ok(false); }
    let hsw_mask = caps.display_version >= 9 || caps.broadwell || caps.haswell;
    ops.read_transcoder_interlaced(state.pipe, hsw_mask)
}

// upstream: intel_display.c intel_get_transcoder_timings()
pub fn intel_get_transcoder_timings<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    let pipe = state.pipe;
    let transcoder = state.cpu_transcoder;
    let mut m = state.adjusted_mode;
    m.hdisplay = ops.read_timing_field(pipe, transcoder, TimingField::HDisplay)?;
    m.htotal = ops.read_timing_field(pipe, transcoder, TimingField::HTotal)?;
    if !state.transcoder_is_dsi {
        m.hblank_start = ops.read_timing_field(pipe, transcoder, TimingField::HBlankStart)?;
        m.hblank_end = ops.read_timing_field(pipe, transcoder, TimingField::HBlankEnd)?;
    }
    m.hsync_start = ops.read_timing_field(pipe, transcoder, TimingField::HSyncStart)?;
    m.hsync_end = ops.read_timing_field(pipe, transcoder, TimingField::HSyncEnd)?;
    m.vdisplay = ops.read_timing_field(pipe, transcoder, TimingField::VDisplay)?;
    m.vtotal = ops.read_timing_field(pipe, transcoder, TimingField::VTotal)?;
    if !state.transcoder_is_dsi {
        m.vblank_start = ops.read_timing_field(pipe, transcoder, TimingField::VBlankStart)?;
        m.vblank_end = ops.read_timing_field(pipe, transcoder, TimingField::VBlankEnd)?;
    }
    m.vsync_start = ops.read_timing_field(pipe, transcoder, TimingField::VSyncStart)?;
    m.vsync_end = ops.read_timing_field(pipe, transcoder, TimingField::VSyncEnd)?;
    state.is_interlaced = intel_pipe_is_interlaced(caps, state, ops)?;
    if state.is_interlaced {
        m.vtotal = m.vtotal.saturating_add(1);
        m.vblank_end = m.vblank_end.saturating_add(1);
    }
    if caps.display_version >= 13 && !state.transcoder_is_dsi {
        state.set_context_latency = ops.read_timing_field(pipe, transcoder, TimingField::ContextLatency)? as i32;
        m.vblank_start = m.vdisplay.saturating_add(state.set_context_latency as u32);
    } else if caps.display_version == 12 {
        state.set_context_latency = m.vblank_start.saturating_sub(m.vdisplay) as i32;
    }
    if caps.display_version >= 30 {
        state.min_hblank = ops.read_timing_field(pipe, transcoder, TimingField::MinHblank)?;
    }
    state.adjusted_mode = m;
    Ok(())
}

// upstream: intel_display.c intel_joiner_adjust_pipe_src()
pub fn intel_joiner_adjust_pipe_src(state: &mut PipeState) {
    let n = intel_crtc_num_joined_pipes(state);
    if n == 1 { return; }
    let primary = joiner_primary_pipe(state);
    state.pipe_src_x = (i32::from(state.pipe) - i32::from(primary)) * state.pipe_src_width as i32;
}
// upstream: intel_display.c intel_get_pipe_src_size()
pub fn intel_get_pipe_src_size<O: ModesetOps>(
    state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    let (width, height) = ops.read_pipe_src_size(state.pipe)?;
    state.pipe_src_x = 0;
    state.pipe_src_y = 0;
    state.pipe_src_width = width;
    state.pipe_src_height = height;
    intel_joiner_adjust_pipe_src(state);
    Ok(())
}

// upstream: intel_display.c bdw_get_pipe_misc_output_format()
pub fn bdw_get_pipe_misc_output_format<O: ModesetOps>(
    caps: DisplayCaps, pipe: u8, ops: &mut O,
) -> Result<OutputFormat, ModesetError> {
    let misc = ops.read_pipe_misc(pipe)?;
    if misc.yuv420 {
        if caps.display_version < 30 && !misc.yuv420_full_blend {
            ops.action(Action::VerifyCrtc, Some(pipe))?;
        }
        Ok(OutputFormat::Ycbcr420)
    } else if misc.yuv {
        Ok(OutputFormat::Ycbcr444)
    } else {
        Ok(OutputFormat::Rgb)
    }
}

// upstream: intel_display.c hsw_set_transconf()
pub fn hsw_set_transconf<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.write_transconf(
        state.pipe,
        !needs_modeset(state),
        caps.display_version < 35 && state.is_interlaced,
        caps.haswell && state.dither,
        caps.haswell && state.is_yuv_output,
    )
}
// upstream: intel_display.c bdw_set_pipe_misc()
pub fn bdw_set_pipe_misc<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    let bpc = match state.pipe_bpp {
        18 => 6,
        24 => 8,
        30 => 10,
        36 if caps.display_version >= 13 => 12,
        _ => 0,
    };
    if bpc == 0 { return Err(ModesetError::Invalid); }
    let yuv420 = state.output_format == OutputFormat::Ycbcr420;
    let yuv = yuv420 || state.output_format == OutputFormat::Ycbcr444;
    ops.write_pipe_misc(state.pipe, PipeMiscWriteout {
        bpc,
        dither: state.dither,
        yuv,
        yuv420,
        yuv420_full_blend: yuv420 && caps.display_version < 30,
        hdr_precision: caps.display_version >= 11 && is_hdr_mode(state),
        pixel_rounding_trunc: caps.display_version >= 12,
        psr_mask_sprite: caps.broadwell,
    })
}

// upstream: intel_display.c bdw_get_pipe_misc_bpp()
pub fn bdw_get_pipe_misc_bpp<O: ModesetOps>(
    caps: DisplayCaps, pipe: u8, ops: &mut O,
) -> Result<u8, ModesetError> {
    let bpc = ops.read_pipe_misc(pipe)?.bpc;
    Ok(match bpc {
        6 => 18,
        8 => 24,
        10 => 30,
        12 if caps.display_version >= 13 => 36,
        _ => 0,
    })
}

// upstream: intel_display.c intel_get_m_n()
pub fn intel_get_m_n<O: ModesetOps>(
    state: &PipeState, index: u8, ops: &mut O,
) -> Result<LinkMn, ModesetError> {
    let mut m_n = ops.read_link_m_n(state.pipe, state.cpu_transcoder, index)?;
    m_n.data_m &= DATA_LINK_M_N_MASK;
    m_n.data_n &= DATA_LINK_M_N_MASK;
    m_n.link_m &= DATA_LINK_M_N_MASK;
    m_n.link_n &= DATA_LINK_M_N_MASK;
    Ok(m_n)
}

// upstream: intel_display.c intel_cpu_transcoder_get_m1_n1()
pub fn intel_cpu_transcoder_get_m1_n1<O: ModesetOps>(
    state: &PipeState, ops: &mut O,
) -> Result<LinkMn, ModesetError> {
    intel_get_m_n(state, 1, ops)
}

// upstream: intel_display.c intel_cpu_transcoder_get_m2_n2()
pub fn intel_cpu_transcoder_get_m2_n2<O: ModesetOps>(
    state: &PipeState, ops: &mut O,
) -> Result<LinkMn, ModesetError> {
    intel_get_m_n(state, 2, ops)
}

// upstream: intel_display.c joiner_pipes()
pub fn joiner_pipes(caps: DisplayCaps) -> u8 {
    let supported = if caps.display_version >= 12 { 0x0f }
        else if caps.display_version >= 11 { 0x06 } else { 0 };
    supported & caps.pipe_mask
}

// upstream: intel_display.c enabled_uncompressed_joiner_pipes()
pub fn enabled_uncompressed_joiner_pipes<O: ModesetOps>(
    caps: DisplayCaps, ops: &mut O,
) -> Result<(u8, u8), ModesetError> {
    let mut primary = 0;
    let mut secondary = 0;
    if !caps.has_uncompressed_joiner { return Ok((primary, secondary)); }
    for pipe in 0..8 {
        if joiner_pipes(caps) & pipe_bit(pipe) == 0 { continue; }
        let flags = ops.read_joiner_hw_flags(pipe)?;
        if flags.uncompressed_primary { primary |= pipe_bit(pipe); }
        if flags.uncompressed_secondary { secondary |= pipe_bit(pipe); }
    }
    Ok((primary, secondary))
}

// upstream: intel_display.c enabled_bigjoiner_pipes()
pub fn enabled_bigjoiner_pipes<O: ModesetOps>(
    caps: DisplayCaps, ops: &mut O,
) -> Result<(u8, u8), ModesetError> {
    let mut primary = 0;
    let mut secondary = 0;
    if !caps.has_bigjoiner { return Ok((primary, secondary)); }
    for pipe in 0..8 {
        if joiner_pipes(caps) & pipe_bit(pipe) == 0 { continue; }
        let flags = ops.read_joiner_hw_flags(pipe)?;
        if !flags.big_enabled { continue; }
        if flags.big_primary { primary |= pipe_bit(pipe); }
        else { secondary |= pipe_bit(pipe); }
    }
    Ok((primary, secondary))
}

// upstream: intel_display.c expected_secondary_pipes()
pub fn expected_secondary_pipes(primary_pipes: u8, num_pipes: u8) -> u8 {
    let mut secondary = 0u8;
    for i in 1..num_pipes {
        secondary |= primary_pipes.wrapping_shl(u32::from(i));
    }
    secondary
}

// upstream: intel_display.c expected_uncompressed_joiner_secondary_pipes()
pub fn expected_uncompressed_joiner_secondary_pipes(primary: u8) -> u8 {
    expected_secondary_pipes(primary, 2)
}

// upstream: intel_display.c expected_bigjoiner_secondary_pipes()
pub fn expected_bigjoiner_secondary_pipes(primary: u8) -> u8 {
    expected_secondary_pipes(primary, 2)
}

// upstream: intel_display.c get_joiner_primary_pipe()
pub fn get_joiner_primary_pipe(pipe: u8, primary_pipes: u8) -> u8 {
    let lower_or_equal = if pipe >= 7 { primary_pipes }
        else { primary_pipes & ((1u16 << (u32::from(pipe) + 1)) - 1) as u8 };
    for bit in (0..=pipe.min(7)).rev() {
        if lower_or_equal & pipe_bit(bit) != 0 { return pipe_bit(bit); }
    }
    0
}

// upstream: intel_display.c expected_ultrajoiner_secondary_pipes()
pub fn expected_ultrajoiner_secondary_pipes(primary: u8) -> u8 {
    expected_secondary_pipes(primary, 4)
}

// upstream: intel_display.c fixup_ultrajoiner_secondary_pipes()
pub fn fixup_ultrajoiner_secondary_pipes(primary: u8, secondary: u8) -> u8 {
    secondary | primary.wrapping_shl(3)
}

// upstream: intel_display.c enabled_ultrajoiner_pipes()
pub fn enabled_ultrajoiner_pipes<O: ModesetOps>(
    caps: DisplayCaps, ops: &mut O,
) -> Result<(u8, u8), ModesetError> {
    let mut primary = 0;
    let mut secondary = 0;
    if !caps.has_ultrajoiner { return Ok((primary, secondary)); }
    for pipe in 0..8 {
        if joiner_pipes(caps) & pipe_bit(pipe) == 0 { continue; }
        let flags = ops.read_joiner_hw_flags(pipe)?;
        if !flags.ultra_enabled { continue; }
        if flags.ultra_primary { primary |= pipe_bit(pipe); }
        else { secondary |= pipe_bit(pipe); }
    }
    Ok((primary, secondary))
}

// upstream: intel_display.c enabled_joiner_pipes()
pub fn enabled_joiner_pipes<O: ModesetOps>(
    caps: DisplayCaps, pipe: u8, ops: &mut O,
) -> Result<(u8, u8), ModesetError> {
    let (primary_ultra, mut secondary_ultra) = enabled_ultrajoiner_pipes(caps, ops)?;
    if expected_secondary_pipes(primary_ultra, 3) != secondary_ultra {
        return Err(ModesetError::Invalid);
    }
    secondary_ultra = fixup_ultrajoiner_secondary_pipes(primary_ultra, secondary_ultra);
    let (primary_uncompressed, secondary_uncompressed) = enabled_uncompressed_joiner_pipes(caps, ops)?;
    let (primary_big, secondary_big) = enabled_bigjoiner_pipes(caps, ops)?;
    let ultra = primary_ultra | secondary_ultra;
    let uncompressed = primary_uncompressed | secondary_uncompressed;
    let big = primary_big | secondary_big;
    if (primary_ultra & secondary_ultra) != 0 ||
        (primary_uncompressed & secondary_uncompressed) != 0 ||
        (primary_big & secondary_big) != 0 ||
        (ultra & big) != ultra ||
        secondary_ultra != expected_ultrajoiner_secondary_pipes(primary_ultra) ||
        (uncompressed & big) != 0 ||
        secondary_big != expected_bigjoiner_secondary_pipes(primary_big) ||
        secondary_uncompressed != expected_uncompressed_joiner_secondary_pipes(primary_uncompressed) {
        return Err(ModesetError::Invalid);
    }
    let bit = pipe_bit(pipe);
    let checks: [(u8, u8, fn(u8) -> u8); 3] = [
        (ultra, primary_ultra, expected_ultrajoiner_secondary_pipes),
        (uncompressed, primary_uncompressed, expected_uncompressed_joiner_secondary_pipes),
        (big, primary_big, expected_bigjoiner_secondary_pipes),
    ];
    for (all, primaries, expected) in checks {
        if all & bit == 0 { continue; }
        let primary = get_joiner_primary_pipe(pipe, primaries);
        let secondaries = match all {
            _ if all == ultra => secondary_ultra,
            _ if all == uncompressed => secondary_uncompressed,
            _ => secondary_big,
        } & expected(primary);
        if expected(primary) != secondaries { return Err(ModesetError::Invalid); }
        return Ok((primary, secondaries));
    }
    Ok((0, 0))
}

// upstream: intel_display.c hsw_panel_transcoders()
pub fn hsw_panel_transcoders(caps: DisplayCaps) -> u8 {
    let mut mask = caps.edp_transcoder_bit;
    if caps.display_version >= 11 { mask |= caps.dsi_transcoder_mask; }
    mask
}

// upstream: intel_display.c has_edp_transcoders()
pub fn has_edp_transcoders(mask: u8, edp_bit: u8) -> bool { mask & edp_bit != 0 }

// upstream: intel_display.c has_dsi_transcoders()
pub fn has_dsi_transcoders(mask: u8, dsi_mask: u8) -> bool { mask & dsi_mask != 0 }

// upstream: intel_display.c has_pipe_transcoders()
pub fn has_pipe_transcoders(mask: u8, panel_mask: u8) -> bool { mask & !panel_mask != 0 }

// upstream: intel_display.c assert_enabled_transcoders()
pub fn assert_enabled_transcoders(
    mask: u8, edp_bit: u8, dsi_mask: u8,
) -> Result<(), ModesetError> {
    let classes = u8::from(has_edp_transcoders(mask, edp_bit)) +
        u8::from(has_dsi_transcoders(mask, dsi_mask)) +
        u8::from(has_pipe_transcoders(mask, edp_bit | dsi_mask));
    if classes > 1 || (!has_dsi_transcoders(mask, dsi_mask) && mask.count_ones() > 1) {
        Err(ModesetError::Invalid)
    } else { Ok(()) }
}

// upstream: intel_display.c intel_joiner_get_config()
pub fn intel_joiner_get_config<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    let (primary, secondary) = enabled_joiner_pipes(caps, state.pipe, ops)?;
    if (primary | secondary) & pipe_bit(state.pipe) != 0 {
        state.joiner_pipes = primary | secondary;
    }
    Ok(())
}

// upstream: intel_display.c hsw_get_pipe_config()
pub fn hsw_get_pipe_config<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, ops: &mut O,
) -> Result<bool, ModesetError> {
    if !ops.action_condition(Action::ReadoutPipePowerDomain, state.pipe)? { return Ok(false); }
    let result = (|| -> Result<bool, ModesetError> {
        let active = ops.hsw_get_transcoder_state(state)?;
        if !active { return Ok(false); }
        intel_joiner_get_config(caps, state, ops)?;
        ops.action(Action::DscGetConfig, Some(state.pipe))?;
        if !state.transcoder_is_dsi {
            state.framestart_delay = ops.read_timing_field(
                state.pipe, state.cpu_transcoder, TimingField::FrameStartDelay,
            )?.saturating_add(1);
        } else {
            state.framestart_delay = 1;
        }
        if !state.transcoder_is_dsi || caps.display_version >= 11 {
            intel_get_transcoder_timings(caps, state, ops)?;
        }
        if caps.has_vrr && !state.transcoder_is_dsi {
            ops.action(Action::VrrGetConfig, Some(state.pipe))?;
        }
        intel_get_pipe_src_size(state, ops)?;
        state.output_format = if caps.haswell {
            if ops.action_condition(Action::SetTransconf, state.pipe)? {
                OutputFormat::Ycbcr444
            } else { OutputFormat::Rgb }
        } else {
            bdw_get_pipe_misc_output_format(caps, state.pipe, ops)?
        };
        state.sink_format = state.output_format;
        ops.action(Action::ColorGetConfig, Some(state.pipe))?;
        state.linetime = ops.read_timing_field(state.pipe, state.cpu_transcoder, TimingField::Linetime)? as u16;
        if caps.broadwell || caps.haswell {
            state.ips_linetime = ops.read_timing_field(state.pipe, state.cpu_transcoder, TimingField::IpsLinetime)? as u16;
        }
        if ops.action_condition(Action::ReadoutPanelFitterPowerDomain, state.pipe)? {
            if caps.display_version >= 9 {
                ops.action(Action::SkylakeScalerGetConfig, Some(state.pipe))?;
            } else {
                ops.action(Action::IlkPfitGetConfig, Some(state.pipe))?;
            }
        }
        ops.action(Action::IpsGetConfig, Some(state.pipe))?;
        if !state.is_edp_transcoder && !state.transcoder_is_dsi {
            state.pixel_multiplier = ops.read_timing_field(
                state.pipe, state.cpu_transcoder, TimingField::PixelMultiplier,
            )?.saturating_add(1) as u8;
        } else {
            state.pixel_multiplier = 1;
        }
        Ok(true)
    })();
    ops.action(Action::ReadoutPowerDomainsPutAll, Some(state.pipe))?;
    result
}

// upstream: intel_display.c intel_crtc_get_pipe_config()
pub fn intel_crtc_get_pipe_config<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, ops: &mut O,
) -> Result<bool, ModesetError> {
    if !hsw_get_pipe_config(caps, state, ops)? { return Ok(false); }
    state.hw_active = true;
    intel_crtc_readout_derived_state(caps, state);
    Ok(true)
}

// upstream: intel_display.c intel_dotclock_calculate()
pub fn intel_dotclock_calculate<O: ModesetOps>(
    link_freq: u32, m_n: &LinkMn, ops: &mut O,
) -> u32 {
    if m_n.link_n == 0 { return 0; }
    let symbol_size = ops.dp_link_symbol_size(link_freq).max(1);
    let numerator = u64::from(m_n.link_m) * u64::from(link_freq) * 10;
    let denominator = u64::from(m_n.link_n) * u64::from(symbol_size);
    numerator.div_ceil(denominator).min(u64::from(u32::MAX)) as u32
}

// upstream: intel_display.c intel_crtc_dotclock()
pub fn intel_crtc_dotclock<O: ModesetOps>(
    state: &PipeState, ops: &mut O,
) -> u32 {
    let dp_encoder = state.has_dp_encoder;
    let mut dotclock = if dp_encoder {
        intel_dotclock_calculate(state.port_clock, &state.dp_m_n, ops)
    } else if state.has_hdmi_sink && state.pipe_bpp > 24 {
        ((u64::from(state.port_clock) * 24 + u64::from(state.pipe_bpp) / 2) /
            u64::from(state.pipe_bpp)) as u32
    } else {
        state.port_clock
    };
    if state.output_format == OutputFormat::Ycbcr420 && !dp_encoder {
        dotclock = dotclock.saturating_mul(2);
    }
    if state.pixel_multiplier != 0 { dotclock /= u32::from(state.pixel_multiplier); }
    dotclock
}

// upstream: intel_display.c intel_encoder_current_mode()
pub fn intel_encoder_current_mode<O: ModesetOps>(
    state: &AtomicState,
    caps: DisplayCaps,
    encoder: &EncoderTransition,
    hw_pipe: Option<u8>,
    ops: &mut O,
) -> Result<Option<DisplayMode>, ModesetError> {
    let Some(pipe) = hw_pipe else { return Ok(None); };
    let Some(source) = state.pipes.iter().find(|tr| tr.new.pipe == pipe) else {
        return Ok(None);
    };

    // The kernel allocates a temporary CRTC state; copy the modeled pipe
    // identity/config into a local atomic-state scratch object for readout.
    let mut scratch = AtomicState::default();
    scratch.pipes.push(PipeTransition { old: source.old, new: source.new });
    if !intel_crtc_get_pipe_config(caps, &mut scratch.pipes[0].new, ops)? {
        return Ok(None);
    }
    intel_encoder_get_config(&mut scratch, 0, encoder.encoder_id, caps, ops)?;

    let mut mode = DisplayMode::default();
    intel_mode_from_crtc_timings(&mut mode.timing, &scratch.pipes[0].new.hw_adjusted_mode);
    Ok(Some(mode))
}

#[cfg(test)]
mod tests {
    use super::{expected_secondary_pipes, fixup_ultrajoiner_secondary_pipes};

    #[test]
    fn joined_pipe_masks_follow_i915_shift_ranges() {
        assert_eq!(expected_secondary_pipes(1 << 0, 2), 1 << 1);
        assert_eq!(expected_secondary_pipes(1 << 0, 4), 0b0000_1110);
        assert_eq!(expected_secondary_pipes(1 << 2, 3), 0b0001_1000);
    }

    #[test]
    fn ultrajoiner_missing_final_pipe_uses_source_fixup() {
        assert_eq!(fixup_ultrajoiner_secondary_pipes(0b0000_0001, 0b0000_0110), 0b0000_1110);
        assert_eq!(fixup_ultrajoiner_secondary_pipes(0b0000_0010, 0b0000_1100), 0b0001_1100);
    }
}
// upstream: intel_display.c encoders_cloneable()
pub fn encoders_cloneable(a: &EncoderTransition, b: &EncoderTransition) -> bool {
    a.encoder_id == b.encoder_id ||
        (a.cloneable_types & b.encoder_type_bit != 0 && b.cloneable_types & a.encoder_type_bit != 0)
}

// upstream: intel_display.c check_single_encoder_cloning()
pub fn check_single_encoder_cloning(
    state: &AtomicState, pipe: u8, encoder: &EncoderTransition,
) -> bool {
    state.encoders.iter().filter(|other| other.new_crtc == Some(pipe))
        .all(|other| encoders_cloneable(encoder, other))
}

// upstream: intel_display.c hsw_linetime_wm()
pub fn hsw_linetime_wm(state: &PipeState) -> u16 {
    if !state.hw_enable || state.crtc_clock == 0 { return 0; }
    let numerator = u64::from(state.new_timing.htotal) * 1000 * 8;
    (((numerator + u64::from(state.crtc_clock) / 2) / u64::from(state.crtc_clock))
        .min(0x1ff)) as u16
}

// upstream: intel_display.c hsw_ips_linetime_wm()
pub fn hsw_ips_linetime_wm(state: &PipeState, cdclk_khz: u32) -> u16 {
    if !state.hw_enable || cdclk_khz == 0 { return 0; }
    let numerator = u64::from(state.new_timing.htotal) * 1000 * 8;
    (((numerator + u64::from(cdclk_khz) / 2) / u64::from(cdclk_khz)).min(0x1ff)) as u16
}

// upstream: intel_display.c skl_linetime_wm()
pub fn skl_linetime_wm(state: &PipeState, ipc_wa: bool) -> u16 {
    if !state.hw_enable || state.pixel_rate == 0 { return 0; }
    let numerator = u64::from(state.new_timing.htotal) * 1000 * 8;
    let mut wm = numerator.div_ceil(u64::from(state.pixel_rate));
    if ipc_wa { wm /= 2; }
    wm.min(0x1ff) as u16
}

// upstream: intel_display.c hsw_compute_linetime_wm()
pub fn hsw_compute_linetime_wm<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    state.linetime = if caps.display_version >= 9 { skl_linetime_wm(state, false) }
        else { hsw_linetime_wm(state) };
    if !caps.has_ips { return Ok(()); }
    let cdclk = ops.cdclk_khz(state.pipe)?;
    state.ips_linetime = hsw_ips_linetime_wm(state, cdclk);
    Ok(())
}

// upstream: intel_display.c intel_crtc_atomic_check()
pub fn intel_crtc_atomic_check<O: ModesetOps>(
    caps: DisplayCaps, state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if needs_modeset(state) && !state.hw_active && caps.display_version < 5 {
        state.update_wm_post = true;
    }
    if needs_modeset(state) {
        ops.action(Action::DpllGet, Some(state.pipe))?;
    }
    ops.action(Action::ColorCheck, Some(state.pipe))?;
    ops.action(Action::WatermarkCompute, Some(state.pipe))?;
    if caps.display_version >= 9 {
        if needs_modeset(state) || needs_fastset(state) {
            ops.action(Action::ScalerUpdate, Some(state.pipe))?;
        }
        ops.action(Action::ScalerSetup, Some(state.pipe))?;
    }
    if caps.has_ips {
        ops.action(Action::IpsCompute, Some(state.pipe))?;
    }
    if caps.display_version >= 9 || caps.broadwell || caps.haswell {
        hsw_compute_linetime_wm(caps, state, ops)?;
    }
    ops.action(Action::Psr2SelFetchUpdate, Some(state.pipe))?;
    Ok(())
}

// upstream: intel_display.c bpc_to_bpp()
pub fn bpc_to_bpp(bpc: i32) -> Result<u8, ModesetError> {
    match bpc {
        6..=7 => Ok(18),
        8..=9 => Ok(24),
        10..=11 => Ok(30),
        12..=16 => Ok(36),
        _ => Err(ModesetError::Invalid),
    }
}

// upstream: intel_display.c compute_sink_pipe_bpp()
pub fn compute_sink_pipe_bpp(
    connector: &EncoderTransition, crtc_state: &mut PipeState,
) -> Result<(), ModesetError> {
    let edid_bpc = if connector.edid_bpc == 0 { 8 } else { connector.edid_bpc };
    let max_edid_bpp = bpc_to_bpp(i32::from(edid_bpc))?;
    let target_pipe_bpp = bpc_to_bpp(i32::from(connector.max_bpc))?;
    crtc_state.max_pipe_bpp = crtc_state.pipe_bpp.min(max_edid_bpp);
    if target_pipe_bpp < crtc_state.pipe_bpp {
        crtc_state.pipe_bpp = target_pipe_bpp;
    }
    Ok(())
}

// upstream: intel_display.c intel_display_min_pipe_bpp()
pub fn intel_display_min_pipe_bpp() -> u8 { 18 }

// upstream: intel_display.c intel_display_max_pipe_bpp()
pub fn intel_display_max_pipe_bpp(caps: DisplayCaps) -> u8 {
    if caps.g4x || caps.valleyview || caps.cherryview { 30 }
    else if caps.display_version >= 5 { 36 }
    else { 24 }
}

// upstream: intel_display.c compute_baseline_pipe_bpp()
pub fn compute_baseline_pipe_bpp(
    state: &AtomicState, crtc_state: &mut PipeState, caps: DisplayCaps,
) -> Result<(), ModesetError> {
    crtc_state.pipe_bpp = intel_display_max_pipe_bpp(caps);
    for connector in &state.encoders {
        if connector.new_crtc == Some(crtc_state.pipe) {
            compute_sink_pipe_bpp(connector, crtc_state)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c check_digital_port_conflicts()
pub fn check_digital_port_conflicts(state: &AtomicState, caps: DisplayCaps) -> bool {
    let mut used_ports = 0u16;
    let mut used_mst_ports = 0u16;
    for connector in &state.encoders {
        if !connector.has_best_encoder || connector.new_crtc.is_none() { continue; }
        let bit = 1u16.checked_shl(u32::from(connector.port)).unwrap_or(0);
        match connector.kind {
            EncoderKind::Ddi if !caps.has_ddi => continue,
            EncoderKind::Ddi | EncoderKind::Dp | EncoderKind::Hdmi | EncoderKind::Edp => {
                if used_ports & bit != 0 { return false; }
                used_ports |= bit;
            }
            EncoderKind::DpMst => used_mst_ports |= bit,
            EncoderKind::Other => {}
        }
    }
    used_ports & used_mst_ports == 0
}

// upstream: intel_display.c intel_crtc_copy_uapi_to_hw_state_nomodeset()
pub fn intel_crtc_copy_uapi_to_hw_state_nomodeset<O: ModesetOps>(
    state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.is_joiner_secondary { ops.action(Action::VerifyCrtc, Some(state.pipe))?; }
    state.hw_color_luts = state.uapi_color_luts;
    state.hw_background_color = state.uapi_background_color;
    Ok(())
}

// upstream: intel_display.c intel_crtc_copy_uapi_to_hw_state_modeset()
pub fn intel_crtc_copy_uapi_to_hw_state_modeset<O: ModesetOps>(
    state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.is_joiner_secondary { ops.action(Action::VerifyCrtc, Some(state.pipe))?; }
    state.hw_enable = state.uapi_enable;
    state.hw_active = state.uapi_active;
    state.hw_mode = state.uapi_mode;
    state.hw_adjusted_mode = state.adjusted_mode;
    state.hw_scaling_filter = state.uapi_scaling_filter;
    state.hw_sharpness_strength = state.uapi_sharpness_strength;
    intel_crtc_copy_uapi_to_hw_state_nomodeset(state, ops)
}

// upstream: intel_display.c copy_joiner_crtc_state_nomodeset()
pub fn copy_joiner_crtc_state_nomodeset(
    state: &mut AtomicState, secondary_index: usize,
) -> Result<(), ModesetError> {
    let secondary = state.pipes.get(secondary_index).ok_or(ModesetError::Invalid)?.new;
    let primary_pipe = intel_primary_crtc_pipe(&secondary);
    let primary = state.pipes.iter().find(|tr| tr.new.pipe == primary_pipe)
        .ok_or(ModesetError::Invalid)?.new;
    let target = &mut state.pipes[secondary_index].new;
    target.hw_color_luts = primary.hw_color_luts;
    target.hw_background_color = primary.hw_background_color;
    target.uapi_color_mgmt_changed = primary.uapi_color_mgmt_changed;
    Ok(())
}

// upstream: intel_display.c copy_joiner_crtc_state_modeset()
pub fn copy_joiner_crtc_state_modeset<O: ModesetOps>(
    state: &mut AtomicState, secondary_index: usize, ops: &mut O,
) -> Result<(), ModesetError> {
    let saved = state.pipes.get(secondary_index).ok_or(ModesetError::Invalid)?.new;
    let primary_pipe = intel_primary_crtc_pipe(&saved);
    let primary = state.pipes.iter().find(|tr| tr.new.pipe == primary_pipe)
        .ok_or(ModesetError::Invalid)?.new;
    let mut copied = primary;
    copied.pipe = saved.pipe;
    copied.uapi_enable = saved.uapi_enable;
    copied.uapi_active = saved.uapi_active;
    copied.uapi_async_flip = saved.uapi_async_flip;
    copied.uapi_mode = saved.uapi_mode;
    copied.uapi_scaling_filter = saved.uapi_scaling_filter;
    copied.uapi_sharpness_strength = saved.uapi_sharpness_strength;
    copied.has_intel_dpll = saved.has_intel_dpll;
    copied.dpll_hw_state = saved.dpll_hw_state;
    copied.update_planes = saved.update_planes;
    copied.async_flip_planes = saved.async_flip_planes;
    copied.do_async_flip = saved.do_async_flip;
    copied.needs_modeset = primary.needs_modeset;
    copied.mode_changed = primary.mode_changed;
    state.pipes[secondary_index].new = copied;
    intel_crtc_copy_uapi_to_hw_state_modeset(&mut state.pipes[secondary_index].new, ops)?;
    copy_joiner_crtc_state_nomodeset(state, secondary_index)
}

// upstream: intel_display.c intel_crtc_prepare_cleared_state()
pub fn intel_crtc_prepare_cleared_state<O: ModesetOps>(
    state: &mut AtomicState, index: usize, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let old = state.pipes.get(index).ok_or(ModesetError::Invalid)?.new;
    ops.action(Action::AllocateCrtcScratchState, Some(old.pipe))?;
    ops.action(Action::FreeCrtcHwState, Some(old.pipe))?;
    if let Err(err) = ops.action(Action::ClearDpTunnelStreamBandwidth, Some(old.pipe)) {
        let _ = ops.action(Action::FreeCrtcScratchState, Some(old.pipe));
        return Err(err);
    }
    let mut preserved = PipeState::default();
    preserved.pipe = old.pipe;
    preserved.uapi_enable = old.uapi_enable;
    preserved.uapi_active = old.uapi_active;
    preserved.uapi_async_flip = old.uapi_async_flip;
    preserved.uapi_mode = old.uapi_mode;
    preserved.uapi_scaling_filter = old.uapi_scaling_filter;
    preserved.uapi_sharpness_strength = old.uapi_sharpness_strength;
    preserved.uapi_connectors_changed = old.uapi_connectors_changed;
    preserved.uapi_color_mgmt_changed = old.uapi_color_mgmt_changed;
    preserved.uapi_color_luts = old.uapi_color_luts;
    preserved.uapi_background_color = old.uapi_background_color;
    preserved.inherited = old.inherited;
    preserved.scaler_state = old.scaler_state;
    preserved.has_intel_dpll = old.has_intel_dpll;
    preserved.dpll_hw_state = old.dpll_hw_state;
    preserved.crc_enabled = old.crc_enabled;
    if caps.g4x || caps.valleyview || caps.cherryview { preserved.wm_state = old.wm_state; }
    state.pipes[index].new = preserved;
    ops.action(Action::FreeCrtcScratchState, Some(old.pipe))?;
    intel_crtc_copy_uapi_to_hw_state_modeset(&mut state.pipes[index].new, ops)
}
// upstream: intel_display.c intel_modeset_pipe_config()
pub fn intel_modeset_pipe_config<O: ModesetOps>(
    state: &mut AtomicState, index: usize, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let mut crtc = state.pipes.get(index).ok_or(ModesetError::Invalid)?.new;
    crtc.cpu_transcoder = crtc.pipe;
    crtc.framestart_delay = 1;
    crtc.output_types = 0;
    compute_baseline_pipe_bpp(state, &mut crtc, caps)?;
    crtc.pipe_src_width = crtc.uapi_mode.hdisplay;
    crtc.pipe_src_height = crtc.uapi_mode.vdisplay;
    for encoder in state.encoders.iter().filter(|e| e.new_crtc == Some(crtc.pipe)) {
        if !check_single_encoder_cloning(state, crtc.pipe, encoder) {
            return Err(ModesetError::Invalid);
        }
        crtc.output_types |= encoder.output_type;
    }
    crtc.port_clock = 0;
    crtc.pixel_multiplier = 1;
    crtc.adjusted_mode = crtc.uapi_mode;
    ops.action(Action::InitializeAdjustedMode, Some(crtc.pipe))?;
    for encoder in state.encoders.iter().filter(|e| e.new_crtc == Some(crtc.pipe)) {
        if encoder.hook_mask & ENCODER_HOOK_COMPUTE_CONFIG != 0 {
            ops.encoder_action(Action::EncoderComputeConfigHook, crtc.pipe, encoder.encoder_id)?;
        }
    }
    if crtc.port_clock == 0 {
        crtc.port_clock = crtc.adjusted_mode.clock_khz.saturating_mul(u32::from(crtc.pixel_multiplier));
    }
    intel_crtc_compute_config(caps, &mut crtc, ops)?;
    crtc.dither = crtc.pipe_bpp == 18 && !crtc.dither_force_disable;
    state.pipes[index].new = crtc;
    Ok(())
}

// upstream: intel_display.c intel_modeset_pipe_config_late()
pub fn intel_modeset_pipe_config_late<O: ModesetOps>(
    state: &AtomicState, index: usize, ops: &mut O,
) -> Result<(), ModesetError> {
    let pipe = state.pipes.get(index).ok_or(ModesetError::Invalid)?.new.pipe;
    for encoder in state.encoders.iter().filter(|e| e.new_crtc == Some(pipe)) {
        if encoder.hook_mask & ENCODER_HOOK_COMPUTE_CONFIG_LATE != 0 {
            ops.encoder_action(Action::EncoderComputeConfigLateHook, pipe, encoder.encoder_id)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_fuzzy_clock_check()
pub fn intel_fuzzy_clock_check(clock1: u32, clock2: u32) -> bool {
    if clock1 == clock2 { return true; }
    if clock1 == 0 || clock2 == 0 { return false; }
    let diff = clock1.abs_diff(clock2);
    ((u64::from(diff) + u64::from(clock1) + u64::from(clock2)) * 100 /
        (u64::from(clock1) + u64::from(clock2))) < 105
}

// upstream: intel_display.c intel_compare_link_m_n()
pub fn intel_compare_link_m_n(a: &LinkMn, b: &LinkMn) -> bool { a == b }

// upstream: intel_display.c intel_compare_infoframe()
pub fn intel_compare_infoframe(a: &[u8], b: &[u8]) -> bool { a == b }

// upstream: intel_display.c intel_compare_dp_vsc_sdp()
pub fn intel_compare_dp_vsc_sdp(a: &DpVscSdp, b: &DpVscSdp) -> bool {
    a.pixelformat == b.pixelformat && a.colorimetry == b.colorimetry && a.bpc == b.bpc &&
        a.dynamic_range == b.dynamic_range && a.content_type == b.content_type
}

// upstream: intel_display.c intel_compare_dp_as_sdp()
pub fn intel_compare_dp_as_sdp(a: &DpAsSdp, b: &DpAsSdp) -> bool { a == b }

// upstream: intel_display.c intel_compare_buffer()
pub fn intel_compare_buffer(a: &[u8], b: &[u8], len: usize) -> bool {
    match (a.get(..len), b.get(..len)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

// upstream: intel_display.c memcmp_diff_len()
pub fn memcmp_diff_len(a: &[u8], b: &[u8], len: usize) -> usize {
    let limit = len.min(a.len()).min(b.len());
    for i in (0..limit).rev() {
        if a[i] != b[i] { return i + 1; }
    }
    0
}

// upstream: intel_display.c allow_vblank_delay_fastset()
pub fn allow_vblank_delay_fastset(caps: DisplayCaps, old: &PipeState) -> bool {
    caps.has_lrr && (old.inherited || old.vrr_always_use_vrr_tg) && !old.is_dsi_output
}

fn timing_config_equal(a: Timing, b: Timing, fastset: bool, allow_vblank: bool, update_lrr: bool) -> bool {
    a.hdisplay == b.hdisplay && a.htotal == b.htotal &&
        a.hblank_start == b.hblank_start && a.hblank_end == b.hblank_end &&
        a.hsync_start == b.hsync_start && a.hsync_end == b.hsync_end &&
        a.vdisplay == b.vdisplay && a.vsync_start == b.vsync_start && a.vsync_end == b.vsync_end &&
        (fastset && allow_vblank || a.vblank_start == b.vblank_start) &&
        (fastset && update_lrr || (a.vtotal == b.vtotal && a.vblank_end == b.vblank_end))
}

// upstream: intel_display.c intel_pipe_config_compare()
pub fn intel_pipe_config_compare(
    caps: DisplayCaps, current: &PipeState, proposed: &PipeState, fastset: bool,
) -> bool {
    if current.hw_enable != proposed.hw_enable || current.hw_active != proposed.hw_active ||
        current.cpu_transcoder != proposed.cpu_transcoder ||
        current.mst_master_transcoder != proposed.mst_master_transcoder ||
        current.has_pch_encoder != proposed.has_pch_encoder ||
        current.lane_count != proposed.lane_count ||
        current.lane_lat_optim_mask != proposed.lane_lat_optim_mask ||
        current.min_hblank != proposed.min_hblank || current.output_types != proposed.output_types ||
        current.framestart_delay != proposed.framestart_delay ||
        current.msa_timing_delay != proposed.msa_timing_delay ||
        current.pixel_multiplier != proposed.pixel_multiplier ||
        current.output_format != proposed.output_format ||
        current.has_hdmi_sink != proposed.has_hdmi_sink ||
        current.hdmi_scrambling != proposed.hdmi_scrambling ||
        current.hdmi_high_tmds_clock_ratio != proposed.hdmi_high_tmds_clock_ratio ||
        current.enhanced_framing != proposed.enhanced_framing ||
        current.fec_enable != proposed.fec_enable ||
        current.joiner_pipes != proposed.joiner_pipes ||
        current.set_context_latency != proposed.set_context_latency {
        return false;
    }
    if !caps.has_double_buffered_m_n || !fastset || !proposed.update_m_n {
        if current.compare_data.dp_m_n != proposed.compare_data.dp_m_n { return false; }
    }
    if current.compare_data.always_equal != proposed.compare_data.always_equal { return false; }
    let allow_vblank = fastset && allow_vblank_delay_fastset(caps, current);
    if !timing_config_equal(current.pipe_mode, proposed.pipe_mode, fastset, allow_vblank, proposed.update_lrr) ||
        !timing_config_equal(current.adjusted_mode, proposed.adjusted_mode, fastset, allow_vblank, proposed.update_lrr) {
        return false;
    }
    if fastset && proposed.update_m_n == false {
        if current.pipe_mode.clock_khz != proposed.pipe_mode.clock_khz ||
            current.adjusted_mode.clock_khz != proposed.adjusted_mode.clock_khz { return false; }
    } else if !fastset && (current.pipe_mode.clock_khz != proposed.pipe_mode.clock_khz ||
        current.adjusted_mode.clock_khz != proposed.adjusted_mode.clock_khz) {
        return false;
    }
    if !fastset && current.compare_data.full_modeset_only != proposed.compare_data.full_modeset_only {
        return false;
    }
    if !fastset || current.vrr_always_use_vrr_tg || proposed.vrr_always_use_vrr_tg {
        if current.vrr_pipeline_full != proposed.vrr_pipeline_full ||
            current.vrr_guardband != proposed.vrr_guardband { return false; }
    }
    if !fastset && (current.vrr_enabled != proposed.vrr_enabled ||
        current.vrr_vmin != proposed.vrr_vmin || current.vrr_vmax != proposed.vrr_vmax ||
        current.vrr_flipline != proposed.vrr_flipline ||
        current.vrr_vsync_start != proposed.vrr_vsync_start ||
        current.vrr_vsync_end != proposed.vrr_vsync_end ||
        current.cmrr_m != proposed.cmrr_m || current.cmrr_n != proposed.cmrr_n ||
        current.cmrr_enabled != proposed.cmrr_enabled) { return false; }
    true
}

// upstream: intel_display.c intel_verify_planes()
pub fn intel_verify_planes<O: ModesetOps>(
    state: &AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    for plane in &state.planes {
        let action = if plane.is_y_plane || plane.new_visible {
            Action::VerifyPlaneEnabled
        } else {
            Action::VerifyPlaneDisabled
        };
        ops.plane_action(action, plane.pipe, plane.id)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_modeset_pipe()
pub fn intel_modeset_pipe<O: ModesetOps>(
    state: &mut AtomicState, index: usize, ops: &mut O,
) -> Result<(), ModesetError> {
    let pipe = state.pipes.get(index).ok_or(ModesetError::Invalid)?.new.pipe;
    ops.action(Action::AddAffectedConnectors, Some(pipe))?;
    ops.action(Action::AddDpTunnelState, Some(pipe))?;
    ops.action(Action::AddMstTopologyState, Some(pipe))?;
    ops.action(Action::AddAffectedPlanes, Some(pipe))?;
    state.pipes[index].new.mode_changed = true;
    Ok(())
}

// upstream: intel_display.c intel_modeset_pipes_in_mask_early()
pub fn intel_modeset_pipes_in_mask_early<O: ModesetOps>(
    state: &mut AtomicState, mask: u8, ops: &mut O,
) -> Result<(), ModesetError> {
    for index in 0..state.pipes.len() {
        let new = state.pipes[index].new;
        if mask & pipe_bit(new.pipe) == 0 || !new.hw_enable || needs_modeset(&new) { continue; }
        intel_modeset_pipe(state, index, ops)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_crtc_flag_modeset()
pub fn intel_crtc_flag_modeset(state: &mut PipeState) {
    state.mode_changed = true;
    state.update_pipe = false;
    state.update_m_n = false;
    state.update_lrr = false;
}

// upstream: intel_display.c intel_modeset_all_pipes_late()
pub fn intel_modeset_all_pipes_late<O: ModesetOps>(
    state: &mut AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    for index in 0..state.pipes.len() {
        let new = state.pipes[index].new;
        if !new.hw_active || needs_modeset(&new) { continue; }
        intel_modeset_pipe(state, index, ops)?;
        let new = &mut state.pipes[index].new;
        intel_crtc_flag_modeset(new);
        new.update_planes |= new.active_planes;
        new.async_flip_planes = 0;
        new.do_async_flip = false;
    }
    Ok(())
}

// upstream: intel_display.c intel_modeset_commit_pipes()
pub fn intel_modeset_commit_pipes<O: ModesetOps>(
    state: &mut AtomicState, mask: u8, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    state.internal = true;
    for tr in &mut state.pipes {
        if mask & pipe_bit(tr.new.pipe) != 0 { tr.new.uapi_connectors_changed = true; }
    }
    intel_atomic_commit(state, caps, ops)
}

// upstream: intel_display.c intel_calc_enabled_pipes()
pub fn intel_calc_enabled_pipes(state: &AtomicState, mut mask: u8) -> u8 {
    for tr in &state.pipes {
        let bit = pipe_bit(tr.new.pipe);
        mask = if tr.new.hw_enable { mask | bit } else { mask & !bit };
    }
    mask
}

// upstream: intel_display.c intel_calc_active_pipes()
pub fn intel_calc_active_pipes(state: &AtomicState, mut mask: u8) -> u8 {
    for tr in &state.pipes {
        let bit = pipe_bit(tr.new.pipe);
        mask = if tr.new.hw_active { mask | bit } else { mask & !bit };
    }
    mask
}

// upstream: intel_display.c intel_modeset_checks()
pub fn intel_modeset_checks<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    state.modeset = true;
    if caps.haswell { ops.action(Action::HaswellModeSetPlanesWorkaround, None)?; }
    Ok(())
}

// upstream: intel_display.c lrr_params_changed()
pub fn lrr_params_changed(old: &PipeState, new: &PipeState) -> bool {
    old.old_timing.vblank_start != new.new_timing.vblank_start ||
        old.old_timing.vblank_end != new.new_timing.vblank_end ||
        old.old_timing.vtotal != new.new_timing.vtotal ||
        old.set_context_latency != new.set_context_latency
}

// upstream: intel_display.c intel_crtc_check_fastset()
pub fn intel_crtc_check_fastset<O: ModesetOps>(
    caps: DisplayCaps, old: &PipeState, new: &mut PipeState, ops: &mut O,
) {
    if old.vrr_in_range != new.vrr_in_range { new.update_lrr = false; }
    if intel_pipe_config_compare(caps, old, new, true) {
        if allow_vblank_delay_fastset(caps, old) { new.update_lrr = true; }
        new.mode_changed = false;
    }
    if ops.link_m_n_equal(old, new) { new.update_m_n = false; }
    if !lrr_params_changed(old, new) { new.update_lrr = false; }
    if needs_modeset(new) { new.mode_changed = true; }
    else { new.update_pipe = true; }
}

// upstream: intel_display.c intel_atomic_check_crtcs()
pub fn intel_atomic_check_crtcs<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    for tr in &mut state.pipes {
        intel_crtc_atomic_check(caps, &mut tr.new, ops)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_cpu_transcoders_need_modeset()
pub fn intel_cpu_transcoders_need_modeset(state: &AtomicState, mask: u8) -> bool {
    state.pipes.iter().any(|tr| tr.new.hw_enable &&
        (mask & pipe_bit(tr.new.cpu_transcoder)) != 0 && needs_modeset(&tr.new))
}

// upstream: intel_display.c intel_pipes_need_modeset()
pub fn intel_pipes_need_modeset(state: &AtomicState, mask: u8) -> bool {
    state.pipes.iter().any(|tr| tr.new.hw_enable &&
        (mask & pipe_bit(tr.new.pipe)) != 0 && needs_modeset(&tr.new))
}
// Pipe configuration and atomic validation (source order from intel_display.c).

// upstream: intel_display.c intel_atomic_check_joiner()
pub fn intel_atomic_check_joiner<O: ModesetOps>(
    state: &mut AtomicState, primary_index: usize, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let primary = state.pipes.get(primary_index).ok_or(ModesetError::Invalid)?.new;
    if primary.joiner_pipes == 0 { return Ok(()); }
    if primary.pipe != joiner_primary_pipe(&primary) ||
        primary.joiner_pipes & !joiner_pipes(caps) != 0 {
        return Err(ModesetError::Invalid);
    }
    let secondaries = intel_crtc_joiner_secondary_pipes(&primary);
    for pipe in 0..8 {
        if secondaries & pipe_bit(pipe) == 0 { continue; }
        let index = state.pipes.iter().position(|tr| tr.new.pipe == pipe)
            .ok_or(ModesetError::Invalid)?;
        if state.pipes[index].new.uapi_enable { return Err(ModesetError::Invalid); }
        state.pipes[index].new.joiner_pipes = primary.joiner_pipes;
        state.pipes[index].new.is_joiner_secondary = true;
        copy_joiner_crtc_state_modeset(state, index, ops)?;
    }
    Ok(())
}

// upstream: intel_display.c kill_joiner_secondaries()
pub fn kill_joiner_secondaries<O: ModesetOps>(
    state: &mut AtomicState, primary_index: usize, ops: &mut O,
) -> Result<(), ModesetError> {
    let primary = state.pipes.get(primary_index).ok_or(ModesetError::Invalid)?.new;
    for pipe in 0..8 {
        if intel_crtc_joiner_secondary_pipes(&primary) & pipe_bit(pipe) == 0 { continue; }
        let index = state.pipes.iter().position(|tr| tr.new.pipe == pipe)
            .ok_or(ModesetError::Invalid)?;
        state.pipes[index].new.joiner_pipes = 0;
        state.pipes[index].new.is_joiner_secondary = false;
        intel_crtc_copy_uapi_to_hw_state_modeset(&mut state.pipes[index].new, ops)?;
    }
    state.pipes[primary_index].new.joiner_pipes = 0;
    Ok(())
}

// upstream: intel_display.c intel_async_flip_check_uapi()
pub fn intel_async_flip_check_uapi(
    state: &AtomicState, pipe_index: usize,
) -> Result<(), ModesetError> {
    let crtc = state.pipes.get(pipe_index).ok_or(ModesetError::Invalid)?;
    let new = &crtc.new;
    if !new.uapi_async_flip { return Ok(()); }
    if !new.uapi_active || needs_modeset(new) || new.joiner_pipes != 0 {
        return Err(ModesetError::Invalid);
    }
    for plane in &state.planes {
        if plane.pipe != new.pipe { continue; }
        if !plane.async_flip_capable || !plane.old_fb_exists || !plane.new_fb_exists {
            return Err(ModesetError::Invalid);
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_async_flip_check_hw()
pub fn intel_async_flip_check_hw<O: ModesetOps>(
    state: &AtomicState, pipe_index: usize, ops: &mut O,
) -> Result<(), ModesetError> {
    let crtc = state.pipes.get(pipe_index).ok_or(ModesetError::Invalid)?;
    let old = &crtc.old;
    let new = &crtc.new;
    if !new.uapi_async_flip { return Ok(()); }
    if !new.hw_active || needs_modeset(new) || old.active_planes != new.active_planes {
        return Err(ModesetError::Invalid);
    }
    for plane in &state.planes {
        if plane.pipe != new.pipe { continue; }
        if new.do_async_flip && !plane.async_flip_capable { return Err(ModesetError::Invalid); }
        if !plane.async_flip_capable { continue; }
        if !plane.old_fb_exists || !plane.new_fb_exists ||
            !ops.plane_can_async_flip(plane.id, plane.new_format, plane.new_modifier) {
            return Err(ModesetError::Invalid);
        }
        // The first request is synchronous so the plane may be reconfigured.
        if !new.do_async_flip { continue; }
        if plane.old_mapping_stride != plane.new_mapping_stride ||
            plane.old_modifier != plane.new_modifier || plane.old_format != plane.new_format ||
            plane.old_rotation != plane.new_rotation || plane.old_aux_dist != plane.new_aux_dist ||
            plane.old_src_rect != plane.new_src_rect || plane.old_dst_rect != plane.new_dst_rect ||
            plane.old_alpha != plane.new_alpha ||
            plane.old_pixel_blend_mode != plane.new_pixel_blend_mode ||
            plane.old_color_encoding != plane.new_color_encoding ||
            plane.old_color_range != plane.new_color_range ||
            plane.old_decrypt != plane.new_decrypt {
            return Err(ModesetError::Invalid);
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_joiner_add_affected_crtcs()
pub fn intel_joiner_add_affected_crtcs<O: ModesetOps>(
    state: &mut AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    let mut affected_pipes = 0u8;
    let mut modeset_pipes = 0u8;
    for tr in &state.pipes {
        affected_pipes |= tr.new.joiner_pipes;
        if needs_modeset(&tr.new) { modeset_pipes |= tr.new.joiner_pipes; }
    }
    for pipe in 0..8 {
        if affected_pipes & pipe_bit(pipe) != 0 &&
            !state.pipes.iter().any(|tr| tr.new.pipe == pipe) {
            return Err(ModesetError::Invalid);
        }
    }
    for index in 0..state.pipes.len() {
        let pipe = state.pipes[index].new.pipe;
        if modeset_pipes & pipe_bit(pipe) == 0 { continue; }
        state.pipes[index].new.mode_changed = true;
        state.pipes[index].new.uapi_connectors_changed = true;
        ops.action(Action::AddAffectedConnectors, Some(pipe))?;
        ops.action(Action::AddAffectedPlanes, Some(pipe))?;
    }
    for index in 0..state.pipes.len() {
        let new = state.pipes[index].new;
        if needs_modeset(&new) && intel_crtc_is_joiner_primary(&new) {
            kill_joiner_secondaries(state, index, ops)?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_atomic_check_config()
pub fn intel_atomic_check_config<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), (ModesetError, Option<u8>)> {
    intel_joiner_add_affected_crtcs(state, ops).map_err(|e| (e, None))?;
    ops.action(Action::FdiAddAffectedCrtcs, None).map_err(|e| (e, None))?;
    for index in 0..state.pipes.len() {
        let pipe = state.pipes[index].new.pipe;
        let new = state.pipes[index].new;
        if !needs_modeset(&new) {
            if !new.is_joiner_secondary {
                intel_crtc_copy_uapi_to_hw_state_nomodeset(&mut state.pipes[index].new, ops)
                    .map_err(|e| (e, Some(pipe)))?;
            }
            continue;
        }
        if new.is_joiner_secondary { continue; }
        if let Err(err) = intel_crtc_prepare_cleared_state(state, index, caps, ops) {
            return Err((err, Some(pipe)));
        }
        if !new.hw_enable { continue; }
        if let Err(err) = intel_modeset_pipe_config(state, index, caps, ops) {
            return Err((err, Some(pipe)));
        }
    }
    for index in 0..state.pipes.len() {
        let new = state.pipes[index].new;
        if !needs_modeset(&new) || new.is_joiner_secondary || !new.hw_enable { continue; }
        if let Err(err) = intel_modeset_pipe_config_late(state, index, ops) {
            return Err((err, Some(new.pipe)));
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_atomic_check_config_and_link()
pub fn intel_atomic_check_config_and_link<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::LinkBandwidthInit, None)?;
    loop {
        match intel_atomic_check_config(state, caps, ops) {
            Ok(()) => match ops.action(Action::LinkBandwidthAtomicCheck, None) {
                Err(ModesetError::Again) => continue,
                result => return result,
            },
            Err((ModesetError::Invalid, Some(pipe))) if ops.reduce_link_bpp_limit(pipe) => {
                continue;
            }
            Err((err, _)) => return Err(err),
        }
    }
}

// upstream: intel_display.c intel_atomic_check()
pub fn intel_atomic_check<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::DriverAccess, None)?;
    let result = (|| -> Result<(), ModesetError> {
    for tr in &mut state.pipes {
        if !state.internal { tr.new.inherited = false; }
        if tr.new.inherited != tr.old.inherited ||
            tr.new.uapi_scaling_filter != tr.old.uapi_scaling_filter ||
            tr.new.uapi_sharpness_strength != tr.old.uapi_sharpness_strength {
            tr.new.mode_changed = true;
        }
    }
    ops.action(Action::VrrCheckModeset, None)?;
    ops.framework_check_modeset(state)?;

    for i in 0..state.pipes.len() {
        intel_async_flip_check_uapi(state, i)?;
    }
    intel_atomic_check_config_and_link(state, caps, ops)?;

    for i in 0..state.pipes.len() {
        let new = state.pipes[i].new;
        if !needs_modeset(&new) {
            if new.is_joiner_secondary {
                copy_joiner_crtc_state_nomodeset(state, i)?;
            }
            continue;
        }
        if new.is_joiner_secondary { continue; }
        intel_atomic_check_joiner(state, i, caps, ops)?;
    }
    for tr in &mut state.pipes {
        if !needs_modeset(&tr.new) { continue; }
        intel_joiner_adjust_pipe_src(&mut tr.new);
        intel_crtc_check_fastset(caps, &tr.old, &mut tr.new, ops);
    }

    // External MST, port-sync, and joiner dependencies promote all linked pipes.
    for i in 0..state.pipes.len() {
        if !state.pipes[i].new.hw_enable || needs_modeset(&state.pipes[i].new) { continue; }
        let dependency_modeset = ops.mst_crtc_needs_modeset(state, i) ||
            ops.promote_external_modeset_dependency(state, i);
        if dependency_modeset { state.pipes[i].new.needs_modeset = true; }
    }

    for tr in &state.pipes {
        if needs_modeset(&tr.new) { ops.action(Action::DpllRelease, Some(tr.new.pipe))?; }
    }
    if state.pipes.iter().any(|tr| needs_modeset(&tr.new)) {
        if !check_digital_port_conflicts(state, caps) { return Err(ModesetError::Invalid); }
    }
    ops.action(Action::PlaneAtomicCheck, None)?;
    for tr in &state.pipes {
        if tr.new.hw_enable { ops.action(Action::ComputeMinCdclk, Some(tr.new.pipe))?; }
    }
    ops.action(Action::ComputeGlobalWatermarks, None)?;
    ops.action(Action::BandwidthAtomicCheck, None)?;
    ops.action(Action::CdclkAtomicCheck, None)?;
    if state.pipes.iter().any(|tr| needs_modeset(&tr.new)) {
        intel_modeset_checks(state, caps, ops)?;
    }
    ops.action(Action::PmdemandAtomicCheck, None)?;
    intel_atomic_check_crtcs(state, caps, ops)?;
    ops.action(Action::FbcAtomicCheck, None)?;
    for i in 0..state.pipes.len() {
        let tr = &state.pipes[i];
        ops.action(Action::ColorAssertLuts, Some(tr.new.pipe))?;
        intel_async_flip_check_hw(state, i, ops)?;
        if !needs_modeset(&tr.new) && !needs_fastset(&tr.new) { continue; }
        ops.action(Action::DumpCrtcState, Some(tr.new.pipe))?;
    }
    Ok(())
    })();
    if result == Err(ModesetError::Deadlock) { return result; }
    if result.is_err() {
        for tr in &state.pipes {
            let _ = ops.action(Action::DumpCrtcState, Some(tr.new.pipe));
        }
    }
    result
}

// upstream: intel_display.c intel_atomic_prepare_commit()
pub fn intel_atomic_prepare_commit<O: ModesetOps>(
    state: &AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::PreparePlanes, None)?;
    let _ = state;
    Ok(())
}

// upstream: intel_display.c intel_crtc_arm_fifo_underrun()
pub fn intel_crtc_arm_fifo_underrun<O: ModesetOps>(
    caps: DisplayCaps, state: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if caps.display_version != 2 || state.active_planes != 0 {
        ops.action(Action::EnableCpuFifoUnderrunReporting, Some(state.pipe))?;
    }
    if state.has_pch_encoder {
        ops.action(Action::EnablePchFifoUnderrunReporting, Some(state.cpu_transcoder))?;
    }
    Ok(())
}

// upstream: intel_display.c intel_pipe_fastset()
pub fn intel_pipe_fastset<O: ModesetOps>(
    caps: DisplayCaps, old: &PipeState, new: &PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    intel_set_pipe_src_size(new, ops)?;
    if caps.display_version >= 9 {
        if new.dpt_configure { ops.action(Action::EnableSkylakePfit, Some(new.pipe))?; }
    } else if new.dpt_configure {
        ops.action(Action::EnableIlkPfit, Some(new.pipe))?;
    } else if old.dpt_configure {
        ops.action(Action::DisableIlkPfit, Some(old.pipe))?;
    }
    if caps.display_version >= 9 || caps.broadwell || caps.haswell {
        hsw_set_linetime_wm(new, ops)?;
    }
    if new.update_m_n { intel_cpu_transcoder_set_m1_n1(new, &new.dp_m_n, ops)?; }
    if new.update_lrr { intel_set_transcoder_timings_lrr(caps, new, ops)?; }
    Ok(())
}

// upstream: intel_display.c commit_pipe_pre_planes()
pub fn commit_pipe_pre_planes<O: ModesetOps>(
    caps: DisplayCaps, state: &AtomicState, tr: &PipeTransition, ops: &mut O,
) -> Result<(), ModesetError> {
    let old = &tr.old;
    let new = &tr.new;
    if !needs_modeset(new) {
        if new.color_update { ops.action(Action::ColorCommitArm, Some(new.pipe))?; }
        if caps.display_version >= 9 || caps.broadwell {
            bdw_set_pipe_misc(caps, new, ops)?;
        }
        if needs_fastset(new) { intel_pipe_fastset(caps, old, new, ops)?; }
    }
    ops.action(Action::ProgramPsr2Tracking, Some(new.pipe))?;
    ops.action(Action::UpdateWatermarks, Some(new.pipe))?;
    let _ = state; // State is passed to preserve the upstream call contract.
    Ok(())
}

// upstream: intel_display.c commit_pipe_post_planes()
pub fn commit_pipe_post_planes<O: ModesetOps>(
    caps: DisplayCaps, tr: &PipeTransition, ops: &mut O,
) -> Result<(), ModesetError> {
    let new = &tr.new;
    if caps.display_version >= 9 && !needs_modeset(new) {
        ops.action(Action::DetachScalers, Some(new.pipe))?;
    }
    if !needs_modeset(new) && new.color_update && caps.has_double_buffered_lut {
        ops.action(Action::LoadColorLuts, Some(new.pipe))?;
    }
    if new.vrr_enabling { ops.action(Action::EnableVrr, Some(new.pipe))?; }
    Ok(())
}

// upstream: intel_display.c intel_enable_crtc()
pub fn intel_enable_crtc<O: ModesetOps>(
    state: &AtomicState, tr: &PipeTransition, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    if !needs_modeset(&tr.new) { return Ok(()); }
    for member in state.pipes.iter().rev() {
        if joined_mask(&tr.new) & pipe_bit(member.new.pipe) == 0 { continue; }
        ops.action(Action::UpdateActiveTimings, Some(member.new.pipe))?;
    }
    ops.action(Action::PsrPipeChange, Some(tr.new.pipe))?;
    if caps.display_version >= 9 {
        hsw_crtc_enable(state, &tr.new, caps, ops)?;
    } else {
        ops.action(Action::EnableCrtc, Some(tr.new.pipe))?;
    }
    ops.action(Action::WaitNextVblank, Some(tr.new.pipe))?;
    ops.action(Action::SetPipeCrcEnabled, Some(tr.new.pipe))?;
    Ok(())
}
// Atomic commit sequencing: retain the source's reverse/joiner and vblank order.

// upstream: intel_display.c intel_pre_update_crtc()
pub fn intel_pre_update_crtc<O: ModesetOps>(
    caps: DisplayCaps, tr: &PipeTransition, state: &AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    let old = &tr.old;
    let new = &tr.new;
    let modeset = needs_modeset(new);
    if (old.inherited || modeset) && caps.has_dpt {
        ops.action(Action::ConfigureDpt, Some(new.pipe))?;
    }
    if !modeset {
        if new.preload_luts && new.color_update {
            ops.action(Action::PreloadColorLuts, Some(new.pipe))?;
        }
        intel_pre_plane_update(state, tr, ops)?;
        if needs_fastset(new) {
            intel_encoders_update_pipe(state, new.pipe, ops)?;
        }
        if caps.display_version >= 11 && needs_fastset(new) {
            icl_set_pipe_chicken(caps, new, ops)?;
        }
        if vrr_params_changed(old, new) || cmrr_params_changed(old, new) {
            ops.action(Action::SetVrrTranscoderTimings, Some(new.pipe))?;
        }
    }
    ops.action(Action::UpdateFbc, Some(new.pipe))?;
    if !modeset && new.color_update && !new.use_dsb && !new.use_flipq {
        ops.action(Action::ColorCommitNoarm, Some(new.pipe))?;
    }
    if !new.use_dsb && !new.use_flipq {
        ops.action(Action::PlanesUpdateNoarm, Some(new.pipe))?;
    }
    let _ = state;
    Ok(())
}

// upstream: intel_display.c intel_update_crtc()
pub fn intel_update_crtc<O: ModesetOps>(
    caps: DisplayCaps, tr: &PipeTransition, state: &AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    let old = &tr.old;
    let new = &tr.new;
    if new.use_flipq {
        ops.action(Action::FlipqEnable, Some(new.pipe))?;
        ops.action(Action::PrepareVblankEvent, Some(new.pipe))?;
        ops.action(Action::FlipqAdd, Some(new.pipe))?;
    } else if new.use_dsb {
        ops.action(Action::PrepareVblankEvent, Some(new.pipe))?;
        ops.action(Action::DsbCommit, Some(new.pipe))?;
    } else {
        ops.action(Action::PipeUpdateStart, Some(new.pipe))?;
        if new.has_dsb_commit { ops.action(Action::DsbCommit, Some(new.pipe))?; }
        commit_pipe_pre_planes(caps, state, tr, ops)?;
        ops.action(Action::PlanesUpdateArm, Some(new.pipe))?;
        commit_pipe_post_planes(caps, tr, ops)?;
        ops.action(Action::PipeUpdateEnd, Some(new.pipe))?;
    }
    if new.vrr_enabling || new.update_m_n || new.update_lrr_timing {
        ops.action(Action::UpdateActiveTimings, Some(new.pipe))?;
    }
    if new.vrr_dcb_enabled { ops.action(Action::IncrementVrrFlipCount, Some(new.pipe))?; }
    if needs_fastset(new) && old.inherited_first_fastset {
        intel_crtc_arm_fifo_underrun(caps, new, ops)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_old_crtc_state_disables()
pub fn intel_old_crtc_state_disables<O: ModesetOps>(
    state: &mut AtomicState, index: usize, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let old = state.pipes[index].old;
    let mask = joined_mask(&old);
    for tr in &state.pipes {
        if mask & pipe_bit(tr.old.pipe) != 0 {
            ops.action(Action::DisablePipeCrc, Some(tr.old.pipe))?;
        }
    }
    ops.action(Action::PsrPipeChange, Some(old.pipe))?;
    if caps.display_version >= 9 {
        hsw_crtc_disable(state, &old, ops)?;
    } else {
        ops.action(Action::DisableCrtc, Some(old.pipe))?;
    }
    for tr in &mut state.pipes {
        if mask & pipe_bit(tr.old.pipe) == 0 { continue; }
        tr.old.hw_active = false;
        ops.action(Action::MarkPipeInactive, Some(tr.old.pipe))?;
        ops.action(Action::DisableFbc, Some(tr.old.pipe))?;
        if !tr.new.hw_active {
            ops.action(Action::InitialWatermarks, Some(tr.old.pipe))?;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_commit_modeset_disables()
pub fn intel_commit_modeset_disables<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let mut disable_pipes = 0u8;
    for i in 0..state.pipes.len() {
        if !needs_modeset(&state.pipes[i].new) { continue; }
        intel_pre_plane_update(state, &state.pipes[i], ops)?;
        if state.pipes[i].old.hw_active {
            disable_pipes |= pipe_bit(state.pipes[i].old.pipe);
        }
    }
    for tr in &state.pipes {
        if disable_pipes & pipe_bit(tr.old.pipe) == 0 { continue; }
        intel_crtc_disable_planes(state, tr, ops)?;
        ops.action(Action::FlushVblankWork, Some(tr.old.pipe))?;
    }
    // Port-sync and MST slaves are disabled before their masters.
    for i in 0..state.pipes.len() {
        let old = state.pipes[i].old;
        if disable_pipes & pipe_bit(old.pipe) == 0 || old.is_joiner_secondary ||
            (!old.port_sync_slave && !old.mst_slave) { continue; }
        intel_old_crtc_state_disables(state, i, caps, ops)?;
        disable_pipes &= !joined_mask(&old);
    }
    // Then disable every remaining primary pipe.
    for i in 0..state.pipes.len() {
        let old = state.pipes[i].old;
        if disable_pipes & pipe_bit(old.pipe) == 0 || old.is_joiner_secondary { continue; }
        intel_old_crtc_state_disables(state, i, caps, ops)?;
        disable_pipes &= !joined_mask(&old);
    }
    if disable_pipes != 0 { ops.action(Action::VerifyDisabledModeset, None)?; }
    Ok(())
}

// upstream: intel_display.c intel_commit_modeset_enables()
pub fn intel_commit_modeset_enables<O: ModesetOps>(
    state: &AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    for tr in &state.pipes {
        if !tr.new.hw_active { continue; }
        intel_enable_crtc(state, tr, caps, ops)?;
        intel_pre_update_crtc(caps, tr, state, ops)?;
    }
    for tr in &state.pipes {
        if tr.new.hw_active { intel_update_crtc(caps, tr, state, ops)?; }
    }
    Ok(())
}

// upstream: intel_display.c skl_commit_modeset_enables()
pub fn skl_commit_modeset_enables<O: ModesetOps>(
    state: &AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    const MAX_PIPES: usize = 8;
    let mut entries = alloc::vec![DdbEntry::default(); MAX_PIPES];
    let mut update_pipes = 0u8;
    let mut modeset_pipes = 0u8;
    for tr in &state.pipes {
        let pipe = tr.new.pipe;
        if !tr.new.hw_active { continue; }
        if !needs_modeset(&tr.new) {
            if usize::from(pipe) < MAX_PIPES { entries[usize::from(pipe)] = tr.old_ddb(); }
            update_pipes |= pipe_bit(pipe);
        } else {
            modeset_pipes |= pipe_bit(pipe);
        }
    }
    for tr in &state.pipes {
        if update_pipes & pipe_bit(tr.new.pipe) != 0 {
            intel_pre_update_crtc(caps, tr, state, ops)?;
        }
    }
    ops.action(Action::DbufPrePlaneUpdate, None)?;
    while update_pipes != 0 {
        let mut progressed = false;
        for tr in state.pipes.iter().rev() {
            let pipe = tr.new.pipe;
            if update_pipes & pipe_bit(pipe) == 0 { continue; }
            if ops.ddb_overlaps(tr.new.ddb, &entries, pipe) { continue; }
            if usize::from(pipe) < MAX_PIPES { entries[usize::from(pipe)] = tr.new.ddb; }
            update_pipes &= !pipe_bit(pipe);
            intel_update_crtc(caps, tr, state, ops)?;
            progressed = true;
            if tr.new.ddb != tr.old.ddb && (update_pipes | modeset_pipes) != 0 {
                ops.action(Action::WaitNextVblank, Some(pipe))?;
            }
        }
        if !progressed { return Err(ModesetError::Invalid); }
    }
    ops.action(Action::DbufPostPlaneUpdate, None)?;
    update_pipes = modeset_pipes;
    for tr in &state.pipes {
        let pipe = tr.new.pipe;
        if modeset_pipes & pipe_bit(pipe) == 0 || tr.new.is_joiner_secondary ||
            tr.new.mst_slave || tr.new.port_sync_master { continue; }
        modeset_pipes &= !joined_mask(&tr.new);
        intel_enable_crtc(state, tr, caps, ops)?;
    }
    for tr in &state.pipes {
        let pipe = tr.new.pipe;
        if modeset_pipes & pipe_bit(pipe) == 0 || tr.new.is_joiner_secondary { continue; }
        modeset_pipes &= !joined_mask(&tr.new);
        intel_enable_crtc(state, tr, caps, ops)?;
    }
    for tr in &state.pipes {
        if update_pipes & pipe_bit(tr.new.pipe) != 0 {
            intel_pre_update_crtc(caps, tr, state, ops)?;
        }
    }
    for tr in state.pipes.iter().rev() {
        let pipe = tr.new.pipe;
        if update_pipes & pipe_bit(pipe) == 0 { continue; }
        if ops.ddb_overlaps(tr.new.ddb, &entries, pipe) {
            ops.action(Action::VerifyCrtc, Some(pipe))?;
        }
        if usize::from(pipe) < MAX_PIPES { entries[usize::from(pipe)] = tr.new.ddb; }
        update_pipes &= !pipe_bit(pipe);
        intel_update_crtc(caps, tr, state, ops)?;
    }
    if modeset_pipes != 0 || update_pipes != 0 {
        ops.action(Action::VerifyDisabledModeset, None)?;
    }
    Ok(())
}

impl PipeTransition {
    fn old_ddb(&self) -> DdbEntry { self.old.ddb }
}
// Atomic commit tail and ioctl-facing glue.

// upstream: intel_display.c intel_atomic_commit_fence_wait()
pub fn intel_atomic_commit_fence_wait<O: ModesetOps>(
    state: &mut AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    for plane in &mut state.planes {
        if !plane.fence_pending { continue; }
        if !ops.action_condition(Action::FenceWaitPlane, plane.pipe)? { break; }
        ops.plane_action(Action::FencePutPlane, plane.pipe, plane.id)?;
        plane.fence_pending = false;
    }
    Ok(())
}

// upstream: intel_display.c intel_atomic_dsb_wait_commit()
pub fn intel_atomic_dsb_wait_commit<O: ModesetOps>(
    state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.has_dsb_commit { ops.action(Action::DsbWaitCommit, Some(state.pipe))?; }
    ops.action(Action::ColorWaitCommit, Some(state.pipe))
}

// upstream: intel_display.c intel_atomic_dsb_cleanup()
pub fn intel_atomic_dsb_cleanup<O: ModesetOps>(
    state: &mut PipeState, ops: &mut O,
) -> Result<(), ModesetError> {
    if state.has_dsb_commit {
        ops.action(Action::DsbCleanup, Some(state.pipe))?;
        state.has_dsb_commit = false;
    }
    ops.action(Action::ColorCleanupCommit, Some(state.pipe))
}

// upstream: intel_display.c intel_atomic_cleanup_work()
pub fn intel_atomic_cleanup_work<O: ModesetOps>(
    state: &mut AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    for tr in &mut state.pipes {
        intel_atomic_dsb_cleanup(&mut tr.old, ops)?;
    }
    ops.action(Action::CleanupPlanes, None)?;
    ops.action(Action::CommitCleanupDone, None)?;
    ops.action(Action::AtomicCommitPut, None)
}

// upstream: intel_display.c intel_atomic_prepare_plane_clear_colors()
pub fn intel_atomic_prepare_plane_clear_colors<O: ModesetOps>(
    state: &mut AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    for plane in &mut state.planes {
        if !plane.new_fb_exists || !plane.clear_color_plane { continue; }
        if let Ok(Some(value)) = ops.read_plane_clear_color(plane.pipe, plane.id) {
            plane.clear_color_value = value;
        }
    }
    Ok(())
}

// upstream: intel_display.c intel_atomic_dsb_prepare()
pub fn intel_atomic_dsb_prepare<O: ModesetOps>(
    state: &mut AtomicState, index: usize, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let tr = state.pipes.get_mut(index).ok_or(ModesetError::Invalid)?;
    let crtc = &mut tr.new;
    if !crtc.hw_active || state.legacy_cursor_update { return Ok(()); }
    crtc.use_flipq = caps.supports_flipq && !crtc.do_async_flip && !crtc.vrr_enabled &&
        !crtc.has_psr && !needs_modeset(crtc) && !needs_fastset(crtc) && !crtc.color_update;
    crtc.use_dsb = !crtc.use_flipq && !crtc.do_async_flip &&
        (caps.display_version >= 20 || !crtc.has_psr) &&
        !needs_modeset(crtc) && !needs_fastset(crtc);
    ops.action(Action::ColorPrepareCommit, Some(crtc.pipe))
}

// upstream: intel_display.c intel_atomic_dsb_finish()
pub fn intel_atomic_dsb_finish<O: ModesetOps>(
    state: &mut AtomicState, index: usize, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let crtc = &mut state.pipes.get_mut(index).ok_or(ModesetError::Invalid)?.new;
    let pipe = crtc.pipe;
    if !crtc.use_flipq && !crtc.use_dsb && !crtc.has_dsb_color { return Ok(()); }
    let size = if crtc.plane_color_changed { 8192 } else { 1024 };
    let alloc_size = if crtc.use_flipq || crtc.use_dsb { size } else { 16 };
    if !ops.prepare_dsb(pipe, alloc_size)? {
        crtc.use_flipq = false;
        crtc.use_dsb = false;
        crtc.has_dsb_commit = false;
        ops.action(Action::ColorCleanupCommit, Some(pipe))?;
        return Ok(());
    }
    crtc.has_dsb_commit = true;
    if crtc.use_flipq || crtc.use_dsb {
        if crtc.use_flipq { ops.action(Action::DsbDmcHalt, Some(pipe))?; }
        if crtc.vrr_dcb_enabled { ops.action(Action::ResetVrrDcb, Some(pipe))?; }
        if crtc.color_update { ops.action(Action::ColorCommitNoarm, Some(pipe))?; }
        ops.action(Action::PlanesUpdateNoarm, Some(pipe))?;
        if crtc.has_psr { ops.action(Action::DsbFrameChange, Some(pipe))?; }
        if crtc.use_dsb { ops.action(Action::DsbVblankEvade, Some(pipe))?; }
        if crtc.color_update { ops.action(Action::ColorCommitArm, Some(pipe))?; }
        bdw_set_pipe_misc(caps, crtc, ops)?;
        ops.action(Action::ProgramPsr2Tracking, Some(pipe))?;
        ops.action(Action::PlanesUpdateArm, Some(pipe))?;
        if caps.display_version >= 9 { ops.action(Action::DetachScalers, Some(pipe))?; }
        if crtc.use_flipq { ops.action(Action::DsbDmcUnhalt, Some(pipe))?; }
    }
    if crtc.uses_chained_dsb {
        ops.action(Action::DsbChain, Some(pipe))?;
    } else if crtc.uses_gosub_dsb {
        ops.action(Action::DsbGosub, Some(pipe))?;
    }
    if crtc.use_dsb && !crtc.uses_chained_dsb {
        if !crtc.psr_use_trans_push { ops.action(Action::DsbWaitVblanks, Some(pipe))?; }
        ops.action(Action::DsbVrrPush, Some(pipe))?;
        ops.action(Action::DsbPsrWaitIdle, Some(pipe))?;
        if crtc.psr_use_trans_push { ops.action(Action::DsbWaitVblanks, Some(pipe))?; }
        ops.action(Action::DsbWaitCommit, Some(pipe))?;
        ops.action(Action::VrrCheckPushSent, Some(pipe))?;
        if crtc.vrr_dcb_enabled { ops.action(Action::EnableVrr, Some(pipe))?; }
        ops.action(Action::DsbInterrupt, Some(pipe))?;
    }
    ops.action(Action::DsbFinish, Some(pipe))
}

// upstream: intel_display.c intel_atomic_commit_tail()
pub fn intel_atomic_commit_tail<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    let mut put_domains = [0u32; 8];
    for index in 0..state.pipes.len() {
        intel_atomic_dsb_prepare(state, index, caps, ops)?;
    }
    intel_atomic_commit_fence_wait(state, ops)?;
    ops.action(Action::DisplayTdFlush, None)?;
    intel_atomic_prepare_plane_clear_colors(state, ops)?;
    for tr in &state.pipes {
        ops.action(Action::FbcPrepareDirtyRect, Some(tr.new.pipe))?;
    }
    for index in 0..state.pipes.len() {
        intel_atomic_dsb_finish(state, index, caps, ops)?;
    }
    ops.action(Action::WaitAtomicDependencies, None)?;
    ops.action(Action::WaitMstDependencies, None)?;
    ops.action(Action::WaitGlobalStateDependencies, None)?;
    ops.action(Action::AcquireDcOffPower, None)?;
    for i in 0..state.pipes.len() {
        if needs_modeset(&state.pipes[i].new) || needs_fastset(&state.pipes[i].new) {
            let pipe = state.pipes[i].new.pipe;
            if usize::from(pipe) < put_domains.len() {
                intel_modeset_get_crtc_power_domains(
                    caps, &mut state.pipes[i].new, &mut put_domains[usize::from(pipe)], ops,
                )?;
            }
        }
    }

    intel_commit_modeset_disables(state, caps, ops)?;
    ops.action(Action::AllocateDpTunnelBandwidth, None)?;
    for tr in &state.pipes {
        ops.action(Action::SetCrtcConfig, Some(tr.new.pipe))?;
    }
    ops.action(Action::PmdemandPrePlaneUpdate, None)?;
    if state.modeset {
        ops.action(Action::UpdateLegacyModesetState, None)?;
    }
    ops.action(Action::SetCdclkPrePlaneUpdate, None)?;
    if state.modeset { ops.action(Action::VerifyDisabledModeset, None)?; }
    ops.action(Action::SagvPrePlaneUpdate, None)?;

    for tr in &mut state.pipes {
        if needs_modeset(&tr.new) && !tr.new.hw_active && tr.new.event_pending {
            ops.action(Action::SendDisabledPipeEvent, Some(tr.new.pipe))?;
            tr.new.event_pending = false;
        }
    }
    intel_encoders_update_prepare(state, caps, ops);
    ops.action(Action::DbufPrePlaneUpdate, None)?;
    for tr in &state.pipes {
        if tr.new.do_async_flip {
            intel_crtc_enable_flip_done(state, &tr.new, ops)?;
        }
    }

    if caps.display_version >= 9 {
        skl_commit_modeset_enables(state, caps, ops)?;
    } else {
        intel_commit_modeset_enables(state, caps, ops)?;
    }
    ops.action(Action::DpkgcProgramLatency, None)?;
    ops.action(Action::WaitVblankWorkers, None)?;
    ops.action(Action::WaitFlipDone, None)?;

    for index in 0..state.pipes.len() {
        let tr = state.pipes[index];
        if tr.new.do_async_flip {
            intel_crtc_disable_flip_done(state, &tr.new, ops)?;
        }
        intel_atomic_dsb_wait_commit(&mut state.pipes[index].new, ops)?;
        if !state.legacy_cursor_update && !tr.new.use_dsb {
            ops.action(Action::VrrCheckPushSent, Some(tr.new.pipe))?;
        }
        if tr.new.use_flipq {
            ops.action(Action::DisableFlipq, Some(tr.new.pipe))?;
        }
    }
    for tr in &state.pipes {
        // The gen-2 FIFO special case is intentionally unreachable on display 12/13.
        if caps.display_version == 2 && planes_enabling(&tr.old, &tr.new) {
            ops.action(Action::EnableCpuFifoUnderrunReporting, Some(tr.new.pipe))?;
        }
        ops.action(Action::OptimizeWatermarks, Some(tr.new.pipe))?;
    }
    ops.action(Action::DbufPostPlaneUpdate, None)?;
    for i in 0..state.pipes.len() {
        let tr = state.pipes[i];
        intel_post_plane_update(state, &tr, ops)?;
        let pipe = tr.new.pipe;
        if usize::from(pipe) < put_domains.len() {
            intel_modeset_put_crtc_power_domains(
                &mut state.pipes[i].new, put_domains[usize::from(pipe)], ops,
            )?;
        }
        ops.action(Action::VerifyCrtc, Some(pipe))?;
        intel_post_plane_update_after_readout(&tr, ops)?;
        ops.action(Action::TransferDsbOwnership, Some(pipe))?;
    }
    ops.action(Action::CheckCpuFifoUnderruns, None)?;
    ops.action(Action::CheckPchFifoUnderruns, None)?;
    if state.modeset { ops.action(Action::VerifyPlanes, None)?; }
    ops.action(Action::SagvPostPlaneUpdate, None)?;
    ops.action(Action::SetCdclkPostPlaneUpdate, None)?;
    ops.action(Action::PmdemandPostPlaneUpdate, None)?;
    ops.action(Action::CommitHardwareDone, None)?;
    ops.action(Action::CommitGlobalStateDone, None)?;
    if state.modeset { ops.action(Action::ArmUnclaimedMmioDetection, None)?; }
    ops.action(Action::DelayedDcOffPowerPut17ms, None)?;
    ops.action(Action::RuntimePowerPut, None)?;
    ops.action(Action::QueueCleanupWork, None)?;
    Ok(())
}

// upstream: intel_display.c intel_atomic_commit_work()
pub fn intel_atomic_commit_work<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    intel_atomic_commit_tail(state, caps, ops)
}

// upstream: intel_display.c intel_atomic_track_fbs()
pub fn intel_atomic_track_fbs<O: ModesetOps>(
    state: &AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    for plane in &state.planes {
        ops.plane_action(Action::TrackFramebufferPlane, plane.pipe, plane.id)?;
    }
    Ok(())
}

// upstream: intel_display.c intel_atomic_setup_commit()
pub fn intel_atomic_setup_commit<O: ModesetOps>(
    state: &AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::SetupCommit, None)?;
    ops.action(Action::SetupGlobalStateCommit, None)?;
    let _ = state;
    Ok(())
}

// upstream: intel_display.c intel_atomic_swap_state()
pub fn intel_atomic_swap_state<O: ModesetOps>(
    state: &AtomicState, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::SwapFrameworkState, None)?;
    ops.action(Action::SwapGlobalState, None)?;
    ops.action(Action::SwapDpllState, None)?;
    intel_atomic_track_fbs(state, ops)
}

// upstream: intel_display.c intel_atomic_commit()
pub fn intel_atomic_commit<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::RuntimePowerGet, None)?;
    if caps.display_version < 9 && state.legacy_cursor_update {
        for tr in &state.pipes {
            if tr.new.wm_need_postvbl_update || tr.new.update_wm_post {
                state.legacy_cursor_update = false;
            }
        }
    }
    if let Err(err) = intel_atomic_prepare_commit(state, ops) {
        ops.action(Action::RuntimePowerPut, None)?;
        return Err(err);
    }
    let setup = intel_atomic_setup_commit(state, ops)
        .and_then(|()| intel_atomic_swap_state(state, ops));
    if let Err(err) = setup {
        ops.action(Action::UnpreparePlanes, None)?;
        ops.action(Action::RuntimePowerPut, None)?;
        return Err(err);
    }
    ops.action(Action::AtomicCommitGet, None)?;
    if state.nonblock && state.modeset {
        ops.action(Action::QueueModesetWork, None)
    } else if state.nonblock {
        ops.action(Action::QueueFlipWork, None)
    } else {
        if state.modeset { ops.action(Action::FlushModesetWork, None)?; }
        intel_atomic_commit_tail(state, caps, ops)
    }
}
// Mode validation and dotclock limits.

// upstream: intel_display.c intel_encoder_possible_clones()
pub fn intel_encoder_possible_clones(
    encoder: &EncoderTransition, all: &[EncoderTransition],
) -> u32 {
    let mut clones = 0u32;
    for other in all {
        let cloneable = encoder.encoder_id == other.encoder_id ||
            (encoder.cloneable_types & other.encoder_type_bit != 0 &&
             other.cloneable_types & encoder.encoder_type_bit != 0);
        if cloneable { clones |= 1u32.checked_shl(u32::from(other.encoder_id)).unwrap_or(0); }
    }
    clones
}

// upstream: intel_display.c intel_encoder_possible_crtcs()
pub fn intel_encoder_possible_crtcs(encoder: &EncoderTransition) -> u8 {
    encoder.possible_crtcs_mask
}

// upstream: intel_display.c assert_port_valid()
pub fn assert_port_valid<O: ModesetOps>(
    caps: DisplayCaps, port: u8, ops: &mut O,
) -> Result<bool, ModesetError> {
    let valid = port < 16 && caps.port_mask & (1u16 << port) != 0;
    if !valid { ops.action(Action::VerifyCrtc, Some(port))?; }
    Ok(valid)
}

// upstream: intel_display.c intel_setup_outputs()
pub fn intel_setup_outputs<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::UnlockPpsRegisters, None)?;
    if !caps.has_display { return Ok(()); }
    if caps.has_ddi {
        for encoder in &state.encoders {
            ops.encoder_action(Action::DdiInitEncoder, 0, encoder.encoder_id)?;
        }
    } else {
        ops.action(Action::ComputeEncoderPossibleCrtcs, None)?;
    }
    for i in 0..state.encoders.len() {
        let encoder = state.encoders[i];
        let possible_crtcs = intel_encoder_possible_crtcs(&encoder);
        let possible_clones = intel_encoder_possible_clones(&encoder, &state.encoders);
        state.encoders[i].possible_crtcs_mask = possible_crtcs;
        state.encoders[i].possible_clones_mask = possible_clones;
    }
    ops.action(Action::InitPchRefclk, None)?;
    ops.action(Action::MovePanelConnectorsHead, None)
}
// upstream: intel_display.c intel_max_uncompressed_dotclock()
pub fn intel_max_uncompressed_dotclock(caps: DisplayCaps) -> u32 {
    let limit = if caps.display_version_x100 == 3002 {
        937_500
    } else if caps.display_version >= 30 {
        1_350_000
    } else {
        caps.cdclk_max_dotclock
    };
    caps.cdclk_max_dotclock.min(limit)
}

// upstream: intel_display.c max_dotclock()
pub fn max_dotclock(caps: DisplayCaps) -> u32 {
    let multiplier = if caps.has_ultrajoiner { 4 } else if caps.has_uncompressed_joiner || caps.has_bigjoiner { 2 } else { 1 };
    caps.cdclk_max_dotclock.saturating_mul(multiplier)
}

// upstream: intel_display.c intel_mode_valid()
pub fn intel_mode_valid(caps: DisplayCaps, mode: &DisplayMode) -> ModeStatus {
    if mode.vscan > 1 { return ModeStatus::NoVscan; }
    if mode.hskew { return ModeStatus::HorizontalIllegal; }
    if mode.csync || mode.ncsync || mode.pcsync { return ModeStatus::HorizontalSync; }
    if mode.bcast || mode.pixmux || mode.clkdiv2 { return ModeStatus::Bad; }
    let t = mode.timing;
    if t.clock_khz > max_dotclock(caps) { return ModeStatus::ClockHigh; }

    let (hdisplay_max, vdisplay_max, htotal_max, vtotal_max) = if caps.display_version >= 11 {
        (16_384, 8_192, 16_384, 8_192)
    } else if caps.display_version >= 9 || caps.broadwell || caps.haswell {
        (8_192, 4_096, 8_192, 8_192)
    } else if caps.display_version >= 3 {
        (4_096, 4_096, 8_192, 8_192)
    } else {
        (2_048, 2_048, 4_096, 4_096)
    };
    if t.hdisplay > hdisplay_max || t.hsync_start > htotal_max ||
        t.hsync_end > htotal_max || t.htotal > htotal_max {
        return ModeStatus::HorizontalIllegal;
    }
    if t.vdisplay > vdisplay_max || t.vsync_start > vtotal_max ||
        t.vsync_end > vtotal_max || t.vtotal > vtotal_max {
        return ModeStatus::VerticalIllegal;
    }
    if t.clock_khz == 0 || u64::from(t.htotal) * 1000 > u64::from(t.clock_khz) * 64 {
        return ModeStatus::HorizontalIllegal;
    }
    ModeStatus::Ok
}

// upstream: intel_display.c intel_cpu_transcoder_mode_valid()
pub fn intel_cpu_transcoder_mode_valid(caps: DisplayCaps, mode: &DisplayMode) -> ModeStatus {
    let t = mode.timing;
    let hblank = t.htotal.checked_sub(t.hdisplay).unwrap_or(0);
    let vblank = t.vtotal.checked_sub(t.vdisplay).unwrap_or(0);
    if caps.display_version >= 5 {
        if t.hdisplay < 64 || hblank < 32 { return ModeStatus::HorizontalIllegal; }
        if vblank < 5 { return ModeStatus::VerticalIllegal; }
    } else {
        if hblank < 32 { return ModeStatus::HorizontalIllegal; }
        if vblank < 3 { return ModeStatus::VerticalIllegal; }
    }
    if (caps.display_version >= 5 || caps.haswell) && t.hsync_start == t.hdisplay {
        return ModeStatus::HorizontalIllegal;
    }
    ModeStatus::Ok
}

// upstream: intel_display.c intel_mode_valid_max_plane_size()
pub fn intel_mode_valid_max_plane_size(
    caps: DisplayCaps, mode: &DisplayMode, num_joined_pipes: u8,
) -> ModeStatus {
    if caps.display_version < 9 { return ModeStatus::Ok; }
    let pipes = u32::from(num_joined_pipes);
    let (width_max, height_max) = if caps.display_version >= 30 {
        (6_144u32.saturating_mul(pipes), 4_800)
    } else if caps.display_version >= 11 {
        (5_120u32.saturating_mul(pipes), 4_320)
    } else {
        (5_120, 4_096)
    };
    if mode.timing.hdisplay > width_max { return ModeStatus::HorizontalIllegal; }
    if mode.timing.vdisplay > height_max { return ModeStatus::VerticalIllegal; }
    ModeStatus::Ok
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayModesetHooks { Skylake, Ddi, PchSplit, Valleyview, I9xx }

// upstream: intel_display.c intel_init_display_hooks()
pub fn intel_init_display_hooks(caps: DisplayCaps) -> DisplayModesetHooks {
    if caps.display_version >= 9 { DisplayModesetHooks::Skylake }
    else if caps.has_ddi { DisplayModesetHooks::Ddi }
    else if caps.has_pch_split { DisplayModesetHooks::PchSplit }
    else if caps.cherryview || caps.valleyview { DisplayModesetHooks::Valleyview }
    else { DisplayModesetHooks::I9xx }
}

// upstream: intel_display.c intel_initial_commit()
pub fn intel_initial_commit<O: ModesetOps>(
    state: &mut AtomicState, caps: DisplayCaps, ops: &mut O,
) -> Result<(), ModesetError> {
    ops.action(Action::AllocateAtomicState, None)?;
    ops.action(Action::AcquireModesetLocks, None)?;
    state.internal = true;
    let result = loop {
        let mut attempt = Ok(());
        for tr in &mut state.pipes {
            if !tr.new.hw_active { tr.new.inherited = false; }
            if !tr.new.hw_active { continue; }
            if let Err(err) = ops.action(Action::AddAffectedPlanes, Some(tr.new.pipe)) {
                attempt = Err(err);
                break;
            }
            tr.new.color_mgmt_changed = true;
            if let Err(err) = ops.action(Action::ForceColorManagementUpdate, Some(tr.new.pipe)) {
                attempt = Err(err);
                break;
            }
            if ops.initial_fastset_check_failed(tr.new.pipe) {
                if let Err(err) = ops.action(Action::AddAffectedConnectors, Some(tr.new.pipe)) {
                    attempt = Err(err);
                    break;
                }
            }
        }
        if attempt.is_ok() { attempt = intel_atomic_commit(state, caps, ops); }
        match attempt {
            Err(ModesetError::Deadlock) => {
                ops.action(Action::ClearAtomicState, None)?;
                ops.action(Action::ModesetBackoff, None)?;
            }
            result => break result,
        }
    };
    ops.action(Action::PutAtomicState, None)?;
    ops.action(Action::DropModesetLocks, None)?;
    result
}

fn encoder_hook<O: ModesetOps>(
    ops: &mut O, action: Action, pipe: u8, encoder: &EncoderTransition,
) -> Result<(), ModesetError> {
    ops.encoder_action(action, pipe, encoder.encoder_id)
}
