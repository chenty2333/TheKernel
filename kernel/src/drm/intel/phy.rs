//! Combo PHY initialisation.
//!
//! The two combo PHYs on `XE_LPD` (`A` at `0x162000`, `B` at `0x06C000`) are
//! analogue blocks that have to be told what silicon they are sitting on before
//! any lane can be used.  `PORT_COMP_DW3` reports the process and voltage
//! variant; the driver looks that up in a table and writes the matching
//! reference values into `PORT_COMP_DW1`, `DW9` and `DW10`.  Get it wrong and
//! nothing fails loudly: the link simply does not train, later, somewhere else.
//!
//! Two structural facts drive the order, both from reference §8.3:
//!
//! * **PHY A is the compensation source for PHY B.**  A comp source must be
//!   initialised before its sinks and must stay initialised while they are in
//!   use, so A comes first and it is A that gets `IREFGEN`
//!   (`[I915]` `display/intel_combo_phy.c:189-215`, `phy_is_master`, which
//!   returns true for `PHY_A` unconditionally and false for everything else on
//!   ADL-P/N).  Initialising B while A is not initialised gives wrong
//!   termination.
//! * **The step order within one PHY is not arbitrary.**  `PORT_COMP_DW0`'s
//!   `COMP_INIT` bit is written *last*, after the reference values it
//!   validates, and `CL_DW5`'s `CL_POWER_DOWN_ENABLE` after that.
//!
//! This runs **before** power well `PW_1` is enabled, which is the order
//! §4.9's `icl_display_core_init` uses (step 2 before step 3).  That is why
//! `COMP_INIT` not staying set is only *recorded* here rather than treated as
//! fatal: §11 phase 1.2 says the cause is a missing `PW_1`, which at this point
//! in the sequence has not been asked for yet.  [`super::power::bring_up`]
//! re-reads it after `PW_1` is up and reports the pair.
//!
//! ## What this module does not do
//!
//! It does not program DDI buffer translation, lane power, or anything about a
//! port.  It initialises the PHY's compensation and clock-detect paths and
//! stops; §8.5 and §11 phase 5 belong to the DDI workstream.
//!
//! It also does not probe for PHY presence by writing a scratch pattern, which
//! §12.2 marks `[INF]` as the way to tell an absent instance from a reset one.
//! On ADL-N the two combo PHYs are known from `[I915]`'s `xe_lpd_display` port
//! mask and from `intel_combo_phy_regs.h`'s own naming (the `C`, `D` and `E`
//! instances are EHL's, RKL's and ADL-S's), so there is nothing to discover —
//! and a scratch write would have to be undone before `COMP_DW1`'s process and
//! voltage fields are programmed, which is a worse risk than the one it
//! removes.  The all-zero/all-ones read §12.2 describes is still taken and
//! reported, as a warning rather than as a decision, because a block that is
//! merely power-gated reads zero too and skipping initialisation on that basis
//! would turn a diagnostic into a failure.

use alloc::{format, string::String, vec::Vec};

use super::regs::{self, ComboPhyRegisters, Register, Registers};

/// One row of the process/voltage reference table.
///
/// `[I915]` `icl_procmon_values[]` (`display/intel_combo_phy.c:28-52`) and
/// `[PRM]` "Procmon Reference Values" — the reference document §8.3 records
/// that the two give **identical** values, which is the strongest kind of
/// agreement available here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcmonRow {
    pub(crate) name: &'static str,
    /// The two masked fields of `PORT_COMP_DW1` this row states.
    pub(crate) dw1: u32,
    /// A whole-register value for `PORT_COMP_DW9`.
    pub(crate) dw9: u32,
    /// A whole-register value for `PORT_COMP_DW10`.
    pub(crate) dw10: u32,
}

/// The five reference rows, in the order `[I915]` declares them.
pub(crate) const PROCMON_ROWS: &[ProcmonRow] = &[
    ProcmonRow {
        name: "0.85V dot0 (low-voltage)",
        dw1: 0x0000_0000,
        dw9: 0x62AB_67BB,
        dw10: 0x5191_4F96,
    },
    ProcmonRow {
        name: "0.95V dot0",
        dw1: 0x0000_0000,
        dw9: 0x86E1_72C7,
        dw10: 0x77CA_5EAB,
    },
    ProcmonRow {
        name: "0.95V dot1",
        dw1: 0x0000_0000,
        dw9: 0x93F8_7FE1,
        dw10: 0x8AE8_71C5,
    },
    ProcmonRow {
        name: "1.05V dot0",
        dw1: 0x0000_0000,
        dw9: 0x98FA_82DD,
        dw10: 0x89E4_6DC1,
    },
    ProcmonRow {
        name: "1.05V dot1",
        dw1: 0x0044_0000,
        dw9: 0x9A00_AB25,
        dw10: 0x8AE3_8FF1,
    },
];

