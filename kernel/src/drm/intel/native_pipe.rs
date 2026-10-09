//! Checked pipe/plane helpers used by a Native atomic commit backend.
//!
//! This module composes the source-derived per-plane, scaler and transcoder
//! operations into a CRTC disable phase. The selector remains fail-closed
//! until the atomic-state projection supplies complete pipe/plane objects and
//! the caller owns rollback for a partial hardware disable.

use super::{
    native_scaler::{self, NativeScalerError},
    output::{self, OutputError},
    pipe::{self, Pipe, PipeError},
    regs::Registers,
};

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
    use crate::drm::intel::regs::{mock::MockRegisters, pipe as pipe_regs};

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
