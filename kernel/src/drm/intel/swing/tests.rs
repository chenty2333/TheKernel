//! Host tests for [`super`], driven through `regs::mock::MockRegisters`.
//!
//! What a test here can establish: that a register state a firmware could have
//! left is read back into the right [`SwingProgram`] fields, that the level
//! comes from `DDI_BUF_CTL.BUF_TRANS_SELECT`, that `PORT_TX_DW2`, `PORT_TX_DW4`
//! and `PORT_TX_DW7` each come from four *different* registers -- one per lane,
//! at the lane stride -- while `PORT_TX_DW5` comes from lane 0 and not from the
//! group instance the sequence writes, that DDI A and DDI B read different
//! registers, and that every state this module refuses is refused by name
//! rather than quietly believed.  What no test here can establish: that any of
//! it matches a real Gen12 display engine, or that the firmware on the target
//! machine leaves the state these tests model.  Nothing in this module has run
//! on hardware, so the register state below is *invented* -- it is shaped like
//! a program (enable set, not idle, training enabled, lanes up) but its numbers
//! are not from any table and are not claimed to be.

use alloc::{format, vec, vec::Vec};

use super::*;
use crate::drm::{
    intel::{pll::PllFieldEncoding, regs::mock::MockRegisters},
    modes::CTA_VIC_TIMINGS,
};

// -- a register state shaped like a firmware program -------------------------

/// The level the invented state below selected.
const LEVEL: u8 = 4;

/// `PORT_TX_DW2`'s invented values, one per lane and all different.
///
/// All different on purpose: the four lanes are four addresses, and a read that
/// collapsed them (the group instance, a wrong stride, one lane read four
/// times) would fail a test whose values were equal.
const DW2: [u32; 4] = [0x0000_0198, 0x0000_01A8, 0x0000_01B8, 0x0000_01C8];

/// The four invented `PORT_TX_DW4` values, one per lane and all different, so
/// that a read which collapsed them fails the test instead of passing it.
const DW4: [u32; 4] = [0x8000_003F, 0x8000_0039, 0x8000_0037, 0x8000_0031];

/// `PORT_TX_DW7`'s invented values, one per lane and all different.
const DW7: [u32; 4] = [0x0000_0071, 0x0000_0072, 0x0000_0073, 0x0000_0074];

/// The invented contents of `PORT_TX_DW5`, **lane 0**, after §8.5's step 6.
///
/// The fields the sequence sets are the sourced part: `TX_TRAINING_EN` (bit 31,
/// §8.5 step 6, `[I915]` `display/intel_combo_phy_regs.h:133`),
/// `SCALING_MODE_SEL = 0b010` (bits [20:18], §8.5 step 5) and `TAP3_DISABLE`
/// plus `RTERM_SELECT = 0b110`, which i915 writes on the same register
/// (`[I915]` `display/intel_ddi.c:1141-1145`) and §8.5 does not name.  The
/// point of putting them in is that the read must copy the whole dword: a
/// masked read would lose the fields this module has no names for.
fn dw5_training_enabled() -> u32 {
    TX_TRAINING_EN | (0b010 << 18) | (1 << 29) | (0b110 << 3)
}

/// The invented contents of the **group** `PORT_TX_DW5`, deliberately a
/// different word.
///
/// The module reads lane 0 (`[I915]` `display/intel_ddi.c:1141`, `:1218`,
/// `:1226`, all `ICL_PORT_TX_DW5_LN(0, phy)`) and writes the group instance
/// (`:1146`, `:1221`, `:1229`).  It differs from lane 0 in the scaling-mode and
/// RTERM fields, so a read of the group instance would produce a different
/// `SwingProgram` -- and, one field further out, could still pass the training
/// check.  These are the numbers that make "lane 0, not the group" measurable.
fn dw5_group() -> u32 {
    TX_TRAINING_EN | (0b001 << 18) | (1 << 29) | (0b001 << 3)
}

/// The group `PORT_TX_DW5` of the same port: written by the sequence, never
/// read by it.
fn dw5_group_register(ddi: Ddi) -> Register {
    match ddi {
        Ddi::A => port::PORT_TX_DW5_GRP_A,
        Ddi::B => port::PORT_TX_DW5_GRP_B,
        Ddi::C | Ddi::D => panic!("C and D are not combo-PHY ports"),
    }
}

