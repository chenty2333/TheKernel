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
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, HeaderType, PciRoot};
use axhal::mem::virt_to_phys;
use log::{info, warn};

use crate::block_irq::PciBlockInterrupt;

const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 0x0002;
const PCI_COMMAND_MASTER: u16 = 0x0004;
const PCI_STATUS_CAPABILITIES_LIST: u16 = 1 << 4;
const PCI_CAPABILITIES_POINTER: u8 = 0x34;
const PCI_CAPABILITY_MIN_OFFSET: u8 = 0x40;
const PCI_CONFIG_LAST_DWORD: u8 = 0xfc;
const PCI_CAP_ID_POWER_MANAGEMENT: u8 = 0x01;
const PCI_PM_CAP_VERSION_MASK: u16 = 0x0007;
const PCI_PM_CAP_D1: u16 = 1 << 9;
const PCI_PM_CAP_D2: u16 = 1 << 10;
const PCI_PM_CONTROL_OFFSET: u8 = 4;
const PCI_PM_STATE_MASK: u16 = 0x0003;
const PCI_PM_NO_SOFT_RESET: u16 = 1 << 3;
const PCI_PM_D1_DELAY_US: u32 = 0;
const PCI_PM_D2_DELAY_US: u32 = 200;
const PCI_PM_D3HOT_DELAY_US: u32 = 10_000;
const PCI_PM_RESET_READY_POLL_US: u32 = 1_000;
const PCI_PM_RESET_READY_POLLS: usize = 1_000;
const PCI_PM_READBACK_POLL_US: u32 = 100;
const PCI_PM_READBACK_POLLS: usize = 100;
const PCI_PM_PME_STATUS: u16 = 1 << 15;
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
const SDHCI_CARD_ATTACH_MAX_ATTEMPTS: u8 = 3;
static SDHCI_HOTPLUG_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static SDHCI_HOTPLUG_SLOTS: spin::Mutex<alloc::vec::Vec<SdhciHotplugSlot>> =
    spin::Mutex::new(alloc::vec::Vec::new());
static NEXT_RUNTIME_MMC_INDEX: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct SdhciPciId {
    id: u32,
    quirks: u32,
}

trait PciPmConfig {
    fn read_config_dword(&mut self, offset: u8) -> Option<u32>;
    fn write_config_u16(&mut self, offset: u8, value: u16) -> bool;
    fn delay_us(&mut self, micros: u32);
}

struct PciPmRoot<'a> {
    root: &'a mut PciRoot,
    bdf: DeviceFunction,
}

impl PciPmConfig for PciPmRoot<'_> {
    fn read_config_dword(&mut self, offset: u8) -> Option<u32> {
        self.root.read_config_dword(self.bdf, offset)
    }

    fn write_config_u16(&mut self, offset: u8, value: u16) -> bool {
        self.root.write_config_u16(self.bdf, offset, value)
    }

    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PciPmError {
    ConfigUnavailable,
    InvalidCapabilityList,
    CapabilityCycle,
    UnsupportedPmVersion,
    UnsupportedCurrentState,
    PmWriteFailed,
    TransitionTimedOut,
}

fn find_pci_pm_capability(io: &mut impl PciPmConfig) -> Result<Option<u8>, PciPmError> {
    let status_command = io
        .read_config_dword(PCI_COMMAND)
        .filter(|value| *value != u32::MAX)
        .ok_or(PciPmError::ConfigUnavailable)?;
    if (status_command >> 16) as u16 & PCI_STATUS_CAPABILITIES_LIST == 0 {
        return Ok(None);
    }

    let pointer_word = io
        .read_config_dword(PCI_CAPABILITIES_POINTER & !0x3)
        .filter(|value| *value != u32::MAX)
        .ok_or(PciPmError::ConfigUnavailable)?;
    let mut offset = (pointer_word & 0xff) as u8;
    if offset == 0 || offset & 0x3 != 0 || offset < PCI_CAPABILITY_MIN_OFFSET {
        return Err(PciPmError::InvalidCapabilityList);
    }

    // Conventional capabilities occupy aligned dwords from 0x40 through
    // 0xfc. A fixed visited set makes malformed loops bounded and explicit.
    let mut visited = [false; 64];
    let mut pm_capability = None;
    loop {
        if offset < PCI_CAPABILITY_MIN_OFFSET || offset > PCI_CONFIG_LAST_DWORD || offset & 0x3 != 0
        {
            return Err(PciPmError::InvalidCapabilityList);
        }
        let index = usize::from(offset >> 2);
        if visited[index] {
            return Err(PciPmError::CapabilityCycle);
        }
        visited[index] = true;

        let header = io
            .read_config_dword(offset)
            .filter(|value| *value != u32::MAX)
            .ok_or(PciPmError::ConfigUnavailable)?;
        let capability_id = header as u8;
        let next = (header >> 8) as u8;
        if capability_id == PCI_CAP_ID_POWER_MANAGEMENT {
            if pm_capability.is_some() || offset > PCI_CONFIG_LAST_DWORD - PCI_PM_CONTROL_OFFSET {
                return Err(PciPmError::InvalidCapabilityList);
            }
            let version = ((header >> 16) as u16) & PCI_PM_CAP_VERSION_MASK;
            if version == 0 || version > 3 {
                return Err(PciPmError::UnsupportedPmVersion);
            }
            pm_capability = Some(offset);
        }

        if next == 0 {
            return Ok(pm_capability);
        }
        if next < PCI_CAPABILITY_MIN_OFFSET || next > PCI_CONFIG_LAST_DWORD || next & 0x3 != 0 {
            return Err(PciPmError::InvalidCapabilityList);
        }
        offset = next;
    }
}

