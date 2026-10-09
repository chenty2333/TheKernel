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

use alloc::{string::String, sync::Arc};
use core::{
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};

#[cfg(feature = "shared-block")]
use axdriver_base::BaseDriverOps;

static NEXT_DISK_INDEX: AtomicUsize = AtomicUsize::new(0);
static HOTPLUG_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static HOTPLUG_PORTS: spin::Mutex<alloc::vec::Vec<HotplugPort>> =
    spin::Mutex::new(alloc::vec::Vec::new());

use axalloc::{UsageKind, global_allocator};
use axdriver_block::{
    BlockDriverOps,
    ahci::{
        AhciController, AhciDisk, AhciIo, AhciPmpTargetDisk, DmaRegion, PortState, PortWorkspace,
        regs::{
            AHCI_CAP_SPM, AHCI_MAX_PORTS, AHCI_OFFSET, AHCI_P_IE, AHCI_P_SIG, AHCI_P_SSTS,
            AHCI_STEP, ATA_SS_DET_MASK, ATA_SS_DET_NO_DEVICE,
        },
    },
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use axhal::mem::virt_to_phys;
use log::{info, warn};

use crate::{block_irq::PciBlockInterrupt, drivers::BusProbeResult};

const PCI_CLASS_STORAGE: u8 = 0x01;
const PCI_SUBCLASS_SATA: u8 = 0x06;
const PCI_SUBCLASS_RAID: u8 = 0x04;
const PCI_PROGIF_AHCI: u8 = 0x01;
const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 1 << 1;
const PCI_COMMAND_MASTER: u16 = 1 << 2;
const ABAR_BAR: u8 = 5;
// BAR5 need only cover the HBA header and port zero. Many conforming HBAs
// expose fewer than the architectural maximum of 32 ports in their MMIO size.
const ABAR_MIN_BYTES: usize = AHCI_OFFSET + AHCI_STEP;
const BOUNCE_PAGES: usize = 16;
const ATA_PORT_MULTIPLIER_SIGNATURE: u32 = 0x9669_0101;

/// Driver state is concrete for static builds and type-erased by the block
/// device enum after PCI probe.
/// BAR-backed AHCI register window and platform delay implementation.
struct AhciIrqContext {
    base: usize,
    size: usize,
    port_status: [AtomicU32; AHCI_MAX_PORTS],
}

// upstream: ahci.c ahci_intr() AHCI_IRQ_MODE_ALL controller status routing
fn ack_ahci_interrupt(context: usize) -> bool {
    // SAFETY: the frontend leaks the immutable BAR context for the endpoint's
    // boot lifetime and stores only this pointer in its static IRQ slot.
    let context = unsafe { &*(context as *const AhciIrqContext) };
    let read = |offset: usize| {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > context.size) {
            u32::MAX
        } else {
            // SAFETY: the mapped BAR bounds were checked and this is an aligned MMIO dword.
            unsafe { ((context.base + offset) as *const u32).read_volatile() }
        }
    };
    let write = |offset: usize, value: u32| {
        if offset & 3 == 0 && offset.checked_add(4).is_some_and(|end| end <= context.size) {
            // SAFETY: same validated BAR aperture; AHCI PxIS/IS are W1C status registers.
            unsafe { ((context.base + offset) as *mut u32).write_volatile(value) };
        }
    };
    let pending = read(axdriver_block::ahci::regs::AHCI_IS);
    if pending == 0 || pending == u32::MAX {
        return false;
    }
    for port in 0..AHCI_MAX_PORTS {
        if pending & (1 << port) == 0 {
            continue;
        }
        let base = AHCI_OFFSET + port * AHCI_STEP;
        let status = read(base + axdriver_block::ahci::regs::AHCI_P_IS);
        if status != 0 && status != u32::MAX {
            context.port_status[port].fetch_or(status, Ordering::AcqRel);
            write(base + axdriver_block::ahci::regs::AHCI_P_IS, status);
        }
    }
    write(axdriver_block::ahci::regs::AHCI_IS, pending);
    true
}

#[derive(Clone)]
pub struct Window {
    base: usize,
    size: usize,
    irq: Option<PciBlockInterrupt>,
    irq_context: Option<&'static AhciIrqContext>,
}

struct HotplugPort {
    window: Window,
    index: u8,
    quirks: u32,
    capabilities: u32,
    capabilities2: u32,
    present: bool,
    name: Option<String>,
}

