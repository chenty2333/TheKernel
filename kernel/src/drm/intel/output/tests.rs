//! Host tests for [`super`], driven through `regs::mock::MockRegisters`.
//!
//! What a test here can establish: the *sequence*.  Which registers are
//! written, in what order, how many times, with what values; that a refusal
//! happens before the first write; that a status bit which never appears
//! produces a named error rather than a panic; and that the values the
//! reference document prints for the target mode are the values this module
//! computes.  What no test here can establish: that any of it is accepted by a
//! Gen12 display engine.  Nothing in this module has run on real hardware.

use alloc::{rc::Rc, vec::Vec};
use core::cell::Cell;

use super::*;
use crate::drm::{
    intel::regs::mock::MockRegisters,
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
fn test_swing() -> SwingProgram {
    SwingProgram {
        level: 2,
        dw2: 0x0C,
        dw4: [0x30, 0x31, 0x31, 0x31],
        dw5_training_disabled: 0x0000_0000,
        dw5_training_enabled: 0x0002_0000,
        dw7: 0x0071,
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
fn ready_mock() -> MockRegisters {
    let regs = MockRegisters::new();
    regs.derive(dpll::DPLL0_ENABLE, |value| {
        let mut stored = value;
        if value & PLL_POWER_ENABLE != 0 {
            stored |= PLL_POWER_STATE;
        }
        if value & PLL_ENABLE != 0 {
            stored |= PLL_LOCK;
        }
        stored
    });
    regs.derive(ddi::DDI_BUF_CTL_A, |value| {
        if value & DDI_BUF_CTL_ENABLE != 0 {
            value & !DDI_BUF_CTL_IS_IDLE
        } else {
            value | DDI_BUF_CTL_IS_IDLE
        }
    });
    regs.derive(regs::ICL_PWR_WELL_CTL_DDI2, |value| {
        value | power::well_state(power::DDI_IO_A.index)
    });
    regs
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
#[test]
fn the_transcoder_and_ddi_values_are_the_reference_encodings() {
    let plan = target_plan();
    assert_eq!(plan.trans_clk_sel, 0x1000_0000, "TRANS_CLK_SEL(A)");
    assert_eq!(
        plan.trans_ddi_func_ctl, 0x8803_0006,
        "ENABLE | SELECT_PORT(A) | HDMI | 8bpc | both syncs | four lanes"
    );
    assert_eq!(plan.transconf, 0xC000_0000, "ENABLE | STATE_ENABLE");
    assert_eq!(
        plan.ddi_buf_ctl, 0x8200_0016,
        "ENABLE | BUF_TRANS_SELECT(2) | PORT_WIDTH(4 lanes) | A_4_LANES"
    );
}

/// No output bit depth in `TRANSCONF`: §8.4's correction and §11 phase 5.6 put
/// it in `PIPE_MISC`, and §8.6 step 12 still lists it here.
#[test]
fn transconf_carries_no_bit_depth() {
    let plan = target_plan();
    // `TRANSCONF`'s BPC field is a pre-Haswell leftover; the value is exactly
    // enable plus state-enable, nothing else.
    assert_eq!(plan.transconf, (1 << 31) | (1 << 30));
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
            // 5.3: §8.5's swing sequence, then the lane power.
            "PORT_CL_DW5(A)",
            "PORT_TX_DW5_GRP(A)",
            "PORT_TX_DW2_GRP(A)",
            "PORT_TX_DW4_LN0(A)",
            "PORT_TX_DW4_LN1(A)",
            "PORT_TX_DW4_LN2(A)",
            "PORT_TX_DW4_LN3(A)",
            "PORT_TX_DW7_GRP(A)",
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

/// §8.5 step 2: `PORT_TX_DW4` is written once per lane, in lane order, with the
/// four values the caller supplied.
#[test]
fn the_swing_writes_reach_the_per_lane_registers_in_lane_order() {
    let regs = ready_mock();
    let swing = test_swing();
    program(&regs, &target_plan()).unwrap();
    let lane_writes: Vec<(&str, u32)> = regs
        .writes()
        .into_iter()
        .filter(|(name, _)| name.starts_with("PORT_TX_DW4_LN"))
        .collect();
    assert_eq!(
        lane_writes,
        [
            ("PORT_TX_DW4_LN0(A)", swing.dw4[0]),
            ("PORT_TX_DW4_LN1(A)", swing.dw4[1]),
            ("PORT_TX_DW4_LN2(A)", swing.dw4[2]),
            ("PORT_TX_DW4_LN3(A)", swing.dw4[3]),
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

/// Combo PHY B is refused for the one reason it can be: its PLL's config
/// registers are not in the register table, because the reference never states
/// their offsets.
#[test]
fn combo_phy_b_is_refused_for_its_missing_pll_config_registers() {
    let error = OutputProgram::plan(&hdmi_request(Ddi::B), STRAP_38_4)
        .expect_err("DPLL1's config offsets are not in the table");
    match error {
        OutputError::PllConfigRegisterMissing { phy } => assert_eq!(phy, ComboPhy::B),
        other => panic!("wrong error: {other:?}"),
    }
    let text = error.describe();
    for needle in ["DPLL1_CFGCR0", "0x164284", "13.4"] {
        assert!(text.contains(needle), "{needle:?} missing from: {text}");
    }
    // The refusal is about the PLL config registers and nothing else: every
    // other register of B's port is declared.
    let registers = port_registers(ComboPhy::B);
    assert!(registers.pll_cfgcr0.is_none());
    assert!(registers.pll_cfgcr1.is_none());
    assert_eq!(registers.pll_enable.name(), "DPLL1_ENABLE");
    assert_eq!(registers.ddi_buf_ctl.name(), "DDI_BUF_CTL(B)");
    assert_eq!(registers.cl_dw10.name(), "PORT_CL_DW10(B)");
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
