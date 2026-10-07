//! TheKernel adapter for the MIT display-13 readout slice.
//! Only used after the existing transaction proves live firmware scanout.
//! No default-boot hardware accesses or additional modeset permission.
use alloc::{format, string::String};

use intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo, Timings},
};

use super::regs::{Meaning, Register, Registers};

/// Only the exact characterized N305 SKU/stepping enters the existing native
/// transaction. Platform identity never means an arbitrary revision is safe.
pub(super) fn native_device_supported(vendor: u16, device_id: u16, revision: u8) -> bool {
    device_id == 0x46d0
        && intel_display::device::Device::identify(vendor, device_id, revision)
            .is_ok_and(|d| d.exact_step && d.step == intel_display::device::Step::D0)
}

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
            "firmware pipe A stopped, transitioning or interlaced; no writes",
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
    extern crate std;
    use super::*;
    use crate::drm::intel::regs::mock::MockRegisters;
    #[test]
    fn unknown_revision_or_other_sku_never_grants_native_permission() {
        assert!(native_device_supported(0x8086, 0x46d0, 0));
        for revision in [1, 4, 8, 12, 255] {
            assert!(!native_device_supported(0x8086, 0x46d0, revision));
        }
        for device in [0x46d1, 0x46a0, 0xa7a0, 0] {
            assert!(!native_device_supported(0x8086, device, 0));
        }
        assert!(!native_device_supported(0x1234, 0x46d0, 0));
    }
    #[test]
    #[ignore = "requires THEKERNEL_N305_CAPTURE private input; run explicitly"]
    fn captured_edid_to_tc_pll_and_preserved_cdclk() {
        use intel_display::{
            bios::Vbt,
            cdclk,
            device::{Port, Step},
            dpll_mgr,
        };

        use crate::drm::modes::{Constraints, plan_modeset};
        let root = std::path::PathBuf::from(
            std::env::var_os("THEKERNEL_N305_CAPTURE").expect("set THEKERNEL_N305_CAPTURE"),
        );
        let edid = std::fs::read(root.join("display/edid/card1-HDMI-A-1.edid")).unwrap();
        let plan = plan_modeset(&edid, &Constraints::unlimited());
        // This capture's EDID 1.3 has a range descriptor with a 1.4-only
        // "bare limits" tag. Preserve the explicit warning/lossy path, not a
        // false strict-parse success or a relaxation of parser validation.
        assert!(!plan.strict && plan.used_edid(), "{plan:?}");
        assert_eq!(
            plan.edid_error,
            Some(crate::drm::modes::EdidError::InvalidDisplayDescriptor { index: 2 })
        );
        assert_eq!(plan.warnings.invalid_descriptors, 1);
        assert_eq!(plan.selection.mode.clock_khz, 297000);
        let choice = super::super::modeset::choose_mode(
            &plan,
            &edid,
            super::super::modeset::EngineLimits::at_cdclk(192000),
        );
        let mode = choice.into_mode().unwrap();
        assert_eq!(
            (
                mode.clock_khz,
                mode.hdisplay,
                mode.hsync_start,
                mode.hsync_end,
                mode.htotal
            ),
            (148500, 1920, 2008, 2052, 2200)
        );
        assert_eq!(
            (
                mode.vdisplay,
                mode.vsync_start,
                mode.vsync_end,
                mode.vtotal,
                mode.refresh_millihz()
            ),
            (1080, 1084, 1089, 1125, 60000)
        );
        let debug = root.join("graphics/debugfs-0000:00:02.0");
        let bytes = std::fs::read(debug.join("i915_vbt")).unwrap();
        let vbt = Vbt::parse(&bytes).unwrap();
        let route = vbt
            .parse_general_definitions()
            .unwrap()
            .encoder(Port::Tc1)
            .unwrap()
            .unwrap();
        assert!(route.supports_hdmi() && !route.usb_type_c && !route.lspcon);
        assert_eq!(route.gmbus_pin(), Some(9));
        let report = std::fs::read_to_string(debug.join("i915_cdclk_info")).unwrap();
        let current = report
            .lines()
            .find_map(|line| line.strip_prefix("Current CD clock frequency: "))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_eq!(current, 192000);
        let maximum = report
            .lines()
            .find_map(|line| line.strip_prefix("Max CD clock frequency: "))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_eq!(maximum, 652800);
        // Capture doesn't establish actual firmware refclk; test all supported
        // references, not a guessed fixed value. No MMIO or full WM policy here.
        for reference in [19200, 24000, 38400] {
            let pll = dpll_mgr::icl_calc_mg_pll_state(mode.clock_khz, reference, None).unwrap();
            assert_eq!(
                dpll_mgr::icl_ddi_mg_pll_get_freq(&pll, reference),
                Ok(mode.clock_khz)
            );
            let preserved = cdclk::bxt_calc_cdclk(Step::D0, reference, current, maximum).unwrap();
            assert_eq!(preserved.cdclk_khz, current);
        }
        assert!(cdclk::pixel_rate_min_cdclk(mode.clock_khz) < current);
        std::println!(
            "captured EDID: {mode:?}; VBT TC1/GMBUS9; 148500 TMDS DKL calculation; preserve \
             192000 CDCLK, full WM/power policy still pending"
        );
    }
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
