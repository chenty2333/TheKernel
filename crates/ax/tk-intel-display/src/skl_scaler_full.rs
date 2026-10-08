// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.

//! Source-ordered scaler translation from Linux 7.2.3 `skl_scaler.c`.
//! DRM atomic objects and display writes are explicit `ScalerIo` boundaries.

#![allow(dead_code, clippy::too_many_arguments)]

pub const SKL_CRTC_INDEX: u8 = 31;
pub const PS_PHASE_TRIP: u16 = 1;
pub const PS_PHASE_MASK: u16 = 0x7fff << 1;
pub const PS_SCALER_EN: u32 = 1 << 31;
pub const PS_BINDING_PIPE: u32 = 0;
pub const PS_BINDING_MASK: u32 = 7 << 25;
pub const PS_BINDING_PLANE_SHIFT: u32 = 25;
pub const PS_SCALER_MODE_NORMAL: u32 = 0;
pub const PS_SCALER_MODE_PLANAR: u32 = 1 << 29;
pub const PS_SCALER_MODE_HQ: u32 = 1 << 28;
pub const PS_SCALER_MODE_NV12: u32 = 2 << 28;
pub const PS_SCALER_MODE_DYN: u32 = 0;
pub const PS_FILTER_PROGRAMMED: u32 = 1 << 23;
pub const PS_FILTER_MEDIUM: u32 = 0;
pub const PS_COEF_INDEX_AUTO_INC: u32 = 1 << 10;
pub const FILTER_EN: u32 = 1 << 31;

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
    pub const fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Self { x1, y1, x2, y2 }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Format {
    pub yuv_semiplanar: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Scaler {
    pub in_use: bool,
    pub hscale: i32,
    pub vscale: i32,
    pub mode: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScalerState {
    pub scaler_users: u32,
    pub scaler_id: i32,
    pub scalers: [Scaler; 4],
}
impl Default for ScalerState {
    fn default() -> Self {
        Self {
            scaler_users: 0,
            scaler_id: -1,
            scalers: [Scaler::default(); 4],
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Casf {
    pub enable: bool,
    pub strength: u8,
    pub win_size: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Display {
    pub display_ver: u8,
    pub max_dotclk_freq: u32,
    pub has_casf: bool,
    pub wa_14011503117: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub display: Display,
    pub pipe: u8,
    pub crtc_id: u32,
    pub crtc_name: &'static str,
    pub enable: bool,
    pub active: bool,
    pub interlace: bool,
    pub pipe_src: Rect,
    pub pipe_mode_clock: u32,
    pub pipe_mode_width: i32,
    pub pipe_mode_height: i32,
    pub output_ycbcr420: bool,
    pub pfit_enabled: bool,
    pub pfit_casf: Casf,
    pub pfit_dst: Rect,
    pub scaling_filter_nearest: bool,
    pub scaler_state: ScalerState,
    pub num_scalers: usize,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneState {
    pub plane_index: u8,
    pub plane_id: u8,
    pub crtc_pipe: u8,
    pub visible: bool,
    pub has_fb: bool,
    pub format: Format,
    pub src: Rect,
    pub dst: Rect,
    pub scaler_id: i32,
    pub is_hdr_plane: bool,
    pub planar_linked_plane: Option<u8>,
    pub filter_nearest: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScalingFilter {
    Default,
    NearestNeighbor,
    Other(u32),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScalerError {
    Invalid,
    MissingPlane,
    Io,
}

/// DRM rectangle/state and DSB/MMIO operations used by the source functions.
pub trait ScalerIo {
    fn log(&mut self, _event: u32, _a: i64, _b: i64) {}
    fn hweight32(&self, value: u32) -> u32 {
        value.count_ones()
    }
    fn max_dst_size(&self, display: Display) -> (i32, i32) {
        skl_scaler_max_dst_size(display)
    }
    fn read(&mut self, _reg: u32) -> u32 {
        0
    }
    fn write_fw(&mut self, _reg: u32, _value: u32) {}
    fn write(&mut self, reg: u32, value: u32) {
        self.write_fw(reg, value)
    }
    fn write_dsb(&mut self, _reg: u32, _value: u32) {}
    fn trace_update(&mut self, _kind: u8, _pipe: u8, _id: i32, _x: i32, _y: i32, _w: i32, _h: i32) {
    }
    fn casf_compute(&mut self, _state: &mut CrtcState) -> i32 {
        0
    }
    fn casf_setup(&mut self, _state: &CrtcState) {}
    fn casf_get_config(&mut self, _state: &mut CrtcState) {}
    fn plane_for_index(&mut self, _index: u8) -> Option<PlaneState> {
        None
    }
    fn update_plane_state(&mut self, _plane: PlaneState) {}
    fn display_err_fatal_mask(&self) -> u32 {
        0x44484
    }
}

const fn pipe_offset(pipe: u8) -> u32 {
    pipe as u32 * 0x800
}
const fn ps_y_phase(value: u16) -> u32 {
    (value as u32) << 16
}
const fn ps_uv_rgb_phase(value: u16) -> u32 {
    value as u32
}
const fn ps_win_xpos(value: i32) -> u32 {
    (value as u32 & 0xffff) << 16
}
const fn ps_win_ypos(value: i32) -> u32 {
    value as u32 & 0xffff
}
const fn ps_win_xsize(value: i32) -> u32 {
    (value as u32 & 0xffff) << 16
}
const fn ps_win_ysize(value: i32) -> u32 {
    value as u32 & 0xffff
}
const fn ps_binding_plane(id: u8) -> u32 {
    ((id + 1) as u32 & 7) << PS_BINDING_PLANE_SHIFT
}
const fn ps_binding_y_plane(id: u8) -> u32 {
    ((id + 1) as u32 & 7) << 5
}
const fn ps_y_vert_filter_select(v: u32) -> u32 {
    (v & 1) << 4
}
const fn ps_y_horz_filter_select(v: u32) -> u32 {
    (v & 1) << 3
}
const fn ps_uv_vert_filter_select(v: u32) -> u32 {
    (v & 1) << 2
}
const fn ps_uv_horz_filter_select(v: u32) -> u32 {
    (v & 1) << 1
}
const fn ps_coef_index_set(pipe: u8, id: i32, set: i32) -> u32 {
    0x68198 + pipe_offset(pipe) + id as u32 * 0x100 + set as u32 * 8
}
const fn ps_coef_data_set(pipe: u8, id: i32, set: i32) -> u32 {
    ps_coef_index_set(pipe, id, set) + 4
}
const fn ps_ctrl_reg(pipe: u8, id: i32) -> u32 {
    0x68180 + pipe_offset(pipe) + id as u32 * 0x100
}
const fn ps_vphase(pipe: u8, id: i32) -> u32 {
    ps_ctrl_reg(pipe, id) + 8
}
const fn ps_hphase(pipe: u8, id: i32) -> u32 {
    ps_ctrl_reg(pipe, id) + 0x14
}
const fn ps_win_pos(pipe: u8, id: i32) -> u32 {
    ps_ctrl_reg(pipe, id) - 0x10
}
const fn ps_win_sz(pipe: u8, id: i32) -> u32 {
    ps_ctrl_reg(pipe, id) - 0xc
}
const fn sharpness_ctl(pipe: u8) -> u32 {
    0x682b0 + pipe_offset(pipe)
}
const fn ps_ecc_stat(pipe: u8, id: i32) -> u32 {
    ps_ctrl_reg(pipe, id) + 0x50
}

// upstream: skl_scaler.c skl_scaler_calc_phase()
pub fn skl_scaler_calc_phase(sub: i32, scale: i32, chroma_cosited: bool) -> u16 {
    let mut phase = -0x8000;
    let mut trip = 0;
    if chroma_cosited {
        phase += (sub - 1) * 0x8000 / sub;
    }
    phase += scale / (2 * sub);
    debug_assert!((-0x8000..=0x18000).contains(&phase));
    if phase < 0 {
        phase = 0x10000 + phase;
    } else {
        trip = PS_PHASE_TRIP;
    }
    ((phase >> 2) as u16 & PS_PHASE_MASK) | trip
}
// upstream: skl_scaler.c skl_scaler_min_src_size()
pub fn skl_scaler_min_src_size(format: Option<Format>, _modifier: u64) -> (i32, i32) {
    if format.is_some_and(|f| f.yuv_semiplanar) {
        (16, 16)
    } else {
        (8, 8)
    }
}
// upstream: skl_scaler.c skl_scaler_max_src_size()
pub fn skl_scaler_max_src_size(display: Display) -> (i32, i32) {
    if display.display_ver >= 14 {
        (4096, 8192)
    } else if display.display_ver >= 12 {
        (5120, 8192)
    } else if display.display_ver == 11 {
        (5120, 4096)
    } else {
        (4096, 4096)
    }
}
// upstream: skl_scaler.c skl_scaler_min_dst_size()
pub const fn skl_scaler_min_dst_size() -> (i32, i32) {
    (8, 8)
}
// upstream: skl_scaler.c skl_scaler_max_dst_size()
pub fn skl_scaler_max_dst_size(display: Display) -> (i32, i32) {
    if display.display_ver >= 12 {
        (8192, 8192)
    } else if display.display_ver == 11 {
        (5120, 4096)
    } else {
        (4096, 4096)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeStatus {
    Ok,
    No420,
    Bad,
}
// upstream: skl_scaler.c skl_scaler_mode_valid()
pub fn skl_scaler_mode_valid(
    display: Display,
    hdisplay: i32,
    output_ycbcr420: bool,
    joined_pipes: i32,
) -> ModeStatus {
    if joined_pipes < 2 && output_ycbcr420 {
        let (max_w, max_h) = skl_scaler_max_src_size(display);
        let _ = max_h;
        if hdisplay > max_h {
            return ModeStatus::No420;
        }
        let _ = max_w;
    }
    ModeStatus::Ok
}
// upstream: skl_scaler.c skl_update_scaler()
pub fn skl_update_scaler(
    state: &mut CrtcState,
    force_detach: bool,
    user: u8,
    scaler_id: &mut i32,
    src_w: i32,
    src_h: i32,
    dst_w: i32,
    dst_h: i32,
    format: Option<Format>,
    modifier: u64,
    mut need_scaler: bool,
    io: &mut impl ScalerIo,
) -> Result<(), ScalerError> {
    if src_w != dst_w || src_h != dst_h {
        need_scaler = true;
    }
    if state.display.display_ver >= 9 && state.enable && need_scaler && state.interlace {
        io.log(1, state.crtc_id as i64, user as i64);
        return Err(ScalerError::Invalid);
    }
    if force_detach || !need_scaler {
        if *scaler_id >= 0 {
            state.scaler_state.scaler_users &= !(1 << user);
            state.scaler_state.scalers[*scaler_id as usize].in_use = false;
            io.log(2, *scaler_id as i64, state.scaler_state.scaler_users as i64);
            *scaler_id = -1;
        }
        return Ok(());
    }
    let (min_src_w, min_src_h) = skl_scaler_min_src_size(format, modifier);
    let (max_src_w, max_src_h) = skl_scaler_max_src_size(state.display);
    let (min_dst_w, min_dst_h) = skl_scaler_min_dst_size();
    let (max_dst_w, max_dst_h) = io.max_dst_size(state.display);
    if src_w < min_src_w
        || src_h < min_src_h
        || dst_w < min_dst_w
        || dst_h < min_dst_h
        || src_w > max_src_w
        || src_h > max_src_h
        || dst_w > max_dst_w
        || dst_h > max_dst_h
    {
        io.log(3, src_w as i64, dst_w as i64);
        return Err(ScalerError::Invalid);
    }
    if state.pipe_src.width() > max_dst_w || state.pipe_src.height() > max_dst_h {
        io.log(
            4,
            state.pipe_src.width() as i64,
            state.pipe_src.height() as i64,
        );
        return Err(ScalerError::Invalid);
    }
    state.scaler_state.scaler_users |= 1 << user;
    io.log(5, src_w as i64, dst_w as i64);
    Ok(())
}
// upstream: skl_scaler.c skl_update_scaler_crtc()
pub fn skl_update_scaler_crtc(
    state: &mut CrtcState,
    io: &mut impl ScalerIo,
) -> Result<(), ScalerError> {
    if io.casf_compute(state) != 0 {
        return Err(ScalerError::Invalid);
    }
    let (width, height) = if state.pfit_enabled {
        (state.pfit_dst.width(), state.pfit_dst.height())
    } else {
        (state.pipe_mode_width, state.pipe_mode_height)
    };
    let mut id = state.scaler_state.scaler_id;
    let result = skl_update_scaler(
        state,
        !state.active,
        SKL_CRTC_INDEX,
        &mut id,
        state.pipe_src.width(),
        state.pipe_src.height(),
        width,
        height,
        None,
        0,
        state.pfit_enabled,
        io,
    );
    state.scaler_state.scaler_id = id;
    result
}
// upstream: skl_scaler.c skl_update_scaler_plane()
pub fn skl_update_scaler_plane(
    state: &mut CrtcState,
    plane: &mut PlaneState,
    io: &mut impl ScalerIo,
) -> Result<(), ScalerError> {
    let force_detach = !plane.has_fb || !plane.visible;
    let need_scaler = !plane.is_hdr_plane && plane.has_fb && plane.format.yuv_semiplanar;
    skl_update_scaler(
        state,
        force_detach,
        plane.plane_index,
        &mut plane.scaler_id,
        plane.src.width() >> 16,
        plane.src.height() >> 16,
        plane.dst.width(),
        plane.dst.height(),
        plane.has_fb.then_some(plane.format),
        0,
        need_scaler,
        io,
    )
}
// upstream: skl_scaler.c scaler_has_casf()
pub const fn scaler_has_casf(display: Display, id: i32) -> bool {
    display.has_casf && id == 1
}
// upstream: skl_scaler.c intel_allocate_scaler()
pub fn intel_allocate_scaler(
    state: &mut ScalerState,
    num_scalers: usize,
    display: Display,
    casf: bool,
) -> i32 {
    for id in 0..num_scalers.min(state.scalers.len()) {
        if !state.scalers[id].in_use && (!casf || scaler_has_casf(display, id as i32)) {
            state.scalers[id].in_use = true;
            return id as i32;
        }
    }
    -1
}
// upstream: skl_scaler.c calculate_max_scale()
pub fn calculate_max_scale(display: Display, is_yuv: bool, id: i32) -> (i32, i32) {
    if display.display_ver >= 14 {
        (0x30000 - 1, if id == 0 { 0x30000 - 1 } else { 0x10000 })
    } else if display.display_ver >= 10 || !is_yuv {
        (0x30000 - 1, 0x30000 - 1)
    } else {
        (0x20000 - 1, 0x20000 - 1)
    }
}
fn scale_factor(src: i32, dst: i32, min: i32, max: i32) -> i32 {
    if src <= 0 || dst <= 0 {
        return -22;
    }
    let v = ((src as i64) << 16) / dst as i64;
    if v < min as i64 || v > max as i64 {
        -22
    } else {
        v as i32
    }
}
// upstream: skl_scaler.c intel_atomic_setup_scaler()
pub fn intel_atomic_setup_scaler(
    state: &mut CrtcState,
    num_need: u32,
    num_scalers: usize,
    name: &str,
    idx: u32,
    plane: Option<&PlaneState>,
    scaler_id: &mut i32,
    casf: bool,
) -> Result<(), ScalerError> {
    if *scaler_id < 0 {
        *scaler_id =
            intel_allocate_scaler(&mut state.scaler_state, num_scalers, state.display, casf);
    }
    if *scaler_id < 0 {
        return Err(ScalerError::Invalid);
    }
    let id = *scaler_id as usize;
    let mut mode;
    if plane.is_some_and(|p| p.has_fb && p.format.yuv_semiplanar) {
        mode = if state.display.display_ver == 9 {
            PS_SCALER_MODE_NV12
        } else if plane.unwrap().is_hdr_plane {
            PS_SCALER_MODE_NORMAL
        } else {
            PS_SCALER_MODE_PLANAR
        };
        if let Some(linked) = plane.and_then(|p| p.planar_linked_plane) {
            mode |= ps_binding_y_plane(linked);
        }
    } else if state.display.display_ver >= 10 {
        mode = PS_SCALER_MODE_NORMAL;
    } else if num_need == 1 && num_scalers > 1 {
        state.scaler_state.scalers[id].in_use = false;
        *scaler_id = 0;
        state.scaler_state.scalers[0].in_use = true;
        mode = PS_SCALER_MODE_HQ;
    } else {
        mode = PS_SCALER_MODE_DYN;
    }
    let (mut hs, mut vs) = (0, 0);
    if let Some(p) = plane.filter(|p| p.has_fb) {
        let (max_h, max_v) =
            calculate_max_scale(state.display, p.format.yuv_semiplanar, *scaler_id);
        hs = scale_factor(p.src.width(), p.dst.width(), 1, max_h);
        vs = scale_factor(p.src.height(), p.dst.height(), 1, max_v);
        if hs < 0 || vs < 0 {
            return Err(ScalerError::Invalid);
        }
    }
    if state.pfit_enabled {
        let (max_h, mut max_v) = calculate_max_scale(state.display, false, *scaler_id);
        let mut max_h = max_h;
        if state.output_ycbcr420 {
            max_h = 0x18000 - 1;
            max_v = 0x10000;
        }
        let a = scale_factor(
            state.pipe_src.width() << 16,
            state.pfit_dst.width(),
            0,
            max_h,
        );
        let b = scale_factor(
            state.pipe_src.height() << 16,
            state.pfit_dst.height(),
            0,
            max_v,
        );
        if a < 0 || b < 0 {
            return Err(ScalerError::Invalid);
        }
        hs = a;
        vs = b;
    }
    state.scaler_state.scalers[id].hscale = hs;
    state.scaler_state.scalers[id].vscale = vs;
    state.scaler_state.scalers[id].mode = mode;
    let _ = (name, idx);
    Ok(())
}
// upstream: skl_scaler.c setup_crtc_scaler()
pub fn setup_crtc_scaler(state: &mut CrtcState, io: &mut impl ScalerIo) -> Result<(), ScalerError> {
    let users = io.hweight32(state.scaler_state.scaler_users);
    let mut id = state.scaler_state.scaler_id;
    intel_atomic_setup_scaler(
        state,
        users,
        state.num_scalers,
        "CRTC",
        state.crtc_id,
        None,
        &mut id,
        state.pfit_casf.enable,
    )?;
    state.scaler_state.scaler_id = id;
    Ok(())
}
// upstream: skl_scaler.c setup_plane_scaler()
pub fn setup_plane_scaler(
    state: &mut CrtcState,
    plane: &mut PlaneState,
    io: &mut impl ScalerIo,
) -> Result<(), ScalerError> {
    if plane.crtc_pipe != state.pipe {
        io.log(6, plane.crtc_pipe as i64, state.pipe as i64);
        return Ok(());
    }
    let users = io.hweight32(state.scaler_state.scaler_users);
    let mut id = plane.scaler_id;
    intel_atomic_setup_scaler(
        state,
        users,
        state.num_scalers,
        "PLANE",
        plane.plane_id as u32,
        Some(plane),
        &mut id,
        false,
    )?;
    plane.scaler_id = id;
    Ok(())
}
// upstream: skl_scaler.c intel_atomic_setup_scalers()
pub fn intel_atomic_setup_scalers(
    state: &mut CrtcState,
    planes: &mut [PlaneState],
    io: &mut impl ScalerIo,
) -> Result<(), ScalerError> {
    let need = io.hweight32(state.scaler_state.scaler_users);
    if need > state.num_scalers as u32 {
        return Err(ScalerError::Invalid);
    }
    for i in 0..32 {
        if state.scaler_state.scaler_users & (1 << i) == 0 {
            continue;
        }
        if i == u32::from(SKL_CRTC_INDEX) {
            setup_crtc_scaler(state, io)?;
        } else {
            let plane = planes
                .iter_mut()
                .find(|p| u32::from(p.plane_index) == i)
                .ok_or(ScalerError::MissingPlane)?;
            setup_plane_scaler(state, plane, io)?;
        }
    }
    Ok(())
}
// upstream: skl_scaler.c glk_coef_tap()
pub const fn glk_coef_tap(i: i32) -> i32 {
    i % 7
}
// upstream: skl_scaler.c glk_nearest_filter_coef()
pub const fn glk_nearest_filter_coef(tap: i32) -> u16 {
    if tap == 3 { 0x0800 } else { 0x3000 }
}
// upstream: skl_scaler.c glk_program_nearest_filter_coefs()
pub fn glk_program_nearest_filter_coefs(io: &mut impl ScalerIo, pipe: u8, id: i32, set: i32) {
    io.write_dsb(ps_coef_index_set(pipe, id, set), PS_COEF_INDEX_AUTO_INC);
    for i in (0..17 * 7).step_by(2) {
        let value = u32::from(glk_nearest_filter_coef(glk_coef_tap(i)))
            | u32::from(glk_nearest_filter_coef(glk_coef_tap(i + 1))) << 16;
        io.write_dsb(ps_coef_data_set(pipe, id, set), value);
    }
    io.write_dsb(ps_coef_index_set(pipe, id, set), 0);
}
// upstream: skl_scaler.c skl_scaler_get_filter_select()
pub const fn skl_scaler_get_filter_select(filter: ScalingFilter, casf: bool) -> u32 {
    if matches!(filter, ScalingFilter::NearestNeighbor) || casf {
        PS_FILTER_PROGRAMMED
            | ps_y_vert_filter_select(0)
            | ps_y_horz_filter_select(0)
            | ps_uv_vert_filter_select(0)
            | ps_uv_horz_filter_select(0)
    } else {
        PS_FILTER_MEDIUM
    }
}
// upstream: skl_scaler.c skl_scaler_setup_filter()
pub fn skl_scaler_setup_filter(
    io: &mut impl ScalerIo,
    pipe: u8,
    id: i32,
    set: i32,
    filter: ScalingFilter,
) {
    match filter {
        ScalingFilter::Default => {}
        ScalingFilter::NearestNeighbor => glk_program_nearest_filter_coefs(io, pipe, id, set),
        ScalingFilter::Other(_) => io.log(7, pipe as i64, id as i64),
    }
}
// upstream: skl_scaler.c casf_sharpness_ctl()
pub const fn casf_sharpness_ctl(casf: Casf) -> u32 {
    if !casf.enable {
        0
    } else {
        FILTER_EN | ((casf.strength as u32) << 8) | casf.win_size
    }
}
// upstream: skl_scaler.c skl_pfit_enable()
pub fn skl_pfit_enable(io: &mut impl ScalerIo, state: &CrtcState) {
    if !state.pfit_enabled || state.scaler_state.scaler_id < 0 {
        return;
    }
    if state.display.wa_14011503117 {
        adl_scaler_ecc_mask(io, state);
    }
    let h = scale_factor(
        state.pipe_src.width() << 16,
        state.pfit_dst.width(),
        0,
        i32::MAX,
    );
    let v = scale_factor(
        state.pipe_src.height() << 16,
        state.pfit_dst.height(),
        0,
        i32::MAX,
    );
    let hp = skl_scaler_calc_phase(1, h, false);
    let vp = skl_scaler_calc_phase(1, v, false);
    let id = state.scaler_state.scaler_id;
    let scaler = &state.scaler_state.scalers[id as usize];
    let ctrl = PS_SCALER_EN
        | PS_BINDING_PIPE
        | scaler.mode
        | skl_scaler_get_filter_select(
            if state.scaling_filter_nearest {
                ScalingFilter::NearestNeighbor
            } else {
                ScalingFilter::Default
            },
            state.pfit_casf.enable,
        );
    io.trace_update(
        0,
        state.pipe,
        id,
        state.pfit_dst.x1,
        state.pfit_dst.y1,
        state.pfit_dst.width(),
        state.pfit_dst.height(),
    );
    if state.pfit_casf.enable {
        io.casf_setup(state);
    } else {
        skl_scaler_setup_filter(
            io,
            state.pipe,
            id,
            0,
            if state.scaling_filter_nearest {
                ScalingFilter::NearestNeighbor
            } else {
                ScalingFilter::Default
            },
        );
    }
    if scaler_has_casf(state.display, id) {
        io.write(
            sharpness_ctl(state.pipe),
            casf_sharpness_ctl(state.pfit_casf),
        );
    }
    io.write(ps_ctrl_reg(state.pipe, id), ctrl);
    io.write(
        ps_vphase(state.pipe, id),
        ps_y_phase(0) | ps_uv_rgb_phase(vp),
    );
    io.write(
        ps_hphase(state.pipe, id),
        ps_y_phase(0) | ps_uv_rgb_phase(hp),
    );
    io.write(
        ps_win_pos(state.pipe, id),
        ps_win_xpos(state.pfit_dst.x1) | ps_win_ypos(state.pfit_dst.y1),
    );
    io.write(
        ps_win_sz(state.pipe, id),
        ps_win_xsize(state.pfit_dst.width()) | ps_win_ysize(state.pfit_dst.height()),
    );
}
// upstream: skl_scaler.c skl_pipe_scaler_get_hw_state()
pub fn skl_pipe_scaler_get_hw_state(io: &mut impl ScalerIo, pipe: u8, num_scalers: usize) -> i32 {
    for id in 0..num_scalers {
        let ctl = io.read(ps_ctrl_reg(pipe, id as i32));
        if ctl & (PS_SCALER_EN | PS_BINDING_MASK) == (PS_SCALER_EN | PS_BINDING_PIPE) {
            return id as i32;
        }
    }
    -1
}
// upstream: skl_scaler.c skl_program_plane_scaler()
pub fn skl_program_plane_scaler(io: &mut impl ScalerIo, plane: &PlaneState, state: &CrtcState) {
    let id = plane.scaler_id;
    let scaler = &state.scaler_state.scalers[id as usize];
    let hs = scale_factor(plane.src.width(), plane.dst.width(), 0, i32::MAX);
    let vs = scale_factor(plane.src.height(), plane.dst.height(), 0, i32::MAX);
    let (yh, yv, uh, uv) = if plane.format.yuv_semiplanar && !plane.is_hdr_plane {
        (
            skl_scaler_calc_phase(1, hs, false),
            skl_scaler_calc_phase(1, vs, false),
            skl_scaler_calc_phase(2, hs, true),
            skl_scaler_calc_phase(2, vs, false),
        )
    } else {
        (
            0,
            0,
            skl_scaler_calc_phase(1, hs, false),
            skl_scaler_calc_phase(1, vs, false),
        )
    };
    let filter = if plane.filter_nearest {
        ScalingFilter::NearestNeighbor
    } else {
        ScalingFilter::Default
    };
    let ctrl = PS_SCALER_EN
        | ps_binding_plane(plane.plane_id)
        | scaler.mode
        | skl_scaler_get_filter_select(filter, false);
    io.trace_update(
        1,
        state.pipe,
        id,
        plane.dst.x1,
        plane.dst.y1,
        plane.dst.width(),
        plane.dst.height(),
    );
    skl_scaler_setup_filter(io, state.pipe, id, 0, filter);
    io.write_dsb(ps_ctrl_reg(state.pipe, id), ctrl);
    io.write_dsb(
        ps_vphase(state.pipe, id),
        ps_y_phase(yv) | ps_uv_rgb_phase(uv),
    );
    io.write_dsb(
        ps_hphase(state.pipe, id),
        ps_y_phase(yh) | ps_uv_rgb_phase(uh),
    );
    io.write_dsb(
        ps_win_pos(state.pipe, id),
        ps_win_xpos(plane.dst.x1) | ps_win_ypos(plane.dst.y1),
    );
    io.write_dsb(
        ps_win_sz(state.pipe, id),
        ps_win_xsize(plane.dst.width()) | ps_win_ysize(plane.dst.height()),
    );
}
// upstream: skl_scaler.c skl_detach_scaler()
pub fn skl_detach_scaler(io: &mut impl ScalerIo, display: Display, pipe: u8, id: i32) {
    io.trace_update(2, pipe, id, 0, 0, 0, 0);
    if scaler_has_casf(display, id) {
        io.write_dsb(sharpness_ctl(pipe), 0);
    }
    io.write_dsb(ps_ctrl_reg(pipe, id), 0);
    io.write_dsb(ps_win_pos(pipe, id), 0);
    io.write_dsb(ps_win_sz(pipe, id), 0);
}
// upstream: skl_scaler.c skl_detach_scalers()
pub fn skl_detach_scalers(io: &mut impl ScalerIo, state: &CrtcState) {
    for id in 0..state.num_scalers {
        if !state.scaler_state.scalers[id].in_use {
            skl_detach_scaler(io, state.display, state.pipe, id as i32);
        }
    }
}
// upstream: skl_scaler.c skl_scaler_disable()
pub fn skl_scaler_disable(io: &mut impl ScalerIo, state: &CrtcState) {
    for id in 0..state.num_scalers {
        skl_detach_scaler(io, state.display, state.pipe, id as i32);
    }
}
// upstream: skl_scaler.c skl_scaler_get_config()
pub fn skl_scaler_get_config(io: &mut impl ScalerIo, state: &mut CrtcState) {
    let id = skl_pipe_scaler_get_hw_state(io, state.pipe, state.num_scalers);
    if id < 0 {
        return;
    }
    if scaler_has_casf(state.display, id) {
        io.casf_get_config(state);
    }
    state.pfit_enabled = true;
    let pos = io.read(ps_win_pos(state.pipe, id));
    let size = io.read(ps_win_sz(state.pipe, id));
    let x = ((pos >> 16) & 0xffff) as i32;
    let y = (pos & 0xffff) as i32;
    let w = ((size >> 16) & 0xffff) as i32;
    let h = (size & 0xffff) as i32;
    state.pfit_dst = Rect::new(x, y, x + w, y + h);
    state.scaler_state.scalers[id as usize].in_use = true;
    state.scaler_state.scaler_id = id;
    state.scaler_state.scaler_users |= 1 << SKL_CRTC_INDEX;
}
// upstream: skl_scaler.c adl_scaler_ecc_mask()
pub fn adl_scaler_ecc_mask(io: &mut impl ScalerIo, state: &CrtcState) {
    if state.pfit_enabled {
        io.write(io.display_err_fatal_mask(), u32::MAX);
    }
}
// upstream: skl_scaler.c adl_scaler_ecc_unmask()
pub fn adl_scaler_ecc_unmask(io: &mut impl ScalerIo, state: &CrtcState) {
    let id = state.scaler_state.scaler_id;
    if id < 0 {
        return;
    }
    io.write_fw(ps_ecc_stat(state.pipe, id), 1);
    io.write(io.display_err_fatal_mask(), 0);
}
// upstream: skl_scaler.c skl_scaler_1st_prefill_adjustment()
pub const fn skl_scaler_1st_prefill_adjustment(_state: &CrtcState) -> u32 {
    0x10000
}
// upstream: skl_scaler.c skl_scaler_2nd_prefill_adjustment()
pub const fn skl_scaler_2nd_prefill_adjustment(_state: &CrtcState) -> u32 {
    0x10000
}
// upstream: skl_scaler.c skl_scaler_1st_prefill_lines()
pub fn skl_scaler_1st_prefill_lines(state: &CrtcState) -> u32 {
    if state.scaler_state.scaler_users.count_ones() > 0 {
        4 << 16
    } else {
        0
    }
}
// upstream: skl_scaler.c skl_scaler_2nd_prefill_lines()
pub fn skl_scaler_2nd_prefill_lines(state: &CrtcState) -> u32 {
    if state.scaler_state.scaler_users.count_ones() > 1 && state.pfit_enabled {
        4 << 16
    } else {
        0
    }
}
// upstream: skl_scaler.c _skl_scaler_max_scale()
pub fn _skl_scaler_max_scale(state: &CrtcState, max: u32) -> u32 {
    if state.pipe_mode_clock == 0 {
        return 0;
    }
    max.min(
        ((u64::from(state.display.max_dotclk_freq) << 16)
            .div_ceil(u64::from(state.pipe_mode_clock))) as u32,
    )
}
// upstream: skl_scaler.c skl_scaler_max_total_scale()
pub fn skl_scaler_max_total_scale(state: &CrtcState) -> u32 {
    if state.num_scalers < 1 {
        return 0x10000;
    }
    let mut max = 9 << 16;
    if state.num_scalers > 1 {
        max *= 9;
    }
    _skl_scaler_max_scale(state, max)
}
// upstream: skl_scaler.c skl_scaler_max_hscale()
pub fn skl_scaler_max_hscale(state: &CrtcState) -> u32 {
    if state.num_scalers < 1 {
        return 0x10000;
    }
    _skl_scaler_max_scale(state, 3 << 16)
}
// upstream: skl_scaler.c skl_scaler_max_scale()
pub fn skl_scaler_max_scale(state: &CrtcState) -> u32 {
    if state.num_scalers < 1 {
        return 0x10000;
    }
    _skl_scaler_max_scale(state, 9 << 16)
}
// upstream: skl_scaler.c skl_scaler_1st_prefill_adjustment_worst()
pub fn skl_scaler_1st_prefill_adjustment_worst(state: &CrtcState) -> u32 {
    if state.num_scalers > 0 {
        skl_scaler_max_scale(state)
    } else {
        0x10000
    }
}
// upstream: skl_scaler.c skl_scaler_2nd_prefill_adjustment_worst()
pub fn skl_scaler_2nd_prefill_adjustment_worst(state: &CrtcState) -> u32 {
    if state.num_scalers > 1 {
        skl_scaler_max_scale(state)
    } else {
        0x10000
    }
}
// upstream: skl_scaler.c skl_scaler_1st_prefill_lines_worst()
pub fn skl_scaler_1st_prefill_lines_worst(state: &CrtcState) -> u32 {
    if state.num_scalers > 0 { 4 << 16 } else { 0 }
}
// upstream: skl_scaler.c skl_scaler_2nd_prefill_lines_worst()
pub fn skl_scaler_2nd_prefill_lines_worst(state: &CrtcState) -> u32 {
    if state.num_scalers > 1 { 4 << 16 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phases_and_scale_limits_follow_gen12_rules() {
        assert_eq!(skl_scaler_calc_phase(1, 0, false), 0x2000);
        assert_eq!(
            skl_scaler_max_src_size(Display {
                display_ver: 12,
                ..Display::default()
            }),
            (5120, 8192)
        );
        assert_eq!(
            skl_scaler_max_dst_size(Display {
                display_ver: 12,
                ..Display::default()
            }),
            (8192, 8192)
        );
    }
}
