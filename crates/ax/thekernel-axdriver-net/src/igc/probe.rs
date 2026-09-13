//! The identify-only probe: read what the device says about itself, and say
//! whether it agrees with the device id that bound it.
//!
//! The probe answers four questions, in order, and stops at the first one it
//! cannot answer:
//!
//! 1. What does configuration space say this function is?
//! 2. Is the aperture the firmware assigned large enough to hold the registers
//!    this driver names?
//! 3. Do the identification registers answer at all?
//! 4. Are the values they answer with consistent with the part the device id
//!    claims to be?
//!
//! It writes nothing.  The register table this phase uses declares no writable
//! register, so "identify, do not program" is not a promise the code makes
//! about itself -- there is no writable register for it to reach.
//!
//! # What the verdict means, and what it does not
//!
//! A confirmed verdict means: a PCI function with one of the 16 device ids
//! `igc_pci_tbl` binds is present at a specific bus address, the aperture its
//! BAR names answered, and the registers that identify the part read back
//! values a part of this family can produce.  It does **not** mean the MAC
//! address is usable (nothing has reset the part, so the NVM auto-read may not
//! have run), it does not mean the link will come up, and it is not evidence
//! about any other machine: the only machine this code has ever run on is
//! QEMU, which has no such device.
//!
//! The negative case matters at least as much.  A machine without the part
//! must produce one greppable line saying so, plus -- when an Intel network
//! function was seen with a device id the table does not bind -- that id, so
//! the assumption this workstream rests on can be refuted from the log alone.

use alloc::{format, string::String, vec::Vec};

use super::{
    IgcBus,
    ids::{self, DeviceId},
    regs::{
        self, IDENTIFY, Meaning, NAMED, NAMED_SPAN, NvmControl, ReceiveAddressHigh, Register,
        WINDOW_BYTES, assemble_receive_address,
    },
};

/// Every report line starts with this, so the whole probe is one grep.
pub const PREFIX: &str = "igc";

/// One BAR, as the PCI layer decoded it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarFacts {
    /// The BAR index, 0..6.
    pub index: u8,
    /// Whether the BAR decodes memory (as opposed to I/O port space).
    pub is_memory: bool,
    /// Whether the address is 64-bit.
    pub is_64bit: bool,
    /// Whether the region is prefetchable.
    pub prefetchable: bool,
    /// The address the firmware assigned.  Zero means unassigned.
    pub address: u64,
    /// The size the PCI layer measured by the standard write/restore probe.
    pub size: u32,
}

impl BarFacts {
    /// Whether this BAR can hold the registers the driver names.
    pub const fn holds_the_named_registers(&self) -> bool {
        self.is_memory && self.address != 0 && (self.size as usize) >= NAMED_SPAN
    }

    /// How the report spells it.
    pub fn describe(&self) -> String {
        format!(
            "BAR{} {} at {:#014x} size {} bytes{}{}{}",
            self.index,
            if self.is_memory { "memory" } else { "I/O" },
            self.address,
            self.size,
            if self.is_64bit { ", 64-bit" } else { ", 32-bit" },
            if self.prefetchable {
                ", prefetchable"
            } else {
                ", non-prefetchable"
            },
            if self.address == 0 { ", UNASSIGNED" } else { "" },
        )
    }
}

/// An MSI-X capability, as the PCI layer found it.
///
/// Only two fields: the capability's offset and its Message Control register.
/// The table's own location (the BIR and offset in the dword after Message
/// Control) is *not* reported, because this kernel's PCI layer exposes typed
/// accessors -- `PciRoot::capabilities` hands back the capability id and the
/// two bytes after it -- and no raw configuration-space read.  The Message
/// Control word is enough to state whether MSI-X is there, how many vectors
/// it declares and whether firmware left it masked, which is what a reader of
/// this report wants; where the table lives needs reads this layer cannot
/// make, and this driver does not use interrupts anyway.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsixFacts {
    /// The capability's offset in configuration space.
    pub offset: u8,
    /// The message-control register: table size in bits 10:0, function mask in
    /// bit 14, enable in bit 15.
    pub message_control: u16,
}

impl MsixFacts {
    /// The number of MSI-X table entries the capability declares.
    pub const fn table_size(&self) -> u16 {
        (self.message_control & 0x07ff) + 1
    }

    /// Whether the capability is masked by firmware.
    pub const fn masked(&self) -> bool {
        self.message_control & 0x4000 != 0
    }

