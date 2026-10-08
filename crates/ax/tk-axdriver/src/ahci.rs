//! AHCI PCI frontend, mapping the controller into TheKernel's block registry.
//!
//! Translated/adapted from FreeBSD `sys/dev/ahci/ahci_pci.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2009-2012 Alexander Motin <mav@FreeBSD.org>
//!
//! The FreeBSD `ahci_pci_attach()` resource path is reduced to PCI BAR5, bus
//! mastering, controller reset, and a polling block device. Interrupt routing,
//! MSI/MSI-X, enclosure management, Intel remapped NVMe, and vendor-specific
//! quirk-table coverage are added in later AHCI stages.

pub mod pci_ids;

use core::{ptr, ptr::NonNull};

use axalloc::{UsageKind, global_allocator};
use axdriver_block::{
    BlockDriverOps,
    ahci::{
        AhciController, AhciDisk, AhciIo, DmaRegion, PortState, PortWorkspace,
        regs::{
            AHCI_MAX_PORTS, AHCI_OFFSET, AHCI_P_IE, AHCI_P_SSTS, AHCI_STEP, ATA_SS_DET_MASK,
            ATA_SS_DET_NO_DEVICE,
        },
    },
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use axhal::mem::virt_to_phys;
use log::{info, warn};

use crate::drivers::BusProbeResult;

const PCI_CLASS_STORAGE: u8 = 0x01;
const PCI_SUBCLASS_SATA: u8 = 0x06;
const PCI_SUBCLASS_RAID: u8 = 0x04;
const PCI_PROGIF_AHCI: u8 = 0x01;
const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 1 << 1;
const PCI_COMMAND_MASTER: u16 = 1 << 2;
const ABAR_BAR: u8 = 5;
const ABAR_MIN_BYTES: usize = AHCI_OFFSET + AHCI_MAX_PORTS * AHCI_STEP;
const BOUNCE_PAGES: usize = 16;

/// Driver state is concrete for static builds and type-erased by the block
/// device enum after PCI probe.
/// BAR-backed AHCI register window and platform delay implementation.
#[derive(Clone, Copy)]
pub struct Window {
    base: usize,
    size: usize,
}

impl AhciIo for Window {
    fn read32(&mut self, offset: usize) -> u32 {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > self.size) {
            return u32::MAX;
        }
        // SAFETY: the PCI frontend mapped the complete memory BAR and checked
        // every register access against the assigned aperture.
        unsafe { ((self.base + offset) as *const u32).read_volatile() }
    }

    fn write32(&mut self, offset: usize, value: u32) {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > self.size) {
            return;
        }
        // SAFETY: same bounded, aligned BAR aperture as `read32`.
        unsafe { ((self.base + offset) as *mut u32).write_volatile(value) };
    }

    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
}

/// Return an owned page-backed DMA region for command/FIS/data memory.
fn alloc_dma(pages: usize) -> Option<DmaRegion> {
    let virtual_address = global_allocator()
        .alloc_pages(pages, 4096, UsageKind::Dma)
        .ok()?;
    if virtual_address == 0 {
        global_allocator().dealloc_pages(virtual_address, pages, UsageKind::Dma);
        return None;
    }
    let Some(cpu) = NonNull::new(virtual_address as *mut u8) else {
        global_allocator().dealloc_pages(virtual_address, pages, UsageKind::Dma);
        return None;
    };
    let bus = virt_to_phys(virtual_address.into()).as_usize() as u64;
    // SAFETY: this allocator returns `pages` owned, page-aligned DMA pages.
    unsafe { ptr::write_bytes(cpu.as_ptr(), 0, pages * 4096) };
    // SAFETY: the callback frees this exact allocator-owned page range.
    Some(unsafe { DmaRegion::from_raw_parts(cpu, bus, pages * 4096, pages, free_dma) })
}

/// Release the platform DMA allocation after AHCI proves it is quiescent.
unsafe fn free_dma(cpu: NonNull<u8>, pages: usize) {
    global_allocator().dealloc_pages(cpu.as_ptr() as usize, pages, UsageKind::Dma);
}

fn allocate_workspace() -> Option<PortWorkspace> {
    let command_list = alloc_dma(1)?;
    let received_fis = alloc_dma(1)?;
    let command_table = alloc_dma(1)?;
    let bounce = alloc_dma(BOUNCE_PAGES)?;
    PortWorkspace::new(command_list, received_fis, command_table, bounce).ok()
}

