// SPDX-License-Identifier: MIT
// Copyright © 2022-2023 Intel Corporation.
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_vblank.c translation.
// Register, clock, IRQ, waitqueue, VRR and DRM timestamp interfaces are traits.

#![allow(dead_code, clippy::too_many_arguments)]

pub const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
pub const I915_MODE_FLAG_GET_SCANLINE_FROM_TIMESTAMP: u32 = 1 << 0;
pub const I915_MODE_FLAG_USE_SCANLINE_COUNTER: u32 = 1 << 1;
pub const I915_MODE_FLAG_VRR: u32 = 1 << 2;
pub const I915_OUTPUT_HDMI: u32 = 1 << 0;
pub const I915_OUTPUT_DSI: u32 = 1 << 1;
pub const VBLANK_EVASION_TIME_US: u32 = 100;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Display {
    pub display_ver: u8,
    pub g4x: bool,
    pub ddi: bool,
    pub broadwell: bool,
    pub haswell: bool,
    pub valleyview: bool,
    pub cherryview: bool,
    pub battlemage: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Mode {
    pub flags: u32,
    pub clock: u32,
    pub crtc_clock: u32,
    pub htotal: i32,
    pub hsync_start: i32,
    pub vdisplay: i32,
    pub vblank_start: i32,
    pub vblank_end: i32,
    pub vtotal: i32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Vrr {
    pub enable: bool,
    pub guardband: i32,
    pub vmax_vtotal: i32,
    pub dc_balance_enable: bool,
    pub is_push_sent: bool,
    pub vmin_vblank_start: i32,
    pub vmax_vblank_start: i32,
    pub dcb_vmin_next: i32,
    pub dcb_vmax_next: i32,
    pub dcb_vmin_final: i32,
    pub dcb_vmax_final: i32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub display: Display,
    pub pipe: u8,
    pub output_types: u32,
    pub adjusted_mode: Mode,
    pub mode_flags: u32,
    pub scanline_offset: i32,
    pub vmax_vblank_start: i32,
    pub vrr: Vrr,
    pub update_m_n: bool,
    pub update_lrr: bool,
    pub color_uses_dsb: bool,
    pub set_context_latency: i32,
    pub needs_modeset: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Crtc {
    pub display: Display,
    pub pipe: u8,
    pub active: bool,
    pub mode_flags: u32,
    pub scanline_offset: i32,
    pub vmax_vblank_start: i32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VblankCrtc {
    pub hwmode: Mode,
    pub max_vblank_count: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VblankEvade {
    pub pipe: u8,
    pub need_vlv_dsi_wa: bool,
    pub vblank_start: i32,
    pub min: i32,
    pub max: i32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanoutPosition {
    pub vpos: i32,
    pub hpos: i32,
}

/// i915 display/DRM operations required by scanline and vblank algorithms.
pub trait VblankIo {
    fn read32(&mut self, _offset: u32) -> u32 {
        0
    }
    fn read32_fw(&mut self, offset: u32) -> u32 {
        self.read32(offset)
    }
    fn read64_frame_pixel(&mut self, _pipe: u8) -> u64 {
        0
    }
    fn read_timestamp(&mut self) -> u32 {
        0
    }
    fn now_ns(&mut self) -> u64 {
        0
    }
    fn now_us(&mut self) -> u64 {
        self.now_ns() / 1000
    }
    fn now_ms(&mut self) -> u64 {
        self.now_us() / 1000
    }
    fn delay_us(&mut self, _us: u32) {}
    fn sleep_ms(&mut self, _ms: u32) {}
    fn local_irq_save(&mut self) -> u64 {
        0
    }
    fn local_irq_disable(&mut self) {}
    fn local_irq_enable(&mut self) {}
    fn local_irq_restore(&mut self, _flags: u64) {}
    fn vblank_section_enter(&mut self) {}
    fn vblank_section_exit(&mut self) {}
    fn warn(&mut self, _code: u32, _value: i64) {}
    fn debug(&mut self, _code: u32, _value: i64) {}
    fn drm_vblank_timestamp(
        &mut self,
        _max_error: &mut i32,
        _vblank_time: &mut u64,
        _in_irq: bool,
    ) -> bool {
        false
    }
    fn scanline_wait_prepare(&mut self) {}
    fn scanline_wait_finish(&mut self) {}
    fn schedule_timeout_ms(&mut self, _timeout_ms: u32) {}
    fn calculate_timestamping_constants(&mut self, _mode: Mode) {}
    fn vblank_time_lock(&mut self) {}
    fn vblank_time_unlock(&mut self) {}
    fn hsync_window_pixel_bias(&self, mode: &Mode) -> i32 {
        mode.htotal - mode.hsync_start
    }
    fn use_scanline_counter(&self, display: Display, crtc: &Crtc) -> bool {
        display.display_ver >= 5
            || display.g4x
            || display.display_ver == 2
            || crtc.mode_flags & I915_MODE_FLAG_USE_SCANLINE_COUNTER != 0
    }
    fn pipe_frame_pixel(&self, pipe: u8) -> u32 {
        0x70040 + u32::from(pipe) * 0x1000
    }
    fn pipe_frame_pixel_reg(&self, pipe: u8) -> u32 {
        self.pipe_frame_pixel(pipe)
    }
    fn frame_timestamp_reg(&self, pipe: u8) -> u32 {
        0x7004c + u32::from(pipe) * 0x1000
    }
    fn pipe_dsl_reg(&self, pipe: u8) -> u32 {
        0x70000 + u32::from(pipe) * 0x1000
    }
    fn vblank_wait_expired(&mut self) -> bool {
        false
    }
    fn prepare_vblank_wait(&mut self) {}
    fn finish_vblank_wait(&mut self) {}
    fn useconds_to_scanlines(&self, mode: &Mode, usecs: u32) -> i32 {
        if mode.crtc_clock == 0 || mode.htotal == 0 {
            0
        } else {
            ((u64::from(mode.crtc_clock) * u64::from(usecs))
                / (1000 * u64::from(mode.htotal as u32))) as i32
        }
    }
    fn vrr_vmin_vblank_start(&self, state: &CrtcState) -> i32 {
        state.vrr.vmin_vblank_start
    }
    fn vrr_vmax_vblank_start(&self, state: &CrtcState) -> i32 {
        state.vrr.vmax_vblank_start
    }
    fn vrr_dcb_vmin_vblank_start_next(&self, state: &CrtcState) -> i32 {
        state.vrr.dcb_vmin_next
    }
    fn vrr_dcb_vmax_vblank_start_next(&self, state: &CrtcState) -> i32 {
        state.vrr.dcb_vmax_next
    }
    fn vrr_dcb_vmin_vblank_start_final(&self, state: &CrtcState) -> i32 {
        state.vrr.dcb_vmin_final
    }
    fn vrr_dcb_vmax_vblank_start_final(&self, state: &CrtcState) -> i32 {
        state.vrr.dcb_vmax_final
    }
}

const PIPE_PIXEL_MASK: u64 = 0x00ff_ffff;
const PIPE_PIXEL_SHIFT: u32 = 0;
const PIPE_FRAME_LOW_SHIFT: u32 = 24;
const PIPE_FRAME_MASK: u64 = 0xffffff;
const PIPEDSL_LINE_MASK: u32 = 0x1fff;
fn mode_interlaced(mode: &Mode) -> bool {
    mode.flags & DRM_MODE_FLAG_INTERLACE != 0
}
// upstream: intel_vblank.c i915_get_vblank_counter()
pub fn i915_get_vblank_counter(
    io: &mut impl VblankIo,
    display: Display,
    pipe: u8,
    vblank: VblankCrtc,
) -> u32 {
    if vblank.max_vblank_count == 0 {
        return 0;
    }
    let mode = vblank.hwmode;
    let htotal = mode.htotal;
    let hsync_start = mode.hsync_start;
    let mut start = intel_mode_vblank_start(&mode);
    start *= htotal;
    start -= htotal - hsync_start;
    let frame = io.read64_frame_pixel(pipe);
    let pixel = (frame & PIPE_PIXEL_MASK) as i32;
    let frame = ((frame >> PIPE_FRAME_LOW_SHIFT) & PIPE_FRAME_MASK as u64) as u32;
    let _ = display;
    (frame + u32::from(pixel >= start)) & 0xffffff
}
// upstream: intel_vblank.c g4x_get_vblank_counter()
pub fn g4x_get_vblank_counter(io: &mut impl VblankIo, pipe: u8, vblank: VblankCrtc) -> u32 {
    if vblank.max_vblank_count == 0 {
        0
    } else {
        io.read32(0x70040 + u32::from(pipe) * 0x1000)
    }
}
// upstream: intel_vblank.c intel_crtc_scanlines_since_frame_timestamp()
pub fn intel_crtc_scanlines_since_frame_timestamp(
    io: &mut impl VblankIo,
    pipe: u8,
    mode: Mode,
) -> u32 {
    let htotal = mode.htotal as u64;
    let clock = mode.crtc_clock as u64;
    let mut prev;
    let mut curr;
    let mut post;
    loop {
        prev = io.read32_fw(io.frame_timestamp_reg(pipe));
        curr = io.read_timestamp();
        post = io.read32_fw(io.frame_timestamp_reg(pipe));
        if post == prev {
            break;
        }
    }
    if htotal == 0 {
        0
    } else {
        (((curr.wrapping_sub(prev) as u64) * clock) / (1000 * htotal)) as u32
    }
}
// upstream: intel_vblank.c __intel_get_crtc_scanline_from_timestamp()
pub fn __intel_get_crtc_scanline_from_timestamp(
    io: &mut impl VblankIo,
    pipe: u8,
    mode: Mode,
) -> u32 {
    let vtotal = mode.vtotal;
    if vtotal <= 0 {
        return 0;
    }
    let scan = intel_crtc_scanlines_since_frame_timestamp(io, pipe, mode).min((vtotal - 1) as u32);
    (scan + mode.vblank_start as u32) % vtotal as u32
}
// upstream: intel_vblank.c intel_crtc_scanline_offset()
pub fn intel_crtc_scanline_offset(state: &CrtcState) -> i32 {
    let display = state.display;
    if display.display_ver >= 20 || display.battlemage {
        1
    } else if display.display_ver >= 9 || display.broadwell || display.haswell {
        if state.output_types & I915_OUTPUT_HDMI != 0 {
            2
        } else {
            1
        }
    } else if display.display_ver >= 3 {
        1
    } else {
        -1
    }
}
// upstream: intel_vblank.c __intel_get_crtc_scanline()
pub fn __intel_get_crtc_scanline(io: &mut impl VblankIo, crtc: Crtc, vblank: VblankCrtc) -> i32 {
    if !crtc.active {
        return 0;
    }
    let mode = vblank.hwmode;
    if crtc.mode_flags & I915_MODE_FLAG_GET_SCANLINE_FROM_TIMESTAMP != 0 {
        return __intel_get_crtc_scanline_from_timestamp(io, crtc.pipe, mode) as i32;
    }
    let vtotal = intel_mode_vtotal(&mode);
    if vtotal <= 0 {
        return 0;
    }
    let mut position = (io.read32_fw(io.pipe_dsl_reg(crtc.pipe)) & PIPEDSL_LINE_MASK) as i32;
    if crtc.display.ddi && position == 0 {
        for _ in 0..100 {
            io.delay_us(1);
            let temp = (io.read32_fw(io.pipe_dsl_reg(crtc.pipe)) & PIPEDSL_LINE_MASK) as i32;
            if temp != position {
                position = temp;
                break;
            }
        }
    }
    (position + vtotal + crtc.scanline_offset) % vtotal
}
// upstream: intel_vblank.c intel_vblank_section_enter()
pub fn intel_vblank_section_enter(io: &mut impl VblankIo) {
    io.vblank_section_enter();
}
// upstream: intel_vblank.c intel_vblank_section_exit()
pub fn intel_vblank_section_exit(io: &mut impl VblankIo) {
    io.vblank_section_exit();
}
// upstream: intel_vblank.c intel_vblank_section_enter()
pub fn intel_vblank_section_enter_xe(_io: &mut impl VblankIo) {}
// upstream: intel_vblank.c intel_vblank_section_exit()
pub fn intel_vblank_section_exit_xe(_io: &mut impl VblankIo) {}
// upstream: intel_vblank.c i915_get_crtc_scanoutpos()
pub fn i915_get_crtc_scanoutpos(
    io: &mut impl VblankIo,
    crtc: Crtc,
    mode: Mode,
    in_vblank_irq: bool,
    stime: Option<&mut u64>,
    etime: Option<&mut u64>,
) -> Option<ScanoutPosition> {
    if mode.crtc_clock == 0 {
        io.warn(1, crtc.pipe as i64);
        return None;
    }
    let htotal = mode.htotal;
    let hsync = mode.hsync_start;
    let mut vtotal = intel_mode_vtotal(&mode);
    let mut vstart = intel_mode_vblank_start(&mode);
    let mut vend = intel_mode_vblank_end(&mode);
    let use_counter = io.use_scanline_counter(crtc.display, &crtc);
    let irqflags = io.local_irq_save();
    io.vblank_section_enter();
    if let Some(t) = stime {
        *t = io.now_ns();
    }
    let mut position;
    if crtc.mode_flags & I915_MODE_FLAG_VRR != 0 {
        let scan = intel_crtc_scanlines_since_frame_timestamp(io, crtc.pipe, mode) as i32;
        position = __intel_get_crtc_scanline(
            io,
            crtc,
            VblankCrtc {
                hwmode: mode,
                max_vblank_count: 0,
            },
        );
        if position >= vstart && scan < position {
            position = (crtc.vmax_vblank_start + scan).min(vtotal - 1);
        }
    } else if use_counter {
        position = __intel_get_crtc_scanline(
            io,
            crtc,
            VblankCrtc {
                hwmode: mode,
                max_vblank_count: 0,
            },
        );
    } else {
        position = ((io.read32_fw(io.pipe_frame_pixel_reg(crtc.pipe)) & PIPE_PIXEL_MASK as u32)
            >> PIPE_PIXEL_SHIFT) as i32;
        vstart *= htotal;
        vend *= htotal;
        vtotal *= htotal;
        position = position.min(vtotal - 1);
        position = (position + htotal - hsync) % vtotal;
    }
    if let Some(t) = etime {
        *t = io.now_ns();
    }
    io.vblank_section_exit();
    io.local_irq_restore(irqflags);
    if position >= vstart {
        position -= vend;
    } else {
        position += vtotal - vend;
    }
    let (vpos, hpos) = if use_counter {
        (position, 0)
    } else {
        (position / htotal, position - (position / htotal) * htotal)
    };
    let _ = in_vblank_irq;
    Some(ScanoutPosition { vpos, hpos })
}
// upstream: intel_vblank.c intel_crtc_get_vblank_timestamp()
pub fn intel_crtc_get_vblank_timestamp(
    io: &mut impl VblankIo,
    max_error: &mut i32,
    vblank_time: &mut u64,
    in_vblank_irq: bool,
) -> bool {
    io.drm_vblank_timestamp(max_error, vblank_time, in_vblank_irq)
}
// upstream: intel_vblank.c intel_get_crtc_scanline()
pub fn intel_get_crtc_scanline(io: &mut impl VblankIo, crtc: Crtc, vblank: VblankCrtc) -> i32 {
    let flags = io.local_irq_save();
    io.vblank_section_enter();
    let pos = __intel_get_crtc_scanline(io, crtc, vblank);
    io.vblank_section_exit();
    io.local_irq_restore(flags);
    pos
}
// upstream: intel_vblank.c pipe_scanline_is_moving()
pub fn pipe_scanline_is_moving(io: &mut impl VblankIo, pipe: u8) -> bool {
    let reg = io.pipe_dsl_reg(pipe);
    let a = io.read32(reg) & PIPEDSL_LINE_MASK;
    io.sleep_ms(5);
    let b = io.read32(reg) & PIPEDSL_LINE_MASK;
    a != b
}
// upstream: intel_vblank.c wait_for_pipe_scanline_moving()
pub fn wait_for_pipe_scanline_moving(io: &mut impl VblankIo, pipe: u8, moving: bool) {
    let start = io.now_us();
    loop {
        let state = pipe_scanline_is_moving(io, pipe);
        if state == moving {
            return;
        }
        if io.now_us().saturating_sub(start) >= 100_000 {
            io.debug(2, pipe as i64);
            return;
        }
        io.delay_us(500);
    }
}
// upstream: intel_vblank.c intel_wait_for_pipe_scanline_stopped()
pub fn intel_wait_for_pipe_scanline_stopped(io: &mut impl VblankIo, pipe: u8) {
    wait_for_pipe_scanline_moving(io, pipe, false)
}
// upstream: intel_vblank.c intel_wait_for_pipe_scanline_moving()
pub fn intel_wait_for_pipe_scanline_moving(io: &mut impl VblankIo, pipe: u8) {
    wait_for_pipe_scanline_moving(io, pipe, true)
}
// upstream: intel_vblank.c intel_crtc_active_timings()
pub fn intel_crtc_active_timings(
    mode: &mut Mode,
    vmax_vblank_start: &mut i32,
    state: &CrtcState,
    vrr_enable: bool,
) {
    *mode = state.adjusted_mode;
    *vmax_vblank_start = 0;
    if !vrr_enable {
        return;
    }
    mode.vtotal = state.vrr.vmax_vtotal;
    mode.vblank_end = state.vrr.vmax_vtotal;
    mode.vblank_start = state.vrr.vmin_vblank_start;
    *vmax_vblank_start = state.vrr.vmax_vblank_start;
}
// upstream: intel_vblank.c intel_crtc_update_active_timings()
pub fn intel_crtc_update_active_timings(
    io: &mut impl VblankIo,
    state: &CrtcState,
    crtc: &mut Crtc,
    vrr_enable: bool,
) {
    let mut mode = Mode::default();
    let mut vmax = 0;
    intel_crtc_active_timings(&mut mode, &mut vmax, state, vrr_enable);
    let mut flags = state.mode_flags;
    if vrr_enable {
        if flags & I915_MODE_FLAG_VRR == 0 {
            io.warn(3, flags as i64);
        }
    } else {
        flags &= !I915_MODE_FLAG_VRR;
    }
    io.vblank_time_lock();
    io.vblank_section_enter();
    io.calculate_timestamping_constants(mode);
    crtc.vmax_vblank_start = vmax;
    crtc.mode_flags = flags;
    crtc.scanline_offset = intel_crtc_scanline_offset(state);
    io.vblank_section_exit();
    io.vblank_time_unlock();
}
// upstream: intel_vblank.c intel_mode_vdisplay()
pub fn intel_mode_vdisplay(mode: &Mode) -> i32 {
    if mode_interlaced(mode) {
        (mode.vdisplay + 1) / 2
    } else {
        mode.vdisplay
    }
}
// upstream: intel_vblank.c intel_mode_vblank_start()
pub fn intel_mode_vblank_start(mode: &Mode) -> i32 {
    if mode_interlaced(mode) {
        (mode.vblank_start + 1) / 2
    } else {
        mode.vblank_start
    }
}
// upstream: intel_vblank.c intel_mode_vblank_end()
pub fn intel_mode_vblank_end(mode: &Mode) -> i32 {
    if mode_interlaced(mode) {
        mode.vblank_end / 2
    } else {
        mode.vblank_end
    }
}
// upstream: intel_vblank.c intel_mode_vtotal()
pub fn intel_mode_vtotal(mode: &Mode) -> i32 {
    if mode_interlaced(mode) {
        mode.vtotal / 2
    } else {
        mode.vtotal
    }
}
// upstream: intel_vblank.c intel_mode_vblank_delay()
pub fn intel_mode_vblank_delay(mode: &Mode) -> i32 {
    intel_mode_vblank_start(mode) - intel_mode_vdisplay(mode)
}
// upstream: intel_vblank.c pre_commit_crtc_state()
pub fn pre_commit_crtc_state<'a>(old: &'a CrtcState, new: &'a CrtcState) -> &'a CrtcState {
    if new.needs_modeset { new } else { old }
}
// upstream: intel_vblank.c intel_pre_commit_crtc_state()
pub fn intel_pre_commit_crtc_state<'a>(old: &'a CrtcState, new: &'a CrtcState) -> &'a CrtcState {
    pre_commit_crtc_state(old, new)
}
// upstream: intel_vblank.c vrr_vblank_start()
pub fn vrr_vblank_start(io: &impl VblankIo, state: &CrtcState) -> i32 {
    let sent = state.vrr.is_push_sent;
    if !state.vrr.dc_balance_enable {
        return if sent {
            io.vrr_vmin_vblank_start(state)
        } else {
            io.vrr_vmax_vblank_start(state)
        };
    }
    let next = if sent {
        io.vrr_dcb_vmin_vblank_start_next(state)
    } else {
        io.vrr_dcb_vmax_vblank_start_next(state)
    };
    if next >= 0 {
        return next;
    }
    if sent {
        io.vrr_dcb_vmin_vblank_start_final(state)
    } else {
        io.vrr_dcb_vmax_vblank_start_final(state)
    }
}
// upstream: intel_vblank.c intel_vblank_evade_init()
pub fn intel_vblank_evade_init(
    io: &mut impl VblankIo,
    old: &CrtcState,
    new: &CrtcState,
    evade: &mut VblankEvade,
) {
    let state = pre_commit_crtc_state(old, new);
    evade.pipe = new.pipe;
    evade.need_vlv_dsi_wa = (new.display.valleyview || new.display.cherryview)
        && new.output_types & I915_OUTPUT_DSI != 0;
    let mode = state.adjusted_mode;
    if state.mode_flags & I915_MODE_FLAG_VRR != 0 {
        if new.needs_modeset || new.update_m_n || new.update_lrr {
            io.warn(4, new.pipe as i64);
        }
        evade.vblank_start = vrr_vblank_start(io, state);
    } else {
        evade.vblank_start = intel_mode_vblank_start(&mode);
    }
    let delay = if state.mode_flags & I915_MODE_FLAG_VRR != 0 {
        state.set_context_latency
    } else {
        intel_mode_vblank_delay(&mode)
    };
    evade.min = evade.vblank_start - io.useconds_to_scanlines(&mode, VBLANK_EVASION_TIME_US);
    evade.max = evade.vblank_start - 1;
    if new.color_uses_dsb || new.update_m_n || new.update_lrr {
        evade.min -= delay;
    }
}
// upstream: intel_vblank.c intel_vblank_evade()
pub fn intel_vblank_evade(
    io: &mut impl VblankIo,
    evade: &VblankEvade,
    crtc: Crtc,
    vblank: VblankCrtc,
) -> i32 {
    if evade.min <= 0 || evade.max <= 0 {
        return 0;
    }
    let start = io.now_ms();
    loop {
        io.prepare_vblank_wait();
        let scanline = intel_get_crtc_scanline(io, crtc, vblank);
        if scanline < evade.min || scanline > evade.max {
            break;
        }
        if io.now_ms().saturating_sub(start) >= 1 {
            io.debug(5, crtc.pipe as i64);
            break;
        }
        io.local_irq_enable();
        io.schedule_timeout_ms(1);
        io.local_irq_disable();
    }
    io.finish_vblank_wait();
    let mut scanline = intel_get_crtc_scanline(io, crtc, vblank);
    while evade.need_vlv_dsi_wa && scanline == evade.vblank_start {
        scanline = intel_get_crtc_scanline(io, crtc, vblank);
    }
    scanline
}
// upstream: intel_vblank.c intel_crtc_vblank_length()
pub fn intel_crtc_vblank_length(state: &CrtcState) -> i32 {
    if state.vrr.enable {
        state.vrr.guardband
    } else {
        state.adjusted_mode.vtotal - state.adjusted_mode.vblank_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interlace_mode_helpers_and_vblank_offset_follow_source() {
        let mode = Mode {
            flags: DRM_MODE_FLAG_INTERLACE,
            vdisplay: 1080,
            vblank_start: 1084,
            vblank_end: 1124,
            vtotal: 1125,
            ..Mode::default()
        };
        assert_eq!(intel_mode_vdisplay(&mode), 540);
        assert_eq!(intel_mode_vblank_start(&mode), 542);
        assert_eq!(intel_mode_vblank_end(&mode), 562);
        assert_eq!(intel_mode_vtotal(&mode), 562);
    }
    #[test]
    fn vblank_evade_precommit_selects_new_state_only_for_modeset() {
        let a = CrtcState::default();
        let b = CrtcState {
            needs_modeset: true,
            ..CrtcState::default()
        };
        assert_eq!(pre_commit_crtc_state(&a, &b), &b);
        assert_eq!(pre_commit_crtc_state(&b, &a), &b);
    }
}