/// A mock in the state the module's premise describes: combo PHY initialised,
/// DDI buffer enabled and not idle, lanes powered, the four TX registers
/// programmed one lane at a time, training enabled on lane 0.
fn firmware_mock(ddi: Ddi) -> MockRegisters {
    let registers = source_registers(ddi).expect("A and B are combo-PHY ports");
    let regs = MockRegisters::new();
    regs.set(registers.comp_dw0, COMP_INIT);
    regs.set(
        registers.ddi_buf_ctl,
        DDI_BUF_CTL_ENABLE | (u32::from(LEVEL) << DDI_BUF_TRANS_SELECT_SHIFT),
    );
    regs.set(registers.cl_dw10, 0);
    for (lane, register) in registers.tx_dw2.iter().enumerate() {
        regs.set(*register, DW2[lane]);
    }
    for (lane, register) in registers.tx_dw4.iter().enumerate() {
        regs.set(*register, DW4[lane]);
    }
    regs.set(registers.tx_dw5_lane0, dw5_training_enabled());
    regs.set(dw5_group_register(ddi), dw5_group());
    for (lane, register) in registers.tx_dw7.iter().enumerate() {
        regs.set(*register, DW7[lane]);
    }
    regs
}

// -- the values --------------------------------------------------------------

/// The read returns every programmed value, in its own field.
#[test]
fn a_firmware_programmed_port_reads_back_whole() {
    let regs = firmware_mock(Ddi::A);
    let swing =
        read_firmware_swing(&regs, Ddi::A).expect("the state is what the module believes in");
    assert_eq!(swing.level, LEVEL);
    assert_eq!(swing.dw2, DW2);
    assert_eq!(swing.dw4, DW4);
    assert_eq!(swing.dw5_training_enabled, dw5_training_enabled());
    assert_eq!(swing.dw7, DW7);
    assert_eq!(
        swing.source,
        "read back from the PHY the firmware programmed, DDI A level 4"
    );
}

/// The level comes from `BUF_TRANS_SELECT[27:24]` and nothing else does.
///
/// A second level must move `level` and `source` and leave the swing words
/// alone: they are the firmware's values, not a function of the index.
#[test]
fn the_level_comes_from_buf_trans_select() {
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let regs = firmware_mock(Ddi::A);
    regs.set(
        registers.ddi_buf_ctl,
        DDI_BUF_CTL_ENABLE | (9 << DDI_BUF_TRANS_SELECT_SHIFT),
    );
    let swing = read_firmware_swing(&regs, Ddi::A).expect("still a programmed port");
    assert_eq!(swing.level, 9);
    assert_eq!(
        swing.source,
        "read back from the PHY the firmware programmed, DDI A level 9"
    );
    assert_eq!(swing.dw2, DW2, "the level is not derived from the values");
    assert_eq!(swing.dw4, DW4);
    assert_eq!(swing.dw7, DW7);
}

/// The two `PORT_TX_DW5` states differ by `TX_TRAINING_EN` and by nothing else.
///
/// §8.5 steps 4 and 6, and `[I915]` `display/intel_ddi.c:1218-1229`, which
/// reads the register, clears that bit, writes it, programs the batch, then
/// reads, sets the bit and writes again.
#[test]
fn the_training_disabled_state_is_the_read_state_with_one_bit_cleared() {
    let regs = firmware_mock(Ddi::A);
    let swing = read_firmware_swing(&regs, Ddi::A).expect("a programmed port");
    assert_eq!(swing.dw5_training_enabled, dw5_training_enabled());
    assert_eq!(
        swing.dw5_training_disabled,
        dw5_training_enabled() & !TX_TRAINING_EN
    );
    // The derived state keeps the fields §8.5 does not name, which is why the
    // dword is carried whole rather than rebuilt from named fields.
    assert_eq!(swing.dw5_training_disabled & (0b010 << 18), 0b010 << 18);
    assert_eq!(swing.dw5_training_disabled & (0b110 << 3), 0b110 << 3);
    assert_ne!(swing.dw5_training_disabled, 0);
}

