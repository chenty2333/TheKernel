//! Host tests for [`super`], driven through `regs::mock::MockRegisters`.
//!
//! What a test here can establish: the *sequence*.  Which registers are
//! written, in what order, how many times, with what values; that a refusal
//! happens before the first write; that a status bit which never appears
//! produces a named error rather than a panic; and that the values the
//! reference document prints for the target mode are the values this module
//! computes.  What no test here can establish: that any of it is accepted by a
//! Gen12 display engine.  Nothing in this module has run on real hardware.

use alloc::{format, rc::Rc, vec::Vec};
use core::cell::Cell;

use super::*;
use crate::drm::{
    intel::{phy::COMP_INIT, regs::mock::MockRegisters, swing},
    modes::{CTA_VIC_TIMINGS, DMT_TIMINGS},
};

/// The CTA-861 row with this video identification code.
fn vic(code: u8) -> Mode {
    CTA_VIC_TIMINGS
        .iter()
        .find(|entry| entry.vic == code)
        .unwrap_or_else(|| panic!("VIC {code} is not in the table"))
        .mode
}

/// The VESA DMT row with this code.
fn dmt(code: u8) -> Mode {
    DMT_TIMINGS
        .iter()
        .find(|entry| entry.code == code)
        .unwrap_or_else(|| panic!("DMT {code:#04x} is not in the table"))
        .mode
}

/// The mode §6.3 works through and §11 phase 3.1 names as the first target:
/// 1920x1080@60 over HDMI, 148.5 MHz, positive syncs.
fn target_mode() -> Mode {
    vic(16)
}

/// The 38.4 MHz strap.  §12.1: the target machine's `SKL_DSSM[31:29]` decides
/// this, and 38.4 MHz exercises both the 19.2 MHz division and the ADL-P/N
/// fraction workaround.
const STRAP_38_4: u32 = 38_400;

/// Buffer-translation values that are *not* sourced from anything.
///
/// The reference does not contain the HDMI table (§8.5 `[GAP]`, §13.1 item 12),
/// which is why `swing` is a caller field at all.  These numbers exist to give
/// the sequence something to write so that its order can be asserted; the
/// `source` string says so, and the log carries it.
///
/// `dw2` and `dw7` differ lane by lane on purpose.  The sequence writes them to
/// four different addresses, and a test whose four values were equal could not
/// tell that from a group write or from one lane written four times.
fn test_swing() -> SwingProgram {
    SwingProgram {
        level: 2,
        dw2: [0x0C, 0x1C, 0x2C, 0x3C],
        dw4: [0x30, 0x31, 0x31, 0x31],
        dw5_training_disabled: 0x0000_0000,
        dw5_training_enabled: 0x0002_0000,
        dw7: [0x71, 0x72, 0x73, 0x74],
        source: "test fixture, not sourced from the reference",
    }
}

/// The request §11's bring-up makes: HDMI on the DDI the monitor answered on,
/// four lanes, the Gen12 named-constant PLL encoding, and swing values.
fn hdmi_request(ddi: Ddi) -> OutputRequest {
    OutputRequest::hdmi(ddi, target_mode(), PllFieldEncoding::Named).with_swing(test_swing())
}

/// A plan for the target mode, which everything below starts from.
fn target_plan() -> OutputProgram {
    OutputProgram::plan(&hdmi_request(Ddi::A), STRAP_38_4)
        .expect("the target mode is what the reference works through")
}

/// A mock whose status bits behave: the PLL's power state and lock follow its
/// enable, the DDI leaves idle when it is enabled, and the DDI-IO well reports
/// its state.
///
/// One hook per register, which is the mock's rule: a second `derive` on the
/// same register would replace this one and a test that installed one per
/// thing it believed in would be asserting against a mock that models only the
/// last of them.
///
/// The registers come from [`port_registers`], so one helper serves both combo
/// PHYs and the registers it models are the ones the sequence will write.
fn ready_mock_for(phy: ComboPhy) -> MockRegisters {
    let registers = port_registers(phy);
    let regs = MockRegisters::new();
    regs.derive(registers.pll_enable, |value| {
        let mut stored = value;
        if value & PLL_POWER_ENABLE != 0 {
            stored |= PLL_POWER_STATE;
        }
        if value & PLL_ENABLE != 0 {
            stored |= PLL_LOCK;
        }
        stored
    });
    regs.derive(registers.ddi_buf_ctl, |value| {
        if value & DDI_BUF_CTL_ENABLE != 0 {
            value & !DDI_BUF_CTL_IS_IDLE
        } else {
            value | DDI_BUF_CTL_IS_IDLE
        }
    });
    regs.derive(regs::ICL_PWR_WELL_CTL_DDI2, move |value| {
        value | power::well_state(ddi_io_well(phy).index)
    });
    regs
}

/// The PHY A mock: DPLL0, `DDI_BUF_CTL(A)` and the DDI-IO A well.
fn ready_mock() -> MockRegisters {
    ready_mock_for(ComboPhy::A)
}

// -- the computed values ----------------------------------------------------

/// The reference's worked example, to the hex digit.
///
/// §6.3, "Worked example -- 1080p60, 148.5 MHz pixel clock, HDMI, 38.4 MHz
/// strap" prints the whole chain and ends at `DPLL0_CFGCR0 = 0x001001D0` and
/// `DPLL0_CFGCR1 = 0x00000E84`.  This is the strongest sourced check available
/// to this module: no arithmetic of its own is involved -- `pll.rs` computed
/// both -- but the values the sequence would write are the document's.
#[test]
fn the_target_mode_matches_the_reference_worked_example() {
    let plan = target_plan();
    assert_eq!(plan.dividers.p(), 2, "P");
    assert_eq!(plan.dividers.q(), 3, "Q");
    assert_eq!(plan.dividers.k(), 2, "K");
    assert_eq!(plan.dividers.total_divider(), 12);
    assert_eq!(plan.dividers.target_dco_khz(), 8_910_000);
    assert_eq!(plan.dividers.symbol_rate_khz(), 148_500);
    assert_eq!(plan.dividers.wrpll_ref_khz(), 19_200, "38.4 -> 19.2 MHz");
    assert_eq!(plan.platform_ref_khz, STRAP_38_4);
    assert_eq!(
        plan.fraction_workaround,
        DcoFractionWorkaround::HalveFraction
    );
    assert_eq!(
        plan.pll_registers,
        PllRegisters {
            cfgcr0: 0x0010_01D0,
            cfgcr1: 0x0000_0E84,
        }
    );
    // DCO_FRACTION is 2048 before the workaround and 1024 after it -- the
    // document prints both.
    assert_eq!(plan.dividers.dco_fraction(), 2048);
    assert_eq!((plan.pll_registers.cfgcr0 >> 10) & 0x7fff, 1024);
    assert_eq!(plan.pll_registers.cfgcr0 & 0x3ff, 464);
}

/// The same mode under the other encoding differs, and the difference is the
/// one `docs/design/intel-pll.md` §3.5 describes.
///
/// This is the `[GAP]` as a measurement rather than a claim: `K = 2` is coded
/// `2 << 6` by the Gen12 named constants and `1 << 6` by the Skylake-convention
/// codes `pll.rs` also offers.  A reader who wonders why the encoding is a
/// caller decision can see it here in one line.
#[test]
fn the_two_encodings_disagree_on_the_same_divider_set() {
    let named = OutputProgram::plan(
        &OutputRequest::hdmi(Ddi::A, target_mode(), PllFieldEncoding::Named)
            .with_swing(test_swing()),
        STRAP_38_4,
    )
    .unwrap();
    let executed = OutputProgram::plan(
        &OutputRequest::hdmi(Ddi::A, target_mode(), PllFieldEncoding::Executed)
            .with_swing(test_swing()),
        STRAP_38_4,
    )
    .unwrap();
    assert_eq!(named.dividers.p(), executed.dividers.p());
    assert_eq!(named.dividers.k(), executed.dividers.k());
    assert_eq!(named.pll_registers.cfgcr0, executed.pll_registers.cfgcr0);
    assert_eq!(named.pll_registers.cfgcr1, 0x0000_0E84);
    assert_eq!(executed.pll_registers.cfgcr1, 0x0000_0E44);
}

/// The encoder-side registers, to the hex digit, from §8.4's field tables.
///
/// `TRANS_CLK_SEL`'s `0x1000_0000` is PHY A's field on this display version
/// (`[I915]` `display/intel_ddi.c:999-1000`), which for this port is also DDI
/// A's number -- see
/// `the_clock_select_is_keyed_by_phy_and_the_port_select_by_ddi`.
#[test]
fn the_transcoder_and_ddi_values_are_the_reference_encodings() {
    let plan = target_plan();
    assert_eq!(plan.trans_clk_sel, 0x1000_0000, "TRANS_CLK_SEL(A)");
    assert_eq!(
        plan.trans_ddi_func_ctl, 0x8803_0006,
        "ENABLE | SELECT_PORT(A) | HDMI | 8bpc | both syncs | four lanes"
    );
    assert_eq!(
        plan.transconf, 0x8000_0000,
        "TRANSCONF(A): ENABLE only, because bit 30 is a status"
    );
    assert_eq!(
        plan.ddi_buf_ctl, 0x8200_0016,
        "ENABLE | BUF_TRANS_SELECT(2) | PORT_WIDTH(4 lanes) | A_4_LANES"
    );
}

/// No output bit depth in `TRANSCONF`, and no `STATE_ENABLE` either.
///
/// §8.4's correction and §11 phase 5.6 put the bit depth in `PIPE_MISC`;
/// §8.6 step 12 still lists it here.  Bit 30 is the other half: §11 phase 5.6
/// prints `ENABLE | STATE_ENABLE` and bit 30 is a **hardware status**, so the
/// value is the enable bit exactly.  The literal is asserted rather than the
/// module's own constants, because
/// `TRANSCONF_ENABLE | TRANSCONF_STATE_ENABLE_STATUS` is what the defect was.
#[test]
fn transconf_carries_no_bit_depth() {
    let plan = target_plan();
    // `TRANSCONF`'s BPC field is a pre-Haswell leftover; the value is exactly
    // the enable bit, nothing else.
    assert_eq!(plan.transconf, 0x8000_0000);
}

