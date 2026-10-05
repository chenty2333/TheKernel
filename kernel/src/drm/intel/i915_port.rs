//! TheKernel adapter for the MIT display-13 readout slice.
//! Only used after the existing transaction proves live firmware scanout.
//! No default-boot hardware accesses or additional modeset permission.
use alloc::{format, string::String};

use intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo, Timings},
};

use super::regs::{Meaning, Register, Registers};

// A precise whitelist, not a way to turn the crate's raw offsets into arbitrary
// aperture access. The new SCL read is pipe-A only and read-only in the adapter.
const TIMING_READS: [Register; 7] = [
    super::regs::ddi::TRANS_HTOTAL_A,
    super::regs::ddi::TRANS_HBLANK_A,
    super::regs::ddi::TRANS_HSYNC_A,
    super::regs::ddi::TRANS_VTOTAL_A,
    super::regs::ddi::TRANS_VBLANK_A,
    super::regs::ddi::TRANS_VSYNC_A,
    Register::read_only(
        "TRANS_SET_CONTEXT_LATENCY(A)",
        0x6007c,
        Meaning::BringUp,
        None,
    ),
];
struct ReadOnly<'a, R: Registers>(&'a R);
impl<R: Registers> RegisterIo for ReadOnly<'_, R> {
    fn read32(&self, offset: u32) -> Result<u32, Error> {
        let register = TIMING_READS
            .iter()
            .find(|r| r.offset() == offset)
            .ok_or(Error::Unavailable(offset))?;
        self.0.read(*register).ok_or(Error::Unavailable(offset))
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        Err(Error::Refused)
    }
}
impl<R: Registers> ReadoutIo for ReadOnly<'_, R> {
    fn pipe_powered(&self, pipe: Pipe) -> bool {
        pipe == Pipe::A
    }
}
#[cfg(target_os = "none")]
pub(super) fn read_admitted_timings(
    tx: &super::rollback::Transaction<'_, super::regs::RegisterWindow>,
) -> Result<Timings, String> {
    decode(tx)
}
fn decode(registers: &impl Registers) -> Result<Timings, String> {
    // This private helper is only reached after Transaction::begin validated the
    // firmware's live pipe A, with no competing modeset/GT/S0ix owner at boot.
    let conf = registers
        .read(super::regs::pipe::PIPECONF_A)
        .ok_or_else(|| String::from("firmware TRANSCONF unavailable; no writes"))?;
    // Initial port is progressive HDMI only. Do not misinterpret an interlaced
    // state as the source for a progressive native rollback/modeset transaction.
    if conf & ((1 << 31) | (1 << 30)) != ((1 << 31) | (1 << 30)) || conf & (3 << 21) != 0 {
        return Err(String::from(
            "interlaced firmware readout unsupported; no writes",
        ));
    }
    let timings =
        intel_display::display::intel_get_transcoder_timings(&ReadOnly(registers), Pipe::A, false)
            .map_err(|e| format!("i915 firmware timing readout failed: {e:?}; no writes"))?;
    timings
        .validate()
        .map_err(|e| format!("i915 firmware timing admission failed: {e:?}; no writes"))?;
    Ok(timings)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::intel::regs::mock::MockRegisters;
    #[test]
    fn admitted_progressive_timings_are_read_without_writes() {
        let r = MockRegisters::new();
        r.set(super::super::regs::pipe::PIPECONF_A, (1 << 31) | (1 << 30));
        let pair = |a: u32, b: u32| (a - 1) | ((b - 1) << 16);
        for (reg, value) in TIMING_READS.iter().zip([
            pair(1920, 2200),
            pair(1920, 2200),
            pair(2008, 2052),
            pair(1080, 1125),
            pair(1080, 1125),
            pair(1084, 1089),
            0,
        ]) {
            r.set(*reg, value);
        }
        let timing = decode(&r).unwrap();
        assert_eq!(
            (timing.hdisplay, timing.vdisplay, timing.vtotal),
            (1920, 1080, 1125)
        );
        assert!(r.writes().is_empty());
        r.hide(TIMING_READS[6]);
        assert!(decode(&r).is_err());
        assert!(r.writes().is_empty());
    }
    #[test]
    fn adapter_is_read_only_and_missing_scl_refuses() {
        let r = MockRegisters::new();
        r.set(super::super::regs::pipe::PIPECONF_A, (1 << 31) | (1 << 30));
        let read = ReadOnly(&r);
        assert_eq!(read.write32(0x6007c, 0), Err(Error::Refused));
        assert_eq!(read.read32(0x1234), Err(Error::Unavailable(0x1234)));
        r.hide(TIMING_READS[6]);
        assert!(decode(&r).is_err());
        assert!(r.writes().is_empty());
    }
}
