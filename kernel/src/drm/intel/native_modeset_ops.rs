//! Hardware callback contract for the translated Native atomic commit tail.
//!
//! The checked pipe projection and register-writer wrappers live here, but
//! this is not yet a complete atomic dispatcher/backend: the opt-in selector
//! remains fail-closed until every callback is backed by power ownership,
//! readback, and rollback. Keeping callbacks explicit prevents a generic
//! commit-tail action from being reported as successful by an empty hook.

use intel_display::intel_display_modeset_full::{
    EncoderTransition, PipeState, PipeTransition, PlaneTransition,
};

use super::{
    native_pipe::{self, NativePipeDisableError},
    pipe::{self, MultiPlaneArmPlan, MultiPlaneDdbPlan, PlaneScanout, WatermarkConfig},
    regs::Registers,
};
use crate::drm::modes::{Mode, ModeFlags, TimingSource};

/// A plane's complete state at the source-atomic boundary. `PlaneTransition`
/// describes DRM changes, but intentionally does not carry the backing GGTT
/// address or the engine index used by the display-12/13 register planner.
/// Those are resolved by the KMS state owner and must be supplied explicitly;
/// deriving either from the DRM object id is not safe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NativePlaneState {
    pub(super) transition: PlaneTransition,
    /// Zero-based universal-plane engine index (not `transition.id`).
    pub(super) engine_index: usize,
    /// Required exactly when the new state is visible. Includes the pinned
    /// allocation size and GGTT address, so DDB/WMs and surface programming
    /// consume the same resolved framebuffer state.
    pub(super) scanout: Option<PlaneScanout>,
}

/// Checked projection of the source CRTC/plane state into the currently
/// translated pipe/DBUF writer. This adapter admits only a complete, linear,
/// unscaled packed-RGB plane set. Unsupported features are rejected before
/// the first register write instead of being silently omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NativePipeProgram {
    source: PipeState,
    source_pipe: u8,
    pipe: pipe::Pipe,
    mode: Mode,
    scanouts: alloc::vec::Vec<PlaneScanout>,
    disabled_planes: alloc::vec::Vec<usize>,
    watermarks: WatermarkConfig,
}

/// Checked old-state projection for a CRTC disable. Kept distinct from an
/// enabled program so a disabled target can never be accidentally sent down
/// the pipe-config/plane-arm path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NativePipeDisableProgram {
    pipe: pipe::Pipe,
    plane_indices: alloc::vec::Vec<usize>,
}