/// The register the sequence leaves behind holds `0x8000_0000`, whatever the
/// module's constants say.
///
/// §11 phase 5.6's `TRANSCONF(A) = (1<<31) | (1<<30)` is the reference's defect
/// and this module followed it.  `[I915]`'s `TRANSCONF_STATE_ENABLE`
/// (`i915_reg.h:1591`) is bit 30, and i915 only ever *polls* it: clear after a
/// disable (`display/intel_display.c:302-318`), never set by
/// `intel_enable_transcoder`, which ORs `TRANSCONF_ENABLE` into the value it
/// read (`:459`, `:474-475`).  Asserting the literal at the aperture is the
/// point: a test that compared against the module's own composition would have
/// passed with the status bit in it.
#[test]
fn the_transcoder_is_enabled_with_bit_31_alone() {
    let regs = ready_mock();
    program(&regs, &target_plan()).expect("the mock's status bits all behave");

    let (_, written) = regs
        .writes()
        .into_iter()
        .find(|(name, _)| *name == "PIPECONF_A")
        .expect("the sequence writes the transcoder's own register");
    assert_eq!(
        written, 0x8000_0000,
        "TRANSCONF(A) is ENABLE and nothing else"
    );
    assert_eq!(
        written & 0x4000_0000,
        0,
        "bit 30 is STATE_ENABLE, the hardware's pipe-running status"
    );
    assert_eq!(
        regs.read(pipe::PIPECONF_A),
        Some(0x8000_0000),
        "the register holds the enable bit alone"
    );
    assert_eq!(
        TRANSCONF_STATE_ENABLE_STATUS, 0x4000_0000,
        "the renamed constant is the status bit, and the write does not carry it"
    );
    // The log is the only evidence a machine with no serial port leaves, so it
    // prints the value the register gets and names the bit it does not.
    let log = target_plan().render();
    assert!(
        log.contains("TRANSCONF(A) = 0x80000000"),
        "the plan log does not print the transcoder value:\n{log}"
    );
    assert!(
        log.contains("0x40000000 is STATE_ENABLE"),
        "the plan log does not name the status bit:\n{log}"
    );
}

/// §11 phase 5.5's polarities: the two bits follow the mode, in both
/// directions, and nothing else changes.
///
/// CTA-861 VIC 16 and VESA DMT 0x52 are the same timing published by two
/// tables, with opposite sync polarities -- `timing.rs`'s tests use the same
/// pair for the same reason.
#[test]
fn the_polarity_bits_follow_the_mode_in_both_directions() {
    let positive = target_mode();
    assert!(positive.hsync_positive && positive.vsync_positive);
    let negative = dmt(0x52);
    assert!(!negative.hsync_positive && !negative.vsync_positive);
    assert!(positive.same_timing(&negative));

    let positive_plan = OutputProgram::plan(
        &OutputRequest::hdmi(Ddi::A, positive, PllFieldEncoding::Named).with_swing(test_swing()),
        STRAP_38_4,
    )
    .unwrap();
    let negative_plan = OutputProgram::plan(
        &OutputRequest::hdmi(Ddi::A, negative, PllFieldEncoding::Named).with_swing(test_swing()),
        STRAP_38_4,
    )
    .unwrap();

    assert_eq!(
        positive_plan.trans_ddi_func_ctl & TRANS_DDI_PHSYNC,
        TRANS_DDI_PHSYNC
    );
    assert_eq!(
        positive_plan.trans_ddi_func_ctl & TRANS_DDI_PVSYNC,
        TRANS_DDI_PVSYNC
    );
    assert_eq!(negative_plan.trans_ddi_func_ctl & TRANS_DDI_PHSYNC, 0);
    assert_eq!(negative_plan.trans_ddi_func_ctl & TRANS_DDI_PVSYNC, 0);
    // Only the polarity bits differ.
    assert_eq!(
        positive_plan.trans_ddi_func_ctl & !(TRANS_DDI_PHSYNC | TRANS_DDI_PVSYNC),
        negative_plan.trans_ddi_func_ctl & !(TRANS_DDI_PHSYNC | TRANS_DDI_PVSYNC),
    );
}

/// The log the caller prints before the first write carries what §11 phase 5.1
/// asks it to carry.
#[test]
fn the_plan_log_carries_the_reference_the_dividers_and_the_symbol_rate() {
    let text = target_plan().render();
    for needle in [
        "reference 38400 kHz",
        "19200 kHz",
        "(P, Q, K) = (2, 3, 2)",
        "symbol rate 148500 kHz",
        "0x001001d0",
        "0x00000e84",
        "halved per WA #22010492432",
        "icl_combo_phy_trans_hdmi",
    ] {
        assert!(
            text.contains(needle),
            "the plan log does not carry {needle:?}:\n{text}"
        );
    }
}

// -- the sequence -----------------------------------------------------------

/// The order §8.6 states, asserted on the write log rather than on the return
/// value.
///
/// The swing batch's shape is i915's: one group `PORT_TX_DW5` write, then
/// `DW2`, `DW4` and `DW7` once per lane in lane order, then the group `DW5`
/// write that commits them (`[I915]` `display/intel_ddi.c:1140-1179` and
/// `:1218-1229`).
#[test]
fn the_write_order_is_the_sequence_the_reference_states() {
    let regs = ready_mock();
    let plan = target_plan();
    program(&regs, &plan).expect("the mock's status bits all behave");

    let names: Vec<&str> = regs.writes().into_iter().map(|(name, _)| name).collect();
    assert_eq!(
        names,
        [
            // 5.1: power, dividers, enable.
            "DPLL0_ENABLE",
            "DPLL0_CFGCR0",
            "DPLL0_CFGCR1",
            "DPLL0_ENABLE",
            // 5.2: the clock select, then the clock-off clear, in two writes.
            "ICL_DPCLKA_CFGCR0",
            "ICL_DPCLKA_CFGCR0",
            // §8.6 step 5: the port's DDI-IO power.
            "ICL_PWR_WELL_CTL_DDI2",
            // 5.3: §8.5's swing sequence, then the lane power.  Step 4's group
            // write first, the three per-lane dwords next, step 6's group write
            // last.
            "PORT_CL_DW5(A)",
            "PORT_TX_DW5_GRP(A)",
            "PORT_TX_DW2_LN0(A)",
            "PORT_TX_DW2_LN1(A)",
            "PORT_TX_DW2_LN2(A)",
            "PORT_TX_DW2_LN3(A)",
            "PORT_TX_DW4_LN0(A)",
            "PORT_TX_DW4_LN1(A)",
            "PORT_TX_DW4_LN2(A)",
            "PORT_TX_DW4_LN3(A)",
            "PORT_TX_DW7_LN0(A)",
            "PORT_TX_DW7_LN1(A)",
            "PORT_TX_DW7_LN2(A)",
            "PORT_TX_DW7_LN3(A)",
            "PORT_TX_DW5_GRP(A)",
            "PORT_CL_DW10(A)",
            // 5.4, 5.5, 5.6, 5.7.
            "TRANS_CLK_SEL(A)",
            "TRANS_DDI_FUNC_CTL(A)",
            "PIPECONF_A",
            "DDI_BUF_CTL(A)",
        ]
    );
}

/// The PLL is powered before it is enabled, and the dividers land between the
/// two -- which is what §6.3's sequence exists to guarantee.
#[test]
fn the_pll_is_powered_before_it_is_enabled_and_loaded_in_between() {
    let regs = ready_mock();
    program(&regs, &target_plan()).unwrap();
    let writes = regs.writes();
    let enable_writes: Vec<u32> = writes
        .iter()
        .filter(|(name, _)| *name == "DPLL0_ENABLE")
        .map(|(_, value)| *value)
        .collect();
    assert_eq!(enable_writes.len(), 2, "one power write, one enable write");
    assert_eq!(
        enable_writes[0] & (PLL_POWER_ENABLE | PLL_ENABLE),
        PLL_POWER_ENABLE,
        "the first write must power the block and must not enable it"
    );
    assert_eq!(
        enable_writes[1] & (PLL_POWER_ENABLE | PLL_ENABLE),
        PLL_POWER_ENABLE | PLL_ENABLE,
        "the second write enables the PLL and leaves it powered"
    );
    let position = |name: &str| {
        writes
            .iter()
            .position(|(written, _)| *written == name)
            .unwrap_or_else(|| panic!("{name} was never written"))
    };
    assert!(position("DPLL0_ENABLE") < position("DPLL0_CFGCR0"));
    assert!(position("DPLL0_CFGCR0") < position("DPLL0_CFGCR1"));
}

/// §6.3's bold requirement: the clock-select write and the clock-off clear are
/// two register writes, not one.
#[test]
fn the_clock_select_and_the_clock_off_clear_are_separate_writes() {
    let regs = ready_mock();
    // Firmware left this DDI's clock gated, which is the state the clear is
    // for; without this the clear would be a no-op on a zero register and the
    // test would pass for the wrong reason.
    regs.set(dpll::ICL_DPCLKA_CFGCR0, 1 << 10);
    let plan = target_plan();
    program(&regs, &plan).unwrap();

    assert_eq!(regs.write_count(dpll::ICL_DPCLKA_CFGCR0), 2);
    let writes: Vec<u32> = regs
        .writes()
        .into_iter()
        .filter(|(name, _)| *name == "ICL_DPCLKA_CFGCR0")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(
        writes[0],
        plan.dpclka_select | (1 << 10),
        "select, gate still set"
    );
    assert_eq!(writes[1], plan.dpclka_select, "gate cleared, select kept");
}

