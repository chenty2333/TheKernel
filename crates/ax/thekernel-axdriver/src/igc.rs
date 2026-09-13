//! The platform half of the `igc` driver: mapping, waiting, and the probe.
//!
//! The driver's logic lives in `axdriver_net::igc`, which has no architecture
//! underneath it and is therefore testable on the host.  This module is the
//! other half: the part that needs this platform's PCI layer, its physical
//! memory map and its clock.  Keeping the split sharp is what lets the
//! arithmetic, the register encodings and the state machines be verified on a
//! machine that has no such NIC.
//!
//! # What this half does, and does not, do
//!
//! It reads configuration space through the bus walk's `PciRoot` (whose
//! accessors are read-only except for the command register and BAR sizing that
//! the walk itself performs before any driver is consulted), maps BAR0 at its
//! direct-map address, and hands the driver a [`WindowBus`] over it.  It does
//! not write a register: the driver's register table declares no writable
//! register in this phase, so there is nothing for it to write through.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axdriver_net::igc::{
    self, IgcHal, ProbeReport, WindowBus,
    ids::{self, INTEL_VENDOR},
    probe::{BarFacts, Candidate, ConfigFacts, MsixFacts},
    regs::{RegisterWindow, WINDOW_BYTES},
};
use axhal::mem::phys_to_virt;
use log::*;

/// The PCI class and subclass a network controller reports.
const CLASS_NETWORK: u8 = 0x02;
const SUBCLASS_ETHERNET: u8 = 0x00;

/// The MSI-X capability id (PCI Local Bus specification; Linux spells it
/// `PCI_CAP_ID_MSIX` in `include/uapi/linux/pci_regs.h`).
const CAPABILITY_ID_MSIX: u8 = 0x11;
/// The MSI capability id (Linux `PCI_CAP_ID_MSI`).
const CAPABILITY_ID_MSI: u8 = 0x05;

/// What the platform provides to the driver.
pub struct IgcHalImpl;

