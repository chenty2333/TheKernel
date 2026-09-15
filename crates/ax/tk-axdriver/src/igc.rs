//! The platform half of the `igc` driver: mapping, waiting, and the three
//! phases that turn a PCI function into a NIC.
//!
//! The driver's logic lives in `axdriver_net::igc`, which has no architecture
//! underneath it and is therefore testable on the host.  This module is the
//! other half: the part that needs this platform's PCI layer, its physical
//! memory map, its DMA allocator and its clock.  Keeping the split sharp is
//! what lets the arithmetic, the register encodings, the descriptor layout and
//! the ring state machines be verified on a machine that has no such NIC.
//!
//! # The three phases, in order
//!
//! 1. **Identify.**  Read configuration space, map BAR0, read the registers
//!    that identify the part, and print one verdict.  Writes nothing.
//! 2. **Bring up.**  Reset the MAC, wait for the NVM auto-read, read the
//!    station address, ask the PHY to autonegotiate and wait for link.  Sets up
//!    no ring, so nothing can be sent or received yet.
//! 3. **Take over.**  Build the descriptor rings and hand the NIC to the bus.
//!
//! Each phase logs its own outcome before the next begins, so a machine that
//! fails in the middle says where it failed instead of going quiet -- which
//! matters on the target, where the screen is the only output there is.