    /// Whether MSI-X is enabled.
    pub const fn enabled(&self) -> bool {
        self.message_control & 0x8000 != 0
    }

    /// How the report spells it.
    pub fn describe(&self) -> String {
        format!(
            "MSI-X capability at {:#04x}: {} table entries, {}{}",
            self.offset,
            self.table_size(),
            if self.enabled() { "enabled" } else { "disabled" },
            if self.masked() { ", masked" } else { "" },
        )
    }
}

/// What configuration space says about the function, with no interpretation.
///
/// The fields are plain data on purpose: reading configuration space needs the
/// platform's PCI layer, which the net crate does not have, so the layer that
/// has it fills this in and this crate decides what it means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigFacts {
    /// The function's bus address, spelled `bbbb:dd:f`.
    pub bdf: String,
    /// The vendor id.
    pub vendor_id: u16,
    /// The device id.
    pub device_id: u16,
    /// The subsystem vendor id.
    pub subsystem_vendor_id: u16,
    /// The subsystem device id.
    pub subsystem_device_id: u16,
    /// The revision id.  Linux's `igc` records it (`igc_main.c:7011`
    /// `hw->revision_id = pdev->revision`) and never uses it, so this driver
    /// reports it and draws no conclusion from it.
    pub revision: u8,
    /// The class code.
    pub class: u8,
    /// The subclass code.
    pub subclass: u8,
    /// The programming interface.
    pub prog_if: u8,
    /// BAR0, the register aperture.
    pub bar0: BarFacts,
    /// The number of BARs the PCI layer decoded, including BAR0.
    pub bars_decoded: u8,
    /// The MSI-X capability, when one is present.
    pub msix: Option<MsixFacts>,
    /// Whether an MSI capability is present.
    pub has_msi: bool,
    /// The firmware-assigned legacy interrupt line and pin, when the PCI layer
    /// could decode them.  `0xff` for the line means "none assigned".
    pub interrupt_line_and_pin: Option<(u8, u8)>,
}

impl ConfigFacts {
    /// One line of configuration space, as the report prints it.
    pub fn describe(&self) -> String {
        format!(
            "{}: vendor {:04x} device {:04x} ({}), subsystem {:04x}:{:04x}, revision {:#04x}, \
             class {:02x}:{:02x}:{:02x}, {} BARs",
            self.bdf,
            self.vendor_id,
            self.device_id,
            match ids::identify(self.vendor_id, self.device_id) {
                Some(device) => device.linux_symbol,
                None => "not in the device table",
            },
            self.subsystem_vendor_id,
            self.subsystem_device_id,
            self.revision,
            self.class,
            self.subclass,
            self.prog_if,
            self.bars_decoded,
        )
    }

    /// Whether the function's class says it is a network controller.
    ///
    /// Class `0x02` is "network controller" and subclass `0x00` is Ethernet
    /// (`pci.ids` and the PCI code tables agree on this; Linux's `igc` does
    /// not check the class at all, because the device table is the stronger
    /// statement).  A mismatch is reported rather than used to reject the
    /// device, because firmware that misdescribes a function is a real thing
    /// and the device id is the fact this driver binds on.
    pub const fn claims_to_be_ethernet(&self) -> bool {
        self.class == 0x02 && self.subclass == 0x00
    }
}

/// One register the probe read, or did not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reading {
    /// Which register.
    pub register: Register,
    /// What it read, or `None` when it was not read.
    pub value: Option<u32>,
    /// Why it was not read, when it was not.
    pub skipped: Option<&'static str>,
}

/// One thing the probe checked that could have come out either way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Check {
    /// A short name for the check.
    pub name: &'static str,
    /// What the check found.
    pub agrees: bool,
    /// The sentence the report prints.
    pub note: String,
}

/// How the probe's reading of the device came out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The id matched the table and the registers are consistent with the part
    /// the id claims to be.
    Identified,
    /// The id matched the table, but the hardware contradicted it.  The string
    /// is the reason, in the report's words.
    Contradicted(&'static str),
    /// The aperture did not answer, so nothing could be established.
    Unreadable(&'static str),
}

/// Everything the probe learned about one function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeReport {
    /// The device table entry that bound this function.
    pub device: &'static DeviceId,
    /// What configuration space said.
    pub facts: ConfigFacts,
    /// Every register the probe read, in table order.
    pub readings: Vec<Reading>,
    /// Every check the probe made.
    pub checks: Vec<Check>,
    /// Where it ended up.
    pub verdict: Verdict,
}