/// The DDI-IO well is enabled through `power.rs`'s handshake: the request bit
/// is set, the state bit is polled, and the observation is carried out.
#[test]
fn the_ddi_io_well_is_enabled_before_the_swing_values_are_written() {
    let regs = ready_mock();
    let state = program(&regs, &target_plan()).unwrap();
    assert_eq!(state.ddi_io_well.name, "DDI_IO_A");
    assert!(!state.ddi_io_well.already_on, "the mock starts with it off");
    assert_eq!(regs.write_count(regs::ICL_PWR_WELL_CTL_DDI2), 1);
    let (_, written) = regs
        .writes()
        .into_iter()
        .find(|(name, _)| *name == "ICL_PWR_WELL_CTL_DDI2")
        .unwrap();
    assert_eq!(
        written & power::well_request(power::DDI_IO_A.index),
        power::well_request(power::DDI_IO_A.index)
    );
    let well = regs
        .writes()
        .iter()
        .position(|(name, _)| *name == "ICL_PWR_WELL_CTL_DDI2")
        .unwrap();
    let swing = regs
        .writes()
        .iter()
        .position(|(name, _)| *name == "PORT_TX_DW5_GRP(A)")
        .unwrap();
    assert!(
        well < swing,
        "the port is powered before its TX block is written"
    );
}

/// §8.5's batch, as i915 writes it: `PORT_TX_DW2`, `PORT_TX_DW4` and
/// `PORT_TX_DW7` once per lane, in lane order, with the four values the caller
/// supplied.
///
/// `[I915]` `display/intel_ddi.c:1148-1178` runs one read-modify-write per lane
/// over `ln = 0..3` for each of the three dwords, and `:1160` says why the
/// group instance must not be used for `DW4`.  Both `PORT_TX_DW5` writes go to
/// the group instance and straddle the three loops.
#[test]
fn the_swing_writes_reach_the_per_lane_registers_in_lane_order() {
    let regs = ready_mock();
    let swing = test_swing();
    program(&regs, &target_plan()).unwrap();
    let lane_writes: Vec<(&str, u32)> = regs
        .writes()
        .into_iter()
        .filter(|(name, _)| {
            name.starts_with("PORT_TX_DW2_LN")
                || name.starts_with("PORT_TX_DW4_LN")
                || name.starts_with("PORT_TX_DW7_LN")
        })
        .collect();
    assert_eq!(
        lane_writes,
        [
            ("PORT_TX_DW2_LN0(A)", swing.dw2[0]),
            ("PORT_TX_DW2_LN1(A)", swing.dw2[1]),
            ("PORT_TX_DW2_LN2(A)", swing.dw2[2]),
            ("PORT_TX_DW2_LN3(A)", swing.dw2[3]),
            ("PORT_TX_DW4_LN0(A)", swing.dw4[0]),
            ("PORT_TX_DW4_LN1(A)", swing.dw4[1]),
            ("PORT_TX_DW4_LN2(A)", swing.dw4[2]),
            ("PORT_TX_DW4_LN3(A)", swing.dw4[3]),
            ("PORT_TX_DW7_LN0(A)", swing.dw7[0]),
            ("PORT_TX_DW7_LN1(A)", swing.dw7[1]),
            ("PORT_TX_DW7_LN2(A)", swing.dw7[2]),
            ("PORT_TX_DW7_LN3(A)", swing.dw7[3]),
        ]
    );
    // Step 4 disables TX training, step 6 re-enables it; both writes are to the
    // same group register and both are in the log.
    let dw5: Vec<u32> = regs
        .writes()
        .into_iter()
        .filter(|(name, _)| *name == "PORT_TX_DW5_GRP(A)")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(
        dw5,
        [swing.dw5_training_disabled, swing.dw5_training_enabled]
    );
}

/// One lane's write is not another lane's, and the group instances of `DW2` and
/// `DW7` are not written at all.
///
/// This is the defect this workstream exists for, asserted where it is visible:
/// the four lane instances of each dword are four addresses, so what the
/// aperture holds afterwards is four values and not the last one four times,
/// while the group registers keep whatever was there.  The values differ lane
/// by lane (`test_swing`), so a group write or a wrong stride would show up as
/// a value at the wrong address.
#[test]
fn a_lane_write_is_not_visible_at_another_lanes_address() {
    let regs = ready_mock();
    let swing = test_swing();
    program(&regs, &target_plan()).unwrap();

    let lanes = tx_lane_registers(ComboPhy::A);
    for (register, value) in lanes.dw2.iter().zip(swing.dw2) {
        assert_eq!(regs.read(*register), Some(value), "{}", register.name());
    }
    for (register, value) in lanes.dw4.iter().zip(swing.dw4) {
        assert_eq!(regs.read(*register), Some(value), "{}", register.name());
    }
    for (register, value) in lanes.dw7.iter().zip(swing.dw7) {
        assert_eq!(regs.read(*register), Some(value), "{}", register.name());
    }
    // The sentence the defect was, said directly: lane 1's write is not visible
    // at lane 0's address, and lane 0 kept its own word.
    assert_eq!(
        regs.read(lanes.dw2[0]),
        Some(swing.dw2[0]),
        "lane 0's PORT_TX_DW2 does not hold lane 0's value"
    );
    assert_ne!(
        regs.read(lanes.dw2[0]),
        regs.read(lanes.dw2[1]),
        "lane 1's write is visible at lane 0's address"
    );
    assert_ne!(
        regs.read(lanes.dw7[0]),
        regs.read(lanes.dw7[3]),
        "lane 3's write is visible at lane 0's address"
    );
    for (lane, register) in lanes.dw2.iter().enumerate() {
        assert_eq!(
            register.offset(),
            0x16_2888 + 0x100 * lane as u32,
            "PORT_TX_DW2 lane {lane} is not at the lane stride"
        );
    }
    for (lane, register) in lanes.dw7.iter().enumerate() {
        assert_eq!(
            register.offset(),
            0x16_289c + 0x100 * lane as u32,
            "PORT_TX_DW7 lane {lane} is not at the lane stride"
        );
    }
    for name in ["PORT_TX_DW2_GRP(A)", "PORT_TX_DW7_GRP(A)"] {
        assert!(
            !regs.writes().iter().any(|(written, _)| *written == name),
            "{name} must not be written: group access is what collapsed the lanes"
        );
    }
    assert_eq!(regs.read(port::PORT_TX_DW2_GRP_A), Some(0));
    assert_eq!(regs.read(port::PORT_TX_DW7_GRP_A), Some(0));
}

/// The two group `DW5` writes carry the word that was read from lane 0.
///
/// This is where the read route and the write sequence meet: `swing.rs` reads
/// `PORT_TX_DW5_LN0` (`[I915]` `display/intel_ddi.c:1226`) and this module
/// writes `PORT_TX_DW5_GRP` twice (`:1221`, `:1229`).  The mock's two instances
/// hold different words, so a group write that came from anywhere but lane 0's
/// read would be visible in the log the test reads.
#[test]
fn the_group_dw5_writes_carry_the_word_read_from_lane_zero() {
    /// Lane 0's `PORT_TX_DW5`: `TX_TRAINING_EN`, bit 31
    /// (`[I915]` `display/intel_combo_phy_regs.h:133`), plus fields this module
    /// deliberately has no names for.
    const LANE0_DW5: u32 = (1 << 31) | 0x0002_0030;
    /// The group instance's own word, deliberately a different one.
    const GROUP_DW5: u32 = 0x0002_0008;

    let regs = ready_mock();
    // A firmware program, in the shape `swing.rs`'s read requires: PHY
    // initialised, buffer enabled and not idle, lanes up, the swing words
    // programmed one lane at a time.
    regs.set(regs::COMBO_PHY_A.comp_dw0, COMP_INIT);
    regs.set(
        ddi::DDI_BUF_CTL_A,
        DDI_BUF_CTL_ENABLE | (2 << DDI_BUF_CTL_BUF_TRANS_SELECT_SHIFT),
    );
    regs.set(port::PORT_CL_DW10_A, 0);
    let lanes = tx_lane_registers(ComboPhy::A);
    for (lane, register) in lanes.dw2.iter().enumerate() {
        regs.set(*register, 0x0000_0100 | lane as u32);
    }
    for (lane, register) in lanes.dw4.iter().enumerate() {
        regs.set(*register, 0x0000_0200 | lane as u32);
    }
    for (lane, register) in lanes.dw7.iter().enumerate() {
        regs.set(*register, 0x0000_0300 | lane as u32);
    }
    regs.set(port::PORT_TX_DW5_LN0_A, LANE0_DW5);
    regs.set(port::PORT_TX_DW5_GRP_A, GROUP_DW5);

    let read_back =
        swing::read_firmware_swing(&regs, Ddi::A).expect("the mock is a firmware program");
    assert_eq!(read_back.dw5_training_enabled, LANE0_DW5);
    let plan = OutputProgram::plan(
        &OutputRequest::hdmi(Ddi::A, target_mode(), PllFieldEncoding::Named).with_swing(read_back),
        STRAP_38_4,
    )
    .expect("the target mode plans");
    program(&regs, &plan).expect("the mock's status bits behave");

    let dw5: Vec<u32> = regs
        .writes()
        .into_iter()
        .filter(|(name, _)| *name == "PORT_TX_DW5_GRP(A)")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(
        dw5,
        [LANE0_DW5 & !(1 << 31), LANE0_DW5],
        "step 4's write is lane 0's word with the training bit cleared, step 6's is lane 0's word"
    );
    assert!(
        !dw5.contains(&GROUP_DW5),
        "the group instance's own word reached the write log"
    );
    assert_eq!(regs.read(port::PORT_TX_DW5_GRP_A), Some(LANE0_DW5));
}