/// `PORT_TX_DW5` is read from **lane 0**, not from the group instance.
///
/// The two hold different words in this mock, and they differ in fields a
/// masked read would hide (an implementation that read the group could still
/// find `TX_TRAINING_EN` set and believe the wrong swing).  i915 reads
/// `ICL_PORT_TX_DW5_LN(0, phy)` and writes `ICL_PORT_TX_DW5_GRP(phy)` on both
/// sides of the batch (`[I915]` `display/intel_ddi.c:1218-1229`), which is the
/// shape `output::program` replays.
#[test]
fn the_dw5_read_is_lane_zero_and_not_the_group() {
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let regs = firmware_mock(Ddi::A);
    assert_ne!(
        registers.tx_dw5_lane0.offset(),
        dw5_group_register(Ddi::A).offset(),
        "lane 0 and the group instance are one address only if something is wrong"
    );
    let swing = read_firmware_swing(&regs, Ddi::A).expect("a programmed port");
    assert_eq!(swing.dw5_training_enabled, dw5_training_enabled());
    assert_ne!(
        swing.dw5_training_enabled,
        dw5_group(),
        "the group instance's word is not the one to carry"
    );

    // And the other way round: a port whose group holds the training bit while
    // lane 0 does not is refused, because lane 0 is the copy the sequence
    // reads.
    regs.set(registers.tx_dw5_lane0, dw5_group() & !TX_TRAINING_EN);
    regs.set(dw5_group_register(Ddi::A), dw5_training_enabled());
    let refusal = read_firmware_swing(&regs, Ddi::A).expect_err("lane 0 has no training bit");
    assert_eq!(
        refusal,
        SwingSource::TrainingNotEnabled {
            ddi: Ddi::A,
            register: "PORT_TX_DW5_LN0(A)",
            readback: dw5_group() & !TX_TRAINING_EN,
        }
    );
}

/// Every lane of `PORT_TX_DW2`, `DW4` and `DW7` comes from its own register.
///
/// Four distinct values in four distinct registers per dword: a group read would
/// have returned one value four times, and a wrong stride would have returned a
/// neighbour's word.
#[test]
fn every_lane_of_every_per_lane_dword_is_read_from_its_own_register() {
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let regs = firmware_mock(Ddi::A);
    let swing = read_firmware_swing(&regs, Ddi::A).expect("a programmed port");
    assert_eq!(swing.dw2, DW2);
    assert_eq!(swing.dw4, DW4);
    assert_eq!(swing.dw7, DW7);
    for (lane, value) in DW2.iter().enumerate() {
        assert_eq!(swing.dw2[lane], *value, "DW2 lane {lane}");
        assert_eq!(
            registers.tx_dw2[lane].offset(),
            0x16_2888 + 0x100 * lane as u32,
            "DW2 lane {lane} is not at the lane stride"
        );
    }
    for (lane, value) in DW4.iter().enumerate() {
        assert_eq!(swing.dw4[lane], *value, "DW4 lane {lane}");
    }
    for (lane, value) in DW7.iter().enumerate() {
        assert_eq!(swing.dw7[lane], *value, "DW7 lane {lane}");
        assert_eq!(
            registers.tx_dw7[lane].offset(),
            0x16_289c + 0x100 * lane as u32,
            "DW7 lane {lane} is not at the lane stride"
        );
    }
    // One lane's address is not another's: the group instances a collapsed read
    // would have to have used are somewhere else entirely.
    for (lane, register) in registers.tx_dw2.iter().enumerate() {
        assert_ne!(
            register.offset(),
            port::PORT_TX_DW2_GRP_A.offset(),
            "DW2 lane {lane} is the group instance"
        );
    }
    for (lane, register) in registers.tx_dw7.iter().enumerate() {
        assert_ne!(
            register.offset(),
            port::PORT_TX_DW7_GRP_A.offset(),
            "DW7 lane {lane} is the group instance"
        );
    }
}

