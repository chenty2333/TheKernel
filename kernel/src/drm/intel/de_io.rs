//! Checked display-engine MMIO helpers, following i915's `intel_de_*` shape.
//!
//! This module is a thin policy boundary over the checked
//! [`super::regs::RegisterWindow`]
//! and [`Registers`] interface.  It does not derive register offsets or
//! reinterpret the register table: failures carry the original [`Register`]
//! value so callers retain its name, width, access class, meaning, and quirk
//! metadata when diagnosing an incomplete mapping or refused access.
//!
//! A write is intentionally a direct write, not an implicit read/modify/write.
//! Some display registers are write-one-to-clear (W1C), and the current
//! register metadata classifies access but does not describe per-bit behavior.
//! Callers must use [`DeIo::rmw`] only with an explicit [`RmwSafeRegister`]
//! assertion for an ordinary read/write register with no W1C or other
//! read-sensitive bits.  W1C registers must use [`DeIo::write`] with the exact
//! acknowledgement mask.

use super::regs::{Register, Registers, Width, poll_attempts};

/// Error returned by a checked display-engine MMIO operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeIoError {
    /// The register is not a 32-bit DE register.
    WrongWidth { register: Register },
    /// The register could not be read through the current aperture.
    Unreadable { register: Register },
    /// The register is not declared writable by the kernel.
    NotWritable { register: Register },
    /// The register table or mapped aperture refused the write.
    WriteRefused { register: Register },
    /// The masked wait exhausted its bounded read budget.
    TimedOut {
        register: Register,
        mask: u32,
        expected: u32,
        last_value: u32,
        attempts: u32,
    },
    /// The expected bits include bits outside `mask`, so the requested wait
    /// cannot have a matching result.
    InvalidWaitValue {
        register: Register,
        mask: u32,
        expected: u32,
    },
}

/// Explicit attestation that a register's hardware semantics permit RMW.
///
/// The shared register table currently records width and whole-register access
/// but does not encode W1C/read-sensitive bit behavior.  Consequently this
/// token must be constructed deliberately at an audited call site; it is not
/// inferred from `Register::is_writable()`.  Do not construct it for interrupt
/// identity/status registers, command registers, or any register whose write
/// value is an acknowledgement mask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RmwSafeRegister(Register);

impl RmwSafeRegister {
    /// Assert that this register is a plain read/write control register, not a
    /// W1C or otherwise read-sensitive register.
    pub(crate) const fn new(register: Register) -> Self {
        Self(register)
    }

    /// Return the exact metadata-bearing register wrapped by this assertion.
    const fn register(self) -> Register {
        self.0
    }
}

/// A checked DE register view over the existing MMIO access abstraction.
pub(crate) struct DeIo<'a, R: Registers> {
    registers: &'a R,
}

impl<'a, R: Registers> DeIo<'a, R> {
    /// Wrap an existing register backend without changing its register table
    /// or mapping lifetime.
    pub(crate) const fn new(registers: &'a R) -> Self {
        Self { registers }
    }

    /// Read one 32-bit display-engine register.
    pub(crate) fn read(&self, register: Register) -> Result<u32, DeIoError> {
        self.ensure_32bit(register)?;
        self.registers
            .read(register)
            .ok_or(DeIoError::Unreadable { register })
    }

    /// Write an exact value to one 32-bit display-engine register.
    ///
    /// This deliberately does not read first: callers that acknowledge a W1C
    /// status register must write only the acknowledgement bits, as on i915.
    pub(crate) fn write(&self, register: Register, value: u32) -> Result<(), DeIoError> {
        self.ensure_32bit(register)?;
        if !register.is_writable() {
            return Err(DeIoError::NotWritable { register });
        }
        if self.registers.write(register, value) {
            Ok(())
        } else {
            Err(DeIoError::WriteRefused { register })
        }
    }

    /// Perform i915-style `(old & !clear) | set` on an explicitly attested
    /// plain read/write register.
    ///
    /// There is no RMW fallback for a failed read or refused write, and no
    /// posted-write readback is hidden here.  Sequences that require a
    /// posting read must perform it explicitly against the register required
    /// by the hardware programming sequence.
    pub(crate) fn rmw(
        &self,
        register: RmwSafeRegister,
        clear: u32,
        set: u32,
    ) -> Result<u32, DeIoError> {
        let register = register.register();
        self.ensure_32bit(register)?;
        if !register.is_writable() {
            return Err(DeIoError::NotWritable { register });
        }
        let old = self.read(register)?;
        self.write(register, (old & !clear) | set)?;
        Ok(old)
    }