/// §8.2's `PWR_DOWN_LN_MASK[7:4]` with §8.6 step 7's values, as a
/// read-modify-write so that the rest of `PORT_CL_DW10` survives.
#[test]
fn the_lane_power_field_is_shifted_into_place_and_leaves_other_bits_alone() {
    let regs = ready_mock();
    // A bit outside the field, as firmware could leave one.
    regs.set(port::PORT_CL_DW10_A, 0x1);
    program(&regs, &target_plan()).unwrap();
    let (_, written) = regs
        .writes()
        .into_iter()
        .find(|(name, _)| *name == "PORT_CL_DW10(A)")
        .unwrap();
    assert_eq!(written, 0x1, "four lanes: field 0x0, the other bit kept");

    for (width, field) in [
        (PortWidth::Four, 0x0u32),
        (PortWidth::Two, 0xC),
        (PortWidth::One, 0xE),
    ] {
        let regs = ready_mock();
        regs.set(port::PORT_CL_DW10_A, 0x1);
        let mut request = hdmi_request(Ddi::A);
        request.width = width;
        let plan = OutputProgram::plan(&request, STRAP_38_4).unwrap();
        program(&regs, &plan).unwrap();
        let (_, written) = regs
            .writes()
            .into_iter()
            .find(|(name, _)| *name == "PORT_CL_DW10(A)")
            .unwrap();
        assert_eq!(
            written,
            0x1 | (field << PWR_DOWN_LN_MASK_SHIFT),
            "{width:?} lanes"
        );
    }
}

/// `SUS_CLOCK_CONFIG` is a read-modify-write: `CL_POWER_DOWN_ENABLE` is bit 4
/// of the same register and §8.3 set it during phase 1.
#[test]
fn the_sus_clock_config_write_keeps_the_power_down_enable() {
    let regs = ready_mock();
    regs.set(regs::COMBO_PHY_A.cl_dw5, 1 << 4);
    program(&regs, &target_plan()).unwrap();
    let (_, written) = regs
        .writes()
        .into_iter()
        .find(|(name, _)| *name == "PORT_CL_DW5(A)")
        .unwrap();
    assert_eq!(written, (1 << 4) | CL_DW5_SUS_CLOCK_CONFIG_MASK);
}

/// The successful run reports what §11 phase 6 will read back.
#[test]
fn a_successful_run_reports_the_state_the_prove_it_phase_needs() {
    let regs = ready_mock();
    let state = program(&regs, &target_plan()).unwrap();
    assert_eq!(state.ddi, Ddi::A);
    assert_eq!(state.pll_enable_readback & PLL_LOCK, PLL_LOCK);
    assert_eq!(state.ddi_buf_ctl_readback & DDI_BUF_CTL_IS_IDLE, 0);
    assert_eq!(state.ddi_buf_ctl, 0x8200_0016);
    assert_eq!(state.symbol_rate_khz, 148_500);
    assert!(state.render().contains("IS_IDLE clear"));
}

/// `IS_IDLE` is a handshake, not a value: a DDI that takes several polls to
/// come out of idle must succeed, which is what the poll loop is for.
#[test]
fn the_idle_poll_waits_for_a_ddi_that_takes_several_polls() {
    let regs = ready_mock();
    let reads = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&reads);
    regs.set(ddi::DDI_BUF_CTL_A, DDI_BUF_CTL_IS_IDLE);
    regs.on_read(ddi::DDI_BUF_CTL_A, move |stored| {
        counter.set(counter.get() + 1);
        if counter.get() >= 3 {
            stored & !DDI_BUF_CTL_IS_IDLE
        } else {
            stored | DDI_BUF_CTL_IS_IDLE
        }
    });
    let state = program(&regs, &target_plan()).expect("the third poll clears it");
    assert!(reads.get() >= 3, "the poll had to retry");
    assert_eq!(state.ddi_buf_ctl_readback & DDI_BUF_CTL_IS_IDLE, 0);
}

/// `DDI_BUF_CTL`'s enable is a read-modify-write over the plan's fields, so the
/// board's `PORT_REVERSAL` bit survives it.
///
/// `PORT_REVERSAL[16]` (`[I915]` `i915_reg.h:3868`) is not this sequence's to
/// set or clear.  i915 reads it out of this same register while initialising
/// the encoder and keeps that bit alone for `DISPLAY_VER >= 11`
/// (`display/intel_ddi.c:5115-5120`), ORs in the VBT's lane-reversal flag
/// (`:5124`), and composes the HDMI enable as
/// `saved_port_bits | DDI_BUF_CTL_ENABLE` (`:3353`, written at `:3375`).  A
/// whole-value write composed from constants would clear a lane order the
/// firmware declared, which on a board that declares one puts the TMDS pairs
/// on the wrong lanes.  Reference §8.4 names the field in its `DDI_BUF_CTL` row
/// and §11 phase 5.7's write does not carry it, so this write is the one place
/// the sequence departs from §11's step -- and only by leaving a bit alone.
#[test]
fn the_ddi_buffer_enable_keeps_the_port_reversal_bit() {
    /// `DDI_BUF_PORT_REVERSAL`, `[I915]` `i915_reg.h:3868`.
    const PORT_REVERSAL: u32 = 1 << 16;
    /// `DDI_INIT_DISPLAY_DETECTED`, bit 0 -- the legacy presence detect §8.4
    /// describes and this sequence deliberately does not write.
    const PRESENCE_DETECT: u32 = 1;

    let regs = ready_mock();
    // The firmware's word: the board's lane reversal is set, and the presence
    // detect reads set.
    regs.set(ddi::DDI_BUF_CTL_A, PORT_REVERSAL | PRESENCE_DETECT);
    let plan = target_plan();
    program(&regs, &plan).expect("the mock's status bits all behave");

    let (_, written) = regs
        .writes()
        .into_iter()
        .find(|(name, _)| *name == "DDI_BUF_CTL(A)")
        .expect("the sequence enables the buffer");
    assert_eq!(
        written,
        plan.ddi_buf_ctl | PORT_REVERSAL | PRESENCE_DETECT,
        "the enable write is the plan's fields over the word that was already there"
    );
    assert_eq!(
        regs.read(ddi::DDI_BUF_CTL_A),
        Some(0x8200_0016 | PORT_REVERSAL | PRESENCE_DETECT),
        "the register keeps the board's bit and holds the plan's fields, to the literal"
    );
    // And the plan's own word does not carry bit 16, which is exactly why the
    // write cannot be a whole-value one.
    assert_eq!(plan.ddi_buf_ctl & PORT_REVERSAL, 0);
    let log = target_plan().render();
    assert!(
        log.contains("PORT_REVERSAL[16]"),
        "the plan log does not say which bit the read-modify-write keeps:\n{log}"
    );
}

// -- the second combo PHY ---------------------------------------------------
//
// Which combo PHY the monitor is on is not known when this module is written:
// the connector probe decides it at run time from the EDID read on the GMBUS
// pin (`Pin::ddi()`), and §8.1 says the rear HDMI may be on either.  So the
// DDI B / PHY B path is exercised as a sequence of its own rather than as the
// A path with a different argument.

/// The two transcoder-side selects are keyed by different things:
/// `TRANS_CLK_SEL` by PHY on this display version, `TRANS_DDI_FUNC_CTL`'s
/// `SELECT_PORT` by DDI on every one.
///
/// `[I915]` `intel_ddi_enable_transcoder_clock` hands the **PHY** to
/// `TGL_TRANS_CLK_SEL_PORT` for `DISPLAY_VER >= 13` and the port only for
/// version 12 (`display/intel_ddi.c:996-1004`), while
/// `intel_ddi_transcoder_func_reg_val_get` composes `SELECT_PORT` from
/// `encoder->port` on every version (`:481,488-490`).  §11's steps 5.4 and 5.5
/// both say "port", so the reference states one rule where the hardware has
/// two.
///
/// **What this test cannot see, said plainly.**  The only DDIs this module
/// accepts are the combo ports A and B, and i915 maps those to PHY A and PHY B
/// one-for-one (`intel_port_to_phy` is `PHY_A + port - PORT_A` below
/// `PORT_TC1`, `display/intel_display.c:1950-1965`).  A PHY-keyed value and a
/// port-keyed one are therefore the *same number* for every plan that can be
/// built today, and a revert to `Ddi::index()` would pass every assertion
/// below.  What is pinned is the literal each field carries, and the
/// derivation's type: [`phy_index`] takes a [`ComboPhy`], which a [`Ddi`]
/// cannot be passed for.  A port whose PHY is not its own letter is where the
/// two diverge, and this sequence refuses that port before computing anything.
#[test]
fn the_clock_select_is_keyed_by_phy_and_the_port_select_by_ddi() {
    assert_eq!(phy_index(ComboPhy::A), 0, "PHY_A = 0");
    assert_eq!(phy_index(ComboPhy::B), 1, "PHY_B = 1");
    // The equality the comment above is about: true for the two combo ports,
    // and a property of i915's port-to-PHY mapping rather than of the encoding.
    assert_eq!(phy_index(ComboPhy::A), Ddi::A.index());
    assert_eq!(phy_index(ComboPhy::B), Ddi::B.index());

    let a = target_plan();
    let b = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4).unwrap();
    // `(phy + 1) << 28`, and `(ddi + 1) << 27`.
    assert_eq!(a.trans_clk_sel, 0x1000_0000);
    assert_eq!(b.trans_clk_sel, 0x2000_0000);
    assert_eq!(a.trans_ddi_func_ctl & (0b1111 << 27), 1 << 27);
    assert_eq!(b.trans_ddi_func_ctl & (0b1111 << 27), 2 << 27);
}

/// The target mode plans for DDI B, and the plan is DDI B's: PHY B, DPLL1's
/// port, and the same divider program -- the arithmetic is `pll.rs`'s and does
/// not depend on which combo PHY the port is on.
#[test]
fn combo_phy_b_plans_the_target_mode() {
    let plan = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4)
        .expect("both combo PHYs have their PLL config registers");
    assert_eq!(plan.ddi, Ddi::B);
    assert_eq!(plan.phy, ComboPhy::B);
    assert_eq!(plan.pll_registers, target_plan().pll_registers);
    assert_eq!(plan.dividers.total_divider(), 12);
    assert_eq!(plan.dividers.symbol_rate_khz(), 148_500);
    assert_eq!(plan.ddi_io_well.name, "DDI_IO_B");
    // `TRANS_CLK_SEL` takes PHY B here, and `SELECT_PORT` takes DDI B: the
    // literals are the two fields' encodings, which for this platform's combo
    // ports are the same number.  Why they are computed from different things
    // is `the_clock_select_is_keyed_by_phy_and_the_port_select_by_ddi`.
    assert_eq!(plan.trans_clk_sel, 0x2000_0000);
    assert_eq!(plan.trans_ddi_func_ctl >> 27 & 0b1111, 2, "SELECT_PORT(B)");
}