/// Request PCI D0 before the SDHCI driver touches its BAR. Returns true when
/// a D3hot transition may have reset the function and its saved BARs must be
/// restored before mapping them.
fn request_pci_d0(io: &mut impl PciPmConfig) -> Result<bool, PciPmError> {
    // A function in D3cold is not configuration-space accessible. Do not
    // attempt PMCSR writes or trust a BAR snapshot in that state.
    let vendor_device = io
        .read_config_dword(0)
        .filter(|value| *value != u32::MAX && *value as u16 != u16::MAX)
        .ok_or(PciPmError::ConfigUnavailable)?;
    let Some(pm_capability) = find_pci_pm_capability(io)? else {
        // PCI functions without a PM capability are not software-managed by
        // PCI PM; retain the firmware's default D0 behavior.
        return Ok(false);
    };

    let capability = io
        .read_config_dword(pm_capability)
        .filter(|value| *value != u32::MAX)
        .ok_or(PciPmError::ConfigUnavailable)?;
    let pmc = (capability >> 16) as u16;
    let version = pmc & PCI_PM_CAP_VERSION_MASK;
    if version == 0 || version > 3 {
        return Err(PciPmError::UnsupportedPmVersion);
    }
    let pmcsr_offset = pm_capability + PCI_PM_CONTROL_OFFSET;
    let pmcsr_word = io
        .read_config_dword(pmcsr_offset)
        .filter(|value| *value != u32::MAX && *value as u16 != u16::MAX)
        .ok_or(PciPmError::ConfigUnavailable)? as u16;
    let current_state = pmcsr_word & PCI_PM_STATE_MASK;
    if (current_state == 1 && pmc & PCI_PM_CAP_D1 == 0)
        || (current_state == 2 && pmc & PCI_PM_CAP_D2 == 0)
    {
        return Err(PciPmError::UnsupportedCurrentState);
    }
    if current_state == 0 {
        return Ok(false);
    }

    // PMCSR.PME_Status is RW1C. Never echo it back as one while changing only
    // PowerState; keep PME_Enable and every other readable control bit intact.
    let write_value = pmcsr_word & !(PCI_PM_STATE_MASK | PCI_PM_PME_STATUS);
    if !io.write_config_u16(pmcsr_offset, write_value) {
        return Err(PciPmError::PmWriteFailed);
    }

    match current_state {
        1 => io.delay_us(PCI_PM_D1_DELAY_US),
        2 => io.delay_us(PCI_PM_D2_DELAY_US),
        3 => io.delay_us(PCI_PM_D3HOT_DELAY_US),
        _ => return Err(PciPmError::UnsupportedCurrentState),
    }

    let restore_bars = current_state == 3 && pmcsr_word & PCI_PM_NO_SOFT_RESET == 0;
    if restore_bars {
        let mut ready = false;
        for _ in 0..PCI_PM_RESET_READY_POLLS {
            let current = io.read_config_dword(0);
            if current.is_some_and(|value| {
                value != u32::MAX && value as u16 != u16::MAX && value == vendor_device
            }) {
                ready = true;
                break;
            }
            io.delay_us(PCI_PM_RESET_READY_POLL_US);
        }
        if !ready {
            return Err(PciPmError::TransitionTimedOut);
        }
    }

    for _ in 0..PCI_PM_READBACK_POLLS {
        let Some(pmcsr) = io
            .read_config_dword(pmcsr_offset)
            .filter(|value| *value != u32::MAX && *value as u16 != u16::MAX)
        else {
            return Err(PciPmError::ConfigUnavailable);
        };
        if pmcsr as u16 & PCI_PM_STATE_MASK == 0 {
            return Ok(restore_bars);
        }
        io.delay_us(PCI_PM_READBACK_POLL_US);
    }
    Err(PciPmError::TransitionTimedOut)
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
        // storage acceptance remains deterministic. This virtual function's
        // INTx handler can consume SDHCI status before CMD17 polling observes
        // it; signal masking is tracked by the PCI window adapter.
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

fn irq_signal_usable(vendor_id: u16, device_id: u16) -> bool {
    !(vendor_id == QEMU_SDHCI_VID && device_id == QEMU_SDHCI_DID)
}

unsafe fn free_sdma_buffer(cpu: NonNull<u8>, pages: usize) {
    global_allocator().dealloc_pages(cpu.as_ptr() as usize, pages, UsageKind::Dma);
}

struct SdhciIrqContext {
    base: usize,
    size: usize,
    pending: AtomicU32,
}

// upstream: sdhci_pci.c sdhci_pci_intr() multi-slot generic interrupt dispatch
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
    irq_signal_usable: bool,
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
    /// Slot-level write policy retained even when initial card enumeration
    /// fails and a later insertion causes a fresh `SdhciDisk::attach`.
    read_only: bool,
    /// Bounded attach retries for a continuously-present card. Once the
    /// budget is exhausted, wait for card removal/reinsertion instead of
    /// repeatedly reinitializing a failed controller forever.
    attach_attempts: u8,
    retry_abandoned: bool,
    present: bool,
}

