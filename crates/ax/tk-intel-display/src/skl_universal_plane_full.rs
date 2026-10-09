// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Linux 7.2.3 drivers/gpu/drm/i915/display/skl_universal_plane.c translation.
// DRM atomic/framebuffer/DSB/MMIO operations are represented by explicit traits.
#![allow(dead_code, clippy::too_many_arguments)]

pub const fn fourcc(bytes: [u8; 4]) -> u32 {
    u32::from_le_bytes(bytes)
}
pub const DRM_FORMAT_C8: u32 = fourcc(*b"C8  ");
pub const DRM_FORMAT_YUYV: u32 = fourcc(*b"YUYV");
pub const DRM_FORMAT_YVYU: u32 = fourcc(*b"YVYU");
pub const DRM_FORMAT_UYVY: u32 = fourcc(*b"UYVY");
pub const DRM_FORMAT_VYUY: u32 = fourcc(*b"VYUY");
pub const DRM_FORMAT_RGB565: u32 = fourcc(*b"RG16");
pub const DRM_FORMAT_NV12: u32 = fourcc(*b"NV12");
pub const DRM_FORMAT_XYUV8888: u32 = fourcc(*b"XYUV");
pub const DRM_FORMAT_P010: u32 = fourcc(*b"P010");
pub const DRM_FORMAT_P012: u32 = fourcc(*b"P012");
pub const DRM_FORMAT_P016: u32 = fourcc(*b"P016");
pub const DRM_FORMAT_Y210: u32 = fourcc(*b"Y210");
pub const DRM_FORMAT_Y212: u32 = fourcc(*b"Y212");
pub const DRM_FORMAT_Y216: u32 = fourcc(*b"Y216");
pub const DRM_FORMAT_XVYU2101010: u32 = fourcc(*b"XV30");
pub const DRM_FORMAT_XVYU12_16161616: u32 = fourcc(*b"XV36");
pub const DRM_FORMAT_XVYU16161616: u32 = fourcc(*b"XV48");
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(*b"XR24");
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(*b"XB24");
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(*b"AR24");
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(*b"AB24");
pub const DRM_FORMAT_XRGB2101010: u32 = fourcc(*b"XR30");
pub const DRM_FORMAT_XBGR2101010: u32 = fourcc(*b"XB30");
pub const DRM_FORMAT_ARGB2101010: u32 = fourcc(*b"AR30");
pub const DRM_FORMAT_ABGR2101010: u32 = fourcc(*b"AB30");
pub const DRM_FORMAT_XRGB16161616F: u32 = fourcc(*b"XR4H");
pub const DRM_FORMAT_XBGR16161616F: u32 = fourcc(*b"XB4H");
pub const DRM_FORMAT_ARGB16161616F: u32 = fourcc(*b"AR4H");
pub const DRM_FORMAT_ABGR16161616F: u32 = fourcc(*b"AB4H");

pub const SKL_PLANE_FORMATS: &[u32] = &[
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_XRGB16161616F,
    DRM_FORMAT_XBGR16161616F,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_XYUV8888,
];
pub const SKL_PLANAR_FORMATS: &[u32] = &[
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_XRGB16161616F,
    DRM_FORMAT_XBGR16161616F,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_NV12,
    DRM_FORMAT_XYUV8888,
];
pub const GLK_PLANAR_FORMATS: &[u32] = &[
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_XRGB16161616F,
    DRM_FORMAT_XBGR16161616F,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_NV12,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_P010,
    DRM_FORMAT_P012,
    DRM_FORMAT_P016,
];
pub const ICL_SDR_Y_PLANE_FORMATS: &[u32] = &[
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_Y210,
    DRM_FORMAT_Y212,
    DRM_FORMAT_Y216,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_XVYU2101010,
];
pub const ICL_SDR_UV_PLANE_FORMATS: &[u32] = &[
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_NV12,
    DRM_FORMAT_P010,
    DRM_FORMAT_P012,
    DRM_FORMAT_P016,
    DRM_FORMAT_Y210,
    DRM_FORMAT_Y212,
    DRM_FORMAT_Y216,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_XVYU2101010,
];
pub const ICL_HDR_PLANE_FORMATS: &[u32] = &[
    DRM_FORMAT_C8,
    DRM_FORMAT_RGB565,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XBGR2101010,
    DRM_FORMAT_ARGB2101010,
    DRM_FORMAT_ABGR2101010,
    DRM_FORMAT_XRGB16161616F,
    DRM_FORMAT_XBGR16161616F,
    DRM_FORMAT_ARGB16161616F,
    DRM_FORMAT_ABGR16161616F,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_YVYU,
    DRM_FORMAT_UYVY,
    DRM_FORMAT_VYUY,
    DRM_FORMAT_NV12,
    DRM_FORMAT_P010,
    DRM_FORMAT_P012,
    DRM_FORMAT_P016,
    DRM_FORMAT_Y210,
    DRM_FORMAT_Y212,
    DRM_FORMAT_Y216,
    DRM_FORMAT_XYUV8888,
    DRM_FORMAT_XVYU2101010,
    DRM_FORMAT_XVYU12_16161616,
    DRM_FORMAT_XVYU16161616,
];