/// The whole phase-5 sequence for DDI B, on the write log: every register is
/// the B instance, and the transcoder registers are still A's -- §5.1 gives
/// the PRM's "Transcoders A-D can connect to any DDI", so the transcoder is
/// the pipe's and the port number rides inside the value.
#[test]
fn the_phy_b_write_order_is_the_sequence_with_b_registers() {
    let regs = ready_mock_for(ComboPhy::B);
    let plan = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4).unwrap();
    let state = program(&regs, &plan).expect("the mock's status bits all behave");

    let names: Vec<&str> = regs.writes().into_iter().map(|(name, _)| name).collect();
    assert_eq!(
        names,
        [
            // 5.1: power, dividers, enable -- DPLL1's registers.
            "DPLL1_ENABLE",
            "DPLL1_CFGCR0",
            "DPLL1_CFGCR1",
            "DPLL1_ENABLE",
            // 5.2: the clock select, then the clock-off clear, in two writes.
            "ICL_DPCLKA_CFGCR0",
            "ICL_DPCLKA_CFGCR0",
            // §8.6 step 5: the port's own DDI-IO power.
            "ICL_PWR_WELL_CTL_DDI2",
            // 5.3: §8.5's swing sequence, then the lane power.
            "PORT_CL_DW5(B)",
            "PORT_TX_DW5_GRP(B)",
            // §8.5 steps 2 and 5, in i915's per-lane shape: `DW2`, `DW4` and
            // `DW7` are written once per lane and `DW5` twice to the group, so
            // the firmware's per-lane values are replayed where they were read
            // ([I915] `intel_ddi.c:1148-1178`).
            "PORT_TX_DW2_LN0(B)",
            "PORT_TX_DW2_LN1(B)",
            "PORT_TX_DW2_LN2(B)",
            "PORT_TX_DW2_LN3(B)",
            "PORT_TX_DW4_LN0(B)",
            "PORT_TX_DW4_LN1(B)",
            "PORT_TX_DW4_LN2(B)",
            "PORT_TX_DW4_LN3(B)",
            "PORT_TX_DW7_LN0(B)",
            "PORT_TX_DW7_LN1(B)",
            "PORT_TX_DW7_LN2(B)",
            "PORT_TX_DW7_LN3(B)",
            "PORT_TX_DW5_GRP(B)",
            "PORT_CL_DW10(B)",
            // 5.4, 5.5, 5.6, 5.7: the transcoder is A's, the DDI inside the
            // values is B's, and the buffer is B's.
            "TRANS_CLK_SEL(A)",
            "TRANS_DDI_FUNC_CTL(A)",
            "PIPECONF_A",
            "DDI_BUF_CTL(B)",
        ]
    );
    assert_eq!(state.ddi, Ddi::B);
    assert_eq!(state.ddi_io_well.name, "DDI_IO_B");
    assert_eq!(state.trans_clk_sel, 0x2000_0000, "PHY B's clock select");
}

/// The DPLL1 config pair is written at DPLL1's addresses and DPLL0's are left
/// alone, which is the fact the refusal this path used to end in was about.
///
/// The mock stores by address, so reading DPLL0's registers back is what makes
/// "not DPLL0's address" an address-level claim rather than a name-level one.
#[test]
fn the_phy_b_plan_writes_the_dpll1_config_addresses_and_not_dpll0s() {
    // The offsets themselves, against the `[I915]` header region §6.3 cites.
    assert_eq!(dpll::DPLL0_CFGCR0.offset(), 0x16_4284);
    assert_eq!(dpll::DPLL0_CFGCR1.offset(), 0x16_4288);
    assert_eq!(dpll::DPLL1_CFGCR0.offset(), 0x16_428C);
    assert_eq!(dpll::DPLL1_CFGCR1.offset(), 0x16_4290);
    assert_ne!(
        dpll::DPLL1_CFGCR0.offset(),
        dpll::DPLL0_CFGCR0.offset() + 4,
        "DPLL1 is not the next dword after DPLL0: the pairs interleave"
    );

    let regs = ready_mock_for(ComboPhy::B);
    let plan = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4).unwrap();
    program(&regs, &plan).unwrap();

    assert_eq!(regs.write_count(dpll::DPLL1_CFGCR0), 1);
    assert_eq!(regs.write_count(dpll::DPLL1_CFGCR1), 1);
    assert_eq!(regs.write_count(dpll::DPLL0_CFGCR0), 0);
    assert_eq!(regs.write_count(dpll::DPLL0_CFGCR1), 0);
    assert_eq!(regs.write_count(dpll::DPLL0_ENABLE), 0);
    let value = |name: &str| {
        regs.writes()
            .into_iter()
            .find(|(written, _)| *written == name)
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("{name} was never written"))
    };
    assert_eq!(value("DPLL1_CFGCR0"), plan.pll_registers.cfgcr0);
    assert_eq!(value("DPLL1_CFGCR1"), plan.pll_registers.cfgcr1);
    // The same mode's divider program, byte for byte, as the reference works it
    // through -- `pll.rs` computed it and only the address changed.
    assert_eq!(value("DPLL1_CFGCR0"), 0x0010_01D0);
    assert_eq!(value("DPLL1_CFGCR1"), 0x0000_0E84);
    // Address level: DPLL0's words are untouched and DPLL1's hold the values.
    assert_eq!(regs.read(dpll::DPLL0_CFGCR0), Some(0));
    assert_eq!(regs.read(dpll::DPLL0_CFGCR1), Some(0));
    assert_eq!(regs.read(dpll::DPLL1_CFGCR0), Some(0x0010_01D0));
    assert_eq!(regs.read(dpll::DPLL1_CFGCR1), Some(0x0000_0E84));
}

/// `DPLL1_ENABLE` gets `DPLL0_ENABLE`'s dance: `POWER_ENABLE` and the
/// `POWER_STATE` poll first, then the dividers, then `PLL_ENABLE` and the
/// `LOCK` poll.
#[test]
fn dpll1_gets_the_power_enable_dance_dpll0_gets() {
    let regs = ready_mock_for(ComboPhy::B);
    let plan = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4).unwrap();
    let state = program(&regs, &plan).unwrap();

    let writes = regs.writes();
    let enable_writes: Vec<u32> = writes
        .iter()
        .filter(|(name, _)| *name == "DPLL1_ENABLE")
        .map(|(_, value)| *value)
        .collect();
    assert_eq!(enable_writes.len(), 2, "one power write, one enable write");
    assert_eq!(
        enable_writes[0] & (PLL_POWER_ENABLE | PLL_ENABLE),
        PLL_POWER_ENABLE,
        "the first write must power the block and must not enable it"
    );
    assert_eq!(
        enable_writes[1] & (PLL_POWER_ENABLE | PLL_ENABLE),
        PLL_POWER_ENABLE | PLL_ENABLE,
        "the second write enables the PLL and leaves it powered"
    );
    let position = |name: &str| {
        writes
            .iter()
            .position(|(written, _)| *written == name)
            .unwrap_or_else(|| panic!("{name} was never written"))
    };
    // The *last* enable write is the one that has to follow the dividers: the
    // first is the power write, which comes before them.
    let last_enable = writes
        .iter()
        .rposition(|(written, _)| *written == "DPLL1_ENABLE")
        .unwrap_or_else(|| panic!("DPLL1_ENABLE was never written"));
    assert!(
        position("DPLL1_ENABLE") < position("DPLL1_CFGCR0"),
        "the block is powered before its dividers are loaded"
    );
    assert!(
        position("DPLL1_CFGCR0") < position("DPLL1_CFGCR1"),
        "CFGCR0 before CFGCR1"
    );
    assert!(
        position("DPLL1_CFGCR1") < last_enable,
        "the PLL is enabled only after the dividers have landed"
    );
    // The run reached the end, so the register's `LOCK` bit was polled and
    // seen: the mock sets it only when `PLL_ENABLE` is written.
    assert_eq!(state.pll_enable_readback & PLL_LOCK, PLL_LOCK);
    assert_eq!(state.pll_enable_readback & PLL_POWER_STATE, PLL_POWER_STATE);
}

/// The `LOCK` poll on DPLL1 is a poll and not a single read: a PLL whose lock
/// bit takes a second read after `PLL_ENABLE` must still succeed.
#[test]
fn the_dpll1_lock_poll_retries() {
    let regs = ready_mock_for(ComboPhy::B);
    // One hook per register: this replaces `ready_mock_for`'s, so the power
    // state still follows the power write and only LOCK is withheld from the
    // write path.
    regs.derive(dpll::DPLL1_ENABLE, |value| value | PLL_POWER_STATE);
    let reads = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&reads);
    regs.on_read(dpll::DPLL1_ENABLE, move |stored| {
        if stored & PLL_ENABLE == 0 {
            return stored;
        }
        // Counts only the reads where the enable bit is already set, so the
        // first `LOCK` poll sees no lock and the second one does.
        counter.set(counter.get() + 1);
        if counter.get() >= 2 {
            stored | PLL_LOCK
        } else {
            stored
        }
    });
    let plan = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4).unwrap();
    let state = program(&regs, &plan).expect("the second poll locks");
    assert!(reads.get() >= 2, "the lock poll had to retry");
    assert_eq!(state.pll_enable_readback & PLL_LOCK, PLL_LOCK);
}