/// Token produced after DBUF/watermark and plane shadow writes. It is valid
/// only for `NativePipeProgram::arm_plane_update`, so the commit-tail owner
/// can place the arm after the CRTC enable callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NativePlaneUpdatePlan {
    ddb: MultiPlaneDdbPlan,
    arm: MultiPlaneArmPlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativePipeProjectionError {
    InvalidPipe(u8),
    PipeMismatch { old: u8, new: u8 },
    InactivePipe,
    UnsupportedPipeState,
    InvalidTiming,
    PlanePipeMismatch { plane: u8, pipe: u8 },
    InvalidPlaneIndex(usize),
    DuplicatePlaneIndex(usize),
    MissingScanout(usize),
    UnexpectedScanout(usize),
    PlaneMaskMismatch { expected: u32, projected: u32 },
    PlaneStateMismatch(usize),
    UnsupportedOldPipeState,
    TooManyPlanes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NativePipeProgramError {
    Projection(NativePipeProjectionError),
    Pipe(pipe::PipeError),
    Disable(NativePipeDisableError),
}

impl NativePipeProgram {
    /// Resolve the source pipe/plane transition before touching MMIO.
    ///
    /// `planes` must be the complete post-atomic plane set for this CRTC, not
    /// only the planes whose DRM properties changed. Its visible mask must
    /// agree with `PipeState::enabled_planes`, which lets the projection prove
    /// that neither a currently visible plane nor a disabling old plane was
    /// dropped by the caller.
    pub(super) fn from_source(
        transition: &PipeTransition,
        planes: &[NativePlaneState],
        watermarks: WatermarkConfig,
    ) -> Result<Self, NativePipeProjectionError> {
        let mode = mode_from_source(transition.new.pipe_mode)
            .ok_or(NativePipeProjectionError::InvalidTiming)?;
        Self::from_source_with_mode(transition, planes, watermarks, mode)
    }

    /// Project a complete source transition while retaining the exact crate
    /// mode resolved from the DRM framebuffer/CRTC state. Source `Timing`
    /// omits polarity and provenance, so production KMS callers must use this
    /// entry point rather than reconstructing those fields from timing totals.
    /// Interlace and double-clock are deliberately unsupported by this
    /// single-scanout writer and fail before any register access.
    pub(super) fn from_source_with_mode(
        transition: &PipeTransition,
        planes: &[NativePlaneState],
        watermarks: WatermarkConfig,
        mode: Mode,
    ) -> Result<Self, NativePipeProjectionError> {
        let old = transition.old.pipe;
        let new = transition.new.pipe;
        if old != new {
            return Err(NativePipeProjectionError::PipeMismatch { old, new });
        }
        let pipe = pipe_for_source(new)?;
        let state = &transition.new;

        // The current writer models one active CPU transcoder per pipe and
        // cannot preserve joiner, DSI, DSC, VRR, scaling, or YUV state.
        if !state.hw_enable || !state.hw_active || !state.uapi_enable || !state.uapi_active {
            return Err(NativePipeProjectionError::InactivePipe);
        }
        if state.cpu_transcoder != new
            || state.is_joiner_secondary
            || state.joiner_pipes != 0
            || state.port_sync_slave
            || state.port_sync_master
            || state.port_sync_mode
            || state.transcoder_is_dsi
            || state.is_dsi_output
            || state.has_dsc
            || state.vrr_enabled
            || state.vrr_enabling
            || state.vrr_disabling
            || state.has_ips
            || state.has_dsb
            || state.use_dsb
            || state.use_flipq
            || state.scaler_update_needed
            || state.scalers_setup_needed
            || state.scaled_planes != 0
            || state.nv12_planes != 0
            || state.c8_planes != 0
            || state.is_yuv_output
            || state.output_format != intel_display::intel_display_modeset_full::OutputFormat::Rgb
            || state.sink_format != intel_display::intel_display_modeset_full::OutputFormat::Rgb
            || state.pipe_bpp != 24
            || state.dither
            || state.dither_force_disable
            || state.double_wide
        {
            return Err(NativePipeProjectionError::UnsupportedPipeState);
        }
        if !mode.flags.is_empty() || !mode_matches_source_timing(mode, state.pipe_mode) {
            return Err(NativePipeProjectionError::InvalidTiming);
        }

        if planes.len() > 4 {
            return Err(NativePipeProjectionError::TooManyPlanes);
        }
        let mut seen = [false; 4];
        let mut old_mask = 0u32;
        let mut projected_mask = 0u32;
        let mut scanouts = alloc::vec::Vec::with_capacity(planes.len());
        let mut disabled_planes = alloc::vec::Vec::with_capacity(planes.len());
        for plane in planes {
            let update = &plane.transition;
            if update.pipe != new {
                return Err(NativePipeProjectionError::PlanePipeMismatch {
                    plane: update.pipe,
                    pipe: new,
                });
            }
            let index = plane.engine_index;
            if index >= seen.len() {
                return Err(NativePipeProjectionError::InvalidPlaneIndex(index));
            }
            if core::mem::replace(&mut seen[index], true) {
                return Err(NativePipeProjectionError::DuplicatePlaneIndex(index));
            }
            if update.old_visible {
                old_mask |= 1u32 << index;
            }
            match (update.new_visible, plane.scanout) {
                (true, Some(scanout)) => {
                    if scanout.plane_index != index
                        || scanout.pixel_format != update.new_format
                        || !scanout_matches_transition(scanout, update)
                    {
                        return Err(NativePipeProjectionError::PlaneStateMismatch(index));
                    }
                    projected_mask |= 1u32 << index;
                    scanouts.push(scanout);
                }
                (true, None) => return Err(NativePipeProjectionError::MissingScanout(index)),
                (false, Some(_)) => {
                    return Err(NativePipeProjectionError::UnexpectedScanout(index));
                }
                (false, None) if update.old_visible => disabled_planes.push(index),
                (false, None) => {}
            }
        }
        if transition.old.enabled_planes != old_mask
            || transition.old.active_planes != old_mask
            || state.enabled_planes != projected_mask
            || state.active_planes != projected_mask
        {
            return Err(NativePipeProjectionError::PlaneMaskMismatch {
                expected: transition.old.enabled_planes
                    | transition.old.active_planes
                    | state.enabled_planes
                    | state.active_planes,
                projected: old_mask | projected_mask,
            });
        }
        Ok(Self {
            source: *state,
            source_pipe: new,
            pipe,
            mode,
            scanouts,
            disabled_planes,
            watermarks,
        })
    }

    /// Program source `update_pipe`'s pipe timing and PIPE_MISC/ARB state. The
    /// commit-tail owner calls this before its CRTC enable phase.
    pub(super) fn configure_pipe(
        &self,
        regs: &impl Registers,
    ) -> Result<(), NativePipeProgramError> {
        pipe::program_multi_plane_pipe_config(regs, self.pipe, &self.mode)
            .map_err(NativePipeProgramError::Pipe)
    }

    /// Program the DDB allocation, watermarks and plane shadow state at the
    /// source universal-plane update point (after CRTC enable). No plane is
    /// armed until `arm_plane_update` is called.
    pub(super) fn prepare_plane_update(
        &self,
        regs: &impl Registers,
    ) -> Result<NativePlaneUpdatePlan, NativePipeProgramError> {
        let (ddb, arm) = pipe::prepare_multi_plane_scanout(
            regs,
            self.pipe,
            &self.mode,
            &self.scanouts,
            self.watermarks,
        )
        .map_err(NativePipeProgramError::Pipe)?;
        Ok(NativePlaneUpdatePlan { ddb, arm })
    }

    /// Arm all validated plane shadow states after CRTC enable and every DDB / WM write.
    pub(super) fn arm_plane_update(
        &self,
        regs: &impl Registers,
        plan: &NativePlaneUpdatePlan,
    ) -> Result<(), NativePipeProgramError> {
        pipe::arm_multi_plane_scanout(regs, &plan.arm).map_err(NativePipeProgramError::Pipe)
    }

    pub(super) fn ddb_plan<'a>(&self, plan: &'a NativePlaneUpdatePlan) -> &'a MultiPlaneDdbPlan {
        &plan.ddb
    }

    /// Disable only planes removed while this CRTC remains active. Full CRTC
    /// shutdown uses `NativePipeDisableProgram::disable` instead.
    pub(super) fn disable_plane_updates(
        &self,
        regs: &impl Registers,
    ) -> Result<(), NativePipeProgramError> {
        for &plane_index in &self.disabled_planes {
            pipe::disable_multi_plane(regs, self.pipe, plane_index)
                .map_err(NativePipeProgramError::Pipe)?;
        }
        Ok(())
    }

    /// Disable one changed plane without stopping the still-active CRTC.
    /// Returns success for unchanged/nonvisible planes and rejects a bogus
    /// disable record rather than issuing a write against an inferred id.
    pub(super) fn disable_plane(
        regs: &impl Registers,
        plane: &NativePlaneState,
    ) -> Result<(), NativePipeProgramError> {
        if plane.transition.new_visible || !plane.transition.old_visible {
            return Ok(());
        }
        if plane.engine_index >= 4 {
            return Err(NativePipeProgramError::Projection(
                NativePipeProjectionError::InvalidPlaneIndex(plane.engine_index),
            ));
        }
        pipe::disable_multi_plane(
            regs,
            pipe_for_source(plane.transition.pipe).map_err(NativePipeProgramError::Projection)?,
            plane.engine_index,
        )
        .map_err(NativePipeProgramError::Pipe)
    }

    pub(super) const fn source_pipe(&self) -> u8 {
        self.source_pipe
    }

    pub(super) const fn source_state(&self) -> &PipeState {
        &self.source
    }

    /// Exact resolved crate mode retained by `from_source_with_mode`.
    pub(super) const fn mode(&self) -> &Mode {
        &self.mode
    }
}