/// What `PORT_COMP_DW3` says the part is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessVoltage {
    /// `PORT_COMP_DW3[28:26]`: 0 is dot-0, 1 is dot-1, 2 is dot-4.
    pub(crate) process: u32,
    /// `PORT_COMP_DW3[25:24]`: 0 is 0.85 V, 1 is 0.95 V, 2 is 1.05 V.
    pub(crate) voltage: u32,
    /// Whether the pair is one of the five the table has a row for.
    pub(crate) recognised: bool,
}

impl ProcessVoltage {
    pub(crate) fn describe(&self) -> String {
        format!(
            "process dot-{}, {} mV{}",
            self.process,
            voltage_millivolts(self.voltage),
            if self.recognised {
                ""
            } else {
                " (no table row: using the low-voltage row, as i915's fallthrough does)"
            },
        )
    }
}

/// The voltage encoding in millivolts, for a log line.
const fn voltage_millivolts(voltage: u32) -> u32 {
    match voltage {
        1 => 950,
        2 => 1050,
        _ => 850,
    }
}

/// Decode `PORT_COMP_DW3`.
///
/// `PROCESS_INFO_MASK = [28:26]` and `VOLTAGE_INFO_MASK = [25:24]`
/// (`[I915]` `intel_combo_phy_regs.h:59-67`).  The five combinations the table
/// has rows for are 0.85 V dot-0, 0.95 V dot-0, 0.95 V dot-1, 1.05 V dot-0 and
/// 1.05 V dot-1; anything else — including dot-4, which the mask can encode but
/// no row covers — is reported as unrecognised and falls back to the 0.85 V
/// low-voltage row, which is what `icl_get_procmon_ref_values`'s `default:
/// MISSING_CASE(val); fallthrough;` does.
pub(crate) fn process_voltage(comp_dw3: u32) -> ProcessVoltage {
    let process = comp_dw3 >> 26 & 0b111;
    let voltage = comp_dw3 >> 24 & 0b11;
    let recognised = matches!(
        (process, voltage),
        (0, 0) | (0, 1) | (1, 1) | (0, 2) | (1, 2)
    );
    ProcessVoltage {
        process,
        voltage,
        recognised,
    }
}

/// The reference row for a decoded process and voltage.
pub(crate) fn procmon_row(part: ProcessVoltage) -> &'static ProcmonRow {
    match (part.process, part.voltage) {
        (0, 1) => &PROCMON_ROWS[1],
        (1, 1) => &PROCMON_ROWS[2],
        (0, 2) => &PROCMON_ROWS[3],
        (1, 2) => &PROCMON_ROWS[4],
        // 0.85 V dot-0, and every combination the table does not state.
        _ => &PROCMON_ROWS[0],
    }
}

/// `PORT_COMP_DW1[7:0]` and `[23:16]`, the fields the reference values live in.
pub(crate) const PROC_DW1_MASK: u32 = (0xFF << 16) | 0xFF;
/// `PORT_COMP_DW0[31]`.
pub(crate) const COMP_INIT: u32 = 1 << 31;
/// `PORT_COMP_DW8[24]`.
pub(crate) const IREFGEN: u32 = 1 << 24;
/// `PORT_TX_DW8[31]`.
pub(crate) const ODCC_CLK_SEL: u32 = 1 << 31;
/// `PORT_TX_DW8[30:29]`.
pub(crate) const ODCC_CLK_DIV_SEL_MASK: u32 = 0b11 << 29;
/// `PORT_TX_DW8[30:29] = 01b`, divide by two.
pub(crate) const ODCC_CLK_DIV_SEL_DIV2: u32 = 1 << 29;
/// `PORT_PCS_DW1[21:20]`.
pub(crate) const DCC_MODE_SELECT_MASK: u32 = 0b11 << 20;
/// `PORT_PCS_DW1[21:20] = 00b`: run DCC once.
pub(crate) const RUN_DCC_ONCE: u32 = 0;
/// `PORT_CL_DW5[4]`.
pub(crate) const CL_POWER_DOWN_ENABLE: u32 = 1 << 4;
/// `ICL_PHY_MISC[23]`.
pub(crate) const DE_IO_COMP_PWR_DOWN: u32 = 1 << 23;

/// One check from the verification pass, so a failure names itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PhyCheck {
    pub(crate) what: &'static str,
    pub(crate) passed: bool,
    pub(crate) read: u32,
}

/// Everything observed about one PHY.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PhyState {
    pub(crate) port: &'static str,
    /// Whether the block answered with a value that is not all-zero or
    /// all-ones.  A warning, not a decision: see the module header.
    pub(crate) looks_present: bool,
    /// The raw `PORT_COMP_DW0` reads that decided `looks_present`.
    pub(crate) comp_dw0_reads: [u32; 2],
    /// What `PORT_COMP_DW3` said.
    pub(crate) part: ProcessVoltage,
    pub(crate) procmon: &'static ProcmonRow,
    /// Whether this PHY is the compensation source for the others.
    pub(crate) comp_source: bool,
    /// Whether the PHY was already initialised and was therefore left alone.
    pub(crate) already_initialised: bool,
    /// The verification checks when the PHY claimed to be initialised, or the
    /// single `COMP_INIT` check when it did not.  Empty only if the PHY was not
    /// reached.
    pub(crate) checks: Vec<PhyCheck>,
    /// `COMP_INIT` after this step, which on the success path is the value the
    /// device kept rather than the value that was written.
    pub(crate) comp_init: bool,
    /// Every register this PHY's initialisation wrote, in order.
    pub(crate) writes: usize,
}