impl AhciIo for Window {
    fn read32(&mut self, offset: usize) -> u32 {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > self.size) {
            return u32::MAX;
        }
        // SAFETY: the PCI frontend mapped the complete memory BAR and checked
        // every register access against the assigned aperture.
        let mut value = unsafe { ((self.base + offset) as *const u32).read_volatile() };
        if let Some(context) = self.irq_context {
            let relative = offset.checked_sub(AHCI_OFFSET);
            if let Some(relative) = relative {
                let port = relative / AHCI_STEP;
                if port < AHCI_MAX_PORTS
                    && relative % AHCI_STEP == axdriver_block::ahci::regs::AHCI_P_IS
                {
                    value |= context.port_status[port].swap(0, Ordering::AcqRel);
                }
            }
        }
        value
    }

    fn write32(&mut self, offset: usize, value: u32) {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > self.size) {
            return;
        }
        // SAFETY: same bounded, aligned BAR aperture as `read32`.
        unsafe { ((self.base + offset) as *mut u32).write_volatile(value) };
        if let Some(context) = self.irq_context {
            if let Some(relative) = offset.checked_sub(AHCI_OFFSET) {
                let port = relative / AHCI_STEP;
                if port < AHCI_MAX_PORTS
                    && relative % AHCI_STEP == axdriver_block::ahci::regs::AHCI_P_IS
                {
                    context.port_status[port].fetch_and(!value, Ordering::AcqRel);
                }
            }
        }
    }

    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }

    fn has_interrupt(&self) -> bool {
        self.irq.is_some()
    }

    fn interrupt_generation(&self) -> Option<u64> {
        self.irq.as_ref().map(PciBlockInterrupt::generation)
    }

    fn wait_for_interrupt(&mut self, observed: u64, timeout_us: u64) {
        if let Some(irq) = &self.irq {
            let _ = irq.wait_for_generation(observed, timeout_us);
        } else {
            self.delay_us(timeout_us.min(u64::from(u32::MAX)) as u32);
        }
    }

    fn install_completion_notifier(
        &mut self,
        notifier: Option<axdriver_block::BlockCompletionNotifier>,
        context: usize,
    ) -> bool {
        self.irq
            .as_ref()
            .is_some_and(|irq| irq.install_completion_notifier(notifier, context))
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
    let command_table = alloc_dma(axdriver_block::ahci::regs::AHCI_MAX_SLOTS)?;
    let bounce = alloc_dma(BOUNCE_PAGES)?;
    PortWorkspace::new(command_list, received_fis, command_table, bounce).ok()
}