impl NativePipeDisableProgram {
    /// Resolve a complete old plane set for the source disable callback. The
    /// caller supplies per-plane engine indices explicitly; source DRM ids
    /// are never treated as register indices.
    pub(super) fn from_source(
        transition: &PipeTransition,
        planes: &[NativePlaneState],
    ) -> Result<Self, NativePipeProjectionError> {
        let old = transition.old.pipe;
        let new = transition.new.pipe;
        if old != new {
            return Err(NativePipeProjectionError::PipeMismatch { old, new });
        }
        let pipe = pipe_for_source(old)?;
        if !transition.old.hw_active
            || !transition.old.hw_enable
            || transition.old.cpu_transcoder != old
            || transition.old.is_joiner_secondary
            || transition.old.joiner_pipes != 0
            || transition.old.transcoder_is_dsi
            || transition.old.is_dsi_output
            || transition.old.has_dsc
        {
            return Err(NativePipeProjectionError::UnsupportedOldPipeState);
        }
        if transition.new.hw_enable
            || transition.new.hw_active
            || transition.new.uapi_enable
            || transition.new.uapi_active
        {
            return Err(NativePipeProjectionError::UnsupportedPipeState);
        }
        if planes.len() > 4 {
            return Err(NativePipeProjectionError::TooManyPlanes);
        }
        let mut seen = [false; 4];
        let mut old_mask = 0u32;
        let mut plane_indices = alloc::vec::Vec::with_capacity(planes.len());
        for plane in planes {
            let state = &plane.transition;
            if state.pipe != old {
                return Err(NativePipeProjectionError::PlanePipeMismatch {
                    plane: state.pipe,
                    pipe: old,
                });
            }
            let index = plane.engine_index;
            if index >= seen.len() {
                return Err(NativePipeProjectionError::InvalidPlaneIndex(index));
            }
            if core::mem::replace(&mut seen[index], true) {
                return Err(NativePipeProjectionError::DuplicatePlaneIndex(index));
            }
            if state.old_visible {
                old_mask |= 1u32 << index;
                plane_indices.push(index);
            }
            if plane.scanout.is_some() {
                return Err(NativePipeProjectionError::UnexpectedScanout(index));
            }
        }
        if transition.old.enabled_planes != old_mask || transition.old.active_planes != old_mask {
            return Err(NativePipeProjectionError::PlaneMaskMismatch {
                expected: transition.old.enabled_planes | transition.old.active_planes,
                projected: old_mask,
            });
        }
        Ok(Self {
            pipe,
            plane_indices,
        })
    }