impl IgcHal for IgcHalImpl {
    fn busy_wait_us(micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
}

/// What the walk saw, for the verdict it prints when nothing matched.
///
/// The counts are the walk's own, not the driver's: every function the bus
/// reached is offered to this driver's probe, so counting here counts the
/// functions that answered.
#[derive(Debug, Default)]
pub(crate) struct ProbeTally {
    pub(crate) functions: AtomicUsize,
    pub(crate) intel: AtomicUsize,
    pub(crate) candidates: AtomicUsize,
    pub(crate) matched: AtomicBool,
    pub(crate) reported: AtomicBool,
}

static TALLY: ProbeTally = ProbeTally {
    functions: AtomicUsize::new(0),
    intel: AtomicUsize::new(0),
    candidates: AtomicUsize::new(0),
    matched: AtomicBool::new(false),
    reported: AtomicBool::new(false),
};

/// Decode the configuration space of one function into the driver's facts.
///
/// Everything here is a read.  `bars_decoded` counts the BARs the PCI layer
/// decoded in order to size them; BAR sizing writes the BAR and restores it,
/// which the bus layer does for every function it configures, before this
/// driver is consulted.
fn config_facts(
    root: &mut axdriver_pci::PciRoot,
    bdf: axdriver_pci::DeviceFunction,
    dev_info: &axdriver_pci::DeviceFunctionInfo,
) -> ConfigFacts {
    let (subsystem_vendor_id, subsystem_device_id) = root.endpoint_subsystem_ids(bdf);
    let bars_decoded = match root.bars(bdf) {
        Ok(bars) => bars.iter().filter(|bar| bar.is_some()).count() as u8,
        Err(_) => 0,
    };
    let bar0 = match root.bar_info(bdf, 0) {
        Ok(axdriver_pci::BarInfo::Memory {
            address_type,
            prefetchable,
            address,
            size,
        }) => BarFacts {
            index: 0,
            is_memory: true,
            is_64bit: matches!(address_type, axdriver_pci::MemoryBarType::Width64),
            prefetchable,
            address,
            size,
        },
        Ok(axdriver_pci::BarInfo::IO { address, size }) => BarFacts {
            index: 0,
            is_memory: false,
            is_64bit: false,
            prefetchable: false,
            address: u64::from(address),
            size,
        },
        Err(_) => BarFacts {
            index: 0,
            is_memory: false,
            is_64bit: false,
            prefetchable: false,
            address: 0,
            size: 0,
        },
    };
    let mut msix = None;
    let mut has_msi = false;
    for capability in root.capabilities(bdf) {
        match capability.id {
            // For MSI-X the two bytes after the capability id are the Message
            // Control register, which is what the PCI layer hands back.
            CAPABILITY_ID_MSIX => {
                msix = Some(MsixFacts {
                    offset: capability.offset,
                    message_control: capability.private_header,
                })
            }
            CAPABILITY_ID_MSI => has_msi = true,
            _ => {}
        }
    }
    ConfigFacts {
        bdf: alloc::format!("{bdf}"),
        vendor_id: dev_info.vendor_id,
        device_id: dev_info.device_id,
        subsystem_vendor_id,
        subsystem_device_id,
        revision: dev_info.revision,
        class: dev_info.class,
        subclass: dev_info.subclass,
        prog_if: dev_info.prog_if,
        bar0,
        bars_decoded,
        msix,
        has_msi,
        interrupt_line_and_pin: root.interrupt_line_and_pin(bdf),
    }
}

/// Run the identify-only probe for one function, if it is one of ours.
///
/// Returns the report when the function matched the device table, so the
/// caller can decide what to do with the device; `None` means this function is
/// not this driver's.
pub(crate) fn probe(
    root: &mut axdriver_pci::PciRoot,
    bdf: axdriver_pci::DeviceFunction,
    dev_info: &axdriver_pci::DeviceFunctionInfo,
) -> Option<ProbeReport> {
    TALLY.functions.fetch_add(1, Ordering::Relaxed);
    let intel = dev_info.vendor_id == INTEL_VENDOR;
    if intel {
        TALLY.intel.fetch_add(1, Ordering::Relaxed);
    }
    let Some(device) = ids::identify(dev_info.vendor_id, dev_info.device_id) else {
        // An Intel network function with a device id this driver does not
        // bind is the most valuable line a machine without the assumed part
        // can produce: it says what is actually there.
        if intel && is_ethernet_class(dev_info.class, dev_info.subclass) {
            TALLY.candidates.fetch_add(1, Ordering::Relaxed);
            info!(
                "{}",
                igc::probe::candidate_line(&Candidate {
                    bdf: alloc::format!("{bdf}"),
                    vendor_id: dev_info.vendor_id,
                    device_id: dev_info.device_id,
                    class: dev_info.class,
                    subclass: dev_info.subclass,
                    revision: dev_info.revision,
                })
            );
        }
        return None;
    };

    let facts = config_facts(root, bdf, dev_info);
    info!("igc: {}: {}", facts.bdf, facts.describe());
    if !facts.bar0.holds_the_named_registers() {
        // Report the refusal without touching the aperture at all: mapping a
        // BAR that is unassigned or too small is not something a probe should
        // do to find out what happens.
        warn!(
            "igc: {}: refusing to map BAR0 ({}); the driver needs at least {} bytes",
            facts.bdf,
            facts.bar0.describe(),
            igc::regs::NAMED_SPAN,
        );
        let report = igc::probe::refusal(facts, device);
        info!("{}", report.render());
        note_match();
        return Some(report);
    }

    // The direct map is addressed by `usize`; on x86_64 a BAR address always
    // fits, and if it somehow does not, the aperture is not one this platform
    // can reach and saying so is better than truncating it.
    let Ok(address) = usize::try_from(facts.bar0.address) else {
        warn!(
            "igc: {}: BAR0 address {:#x} does not fit this platform's address size",
            facts.bdf, facts.bar0.address,
        );
        let report = igc::probe::refusal(facts, device);
        info!("{}", report.render());
        note_match();
        return Some(report);
    };

    // SAFETY: the BAR is memory, assigned, and at least as large as the window
    // this driver maps.  The platform's direct map covers the PCIe MMIO ranges
    // the platform profile declares as device memory, which is the same
    // mechanism the ixgbe driver uses for its own aperture.
    let mut bus = unsafe {
        WindowBus::<IgcHalImpl>::new(RegisterWindow::from_mapped(
            phys_to_virt(address.into()).into(),
            WINDOW_BYTES,
        ))
    };
    let bdf = facts.bdf.clone();
    let report = igc::probe::run(facts, device, &mut bus);
    let identified = matches!(report.verdict, igc::probe::Verdict::Identified);
    info!("{}", report.render());

    // Only a device the identification actually confirmed is programmed: a
    // function whose registers contradict its device id is a function this
    // driver does not understand well enough to reset.
    if identified {
        match igc::bringup::bring_up(&mut bus) {
            Ok(up) => info!("{}", up.render(&bdf)),
            Err(error) => warn!("igc: bring-up {bdf} failed: {}", error.describe()),
        }
    } else {
        warn!(
            "igc: {bdf}: not brought up: the identification did not confirm the device, so \
             nothing was programmed"
        );
    }
    note_match();
    Some(report)
}

/// Print the verdict for a machine where nothing matched, once, at the end of
/// the bus walk.
///
/// This is the line that makes the assumption cheap to refute: if the target
/// machine turns out to carry something else, its device id appears in the
/// candidate lines above this verdict.
pub(crate) fn finish_probe(bus_end: u8) {
    if TALLY.reported.swap(true, Ordering::SeqCst) {
        return;
    }
    let functions = TALLY.functions.load(Ordering::Relaxed);
    let intel = TALLY.intel.load(Ordering::Relaxed);
    let candidates = TALLY.candidates.load(Ordering::Relaxed);
    if TALLY.matched.load(Ordering::Relaxed) {
        info!(
            "igc: bus walk: {functions} PCI functions answered on buses 0..={bus_end}, {intel} \
             from Intel, at least one matching a device id this driver binds; the report above \
             is what that device said about itself"
        );
        return;
    }
    let text = igc::probe::absence_report(functions, bus_end, intel, candidates);
    info!("{text}");
}

/// Record that a function matched and was reported.
pub(crate) fn note_match() {
    TALLY.matched.store(true, Ordering::SeqCst);
}

/// Whether the class a function reports is an Ethernet controller.
pub(crate) const fn is_ethernet_class(class: u8, subclass: u8) -> bool {
    class == CLASS_NETWORK && subclass == SUBCLASS_ETHERNET
}