/// DDI A and DDI B are read from different registers.
///
/// §8.1 and §8.2: PHY A is at `0x162000` and PHY B at `0x06C000`, and each has
/// its own `DDI_BUF_CTL`, `PORT_CL_DW10` and TX registers.  This is the test
/// that would fail if the two DDIs shared a register mapping -- the failure
/// mode being a mode set on port B from port A's calibration.
#[test]
fn ddi_a_and_ddi_b_read_different_registers() {
    let a = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let b = source_registers(Ddi::B).expect("B is a combo-PHY port");
    let offsets_a = [
        a.comp_dw0.offset(),
        a.ddi_buf_ctl.offset(),
        a.cl_dw10.offset(),
        a.tx_dw2[0].offset(),
        a.tx_dw2[1].offset(),
        a.tx_dw2[2].offset(),
        a.tx_dw2[3].offset(),
        a.tx_dw4[0].offset(),
        a.tx_dw4[1].offset(),
        a.tx_dw4[2].offset(),
        a.tx_dw4[3].offset(),
        a.tx_dw5_lane0.offset(),
        a.tx_dw7[0].offset(),
        a.tx_dw7[1].offset(),
        a.tx_dw7[2].offset(),
        a.tx_dw7[3].offset(),
    ];
    let offsets_b = [
        b.comp_dw0.offset(),
        b.ddi_buf_ctl.offset(),
        b.cl_dw10.offset(),
        b.tx_dw2[0].offset(),
        b.tx_dw2[1].offset(),
        b.tx_dw2[2].offset(),
        b.tx_dw2[3].offset(),
        b.tx_dw4[0].offset(),
        b.tx_dw4[1].offset(),
        b.tx_dw4[2].offset(),
        b.tx_dw4[3].offset(),
        b.tx_dw5_lane0.offset(),
        b.tx_dw7[0].offset(),
        b.tx_dw7[1].offset(),
        b.tx_dw7[2].offset(),
        b.tx_dw7[3].offset(),
    ];
    for (index, offset_a) in offsets_a.iter().enumerate() {
        assert_ne!(
            *offset_a, offsets_b[index],
            "register {index} is the same address on both ports"
        );
    }
    assert_eq!(a.ddi_buf_ctl.offset(), 0x6_4000, "DDI_BUF_CTL(A)");
    assert_eq!(b.ddi_buf_ctl.offset(), 0x6_4100, "DDI_BUF_CTL(B)");
    assert_eq!(a.tx_dw2[0].offset(), 0x16_2888, "PORT_TX_DW2_LN0(A)");
    assert_eq!(b.tx_dw2[0].offset(), 0x6_c888, "PORT_TX_DW2_LN0(B)");
    assert_eq!(a.tx_dw5_lane0.offset(), 0x16_2894, "PORT_TX_DW5_LN0(A)");
    assert_eq!(b.tx_dw7[3].offset(), 0x6_cb9c, "PORT_TX_DW7_LN3(B)");

    // And the behaviour that follows from it: one mock holding two different
    // programs returns each port's own.
    let regs = firmware_mock(Ddi::A);
    regs.set(b.comp_dw0, COMP_INIT);
    regs.set(
        b.ddi_buf_ctl,
        DDI_BUF_CTL_ENABLE | (7 << DDI_BUF_TRANS_SELECT_SHIFT),
    );
    regs.set(b.cl_dw10, 0);
    for (lane, register) in b.tx_dw2.iter().enumerate() {
        regs.set(*register, 0x0000_0200 | lane as u32);
    }
    for (lane, register) in b.tx_dw4.iter().enumerate() {
        regs.set(*register, 0x4000_0000 | lane as u32);
    }
    regs.set(b.tx_dw5_lane0, dw5_training_enabled() | (1 << 30));
    regs.set(dw5_group_register(Ddi::B), dw5_group());
    for (lane, register) in b.tx_dw7.iter().enumerate() {
        regs.set(*register, 0x0000_0050 | lane as u32);
    }

    let swing_a = read_firmware_swing(&regs, Ddi::A).expect("A is programmed");
    let swing_b = read_firmware_swing(&regs, Ddi::B).expect("B is programmed");
    assert_eq!(swing_a.level, LEVEL);
    assert_eq!(swing_b.level, 7);
    assert_eq!(swing_a.dw2, DW2);
    assert_eq!(swing_b.dw2, [0x200, 0x201, 0x202, 0x203]);
    assert_eq!(swing_a.dw4, DW4);
    assert_eq!(
        swing_b.dw4,
        [0x4000_0000, 0x4000_0001, 0x4000_0002, 0x4000_0003]
    );
    assert_eq!(swing_a.dw7, DW7);
    assert_eq!(swing_b.dw7, [0x50, 0x51, 0x52, 0x53]);
    assert_eq!(
        swing_b.source,
        "read back from the PHY the firmware programmed, DDI B level 7"
    );
    assert_ne!(swing_a, swing_b);
}

