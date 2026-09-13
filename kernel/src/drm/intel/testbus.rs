//! A synthetic PCI bus, for the host tests.
//!
//! The probe's decisions are made from a configuration header, so the only
//! honest way to test them on a machine with no Intel GPU is to hand the probe
//! a header shaped like one.  This module builds those headers from the same
//! encoding rules the specification defines -- vendor and device id in the
//! first dword, class bytes above the revision, BARs at 0x10 -- so a test that
//! passes is a statement about the probe, not about a table the test and the
//! probe happen to share.
//!
//! It exists only in the host test build; nothing here is compiled into the
//! kernel.

use alloc::vec::Vec;

use super::pci::{self, Bdf, ConfigSpace, HEADER_TYPE_STANDARD};

/// One synthetic function's header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub(crate) bdf: Bdf,
    pub(crate) vendor_id: u16,
    pub(crate) device_id: u16,
    pub(crate) revision: u8,
    pub(crate) class: u8,
    pub(crate) subclass: u8,
    pub(crate) prog_if: u8,
    pub(crate) header_type: u8,
    pub(crate) subsystem_vendor_id: u16,
    pub(crate) subsystem_id: u16,
    pub(crate) command: u16,
    pub(crate) bars: [u32; pci::BAR_SLOTS],
}

impl Header {
    pub(crate) fn new(bdf: Bdf, vendor_id: u16, device_id: u16) -> Self {
        Self {
            bdf,
            vendor_id,
            device_id,
            revision: 0,
            class: pci::CLASS_DISPLAY,
            subclass: pci::SUBCLASS_VGA,
            prog_if: 0x00,
            header_type: HEADER_TYPE_STANDARD,
            subsystem_vendor_id: pci::VENDOR_INTEL,
            subsystem_id: 0,
            command: 0x0000_0006,
            bars: [0; pci::BAR_SLOTS],
        }
    }

    pub(crate) fn class(mut self, class: u8, subclass: u8) -> Self {
        self.class = class;
        self.subclass = subclass;
        self
    }

    pub(crate) fn revision(mut self, revision: u8) -> Self {
        self.revision = revision;
        self
    }

    pub(crate) fn prog_if(mut self, prog_if: u8) -> Self {
        self.prog_if = prog_if;
        self
    }

    pub(crate) fn subsystem(mut self, vendor_id: u16, device_id: u16) -> Self {
        self.subsystem_vendor_id = vendor_id;
        self.subsystem_id = device_id;
        self
    }

    /// Mark the device as multi-function, which is what tells the walk to
    /// probe functions 1..7 of its device number.
    pub(crate) fn multifunction(mut self) -> Self {
        self.header_type |= 0x80;
        self
    }

    pub(crate) fn bars(mut self, bars: [u32; pci::BAR_SLOTS]) -> Self {
        self.bars = bars;
        self
    }

    /// The dwords this function answers with, at the offsets a real header
    /// places them.
    fn words(&self) -> Vec<(u16, u32)> {
        let mut words = alloc::vec![
            (
                pci::offset::VENDOR_ID,
                u32::from(self.vendor_id) | (u32::from(self.device_id) << 16)
            ),
            (
                pci::offset::COMMAND,
                u32::from(self.command) | (u32::from(0x0010_u16) << 16)
            ),
            (
                pci::offset::REVISION_ID,
                u32::from(self.revision)
                    | (u32::from(self.prog_if) << 8)
                    | (u32::from(self.subclass) << 16)
                    | (u32::from(self.class) << 24)
            ),
            (0x0c, u32::from(self.header_type) << 16),
            (
                pci::offset::SUBSYSTEM_VENDOR_ID,
                u32::from(self.subsystem_vendor_id) | (u32::from(self.subsystem_id) << 16)
            ),
        ];
        for (slot, raw) in self.bars.iter().enumerate() {
            words.push((pci::bar_offset(slot as u8), *raw));
        }
        words
    }
}

/// A configuration space made of [`Header`]s.
pub(crate) struct FakeBus {
    words: Vec<(Bdf, u16, u32)>,
    /// The highest bus this synthetic platform declares, as
    /// `axconfig::devices::PCI_BUS_END` does for the real one.
    pub(crate) bus_end: u8,
}

impl FakeBus {
    pub(crate) fn new(headers: Vec<Header>) -> Self {
        let mut words = Vec::new();
        for header in &headers {
            for (offset, value) in header.words() {
                words.push((header.bdf, offset, value));
            }
        }
        Self {
            words,
            bus_end: 0xff,
        }
    }

    /// The same bus, but with the platform declaring only buses up to
    /// `bus_end`, as the q35 profile does not have to.
    pub(crate) fn with_bus_end(mut self, bus_end: u8) -> Self {
        self.bus_end = bus_end;
        self
    }

    /// Every declared function, for a test that wants to assert on the shape
    /// of the bus it built.
    pub(crate) fn functions(&self) -> Vec<Bdf> {
        let mut seen = Vec::new();
        for (bdf, ..) in &self.words {
            if !seen.contains(bdf) {
                seen.push(*bdf);
            }
        }
        seen
    }
}

impl ConfigSpace for FakeBus {
    /// Answer exactly as ECAM would: an address outside the declared window is
    /// unreachable, and every other address is looked up by the offset the
    /// arithmetic produced.
    fn read_u32(&self, bdf: Bdf, offset: u16) -> Option<u32> {
        pci::config_address(0xe000_0000, self.bus_end, bdf, offset, 4)?;
        self.words
            .iter()
            .find(|(candidate, candidate_offset, _)| {
                *candidate == bdf && *candidate_offset == offset
            })
            .map(|(_, _, value)| *value)
    }
}
