//! PCI binding for the SDHCI/MMC block path.
//!
//! Adapted from FreeBSD `sys/dev/sdhci/sdhci_pci.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2008 Alexander Motin <mav@FreeBSD.org>.
//! SPDX-License-Identifier: BSD-2-Clause

use core::{
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering, fence},
};

use axalloc::{UsageKind, global_allocator};
use axdriver_base::BaseDriverOps;
use axdriver_block::sdhci::{
    SDHCI_CAPABILITIES, SDHCI_CAPABILITIES2, SDHCI_HOST_VERSION, SDHCI_SPEC_VER_MASK, SdhciDisk,
    SdhciDmaRegion, SdhciHost, SdhciIo, SdhciPartitionDisk,
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use axhal::mem::virt_to_phys;
use log::{info, warn};

use crate::block_irq::PciBlockInterrupt;

const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 0x0002;
const PCI_COMMAND_MASTER: u16 = 0x0004;
const PCI_CLASS_SYSTEM_PERIPHERAL: u8 = 0x08;
const PCI_SUBCLASS_SD_HOST: u8 = 0x05;
const PCI_SLOT_INFO: u8 = 0x40;
const SDHCI_BAR_MIN_BYTES: usize = 0x100;
const INTEL_EMMC_VID: u16 = 0x8086;
const INTEL_EMMC_DID: u16 = 0x54c4;
const QEMU_SDHCI_VID: u16 = 0x1b36;
const QEMU_SDHCI_DID: u16 = 0x0007;
const SDMA_BUFFER_BYTES: usize = 512 * 1024;
const SDMA_BUFFER_PAGES: usize = SDMA_BUFFER_BYTES / 4096;
static SDHCI_HOTPLUG_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static SDHCI_HOTPLUG_SLOTS: spin::Mutex<alloc::vec::Vec<SdhciHotplugSlot>> =
    spin::Mutex::new(alloc::vec::Vec::new());
static NEXT_RUNTIME_MMC_INDEX: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct SdhciPciId {
    id: u32,
    quirks: u32,
}

// FreeBSD sys/dev/sdhci/sdhci_pci.c sdhci_devices[]; descriptions are omitted
// because the PCI core owns display naming in TheKernel.
const SDHCI_PCI_IDS: &[SdhciPciId] = &[
    SdhciPciId {
        id: 0x0822_1180,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_FORCE_DMA,
    },
    SdhciPciId {
        id: 0xe822_1180,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_FORCE_DMA
            | axdriver_block::sdhci::SDHCI_QUIRK_LOWER_FREQUENCY,
    },
    SdhciPciId {
        id: 0xe823_1180,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_LOWER_FREQUENCY,
    },
    SdhciPciId {
        id: 0x8034_104c,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_FORCE_DMA,
    },
    SdhciPciId {
        id: 0x803c_104c,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_FORCE_DMA
            | axdriver_block::sdhci::SDHCI_QUIRK_WAITFOR_RESET_ASSERTED,
    },
    SdhciPciId {
        id: 0x0550_1524,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_TIMINGS,
    },
    SdhciPciId {
        id: 0x0551_1524,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_TIMINGS,
    },
    SdhciPciId {
        id: 0x0750_1524,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_RESET_ON_IOS
            | axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_TIMINGS,
    },
    SdhciPciId {
        id: 0x0751_1524,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_RESET_ON_IOS
            | axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_TIMINGS,
    },
    SdhciPciId {
        id: 0x4101_11ab,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_INCR_TIMEOUT_CONTROL,
    },
    SdhciPciId {
        id: 0x2381_197b,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_32BIT_DMA_SIZE
            | axdriver_block::sdhci::SDHCI_QUIRK_RESET_AFTER_REQUEST,
    },
    SdhciPciId {
        id: 0x16bc_14e4,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_BCM577XX_400KHZ_CLKSRC,
    },
    SdhciPciId {
        id: 0x0f14_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_INTEL_POWER_UP_RESET
            | axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
    SdhciPciId {
        id: 0x0f15_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
    SdhciPciId {
        id: 0x0f50_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_INTEL_POWER_UP_RESET
            | axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
    SdhciPciId {
        id: 0x19db_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_INTEL_POWER_UP_RESET
            | axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_MMC_DDR52
            | axdriver_block::sdhci::SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
    SdhciPciId {
        id: 0x2294_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_DATA_TIMEOUT_1MHZ
            | axdriver_block::sdhci::SDHCI_QUIRK_INTEL_POWER_UP_RESET
            | axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_MMC_DDR52
            | axdriver_block::sdhci::SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
    SdhciPciId {
        id: 0x2296_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
    SdhciPciId {
        id: 0x5aca_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_DMA
            | axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_SLOTTYPE_BROKEN
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
    SdhciPciId {
        id: 0x5acc_8086,
        quirks: axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_DMA
            | axdriver_block::sdhci::SDHCI_QUIRK_INTEL_POWER_UP_RESET
            | axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
            | axdriver_block::sdhci::SDHCI_QUIRK_MMC_DDR52
            | axdriver_block::sdhci::SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400
            | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN,
    },
];

// upstream: sdhci_pci.c sdhci_devices[] match
fn quirks_for_device(vendor_id: u16, device_id: u16) -> u32 {
    let id = (u32::from(device_id) << 16) | u32::from(vendor_id);
    let upstream = SDHCI_PCI_IDS
        .iter()
        .find(|entry| entry.id == id)
        .map_or(0, |entry| entry.quirks);
    if vendor_id == QEMU_SDHCI_VID && device_id == QEMU_SDHCI_DID {
        // QEMU 1b36:0007 advertises DMA but CMD17 fails to reach the card with
        // the current SDMA path. Keep this known virtual model on PIO so the
        // storage acceptance remains deterministic; physical controllers use
        // SDMA when they advertise it and have no broken-DMA quirk.
        upstream | axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_DMA
    } else {
        upstream
    }
}

// PCI_SLOT_INFO_SLOTS()/PCI_SLOT_INFO_FIRST_BAR() macros from sdhci_pci.c.
fn decode_slot_info(slot_info: u8) -> (usize, u8) {
    if slot_info == u8::MAX || slot_info & 0x07 > 5 {
        // Some SDHCI PCI functions omit the legacy slot-info register and
        // return all ones; their architected single slot is BAR0.
        return (1, 0);
    }
    (
        usize::from(((slot_info >> 4) & 0x07) + 1).min(6),
        slot_info & 0x07,
    )
}

// upstream: sdhci.c sdhci_dma_alloc()
fn allocate_dma_buffer() -> Option<SdhciDmaRegion> {
    let virtual_address = global_allocator()
        .alloc_pages(SDMA_BUFFER_PAGES, SDMA_BUFFER_BYTES, UsageKind::Dma)
        .ok()?;
    if virtual_address == 0 {
        global_allocator().dealloc_pages(virtual_address, SDMA_BUFFER_PAGES, UsageKind::Dma);
        return None;
    }
    let Some(cpu) = NonNull::new(virtual_address as *mut u8) else {
        global_allocator().dealloc_pages(virtual_address, SDMA_BUFFER_PAGES, UsageKind::Dma);
        return None;
    };
    let bus = virt_to_phys(virtual_address.into()).as_usize() as u64;
    if bus & (SDMA_BUFFER_BYTES as u64 - 1) != 0
        || bus + SDMA_BUFFER_BYTES as u64 > u64::from(u32::MAX) + 1
    {
        global_allocator().dealloc_pages(virtual_address, SDMA_BUFFER_PAGES, UsageKind::Dma);
        return None;
    }
    // SAFETY: pages are exclusively owned, coherent DMA memory.
    unsafe { ptr::write_bytes(cpu.as_ptr(), 0, SDMA_BUFFER_BYTES) };
    // SAFETY: host owns the exact DMA allocation and releases it after teardown.
    Some(unsafe {
        SdhciDmaRegion::from_raw_parts(
            cpu,
            bus,
            SDMA_BUFFER_BYTES,
            SDMA_BUFFER_PAGES,
            Some(free_sdma_buffer),
        )
    })
}

fn dma_supported(capabilities: u32, quirks: u32) -> bool {
    capabilities
        & (axdriver_block::sdhci::SDHCI_CAN_DO_DMA | axdriver_block::sdhci::SDHCI_CAN_DO_ADMA2)
        != 0
        || quirks & axdriver_block::sdhci::SDHCI_QUIRK_FORCE_DMA != 0
}

unsafe fn free_sdma_buffer(cpu: NonNull<u8>, pages: usize) {
    global_allocator().dealloc_pages(cpu.as_ptr() as usize, pages, UsageKind::Dma);
}

struct SdhciIrqContext {
    base: usize,
    size: usize,
    pending: AtomicU32,
}

fn ack_sdhci_interrupt(context: usize) -> bool {
    // SAFETY: the PCI frontend leaks this immutable BAR/status context for the
    // lifetime of the fixed PCI interrupt endpoint.
    let context = unsafe { &*(context as *const SdhciIrqContext) };
    let offset = axdriver_block::sdhci::SDHCI_INT_STATUS as usize;
    if offset.checked_add(4).is_none_or(|end| end > context.size) {
        return false;
    }
    // SAFETY: the status register is an aligned dword within the mapped BAR.
    let status = unsafe { ((context.base + offset) as *const u32).read_volatile() };
    if status == 0 || status == u32::MAX {
        return false;
    }
    context.pending.fetch_or(status, Ordering::AcqRel);
    // SAFETY: SDHCI_INT_STATUS is write-one-to-clear.
    unsafe { ((context.base + offset) as *mut u32).write_volatile(status) };
    true
}

#[derive(Clone)]
struct SdhciWindow {
    base: NonNull<u8>,
    size: usize,
    irq: Option<PciBlockInterrupt>,
    irq_context: Option<&'static SdhciIrqContext>,
}

struct SdhciHotplugSlot {
    io: SdhciWindow,
    capabilities: u32,
    capabilities2: u32,
    version: u8,
    quirks: u32,
    vendor_id: u16,
    device_id: u16,
    disk_index: usize,
    present: bool,
}

unsafe impl Send for SdhciWindow {}
unsafe impl Sync for SdhciWindow {}

impl SdhciWindow {
    fn read<T: Copy>(&mut self, offset: usize) -> T {
        assert!(
            offset
                .checked_add(core::mem::size_of::<T>())
                .is_some_and(|end| end <= self.size)
        );
        fence(Ordering::SeqCst);
        // SAFETY: `base` is a mapped PCI BAR and register widths/alignment follow SDHCI.
        let value = unsafe { ptr::read_volatile(self.base.as_ptr().add(offset).cast::<T>()) };
        fence(Ordering::SeqCst);
        value
    }

    fn write<T: Copy>(&mut self, offset: usize, value: T) {
        assert!(
            offset
                .checked_add(core::mem::size_of::<T>())
                .is_some_and(|end| end <= self.size)
        );
        fence(Ordering::SeqCst);
        // SAFETY: `base` is a mapped PCI BAR and register widths/alignment follow SDHCI.
        unsafe { ptr::write_volatile(self.base.as_ptr().add(offset).cast::<T>(), value) };
        fence(Ordering::SeqCst);
    }
}

impl SdhciIo for SdhciWindow {
    // upstream: sdhci_pci.c sdhci_pci_read_1()
    fn read8(&mut self, offset: usize) -> u8 {
        self.read(offset)
    }
    // upstream: sdhci_pci.c sdhci_pci_read_2()
    fn read16(&mut self, offset: usize) -> u16 {
        self.read(offset)
    }
    // upstream: sdhci_pci.c sdhci_pci_read_4()
    fn read32(&mut self, offset: usize) -> u32 {
        let mut value = self.read(offset);
        if offset == axdriver_block::sdhci::SDHCI_INT_STATUS as usize {
            if let Some(context) = self.irq_context {
                value |= context.pending.swap(0, Ordering::AcqRel);
            }
        }
        value
    }
    // upstream: sdhci_pci.c sdhci_pci_write_1()
    fn write8(&mut self, offset: usize, value: u8) {
        self.write(offset, value)
    }
    // upstream: sdhci_pci.c sdhci_pci_write_2()
    fn write16(&mut self, offset: usize, value: u16) {
        self.write(offset, value)
    }
    // upstream: sdhci_pci.c sdhci_pci_write_4()
    fn write32(&mut self, offset: usize, value: u32) {
        self.write(offset, value);
        if offset == axdriver_block::sdhci::SDHCI_INT_STATUS as usize {
            if let Some(context) = self.irq_context {
                context.pending.fetch_and(!value, Ordering::AcqRel);
            }
        }
    }
    // upstream: mmc.c mmc_ms_delay()
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
    // upstream: sdhci_pci.c sdhci_pci_read_multi_4()
    fn read_multi32(&mut self, offset: usize, values: &mut [u32]) {
        for value in values {
            *value = self.read32(offset);
        }
    }
    // upstream: sdhci_pci.c sdhci_pci_write_multi_4()
    fn write_multi32(&mut self, offset: usize, values: &[u32]) {
        for value in values {
            self.write32(offset, *value);
        }
    }
}

/// Attach one FreeBSD PCI slot and publish its user/boot areas.
// upstream: sdhci_pci.c sdhci_pci_attach() per-slot body
// upstream: sdhci.c sdhci_init_slot()
fn probe_slot(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
    bar_index: u8,
    irq_index: usize,
    disk_index: usize,
    quirks: u32,
    read_only: bool,
) -> (
    alloc::vec::Vec<crate::AxDeviceEnum>,
    Option<SdhciHotplugSlot>,
) {
    let (address, size) = match root.bar_info(bdf, bar_index) {
        Ok(BarInfo::Memory { address, size, .. })
            if address != 0 && size as usize >= SDHCI_BAR_MIN_BYTES =>
        {
            (address, size as usize)
        }
        _ => {
            warn!("sdhci: {bdf} has no usable MMIO BAR{bar_index} for slot {disk_index}");
            return (alloc::vec::Vec::new(), None);
        }
    };
    let Ok(mapped) = axklib::mem::iomap((address as usize).into(), size) else {
        warn!("sdhci: {bdf} BAR{bar_index} mapping failed");
        return (alloc::vec::Vec::new(), None);
    };
    let Some(base) = NonNull::new(mapped.as_usize() as *mut u8) else {
        return (alloc::vec::Vec::new(), None);
    };
    let irq_context = alloc::boxed::Box::leak(alloc::boxed::Box::new(SdhciIrqContext {
        base: base.as_ptr() as usize,
        size,
        pending: AtomicU32::new(0),
    }));
    let mut io = SdhciWindow {
        base,
        size,
        irq: None,
        irq_context: Some(irq_context),
    };
    // Keep a firmware-left signal mask from asserting a PCI line before the
    // acknowledgment endpoint is installed.
    io.write32(axdriver_block::sdhci::SDHCI_SIGNAL_ENABLE as usize, 0);
    let irq = PciBlockInterrupt::register_slot(
        root,
        bdf,
        irq_index,
        irq_context as *const _ as usize,
        ack_sdhci_interrupt,
    );
    if let Some(irq) = &irq {
        info!(
            "sdhci: {bdf} slot {disk_index} completion IRQ mode={:?} vector={:#x}",
            irq.mode(),
            irq.vector()
        );
    } else {
        warn!("sdhci: {bdf} slot {disk_index} no MSI-X/MSI/INTx route; retaining polling fallback");
    }
    io.irq = irq;
    let hotplug_io = io.clone();
    let capabilities = io.read32(SDHCI_CAPABILITIES as usize);
    let capabilities2 = io.read32(SDHCI_CAPABILITIES2 as usize);
    let version = (io.read16(SDHCI_HOST_VERSION as usize) & SDHCI_SPEC_VER_MASK as u16) as u8;
    let host = SdhciHost::new_with_quirks(io, capabilities, capabilities2, version, quirks);
    let host = if info.vendor_id == QEMU_SDHCI_VID && info.device_id == QEMU_SDHCI_DID {
        // QEMU's PCI SDHCI/card pairing times out on CMD18; use CMD17 reads
        // and CMD24 writes so the integration fixture covers generic PIO.
        host.with_single_block_only()
    } else {
        host
    };
    let dma_advertised = dma_supported(capabilities, quirks);
    let host = if dma_advertised && quirks & axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_DMA == 0 {
        match allocate_dma_buffer() {
            Some(region) => host.with_dma_region(region),
            None => host,
        }
    } else {
        host
    };
    let mut disk = match SdhciDisk::attach(host) {
        Ok(disk) => disk,
        Err(error) => {
            warn!("sdhci: {bdf} slot {disk_index} card initialization failed: {error:?}");
            return (
                alloc::vec::Vec::new(),
                Some(SdhciHotplugSlot {
                    io: hotplug_io,
                    capabilities,
                    capabilities2,
                    version,
                    quirks,
                    vendor_id: info.vendor_id,
                    device_id: info.device_id,
                    disk_index,
                    present: false,
                }),
            );
        }
    };
    disk.log_card();
    let read_only = read_only || disk.is_read_only();
    let partitions = disk.into_partition_devices(read_only, disk_index);
    info!(
        "sdhci: {bdf} BAR{bar_index} {:04x}:{:04x} published {} MMC block areas \
         read_only={read_only}",
        info.vendor_id,
        info.device_id,
        partitions.len()
    );

    let mut devices = alloc::vec::Vec::new();
    for partition in partitions {
        let _name = alloc::string::String::from(partition.device_name());
        let _user_area = _name == alloc::format!("mmcblk{disk_index}");
        #[cfg(feature = "shared-block")]
        {
            #[cfg(feature = "dyn")]
            let parent = crate::SharedBlockDevice::new(partition);
            #[cfg(not(feature = "dyn"))]
            let parent = crate::SharedBlockDevice::new(crate::StaticBlockDevice::Sdhci(
                alloc::boxed::Box::new(partition),
            ));
            #[cfg(feature = "dyn")]
            devices.push(crate::AxDeviceEnum::from_block(parent.clone()));
            #[cfg(not(feature = "dyn"))]
            devices.push(crate::AxDeviceEnum::Block(crate::StaticBlockDevice::Sdhci(
                alloc::boxed::Box::new(parent.clone()),
            )));
            if _user_area {
                match crate::discover_gpt_partitions(&parent, &_name, read_only) {
                    Ok(gpt) => devices.extend(gpt.into_iter().map(crate::AxDeviceEnum::Block)),
                    Err(error) => log::debug!("sdhci: no accepted GPT on {_name}: {error:?}"),
                }
            }
        }
        #[cfg(not(feature = "shared-block"))]
        {
            #[cfg(feature = "dyn")]
            devices.push(crate::AxDeviceEnum::from_block(partition));
            #[cfg(not(feature = "dyn"))]
            devices.push(crate::AxDeviceEnum::Block(crate::StaticBlockDevice::Sdhci(
                alloc::boxed::Box::new(partition),
            )));
        }
    }
    (
        devices,
        Some(SdhciHotplugSlot {
            io: hotplug_io,
            capabilities,
            capabilities2,
            version,
            quirks,
            vendor_id: info.vendor_id,
            device_id: info.device_id,
            disk_index,
            present: true,
        }),
    )
}

fn start_hotplug_worker() {
    if SDHCI_HOTPLUG_WORKER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = axtask::spawn_raw(
        sdhci_hotplug_worker,
        "sdhci_hotplug".into(),
        axconfig::TASK_STACK_SIZE,
    ) {
        SDHCI_HOTPLUG_WORKER_STARTED.store(false, Ordering::Release);
        warn!("sdhci: card hotplug worker unavailable: {error:?}");
    }
}

// upstream: sdhci.c sdhci_card_poll(), sdhci_card_task(), sdhci_handle_card_present(), sdhci_handle_card_present_locked()
fn sdhci_hotplug_worker() {
    loop {
        {
            let mut slots = SDHCI_HOTPLUG_SLOTS.lock();
            for slot in slots.iter_mut() {
                let mut host = SdhciHost::new_with_quirks(
                    slot.io.clone(),
                    slot.capabilities,
                    slot.capabilities2,
                    slot.version,
                    slot.quirks,
                );
                if !host.media_present() {
                    slot.present = false;
                    continue;
                }
                if slot.present {
                    continue;
                }
                if slot.vendor_id == QEMU_SDHCI_VID && slot.device_id == QEMU_SDHCI_DID {
                    host = host.with_single_block_only();
                }
                if dma_supported(slot.capabilities, slot.quirks)
                    && slot.quirks & axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_DMA == 0
                    && let Some(region) = allocate_dma_buffer()
                {
                    host = host.with_dma_region(region);
                }
                let Ok(mut disk) = SdhciDisk::attach(host) else {
                    continue;
                };
                disk.log_card();
                let read_only = disk.is_read_only();
                let areas = disk.into_partition_devices(read_only, slot.disk_index);
                let user_name = alloc::format!("mmcblk{}", slot.disk_index);
                let mut published_user = false;
                for area in areas {
                    let is_user = area.device_name() == user_name;
                    let raw = wrap_sdhci_partition(area);
                    if crate::publish_runtime_block_device(raw) {
                        if is_user {
                            published_user = true;
                        }
                    } else if is_user {
                        break;
                    }
                }
                if published_user {
                    slot.present = true;
                    info!("sdhci: card inserted, published /dev/{user_name}");
                }
            }
        }
        let _ = axtask::sleep(core::time::Duration::from_millis(500));
    }
}

fn wrap_sdhci_partition(partition: SdhciPartitionDisk<SdhciWindow>) -> crate::AxBlockDevice {
    #[cfg(feature = "dyn")]
    {
        alloc::boxed::Box::new(partition)
    }
    #[cfg(not(feature = "dyn"))]
    {
        crate::StaticBlockDevice::Sdhci(alloc::boxed::Box::new(partition))
    }
}

/// FreeBSD `sdhci_pci_attach()` slot enumeration and resource adaptation.
// upstream: sdhci_pci.c sdhci_pci_probe() and sdhci_pci_attach()
pub(crate) fn probe(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
) -> super::drivers::BusProbeResult {
    use super::drivers::BusProbeResult;
    if info.class != PCI_CLASS_SYSTEM_PERIPHERAL || info.subclass != PCI_SUBCLASS_SD_HOST {
        return BusProbeResult::NotMatched;
    }
    let Some(command) = root.read_config_dword(bdf, PCI_COMMAND) else {
        return BusProbeResult::Claimed;
    };
    if !root.write_config_u16(
        bdf,
        PCI_COMMAND,
        command as u16 | PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER,
    ) {
        warn!("sdhci: {bdf} could not enable memory decoding");
        return BusProbeResult::Claimed;
    }
    let slot_info = root
        .read_config_dword(bdf, PCI_SLOT_INFO)
        .map_or(u8::MAX, |value| value as u8);
    let (slots, first_bar) = decode_slot_info(slot_info);
    let quirks = quirks_for_device(info.vendor_id, info.device_id);
    let read_only = info.vendor_id == INTEL_EMMC_VID
        && info.device_id == INTEL_EMMC_DID
        && axhal::boot::command_line_value("mmc.allow_write") != Some("1");
    let mut devices = alloc::vec::Vec::new();
    let mut hotplug_slots = alloc::vec::Vec::new();
    for slot in 0..slots {
        let bar = first_bar.saturating_add(slot as u8);
        if bar > 5 {
            warn!("sdhci: {bdf} slot {slot} maps outside PCI BAR0..5");
            continue;
        }
        let disk_index = NEXT_RUNTIME_MMC_INDEX.fetch_add(1, Ordering::Relaxed);
        let (found, hotplug) =
            probe_slot(root, bdf, info, bar, slot, disk_index, quirks, read_only);
        devices.extend(found);
        if let Some(hotplug) = hotplug {
            hotplug_slots.push(hotplug);
        }
    }
    SDHCI_HOTPLUG_SLOTS.lock().extend(hotplug_slots);
    start_hotplug_worker();
    if devices.is_empty() {
        BusProbeResult::Claimed
    } else {
        BusProbeResult::Devices(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freebsd_pci_quirks_are_selected_for_exact_controller_ids() {
        assert_eq!(
            quirks_for_device(0x8086, 0x5acc),
            axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_DMA
                | axdriver_block::sdhci::SDHCI_QUIRK_INTEL_POWER_UP_RESET
                | axdriver_block::sdhci::SDHCI_QUIRK_WAIT_WHILE_BUSY
                | axdriver_block::sdhci::SDHCI_QUIRK_MMC_DDR52
                | axdriver_block::sdhci::SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400
                | axdriver_block::sdhci::SDHCI_QUIRK_PRESET_VALUE_BROKEN
        );
        assert_eq!(quirks_for_device(0x1234, 0x5678), 0);
        assert_ne!(
            quirks_for_device(QEMU_SDHCI_VID, QEMU_SDHCI_DID)
                & axdriver_block::sdhci::SDHCI_QUIRK_BROKEN_DMA,
            0
        );
        assert_eq!(decode_slot_info(0), (1, 0));
        assert_eq!(decode_slot_info(0x25), (3, 5));
        assert_eq!(decode_slot_info(u8::MAX), (1, 0));
        assert_eq!(decode_slot_info(0x0f), (1, 0));
    }

    #[test]
    fn dma_allocation_covers_sdma_adma2_and_force_dma() {
        assert!(dma_supported(axdriver_block::sdhci::SDHCI_CAN_DO_DMA, 0));
        assert!(dma_supported(axdriver_block::sdhci::SDHCI_CAN_DO_ADMA2, 0));
        assert!(dma_supported(
            0,
            axdriver_block::sdhci::SDHCI_QUIRK_FORCE_DMA
        ));
        assert!(!dma_supported(0, 0));
    }
}
