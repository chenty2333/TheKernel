// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_cursor.c translation.
// DRM atomic, framebuffer, IRQ, power and MMIO interfaces are explicit traits.

#![allow(dead_code, clippy::too_many_arguments)]

extern crate alloc;
use alloc::vec::Vec;

pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_MODE_ROTATE_0: u32 = 1;
pub const DRM_MODE_ROTATE_180: u32 = 4;
pub const CURSOR_ENABLE: u32 = 1 << 31;
pub const CURSOR_FORMAT_ARGB: u32 = 4 << 24;
pub const CURSOR_PIPE_GAMMA_ENABLE: u32 = 1 << 30;
pub const MCURSOR_PIPE_GAMMA_ENABLE: u32 = 1 << 26;
pub const MCURSOR_PIPE_CSC_ENABLE: u32 = 1 << 24;
pub const MCURSOR_TRICKLE_FEED_DISABLE: u32 = 1 << 14;
pub const MCURSOR_MODE_MASK: u32 = 0x27;
pub const MCURSOR_MODE_64_ARGB_AX: u32 = 0x27;
pub const MCURSOR_MODE_128_ARGB_AX: u32 = 0x22;
pub const MCURSOR_MODE_256_ARGB_AX: u32 = 0x23;
pub const MCURSOR_MODE_64_2B: u32 = 0x04;
pub const MCURSOR_ROTATE_180: u32 = 1 << 15;
pub const CUR_FBC_EN: u32 = 1 << 31;
pub const CUR_WM_EN: u32 = 1 << 31;
pub const CUR_WM_IGNORE_LINES: u32 = 1 << 30;
pub const CUR_WM_BLOCKS_MASK: u32 = 0xfff;
pub const CUR_WM_LINES_MASK: u32 = 0x1fff << 14;
pub const MCURSOR_ARB_SLOTS_ONE: u32 = 1 << 28;
pub const CURSOR_POS_SIGN: u32 = 1 << 15;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}
impl Rect {
    pub const fn width(self) -> i32 {
        self.x2 - self.x1
    }
    pub const fn height(self) -> i32 {
        self.y2 - self.y1
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorDisplay {
    pub display_ver: u8,
    pub gmch: bool,
    pub g4x: bool,
    pub i830: bool,
    pub i845g: bool,
    pub i865g: bool,
    pub i85x: bool,
    pub i915g: bool,
    pub i915gm: bool,
    pub ivybridge: bool,
    pub sandybridge: bool,
    pub cherryview: bool,
    pub has_cur_fbc: bool,
    pub vtd_wa: bool,
    pub wa_22012358565: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Framebuffer {
    pub modifier: u64,
    pub cpp0: u8,
    pub pitch0: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorPlaneState {
    pub pipe: u8,
    pub visible: bool,
    pub src: Rect,
    pub dst: Rect,
    pub rotation: u32,
    pub fb: Option<Framebuffer>,
    pub view_offset: u32,
    pub view_x: i32,
    pub view_y: i32,
    pub mapping_stride: u32,
    pub surf: u32,
    pub ctl: u32,
    pub psr2_sel_fetch_area: Rect,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorCrtcState {
    pub pipe: u8,
    pub gamma_enable: bool,
    pub csc_enable: bool,
    pub enable_psr2_sel_fetch: bool,
    pub enable_psr2_su_region_et: bool,
    pub pipe_src: Rect,
    pub psr2_su_area: Rect,
    pub active: bool,
    pub needs_modeset: bool,
    pub needs_fastset: bool,
    pub joiner_pipes: u8,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorPlane {
    pub pipe: u8,
    pub plane_id: u8,
    pub display: CursorDisplay,
    pub frontbuffer_bit: u32,
    pub vtd_guard: u8,
    pub cursor_base: u32,
    pub cursor_size: u32,
    pub cursor_cntl: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorModeConfig {
    pub cursor_width: u32,
    pub cursor_height: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WmLevel {
    pub enable: bool,
    pub ignore_lines: bool,
    pub blocks: u16,
    pub lines: u16,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdbEntry {
    pub start: u16,
    pub end: u16,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorWm {
    pub optimal: [[WmLevel; 8]; 8],
    pub transition: [WmLevel; 8],
    pub sagv: [WmLevel; 2],
    pub ddb: [DdbEntry; 8],
    pub levels: usize,
    pub has_sagv_wm: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorReg {
    Control(u8),
    Base(u8),
    SurfaceLive(u8),
    Position(u8),
    Size(u8),
    FbcControl(u8),
    EarlyPosition(u8),
    SelectFetchControl(u8),
    Wm(u8, u8),
    WmTransition(u8),
    WmSagv(u8),
    WmSagvTransition(u8),
    BufCfg(u8),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorError {
    pub ctl: u32,
    pub surf: u32,
    pub surflive: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorErrorCode {
    Invalid,
    NoMemory,
    Io,
}

pub trait CursorIo {
    fn compute_gtt(&mut self, _state: &mut CursorPlaneState) -> i32 {
        0
    }
    fn add_fb_offsets(&mut self, _x: &mut i32, _y: &mut i32, _state: &CursorPlaneState) {}
    fn compute_aligned_offset(
        &mut self,
        _x: &mut i32,
        _y: &mut i32,
        _state: &CursorPlaneState,
    ) -> u32 {
        0
    }
    fn translate_rect(&mut self, rect: &mut Rect, x: i32, y: i32) {
        rect.x1 += x;
        rect.x2 += x;
        rect.y1 += y;
        rect.y2 += y;
    }
    fn check_clipping(&mut self, _state: &mut CursorPlaneState, _crtc: &CursorCrtcState) -> i32 {
        0
    }
    fn check_src_coordinates(&mut self, _state: &CursorPlaneState) -> i32 {
        0
    }
    fn log(&mut self, _code: u32, _a: i64, _b: i64) {}
    fn warn(&mut self, _code: u32) {}
    fn wa_vtd(&self, display: CursorDisplay) -> bool {
        display.vtd_wa
    }
    fn wa_22012358565(&self, display: CursorDisplay) -> bool {
        display.wa_22012358565
    }
    fn fbc_height(&self, height: u32) -> u32 {
        height.saturating_sub(1) & 0xff
    }
    fn fbc_has(&self, display: CursorDisplay) -> bool {
        display.has_cur_fbc
    }
    fn read(&mut self, _reg: CursorReg) -> u32 {
        0
    }
    fn write(&mut self, _reg: CursorReg, _value: u32, _dsb: bool) {}
    fn pipe_wm(&self, _pipe: u8) -> CursorWm {
        CursorWm::default()
    }
    fn power_get_if_enabled(&mut self, _pipe: u8) -> Option<u64> {
        Some(0)
    }
    fn power_put(&mut self, _pipe: u8, _cookie: u64) {}
}

const fn cursor_pos_x(x: u32) -> u32 {
    x & 0x7fff
}
const fn cursor_pos_y(y: u32) -> u32 {
    (y & 0x7fff) << 16
}
const fn cursor_width(w: u32) -> u32 {
    w & 0x3ff
}
const fn cursor_height(h: u32) -> u32 {
    (h & 0x3ff) << 12
}
const fn cursor_stride(stride: u32) -> u32 {
    ((stride.trailing_zeros() + 1 - 9) & 3) << 28
}
const fn cursor_mode_width(width: i32) -> u32 {
    match width {
        64 => MCURSOR_MODE_64_ARGB_AX,
        128 => MCURSOR_MODE_128_ARGB_AX,
        256 => MCURSOR_MODE_256_ARGB_AX,
        _ => 0,
    }
}
const fn pipe_sel(pipe: u8) -> u32 {
    (pipe as u32) << 28
}
const fn cursor_wm_blocks(v: u16) -> u32 {
    (v as u32) & CUR_WM_BLOCKS_MASK
}
const fn cursor_wm_lines(v: u16) -> u32 {
    ((v as u32) << 14) & CUR_WM_LINES_MASK
}

// upstream: intel_cursor.c intel_cursor_surf_offset()
pub const fn intel_cursor_surf_offset(state: &CursorPlaneState) -> u32 {
    state.view_offset
}
// upstream: intel_cursor.c intel_cursor_position()
pub fn intel_cursor_position(
    crtc: &CursorCrtcState,
    plane: &CursorPlaneState,
    early_tpt: bool,
) -> u32 {
    let mut x = plane.dst.x1;
    let mut y = plane.dst.y1;
    if early_tpt {
        y = (-plane.dst.height() + 1).max(y - crtc.psr2_su_area.y1);
    }
    let mut pos = 0;
    if x < 0 {
        pos |= CURSOR_POS_SIGN;
        x = -x;
    }
    pos |= cursor_pos_x(x as u32);
    if y < 0 {
        pos |= CURSOR_POS_SIGN << 16;
        y = -y;
    }
    pos |= cursor_pos_y(y as u32);
    pos
}
// upstream: intel_cursor.c intel_cursor_size_ok()
pub fn intel_cursor_size_ok(plane: &CursorPlaneState, config: CursorModeConfig) -> bool {
    let w = plane.dst.width();
    let h = plane.dst.height();
    w > 0 && w <= config.cursor_width as i32 && h > 0 && h <= config.cursor_height as i32
}
// upstream: intel_cursor.c intel_cursor_check_surface()
pub fn intel_cursor_check_surface(
    io: &mut impl CursorIo,
    display: CursorDisplay,
    plane: &mut CursorPlaneState,
) -> i32 {
    let ret = io.compute_gtt(plane);
    if ret != 0 {
        return ret;
    }
    if !plane.visible {
        return 0;
    }
    let mut src_x = plane.src.x1 >> 16;
    let mut src_y = plane.src.y1 >> 16;
    io.add_fb_offsets(&mut src_x, &mut src_y, plane);
    let mut offset = io.compute_aligned_offset(&mut src_x, &mut src_y, plane);
    if src_x != 0 || src_y != 0 {
        io.log(1, src_x as i64, src_y as i64);
        return -22;
    }
    let delta_x = (src_x << 16) - plane.src.x1;
    let delta_y = (src_y << 16) - plane.src.y1;
    io.translate_rect(&mut plane.src, delta_x, delta_y);
    if display.gmch && plane.rotation & DRM_MODE_ROTATE_180 != 0 {
        if let Some(fb) = plane.fb {
            let w = plane.src.width() >> 16;
            let h = plane.src.height() >> 16;
            offset = offset.wrapping_add(((h * w - 1) as u32).wrapping_mul(u32::from(fb.cpp0)));
        }
    }
    plane.view_offset = offset;
    plane.view_x = src_x;
    plane.view_y = src_y;
    0
}
// upstream: intel_cursor.c intel_check_cursor()
pub fn intel_check_cursor(
    io: &mut impl CursorIo,
    display: CursorDisplay,
    crtc: &CursorCrtcState,
    plane: &mut CursorPlaneState,
) -> i32 {
    if plane
        .fb
        .is_some_and(|fb| fb.modifier != DRM_FORMAT_MOD_LINEAR)
    {
        io.log(2, plane.dst.width() as i64, plane.dst.height() as i64);
        return -22;
    }
    let src = plane.src;
    let dst = plane.dst;
    let ret = io.check_clipping(plane, crtc);
    if ret != 0 {
        return ret;
    }
    plane.src = src;
    plane.dst = dst;
    io.translate_rect(&mut plane.dst, -crtc.pipe_src.x1, -crtc.pipe_src.y1);
    let ret = intel_cursor_check_surface(io, display, plane);
    if ret != 0 {
        return ret;
    }
    if !plane.visible {
        return 0;
    }
    io.check_src_coordinates(plane)
}
// upstream: intel_cursor.c i845_cursor_max_stride()
pub const fn i845_cursor_max_stride(_plane: u8, _cpp: u8, _modifier: u64, _rotation: u32) -> u32 {
    2048
}
// upstream: intel_cursor.c i845_cursor_min_alignment()
pub const fn i845_cursor_min_alignment() -> u32 {
    32
}
// upstream: intel_cursor.c i845_cursor_ctl_crtc()
pub const fn i845_cursor_ctl_crtc(crtc: &CursorCrtcState) -> u32 {
    if crtc.gamma_enable {
        CURSOR_PIPE_GAMMA_ENABLE
    } else {
        0
    }
}
// upstream: intel_cursor.c i845_cursor_ctl()
pub const fn i845_cursor_ctl(plane: &CursorPlaneState) -> u32 {
    CURSOR_ENABLE | CURSOR_FORMAT_ARGB | cursor_stride(plane.mapping_stride)
}
// upstream: intel_cursor.c i845_cursor_size_ok()
pub fn i845_cursor_size_ok(plane: &CursorPlaneState, config: CursorModeConfig) -> bool {
    intel_cursor_size_ok(plane, config) && plane.dst.width() % 64 == 0
}
// upstream: intel_cursor.c i845_check_cursor()
pub fn i845_check_cursor(
    io: &mut impl CursorIo,
    display: CursorDisplay,
    config: CursorModeConfig,
    crtc: &CursorCrtcState,
    plane: &mut CursorPlaneState,
) -> i32 {
    let ret = intel_check_cursor(io, display, crtc, plane);
    if ret != 0 {
        return ret;
    }
    let Some(fb) = plane.fb else {
        return 0;
    };
    if !i845_cursor_size_ok(plane, config) {
        io.log(3, plane.dst.width() as i64, plane.dst.height() as i64);
        return -22;
    }
    if plane.visible && plane.mapping_stride != fb.pitch0 {
        io.warn(1);
    }
    if !matches!(fb.pitch0, 256 | 512 | 1024 | 2048) {
        io.log(4, fb.pitch0 as i64, 0);
        return -22;
    }
    plane.ctl = i845_cursor_ctl(plane);
    0
}
// upstream: intel_cursor.c i845_cursor_update_arm()
pub fn i845_cursor_update_arm(
    io: &mut impl CursorIo,
    plane: &mut CursorPlane,
    crtc: &CursorCrtcState,
    state: Option<&CursorPlaneState>,
) {
    let (mut ctl, mut base, mut pos, mut size) = (0, 0, 0, 0);
    if let Some(p) = state.filter(|p| p.visible) {
        let w = p.dst.width() as u32;
        let h = p.dst.height() as u32;
        ctl = p.ctl | i845_cursor_ctl_crtc(crtc);
        size = cursor_height(h) | cursor_width(w);
        base = p.surf;
        pos = intel_cursor_position(crtc, p, false);
    }
    if plane.cursor_base != base || plane.cursor_size != size || plane.cursor_cntl != ctl {
        io.write(CursorReg::Control(0), 0, false);
        io.write(CursorReg::Base(0), base, false);
        io.write(CursorReg::Size(0), size, false);
        io.write(CursorReg::Position(0), pos, false);
        io.write(CursorReg::Control(0), ctl, false);
        plane.cursor_base = base;
        plane.cursor_size = size;
        plane.cursor_cntl = ctl;
    } else {
        io.write(CursorReg::Position(0), pos, false);
    }
}
// upstream: intel_cursor.c i845_cursor_disable_arm()
pub fn i845_cursor_disable_arm(
    io: &mut impl CursorIo,
    plane: &mut CursorPlane,
    crtc: &CursorCrtcState,
) {
    i845_cursor_update_arm(io, plane, crtc, None)
}
// upstream: intel_cursor.c i845_cursor_get_hw_state()
pub fn i845_cursor_get_hw_state(io: &mut impl CursorIo) -> bool {
    let Some(w) = io.power_get_if_enabled(0) else {
        return false;
    };
    let enabled = io.read(CursorReg::Control(0)) & CURSOR_ENABLE != 0;
    io.power_put(0, w);
    enabled
}
// upstream: intel_cursor.c i9xx_cursor_max_stride()
pub const fn i9xx_cursor_max_stride(cursor_width: u32) -> u32 {
    cursor_width * 4
}
// upstream: intel_cursor.c i830_cursor_min_alignment()
pub const fn i830_cursor_min_alignment() -> u32 {
    16 * 1024
}
// upstream: intel_cursor.c i85x_cursor_min_alignment()
pub const fn i85x_cursor_min_alignment() -> u32 {
    256
}
// upstream: intel_cursor.c i9xx_cursor_min_alignment()
pub const fn i9xx_cursor_min_alignment(vtd_wa: bool) -> u32 {
    if vtd_wa { 64 * 1024 } else { 4 * 1024 }
}
// upstream: intel_cursor.c i9xx_cursor_ctl_crtc()
pub fn i9xx_cursor_ctl_crtc(display: CursorDisplay, crtc: &CursorCrtcState) -> u32 {
    if display.display_ver >= 11 {
        return 0;
    }
    let mut ctl = 0;
    if crtc.gamma_enable {
        ctl = MCURSOR_PIPE_GAMMA_ENABLE;
    }
    if crtc.csc_enable {
        ctl |= MCURSOR_PIPE_CSC_ENABLE;
    }
    if display.display_ver < 5 && !display.g4x {
        ctl |= pipe_sel(crtc.pipe);
    }
    ctl
}
// upstream: intel_cursor.c i9xx_cursor_ctl()
pub fn i9xx_cursor_ctl(
    io: &mut impl CursorIo,
    plane: &CursorPlaneState,
    display: CursorDisplay,
) -> u32 {
    let mut ctl = if display.sandybridge || display.ivybridge {
        MCURSOR_TRICKLE_FEED_DISABLE
    } else {
        0
    };
    ctl |= cursor_mode_width(plane.dst.width());
    if ctl & MCURSOR_MODE_MASK == 0 {
        io.log(5, plane.dst.width() as i64, 0);
        return 0;
    }
    if plane.rotation & DRM_MODE_ROTATE_180 != 0 {
        ctl |= MCURSOR_ROTATE_180;
    }
    if io.wa_22012358565(display) {
        ctl |= 1 << 16;
    }
    ctl
}
// upstream: intel_cursor.c i9xx_cursor_size_ok()
pub fn i9xx_cursor_size_ok(
    display: CursorDisplay,
    plane: &CursorPlaneState,
    config: CursorModeConfig,
) -> bool {
    let w = plane.dst.width();
    let h = plane.dst.height();
    if !intel_cursor_size_ok(plane, config) || !matches!(w, 64 | 128 | 256) {
        return false;
    }
    if display.has_cur_fbc && plane.rotation & DRM_MODE_ROTATE_0 != 0 {
        h >= 8 && h <= w
    } else {
        h == w
    }
}
// upstream: intel_cursor.c i9xx_check_cursor()
pub fn i9xx_check_cursor(
    io: &mut impl CursorIo,
    display: CursorDisplay,
    config: CursorModeConfig,
    crtc: &CursorCrtcState,
    plane: &mut CursorPlaneState,
) -> i32 {
    let ret = intel_check_cursor(io, display, crtc, plane);
    if ret != 0 {
        return ret;
    }
    let Some(fb) = plane.fb else {
        return 0;
    };
    if !i9xx_cursor_size_ok(display, plane, config) {
        io.log(6, plane.dst.width() as i64, plane.dst.height() as i64);
        return -22;
    }
    if plane.visible && plane.mapping_stride != fb.pitch0 {
        io.warn(2);
    }
    if fb.pitch0 != plane.dst.width() as u32 * u32::from(fb.cpp0) {
        io.log(7, fb.pitch0 as i64, plane.dst.width() as i64);
        return -22;
    }
    if display.cherryview && plane.pipe == 2 && plane.visible && plane.dst.x1 < 0 {
        io.log(8, plane.dst.x1 as i64, 0);
        return -22;
    }
    plane.ctl = i9xx_cursor_ctl(io, plane, display);
    0
}
// upstream: intel_cursor.c i9xx_cursor_disable_sel_fetch_arm()
pub fn i9xx_cursor_disable_sel_fetch_arm(
    io: &mut impl CursorIo,
    plane: &CursorPlane,
    crtc: &CursorCrtcState,
) {
    if crtc.enable_psr2_sel_fetch {
        io.write(CursorReg::SelectFetchControl(plane.pipe), 0, true);
    }
}
// upstream: intel_cursor.c wa_16021440873()
pub fn wa_16021440873(
    io: &mut impl CursorIo,
    plane: &CursorPlane,
    crtc: &CursorCrtcState,
    state: &CursorPlaneState,
) {
    let ctl = (state.ctl & !MCURSOR_MODE_MASK) | MCURSOR_MODE_64_2B;
    let et_y = crtc.pipe_src.height() + 1;
    io.write(CursorReg::SelectFetchControl(plane.pipe), ctl, true);
    io.write(
        CursorReg::EarlyPosition(plane.pipe),
        cursor_pos_y(et_y as u32),
        true,
    );
}
// upstream: intel_cursor.c i9xx_cursor_update_sel_fetch_arm()
pub fn i9xx_cursor_update_sel_fetch_arm(
    io: &mut impl CursorIo,
    plane: &CursorPlane,
    crtc: &CursorCrtcState,
    state: &CursorPlaneState,
) {
    if !crtc.enable_psr2_sel_fetch {
        return;
    }
    if state.psr2_sel_fetch_area.height() > 0 {
        if crtc.enable_psr2_su_region_et {
            io.write(
                CursorReg::EarlyPosition(plane.pipe),
                intel_cursor_position(crtc, state, true),
                true,
            );
        }
        io.write(CursorReg::SelectFetchControl(plane.pipe), state.ctl, true);
    } else if crtc.enable_psr2_su_region_et {
        wa_16021440873(io, plane, crtc, state);
    } else {
        i9xx_cursor_disable_sel_fetch_arm(io, plane, crtc);
    }
}
// upstream: intel_cursor.c skl_cursor_ddb_reg_val()
pub const fn skl_cursor_ddb_reg_val(entry: DdbEntry) -> u32 {
    if entry.end == 0 {
        0
    } else {
        (((entry.end - 1) as u32 & 0xfff) << 16) | (entry.start as u32 & 0xfff)
    }
}
// upstream: intel_cursor.c skl_cursor_wm_reg_val()
pub const fn skl_cursor_wm_reg_val(level: WmLevel) -> u32 {
    (if level.enable { CUR_WM_EN } else { 0 })
        | (if level.ignore_lines {
            CUR_WM_IGNORE_LINES
        } else {
            0
        })
        | cursor_wm_blocks(level.blocks)
        | cursor_wm_lines(level.lines)
}
// upstream: intel_cursor.c skl_write_cursor_wm()
pub fn skl_write_cursor_wm(io: &mut impl CursorIo, plane: &CursorPlane, crtc: &CursorCrtcState) {
    let wm = io.pipe_wm(crtc.pipe);
    for level in 0..wm.levels.min(8) {
        io.write(
            CursorReg::Wm(plane.pipe, level as u8),
            skl_cursor_wm_reg_val(wm.optimal[plane.plane_id as usize][level]),
            true,
        );
    }
    io.write(
        CursorReg::WmTransition(plane.pipe),
        skl_cursor_wm_reg_val(wm.transition[plane.plane_id as usize]),
        true,
    );
    if wm.has_sagv_wm {
        io.write(
            CursorReg::WmSagv(plane.pipe),
            skl_cursor_wm_reg_val(wm.sagv[0]),
            true,
        );
        io.write(
            CursorReg::WmSagvTransition(plane.pipe),
            skl_cursor_wm_reg_val(wm.sagv[1]),
            true,
        );
    }
    io.write(
        CursorReg::BufCfg(plane.pipe),
        skl_cursor_ddb_reg_val(wm.ddb[plane.plane_id as usize]),
        true,
    );
}
// upstream: intel_cursor.c i9xx_cursor_update_arm()
pub fn i9xx_cursor_update_arm(
    io: &mut impl CursorIo,
    plane: &mut CursorPlane,
    crtc: &CursorCrtcState,
    state: Option<&CursorPlaneState>,
) {
    let pipe = plane.pipe;
    let (mut ctl, mut base, mut pos, mut fbc) = (0, 0, 0, 0);
    if let Some(s) = state.filter(|s| s.visible) {
        let w = s.dst.width();
        let h = s.dst.height();
        ctl = s.ctl | i9xx_cursor_ctl_crtc(plane.display, crtc);
        if plane.display.display_ver < 14 && w != h {
            fbc = CUR_FBC_EN | io.fbc_height(h as u32);
        }
        base = s.surf;
        pos = intel_cursor_position(crtc, s, false);
    }
    if plane.display.display_ver >= 9 {
        skl_write_cursor_wm(io, plane, crtc);
    }
    if let Some(s) = state {
        i9xx_cursor_update_sel_fetch_arm(io, plane, crtc, s);
    } else {
        i9xx_cursor_disable_sel_fetch_arm(io, plane, crtc);
    }
    if plane.cursor_base != base || plane.cursor_size != fbc || plane.cursor_cntl != ctl {
        if io.fbc_has(plane.display) {
            io.write(CursorReg::FbcControl(pipe), fbc, true);
        }
        io.write(CursorReg::Control(pipe), ctl, true);
        io.write(CursorReg::Position(pipe), pos, true);
        io.write(CursorReg::Base(pipe), base, true);
        plane.cursor_base = base;
        plane.cursor_size = fbc;
        plane.cursor_cntl = ctl;
    } else {
        io.write(CursorReg::Position(pipe), pos, true);
        io.write(CursorReg::Base(pipe), base, true);
    }
}
// upstream: intel_cursor.c i9xx_cursor_disable_arm()
pub fn i9xx_cursor_disable_arm(
    io: &mut impl CursorIo,
    plane: &mut CursorPlane,
    crtc: &CursorCrtcState,
) {
    i9xx_cursor_update_arm(io, plane, crtc, None)
}
// upstream: intel_cursor.c i9xx_cursor_get_hw_state()
pub fn i9xx_cursor_get_hw_state(
    io: &mut impl CursorIo,
    display: CursorDisplay,
    plane: &CursorPlane,
) -> Option<(bool, u8)> {
    let Some(w) = io.power_get_if_enabled(plane.pipe) else {
        return None;
    };
    let value = io.read(CursorReg::Control(plane.pipe));
    let enabled = value & MCURSOR_MODE_MASK != 0;
    let pipe = if display.display_ver >= 5 || display.g4x {
        plane.pipe
    } else {
        ((value >> 24) & 3) as u8
    };
    io.power_put(plane.pipe, w);
    Some((enabled, pipe))
}
// upstream: intel_cursor.c g4x_cursor_capture_error()
pub fn g4x_cursor_capture_error(io: &mut impl CursorIo, pipe: u8) -> CursorError {
    CursorError {
        ctl: io.read(CursorReg::Control(pipe)),
        surf: io.read(CursorReg::Base(pipe)),
        surflive: io.read(CursorReg::Base(pipe)),
    }
}
// upstream: intel_cursor.c i9xx_cursor_capture_error()
pub fn i9xx_cursor_capture_error(io: &mut impl CursorIo, pipe: u8) -> CursorError {
    CursorError {
        ctl: io.read(CursorReg::Control(pipe)),
        surf: io.read(CursorReg::Base(pipe)),
        surflive: 0,
    }
}
// upstream: intel_cursor.c intel_cursor_format_mod_supported()
pub fn intel_cursor_format_mod_supported(modifier_supported: bool, format: u32) -> bool {
    modifier_supported && format == DRM_FORMAT_ARGB8888
}
// upstream: intel_cursor.c intel_cursor_unpin_work()
pub fn intel_cursor_unpin_work(io: &mut impl CursorLifecycleIo, work: u64) {
    let (plane, state) = io.cursor_work_state(work);
    io.unpin_framebuffer(state);
    io.destroy_plane_state(plane, state);
}
pub trait CursorLifecycleIo {
    fn cursor_work_state(&mut self, work: u64) -> (u64, u64);
    fn unpin_framebuffer(&mut self, state: u64);
    fn destroy_plane_state(&mut self, plane: u64, state: u64);
    fn destroy_crtc_state(&mut self, state: u64);
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LegacyCursorUpdate {
    pub fb: u64,
    pub crtc: u64,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
}
pub trait LegacyCursorIo: CursorLifecycleIo {
    fn slow_path_required(&mut self, request: LegacyCursorUpdate) -> bool;
    fn outstanding_commit(&mut self) -> bool;
    fn only_fb_or_position_changed(&mut self, request: LegacyCursorUpdate) -> bool;
    fn ggtt_vma_changed(&mut self, old_state: u64, new_state: u64) -> bool;
    fn duplicate_plane_state(&mut self) -> Option<u64>;
    fn duplicate_crtc_state(&mut self) -> Option<u64>;
    fn set_fb(&mut self, state: u64, fb: u64);
    fn set_request(&mut self, state: u64, request: LegacyCursorUpdate);
    fn copy_uapi_to_hw(&mut self, state: u64, crtc: u64) -> i32;
    fn atomic_check(&mut self, old_crtc: u64, new_crtc: u64, old_plane: u64, new_plane: u64)
    -> i32;
    fn pin_fb(&mut self, new: u64, old: u64) -> i32;
    fn frontbuffer_flush(&mut self, state: u64);
    fn frontbuffer_track(&mut self, old: u64, new: u64);
    fn swap_plane_state(&mut self, new: u64);
    fn update_active_planes(&mut self, new_crtc: u64);
    fn vblank_evade_init(&mut self);
    fn psr_lock(&mut self);
    fn vblank_get(&mut self) -> bool;
    fn psr_wait_idle(&mut self);
    fn local_irq_disable(&mut self);
    fn vblank_evade(&mut self);
    fn vblank_put(&mut self);
    fn update_noarm(&mut self, state: u64);
    fn update_arm(&mut self, state: u64);
    fn disable_arm(&mut self);
    fn local_irq_enable(&mut self);
    fn psr_unlock(&mut self);
    fn schedule_unpin_after_vblank(&mut self, old: u64);
    fn unpin_fb(&mut self, state: u64);
    fn atomic_update_slow(&mut self, request: LegacyCursorUpdate) -> i32;
}
// upstream: intel_cursor.c intel_legacy_cursor_update()
pub fn intel_legacy_cursor_update(
    io: &mut impl LegacyCursorIo,
    request: LegacyCursorUpdate,
    old_plane: u64,
    old_crtc: u64,
) -> i32 {
    if io.slow_path_required(request)
        || io.outstanding_commit()
        || !io.only_fb_or_position_changed(request)
    {
        return io.atomic_update_slow(request);
    }
    let Some(new_plane) = io.duplicate_plane_state() else {
        return -12;
    };
    let Some(new_crtc) = io.duplicate_crtc_state() else {
        io.destroy_plane_state(0, new_plane);
        return -12;
    };
    io.set_fb(new_plane, request.fb);
    io.set_request(new_plane, request);
    let mut ret = io.copy_uapi_to_hw(new_plane, new_crtc);
    if ret == 0 {
        ret = io.atomic_check(old_crtc, new_crtc, old_plane, new_plane);
    }
    if ret == 0 {
        ret = io.pin_fb(new_plane, old_plane);
    }
    if ret != 0 {
        io.destroy_plane_state(0, new_plane);
        io.destroy_crtc_state(new_crtc);
        return ret;
    }
    io.frontbuffer_flush(new_plane);
    io.frontbuffer_track(old_plane, new_plane);
    io.swap_plane_state(new_plane);
    io.update_active_planes(new_crtc);
    io.vblank_evade_init();
    io.psr_lock();
    if io.vblank_get() {
        io.psr_wait_idle();
        io.local_irq_disable();
        io.vblank_evade();
        io.vblank_put();
    } else {
        io.local_irq_disable();
    }
    if request.width > 0 && request.height > 0 {
        io.update_noarm(new_plane);
        io.update_arm(new_plane);
    } else {
        io.disable_arm();
    }
    io.local_irq_enable();
    io.psr_unlock();
    if io.ggtt_vma_changed(old_plane, new_plane) {
        io.schedule_unpin_after_vblank(old_plane);
    } else {
        io.unpin_fb(old_plane);
    }
    io.destroy_crtc_state(new_crtc);
    0
}
pub trait CursorSizeHintIo {
    fn add_size_hints(&mut self, widths: &[(u32, u32)]);
    fn warn(&mut self);
}
// upstream: intel_cursor.c intel_cursor_add_size_hints_property()
pub fn intel_cursor_add_size_hints_property(
    io: &mut impl CursorSizeHintIo,
    max_width: u32,
    max_height: u32,
) {
    let max = max_width.min(max_height);
    let mut hints = [(0, 0); 4];
    let mut count = 0;
    let mut size = 64;
    while size <= max {
        if count >= hints.len() {
            io.warn();
            break;
        }
        hints[count] = (size, size);
        count += 1;
        size *= 2;
    }
    io.add_size_hints(&hints[..count]);
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorCreateRequest {
    pub display: CursorDisplay,
    pub pipe: u8,
    pub cursor_width: u32,
    pub cursor_height: u32,
    pub num_sprites: u8,
}
pub trait CursorCreateIo {
    fn allocate_plane(&mut self) -> bool;
    fn set_plane_callbacks(&mut self, plane: u8, callbacks: u8);
    fn modifiers(&mut self) -> Vec<u64>;
    fn plane_init(&mut self, formats: &[u32], modifiers: &[u64], plane_name: &str) -> i32;
    fn frontbuffer_bit(&self, pipe: u8, plane_id: u8) -> u32;
    fn free_modifiers(&mut self);
    fn rotation_property(&mut self);
    fn add_size_hints(&mut self);
    fn zpos_property(&mut self, zpos: u8);
    fn enable_damage_clips(&mut self);
    fn helper_add(&mut self);
    fn free_plane(&mut self);
}
// upstream: intel_cursor.c intel_cursor_plane_create()
pub fn intel_cursor_plane_create(
    io: &mut impl CursorCreateIo,
    request: CursorCreateRequest,
) -> Result<CursorPlane, CursorErrorCode> {
    if !io.allocate_plane() {
        return Err(CursorErrorCode::NoMemory);
    }
    let callbacks = if request.display.i845g || request.display.i865g {
        1
    } else {
        2
    };
    io.set_plane_callbacks(request.pipe, callbacks);
    let modifiers = io.modifiers();
    let ret = io.plane_init(&[DRM_FORMAT_ARGB8888], &modifiers, "cursor");
    io.free_modifiers();
    if ret != 0 {
        io.free_plane();
        return Err(CursorErrorCode::Invalid);
    }
    if request.display.display_ver >= 4 {
        io.rotation_property();
    }
    io.add_size_hints();
    io.zpos_property(request.num_sprites + 1);
    if request.display.display_ver >= 12 {
        io.enable_damage_clips();
    }
    io.helper_add();
    Ok(CursorPlane {
        pipe: request.pipe,
        plane_id: 7,
        display: request.display,
        frontbuffer_bit: io.frontbuffer_bit(request.pipe, 7),
        vtd_guard: if request.display.vtd_wa { 2 } else { 0 },
        cursor_base: u32::MAX,
        cursor_size: if request.display.i845g
            || request.display.i865g
            || request.display.has_cur_fbc
        {
            u32::MAX
        } else {
            0
        },
        cursor_cntl: u32::MAX,
    })
}
// upstream: intel_cursor.c intel_cursor_mode_config_init()
pub fn intel_cursor_mode_config_init(display: CursorDisplay) -> CursorModeConfig {
    if display.i845g {
        CursorModeConfig {
            cursor_width: 64,
            cursor_height: 1023,
        }
    } else if display.i865g {
        CursorModeConfig {
            cursor_width: 512,
            cursor_height: 1023,
        }
    } else if display.i830 || display.i85x || display.i915g || display.i915gm {
        CursorModeConfig {
            cursor_width: 64,
            cursor_height: 64,
        }
    } else {
        CursorModeConfig {
            cursor_width: 256,
            cursor_height: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psr_cursor_position_and_size_boundaries_match_source() {
        let crtc = CursorCrtcState {
            psr2_su_area: Rect {
                x1: 0,
                y1: 10,
                x2: 100,
                y2: 20,
            },
            ..CursorCrtcState::default()
        };
        let plane = CursorPlaneState {
            dst: Rect {
                x1: -3,
                y1: 2,
                x2: 61,
                y2: 66,
            },
            ..CursorPlaneState::default()
        };
        assert_eq!(
            intel_cursor_position(&crtc, &plane, false),
            CURSOR_POS_SIGN | 3 | (2 << 16)
        );
        assert_eq!(
            intel_cursor_position(&crtc, &plane, true),
            CURSOR_POS_SIGN | (1 << 31) | 3 | (8 << 16)
        );
        assert!(intel_cursor_size_ok(
            &plane,
            CursorModeConfig {
                cursor_width: 64,
                cursor_height: 64
            }
        ));
        assert!(!intel_cursor_size_ok(
            &plane,
            CursorModeConfig {
                cursor_width: 63,
                cursor_height: 64
            }
        ));
    }

    #[test]
    fn display12_cursor_configuration_and_watermark_encodings_follow_source() {
        assert_eq!(
            intel_cursor_mode_config_init(CursorDisplay {
                display_ver: 12,
                ..CursorDisplay::default()
            }),
            CursorModeConfig {
                cursor_width: 256,
                cursor_height: 256
            }
        );
        assert_eq!(i9xx_cursor_min_alignment(true), 64 * 1024);
        assert_eq!(i9xx_cursor_min_alignment(false), 4 * 1024);
        assert_eq!(
            skl_cursor_ddb_reg_val(DdbEntry { start: 16, end: 33 }),
            (32 << 16) | 16
        );
        assert_eq!(skl_cursor_ddb_reg_val(DdbEntry { start: 0, end: 0 }), 0);
        assert_eq!(
            skl_cursor_wm_reg_val(WmLevel {
                enable: true,
                ignore_lines: true,
                blocks: 7,
                lines: 9
            }),
            CUR_WM_EN | CUR_WM_IGNORE_LINES | 7 | (9 << 14)
        );
    }
}