fn next_sd_name() -> String {
    let mut index = NEXT_DISK_INDEX.fetch_add(1, Ordering::Relaxed);
    let mut letters = [0u8; 8];
    let mut len = 0;
    loop {
        letters[len] = b'a' + (index % 26) as u8;
        len += 1;
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    let mut name = String::from("sd");
    for letter in letters[..len].iter().rev() {
        name.push(*letter as char);
    }
    name
}

fn wrap_disk(disk: AhciDisk<Window>) -> crate::AxBlockDevice {
    #[cfg(feature = "dyn")]
    let raw: crate::AxBlockDevice = alloc::boxed::Box::new(disk);
    #[cfg(not(feature = "dyn"))]
    let raw: crate::AxBlockDevice = crate::StaticBlockDevice::Ahci(alloc::boxed::Box::new(disk));
    raw
}

fn wrap_pmp_disk(disk: AhciPmpTargetDisk<Window>) -> crate::AxBlockDevice {
    #[cfg(feature = "dyn")]
    let raw: crate::AxBlockDevice = alloc::boxed::Box::new(disk);
    #[cfg(not(feature = "dyn"))]
    let raw: crate::AxBlockDevice = crate::StaticBlockDevice::Ahci(alloc::boxed::Box::new(disk));
    raw
}

fn publish_disk(
    mut disk: AhciDisk<Window>,
    devices: &mut alloc::vec::Vec<crate::AxDeviceEnum>,
) -> String {
    let name = next_sd_name();
    disk.set_device_name(name.clone());
    publish_named_block(wrap_disk(disk), &name, devices);
    name
}

fn publish_named_block(
    raw: crate::AxBlockDevice,
    name: &str,
    devices: &mut alloc::vec::Vec<crate::AxDeviceEnum>,
) {
    #[cfg(feature = "shared-block")]
    {
        let parent = crate::SharedBlockDevice::new(raw);
        #[cfg(feature = "dyn")]
        devices.push(crate::AxDeviceEnum::from_block(parent.clone()));
        #[cfg(not(feature = "dyn"))]
        devices.push(crate::AxDeviceEnum::Block(crate::StaticBlockDevice::Ahci(
            alloc::boxed::Box::new(parent.clone()),
        )));
        match crate::discover_gpt_partitions(&parent, &name, false) {
            Ok(partitions) => {
                for partition in partitions {
                    info!("ahci: published GPT partition {}", partition.device_name());
                    devices.push(crate::AxDeviceEnum::Block(partition));
                }
            }
            Err(error) => log::debug!("ahci: no accepted GPT on {name}: {error:?}"),
        }
    }
    #[cfg(not(feature = "shared-block"))]
    {
        #[cfg(feature = "dyn")]
        devices.push(crate::AxDeviceEnum::Block(raw));
        #[cfg(not(feature = "dyn"))]
        devices.push(crate::AxDeviceEnum::Block(raw));
    }
}

fn discover_pmp_targets(
    disk: AhciDisk<Window>,
) -> alloc::vec::Vec<(AhciPmpTargetDisk<Window>, String)> {
    let shared = Arc::new(spin::Mutex::new(disk));
    let mut targets = alloc::vec::Vec::new();
    for pmp_port in 0..15u8 {
        let identified = shared.lock().probe_pmp_target(pmp_port).ok();
        let Some((geometry, identity)) = identified else {
            continue;
        };
        if geometry.blocks == 0 {
            continue;
        }
        let name = next_sd_name();
        targets.push((
            AhciPmpTargetDisk::new(shared.clone(), pmp_port, geometry, identity, name.clone()),
            name,
        ));
    }
    targets
}

fn start_hotplug_worker() {
    if HOTPLUG_WORKER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = axtask::spawn_raw(
        ahci_hotplug_worker,
        "ahci_hotplug".into(),
        axconfig::TASK_STACK_SIZE,
    ) {
        HOTPLUG_WORKER_STARTED.store(false, Ordering::Release);
        warn!("ahci: hotplug worker unavailable: {error:?}");
    }
}

// upstream: ahci.c ahci_phy_check_events() + ahci_cpd_check_events() + ahci_notify_events()
fn ahci_hotplug_worker() {
    loop {
        {
            let mut ports = HOTPLUG_PORTS.lock();
            for port in ports.iter_mut() {
                let register = AHCI_OFFSET + usize::from(port.index) * AHCI_STEP + AHCI_P_SSTS;
                let online = port.window.read32(register) & ATA_SS_DET_MASK
                    == axdriver_block::ahci::regs::ATA_SS_DET_PHY_ONLINE;
                if !online {
                    port.present = false;
                    continue;
                }
                if port.present {
                    continue;
                }
                let Some(workspace) = allocate_workspace() else {
                    continue;
                };
                let mut state = PortState::new(port.index);
                state.quirks = port.quirks;
                if port.quirks & axdriver_block::ahci::regs::AHCI_Q_SATA1_UNIT0 != 0
                    && port.index == 0
                {
                    state.user_revision[0] = 1;
                }
                if port.quirks & axdriver_block::ahci::regs::AHCI_Q_SATA2 != 0 {
                    state.user_revision[0] = 2;
                }
                state.channel_capabilities = port.window.read32(
                    AHCI_OFFSET
                        + usize::from(port.index) * AHCI_STEP
                        + axdriver_block::ahci::regs::AHCI_P_CMD,
                );
                let controller = AhciController::new(
                    port.window.clone(),
                    port.capabilities,
                    port.capabilities2,
                    port.quirks,
                );
                let signature = port
                    .window
                    .read32(AHCI_OFFSET + usize::from(port.index) * AHCI_STEP + AHCI_P_SIG);
                if signature == ATA_PORT_MULTIPLIER_SIGNATURE
                    && port.capabilities & AHCI_CAP_SPM != 0
                {
                    let Ok(engine) = AhciDisk::attach_pmp_controller(controller, state, workspace)
                    else {
                        continue;
                    };
                    let mut first_name = None;
                    for (target, name) in discover_pmp_targets(engine) {
                        if crate::publish_runtime_block_device(wrap_pmp_disk(target)) {
                            info!(
                                "ahci: hotplug published PMP target /dev/{name} on port {}",
                                port.index
                            );
                            if first_name.is_none() {
                                first_name = Some(name);
                            }
                        }
                    }
                    if let Some(name) = first_name {
                        port.name = Some(name);
                        port.present = true;
                    }
                    continue;
                }
                let Ok(mut disk) = AhciDisk::attach(controller, state, workspace) else {
                    continue;
                };
                let name = port.name.get_or_insert_with(next_sd_name).clone();
                disk.set_device_name(name.clone());
                if crate::publish_runtime_block_device(wrap_disk(disk)) {
                    port.present = true;
                    info!("ahci: hotplug published /dev/{name} on port {}", port.index);
                }
            }
        }
        let _ = axtask::sleep(core::time::Duration::from_millis(500));
    }
}

/// FreeBSD `ahci_probe`/`ahci_pci_attach` adaptation: match an AHCI class
/// function, enable memory and bus mastering, reset the HBA, and publish the
/// first attached ATA disk as a TheKernel block device.
// upstream: ahci_pci.c ahci_probe() + ahci_pci_attach()
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
    let force_ahci = axhal::boot::command_line_value("ahci.force_ahci") == Some("1");
    if !ahci_class
        && !(info.subclass == PCI_SUBCLASS_RAID && id_quirk.is_some())
        && !(force_ahci
            && id_quirk.is_some_and(|entry| {
                entry.quirks & axdriver_block::ahci::regs::AHCI_Q_NOFORCE == 0
            }))
    {
        return BusProbeResult::NotMatched;
    }
    let mut quirks = id_quirk.map_or(0, |entry| entry.quirks);
    let (subsystem_vendor, subsystem_device) = root.endpoint_subsystem_ids(bdf);
    if info.vendor_id == 0x197b
        && id_quirk
            .is_some_and(|entry| entry.quirks & axdriver_block::ahci::regs::AHCI_Q_NOFORCE != 0)
        && root
            .read_config_dword(bdf, 0xdc)
            .is_some_and(|value| ((value >> 24) as u8 & 0x40) == 0)
    {
        return BusProbeResult::NotMatched;
    }
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
    // Clear firmware-left interrupt masks before admitting a PCI IRQ route.
    let ghc =
        unsafe { ((base + axdriver_block::ahci::regs::AHCI_GHC) as *const u32).read_volatile() };
    unsafe {
        ((base + axdriver_block::ahci::regs::AHCI_GHC) as *mut u32)
            .write_volatile(ghc & !axdriver_block::ahci::regs::AHCI_GHC_IE);
        for port in 0..AHCI_MAX_PORTS {
            let offset = AHCI_OFFSET + port * AHCI_STEP + AHCI_P_IE;
            if offset.checked_add(4).is_some_and(|end| end <= bar.1) {
                ((base + offset) as *mut u32).write_volatile(0);
            }
        }
    }
    let irq_context = alloc::boxed::Box::leak(alloc::boxed::Box::new(AhciIrqContext {
        base,
        size: bar.1,
        port_status: [const { AtomicU32::new(0) }; AHCI_MAX_PORTS],
    }));
    let irq = PciBlockInterrupt::register(
        root,
        bdf,
        irq_context as *const _ as usize,
        ack_ahci_interrupt,
    );
    if let Some(irq) = &irq {
        info!(
            "ahci: {bdf} completion IRQ mode={:?} vector={:#x}",
            irq.mode(),
            irq.vector()
        );
    } else {
        warn!("ahci: {bdf} no MSI-X/MSI/INTx route; retaining polling fallback");
    }
    let mut window = Window {
        base,
        size: bar.1,
        irq,
        irq_context: Some(irq_context),
    };
    let caps = window.read32(axdriver_block::ahci::regs::AHCI_CAP);
    let caps2 = window.read32(axdriver_block::ahci::regs::AHCI_CAP2);
    let mut controller = AhciController::new(window.clone(), caps, caps2, quirks);
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
    let mut hotplug_ports = alloc::vec::Vec::new();
    for index in 0..channel_count {
        if implemented_ports & (1 << index) == 0 {
            continue;
        }
        let port_base = AHCI_OFFSET + index * AHCI_STEP;
        if port_base
            .checked_add(AHCI_STEP)
            .is_none_or(|end| end > window.size)
        {
            warn!("ahci: {bdf} port {index} lies outside the mapped ABAR");
            continue;
        }
        controller.io_mut().write32(port_base + AHCI_P_IE, 0);
        let sstatus = controller.io_mut().read32(port_base + AHCI_P_SSTS);
        if sstatus & ATA_SS_DET_MASK == ATA_SS_DET_NO_DEVICE {
            hotplug_ports.push(HotplugPort {
                window: window.clone(),
                index: index as u8,
                quirks,
                capabilities: controller.capabilities,
                capabilities2: controller.capabilities2,
                present: false,
                name: None,
            });
            continue;
        }
        let Some(workspace) = allocate_workspace() else {
            warn!("ahci: {bdf} port {index} DMA workspace allocation failed");
            hotplug_ports.push(HotplugPort {
                window: window.clone(),
                index: index as u8,
                quirks,
                capabilities: controller.capabilities,
                capabilities2: controller.capabilities2,
                present: false,
                name: None,
            });
            continue;
        };
        let mut port = PortState::new(index as u8);
        port.quirks = quirks;
        if quirks & axdriver_block::ahci::regs::AHCI_Q_SATA1_UNIT0 != 0 && index == 0 {
            port.user_revision[0] = 1;
        }
        if quirks & axdriver_block::ahci::regs::AHCI_Q_SATA2 != 0 {
            port.user_revision[0] = 2;
        }
        port.channel_capabilities = controller
            .io_mut()
            .read32(port_base + axdriver_block::ahci::regs::AHCI_P_CMD);
        let port_controller = AhciController::new(
            window.clone(),
            controller.capabilities,
            controller.capabilities2,
            quirks,
        );
        let signature = window.read32(port_base + AHCI_P_SIG);
        if signature == ATA_PORT_MULTIPLIER_SIGNATURE && controller.capabilities & AHCI_CAP_SPM != 0
        {
            let Ok(engine) = AhciDisk::attach_pmp_controller(port_controller, port, workspace)
            else {
                warn!("ahci: {bdf} port {index} PMP engine initialization failed");
                hotplug_ports.push(HotplugPort {
                    window: window.clone(),
                    index: index as u8,
                    quirks,
                    capabilities: controller.capabilities,
                    capabilities2: controller.capabilities2,
                    present: false,
                    name: None,
                });
                continue;
            };
            let mut first_name = None;
            for (target, name) in discover_pmp_targets(engine) {
                info!(
                    "ahci: {bdf} port {index} PMP target {} blocks={}",
                    target.pmp_port(),
                    target.num_blocks()
                );
                publish_named_block(wrap_pmp_disk(target), &name, &mut devices);
                if first_name.is_none() {
                    first_name = Some(name);
                }
            }
            hotplug_ports.push(HotplugPort {
                window: window.clone(),
                index: index as u8,
                quirks,
                capabilities: controller.capabilities,
                capabilities2: controller.capabilities2,
                present: first_name.is_some(),
                name: first_name,
            });
            continue;
        }
        match AhciDisk::attach(port_controller, port, workspace) {
            Ok(disk) => {
                info!(
                    "ahci: {bdf} port {index} block_size={} blocks={}",
                    disk.block_size(),
                    disk.num_blocks()
                );
                let name = publish_disk(disk, &mut devices);
                hotplug_ports.push(HotplugPort {
                    window: window.clone(),
                    index: index as u8,
                    quirks,
                    capabilities: controller.capabilities,
                    capabilities2: controller.capabilities2,
                    present: true,
                    name: Some(name),
                });
            }
            Err(error) => {
                warn!("ahci: {bdf} port {index} disk initialization failed: {error:?}");
                hotplug_ports.push(HotplugPort {
                    window: window.clone(),
                    index: index as u8,
                    quirks,
                    capabilities: controller.capabilities,
                    capabilities2: controller.capabilities2,
                    present: false,
                    name: None,
                });
            }
        }
    }
    HOTPLUG_PORTS.lock().extend(hotplug_ports);
    start_hotplug_worker();
    if devices.is_empty() {
        BusProbeResult::Claimed
    } else {
        BusProbeResult::Devices(devices)
    }
}