impl SdhciHotplugSlot {
    fn note_attach_failure(&mut self) -> bool {
        self.attach_attempts = self.attach_attempts.saturating_add(1);
        if self.attach_attempts >= SDHCI_CARD_ATTACH_MAX_ATTEMPTS {
            self.retry_abandoned = true;
        }
        self.retry_abandoned
    }

    fn observe_absent(&mut self) {
        self.present = false;
        self.attach_attempts = 0;
        self.retry_abandoned = false;
    }
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

    fn monotonic_time_ns(&self) -> Option<u64> {
        Some(axhal::time::monotonic_time_nanos())
    }

    fn has_interrupt(&self) -> bool {
        self.irq.is_some()
    }

    fn interrupt_signal_usable(&self) -> bool {
        self.irq_signal_usable
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
        irq_signal_usable: irq_signal_usable(info.vendor_id, info.device_id),
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
    let mut host = SdhciHost::new_with_quirks(io, capabilities, capabilities2, version, quirks);
    let card_present = host.media_present();
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
                    read_only,
                    attach_attempts: u8::from(card_present),
                    retry_abandoned: false,
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
            read_only,
            attach_attempts: 0,
            retry_abandoned: false,
            present: true,
        }),
    )
}

fn start_hotplug_worker() {
    if SDHCI_HOTPLUG_WORKER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = axtask::spawn_raw(
        sdhci_card_task,
        "sdhci_hotplug".into(),
        axconfig::TASK_STACK_SIZE,
    ) {
        SDHCI_HOTPLUG_WORKER_STARTED.store(false, Ordering::Release);
        warn!("sdhci: card hotplug worker unavailable: {error:?}");
    }
}

// upstream: sdhci.c sdhci_card_task()
fn sdhci_card_task() {
    loop {
        sdhci_card_poll();
        let _ = axtask::sleep(core::time::Duration::from_millis(500));
    }
}

// upstream: sdhci.c sdhci_card_poll()
fn sdhci_card_poll() {
    let count = SDHCI_HOTPLUG_SLOTS.lock().len();
    for index in 0..count {
        sdhci_handle_card_present(index);
    }
}

// upstream: sdhci.c sdhci_handle_card_present()
fn sdhci_handle_card_present(index: usize) {
    let mut slots = SDHCI_HOTPLUG_SLOTS.lock();
    if let Some(slot) = slots.get_mut(index) {
        sdhci_handle_card_present_locked(slot);
    }
}