// -- the refusals ------------------------------------------------------------

/// A port nothing has programmed is refused by name, and the name says why.
#[test]
fn an_unprogrammed_port_is_refused_by_name() {
    // An aperture whose words all read zero, which is what an untouched
    // register block answers: COMP_INIT is clear.
    let regs = MockRegisters::new();
    let refusal = read_firmware_swing(&regs, Ddi::A).expect_err("nothing was programmed");
    assert_eq!(
        refusal,
        SwingSource::PhyNotInitialised {
            ddi: Ddi::A,
            register: "PORT_COMP_DW0(A)",
            readback: 0,
        }
    );
    let text = refusal.describe();
    assert!(text.contains("PORT_COMP_DW0(A)"), "{text}");
    assert!(text.contains("COMP_INIT"), "{text}");
    assert!(text.contains("section 8.3"), "{text}");
    assert!(text.contains("Nothing was believed"), "{text}");

    // A PHY that is initialised but whose DDI buffer was never enabled is the
    // other half of "the firmware did not program this port".
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let regs = MockRegisters::new();
    regs.set(registers.comp_dw0, COMP_INIT);
    let refusal = read_firmware_swing(&regs, Ddi::A).expect_err("the buffer is not enabled");
    assert_eq!(
        refusal,
        SwingSource::PortNotEnabled {
            ddi: Ddi::A,
            register: "DDI_BUF_CTL(A)",
            readback: 0,
        }
    );
    let text = refusal.describe();
    assert!(text.contains("DDI_BUF_CTL(A)"), "{text}");
    assert!(text.contains("ENABLE"), "{text}");
    assert!(text.contains("section 8.6"), "{text}");

    // And a DDI with no combo PHY behind it is refused before any read: there
    // is no register mapping to read from.
    for ddi in [Ddi::C, Ddi::D] {
        let regs = firmware_mock(Ddi::A);
        let refusal = read_firmware_swing(&regs, ddi).expect_err("C and D are Type-C ports");
        assert_eq!(refusal, SwingSource::UnsupportedDdi { ddi });
        assert!(refusal.describe().contains("section 8.1"));
    }
}

/// A port that is up but whose committed program is missing is refused by name.
///
/// This is the consistency check the whole module rests on: `DDI_BUF_CTL` says
/// the port is enabled and not idle, so the TX registers *look* like a program,
/// and the one bit that says §8.5's step 6 ran is clear.  Believing the rest
/// would replay a batch the sequence never committed.
#[test]
fn a_state_that_fails_the_training_check_is_refused_by_name() {
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let regs = firmware_mock(Ddi::A);
    regs.set(
        registers.tx_dw5_lane0,
        dw5_training_enabled() & !TX_TRAINING_EN,
    );
    let refusal = read_firmware_swing(&regs, Ddi::A).expect_err("step 6 never ran");
    assert_eq!(
        refusal,
        SwingSource::TrainingNotEnabled {
            ddi: Ddi::A,
            register: "PORT_TX_DW5_LN0(A)",
            readback: dw5_training_enabled() & !TX_TRAINING_EN,
        }
    );
    let text = refusal.describe();
    assert!(text.contains("PORT_TX_DW5_LN0(A)"), "{text}");
    assert!(text.contains("TX_TRAINING_EN"), "{text}");
    assert!(text.contains("section 8.5"), "{text}");
}

/// A port whose buffer says it is driving while every lane is down is refused.
#[test]
fn a_port_with_every_lane_powered_down_is_refused_by_name() {
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let regs = firmware_mock(Ddi::A);
    regs.set(registers.cl_dw10, PWR_DOWN_LN_MASK);
    let refusal = read_firmware_swing(&regs, Ddi::A).expect_err("the lanes cannot be down");
    assert_eq!(
        refusal,
        SwingSource::LanesAllPoweredDown {
            ddi: Ddi::A,
            register: "PORT_CL_DW10(A)",
            readback: PWR_DOWN_LN_MASK,
        }
    );
    let text = refusal.describe();
    assert!(text.contains("PORT_CL_DW10(A)"), "{text}");
    assert!(text.contains("8.6 step 7"), "{text}");
}