impl ProbeReport {
    /// The register with this meaning, when the probe read one.
    pub fn reading(&self, meaning: Meaning) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.register.meaning() == meaning)
    }

    /// The value of the register with this meaning, when one was read.
    pub fn value(&self, meaning: Meaning) -> Option<u32> {
        self.reading(meaning)?.value
    }

    /// The one sentence a log reader should take away.
    pub fn verdict(&self) -> String {
        match &self.verdict {
            Verdict::Identified => format!(
                "{} identified as {} by PCI id and confirmed by {} identification registers; \
                 nothing was written",
                self.facts.bdf,
                self.device.linux_symbol,
                self.readings
                    .iter()
                    .filter(|reading| reading.value.is_some())
                    .count(),
            ),
            Verdict::Contradicted(reason) => format!(
                "{} bound as {} but NOT confirmed: {reason}; nothing was written",
                self.facts.bdf, self.device.linux_symbol,
            ),
            Verdict::Unreadable(reason) => format!(
                "{} bound as {} but the aperture did not answer: {reason}; nothing was written",
                self.facts.bdf, self.device.linux_symbol,
            ),
        }
    }

    /// The report as text.
    ///
    /// One string, printed to the kernel log; every line carries [`PREFIX`].
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{PREFIX}: device {}: {}\n",
            self.facts.bdf,
            self.device.linux_symbol,
        ));
        out.push_str(&format!("{PREFIX}:   {}\n", self.facts.describe()));
        out.push_str(&format!("{PREFIX}:   {}\n", self.facts.bar0.describe()));
        out.push_str(&format!(
            "{PREFIX}:   window: {} bytes mapped of BAR0, {} bytes needed by {} named registers\n",
            WINDOW_BYTES,
            NAMED_SPAN,
            NAMED.len(),
        ));
        match self.facts.msix {
            Some(msix) => out.push_str(&format!("{PREFIX}:   {}\n", msix.describe())),
            None => out.push_str(&format!("{PREFIX}:   no MSI-X capability\n")),
        }
        if let Some((line, pin)) = self.facts.interrupt_line_and_pin {
            out.push_str(&format!(
                "{PREFIX}:   legacy interrupt line {} pin {} (reported, not used: this phase \
                 programs no interrupts and enables none)\n",
                line, pin,
            ));
        }
        if self.readings.is_empty() {
            out.push_str(&format!(
                "{PREFIX}:   registers: none read; the aperture was never mapped\n"
            ));
        } else {
            out.push_str(&format!("{PREFIX}:   registers:\n"));
        }
        for reading in &self.readings {
            match reading.value {
                Some(value) => out.push_str(&format!(
                    "{PREFIX}:     {:<16} {:#07x} = {:#010x} [{}]\n",
                    reading.register.name(),
                    reading.register.offset(),
                    value,
                    reading.register.source(),
                )),
                None => out.push_str(&format!(
                    "{PREFIX}:     {:<16} {:#07x} = not read: {}\n",
                    reading.register.name(),
                    reading.register.offset(),
                    reading.skipped.unwrap_or("no reason recorded"),
                )),
            }
        }
        for check in &self.checks {
            out.push_str(&format!(
                "{PREFIX}:   {}: {}: {}\n",
                if check.agrees {
                    "consistent"
                } else {
                    "INCONSISTENT"
                },
                check.name,
                check.note,
            ));
        }
        out.push_str(&format!("{PREFIX}: verdict: {}\n", self.verdict()));
        out.push_str(&format!(
            "{PREFIX}: this proves the function was enumerated, its aperture was mapped and its \
             identification registers answered; it does not prove the part works, it does not \
             prove the MAC address is usable before a reset, and it is not evidence about any \
             other machine -- no register value in this report has ever been read from a real \
             i225 or i226\n"
        ));
        out
    }
}

/// An Intel function the walk saw that this driver does not bind.
///
/// Only the fields the configuration space of a *non-matching* function can be
/// read for without touching its BARs: a device this driver does not bind is
/// not a device it may size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// The function's bus address.
    pub bdf: String,
    /// The vendor id.
    pub vendor_id: u16,
    /// The device id.
    pub device_id: u16,
    /// The class code.
    pub class: u8,
    /// The subclass code.
    pub subclass: u8,
    /// The revision id.
    pub revision: u8,
}