// upstream: sdhci.c sdhci_handle_card_present_locked()
fn sdhci_handle_card_present_locked(slot: &mut SdhciHotplugSlot) {
    let mut host = SdhciHost::new_with_quirks(
        slot.io.clone(),
        slot.capabilities,
        slot.capabilities2,
        slot.version,
        slot.quirks,
    );
    if !host.media_present() {
        slot.observe_absent();
        return;
    }
    if slot.present || slot.retry_abandoned {
        return;
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
    let mut disk = match SdhciDisk::attach(host) {
        Ok(disk) => disk,
        Err(error) => {
            if slot.note_attach_failure() {
                warn!(
                    "sdhci: card initialization failed after {} attempts ({error:?}); retry \
                     disabled until media removal or reboot",
                    slot.attach_attempts
                );
            } else {
                warn!(
                    "sdhci: card initialization failed attempt {}/{} ({error:?}); will retry",
                    slot.attach_attempts, SDHCI_CARD_ATTACH_MAX_ATTEMPTS
                );
            }
            return;
        }
    };
    disk.log_card();
    let read_only =
        axdriver_block::sdhci::sdhci_effective_read_only(slot.read_only, disk.is_read_only());
    let areas = disk.into_partition_devices(read_only, slot.disk_index);
    let user_name = alloc::format!("mmcblk{}", slot.disk_index);
    let mut published_user = false;
    for area in areas {
        let is_user = area.device_name() == user_name;
        let raw = wrap_sdhci_partition(area);
        if crate::publish_runtime_block_device(raw) {
            published_user |= is_user;
        } else if is_user {
            break;
        }
    }
    if published_user {
        slot.present = true;
        slot.attach_attempts = 0;
        slot.retry_abandoned = false;
        info!("sdhci: card inserted, published /dev/{user_name}");
    } else {
        slot.retry_abandoned = true;
        warn!(
            "sdhci: card attached but /dev/{user_name} could not be published; retry disabled \
             until media removal or reboot"
        );
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
    if info.header_type != HeaderType::Standard {
        warn!(
            "sdhci: {bdf} has unsupported PCI header type {:?}",
            info.header_type
        );
        return BusProbeResult::Claimed;
    }
    let Some(identity) = root
        .read_config_dword(bdf, 0)
        .filter(|value| *value != u32::MAX && *value as u16 != u16::MAX)
    else {
        warn!("sdhci: {bdf} config space is inaccessible before power-up");
        return BusProbeResult::Claimed;
    };
    if identity as u16 != info.vendor_id || (identity >> 16) as u16 != info.device_id {
        warn!("sdhci: {bdf} PCI identity changed during probe");
        return BusProbeResult::Claimed;
    }

    // A D3hot->D0 transition may cause an internal reset when PMCSR says
    // No_Soft_Reset is clear. Save firmware-assigned BARs before requesting
    // D0 so that those addresses can be restored before any BAR mapping.
    let original_bars = root.raw_bars(bdf, HeaderType::Standard);
    if root
        .read_config_dword(bdf, PCI_COMMAND)
        .filter(|value| *value != u32::MAX)
        .is_none()
    {
        warn!("sdhci: {bdf} command register is inaccessible");
        return BusProbeResult::Claimed;
    }

    let restore_bars = {
        let mut pm = PciPmRoot { root, bdf };
        match request_pci_d0(&mut pm) {
            Ok(restore_bars) => restore_bars,
            Err(error) => {
                warn!("sdhci: {bdf} refused PCI D0 transition: {error:?}");
                return BusProbeResult::Claimed;
            }
        }
    };
    let Some(command) = root
        .read_config_dword(bdf, PCI_COMMAND)
        .filter(|value| *value != u32::MAX)
    else {
        warn!("sdhci: {bdf} command register unavailable after PCI D0 transition");
        return BusProbeResult::Claimed;
    };
    if !root.write_config_u16(
        bdf,
        PCI_COMMAND,
        command as u16 & !(PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER),
    ) {
        warn!("sdhci: {bdf} could not quiesce PCI memory/bus-master decoding");
        return BusProbeResult::Claimed;
    }
    if restore_bars {
        for (bar, value) in original_bars.iter().copied().enumerate() {
            root.set_bar_32(bdf, bar as u8, value);
        }
        if root.raw_bars(bdf, HeaderType::Standard) != original_bars {
            warn!("sdhci: {bdf} could not restore BARs after D3hot reset");
            return BusProbeResult::Claimed;
        }
    }
    let Some(command) = root
        .read_config_dword(bdf, PCI_COMMAND)
        .filter(|value| *value != u32::MAX)
    else {
        warn!("sdhci: {bdf} command register became inaccessible while restoring resources");
        return BusProbeResult::Claimed;
    };
    if !root.write_config_u16(
        bdf,
        PCI_COMMAND,
        command as u16 | PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER,
    ) {
        warn!("sdhci: {bdf} could not enable memory decoding after PCI D0 transition");
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

    struct FakePciPm {
        config: [u32; 64],
        writes: alloc::vec::Vec<(u8, u16)>,
        delays: alloc::vec::Vec<u32>,
        accept_pm_write: bool,
    }

    impl FakePciPm {
        fn new(pmc: u16, pmcsr: u16) -> Self {
            let mut config = [0; 64];
            config[0] = (u32::from(INTEL_EMMC_DID) << 16) | u32::from(INTEL_EMMC_VID);
            config[PCI_COMMAND as usize / 4] = u32::from(PCI_STATUS_CAPABILITIES_LIST) << 16;
            config[PCI_CAPABILITIES_POINTER as usize / 4] = 0x50;
            config[0x50 / 4] = (u32::from(pmc) << 16) | u32::from(PCI_CAP_ID_POWER_MANAGEMENT);
            config[(0x50 + PCI_PM_CONTROL_OFFSET) as usize / 4] = u32::from(pmcsr);
            Self {
                config,
                writes: alloc::vec::Vec::new(),
                delays: alloc::vec::Vec::new(),
                accept_pm_write: true,
            }
        }

        fn add_capability(&mut self, offset: u8, id: u8, next: u8) {
            self.config[usize::from(offset / 4)] = u32::from(id) | (u32::from(next) << 8);
        }
    }

    impl PciPmConfig for FakePciPm {
        fn read_config_dword(&mut self, offset: u8) -> Option<u32> {
            self.config.get(usize::from(offset / 4)).copied()
        }

        fn write_config_u16(&mut self, offset: u8, value: u16) -> bool {
            self.writes.push((offset, value));
            if offset == 0x50 + PCI_PM_CONTROL_OFFSET && !self.accept_pm_write {
                return true;
            }
            let index = usize::from(offset / 4);
            let shift = u32::from(offset & 2) * 8;
            let old_dword = self.config[index];
            let old_word = (old_dword >> shift) as u16;
            // PME_Status is RW1C: writing zero leaves the old status set.
            let status = if value & PCI_PM_PME_STATUS != 0 {
                0
            } else {
                old_word & PCI_PM_PME_STATUS
            };
            let next_word = (value & !PCI_PM_PME_STATUS) | status;
            self.config[index] =
                (old_dword & !(u32::from(u16::MAX) << shift)) | (u32::from(next_word) << shift);
            true
        }

        fn delay_us(&mut self, micros: u32) {
            self.delays.push(micros);
        }
    }

    #[test]
    fn failed_initial_attach_keeps_emmc_write_policy_for_hotplug_retry() {
        let slot = SdhciHotplugSlot {
            io: SdhciWindow {
                base: NonNull::dangling(),
                size: 0,
                irq: None,
                irq_context: None,
                irq_signal_usable: false,
            },
            capabilities: 0,
            capabilities2: 0,
            version: 0,
            quirks: 0,
            vendor_id: INTEL_EMMC_VID,
            device_id: INTEL_EMMC_DID,
            disk_index: 0,
            read_only: true,
            attach_attempts: 0,
            retry_abandoned: false,
            present: false,
        };

        // The first card initialization failed, so the worker retains this
        // slot and retries attach after card-detect rises.
        assert!(!slot.present);
        // Re-attach sees a card whose write-protect switch is off; the
        // retained slot policy must still force a read-only publication.
        let retry_disk_write_protected = false;
        assert!(axdriver_block::sdhci::sdhci_effective_read_only(
            slot.read_only,
            retry_disk_write_protected
        ));
        // With mmc.allow_write=1 the slot policy is writable and the card
        // switch decides.
        assert!(!axdriver_block::sdhci::sdhci_effective_read_only(
            false,
            retry_disk_write_protected
        ));
    }

    #[test]
    fn failed_card_attach_retries_are_bounded_until_removal() {
        let mut slot = SdhciHotplugSlot {
            io: SdhciWindow {
                base: NonNull::dangling(),
                size: 0,
                irq: None,
                irq_context: None,
                irq_signal_usable: false,
            },
            capabilities: 0,
            capabilities2: 0,
            version: 0,
            quirks: 0,
            vendor_id: INTEL_EMMC_VID,
            device_id: INTEL_EMMC_DID,
            disk_index: 0,
            read_only: true,
            attach_attempts: 1,
            retry_abandoned: false,
            present: false,
        };

        assert!(!slot.note_attach_failure());
        assert!(slot.note_attach_failure());
        assert_eq!(slot.attach_attempts, SDHCI_CARD_ATTACH_MAX_ATTEMPTS);
        assert!(slot.retry_abandoned);
        assert!(slot.retry_abandoned);

        slot.observe_absent();
        assert_eq!(slot.attach_attempts, 0);
        assert!(!slot.retry_abandoned);
    }

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
        assert!(!irq_signal_usable(QEMU_SDHCI_VID, QEMU_SDHCI_DID));
        assert!(irq_signal_usable(0x8086, 0x54c4));
    }

    #[test]
    fn pci_pm_moves_d3hot_to_d0_preserving_controls_without_clearing_pme_status() {
        let mut pci = FakePciPm::new(3, 0x810b); // D3hot, No_Soft_Reset, PME_EN, PME_STATUS
        assert!(!request_pci_d0(&mut pci).unwrap());
        assert_eq!(pci.writes, [(0x54, 0x0108)]);
        assert_eq!(pci.delays, [PCI_PM_D3HOT_DELAY_US]);
        assert_eq!(pci.config[0x54 / 4] as u16, 0x8108);
    }

    #[test]
    fn pci_pm_restores_bars_after_d3hot_that_may_reset_the_function() {
        let mut pci = FakePciPm::new(3, 0x0003); // D3hot, No_Soft_Reset clear
        assert!(request_pci_d0(&mut pci).unwrap());
        assert_eq!(pci.delays, [PCI_PM_D3HOT_DELAY_US]);
    }

    #[test]
    fn pci_pm_accepts_devices_without_a_pm_capability_as_firmware_d0() {
        let mut pci = FakePciPm::new(3, 0);
        pci.config[PCI_COMMAND as usize / 4] = 0;
        assert!(!request_pci_d0(&mut pci).unwrap());
        assert!(pci.writes.is_empty());
        assert!(pci.delays.is_empty());
    }

    #[test]
    fn pci_pm_rejects_malformed_or_cyclic_capability_chains() {
        let mut invalid = FakePciPm::new(3, 3);
        invalid.config[PCI_CAPABILITIES_POINTER as usize / 4] = 0x20;
        assert_eq!(
            find_pci_pm_capability(&mut invalid),
            Err(PciPmError::InvalidCapabilityList)
        );

        let mut cyclic = FakePciPm::new(3, 3);
        cyclic.add_capability(0x50, 2, 0x54);
        cyclic.add_capability(0x54, 3, 0x50);
        assert_eq!(
            find_pci_pm_capability(&mut cyclic),
            Err(PciPmError::CapabilityCycle)
        );
    }

    #[test]
    fn pci_pm_rejects_unsupported_power_states_and_inaccessible_pmcsr() {
        let mut unsupported_d1 = FakePciPm::new(3, 1);
        assert_eq!(
            request_pci_d0(&mut unsupported_d1),
            Err(PciPmError::UnsupportedCurrentState)
        );
        assert!(unsupported_d1.writes.is_empty());

        let mut inaccessible = FakePciPm::new(3, u16::MAX);
        assert_eq!(
            request_pci_d0(&mut inaccessible),
            Err(PciPmError::ConfigUnavailable)
        );
        assert!(inaccessible.writes.is_empty());
    }

    #[test]
    fn pci_pm_rejects_config_space_that_is_unresponsive_in_d3cold() {
        let mut cold = FakePciPm::new(3, 3);
        cold.config[0] = u32::MAX;
        assert_eq!(
            request_pci_d0(&mut cold),
            Err(PciPmError::ConfigUnavailable)
        );
        assert!(cold.writes.is_empty());
        assert!(cold.delays.is_empty());
    }

    #[test]
    fn pci_pm_refuses_a_power_transition_without_d0_readback() {
        let mut pci = FakePciPm::new(3, 0x000b);
        pci.accept_pm_write = false;
        assert_eq!(
            request_pci_d0(&mut pci),
            Err(PciPmError::TransitionTimedOut)
        );
        assert_eq!(pci.delays.len(), PCI_PM_READBACK_POLLS + 1);
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