/// An enabled port that is still idle is refused: §11's "is my DDI alive" bit.
#[test]
fn an_idle_port_is_refused_by_name() {
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let regs = firmware_mock(Ddi::A);
    regs.set(registers.ddi_buf_ctl, DDI_BUF_CTL_ENABLE | DDI_BUF_IS_IDLE);
    let refusal = read_firmware_swing(&regs, Ddi::A).expect_err("the port is not driving");
    assert_eq!(
        refusal,
        SwingSource::PortStillIdle {
            ddi: Ddi::A,
            register: "DDI_BUF_CTL(A)",
            readback: DDI_BUF_CTL_ENABLE | DDI_BUF_IS_IDLE,
        }
    );
    let text = refusal.describe();
    assert!(text.contains("IS_IDLE"), "{text}");
    assert!(text.contains("phase 5.7"), "{text}");
}

/// A PHY that answers as an absent block is refused, both degenerate values.
#[test]
fn an_absent_phy_is_refused_by_name() {
    for dead in [0u32, u32::MAX] {
        let registers = source_registers(Ddi::B).expect("B is a combo-PHY port");
        let regs = firmware_mock(Ddi::B);
        for register in registers.tx_dw2.iter().chain(registers.tx_dw7.iter()) {
            regs.set(*register, dead);
        }
        regs.set(registers.tx_dw5_lane0, dead);
        let refusal = read_firmware_swing(&regs, Ddi::B).expect_err("the block is not answering");
        assert_eq!(
            refusal,
            SwingSource::PhyNotResponding {
                ddi: Ddi::B,
                dw2: [dead; 4],
                dw5: dead,
                dw7: [dead; 4],
            }
        );
        let text = refusal.describe();
        assert!(text.contains("section 12.2"), "{text}");
    }
}

/// One degenerate lane is not an absent block: §12.2's answer is every instance.
#[test]
fn one_degenerate_lane_is_not_an_absent_block() {
    let registers = source_registers(Ddi::B).expect("B is a combo-PHY port");
    let regs = firmware_mock(Ddi::B);
    regs.set(registers.tx_dw2[2], 0);
    let swing = read_firmware_swing(&regs, Ddi::B).expect("the other lanes answered");
    assert_eq!(swing.dw2[2], 0);
    assert_eq!(swing.dw2[0], DW2[0]);
}

/// A register that cannot be read at all is a named refusal, not a zero.
#[test]
fn a_register_that_cannot_be_read_is_refused_by_name() {
    let registers = source_registers(Ddi::A).expect("A is a combo-PHY port");
    let mut every = vec![
        registers.comp_dw0,
        registers.ddi_buf_ctl,
        registers.cl_dw10,
        registers.tx_dw5_lane0,
    ];
    every.extend(registers.tx_dw2.iter().copied());
    every.extend(registers.tx_dw4.iter().copied());
    every.extend(registers.tx_dw7.iter().copied());
    for register in every {
        let regs = firmware_mock(Ddi::A);
        regs.hide(register);
        let refusal =
            read_firmware_swing(&regs, Ddi::A).expect_err("a hidden register cannot be read");
        assert_eq!(
            refusal,
            SwingSource::Unreadable {
                ddi: Ddi::A,
                register: register.name(),
            },
            "{}",
            register.name()
        );
        assert!(refusal.describe().contains("section 2.2"));
    }
}