/// One log line for a candidate, printed as soon as the walk sees it.
///
/// The wording is deliberately blunt: the whole point of this line is that a
/// machine which turns out not to carry the assumed part says what it does
/// carry, in the same log as the verdict that says nothing matched.
pub fn candidate_line(candidate: &Candidate) -> String {
    format!(
        "{PREFIX}: candidate {}: {:04x}:{:04x} class {:02x}:{:02x} rev {:#04x} is not a device \
         id this driver binds ({} ids, vendor {:04x}); if this is the machine's NIC, the \
         device-id assumption behind this driver is wrong",
        candidate.bdf,
        candidate.vendor_id,
        candidate.device_id,
        candidate.class,
        candidate.subclass,
        candidate.revision,
        ids::DEVICES.len(),
        ids::INTEL_VENDOR,
    )
}

/// The verdict for a machine where no function matched the table.
///
/// `candidate_count` is how many Intel network functions the walk saw that
/// this driver does not bind; their device ids are in the candidate lines the
/// walk printed, which are the answer to "what NIC is actually there".
pub fn absence_report(
    functions_walked: usize,
    bus_end: u8,
    intel_functions: usize,
    candidate_count: usize,
) -> String {
    format!(
        "{PREFIX}: bus walk: {functions_walked} PCI functions answered on buses 0..={bus_end}, \
         {intel_functions} from Intel, none matching a device id this driver binds ({} ids, \
         vendor {:04x})\n{PREFIX}: verdict: no supported device present: no PCI function \
         matched a device id this driver binds. No aperture was mapped, no register was read, \
         and nothing was written{}\n",
        ids::DEVICES.len(),
        ids::INTEL_VENDOR,
        if candidate_count == 0 {
            String::new()
        } else {
            format!(
                ". {candidate_count} Intel network function{} above {} not bound by this driver, \
                 and {} device id{} {} what this machine actually has -- which is what the \
                 assumption behind this driver has to be checked against",
                if candidate_count == 1 { "" } else { "s" },
                if candidate_count == 1 { "is" } else { "are" },
                if candidate_count == 1 { "that" } else { "those" },
                if candidate_count == 1 { "" } else { "s" },
                if candidate_count == 1 { "is" } else { "are" },
            )
        },
    )
}

/// The report for a function this driver refuses before mapping anything.
///
/// The refusal path exists so that a function whose BAR is unassigned or too
/// small is *reported* rather than mapped: the report says the aperture was
/// never touched, and the readings are empty because nothing was read.
pub fn refusal(facts: ConfigFacts, device: &'static DeviceId) -> ProbeReport {
    let note = format!(
        "not mapped: {} cannot hold the {} bytes this driver's {} named registers need",
        facts.bar0.describe(),
        NAMED_SPAN,
        NAMED.len(),
    );
    ProbeReport {
        device,
        facts,
        readings: Vec::new(),
        checks: alloc::vec![Check {
            name: "aperture",
            agrees: false,
            note,
        }],
        verdict: Verdict::Contradicted(
            "the BAR is unassigned, is I/O space, or is too small for the registers this driver \
             reads",
        ),
    }
}

/// Whether an address is a usable unicast Ethernet address.
///
/// The rule is the kernel's `is_valid_ether_addr`
/// (`include/linux/etherdevice.h`): not all zero, and not multicast (the
/// multicast bit is the low bit of the first byte, which also makes the
/// broadcast address invalid).  This is this project's own implementation of
/// that rule, not a copy of it.
pub fn is_valid_unicast(address: [u8; 6]) -> bool {
    address != [0; 6] && address[0] & 0x01 == 0
}