/// `ICL_DPCLKA_CFGCR0`'s `DDI_CLK_SEL` field carries the **PLL id**, so PHY B's
/// field selects 1 and PHY A's selects 0, and each PHY's `DDI_CLK_OFF` bit is
/// its own (§6.3 routing step 1: the field starts at `phy * 2`, two bits wide,
/// and `DDI_CLK_OFF` is bit 10 for A and 11 for B).
#[test]
fn the_clock_select_field_carries_the_pll_id_for_each_phy() {
    let a = target_plan();
    let b = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4).unwrap();
    assert_eq!(a.dpclka_select, 0, "PHY A selects DPLL0, whose id is 0");
    assert_eq!(
        b.dpclka_select,
        1 << 2,
        "PHY B selects DPLL1, id 1, at phy*2"
    );
    assert_eq!(a.dpclka_clock_off, 1 << 10, "DDI A's clock-off bit");
    assert_eq!(b.dpclka_clock_off, 1 << 11, "DDI B's clock-off bit");

    // On the wire, with the other PHY's field and the gate bit as firmware
    // could have left them.
    let regs = ready_mock_for(ComboPhy::B);
    regs.set(dpll::ICL_DPCLKA_CFGCR0, (1 << 11) | 0b11);
    program(&regs, &b).unwrap();
    let writes: Vec<u32> = regs
        .writes()
        .into_iter()
        .filter(|(name, _)| *name == "ICL_DPCLKA_CFGCR0")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(writes.len(), 2, "select and clock-off clear are two writes");
    assert_eq!(writes[0] >> 2 & 0b11, 1, "PHY B's field selects PLL 1");
    assert_eq!(writes[0] & 0b11, 0b11, "PHY A's field is left alone");
    assert_eq!(writes[0] & (1 << 11), 1 << 11, "gate still set");
    assert_eq!(writes[1] >> 2 & 0b11, 1, "select kept");
    assert_eq!(writes[1] & 0b11, 0b11, "PHY A's field still left alone");
    assert_eq!(writes[1] & (1 << 11), 0, "gate cleared");
}

// -- the failures -----------------------------------------------------------

/// `IS_IDLE` never clearing is the failure §11 phase 5.7 gives a field report
/// for, and the error has to name the DDI, the register and what to check.
#[test]
fn is_idle_never_clearing_is_a_named_error_naming_the_ddi() {
    let regs = ready_mock();
    regs.derive(ddi::DDI_BUF_CTL_A, |value| value | DDI_BUF_CTL_IS_IDLE);
    let error = program(&regs, &target_plan()).expect_err("the DDI stays idle");
    match &error {
        OutputError::DdiNeverIdle {
            ddi,
            register,
            readback,
            wrote,
            ..
        } => {
            assert_eq!(*ddi, Ddi::A);
            assert_eq!(*register, "DDI_BUF_CTL(A)");
            assert_ne!(*readback & DDI_BUF_CTL_IS_IDLE, 0);
            assert_eq!(*wrote, 0x8200_0016);
        }
        other => panic!("wrong error: {other:?}"),
    }
    let text = error.describe();
    for needle in [
        "DDI A",
        "IS_IDLE",
        "ICL_DPCLKA_CFGCR0",
        "port/aux_ch",
        "#10932",
    ] {
        assert!(text.contains(needle), "{needle:?} missing from: {text}");
    }
}

/// A PLL that never locks is named, and the error carries the divider set that
/// failed -- §11 phase 5.1's whole diagnostic point.
#[test]
fn the_pll_lock_never_setting_is_named_and_carries_the_dividers() {
    let regs = ready_mock();
    // One hook per register: this replaces `ready_mock`'s, so the power state
    // is set here too and only LOCK is withheld.
    regs.derive(dpll::DPLL0_ENABLE, |value| value | PLL_POWER_STATE);
    let error = program(&regs, &target_plan()).expect_err("LOCK never sets");
    match &error {
        OutputError::PllNeverLocked {
            register,
            p,
            q,
            k,
            wrpll_ref_khz,
            timeout_us,
            ..
        } => {
            assert_eq!(*register, "DPLL0_ENABLE");
            assert_eq!((*p, *q, *k), (2, 3, 2));
            assert_eq!(*wrpll_ref_khz, 19_200);
            assert_eq!(*timeout_us, PLL_LOCK_TIMEOUT_US);
        }
        other => panic!("wrong error: {other:?}"),
    }
    let text = error.describe();
    for needle in ["LOCK", "P=2", "K=2", "19200 kHz", "SKL_DSSM"] {
        assert!(text.contains(needle), "{needle:?} missing from: {text}");
    }
}