/// Every refusal describes itself, names the DDI, and is not a panic.
#[test]
fn every_refusal_describes_itself() {
    let refusals: Vec<SwingSource> = vec![
        SwingSource::UnsupportedDdi { ddi: Ddi::C },
        SwingSource::Unreadable {
            ddi: Ddi::A,
            register: "PORT_TX_DW2_LN0(A)",
        },
        SwingSource::PhyNotInitialised {
            ddi: Ddi::A,
            register: "PORT_COMP_DW0(A)",
            readback: 0,
        },
        SwingSource::PortNotEnabled {
            ddi: Ddi::B,
            register: "DDI_BUF_CTL(B)",
            readback: 0,
        },
        SwingSource::PortStillIdle {
            ddi: Ddi::B,
            register: "DDI_BUF_CTL(B)",
            readback: DDI_BUF_CTL_ENABLE | DDI_BUF_IS_IDLE,
        },
        SwingSource::LanesAllPoweredDown {
            ddi: Ddi::B,
            register: "PORT_CL_DW10(B)",
            readback: PWR_DOWN_LN_MASK,
        },
        SwingSource::PhyNotResponding {
            ddi: Ddi::A,
            dw2: [0; 4],
            dw5: 0,
            dw7: [0; 4],
        },
        SwingSource::TrainingNotEnabled {
            ddi: Ddi::A,
            register: "PORT_TX_DW5_LN0(A)",
            readback: 0,
        },
    ];
    for refusal in refusals {
        let text = refusal.describe();
        assert!(!text.is_empty());
        assert_eq!(format!("{refusal}"), text, "Display and describe agree");
        // Every one of these refusals is about a specific port, so the text has
        // to name it: a log line without the DDI sends a reader to the wrong
        // PHY.
        let ddi = match &refusal {
            SwingSource::UnsupportedDdi { ddi }
            | SwingSource::Unreadable { ddi, .. }
            | SwingSource::PhyNotInitialised { ddi, .. }
            | SwingSource::PortNotEnabled { ddi, .. }
            | SwingSource::PortStillIdle { ddi, .. }
            | SwingSource::LanesAllPoweredDown { ddi, .. }
            | SwingSource::PhyNotResponding { ddi, .. }
            | SwingSource::TrainingNotEnabled { ddi, .. } => *ddi,
        };
        assert!(
            text.contains(&format!("DDI {}", ddi.name())),
            "{} does not name its DDI: {text}",
            ddi.name()
        );
    }
}

// -- the composition phase 5 will make ---------------------------------------

/// The read-back program is exactly what `OutputProgram::plan` was refusing for.
///
/// `OutputRequest::hdmi` starts with `swing: None` by design, so phase 5 cannot
/// run at all until something supplies a `SwingProgram`
/// (`OutputError::MissingBufferTranslation`).  This is the composition the
/// module documentation describes, end to end, on a mock: read, `with_swing`,
/// plan -- and the plan carries the level and the whole dword the read found.
#[test]
fn the_read_back_program_unblocks_phase_five() {
    let regs = firmware_mock(Ddi::A);
    let swing = read_firmware_swing(&regs, Ddi::A).expect("a programmed port");

    let mode = CTA_VIC_TIMINGS
        .iter()
        .find(|entry| entry.vic == 16)
        .expect("CTA-861 VIC 16 is in the table")
        .mode;
    let request =
        crate::drm::intel::output::OutputRequest::hdmi(Ddi::A, mode, PllFieldEncoding::Named);
    // Without the read the plan is refused, naming the gap; that is the
    // deliberate refusal this module exists to satisfy.
    let refusal = crate::drm::intel::output::OutputProgram::plan(&request, 38_400)
        .expect_err("no swing values, no plan");
    assert!(matches!(
        refusal,
        crate::drm::intel::output::OutputError::MissingBufferTranslation { .. }
    ));

    let request = request.with_swing(swing);
    let plan = crate::drm::intel::output::OutputProgram::plan(&request, 38_400)
        .expect("the read supplied what was missing");
    assert_eq!(plan.ddi, Ddi::A);
    assert_eq!(plan.swing, swing);
    // §8.6 step 13: the level read out of the port is the level written back.
    assert_eq!(
        (plan.ddi_buf_ctl >> DDI_BUF_TRANS_SELECT_SHIFT) & 0xf,
        u32::from(LEVEL)
    );
    assert_ne!(plan.ddi_buf_ctl & DDI_BUF_CTL_ENABLE, 0);
    assert_eq!(plan.swing.dw2, DW2);
    assert_eq!(plan.swing.dw4, DW4);
    assert_eq!(plan.swing.dw7, DW7);
}
