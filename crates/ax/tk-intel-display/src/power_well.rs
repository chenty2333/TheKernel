// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_display_power_well.c.
// Copyright © 2022 Intel Corporation. Full grant: ../LICENSE-MIT.
//! Haswell-style power-well request, handshake, fuse, and requester behavior.
//!
//! The caller supplies the platform-selected register offsets and owns power
//! domain references. A timed-out hardware handshake is returned as a report,
//! matching i915's warning-and-continue behavior; MMIO transport failures are
//! still errors.

use crate::{Error, RegisterIo};

// upstream: intel_display_power_well.c pw_idx_to_pg()
pub const fn pw_idx_to_pg(
    display_version: u8,
    well_index: i16,
    skl_pw1_index: i16,
    icl_pw1_index: i16,
) -> i16 {
    let pw1_index = if display_version >= 11 {
        icl_pw1_index
    } else {
        skl_pw1_index
    };
    well_index - pw1_index + 1
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HswWellRegisters {
    pub bios: u32,
    pub driver: u32,
    pub kvmr: Option<u32>,
    pub debug: u32,
    pub fuse_status: u32,
    pub gen8_chicken_dcpr1: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HswWellSpec {
    pub name: &'static str,
    pub registers: HswWellRegisters,
    pub index: u8,
    pub pg: Option<u8>,
    pub timeout_ms: u16,
    pub has_fuses: bool,
    pub alderlake_pw1_wa: bool,
    pub irq_pipe_mask: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Requesters {
    pub bios: bool,
    pub driver: bool,
    pub kvmr: bool,
    pub debug: bool,
}

impl Requesters {
    pub const fn any(self) -> bool {
        self.bios || self.driver || self.kvmr || self.debug
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WellEnableReport {
    pub state_set: bool,
    pub pg0_distributed: Option<bool>,
    pub pg_distributed: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WellDisableReport {
    pub requesters: Requesters,
    pub state_cleared: bool,
}

/// Register access and wait callbacks for `hsw_power_well_*`.
pub trait HswPowerWellIo: RegisterIo {
    fn wait_set(&self, register: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error>;
    fn wait_clear(&self, register: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error>;
    fn post_enable(&self, irq_pipe_mask: u8) -> Result<(), Error>;
    fn pre_disable(&self, irq_pipe_mask: u8) -> Result<(), Error>;
}

const fn request_mask(index: u8) -> u32 {
    2 << (index as u32 * 2)
}

const fn state_mask(index: u8) -> u32 {
    1 << (index as u32 * 2)
}

const fn pg_mask(pg: u8) -> u32 {
    1 << (27 - pg as u32)
}

fn rmw(io: &impl HswPowerWellIo, register: u32, clear: u32, set: u32) -> Result<u32, Error> {
    let value = io.read32(register)?;
    let next = (value & !clear) | set;
    io.write32(register, next)?;
    Ok(next)
}

// upstream: intel_display_power_well.c hsw_power_well_requesters()
pub fn hsw_power_well_requesters(
    io: &impl HswPowerWellIo,
    registers: HswWellRegisters,
    index: u8,
) -> Result<Requesters, Error> {
    let mask = request_mask(index);
    let bios = io.read32(registers.bios)? & mask != 0;
    let driver = io.read32(registers.driver)? & mask != 0;
    let kvmr = match registers.kvmr {
        Some(register) => io.read32(register)? & mask != 0,
        None => false,
    };
    let debug = io.read32(registers.debug)? & mask != 0;
    Ok(Requesters {
        bios,
        driver,
        kvmr,
        debug,
    })
}

/// Transfer a BIOS-owned request to the driver without dropping the well.
// upstream: intel_display_power_well.c hsw_power_well_sync_hw()
pub fn hsw_power_well_sync_hw(
    io: &impl HswPowerWellIo,
    registers: HswWellRegisters,
    index: u8,
) -> Result<bool, Error> {
    let mask = request_mask(index);
    let bios = io.read32(registers.bios)?;
    if bios & mask == 0 {
        return Ok(false);
    }

    let driver = io.read32(registers.driver)?;
    if driver & mask == 0 {
        io.write32(registers.driver, driver | mask)?;
    }
    io.write32(registers.bios, bios & !mask)?;
    Ok(true)
}

// upstream: intel_display_power_well.c gen9_wait_for_power_well_fuses()
pub fn gen9_wait_for_power_well_fuses(
    io: &impl HswPowerWellIo,
    registers: HswWellRegisters,
    pg: u8,
) -> Result<bool, Error> {
    io.wait_set(registers.fuse_status, pg_mask(pg), 1)
}

// upstream: intel_display_power_well.c hsw_wait_for_power_well_enable()
pub fn hsw_wait_for_power_well_enable(
    io: &impl HswPowerWellIo,
    spec: HswWellSpec,
) -> Result<bool, Error> {
    io.wait_set(
        spec.registers.driver,
        state_mask(spec.index),
        spec.timeout_ms.max(1),
    )
}

// upstream: intel_display_power_well.c hsw_wait_for_power_well_disable()
pub fn hsw_wait_for_power_well_disable(
    io: &impl HswPowerWellIo,
    spec: HswWellSpec,
) -> Result<WellDisableReport, Error> {
    let mut requesters = hsw_power_well_requesters(io, spec.registers, spec.index)?;
    let timeout_ms = if requesters.any() { 0 } else { 1 };
    let state_cleared = io.wait_clear(spec.registers.driver, state_mask(spec.index), timeout_ms)?;
    if !state_cleared && !requesters.any() {
        requesters = hsw_power_well_requesters(io, spec.registers, spec.index)?;
    }
    Ok(WellDisableReport {
        requesters,
        state_cleared,
    })
}

// upstream: intel_display_power_well.c hsw_power_well_enable()
pub fn hsw_power_well_enable(
    io: &impl HswPowerWellIo,
    spec: HswWellSpec,
) -> Result<WellEnableReport, Error> {
    let request = request_mask(spec.index);
    let mut pg0_distributed = None;
    if spec.has_fuses {
        if spec.alderlake_pw1_wa && spec.pg == Some(1) {
            rmw(io, spec.registers.gen8_chicken_dcpr1, 0, 1 << 15)?;
        }
        if spec.pg == Some(1) {
            pg0_distributed = Some(gen9_wait_for_power_well_fuses(io, spec.registers, 0)?);
        }
    }
    rmw(io, spec.registers.driver, 0, request)?;
    let state_set = hsw_wait_for_power_well_enable(io, spec)?;
    let pg_distributed = if spec.has_fuses {
        spec.pg
            .map(|pg| gen9_wait_for_power_well_fuses(io, spec.registers, pg))
            .transpose()?
    } else {
        None
    };
    io.post_enable(spec.irq_pipe_mask)?;
    Ok(WellEnableReport {
        state_set,
        pg0_distributed,
        pg_distributed,
    })
}

// upstream: intel_display_power_well.c hsw_power_well_disable()
pub fn hsw_power_well_disable(
    io: &impl HswPowerWellIo,
    spec: HswWellSpec,
) -> Result<WellDisableReport, Error> {
    io.pre_disable(spec.irq_pipe_mask)?;
    rmw(io, spec.registers.driver, request_mask(spec.index), 0)?;
    hsw_wait_for_power_well_disable(io, spec)
}

// upstream: intel_display_power_well.c hsw_power_well_enabled()
pub fn hsw_power_well_enabled(
    io: &impl HswPowerWellIo,
    spec: HswWellSpec,
    check_bios_request: bool,
) -> Result<bool, Error> {
    let mask = request_mask(spec.index) | state_mask(spec.index);
    let mut value = io.read32(spec.registers.driver)?;
    if check_bios_request {
        value |= io.read32(spec.registers.bios)?;
    }
    Ok(value & mask == mask)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct Fake {
        registers: RefCell<Vec<(u32, u32)>>,
        reads: RefCell<Vec<u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
        waits: RefCell<Vec<(u32, u32, u16, bool)>>,
    }

    impl Fake {
        fn value(&self, register: u32) -> u32 {
            self.registers
                .borrow()
                .iter()
                .find(|(address, _)| *address == register)
                .map(|(_, value)| *value)
                .unwrap_or(0)
        }

        fn set(&self, register: u32, value: u32) {
            let mut registers = self.registers.borrow_mut();
            if let Some((_, current)) = registers
                .iter_mut()
                .find(|(address, _)| *address == register)
            {
                *current = value;
            } else {
                registers.push((register, value));
            }
        }
    }

    impl RegisterIo for Fake {
        fn read32(&self, offset: u32) -> Result<u32, Error> {
            self.reads.borrow_mut().push(offset);
            Ok(self.value(offset))
        }

        fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            let value = if offset == 2 {
                if value & request_mask(0) != 0 {
                    value | state_mask(0)
                } else {
                    value & !state_mask(0)
                }
            } else {
                value
            };
            self.set(offset, value);
            Ok(())
        }
    }

    impl HswPowerWellIo for Fake {
        fn wait_set(&self, register: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error> {
            let set = self.value(register) & mask == mask;
            self.waits
                .borrow_mut()
                .push((register, mask, timeout_ms, set));
            Ok(set)
        }

        fn wait_clear(&self, register: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error> {
            let clear = self.value(register) & mask == 0;
            self.waits
                .borrow_mut()
                .push((register, mask, timeout_ms, clear));
            Ok(clear)
        }

        fn post_enable(&self, _irq_pipe_mask: u8) -> Result<(), Error> {
            Ok(())
        }

        fn pre_disable(&self, _irq_pipe_mask: u8) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn adlp_pw1_applies_wa_then_fuse_request_and_state_handshakes() {
        let io = Fake::default();
        io.set(4, pg_mask(0) | pg_mask(1));
        let spec = HswWellSpec {
            name: "PW_1",
            registers: HswWellRegisters {
                bios: 1,
                driver: 2,
                kvmr: None,
                debug: 3,
                fuse_status: 4,
                gen8_chicken_dcpr1: 5,
            },
            index: 0,
            pg: Some(1),
            timeout_ms: 1,
            has_fuses: true,
            alderlake_pw1_wa: true,
            irq_pipe_mask: 0,
        };
        let report = hsw_power_well_enable(&io, spec).unwrap();
        assert!(report.state_set);
        assert_eq!(report.pg0_distributed, Some(true));
        assert_eq!(report.pg_distributed, Some(true));
        assert_eq!(
            io.writes.borrow().as_slice(),
            &[(5, 1 << 15), (2, request_mask(0))]
        );
        assert_eq!(
            io.waits.borrow().as_slice(),
            &[
                (4, pg_mask(0), 1, true),
                (2, state_mask(0), 1, true),
                (4, pg_mask(1), 1, true),
            ]
        );
    }

    #[test]
    fn requester_reads_follow_bios_driver_kvmr_debug_order() {
        let io = Fake::default();
        let registers = HswWellRegisters {
            bios: 10,
            driver: 11,
            kvmr: Some(12),
            debug: 13,
            fuse_status: 14,
            gen8_chicken_dcpr1: 15,
        };
        hsw_power_well_requesters(&io, registers, 0).unwrap();
        assert_eq!(io.reads.borrow().as_slice(), &[10, 11, 12, 13]);
    }

    #[test]
    fn sync_transfers_only_a_bios_request_to_driver_then_clears_bios() {
        let io = Fake::default();
        let registers = HswWellRegisters {
            bios: 10,
            driver: 11,
            kvmr: None,
            debug: 13,
            fuse_status: 14,
            gen8_chicken_dcpr1: 15,
        };
        io.set(10, request_mask(0) | 0x80);
        io.set(11, 0x40);
        assert!(hsw_power_well_sync_hw(&io, registers, 0).unwrap());
        assert_eq!(io.reads.borrow().as_slice(), &[10, 11]);
        assert_eq!(
            io.writes.borrow().as_slice(),
            &[(11, 0x40 | request_mask(0)), (10, 0x80)]
        );
    }
}
