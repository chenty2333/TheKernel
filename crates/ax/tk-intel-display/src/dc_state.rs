// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_display_power_well.c.
// Copyright © 2022 Intel Corporation. Full grant: ../LICENSE-MIT.
//! Gen9+ display DC-state field ownership and write/readback retries.

use crate::{Error, RegisterIo};

const DC_STATE_EN_UPTO_DC5: u32 = 1 << 0;
pub const DC_STATE_EN_UPTO_DC6: u32 = 1 << 1;
const DC_STATE_EN_DC9: u32 = 1 << 3;
const DC_STATE_EN_DC3CO: u32 = 1 << 30;

pub const DC_STATE_DISABLE: u32 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DcStateCaps {
    pub display_version: u8,
    pub has_display: bool,
    pub dg2: bool,
    pub dg1: bool,
    pub geminilake: bool,
    pub broxton: bool,
    pub disable_power_well: bool,
}

/// Calculate the i915 DC states that this display may request.
// upstream: intel_display_power.c get_allowed_dc_mask()
pub const fn get_allowed_dc_mask(caps: DcStateCaps, enable_dc: i32) -> u32 {
    if !caps.has_display {
        return 0;
    }
    let mut max_dc = if caps.display_version >= 20 {
        2
    } else if caps.dg2 {
        1
    } else if caps.dg1 {
        3
    } else if caps.display_version >= 12 {
        4
    } else if caps.geminilake || caps.broxton {
        1
    } else if caps.display_version >= 9 {
        2
    } else {
        0
    };
    let mut mask = if caps.geminilake || caps.broxton || caps.display_version >= 11 {
        DC_STATE_EN_DC9
    } else {
        0
    };
    if !caps.disable_power_well {
        max_dc = 0;
    }
    let requested = if enable_dc >= 0 && enable_dc <= max_dc {
        enable_dc
    } else {
        max_dc
    };
    match requested {
        4 => mask |= DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC6,
        3 => mask |= DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC5,
        2 => mask |= DC_STATE_EN_UPTO_DC6,
        1 => mask |= DC_STATE_EN_UPTO_DC5,
        _ => {}
    }
    mask
}

/// Reduce an unsupported target through the source's DC6→DC5→DC3CO→disable order.
// upstream: intel_display_power.c sanitize_target_dc_state()
pub const fn sanitize_target_dc_state(mut target: u32, allowed_dc_mask: u32) -> u32 {
    let states = [
        DC_STATE_EN_UPTO_DC6,
        DC_STATE_EN_UPTO_DC5,
        DC_STATE_EN_DC3CO,
        DC_STATE_DISABLE,
    ];
    let mut index = 0;
    while index < states.len() - 1 {
        if target == states[index] {
            if allowed_dc_mask & target != 0 {
                break;
            }
            target = states[index + 1];
        }
        index += 1;
    }
    target
}

pub trait DcOffPowerWellIo {
    fn dc_off_well_is_enabled(&self) -> Result<bool, Error>;
    fn enable_dc_off_well(&mut self) -> Result<(), Error>;
    fn disable_dc_off_well(&mut self) -> Result<(), Error>;
}

/// Change the target by cycling the DC-off well when needed to notify DMC.
// upstream: intel_display_power.c intel_display_power_set_target_dc_state()
pub fn set_target_dc_state(
    target_dc_state: &mut u32,
    allowed_dc_mask: u32,
    requested: u32,
    io: &mut impl DcOffPowerWellIo,
) -> Result<u32, Error> {
    let state = sanitize_target_dc_state(requested, allowed_dc_mask);
    if state == *target_dc_state {
        return Ok(state);
    }
    let dc_off_enabled = io.dc_off_well_is_enabled()?;
    if !dc_off_enabled {
        io.enable_dc_off_well()?;
        let previous = *target_dc_state;
        *target_dc_state = state;
        if let Err(error) = io.disable_dc_off_well() {
            *target_dc_state = previous;
            return Err(error);
        }
    } else {
        *target_dc_state = state;
    }
    Ok(state)
}

