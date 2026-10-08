// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_display_power_well.c.
// Copyright © 2022 Intel Corporation. Full grant: ../LICENSE-MIT.
//! Gen9+ display DC-state field ownership and write/readback retries.

use crate::{Error, RegisterIo};

const DC_STATE_EN_UPTO_DC5: u32 = 1 << 0;
const DC_STATE_EN_UPTO_DC6: u32 = 1 << 1;
const DC_STATE_EN_DC9: u32 = 1 << 3;
const DC_STATE_EN_DC3CO: u32 = 1 << 30;

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
}
