//! The Intel i225/i226 2.5 GbE controller driver.
//!
//! # What this is for
//!
//! TheKernel's acceptance plan for real hardware has two steps: a person looks
//! at the machine's screen and sees that the kernel booted, and then a checker
//! on another machine reaches it over the network with nobody in the loop.
//! The second step needs a driver for the NIC that is actually on the machine.
//! This module is that driver.
//!
//! # The assumption this driver exists to test
//!
//! The target is an Alder Lake-N mini-PC whose on-board controller is
//! **assumed** to be an Intel i225-V (`8086:15f3`) or i226-V (`8086:125c`).
//! That assumption is not a measurement: no PCI configuration-space byte has
//! ever been read from that machine.  It comes from the class of machine and
//! from this host's `pci.ids`, and it is recorded as an assumption everywhere
//! it appears -- here, in [`ids`], in the design note, and in the report's own
//! wording.  The first job of the code below is therefore to make the
//! assumption cheap to confirm or refute, and to say plainly which of the two
//! happened.
//!
//! # The phases
//!
//! 1. **Identify, do not program.**  Match the device ids, read configuration
//!    space, map BAR0, read the registers that identify the part, and print one
//!    verdict.  It writes nothing, and the report says so.
//! 2. **The register map.**  The named-register table in [`regs`]: every
//!    register the reset, link-up and descriptor-ring phases need, each with
//!    the vendor symbol and line its offset and access came from, a typed
//!    accessor where the field encoding is non-trivial, and an access rule the
//!    window enforces -- including the two registers whose *read* has a side
//!    effect, which the identify-only phase's register list therefore excludes.
//! 3. **Reset, station address and link.**  [`bringup`]: stop the MAC's DMA
//!    engine, reset the part, wait for the NVM auto-read, read the station
//!    address out of the receive-address registers, ask the PHY to
//!    autonegotiate and wait for link, then report speed and duplex.  It sets
//!    up no descriptor ring, so nothing can be sent or received yet, and it
//!    writes no PHY register -- the advertisement the firmware left is the one
//!    used, which is stated as a limitation rather than hidden.
//!
//! The phase that follows -- descriptor rings and the `NetDriverOps`
//! implementation -- arrives as its own change, and the driver grows into the
//! table the second phase declares.  Nothing below claims to be finished, and
//! the module documentation of each later phase states what it does *not*
//! establish.
//!
//! # What cannot be verified here
//!
//! There is no i225/i226 device model in QEMU, so this driver cannot be
//! boot-tested: a QEMU boot can only ever exercise the paths that *reject* a
//! machine without the part.  The evidence for the paths that touch the device
//! is host tests over synthetic apertures plus the cited register facts, and
//! the design note `docs/design/nic-igc.md` lists claim by claim what that
//! does and does not establish.  Every value this driver ever read from a real
//! i225 has, so far, been read zero times.

pub mod bringup;
pub mod ids;
pub mod probe;
pub mod regs;

#[cfg(test)]
pub(crate) mod fake;

use core::{marker::PhantomData, time::Duration};

pub use self::{
    bringup::{BringUp, BringUpError, LinkOutcome, StationAddress},
    ids::{DeviceId, Family, INTEL_VENDOR, identify},
    probe::{ConfigFacts, ProbeReport, Verdict},
    regs::{
        Access, DeviceControl, DeviceStatus, Meaning, NvmControl, ReceiveAddressHigh, Register,
        RegisterWindow, Speed, WINDOW_BYTES, assemble_receive_address, named,
    },
};

/// What the driver needs from the platform it is running on.
///
/// The trait exists for the same reason `IxgbeHal` does: the driver's logic
/// must be testable and portable without an architecture underneath it.  The
/// implementation lives beside the platform code that can provide it
/// (`axdriver`), and the host tests use a fake.
pub trait IgcHal {
    /// Busy-wait for at least `micros` microseconds.
    ///
    /// Every wait in this driver is bounded and every bound is stated in the
    /// vendor source it came from; a wait that expires is a reported failure,
    /// never a hang.
    fn busy_wait_us(micros: u32);
}