/// FreeBSD `ahci_probe`/`ahci_pci_attach` adaptation: match an AHCI class
/// function, enable memory and bus mastering, reset the HBA, and publish the
/// first attached ATA disk as a TheKernel block device.
// upstream: ahci_pci.c ahci_pci_attach()
pub(crate) fn probe(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
) -> BusProbeResult {
    if info.class != PCI_CLASS_STORAGE {
        return BusProbeResult::NotMatched;
    }
    let id_quirk = pci_ids::identify(info.vendor_id, info.device_id, info.revision);
    let ahci_class = info.subclass == PCI_SUBCLASS_SATA && info.prog_if == PCI_PROGIF_AHCI;
    // FreeBSD also admits known AHCI controllers that advertise RAID class.
    if !ahci_class && !(info.subclass == PCI_SUBCLASS_RAID && id_quirk.is_some()) {
        return BusProbeResult::NotMatched;
    }
    let mut quirks = id_quirk.map_or(0, |entry| entry.quirks);
    let (subsystem_vendor, subsystem_device) = root.endpoint_subsystem_ids(bdf);
    if info.vendor_id == 0x197b
        && info.device_id == 0x2363
        && subsystem_vendor == 0x1043
        && subsystem_device == 0x81e4
    {
        quirks |= axdriver_block::ahci::regs::AHCI_Q_SATA1_UNIT0;
    }
    let bar_index = if quirks & axdriver_block::ahci::regs::AHCI_Q_ABAR0 != 0 {
        0
    } else {
        ABAR_BAR
    };
    let bar = match root.bar_info(bdf, bar_index) {
        Ok(BarInfo::Memory { address, size, .. })
            if address != 0 && size >= ABAR_MIN_BYTES as u32 =>
        {
            (address, size as usize)
        }
        _ => {
            warn!("ahci: {bdf} has no usable ABAR in BAR{bar_index}");
            return BusProbeResult::Claimed;
        }
    };
    let Some(command) = root.read_config_dword(bdf, PCI_COMMAND) else {
        return BusProbeResult::Claimed;
    };
    if !root.write_config_u16(
        bdf,
        PCI_COMMAND,
        command as u16 | PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER,
    ) {
        warn!("ahci: {bdf} could not enable memory/bus-master PCI command bits");
        return BusProbeResult::Claimed;
    }
    let Ok(mapped) = axklib::mem::iomap((bar.0 as usize).into(), bar.1) else {
        warn!("ahci: {bdf} could not map ABAR at {:#x}", bar.0);
        return BusProbeResult::Claimed;
    };
    let base = mapped.as_usize();
    let mut window = Window { base, size: bar.1 };
    let caps = window.read32(axdriver_block::ahci::regs::AHCI_CAP);
    let caps2 = window.read32(axdriver_block::ahci::regs::AHCI_CAP2);
    let mut controller = AhciController::new(window, caps, caps2, quirks);
    if let Err(error) = controller.ahci_ctlr_reset() {
        warn!("ahci: {bdf} HBA reset failed: {error:?}");
        return BusProbeResult::Claimed;
    }
    controller.ahci_attach(0);
    let implemented_ports = controller.implemented_ports;
    let channel_count = usize::from(controller.num_channels).min(AHCI_MAX_PORTS);
    info!(
        "ahci: {bdf} {:04x}:{:04x} {} quirks={quirks:#x} ports={implemented_ports:#010x} \
         channels={channel_count}",
        info.vendor_id,
        info.device_id,
        id_quirk.map_or("generic AHCI", |entry| entry.name),
    );

    let mut devices = alloc::vec::Vec::new();
    for index in 0..channel_count {
        if implemented_ports & (1 << index) == 0 {
            continue;
        }
        let port_base = AHCI_OFFSET + index * AHCI_STEP;
        controller.io_mut().write32(port_base + AHCI_P_IE, 0);
        let sstatus = controller.io_mut().read32(port_base + AHCI_P_SSTS);
        if sstatus & ATA_SS_DET_MASK == ATA_SS_DET_NO_DEVICE {
            continue;
        }
        let Some(workspace) = allocate_workspace() else {
            warn!("ahci: {bdf} port {index} DMA workspace allocation failed");
            continue;
        };
        let mut port = PortState::new(index as u8);
        port.quirks = quirks;
        if quirks & axdriver_block::ahci::regs::AHCI_Q_SATA1_UNIT0 != 0 && index == 0 {
            port.user_revision[0] = 1;
        }
        port.channel_capabilities = controller
            .io_mut()
            .read32(port_base + axdriver_block::ahci::regs::AHCI_P_CMD);
        let port_controller = AhciController::new(
            window,
            controller.capabilities,
            controller.capabilities2,
            quirks,
        );
        match AhciDisk::attach(port_controller, port, workspace) {
            Ok(disk) => {
                info!(
                    "ahci: {bdf} port {index} block_size={} blocks={} polling",
                    disk.block_size(),
                    disk.num_blocks()
                );
                #[cfg(feature = "dyn")]
                devices.push(crate::AxDeviceEnum::from_block(disk));
                #[cfg(not(feature = "dyn"))]
                devices.push(crate::AxDeviceEnum::Block(crate::StaticBlockDevice::Ahci(
                    alloc::boxed::Box::new(disk),
                )));
            }
            Err(error) => {
                warn!("ahci: {bdf} port {index} disk initialization failed: {error:?}");
            }
        }
    }
    if devices.is_empty() {
        BusProbeResult::Claimed
    } else {
        BusProbeResult::Devices(devices)
    }
}