/// Read the identification registers and decide what they say.
///
/// This is the whole of the identify-only phase.  It reads; it never writes,
/// and a test asserts that the bus saw no write at all.
pub fn run<B: IgcBus>(facts: ConfigFacts, device: &'static DeviceId, bus: &mut B) -> ProbeReport {
    let mut readings = Vec::with_capacity(IDENTIFY.len());
    for register in IDENTIFY {
        readings.push(Reading {
            register: *register,
            value: bus.read(*register),
            skipped: None,
        });
    }
    let value = |meaning: Meaning| -> Option<u32> {
        readings
            .iter()
            .find(|reading| reading.register.meaning() == meaning)
            .and_then(|reading| reading.value)
    };

    let mut checks = Vec::new();

    // The aperture must answer at all.  A read of an address that no device
    // decodes returns all ones on x86, so an identification register that
    // reads all ones is evidence that this window is not the device the
    // configuration space described -- which is the one failure mode a probe
    // that maps a BAR can have.
    let control = value(Meaning::DeviceControl);
    let status = value(Meaning::DeviceStatus);
    let undecoded = [control, status]
        .iter()
        .filter(|value| **value == Some(0xffff_ffff))
        .count();
    if undecoded > 0 {
        checks.push(Check {
            name: "aperture",
            agrees: false,
            note: format!(
                "{undecoded} of the two always-present registers read 0xffffffff, which is what \
                 an address no device decodes returns on x86: the BAR does not belong to this \
                 function, or the function is not answering"
            ),
        });
        return ProbeReport {
            device,
            facts,
            readings,
            verdict: Verdict::Unreadable(
                "the register aperture returned all ones for a register the part must implement",
            ),
            checks,
        };
    }

    // The BAR has to be big enough for the registers the driver named.  The
    // size came from the PCI layer's own write/restore probe, so this compares
    // two independent facts: what configuration space says the region is, and
    // what this driver intends to touch.
    checks.push(Check {
        name: "bar size",
        agrees: facts.bar0.holds_the_named_registers(),
        note: format!(
            "{} holds the highest named register end {:#x}",
            facts.bar0.describe(),
            NAMED_SPAN,
        ),
    });

    // The receive address the NVM auto-read leaves in RAL0/RAH0.  Nothing has
    // reset the part in this phase, so these may legitimately read as zero --
    // which is reported as what it is rather than treated as a failure of
    // identity.
    let low = value(Meaning::ReceiveAddressLow);
    let high = value(Meaning::ReceiveAddressHigh);
    if let (Some(low), Some(high)) = (low, high) {
        let high = ReceiveAddressHigh::new(high);
        let address = assemble_receive_address(low, high);
        let valid = is_valid_unicast(address);
        checks.push(Check {
            name: "receive address 0",
            agrees: valid,
            note: format!(
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} with RAH.AV {}: {}",
                address[0],
                address[1],
                address[2],
                address[3],
                address[4],
                address[5],
                u8::from(high.address_valid()),
                if valid {
                    "a usable unicast address"
                } else {
                    "not a usable unicast address; a blank or unread NVM looks like this, and \
                     this phase deliberately does not reset the part to make the auto-read run"
                },
            ),
        });
    }

    // The LAN function number.  A single-port part reports function 0; a
    // non-zero value is not a fault, but it is the kind of thing a reader
    // wants to see next to the BAR address.
    if let Some(status) = status {
        let status = regs::DeviceStatus::new(status);
        checks.push(Check {
            name: "link",
            agrees: true,
            note: format!(
                "STATUS says link {}, {}, speed {}{}",
                if status.link_up() { "up" } else { "down" },
                if status.full_duplex() {
                    "full duplex"
                } else {
                    "half duplex"
                },
                status.speed().describe(),
                if status.link_up() {
                    ""
                } else {
                    " (the speed field is only meaningful once link is up)"
                },
            ),
        });
        checks.push(Check {
            name: "PCI function",
            agrees: status.function_id() == 0,
            note: format!(
                "STATUS.FUNC is {}, which is the LAN function number{}",
                status.function_id(),
                if status.function_id() == 0 {
                    ""
                } else {
                    "; a single-port part reports 0, so this is worth a second look"
                },
            ),
        });
    }

    // The NVM/flash state.  This is state, not identity: it is reported so
    // that a blank-NVM part and a part whose auto-read has not run are
    // distinguishable in the log.
    if let Some(eecd) = value(Meaning::NvmControl) {
        let eecd = NvmControl::new(eecd);
        checks.push(Check {
            name: "NVM",
            agrees: true,
            note: format!(
                "auto-read {}, flash {}, size field {}{}",
                if eecd.auto_read_done() {
                    "done"
                } else {
                    "not done"
                },
                if eecd.flash_detected() {
                    "detected"
                } else {
                    "not detected"
                },
                eecd.size_field(),
                if device.is_blank_nvm() {
                    "; this device id is one of the blank-NVM parts, so the address above is \
                     whatever the unprogrammed NVM left behind"
                } else {
                    ""
                },
            ),
        });
    }

    // The assumption check: is this one of the two ids the target machine is
    // assumed to carry?
    let assumed = ids::ASSUMED_TARGET_IDS.contains(&facts.device_id);
    checks.push(Check {
        name: "device-id assumption",
        agrees: assumed,
        note: format!(
            "{:04x}:{:04x} is {}one of the two ids the target machine is assumed to carry \
             ({:04x}, {:04x})",
            facts.vendor_id,
            facts.device_id,
            if assumed { "" } else { "NOT " },
            ids::ASSUMED_TARGET_IDS[0],
            ids::ASSUMED_TARGET_IDS[1],
        ),
    });

    let verdict = if !facts.bar0.holds_the_named_registers() {
        Verdict::Contradicted("the BAR is too small or unassigned for the registers this driver reads")
    } else {
        // Identity rests on the receive address being a usable unicast
        // address: everything else in the table is either a state bit or a
        // value with no documented valid range.
        match (low, high) {
            (Some(low), Some(high)) => {
                let address = assemble_receive_address(low, ReceiveAddressHigh::new(high));
                if is_valid_unicast(address) {
                    Verdict::Identified
                } else {
                    Verdict::Contradicted(
                        "the receive address registers hold something that is not a usable \
                         unicast MAC address",
                    )
                }
            }
            _ => Verdict::Unreadable("a register the identity check needs was not readable"),
        }
    };

    ProbeReport {
        device,
        facts,
        readings,
        checks,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::igc::fake::FakeBus;

    fn facts() -> ConfigFacts {
        ConfigFacts {
            bdf: String::from("0000:01:00.0"),
            vendor_id: ids::INTEL_VENDOR,
            device_id: 0x15F3,
            subsystem_vendor_id: 0x8086,
            subsystem_device_id: 0x0000,
            revision: 0x03,
            class: 0x02,
            subclass: 0x00,
            prog_if: 0x00,
            bar0: BarFacts {
                index: 0,
                is_memory: true,
                is_64bit: true,
                prefetchable: false,
                address: 0xf7a0_0000,
                size: 0x20_0000,
            },
            bars_decoded: 2,
            msix: Some(MsixFacts {
                offset: 0xa0,
                message_control: 4,
            }),
            has_msi: false,
            interrupt_line_and_pin: Some((0xff, 1)),
        }
    }

    /// The registers a healthy i225-V answers with, after a reset: a valid
    /// unicast receive address with RAH.AV set, link up at 2.5 Gb/s full
    /// duplex, auto-read done, flash present.
    fn healthy_bus() -> FakeBus {
        FakeBus::new().with(&[
            ("IGC_CTRL", regs::bits::CTRL_SLU),
            ("IGC_STATUS", 0x0000_0083 | regs::bits::STATUS_SPEED_2500),
            (
                "IGC_EECD",
                regs::bits::EECD_AUTO_RD | regs::bits::EECD_FLASH_DETECTED_I225 | (5 << 11),
            ),
            ("IGC_RAL(0)", 0x3322_1100),
            ("IGC_RAH(0)", 0x8000_0000 | 0x5544),
        ])
    }

    fn device() -> &'static DeviceId {
        ids::identify(ids::INTEL_VENDOR, 0x15F3).expect("the assumed id")
    }

    #[test]
    fn a_healthy_part_is_identified_and_nothing_is_written() {
        let mut bus = healthy_bus();
        let report = run(facts(), device(), &mut bus);
        assert_eq!(report.verdict, Verdict::Identified, "{:#?}", report.checks);
        assert_eq!(report.readings.len(), IDENTIFY.len());
        assert!(
            report.readings.iter().all(|reading| reading.value.is_some()),
            "every named register was read"
        );
        // The whole point of the phase: not one write.
        assert!(
            bus.writes().is_empty(),
            "the identify-only phase wrote: {:?}",
            bus.writes()
        );
        assert!(bus.delays().is_empty(), "the probe waits for nothing");
        assert!(report.checks.iter().all(|check| check.agrees), "{:#?}", report.checks);
    }

    #[test]
    fn the_report_renders_every_line_with_the_prefix_and_the_facts() {
        let mut bus = healthy_bus();
        let report = run(facts(), device(), &mut bus);
        let text = report.render();
        for line in text.lines() {
            assert!(line.starts_with(PREFIX), "unprefixed line: {line}");
        }
        assert!(text.contains("0000:01:00.0"), "{text}");
        assert!(text.contains("IGC_RAH(0)"), "{text}");
        assert!(text.contains("igc_regs.h:115"), "the citation is printed: {text}");
        assert!(text.contains("00:11:22:33:44:55"), "{text}");
        assert!(text.contains("2500 Mb/s"), "{text}");
        assert!(text.contains("MSI-X capability"), "{text}");
        assert!(
            text.contains("verdict: 0000:01:00.0 identified as IGC_DEV_ID_I225_V"),
            "{text}"
        );
        // The honesty footer is not optional.
        assert!(
            text.contains("has ever been read from a real i225"),
            "the footer that says no value here came from real hardware: {text}"
        );
        assert!(text.contains("nothing was written"), "{text}");
    }

    #[test]
    fn an_aperture_that_returns_all_ones_is_not_claimed_as_a_device() {
        let mut bus = FakeBus::undecoded();
        let report = run(facts(), device(), &mut bus);
        assert!(matches!(report.verdict, Verdict::Unreadable(_)), "{report:#?}");
        let text = report.render();
        assert!(text.contains("INCONSISTENT: aperture"), "{text}");
        assert!(text.contains("the aperture did not answer"), "{text}");
        // Nothing may be claimed about a part whose aperture never answered.
        assert!(!text.contains("identified as"), "{text}");
    }

    #[test]
    fn a_bar_too_small_for_the_named_registers_is_contradicted() {
        let mut facts = facts();
        facts.bar0.size = 0x1000;
        let mut bus = healthy_bus();
        let report = run(facts, device(), &mut bus);
        assert!(
            matches!(report.verdict, Verdict::Contradicted(reason) if reason.contains("BAR")),
            "{report:#?}"
        );
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "bar size" && !check.agrees)
        );
    }

    #[test]
    fn an_unassigned_bar_is_contradicted_rather_than_mapped() {
        let mut facts = facts();
        facts.bar0.address = 0;
        let mut bus = healthy_bus();
        let report = run(facts, device(), &mut bus);
        assert!(matches!(report.verdict, Verdict::Contradicted(_)));
        assert!(report.render().contains("UNASSIGNED"), "{}", report.render());
    }

    #[test]
    fn an_invalid_receive_address_is_contradicted_with_the_reason() {
        // A blank NVM leaves zeros behind.
        let mut bus = FakeBus::new().with(&[
            ("IGC_CTRL", 0),
            ("IGC_STATUS", 0x0000_0000),
            ("IGC_EECD", 0),
            ("IGC_RAL(0)", 0),
            ("IGC_RAH(0)", 0),
        ]);
        let report = run(facts(), device(), &mut bus);
        assert!(
            matches!(report.verdict, Verdict::Contradicted(reason) if reason.contains("unicast")),
            "{report:#?}"
        );
        let text = report.render();
        assert!(text.contains("blank or unread NVM"), "{text}");
        // A multicast address is invalid too, and the multicast bit is the
        // low bit of the first byte.
        let mut multicast = FakeBus::new().with(&[
            ("IGC_RAL(0)", 0x3322_1101),
            ("IGC_RAH(0)", 0x8000_0000 | 0x5544),
        ]);
        let report = run(facts(), device(), &mut multicast);
        assert!(matches!(report.verdict, Verdict::Contradicted(_)));
    }

    #[test]
    fn the_receive_address_rule_matches_is_valid_ether_addr() {
        assert!(is_valid_unicast([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]));
        assert!(!is_valid_unicast([0; 6]), "all zero");
        assert!(!is_valid_unicast([0xff; 6]), "broadcast is multicast");
        assert!(
            !is_valid_unicast([0x01, 0, 0, 0, 0, 0]),
            "the multicast bit is bit 0 of the first byte"
        );
        assert!(is_valid_unicast([0x02, 0, 0, 0, 0, 0]), "locally administered");
    }

    #[test]
    fn a_part_whose_registers_read_zero_is_not_claimed() {
        // Everything zero: the aperture answers, but nothing that identifies
        // the part is there.  The report says so; it does not call it a device.
        let mut bus = FakeBus::new();
        let report = run(facts(), device(), &mut bus);
        assert!(matches!(report.verdict, Verdict::Contradicted(_)));
        let text = report.render();
        assert!(text.contains("INCONSISTENT: receive address 0"), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
    }

    #[test]
    fn an_id_that_is_not_one_the_target_is_assumed_to_carry_says_so() {
        let mut facts = facts();
        facts.device_id = 0x125B; // I226-LM: bound, but not the assumed id
        let device = ids::identify(ids::INTEL_VENDOR, 0x125B).unwrap();
        let mut bus = healthy_bus();
        let report = run(facts, device, &mut bus);
        assert_eq!(report.verdict, Verdict::Identified);
        let assumption = report
            .checks
            .iter()
            .find(|check| check.name == "device-id assumption")
            .unwrap();
        assert!(!assumption.agrees);
        assert!(assumption.note.contains("NOT one of the two ids"), "{}", assumption.note);
    }

    #[test]
    fn the_msix_facts_decode_table_size_and_state() {
        let msix = MsixFacts {
            offset: 0xa0,
            message_control: 0x801f,
        };
        assert_eq!(msix.table_size(), 0x20);
        assert!(msix.enabled());
        assert!(!msix.masked());
        let text = msix.describe();
        assert!(text.contains("32 table entries"), "{text}");
        assert!(text.contains("enabled"), "{text}");
        // A masked, disabled capability reads the other way.
        let masked = MsixFacts {
            offset: 0x70,
            message_control: 0x4004,
        };
        assert_eq!(masked.table_size(), 5);
        assert!(masked.masked());
        assert!(!masked.enabled());
    }

    #[test]
    fn the_absence_report_is_greppable_and_says_nothing_was_touched() {
        let empty = absence_report(12, 0xff, 3, 0);
        for line in empty.lines() {
            assert!(line.starts_with(PREFIX), "{line}");
        }
        assert!(
            empty.contains("verdict: no supported device present"),
            "{empty}"
        );
        assert!(empty.contains("nothing was written"), "{empty}");
        assert!(empty.contains("No aperture was mapped"), "{empty}");
        assert!(!empty.contains("candidate"), "{empty}");
    }

    #[test]
    fn a_candidate_line_names_the_id_that_refutes_the_assumption() {
        // The case that actually refutes it: an Intel network function whose
        // device id the table does not bind.
        let line = candidate_line(&Candidate {
            bdf: String::from("0000:00:1f.6"),
            vendor_id: ids::INTEL_VENDOR,
            device_id: 0x15B8, // an e1000e-class id
            class: 0x02,
            subclass: 0x00,
            revision: 0x00,
        });
        assert!(line.starts_with(PREFIX), "{line}");
        assert!(line.contains("candidate 0000:00:1f.6"), "{line}");
        assert!(line.contains("8086:15b8"), "{line}");
        assert!(line.contains("the device-id assumption"), "{line}");

        let report = absence_report(12, 0xff, 3, 1);
        assert!(report.contains("1 Intel network function"), "{report}");
        assert!(report.contains("what this machine actually has"), "{report}");
        let many = absence_report(12, 0xff, 3, 2);
        assert!(many.contains("2 Intel network functions"), "{many}");
        assert!(many.contains("are not bound"), "{many}");
    }

    #[test]
    fn a_refusal_reports_that_the_aperture_was_never_mapped() {
        let mut facts = facts();
        facts.bar0.address = 0;
        let report = refusal(facts, device());
        assert!(matches!(report.verdict, Verdict::Contradicted(_)));
        assert!(report.readings.is_empty(), "nothing may be read");
        let text = report.render();
        assert!(text.contains("the aperture was never mapped"), "{text}");
        assert!(text.contains("UNASSIGNED"), "{text}");
    }

    #[test]
    fn configuration_space_renders_the_table_symbol_not_a_marketing_name() {
        let text = facts().describe();
        assert!(text.contains("IGC_DEV_ID_I225_V"), "{text}");
        assert!(text.contains("subsystem 8086:0000"), "{text}");
        assert!(text.contains("class 02:00:00"), "{text}");
        assert!(facts().claims_to_be_ethernet());
        let mut not_ethernet = facts();
        not_ethernet.class = 0x0c;
        assert!(!not_ethernet.claims_to_be_ethernet());
    }

    #[test]
    fn every_check_the_report_makes_carries_a_sentence() {
        let mut bus = healthy_bus();
        let report = run(facts(), device(), &mut bus);
        assert!(report.checks.len() >= 5, "{:#?}", report.checks);
        for check in &report.checks {
            assert!(!check.note.is_empty(), "{check:?}");
            assert!(!check.name.is_empty(), "{check:?}");
        }
    }
}