/// Report effective state, where an enabled DC-off well means DC is disabled.
// upstream: intel_display_power.c intel_display_power_get_current_dc_state()
pub fn get_current_dc_state(dc_off_well_enabled: bool, target_dc_state: u32) -> u32 {
    if dc_off_well_enabled {
        DC_STATE_DISABLE
    } else {
        target_dc_state
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DcStateWrite {
    pub before: Option<u32>,
    pub target: u32,
    /// Last value read by the source loop. On exhaustion this intentionally
    /// predates the final retry write, matching i915's diagnostic value.
    pub readback: u32,
    pub rewrites: u8,
    pub stable_reads: u8,
}

/// Return the register bits software controls for a platform family.
// upstream: intel_display_power_well.c gen9_dc_mask()
pub const fn gen9_dc_mask(display_version: u8, legacy_dc9: bool) -> u32 {
    let mut mask = DC_STATE_EN_UPTO_DC5;
    if display_version >= 12 {
        mask |= DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC6 | DC_STATE_EN_DC9;
    } else if display_version == 11 {
        mask |= DC_STATE_EN_UPTO_DC6 | DC_STATE_EN_DC9;
    } else if legacy_dc9 {
        mask |= DC_STATE_EN_DC9;
    } else {
        mask |= DC_STATE_EN_UPTO_DC6;
    }
    mask
}

/// Write a complete `DC_STATE_EN` value, retrying mismatches exactly as i915.
// upstream: intel_display_power_well.c gen9_write_dc_state()
pub fn gen9_write_dc_state(
    io: &impl RegisterIo,
    register: u32,
    state: u32,
) -> Result<DcStateWrite, Error> {
    io.write32(register, state)?;
    let mut rewrites = 0u8;
    let mut stable_reads = 0u8;
    let mut readback;
    loop {
        readback = io.read32(register)?;
        if readback != state {
            io.write32(register, state)?;
            rewrites += 1;
            stable_reads = 0;
        } else {
            if stable_reads > 5 {
                break;
            }
            stable_reads += 1;
        }
        if rewrites >= 100 {
            break;
        }
    }
    Ok(DcStateWrite {
        before: None,
        target: state,
        readback,
        rewrites,
        stable_reads,
    })
}

/// Change only the software-owned DC-state field, preserving all other bits.
// upstream: intel_display_power_well.c gen9_set_dc_state()
pub fn gen9_set_dc_state_field(
    io: &impl RegisterIo,
    register: u32,
    display_version: u8,
    legacy_dc9: bool,
    state: u32,
) -> Result<DcStateWrite, Error> {
    let before = io.read32(register)?;
    let mask = gen9_dc_mask(display_version, legacy_dc9);
    let target = (before & !mask) | (state & mask);
    let mut report = gen9_write_dc_state(io, register, target)?;
    report.before = Some(before);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

    use super::*;

    struct Fake {
        value: Cell<u32>,
        ignore_writes: Cell<bool>,
        writes: RefCell<Vec<u32>>,
        reads: Cell<u32>,
    }

    impl Fake {
        fn new(value: u32) -> Self {
            Self {
                value: Cell::new(value),
                ignore_writes: Cell::new(false),
                writes: RefCell::new(Vec::new()),
                reads: Cell::new(0),
            }
        }
    }

    impl RegisterIo for Fake {
        fn read32(&self, _offset: u32) -> Result<u32, Error> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.value.get())
        }

        fn write32(&self, _offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push(value);
            if !self.ignore_writes.get() {
                self.value.set(value);
            }
            Ok(())
        }
    }

    #[test]
    fn display13_mask_and_reserved_bits_follow_i915_field_ownership() {
        let mask = gen9_dc_mask(13, false);
        assert_eq!(
            mask,
            DC_STATE_EN_UPTO_DC5 | DC_STATE_EN_UPTO_DC6 | DC_STATE_EN_DC9 | DC_STATE_EN_DC3CO
        );
        let io = Fake::new(0x8000_0000 | mask | (1 << 9));
        let result = gen9_set_dc_state_field(&io, 4, 13, false, 0).unwrap();
        assert_eq!(result.target, 0x8000_0000 | (1 << 9));
        assert_eq!(result.readback, result.target);
        assert_eq!(result.rewrites, 0);
        assert_eq!(result.stable_reads, 6);
        assert_eq!(io.writes.borrow().as_slice(), &[result.target]);
    }

    #[test]
    fn ignored_dc_state_writes_stop_after_i915s_hundred_rewrites() {
        let io = Fake::new(0x55);
        io.ignore_writes.set(true);
        let result = gen9_write_dc_state(&io, 4, 0xaa).unwrap();
        assert_eq!(result.rewrites, 100);
        assert_eq!(result.readback, 0x55);
        assert_eq!(io.writes.borrow().len(), 101);
        assert_eq!(io.reads.get(), 100);
    }

    #[test]
    fn allowed_dc_mask_and_target_sanitization_follow_i915_priority() {
        let caps = DcStateCaps {
            display_version: 13,
            has_display: true,
            dg2: false,
            dg1: false,
            geminilake: false,
            broxton: false,
            disable_power_well: true,
        };
        let allowed = get_allowed_dc_mask(caps, -1);
        assert_eq!(
            allowed,
            DC_STATE_EN_DC9 | DC_STATE_EN_DC3CO | DC_STATE_EN_UPTO_DC6
        );
        assert_eq!(
            sanitize_target_dc_state(DC_STATE_EN_UPTO_DC6, DC_STATE_EN_UPTO_DC5),
            DC_STATE_EN_UPTO_DC5
        );
        assert_eq!(
            sanitize_target_dc_state(DC_STATE_EN_UPTO_DC6, 0),
            DC_STATE_DISABLE
        );
        assert_eq!(
            sanitize_target_dc_state(DC_STATE_EN_UPTO_DC6, allowed),
            DC_STATE_EN_UPTO_DC6
        );
        assert_eq!(
            get_current_dc_state(true, DC_STATE_EN_UPTO_DC6),
            DC_STATE_DISABLE
        );
        assert_eq!(
            get_current_dc_state(false, DC_STATE_EN_UPTO_DC6),
            DC_STATE_EN_UPTO_DC6
        );
        let disabled = DcStateCaps {
            disable_power_well: false,
            ..caps
        };
        assert_eq!(get_allowed_dc_mask(disabled, -1), DC_STATE_EN_DC9);
    }

    #[test]
    fn target_dc_state_cycles_dc_off_well_only_when_it_is_currently_disabled() {
        #[derive(Default)]
        struct DcWell {
            enabled: bool,
            calls: u8,
        }
        impl DcOffPowerWellIo for DcWell {
            fn dc_off_well_is_enabled(&self) -> Result<bool, Error> {
                Ok(self.enabled)
            }
            fn enable_dc_off_well(&mut self) -> Result<(), Error> {
                self.enabled = true;
                self.calls += 1;
                Ok(())
            }
            fn disable_dc_off_well(&mut self) -> Result<(), Error> {
                self.enabled = false;
                self.calls += 1;
                Ok(())
            }
        }
        let allowed = DC_STATE_EN_DC9 | DC_STATE_EN_UPTO_DC6;
        let mut target = DC_STATE_DISABLE;
        let mut well = DcWell::default();
        assert_eq!(
            set_target_dc_state(&mut target, allowed, DC_STATE_EN_UPTO_DC6, &mut well).unwrap(),
            DC_STATE_EN_UPTO_DC6
        );
        assert_eq!(well.calls, 2);
        well.enabled = true;
        assert_eq!(
            set_target_dc_state(&mut target, allowed, DC_STATE_EN_DC9, &mut well).unwrap(),
            DC_STATE_EN_DC9
        );
        assert_eq!(well.calls, 2);
    }
}