    pub(super) fn disable(&self, regs: &impl Registers) -> Result<(), NativePipeProgramError> {
        native_pipe::disable_pipe(regs, self.pipe, &self.plane_indices)
            .map_err(NativePipeProgramError::Disable)
    }
}

fn pipe_for_source(index: u8) -> Result<pipe::Pipe, NativePipeProjectionError> {
    match index {
        0 => Ok(pipe::Pipe::A),
        1 => Ok(pipe::Pipe::B),
        2 => Ok(pipe::Pipe::C),
        3 => Ok(pipe::Pipe::D),
        _ => Err(NativePipeProjectionError::InvalidPipe(index)),
    }
}

fn mode_from_source(timing: intel_display::intel_display_modeset_full::Timing) -> Option<Mode> {
    let cvt = |value: u32| u16::try_from(value).ok();
    let mode = Mode {
        clock_khz: timing.clock_khz,
        hdisplay: cvt(timing.hdisplay)?,
        hsync_start: cvt(timing.hsync_start)?,
        hsync_end: cvt(timing.hsync_end)?,
        htotal: cvt(timing.htotal)?,
        vdisplay: cvt(timing.vdisplay)?,
        vsync_start: cvt(timing.vsync_start)?,
        vsync_end: cvt(timing.vsync_end)?,
        vtotal: cvt(timing.vtotal)?,
        hsync_positive: false,
        vsync_positive: false,
        flags: ModeFlags::NONE,
        source: TimingSource::Firmware,
    };
    (mode.clock_khz != 0
        && mode.hdisplay != 0
        && mode.vdisplay != 0
        && mode.hdisplay < mode.hsync_start
        && mode.hsync_start < mode.hsync_end
        && mode.hsync_end <= mode.htotal
        && mode.vdisplay < mode.vsync_start
        && mode.vsync_start < mode.vsync_end
        && mode.vsync_end <= mode.vtotal
        && timing.hblank_start == timing.hdisplay
        && timing.hblank_end == timing.htotal
        && timing.vblank_start == timing.vdisplay
        && timing.vblank_end == timing.vtotal)
        .then_some(mode)
}