impl PhyState {
    /// Whether the PHY is initialised as far as this step can tell.
    pub(crate) fn initialised(&self) -> bool {
        self.comp_init
    }

    pub(crate) fn describe(&self) -> String {
        let mut text = format!(
            "combo PHY {}: {}, {}, {} compensation source",
            self.port,
            if self.looks_present {
                format!("present (COMP_DW0 {:#010x})", self.comp_dw0_reads[1])
            } else {
                format!(
                    "reads all-zero or all-ones twice ({:#010x}, {:#010x}): if this PHY never \
                     comes up, that is the first thing to check",
                    self.comp_dw0_reads[0], self.comp_dw0_reads[1]
                )
            },
            self.part.describe(),
            if self.comp_source {
                "is the"
            } else {
                "is not a"
            },
        );
        if self.already_initialised {
            text.push_str("; already initialised by firmware, left alone");
        } else {
            text.push_str(&format!("; initialised ({} register writes)", self.writes));
        }
        if !self.comp_init {
            text.push_str(
                "; COMP_INIT does not read back set -- at this point in the sequence that means \
                 PW_1 is not on yet (reference section 11 phase 1.2), which is expected here and \
                 is re-checked after PW_1",
            );
        }
        for check in &self.checks {
            if !check.passed {
                text.push_str(&format!(
                    "; check failed: {} (read {:#010x})",
                    check.what, check.read
                ));
            }
        }
        text
    }
}

/// What can go wrong initialising a PHY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PhyError {
    Unreadable { register: &'static str },
    WriteRefused { register: &'static str },
}

impl PhyError {
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Unreadable { register } => {
                format!("{register} could not be read: it is outside the mapped register window")
            }
            Self::WriteRefused { register } => {
                format!("{register} refused the write, so the PHY was not initialised")
            }
        }
    }
}

/// Read a register the sequence cannot do without.
fn read(regs: &impl Registers, register: Register) -> Result<u32, PhyError> {
    regs.read(register).ok_or(PhyError::Unreadable {
        register: register.name(),
    })
}

/// Write a register, refusing to continue if the write did not happen.
fn write(regs: &impl Registers, register: Register, value: u32) -> Result<(), PhyError> {
    if regs.write(register, value) {
        Ok(())
    } else {
        Err(PhyError::WriteRefused {
            register: register.name(),
        })
    }
}

/// Read-modify-write, which is how every field in these registers is set.
///
/// `clear` and `set` are the two halves of i915's `intel_de_rmw(reg, clear,
/// set)`, which computes `(read & !clear) | set`.  That convention is taken
/// rather than a "field mask and value" one on purpose: this sequence is
/// checked against `icl_combo_phys_init`, where `rmw(reg, 0, BIT)` means "set
/// this bit and clear nothing" -- which a mask-and-value helper would silently
/// turn into a no-op.
///
/// The read and the write are separate bus transactions with nothing
/// serialising them against another agent, which is fine here for the reason it
/// is fine in the vendor driver: nothing else owns these registers during
/// display bring-up.
fn rmw(regs: &impl Registers, register: Register, clear: u32, set: u32) -> Result<u32, PhyError> {
    let current = read(regs, register)?;
    write(regs, register, (current & !clear) | set)?;
    Ok(current)
}

/// Initialise every combo PHY, source first.
pub(crate) fn init_all(regs: &impl Registers) -> Result<Vec<PhyState>, PhyError> {
    // The list in `regs` is ordered source-first, and PHY A is the source.  The
    // invariant the order exists for is asserted rather than trusted: if the
    // first entry is not PHY A, the compensation source is being initialised
    // after a sink that depends on it.
    debug_assert_eq!(
        regs::COMBO_PHYS.first().map(|phy| phy.port),
        Some("A"),
        "the compensation source must be initialised before its sinks"
    );
    let mut states = Vec::with_capacity(regs::COMBO_PHYS.len());
    for (index, phy) in regs::COMBO_PHYS.iter().enumerate() {
        states.push(init_one(regs, *phy, index == 0)?);
    }
    Ok(states)
}

