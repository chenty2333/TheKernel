// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_crtc.c translation.
// DRM object, atomic, vblank-work, plane, and QoS operations are trait hooks.

#![allow(dead_code, clippy::too_many_arguments)]

extern crate alloc;
use alloc::vec::Vec;

pub const INVALID_TRANSCODER: u8 = 0xff;
pub const INVALID_PIPE: u8 = 0xff;
pub const INVALID_PLANE: u8 = 0xff;
pub const PLANE_CURSOR: u8 = 7;
pub const I915_MODE_FLAG_DSI_USE_TE0: u32 = 1 << 3;
pub const I915_MODE_FLAG_DSI_USE_TE1: u32 = 1 << 4;
pub const PM_QOS_DEFAULT_VALUE: i32 = -1;
pub const INT_MAX: i32 = i32::MAX;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Display {
    pub display_ver: u8,
    pub pipe_mask: u8,
    pub dgfx: bool,
    pub i965gm: bool,
    pub g4x: bool,
    pub valleyview: bool,
    pub cherryview: bool,
    pub i945gm: bool,
    pub i915gm: bool,
    pub i830: bool,
    pub i85x: bool,
    pub i915g: bool,
    pub tv_output_bit: u32,
    pub has_gmch: bool,
    pub num_sprites: [u8; 8],
    pub num_scalers: [u8; 8],
    pub has_casf: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub pipe: u8,
    pub display: Display,
    pub active: bool,
    pub enable: bool,
    pub needs_modeset: bool,
    pub needs_fastset: bool,
    pub preload_luts: bool,
    pub needs_color_update: bool,
    pub double_buffered_lut: bool,
    pub color_uses_dsb: bool,
    pub use_dsb: bool,
    pub do_async_flip: bool,
    pub legacy_cursor_update: bool,
    pub active_planes: u8,
    pub data_rate: [u32; 8],
    pub data_rate_y: [u32; 8],
    pub mode_flags: u32,
    pub output_types: u32,
    pub update_m_n: bool,
    pub update_lrr: bool,
    pub vrr_enable: bool,
    pub event: Option<u64>,
    pub vblank_pm_qos: i32,
    pub debug_min_vbl: i32,
    pub debug_max_vbl: i32,
    pub debug_scanline_start: i32,
    pub debug_start_count: u32,
    pub context_latency: i32,
    pub cpu_transcoder: u8,
    pub master_transcoder: u8,
    pub hsw_workaround_pipe: u8,
    pub scaler_id: i8,
    pub mst_master_transcoder: u8,
    pub max_link_bpp_x16: i32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Crtc {
    pub pipe: u8,
    pub display: Display,
    pub active: bool,
    pub state_id: u64,
    pub crtc_id: u32,
    pub crtc_name: &'static str,
    pub plane_ids_mask: u32,
    pub num_scalers: u8,
    pub vblank_psr_notify: bool,
    pub scanline_offset: i32,
    pub mode_flags: u32,
    pub vmax_vblank_start: i32,
    pub vblank_pm_qos: i32,
    pub debug: CrtcDebug,
    pub flip_done_event: Option<u64>,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcDebug {
    pub min_vbl: i32,
    pub max_vbl: i32,
    pub scanline_start: i32,
    pub start_vbl_time: u64,
    pub start_vbl_count: u32,
    pub vbl_sum: u64,
    pub vbl_min: u64,
    pub vbl_max: u64,
    pub vbl_over: u64,
    pub histogram: [u64; 32],
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AtomicCrtcChange {
    pub pipe: u8,
    pub old: CrtcState,
    pub new: CrtcState,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipeUpdateDebug {
    pub scanline: i32,
    pub vblank_count: u32,
    pub timestamp_ns: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrtcFuncs {
    Bdw,
    Ilk,
    G4x,
    I965,
    I915gm,
    I915,
    I8xx,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneKind {
    Primary,
    Sprite(u8),
    Cursor,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrtcError {
    NoMemory,
    Framework(i32),
    NoDevice,
}

/// DRM/atomic/vblank/event and platform operations called by source routines.
pub trait CrtcIo {
    fn warn(&mut self, _code: u32, _value: i64) {}
    fn debug(&mut self, _code: u32, _value: i64) {}
    fn trace(&mut self, _code: u32, _pipe: u8, _value: u64) {}
    fn drm_crtc_vblank_get(&mut self, _crtc: u32) -> i32 {
        0
    }
    fn drm_crtc_vblank_put(&mut self, _crtc: u32) {}
    fn drm_crtc_wait_one_vblank(&mut self, _crtc: u32) {}
    fn drm_crtc_vblank_count(&mut self, _crtc: u32) -> u32 {
        0
    }
    fn drm_crtc_accurate_vblank_count(&mut self, _crtc: u32) -> u32 {
        0
    }
    fn preempt_rt(&self) -> bool {
        false
    }
    fn get_vblank_counter(&mut self, _crtc: u32) -> u32 {
        0
    }
    fn psr_vblank_notification(&mut self, _state: &CrtcState) -> bool {
        false
    }
    fn set_max_vblank_count(&mut self, _crtc: u32, _count: u32) {}
    fn vblank_on(&mut self, _crtc: u32) {}
    fn vblank_off(&mut self, _crtc: u32) {}
    fn flush_vblank_notify_work(&mut self) {}
    fn alloc_crtc_state(&mut self) -> Option<u64> {
        None
    }
    fn reset_crtc_state(&mut self, _state: u64, _crtc: u32) {}
    fn set_crtc_state_defaults(&mut self, _state: u64, _defaults: CrtcState) {}
    fn free_crtc_state(&mut self, _state: u64) {}
    fn alloc_crtc(&mut self) -> Option<u64> {
        None
    }
    fn allocate_crtc_id(&mut self) -> u32 {
        0
    }
    fn crtc_id(&mut self, _crtc: u64) -> u32 {
        0
    }
    fn set_crtc_config_state(&mut self, _crtc: u64, _state: u64) {}
    fn free_crtc(&mut self, _crtc: u64) {}
    fn destroy_crtc(&mut self, _crtc: u64) {}
    fn remove_crtc_from_pipe_list(&mut self, _crtc: u64) {}
    fn crtc_cleanup(&mut self, _crtc: u64) {}
    fn remove_cpu_latency_qos(&mut self, _crtc: u64) {}
    fn debugfs_add(&mut self, _crtc: u64) -> i32 {
        0
    }
    fn init_fifo_underrun_reporting(&mut self, _pipe: u8, _enable: bool) {}
    fn create_plane(&mut self, _display: Display, _pipe: u8, _kind: PlaneKind) -> Result<u8, i32> {
        Ok(0)
    }
    fn set_crtc_identity(&mut self, _crtc: u64, _pipe: u8, _num_scalers: u8) {}
    fn set_crtc_plane_ids_mask(&mut self, _crtc: u64, _mask: u32) {}
    fn drm_crtc_init(&mut self, _crtc: u64, _primary: u8, _cursor: u8, _funcs: CrtcFuncs) -> i32 {
        0
    }
    fn create_scaling_filter_property(&mut self, _pipe: u8, _mask: u32) {}
    fn create_background_color_property(&mut self, _pipe: u8) {}
    fn color_crtc_init(&mut self, _pipe: u8) {}
    fn drrs_crtc_init(&mut self, _pipe: u8) {}
    fn crc_crtc_init(&mut self, _pipe: u8) {}
    fn add_cpu_latency_qos(&mut self, _crtc: u64, _value: i32) {}
    fn create_sharpness_property(&mut self, _pipe: u8) {}
    fn insert_crtc_sorted(&mut self, _crtc: u64, _pipe: u8) {}
    fn debug_num_pipes(&mut self) -> u8 {
        0
    }
    fn crtc_find_pipe(&mut self, _crtc_id: u32) -> Option<u8> {
        None
    }
    fn get_plane_state(&mut self, _pipe: u8, _plane: u8) -> Option<(bool, u32, u32)> {
        None
    }
    fn vblank_work_start(&mut self, _pipe: u8) {}
    fn load_luts(&mut self, _pipe: u8) {}
    fn event_lock(&mut self) {}
    fn send_vblank_event(&mut self, _crtc: u32, _event: u64) {}
    fn event_unlock(&mut self) {}
    fn vblank_work_end(&mut self, _pipe: u8) {}
    fn vblank_work_init(&mut self, _pipe: u8) {}
    fn cpu_latency_qos_update(&mut self, _crtc: u64, _value: i32) {}
    fn flush_vblank_work(&mut self, _pipe: u8) {}
    fn usecs_to_scanlines(&self, mode_clock: u32, htotal: u32, usecs: u32) -> u32 {
        if htotal == 0 {
            1
        } else {
            ((u64::from(usecs) * u64::from(mode_clock) + 1000 * u64::from(htotal) - 1)
                / (1000 * u64::from(htotal))) as u32
        }
    }
    fn prepare_cursor_vblank_work(&mut self, _pipe: u8) {}
    fn psr_lock(&mut self, _pipe: u8) {}
    fn psr_unlock(&mut self, _pipe: u8) {}
    fn prepare_vblank_event(&mut self, _pipe: u8) {}
    fn psr_wait_idle(&mut self, _pipe: u8) {}
    fn vblank_evade_init(&mut self, _pipe: u8, _old: &CrtcState, _new: &CrtcState) -> (i32, i32) {
        (0, 0)
    }
    fn vblank_evade(&mut self, _pipe: u8) -> i32 {
        0
    }
    fn vblank_timestamp_ns(&mut self) -> u64 {
        0
    }
    fn intel_vblank_count(&mut self, _pipe: u8) -> u32 {
        0
    }
    fn scanline(&mut self, _pipe: u8) -> i32 {
        0
    }
    fn vblank_work_schedule(&mut self, _pipe: u8, _count: u32) {}
    fn arm_vblank_event(&mut self, _pipe: u8) {}
    fn schedule_unpin_work(&mut self, _pipe: u8, _index: usize, _count: u32) {}
    fn clear_removed_plane(&mut self, _index: usize) {}
    fn legacy_cursor_state(&mut self, _pipe: u8, _index: usize) -> Option<(bool, bool)> {
        None
    }
    fn vrr_trans_push(&mut self, _pipe: u8) {}
    fn local_irq_disable(&mut self) {}
    fn local_irq_enable(&mut self) {}
    fn parent_vgpu_active(&mut self) -> bool {
        false
    }
    fn debug_pipe_update_error(
        &mut self,
        _pipe: u8,
        _start: u32,
        _end: u32,
        _duration_us: i64,
        _min: i32,
        _max: i32,
        _start_line: i32,
        _end_line: i32,
    ) {
    }
    fn debug_vblank_evasion(&mut self, _pipe: u8, _delta_ns: u64) {}
    fn display_error(&mut self, _code: u32, _value: i64) {}
    fn crtc_state_iter(&mut self) -> Vec<AtomicCrtcChange> {
        Vec::new()
    }
    fn sprite_count(&self, _pipe: u8) -> u8 {
        0
    }
    fn crtc_state_needs_vblank_work(&self, state: &CrtcState) -> bool {
        state.active
            && !state.preload_luts
            && !state.needs_modeset
            && (state.needs_color_update && !state.double_buffered_lut)
            && !state.color_uses_dsb
            && !state.use_dsb
    }
    fn needs_modeset(&self, state: &CrtcState) -> bool {
        state.needs_modeset
    }
    fn set_context_latency(&self, state: &CrtcState) -> i32 {
        state.context_latency
    }
    fn debug_use_dsb(&mut self, _pipe: u8) {}
    fn legacy_cursor_plane_indices(&mut self, _pipe: u8) -> Vec<usize> {
        Vec::new()
    }
    fn legacy_cursor_old_plane_crtc(&mut self, _index: usize, _pipe: u8) -> bool {
        false
    }
    fn legacy_cursor_has_unpin_vblank_work(&mut self, _index: usize) -> bool {
        false
    }
    fn dsi_frame_update(&mut self, _pipe: u8) {}
    fn use_trans_push(&mut self, _pipe: u8) -> bool {
        false
    }
    fn debug_vblank_update(
        &mut self,
        _pipe: u8,
        _delta_ns: u64,
        _histogram_bucket: u32,
        _overrun: bool,
    ) {
    }
}

// upstream: intel_crtc.c assert_vblank_disabled()
pub fn assert_vblank_disabled(io: &mut impl CrtcIo, crtc: &Crtc) {
    if io.drm_crtc_vblank_get(crtc.crtc_id) == 0 {
        io.warn(1, crtc.crtc_id as i64);
        io.drm_crtc_vblank_put(crtc.crtc_id);
    }
}
// upstream: intel_crtc.c intel_first_crtc()
pub fn intel_first_crtc(crtcs: &[Crtc]) -> Option<Crtc> {
    crtcs.first().copied()
}
// upstream: intel_crtc.c intel_crtc_for_pipe()
pub fn intel_crtc_for_pipe(crtcs: &[Crtc], pipe: u8) -> Option<Crtc> {
    crtcs.iter().copied().find(|c| c.pipe == pipe)
}
// upstream: intel_crtc.c intel_crtc_wait_for_next_vblank()
pub fn intel_crtc_wait_for_next_vblank(io: &mut impl CrtcIo, crtc: &Crtc) {
    io.drm_crtc_wait_one_vblank(crtc.crtc_id);
}
// upstream: intel_crtc.c intel_wait_for_vblank_if_active()
pub fn intel_wait_for_vblank_if_active(io: &mut impl CrtcIo, crtcs: &[Crtc], pipe: u8) {
    if let Some(crtc) = intel_crtc_for_pipe(crtcs, pipe) {
        if crtc.active {
            intel_crtc_wait_for_next_vblank(io, &crtc);
        }
    }
}
// upstream: intel_crtc.c intel_crtc_get_vblank_counter()
pub fn intel_crtc_get_vblank_counter(io: &mut impl CrtcIo, crtc: &Crtc, hardware_max: u32) -> u32 {
    if !crtc.active {
        return 0;
    }
    if hardware_max == 0 {
        if io.preempt_rt() {
            io.drm_crtc_vblank_count(crtc.crtc_id)
        } else {
            io.drm_crtc_accurate_vblank_count(crtc.crtc_id)
        }
    } else {
        io.get_vblank_counter(crtc.crtc_id)
    }
}
// upstream: intel_crtc.c intel_crtc_max_vblank_count()
pub fn intel_crtc_max_vblank_count(display: Display, state: &CrtcState) -> u32 {
    if state.mode_flags & (I915_MODE_FLAG_DSI_USE_TE0 | I915_MODE_FLAG_DSI_USE_TE1) != 0 {
        return 0;
    }
    if display.i965gm && state.output_types & display.tv_output_bit != 0 {
        return 0;
    }
    if display.display_ver >= 5 || display.g4x {
        u32::MAX
    } else if display.display_ver >= 3 {
        0xffffff
    } else {
        0
    }
}
// upstream: intel_crtc.c intel_crtc_vblank_on()
pub fn intel_crtc_vblank_on(io: &mut impl CrtcIo, crtc: &mut Crtc, state: &CrtcState) {
    crtc.vblank_psr_notify = io.psr_vblank_notification(state);
    assert_vblank_disabled(io, crtc);
    io.set_max_vblank_count(
        crtc.crtc_id,
        intel_crtc_max_vblank_count(crtc.display, state),
    );
    io.vblank_on(crtc.crtc_id);
    io.trace(1, crtc.pipe, crtc.crtc_id as u64);
}
// upstream: intel_crtc.c intel_crtc_vblank_off()
pub fn intel_crtc_vblank_off(io: &mut impl CrtcIo, crtc: &mut Crtc) {
    io.trace(2, crtc.pipe, crtc.crtc_id as u64);
    io.vblank_off(crtc.crtc_id);
    assert_vblank_disabled(io, crtc);
    crtc.vblank_psr_notify = false;
    io.flush_vblank_notify_work();
}
// upstream: intel_crtc.c intel_crtc_state_alloc()
pub fn intel_crtc_state_alloc(io: &mut impl CrtcIo, crtc: &Crtc) -> Option<u64> {
    let state = io.alloc_crtc_state()?;
    intel_crtc_state_reset(io, state, crtc);
    Some(state)
}
// upstream: intel_crtc.c intel_crtc_state_reset()
pub fn intel_crtc_state_reset(io: &mut impl CrtcIo, state: u64, crtc: &Crtc) {
    io.reset_crtc_state(state, crtc.crtc_id);
    io.set_crtc_state_defaults(
        state,
        CrtcState {
            cpu_transcoder: INVALID_TRANSCODER,
            master_transcoder: INVALID_TRANSCODER,
            hsw_workaround_pipe: INVALID_PIPE,
            scaler_id: -1,
            mst_master_transcoder: INVALID_TRANSCODER,
            max_link_bpp_x16: INT_MAX,
            ..CrtcState::default()
        },
    );
}
// upstream: intel_crtc.c intel_crtc_alloc()
pub fn intel_crtc_alloc(io: &mut impl CrtcIo, crtc_id: u32) -> Result<(u64, u64), CrtcError> {
    let crtc = io.alloc_crtc().ok_or(CrtcError::NoMemory)?;
    let value = Crtc {
        crtc_id,
        ..Crtc::default()
    };
    let Some(state) = intel_crtc_state_alloc(io, &value) else {
        io.free_crtc(crtc);
        return Err(CrtcError::NoMemory);
    };
    io.set_crtc_config_state(crtc, state);
    Ok((crtc, state))
}
// upstream: intel_crtc.c intel_crtc_free()
pub fn intel_crtc_free(io: &mut impl CrtcIo, crtc: u64, state: u64) {
    io.free_crtc_state(state);
    io.free_crtc(crtc);
}
// upstream: intel_crtc.c intel_crtc_destroy()
pub fn intel_crtc_destroy(io: &mut impl CrtcIo, crtc: u64) {
    io.remove_crtc_from_pipe_list(crtc);
    io.remove_cpu_latency_qos(crtc);
    io.crtc_cleanup(crtc);
    io.destroy_crtc(crtc);
}
// upstream: intel_crtc.c intel_crtc_late_register()
pub fn intel_crtc_late_register(io: &mut impl CrtcIo, crtc: u64) -> i32 {
    io.debugfs_add(crtc)
}
// upstream: intel_crtc.c add_crtc_to_pipe_list()
pub fn add_crtc_to_pipe_list(io: &mut impl CrtcIo, crtc: u64, pipe: u8) {
    io.insert_crtc_sorted(crtc, pipe);
}
// upstream: intel_crtc.c __intel_crtc_init()
pub fn __intel_crtc_init(
    io: &mut impl CrtcIo,
    display: Display,
    pipe: u8,
) -> Result<(), CrtcError> {
    let crtc_id = io.allocate_crtc_id();
    let (crtc, state) = intel_crtc_alloc(io, crtc_id)?;
    io.set_crtc_identity(crtc, pipe, display.num_scalers[pipe as usize]);
    let result = (|| {
        let primary = io
            .create_plane(display, pipe, PlaneKind::Primary)
            .map_err(CrtcError::Framework)?;
        let mut mask = 1u32 << primary;
        io.init_fifo_underrun_reporting(pipe, false);
        for sprite in 0..io.sprite_count(pipe) {
            let plane = io
                .create_plane(display, pipe, PlaneKind::Sprite(sprite))
                .map_err(CrtcError::Framework)?;
            mask |= 1u32 << plane;
        }
        let cursor = io
            .create_plane(display, pipe, PlaneKind::Cursor)
            .map_err(CrtcError::Framework)?;
        mask |= 1u32 << cursor;
        io.set_crtc_plane_ids_mask(crtc, mask);
        let funcs = select_crtc_funcs(display);
        let ret = io.drm_crtc_init(crtc, primary, cursor, funcs);
        if ret != 0 {
            return Err(CrtcError::Framework(ret));
        }
        if display.display_ver >= 11 {
            io.create_scaling_filter_property(pipe, 3);
        }
        if display.display_ver >= 9 {
            io.create_background_color_property(pipe);
        }
        io.color_crtc_init(pipe);
        io.drrs_crtc_init(pipe);
        io.crc_crtc_init(pipe);
        io.add_cpu_latency_qos(crtc, PM_QOS_DEFAULT_VALUE);
        if display.has_casf && display.num_scalers[pipe as usize] >= 2 {
            io.create_sharpness_property(pipe);
        }
        add_crtc_to_pipe_list(io, crtc, pipe);
        Ok(())
    })();
    if result.is_err() {
        intel_crtc_free(io, crtc, state);
    }
    result
}
fn select_crtc_funcs(display: Display) -> CrtcFuncs {
    if display.has_gmch {
        if display.cherryview || display.valleyview || display.g4x {
            CrtcFuncs::G4x
        } else if display.display_ver == 4 {
            CrtcFuncs::I965
        } else if display.i945gm || display.i915gm {
            CrtcFuncs::I915gm
        } else if display.display_ver == 3 {
            CrtcFuncs::I915
        } else {
            CrtcFuncs::I8xx
        }
    } else if display.display_ver >= 8 {
        CrtcFuncs::Bdw
    } else {
        CrtcFuncs::Ilk
    }
}
// upstream: intel_crtc.c reorder_pipe()
pub fn reorder_pipe(display: Display, pipe: u8) -> u8 {
    if !display.dgfx || display.pipe_mask & (1 << 1) == 0 || display.pipe_mask & (1 << 2) == 0 {
        return pipe;
    }
    match pipe {
        1 => 2,
        2 => 1,
        _ => pipe,
    }
}
// upstream: intel_crtc.c intel_crtc_init()
pub fn intel_crtc_init(io: &mut impl CrtcIo, display: Display) -> i32 {
    let _ = io.debug_num_pipes();
    for pipe in 0..8 {
        if display.pipe_mask & (1 << pipe) == 0 {
            continue;
        }
        if let Err(error) = __intel_crtc_init(io, display, reorder_pipe(display, pipe)) {
            return match error {
                CrtcError::NoMemory => -12,
                CrtcError::Framework(e) => e,
                CrtcError::NoDevice => -19,
            };
        }
    }
    0
}
// upstream: intel_crtc.c intel_crtc_get_pipe_from_crtc_id_ioctl()
pub fn intel_crtc_get_pipe_from_crtc_id_ioctl(
    io: &mut impl CrtcIo,
    crtc_id: u32,
) -> Result<u8, i32> {
    io.crtc_find_pipe(crtc_id).ok_or(-2)
}
// upstream: intel_crtc.c intel_crtc_needs_vblank_work()
pub fn intel_crtc_needs_vblank_work(io: &impl CrtcIo, state: &CrtcState) -> bool {
    io.crtc_state_needs_vblank_work(state)
}
// upstream: intel_crtc.c intel_crtc_vblank_work()
pub fn intel_crtc_vblank_work(io: &mut impl CrtcIo, crtc: &Crtc, state: &mut CrtcState) {
    io.vblank_work_start(crtc.pipe);
    io.load_luts(crtc.pipe);
    if let Some(event) = state.event {
        io.event_lock();
        io.send_vblank_event(crtc.crtc_id, event);
        io.event_unlock();
        state.event = None;
    }
    io.vblank_work_end(crtc.pipe);
}
// upstream: intel_crtc.c intel_crtc_vblank_work_init()
pub fn intel_crtc_vblank_work_init(io: &mut impl CrtcIo, crtc: &Crtc, state: &CrtcState) {
    io.vblank_work_init(crtc.pipe);
    io.cpu_latency_qos_update(crtc.state_id, 0);
    let _ = state;
}
// upstream: intel_crtc.c intel_wait_for_vblank_workers()
pub fn intel_wait_for_vblank_workers(io: &mut impl CrtcIo, states: &[AtomicCrtcChange]) {
    for change in states {
        if !intel_crtc_needs_vblank_work(io, &change.new) {
            continue;
        }
        io.flush_vblank_work(change.pipe);
        io.cpu_latency_qos_update(change.pipe as u64, PM_QOS_DEFAULT_VALUE);
    }
}
// upstream: intel_crtc.c intel_usecs_to_scanlines()
pub fn intel_usecs_to_scanlines(io: &impl CrtcIo, mode_clock: u32, htotal: u32, usecs: u32) -> u32 {
    if htotal == 0 {
        1
    } else {
        io.usecs_to_scanlines(mode_clock, htotal, usecs)
    }
}
// upstream: intel_crtc.c intel_scanlines_to_usecs()
pub fn intel_scanlines_to_usecs(mode_clock: u32, htotal: u32, scanlines: u32) -> u32 {
    if mode_clock == 0 {
        1
    } else {
        ((u64::from(scanlines) * u64::from(htotal) * 1000 + u64::from(mode_clock) - 1)
            / u64::from(mode_clock)) as u32
    }
}

// upstream: intel_crtc.c intel_pipe_update_start()
pub fn intel_pipe_update_start(
    io: &mut impl CrtcIo,
    crtc: &mut Crtc,
    old: &CrtcState,
    new: &mut CrtcState,
    legacy_cursor_update: bool,
) {
    io.debug_use_dsb(crtc.pipe);
    io.psr_lock(crtc.pipe);
    if new.do_async_flip {
        intel_crtc_prepare_vblank_event(io, new, &mut crtc.flip_done_event);
        return;
    }
    if intel_crtc_needs_vblank_work(io, new) {
        intel_crtc_vblank_work_init(io, crtc, new);
    }
    if legacy_cursor_update {
        for index in io.legacy_cursor_plane_indices(crtc.pipe) {
            if io.legacy_cursor_old_plane_crtc(index, crtc.pipe) {
                io.prepare_cursor_vblank_work(crtc.pipe);
            }
        }
    }
    let (min_vbl, max_vbl) = io.vblank_evade_init(crtc.pipe, old, new);
    if io.drm_crtc_vblank_get(crtc.crtc_id) != 0 {
        io.local_irq_disable();
        return;
    }
    io.psr_wait_idle(crtc.pipe);
    io.local_irq_disable();
    let scanline = io.vblank_evade(crtc.pipe);
    io.drm_crtc_vblank_put(crtc.crtc_id);
    crtc.debug.min_vbl = min_vbl;
    crtc.debug.max_vbl = max_vbl;
    crtc.debug.scanline_start = scanline;
    crtc.debug.start_vbl_time = io.vblank_timestamp_ns();
    crtc.debug.start_vbl_count = io.intel_vblank_count(crtc.pipe);
    io.trace(20, crtc.pipe, scanline as u64);
}
// upstream: intel_crtc.c dbg_vblank_evade()
pub fn dbg_vblank_evade(io: &mut impl CrtcIo, crtc: &mut Crtc, end_ns: u64) {
    let delta = end_ns.saturating_sub(crtc.debug.start_vbl_time);
    let shift = delta >> 9;
    let bucket = if shift == 0 {
        0
    } else {
        63 - shift.leading_zeros()
    }
    .min(31) as usize;
    crtc.debug.histogram[bucket] = crtc.debug.histogram[bucket].saturating_add(1);
    crtc.debug.vbl_sum = crtc.debug.vbl_sum.saturating_add(delta);
    if crtc.debug.vbl_min == 0 || delta < crtc.debug.vbl_min {
        crtc.debug.vbl_min = delta;
    }
    if delta > crtc.debug.vbl_max {
        crtc.debug.vbl_max = delta;
    }
    let over = delta > 1000 * 100;
    if over {
        crtc.debug.vbl_over = crtc.debug.vbl_over.saturating_add(1);
    }
    io.debug_vblank_update(crtc.pipe, delta, bucket as u32, over);
}
// upstream: intel_crtc.c dbg_vblank_evade()
pub fn dbg_vblank_evade_disabled(_io: &mut impl CrtcIo, _crtc: &mut Crtc, _end_ns: u64) {}
// upstream: intel_crtc.c intel_crtc_arm_vblank_event()
pub fn intel_crtc_arm_vblank_event(io: &mut impl CrtcIo, crtc: &Crtc, state: &mut CrtcState) {
    let Some(event) = state.event else {
        return;
    };
    if io.drm_crtc_vblank_get(crtc.crtc_id) != 0 {
        io.warn(2, crtc.crtc_id as i64);
    }
    io.event_lock();
    io.send_vblank_event(crtc.crtc_id, event);
    io.event_unlock();
    state.event = None;
}
// upstream: intel_crtc.c intel_crtc_prepare_vblank_event()
pub fn intel_crtc_prepare_vblank_event(
    io: &mut impl CrtcIo,
    state: &mut CrtcState,
    event_out: &mut Option<u64>,
) {
    io.event_lock();
    *event_out = state.event;
    io.event_unlock();
    state.event = None;
}
// upstream: intel_crtc.c intel_pipe_update_end()
pub fn intel_pipe_update_end(
    io: &mut impl CrtcIo,
    crtc: &mut Crtc,
    state: &mut CrtcState,
    legacy_cursor_update: bool,
    pipe_has_dsi: bool,
) {
    let scanline_end = io.scanline(crtc.pipe);
    let end_vblank = io.intel_vblank_count(crtc.pipe);
    let end_time = io.vblank_timestamp_ns();
    io.debug_use_dsb(crtc.pipe);
    if state.do_async_flip {
        io.psr_unlock(crtc.pipe);
        return;
    }
    io.trace(21, crtc.pipe, end_vblank as u64);
    if state.display.display_ver >= 11 && pipe_has_dsi {
        io.dsi_frame_update(crtc.pipe);
    }
    if intel_crtc_needs_vblank_work(io, state) {
        io.vblank_work_schedule(crtc.pipe, end_vblank.wrapping_add(1));
    } else {
        intel_crtc_arm_vblank_event(io, crtc, state);
    }
    if legacy_cursor_update {
        for index in io.legacy_cursor_plane_indices(crtc.pipe) {
            if io.legacy_cursor_old_plane_crtc(index, crtc.pipe)
                && io.legacy_cursor_has_unpin_vblank_work(index)
            {
                io.schedule_unpin_work(crtc.pipe, index, end_vblank.wrapping_add(1));
                io.clear_removed_plane(index);
            }
        }
    }
    if !legacy_cursor_update || (io.use_trans_push(crtc.pipe) && !state.vrr_enable) {
        io.vrr_trans_push(crtc.pipe);
    }
    io.local_irq_enable();
    if io.parent_vgpu_active() {
        io.psr_unlock(crtc.pipe);
        return;
    }
    if crtc.debug.start_vbl_count != 0 && crtc.debug.start_vbl_count != end_vblank {
        let delta = (end_time as i64 - crtc.debug.start_vbl_time as i64) / 1000;
        io.debug_pipe_update_error(
            crtc.pipe,
            crtc.debug.start_vbl_count,
            end_vblank,
            delta,
            crtc.debug.min_vbl,
            crtc.debug.max_vbl,
            crtc.debug.scanline_start,
            scanline_end,
        );
    }
    dbg_vblank_evade(io, crtc, end_time);
    io.psr_unlock(crtc.pipe);
}
// upstream: intel_crtc.c intel_crtc_enable_changed()
pub fn intel_crtc_enable_changed(old: &CrtcState, new: &CrtcState) -> bool {
    old.enable != new.enable
}
// upstream: intel_crtc.c intel_any_crtc_enable_changed()
pub fn intel_any_crtc_enable_changed(io: &mut impl CrtcIo) -> bool {
    io.crtc_state_iter()
        .iter()
        .any(|change| intel_crtc_enable_changed(&change.old, &change.new))
}
// upstream: intel_crtc.c intel_crtc_active_changed()
pub fn intel_crtc_active_changed(old: &CrtcState, new: &CrtcState) -> bool {
    old.active != new.active
}
// upstream: intel_crtc.c intel_any_crtc_active_changed()
pub fn intel_any_crtc_active_changed(io: &mut impl CrtcIo) -> bool {
    io.crtc_state_iter()
        .iter()
        .any(|change| intel_crtc_active_changed(&change.old, &change.new))
}
// upstream: intel_crtc.c intel_crtc_bw_num_active_planes()
pub fn intel_crtc_bw_num_active_planes(state: &CrtcState) -> u32 {
    (state.active_planes & !(1 << PLANE_CURSOR)).count_ones()
}
// upstream: intel_crtc.c intel_crtc_bw_data_rate()
pub fn intel_crtc_bw_data_rate(state: &CrtcState) -> u32 {
    let mut rate = 0u32;
    for plane in 0..state.data_rate.len() {
        if plane == PLANE_CURSOR as usize {
            continue;
        }
        rate = rate.wrapping_add(state.data_rate[plane]);
        if state.display.display_ver < 11 {
            rate = rate.wrapping_add(state.data_rate_y[plane]);
        }
    }
    rate
}
// upstream: intel_crtc.c intel_crtc_bw_min_cdclk()
pub fn intel_crtc_bw_min_cdclk(state: &CrtcState) -> u32 {
    if state.display.display_ver < 12 {
        0
    } else {
        (u64::from(intel_crtc_bw_data_rate(state)) * 10).div_ceil(512) as u32
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    struct TestIo;
    impl CrtcIo for TestIo {}

    #[test]
    fn vblank_counter_and_platform_reorder_follow_display_policy() {
        let gen13 = Display {
            display_ver: 13,
            ..Display::default()
        };
        assert_eq!(
            intel_crtc_max_vblank_count(gen13, &CrtcState::default()),
            u32::MAX
        );
        assert_eq!(
            intel_crtc_max_vblank_count(
                Display {
                    display_ver: 3,
                    ..gen13
                },
                &CrtcState::default()
            ),
            0x00ff_ffff
        );
        assert_eq!(
            intel_crtc_max_vblank_count(
                Display {
                    display_ver: 2,
                    ..gen13
                },
                &CrtcState::default()
            ),
            0
        );
        assert_eq!(
            intel_crtc_max_vblank_count(
                gen13,
                &CrtcState {
                    mode_flags: I915_MODE_FLAG_DSI_USE_TE1,
                    ..CrtcState::default()
                }
            ),
            0
        );
        let dgfx = Display {
            dgfx: true,
            pipe_mask: 0b111,
            ..gen13
        };
        assert_eq!(reorder_pipe(dgfx, 1), 2);
        assert_eq!(reorder_pipe(dgfx, 2), 1);
        assert_eq!(
            reorder_pipe(
                Display {
                    pipe_mask: 0b011,
                    ..dgfx
                },
                1
            ),
            1
        );
    }

    #[test]
    fn pipe_bandwidth_excludes_cursor_and_cdclk_ceil_matches_source() {
        let state = CrtcState {
            display: Display {
                display_ver: 10,
                ..Display::default()
            },
            active_planes: 0xff,
            data_rate: [10, 20, 30, 40, 50, 60, 70, 1000],
            data_rate_y: [1, 2, 3, 4, 5, 6, 7, 100],
            ..CrtcState::default()
        };
        assert_eq!(intel_crtc_bw_num_active_planes(&state), 7);
        assert_eq!(intel_crtc_bw_data_rate(&state), 308);
        assert_eq!(intel_crtc_bw_min_cdclk(&state), 0);
        let gen12 = CrtcState {
            display: Display {
                display_ver: 12,
                ..Display::default()
            },
            ..state
        };
        assert_eq!(intel_crtc_bw_min_cdclk(&gen12), (2800_u32 + 511) / 512);
        assert_eq!(intel_usecs_to_scanlines(&TestIo, 148_500, 2_200, 100), 7);
        assert_eq!(intel_scanlines_to_usecs(148_500, 2_200, 1), 15);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TestIo {
        preempt_rt: bool,
        software_count: u32,
        accurate_count: u32,
        hardware_count: u32,
    }
    impl CrtcIo for TestIo {
        fn preempt_rt(&self) -> bool {
            self.preempt_rt
        }
        fn drm_crtc_vblank_count(&mut self, _crtc: u32) -> u32 {
            self.software_count
        }
        fn drm_crtc_accurate_vblank_count(&mut self, _crtc: u32) -> u32 {
            self.accurate_count
        }
        fn get_vblank_counter(&mut self, _crtc: u32) -> u32 {
            self.hardware_count
        }
    }

    #[test]
    fn vblank_counter_uses_runtime_and_generation_policy() {
        let display = Display {
            display_ver: 13,
            ..Display::default()
        };
        assert_eq!(
            intel_crtc_max_vblank_count(display, &CrtcState::default()),
            u32::MAX
        );
        assert_eq!(
            intel_crtc_max_vblank_count(
                Display {
                    display_ver: 3,
                    ..display
                },
                &CrtcState::default()
            ),
            0x00ff_ffff
        );
        assert_eq!(
            intel_crtc_max_vblank_count(
                Display {
                    display_ver: 2,
                    ..display
                },
                &CrtcState::default()
            ),
            0
        );
        assert_eq!(
            intel_crtc_max_vblank_count(
                display,
                &CrtcState {
                    mode_flags: I915_MODE_FLAG_DSI_USE_TE0,
                    ..CrtcState::default()
                }
            ),
            0
        );
        let crtc = Crtc {
            crtc_id: 7,
            active: true,
            ..Crtc::default()
        };
        let mut io = TestIo {
            preempt_rt: true,
            software_count: 11,
            accurate_count: 12,
            hardware_count: 99,
        };
        assert_eq!(intel_crtc_get_vblank_counter(&mut io, &crtc, 0), 11);
        io.preempt_rt = false;
        assert_eq!(intel_crtc_get_vblank_counter(&mut io, &crtc, 0), 12);
        assert_eq!(intel_crtc_get_vblank_counter(&mut io, &crtc, u32::MAX), 99);
        assert_eq!(
            intel_crtc_get_vblank_counter(&mut io, &Crtc::default(), u32::MAX),
            0
        );
    }

    #[test]
    fn pipe_reorder_bandwidth_and_timing_rounding_follow_source() {
        let display = Display {
            display_ver: 10,
            dgfx: true,
            pipe_mask: 0b111,
            ..Display::default()
        };
        assert_eq!(reorder_pipe(display, 1), 2);
        assert_eq!(reorder_pipe(display, 2), 1);
        assert_eq!(
            reorder_pipe(
                Display {
                    pipe_mask: 0b011,
                    ..display
                },
                1
            ),
            1
        );
        let state = CrtcState {
            display,
            active_planes: 0b1111_1111,
            data_rate: [10, 20, 30, 40, 50, 60, 70, 1000],
            data_rate_y: [1, 2, 3, 4, 5, 6, 7, 100],
            ..CrtcState::default()
        };
        assert_eq!(intel_crtc_bw_num_active_planes(&state), 7);
        assert_eq!(intel_crtc_bw_data_rate(&state), 308);
        assert_eq!(intel_crtc_bw_min_cdclk(&state), 0);
        assert_eq!(
            intel_usecs_to_scanlines(&TestIo::default(), 148_500, 2_200, 100),
            7
        );
        assert_eq!(intel_scanlines_to_usecs(148_500, 2_200, 1), 15);
    }
}