fn mode_matches_source_timing(
    mode: Mode,
    timing: intel_display::intel_display_modeset_full::Timing,
) -> bool {
    u32::from(mode.hdisplay) == timing.hdisplay
        && u32::from(mode.hsync_start) == timing.hsync_start
        && u32::from(mode.hsync_end) == timing.hsync_end
        && u32::from(mode.htotal) == timing.htotal
        && timing.hblank_start == timing.hdisplay
        && timing.hblank_end == timing.htotal
        && u32::from(mode.vdisplay) == timing.vdisplay
        && u32::from(mode.vsync_start) == timing.vsync_start
        && u32::from(mode.vsync_end) == timing.vsync_end
        && u32::from(mode.vtotal) == timing.vtotal
        && timing.vblank_start == timing.vdisplay
        && timing.vblank_end == timing.vtotal
        && mode.clock_khz == timing.clock_khz
        && mode.hdisplay != 0
        && mode.vdisplay != 0
        && mode.clock_khz != 0
        && mode.hdisplay < mode.hsync_start
        && mode.hsync_start < mode.hsync_end
        && mode.hsync_end <= mode.htotal
        && mode.vdisplay < mode.vsync_start
        && mode.vsync_start < mode.vsync_end
        && mode.vsync_end <= mode.vtotal
}

fn scanout_matches_transition(scanout: PlaneScanout, state: &PlaneTransition) -> bool {
    let integral = |value: i32| value >= 0 && (value as u32 & 0xffff) == 0;
    // DRM's source and destination rectangles are x1/y1/x2/y2, not
    // x/y/width/height. The helper supports a full-frame source only (it
    // programs PLANE_OFFSET=0), so crop origins and fractional scaling are
    // rejected rather than rounded.
    let source_end = |value: i32| (value as u32) >> 16;
    let dst_end = |value: i32| value as u32;
    state.new_src_rect.iter().all(|&v| integral(v))
        && state.new_src_rect[0] == 0
        && state.new_src_rect[1] == 0
        && scanout.source_width == source_end(state.new_src_rect[2])
        && scanout.source_height == source_end(state.new_src_rect[3])
        && state.new_dst_rect.iter().all(|&v| v >= 0)
        && scanout.dst_x == state.new_dst_rect[0] as u32
        && scanout.dst_y == state.new_dst_rect[1] as u32
        && state.new_dst_rect[2] > state.new_dst_rect[0]
        && state.new_dst_rect[3] > state.new_dst_rect[1]
        && scanout.dst_width == dst_end(state.new_dst_rect[2]) - dst_end(state.new_dst_rect[0])
        && scanout.dst_height == dst_end(state.new_dst_rect[3]) - dst_end(state.new_dst_rect[1])
        && scanout.surface.stride_bytes == state.new_mapping_stride
        && state.new_modifier == 0
        && state.new_rotation == 0
}

/// Native hardware operations required by the display-12/13 atomic commit
/// sequence. Implementations must return an error on unsupported state and
/// must not use a successful no-op as a placeholder.
pub(super) trait NativeCdclkOps {
    type Error;

