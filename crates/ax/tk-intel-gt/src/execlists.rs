// SPDX-License-Identifier: MIT
// Translated from Linux 7.2.3 drivers/gpu/drm/i915/gt/intel_execlists_submission.c
// Copyright © 2014 Intel Corporation. Full MIT grant: LICENSE-MIT.
//! Gen12 Execlists submission-queue MMIO operations.

use crate::{Error, GtIo};

pub const EL_CTRL_LOAD: u32 = 1;
pub const GEN12_NUM_PORTS: usize = 2;

/// Write one descriptor into an Execlists submit queue port.
/// upstream: intel_execlists_submission.c write_desc()
pub fn write_desc(
    io: &impl GtIo,
    submit_reg: u32,
    _ctrl_reg: Option<u32>,
    descriptor: u64,
    port: u32,
) -> Result<(), Error> {
    if _ctrl_reg.is_some() {
        let offset = port.checked_mul(2).ok_or(Error::Refused)?;
        io.write(
            submit_reg.checked_add(offset).ok_or(Error::Refused)?,
            descriptor as u32,
        )?;
        io.write(
            submit_reg
                .checked_add(offset)
                .and_then(|value| value.checked_add(1))
                .ok_or(Error::Refused)?,
            (descriptor >> 32) as u32,
        )?;
    } else {
        // Legacy ELSP expects descriptor high dword before low dword.
        io.write(submit_reg, (descriptor >> 32) as u32)?;
        io.write(submit_reg, descriptor as u32)?;
    }
    Ok(())
}

/// Program every ELSQ entry (including zero for an empty port), then load it.
/// The reverse-order loop and identical entry count are required because ELSQ
/// is not cleared by hardware after a submit.
/// upstream: intel_execlists_submission.c execlists_submit_ports()
pub fn execlists_submit_ports(
    io: &impl GtIo,
    submit_reg: u32,
    ctrl_reg: u32,
    descriptors: [u64; GEN12_NUM_PORTS],
) -> Result<(), Error> {
    for port in (0..GEN12_NUM_PORTS).rev() {
        write_desc(
            io,
            submit_reg,
            Some(ctrl_reg),
            descriptors[port],
            port as u32,
        )?;
    }
    io.write(ctrl_reg, EL_CTRL_LOAD)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct Io(RefCell<Vec<(u32, u32)>>);

    impl GtIo for Io {
        fn read(&self, _: u32) -> Result<u32, Error> {
            Err(Error::Refused)
        }

        fn write(&self, register: u32, value: u32) -> Result<(), Error> {
            // This mock is only used by the single-threaded test below.
            self.0.borrow_mut().push((register, value));
            Ok(())
        }

        fn now_us(&self) -> u64 {
            0
        }

        fn delay_us(&self, _: u32) {}
    }

    #[test]
    fn gen12_programs_both_elsq_ports_in_reverse_order_then_loads() {
        let io = Io::default();
        execlists_submit_ports(&io, 0x2510, 0x2550, [0x1122_3344_5566_7788, 0]).unwrap();
        assert_eq!(
            *io.0.borrow(),
            [
                (0x2512, 0),
                (0x2513, 0),
                (0x2510, 0x5566_7788),
                (0x2511, 0x1122_3344),
                (0x2550, EL_CTRL_LOAD),
            ]
        );
    }
}
