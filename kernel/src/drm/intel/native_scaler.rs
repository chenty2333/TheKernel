//! Checked MMIO adapter for the translated Skylake scaler detach path.
//!
//! The atomic state translation carries scaler ownership separately from the
//! pipe/plane program.  During a CRTC disable, use the upstream
//! `skl_scaler_disable()` helper to detach both hardware scalers before the
//! transcoder is stopped.  This backend deliberately only admits direct-MMIO
//! commits; callers using DSB must route the same source helper through their
//! DSB implementation instead of silently sending DSB writes as MMIO.

use super::{
    pipe::Pipe,
    regs::{Meaning, Register, Registers},
};

const SCALERS_PER_PIPE: usize = 2;

/// Errors from scaler detachment, kept separate from the mode/plane writer so
/// a commit can report whether the source helper requested an unsupported DSB
/// operation or a checked register access failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeScalerError {
    DsbCommitUnsupported,
    InvalidRegister(u32),
    RegisterRead(u32),
    RegisterWrite(u32),
}

/// Detach both scalers for one pipe with translated `skl_scaler_disable()`.
///
/// This is the unscaled native KMS disable path; scaling setup and filter
/// coefficient programming require additional plane/DSB state and remain
/// rejected until that state is present.  The function validates `dsb` before
/// the first write, uses an exact source-derived register allowlist, and
/// returns an error if any source write was refused.
pub(crate) fn disable_pipe_scalers(
    regs: &impl Registers,
    pipe: Pipe,
    dsb: bool,
) -> Result<(), NativeScalerError> {
    if dsb {
        return Err(NativeScalerError::DsbCommitUnsupported);
    }
    let mut io = CheckedScalerIo {
        regs,
        pipe,
        failed: None,
    };
    let state = intel_display::skl_scaler_full::CrtcState {
        display: intel_display::skl_scaler_full::Display {
            display_ver: 13,
            ..Default::default()
        },
        pipe: pipe.index() as u8,
        num_scalers: SCALERS_PER_PIPE,
        ..Default::default()
    };
    intel_display::skl_scaler_full::skl_scaler_disable(&mut io, &state);
    match io.failed {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct CheckedScalerIo<'a, R> {
    regs: &'a R,
    pipe: Pipe,
    failed: Option<NativeScalerError>,
}

impl<R: Registers> CheckedScalerIo<'_, R> {
    /// Translate only the three registers `skl_detach_scaler()` writes for
    /// each of the two Gen12 pipe scalers.  Offsets are derived from
    /// `skl_scaler_full.rs`'s `ps_ctrl_reg()` / `ps_win_pos()` / `ps_win_sz()`.
    fn register(&self, offset: u32) -> Option<Register> {
        for id in 0..SCALERS_PER_PIPE {
            let ctrl = 0x6_8180 + self.pipe.index() * 0x800 + (id as u32) * 0x100;
            let name = match offset {
                value if value == ctrl => "PS_CTRL",
                value if value == ctrl - 0x10 => "PS_WIN_POS",
                value if value == ctrl - 0x0c => "PS_WIN_SZ",
                _ => continue,
            };
            return Some(Register::read_write(name, offset, Meaning::BringUp, None));
        }
        None
    }

    fn fail(&mut self, error: NativeScalerError) {
        if self.failed.is_none() {
            self.failed = Some(error);
        }
    }

    fn write_checked(&mut self, offset: u32, value: u32) {
        let Some(register) = self.register(offset) else {
            self.fail(NativeScalerError::InvalidRegister(offset));
            return;
        };
        if !self.regs.write(register, value) {
            self.fail(NativeScalerError::RegisterWrite(offset));
        }
    }
}

impl<R: Registers> intel_display::skl_scaler_full::ScalerIo for CheckedScalerIo<'_, R> {
    fn read(&mut self, offset: u32) -> u32 {
        let Some(register) = self.register(offset) else {
            self.fail(NativeScalerError::InvalidRegister(offset));
            return 0;
        };
        match self.regs.read(register) {
            Some(value) => value,
            None => {
                self.fail(NativeScalerError::RegisterRead(offset));
                0
            }
        }
    }

    fn write_fw(&mut self, offset: u32, value: u32) {
        self.write_checked(offset, value);
    }

    fn write_dsb(&mut self, offset: u32, value: u32) {
        // This function is only called after the public helper rejected DSB
        // mode, so source DSB writes are safely lowered to ordered DE MMIO.
        self.write_checked(offset, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::intel::regs::mock::MockRegisters;

    #[test]
    fn scaler_disable_uses_source_layout_for_both_scalers() {
        let regs = MockRegisters::new();

        disable_pipe_scalers(&regs, Pipe::A, false).expect("source scaler disable should write");

        let writes = regs.writes();
        assert_eq!(writes.len(), 6);
        assert_eq!(writes[0], ("PS_CTRL", 0));
        assert_eq!(writes[1], ("PS_WIN_POS", 0));
        assert_eq!(writes[2], ("PS_WIN_SZ", 0));
        assert_eq!(writes[3], ("PS_CTRL", 0));
        assert_eq!(writes[4], ("PS_WIN_POS", 0));
        assert_eq!(writes[5], ("PS_WIN_SZ", 0));
    }

    #[test]
    fn scaler_disable_refuses_dsb_before_any_mmio_write() {
        let regs = MockRegisters::new();
        assert_eq!(
            disable_pipe_scalers(&regs, Pipe::B, true),
            Err(NativeScalerError::DsbCommitUnsupported)
        );
        assert!(regs.writes().is_empty());
    }

    #[test]
    fn scaler_register_whitelist_is_pipe_and_id_bounded() {
        let regs = MockRegisters::new();
        let adapter = CheckedScalerIo {
            regs: &regs,
            pipe: Pipe::B,
            failed: None,
        };
        assert_eq!(adapter.register(0x68980).unwrap().offset(), 0x68980);
        assert_eq!(adapter.register(0x68a70).unwrap().offset(), 0x68a70);
        assert!(adapter.register(0x68b80).is_none());
        assert!(adapter.register(0x70008).is_none());
    }
}
