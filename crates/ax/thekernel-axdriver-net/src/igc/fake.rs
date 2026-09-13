//! Synthetic hardware for the host tests.
//!
//! There is no i225/i226 device model in QEMU, so no host or guest run can put
//! this driver in front of the real thing.  What *can* be tested is everything
//! between the driver's logic and the bus: given a register file that answers
//! the way the part is documented to answer, does the driver reach the right
//! conclusion, refuse the right operation, and terminate?  That is what these
//! types are for.
//!
//! The synthetic device is deliberately not a simulator: it models *only* the
//! behaviours the driver relies on, and each modelled behaviour is a cited
//! fact with a comment saying where it came from.  A behaviour that is not
//! modelled is not silently benign either -- a read of a register the test did
//! not seed returns [`FakeBus::unseeded_value`], which the tests set to
//! `0xffff_ffff` when they mean "nothing decoded this address".
//!
//! The types are `#[cfg(test)]` only: none of this exists in a kernel build.

use alloc::{
    collections::BTreeMap,
    vec::Vec,
};

use super::{
    IgcBus, IgcHal,
    regs::{self, Register},
};

/// A test HAL that records the waits the driver asked for instead of waiting.
///
/// The record is thread-local, so tests cannot see each other's waits even
/// when the test harness runs them in parallel.
pub struct FakeHal;

std::thread_local! {
    static DELAYS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl FakeHal {
    /// Forget every wait recorded on this thread.
    pub fn reset() {
        DELAYS.with(|delays| delays.borrow_mut().clear());
    }

    /// Every wait, in microseconds and in order, recorded on this thread.
    pub fn delays() -> Vec<u32> {
        DELAYS.with(|delays| delays.borrow().clone())
    }

    /// How many waits were recorded on this thread.
    pub fn delay_count() -> usize {
        DELAYS.with(|delays| delays.borrow().len())
    }

    /// The total time waited on this thread, in microseconds.
    pub fn total_us() -> u64 {
        DELAYS.with(|delays| delays.borrow().iter().map(|us| u64::from(*us)).sum())
    }
}

impl IgcHal for FakeHal {
    fn busy_wait_us(micros: u32) {
        DELAYS.with(|delays| delays.borrow_mut().push(micros));
    }
}

/// A register file standing in for the device aperture.
pub struct FakeBus {
    words: BTreeMap<u32, u32>,
    /// What an unseeded register reads as.  `0xffff_ffff` is what an address
    /// no device decodes returns on x86; tests use it to mean that.
    unseeded: u32,
    /// Every write the bus was asked to perform, in order, as
    /// `(offset, value, accepted)`.
    ///
    /// The identify-only phase must produce none of these, which is a property
    /// a test can assert instead of trusting the register table alone.
    writes: Vec<(u32, u32, bool)>,
    /// Every wait, in order.
    delays: Vec<u32>,
}

impl FakeBus {
    /// A register file whose unseeded registers read as zero.
    pub fn new() -> Self {
        Self {
            words: BTreeMap::new(),
            unseeded: 0,
            writes: Vec::new(),
            delays: Vec::new(),
        }
    }

    /// A register file whose unseeded registers read as `0xffff_ffff`: the
    /// value a read of an address no device decodes returns on x86.
    pub fn undecoded() -> Self {
        Self::new().with_unseeded(0xffff_ffff)
    }

    /// Set what an unseeded register reads as.
    pub fn with_unseeded(mut self, value: u32) -> Self {
        self.unseeded = value;
        self
    }

    /// Seed registers by their Linux names.
    ///
    /// # Panics
    ///
    /// If a name is not in the register table: a seed for a register the
    /// driver cannot name would be a test that proves nothing.
    pub fn with(mut self, values: &[(&str, u32)]) -> Self {
        for (name, value) in values {
            let register = regs::named(name).unwrap_or_else(|| panic!("{name} is not a register"));
            self.words.insert(register.offset(), *value);
        }
        self
    }

    /// Read a register's current contents, for a test's own inspection.
    pub fn peek(&self, name: &str) -> u32 {
        let register = regs::named(name).unwrap_or_else(|| panic!("{name} is not a register"));
        self.words
            .get(&register.offset())
            .copied()
            .unwrap_or(self.unseeded)
    }

    /// Poke a register's contents behind the driver's back.
    pub fn poke(&mut self, name: &str, value: u32) {
        let register = regs::named(name).unwrap_or_else(|| panic!("{name} is not a register"));
        self.words.insert(register.offset(), value);
    }

    /// Every write the bus was asked to perform, accepted or refused.
    pub fn writes(&self) -> &[(u32, u32, bool)] {
        &self.writes
    }

    /// The writes that were accepted, as `(offset, value)`.
    pub fn accepted_writes(&self) -> Vec<(u32, u32)> {
        self.writes
            .iter()
            .filter(|(_, _, accepted)| *accepted)
            .map(|(offset, value, _)| (*offset, *value))
            .collect()
    }

    /// Every wait, in order.
    pub fn delays(&self) -> &[u32] {
        &self.delays
    }
}

impl IgcBus for FakeBus {
    fn read(&mut self, register: Register) -> Option<u32> {
        Some(
            self.words
                .get(&register.offset())
                .copied()
                .unwrap_or(self.unseeded),
        )
    }

    fn write(&mut self, register: Register, value: u32) -> bool {
        let accepted = register.is_writable();
        self.writes.push((register.offset(), value, accepted));
        if accepted {
            self.words.insert(register.offset(), value);
        }
        accepted
    }

    fn delay_us(&mut self, micros: u32) {
        self.delays.push(micros);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_is_by_linux_name_and_unseeded_reads_use_the_chosen_value() {
        let bus = FakeBus::new().with(&[("IGC_STATUS", 0x83)]);
        assert_eq!(bus.peek("IGC_STATUS"), 0x83);
        assert_eq!(bus.peek("IGC_CTRL"), 0);

        let undecoded = FakeBus::undecoded();
        assert_eq!(undecoded.peek("IGC_STATUS"), 0xffff_ffff);

        // `poke` changes the register file behind the driver's back, which is
        // how a test models the device changing state on its own -- a reset
        // completing, or a link coming up between two polls.
        let mut bus = FakeBus::new();
        bus.poke("IGC_STATUS", 0x0000_0002);
        assert_eq!(bus.peek("IGC_STATUS"), 0x0000_0002);
        assert!(bus.writes().is_empty(), "a poke is not a driver write");
    }

    #[test]
    fn a_write_to_a_read_only_register_is_recorded_as_refused() {
        let mut bus = FakeBus::new();
        assert!(!bus.write(regs::named("IGC_STATUS").unwrap(), 7));
        assert_eq!(bus.writes(), &[(0x00008, 7, false)]);
        assert!(bus.accepted_writes().is_empty());
        assert_eq!(bus.peek("IGC_STATUS"), 0);
        // A writable register is accepted and recorded as accepted, so a test
        // can tell the two apart.
        assert!(bus.write(regs::named("IGC_CTRL").unwrap(), 7));
        assert_eq!(bus.accepted_writes(), alloc::vec![(0x00000, 7)]);
    }

    #[test]
    fn the_hal_records_waits_per_thread() {
        FakeHal::reset();
        FakeHal::busy_wait_us(10);
        FakeHal::busy_wait_us(25);
        assert_eq!(FakeHal::delays(), alloc::vec![10, 25]);
        assert_eq!(FakeHal::delay_count(), 2);
        assert_eq!(FakeHal::total_us(), 35);
    }
}