use core::{
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axalloc::{UsageKind, global_allocator};
use axdriver_net::igc::{
    self, DMA_PAGE_BYTES, IgcHal, IgcNic, PhysAddr, WindowBus,
    ids::{self, INTEL_VENDOR},
    probe::{BarFacts, Candidate, ConfigFacts, MsixFacts},
    regs::{RegisterWindow, WINDOW_BYTES},
};
use axhal::mem::{phys_to_virt, virt_to_phys};
use log::*;

/// The queue size both rings are built with: the vendor driver's default of
/// 256 descriptors (`IGC_DEFAULT_TXD`/`IGC_DEFAULT_RXD`, `igc.h:442-447`).
pub(crate) const QUEUE_SIZE: usize = 256;

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

    fn dma_alloc(pages: usize) -> Option<(PhysAddr, NonNull<u8>)> {
        // The same primitive the virtio HAL uses for its buffers: pages from
        // the global allocator, tagged as DMA, whose physical address is the
        // direct map's answer.  Coherent because x86_64 with no IOMMU in the
        // way is coherent, which is what this kernel's DMA users already
        // assume.
        let virtual_address =
            global_allocator().alloc_pages(pages, DMA_PAGE_BYTES, UsageKind::Dma).ok()?;
        if virtual_address == 0 {
            return None;
        }
        let physical = virt_to_phys(virtual_address.into()).as_usize();
        // The allocator returns memory that may hold another user's data; a
        // descriptor ring whose first read is a stale length field is a ring
        // that reports packets nobody sent.
        // SAFETY: the region is `pages` pages long and owned by this
        // allocation.
        unsafe { ptr::write_bytes(virtual_address as *mut u8, 0, pages * DMA_PAGE_BYTES) };
        let pointer = NonNull::new(virtual_address as *mut u8)?;
        Some((physical as PhysAddr, pointer))
    }

    unsafe fn dma_dealloc(_address: PhysAddr, cpu: NonNull<u8>, pages: usize) {
        global_allocator().dealloc_pages(cpu.as_ptr() as usize, pages, UsageKind::Dma);
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

/// The driver's entry point: identify the function, bring it up, and -- when
/// both worked -- build the NIC.
///
/// A function this driver recognises is always `Claimed`, even when it could
/// not be driven: the driver has read and possibly programmed it, so no other
/// driver may be offered it afterwards.
pub(crate) fn probe_and_init(
    root: &mut axdriver_pci::PciRoot,
    bdf: axdriver_pci::DeviceFunction,
    dev_info: &axdriver_pci::DeviceFunctionInfo,
) -> crate::drivers::BusProbeResult {
    match probe(root, bdf, dev_info) {
        Some(Some(nic)) => {
            crate::drivers::BusProbeResult::Device(crate::AxDeviceEnum::from_net(nic))
        }
        Some(None) => crate::drivers::BusProbeResult::Claimed,
        None => crate::drivers::BusProbeResult::NotMatched,
    }
}

/// Run the three phases for one function, if it is one of ours.
///
/// The return value is `None` for a function that is not this driver's,
/// `Some(None)` for one that is but could not be driven, and `Some(Some(nic))`
/// for one that is now a usable interface.
fn probe(
    root: &mut axdriver_pci::PciRoot,
    bdf: axdriver_pci::DeviceFunction,
    dev_info: &axdriver_pci::DeviceFunctionInfo,
) -> Option<Option<IgcNic<IgcHalImpl, QUEUE_SIZE>>> {
    TALLY.functions.fetch_add(1, Ordering::Relaxed);
    let intel = dev_info.vendor_id == INTEL_VENDOR;
    if intel {
        TALLY.intel.fetch_add(1, Ordering::Relaxed);
    }
    let Some(device) = ids::identify(dev_info.vendor_id, dev_info.device_id) else {
        // An Intel network function with a device id this driver does not bind
        // is the most valuable line a machine without the assumed part can
        // produce: it says what is actually there.
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
    let bdf = facts.bdf.clone();
    info!("igc: {}: {}", facts.bdf, facts.describe());

    // Phase 1: identify.  A BAR that cannot hold the registers this driver
    // names is not mapped at all: a probe does not find out what happens next
    // by touching an aperture it has already decided it does not understand.
    if !facts.bar0.holds_the_named_registers() {
        warn!(
            "igc: {bdf}: refusing to map BAR0 ({}); the driver needs at least {} bytes",
            facts.bar0.describe(),
            igc::regs::NAMED_SPAN,
        );
        let report = igc::probe::refusal(facts, device);
        info!("{}", report.render());
        note_match();
        return Some(None);
    }
    // The direct map is addressed by `usize`; on x86_64 a BAR address always
    // fits, and if it somehow does not, the aperture is not one this platform
    // can reach.
    let Ok(address) = usize::try_from(facts.bar0.address) else {
        warn!(
            "igc: {bdf}: BAR0 address {:#x} does not fit this platform's address size",
            facts.bar0.address,
        );
        let report = igc::probe::refusal(facts, device);
        info!("{}", report.render());
        note_match();
        return Some(None);
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
    let report = igc::probe::run(facts, device, &mut bus);
    let identified = matches!(report.verdict, igc::probe::Verdict::Identified);
    info!("{}", report.render());
    note_match();
    if !identified {
        warn!(
            "igc: {bdf}: not brought up: the identification did not confirm the device, so \
             nothing was programmed"
        );
        return Some(None);
    }

    // Phase 2: bring the link up.  No packets yet, and the report says so.
    let up = match igc::bringup::bring_up(&mut bus) {
        Ok(up) => up,
        Err(error) => {
            warn!("igc: bring-up {bdf} failed: {}", error.describe());
            return Some(None);
        }
    };
    info!("{}", up.render(&bdf));

    // Phase 3: take the device over.
    let nic = match IgcNic::<IgcHalImpl, QUEUE_SIZE>::init(bus, &up.station) {
        Ok(nic) => nic,
        Err(error) => {
            warn!(
                "igc: {bdf}: the link is up but the descriptor rings could not be built: {error:?}"
            );
            return Some(None);
        }
    };
    info!(
        "igc: {bdf}: {QUEUE_SIZE} descriptors in each ring, station address {}; the interface is \
         ready, it polls, and it takes no interrupts",
        up.station.describe(),
    );
    Some(Some(nic))
}

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
             from Intel, at least one matching a device id this driver binds; the reports above \
             are what those devices said about themselves"
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
