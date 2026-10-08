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
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Translated from Linux 7.2.3 drivers/gpu/drm/i915/display/intel_fbc.c
//! (Intel permissive/MIT-style license), copyright © 2014 Intel Corporation.
//! DRM objects, allocator, locks, workqueues and debugfs are explicit external
//! ownership. Register plans preserve source ordering and do no implicit MMIO.
//! The four XE3P system-cache functions only apply at display version 35+ and
//! are outside the requested display-12/13 range. Four debugfs status/registration
//! entry points are framework glue; the false-color callbacks remain translated.
#![allow(dead_code, clippy::too_many_arguments, non_upper_case_globals)]

extern crate alloc;
use alloc::vec::Vec;
use core::cmp::{max, min};

pub const FBC_CTL_EN: u32 = 1 << 31;
pub const DPFC_CTL_EN: u32 = 1 << 31;
pub const DPFC_CTL_PLANE_MASK_G4X: u32 = 1 << 30;
pub const DPFC_CTL_FENCE_EN_G4X: u32 = 1 << 29;
pub const DPFC_CTL_PLANE_MASK_IVB: u32 = 3 << 29;
pub const DPFC_CTL_FENCE_EN_IVB: u32 = 1 << 28;
pub const DPFC_CTL_PLANE_BINDING_MASK: u32 = 3 << 11;
pub const DPFC_CTL_FALSE_COLOR: u32 = 1 << 10;
pub const DPFC_CTL_LIMIT_1X: u32 = 0 << 6;
pub const DPFC_CTL_LIMIT_2X: u32 = 1 << 6;
pub const DPFC_CTL_LIMIT_4X: u32 = 2 << 6;
pub const SNB_DPFC_FENCE_EN: u32 = 1 << 29;
pub const SNB_DPFC_FENCENO_MASK: u32 = 0x1f;
pub const FBC_STRIDE_OVERRIDE: u32 = 1 << 15;
pub const FBC_STRIDE_MASK: u32 = 0x7fff;
pub const FBC_DIRTY_RECT_EN: u32 = 1 << 31;
pub const FBC_REND_NUKE: u32 = 1 << 2;
pub const FBC_REND_CACHE_CLEAN: u32 = 1 << 1;
pub const FBC_CTL_PERIODIC: u32 = 1 << 30;
pub const FBC_CTL_INTERVAL_MASK: u32 = 0x3fff << 16;
pub const FBC_CTL_C3_IDLE: u32 = 1 << 13;
pub const FBC_CTL_STRIDE_MASK: u32 = 0xff << 5;
pub const FBC_CTL_FENCENO_MASK: u32 = 0xf;
pub const FBC_CTL_FENCE_DBL: u32 = 1 << 4;
pub const FBC_CTL_CPU_FENCE_EN: u32 = 1 << 1;
pub const DPFC_CTL_SR_EN: u32 = 1 << 10;
pub const DPFC_CTL_FENCENO_MASK: u32 = 0xf;
pub const DPFC_COMP_SEG_MASK: u32 = 0x7ff;
pub const DPFC_COMP_SEG_MASK_IVB: u32 = 0xfff;
pub const MOD_LINEAR: u64 = 0;
pub const MOD_X_TILED: u64 = 0x0100_0000_0000_0001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FbcError {
    InvalidLimit,
    ArithmeticOverflow,
    NoStolenMemory,
    InvalidDirtyRect,
    Io,
    Unsupported,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Xrgb8888,
    Xbgr8888,
    Argb8888,
    Abgr8888,
    Xrgb1555,
    Rgb565,
    Argb16161616F,
    Abgr16161616F,
    Xrgb16161616,
    Xbgr16161616,
    Argb16161616,
    Abgr16161616,
    Other(u32),
}
impl PixelFormat {
    pub const fn cpp(self) -> u32 {
        match self {
            Self::Xrgb1555 | Self::Rgb565 => 2,
            Self::Xrgb8888 | Self::Xbgr8888 | Self::Argb8888 | Self::Abgr8888 => 4,
            Self::Argb16161616F
            | Self::Abgr16161616F
            | Self::Xrgb16161616
            | Self::Xbgr16161616
            | Self::Argb16161616
            | Self::Abgr16161616 => 8,
            Self::Other(_) => 0,
        }
    }
    pub const fn has_alpha(self) -> bool {
        matches!(
            self,
            Self::Argb8888
                | Self::Abgr8888
                | Self::Argb16161616F
                | Self::Abgr16161616F
                | Self::Argb16161616
                | Self::Abgr16161616
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rotation {
    Rotate0,
    Rotate90,
    Rotate180,
    Rotate270,
}
impl Rotation {
    const fn swaps_axes(self) -> bool {
        matches!(self, Self::Rotate90 | Self::Rotate270)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayCaps {
    pub version: u8,
    pub g4x: bool,
    pub haswell: bool,
    pub broadwell: bool,
    pub broxton: bool,
    pub skylake: bool,
    pub kabylake: bool,
    pub coffeelake: bool,
    pub cometlake: bool,
    pub dg2: bool,
    pub battlemage: bool,
    pub wa_16023588340: bool,
    pub wa_15018326506: bool,
    pub wa_14016291713: bool,
    pub wa_16011863758: bool,
    pub wa_1409120013: bool,
    pub wa_22014263786: bool,
    pub vtd_active: bool,
    pub max_cdclk_freq: u32,
    pub pixel_normalizer: bool,
    pub has_dirty_rect: bool,
    pub has_fbc_sys_cache: bool,
    pub clock_gate_wa: bool,
    pub has_fenced_regions: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayState {
    pub caps: DisplayCaps,
    pub enable_fbc_param: i32,
    pub stolen_initialized: bool,
    pub vgpu_active: bool,
}
impl DisplayState {
    pub fn sanitized_fbc_enabled(self, has_fbc: bool) -> bool {
        if self.enable_fbc_param >= 0 {
            self.enable_fbc_param != 0
        } else {
            has_fbc && (self.caps.broadwell || self.caps.version >= 9)
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}
impl Rect {
    pub const fn empty() -> Self {
        Self {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }
    }
    pub const fn visible(self) -> bool {
        self.x1 < self.x2 && self.y1 < self.y2
    }
    pub const fn width(self) -> i32 {
        self.x2 - self.x1
    }
    pub const fn height(self) -> i32 {
        self.y2 - self.y1
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Framebuffer {
    pub format: PixelFormat,
    pub modifier: u64,
}
/// FBC-consumed subset of intel_plane_state. Source dimensions are integer pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneState {
    pub framebuffer: Framebuffer,
    pub mapping_stride: u32,
    pub src_width: u32,
    pub src_height: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub view_x: u32,
    pub view_y: u32,
    pub rotation: Rotation,
    pub fence_id: i8,
    pub fence_y_offset: u32,
    pub visible: bool,
    pub pixel_blend_mode: u32,
    pub frontbuffer_bit: u32,
    pub damage: Rect,
    pub has_sel_update: bool,
    pub has_psr: bool,
    pub has_panel_replay: bool,
    pub interlaced: bool,
    pub double_wide: bool,
    pub needs_modeset: bool,
    pub plane_id: u8,
}
pub trait FbcPipeGraph {
    fn primary_plane_for_pipe(&self, pipe: u8) -> Option<u8>;
    fn fbc_for_plane(&self, plane_id: u8) -> Option<FbcId>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneMetrics {
    pub plane_stride_pixels: u32,
    pub plane_cfb_stride: u32,
    pub cfb_stride: u32,
    pub cfb_size: u32,
    pub override_cfb_stride: u16,
}

fn align_up(v: u32, a: u32) -> Result<u32, FbcError> {
    if a == 0 || !a.is_power_of_two() {
        return Err(FbcError::InvalidLimit);
    }
    v.checked_add(a - 1)
        .map(|x| x & !(a - 1))
        .ok_or(FbcError::ArithmeticOverflow)
}
pub fn plane_metrics(d: &DisplayCaps, p: &PlaneState) -> Result<PlaneMetrics, FbcError> {
    Ok(PlaneMetrics {
        plane_stride_pixels: intel_fbc_plane_stride(p),
        plane_cfb_stride: intel_fbc_plane_cfb_stride(p)?,
        cfb_stride: intel_fbc_cfb_stride(d, p)?,
        cfb_size: intel_fbc_cfb_size(d, p)?,
        override_cfb_stride: intel_fbc_override_cfb_stride(d, p)?,
    })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoFbcReason {
    StolenMemoryNotInitialized,
    VgpuActive,
    Disabled,
    PlaneNotVisible,
    Wa16023588340,
    Wa15018326506,
    VtdEnabled,
    InterlacedMode,
    DoubleWidePipe,
    SelectiveUpdateEnabled,
    Psr1EnabledWa14016291713,
    PixelFormat,
    Tiling,
    Rotation,
    Stride,
    PerPixelAlpha,
    PlaneSize,
    SurfaceSize,
    PlaneStartMisaligned,
    PlaneEndMisaligned,
    PixelRateTooHigh,
    FramebufferNotFenced,
    FifoUnderrun,
    FifoUnderrunCleared,
    NotEnoughStolenMemory,
    UpdatePending,
    EnabledNotActive,
}
/// Ordered eligibility checks in intel_fbc_check_plane(); allocation/fence enable gates follow separately.
/// Find the ratio using the exact speculative 2x allocation and halved retries.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CfbAllocation {
    pub limit: u8,
    pub framebuffer_bytes: u32,
    pub line_buffer_bytes: u32,
}
pub trait StolenAllocator {
    fn insert(&mut self, line_buffer: bool, size: u32, alignment: u32, end: u64) -> bool;
    fn remove(&mut self, line_buffer: bool);
    /// Free the two per-instance stolen-node descriptors during display cleanup.
    fn free_instance(&mut self, id: FbcId);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FbcId {
    A,
    B,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FbcRegister {
    CfbBase(FbcId),
    Control(FbcId),
    Status(FbcId),
    Status2(FbcId),
    FenceYOffset(FbcId),
    SnbFenceControl,
    SnbFenceOffset,
    GlkStride(FbcId),
    DirtyControl(FbcId),
    DirtyRect(FbcId),
    RenderState(FbcId),
    Chicken(FbcId),
    ChickenMisc4,
    LegacyControl,
    LegacyControl2,
    LegacyStatus,
    LegacyCommand,
    LegacyTag(u16),
    LegacyFenceOff,
    PlaneAddr(u8),
    PlaneSurface(u8),
    GlobalDpfcControl,
    GlobalDpfcStatus,
    GlobalDpfcStatus2,
    GlobalDpfcFenceYOffset,
    GlobalDpfcFenceControl,
    GlobalDpfcCbBase,
    IlkChicken(FbcId),
    SkLStrideChicken,
    LegacyLlBase,
    Dg2ClockGateDis,
    MtlPipeClockGateDis(FbcId),
    DebugStatus(FbcId),
}
pub trait FbcRegisterIo {
    fn read(&mut self, reg: FbcRegister) -> Result<u32, FbcError>;
    fn write(&mut self, reg: FbcRegister, value: u32) -> Result<(), FbcError>;
    fn posting_read(&mut self, reg: FbcRegister) -> Result<u32, FbcError>;
    /// Backend implements the source's bounded polling interval for this register.
    fn wait_for_clear_ms(
        &mut self,
        reg: FbcRegister,
        mask: u32,
        timeout_ms: u32,
    ) -> Result<bool, FbcError>;
    fn rmw(&mut self, reg: FbcRegister, clear: u32, set: u32) -> Result<(), FbcError> {
        let value = self.read(reg)?;
        self.write(reg, (value & !clear) | set)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HardwareConfig {
    pub display_version: u8,
    pub id: FbcId,
    pub limit: u8,
    pub plane_id: u8,
    pub i9xx_plane: u8,
    pub ivybridge: bool,
    pub fence_id: i8,
    pub fence_y_offset: u32,
    pub has_fences: bool,
    pub has_dirty_rect: bool,
    pub false_color: bool,
    pub override_cfb_stride: u16,
}
fn dpfc_limit(limit: u8) -> Result<u32, FbcError> {
    match limit {
        1 => Ok(DPFC_CTL_LIMIT_1X),
        2 => Ok(DPFC_CTL_LIMIT_2X),
        4 => Ok(DPFC_CTL_LIMIT_4X),
        _ => Err(FbcError::InvalidLimit),
    }
}
pub fn glk_stride_value(c: &HardwareConfig) -> Result<u32, FbcError> {
    if c.override_cfb_stride == 0 {
        return Ok(0);
    }
    if c.limit == 0 {
        return Err(FbcError::InvalidLimit);
    }
    Ok(FBC_STRIDE_OVERRIDE
        | ((u32::from(c.override_cfb_stride) / u32::from(c.limit)) & FBC_STRIDE_MASK))
}
pub fn deactivate_hardware(
    io: &mut impl FbcRegisterIo,
    c: &HardwareConfig,
) -> Result<(), FbcError> {
    if c.has_dirty_rect {
        io.write(FbcRegister::DirtyControl(c.id), 0)?
    }
    let ctl = io.read(FbcRegister::Control(c.id))?;
    if ctl & DPFC_CTL_EN != 0 {
        io.write(FbcRegister::Control(c.id), ctl & !DPFC_CTL_EN)?
    }
    Ok(())
}
pub fn hardware_is_active(
    io: &mut impl FbcRegisterIo,
    c: &HardwareConfig,
) -> Result<bool, FbcError> {
    Ok(io.read(FbcRegister::Control(c.id))? & DPFC_CTL_EN != 0)
}
pub fn program_cfb(
    io: &mut impl FbcRegisterIo,
    c: &HardwareConfig,
    stolen_offset: u32,
) -> Result<(), FbcError> {
    io.write(FbcRegister::CfbBase(c.id), stolen_offset)
}
pub fn nuke(io: &mut impl FbcRegisterIo, id: FbcId) -> Result<(), FbcError> {
    io.write(FbcRegister::RenderState(id), FBC_REND_NUKE)?;
    io.posting_read(FbcRegister::RenderState(id))?;
    Ok(())
}

pub fn dirty_rect_value(rect: Rect) -> Result<u32, FbcError> {
    if rect.y2 == 0 || rect.y1 < 0 || rect.y2 < rect.y1 {
        return Err(FbcError::InvalidDirtyRect);
    }
    let start = u32::try_from(rect.y1).map_err(|_| FbcError::InvalidDirtyRect)?;
    let end = u32::try_from(rect.y2 - 1).map_err(|_| FbcError::InvalidDirtyRect)?;
    if start > 0xffff || end > 0xffff {
        return Err(FbcError::InvalidDirtyRect);
    }
    Ok((end << 16) | start)
}
pub fn initial_dirty_rect(p: &PlaneState) -> Rect {
    Rect {
        x1: p.src_x as i32,
        y1: p.src_y as i32,
        x2: p.src_x.saturating_add(p.src_width) as i32,
        y2: p.src_y.saturating_add(p.src_height) as i32,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FbOrigin {
    Flip,
    CursorUpdate,
    Cpu,
    Gpu,
    Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FbcPlaneConfig {
    pub plane_id: u8,
    pub frontbuffer_bit: u32,
    pub format: PixelFormat,
    pub modifier: u64,
    pub stride_pixels: u32,
    pub cfb_stride: u32,
    pub cfb_size: u32,
    pub override_cfb_stride: u16,
    pub fence_id: i8,
    pub fence_y_offset: u32,
    pub interval: u16,
    pub dirty_rect: Rect,
}
impl FbcPlaneConfig {
    pub fn from_plane(d: &DisplayCaps, p: &PlaneState, refresh_hz: u16) -> Result<Self, FbcError> {
        let m = plane_metrics(d, p)?;
        Ok(Self {
            plane_id: p.plane_id,
            frontbuffer_bit: p.frontbuffer_bit,
            format: p.framebuffer.format,
            modifier: p.framebuffer.modifier,
            stride_pixels: m.plane_stride_pixels,
            cfb_stride: m.cfb_stride,
            cfb_size: m.cfb_size,
            override_cfb_stride: m.override_cfb_stride,
            fence_id: p.fence_id,
            fence_y_offset: p.fence_y_offset,
            interval: refresh_hz,
            dirty_rect: Rect::empty(),
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FbcRuntime {
    pub id: FbcId,
    pub plane: Option<FbcPlaneConfig>,
    pub limit: u8,
    pub allocation_bytes: u32,
    pub active: bool,
    pub activated: bool,
    pub flip_pending: bool,
    pub underrun_detected: bool,
    pub busy_bits: u32,
    pub false_color: bool,
    pub no_fbc_reason: Option<NoFbcReason>,
    pub cfb_allocated: bool,
    pub llb_allocated: bool,
    pub generation: FbcGeneration,
    pub funcs: &'static FbcFunctionTable,
}
impl Default for FbcRuntime {
    fn default() -> Self {
        Self {
            id: FbcId::A,
            plane: None,
            limit: 0,
            allocation_bytes: 0,
            active: false,
            activated: false,
            flip_pending: false,
            underrun_detected: false,
            busy_bits: 0,
            false_color: false,
            no_fbc_reason: None,
            cfb_allocated: false,
            llb_allocated: false,
            generation: FbcGeneration::I8xx,
            funcs: &I8XX_FBC_FUNCS,
        }
    }
}
impl FbcRuntime {
    pub fn frontbuffer_bit(&self) -> u32 {
        self.plane.map_or(0, |p| p.frontbuffer_bit)
    }
}

/// Explicit display-core boundary. Implementors own locking, MMIO dispatch,
/// stolen-node allocation, system-cache serialization, workqueues and vblank.
pub trait FbcControlHooks {
    fn lock_instance(&mut self, id: FbcId);
    fn unlock_instance(&mut self, id: FbcId);
    fn lock_sys_cache(&mut self);
    fn unlock_sys_cache(&mut self);
    fn is_hardware_active(&mut self, r: &FbcRuntime) -> Result<bool, FbcError>;
    fn is_compressing(&mut self, r: &FbcRuntime) -> Result<bool, FbcError>;
    fn hardware_activate(&mut self, r: &FbcRuntime) -> Result<(), FbcError>;
    fn hardware_deactivate(&mut self, r: &FbcRuntime) -> Result<(), FbcError>;
    fn nuke(&mut self, r: &FbcRuntime) -> Result<(), FbcError>;
    fn allocate_cfb(
        &mut self,
        id: FbcId,
        d: &DisplayCaps,
        size: u32,
        min_limit: u8,
    ) -> Result<CfbAllocation, FbcError>;
    fn cleanup_cfb(&mut self, id: FbcId, r: &mut FbcRuntime) -> Result<(), FbcError>;
    fn program_workarounds(&mut self, id: FbcId, d: &DisplayCaps) -> Result<(), FbcError>;
    fn program_cfb(&mut self, id: FbcId, r: &FbcRuntime) -> Result<(), FbcError>;
    fn initialize_dirty_rect(&mut self, id: FbcId, p: &PlaneState) -> Result<(), FbcError>;
    fn program_dirty_rect(&mut self, id: FbcId, rect: Rect) -> Result<(), FbcError>;
    fn sys_cache_enable(&mut self, id: FbcId, r: &FbcRuntime) -> Result<(), FbcError>;
    fn sys_cache_disable(&mut self, id: FbcId) -> Result<(), FbcError>;
    fn set_compressor_clock_gate(
        &mut self,
        id: FbcId,
        d: &DisplayCaps,
        disable: bool,
    ) -> Result<(), FbcError>;
    fn set_false_color(&mut self, r: &FbcRuntime, enable: bool) -> Result<(), FbcError>;
    fn cancel_underrun_work(&mut self, id: FbcId);
    fn queue_underrun_work(&mut self, id: FbcId);
    fn wait_for_next_vblank(&mut self, pipe: u8) -> Result<(), FbcError>;
    fn allocate_instance_container(&mut self, id: FbcId) -> bool;
    fn allocate_stolen_node(&mut self, id: FbcId, line_buffer: bool) -> bool;
    fn free_stolen_node(&mut self, id: FbcId, line_buffer: bool);
    fn initialize_lock_and_underrun_work(&mut self, id: FbcId);
    fn clear_system_cache(&mut self) -> Result<(), FbcError>;
    fn free_instance_container(&mut self, id: FbcId);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FbcPlaneChange {
    pub pipe: u8,
    pub has_fbc: bool,
    pub fbc_id: Option<FbcId>,
    pub old_state: Option<PlaneState>,
    pub new_state: PlaneState,
    pub old_reason: Option<NoFbcReason>,
    pub no_fbc_reason: Option<NoFbcReason>,
    pub pixel_rate: u32,
    pub refresh_hz: u16,
}
pub struct FbcAtomicState {
    pub crtc_pipe: u8,
    pub needs_fastset: bool,
    pub new_planes: Vec<FbcPlaneChange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneFbcBinding {
    pub plane_id: u8,
    pub fbc_id: Option<FbcId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FbcDisplay {
    pub enable_fbc_param: i32,
    pub instances: [Option<FbcRuntime>; 2],
    pub sys_cache_id: u8,
}
impl FbcDisplay {
    pub fn empty() -> Self {
        Self {
            enable_fbc_param: 0,
            instances: [None, None],
            sys_cache_id: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FbcGeneration {
    I8xx,
    I965,
    G4x,
    Ilk,
    Snb,
    Ivb,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FbcCallback {
    I8xxActivate,
    I8xxDeactivate,
    I8xxIsActive,
    I8xxIsCompressing,
    I8xxNuke,
    I8xxProgramCfb,
    I965Nuke,
    G4xActivate,
    G4xDeactivate,
    G4xIsActive,
    G4xIsCompressing,
    G4xProgramCfb,
    IlkActivate,
    IlkDeactivate,
    IlkIsActive,
    IlkIsCompressing,
    IlkProgramCfb,
    SnbActivate,
    SnbNuke,
    IvbActivate,
    IvbIsCompressing,
    IvbSetFalseColor,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FbcFunctionTable {
    pub activate: FbcCallback,
    pub deactivate: FbcCallback,
    pub is_active: FbcCallback,
    pub is_compressing: FbcCallback,
    pub nuke: FbcCallback,
    pub program_cfb: FbcCallback,
    pub set_false_color: Option<FbcCallback>,
}
pub const I8XX_FBC_FUNCS: FbcFunctionTable = FbcFunctionTable {
    activate: FbcCallback::I8xxActivate,
    deactivate: FbcCallback::I8xxDeactivate,
    is_active: FbcCallback::I8xxIsActive,
    is_compressing: FbcCallback::I8xxIsCompressing,
    nuke: FbcCallback::I8xxNuke,
    program_cfb: FbcCallback::I8xxProgramCfb,
    set_false_color: None,
};
pub const I965_FBC_FUNCS: FbcFunctionTable = FbcFunctionTable {
    activate: FbcCallback::I8xxActivate,
    deactivate: FbcCallback::I8xxDeactivate,
    is_active: FbcCallback::I8xxIsActive,
    is_compressing: FbcCallback::I8xxIsCompressing,
    nuke: FbcCallback::I965Nuke,
    program_cfb: FbcCallback::I8xxProgramCfb,
    set_false_color: None,
};
pub const G4X_FBC_FUNCS: FbcFunctionTable = FbcFunctionTable {
    activate: FbcCallback::G4xActivate,
    deactivate: FbcCallback::G4xDeactivate,
    is_active: FbcCallback::G4xIsActive,
    is_compressing: FbcCallback::G4xIsCompressing,
    nuke: FbcCallback::I965Nuke,
    program_cfb: FbcCallback::G4xProgramCfb,
    set_false_color: None,
};
pub const ILK_FBC_FUNCS: FbcFunctionTable = FbcFunctionTable {
    activate: FbcCallback::IlkActivate,
    deactivate: FbcCallback::IlkDeactivate,
    is_active: FbcCallback::IlkIsActive,
    is_compressing: FbcCallback::IlkIsCompressing,
    nuke: FbcCallback::I965Nuke,
    program_cfb: FbcCallback::IlkProgramCfb,
    set_false_color: None,
};
pub const SNB_FBC_FUNCS: FbcFunctionTable = FbcFunctionTable {
    activate: FbcCallback::SnbActivate,
    deactivate: FbcCallback::IlkDeactivate,
    is_active: FbcCallback::IlkIsActive,
    is_compressing: FbcCallback::IlkIsCompressing,
    nuke: FbcCallback::SnbNuke,
    program_cfb: FbcCallback::IlkProgramCfb,
    set_false_color: None,
};
pub const IVB_FBC_FUNCS: FbcFunctionTable = FbcFunctionTable {
    activate: FbcCallback::IvbActivate,
    deactivate: FbcCallback::IlkDeactivate,
    is_active: FbcCallback::IlkIsActive,
    is_compressing: FbcCallback::IvbIsCompressing,
    nuke: FbcCallback::SnbNuke,
    program_cfb: FbcCallback::IlkProgramCfb,
    set_false_color: Some(FbcCallback::IvbSetFalseColor),
};
pub const fn intel_fbc_function_table(version: u8, g4x: bool) -> &'static FbcFunctionTable {
    match fbc_generation(version, g4x) {
        FbcGeneration::I8xx => &I8XX_FBC_FUNCS,
        FbcGeneration::I965 => &I965_FBC_FUNCS,
        FbcGeneration::G4x => &G4X_FBC_FUNCS,
        FbcGeneration::Ilk => &ILK_FBC_FUNCS,
        FbcGeneration::Snb => &SNB_FBC_FUNCS,
        FbcGeneration::Ivb => &IVB_FBC_FUNCS,
    }
}
pub const fn fbc_generation(version: u8, g4x: bool) -> FbcGeneration {
    if version >= 7 {
        FbcGeneration::Ivb
    } else if version == 6 {
        FbcGeneration::Snb
    } else if version == 5 {
        FbcGeneration::Ilk
    } else if g4x {
        FbcGeneration::G4x
    } else if version == 4 {
        FbcGeneration::I965
    } else {
        FbcGeneration::I8xx
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyFbcConfig {
    pub version: u8,
    pub id: FbcId,
    pub i9xx_plane: u8,
    pub plane_binding: u8,
    pub cfb_stride: u32,
    pub limit: u8,
    pub interval: u16,
    pub fence_id: i8,
    pub fence_y_offset: u32,
    pub g4x: bool,
    pub i945gm: bool,
    pub ivybridge: bool,
    pub false_color: bool,
    pub has_fences: bool,
}
fn field(value: u32, shift: u32, mask: u32) -> u32 {
    (value << shift) & mask
}

/// Translated upstream function bodies in source order.
pub mod upstream_source_order {
    use super::*;

    // upstream: intel_fbc.c intel_fbc_for_pipe()
    pub fn intel_fbc_for_pipe(graph: &impl FbcPipeGraph, pipe: u8) -> Option<FbcId> {
        let primary = graph.primary_plane_for_pipe(pipe)?;
        graph.fbc_for_plane(primary)
    }

    // upstream: intel_fbc.c intel_fbc_plane_stride()
    pub fn intel_fbc_plane_stride(p: &PlaneState) -> u32 {
        if p.rotation.swaps_axes() {
            p.mapping_stride
        } else {
            p.mapping_stride / max(p.framebuffer.format.cpp(), 1)
        }
    }

    // upstream: intel_fbc.c intel_fbc_cfb_cpp()
    pub fn intel_fbc_cfb_cpp(p: &PlaneState) -> u32 {
        max(p.framebuffer.format.cpp(), 4)
    }

    // upstream: intel_fbc.c intel_fbc_plane_cfb_stride()
    pub fn intel_fbc_plane_cfb_stride(p: &PlaneState) -> Result<u32, FbcError> {
        intel_fbc_plane_stride(p)
            .checked_mul(intel_fbc_cfb_cpp(p))
            .ok_or(FbcError::ArithmeticOverflow)
    }

    // upstream: intel_fbc.c skl_fbc_min_cfb_stride()
    pub fn skl_fbc_min_cfb_stride(d: &DisplayCaps, cpp: u32, width: u32) -> Result<u32, FbcError> {
        let mut s = width.checked_mul(cpp).ok_or(FbcError::ArithmeticOverflow)?;
        if d.wa_16011863758 {
            s = s.checked_add(64).ok_or(FbcError::ArithmeticOverflow)?;
        }
        align_up(s, 512)
    }

    // upstream: intel_fbc.c _intel_fbc_cfb_stride()
    pub fn _intel_fbc_cfb_stride(
        d: &DisplayCaps,
        cpp: u32,
        width: u32,
        stride: u32,
    ) -> Result<u32, FbcError> {
        if d.version >= 9 {
            Ok(max(
                align_up(stride, 512)?,
                skl_fbc_min_cfb_stride(d, cpp, width)?,
            ))
        } else {
            Ok(stride)
        }
    }

    // upstream: intel_fbc.c intel_fbc_cfb_stride()
    pub fn intel_fbc_cfb_stride(d: &DisplayCaps, p: &PlaneState) -> Result<u32, FbcError> {
        _intel_fbc_cfb_stride(
            d,
            intel_fbc_cfb_cpp(p),
            p.src_width,
            intel_fbc_plane_cfb_stride(p)?,
        )
    }

    // upstream: intel_fbc.c intel_fbc_max_cfb_height()
    pub const fn intel_fbc_max_cfb_height(d: &DisplayCaps) -> u32 {
        if d.version >= 8 {
            2560
        } else if d.version >= 5 || d.g4x {
            2048
        } else {
            1536
        }
    }

    // upstream: intel_fbc.c _intel_fbc_cfb_size()
    pub fn _intel_fbc_cfb_size(d: &DisplayCaps, height: u32, stride: u32) -> Result<u32, FbcError> {
        min(height, intel_fbc_max_cfb_height(d))
            .checked_mul(stride)
            .ok_or(FbcError::ArithmeticOverflow)
    }

    // upstream: intel_fbc.c intel_fbc_cfb_size()
    pub fn intel_fbc_cfb_size(d: &DisplayCaps, p: &PlaneState) -> Result<u32, FbcError> {
        _intel_fbc_cfb_size(d, p.src_height, intel_fbc_cfb_stride(d, p)?)
    }

    // upstream: intel_fbc.c intel_fbc_override_cfb_stride()
    pub fn intel_fbc_override_cfb_stride(d: &DisplayCaps, p: &PlaneState) -> Result<u16, FbcError> {
        let aligned = intel_fbc_cfb_stride(d, p)?;
        if intel_fbc_plane_cfb_stride(p)? != aligned
            || (d.version == 9 && p.framebuffer.modifier == MOD_LINEAR)
            || d.battlemage
        {
            return u16::try_from(aligned.checked_mul(4).ok_or(FbcError::ArithmeticOverflow)? / 64)
                .map_err(|_| FbcError::InvalidLimit);
        }
        Ok(0)
    }

    // upstream: intel_fbc.c intel_fbc_has_fences()
    pub const fn intel_fbc_has_fences(d: &DisplayCaps) -> bool {
        d.has_fenced_regions
    }

    // upstream: intel_fbc.c i8xx_fbc_ctl()
    pub fn i8xx_fbc_ctl(c: &LegacyFbcConfig) -> Result<u32, FbcError> {
        if c.limit == 0 {
            return Err(FbcError::InvalidLimit);
        }
        let mut stride = c.cfb_stride / u32::from(c.limit);
        if c.version == 2 {
            stride = (stride / 32).wrapping_sub(1)
        } else {
            stride = (stride / 64).wrapping_sub(1)
        }
        let mut ctl = FBC_CTL_PERIODIC
            | field(u32::from(c.interval), 16, FBC_CTL_INTERVAL_MASK)
            | field(stride, 5, FBC_CTL_STRIDE_MASK);
        if c.i945gm {
            ctl |= FBC_CTL_C3_IDLE
        }
        if c.fence_id >= 0 {
            ctl |= field(c.fence_id as u32, 0, FBC_CTL_FENCENO_MASK)
        }
        Ok(ctl)
    }

    // upstream: intel_fbc.c i965_fbc_ctl2()
    pub fn i965_fbc_ctl2(c: &LegacyFbcConfig) -> u32 {
        let mut ctl = FBC_CTL_FENCE_DBL | field(u32::from(c.i9xx_plane), 0, 0x3);
        if c.fence_id >= 0 {
            ctl |= FBC_CTL_CPU_FENCE_EN
        }
        ctl
    }

    // upstream: intel_fbc.c i8xx_fbc_deactivate()
    pub fn i8xx_fbc_deactivate(io: &mut impl FbcRegisterIo) -> Result<(), FbcError> {
        let mut ctl = io.read(FbcRegister::LegacyControl)?;
        if ctl & FBC_CTL_EN == 0 {
            return Ok(());
        }
        ctl &= !FBC_CTL_EN;
        io.write(FbcRegister::LegacyControl, ctl)?;
        let _timed_out = !io.wait_for_clear_ms(FbcRegister::LegacyStatus, 1 << 31, 10)?;
        Ok(())
    }

    // upstream: intel_fbc.c i8xx_fbc_activate()
    pub fn i8xx_fbc_activate(
        io: &mut impl FbcRegisterIo,
        c: &LegacyFbcConfig,
    ) -> Result<(), FbcError> {
        for i in 0..((1536 / 32) + 1) {
            io.write(FbcRegister::LegacyTag(i as u16), 0)?
        }
        if c.version == 4 {
            io.write(FbcRegister::LegacyControl2, i965_fbc_ctl2(c))?;
            io.write(FbcRegister::LegacyFenceOff, c.fence_y_offset)?;
        }
        io.write(FbcRegister::LegacyControl, FBC_CTL_EN | i8xx_fbc_ctl(c)?)?;
        Ok(())
    }

    // upstream: intel_fbc.c i8xx_fbc_is_active()
    pub fn i8xx_fbc_is_active(io: &mut impl FbcRegisterIo) -> Result<bool, FbcError> {
        Ok(io.read(FbcRegister::LegacyControl)? & FBC_CTL_EN != 0)
    }

    // upstream: intel_fbc.c i8xx_fbc_is_compressing()
    pub fn i8xx_fbc_is_compressing(io: &mut impl FbcRegisterIo) -> Result<bool, FbcError> {
        Ok(io.read(FbcRegister::LegacyStatus)? & ((1 << 31) | (1 << 30)) != 0)
    }

    // upstream: intel_fbc.c i8xx_fbc_nuke()
    pub fn i8xx_fbc_nuke(io: &mut impl FbcRegisterIo, plane: u8) -> Result<(), FbcError> {
        let addr = io.read(FbcRegister::PlaneAddr(plane))?;
        io.write(FbcRegister::PlaneAddr(plane), addr)
    }

    // upstream: intel_fbc.c i8xx_fbc_program_cfb()
    pub fn i8xx_fbc_program_cfb(
        io: &mut impl FbcRegisterIo,
        fb: u32,
        llb: u32,
    ) -> Result<(), FbcError> {
        io.write(FbcRegister::CfbBase(FbcId::A), fb)?;
        io.write(FbcRegister::LegacyLlBase, llb)
    }

    // upstream: intel_fbc.c i965_fbc_nuke()
    pub fn i965_fbc_nuke(io: &mut impl FbcRegisterIo, plane: u8) -> Result<(), FbcError> {
        let addr = io.read(FbcRegister::PlaneSurface(plane))?;
        io.write(FbcRegister::PlaneSurface(plane), addr)
    }

    // upstream: intel_fbc.c g4x_dpfc_ctl_limit()
    pub fn g4x_dpfc_ctl_limit(limit: u8) -> Result<u32, FbcError> {
        dpfc_limit(limit)
    }

    // upstream: intel_fbc.c g4x_dpfc_ctl()
    pub fn g4x_dpfc_ctl(c: &LegacyFbcConfig) -> Result<u32, FbcError> {
        let mut ctl = g4x_dpfc_ctl_limit(c.limit)?
            | field(u32::from(c.i9xx_plane), 30, DPFC_CTL_PLANE_MASK_G4X);
        if c.g4x {
            ctl |= DPFC_CTL_SR_EN
        }
        if c.fence_id >= 0 {
            ctl |= DPFC_CTL_FENCE_EN_G4X;
            if c.version < 6 {
                ctl |= field(c.fence_id as u32, 0, DPFC_CTL_FENCENO_MASK)
            }
        }
        Ok(ctl)
    }

    // upstream: intel_fbc.c g4x_fbc_activate()
    pub fn g4x_fbc_activate(
        io: &mut impl FbcRegisterIo,
        c: &LegacyFbcConfig,
    ) -> Result<(), FbcError> {
        io.write(FbcRegister::GlobalDpfcFenceYOffset, c.fence_y_offset)?;
        io.write(
            FbcRegister::GlobalDpfcControl,
            DPFC_CTL_EN | g4x_dpfc_ctl(c)?,
        )?;
        Ok(())
    }

    // upstream: intel_fbc.c g4x_fbc_deactivate()
    pub fn g4x_fbc_deactivate(io: &mut impl FbcRegisterIo) -> Result<(), FbcError> {
        let ctl = io.read(FbcRegister::GlobalDpfcControl)?;
        if ctl & DPFC_CTL_EN != 0 {
            io.write(FbcRegister::GlobalDpfcControl, ctl & !DPFC_CTL_EN)?
        }
        Ok(())
    }

    // upstream: intel_fbc.c g4x_fbc_is_active()
    pub fn g4x_fbc_is_active(io: &mut impl FbcRegisterIo) -> Result<bool, FbcError> {
        Ok(io.read(FbcRegister::GlobalDpfcControl)? & DPFC_CTL_EN != 0)
    }

    // upstream: intel_fbc.c g4x_fbc_is_compressing()
    pub fn g4x_fbc_is_compressing(io: &mut impl FbcRegisterIo) -> Result<bool, FbcError> {
        Ok(io.read(FbcRegister::GlobalDpfcStatus)? & DPFC_COMP_SEG_MASK != 0)
    }

    // upstream: intel_fbc.c g4x_fbc_program_cfb()
    pub fn g4x_fbc_program_cfb(io: &mut impl FbcRegisterIo, offset: u32) -> Result<(), FbcError> {
        io.write(FbcRegister::GlobalDpfcCbBase, offset)
    }

    // upstream: intel_fbc.c ilk_fbc_activate()
    pub fn ilk_fbc_activate(
        io: &mut impl FbcRegisterIo,
        c: &LegacyFbcConfig,
    ) -> Result<(), FbcError> {
        io.write(FbcRegister::FenceYOffset(c.id), c.fence_y_offset)?;
        io.write(FbcRegister::Control(c.id), DPFC_CTL_EN | g4x_dpfc_ctl(c)?)?;
        Ok(())
    }

    // upstream: intel_fbc.c fbc_compressor_clkgate_disable_wa()
    pub fn fbc_compressor_clkgate_disable_wa(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
        display_version: u8,
        is_dg2: bool,
        disable: bool,
    ) -> Result<(), FbcError> {
        if is_dg2 {
            io.rmw(
                FbcRegister::Dg2ClockGateDis,
                1 << 31,
                if disable { 1 << 31 } else { 0 },
            )?
        } else if display_version >= 14 {
            io.rmw(
                FbcRegister::MtlPipeClockGateDis(id),
                1 << 6,
                if disable { 1 << 6 } else { 0 },
            )?
        }
        Ok(())
    }

    // upstream: intel_fbc.c ilk_fbc_deactivate()
    pub fn ilk_fbc_deactivate(
        io: &mut impl FbcRegisterIo,
        c: &HardwareConfig,
    ) -> Result<(), FbcError> {
        if c.has_dirty_rect {
            io.write(FbcRegister::DirtyControl(c.id), 0)?
        }
        let ctl = io.read(FbcRegister::Control(c.id))?;
        if ctl & DPFC_CTL_EN != 0 {
            io.write(FbcRegister::Control(c.id), ctl & !DPFC_CTL_EN)?
        }
        Ok(())
    }

    // upstream: intel_fbc.c ilk_fbc_is_active()
    pub fn ilk_fbc_is_active(io: &mut impl FbcRegisterIo, id: FbcId) -> Result<bool, FbcError> {
        Ok(io.read(FbcRegister::Control(id))? & DPFC_CTL_EN != 0)
    }

    // upstream: intel_fbc.c ilk_fbc_is_compressing()
    pub fn ilk_fbc_is_compressing(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
    ) -> Result<bool, FbcError> {
        Ok(io.read(FbcRegister::Status(id))? & DPFC_COMP_SEG_MASK != 0)
    }

    // upstream: intel_fbc.c ilk_fbc_program_cfb()
    pub fn ilk_fbc_program_cfb(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
        offset: u32,
    ) -> Result<(), FbcError> {
        io.write(FbcRegister::CfbBase(id), offset)
    }

    // upstream: intel_fbc.c snb_fbc_program_fence()
    pub fn snb_fbc_program_fence(
        io: &mut impl FbcRegisterIo,
        c: &LegacyFbcConfig,
    ) -> Result<(), FbcError> {
        let ctl = if c.fence_id >= 0 {
            SNB_DPFC_FENCE_EN | (c.fence_id as u32 & 0x1f)
        } else {
            0
        };
        io.write(FbcRegister::SnbFenceControl, ctl)?;
        io.write(FbcRegister::SnbFenceOffset, c.fence_y_offset)
    }

    // upstream: intel_fbc.c snb_fbc_activate()
    pub fn snb_fbc_activate(
        io: &mut impl FbcRegisterIo,
        c: &LegacyFbcConfig,
    ) -> Result<(), FbcError> {
        snb_fbc_program_fence(io, c)?;
        ilk_fbc_activate(io, c)
    }

    // upstream: intel_fbc.c snb_fbc_nuke()
    pub fn snb_fbc_nuke(io: &mut impl FbcRegisterIo, id: FbcId) -> Result<(), FbcError> {
        nuke(io, id)
    }

    // upstream: intel_fbc.c glk_fbc_program_cfb_stride()
    pub fn glk_fbc_program_cfb_stride(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
        override_stride: u16,
        limit: u8,
    ) -> Result<(), FbcError> {
        let value = if override_stride != 0 {
            if limit == 0 {
                return Err(FbcError::InvalidLimit);
            }
            FBC_STRIDE_OVERRIDE | (u32::from(override_stride) / u32::from(limit) & FBC_STRIDE_MASK)
        } else {
            0
        };
        io.write(FbcRegister::GlkStride(id), value)
    }

    // upstream: intel_fbc.c skl_fbc_program_cfb_stride()
    pub fn skl_fbc_program_cfb_stride(
        io: &mut impl FbcRegisterIo,
        override_stride: u16,
        limit: u8,
    ) -> Result<(), FbcError> {
        let value = if override_stride != 0 {
            if limit == 0 {
                return Err(FbcError::InvalidLimit);
            }
            FBC_STRIDE_OVERRIDE | (u32::from(override_stride) / u32::from(limit) & FBC_STRIDE_MASK)
        } else {
            0
        };
        io.rmw(
            FbcRegister::SkLStrideChicken,
            FBC_STRIDE_OVERRIDE | FBC_STRIDE_MASK,
            value,
        )
    }

    // upstream: intel_fbc.c ivb_dpfc_ctl()
    pub fn ivb_dpfc_ctl(c: &HardwareConfig) -> Result<u32, FbcError> {
        let mut ctl = dpfc_limit(c.limit)?;
        if c.ivybridge {
            ctl |= (u32::from(c.i9xx_plane) << 29) & DPFC_CTL_PLANE_MASK_IVB
        }
        if c.display_version >= 20 {
            ctl |= (u32::from(c.plane_id) << 11) & DPFC_CTL_PLANE_BINDING_MASK
        }
        if c.fence_id >= 0 {
            ctl |= DPFC_CTL_FENCE_EN_IVB
        }
        if c.false_color {
            ctl |= DPFC_CTL_FALSE_COLOR
        }
        Ok(ctl)
    }

    // upstream: intel_fbc.c ivb_fbc_activate()
    pub fn activate_hardware(
        io: &mut impl FbcRegisterIo,
        c: &HardwareConfig,
    ) -> Result<(), FbcError> {
        if c.display_version >= 10 {
            io.write(FbcRegister::GlkStride(c.id), glk_stride_value(c)?)?
        } else if c.display_version == 9 {
            skl_fbc_program_cfb_stride(io, c.override_cfb_stride, c.limit)?
        }
        if c.has_fences {
            let fence = if c.fence_id >= 0 {
                SNB_DPFC_FENCE_EN | (c.fence_id as u32 & SNB_DPFC_FENCENO_MASK)
            } else {
                0
            };
            io.write(FbcRegister::SnbFenceControl, fence)?;
            io.write(FbcRegister::SnbFenceOffset, c.fence_y_offset)?;
        }
        io.write(FbcRegister::FenceYOffset(c.id), c.fence_y_offset)?;
        let ctl = ivb_dpfc_ctl(c)?;
        if c.display_version >= 20 {
            io.write(FbcRegister::Control(c.id), ctl)?
        }
        if c.has_dirty_rect {
            io.write(FbcRegister::DirtyControl(c.id), FBC_DIRTY_RECT_EN)?
        }
        io.write(FbcRegister::Control(c.id), DPFC_CTL_EN | ctl)?;
        Ok(())
    }

    // upstream: intel_fbc.c ivb_fbc_is_compressing()
    pub fn hardware_is_compressing(
        io: &mut impl FbcRegisterIo,
        c: &HardwareConfig,
    ) -> Result<bool, FbcError> {
        let reg = if c.display_version >= 7 {
            FbcRegister::Status2(c.id)
        } else {
            FbcRegister::Status(c.id)
        };
        let mask = if c.display_version >= 7 { 0xfff } else { 0x7ff };
        Ok(io.read(reg)? & mask != 0)
    }

    // upstream: intel_fbc.c ivb_fbc_set_false_color()
    pub fn set_false_color(
        io: &mut impl FbcRegisterIo,
        c: &HardwareConfig,
        enable: bool,
    ) -> Result<(), FbcError> {
        io.rmw(
            FbcRegister::Control(c.id),
            DPFC_CTL_FALSE_COLOR,
            if enable { DPFC_CTL_FALSE_COLOR } else { 0 },
        )
    }

    // upstream: intel_fbc.c intel_fbc_hw_is_active()
    pub fn intel_fbc_hw_is_active(
        h: &mut impl FbcControlHooks,
        r: &FbcRuntime,
    ) -> Result<bool, FbcError> {
        h.is_hardware_active(r)
    }

    // upstream: intel_fbc.c intel_fbc_hw_activate()
    pub fn intel_fbc_hw_activate(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
    ) -> Result<(), FbcError> {
        r.active = true;
        r.activated = true;
        h.hardware_activate(r)
    }

    // upstream: intel_fbc.c intel_fbc_hw_deactivate()
    pub fn intel_fbc_hw_deactivate(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
    ) -> Result<(), FbcError> {
        r.active = false;
        h.hardware_deactivate(r)
    }

    // upstream: intel_fbc.c intel_fbc_is_compressing()
    pub fn intel_fbc_is_compressing(
        h: &mut impl FbcControlHooks,
        r: &FbcRuntime,
    ) -> Result<bool, FbcError> {
        h.is_compressing(r)
    }

    // upstream: intel_fbc.c intel_fbc_nuke()
    pub fn intel_fbc_nuke(h: &mut impl FbcControlHooks, r: &FbcRuntime) -> Result<(), FbcError> {
        let _warn_if_flip_pending = r.flip_pending;
        h.nuke(r)
    }

    // upstream: intel_fbc.c intel_fbc_activate()
    pub fn intel_fbc_activate(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        has_fences: bool,
        has_dirty_rect: bool,
    ) -> Result<(), FbcError> {
        if r.active && !has_fences {
            return Ok(());
        }
        let _warn_if_active_dirty_rect = r.active && has_dirty_rect;
        intel_fbc_hw_activate(h, r)?;
        intel_fbc_nuke(h, r)?;
        r.no_fbc_reason = None;
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_deactivate()
    pub fn intel_fbc_deactivate(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        reason: NoFbcReason,
    ) -> Result<(), FbcError> {
        if r.active {
            intel_fbc_hw_deactivate(h, r)?
        }
        r.no_fbc_reason = Some(reason);
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_cfb_base_max()
    pub fn intel_fbc_cfb_base_max(d: &DisplayCaps) -> u64 {
        if d.version >= 5 || d.g4x {
            1u64 << 28
        } else {
            1u64 << 32
        }
    }

    // upstream: intel_fbc.c intel_fbc_stolen_end()
    pub fn intel_fbc_stolen_end(d: &DisplayCaps, stolen_area_size: u64) -> u64 {
        let base = intel_fbc_cfb_base_max(d);
        if d.broadwell || (d.version == 9 && !d.broxton) {
            stolen_area_size
                .checked_sub(8 * 1024 * 1024)
                .map_or(base, |size| min(base, size))
        } else {
            base
        }
    }

    // upstream: intel_fbc.c intel_fbc_min_limit()
    pub const fn intel_fbc_min_limit(p: &PlaneState) -> u8 {
        if p.framebuffer.format.cpp() == 2 {
            2
        } else {
            1
        }
    }

    // upstream: intel_fbc.c intel_fbc_max_limit()
    pub const fn intel_fbc_max_limit(d: &DisplayCaps) -> u8 {
        if d.g4x { 1 } else { 4 }
    }

    // upstream: intel_fbc.c find_compression_limit()
    pub fn find_compression_limit(
        size: u32,
        min_limit: u8,
        max_limit: u8,
        end: u64,
        mut insert: impl FnMut(u32, u32, u64) -> bool,
    ) -> Result<u8, FbcError> {
        if !matches!(min_limit, 1 | 2 | 4) || max_limit < min_limit || max_limit > 4 {
            return Err(FbcError::InvalidLimit);
        }
        let mut limit = min_limit;
        let mut candidate = (size / u32::from(limit))
            .checked_mul(2)
            .ok_or(FbcError::ArithmeticOverflow)?;
        if insert(candidate, 4096, end) {
            return Ok(limit);
        }
        candidate /= 2;
        while limit <= max_limit {
            if insert(candidate, 4096, end) {
                return Ok(limit);
            }
            limit = limit.checked_mul(2).ok_or(FbcError::InvalidLimit)?;
            candidate /= 2;
        }
        Ok(0)
    }

    // upstream: intel_fbc.c intel_fbc_alloc_cfb()
    pub fn intel_fbc_alloc_cfb(
        d: &DisplayCaps,
        size: u32,
        min_limit: u8,
        end: u64,
        a: &mut impl StolenAllocator,
    ) -> Result<CfbAllocation, FbcError> {
        let needs_llb = d.version < 5 && !d.g4x;
        if needs_llb && !a.insert(true, 4096, 4096, end) {
            return Err(FbcError::NoStolenMemory);
        }
        let mut reserved_bytes = 0;
        let limit = find_compression_limit(
            size,
            min_limit,
            intel_fbc_max_limit(d),
            end,
            |bytes, align, end| {
                let inserted = a.insert(false, bytes, align, end);
                if inserted {
                    reserved_bytes = bytes
                }
                inserted
            },
        )?;
        if limit == 0 {
            if needs_llb {
                a.remove(true)
            }
            return Err(FbcError::NoStolenMemory);
        }
        Ok(CfbAllocation {
            limit,
            framebuffer_bytes: reserved_bytes,
            line_buffer_bytes: if needs_llb { 4096 } else { 0 },
        })
    }

    // upstream: intel_fbc.c intel_fbc_program_cfb()
    pub fn intel_fbc_program_cfb(
        h: &mut impl FbcControlHooks,
        id: FbcId,
        r: &FbcRuntime,
    ) -> Result<(), FbcError> {
        h.program_cfb(id, r)
    }

    // upstream: intel_fbc.c intel_fbc_program_workarounds()
    pub fn program_workarounds(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
        d: &DisplayCaps,
    ) -> Result<(), FbcError> {
        let mut set = 0;
        if d.skylake || d.broxton {
            set |= 1 << 8
        }
        if d.skylake || d.kabylake || d.coffeelake || d.cometlake {
            set |= 1 << 23
        }
        if d.wa_1409120013 {
            set |= 1 << 14
        }
        if d.wa_22014263786 {
            set |= 1 << 13
        }
        if set != 0 {
            io.rmw(FbcRegister::Chicken(id), 0, set)?
        }
        if d.dg2 || d.version >= 14 {
            fbc_compressor_clkgate_disable_wa(io, id, d.version, d.dg2, true)?
        }
        Ok(())
    }

    // upstream: intel_fbc.c __intel_fbc_cleanup_cfb()
    pub fn __intel_fbc_cleanup_cfb(
        r: &mut FbcRuntime,
        id: FbcId,
        a: &mut impl StolenAllocator,
    ) -> bool {
        if r.active {
            return false;
        }
        if r.llb_allocated {
            a.remove(true);
            r.llb_allocated = false
        }
        if r.cfb_allocated {
            a.remove(false);
            r.cfb_allocated = false
        }
        let _ = id;
        true
    }

    // upstream: intel_fbc.c intel_fbc_cleanup()
    pub fn intel_fbc_cleanup(
        display: &mut FbcDisplay,
        ids: &[FbcId],
        a: &mut impl StolenAllocator,
        h: &mut impl FbcControlHooks,
    ) {
        for (index, slot) in display.instances.iter_mut().enumerate() {
            if let Some(runtime) = slot.as_mut() {
                let id = ids.get(index).copied().unwrap_or(runtime.id);
                h.lock_instance(id);
                let _ = __intel_fbc_cleanup_cfb(runtime, id, a);
                h.unlock_instance(id);
                a.free_instance(id);
                h.free_instance_container(id);
                *slot = None;
            }
        }
        h.lock_sys_cache();
        let _warn_if_cache_still_assigned = display.sys_cache_id != 2;
        h.unlock_sys_cache();
    }

    // upstream: intel_fbc.c i8xx_fbc_stride_is_valid()
    pub fn i8xx_fbc_stride_is_valid(p: &PlaneState) -> bool {
        let stride = intel_fbc_plane_stride(p).wrapping_mul(p.framebuffer.format.cpp());
        stride == 4096 || stride == 8192
    }

    // upstream: intel_fbc.c i965_fbc_stride_is_valid()
    pub fn i965_fbc_stride_is_valid(p: &PlaneState) -> bool {
        let stride = intel_fbc_plane_stride(p).wrapping_mul(p.framebuffer.format.cpp());
        stride >= 2048 && stride <= 16384
    }

    // upstream: intel_fbc.c g4x_fbc_stride_is_valid()
    pub fn g4x_fbc_stride_is_valid(_p: &PlaneState) -> bool {
        true
    }

    // upstream: intel_fbc.c skl_fbc_stride_is_valid()
    pub fn skl_fbc_stride_is_valid(p: &PlaneState) -> bool {
        let stride = intel_fbc_plane_stride(p).wrapping_mul(p.framebuffer.format.cpp());
        p.framebuffer.modifier != MOD_LINEAR || (stride & 511) == 0
    }

    // upstream: intel_fbc.c icl_fbc_stride_is_valid()
    pub fn icl_fbc_stride_is_valid(_p: &PlaneState) -> bool {
        true
    }

    // upstream: intel_fbc.c stride_is_valid()
    pub fn plane_stride_is_valid(d: &DisplayCaps, p: &PlaneState) -> bool {
        if d.version >= 11 {
            icl_fbc_stride_is_valid(p)
        } else if d.version >= 9 {
            skl_fbc_stride_is_valid(p)
        } else if d.version >= 5 || d.g4x {
            g4x_fbc_stride_is_valid(p)
        } else if d.version == 4 {
            i965_fbc_stride_is_valid(p)
        } else {
            i8xx_fbc_stride_is_valid(p)
        }
    }

    // upstream: intel_fbc.c i8xx_fbc_pixel_format_is_valid()
    pub const fn i8xx_fbc_pixel_format_is_valid(d: &DisplayCaps, p: &PlaneState) -> bool {
        matches!(
            p.framebuffer.format,
            PixelFormat::Xrgb8888 | PixelFormat::Xbgr8888
        ) || (d.version != 2
            && matches!(
                p.framebuffer.format,
                PixelFormat::Xrgb1555 | PixelFormat::Rgb565
            ))
    }

    // upstream: intel_fbc.c g4x_fbc_pixel_format_is_valid()
    pub const fn g4x_fbc_pixel_format_is_valid(d: &DisplayCaps, p: &PlaneState) -> bool {
        matches!(
            p.framebuffer.format,
            PixelFormat::Xrgb8888 | PixelFormat::Xbgr8888
        ) || (matches!(p.framebuffer.format, PixelFormat::Rgb565) && !d.g4x)
    }

    // upstream: intel_fbc.c lnl_fbc_pixel_format_is_valid()
    pub const fn lnl_fbc_pixel_format_is_valid(p: &PlaneState) -> bool {
        matches!(
            p.framebuffer.format,
            PixelFormat::Xrgb8888
                | PixelFormat::Xbgr8888
                | PixelFormat::Argb8888
                | PixelFormat::Abgr8888
                | PixelFormat::Rgb565
        )
    }

    // upstream: intel_fbc.c xe3p_lpd_fbc_fp16_format_is_valid()
    pub const fn xe3p_lpd_fbc_fp16_format_is_valid(p: &PlaneState) -> bool {
        matches!(
            p.framebuffer.format,
            PixelFormat::Argb16161616F | PixelFormat::Abgr16161616F
        )
    }

    // upstream: intel_fbc.c xe3p_lpd_fbc_pixel_format_is_valid()
    pub const fn xe3p_lpd_fbc_pixel_format_is_valid(p: &PlaneState) -> bool {
        lnl_fbc_pixel_format_is_valid(p)
            || xe3p_lpd_fbc_fp16_format_is_valid(p)
            || matches!(
                p.framebuffer.format,
                PixelFormat::Xrgb16161616
                    | PixelFormat::Xbgr16161616
                    | PixelFormat::Argb16161616
                    | PixelFormat::Abgr16161616
            )
    }

    // upstream: intel_fbc.c intel_fbc_need_pixel_normalizer()
    pub const fn intel_fbc_need_pixel_normalizer(d: &DisplayCaps, p: &PlaneState) -> bool {
        d.pixel_normalizer && xe3p_lpd_fbc_fp16_format_is_valid(p)
    }

    // upstream: intel_fbc.c pixel_format_is_valid()
    pub const fn intel_fbc_pixel_format_is_valid(d: &DisplayCaps, p: &PlaneState) -> bool {
        if d.version >= 35 {
            xe3p_lpd_fbc_pixel_format_is_valid(p)
        } else if d.version >= 20 {
            lnl_fbc_pixel_format_is_valid(p)
        } else if d.version >= 5 || d.g4x {
            g4x_fbc_pixel_format_is_valid(d, p)
        } else {
            i8xx_fbc_pixel_format_is_valid(d, p)
        }
    }

    // upstream: intel_fbc.c i8xx_fbc_rotation_is_valid()
    pub const fn i8xx_fbc_rotation_is_valid(p: &PlaneState) -> bool {
        matches!(p.rotation, Rotation::Rotate0)
    }

    // upstream: intel_fbc.c g4x_fbc_rotation_is_valid()
    pub const fn g4x_fbc_rotation_is_valid(_p: &PlaneState) -> bool {
        true
    }

    // upstream: intel_fbc.c skl_fbc_rotation_is_valid()
    pub const fn skl_fbc_rotation_is_valid(_p: &PlaneState) -> bool {
        true
    }

    // upstream: intel_fbc.c rotation_is_valid()
    pub const fn intel_fbc_rotation_is_valid(d: &DisplayCaps, p: &PlaneState) -> bool {
        if d.version >= 9 {
            skl_fbc_rotation_is_valid(p)
        } else if d.version >= 5 || d.g4x {
            g4x_fbc_rotation_is_valid(p)
        } else {
            i8xx_fbc_rotation_is_valid(p)
        }
    }

    // upstream: intel_fbc.c intel_fbc_max_surface_size()
    pub fn intel_fbc_max_surface_size(d: &DisplayCaps) -> (u32, u32) {
        if d.version >= 11 {
            (8192, 4096)
        } else if d.version >= 10 {
            (5120, 4096)
        } else if d.version >= 7 {
            (4096, 4096)
        } else if d.version >= 5 || d.g4x {
            (4096, 2048)
        } else {
            (2048, 1536)
        }
    }

    // upstream: intel_fbc.c intel_fbc_surface_size_ok()
    pub fn intel_fbc_surface_size_ok(d: &DisplayCaps, p: &PlaneState) -> bool {
        let (mw, mh) = intel_fbc_max_surface_size(d);
        p.view_x.saturating_add(p.src_width) <= mw && p.view_y.saturating_add(p.src_height) <= mh
    }

    // upstream: intel_fbc.c intel_fbc_max_plane_size()
    pub fn intel_fbc_max_plane_size(d: &DisplayCaps) -> (u32, u32) {
        if d.version >= 10 {
            (5120, 4096)
        } else if d.version >= 8 || d.haswell {
            (4096, 4096)
        } else if d.version >= 5 || d.g4x {
            (4096, 2048)
        } else {
            (2048, 1536)
        }
    }

    // upstream: intel_fbc.c intel_fbc_plane_size_valid()
    pub fn intel_fbc_plane_size_valid(d: &DisplayCaps, p: &PlaneState) -> bool {
        let (mw, mh) = intel_fbc_max_plane_size(d);
        p.src_width <= mw && p.src_height <= mh
    }

    // upstream: intel_fbc.c i8xx_fbc_tiling_valid()
    pub const fn i8xx_fbc_tiling_valid(p: &PlaneState) -> bool {
        p.framebuffer.modifier == MOD_X_TILED
    }

    // upstream: intel_fbc.c skl_fbc_tiling_valid()
    pub const fn skl_fbc_tiling_valid(_p: &PlaneState) -> bool {
        true
    }

    // upstream: intel_fbc.c tiling_is_valid()
    pub const fn intel_fbc_tiling_is_valid(d: &DisplayCaps, p: &PlaneState) -> bool {
        if d.version >= 9 {
            skl_fbc_tiling_valid(p)
        } else {
            i8xx_fbc_tiling_valid(p)
        }
    }

    // upstream: intel_fbc.c intel_fbc_invalidate_dirty_rect()
    pub fn intel_fbc_invalidate_dirty_rect(state: &mut FbcPlaneConfig) {
        state.dirty_rect = Rect::empty()
    }

    // upstream: intel_fbc.c intel_fbc_program_dirty_rect()
    pub fn intel_fbc_program_dirty_rect(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
        rect: Rect,
    ) -> Result<(), FbcError> {
        if rect.visible() {
            io.write(FbcRegister::DirtyRect(id), dirty_rect_value(rect)?)?
        }
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_dirty_rect_update()
    pub fn intel_fbc_dirty_rect_update(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
        state: &FbcPlaneConfig,
    ) -> Result<(), FbcError> {
        if !state.dirty_rect.visible() {
            return Ok(());
        }
        intel_fbc_program_dirty_rect(io, id, state.dirty_rect)
    }

    // upstream: intel_fbc.c intel_fbc_dirty_rect_update_noarm()
    pub fn intel_fbc_dirty_rect_update_noarm(
        h: &mut impl FbcControlHooks,
        id: FbcId,
        display_has_dirty_rect: bool,
        runtime: &FbcRuntime,
        plane: &PlaneState,
    ) -> Result<(), FbcError> {
        if !display_has_dirty_rect {
            return Ok(());
        }
        h.lock_instance(id);
        let result = if let Some(state) = runtime.plane {
            if state.plane_id == plane.plane_id && state.dirty_rect.visible() {
                h.program_dirty_rect(id, state.dirty_rect)
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        h.unlock_instance(id);
        result
    }

    // upstream: intel_fbc.c intel_fbc_hw_intialize_dirty_rect()
    pub fn intel_fbc_hw_intialize_dirty_rect(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
        p: &PlaneState,
    ) -> Result<(), FbcError> {
        intel_fbc_program_dirty_rect(io, id, initial_dirty_rect(p))
    }

    // upstream: intel_fbc.c intel_fbc_update_state()
    pub fn intel_fbc_update_state(
        r: &mut FbcRuntime,
        d: &DisplayCaps,
        p: &PlaneState,
        refresh_hz: u16,
    ) -> Result<(), FbcError> {
        let dirty = r.plane.map_or(Rect::empty(), |state| state.dirty_rect);
        let mut state = FbcPlaneConfig::from_plane(d, p, refresh_hz)?;
        state.dirty_rect = dirty;
        r.plane = Some(state);
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_is_fence_ok()
    pub fn intel_fbc_is_fence_ok(d: &DisplayCaps, p: &PlaneState) -> bool {
        d.version >= 9 || p.fence_id >= 0
    }

    // upstream: intel_fbc.c intel_fbc_is_cfb_ok()
    pub fn intel_fbc_is_cfb_ok(
        d: &DisplayCaps,
        p: &PlaneState,
        limit: u8,
        allocation: u32,
    ) -> Result<bool, FbcError> {
        if intel_fbc_min_limit(p) > limit {
            return Ok(false);
        }
        let allowed = u64::from(limit)
            .checked_mul(u64::from(allocation))
            .ok_or(FbcError::ArithmeticOverflow)?;
        Ok(u64::from(intel_fbc_cfb_size(d, p)?) <= allowed)
    }

    // upstream: intel_fbc.c intel_fbc_is_ok()
    pub fn intel_fbc_is_ok(
        d: &DisplayCaps,
        p: &PlaneState,
        reason: Option<NoFbcReason>,
        limit: u8,
        allocation: u32,
    ) -> Result<bool, FbcError> {
        Ok(reason.is_none()
            && intel_fbc_is_fence_ok(d, p)
            && intel_fbc_is_cfb_ok(d, p, limit, allocation)?)
    }

    // upstream: intel_fbc.c __intel_fbc_prepare_dirty_rect()
    pub fn __intel_fbc_prepare_dirty_rect(
        d: &DisplayCaps,
        p: &PlaneState,
        reason: Option<NoFbcReason>,
        limit: u8,
        allocation: u32,
    ) -> Result<Rect, FbcError> {
        if p.needs_modeset || !intel_fbc_is_ok(d, p, reason, limit, allocation)? {
            return Ok(Rect::empty());
        }
        if p.damage.visible() {
            return Ok(p.damage);
        }
        Ok(Rect {
            x1: 0,
            y1: p.view_y as i32,
            x2: p.src_width as i32,
            y2: p.view_y as i32 + 1,
        })
    }

    // upstream: intel_fbc.c intel_fbc_prepare_dirty_rect()
    pub fn intel_fbc_prepare_dirty_rect(
        h: &mut impl FbcControlHooks,
        d: &DisplayCaps,
        state: &FbcAtomicState,
        r: &mut FbcRuntime,
        limit: u8,
        allocation: u32,
    ) -> Result<(), FbcError> {
        if !d.has_dirty_rect {
            return Ok(());
        }
        for change in &state.new_planes {
            if !change.has_fbc || change.fbc_id != Some(r.id) || change.pipe != state.crtc_pipe {
                continue;
            }
            h.lock_instance(r.id);
            let result = (|| {
                if let Some(current) = r.plane {
                    if current.plane_id == change.new_state.plane_id {
                        let dirty = __intel_fbc_prepare_dirty_rect(
                            d,
                            &change.new_state,
                            change.no_fbc_reason,
                            limit,
                            allocation,
                        )?;
                        if let Some(current) = r.plane.as_mut() {
                            current.dirty_rect = dirty
                        }
                    }
                }
                Ok(())
            })();
            h.unlock_instance(r.id);
            result?;
        }
        Ok(())
    }

    // upstream: intel_fbc.c _intel_fbc_min_cdclk()
    pub fn _intel_fbc_min_cdclk(c: &DisplayCaps, pixel_rate: u32) -> u32 {
        if c.haswell || c.broadwell {
            ((u64::from(pixel_rate) * 100 + 94) / 95).min(u64::from(u32::MAX)) as u32
        } else {
            0
        }
    }

    // upstream: intel_fbc.c intel_fbc_check_plane()
    pub fn intel_fbc_check_plane(
        d: &DisplayState,
        p: &PlaneState,
        pixel_rate: u32,
    ) -> Result<(), NoFbcReason> {
        let c = &d.caps;
        if !d.stolen_initialized {
            return Err(NoFbcReason::StolenMemoryNotInitialized);
        }
        if d.vgpu_active {
            return Err(NoFbcReason::VgpuActive);
        }
        if d.enable_fbc_param == 0 {
            return Err(NoFbcReason::Disabled);
        }
        if !p.visible {
            return Err(NoFbcReason::PlaneNotVisible);
        }
        if c.wa_16023588340 {
            return Err(NoFbcReason::Wa16023588340);
        }
        if c.wa_15018326506 {
            return Err(NoFbcReason::Wa15018326506);
        }
        if c.vtd_active && (c.skylake || c.broxton) {
            return Err(NoFbcReason::VtdEnabled);
        }
        if p.interlaced {
            return Err(NoFbcReason::InterlacedMode);
        }
        if p.double_wide {
            return Err(NoFbcReason::DoubleWidePipe);
        }
        if c.version >= 12 && p.has_sel_update {
            return Err(NoFbcReason::SelectiveUpdateEnabled);
        }
        if c.wa_14016291713 && matches!(c.version, 12 | 13) && p.has_psr && !p.has_panel_replay {
            return Err(NoFbcReason::Psr1EnabledWa14016291713);
        }
        if !intel_fbc_pixel_format_is_valid(c, p) {
            return Err(NoFbcReason::PixelFormat);
        }
        if !intel_fbc_tiling_is_valid(c, p) {
            return Err(NoFbcReason::Tiling);
        }
        if !intel_fbc_rotation_is_valid(c, p) {
            return Err(NoFbcReason::Rotation);
        }
        if !plane_stride_is_valid(c, p) {
            return Err(NoFbcReason::Stride);
        }
        if c.version < 20 && p.framebuffer.format.has_alpha() && p.pixel_blend_mode != 0 {
            return Err(NoFbcReason::PerPixelAlpha);
        }
        let (w, h) = intel_fbc_max_plane_size(c);
        if p.src_width > w || p.src_height > h {
            return Err(NoFbcReason::PlaneSize);
        }
        let (w, h) = intel_fbc_max_surface_size(c);
        if p.view_x.saturating_add(p.src_width) > w || p.view_y.saturating_add(p.src_height) > h {
            return Err(NoFbcReason::SurfaceSize);
        }
        if (9..=12).contains(&c.version) && p.view_y & 3 != 0 {
            return Err(NoFbcReason::PlaneStartMisaligned);
        }
        if (9..=12).contains(&c.version) && (p.view_y.saturating_add(p.src_height)) & 3 != 0 {
            return Err(NoFbcReason::PlaneEndMisaligned);
        }
        if _intel_fbc_min_cdclk(c, pixel_rate) > c.max_cdclk_freq {
            return Err(NoFbcReason::PixelRateTooHigh);
        }
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_min_cdclk()
    pub fn intel_fbc_min_cdclk(c: &DisplayCaps, pixel_rate: u32, has_fbc_plane: bool) -> u32 {
        if !has_fbc_plane {
            return 0;
        }
        let requested = _intel_fbc_min_cdclk(c, pixel_rate);
        if requested > c.max_cdclk_freq {
            0
        } else {
            requested
        }
    }

    // upstream: intel_fbc.c intel_fbc_can_flip_nuke()
    pub fn intel_fbc_can_flip_nuke(
        d: &DisplayCaps,
        old: &PlaneState,
        new: &PlaneState,
        r: &FbcRuntime,
        old_reason: Option<NoFbcReason>,
        new_reason: Option<NoFbcReason>,
    ) -> Result<bool, FbcError> {
        if new.needs_modeset {
            return Ok(false);
        }
        if !intel_fbc_is_ok(d, old, old_reason, r.limit, r.allocation_bytes)?
            || !intel_fbc_is_ok(d, new, new_reason, r.limit, r.allocation_bytes)?
        {
            return Ok(false);
        }
        if old.framebuffer.format != new.framebuffer.format
            || old.framebuffer.modifier != new.framebuffer.modifier
        {
            return Ok(false);
        }
        let a = plane_metrics(d, old)?;
        let b = plane_metrics(d, new)?;
        Ok(a.plane_stride_pixels == b.plane_stride_pixels
            && a.cfb_stride == b.cfb_stride
            && a.cfb_size == b.cfb_size
            && a.override_cfb_stride == b.override_cfb_stride)
    }

    // upstream: intel_fbc.c __intel_fbc_pre_update()
    pub fn __intel_fbc_pre_update(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        can_nuke: bool,
        display_version: u8,
    ) -> Result<bool, FbcError> {
        r.flip_pending = true;
        if can_nuke {
            return Ok(false);
        }
        let was_activated = r.activated;
        intel_fbc_deactivate(h, r, NoFbcReason::UpdatePending)?;
        r.activated = false;
        Ok(was_activated && display_version >= 10)
    }

    // upstream: intel_fbc.c intel_fbc_pre_update()
    pub fn intel_fbc_pre_update(
        h: &mut impl FbcControlHooks,
        state: &FbcAtomicState,
        r: &mut FbcRuntime,
        d: &DisplayCaps,
    ) -> Result<bool, FbcError> {
        let mut need_vblank_wait = false;
        for change in &state.new_planes {
            if !change.has_fbc || change.fbc_id != Some(r.id) || change.pipe != state.crtc_pipe {
                continue;
            }
            let id = r.id;
            h.lock_instance(id);
            let result = (|| {
                if r.plane.map_or(false, |current| {
                    current.plane_id == change.new_state.plane_id
                }) {
                    let can_nuke = if let Some(old) = change.old_state {
                        intel_fbc_can_flip_nuke(
                            d,
                            &old,
                            &change.new_state,
                            r,
                            change.old_reason,
                            change.no_fbc_reason,
                        )?
                    } else {
                        false
                    };
                    __intel_fbc_pre_update(h, r, can_nuke, d.version)
                } else {
                    Ok(false)
                }
            })();
            h.unlock_instance(id);
            need_vblank_wait |= result?;
        }
        Ok(need_vblank_wait)
    }

    // upstream: intel_fbc.c __intel_fbc_disable()
    pub fn __intel_fbc_disable(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        id: FbcId,
        d: &DisplayCaps,
        has_dirty_rect: bool,
    ) -> Result<(), FbcError> {
        let _warn_if_still_active = r.active;
        if let Some(state) = r.plane.as_mut() {
            intel_fbc_invalidate_dirty_rect(state)
        }
        h.cleanup_cfb(id, r)?;
        h.sys_cache_disable(id)?;
        if d.dg2 || d.version >= 14 {
            h.set_compressor_clock_gate(id, d, false)?
        }
        r.plane = None;
        r.flip_pending = false;
        r.busy_bits = 0;
        let _dirty_rect_capability = has_dirty_rect;
        Ok(())
    }

    // upstream: intel_fbc.c __intel_fbc_post_update()
    pub fn __intel_fbc_post_update(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        has_fences: bool,
        has_dirty_rect: bool,
    ) -> Result<(), FbcError> {
        r.flip_pending = false;
        r.busy_bits = 0;
        intel_fbc_activate(h, r, has_fences, has_dirty_rect)
    }

    // upstream: intel_fbc.c intel_fbc_post_update()
    pub fn intel_fbc_post_update(
        h: &mut impl FbcControlHooks,
        state: &FbcAtomicState,
        r: &mut FbcRuntime,
        has_fences: bool,
        has_dirty_rect: bool,
    ) -> Result<(), FbcError> {
        for change in &state.new_planes {
            if !change.has_fbc || change.fbc_id != Some(r.id) || change.pipe != state.crtc_pipe {
                continue;
            }
            let id = r.id;
            h.lock_instance(id);
            let result = if r.plane.map_or(false, |current| {
                current.plane_id == change.new_state.plane_id
            }) {
                __intel_fbc_post_update(h, r, has_fences, has_dirty_rect)
            } else {
                Ok(())
            };
            h.unlock_instance(id);
            result?;
        }
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_get_frontbuffer_bit()
    pub fn intel_fbc_get_frontbuffer_bit(r: &FbcRuntime) -> u32 {
        r.frontbuffer_bit()
    }

    // upstream: intel_fbc.c __intel_fbc_invalidate()
    pub fn __intel_fbc_invalidate(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        bits: u32,
        origin: FbOrigin,
    ) -> Result<(), FbcError> {
        if matches!(origin, FbOrigin::Flip | FbOrigin::CursorUpdate) {
            return Ok(());
        }
        let bits = bits & intel_fbc_get_frontbuffer_bit(r);
        if bits == 0 {
            return Ok(());
        }
        r.busy_bits |= bits;
        intel_fbc_deactivate(h, r, NoFbcReason::UpdatePending)
    }

    // upstream: intel_fbc.c intel_fbc_invalidate()
    pub fn intel_fbc_invalidate(
        h: &mut impl FbcControlHooks,
        instances: &mut [FbcRuntime],
        bits: u32,
        origin: FbOrigin,
    ) -> Result<(), FbcError> {
        if matches!(origin, FbOrigin::Flip | FbOrigin::CursorUpdate) {
            return Ok(());
        }
        for runtime in instances {
            let id = runtime.id;
            h.lock_instance(id);
            let result = __intel_fbc_invalidate(h, runtime, bits, origin);
            h.unlock_instance(id);
            result?;
        }
        Ok(())
    }

    // upstream: intel_fbc.c __intel_fbc_flush()
    pub fn __intel_fbc_flush(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        bits: u32,
        origin: FbOrigin,
        has_fences: bool,
        has_dirty_rect: bool,
    ) -> Result<(), FbcError> {
        let bits = bits & intel_fbc_get_frontbuffer_bit(r);
        if bits == 0 {
            return Ok(());
        }
        r.busy_bits &= !bits;
        if matches!(origin, FbOrigin::Flip | FbOrigin::CursorUpdate) {
            return Ok(());
        }
        if r.busy_bits != 0 || r.flip_pending {
            return Ok(());
        }
        if r.active {
            intel_fbc_nuke(h, r)
        } else {
            intel_fbc_activate(h, r, has_fences, has_dirty_rect)
        }
    }

    // upstream: intel_fbc.c intel_fbc_flush()
    pub fn intel_fbc_flush(
        h: &mut impl FbcControlHooks,
        instances: &mut [FbcRuntime],
        bits: u32,
        origin: FbOrigin,
        has_fences: bool,
        has_dirty_rect: bool,
    ) -> Result<(), FbcError> {
        for runtime in instances {
            let id = runtime.id;
            h.lock_instance(id);
            let result = __intel_fbc_flush(h, runtime, bits, origin, has_fences, has_dirty_rect);
            h.unlock_instance(id);
            result?;
        }
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_atomic_check()
    pub fn intel_fbc_atomic_check(d: &DisplayState, state: &mut FbcAtomicState) {
        for change in &mut state.new_planes {
            if !change.has_fbc {
                continue;
            }
            change.no_fbc_reason =
                intel_fbc_check_plane(d, &change.new_state, change.pixel_rate).err();
        }
    }

    // upstream: intel_fbc.c __intel_fbc_enable()
    pub fn __intel_fbc_enable(
        h: &mut impl FbcControlHooks,
        d: &DisplayState,
        r: &mut FbcRuntime,
        id: FbcId,
        p: &PlaneState,
        plane_reason: Option<NoFbcReason>,
        refresh_hz: u16,
    ) -> Result<(), FbcError> {
        if let Some(old) = r.plane {
            if old.plane_id != p.plane_id {
                return Ok(());
            }
            if intel_fbc_is_ok(&d.caps, p, plane_reason, r.limit, r.allocation_bytes)? {
                return intel_fbc_update_state(r, &d.caps, p, refresh_hz);
            }
            __intel_fbc_disable(h, r, id, &d.caps, d.caps.has_dirty_rect)?;
        }
        let _warn_if_active = r.active;
        r.no_fbc_reason = plane_reason;
        if r.no_fbc_reason.is_some() {
            return Ok(());
        }
        if !intel_fbc_is_fence_ok(&d.caps, p) {
            r.no_fbc_reason = Some(NoFbcReason::FramebufferNotFenced);
            return Ok(());
        }
        if r.underrun_detected {
            r.no_fbc_reason = Some(NoFbcReason::FifoUnderrun);
            return Ok(());
        }
        let size = intel_fbc_cfb_size(&d.caps, p)?;
        let allocation = match h.allocate_cfb(id, &d.caps, size, intel_fbc_min_limit(p)) {
            Ok(allocation) => allocation,
            Err(_) => {
                r.no_fbc_reason = Some(NoFbcReason::NotEnoughStolenMemory);
                return Ok(());
            }
        };
        r.limit = allocation.limit;
        r.allocation_bytes = allocation.framebuffer_bytes;
        r.cfb_allocated = true;
        r.llb_allocated = allocation.line_buffer_bytes != 0;
        r.no_fbc_reason = Some(NoFbcReason::EnabledNotActive);
        intel_fbc_update_state(r, &d.caps, p, refresh_hz)?;
        if d.caps.has_dirty_rect {
            h.initialize_dirty_rect(id, p)?
        }
        h.program_workarounds(id, &d.caps)?;
        h.program_cfb(id, r)?;
        h.sys_cache_enable(id, r)?;
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_disable()
    pub fn intel_fbc_disable(
        h: &mut impl FbcControlHooks,
        pipe: u8,
        changes: &[FbcPlaneChange],
        r: &mut FbcRuntime,
        id: FbcId,
        d: &DisplayCaps,
    ) -> Result<(), FbcError> {
        for change in changes {
            if !change.has_fbc || change.fbc_id != Some(r.id) || change.pipe != pipe {
                continue;
            }
            h.lock_instance(id);
            let result = if r.plane.map_or(false, |current| {
                current.plane_id == change.new_state.plane_id
            }) {
                __intel_fbc_disable(h, r, id, d, d.has_dirty_rect)
            } else {
                Ok(())
            };
            h.unlock_instance(id);
            result?;
        }
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_update()
    pub fn intel_fbc_update(
        h: &mut impl FbcControlHooks,
        state: &FbcAtomicState,
        d: &DisplayState,
        r: &mut FbcRuntime,
        id: FbcId,
    ) -> Result<(), FbcError> {
        for change in &state.new_planes {
            if !change.has_fbc || change.fbc_id != Some(r.id) || change.pipe != state.crtc_pipe {
                continue;
            }
            h.lock_instance(id);
            let result = if state.needs_fastset && change.no_fbc_reason.is_some() {
                if r.plane.map_or(false, |current| {
                    current.plane_id == change.new_state.plane_id
                }) {
                    __intel_fbc_disable(h, r, id, &d.caps, d.caps.has_dirty_rect)
                } else {
                    Ok(())
                }
            } else {
                __intel_fbc_enable(
                    h,
                    d,
                    r,
                    id,
                    &change.new_state,
                    change.no_fbc_reason,
                    change.refresh_hz,
                )
            };
            h.unlock_instance(id);
            result?;
        }
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_underrun_work_fn()
    pub fn intel_fbc_underrun_work_fn(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        id: FbcId,
        display: &DisplayCaps,
        pipe: u8,
    ) -> Result<(), FbcError> {
        h.lock_instance(id);
        let result = (|| {
            if r.underrun_detected || r.plane.is_none() {
                return Ok(());
            }
            r.underrun_detected = true;
            intel_fbc_deactivate(h, r, NoFbcReason::FifoUnderrun)?;
            if !r.flip_pending {
                h.wait_for_next_vblank(pipe)?
            }
            __intel_fbc_disable(h, r, id, display, display.has_dirty_rect)
        })();
        h.unlock_instance(id);
        result
    }

    // upstream: intel_fbc.c __intel_fbc_reset_underrun()
    pub fn __intel_fbc_reset_underrun(h: &mut impl FbcControlHooks, r: &mut FbcRuntime, id: FbcId) {
        h.cancel_underrun_work(id);
        h.lock_instance(id);
        if r.underrun_detected {
            r.no_fbc_reason = Some(NoFbcReason::FifoUnderrunCleared)
        }
        r.underrun_detected = false;
        h.unlock_instance(id);
    }

    // upstream: intel_fbc.c intel_fbc_reset_underrun()
    pub fn intel_fbc_reset_underrun(
        h: &mut impl FbcControlHooks,
        instances: &mut [FbcRuntime],
        ids: &[FbcId],
    ) {
        for (i, runtime) in instances.iter_mut().enumerate() {
            __intel_fbc_reset_underrun(h, runtime, ids.get(i).copied().unwrap_or(FbcId::A));
        }
    }

    // upstream: intel_fbc.c __intel_fbc_handle_fifo_underrun_irq()
    pub fn __intel_fbc_handle_fifo_underrun_irq(
        h: &mut impl FbcControlHooks,
        r: &FbcRuntime,
        id: FbcId,
    ) {
        if !r.underrun_detected {
            h.queue_underrun_work(id)
        }
    }

    // upstream: intel_fbc.c intel_fbc_handle_fifo_underrun_irq()
    pub fn intel_fbc_handle_fifo_underrun_irq(
        h: &mut impl FbcControlHooks,
        instances: &[FbcRuntime],
        ids: &[FbcId],
    ) {
        for (i, runtime) in instances.iter().enumerate() {
            let id = ids.get(i).copied().unwrap_or(FbcId::A);
            __intel_fbc_handle_fifo_underrun_irq(h, runtime, id);
        }
    }

    // upstream: intel_fbc.c intel_fbc_read_underrun_dbg_info()
    pub fn intel_fbc_read_underrun_dbg_info(
        io: &mut impl FbcRegisterIo,
        id: FbcId,
    ) -> Result<bool, FbcError> {
        const FBC_UNDERRUN_DECMPR: u32 = 1 << 27;
        let val = io.read(FbcRegister::DebugStatus(id))?;
        if val & FBC_UNDERRUN_DECMPR == 0 {
            return Ok(false);
        }
        io.write(FbcRegister::DebugStatus(id), FBC_UNDERRUN_DECMPR)?;
        io.write(FbcRegister::DebugStatus(id), FBC_UNDERRUN_DECMPR)?;
        Ok(true)
    }

    // upstream: intel_fbc.c intel_sanitize_fbc_option()
    pub fn intel_sanitize_fbc_option(param: i32, has_fbc: bool, c: &DisplayCaps) -> i32 {
        if param >= 0 {
            return if param != 0 { 1 } else { 0 };
        }
        if !has_fbc {
            return 0;
        }
        if c.broadwell || c.version >= 9 { 1 } else { 0 }
    }

    // upstream: intel_fbc.c intel_fbc_add_plane()
    pub fn intel_fbc_add_plane(binding: &mut PlaneFbcBinding, fbc_id: FbcId, plane_id: u8) {
        binding.plane_id = plane_id;
        binding.fbc_id = Some(fbc_id);
    }

    // upstream: intel_fbc.c intel_fbc_create()
    pub fn intel_fbc_create(
        h: &mut impl FbcControlHooks,
        display: &DisplayCaps,
        id: FbcId,
    ) -> Option<FbcRuntime> {
        if !h.allocate_instance_container(id) {
            return None;
        }
        if !h.allocate_stolen_node(id, false) {
            h.free_stolen_node(id, true);
            h.free_stolen_node(id, false);
            h.free_instance_container(id);
            return None;
        }
        if !h.allocate_stolen_node(id, true) {
            h.free_stolen_node(id, true);
            h.free_stolen_node(id, false);
            h.free_instance_container(id);
            return None;
        }
        let mut runtime = FbcRuntime::default();
        runtime.id = id;
        h.initialize_lock_and_underrun_work(id);
        runtime.generation = fbc_generation(display.version, display.g4x);
        runtime.funcs = intel_fbc_function_table(display.version, display.g4x);
        Some(runtime)
    }

    // upstream: intel_fbc.c intel_fbc_init()
    pub fn intel_fbc_init(
        display: &mut FbcDisplay,
        h: &mut impl FbcControlHooks,
        c: &DisplayCaps,
        enable_param: i32,
        has_fbc: bool,
        fbc_mask: u8,
    ) {
        display.enable_fbc_param = intel_sanitize_fbc_option(enable_param, has_fbc, c);
        for i in 0..2 {
            if fbc_mask & (1 << i) != 0 {
                let id = if i == 0 { FbcId::A } else { FbcId::B };
                display.instances[i] = intel_fbc_create(h, c, id);
            }
        }
        display.sys_cache_id = 2;
    }

    // upstream: intel_fbc.c intel_fbc_sanitize()
    pub fn intel_fbc_sanitize(
        display: &mut FbcDisplay,
        h: &mut impl FbcControlHooks,
        c: &DisplayCaps,
    ) -> Result<(), FbcError> {
        for instance in display.instances.iter_mut() {
            let Some(runtime) = instance.as_mut() else {
                continue;
            };
            if intel_fbc_hw_is_active(h, runtime)? {
                intel_fbc_hw_deactivate(h, runtime)?
            }
        }
        // Display-12/13 has no FBC system-cache assignment; newer source clears it
        // under its display-owned mutex, represented by the explicit state update.
        if c.has_fbc_sys_cache {
            h.lock_sys_cache();
            let result = h.clear_system_cache();
            if result.is_ok() {
                display.sys_cache_id = 2
            }
            h.unlock_sys_cache();
            result?;
        }
        Ok(())
    }

    // upstream: intel_fbc.c intel_fbc_debugfs_false_color_get()
    pub fn intel_fbc_debugfs_false_color_get(r: &FbcRuntime) -> u64 {
        if r.false_color { 1 } else { 0 }
    }

    // upstream: intel_fbc.c intel_fbc_debugfs_false_color_set()
    pub fn intel_fbc_debugfs_false_color_set(
        h: &mut impl FbcControlHooks,
        r: &mut FbcRuntime,
        value: u64,
    ) -> Result<(), FbcError> {
        if r.funcs.set_false_color.is_none() {
            return Err(FbcError::Unsupported);
        }
        h.lock_instance(r.id);
        r.false_color = value != 0;
        let result = if r.active {
            h.set_false_color(r, r.false_color)
        } else {
            Ok(())
        };
        h.unlock_instance(r.id);
        result
    }
}

pub use upstream_source_order::*;