/// Register access, as the driver's logic sees it.
///
/// `read` and `write` take a [`Register`] rather than an offset so that no
/// caller can invent an address, and the implementation is where the access
/// rules and the mapped window are enforced.  `delay_us` is here rather than
/// on [`IgcHal`] because the polling loops that use it are part of the device
/// conversation: a test that drives those loops needs to see the waits.
pub trait IgcBus {
    /// Read a register, or `None` if the register is not in the mapped window.
    fn read(&mut self, register: Register) -> Option<u32>;

    /// Write a register, returning whether the write was performed.
    ///
    /// A read-only register, or one outside the mapped window, is refused.
    fn write(&mut self, register: Register, value: u32) -> bool;

    /// Wait at least `micros` microseconds.
    fn delay_us(&mut self, micros: u32);
}

/// The real bus: a mapped [`RegisterWindow`] and a platform that can wait.
pub struct WindowBus<H: IgcHal> {
    window: RegisterWindow,
    _hal: PhantomData<H>,
}

impl<H: IgcHal> WindowBus<H> {
    /// Take a bus over an aperture that is already mapped.
    ///
    /// # Safety
    ///
    /// As for [`RegisterWindow::from_mapped`]: the window must be a live
    /// mapping of the device's register aperture for as long as this value is
    /// used.
    pub const unsafe fn new(window: RegisterWindow) -> Self {
        Self {
            window,
            _hal: PhantomData,
        }
    }

    /// The window this bus reads and writes.
    pub const fn window(&self) -> RegisterWindow {
        self.window
    }
}

impl<H: IgcHal> IgcBus for WindowBus<H> {
    fn read(&mut self, register: Register) -> Option<u32> {
        self.window.read(register)
    }

    fn write(&mut self, register: Register, value: u32) -> bool {
        self.window.write(register, value)
    }

    fn delay_us(&mut self, micros: u32) {
        H::busy_wait_us(micros);
    }
}

/// A duration, for the HAL implementations that take one.
pub const fn micros(micros: u32) -> Duration {
    Duration::from_micros(micros as u64)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::igc::fake::FakeHal;

    struct Scratch {
        words: vec::Vec<u32>,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                words: vec![0; WINDOW_BYTES / 4],
            }
        }

        fn bus(&mut self) -> WindowBus<FakeHal> {
            // SAFETY: the buffer is live and `WINDOW_BYTES` long and outlives
            // the bus.
            unsafe {
                WindowBus::new(regs::RegisterWindow::from_mapped(
                    self.words.as_mut_ptr() as usize,
                    WINDOW_BYTES,
                ))
            }
        }
    }

    #[test]
    fn the_real_bus_enforces_the_register_table() {
        let mut scratch = Scratch::new();
        let mut bus = scratch.bus();
        scratch.words[0x00008 / 4] = 0x0000_0083;
        assert_eq!(bus.read(named("IGC_STATUS").unwrap()), Some(0x0000_0083));
        // Read-only in the table, so the write is refused and nothing changes.
        assert!(!bus.write(named("IGC_STATUS").unwrap(), 0xffff_ffff));
        assert_eq!(bus.read(named("IGC_STATUS").unwrap()), Some(0x0000_0083));
        // Writable in the table, so the write lands.
        assert!(bus.write(named("IGC_CTRL").unwrap(), 0x0400_0040));
        assert_eq!(bus.read(named("IGC_CTRL").unwrap()), Some(0x0400_0040));
        // A register the table does not name cannot be reached at all.
        assert!(named("IGC_RETA(0)").is_none());
    }

    #[test]
    fn the_real_bus_reports_its_window() {
        let mut scratch = Scratch::new();
        let bus = scratch.bus();
        assert_eq!(bus.window().len(), WINDOW_BYTES);
        assert_eq!(bus.window().base(), scratch.words.as_ptr() as usize);
    }

    #[test]
    fn a_delay_reaches_the_hal() {
        let mut scratch = Scratch::new();
        let mut bus = scratch.bus();
        FakeHal::reset();
        bus.delay_us(1234);
        assert_eq!(FakeHal::delays(), alloc::vec![1234]);
    }
}