pub const FMT_RGB565: u32 = 14 << 24;
pub const FMT_NV12: u32 = 1 << 24;
pub const FMT_XYUV: u32 = 8 << 24;
pub const FMT_P010: u32 = 3 << 24;
pub const FMT_P012: u32 = 5 << 24;
pub const FMT_P016: u32 = 7 << 24;
pub const FMT_Y210: u32 = 1 << 23;
pub const FMT_Y212: u32 = 3 << 23;
pub const FMT_Y216: u32 = 5 << 23;
pub const FMT_Y410: u32 = 7 << 23;
pub const FMT_Y412: u32 = 9 << 23;
pub const FMT_Y416: u32 = 11 << 23;
pub const FMT_XRGB8888: u32 = 4 << 24;
pub const FMT_XRGB2101010: u32 = 2 << 24;
pub const FMT_XRGB16161616F: u32 = 6 << 24;
pub const MOD_LINEAR: u64 = 0;
pub const MOD_X: u64 = 0x0100_0000_0000_0001;
pub const MOD_Y: u64 = 0x0100_0000_0000_0002;
pub const MOD_YF: u64 = 0x0100_0000_0000_0003;
pub const MOD_Y_CCS: u64 = 0x0100_0000_0000_0004;
pub const MOD_YF_CCS: u64 = 0x0100_0000_0000_0005;
pub const MOD_Y_RC_CCS: u64 = 0x0100_0000_0000_0006;
pub const MOD_Y_MC_CCS: u64 = 0x0100_0000_0000_0007;
pub const MOD_4TILE: u64 = 0x0100_0000_0000_0009;
pub const MOD_4TILE_MTL_RC_CCS: u64 = 0x0100_0000_0000_000d;
pub const MOD_4TILE_DG2_RC_CCS: u64 = 0x0100_0000_0000_000a;
pub const MOD_4TILE_MTL_MC_CCS: u64 = 0x0100_0000_0000_000e;
pub const MOD_4TILE_DG2_MC_CCS: u64 = 0x0100_0000_0000_000b;
pub const MOD_Y_RC_CCS_CC: u64 = 0x0100_0000_0000_0008;
pub const MOD_4TILE_MTL_RC_CCS_CC: u64 = 0x0100_0000_0000_000f;
pub const MOD_4TILE_DG2_RC_CCS_CC: u64 = 0x0100_0000_0000_000c;
pub const MOD_4TILE_BMG_CCS: u64 = 0x0100_0000_0000_0011;
pub const MOD_4TILE_LNL_CCS: u64 = 0x0100_0000_0000_0010;
pub const PLANE_CTL_ENABLE: u32 = 1 << 31;
pub const PLANE_CTL_ARB_SLOTS_MASK: u32 = 7 << 28;
pub const PLANE_CTL_PIPE_GAMMA_ENABLE: u32 = 1 << 30;
pub const PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE: u32 = 1 << 28;
pub const PLANE_CTL_KEY_ENABLE_SOURCE: u32 = 1 << 21;
pub const PLANE_CTL_KEY_ENABLE_DESTINATION: u32 = 2 << 21;
pub const PLANE_CTL_ORDER_RGBX: u32 = 1 << 20;
pub const PLANE_CTL_YUV420_Y_PLANE: u32 = 1 << 19;
pub const PLANE_CTL_YUV_TO_RGB_CSC_FORMAT_BT709: u32 = 1 << 18;
pub const PLANE_CTL_YUV422_ORDER_YUYV: u32 = 0 << 16;
pub const PLANE_CTL_YUV422_ORDER_UYVY: u32 = 1 << 16;
pub const PLANE_CTL_YUV422_ORDER_YVYU: u32 = 2 << 16;
pub const PLANE_CTL_YUV422_ORDER_VYUY: u32 = 3 << 16;
pub const PLANE_CTL_RENDER_DECOMPRESSION_ENABLE: u32 = 1 << 15;
pub const PLANE_CTL_TRICKLE_FEED_DISABLE: u32 = 1 << 14;
pub const PLANE_CTL_CLEAR_COLOR_DISABLE: u32 = 1 << 13;
pub const PLANE_CTL_PLANE_GAMMA_DISABLE: u32 = 1 << 13;
pub const PLANE_CTL_TILED_X: u32 = 1 << 10;
pub const PLANE_CTL_TILED_Y: u32 = 4 << 10;
pub const PLANE_CTL_TILED_YF: u32 = 5 << 10;
pub const PLANE_CTL_TILED_4: u32 = 5 << 10;
pub const PLANE_CTL_ASYNC_FLIP: u32 = 1 << 9;
pub const PLANE_CTL_FLIP_HORIZONTAL: u32 = 1 << 8;
pub const PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE: u32 = 1 << 4;
pub const PLANE_CTL_ALPHA_DISABLE: u32 = 0;
pub const PLANE_CTL_ALPHA_SW_PREMULTIPLY: u32 = 2 << 4;
pub const PLANE_CTL_ALPHA_HW_PREMULTIPLY: u32 = 3 << 4;
pub const PLANE_CTL_ROTATE_MASK: u32 = 3;
pub const PLANE_COLOR_PIPE_GAMMA_ENABLE: u32 = 1 << 30;
pub const PLANE_COLOR_YUV_RANGE_CORRECTION_DISABLE: u32 = 1 << 28;
pub const PLANE_COLOR_PIPE_CSC_ENABLE: u32 = 1 << 23;
pub const PLANE_COLOR_PLANE_CSC_ENABLE: u32 = 1 << 21;
pub const PLANE_COLOR_INPUT_CSC_ENABLE: u32 = 1 << 20;
pub const PLANE_COLOR_POST_CSC_GAMMA_MULTSEG_ENABLE: u32 = 1 << 15;
pub const PLANE_COLOR_PRE_CSC_GAMMA_ENABLE: u32 = 1 << 14;
pub const PLANE_COLOR_CSC_MODE_YUV601_TO_RGB601: u32 = 1 << 17;
pub const PLANE_COLOR_CSC_MODE_YUV709_TO_RGB709: u32 = 2 << 17;
pub const PLANE_COLOR_CSC_MODE_YUV2020_TO_RGB2020: u32 = 3 << 17;
pub const PLANE_COLOR_PLANE_GAMMA_DISABLE: u32 = 1 << 13;
pub const PLANE_COLOR_ALPHA_SW_PREMULTIPLY: u32 = 2 << 4;
pub const PLANE_COLOR_ALPHA_HW_PREMULTIPLY: u32 = 3 << 4;
pub const PLANE_CUS_ENABLE: u32 = 1 << 31;
pub const PLANE_CUS_Y_PLANE_MASK: u32 = 1 << 30;
pub const PLANE_CUS_HPHASE_SIGN_NEGATIVE: u32 = 1 << 19;
pub const PLANE_CUS_HPHASE_0: u32 = 0;
pub const PLANE_CUS_HPHASE_0_25: u32 = 1 << 16;
pub const PLANE_CUS_VPHASE_SIGN_NEGATIVE: u32 = 1 << 15;
pub const PLANE_CUS_VPHASE_0: u32 = 0;
pub const PLANE_CUS_VPHASE_0_25: u32 = 1 << 12;
pub const PLANE_PIXEL_NORMALIZE_ENABLE: u32 = 1 << 31;
pub const PLANE_PIXEL_NORMALIZE_NORM_FACTOR_1_0: u32 = 0x3c00;
pub const PLANE_WM_EN: u32 = 1 << 31;
pub const PLANE_WM_IGNORE_LINES: u32 = 1 << 30;
pub const PLANE_WM_AUTO_MIN_ALLOC_EN: u32 = 1 << 29;
pub const PLANE_WM_LINES_MASK: u32 = 0x1fff << 14;
pub const PLANE_WM_BLOCKS_MASK: u32 = 0x1fff;
pub const PLANE_BUF_END_MASK: u32 = 0x1fff << 16;
pub const PLANE_BUF_START_MASK: u32 = 0x1fff;
pub const PLANE_AUTO_MIN_DBUF_EN: u32 = 1 << 31;
pub const PLANE_MIN_DBUF_BLOCKS_MASK: u32 = 0x1fff << 16;
pub const PLANE_INTERIM_DBUF_BLOCKS_MASK: u32 = 0x1fff;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FormatInfo {
    pub cpp: [u8; 4],
    pub planes: u8,
    pub yuv_semiplanar: bool,
    pub is_yuv: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Framebuffer {
    pub format: FormatInfo,
    pub modifier: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayInfo {
    pub display_ver: u8,
    pub fbc_mask: u32,
    pub has_d12_plane_minimization: bool,
    pub alderlake_p: bool,
    pub has_4tile: bool,
    pub wm_levels: u8,
    pub has_hw_sagv_wm: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub pipe: u8,
    pub active: bool,
    pub enable: bool,
    pub enable_psr2_sel_fetch: bool,
    pub gamma_enable: bool,
    pub csc_enable: bool,
    pub pixel_rate_cdclk: u32,
    pub output_ycbcr420: bool,
    pub display: DisplayInfo,
    pub wm: [PlaneWm; 8],
    pub ddb: [DdbEntry; 8],
    pub ddb_y: [DdbEntry; 8],
    pub min_ddb: [u16; 8],
    pub interim_ddb: [u16; 8],
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneState {
    pub pixel_rate_cdclk: u32,
    pub fb: Option<Framebuffer>,
    pub pixel_format: u32,
    pub rgb_order: bool,
    pub alpha: bool,
    pub rotation: u32,
    pub reflect_x: bool,
    pub modifier: u64,
    pub stride: u32,
    pub aux_dist: u32,
    pub plane_index: u8,
    pub plane_id: u8,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdbEntry {
    pub start: u16,
    pub end: u16,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WmLevel {
    pub enable: bool,
    pub ignore_lines: bool,
    pub auto_min_alloc_wm_enable: bool,
    pub blocks: u16,
    pub lines: u16,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneWm {
    pub levels: [WmLevel; 8],
    pub trans: WmLevel,
    pub sagv: WmLevel,
    pub sagv_trans: WmLevel,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneError {
    Invalid,
    Unsupported,
    Io,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneDiagnostic {
    MissingCase,
    UnsupportedRotationWithCcs,
    UnsupportedLinearReflect,
    UnsupportedTile4Reflect,
    UnsupportedRotation,
    UnsupportedFormatRotation,
    UnsupportedInterlaceTiling,
    UnsupportedColorKeyFormat,
    InvalidDestination,
    InvalidSourceSize,
    UnableMainOffset,
    UnableCcsOffset,
    InvalidSurfaceAlignment,
    UnexpectedPipe,
    UnsupportedJoiner,
    NonzeroPlaneOffset,
    InvalidNv12Rotation,
    InvalidPlanePair,
}
pub trait PlaneDiagnostics {
    fn warning(&self, kind: PlaneDiagnostic, value0: i64, value1: i64);
}

/// Linux DRM/Intel display services that are not local arithmetic or policy.
pub trait PlaneFramework: PlaneDiagnostics {
    fn pixel_rate_cdclk(&self, crtc: &CrtcState, plane: &PlaneState) -> u32;
    fn yuv_semiplanar(&self, format: FormatInfo, modifier: u64) -> bool;
    fn plane_can_async_flip(&self, plane_index: u8, format: FormatInfo, modifier: u64) -> bool;
    fn fb_uses_dpt(&self, fb: &Framebuffer) -> bool;
    fn fb_is_ccs_aux_plane(&self, fb: &Framebuffer, color_plane: usize) -> bool;
    fn missing_case(&self, value: u64);
}

/// DSB and display-power operations supplied by the DRM atomic/SoC framework.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneRegister {
    SurfaceLive,
    Wm,
    WmTrans,
    WmSagv,
    WmSagvTrans,
    InputCscCoeff,
    InputCscPreoff,
    InputCscPostoff,
    CscCoeff,
    CscPreoff,
    CscPostoff,
    Stride,
    Position,
    Size,
    KeyVal,
    KeyMask,
    KeyMax,
    Offset,
    AuxDist,
    AuxOffset,
    ColorCtl,
    Control,
    Surface,
    SelectFetchCtl,
    SelectFetchPos,
    SelectFetchOffset,
    SelectFetchSize,
    CcVal,
    CusCtl,
    PixelNormalize,
    Nv12Buffer,
    Buffer,
    MinBuffer,
}
pub trait PlaneDsb {
    fn write(&mut self, pipe: u8, plane: u8, register: PlaneRegister, index: u8, value: u32);
    fn program_scaler(&mut self, plane: u8, state: &CrtcState, plane_state: &PlaneUpdate);
    fn program_color_pipeline(&mut self, plane_state: &PlaneUpdate);
    fn color_plane_commit_arm(&mut self, plane_state: &PlaneUpdate);
    fn power_get_if_enabled(&mut self, pipe: u8) -> Option<u32>;
    fn power_put(&mut self, wakeref: u32);
    fn read_control(&mut self, pipe: u8, plane: u8) -> u32;
    fn read_reg(&mut self, pipe: u8, plane: u8, register: PlaneRegister) -> u32;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneRect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}
impl PlaneRect {
    pub fn width(self) -> i32 {
        self.x2 - self.x1
    }
    pub fn height(self) -> i32 {
        self.y2 - self.y1
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneView {
    pub x: u32,
    pub y: u32,
    pub offset: u32,
    pub stride: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneUpdate {
    pub pipe: u8,
    pub plane_id: u8,
    pub stride: u32,
    pub dst: PlaneRect,
    pub src: PlaneRect,
    pub view0: PlaneView,
    pub view1: PlaneView,
    pub control: u32,
    pub color_ctl: u32,
    pub surface: u32,
    pub keyval: u32,
    pub keymask: u32,
    pub keymax: u32,
    pub aux_dist: u32,
    pub aux_dist_enabled: bool,
    pub scaler_id: i8,
    pub async_flip: bool,
    pub color_encoding: u8,
    pub ccs_cc: bool,
    pub ccval: u64,
    pub cus_ctl: u32,
    pub yuv: bool,
    pub force_black: bool,
    pub fbc_needs_pixel_normalizer: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SelectFetch {
    pub enabled: bool,
    pub su_region_et: bool,
    pub clip: PlaneRect,
    pub su_area: PlaneRect,
    pub dst: PlaneRect,
    pub src: PlaneRect,
    pub view: PlaneView,
    pub color_plane: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneErrorState {
    pub control: u32,
    pub surface: u32,
    pub surface_live: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneCheckConfig {
    pub has_fb: bool,
    pub display_ver: u8,
    pub modifier: u64,
    pub format: u32,
    pub rotation: u32,
    pub src: PlaneRect,
    pub dst: PlaneRect,
    pub pipe_src: PlaneRect,
    pub interlace: bool,
    pub color_key_source: bool,
    pub alderlake_s: bool,
    pub tigerlake: bool,
    pub ccs_modifier: bool,
    pub tile4_modifier: bool,
    pub rotation_supported: bool,
}

/// DRM framebuffer helpers stay behind the framework boundary; the SKL/ICL
/// surface-selection and alignment algorithms below are translated locally.
pub trait PlaneSurfaceOps: PlaneDiagnostics {
    fn alignment(&self, color_plane: usize) -> u32;
    fn subsampling(&self, color_plane: usize) -> (i32, i32);
    fn adjust_aligned_offset(
        &mut self,
        x: &mut i32,
        y: &mut i32,
        plane: usize,
        offset: u32,
        new_offset: u32,
    ) -> u32;
    fn source(&self) -> PlaneRect;
    fn set_source(&mut self, src: PlaneRect);
    fn view(&self, color_plane: usize) -> PlaneView;
    fn set_view(&mut self, color_plane: usize, view: PlaneView);
    fn min_size(&self, color_plane: usize, rotation: u32) -> (i32, i32);
    fn max_size(&self, color_plane: usize, rotation: u32) -> (i32, i32);
    fn add_fb_offsets(&mut self, x: &mut i32, y: &mut i32, color_plane: usize);
    fn compute_aligned_offset(&mut self, x: &mut i32, y: &mut i32, color_plane: usize) -> u32;
    fn main_to_aux_plane(&self, color_plane: usize) -> usize;
    fn ccs_to_main_plane(&self, color_plane: usize) -> usize;
    fn is_ccs_aux_plane(&self, color_plane: usize) -> bool;
    fn is_ccs_modifier(&self) -> bool;
    fn is_yuv_semiplanar(&self) -> bool;
    fn num_color_planes(&self) -> usize;
    fn source_cpp(&self, color_plane: usize) -> i32;
    fn mapping_stride(&self, color_plane: usize) -> u32;
    fn rotation(&self) -> u32;
    fn modifier(&self) -> u64;
    fn visible(&self) -> bool;
    fn compute_gtt(&mut self) -> i32;
}

/// Atomic-helper clipping/source checks and object protection are kernel services;
/// plane ordering, policy, and local register-state derivation remain here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneControlInput {
    pub format: u32,
    pub modifier: u64,
    pub rotation: u32,
    pub has_alpha: bool,
    pub blend_mode: u8,
    pub key_flags: u32,
    pub color_encoding: u32,
    pub color_range_full: bool,
    pub adlp_wa: bool,
    pub format_info: FormatInfo,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneColorInput {
    pub format: FormatInfo,
    pub plane_id: u8,
    pub has_alpha: bool,
    pub blend_mode: u8,
    pub color_encoding: u8,
    pub color_range_full: bool,
    pub force_black: bool,
    pub degamma: bool,
    pub ctm: bool,
    pub gamma: bool,
    pub gamma_size: u32,
}
pub trait PlaneValidationOps: PlaneSurfaceOps + PlaneProtectionOps + PlaneFramework {
    fn check_clipping(
        &mut self,
        min_scale: i32,
        max_scale: i32,
        allow_modeset_clipping: bool,
    ) -> i32;
    fn check_source(&mut self) -> i32;
    fn set_visible(&mut self, visible: bool);
    fn alpha(&self) -> u16;
    fn framebuffer_size(&self) -> (i32, i32);
    fn damage(&self) -> PlaneRect;
    fn set_damage(&mut self, damage: PlaneRect);
    fn check_config(&self) -> PlaneCheckConfig;
    fn display_info(&self) -> DisplayInfo;
    fn format(&self) -> u32;
    fn modifier(&self) -> u64;
    fn plane_id(&self) -> u8;
    fn format_info(&self) -> FormatInfo;
    fn key_flags(&self) -> u32;
    fn control_input(&self) -> PlaneControlInput;
    fn color_input(&self) -> PlaneColorInput;
    fn set_ctl(&mut self, ctl: u32);
    fn set_color_ctl(&mut self, ctl: u32);
    fn set_cus_ctl(&mut self, ctl: u32);
    fn set_surface(&mut self, plane: usize, offset: u32, x: i32, y: i32);
}

pub trait PlaneProtectionOps {
    fn display_ver(&self) -> u8;
    fn key_check(&mut self) -> bool;
    fn is_protected(&self) -> bool;
    fn set_decrypt(&mut self, value: bool);
    fn set_force_black(&mut self, value: bool);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlaneFormatSet {
    #[default]
    SklPlanar,
    Skl,
    GlkPlanar,
    IclHdr,
    IclSdrY,
    IclSdrUv,
}

pub trait PlaneFormatOps {
    fn supports_modifier(&self, modifier: u64) -> bool;
    fn is_ccs(&self, modifier: u64) -> bool;
    fn is_mc_ccs(&self, modifier: u64) -> bool;
}

pub trait PlaneIrqOps {
    fn lock(&mut self);
    fn unlock(&mut self);
    fn enable_flip_done(&mut self, pipe: u8, plane: u8);
    fn disable_flip_done(&mut self, pipe: u8, plane: u8);
}

pub trait PlaneLifecycleOps {
    type Plane;
    fn disable_tiling_read_ctl(&mut self, pipe: u8, plane: u8) -> u32;
    fn disable_tiling_write_stride(&mut self, pipe: u8, plane: u8, stride: u32);
    fn disable_tiling_write_ctl(&mut self, pipe: u8, plane: u8, ctl: u32);
    fn disable_tiling_write_surface(&mut self, pipe: u8, plane: u8, surf: u32);
    fn is_dpt(&self) -> bool;
    fn alloc_plane(&mut self) -> Option<Self::Plane>;
    fn add_plane_to_fbc(&mut self, plane: &mut Self::Plane, config: &PlaneCreateConfig, fbc_id: u8);
    fn universal_plane_init(&mut self, plane: &mut Self::Plane, config: &PlaneCreateConfig) -> i32;
    fn free_plane(&mut self, plane: Self::Plane);
    fn create_rotation_property(&mut self, plane: &mut Self::Plane, default: u32, supported: u32);
    fn create_color_properties(
        &mut self,
        plane: &mut Self::Plane,
        csc: u32,
        ranges: u8,
        default_encoding: u8,
        default_range: u8,
    );
    fn initialize_color_pipeline_plane(&mut self, plane: &mut Self::Plane, pipe: u8);
    fn create_alpha_property(&mut self, plane: &mut Self::Plane);
    fn create_blend_mode_property(&mut self, plane: &mut Self::Plane, modes: u8);
    fn create_zpos_immutable_property(&mut self, plane: &mut Self::Plane, plane_id: u8);
    fn enable_fb_damage_clips(&mut self, plane: &mut Self::Plane);
    fn create_scaling_filter_property(&mut self, plane: &mut Self::Plane, filters: u8);
    fn plane_helper_add(&mut self, plane: &mut Self::Plane);
    fn skylake(&self) -> bool;
    fn broxton(&self) -> bool;
    fn scanout_needs_vtd_wa(&self) -> bool;
    fn has_async_flips(&self) -> bool;
    fn display_wa_14010477008(&self) -> bool;
    fn dgfx(&self) -> bool;
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneCreateConfig {
    pub display_ver: u8,
    pub pipe: u8,
    pub plane_id: u8,
    pub min_width_policy: u8,
    pub max_width_policy: u8,
    pub max_height_policy: u8,
    pub min_cdclk_policy: u8,
    pub max_stride_policy: u8,
    pub min_alignment_policy: u8,
    pub vtd_guard: u16,
    pub frontbuffer_bit: u32,
    pub fbc_id: Option<u8>,
    pub update_gen11_plus: bool,
    pub disable_tiling_handler: bool,
    pub surf_offset_handler: bool,
    pub update_noarm_policy: u8,
    pub update_arm_policy: u8,
    pub disable_arm_policy: u8,
    pub capture_error_handler: bool,
    pub get_hw_state_handler: bool,
    pub check_plane_handler: bool,
    pub async_flip: bool,
    pub async_flip_toggle_wa: bool,
    pub async_flip_policy: u8,
    pub flip_done_irq: bool,
    pub format_set: PlaneFormatSet,
    pub function_set: u8,
    pub plane_type_primary: bool,
    pub supported_rotations: u32,
    pub supported_csc: u32,
    pub supported_color_range: u8,
    pub default_color_encoding: u8,
    pub default_color_range: u8,
    pub has_color_pipeline: bool,
    pub has_damage_clips: bool,
    pub has_alpha_property: bool,
    pub blend_modes: u8,
    pub zpos_immutable: bool,
    pub has_scaling_filter: bool,
    pub scaling_filters: u8,
    pub async_format_mod_supported: bool,
    pub caps: u8,
}

// upstream: skl_universal_plane.c skl_format_to_fourcc()
pub fn skl_format_to_fourcc(format: u32, rgb_order: bool, alpha: bool) -> u32 {
    match format {
        FMT_RGB565 => DRM_FORMAT_RGB565,
        FMT_NV12 => DRM_FORMAT_NV12,
        FMT_XYUV => DRM_FORMAT_XYUV8888,
        FMT_P010 => DRM_FORMAT_P010,
        FMT_P012 => DRM_FORMAT_P012,
        FMT_P016 => DRM_FORMAT_P016,
        FMT_Y210 => DRM_FORMAT_Y210,
        FMT_Y212 => DRM_FORMAT_Y212,
        FMT_Y216 => DRM_FORMAT_Y216,
        FMT_Y410 => DRM_FORMAT_XVYU2101010,
        FMT_Y412 => DRM_FORMAT_XVYU12_16161616,
        FMT_Y416 => DRM_FORMAT_XVYU16161616,
        FMT_XRGB8888 => match (rgb_order, alpha) {
            (true, true) => DRM_FORMAT_ABGR8888,
            (true, false) => DRM_FORMAT_XBGR8888,
            (false, true) => DRM_FORMAT_ARGB8888,
            _ => DRM_FORMAT_XRGB8888,
        },
        FMT_XRGB2101010 => match (rgb_order, alpha) {
            (true, true) => DRM_FORMAT_ABGR2101010,
            (true, false) => DRM_FORMAT_XBGR2101010,
            (false, true) => DRM_FORMAT_ARGB2101010,
            _ => DRM_FORMAT_XRGB2101010,
        },
        FMT_XRGB16161616F => match (rgb_order, alpha) {
            (true, true) => DRM_FORMAT_ABGR16161616F,
            (true, false) => DRM_FORMAT_XBGR16161616F,
            (false, true) => DRM_FORMAT_ARGB16161616F,
            _ => DRM_FORMAT_XRGB16161616F,
        },
        _ => match (rgb_order, alpha) {
            (true, true) => DRM_FORMAT_ABGR8888,
            (true, false) => DRM_FORMAT_XBGR8888,
            (false, true) => DRM_FORMAT_ARGB8888,
            _ => DRM_FORMAT_XRGB8888,
        },
    }
}

// upstream: skl_universal_plane.c icl_nv12_y_plane_mask()
fn icl_nv12_y_plane_mask(display: DisplayInfo) -> u8 {
    if display.display_ver >= 13 || display.has_d12_plane_minimization {
        (1 << 4) | (1 << 5)
    } else {
        (1 << 6) | (1 << 7)
    }
}

// upstream: skl_universal_plane.c icl_is_nv12_y_plane()
pub fn icl_is_nv12_y_plane(display: DisplayInfo, plane_id: u8) -> bool {
    display.display_ver >= 11
        && icl_nv12_y_plane_mask(display) & (1u8.wrapping_shl(plane_id as u32)) != 0
}

// upstream: skl_universal_plane.c icl_hdr_plane_mask()
pub fn icl_hdr_plane_mask() -> u8 {
    (1 << 1) | (1 << 2) | (1 << 3)
}

// upstream: skl_universal_plane.c icl_is_hdr_plane()
pub fn icl_is_hdr_plane(display: DisplayInfo, plane_id: u8) -> bool {
    display.display_ver >= 11 && icl_hdr_plane_mask() & (1u8.wrapping_shl(plane_id as u32)) != 0
}

// upstream: skl_universal_plane.c icl_plane_min_cdclk()
pub fn icl_plane_min_cdclk(io: &impl PlaneFramework, crtc: &CrtcState, plane: &PlaneState) -> u32 {
    io.pixel_rate_cdclk(crtc, plane).div_ceil(2)
}

// upstream: skl_universal_plane.c glk_plane_ratio()
pub fn glk_plane_ratio(plane: &PlaneState) -> (u32, u32) {
    if plane.fb.map(|fb| fb.format.cpp[0]) == Some(8) {
        (10, 8)
    } else {
        (1, 1)
    }
}

// upstream: skl_universal_plane.c glk_plane_min_cdclk()
pub fn glk_plane_min_cdclk(io: &impl PlaneFramework, crtc: &CrtcState, plane: &PlaneState) -> u32 {
    let (num, den) = glk_plane_ratio(plane);
    io.pixel_rate_cdclk(crtc, plane)
        .wrapping_mul(num)
        .div_ceil(2 * den)
}

// upstream: skl_universal_plane.c skl_plane_ratio()
pub fn skl_plane_ratio(plane: &PlaneState) -> (u32, u32) {
    if plane.fb.map(|fb| fb.format.cpp[0]) == Some(8) {
        (9, 8)
    } else {
        (1, 1)
    }
}

// upstream: skl_universal_plane.c skl_plane_min_cdclk()
pub fn skl_plane_min_cdclk(io: &impl PlaneFramework, crtc: &CrtcState, plane: &PlaneState) -> u32 {
    let (num, den) = skl_plane_ratio(plane);
    io.pixel_rate_cdclk(crtc, plane)
        .wrapping_mul(num)
        .div_ceil(den)
}

// upstream: skl_universal_plane.c skl_plane_max_width()
pub fn skl_plane_max_width(
    io: &impl PlaneFramework,
    fb: &Framebuffer,
    color_plane: usize,
    _rotation: u32,
) -> i32 {
    let cpp = fb.format.cpp[color_plane];
    match fb.modifier {
        MOD_LINEAR | MOD_X => {
            if cpp == 8 {
                4096
            } else {
                5120
            }
        }
        MOD_Y_CCS | MOD_YF_CCS | MOD_Y | MOD_YF => {
            if cpp == 8 {
                2048
            } else {
                4096
            }
        }
        v => {
            io.missing_case(v);
            2048
        }
    }
}

// upstream: skl_universal_plane.c glk_plane_max_width()
pub fn glk_plane_max_width(
    io: &impl PlaneFramework,
    fb: &Framebuffer,
    color_plane: usize,
    _rotation: u32,
) -> i32 {
    let cpp = fb.format.cpp[color_plane];
    match fb.modifier {
        MOD_LINEAR | MOD_X => {
            if cpp == 8 {
                4096
            } else {
                5120
            }
        }
        MOD_Y_CCS | MOD_YF_CCS | MOD_Y | MOD_YF => {
            if cpp == 8 {
                2048
            } else {
                5120
            }
        }
        v => {
            io.missing_case(v);
            2048
        }
    }
}

// upstream: skl_universal_plane.c adl_plane_min_width()
pub fn adl_plane_min_width(fb: &Framebuffer, color_plane: usize, _rotation: u32) -> i32 {
    16 / fb.format.cpp[color_plane] as i32
}

// upstream: skl_universal_plane.c icl_plane_min_width()
pub fn icl_plane_min_width(fb: &Framebuffer, color_plane: usize, _rotation: u32) -> i32 {
    16 / fb.format.cpp[color_plane] as i32 + 2
}

// upstream: skl_universal_plane.c xe3_plane_max_width()
pub fn xe3_plane_max_width(
    io: &impl PlaneFramework,
    fb: &Framebuffer,
    _color_plane: usize,
    _rotation: u32,
) -> i32 {
    if io.yuv_semiplanar(fb.format, fb.modifier) {
        4096
    } else {
        6144
    }
}

// upstream: skl_universal_plane.c icl_hdr_plane_max_width()
pub fn icl_hdr_plane_max_width(
    io: &impl PlaneFramework,
    fb: &Framebuffer,
    _color_plane: usize,
    _rotation: u32,
) -> i32 {
    if io.yuv_semiplanar(fb.format, fb.modifier) {
        4096
    } else {
        5120
    }
}

// upstream: skl_universal_plane.c icl_sdr_plane_max_width()
pub fn icl_sdr_plane_max_width(_fb: &Framebuffer, _color_plane: usize, _rotation: u32) -> i32 {
    5120
}

// upstream: skl_universal_plane.c skl_plane_max_height()
pub fn skl_plane_max_height(_fb: &Framebuffer, _color_plane: usize, _rotation: u32) -> i32 {
    4096
}

// upstream: skl_universal_plane.c skl_fbc_id_for_pipe()
pub fn skl_fbc_id_for_pipe(pipe: u8) -> u8 {
    pipe
}

// upstream: skl_universal_plane.c skl_plane_has_fbc()
pub fn skl_plane_has_fbc(display: DisplayInfo, fbc_id: u8, plane_id: u8) -> bool {
    display.fbc_mask & (1u32.wrapping_shl(fbc_id as u32)) != 0
        && if display.display_ver >= 20 {
            icl_is_hdr_plane(display, plane_id)
        } else {
            plane_id == 1
        }
}

// upstream: skl_universal_plane.c icl_plane_max_height()
pub fn icl_plane_max_height(_fb: &Framebuffer, _color_plane: usize, _rotation: u32) -> i32 {
    4320
}

// upstream: skl_universal_plane.c plane_max_stride()
pub fn plane_max_stride(
    _plane: u8,
    cpp: u32,
    rotation: u32,
    max_pixels: u32,
    max_bytes: u32,
) -> u32 {
    if rotation & (2 | 8) != 0 {
        max_pixels.min(max_bytes / cpp)
    } else {
        max_pixels.wrapping_mul(cpp).min(max_bytes)
    }
}

// upstream: skl_universal_plane.c adl_plane_max_stride()
pub fn adl_plane_max_stride(plane: u8, cpp: u32, rotation: u32) -> u32 {
    plane_max_stride(plane, cpp, rotation, 65536, 128 * 1024)
}

// upstream: skl_universal_plane.c skl_plane_max_stride()
pub fn skl_plane_max_stride(plane: u8, cpp: u32, rotation: u32) -> u32 {
    plane_max_stride(plane, cpp, rotation, 8192, 32 * 1024)
}

// upstream: skl_universal_plane.c tgl_plane_can_async_flip()
pub fn tgl_plane_can_async_flip(modifier: u64) -> bool {
    matches!(
        modifier,
        MOD_LINEAR
            | MOD_X
            | MOD_Y
            | MOD_4TILE
            | MOD_Y_RC_CCS
            | MOD_4TILE_MTL_RC_CCS
            | MOD_4TILE_DG2_RC_CCS
            | MOD_4TILE_BMG_CCS
            | MOD_4TILE_LNL_CCS
    )
}

// upstream: skl_universal_plane.c icl_plane_can_async_flip()
pub fn icl_plane_can_async_flip(modifier: u64) -> bool {
    matches!(modifier, MOD_X | MOD_Y | MOD_YF | MOD_Y_CCS | MOD_YF_CCS)
}

// upstream: skl_universal_plane.c skl_plane_can_async_flip()
pub fn skl_plane_can_async_flip(modifier: u64) -> bool {
    matches!(modifier, MOD_X | MOD_Y | MOD_YF)
}

// upstream: skl_universal_plane.c tgl_plane_min_alignment()
pub fn tgl_plane_min_alignment(
    io: &impl PlaneFramework,
    fb: &Framebuffer,
    color_plane: usize,
    plane_index: u8,
    display: DisplayInfo,
) -> u32 {
    let mult = if io.fb_uses_dpt(fb) { 512 } else { 1 };
    if io.fb_is_ccs_aux_plane(fb, color_plane) {
        return mult * 4 * 1024;
    }
    if display.alderlake_p && io.plane_can_async_flip(plane_index, fb.format, fb.modifier) {
        return mult * 16 * 1024;
    }
    match fb.modifier {
        MOD_LINEAR | MOD_X | MOD_Y | MOD_4TILE => mult * 4 * 1024,
        MOD_Y_RC_CCS
        | MOD_4TILE_MTL_RC_CCS
        | MOD_4TILE_DG2_RC_CCS
        | MOD_Y_MC_CCS
        | MOD_4TILE_MTL_MC_CCS
        | MOD_4TILE_DG2_MC_CCS
        | MOD_Y_RC_CCS_CC
        | MOD_4TILE_MTL_RC_CCS_CC
        | MOD_4TILE_DG2_RC_CCS_CC
        | MOD_4TILE_BMG_CCS
        | MOD_4TILE_LNL_CCS => (mult * 4 * 1024).max(16 * 1024),
        _ => {
            io.missing_case(fb.modifier);
            0
        }
    }
}

// upstream: skl_universal_plane.c skl_plane_min_alignment()
pub fn skl_plane_min_alignment(
    io: &impl PlaneFramework,
    fb: &Framebuffer,
    color_plane: usize,
) -> u32 {
    if color_plane != 0 {
        return 4 * 1024;
    }
    match fb.modifier {
        MOD_LINEAR | MOD_X => 256 * 1024,
        MOD_Y_CCS | MOD_YF_CCS | MOD_Y | MOD_YF => 1024 * 1024,
        _ => {
            io.missing_case(fb.modifier);
            0
        }
    }
}

// upstream: skl_universal_plane.c icl_program_input_csc()
pub fn icl_program_input_csc(dsb: &mut impl PlaneDsb, pipe: u8, plane: u8, encoding: u8) {
    let matrix: [u16; 9] = match encoding {
        0 => [0x7af8, 0x7800, 0, 0x8b28, 0x7800, 0x9ac0, 0, 0x7800, 0x7dd8],
        1 => [0x7c98, 0x7800, 0, 0x9ef8, 0x7800, 0xac00, 0, 0x7800, 0x7ed8],
        2 => [0x7bc8, 0x7800, 0, 0x8928, 0x7800, 0xaa88, 0, 0x7800, 0x7f10],
        _ => [0; 9],
    };
    let roff = |x: u16| (x as u32 & 0xffff) << 16;
    let goff = |x: u16| x as u32 & 0xffff;
    let boff = |x: u16| (x as u32 & 0xffff) << 16;
    for (index, value) in [
        roff(matrix[0]) | goff(matrix[1]),
        boff(matrix[2]),
        roff(matrix[3]) | goff(matrix[4]),
        boff(matrix[5]),
        roff(matrix[6]) | goff(matrix[7]),
        boff(matrix[8]),
    ]
    .into_iter()
    .enumerate()
    {
        dsb.write(
            pipe,
            plane,
            PlaneRegister::InputCscCoeff,
            index as u8,
            value,
        );
    }
    for (i, v) in [0x1800, 0, 0x1800].into_iter().enumerate() {
        dsb.write(pipe, plane, PlaneRegister::InputCscPreoff, i as u8, v);
    }
    for i in 0..3 {
        dsb.write(pipe, plane, PlaneRegister::InputCscPostoff, i, 0);
    }
}

// upstream: skl_universal_plane.c skl_plane_stride_mult()
pub fn skl_plane_stride_mult(
    fb: &Framebuffer,
    color_plane: usize,
    rotation: u32,
    tile_width_bytes: u32,
    tile_height: u32,
) -> u32 {
    let _color_plane = color_plane;
    if fb.modifier == MOD_LINEAR {
        64
    } else if rotation & (2 | 8) != 0 {
        tile_height
    } else {
        tile_width_bytes
    }
}

// upstream: skl_universal_plane.c skl_plane_stride()
pub fn skl_plane_stride(
    fb: &Framebuffer,
    color_plane: usize,
    rotation: u32,
    scanout_stride: u32,
    tile_width_bytes: u32,
    tile_height: u32,
) -> u32 {
    if color_plane >= fb.format.planes as usize {
        return 0;
    }
    scanout_stride / skl_plane_stride_mult(fb, color_plane, rotation, tile_width_bytes, tile_height)
}

// upstream: skl_universal_plane.c skl_plane_ddb_reg_val()
pub fn skl_plane_ddb_reg_val(entry: DdbEntry) -> u32 {
    if entry.end == 0 {
        0
    } else {
        ((((entry.end - 1) as u32) << 16) & PLANE_BUF_END_MASK)
            | (entry.start as u32) & PLANE_BUF_START_MASK
    }
}

// upstream: skl_universal_plane.c xe3_plane_min_ddb_reg_val()
pub fn xe3_plane_min_ddb_reg_val(min_ddb: u16, interim_ddb: u16) -> u32 {
    let mut value = 0;
    if min_ddb != 0 {
        value |= ((min_ddb as u32) << 16) & PLANE_MIN_DBUF_BLOCKS_MASK;
    }
    if interim_ddb != 0 {
        value |= (interim_ddb as u32) & PLANE_INTERIM_DBUF_BLOCKS_MASK;
    }
    if value != 0 {
        value |= PLANE_AUTO_MIN_DBUF_EN;
    }
    value
}

// upstream: skl_universal_plane.c skl_plane_wm_reg_val()
pub fn skl_plane_wm_reg_val(
    level: WmLevel,
    ignore_lines: bool,
    auto_min_alloc_wm_enable: bool,
    blocks: u16,
    lines: u16,
) -> u32 {
    (if level.enable { PLANE_WM_EN } else { 0 })
        | (if ignore_lines {
            PLANE_WM_IGNORE_LINES
        } else {
            0
        })
        | (if auto_min_alloc_wm_enable {
            PLANE_WM_AUTO_MIN_ALLOC_EN
        } else {
            0
        })
        | ((blocks as u32) & PLANE_WM_BLOCKS_MASK)
        | (((lines as u32) << 14) & PLANE_WM_LINES_MASK)
}

// upstream: skl_universal_plane.c skl_write_plane_wm()
pub fn skl_write_plane_wm(dsb: &mut impl PlaneDsb, plane_id: u8, crtc_state: &CrtcState) {
    let pipe = crtc_state.pipe;
    let wm = &crtc_state.wm[plane_id as usize];
    for level in 0..crtc_state.display.wm_levels.min(8) {
        dsb.write(
            pipe,
            plane_id,
            PlaneRegister::Wm,
            level,
            skl_plane_wm_reg_val(
                wm.levels[level as usize],
                wm.levels[level as usize].ignore_lines,
                wm.levels[level as usize].auto_min_alloc_wm_enable,
                wm.levels[level as usize].blocks,
                wm.levels[level as usize].lines,
            ),
        );
    }
    dsb.write(
        pipe,
        plane_id,
        PlaneRegister::WmTrans,
        0,
        skl_plane_wm_reg_val(
            wm.trans,
            wm.trans.ignore_lines,
            wm.trans.auto_min_alloc_wm_enable,
            wm.trans.blocks,
            wm.trans.lines,
        ),
    );
    if crtc_state.display.has_hw_sagv_wm {
        dsb.write(
            pipe,
            plane_id,
            PlaneRegister::WmSagv,
            0,
            skl_plane_wm_reg_val(
                wm.sagv,
                wm.sagv.ignore_lines,
                wm.sagv.auto_min_alloc_wm_enable,
                wm.sagv.blocks,
                wm.sagv.lines,
            ),
        );
        dsb.write(
            pipe,
            plane_id,
            PlaneRegister::WmSagvTrans,
            0,
            skl_plane_wm_reg_val(
                wm.sagv_trans,
                wm.sagv_trans.ignore_lines,
                wm.sagv_trans.auto_min_alloc_wm_enable,
                wm.sagv_trans.blocks,
                wm.sagv_trans.lines,
            ),
        );
    }
    dsb.write(
        pipe,
        plane_id,
        PlaneRegister::Buffer,
        0,
        skl_plane_ddb_reg_val(crtc_state.ddb[plane_id as usize]),
    );
    if crtc_state.display.display_ver < 11 {
        dsb.write(
            pipe,
            plane_id,
            PlaneRegister::Nv12Buffer,
            0,
            skl_plane_ddb_reg_val(crtc_state.ddb_y[plane_id as usize]),
        );
    }
    if crtc_state.display.display_ver >= 30 {
        dsb.write(
            pipe,
            plane_id,
            PlaneRegister::MinBuffer,
            0,
            xe3_plane_min_ddb_reg_val(
                crtc_state.min_ddb[plane_id as usize],
                crtc_state.interim_ddb[plane_id as usize],
            ),
        );
    }
}

// upstream: skl_universal_plane.c skl_plane_disable_arm()
pub fn skl_plane_disable_arm(dsb: &mut impl PlaneDsb, pipe: u8, plane_id: u8, crtc: &CrtcState) {
    skl_write_plane_wm(dsb, plane_id, crtc);
    dsb.write(pipe, plane_id, PlaneRegister::Control, 0, 0);
    dsb.write(pipe, plane_id, PlaneRegister::Surface, 0, 0);
}

// upstream: skl_universal_plane.c icl_plane_disable_sel_fetch_arm()
pub fn icl_plane_disable_sel_fetch_arm(
    dsb: &mut impl PlaneDsb,
    pipe: u8,
    plane_id: u8,
    psr2_sel_fetch: bool,
) {
    if psr2_sel_fetch {
        dsb.write(pipe, plane_id, PlaneRegister::SelectFetchCtl, 0, 0);
    }
}

// upstream: skl_universal_plane.c plane_has_normalizer()
pub fn plane_has_normalizer(
    display: DisplayInfo,
    plane_id: u8,
    has_pixel_normalizer: bool,
) -> bool {
    has_pixel_normalizer && icl_is_hdr_plane(display, plane_id)
}

// upstream: skl_universal_plane.c pixel_normalizer_value()
pub fn pixel_normalizer_value(needed: bool) -> u32 {
    if needed {
        PLANE_PIXEL_NORMALIZE_ENABLE | PLANE_PIXEL_NORMALIZE_NORM_FACTOR_1_0
    } else {
        0
    }
}

// upstream: skl_universal_plane.c icl_plane_disable_arm()
pub fn icl_plane_disable_arm(
    dsb: &mut impl PlaneDsb,
    display: DisplayInfo,
    pipe: u8,
    plane_id: u8,
    crtc: &CrtcState,
    has_normalizer: bool,
) {
    if icl_is_hdr_plane(display, plane_id) {
        dsb.write(pipe, plane_id, PlaneRegister::CusCtl, 0, 0);
    }
    skl_write_plane_wm(dsb, plane_id, crtc);
    icl_plane_disable_sel_fetch_arm(dsb, pipe, plane_id, crtc.enable_psr2_sel_fetch);
    if has_normalizer && icl_is_hdr_plane(display, plane_id) {
        dsb.write(pipe, plane_id, PlaneRegister::PixelNormalize, 0, 0);
    }
    dsb.write(pipe, plane_id, PlaneRegister::Control, 0, 0);
    dsb.write(pipe, plane_id, PlaneRegister::Surface, 0, 0);
}

// upstream: skl_universal_plane.c skl_plane_get_hw_state()
pub fn skl_plane_get_hw_state(
    dsb: &mut impl PlaneDsb,
    plane_id: u8,
    plane_pipe: u8,
) -> Option<(bool, u8)> {
    let wakeref = dsb.power_get_if_enabled(plane_pipe)?;
    let enabled = dsb.read_control(plane_pipe, plane_id) & (1 << 31) != 0;
    dsb.power_put(wakeref);
    Some((enabled, plane_pipe))
}

// upstream: skl_universal_plane.c skl_plane_ctl_format()
pub fn skl_plane_ctl_format(io: &impl PlaneFramework, pixel_format: u32) -> u32 {
    match pixel_format {
        DRM_FORMAT_C8 => 12 << 24,
        DRM_FORMAT_RGB565 => 14 << 24,
        DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => (4 << 24) | (1 << 20),
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => 4 << 24,
        DRM_FORMAT_XBGR2101010 | DRM_FORMAT_ABGR2101010 => (2 << 24) | (1 << 20),
        DRM_FORMAT_XRGB2101010 | DRM_FORMAT_ARGB2101010 => 2 << 24,
        DRM_FORMAT_XBGR16161616F | DRM_FORMAT_ABGR16161616F => (6 << 24) | (1 << 20),
        DRM_FORMAT_XRGB16161616F | DRM_FORMAT_ARGB16161616F => 6 << 24,
        DRM_FORMAT_XYUV8888 => 8 << 24,
        DRM_FORMAT_YUYV => 0 << 24,
        DRM_FORMAT_YVYU => 2 << 16,
        DRM_FORMAT_UYVY => 1 << 16,
        DRM_FORMAT_VYUY => 3 << 16,
        DRM_FORMAT_NV12 => 1 << 24,
        DRM_FORMAT_P010 => 3 << 24,
        DRM_FORMAT_P012 => 5 << 24,
        DRM_FORMAT_P016 => 7 << 24,
        DRM_FORMAT_Y210 => 1 << 23,
        DRM_FORMAT_Y212 => 3 << 23,
        DRM_FORMAT_Y216 => 5 << 23,
        DRM_FORMAT_XVYU2101010 => 7 << 23,
        DRM_FORMAT_XVYU12_16161616 => 9 << 23,
        DRM_FORMAT_XVYU16161616 => 11 << 23,
        _ => {
            io.missing_case(pixel_format as u64);
            0
        }
    }
}

// upstream: skl_universal_plane.c skl_plane_ctl_alpha()
pub fn skl_plane_ctl_alpha(io: &impl PlaneFramework, has_alpha: bool, blend_mode: u8) -> u32 {
    if !has_alpha {
        0
    } else {
        match blend_mode {
            0 => 0,
            1 => PLANE_CTL_ALPHA_SW_PREMULTIPLY,
            2 => PLANE_CTL_ALPHA_HW_PREMULTIPLY,
            v => {
                io.missing_case(v as u64);
                0
            }
        }
    }
}

// upstream: skl_universal_plane.c glk_plane_color_ctl_alpha()
pub fn glk_plane_color_ctl_alpha(io: &impl PlaneFramework, has_alpha: bool, blend_mode: u8) -> u32 {
    if !has_alpha {
        0
    } else {
        match blend_mode {
            0 => 0,
            1 => PLANE_COLOR_ALPHA_SW_PREMULTIPLY,
            2 => PLANE_COLOR_ALPHA_HW_PREMULTIPLY,
            v => {
                io.missing_case(v as u64);
                0
            }
        }
    }
}

// upstream: skl_universal_plane.c skl_plane_ctl_tiling()
pub fn skl_plane_ctl_tiling(io: &impl PlaneFramework, modifier: u64) -> u32 {
    match modifier {
        MOD_LINEAR => 0,
        MOD_X => PLANE_CTL_TILED_X,
        MOD_Y => PLANE_CTL_TILED_Y,
        MOD_4TILE => PLANE_CTL_TILED_4,
        MOD_4TILE_DG2_RC_CCS | MOD_4TILE_MTL_RC_CCS => {
            (PLANE_CTL_TILED_4)
                | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
                | PLANE_CTL_CLEAR_COLOR_DISABLE
        }
        MOD_4TILE_DG2_MC_CCS => {
            (PLANE_CTL_TILED_4)
                | PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE
                | PLANE_CTL_CLEAR_COLOR_DISABLE
        }
        MOD_4TILE_DG2_RC_CCS_CC | MOD_4TILE_MTL_RC_CCS_CC => {
            (PLANE_CTL_TILED_4) | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
        }
        MOD_4TILE_MTL_MC_CCS => (PLANE_CTL_TILED_4) | PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE,
        MOD_4TILE_BMG_CCS | MOD_4TILE_LNL_CCS => {
            (PLANE_CTL_TILED_4) | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
        }
        MOD_Y_CCS | MOD_Y_RC_CCS_CC => (PLANE_CTL_TILED_Y) | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE,
        MOD_Y_RC_CCS => {
            (PLANE_CTL_TILED_Y)
                | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE
                | PLANE_CTL_CLEAR_COLOR_DISABLE
        }
        MOD_Y_MC_CCS => (PLANE_CTL_TILED_Y) | PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE,
        MOD_YF => PLANE_CTL_TILED_4,
        MOD_YF_CCS => (PLANE_CTL_TILED_4) | PLANE_CTL_RENDER_DECOMPRESSION_ENABLE,
        _ => {
            io.missing_case(modifier);
            0
        }
    }
}

// upstream: skl_universal_plane.c skl_plane_ctl_rotate()
pub fn skl_plane_ctl_rotate(io: &impl PlaneFramework, rotation: u32) -> u32 {
    match rotation {
        1 => 0,
        2 => 3,
        4 => 2,
        8 => 1,
        v => {
            io.missing_case(v as u64);
            0
        }
    }
}

// upstream: skl_universal_plane.c icl_plane_ctl_flip()
pub fn icl_plane_ctl_flip(io: &impl PlaneFramework, reflect: u32) -> u32 {
    match reflect {
        0 => 0,
        0x10 => 1 << 8,
        v => {
            io.missing_case(v as u64);
            0
        }
    }
}

// upstream: skl_universal_plane.c adlp_plane_ctl_arb_slots()
pub fn adlp_plane_ctl_arb_slots(format: FormatInfo) -> u32 {
    if format.yuv_semiplanar {
        match format.cpp[0] {
            2 => 1 << 28,
            _ => 0,
        }
    } else {
        match format.cpp[0] {
            8 => 3 << 28,
            4 => 1 << 28,
            _ => 0,
        }
    }
}

// upstream: skl_universal_plane.c skl_plane_ctl_crtc()
pub fn skl_plane_ctl_crtc(display: DisplayInfo, gamma_enable: bool, csc_enable: bool) -> u32 {
    if display.display_ver >= 10 {
        0
    } else {
        (if gamma_enable { 1 << 30 } else { 0 }) | (if csc_enable { 1 << 23 } else { 0 })
    }
}

// upstream: skl_universal_plane.c skl_plane_ctl()
pub fn skl_plane_ctl(
    io: &impl PlaneFramework,
    display: DisplayInfo,
    format: u32,
    modifier: u64,
    rotation: u32,
    reflect: u32,
    has_alpha: bool,
    blend_mode: u8,
    key_flags: u32,
    color_encoding: u32,
    color_range_full: bool,
    adlp_wa: bool,
    format_info: FormatInfo,
) -> u32 {
    let mut ctl = PLANE_CTL_ENABLE;
    let transform = rotation | reflect;
    if display.display_ver < 10 {
        ctl |= skl_plane_ctl_alpha(io, has_alpha, blend_mode) | PLANE_CTL_PLANE_GAMMA_DISABLE;
        if color_encoding == 1 {
            ctl |= PLANE_CTL_YUV_TO_RGB_CSC_FORMAT_BT709;
        }
        if color_range_full {
            ctl |= PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE;
        }
    }
    ctl |= skl_plane_ctl_format(io, format)
        | skl_plane_ctl_tiling(io, modifier)
        | skl_plane_ctl_rotate(io, rotation & 0xf);
    if display.display_ver >= 11 {
        ctl |= icl_plane_ctl_flip(io, transform & 0x30);
    }
    if key_flags & 2 != 0 {
        ctl |= PLANE_CTL_KEY_ENABLE_DESTINATION;
    } else if key_flags & (1 << 2) != 0 {
        ctl |= PLANE_CTL_KEY_ENABLE_SOURCE;
    }
    if adlp_wa {
        ctl |= adlp_plane_ctl_arb_slots(format_info);
    }
    ctl
}

// upstream: skl_universal_plane.c glk_plane_color_ctl_crtc()
pub fn glk_plane_color_ctl_crtc(display: DisplayInfo, gamma_enable: bool, csc_enable: bool) -> u32 {
    if display.display_ver >= 11 {
        0
    } else {
        (if gamma_enable { 1 << 30 } else { 0 }) | (if csc_enable { 1 << 23 } else { 0 })
    }
}

// upstream: skl_universal_plane.c glk_plane_color_ctl()
pub fn glk_plane_color_ctl(
    io: &impl PlaneFramework,
    display: DisplayInfo,
    format: FormatInfo,
    plane_id: u8,
    has_alpha: bool,
    blend_mode: u8,
    color_encoding: u8,
    color_range_full: bool,
    force_black: bool,
    degamma: bool,
    ctm: bool,
    gamma: bool,
    gamma_size: u32,
) -> u32 {
    let mut ctl =
        (PLANE_COLOR_PLANE_GAMMA_DISABLE) | glk_plane_color_ctl_alpha(io, has_alpha, blend_mode);
    if format.is_yuv && !icl_is_hdr_plane(display, plane_id) {
        ctl |= match color_encoding {
            1 => PLANE_COLOR_CSC_MODE_YUV709_TO_RGB709,
            2 => PLANE_COLOR_CSC_MODE_YUV2020_TO_RGB2020,
            _ => PLANE_COLOR_CSC_MODE_YUV601_TO_RGB601,
        };
        if color_range_full {
            ctl |= PLANE_COLOR_YUV_RANGE_CORRECTION_DISABLE;
        }
    } else if format.is_yuv {
        ctl |= PLANE_COLOR_INPUT_CSC_ENABLE;
        if color_range_full {
            ctl |= PLANE_COLOR_YUV_RANGE_CORRECTION_DISABLE;
        }
    }
    if force_black || ctm {
        ctl |= PLANE_COLOR_PLANE_CSC_ENABLE;
    }
    if degamma {
        ctl |= PLANE_COLOR_PRE_CSC_GAMMA_ENABLE;
    }
    if gamma {
        ctl &= !(PLANE_COLOR_PLANE_GAMMA_DISABLE);
        if gamma_size != 32 {
            ctl |= PLANE_COLOR_POST_CSC_GAMMA_MULTSEG_ENABLE;
        }
    }
    ctl
}

// upstream: skl_universal_plane.c skl_surf_address()
pub fn skl_surf_address(offset: u32, uses_dpt: bool) -> u32 {
    if uses_dpt { offset >> 9 } else { offset }
}

// upstream: skl_universal_plane.c icl_plane_color_plane()
pub fn icl_plane_color_plane(planar_linked: bool, is_y_plane: bool) -> usize {
    if planar_linked && !is_y_plane { 1 } else { 0 }
}

// upstream: skl_universal_plane.c skl_plane_surf_offset()
pub fn skl_plane_surf_offset(offset: u32, uses_dpt: bool, decrypt: bool) -> u32 {
    skl_surf_address(offset, uses_dpt) | if decrypt { 1 << 2 } else { 0 }
}

// upstream: skl_universal_plane.c skl_plane_aux_dist()
pub fn skl_plane_aux_dist(
    main_address: u32,
    aux_address: u32,
    has_aux: bool,
    display_ver: u8,
    aux_stride: u32,
) -> u32 {
    if !has_aux {
        return 0;
    }
    let mut aux_dist = aux_address.wrapping_sub(main_address);
    if display_ver < 12 {
        aux_dist |= aux_stride & 0xfff;
    }
    aux_dist
}

// upstream: skl_universal_plane.c skl_plane_keyval()
pub fn skl_plane_keyval(min_value: u32) -> u32 {
    min_value
}

// upstream: skl_universal_plane.c skl_plane_keymax()
pub fn skl_plane_keymax(max_value: u32, alpha: u16) -> u32 {
    (max_value & 0x00ff_ffff) | (((alpha >> 8) as u32) << 24)
}

// upstream: skl_universal_plane.c skl_plane_keymsk()
pub fn skl_plane_keymsk(channel_mask: u32, alpha: u16) -> u32 {
    (channel_mask & 0x07ff_ffff) | if alpha >> 8 < 0xff { 1 << 31 } else { 0 }
}

// upstream: skl_universal_plane.c icl_plane_csc_load_black()
pub fn icl_plane_csc_load_black(dsb: &mut impl PlaneDsb, pipe: u8, plane: u8) {
    for i in 0..6 {
        dsb.write(pipe, plane, PlaneRegister::CscCoeff, i, 0);
    }
    for i in 0..3 {
        dsb.write(pipe, plane, PlaneRegister::CscPreoff, i, 0);
    }
    for i in 0..3 {
        dsb.write(pipe, plane, PlaneRegister::CscPostoff, i, 0);
    }
}

// upstream: skl_universal_plane.c skl_plane_update_noarm()
pub fn skl_plane_update_noarm(dsb: &mut impl PlaneDsb, crtc: &CrtcState, update: PlaneUpdate) {
    let (x, y) = if update.scaler_id >= 0 {
        (0, 0)
    } else {
        (update.dst.x1, update.dst.y1)
    };
    let src_w = (update.src.width() >> 16) as u32;
    let src_h = (update.src.height() >> 16) as u32;
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Stride,
        0,
        update.stride & 0xfff,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Position,
        0,
        ((y as u32) << 16) | (x as u32 & 0xffff),
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Size,
        0,
        ((src_h.wrapping_sub(1) & 0x1fff) << 16) | (src_w.wrapping_sub(1) & 0x1fff),
    );
    skl_write_plane_wm(dsb, update.plane_id, crtc);
}
// upstream: skl_universal_plane.c skl_plane_update_arm()
pub fn skl_plane_update_arm(
    dsb: &mut impl PlaneDsb,
    display: DisplayInfo,
    crtc: &CrtcState,
    update: PlaneUpdate,
    need_async_flip_toggle_wa: bool,
    async_flip_planes: u32,
) {
    let mut plane_ctl =
        update.control | skl_plane_ctl_crtc(display, crtc.gamma_enable, crtc.csc_enable);
    let mut color_ctl = 0;
    if need_async_flip_toggle_wa
        && async_flip_planes & (1u32.wrapping_shl(update.plane_id as u32)) != 0
    {
        plane_ctl |= 1 << 9;
    }
    if display.display_ver >= 10 {
        color_ctl = update.color_ctl
            | glk_plane_color_ctl_crtc(display, crtc.gamma_enable, crtc.csc_enable);
    }
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::KeyVal,
        0,
        update.keyval,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::KeyMask,
        0,
        update.keymask,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::KeyMax,
        0,
        update.keymax,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Offset,
        0,
        (update.view0.y << 16) | (update.view0.x & 0xffff),
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::AuxDist,
        0,
        update.aux_dist,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::AuxOffset,
        0,
        (update.view1.y << 16) | (update.view1.x & 0xffff),
    );
    if display.display_ver >= 10 {
        dsb.write(
            update.pipe,
            update.plane_id,
            PlaneRegister::ColorCtl,
            0,
            color_ctl,
        );
    }
    if update.scaler_id >= 0 {
        dsb.program_scaler(update.plane_id, crtc, &update);
    }
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Control,
        0,
        plane_ctl,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Surface,
        0,
        update.surface,
    );
}
// upstream: skl_universal_plane.c icl_plane_update_sel_fetch_noarm()
pub fn icl_plane_update_sel_fetch_noarm(
    dsb: &mut impl PlaneDsb,
    pipe: u8,
    plane: u8,
    sf: SelectFetch,
) {
    if !sf.enabled {
        return;
    }
    let y = if sf.su_region_et {
        (sf.dst.y1 - sf.su_area.y1).max(0)
    } else {
        sf.clip.y1 + sf.dst.y1
    };
    dsb.write(
        pipe,
        plane,
        PlaneRegister::SelectFetchPos,
        0,
        ((y as u32) << 16) | (sf.dst.x1 as u32 & 0xffff),
    );
    let surf_y = if sf.color_plane == 0 {
        sf.view.y as i32 + sf.clip.y1
    } else {
        sf.view.y as i32 + (sf.clip.y1 + 1) / 2
    };
    dsb.write(
        pipe,
        plane,
        PlaneRegister::SelectFetchOffset,
        0,
        ((surf_y as u32) << 16) | (sf.view.x & 0xffff),
    );
    dsb.write(
        pipe,
        plane,
        PlaneRegister::SelectFetchSize,
        0,
        (((sf.clip.height() - 1) as u32) << 16) | (((sf.src.width() >> 16) - 1) as u32 & 0xffff),
    );
}

// upstream: skl_universal_plane.c icl_plane_update_noarm()
pub fn icl_plane_update_noarm(
    dsb: &mut impl PlaneDsb,
    crtc: &CrtcState,
    update: PlaneUpdate,
    color_plane: usize,
    display: DisplayInfo,
    sel_fetch: SelectFetch,
) {
    let (x, y) = if update.scaler_id >= 0 {
        (0, 0)
    } else {
        (update.dst.x1, update.dst.y1)
    };
    let src_w = update.src.width() >> 16;
    let src_h = update.src.height() >> 16;
    let color_ctl =
        update.color_ctl | glk_plane_color_ctl_crtc(display, crtc.gamma_enable, crtc.csc_enable);
    dsb.program_color_pipeline(&update);
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Stride,
        0,
        update.stride & 0xfff,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Position,
        0,
        ((y as u32) << 16) | (x as u32 & 0xffff),
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Size,
        0,
        ((src_h.wrapping_sub(1) as u32 & 0x1fff) << 16) | (src_w.wrapping_sub(1) as u32 & 0x1fff),
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::KeyVal,
        0,
        update.keyval,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::KeyMask,
        0,
        update.keymask,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::KeyMax,
        0,
        update.keymax,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Offset,
        0,
        (update.view0.y << 16) | (update.view0.x & 0xffff),
    );
    if update.ccs_cc {
        dsb.write(
            update.pipe,
            update.plane_id,
            PlaneRegister::CcVal,
            0,
            update.ccval as u32,
        );
        dsb.write(
            update.pipe,
            update.plane_id,
            PlaneRegister::CcVal,
            1,
            (update.ccval >> 32) as u32,
        );
    }
    if update.aux_dist_enabled {
        dsb.write(
            update.pipe,
            update.plane_id,
            PlaneRegister::AuxDist,
            0,
            update.aux_dist,
        );
    }
    if icl_is_hdr_plane(display, update.plane_id) {
        dsb.write(
            update.pipe,
            update.plane_id,
            PlaneRegister::CusCtl,
            0,
            update.cus_ctl,
        );
    }
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::ColorCtl,
        0,
        color_ctl,
    );
    if update.yuv && icl_is_hdr_plane(display, update.plane_id) {
        icl_program_input_csc(dsb, update.pipe, update.plane_id, update.color_encoding);
    }
    skl_write_plane_wm(dsb, update.plane_id, crtc);
    if update.force_black {
        icl_plane_csc_load_black(dsb, update.pipe, update.plane_id);
    }
    icl_plane_update_sel_fetch_noarm(
        dsb,
        update.pipe,
        update.plane_id,
        SelectFetch {
            color_plane: color_plane as u8,
            ..sel_fetch
        },
    );
}
// upstream: skl_universal_plane.c icl_plane_update_sel_fetch_arm()
pub fn icl_plane_update_sel_fetch_arm(
    dsb: &mut impl PlaneDsb,
    pipe: u8,
    plane: u8,
    enabled: bool,
    clip: PlaneRect,
) {
    if !enabled {
        return;
    }
    if clip.height() > 0 {
        dsb.write(pipe, plane, PlaneRegister::SelectFetchCtl, 0, 1 << 31);
    } else {
        icl_plane_disable_sel_fetch_arm(dsb, pipe, plane, true);
    }
}

// upstream: skl_universal_plane.c icl_plane_update_arm()
pub fn icl_plane_update_arm(
    dsb: &mut impl PlaneDsb,
    crtc: &CrtcState,
    update: PlaneUpdate,
    display: DisplayInfo,
    select_fetch: SelectFetch,
    normalizer: bool,
) {
    let plane_ctl =
        update.control | skl_plane_ctl_crtc(display, crtc.gamma_enable, crtc.csc_enable);
    if update.scaler_id >= 0 {
        dsb.program_scaler(update.plane_id, crtc, &update);
    }
    icl_plane_update_sel_fetch_arm(
        dsb,
        update.pipe,
        update.plane_id,
        select_fetch.enabled,
        select_fetch.clip,
    );
    dsb.color_plane_commit_arm(&update);
    if plane_has_normalizer(display, update.plane_id, normalizer) {
        dsb.write(
            update.pipe,
            update.plane_id,
            PlaneRegister::PixelNormalize,
            0,
            pixel_normalizer_value(update.fbc_needs_pixel_normalizer),
        );
    }
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Control,
        0,
        plane_ctl,
    );
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Surface,
        0,
        update.surface,
    );
}
// upstream: skl_universal_plane.c skl_plane_capture_error()
pub fn skl_plane_capture_error(
    dsb: &mut impl PlaneDsb,
    pipe: u8,
    plane: u8,
    error: &mut PlaneErrorState,
) {
    error.control = dsb.read_control(pipe, plane);
    error.surface = dsb.read_reg(pipe, plane, PlaneRegister::Surface);
    error.surface_live = dsb.read_reg(pipe, plane, PlaneRegister::SurfaceLive);
}

// upstream: skl_universal_plane.c skl_plane_async_flip()
pub fn skl_plane_async_flip(
    dsb: &mut impl PlaneDsb,
    display: DisplayInfo,
    crtc: &CrtcState,
    update: PlaneUpdate,
    async_flip: bool,
) {
    let mut ctl = update.control | skl_plane_ctl_crtc(display, crtc.gamma_enable, crtc.csc_enable);
    let mut surf = update.surface;
    if async_flip {
        if display.display_ver >= 30 {
            surf |= 1;
        } else {
            ctl |= 1 << 9;
        }
    }
    dsb.write(update.pipe, update.plane_id, PlaneRegister::Control, 0, ctl);
    dsb.write(
        update.pipe,
        update.plane_id,
        PlaneRegister::Surface,
        0,
        surf,
    );
}
// upstream: skl_universal_plane.c intel_format_is_p01x()
pub fn intel_format_is_p01x(format: u32) -> bool {
    matches!(format, DRM_FORMAT_P010 | DRM_FORMAT_P012 | DRM_FORMAT_P016)
}

// upstream: skl_universal_plane.c skl_plane_check_fb()
pub fn skl_plane_check_fb(io: &impl PlaneDiagnostics, c: PlaneCheckConfig) -> i32 {
    if !c.has_fb {
        return 0;
    }
    let rot90 = c.rotation & (2 | 8) != 0;
    if c.rotation & !(1 | 4) != 0 && c.ccs_modifier {
        io.warning(
            PlaneDiagnostic::UnsupportedRotationWithCcs,
            c.rotation as i64,
            c.modifier as i64,
        );
        return -22;
    }
    if c.rotation & (1 << 4) != 0 && c.modifier == MOD_LINEAR && c.display_ver < 35 {
        io.warning(
            PlaneDiagnostic::UnsupportedLinearReflect,
            c.rotation as i64,
            c.display_ver as i64,
        );
        return -22;
    }
    if c.rotation & (1 << 4) != 0 && c.tile4_modifier && c.display_ver >= 20 {
        io.warning(
            PlaneDiagnostic::UnsupportedTile4Reflect,
            c.rotation as i64,
            c.display_ver as i64,
        );
        return -22;
    }
    if rot90 {
        if !c.rotation_supported {
            io.warning(
                PlaneDiagnostic::UnsupportedRotation,
                c.rotation as i64,
                c.format as i64,
            );
            return -22;
        }
        match c.format {
            DRM_FORMAT_RGB565 if c.display_ver >= 11 => {}
            DRM_FORMAT_RGB565
            | DRM_FORMAT_C8
            | DRM_FORMAT_XRGB16161616F
            | DRM_FORMAT_XBGR16161616F
            | DRM_FORMAT_ARGB16161616F
            | DRM_FORMAT_ABGR16161616F
            | DRM_FORMAT_Y210
            | DRM_FORMAT_Y212
            | DRM_FORMAT_Y216
            | DRM_FORMAT_XVYU12_16161616
            | DRM_FORMAT_XVYU16161616 => {
                io.warning(
                    PlaneDiagnostic::UnsupportedFormatRotation,
                    c.format as i64,
                    c.rotation as i64,
                );
                return -22;
            }
            _ => {}
        }
    }
    if c.interlace && c.modifier != MOD_LINEAR && c.modifier != MOD_X {
        io.warning(
            PlaneDiagnostic::UnsupportedInterlaceTiling,
            c.modifier as i64,
            c.display_ver as i64,
        );
        return -22;
    }
    if (c.alderlake_s || c.tigerlake) && c.color_key_source && intel_format_is_p01x(c.format) {
        io.warning(
            PlaneDiagnostic::UnsupportedColorKeyFormat,
            c.format as i64,
            c.display_ver as i64,
        );
        return -22;
    }
    0
}

// upstream: skl_universal_plane.c skl_plane_check_dst_coordinates()
pub fn skl_plane_check_dst_coordinates(
    io: &impl PlaneDiagnostics,
    display_ver: u8,
    dst: PlaneRect,
    pipe_src: PlaneRect,
) -> i32 {
    let x = dst.x1;
    let w = dst.width();
    let pw = pipe_src.width();
    if display_ver == 10 && (x + w < 4 || x > pw - 4) {
        let position = if x + w < 4 { x + w } else { x };
        io.warning(
            PlaneDiagnostic::InvalidDestination,
            position as i64,
            (pw - 4) as i64,
        );
        -34
    } else {
        0
    }
}

// upstream: skl_universal_plane.c skl_plane_check_nv12_rotation()
pub fn skl_plane_check_nv12_rotation(
    io: &impl PlaneFramework,
    format: FormatInfo,
    modifier: u64,
    src: PlaneRect,
    rotation: u32,
) -> i32 {
    let src_w = src.width() >> 16;
    if io.yuv_semiplanar(format, modifier)
        && src_w & 3 != 0
        && (rotation == 8 || rotation == (0x10 | 2))
    {
        io.warning(
            PlaneDiagnostic::InvalidNv12Rotation,
            src_w as i64,
            rotation as i64,
        );
        -22
    } else {
        0
    }
}

// upstream: skl_universal_plane.c skl_plane_max_scale()
pub fn skl_plane_max_scale(io: &impl PlaneFramework, display_ver: u8, fb: &Framebuffer) -> i32 {
    if display_ver >= 10 || !io.yuv_semiplanar(fb.format, fb.modifier) {
        0x30000 - 1
    } else {
        0x20000 - 1
    }
}

// upstream: skl_universal_plane.c intel_plane_min_width()
pub fn intel_plane_min_width(
    fb: &Framebuffer,
    color_plane: usize,
    min_width: Option<fn(&Framebuffer, usize, u32) -> i32>,
    rotation: u32,
) -> i32 {
    min_width.map_or(1, |f| f(fb, color_plane, rotation))
}

// upstream: skl_universal_plane.c intel_plane_min_height()
pub fn intel_plane_min_height(_fb: &Framebuffer, _color_plane: usize, _rotation: u32) -> i32 {
    1
}

// upstream: skl_universal_plane.c intel_plane_max_width()
pub fn intel_plane_max_width(
    fb: &Framebuffer,
    color_plane: usize,
    max_width: Option<fn(&Framebuffer, usize, u32) -> i32>,
    rotation: u32,
) -> i32 {
    max_width.map_or(i32::MAX, |f| f(fb, color_plane, rotation))
}

// upstream: skl_universal_plane.c intel_plane_max_height()
pub fn intel_plane_max_height(
    fb: &Framebuffer,
    color_plane: usize,
    max_height: Option<fn(&Framebuffer, usize, u32) -> i32>,
    rotation: u32,
) -> i32 {
    max_height.map_or(i32::MAX, |f| f(fb, color_plane, rotation))
}

// upstream: skl_universal_plane.c skl_check_main_ccs_coordinates()
pub fn skl_check_main_ccs_coordinates(
    ops: &mut impl PlaneSurfaceOps,
    main_x: i32,
    main_y: i32,
    main_offset: u32,
    ccs_plane: usize,
    aux_x: &mut i32,
    aux_y: &mut i32,
    aux_offset: &mut u32,
) -> bool {
    let alignment = ops.alignment(ccs_plane);
    let (hsub, vsub) = ops.subsampling(ccs_plane);
    while *aux_offset >= main_offset && *aux_y <= main_y {
        if *aux_x == main_x && *aux_y == main_y {
            break;
        }
        if *aux_offset == 0 {
            break;
        }
        let mut x = *aux_x / hsub;
        let mut y = *aux_y / vsub;
        *aux_offset = ops.adjust_aligned_offset(
            &mut x,
            &mut y,
            ccs_plane,
            *aux_offset,
            aux_offset.wrapping_sub(alignment),
        );
        *aux_x = x * hsub + *aux_x % hsub;
        *aux_y = y * vsub + *aux_y % vsub;
    }
    *aux_x == main_x && *aux_y == main_y
}

// upstream: skl_universal_plane.c skl_calc_main_surface_offset()
pub fn skl_calc_main_surface_offset(
    ops: &mut impl PlaneSurfaceOps,
    x: &mut i32,
    y: &mut i32,
    offset: &mut u32,
) -> i32 {
    let aux_plane = ops.main_to_aux_plane(0);
    let aux_offset = if aux_plane != 0 {
        ops.view(aux_plane).offset
    } else {
        0
    };
    let alignment = ops.alignment(0);
    let width = ops.source().width() >> 16;
    ops.add_fb_offsets(x, y, 0);
    *offset = ops.compute_aligned_offset(x, y, 0);
    if alignment != 0 && !alignment.is_power_of_two() {
        ops.warning(
            PlaneDiagnostic::InvalidSurfaceAlignment,
            alignment as i64,
            0,
        );
        return -22;
    }
    // AUX offset is relative to the main surface and must remain non-negative.
    if aux_plane != 0 && *offset > aux_offset {
        *offset =
            ops.adjust_aligned_offset(x, y, 0, *offset, aux_offset & !alignment.wrapping_sub(1));
    }
    // X tiling faults when the source right edge extends past the stride.
    if ops.modifier() == MOD_X {
        let cpp = ops.source_cpp(0);
        while (*x + width) * cpp > ops.mapping_stride(0) as i32 {
            if *offset == 0 {
                ops.warning(PlaneDiagnostic::UnableMainOffset, *x as i64, *y as i64);
                return -22;
            }
            *offset = ops.adjust_aligned_offset(x, y, 0, *offset, *offset - alignment);
        }
    }
    0
}

// upstream: skl_universal_plane.c skl_check_main_surface()
pub fn skl_check_main_surface(ops: &mut impl PlaneValidationOps) -> i32 {
    let src = ops.source();
    let rotation = ops.rotation();
    let mut x = src.x1 >> 16;
    let mut y = src.y1 >> 16;
    let w = src.width() >> 16;
    let h = src.height() >> 16;
    let (min_w, min_h) = ops.min_size(0, rotation);
    let (max_w, max_h) = ops.max_size(0, rotation);
    let alignment = ops.alignment(0);
    let aux_plane = ops.main_to_aux_plane(0);
    if w > max_w || w < min_w || h > max_h || h < min_h {
        ops.warning(
            PlaneDiagnostic::InvalidSourceSize,
            ((w as i64) << 32) | (h as u32 as i64),
            ((max_w as i64) << 32) | (max_h as u32 as i64),
        );
        return -22;
    }
    let mut offset = 0;
    let ret = skl_calc_main_surface_offset(ops, &mut x, &mut y, &mut offset);
    if ret != 0 {
        return ret;
    }
    if ops.is_ccs_modifier() && aux_plane != 0 {
        loop {
            let mut aux = ops.view(aux_plane);
            let mut aux_x = aux.x as i32;
            let mut aux_y = aux.y as i32;
            let matched = skl_check_main_ccs_coordinates(
                ops,
                x,
                y,
                offset,
                aux_plane,
                &mut aux_x,
                &mut aux_y,
                &mut aux.offset,
            );
            aux.x = aux_x as u32;
            aux.y = aux_y as u32;
            ops.set_view(aux_plane, aux);
            if matched {
                break;
            }
            if offset == 0 {
                break;
            }
            offset = ops.adjust_aligned_offset(
                &mut x,
                &mut y,
                0,
                offset,
                offset.wrapping_sub(alignment),
            );
        }
        let aux = ops.view(aux_plane);
        if x != aux.x as i32 || y != aux.y as i32 {
            ops.warning(
                PlaneDiagnostic::UnableCcsOffset,
                ((x as i64) << 32) | (y as u32 as i64),
                ((aux.x as i64) << 32) | (aux.y as i64),
            );
            return -22;
        }
    }
    if ops.display_ver() >= 13 {
        ops.warning(
            PlaneDiagnostic::InvalidSurfaceAlignment,
            ((x as i64) << 32) | (y as u32 as i64),
            65535,
        );
    } else {
        ops.warning(
            PlaneDiagnostic::InvalidSurfaceAlignment,
            ((x as i64) << 32) | (y as u32 as i64),
            8191,
        );
    }
    ops.set_surface(0, offset, x, y);
    let dx = (x << 16) - src.x1;
    let dy = (y << 16) - src.y1;
    ops.set_source(PlaneRect {
        x1: src.x1 + dx,
        y1: src.y1 + dy,
        x2: src.x2 + dx,
        y2: src.y2 + dy,
    });
    0
}

// upstream: skl_universal_plane.c skl_check_nv12_aux_surface()
pub fn skl_check_nv12_aux_surface(ops: &mut impl PlaneValidationOps) -> i32 {
    let uv_plane = 1usize;
    let rotation = ops.rotation();
    let ccs_plane = if ops.is_ccs_modifier() {
        ops.main_to_aux_plane(uv_plane)
    } else {
        0
    };
    let (min_w, min_h) = ops.min_size(uv_plane, rotation);
    let (max_w, max_h) = ops.max_size(uv_plane, rotation);
    let src = ops.source();
    let lx = src.x1 >> 16;
    let ly = src.y1 >> 16;
    let lw = src.width() >> 16;
    let lh = src.height() >> 16;
    let mut x = (lx + 1) / 2;
    let mut y = (ly + 1) / 2;
    let w = (lx + lw + 1) / 2 - x;
    let h = (ly + lh + 1) / 2 - y;
    if w > max_w || w < min_w || h > max_h || h < min_h {
        ops.warning(
            PlaneDiagnostic::InvalidSourceSize,
            ((w as i64) << 32) | (h as u32 as i64),
            ((max_w as i64) << 32) | (max_h as u32 as i64),
        );
        return -22;
    }
    ops.add_fb_offsets(&mut x, &mut y, uv_plane);
    let mut offset = ops.compute_aligned_offset(&mut x, &mut y, uv_plane);
    if ccs_plane != 0 {
        let aux_offset = ops.view(ccs_plane).offset;
        let alignment = ops.alignment(uv_plane);
        if offset > aux_offset {
            offset = ops.adjust_aligned_offset(
                &mut x,
                &mut y,
                uv_plane,
                offset,
                aux_offset & !alignment.wrapping_sub(1),
            );
        }
        loop {
            let mut ccs = ops.view(ccs_plane);
            let mut aux_x = ccs.x as i32;
            let mut aux_y = ccs.y as i32;
            let matched = skl_check_main_ccs_coordinates(
                ops,
                x,
                y,
                offset,
                ccs_plane,
                &mut aux_x,
                &mut aux_y,
                &mut ccs.offset,
            );
            ccs.x = aux_x as u32;
            ccs.y = aux_y as u32;
            ops.set_view(ccs_plane, ccs);
            if matched {
                break;
            }
            if offset == 0 {
                break;
            }
            offset = ops.adjust_aligned_offset(
                &mut x,
                &mut y,
                uv_plane,
                offset,
                offset.wrapping_sub(alignment),
            );
        }
        let ccs = ops.view(ccs_plane);
        if x != ccs.x as i32 || y != ccs.y as i32 {
            ops.warning(
                PlaneDiagnostic::UnableCcsOffset,
                ((x as i64) << 32) | (y as u32 as i64),
                ((ccs.x as i64) << 32) | (ccs.y as i64),
            );
            return -22;
        }
    }
    if ops.display_ver() >= 13 {
        ops.warning(
            PlaneDiagnostic::InvalidSurfaceAlignment,
            ((x as i64) << 32) | (y as u32 as i64),
            65535,
        );
    } else {
        ops.warning(
            PlaneDiagnostic::InvalidSurfaceAlignment,
            ((x as i64) << 32) | (y as u32 as i64),
            8191,
        );
    }
    ops.set_surface(uv_plane, offset, x, y);
    0
}

// upstream: skl_universal_plane.c skl_check_ccs_aux_surface()
pub fn skl_check_ccs_aux_surface(ops: &mut impl PlaneValidationOps) -> i32 {
    let src = ops.source();
    let src_x = src.x1 >> 16;
    let src_y = src.y1 >> 16;
    for ccs_plane in 0..ops.num_color_planes() {
        if !ops.is_ccs_aux_plane(ccs_plane) {
            continue;
        }
        let (main_hsub, main_vsub) = ops.subsampling(ops.ccs_to_main_plane(ccs_plane));
        let (mut hsub, mut vsub) = ops.subsampling(ccs_plane);
        hsub *= main_hsub;
        vsub *= main_vsub;
        let mut x = src_x / hsub;
        let mut y = src_y / vsub;
        ops.add_fb_offsets(&mut x, &mut y, ccs_plane);
        let offset = ops.compute_aligned_offset(&mut x, &mut y, ccs_plane);
        let out_x = (x * hsub + src_x % hsub) / main_hsub;
        let out_y = (y * vsub + src_y % vsub) / main_vsub;
        ops.set_surface(ccs_plane, offset, out_x, out_y);
    }
    0
}

// upstream: skl_universal_plane.c skl_check_plane_surface()
pub fn skl_check_plane_surface(ops: &mut impl PlaneValidationOps) -> i32 {
    let ret = ops.compute_gtt();
    if ret != 0 {
        return ret;
    }
    if !ops.visible() {
        return 0;
    }
    if ops.is_ccs_modifier() {
        let ret = skl_check_ccs_aux_surface(ops);
        if ret != 0 {
            return ret;
        }
    }
    if ops.is_yuv_semiplanar() {
        let ret = skl_check_nv12_aux_surface(ops);
        if ret != 0 {
            return ret;
        }
    }
    skl_check_main_surface(ops)
}

// upstream: skl_universal_plane.c skl_fb_scalable()
pub fn skl_fb_scalable(display_ver: u8, format: u32, has_fb: bool) -> bool {
    if !has_fb {
        return false;
    }
    match format {
        0x20203843 => false,
        DRM_FORMAT_XRGB16161616F
        | DRM_FORMAT_ARGB16161616F
        | DRM_FORMAT_XBGR16161616F
        | DRM_FORMAT_ABGR16161616F => display_ver >= 11,
        _ => true,
    }
}

// upstream: skl_universal_plane.c check_protection()
pub fn check_protection(ops: &mut impl PlaneProtectionOps) {
    if ops.display_ver() < 11 {
        return;
    }
    let decrypt = ops.key_check();
    ops.set_decrypt(decrypt);
    ops.set_force_black(ops.is_protected() && !decrypt);
}

// upstream: skl_universal_plane.c make_damage_viewport_relative()
pub fn make_damage_viewport_relative(
    damage: &mut PlaneRect,
    src: PlaneRect,
    rotation: u32,
    fb_width: i32,
    fb_height: i32,
    has_fb: bool,
    visible: bool,
) {
    if damage.width() <= 0 || damage.height() <= 0 {
        return;
    }
    if !has_fb || !visible {
        *damage = PlaneRect::default();
        return;
    }
    if rotation & (2 | 8) != 0 {
        let old = *damage;
        *damage = PlaneRect {
            x1: fb_height - old.y2,
            y1: old.x1,
            x2: fb_height - old.y1,
            y2: old.x2,
        };
        damage.x1 -= src.y1 >> 16;
        damage.x2 -= src.y1 >> 16;
        damage.y1 -= src.x1 >> 16;
        damage.y2 -= src.x1 >> 16;
    } else {
        damage.x1 -= src.x1 >> 16;
        damage.x2 -= src.x1 >> 16;
        damage.y1 -= src.y1 >> 16;
        damage.y2 -= src.y1 >> 16;
    }
    let _ = fb_width;
}

// upstream: skl_universal_plane.c clip_damage()
pub fn clip_damage(damage: &mut PlaneRect, src: PlaneRect) {
    if damage.width() <= 0 || damage.height() <= 0 {
        return;
    }
    let src_x = src.x1 >> 16;
    let src_y = src.y1 >> 16;
    let src_w = src.width() >> 16;
    let src_h = src.height() >> 16;
    let src_int = PlaneRect {
        x1: src_x,
        y1: src_y,
        x2: src_x + src_w,
        y2: src_y + src_h,
    };
    damage.x1 += src_int.x1;
    damage.x2 += src_int.x1;
    damage.y1 += src_int.y1;
    damage.y2 += src_int.y1;
    damage.x1 = damage.x1.max(src_int.x1);
    damage.y1 = damage.y1.max(src_int.y1);
    damage.x2 = damage.x2.min(src_int.x2);
    damage.y2 = damage.y2.min(src_int.y2);
}

// upstream: skl_universal_plane.c skl_plane_check()
pub fn skl_plane_check(ops: &mut impl PlaneValidationOps) -> i32 {
    let config = ops.check_config();
    let ret = skl_plane_check_fb(ops, config);
    if ret != 0 {
        return ret;
    }
    let mut min_scale = 0;
    let mut max_scale = 0;
    if ops.key_flags() == 0
        && skl_fb_scalable(ops.display_ver(), ops.format(), ops.check_config().has_fb)
    {
        min_scale = 1;
        max_scale = if ops.display_ver() >= 10 || !ops.is_yuv_semiplanar() {
            0x30000 - 1
        } else {
            0x20000 - 1
        };
    }
    let ret = ops.check_clipping(min_scale, max_scale, true);
    if ret != 0 {
        return ret;
    }
    let src = ops.source();
    let mut damage = ops.damage();
    let (fb_width, fb_height) = ops.framebuffer_size();
    make_damage_viewport_relative(
        &mut damage,
        src,
        ops.rotation(),
        fb_width,
        fb_height,
        ops.check_config().has_fb,
        ops.visible(),
    );
    ops.set_damage(damage);
    let ret = skl_check_plane_surface(ops);
    if ret != 0 {
        return ret;
    }
    if !ops.visible() {
        return 0;
    }
    let config = ops.check_config();
    let ret = skl_plane_check_dst_coordinates(ops, config.display_ver, config.dst, config.pipe_src);
    if ret != 0 {
        return ret;
    }
    let ret = ops.check_source();
    if ret != 0 {
        return ret;
    }
    let mut damage = ops.damage();
    clip_damage(&mut damage, ops.source());
    ops.set_damage(damage);
    let ret = if ops.is_yuv_semiplanar()
        && (ops.source().width() >> 16) & 3 != 0
        && (ops.rotation() == 8 || ops.rotation() == (0x10 | 2))
    {
        -22
    } else {
        0
    };
    if ret != 0 {
        return ret;
    }
    check_protection(ops);
    if ops.alpha() >> 8 == 0 {
        ops.set_visible(false);
        ops.set_damage(PlaneRect::default());
    }
    let control = ops.control_input();
    ops.set_ctl(skl_plane_ctl(
        ops,
        ops.display_info(),
        control.format,
        control.modifier,
        control.rotation,
        control.rotation & 0x30,
        control.has_alpha,
        control.blend_mode,
        control.key_flags,
        control.color_encoding,
        control.color_range_full,
        control.adlp_wa,
        control.format_info,
    ));
    if config.display_ver >= 10 {
        let color = ops.color_input();
        ops.set_color_ctl(glk_plane_color_ctl(
            ops,
            ops.display_info(),
            color.format,
            color.plane_id,
            color.has_alpha,
            color.blend_mode,
            color.color_encoding,
            color.color_range_full,
            color.force_black,
            color.degamma,
            color.ctm,
            color.gamma,
            color.gamma_size,
        ));
    }
    if ops.is_yuv_semiplanar() && icl_is_hdr_plane(ops.display_info(), ops.plane_id()) {
        ops.set_cus_ctl(PLANE_CUS_ENABLE | PLANE_CUS_VPHASE_SIGN_NEGATIVE | PLANE_CUS_VPHASE_0_25);
    } else {
        ops.set_cus_ctl(0);
    }
    0
}

// upstream: skl_universal_plane.c icl_link_nv12_planes()
pub fn icl_link_nv12_planes(
    io: &impl PlaneDiagnostics,
    display: DisplayInfo,
    uv_plane: u8,
    y_plane: u8,
    uv_cus: &mut u32,
    y_ctl: &mut u32,
) {
    if icl_is_nv12_y_plane(display, uv_plane) {
        io.warning(PlaneDiagnostic::InvalidPlanePair, uv_plane as i64, 1);
    }
    if !icl_is_nv12_y_plane(display, y_plane) {
        io.warning(PlaneDiagnostic::InvalidPlanePair, y_plane as i64, 0);
    }
    *y_ctl |= PLANE_CTL_YUV420_Y_PLANE;
    if icl_is_hdr_plane(display, uv_plane) {
        *uv_cus |= match y_plane {
            7 | 6 | 5 | 4 => PLANE_CUS_Y_PLANE_MASK,
            _ => 0,
        };
    }
}

// upstream: skl_universal_plane.c skl_plane_fbc()
pub fn skl_plane_fbc(display: DisplayInfo, pipe: u8, plane_id: u8) -> Option<u8> {
    let id = skl_fbc_id_for_pipe(pipe);
    if skl_plane_has_fbc(display, id, plane_id) {
        Some(id)
    } else {
        None
    }
}

// upstream: skl_universal_plane.c skl_plane_has_planar()
pub fn skl_plane_has_planar(
    display_ver: u8,
    skylake: bool,
    broxton: bool,
    pipe: u8,
    plane_id: u8,
) -> bool {
    let _ = display_ver;
    !skylake && !broxton && pipe != 2 && (plane_id == 1 || plane_id == 2)
}

// upstream: skl_universal_plane.c skl_get_plane_formats()
pub fn skl_get_plane_formats(
    display_ver: u8,
    skylake: bool,
    broxton: bool,
    pipe: u8,
    plane_id: u8,
) -> &'static [u32] {
    if skl_plane_has_planar(display_ver, skylake, broxton, pipe, plane_id) {
        SKL_PLANAR_FORMATS
    } else {
        SKL_PLANE_FORMATS
    }
}

// upstream: skl_universal_plane.c glk_plane_has_planar()
pub fn glk_plane_has_planar(plane_id: u8) -> bool {
    plane_id == 1 || plane_id == 2
}

// upstream: skl_universal_plane.c glk_get_plane_formats()
pub fn glk_get_plane_formats(plane_id: u8) -> &'static [u32] {
    if glk_plane_has_planar(plane_id) {
        GLK_PLANAR_FORMATS
    } else {
        SKL_PLANE_FORMATS
    }
}

// upstream: skl_universal_plane.c icl_get_plane_formats()
pub fn icl_get_plane_formats(display: DisplayInfo, plane_id: u8) -> &'static [u32] {
    if icl_is_hdr_plane(display, plane_id) {
        ICL_HDR_PLANE_FORMATS
    } else if icl_is_nv12_y_plane(display, plane_id) {
        ICL_SDR_Y_PLANE_FORMATS
    } else {
        ICL_SDR_UV_PLANE_FORMATS
    }
}

// upstream: skl_universal_plane.c skl_plane_format_mod_supported()
pub fn skl_plane_format_mod_supported(
    io: &impl PlaneFormatOps,
    format: u32,
    modifier: u64,
) -> bool {
    if !io.supports_modifier(modifier) {
        return false;
    }
    let rgba = matches!(
        format,
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_XBGR8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_ABGR8888
    );
    let yf_group = rgba
        || matches!(
            format,
            DRM_FORMAT_RGB565
                | DRM_FORMAT_XRGB2101010
                | DRM_FORMAT_XBGR2101010
                | DRM_FORMAT_ARGB2101010
                | DRM_FORMAT_ABGR2101010
                | DRM_FORMAT_YUYV
                | DRM_FORMAT_YVYU
                | DRM_FORMAT_UYVY
                | DRM_FORMAT_VYUY
                | DRM_FORMAT_NV12
                | DRM_FORMAT_XYUV8888
                | DRM_FORMAT_P010
                | DRM_FORMAT_P012
                | DRM_FORMAT_P016
                | DRM_FORMAT_XVYU2101010
        );
    let linear_group = yf_group
        || matches!(
            format,
            DRM_FORMAT_C8
                | DRM_FORMAT_XBGR16161616F
                | DRM_FORMAT_ABGR16161616F
                | DRM_FORMAT_XRGB16161616F
                | DRM_FORMAT_ARGB16161616F
                | DRM_FORMAT_Y210
                | DRM_FORMAT_Y212
                | DRM_FORMAT_Y216
                | DRM_FORMAT_XVYU12_16161616
                | DRM_FORMAT_XVYU16161616
        );
    if rgba && io.is_ccs(modifier) {
        return true;
    }
    if yf_group && modifier == MOD_YF {
        return true;
    }
    linear_group && matches!(modifier, MOD_LINEAR | MOD_X | MOD_Y)
}
// upstream: skl_universal_plane.c icl_plane_format_mod_supported()
pub fn icl_plane_format_mod_supported(
    io: &impl PlaneFormatOps,
    format: u32,
    modifier: u64,
) -> bool {
    if !io.supports_modifier(modifier) {
        return false;
    }
    let rgba10 = matches!(
        format,
        DRM_FORMAT_XRGB8888
            | DRM_FORMAT_XBGR8888
            | DRM_FORMAT_ARGB8888
            | DRM_FORMAT_ABGR8888
            | DRM_FORMAT_XRGB2101010
            | DRM_FORMAT_XBGR2101010
            | DRM_FORMAT_ARGB2101010
            | DRM_FORMAT_ABGR2101010
    );
    let yf_group = rgba10
        || matches!(
            format,
            DRM_FORMAT_RGB565
                | DRM_FORMAT_YUYV
                | DRM_FORMAT_YVYU
                | DRM_FORMAT_UYVY
                | DRM_FORMAT_VYUY
                | DRM_FORMAT_NV12
                | DRM_FORMAT_XYUV8888
                | DRM_FORMAT_P010
                | DRM_FORMAT_P012
                | DRM_FORMAT_P016
                | DRM_FORMAT_XVYU2101010
        );
    let linear_group = yf_group
        || matches!(
            format,
            DRM_FORMAT_C8
                | DRM_FORMAT_XBGR16161616F
                | DRM_FORMAT_ABGR16161616F
                | DRM_FORMAT_XRGB16161616F
                | DRM_FORMAT_ARGB16161616F
                | DRM_FORMAT_Y210
                | DRM_FORMAT_Y212
                | DRM_FORMAT_Y216
                | DRM_FORMAT_XVYU12_16161616
                | DRM_FORMAT_XVYU16161616
        );
    if rgba10 && io.is_ccs(modifier) {
        return true;
    }
    if yf_group && modifier == MOD_YF {
        return true;
    }
    linear_group && matches!(modifier, MOD_LINEAR | MOD_X | MOD_Y)
}
// upstream: skl_universal_plane.c tgl_plane_format_mod_supported()
pub fn tgl_plane_format_mod_supported(
    io: &impl PlaneFormatOps,
    format: u32,
    modifier: u64,
) -> bool {
    if !io.supports_modifier(modifier) {
        return false;
    }
    let ccs_group = matches!(
        format,
        DRM_FORMAT_XRGB8888
            | DRM_FORMAT_XBGR8888
            | DRM_FORMAT_ARGB8888
            | DRM_FORMAT_ABGR8888
            | DRM_FORMAT_XRGB2101010
            | DRM_FORMAT_XBGR2101010
            | DRM_FORMAT_ARGB2101010
            | DRM_FORMAT_ABGR2101010
            | DRM_FORMAT_XBGR16161616F
            | DRM_FORMAT_ABGR16161616F
            | DRM_FORMAT_XRGB16161616F
            | DRM_FORMAT_ARGB16161616F
    );
    if ccs_group && io.is_ccs(modifier) {
        return true;
    }
    let mc_group = ccs_group
        || matches!(
            format,
            DRM_FORMAT_YUYV
                | DRM_FORMAT_YVYU
                | DRM_FORMAT_UYVY
                | DRM_FORMAT_VYUY
                | DRM_FORMAT_NV12
                | DRM_FORMAT_XYUV8888
                | DRM_FORMAT_P010
                | DRM_FORMAT_P012
                | DRM_FORMAT_P016
        );
    if mc_group && io.is_mc_ccs(modifier) {
        return true;
    }
    let uncompressed_group = mc_group
        || matches!(
            format,
            DRM_FORMAT_RGB565
                | DRM_FORMAT_XVYU2101010
                | DRM_FORMAT_C8
                | DRM_FORMAT_Y210
                | DRM_FORMAT_Y212
                | DRM_FORMAT_Y216
                | DRM_FORMAT_XVYU12_16161616
                | DRM_FORMAT_XVYU16161616
        );
    uncompressed_group && !io.is_ccs(modifier)
}

// upstream: skl_universal_plane.c skl_plane_enable_flip_done()
pub fn skl_plane_enable_flip_done(io: &mut impl PlaneIrqOps, pipe: u8, plane: u8) {
    io.lock();
    io.enable_flip_done(pipe, plane);
    io.unlock();
}

// upstream: skl_universal_plane.c skl_plane_disable_flip_done()
pub fn skl_plane_disable_flip_done(io: &mut impl PlaneIrqOps, pipe: u8, plane: u8) {
    io.lock();
    io.disable_flip_done(pipe, plane);
    io.unlock();
}

// upstream: skl_universal_plane.c skl_plane_has_rc_ccs()
pub fn skl_plane_has_rc_ccs(pipe: u8, plane: u8) -> bool {
    pipe != 2 && (plane == 1 || plane == 2)
}

// upstream: skl_universal_plane.c skl_plane_caps()
pub fn skl_plane_caps(pipe: u8, plane: u8) -> u8 {
    (1 << 3 | 1 << 4 | 1 << 5)
        | if skl_plane_has_rc_ccs(pipe, plane) {
            1
        } else {
            0
        }
}

// upstream: skl_universal_plane.c glk_plane_has_rc_ccs()
pub fn glk_plane_has_rc_ccs(pipe: u8) -> bool {
    pipe != 2
}

// upstream: skl_universal_plane.c glk_plane_caps()
pub fn glk_plane_caps(pipe: u8) -> u8 {
    (1 << 3 | 1 << 4 | 1 << 5) | if glk_plane_has_rc_ccs(pipe) { 1 } else { 0 }
}

// upstream: skl_universal_plane.c icl_plane_caps()
pub fn icl_plane_caps() -> u8 {
    (1 << 3) | (1 << 4) | (1 << 5) | 1
}

// upstream: skl_universal_plane.c tgl_plane_has_mc_ccs()
pub fn tgl_plane_has_mc_ccs(plane: u8, wa_14010477008: bool) -> bool {
    !wa_14010477008 && plane < 6
}

// upstream: skl_universal_plane.c tgl_plane_caps()
pub fn tgl_plane_caps(
    plane: u8,
    wa_14010477008: bool,
    has_4tile: bool,
    display_ver: u8,
    dgfx: bool,
) -> u8 {
    let mut caps = (1 << 0) | (1 << 1) | (1 << 3);
    caps |= if has_4tile { 1 << 6 } else { 1 << 4 };
    if tgl_plane_has_mc_ccs(plane, wa_14010477008) {
        caps |= 1 << 2;
    }
    if display_ver >= 14 && dgfx {
        caps |= 1 << 7;
    }
    caps
}

// upstream: skl_universal_plane.c skl_disable_tiling()
pub fn skl_disable_tiling(
    io: &mut impl PlaneLifecycleOps,
    pipe: u8,
    plane: u8,
    scanout_stride: u32,
    surface: u32,
) -> u32 {
    let mut ctl = io.disable_tiling_read_ctl(pipe, plane);
    if io.is_dpt() {
        ctl &= !(1 << 15);
    } else {
        ctl &= !(7 << 10);
        io.disable_tiling_write_stride(pipe, plane, scanout_stride / 64);
    }
    io.disable_tiling_write_ctl(pipe, plane, ctl);
    io.disable_tiling_write_surface(pipe, plane, surface);
    ctl
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedPlane<P> {
    pub framework_plane: P,
    pub config: PlaneCreateConfig,
}

// upstream: skl_universal_plane.c skl_universal_plane_create()
pub fn skl_universal_plane_create<T: PlaneLifecycleOps>(
    io: &mut T,
    display: DisplayInfo,
    pipe: u8,
    plane_id: u8,
) -> Result<CreatedPlane<T::Plane>, i32> {
    let skylake = io.skylake();
    let broxton = io.broxton();
    let mut c = PlaneCreateConfig {
        display_ver: display.display_ver,
        pipe,
        plane_id,
        ..PlaneCreateConfig::default()
    };
    c.frontbuffer_bit = 1u32.wrapping_shl(pipe as u32 * 8 + plane_id as u32);
    c.fbc_id = skl_plane_fbc(display, pipe, plane_id);
    c.disable_tiling_handler = true;
    c.surf_offset_handler = true;
    c.update_noarm_policy = if display.display_ver >= 11 { 1 } else { 0 };
    c.update_arm_policy = if display.display_ver >= 11 { 1 } else { 0 };
    c.disable_arm_policy = if display.display_ver >= 11 { 1 } else { 0 };
    c.capture_error_handler = true;
    c.get_hw_state_handler = true;
    c.check_plane_handler = true;

    if display.display_ver >= 30 {
        c.min_width_policy = 1; // adl_plane_min_width()
        c.max_width_policy = 4; // xe3_plane_max_width()
        c.max_height_policy = 2; // icl_plane_max_height()
        c.min_cdclk_policy = 1; // icl_plane_min_cdclk()
    } else if display.display_ver >= 11 {
        c.min_width_policy = if display.display_ver >= 14 || display.alderlake_p {
            1
        } else {
            2
        };
        c.max_width_policy = if icl_is_hdr_plane(display, plane_id) {
            2
        } else {
            3
        };
        c.max_height_policy = 2;
        c.min_cdclk_policy = 1;
    } else if display.display_ver >= 10 {
        c.max_width_policy = 1; // glk_plane_max_width()
        c.max_height_policy = 1; // skl_plane_max_height()
        c.min_cdclk_policy = 2; // glk_plane_min_cdclk()
    } else {
        c.max_width_policy = 0; // skl_plane_max_width()
        c.max_height_policy = 0;
        c.min_cdclk_policy = 0;
    }
    c.max_stride_policy = if display.display_ver >= 13 { 1 } else { 0 };
    c.min_alignment_policy = if display.display_ver >= 12 { 1 } else { 0 };
    c.vtd_guard = if io.scanout_needs_vtd_wa() {
        if display.display_ver >= 10 { 168 } else { 136 }
    } else {
        0
    };
    c.update_gen11_plus = display.display_ver >= 11;
    c.async_flip = io.has_async_flips() && plane_id == 1;
    c.async_flip_toggle_wa =
        c.async_flip && (display.display_ver == 9 || display.display_ver == 10);
    c.async_flip_policy = if c.async_flip {
        if display.display_ver >= 12 {
            2
        } else if display.display_ver == 11 {
            1
        } else {
            0
        }
    } else {
        0
    };
    c.flip_done_irq = c.async_flip;

    c.format_set = if display.display_ver >= 11 {
        if icl_is_hdr_plane(display, plane_id) {
            PlaneFormatSet::IclHdr
        } else if icl_is_nv12_y_plane(display, plane_id) {
            PlaneFormatSet::IclSdrY
        } else {
            PlaneFormatSet::IclSdrUv
        }
    } else if display.display_ver >= 10 {
        if glk_plane_has_planar(plane_id) {
            PlaneFormatSet::GlkPlanar
        } else {
            PlaneFormatSet::Skl
        }
    } else if skl_plane_has_planar(display.display_ver, skylake, broxton, pipe, plane_id) {
        PlaneFormatSet::SklPlanar
    } else {
        PlaneFormatSet::Skl
    };
    c.function_set = if display.display_ver >= 12 {
        2
    } else if display.display_ver == 11 {
        1
    } else {
        0
    };
    c.plane_type_primary = plane_id == 1;

    if display.display_ver >= 13 {
        c.supported_rotations = 1 | 4;
    } else {
        c.supported_rotations = 1 | 2 | 4 | 8;
    }
    if display.display_ver >= 11 {
        c.supported_rotations |= 1 << 4;
    }
    c.supported_csc = (1 << 0) | (1 << 1);
    if display.display_ver >= 10 {
        c.supported_csc |= 1 << 2;
    }
    c.supported_color_range = (1 << 0) | (1 << 1);
    c.default_color_encoding = 1; // DRM_COLOR_YCBCR_BT709
    c.default_color_range = 0; // DRM_COLOR_YCBCR_LIMITED_RANGE
    c.has_color_pipeline = display.display_ver >= 12;
    c.has_damage_clips = display.display_ver >= 12;
    c.has_alpha_property = true;
    c.blend_modes = (1 << 0) | (1 << 1) | (1 << 2);
    c.zpos_immutable = true;
    c.has_scaling_filter = display.display_ver >= 11;
    c.scaling_filters = (1 << 0) | (1 << 1);
    c.async_format_mod_supported = true;

    c.caps = if display.display_ver >= 12 {
        tgl_plane_caps(
            plane_id,
            io.display_wa_14010477008(),
            display.has_4tile,
            display.display_ver,
            io.dgfx(),
        )
    } else if display.display_ver == 11 {
        icl_plane_caps()
    } else if display.display_ver == 10 {
        glk_plane_caps(pipe)
    } else {
        skl_plane_caps(pipe, plane_id)
    };

    let mut plane = match io.alloc_plane() {
        Some(plane) => plane,
        None => return Err(-12),
    };
    if let Some(fbc_id) = c.fbc_id {
        io.add_plane_to_fbc(&mut plane, &c, fbc_id);
    }
    let ret = io.universal_plane_init(&mut plane, &c);
    if ret != 0 {
        io.free_plane(plane);
        return Err(ret);
    }

    io.create_rotation_property(&mut plane, 1, c.supported_rotations);
    io.create_color_properties(
        &mut plane,
        c.supported_csc,
        c.supported_color_range,
        c.default_color_encoding,
        c.default_color_range,
    );
    if c.has_color_pipeline {
        io.initialize_color_pipeline_plane(&mut plane, pipe);
    }
    io.create_alpha_property(&mut plane);
    io.create_blend_mode_property(&mut plane, c.blend_modes);
    io.create_zpos_immutable_property(&mut plane, plane_id);
    if c.has_damage_clips {
        io.enable_fb_damage_clips(&mut plane);
    }
    if c.has_scaling_filter {
        io.create_scaling_filter_property(&mut plane, c.scaling_filters);
    }
    io.plane_helper_add(&mut plane);
    Ok(CreatedPlane {
        framework_plane: plane,
        config: c,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialPlaneRegister {
    Control,
    ColorControl,
    Surface,
    Offset,
    Size,
    Stride,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InitialPlaneConfig {
    pub pipe: u8,
    pub plane_id: u8,
    pub base: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub rotation: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub size: u64,
    pub format: FormatInfo,
}
pub trait InitialPlaneIo: PlaneDiagnostics {
    fn get_hw_state(&mut self, plane_id: u8, crtc_pipe: u8) -> Option<u8>;
    fn joiner_active(&self) -> bool;
    fn allocate_initial_framebuffer(&mut self) -> bool;
    fn release_initial_framebuffer(&mut self);
    fn warn_joiner(&self);
    fn read_plane_reg(&mut self, pipe: u8, plane: u8, reg: InitialPlaneRegister) -> u32;
    fn format_info(&self, fourcc: u32, modifier: u64) -> Option<FormatInfo>;
    fn stride_multiplier(&self, format: FormatInfo, modifier: u64, color_plane: usize) -> u32;
    fn aligned_height(&self, format: FormatInfo, modifier: u64, height: u32) -> u32;
}
// upstream: skl_universal_plane.c skl_get_initial_plane_config()
pub fn skl_get_initial_plane_config(
    io: &mut impl InitialPlaneIo,
    display: DisplayInfo,
    pipe: u8,
    plane_id: u8,
) -> Option<InitialPlaneConfig> {
    let hw_pipe = io.get_hw_state(plane_id, pipe)?;
    io.warning(PlaneDiagnostic::UnexpectedPipe, hw_pipe as i64, pipe as i64);
    if io.joiner_active() {
        io.warn_joiner();
        return None;
    }
    if !io.allocate_initial_framebuffer() {
        return None;
    }
    let ctl = io.read_plane_reg(hw_pipe, plane_id, InitialPlaneRegister::Control);
    let pixel_format = if display.display_ver >= 11 {
        ctl & (0x1f << 23)
    } else {
        ctl & (0x0f << 24)
    };
    let alpha_field = if display.display_ver >= 10 {
        (io.read_plane_reg(hw_pipe, plane_id, InitialPlaneRegister::ColorControl) >> 4) & 3
    } else {
        (ctl >> 4) & 3
    };
    let fourcc = skl_format_to_fourcc(pixel_format, ctl & (1 << 20) != 0, alpha_field != 0);
    let tiling = (ctl >> 10) & 7;
    let modifier = match tiling {
        0 => MOD_LINEAR,
        1 => MOD_X,
        4 => {
            if ctl & (1 << 15) != 0 {
                if display.display_ver >= 14 {
                    MOD_4TILE_MTL_RC_CCS
                } else if display.display_ver >= 12 {
                    MOD_Y_RC_CCS
                } else {
                    MOD_Y_CCS
                }
            } else if ctl & (1 << 4) != 0 {
                if display.display_ver >= 14 {
                    MOD_4TILE_MTL_MC_CCS
                } else {
                    MOD_Y_MC_CCS
                }
            } else {
                MOD_Y
            }
        }
        5 => {
            if display.has_4tile {
                let rc = (1 << 15) | (1 << 13);
                if ctl & rc == rc {
                    MOD_4TILE_DG2_RC_CCS
                } else if ctl & (1 << 4) != 0 {
                    MOD_4TILE_DG2_MC_CCS
                } else if ctl & (1 << 15) != 0 {
                    MOD_4TILE_DG2_RC_CCS_CC
                } else {
                    MOD_4TILE
                }
            } else if ctl & (1 << 15) != 0 {
                MOD_YF_CCS
            } else {
                MOD_YF
            }
        }
        _ => {
            io.warning(
                PlaneDiagnostic::MissingCase,
                tiling as i64,
                display.display_ver as i64,
            );
            io.release_initial_framebuffer();
            return None;
        }
    };
    let format = match io.format_info(fourcc, modifier) {
        Some(format) => format,
        None => {
            io.release_initial_framebuffer();
            return None;
        }
    };
    let rotation = match ctl & 3 {
        0 => 1,
        1 => 8,
        2 => 4,
        _ => 2,
    };
    let rotation = rotation
        | if display.display_ver >= 11 && ctl & (1 << 8) != 0 {
            1 << 4
        } else {
            0
        };
    if rotation & (2 | 8) != 0 {
        io.release_initial_framebuffer();
        return None;
    }
    let base = io.read_plane_reg(hw_pipe, plane_id, InitialPlaneRegister::Surface) & 0xffff_f000;
    let plane_offset = io.read_plane_reg(hw_pipe, plane_id, InitialPlaneRegister::Offset);
    io.warning(PlaneDiagnostic::NonzeroPlaneOffset, plane_offset as i64, 0);
    let size = io.read_plane_reg(hw_pipe, plane_id, InitialPlaneRegister::Size);
    let width = (size & 0xffff) + 1;
    let height = (size >> 16) + 1;
    let stride = io.read_plane_reg(hw_pipe, plane_id, InitialPlaneRegister::Stride);
    let pitch = (stride & 0xfff) * io.stride_multiplier(format, modifier, 0);
    let aligned_height = io.aligned_height(format, modifier, height);
    Some(InitialPlaneConfig {
        pipe: hw_pipe,
        plane_id,
        base,
        fourcc,
        modifier,
        rotation,
        width,
        height,
        pitch,
        size: pitch as u64 * aligned_height as u64,
        format,
    })
}
pub trait InitialPlaneFixupIo {
    fn plane_visible(&self) -> bool;
    fn state_surface(&self) -> u32;
    fn write_plane_surface(&mut self, pipe: u8, plane: u8, surface: u32);
}
// upstream: skl_universal_plane.c skl_fixup_initial_plane_config()
pub fn skl_fixup_initial_plane_config(
    io: &mut impl InitialPlaneFixupIo,
    pipe: u8,
    plane: u8,
    base: u32,
) -> bool {
    if !io.plane_visible() {
        return false;
    }
    let surface = io.state_surface();
    if base == surface {
        return false;
    }
    io.write_plane_surface(pipe, plane, surface);
    true
}