    /// Wait until `(register & mask) == expected`, using a bounded number of
    /// reads rather than a wall-clock source unavailable to host tests.
    ///
    /// The budget follows the existing register helper's one-read-per-
    /// microsecond estimate and clamps zero to one immediate observation, as
    /// `intel_de_wait_for_*` does for an already-satisfied condition.
    pub(crate) fn wait_for_register(
        &self,
        register: Register,
        mask: u32,
        expected: u32,
        timeout_us: u32,
    ) -> Result<u32, DeIoError> {
        self.ensure_32bit(register)?;
        if expected & !mask != 0 {
            return Err(DeIoError::InvalidWaitValue {
                register,
                mask,
                expected,
            });
        }

        let attempts = poll_attempts(timeout_us);
        let mut last_value = 0;
        for _ in 0..attempts {
            last_value = self.read(register)?;
            if last_value & mask == expected {
                return Ok(last_value);
            }
            core::hint::spin_loop();
        }

        Err(DeIoError::TimedOut {
            register,
            mask,
            expected,
            last_value,
            attempts,
        })
    }

    fn ensure_32bit(&self, register: Register) -> Result<(), DeIoError> {
        if register.width() == Width::Bits32 {
            Ok(())
        } else {
            Err(DeIoError::WrongWidth { register })
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::cell::Cell;

    use super::*;
    use crate::drm::intel::regs::{self, Meaning, mock::MockRegisters};

    fn test_rw(name: &'static str, offset: u32) -> Register {
        Register::read_write(name, offset, Meaning::BringUp, None)
    }

    #[test]
    fn direct_write_does_not_read_modify_or_merge_w1c_acknowledgement_bits() {
        // DEIIR is a status/acknowledgement register in the interrupt table:
        // preserve the exact write value instead of merging its old value.
        let registers = MockRegisters::new();
        registers.set(regs::interrupt::DEIIR, 0b1100);
        let io = DeIo::new(&registers);

        io.write(regs::interrupt::DEIIR, 0b0001).unwrap();

        assert_eq!(registers.writes(), vec![("DEIIR", 0b0001)]);
        assert_eq!(registers.read(regs::interrupt::DEIIR), Some(0b0001));
    }

    #[test]
    fn rmw_requires_an_explicit_plain_register_attestation_and_preserves_other_bits() {
        let registers = MockRegisters::new();
        let control = regs::DC_STATE_EN;
        registers.set(control, 0b1010);
        let io = DeIo::new(&registers);

        let old = io
            .rmw(RmwSafeRegister::new(control), 0b0010, 0b0100)
            .unwrap();

        assert_eq!(old, 0b1010);
        assert_eq!(registers.writes(), vec![("DC_STATE_EN", 0b1100)]);
    }

    #[test]
    fn read_and_write_keep_register_metadata_on_access_errors() {
        let registers = MockRegisters::new();
        let hidden = test_rw("HIDDEN", 0x1000);
        let refused = test_rw("REFUSED", 0x1004);
        registers.hide(hidden);
        registers.refuse(refused);
        let io = DeIo::new(&registers);

        assert_eq!(
            io.read(hidden),
            Err(DeIoError::Unreadable { register: hidden })
        );
        assert_eq!(
            io.write(refused, 7),
            Err(DeIoError::WriteRefused { register: refused })
        );
        assert_eq!(
            io.write(regs::NAMED[3], 7),
            Err(DeIoError::NotWritable {
                register: regs::NAMED[3]
            })
        );
        assert_eq!(
            io.read(regs::NAMED[4]),
            Err(DeIoError::WrongWidth {
                register: regs::NAMED[4]
            })
        );
    }

    #[test]
    fn bounded_wait_returns_matching_value_and_reports_the_last_value_on_timeout() {
        let registers = MockRegisters::new();
        let status = test_rw("STATUS", 0x1000);
        let reads = Cell::new(0);
        registers.on_read(status, move |value| {
            let reads = reads.get() + 1;
            reads.set(reads);
            if reads == 3 { value | 0x8 } else { value }
        });
        let io = DeIo::new(&registers);

        assert_eq!(io.wait_for_register(status, 0x8, 0x8, 4), Ok(0x8));
        assert_eq!(
            io.wait_for_register(status, 0x10, 0x10, 2),
            Err(DeIoError::TimedOut {
                register: status,
                mask: 0x10,
                expected: 0x10,
                last_value: 0x8,
                attempts: 2,
            })
        );
        assert_eq!(
            io.wait_for_register(status, 0x1, 0x2, 2),
            Err(DeIoError::InvalidWaitValue {
                register: status,
                mask: 0x1,
                expected: 0x2,
            })
        );
    }
}
