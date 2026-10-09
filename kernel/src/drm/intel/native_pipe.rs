//! Checked pipe/plane helpers used by a Native atomic commit backend.
//!
//! This module composes the source-derived per-plane, scaler and transcoder
//! operations into a CRTC disable phase. The selector remains fail-closed
//! until the atomic-state projection supplies complete pipe/plane objects and
//! the caller owns rollback for a partial hardware disable.

use super::{
    native_scaler::{self, NativeScalerError},
    output::{self, OutputError},
    pipe::{
        self, MultiPlaneArmPlan, MultiPlaneDdbPlan, Pipe, PipeError, PlaneScanout, WatermarkConfig,
    },
    regs::Registers,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativePipeEnablePlan {
    ddb: MultiPlaneDdbPlan,
    arm: MultiPlaneArmPlan,
}

impl NativePipeEnablePlan {
    pub(crate) fn ddb(&self) -> &MultiPlaneDdbPlan {
        &self.ddb
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativePipeEnableError {
    Pipe(PipeError),
}

/// Prepare an enable without arming a plane. The source commit-tail order is:
/// pipe timing/misc, DBUF and plane shadow state, CRTC/transcoder enable, then
/// [`arm_pipe_enable`] at the plane-update phase.
pub(crate) fn prepare_pipe_enable(
    regs: &impl Registers,
    pipe_id: Pipe,
    mode: &crate::drm::modes::Mode,
    scanouts: &[PlaneScanout],
    config: WatermarkConfig,
) -> Result<NativePipeEnablePlan, NativePipeEnableError> {
    pipe::program_multi_plane_pipe_config(regs, pipe_id, mode)
        .map_err(NativePipeEnableError::Pipe)?;
    let (ddb, arm) = pipe::prepare_multi_plane_scanout(regs, pipe_id, mode, scanouts, config)
        .map_err(NativePipeEnableError::Pipe)?;
    Ok(NativePipeEnablePlan { ddb, arm })
}

/// Arm planes after the caller's checked transcoder/CRTC enable has succeeded.
pub(crate) fn arm_pipe_enable(
    regs: &impl Registers,
    plan: &NativePipeEnablePlan,
) -> Result<(), NativePipeEnableError> {
    pipe::arm_multi_plane_scanout(regs, &plan.arm).map_err(NativePipeEnableError::Pipe)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativePipeDisableError {
    Plane(PipeError),
    Scaler(NativeScalerError),
    Transcoder(OutputError),
}

/// Disable the listed non-cursor planes, detach the pipe's scalers, then stop
/// its transcoder and wait for the status bit to clear. The explicit plane
/// list comes from the checked atomic transition; duplicate or unsupported
/// plane ids are refused by the existing register planner.
pub(crate) fn disable_pipe(
    regs: &impl Registers,
    pipe_id: Pipe,
    plane_indices: &[usize],
) -> Result<(), NativePipeDisableError> {
    let mut seen = [false; 4];
    for &plane_index in plane_indices {
        if plane_index >= seen.len() {
            return Err(NativePipeDisableError::Plane(
                PipeError::PlaneIndexOutOfRange { plane_index },
            ));
        }
        if core::mem::replace(&mut seen[plane_index], true) {
            return Err(NativePipeDisableError::Plane(
                PipeError::DuplicatePlaneIndex { plane_index },
            ));
        }
    }
    for &plane_index in plane_indices {
        pipe::disable_multi_plane(regs, pipe_id, plane_index)
            .map_err(NativePipeDisableError::Plane)?;
    }
    native_scaler::disable_pipe_scalers(regs, pipe_id, false)
        .map_err(NativePipeDisableError::Scaler)?;
    output::disable_transcoder(regs, pipe_id).map_err(NativePipeDisableError::Transcoder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::{
        intel::regs::{mock::MockRegisters, pipe as pipe_regs},
        modes::CTA_VIC_TIMINGS,
    };

    fn mode_1080p() -> crate::drm::modes::Mode {
        CTA_VIC_TIMINGS
            .iter()
            .find(|entry| entry.vic == 16)
            .expect("CTA 1080p60 mode")
            .mode
    }

    fn config() -> WatermarkConfig {
        WatermarkConfig {
            display_ver: 13,
            latencies: [2, 4, 6, 8, 14, 16, 0, 0],
            num_levels: 6,
            sagv_block_time_us: 0,
        }
    }

    fn half_screen(plane_index: usize, dst_x: u32, address: u64) -> PlaneScanout {
        PlaneScanout {
            plane_index,
            pixel_format: intel_display::universal_plane::XRGB8888,
            surface: pipe::PlaneSurface {
                ggtt_address: address,
                stride_bytes: 1920 * 4,
            },
            allocation_bytes: 1920 * 1080 * 4,
            source_width: 960,
            source_height: 1080,
            dst_x,
            dst_y: 0,
            dst_width: 960,
            dst_height: 1080,
        }
    }

    #[test]
    fn prepare_crtc_enable_defers_plane_arm_to_post_enable_phase() {
        let regs = MockRegisters::new();
        let scanouts = [
            half_screen(0, 0, 0x0100_0000),
            half_screen(1, 960, 0x0200_0000),
        ];
        let plan = prepare_pipe_enable(&regs, Pipe::A, &mode_1080p(), &scanouts, config())
            .expect("pipe and plane shadow state should prepare");

        let before = regs.writes();
        assert!(before.iter().any(|(name, _)| *name == "PIPESRC_A"));
        assert!(
            before
                .iter()
                .any(|(name, _)| *name == "PLANE_BUF_CFG_MULTI_1")
        );
        assert!(
            before
                .iter()
                .all(|(name, _)| *name != "PLANE_CTL_MULTI" && *name != "PLANE_SURF_MULTI")
        );

        arm_pipe_enable(&regs, &plan).expect("planes arm only after CRTC enable");
        let after = regs.writes();
        assert_eq!(after[after.len() - 4].0, "PLANE_CTL_MULTI");
        assert_eq!(after[after.len() - 3].0, "PLANE_SURF_MULTI");
        assert_eq!(after[after.len() - 2].0, "PLANE_CTL_MULTI");
        assert_eq!(after[after.len() - 1].0, "PLANE_SURF_MULTI");
    }

    #[test]
    fn crtc_disable_orders_plane_scalers_then_transcoder() {
        let regs = MockRegisters::new();
        let transcoder = pipe_regs::PIPECONF_A;
        regs.set(transcoder, (1 << 31) | (1 << 30));
        regs.derive(transcoder, |value| value & !(1 << 30));

        disable_pipe(&regs, Pipe::A, &[0]).expect("all dependent blocks should disable");

        let writes = regs.writes();
        assert_eq!(writes[0].0, "PLANE_CTL_MULTI_1");
        assert_eq!(writes[1].0, "PS_CTRL");
        assert_eq!(writes[2].0, "PS_WIN_POS");
        assert_eq!(writes[3].0, "PS_WIN_SZ");
        assert_eq!(writes[4].0, "PS_CTRL");
        assert_eq!(writes[5].0, "PS_WIN_POS");
        assert_eq!(writes[6].0, "PS_WIN_SZ");
        assert_eq!(writes[7], ("PIPECONF_A", 1 << 30));
    }

    #[test]
    fn invalid_plane_refuses_before_scaler_or_transcoder_writes() {
        let regs = MockRegisters::new();
        assert_eq!(
            disable_pipe(&regs, Pipe::B, &[4]),
            Err(NativePipeDisableError::Plane(
                PipeError::PlaneIndexOutOfRange { plane_index: 4 }
            ))
        );
        assert!(regs.writes().is_empty());
    }

    #[test]
    fn duplicate_plane_refuses_before_first_disable_write() {
        let regs = MockRegisters::new();
        assert_eq!(
            disable_pipe(&regs, Pipe::C, &[1, 1]),
            Err(NativePipeDisableError::Plane(
                PipeError::DuplicatePlaneIndex { plane_index: 1 }
            ))
        );
        assert!(regs.writes().is_empty());
    }
}