    /// Pre-plane CDCLK transition (`intel_cdclk_set_cdclk()`), including the
    /// required PCode and peripheral-ordering hooks.
    fn set_cdclk_pre_plane(&mut self, target_khz: u32) -> Result<(), Self::Error>;

    /// Post-plane CDCLK transition/readback (`intel_cdclk_set_cdclk()`).
    fn set_cdclk_post_plane(&mut self, target_khz: u32) -> Result<(), Self::Error>;
}

/// Shared-PLL callbacks use the source manager's typed atomic/CRTC/encoder
/// state. A DRM projection alone is not enough to synthesize this state.
pub(super) trait NativeDpllOps {
    /// Compute/release/reserve and swap the shared PLL state
    /// (`intel_dpll_compute()` / `intel_dpll_reserve()` / `intel_dpll_swap_state()`).
    fn dpll_get(
        &mut self,
        atomic: &mut intel_display::intel_dpll_mgr_full::IntelAtomicState,
        crtc: &intel_display::intel_dpll_mgr_full::IntelCrtc,
        encoder: &intel_display::intel_dpll_mgr_full::IntelEncoder,
    ) -> Result<(), super::shared_dpll::DpllFailure>;

    /// Enable the reserved PLL (`intel_enable_shared_dpll()`).
    fn dpll_enable(
        &mut self,
        state: &intel_display::intel_dpll_mgr_full::CrtcState,
    ) -> Result<(), super::shared_dpll::DpllFailure>;

    /// Disable/release an old PLL (`intel_disable_shared_dpll()`).
    fn dpll_disable(
        &mut self,
        state: &intel_display::intel_dpll_mgr_full::CrtcState,
    ) -> Result<(), super::shared_dpll::DpllFailure>;
}

pub(super) trait NativeModesetOps: NativeCdclkOps + NativeDpllOps {
    /// CRTC enable phase (`hsw_crtc_enable()` / `skl_commit_modeset_enables()`).
    /// This gets the same checked program later consumed by plane updates, so
    /// pipe identity/mode cannot be independently re-derived at enable time.
    fn crtc_enable(&mut self, program: &NativePipeProgram) -> Result<(), Self::Error>;

    /// CRTC disable phase (`intel_crtc_disable()` / `intel_commit_modeset_disables()`).
    fn crtc_disable(&mut self, program: &NativePipeDisableProgram) -> Result<(), Self::Error>;

    /// Encoder pre-enable (`intel_encoders_pre_enable()`).
    fn encoder_pre_enable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Encoder enable (`intel_encoders_enable()`).
    fn encoder_enable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Encoder disable (`intel_encoders_disable()`).
    fn encoder_disable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Encoder post-disable (`intel_encoders_post_disable()`).
    fn encoder_post_disable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Pipe timing/PIPE_MISC/ARB configuration (`hsw_crtc_enable()`). The
    /// implementation calls `NativePipeProgram::configure_pipe` before CRTC
    /// enable; it must not program DDB/watermarks or arm planes in this callback.
    fn update_pipe(&mut self, program: &NativePipeProgram) -> Result<(), Self::Error>;

    /// Universal-plane update (`skl_universal_plane` update/arm path), after
    /// CRTC enable. Apply `disable_plane_updates`, then prepare all DDB/WMs and
    /// shadow values, then arm them using the returned update token. Any error
    /// stops the phase; the enclosing transaction must own rollback.
    fn update_plane(&mut self, program: &NativePipeProgram) -> Result<(), Self::Error>;

    /// Universal-plane disable (`skl_universal_plane_disable_arm()`) for an
    /// in-place update that does not disable its CRTC.
    fn disable_plane(&mut self, plane: &NativePlaneState) -> Result<(), Self::Error>;

    /// Acquire a map-backed display power-domain reference (`intel_display_power_get()`).
    fn power_domain_get(&mut self, domain: u8) -> Result<(), Self::Error>;

    /// Release a previously acquired display power-domain reference
    /// (`intel_display_power_put()`).
    fn power_domain_put(&mut self, domain: u8) -> Result<(), Self::Error>;
}