/// `PLL_POWER_STATE` never setting is a different named error from a lock
/// timeout: the dividers were never reached, so blaming them would mislead.
#[test]
fn the_pll_power_state_never_setting_is_its_own_error() {
    let regs = MockRegisters::new();
    let error = program(&regs, &target_plan()).expect_err("POWER_STATE never sets");
    match &error {
        OutputError::PllPowerNeverCameUp {
            register,
            readback,
            timeout_us,
        } => {
            assert_eq!(*register, "DPLL0_ENABLE");
            assert_eq!(*readback & PLL_POWER_STATE, 0);
            assert_eq!(*timeout_us, PLL_POWER_STATE_TIMEOUT_US);
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert!(error.describe().contains("PLL_POWER_STATE"));
}

/// A DDI that is not a combo-PHY port is refused before anything is written.
///
/// The plan refuses it when it is built, and the sequence refuses it again
/// before the first write -- a plan is data, and this test tampers with one to
/// prove the second check is real rather than decorative.
#[test]
fn a_ddi_that_is_not_a_combo_phy_port_is_refused_before_anything_is_written() {
    for ddi in [Ddi::C, Ddi::D] {
        let error = OutputProgram::plan(&hdmi_request(ddi), STRAP_38_4)
            .expect_err("Type-C ports are not this sequence");
        match error {
            OutputError::UnsupportedDdi { ddi: refused } => assert_eq!(refused, ddi),
            other => panic!("wrong error: {other:?}"),
        }
        assert!(
            error.describe().contains("8.8"),
            "the deferral is the reason"
        );
    }

    let regs = ready_mock();
    let mut plan = target_plan();
    plan.ddi = Ddi::C;
    let error = program(&regs, &plan).expect_err("the sequence re-checks");
    assert!(matches!(error, OutputError::UnsupportedDdi { ddi: Ddi::C }));
    assert!(
        regs.writes().is_empty(),
        "a refusal must not have written anything: {:?}",
        regs.writes()
    );
}

/// Every register of PHY B's port is declared, and its PLL config pair with it.
///
/// This is the check the old refusal was a consequence of: it used to fail on
/// the missing `DPLL1_CFGCR*`, and the point of asserting the whole set is
/// that a *different* register going missing on this path would be found here
/// rather than on the machine.
#[test]
fn every_register_of_the_phy_b_port_is_declared() {
    let registers = port_registers(ComboPhy::B);
    assert_eq!(
        registers
            .pll_cfgcr0
            .expect("DPLL1_CFGCR0 is in regs/dpll.rs")
            .name(),
        "DPLL1_CFGCR0"
    );
    assert_eq!(
        registers
            .pll_cfgcr1
            .expect("DPLL1_CFGCR1 is in regs/dpll.rs")
            .name(),
        "DPLL1_CFGCR1"
    );
    assert_eq!(registers.pll_enable.name(), "DPLL1_ENABLE");
    assert_eq!(registers.pll_enable.offset(), 0x4_6014, "LCPLL2_CTL");
    assert_eq!(registers.cl_dw5.name(), "PORT_CL_DW5(B)");
    assert_eq!(registers.cl_dw10.name(), "PORT_CL_DW10(B)");
    assert_eq!(registers.tx_dw2.name(), "PORT_TX_DW2_GRP(B)");
    assert_eq!(registers.tx_dw5.name(), "PORT_TX_DW5_GRP(B)");
    assert_eq!(registers.tx_dw7.name(), "PORT_TX_DW7_GRP(B)");
    assert_eq!(
        registers.tx_dw4.map(|register| register.name()),
        [
            "PORT_TX_DW4_LN0(B)",
            "PORT_TX_DW4_LN1(B)",
            "PORT_TX_DW4_LN2(B)",
            "PORT_TX_DW4_LN3(B)",
        ]
    );
    assert_eq!(registers.ddi_buf_ctl.name(), "DDI_BUF_CTL(B)");
    // And A's are not B's: the two arms are different registers, not one set
    // reached twice.
    let a = port_registers(ComboPhy::A);
    assert_ne!(a.ddi_buf_ctl.offset(), registers.ddi_buf_ctl.offset());
    assert_ne!(a.pll_enable.offset(), registers.pll_enable.offset());
}

/// The refusal for a PLL whose config offsets are not in the table is still
/// there, and still names what would have to be sourced.
///
/// Nothing raises it today -- DPLL0's offsets are in §6.3 and DPLL1's in the
/// `[I915]` header region §6.3 cites, so both combo PHYs plan -- and the
/// variant stays because the property it covers is the register table's
/// completeness rather than this sequence's reach: a third combo PHY added
/// without a sourced offset must be refused rather than pointed at a
/// neighbouring address.  This pins the text it would be refused with.
#[test]
fn a_phy_without_its_pll_config_offsets_would_still_be_refused_by_name() {
    for phy in [ComboPhy::A, ComboPhy::B] {
        let error = OutputError::PllConfigRegisterMissing { phy };
        let text = error.describe();
        for needle in ["CFGCR0", "CFGCR1", "13.4", "Nothing was written", "pll.rs"] {
            assert!(text.contains(needle), "{needle:?} missing from: {text}");
        }
        // The text names the PHY and its PLL, so a reader knows which one.
        assert!(
            text.contains(&format!("DPLL{}", phy.dpll_index())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "PHY {}",
                if phy == ComboPhy::A { "A" } else { "B" }
            )),
            "{text}"
        );
    }
}

/// §8.5's HDMI translation values are a `[GAP]`, and the honest implementation
/// is the one that says so before it writes anything.
#[test]
fn the_hdmi_translation_values_are_a_named_gap_and_nothing_is_written() {
    let regs = ready_mock();
    let request = OutputRequest::hdmi(Ddi::A, target_mode(), PllFieldEncoding::Named);
    let error = OutputProgram::plan(&request, STRAP_38_4).expect_err("no values to write");
    match error {
        OutputError::MissingBufferTranslation { port_type, table } => {
            assert_eq!(port_type, PortType::Hdmi);
            assert_eq!(table, Some("icl_combo_phy_trans_hdmi"));
        }
        other => panic!("wrong error: {other:?}"),
    }
    let text = error.describe();
    for needle in ["[GAP]", "13.1 item 12", "icl_combo_phy_trans_hdmi", "13.4"] {
        assert!(text.contains(needle), "{needle:?} missing from: {text}");
    }
    assert!(regs.writes().is_empty());
}

/// A pixel clock at or above the HDMI scrambling threshold is refused rather
/// than programmed half-way: the sink-side SCDC enable does not exist here.
#[test]
fn a_mode_that_needs_hdmi_scrambling_is_refused() {
    let mut mode = target_mode();
    mode.clock_khz = HDMI_SCRAMBLING_THRESHOLD_KHZ;
    let error = OutputProgram::plan(
        &OutputRequest::hdmi(Ddi::A, mode, PllFieldEncoding::Named).with_swing(test_swing()),
        STRAP_38_4,
    )
    .expect_err("340 MHz needs scrambling");
    match error {
        OutputError::HdmiScramblingNotImplemented { pixel_clock_khz } => {
            assert_eq!(pixel_clock_khz, HDMI_SCRAMBLING_THRESHOLD_KHZ);
        }
        other => panic!("wrong error: {other:?}"),
    }
    let text = error.describe();
    for needle in ["SCDC", "1080p60", "8.4"] {
        assert!(text.contains(needle), "{needle:?} missing from: {text}");
    }

    // One kHz below the threshold the same mode plans: the gate is the
    // threshold and not something else about the mode.
    mode.clock_khz = HDMI_SCRAMBLING_THRESHOLD_KHZ - 1;
    assert!(
        OutputProgram::plan(
            &OutputRequest::hdmi(Ddi::A, mode, PllFieldEncoding::Named).with_swing(test_swing()),
            STRAP_38_4,
        )
        .is_ok()
    );
}

/// A swing level outside `BUF_TRANS_SELECT`'s four bits is refused, and a
/// `PHY_LINK_RATE` code outside its four bits likewise.
#[test]
fn field_values_that_do_not_fit_their_register_fields_are_refused() {
    let mut swing = test_swing();
    swing.level = 16;
    let request =
        OutputRequest::hdmi(Ddi::A, target_mode(), PllFieldEncoding::Named).with_swing(swing);
    assert_eq!(
        OutputProgram::plan(&request, STRAP_38_4).unwrap_err(),
        OutputError::SwingLevelOutOfRange { level: 16 }
    );

    let mut request = hdmi_request(Ddi::A);
    request.link_rate = LinkRate::Code {
        code: 0x10,
        source: "test fixture",
    };
    assert_eq!(
        OutputProgram::plan(&request, STRAP_38_4).unwrap_err(),
        OutputError::LinkRateOutOfRange { code: 0x10 }
    );
}

/// An unsourced reference frequency is refused by the arithmetic before any
/// register value is computed.
#[test]
fn an_unsourced_reference_clock_is_refused() {
    let error = OutputProgram::plan(&hdmi_request(Ddi::A), 25_000).expect_err("not a real strap");
    assert!(matches!(
        error,
        OutputError::Pll(PllError::UnsupportedReference { .. })
    ));
    assert!(error.describe().contains("24 MHz"));
}

/// The reference is read from `SKL_DSSM`, and an undefined field is an error
/// rather than a fallback: §12.1 makes this field the first thing to read.
#[test]
fn the_platform_reference_comes_from_skl_dssm_and_an_undefined_field_is_an_error() {
    let regs = MockRegisters::new();
    regs.set(regs::SKL_DSSM, pll::DSSM_REFCLK_38_4MHZ);
    assert_eq!(read_platform_reference_khz(&regs).unwrap(), 38_400);
    regs.set(regs::SKL_DSSM, 3 << 29);
    let error = read_platform_reference_khz(&regs).expect_err("field 3 is not defined");
    assert!(matches!(
        error,
        OutputError::Pll(PllError::UnsupportedDssmReference { .. })
    ));
    regs.set(regs::SKL_DSSM, 0);
    assert_eq!(read_platform_reference_khz(&regs).unwrap(), 24_000);
}

/// A DBUF state of "no well" style failure: the DDI-IO well that never comes
/// up leaves the request bit withdrawn, which is `power.rs`'s rollback
/// observed through this sequence.
#[test]
fn a_ddi_io_well_that_never_comes_up_leaves_no_request_bit_set() {
    let regs = MockRegisters::new();
    // Make the PLL behave so the failure is the well's and nothing earlier.
    regs.derive(dpll::DPLL0_ENABLE, |value| {
        value | PLL_POWER_STATE | if value & PLL_ENABLE != 0 { PLL_LOCK } else { 0 }
    });
    let error = program(&regs, &target_plan()).expect_err("the well never reports state");
    match &error {
        OutputError::Well(PowerError::WellStateNeverSet {
            well, rolled_back, ..
        }) => {
            assert_eq!(*well, "DDI_IO_A");
            assert!(*rolled_back, "the request this call added was withdrawn");
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(
        regs.read(regs::ICL_PWR_WELL_CTL_DDI2),
        Some(0),
        "the request bit must not be left set"
    );
}

/// A register the sequence cannot read is named rather than treated as zero.
#[test]
fn an_unreadable_register_is_a_named_error() {
    let regs = ready_mock();
    regs.hide(dpll::ICL_DPCLKA_CFGCR0);
    let error = program(&regs, &target_plan()).expect_err("the mapping register is gone");
    match error {
        OutputError::Unreadable { register } => assert_eq!(register, "ICL_DPCLKA_CFGCR0"),
        other => panic!("wrong error: {other:?}"),
    }
}

// -- the arithmetic's provenance --------------------------------------------

/// The ADL-N search §6.3 transcribes, re-derived here from the reference rather
/// than called: the production arithmetic is `pll.rs`'s, and this is the second
/// implementation the comparison below measures it against.
///
/// §6.3: a flat divider list, the window `[7998, 10000] MHz`, a 8999 MHz
/// midpoint, and the first candidate achieving the minimum distance from that
/// midpoint.
const ADL_N_TOTAL_DIVIDERS: &[u32] = &[
    2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 50, 52, 54, 56, 60, 64,
    66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98, 100, 102, 3, 5, 7, 9, 15, 21,
];

/// The divider §6.3's search would pick for a symbol rate, in kHz.
fn documented_total_divider(symbol_rate_khz: u32) -> Option<u32> {
    const DCO_MIN_KHZ: u64 = 7_998_000;
    const DCO_MAX_KHZ: u64 = 10_000_000;
    let midpoint = (DCO_MIN_KHZ + DCO_MAX_KHZ) / 2;
    let afe_clock = 5 * u64::from(symbol_rate_khz);
    let mut best: Option<(u64, u32)> = None;
    for &divider in ADL_N_TOTAL_DIVIDERS {
        let dco = afe_clock * u64::from(divider);
        if !(DCO_MIN_KHZ..=DCO_MAX_KHZ).contains(&dco) {
            continue;
        }
        let centrality = dco.abs_diff(midpoint);
        if best.is_none_or(|(distance, _)| centrality < distance) {
            best = Some((centrality, divider));
        }
    }
    best.map(|(_, divider)| divider)
}

/// The Skylake search `pll.rs` implemented before `fix/intel-pll-adln`, kept
/// here as the other half of the measurement below.
///
/// `[I915]` `skl_ddi_calculate_wrpll` (`display/intel_dpll_mgr.c:1660-1730`):
/// three central frequencies, the asymmetric `+1%`/`-6%` tolerance, an exact
/// hit ending the search, and the even list winning outright if anything in it
/// was accepted.  This is test code and it is *deliberately* dead arithmetic:
/// the numbers it produces are the historical measurement, and without it
/// there would be no way to re-measure the divergence the fix removed, or to
/// notice a revert to it.
///
/// It returns the divider and the DCO that divider produces, in kHz.
fn skylake_total_divider(symbol_rate_khz: u32) -> Option<(u32, u64)> {
    const CENTRES_KHZ: [u64; 3] = [8_400_000, 9_000_000, 9_600_000];
    const EVEN: &[u32] = &[
        4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 52, 54, 56, 60, 64,
        66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98,
    ];
    const ODD: &[u32] = &[3, 5, 7, 9, 15, 21, 35];
    let afe_clock = 5 * u64::from(symbol_rate_khz);
    let mut best: Option<(u64, u32)> = None;
    for (index, list) in [EVEN, ODD].into_iter().enumerate() {
        let mut exact = false;
        for centre in CENTRES_KHZ {
            for &divider in list {
                let dco = u64::from(divider) * afe_clock;
                let deviation = 10_000 * centre.abs_diff(dco) / centre;
                // Positive deviation above the centre has the tighter bound.
                let within_tolerance = if dco >= centre {
                    deviation < 100
                } else {
                    deviation < 600
                };
                if within_tolerance && best.is_none_or(|(smallest, _)| deviation < smallest) {
                    best = Some((deviation, divider));
                }
                if best.is_some_and(|(smallest, _)| smallest == 0) {
                    exact = true;
                    break;
                }
            }
            if exact {
                break;
            }
        }
        // The even list is index 0, and an accepted even divider ends the
        // search -- the rule that let a 5.95%-off even divider beat an exact odd
        // one.
        if index == 0 && best.is_some() {
            break;
        }
    }
    best.map(|(_, divider)| (divider, u64::from(divider) * afe_clock))
}

/// The PRM's DCO window, as the comparison below uses it.
fn inside_window(dco_khz: u64) -> bool {
    (7_998_000..=10_000_000).contains(&dco_khz)
}

/// How far `pll.rs` is from the search §6.3 transcribes for ADL-N, and how far
/// the Skylake search it used to implement is from it.
///
/// **The first measurement is now zero**, and that is the point: `pll.rs`
/// implements the ADL-N search, so it agrees with the reference transcription on
/// every rate and every published mode, and every divider it returns is inside
/// the PRM's DCO window.  Before `fix/intel-pll-adln` it implemented the
/// Skylake search, and the second measurement here re-derives what that cost:
/// 114 of the 574 rates both searches could reach got a different divider, 12 of
/// those put the DCO outside the window -- and so did all 7 of the rates only
/// the Skylake search could reach at all.
///
/// Both halves are pinned deliberately.  The first fails if `pll.rs` is ever
/// moved off the ADL-N path; the second fails if this test stops being able to
/// see the difference, which is what would happen if the measurement were
/// reduced to a claim about a search nobody re-derives.  They are the one place
/// a reader can see the size of the gap, and `docs/design/intel-output.md` and
/// `docs/design/intel-pll.md` §3.3 carry the same figures.
#[test]
fn pll_rs_search_is_measured_against_the_documented_adl_n_search() {
    // The whole symbol-rate range the DCO window and the divider list can
    // reach, in 1 MHz steps: 16 to 1000 MHz is 985 rates.
    let rates = (16_000..=1_000_000u32).step_by(1_000);

    // -- the production search against the reference transcription ----------
    let mut covered = 0u32;
    let mut different_divider = 0u32;
    let mut outside_window = 0u32;
    let mut only_pll = 0u32;
    let mut only_documented = 0u32;
    let mut neither = 0u32;
    for rate in rates.clone() {
        let ours = pll::ddi_pll_dividers(rate, STRAP_38_4, ComboPhy::A);
        let documented = documented_total_divider(rate);
        match (ours, documented) {
            (Ok(ours), Some(documented)) => {
                covered += 1;
                if ours.total_divider() != documented {
                    different_divider += 1;
                }
                if !ours.inside_prm_dco_window() {
                    outside_window += 1;
                }
            }
            (Ok(_), None) => only_pll += 1,
            (Err(_), Some(_)) => only_documented += 1,
            (Err(_), None) => neither += 1,
        }
    }
    // The target mode is one of the rates where the two always agreed, which is
    // why this bring-up could proceed before the fix as well as after it.
    assert_eq!(documented_total_divider(148_500), Some(12));
    assert_eq!(
        pll::ddi_pll_dividers(148_500, STRAP_38_4, ComboPhy::A)
            .unwrap()
            .total_divider(),
        12
    );

    // Measured on this host with the tree as committed: `pll.rs` and the
    // transcription agree everywhere, no answer is outside the window, and the
    // 166 rates neither can make are the two gap bands between the candidate
    // list's entries (500.001-533.199 MHz and 666.667-799.799 MHz).
    assert_eq!(
        (
            covered,
            different_divider,
            outside_window,
            only_pll,
            only_documented,
            neither
        ),
        (819, 0, 0, 0, 0, 166)
    );

    // -- the same 985 rates against the search `pll.rs` used to implement ----
    let mut compared = 0u32;
    let mut skylake_different = 0u32;
    let mut skylake_different_and_outside = 0u32;
    let mut skylake_outside = 0u32;
    let mut only_skylake = 0u32;
    let mut only_skylake_outside = 0u32;
    let mut only_adl_n = 0u32;
    for rate in rates {
        let skylake = skylake_total_divider(rate);
        let documented = documented_total_divider(rate);
        match (skylake, documented) {
            (Some((divider, dco)), Some(documented)) => {
                compared += 1;
                if divider != documented {
                    skylake_different += 1;
                    if !inside_window(dco) {
                        skylake_different_and_outside += 1;
                    }
                }
                if !inside_window(dco) {
                    skylake_outside += 1;
                }
            }
            (Some((_, dco)), None) => {
                only_skylake += 1;
                if !inside_window(dco) {
                    only_skylake_outside += 1;
                }
            }
            (None, Some(_)) => only_adl_n += 1,
            (None, None) => {}
        }
    }
    // Reading the tuple: 574 rates both searches can make, 114 of those where
    // they pick a different total divider, 12 of those 114 where the Skylake
    // choice is also outside the PRM's `[7998, 10000] MHz` window (and 12 is
    // the whole out-of-window count), 7 rates only the Skylake list reaches,
    // and 245 rates only the documented ADL-N list reaches.
    assert_eq!(
        (
            compared,
            skylake_different,
            skylake_different_and_outside,
            skylake_outside,
            only_skylake,
            only_adl_n
        ),
        (574, 114, 12, 12, 7, 245)
    );
    // The 7 rates the fix gives up are 527-533 MHz, where the Skylake search
    // took divider 3 at 7905-7995 MHz -- every one of them below the window's
    // 7998 MHz floor.  Nothing that used to lock stops locking.
    assert_eq!(
        only_skylake_outside, only_skylake,
        "every rate only the Skylake search could reach had an out-of-window DCO anyway"
    );

    // -- the same comparison over the modes this kernel publishes ----------
    // The interlaced rows are skipped because `timing.rs` refuses them anyway.
    let mut modes_covered = 0u32;
    let mut modes_different = 0u32;
    let mut modes_only_pll = 0u32;
    let mut modes_only_documented = 0u32;
    let mut modes_skylake_compared = 0u32;
    let mut modes_skylake_different = 0u32;
    let mut modes_only_skylake = 0u32;
    let mut modes_only_adl_n = 0u32;
    let dmt = DMT_TIMINGS.iter().map(|entry| entry.mode);
    let cta = CTA_VIC_TIMINGS.iter().map(|entry| entry.mode);
    for mode in dmt.chain(cta) {
        if mode.flags.contains(crate::drm::modes::ModeFlags::INTERLACE) {
            continue;
        }
        let ours = pll::ddi_pll_dividers(mode.clock_khz, STRAP_38_4, ComboPhy::A);
        let documented = documented_total_divider(mode.clock_khz);
        match (ours, documented) {
            (Ok(ours), Some(documented)) => {
                modes_covered += 1;
                if ours.total_divider() != documented {
                    modes_different += 1;
                }
            }
            (Ok(_), None) => modes_only_pll += 1,
            (Err(_), Some(_)) => modes_only_documented += 1,
            (Err(_), None) => {}
        }
        match (skylake_total_divider(mode.clock_khz), documented) {
            (Some((divider, _)), Some(documented)) => {
                modes_skylake_compared += 1;
                if divider != documented {
                    modes_skylake_different += 1;
                }
            }
            (Some(_), None) => modes_only_skylake += 1,
            (None, Some(_)) => modes_only_adl_n += 1,
            (None, None) => {}
        }
    }
    // `pll.rs` now makes every published progressive mode the transcription
    // can, with no disagreement at all.
    assert_eq!(
        (
            modes_covered,
            modes_different,
            modes_only_pll,
            modes_only_documented
        ),
        (182, 0, 0, 0)
    );
    // The historical measurement, re-derived: one published mode (CTA VIC 92)
    // was reachable only by the documented search, and 56 of the 181 both could
    // make got a different divider.  56 of 181 is nearly a third.
    assert_eq!(
        (
            modes_skylake_compared,
            modes_skylake_different,
            modes_only_skylake,
            modes_only_adl_n
        ),
        (181, 56, 0, 1)
    );

    // The single published mode in that last bucket is CTA VIC 92 --
    // 2560x1440p @ 120 Hz, 495 MHz.  Before the fix the PLL search refused it
    // and now it does not; the sequence still refuses it, but at the scrambling
    // gate, which is earlier than the arithmetic.
    assert!(documented_total_divider(495_000).is_some());
    assert!(pll::ddi_pll_dividers(495_000, STRAP_38_4, ComboPhy::A).is_ok());
    assert!(matches!(
        OutputProgram::plan(
            &OutputRequest::hdmi(Ddi::A, vic(92), PllFieldEncoding::Named).with_swing(test_swing()),
            STRAP_38_4
        ),
        Err(OutputError::HdmiScramblingNotImplemented { .. })
    ));
}

/// DVI is the same sequence with a different mode select, and the reference
/// names no translation table for it -- which the refusal has to say.
#[test]
fn dvi_differs_from_hdmi_only_in_the_mode_select_and_has_no_named_table() {
    let mut dvi = hdmi_request(Ddi::A);
    dvi.port_type = PortType::Dvi;
    let dvi_plan = OutputProgram::plan(&dvi, STRAP_38_4).unwrap();
    let hdmi_plan = target_plan();

    // §8.4: `TRANS_DDI_MODE_SELECT_MASK[26:24]` is HDMI = 0, DVI = 1, and
    // nothing else in the register moves.
    let mode_select = 0b111 << TRANS_DDI_MODE_SELECT_SHIFT;
    assert_eq!(hdmi_plan.trans_ddi_func_ctl & mode_select, 0);
    assert_eq!(
        dvi_plan.trans_ddi_func_ctl & mode_select,
        1 << TRANS_DDI_MODE_SELECT_SHIFT
    );
    assert_eq!(
        dvi_plan.trans_ddi_func_ctl & !mode_select,
        hdmi_plan.trans_ddi_func_ctl & !mode_select
    );
    assert_eq!(dvi_plan.ddi_buf_ctl, hdmi_plan.ddi_buf_ctl);
    assert_eq!(dvi_plan.trans_clk_sel, hdmi_plan.trans_clk_sel);
    assert_eq!(dvi_plan.transconf, hdmi_plan.transconf);

    dvi.swing = None;
    let error = OutputProgram::plan(&dvi, STRAP_38_4).unwrap_err();
    match error {
        OutputError::MissingBufferTranslation { port_type, table } => {
            assert_eq!(port_type, PortType::Dvi);
            assert_eq!(table, None);
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert!(
        error
            .describe()
            .contains("names one for HDMI and none for DVI")
    );
}