/// Initialise one PHY.
///
/// The sequence is §8.3, which is `[I915]` `icl_combo_phys_init`
/// (`display/intel_combo_phy.c:308-372`) step for step:
///
/// ```text
/// if already initialised (COMP_INIT set and every reference value verified):
///     leave it alone, as i915's verify_state does
/// clear ICL_PHY_MISC.DE_IO_COMP_PWR_DOWN
/// PORT_TX_DW8 (group) <- lane 0's value, with ODCC_CLK_SEL and divisor 2   [display 12+]
/// PORT_PCS_DW1 (group) <- lane 0's value, with DCC mode RUN_DCC_ONCE      [display 12+]
/// PORT_COMP_DW1 <- the row's fields, leaving every other bit alone
/// PORT_COMP_DW9  <- the row's value
/// PORT_COMP_DW10 <- the row's value
/// if this PHY is a comp source: PORT_COMP_DW8 |= IREFGEN
/// PORT_COMP_DW0 |= COMP_INIT
/// PORT_CL_DW5 |= CL_POWER_DOWN_ENABLE
/// ```
///
/// Steps 2 and 3 of that list are the display-version-12 additions: the
/// reference records that `[I915]` gates them on `DISPLAY_VER >= 12` and that
/// ADL-N is display 13, so they apply.  The `[PRM]` volumes disagree about what
/// the DCC step means ("DCC continuous mode" in one, "divide by 2" in another);
/// §13.3 records that i915 is unambiguous and targets Gen12, so
/// `RUN_DCC_ONCE` is what is written here.
pub(crate) fn init_one(
    regs: &impl Registers,
    phy: ComboPhyRegisters,
    comp_source: bool,
) -> Result<PhyState, PhyError> {
    // Two reads of COMP_DW0, which is the whole of the presence test: a block
    // that is not there answers all-zero or all-ones, twice.
    let first = read(regs, phy.comp_dw0)?;
    let second = read(regs, phy.comp_dw0)?;
    let looks_present = !matches!(first, 0 | u32::MAX) || !matches!(second, 0 | u32::MAX);

    let comp_dw3 = read(regs, phy.comp_dw3)?;
    let part = process_voltage(comp_dw3);
    let procmon = procmon_row(part);

    let before = read(regs, phy.phy_misc)?;
    let mut checks = Vec::new();
    let mut already_initialised = false;

    // The verification `[I915]` `icl_combo_phy_verify_state` performs on
    // `COMP_INIT` being set.  Doing it rather than trusting the one bit is what
    // keeps a firmware-initialised PHY untouched -- and, when it does not
    // verify, it says which value disagreed instead of reprogramming blind.
    if second & COMP_INIT != 0 {
        checks.push(PhyCheck {
            what: "COMP_INIT is set",
            passed: true,
            read: second,
        });
        checks.push(PhyCheck {
            what: "ICL_PHY_MISC does not power down the IO compensation",
            passed: before & DE_IO_COMP_PWR_DOWN == 0,
            read: before,
        });
        let tx_dw8 = read(regs, phy.tx_dw8_ln0)?;
        let odcc = ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_MASK;
        checks.push(PhyCheck {
            what: "lane 0 TX_DW8 selects the ODCC clock with divisor 2",
            passed: tx_dw8 & odcc == ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_DIV2,
            read: tx_dw8,
        });
        let pcs_dw1 = read(regs, phy.pcs_dw1_ln0)?;
        checks.push(PhyCheck {
            what: "lane 0 PCS_DW1 runs DCC once",
            passed: pcs_dw1 & DCC_MODE_SELECT_MASK == RUN_DCC_ONCE,
            read: pcs_dw1,
        });
        let dw1 = read(regs, phy.comp_dw1)?;
        checks.push(PhyCheck {
            what: "COMP_DW1 holds the reference row's fields",
            passed: dw1 & PROC_DW1_MASK == procmon.dw1,
            read: dw1,
        });
        let dw9 = read(regs, phy.comp_dw9)?;
        checks.push(PhyCheck {
            what: "COMP_DW9 holds the reference row's value",
            passed: dw9 == procmon.dw9,
            read: dw9,
        });
        let dw10 = read(regs, phy.comp_dw10)?;
        checks.push(PhyCheck {
            what: "COMP_DW10 holds the reference row's value",
            passed: dw10 == procmon.dw10,
            read: dw10,
        });
        if comp_source {
            let dw8 = read(regs, phy.comp_dw8)?;
            checks.push(PhyCheck {
                what: "COMP_DW8 has IREFGEN, which a compensation source needs",
                passed: dw8 & IREFGEN == IREFGEN,
                read: dw8,
            });
        }
        let cl_dw5 = read(regs, phy.cl_dw5)?;
        checks.push(PhyCheck {
            what: "CL_DW5 enables the PHY's clock power-down",
            passed: cl_dw5 & CL_POWER_DOWN_ENABLE == CL_POWER_DOWN_ENABLE,
            read: cl_dw5,
        });
        already_initialised = checks.iter().all(|check| check.passed);
    }

    if already_initialised {
        return Ok(PhyState {
            port: phy.port,
            looks_present,
            comp_dw0_reads: [first, second],
            part,
            procmon,
            comp_source,
            already_initialised: true,
            checks,
            comp_init: true,
            writes: 0,
        });
    }

    // From here the PHY is programmed.  A verification that failed leaves its
    // checks in the log, which is what says *which* value disagreed.
    let mut writes = 0usize;

    rmw(regs, phy.phy_misc, DE_IO_COMP_PWR_DOWN, 0)?;
    writes += 1;

    // The ODCC and DCC steps start from lane 0's value and write the group
    // register, which is what `[I915]` does; see the register table's comment.
    let tx_dw8 = read(regs, phy.tx_dw8_ln0)?;
    let tx_dw8 = (tx_dw8 & !ODCC_CLK_DIV_SEL_MASK) | ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_DIV2;
    write(regs, phy.tx_dw8, tx_dw8)?;
    writes += 1;

    let pcs_dw1 = read(regs, phy.pcs_dw1_ln0)?;
    let pcs_dw1 = (pcs_dw1 & !DCC_MODE_SELECT_MASK) | RUN_DCC_ONCE;
    write(regs, phy.pcs_dw1, pcs_dw1)?;
    writes += 1;

    rmw(regs, phy.comp_dw1, PROC_DW1_MASK, procmon.dw1)?;
    writes += 1;
    write(regs, phy.comp_dw9, procmon.dw9)?;
    writes += 1;
    write(regs, phy.comp_dw10, procmon.dw10)?;
    writes += 1;

    if comp_source {
        rmw(regs, phy.comp_dw8, 0, IREFGEN)?;
        writes += 1;
    }

    rmw(regs, phy.comp_dw0, 0, COMP_INIT)?;
    writes += 1;
    rmw(regs, phy.cl_dw5, 0, CL_POWER_DOWN_ENABLE)?;
    writes += 1;

    // §11 phase 1.2: "COMP_INIT does not stay set.  Re-read it."  It is read
    // back here, and again after PW_1 by `power::bring_up`, because at this
    // point in the sequence a zero is expected rather than a fault.
    let comp_dw0 = read(regs, phy.comp_dw0)?;
    let comp_init = comp_dw0 & COMP_INIT != 0;
    checks.push(PhyCheck {
        what: "COMP_INIT stayed set after being written",
        passed: comp_init,
        read: comp_dw0,
    });

    Ok(PhyState {
        port: phy.port,
        looks_present,
        comp_dw0_reads: [first, second],
        part,
        procmon,
        comp_source,
        already_initialised: false,
        checks,
        comp_init,
        writes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::intel::regs::mock::MockRegisters;

    /// A PHY whose `COMP_DW3` reports 0.95 V dot-1 and whose registers respond
    /// the way a device would.
    fn mock_phy(phy: ComboPhyRegisters, part: u32) -> MockRegisters {
        let regs = MockRegisters::new();
        regs.set(phy.comp_dw3, part);
        regs
    }

    #[test]
    fn the_reference_table_is_the_one_both_sources_agree_on() {
        // §8.3's "procmon table", which the reference records as identical
        // between `[PRM]` "Procmon Reference Values" and `[I915]`
        // `icl_procmon_values[]` -- a mutual verification worth pinning.
        let expected: [(&str, u32, u32, u32); 5] = [
            (
                "0.85V dot0 (low-voltage)",
                0x0000_0000,
                0x62AB_67BB,
                0x5191_4F96,
            ),
            ("0.95V dot0", 0x0000_0000, 0x86E1_72C7, 0x77CA_5EAB),
            ("0.95V dot1", 0x0000_0000, 0x93F8_7FE1, 0x8AE8_71C5),
            ("1.05V dot0", 0x0000_0000, 0x98FA_82DD, 0x89E4_6DC1),
            ("1.05V dot1", 0x0044_0000, 0x9A00_AB25, 0x8AE3_8FF1),
        ];
        assert_eq!(PROCMON_ROWS.len(), expected.len());
        for (row, (name, dw1, dw9, dw10)) in PROCMON_ROWS.iter().zip(expected) {
            assert_eq!(
                (row.name, row.dw1, row.dw9, row.dw10),
                (name, dw1, dw9, dw10)
            );
        }
        // Only the 1.05 V dot-1 row has anything in DW1, which is the fact
        // that makes the masked write at that register observable: a row with
        // `dw1 == 0` still has to clear whatever was there.
        assert_eq!(
            PROCMON_ROWS
                .iter()
                .filter(|row| row.dw1 != 0)
                .map(|row| row.name)
                .collect::<alloc::vec::Vec<_>>(),
            alloc::vec!["1.05V dot1"]
        );
    }

    #[test]
    fn every_documented_process_and_voltage_selects_its_row() {
        // `PORT_COMP_DW3[28:26]` is the process, `[25:24]` the voltage
        // (`intel_combo_phy_regs.h:59-67`).  The five combinations with a row.
        let cases: [(u32, u32, usize); 5] = [(0, 0, 0), (0, 1, 1), (1, 1, 2), (0, 2, 3), (1, 2, 4)];
        for (process, voltage, index) in cases {
            let comp_dw3 = (process << 26) | (voltage << 24);
            let part = process_voltage(comp_dw3);
            assert!(part.recognised, "dot-{process} {voltage}");
            assert_eq!((part.process, part.voltage), (process, voltage));
            assert_eq!(procmon_row(part).name, PROCMON_ROWS[index].name);
            // Bits outside the two fields must not change the answer: DW3
            // carries other live information.  The fields are [28:24], so the
            // noise deliberately stays out of them.
            let noisy = comp_dw3 | 0xE0FF_FFFF;
            assert_eq!(process_voltage(noisy), part);
        }
    }

    #[test]
    fn an_unrecognised_process_or_voltage_falls_back_and_says_so() {
        // dot-4 is encodable by the process field and has no row; 1.05 V dot-4
        // and the voltage encoding 3 are the same kind of case.  i915 logs a
        // missing case and uses the low-voltage row, which is what happens
        // here -- with the flag that keeps it from looking like a reading.
        for (process, voltage) in [(2u32, 0u32), (2, 2), (0, 3), (3, 3), (7, 2)] {
            let part = process_voltage((process << 26) | (voltage << 24));
            assert!(!part.recognised, "dot-{process} voltage {voltage}");
            assert_eq!(procmon_row(part).name, "0.85V dot0 (low-voltage)");
            assert!(
                part.describe().contains("no table row"),
                "{}",
                part.describe()
            );
        }
    }

    #[test]
    fn initialising_a_phy_writes_the_sequence_the_reference_gives() {
        // The whole sequence, in order, against a PHY that reports 0.95 V
        // dot-1 and keeps what it is given.
        let phy = regs::COMBO_PHY_A;
        let regs = mock_phy(phy, (1 << 26) | (1 << 24));
        // A COMP_DW0 that is neither zero nor all-ones and does not have
        // COMP_INIT: a PHY that is there and has not been initialised.
        regs.set(phy.comp_dw0, 1);
        // Values that must survive the read-modify-write steps.
        regs.set(phy.phy_misc, DE_IO_COMP_PWR_DOWN | (1 << 28));
        regs.set(phy.tx_dw8_ln0, 0x1234_5678);
        regs.set(phy.pcs_dw1_ln0, 0x0000_0001);
        regs.set(phy.comp_dw1, 0xFFFF_FFFF);
        regs.set(phy.cl_dw5, 0b11);

        let state = init_one(&regs, phy, true).unwrap();
        assert!(state.comp_source);
        assert!(state.looks_present);
        assert!(!state.already_initialised);
        assert!(state.comp_init, "the mock keeps COMP_INIT");
        assert_eq!(state.part.recognised, true);
        assert_eq!(state.procmon.name, "0.95V dot1");

        let writes = regs.writes();
        let names: alloc::vec::Vec<&str> = writes.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            alloc::vec![
                "ICL_PHY_MISC(A)",
                "PORT_TX_DW8(A)",
                "PORT_PCS_DW1(A)",
                "PORT_COMP_DW1(A)",
                "PORT_COMP_DW9(A)",
                "PORT_COMP_DW10(A)",
                "PORT_COMP_DW8(A)",
                "PORT_COMP_DW0(A)",
                "PORT_CL_DW5(A)",
            ],
        );
        // The values, checked individually because each is a different kind of
        // claim: a mask-preserving write, a derived value, a whole-register
        // constant, and two bit sets.
        let value = |name: &str| {
            writes
                .iter()
                .find(|(written, _)| *written == name)
                .map(|(_, value)| *value)
                .unwrap_or_else(|| panic!("{name} was not written"))
        };
        // PHY_MISC: the power-down bit cleared, the unrelated bit kept.
        assert_eq!(value("ICL_PHY_MISC(A)"), 1 << 28);
        // TX_DW8: lane 0's value with the ODCC clock selected and divided by 2.
        assert_eq!(
            value("PORT_TX_DW8(A)"),
            0x1234_5678 | ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_DIV2
        );
        // PCS_DW1: lane 0's value with the DCC mode field cleared to RUN_DCC_ONCE.
        assert_eq!(value("PORT_PCS_DW1(A)"), 0x0000_0001);
        // COMP_DW1: only the two reference fields, everything else preserved.
        assert_eq!(value("PORT_COMP_DW1(A)"), 0xFFFF_FFFF & !PROC_DW1_MASK);
        assert_eq!(value("PORT_COMP_DW9(A)"), 0x93F8_7FE1);
        assert_eq!(value("PORT_COMP_DW10(A)"), 0x8AE8_71C5);
        assert_eq!(value("PORT_COMP_DW8(A)"), IREFGEN);
        // The read-modify-write sets COMP_INIT and leaves the unrelated bit
        // that was already there.
        assert_eq!(value("PORT_COMP_DW0(A)"), 1 | COMP_INIT);
        assert_eq!(value("PORT_CL_DW5(A)"), 0b11 | CL_POWER_DOWN_ENABLE);
    }

    #[test]
    fn a_phy_b_is_not_a_compensation_source_and_gets_no_irefgen() {
        // §8.3: PHY A is the comp source for PHY B, and on ADL-N only PHY A
        // gets `IREFGEN` (`phy_is_master`).  This is the difference that makes
        // the initialisation order matter, so it is asserted directly.
        let phy = regs::COMBO_PHY_B;
        let regs = mock_phy(phy, 1 << 26 | 1 << 24);
        let state = init_one(&regs, phy, false).unwrap();
        assert!(!state.comp_source);
        assert_eq!(state.procmon.name, "0.95V dot1");
        assert!(
            !regs
                .writes()
                .iter()
                .any(|(name, _)| *name == "PORT_COMP_DW8(B)"),
            "a sink must not be given IREFGEN"
        );
        // And the source does get it, so the test above is not vacuous.
        let phy = regs::COMBO_PHY_A;
        let regs = mock_phy(phy, 1 << 26 | 1 << 24);
        init_one(&regs, phy, true).unwrap();
        assert!(
            regs.writes()
                .iter()
                .any(|(name, _)| *name == "PORT_COMP_DW8(A)"),
            "a source needs IREFGEN"
        );
    }

    #[test]
    fn a_phy_that_firmware_already_initialised_is_left_alone() {
        // The whole point of the verification pass: a PHY the firmware set up
        // correctly must not be rewritten.  Build the exact state i915's
        // `icl_combo_phy_verify_state` accepts and assert that nothing at all
        // is written.
        let phy = regs::COMBO_PHY_A;
        let row = &PROCMON_ROWS[2]; // 0.95 V dot-1
        let regs = mock_phy(phy, 1 << 26 | 1 << 24);
        regs.set(phy.comp_dw0, COMP_INIT);
        regs.set(phy.phy_misc, 0);
        regs.set(
            phy.tx_dw8_ln0,
            ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_DIV2 | 0x1000,
        );
        regs.set(phy.pcs_dw1_ln0, RUN_DCC_ONCE | 0x3);
        regs.set(phy.comp_dw1, row.dw1);
        regs.set(phy.comp_dw9, row.dw9);
        regs.set(phy.comp_dw10, row.dw10);
        regs.set(phy.comp_dw8, IREFGEN);
        regs.set(phy.cl_dw5, CL_POWER_DOWN_ENABLE);

        let state = init_one(&regs, phy, true).unwrap();
        assert!(state.already_initialised);
        assert!(state.comp_init);
        assert_eq!(state.writes, 0);
        assert!(
            regs.writes().is_empty(),
            "a verified PHY must not be touched"
        );
        assert!(state.checks.iter().all(|check| check.passed));

        // Each check, failing one at a time, must send the sequence back to
        // programming -- and must name the value that disagreed.
        let failures: [(&str, fn(&MockRegisters, ComboPhyRegisters)); 8] = [
            (
                "ICL_PHY_MISC does not power down the IO compensation",
                |regs, phy| regs.set(phy.phy_misc, DE_IO_COMP_PWR_DOWN),
            ),
            (
                "lane 0 TX_DW8 selects the ODCC clock with divisor 2",
                |regs, phy| regs.set(phy.tx_dw8_ln0, 0),
            ),
            ("lane 0 PCS_DW1 runs DCC once", |regs, phy| {
                regs.set(phy.pcs_dw1_ln0, DCC_MODE_SELECT_MASK)
            }),
            ("COMP_DW1 holds the reference row's fields", |regs, phy| {
                regs.set(phy.comp_dw1, 0xFFFF_FFFF)
            }),
            ("COMP_DW9 holds the reference row's value", |regs, phy| {
                regs.set(phy.comp_dw9, 0)
            }),
            ("COMP_DW10 holds the reference row's value", |regs, phy| {
                regs.set(phy.comp_dw10, 0)
            }),
            (
                "COMP_DW8 has IREFGEN, which a compensation source needs",
                |regs, phy| regs.set(phy.comp_dw8, 0),
            ),
            ("CL_DW5 enables the PHY's clock power-down", |regs, phy| {
                regs.set(phy.cl_dw5, 0)
            }),
        ];
        for (what, break_it) in failures {
            let regs = mock_phy(phy, 1 << 26 | 1 << 24);
            regs.set(phy.comp_dw0, COMP_INIT);
            regs.set(phy.phy_misc, 0);
            regs.set(phy.tx_dw8_ln0, ODCC_CLK_SEL | ODCC_CLK_DIV_SEL_DIV2);
            regs.set(phy.pcs_dw1_ln0, RUN_DCC_ONCE);
            regs.set(phy.comp_dw1, row.dw1);
            regs.set(phy.comp_dw9, row.dw9);
            regs.set(phy.comp_dw10, row.dw10);
            regs.set(phy.comp_dw8, IREFGEN);
            regs.set(phy.cl_dw5, CL_POWER_DOWN_ENABLE);
            break_it(&regs, phy);

            let state = init_one(&regs, phy, true).unwrap();
            assert!(!state.already_initialised, "{what}");
            assert!(!regs.writes().is_empty(), "{what} must cause programming");
            // The failed check is named in the log, so a hardware bring-up
            // says which value disagreed rather than just "not verified".
            assert!(
                state
                    .checks
                    .iter()
                    .any(|check| check.what == what && !check.passed),
                "{what} was not reported; checks: {:?}",
                state.checks
            );
        }
    }

    #[test]
    fn a_phy_whose_comp_init_does_not_stick_says_what_that_means() {
        // §11 phase 1.2's documented failure: COMP_INIT is written and reads
        // back zero.  That is expected before PW_1, so it is recorded rather
        // than fatal -- and the log has to name the cause, because a reader who
        // does not know the order will chase the PHY instead of the well.
        let phy = regs::COMBO_PHY_A;
        let regs = mock_phy(phy, 0);
        regs.derive(phy.comp_dw0, |written| written & !COMP_INIT);

        let state = init_one(&regs, phy, true).unwrap();
        assert!(!state.comp_init);
        assert!(!state.initialised());
        let text = state.describe();
        assert!(text.contains("COMP_INIT does not read back set"), "{text}");
        assert!(text.contains("PW_1 is not on yet"), "{text}");
        // The sequence still ran to the end: stopping at the first sign of
        // trouble would leave the PHY half-programmed before PW_1 exists.
        assert!(
            regs.writes()
                .iter()
                .any(|(name, _)| *name == "PORT_CL_DW5(A)")
        );
    }

    #[test]
    fn a_phy_that_reads_all_zero_or_all_ones_is_reported_but_still_initialised() {
        // §12.2's presence test.  It is a warning here, not a decision: a
        // present-but-gated block reads zero too, so skipping the
        // initialisation on this evidence would turn a diagnostic into a
        // failure.  Writes to a genuinely absent block are dropped by the bus
        // and cost nothing.
        for value in [0u32, u32::MAX] {
            let phy = regs::COMBO_PHY_A;
            let regs = MockRegisters::new();
            regs.set(phy.comp_dw0, value);
            let state = init_one(&regs, phy, true).unwrap();
            assert!(!state.looks_present, "{value:#010x}");
            assert_eq!(state.comp_dw0_reads, [value, value]);
            let text = state.describe();
            assert!(text.contains("reads all-zero or all-ones twice"), "{text}");
            assert!(
                regs.writes()
                    .iter()
                    .any(|(name, _)| *name == "PORT_COMP_DW0(A)"),
                "the initialisation must still be attempted"
            );
        }
        // One read that is neither all-zero nor all-ones is enough to call a
        // PHY present, which is the strongest claim two reads can support: a
        // live value hidden on the first read only.
        let phy = regs::COMBO_PHY_A;
        let regs = MockRegisters::new();
        regs.set(phy.comp_dw0, COMP_INIT);
        let reads = core::cell::Cell::new(0u32);
        regs.on_read(phy.comp_dw0, move |stored| {
            reads.set(reads.get() + 1);
            if reads.get() == 1 { 0 } else { stored }
        });
        let state = init_one(&regs, phy, true).unwrap();
        assert_eq!(state.comp_dw0_reads[0], 0);
        assert!(state.looks_present);
    }

    #[test]
    fn both_phys_are_initialised_source_first() {
        // `init_all` walks the table in `regs`, and the property that the table
        // is ordered source-first is asserted here rather than assumed.
        let regs = MockRegisters::new();
        for phy in regs::COMBO_PHYS {
            regs.set(phy.comp_dw3, 1 << 26);
        }
        let states = init_all(&regs).unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].port, "A");
        assert_eq!(states[1].port, "B");
        assert!(states[0].comp_source);
        assert!(!states[1].comp_source);
        // A's writes all precede B's, which is the ordering the compensation
        // source rule requires.
        let names: alloc::vec::Vec<&str> = regs.writes().iter().map(|(name, _)| *name).collect();
        let last_a = names
            .iter()
            .rposition(|name| name.ends_with("(A)"))
            .expect("PHY A was programmed");
        let first_b = names
            .iter()
            .position(|name| name.ends_with("(B)"))
            .expect("PHY B was programmed");
        assert!(last_a < first_b, "{names:?}");
    }

    #[test]
    fn a_phy_register_outside_the_window_is_an_error_rather_than_a_zero() {
        let phy = regs::COMBO_PHY_A;
        let regs = MockRegisters::new();
        regs.hide(phy.comp_dw0);
        assert_eq!(
            init_one(&regs, phy, true).unwrap_err(),
            PhyError::Unreadable {
                register: "PORT_COMP_DW0(A)"
            }
        );

        let regs = MockRegisters::new();
        regs.refuse(phy.phy_misc);
        let error = init_one(&regs, phy, true).unwrap_err();
        assert_eq!(
            error,
            PhyError::WriteRefused {
                register: "ICL_PHY_MISC(A)"
            }
        );
        assert!(error.describe().contains("was not initialised"));
    }
}
