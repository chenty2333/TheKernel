//! PCI match and early attach boundary for Intel iwx.
//!
//! The target hardware path is continued in `tk-axdriver-iwx`; this module
//! wires the upstream PCI match decision into TheKernel's PCI driver walk.

use alloc::{collections::VecDeque, vec::Vec};
#[cfg(feature = "irq")]
use core::sync::atomic::AtomicUsize;
use core::{
    convert::Infallible,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering, fence},
};

use axalloc::{UsageKind, global_allocator};
use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use axdriver_iwx::{
    AX211_DEVICE_ID, AttachAllocationError, AttachProfile, CsrAccess, DmaAllocator, DmaError,
    DmaRegion, FirmwareBundle, FirmwareError, FirmwareImage, INTEL_VENDOR_ID, IoBarrier,
    IwxController, NvmInfo, PreinitPlan, RuntimeConfig, attach_profile, matches_pci_device,
    preinit_plan,
};
#[cfg(feature = "irq")]
use axdriver_iwx::{DeviceFamily, IwxRegisters};
use axdriver_net::{
    EthernetAddress, NetBuf, NetBufPool, NetBufPtr, NetDriverOps, WirelessDisconnectEvent,
    WirelessFrequency, WirelessHtCapabilities, WirelessKeyConfig, WirelessKeyInfo,
    WirelessKeyOperation, WirelessPhyCapabilities, WirelessScanEvent, WirelessVhtCapabilities,
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use axhal::mem::{phys_to_virt, virt_to_phys};
use spin::Mutex;

use crate::drivers::BusProbeResult;

const CSR_HW_REV: usize = 0x028;
const CSR_HW_RF_ID: usize = 0x09c;
const CSR_HW_RFID_TYPE_MASK: u32 = 0x00ff_f000;
const CSR_HW_RFID_TYPE_SHIFT: u32 = 12;
const RF_TYPE_GF: u16 = 0x10d;
const SPECIAL_BZ_DEVICE: u16 = 0x7740;
const IWX_SCAN_RATES_2GHZ: [u8; 12] = [
    0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6c,
];
const IWX_SCAN_RATES_5GHZ: [u8; 8] = [0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c];
const IWX_STATION_DATA_QUEUE: u8 = axdriver_iwx::DQA_CMD_QUEUE + 2;
const PCI_CAPABILITY_MSIX: u8 = 0x11;
#[cfg(feature = "irq")]
const MAX_IWX_MSIX_DEVICES: usize = 8;
#[cfg(feature = "irq")]
const CSR_MSIX_FH_CAUSES: u32 = 0x2800;
#[cfg(feature = "irq")]
const CSR_MSIX_HW_CAUSES: u32 = 0x2808;
const IWX_UCODE_TLV_CAPA_MULTI_QUEUE_RX_SUPPORT: usize = 68;

#[cfg(any(feature = "irq", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IwxMsixTable {
    bir: u8,
    offset: usize,
    vectors: usize,
}

fn disable_msix_configuration(root: &mut PciRoot, bdf: DeviceFunction) {
    if let Some(capability) = root
        .capabilities(bdf)
        .take(48)
        .find(|capability| capability.id == PCI_CAPABILITY_MSIX && capability.offset <= 0xf4)
    {
        root.write_config_u16(
            bdf,
            capability.offset + 2,
            (capability.private_header | 0x4000) & !0x8000,
        );
        let _ = root.read_config_dword(bdf, capability.offset);
    }
}

#[cfg(any(feature = "irq", test))]
impl IwxMsixTable {
    fn decode(control: u16, table: u32) -> Option<Self> {
        let bir = (table & 7) as u8;
        if bir >= 6 {
            return None;
        }
        Some(Self {
            bir,
            offset: (table & !7) as usize,
            vectors: usize::from(control & 0x07ff) + 1,
        })
    }

    fn fits(self, bytes: usize) -> bool {
        self.vectors
            .checked_mul(16)
            .and_then(|length| self.offset.checked_add(length))
            .is_some_and(|end| end <= bytes)
    }
}

#[cfg(feature = "irq")]
struct IwxIrqSlot {
    bar_base: AtomicUsize,
    bar_size: AtomicUsize,
}

#[cfg(feature = "irq")]
impl IwxIrqSlot {
    const fn new() -> Self {
        Self {
            bar_base: AtomicUsize::new(0),
            bar_size: AtomicUsize::new(0),
        }
    }
}

#[cfg(feature = "irq")]
static IWX_IRQ_SLOTS: [IwxIrqSlot; MAX_IWX_MSIX_DEVICES] =
    [const { IwxIrqSlot::new() }; MAX_IWX_MSIX_DEVICES];

/// The hard-IRQ half only acknowledges cause state and lets axhal's shared
/// IRQ hook wake the permanent net RX worker; packet/command processing stays
/// in task context, outside `ATTACHED_DMA`'s lock.
#[cfg(feature = "irq")]
fn service_iwx_msix_slot(index: usize) {
    let Some(slot) = IWX_IRQ_SLOTS.get(index) else {
        return;
    };
    let base = slot.bar_base.load(Ordering::Acquire);
    let size = slot.bar_size.load(Ordering::Acquire);
    if base == 0 || size < CSR_MSIX_HW_CAUSES as usize + 4 {
        return;
    }
    let mut bus = MmioCsrAccess { base, size };
    let flow = bus.read32(CSR_MSIX_FH_CAUSES);
    let hardware = bus.read32(CSR_MSIX_HW_CAUSES);
    if flow == 0 && hardware == 0 || flow == u32::MAX || hardware == u32::MAX {
        return;
    }
    let mut registers = IwxRegisters::new(bus, DeviceFamily::Legacy, 0);
    let work = axdriver_iwx::service_msix_interrupt_from_hardware(&mut registers);
    if work.top_fatal_error || work.software_error || work.hardware_error {
        warn!("iwx: MSI-X reported fatal hardware/firmware cause; vector remains masked");
    }
}

#[cfg(feature = "irq")]
macro_rules! msix_slot_handler {
    ($handler:ident, $index:expr) => {
        fn $handler() {
            service_iwx_msix_slot($index);
        }
    };
}

#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_0, 0);
#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_1, 1);
#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_2, 2);
#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_3, 3);
#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_4, 4);
#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_5, 5);
#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_6, 6);
#[cfg(feature = "irq")]
msix_slot_handler!(iwx_msix_interrupt_7, 7);

#[cfg(feature = "irq")]
const IWX_MSIX_HANDLERS: [fn(); MAX_IWX_MSIX_DEVICES] = [
    iwx_msix_interrupt_0,
    iwx_msix_interrupt_1,
    iwx_msix_interrupt_2,
    iwx_msix_interrupt_3,
    iwx_msix_interrupt_4,
    iwx_msix_interrupt_5,
    iwx_msix_interrupt_6,
    iwx_msix_interrupt_7,
];

#[cfg(feature = "irq")]
fn reserve_iwx_irq_slot(base: usize, size: usize) -> Option<usize> {
    IWX_IRQ_SLOTS.iter().position(|slot| {
        if slot
            .bar_base
            .compare_exchange(0, base, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            slot.bar_size.store(size, Ordering::Release);
            true
        } else {
            false
        }
    })
}

#[cfg(feature = "irq")]
fn release_iwx_irq_slot(index: usize) {
    let slot = &IWX_IRQ_SLOTS[index];
    slot.bar_size.store(0, Ordering::Release);
    slot.bar_base.store(0, Ordering::Release);
}

#[cfg(feature = "irq")]
struct IwxMsixRoute {
    table: usize,
    capability: u8,
    control: u16,
    vector: usize,
}

#[cfg(feature = "irq")]
impl IwxMsixRoute {
    fn disable(root: &mut PciRoot, bdf: DeviceFunction) {
        disable_msix_configuration(root, bdf);
    }

    fn prepare(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        bar0_base: usize,
        bar0_size: usize,
    ) -> Option<Self> {
        let capability = root
            .capabilities(bdf)
            .take(48)
            .find(|capability| capability.id == PCI_CAPABILITY_MSIX && capability.offset <= 0xf4)?;
        let layout = IwxMsixTable::decode(
            capability.private_header,
            root.read_config_dword(bdf, capability.offset + 4)?,
        )?;
        let BarInfo::Memory { address, size, .. } = root.bar_info(bdf, layout.bir).ok()? else {
            return None;
        };
        if address == 0 || !layout.fits(size as usize) {
            return None;
        }
        let mapped = axklib::mem::iomap((address as usize).into(), size as usize).ok()?;
        let table = mapped.as_usize().checked_add(layout.offset)?;
        let irq_slot = reserve_iwx_irq_slot(bar0_base, bar0_size)?;
        let Some((message, data, vector)) = axhal::irq::allocate_msi(IWX_MSIX_HANDLERS[irq_slot])
        else {
            release_iwx_irq_slot(irq_slot);
            return None;
        };
        let control = capability.private_header | 0xc000;
        if !root.write_config_u16(bdf, capability.offset + 2, control) {
            release_iwx_irq_slot(irq_slot);
            return None;
        }
        // SAFETY: the whole table range is bounded by the selected mapped BAR;
        // function masking is held until every vector entry is initialized.
        unsafe {
            for index in 0..layout.vectors {
                ((table + index * 16 + 12) as *mut u32).write_volatile(1);
            }
            (table as *mut u32).write_volatile(message as u32);
            ((table + 4) as *mut u32).write_volatile((message >> 32) as u32);
            ((table + 8) as *mut u32).write_volatile(data);
            let _ = ((table + 12) as *const u32).read_volatile();
        }
        Some(Self {
            table,
            capability: capability.offset,
            control,
            vector,
        })
    }

    fn enable(&self, root: &mut PciRoot, bdf: DeviceFunction) {
        // SAFETY: entry zero is initialized and has a permanent MSI handler.
        unsafe { ((self.table + 12) as *mut u32).write_volatile(0) };
        root.write_config_u16(bdf, self.capability + 2, self.control & !0x4000);
        let _ = root.read_config_dword(bdf, self.capability);
        // Firmware bootstrap is performed before netdev publication; enable
        // the permanent route now so the same handler services init-uCode.
        axhal::irq::set_enable(self.vector, true);
    }
}

#[cfg(not(feature = "irq"))]
struct IwxMsixRoute {
    vector: usize,
}

#[cfg(not(feature = "irq"))]
impl IwxMsixRoute {
    fn disable(root: &mut PciRoot, bdf: DeviceFunction) {
        disable_msix_configuration(root, bdf);
    }

    fn prepare(
        _root: &mut PciRoot,
        _bdf: DeviceFunction,
        _bar0_base: usize,
        _bar0_size: usize,
    ) -> Option<Self> {
        None
    }

    fn enable(&self, _root: &mut PciRoot, _bdf: DeviceFunction) {}
}

struct PlatformDmaRegion {
    cpu: NonNull<u8>,
    physical: u64,
    capacity: usize,
    pages: usize,
}

unsafe impl Send for PlatformDmaRegion {}

impl DmaRegion for PlatformDmaRegion {
    fn device_address(&self) -> u64 {
        self.physical
    }
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn write(&mut self, bytes: &[u8]) -> Result<(), DmaError> {
        self.write_at(0, bytes)
    }
    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DmaError> {
        let Some(end) = offset.checked_add(bytes.len()) else {
            return Err(DmaError::RegionTooSmall);
        };
        if end > self.capacity {
            return Err(DmaError::RegionTooSmall);
        }
        // SAFETY: offset/end are bounded by this live allocation.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.cpu.as_ptr().add(offset),
                bytes.len(),
            );
        }
        fence(Ordering::Release);
        Ok(())
    }
    fn read_at(&self, offset: usize, bytes: &mut [u8]) -> Result<(), DmaError> {
        let Some(end) = offset.checked_add(bytes.len()) else {
            return Err(DmaError::RegionTooSmall);
        };
        if end > self.capacity {
            return Err(DmaError::RegionTooSmall);
        }
        fence(Ordering::Acquire);
        // SAFETY: offset/end are bounded by this live allocation.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.cpu.as_ptr().add(offset),
                bytes.as_mut_ptr(),
                bytes.len(),
            );
        }
        Ok(())
    }
}

impl Drop for PlatformDmaRegion {
    // upstream: if_iwx.c iwx_dma_contig_free()
    fn drop(&mut self) {
        global_allocator().dealloc_pages(self.cpu.as_ptr() as usize, self.pages, UsageKind::Dma);
    }
}

struct PlatformDmaAllocator;
impl DmaAllocator for PlatformDmaAllocator {
    type Region = PlatformDmaRegion;
    // upstream: if_iwx.c iwx_dma_contig_alloc()
    fn allocate(&mut self, size: usize) -> Result<Self::Region, DmaError> {
        let pages = size.max(1).div_ceil(4096);
        let virtual_address = global_allocator()
            .alloc_pages(pages, 4096, UsageKind::Dma)
            .map_err(|_| DmaError::AllocationFailed)?;
        if virtual_address == 0 {
            return Err(DmaError::AllocationFailed);
        }
        // SAFETY: the allocator returned an owned page-aligned DMA allocation.
        unsafe {
            core::ptr::write_bytes(virtual_address as *mut u8, 0, pages * 4096);
        }
        let cpu = NonNull::new(virtual_address as *mut u8).ok_or(DmaError::AllocationFailed)?;
        let physical = virt_to_phys(virtual_address.into()).as_usize() as u64;
        Ok(PlatformDmaRegion {
            cpu,
            physical,
            capacity: size,
            pages,
        })
    }
}

/// Bounds-checked volatile CSR access over the mapped PCI BAR.
struct MmioCsrAccess {
    base: usize,
    size: usize,
}

impl MmioCsrAccess {
    fn contains(&self, offset: u32, width: usize) -> bool {
        let offset = offset as usize;
        offset.is_multiple_of(width)
            && offset
                .checked_add(width)
                .is_some_and(|end| end <= self.size)
    }
}

impl CsrAccess for MmioCsrAccess {
    fn read32(&mut self, offset: u32) -> u32 {
        if !self.contains(offset, 4) {
            return u32::MAX;
        }
        // SAFETY: PCI probe validated this register window; `contains` proves alignment/range.
        unsafe { ((self.base + offset as usize) as *const u32).read_volatile() }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        if self.contains(offset, 4) {
            // SAFETY: PCI probe validated this register window; `contains` proves alignment/range.
            unsafe { ((self.base + offset as usize) as *mut u32).write_volatile(value) };
        }
    }

    fn write8(&mut self, offset: u32, value: u8) {
        if self.contains(offset, 1) {
            // SAFETY: PCI probe validated this register window; `contains` proves range.
            unsafe { ((self.base + offset as usize) as *mut u8).write_volatile(value) };
        }
    }

    fn barrier(&mut self, direction: IoBarrier) {
        match direction {
            IoBarrier::ReadWrite => core::sync::atomic::fence(Ordering::SeqCst),
            IoBarrier::Write => core::sync::atomic::fence(Ordering::Release),
        }
    }

    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
}

unsafe impl Send for MmioCsrAccess {}

fn map_wireless_sme_error(error: RuntimeStartError) -> DevError {
    match error {
        RuntimeStartError::FirmwareNotReady | RuntimeStartError::SoftBlocked => DevError::BadState,
        RuntimeStartError::Transmission | RuntimeStartError::Firmware => DevError::Io,
        RuntimeStartError::InvalidRequest => DevError::InvalidParam,
        RuntimeStartError::UnsupportedSecurity => DevError::Unsupported,
        RuntimeStartError::DeviceNotFound => DevError::BadState,
        _ => DevError::Io,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Bdf(u8, u8, u8);

struct AttachedDevice {
    bdf: Bdf,
    profile: AttachProfile,
    runtime: RuntimeConfig,
    bar_base: usize,
    bar_size: usize,
    hardware_revision: u32,
    msix_vector: Option<usize>,
    controller: IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    firmware: Option<Result<FirmwareBundle, FirmwareRequestError>>,
    nvm: Option<NvmInfo>,
    preinit: Option<PreinitPlan>,
    scan_cache: Option<axdriver_iwx::ScanCache>,
    scan_phy: Option<axdriver_iwx::RxPhyInfo>,
    scan_complete: bool,
    scan_event: Option<WirelessScanEvent>,
    association: axdriver_iwx::AssociationState,
    session_protection: axdriver_iwx::SessionProtectionState,
    beacon_filter: axdriver_iwx::BeaconFilterState,
    security_keys: IwxSecurityKeys,
    pending_sme: Option<PendingSmeStation>,
    station: Option<StationConnection>,
    disconnect_event: Option<WirelessDisconnectEvent>,
    runtime_started: bool,
    interface_up: bool,
    soft_blocked: bool,
}

#[derive(Default)]
struct IwxSecurityKeys {
    pairwise: Option<IwxKeySlot>,
    groups: [Option<IwxKeySlot>; 4],
    management: [Option<IwxKeySlot>; 4],
    default_group: u8,
    default_management: u8,
}

struct IwxKeySlot {
    index: u8,
    cipher_suite: u32,
    software: tk_net80211::SoftwareKey,
}

#[derive(Clone)]
struct PendingSmeStation {
    bss: axdriver_net::WirelessBssInfo,
    tx_sequence: tk_net80211::ManagementTxSequence,
    authentication_type: u32,
}

#[derive(Clone)]
struct StationConnection {
    bss: axdriver_net::WirelessBssInfo,
    bssid: [u8; 6],
    frequency_mhz: u32,
    signal_mbm: i32,
    association_id: u16,
    security_enabled: bool,
    mfp_negotiated: bool,
    disconnected: bool,
    request_ies: Vec<u8>,
    response_ies: Vec<u8>,
    tx_sequence: tk_net80211::ManagementTxSequence,
    data_sequence: u16,
    rx_ethernet: VecDeque<Vec<u8>>,
    nodes: tk_net80211::NodeTable,
}

#[derive(Debug)]
enum AliveWaitError {
    Interrupt(axdriver_iwx::IctError),
    Receive(axdriver_iwx::RxServiceError<Infallible>),
    Alive(axdriver_iwx::AliveError),
}

#[derive(Debug)]
enum FirmwareBootstrapError {
    Hardware(axdriver_iwx::ApmError),
    Nic(axdriver_iwx::ApmError),
    FirmwareStart(axdriver_iwx::ControllerUcodeStartError<AliveWaitError, AliveWaitError>),
    Interrupt(axdriver_iwx::IctError),
    CommandEncoding(axdriver_iwx::CommandError),
    Command(axdriver_iwx::SyncCommandError<Infallible>),
    InitCompleteTimeout,
    MacAddressUnavailable,
    MissingNvmResponse,
    Nvm(axdriver_iwx::NvmError),
    Stop(axdriver_iwx::StopDeviceError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStartError {
    DeviceNotFound,
    FirmwareNotReady,
    AlreadyStarted,
    Hardware,
    Nic,
    Firmware,
    ManagementQueue,
    SoftBlocked,
    Transmission,
    InvalidRequest,
    UnsupportedSecurity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirmwareRequestError {
    UcodeMissing,
    UcodeInvalid(FirmwareError),
    PnvmMissing,
}

static ATTACHED_DMA: Mutex<Vec<AttachedDevice>> = Mutex::new(Vec::new());
static ROOTFS_CALLBACK_REGISTERED: AtomicBool = AtomicBool::new(false);

/// The Ethernet-compatible interface owner; controller DMA and firmware are
/// retained in `ATTACHED_DMA` until PCI teardown exists.
struct IwxNetDevice {
    bdf: Bdf,
    buffers: alloc::sync::Arc<NetBufPool>,
}

impl IwxNetDevice {
    fn try_new(bdf: DeviceFunction) -> DevResult<Self> {
        let buffers = NetBufPool::new(128, 4096)?;
        Ok(Self {
            bdf: Bdf(bdf.bus, bdf.device, bdf.function),
            buffers,
        })
    }
}

impl BaseDriverOps for IwxNetDevice {
    fn device_name(&self) -> &str {
        "Intel iwx Wi-Fi"
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn irq_num(&self) -> Option<usize> {
        ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .and_then(|device| device.msix_vector)
    }
}

impl NetDriverOps for IwxNetDevice {
    fn interface_name(&self) -> Option<&'static str> {
        Some("wlan0")
    }

    fn is_wireless(&self) -> bool {
        true
    }

    fn rfkill_hard_blocked(&self) -> bool {
        ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .is_none_or(|device| device.controller.hardware_rfkill)
    }

    fn rfkill_soft_blocked(&self) -> bool {
        ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .is_none_or(|device| device.soft_blocked)
    }

    fn set_rfkill_soft_blocked(&mut self, blocked: bool) -> DevResult {
        set_soft_blocked(self.bdf, blocked).map_err(|_| DevError::BadState)
    }

    fn wireless_frequencies(&self) -> Vec<WirelessFrequency> {
        let devices = ATTACHED_DMA.lock();
        let Some(device) = devices.iter().find(|device| device.bdf == self.bdf) else {
            return Vec::new();
        };
        let Some(nvm) = device.nvm.as_ref() else {
            return Vec::new();
        };
        axdriver_iwx::init_channel_map(nvm, device.profile.uhb_supported)
            .into_iter()
            .filter(|channel| channel.flags != 0)
            .map(|channel| WirelessFrequency {
                frequency_mhz: u32::from(channel.frequency_mhz),
                no_ir: channel.flags & axdriver_iwx::CHAN_PASSIVE != 0,
            })
            .collect()
    }

    fn wireless_phy_capabilities(&self) -> WirelessPhyCapabilities {
        let devices = ATTACHED_DMA.lock();
        let Some(device) = devices.iter().find(|device| device.bdf == self.bdf) else {
            return WirelessPhyCapabilities::default();
        };
        let Some(preinit) = device.preinit.as_ref() else {
            return WirelessPhyCapabilities::default();
        };
        match preinit {
            PreinitPlan::Initialize {
                ht_rates,
                vht_rates,
                ..
            } => WirelessPhyCapabilities {
                ht: ht_rates.map(encode_ht_capabilities),
                vht: vht_rates.map(encode_vht_capabilities),
            },
            PreinitPlan::RefreshAddress(_) => WirelessPhyCapabilities::default(),
        }
    }

    fn set_link_up(&mut self, up: bool) -> DevResult {
        let bdf = DeviceFunction {
            bus: self.bdf.0,
            device: self.bdf.1,
            function: self.bdf.2,
        };
        let soft_blocked = ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .is_none_or(|device| device.soft_blocked);
        if up && !soft_blocked {
            start_runtime(bdf).map_err(|_| DevError::BadState)?;
        } else if !up {
            stop_runtime(bdf).map_err(|_| DevError::Io)?;
        }
        let mut devices = ATTACHED_DMA.lock();
        let device = devices
            .iter_mut()
            .find(|device| device.bdf == self.bdf)
            .ok_or(DevError::BadState)?;
        device.interface_up = up;
        Ok(())
    }

    fn trigger_wireless_scan(&mut self, request: &axdriver_net::WirelessScanRequest) -> DevResult {
        trigger_scan(self.bdf, request).map_err(|_| DevError::Io)
    }

    fn abort_wireless_scan(&mut self) -> DevResult {
        abort_scan(self.bdf).map_err(|_| DevError::Io)
    }

    fn connect_wireless(&mut self, request: &axdriver_net::WirelessConnectRequest) -> DevResult {
        connect_station(self.bdf, request).map_err(|error| match error {
            RuntimeStartError::FirmwareNotReady | RuntimeStartError::SoftBlocked => {
                DevError::BadState
            }
            RuntimeStartError::Transmission | RuntimeStartError::Firmware => DevError::Io,
            RuntimeStartError::InvalidRequest => DevError::InvalidParam,
            RuntimeStartError::UnsupportedSecurity => DevError::Unsupported,
            _ => DevError::Unsupported,
        })
    }

    fn authenticate_wireless(
        &mut self,
        request: &axdriver_net::WirelessAuthenticateRequest,
    ) -> DevResult<axdriver_net::WirelessSmeFrame> {
        authenticate_station_sme(self.bdf, request).map_err(map_wireless_sme_error)
    }

    fn associate_wireless(
        &mut self,
        request: &axdriver_net::WirelessAssociateRequest,
    ) -> DevResult<axdriver_net::WirelessSmeFrame> {
        associate_station_sme(self.bdf, request).map_err(map_wireless_sme_error)
    }

    fn disconnect_wireless_sme(&mut self, reason: u16, disassociate: bool) -> DevResult {
        disconnect_station_sme(self.bdf, reason, disassociate).map_err(map_wireless_sme_error)
    }

    fn wireless_station_info(&self) -> Option<axdriver_net::WirelessStationInfo> {
        ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .and_then(|device| device.station.as_ref())
            .filter(|station| !station.disconnected)
            .map(|station| axdriver_net::WirelessStationInfo {
                bssid: station.bssid,
                frequency_mhz: station.frequency_mhz,
                signal_mbm: station.signal_mbm,
                association_id: station.association_id,
                request_ies: station.request_ies.clone(),
                response_ies: station.response_ies.clone(),
            })
    }

    fn take_wireless_disconnect_event(&mut self) -> Option<WirelessDisconnectEvent> {
        ATTACHED_DMA
            .lock()
            .iter_mut()
            .find(|device| device.bdf == self.bdf)
            .and_then(|device| device.disconnect_event.take())
    }

    fn wireless_key_operation(
        &mut self,
        operation: WirelessKeyOperation,
        key: &WirelessKeyConfig,
    ) -> DevResult<Option<WirelessKeyInfo>> {
        iwx_key_operation(self.bdf, operation, key).map_err(|error| match error {
            RuntimeStartError::InvalidRequest => DevError::InvalidParam,
            RuntimeStartError::FirmwareNotReady => DevError::BadState,
            RuntimeStartError::UnsupportedSecurity => DevError::Unsupported,
            _ => DevError::Io,
        })
    }

    fn disconnect_wireless(&mut self, reason: u16) -> DevResult {
        disconnect_station(self.bdf, reason).map_err(|error| match error {
            RuntimeStartError::FirmwareNotReady => DevError::BadState,
            RuntimeStartError::Transmission | RuntimeStartError::Firmware => DevError::Io,
            _ => DevError::Unsupported,
        })
    }

    fn wireless_scan_results(&self) -> Vec<axdriver_net::WirelessBssInfo> {
        let mut devices = ATTACHED_DMA.lock();
        let Some(device) = devices.iter_mut().find(|device| device.bdf == self.bdf) else {
            return Vec::new();
        };
        if device.runtime_started {
            let _ = pump_scan_events(device);
        }
        device
            .scan_cache
            .as_ref()
            .map_or_else(Vec::new, |cache| cache.results().to_vec())
    }

    fn take_wireless_scan_event(&mut self) -> Option<WirelessScanEvent> {
        ATTACHED_DMA
            .lock()
            .iter_mut()
            .find(|device| device.bdf == self.bdf)
            .and_then(|device| device.scan_event.take())
    }

    fn mac_address(&self) -> EthernetAddress {
        let address = ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .and_then(|device| device.nvm.as_ref())
            .map_or([0; 6], |nvm| nvm.hardware_address);
        EthernetAddress(address)
    }

    fn can_transmit(&self) -> bool {
        ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .is_some_and(|device| {
                device.interface_up
                    && device.runtime_started
                    && !device.soft_blocked
                    && device
                        .station
                        .as_ref()
                        .is_some_and(|station| !station.disconnected)
            })
    }

    fn can_receive(&self) -> bool {
        ATTACHED_DMA
            .lock()
            .iter()
            .find(|device| device.bdf == self.bdf)
            .and_then(|device| device.station.as_ref())
            .is_some_and(|station| !station.disconnected && !station.rx_ethernet.is_empty())
    }

    fn rx_queue_size(&self) -> usize {
        128
    }

    fn tx_queue_size(&self) -> usize {
        128
    }

    fn recycle_rx_buffer(&mut self, rx_buf: NetBufPtr) -> DevResult {
        // SAFETY: receive() returns a pointer produced by this device's pool.
        drop(unsafe { NetBuf::from_buf_ptr(rx_buf) });
        Ok(())
    }

    fn recycle_tx_buffers(&mut self) -> DevResult {
        let mut devices = ATTACHED_DMA.lock();
        if let Some(device) = devices.iter_mut().find(|device| device.bdf == self.bdf) {
            pump_tx_completions(device).map_err(|_| DevError::Io)?;
        }
        Ok(())
    }

    fn transmit(&mut self, tx_buf: NetBufPtr) -> DevResult {
        let tx_buf = unsafe { NetBuf::from_buf_ptr(tx_buf) };
        let ethernet = tx_buf.packet_with_header();
        let mut devices = ATTACHED_DMA.lock();
        let device = devices
            .iter_mut()
            .find(|device| device.bdf == self.bdf)
            .ok_or(DevError::BadState)?;
        let station = device.station.as_mut().ok_or(DevError::BadState)?;
        if device.soft_blocked
            || !device.interface_up
            || !device.runtime_started
            || station.disconnected
        {
            return Err(DevError::BadState);
        }
        let mut frame =
            tk_net80211::encap_station(ethernet, station.bssid, station.data_sequence, false)
                .map_err(|_| DevError::InvalidParam)?;
        station.data_sequence = station.data_sequence.wrapping_add(1) & 0x0fff;
        let ether_type = u16::from_be_bytes([ethernet[12], ethernet[13]]);
        if station.security_enabled && ether_type != 0x888e {
            let key = if ethernet[0] & 1 != 0 {
                device.security_keys.groups[usize::from(device.security_keys.default_group)]
                    .as_mut()
            } else {
                device.security_keys.pairwise.as_mut()
            }
            .ok_or(DevError::BadState)?;
            frame = tk_net80211::encrypt_software(&mut key.software, &frame, 24, 0)
                .map_err(|_| DevError::Io)?;
        } else if station.security_enabled && ether_type == 0x888e {
            // EAPOL is the userspace supplicant's unprotected control port.
            frame[1] &= !0x40;
        }
        device
            .controller
            .submit_data_frame(
                IWX_STATION_DATA_QUEUE,
                axdriver_iwx::STA_ID_LINK,
                &frame[..24],
                &frame[24..],
                0,
                0,
                false,
            )
            .map_err(|_| DevError::ResourceBusy)?;
        drop(tx_buf);
        Ok(())
    }

    fn receive(&mut self) -> DevResult<NetBufPtr> {
        let mut devices = ATTACHED_DMA.lock();
        if let Some(device) = devices.iter_mut().find(|device| device.bdf == self.bdf) {
            if device.runtime_started {
                if device.scan_cache.is_some() && !device.scan_complete {
                    let _ = pump_scan_events(device);
                }
                let _ = pump_station_rx(device);
            }
            if let Some(frame) = device
                .station
                .as_mut()
                .and_then(|station| station.rx_ethernet.pop_front())
            {
                let mut buffer = self.buffers.alloc_boxed().ok_or(DevError::NoMemory)?;
                if frame.len() > buffer.capacity() {
                    return Err(DevError::InvalidParam);
                }
                buffer.set_packet_len(frame.len());
                buffer.packet_mut().copy_from_slice(&frame);
                return Ok(buffer.into_buf_ptr());
            }
        }
        Err(DevError::Again)
    }

    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<NetBufPtr> {
        if size == 0 || size > self.buffers.buffer_len() {
            return Err(DevError::InvalidParam);
        }
        let mut buffer = self.buffers.alloc_boxed().ok_or(DevError::NoMemory)?;
        buffer.set_packet_len(size);
        Ok(buffer.into_buf_ptr())
    }

    fn rx_poll_interval_micros(&self) -> Option<u64> {
        Some(if self.irq_num().is_some() {
            100_000
        } else {
            10_000
        })
    }
}

fn encode_ht_capabilities(rates: axdriver_iwx::HtRateCapabilities) -> WirelessHtCapabilities {
    let mut capability = 0x0020 | 0x0040 | 0x0002 | (3 << 2);
    capability |= u16::from(rates.rx_stbc_streams) << 8;
    if rates.tx_stbc {
        capability |= 0x0080;
    }
    let mut mcs_set = rates.supported_mcs;
    if rates.tx_mcs_set_defined {
        mcs_set[12] = 1;
    }
    WirelessHtCapabilities {
        capability,
        ampdu_parameters: (5 << 2) | 3,
        mcs_set,
    }
}

fn encode_vht_capabilities(rates: axdriver_iwx::VhtRateCapabilities) -> WirelessVhtCapabilities {
    let mut capability = (7u32 << 23) | (1 << 2) | 0x20 | 0x40;
    capability |= u32::from(rates.rx_stbc_streams) << 8;
    if rates.rx_antenna_pattern {
        capability |= 0x1000_0000;
    }
    if rates.tx_stbc {
        capability |= 0x80;
    } else if rates.tx_antenna_pattern {
        capability |= 0x2000_0000;
    }
    let mut mcs_set = [0u8; 8];
    mcs_set[..2].copy_from_slice(&rates.rx_mcs_map.to_le_bytes());
    mcs_set[4..6].copy_from_slice(&rates.tx_mcs_map.to_le_bytes());
    WirelessVhtCapabilities {
        capability,
        mcs_set,
    }
}

fn pci_match_decision(vendor_id: u16, device_id: u16, rf_id: Option<u32>) -> bool {
    if !matches_pci_device(vendor_id, device_id) {
        return false;
    }
    if device_id != SPECIAL_BZ_DEVICE {
        return true;
    }
    rf_id.is_some_and(|raw| {
        ((raw & CSR_HW_RFID_TYPE_MASK) >> CSR_HW_RFID_TYPE_SHIFT) as u16 == RF_TYPE_GF
    })
}

fn pcie_power_registers(root: &PciRoot, bdf: DeviceFunction) -> (u16, u16) {
    let Some(capability) = root
        .capabilities(bdf)
        .take(48)
        .find(|capability| capability.id == 0x10)
    else {
        return (0, 0);
    };
    let link_control = capability
        .offset
        .checked_add(0x10)
        .and_then(|offset| root.read_config_dword(bdf, offset))
        .map_or(0, |value| value as u16);
    let device_control2 = capability
        .offset
        .checked_add(0x28)
        .and_then(|offset| root.read_config_dword(bdf, offset))
        .map_or(0, |value| value as u16);
    (link_control, device_control2)
}

fn allocate_resources(
    bdf: DeviceFunction,
    profile: AttachProfile,
    runtime: RuntimeConfig,
    bar_base: usize,
    bar_size: usize,
    hardware_revision: u32,
    pcie_link_control: u16,
    pcie_device_control2: u16,
    msix_vector: Option<usize>,
) -> Result<(), AttachAllocationError> {
    let key = Bdf(bdf.bus, bdf.device, bdf.function);
    let mut attached = ATTACHED_DMA.lock();
    if attached.iter().any(|entry| entry.bdf == key) {
        return Ok(());
    }
    let mut controller = IwxController::attach(
        MmioCsrAccess {
            base: bar_base,
            size: bar_size,
        },
        PlatformDmaAllocator,
        profile.family,
        profile.umac_prph_offset,
        0,
    )?;
    controller.interrupt_masks.msix = msix_vector.is_some();
    controller.set_pcie_power_registers(pcie_link_control, pcie_device_control2);
    attached.try_reserve(1).map_err(|_| {
        AttachAllocationError::Allocation(
            axdriver_iwx::AttachAllocationStage::ContextInfo,
            DmaError::AllocationFailed,
        )
    })?;
    attached.push(AttachedDevice {
        bdf: key,
        profile,
        runtime,
        bar_base,
        bar_size,
        hardware_revision,
        msix_vector,
        controller,
        firmware: None,
        nvm: None,
        preinit: None,
        scan_cache: None,
        scan_phy: None,
        scan_complete: false,
        scan_event: None,
        association: axdriver_iwx::AssociationState::default(),
        session_protection: axdriver_iwx::SessionProtectionState::default(),
        beacon_filter: axdriver_iwx::BeaconFilterState::default(),
        security_keys: IwxSecurityKeys::default(),
        pending_sme: None,
        station: None,
        disconnect_event: None,
        runtime_started: false,
        interface_up: false,
        soft_blocked: false,
    });
    Ok(())
}

fn send_context_command(
    controller: &mut IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    command: &axdriver_iwx::EncodedCommand,
) -> Result<Option<Vec<u8>>, RuntimeStartError> {
    controller
        .send_encoded_command_wait(command, None, |_, _| Ok::<_, Infallible>(true))
        .map(|completed| completed.response)
        .map_err(|_| RuntimeStartError::Firmware)
}

fn clear_station_statistics(
    controller: &mut IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    command_version: u8,
) -> Result<(), RuntimeStartError> {
    let Some(command) =
        axdriver_iwx::statistics_clear_command(u32::from(command_version), 64, 0, 0)
            .map_err(|_| RuntimeStartError::Firmware)?
    else {
        return Ok(());
    };
    if command.flags & axdriver_iwx::CMD_ASYNC == 0 {
        send_context_command(controller, &command)?;
        return Ok(());
    }
    controller
        .send_encoded_command(&command, None)
        .map_err(|_| RuntimeStartError::Firmware)?;
    let notification = (u32::from(axdriver_iwx::STATISTICS_SYSTEM_GROUP) << 8)
        | u32::from(axdriver_iwx::SYSTEM_STATISTICS_END_NOTIFICATION);
    for _ in 0..1_000 {
        let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
            &mut controller.registers,
            &controller.interrupt_masks,
            Some(&mut controller.resources.ict),
        )
        .map_err(|_| RuntimeStartError::Firmware)?;
        let mut complete = false;
        controller
            .process_rx_notifications(|packet, _| {
                complete |= packet.is_notification() && packet.command_id() == notification;
                Ok::<_, Infallible>(true)
            })
            .map_err(|_| RuntimeStartError::Firmware)?;
        if complete {
            return Ok(());
        }
        controller.registers.delay_us(1_000);
    }
    Err(RuntimeStartError::Firmware)
}

fn station_mac_context(
    profile: AttachProfile,
    local_address: [u8; 6],
    bss: &axdriver_net::WirelessBssInfo,
    rates: &[u8],
    associated: bool,
    association_id: u16,
) -> axdriver_iwx::MacContextConfig {
    let is_24ghz = bss.frequency_mhz < 3000;
    let (cck_rates, ofdm_rates) = axdriver_iwx::ack_rate_masks(rates, is_24ghz);
    let capability = bss.capability;
    axdriver_iwx::MacContextConfig {
        action: axdriver_iwx::MAC_ACTION_ADD,
        id_and_color: 0,
        operation_mode: axdriver_iwx::OperationMode::Station,
        local_address,
        bssid: bss.bssid,
        cck_rates,
        ofdm_rates,
        short_preamble: capability & 0x0020 != 0,
        short_slot: capability & 0x0400 != 0,
        edca: [
            axdriver_iwx::EdcaParameters {
                ecw_min: 4,
                ecw_max: 10,
                aifsn: 3,
                txop_limit: 0,
            },
            axdriver_iwx::EdcaParameters {
                ecw_min: 4,
                ecw_max: 10,
                aifsn: 7,
                txop_limit: 0,
            },
            axdriver_iwx::EdcaParameters {
                ecw_min: 3,
                ecw_max: 4,
                aifsn: 2,
                txop_limit: 94,
            },
            axdriver_iwx::EdcaParameters {
                ecw_min: 2,
                ecw_max: 3,
                aifsn: 2,
                txop_limit: 47,
            },
        ],
        firmware_family_bz: profile.family >= axdriver_iwx::DeviceFamily::Bz,
        qos: false,
        ht: false,
        ht_protection: axdriver_iwx::HtProtection::None,
        channel_sco: 0,
        use_protection: false,
        associated,
        assoc_id: association_id,
        beacon_interval: bss.beacon_interval,
        dtim_count: 0,
        dtim_period: 0,
        receive_timestamp: 0,
        beacon_timestamp: bss.timestamp,
    }
}

/// Install the source `iwx_auth()` contexts and its reverse-order rollback.
fn authenticate_station_contexts(
    device: &mut AttachedDevice,
    bss: &axdriver_net::WirelessBssInfo,
    rates: &[u8],
) -> Result<(), RuntimeStartError> {
    let nvm = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?;
    let bundle = match device.firmware.as_ref() {
        Some(Ok(bundle)) => bundle,
        _ => return Err(RuntimeStartError::FirmwareNotReady),
    };
    let channel_info = axdriver_iwx::init_channel_map(nvm, device.profile.uhb_supported)
        .into_iter()
        .find(|channel| u32::from(channel.frequency_mhz) == bss.frequency_mhz)
        .ok_or(RuntimeStartError::Firmware)?;
    let is_24ghz = bss.frequency_mhz < 3000;
    let antennas = nvm.valid_rx_antennas.count_ones().clamp(1, 2) as u8;
    let valid_rx_antennas = nvm.valid_rx_antennas;
    let local_address = nvm.hardware_address;
    let cdb_supported = device.runtime.cdb != 0;
    let rlc_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::RLC_CONFIG_COMMAND,
    );
    let phy_command_version = bundle
        .image
        .lookup_command_version(0, axdriver_iwx::PHY_CONTEXT_COMMAND as u8);
    let queue_command_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::SCD_QUEUE_CONFIG_CMD,
    );
    let statistics_command_version = bundle
        .image
        .lookup_command_version(0, axdriver_iwx::STATISTICS_COMMAND);
    let channel = axdriver_iwx::PhyContextConfig {
        id_and_color: 0,
        action: axdriver_iwx::PHY_CONTEXT_ACTION_ADD,
        channel: channel_info.channel,
        is_24ghz,
        cdb_supported,
        ultra_high_band_channels: false,
        channel_40mhz: false,
        sco: 0,
        vht_width: 0,
        primary_channel_index: i16::from(channel_info.channel),
        center_channel_index: i16::from(channel_info.channel),
        static_chains: antennas,
        dynamic_chains: antennas,
        valid_rx_antennas,
        rlc_command_version: rlc_version,
        command_version: phy_command_version,
    };
    let mac = station_mac_context(device.profile, local_address, bss, rates, false, 0);
    let station = axdriver_iwx::StationAddConfig {
        use_mld_api: false,
        monitor_mode: false,
        update: false,
        mac_id_color: 0,
        address: bss.bssid,
        mimo_enabled: false,
        ht: false,
        vht: false,
        ht_stream2: false,
        ht_stream3: false,
        vht_stream2: false,
        channel_allows_40mhz: false,
        peer_supports_ht40: false,
        channel_allows_80mhz: false,
        peer_supports_vht80: false,
        channel_allows_160mhz: false,
        peer_supports_vht160: false,
        ht_ampdu_exponent: 0,
        vht_ampdu_exponent: 0,
        ampdu_density: 0,
        uapsd_node: false,
        uapsd_supported: false,
        uapsd_access_categories: 0,
        uapsd_max_service_period: 0,
    };
    let request = axdriver_iwx::AuthRequest {
        generation: device.association.generation.wrapping_add(1),
        monitor_mode: false,
        beacon_interval_tu: bss.beacon_interval,
        rlc_api_v2: rlc_version == axdriver_iwx::RLC_CONFIG_VERSION,
    };
    let (controller, state, session) = (
        &mut device.controller,
        &mut device.association,
        &mut device.session_protection,
    );
    let mut mac_active = false;
    axdriver_iwx::authenticate(state, request, |step| {
        match step {
            axdriver_iwx::AssociationStep::AddPhy | axdriver_iwx::AssociationStep::RemovePhy => {
                let mut config = channel;
                config.action = if step == axdriver_iwx::AssociationStep::AddPhy {
                    axdriver_iwx::PHY_CONTEXT_ACTION_ADD
                } else {
                    axdriver_iwx::PHY_CONTEXT_ACTION_REMOVE
                };
                let command = axdriver_iwx::phy_context_command(config, 0, 0)
                    .map_err(|_| RuntimeStartError::Firmware)?;
                send_context_command(controller, &command)?;
            }
            axdriver_iwx::AssociationStep::ConfigureRlc => {
                let command =
                    axdriver_iwx::rlc_config_command(0, valid_rx_antennas, antennas, antennas, 0)
                        .map_err(|_| RuntimeStartError::Firmware)?;
                send_context_command(controller, &command)?;
            }
            axdriver_iwx::AssociationStep::AddMac | axdriver_iwx::AssociationStep::RemoveMac => {
                let add = step == axdriver_iwx::AssociationStep::AddMac;
                let mut config = mac;
                config.action = if add {
                    axdriver_iwx::MAC_ACTION_ADD
                } else {
                    axdriver_iwx::MAC_ACTION_REMOVE
                };
                let command = axdriver_iwx::mac_context_command(&config, mac_active, 0, 0)
                    .map_err(|_| RuntimeStartError::Firmware)?;
                send_context_command(controller, &command)?;
                mac_active = add;
            }
            axdriver_iwx::AssociationStep::AddBinding
            | axdriver_iwx::AssociationStep::RemoveBinding => {
                let add = step == axdriver_iwx::AssociationStep::AddBinding;
                let command = axdriver_iwx::binding_command(
                    false,
                    if add {
                        axdriver_iwx::CONTEXT_ACTION_ADD
                    } else {
                        axdriver_iwx::CONTEXT_ACTION_REMOVE
                    },
                    0,
                    0,
                    is_24ghz,
                    cdb_supported,
                    0,
                    0,
                )
                .map_err(|_| RuntimeStartError::Firmware)?;
                if let Some(command) = command {
                    let response = send_context_command(controller, &command)?
                        .ok_or(RuntimeStartError::Firmware)?;
                    if axdriver_iwx::command_response_status(&response, false)
                        .map_err(|_| RuntimeStartError::Firmware)?
                        != 0
                    {
                        return Err(RuntimeStartError::Firmware);
                    }
                }
            }
            axdriver_iwx::AssociationStep::AddStation => {
                let command = axdriver_iwx::station_add_command(station, 0, 0)
                    .map_err(|_| RuntimeStartError::Firmware)?
                    .ok_or(RuntimeStartError::Firmware)?;
                let response = send_context_command(controller, &command)?
                    .ok_or(RuntimeStartError::Firmware)?;
                axdriver_iwx::validate_station_add_status(&response)
                    .map_err(|_| RuntimeStartError::Firmware)?;
            }
            axdriver_iwx::AssociationStep::RemoveStation => {
                let command = axdriver_iwx::remove_station_command(false, 0, 0)
                    .map_err(|_| RuntimeStartError::Firmware)?;
                send_context_command(controller, &command)?;
            }
            axdriver_iwx::AssociationStep::EnableManagementQueue => {
                controller
                    .enable_management_queue(queue_command_version)
                    .map_err(|_| RuntimeStartError::ManagementQueue)?;
            }
            axdriver_iwx::AssociationStep::DisableManagementQueue => {
                controller
                    .disable_management_queue(queue_command_version)
                    .map_err(|_| RuntimeStartError::ManagementQueue)?;
            }
            axdriver_iwx::AssociationStep::ClearStatistics => {
                clear_station_statistics(controller, statistics_command_version)?;
            }
            axdriver_iwx::AssociationStep::ProtectSession { duration_tu } => {
                axdriver_iwx::schedule_session_protection(session, 0, duration_tu, 0, |command| {
                    send_context_command(controller, command).map(|_| ())
                })
                .map_err(|_| RuntimeStartError::Firmware)?;
            }
            _ => return Err(RuntimeStartError::Firmware),
        }
        Ok::<_, RuntimeStartError>(())
    })
    .map_err(|_| RuntimeStartError::Firmware)?;
    Ok(())
}

fn bss_ssid(information_elements: &[u8]) -> Option<&[u8]> {
    let mut elements = information_elements;
    while elements.len() >= 2 {
        let length = usize::from(elements[1]);
        let end = 2usize.checked_add(length)?;
        let element = elements.get(..end)?;
        if element[0] == 0 {
            return Some(&element[2..]);
        }
        elements = &elements[end..];
    }
    None
}

fn is_open_station_request(request: &axdriver_net::WirelessConnectRequest) -> bool {
    !request.ssid.is_empty()
        && request.ssid.len() <= 32
        && request.authentication_type == 0
        && request.wpa_versions == 0
        && request.use_mfp == 0
        && request.pairwise_ciphers.is_empty()
        && request.group_cipher.is_none()
        && request.akm_suites.is_empty()
        && request.information_elements.is_empty()
}

fn find_information_element(elements: &[u8], id: u8) -> Option<&[u8]> {
    let mut remaining = elements;
    while remaining.len() >= 2 {
        let end = 2usize.checked_add(usize::from(remaining[1]))?;
        let element = remaining.get(..end)?;
        if element[0] == id {
            return Some(element);
        }
        remaining = &remaining[end..];
    }
    None
}

fn supplicant_rsn_policy(
    request: &axdriver_net::WirelessConnectRequest,
    bss: &axdriver_net::WirelessBssInfo,
) -> Result<tk_net80211::RsnIePolicy, RuntimeStartError> {
    const CCMP_128_SUITE: u32 = 0x000f_ac04;
    const AKM_PSK_SUITE: u32 = 0x000f_ac02;
    if request.wpa_versions & 2 == 0
        || request.wpa_versions & !3 != 0
        || request.use_mfp > 2
        || !request.pairwise_ciphers.contains(&CCMP_128_SUITE)
        || request.group_cipher != Some(CCMP_128_SUITE)
        || !request.akm_suites.contains(&AKM_PSK_SUITE)
    {
        return Err(RuntimeStartError::UnsupportedSecurity);
    }
    let rsn_ie = find_information_element(&request.information_elements, 48)
        .or_else(|| find_information_element(&bss.information_elements, 48))
        .ok_or(RuntimeStartError::UnsupportedSecurity)?;
    let parsed =
        tk_net80211::parse_rsn(rsn_ie).map_err(|_| RuntimeStartError::UnsupportedSecurity)?;
    if parsed.group_cipher != tk_net80211::Cipher::Ccmp
        || parsed.pairwise_ciphers & (tk_net80211::Cipher::Ccmp as u32) == 0
        || parsed.akms & (tk_net80211::Akm::Psk as u32) == 0
        || (request.use_mfp == 0 && parsed.capabilities & tk_net80211::RSNCAP_MFPR != 0)
        || (request.use_mfp == 1 && parsed.capabilities & tk_net80211::RSNCAP_MFPC == 0)
        || (parsed.capabilities & tk_net80211::RSNCAP_MFPC != 0
            && parsed.group_management_cipher != tk_net80211::Cipher::Bip)
    {
        return Err(RuntimeStartError::UnsupportedSecurity);
    }
    Ok(tk_net80211::RsnIePolicy {
        wpa: false,
        group_cipher: tk_net80211::Cipher::Ccmp as u32,
        pairwise_ciphers: tk_net80211::Cipher::Ccmp as u32,
        akms: tk_net80211::Akm::Psk as u32,
        peer_capabilities: parsed.capabilities,
        mfp_capable: request.use_mfp != 0,
        station_mode: true,
        mfp_required: request.use_mfp == 1,
        pbac: false,
        // PMKSA cache management is not part of this adapter yet.
        pmkid: None,
        group_management_cipher: if request.use_mfp != 0 {
            tk_net80211::IE_CIPHER_BIP
        } else {
            0
        },
    })
}

fn iwx_key_operation(
    bdf: Bdf,
    operation: WirelessKeyOperation,
    request: &WirelessKeyConfig,
) -> Result<Option<WirelessKeyInfo>, RuntimeStartError> {
    const CCMP_128_SUITE: u32 = 0x000f_ac04;
    const BIP_CMAC_128_SUITE: u32 = 0x000f_ac06;
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == bdf)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    let station = device
        .station
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?;
    let station_bssid = station.bssid;
    let station_mfp = station.mfp_negotiated;
    let station_security = station.security_enabled;
    let is_pairwise = request.peer.is_some();
    if request.index > 7 || request.peer.is_some_and(|peer| peer != station_bssid) {
        return Err(RuntimeStartError::InvalidRequest);
    }
    match operation {
        WirelessKeyOperation::Install => {
            if request.cipher_suite == BIP_CMAC_128_SUITE {
                if !station_mfp
                    || is_pairwise
                    || !(4..=7).contains(&request.index)
                    || request.key_data.len() != 16
                    || request.sequence.len() > 6
                {
                    return Err(RuntimeStartError::UnsupportedSecurity);
                }
                send_firmware_management_key(device, request, false)?;
                let mut software = tk_net80211::set_software_key(
                    tk_net80211::Cipher::Bip,
                    request.index,
                    &request.key_data,
                    true,
                )
                .map_err(|_| RuntimeStartError::InvalidRequest)?;
                if let tk_net80211::SoftwareKey::Bip(key) = &mut software {
                    let mut replay = [0u8; 8];
                    replay[..request.sequence.len()].copy_from_slice(&request.sequence);
                    key.management_replay_counter = u64::from_le_bytes(replay);
                }
                device.security_keys.management[usize::from(request.index - 4)] =
                    Some(IwxKeySlot {
                        index: request.index,
                        cipher_suite: request.cipher_suite,
                        software,
                    });
                return Ok(None);
            }
            if !station_security || request.index > 3 {
                return Err(RuntimeStartError::UnsupportedSecurity);
            }
            if request.cipher_suite != CCMP_128_SUITE {
                return Err(RuntimeStartError::UnsupportedSecurity);
            }
            let mut software = tk_net80211::set_software_key(
                tk_net80211::Cipher::Ccmp,
                request.index,
                &request.key_data,
                true,
            )
            .map_err(|_| RuntimeStartError::InvalidRequest)?;
            if request.sequence.len() > 8 {
                return Err(RuntimeStartError::InvalidRequest);
            }
            if let tk_net80211::SoftwareKey::Ccmp(key) = &mut software {
                let mut packet_number = [0u8; 8];
                packet_number[..request.sequence.len()].copy_from_slice(&request.sequence);
                key.tx_packet_number = u64::from_le_bytes(packet_number);
            }
            let slot = IwxKeySlot {
                index: request.index,
                cipher_suite: request.cipher_suite,
                software,
            };
            if is_pairwise {
                device.security_keys.pairwise = Some(slot);
            } else {
                device.security_keys.groups[usize::from(request.index)] = Some(slot);
            }
            Ok(None)
        }
        WirelessKeyOperation::SetDefault {
            unicast,
            multicast,
            management,
        } => {
            let management_slot = (4..=7)
                .contains(&request.index)
                .then(|| device.security_keys.management[usize::from(request.index - 4)].as_ref())
                .flatten()
                .filter(|slot| slot.index == request.index);
            if management && management_slot.is_none() {
                return Err(RuntimeStartError::FirmwareNotReady);
            }
            if unicast
                && device
                    .security_keys
                    .pairwise
                    .as_ref()
                    .is_none_or(|slot| slot.index != request.index)
            {
                return Err(RuntimeStartError::FirmwareNotReady);
            }
            if multicast
                && (request.index > 3
                    || device.security_keys.groups[usize::from(request.index)].is_none())
            {
                return Err(RuntimeStartError::FirmwareNotReady);
            }
            if management {
                device.security_keys.default_management = request.index;
            }
            if multicast {
                device.security_keys.default_group = request.index;
            }
            Ok(None)
        }
        WirelessKeyOperation::GetSequence => {
            let slot = if (4..=7).contains(&request.index) {
                device.security_keys.management[usize::from(request.index - 4)].as_ref()
            } else if request.index > 7 {
                None
            } else if is_pairwise {
                device.security_keys.pairwise.as_ref()
            } else {
                device.security_keys.groups[usize::from(request.index)].as_ref()
            }
            .filter(|slot| slot.index == request.index)
            .ok_or(RuntimeStartError::FirmwareNotReady)?;
            let mut sequence = Vec::new();
            match &slot.software {
                tk_net80211::SoftwareKey::Ccmp(key) => {
                    sequence.extend_from_slice(&key.tx_packet_number.to_le_bytes()[..6]);
                }
                tk_net80211::SoftwareKey::Bip(key) => {
                    sequence.extend_from_slice(&key.management_replay_counter.to_le_bytes()[..6]);
                }
                _ => return Err(RuntimeStartError::UnsupportedSecurity),
            }
            Ok(Some(WirelessKeyInfo {
                cipher_suite: slot.cipher_suite,
                sequence,
            }))
        }
        WirelessKeyOperation::Delete => {
            if (4..=7).contains(&request.index) {
                let slot = device.security_keys.management[usize::from(request.index - 4)]
                    .as_ref()
                    .filter(|slot| slot.index == request.index)
                    .ok_or(RuntimeStartError::FirmwareNotReady)?;
                let _ = slot;
                send_firmware_management_key(device, request, true)?;
                device.security_keys.management[usize::from(request.index - 4)] = None;
                if device.security_keys.default_management == request.index {
                    device.security_keys.default_management = 0;
                }
                return Ok(None);
            }
            if request.index > 3 {
                return Err(RuntimeStartError::InvalidRequest);
            }
            if is_pairwise {
                if device
                    .security_keys
                    .pairwise
                    .as_ref()
                    .is_some_and(|slot| slot.index != request.index)
                {
                    return Err(RuntimeStartError::InvalidRequest);
                }
                device.security_keys.pairwise = None;
            } else {
                device.security_keys.groups[usize::from(request.index)] = None;
                if device.security_keys.default_group == request.index {
                    device.security_keys.default_group = 0;
                }
            }
            Ok(None)
        }
    }
}

fn send_firmware_management_key(
    device: &mut AttachedDevice,
    request: &WirelessKeyConfig,
    remove: bool,
) -> Result<(), RuntimeStartError> {
    let bundle = device
        .firmware
        .as_ref()
        .and_then(|bundle| bundle.as_ref().ok())
        .ok_or(RuntimeStartError::FirmwareNotReady)?;
    let mut key_bytes = [0u8; 32];
    key_bytes[..request.key_data.len()].copy_from_slice(&request.key_data);
    let mut receive_sequence = [0u8; 8];
    receive_sequence[..request.sequence.len()].copy_from_slice(&request.sequence);
    let key = axdriver_iwx::KeyConfig {
        cipher: axdriver_iwx::KeyCipher::Bip,
        key_id: request.index,
        key: key_bytes,
        key_len: u8::try_from(request.key_data.len())
            .map_err(|_| RuntimeStartError::InvalidRequest)?,
        tx_sequence: 0,
        mgmt_rx_sequence: u64::from_le_bytes(receive_sequence),
        group: false,
        integrity_group: true,
        node_mfp: true,
        is_pairwise_key_slot: false,
    };
    let multi_queue_rx = bundle
        .image
        .enabled_capabilities
        .get(IWX_UCODE_TLV_CAPA_MULTI_QUEUE_RX_SUPPORT / 32)
        .is_some_and(|word| word & (1 << (IWX_UCODE_TLV_CAPA_MULTI_QUEUE_RX_SUPPORT % 32)) != 0);
    let version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::SEC_KEY_COMMAND as u8,
    );
    let command = if remove {
        axdriver_iwx::delete_key_command(
            &key,
            axdriver_iwx::STA_ID_LINK,
            device.runtime_started,
            version,
            multi_queue_rx,
            0,
        )
    } else {
        axdriver_iwx::station_key_command(
            &key,
            axdriver_iwx::STA_ID_LINK,
            version,
            multi_queue_rx,
            0,
        )
        .map(Some)
    }
    .map_err(|_| RuntimeStartError::Firmware)?;
    if let Some(command) = command {
        device
            .controller
            .send_encoded_command_wait(&command, None, |_, _| Ok::<_, Infallible>(true))
            .map_err(|_| RuntimeStartError::Firmware)?;
    }
    Ok(())
}

fn station_management_response(
    device: &mut AttachedDevice,
    bssid: [u8; 6],
    subtype: u8,
    mut rx_payloads: Vec<Vec<u8>>,
) -> Result<Vec<u8>, RuntimeStartError> {
    for _ in 0..500 {
        if rx_payloads.is_empty() {
            let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
                &mut device.controller.registers,
                &device.controller.interrupt_masks,
                Some(&mut device.controller.resources.ict),
            )
            .map_err(|_| RuntimeStartError::Transmission)?;
            device
                .controller
                .process_rx_notifications(|packet, _| {
                    if let axdriver_iwx::FirmwareEvent::RxMpdu(payload) =
                        axdriver_iwx::decode_firmware_event(packet)
                        && rx_payloads.try_reserve(1).is_ok()
                    {
                        rx_payloads.push(payload.to_vec());
                    }
                    Ok::<_, Infallible>(true)
                })
                .map_err(|_| RuntimeStartError::Transmission)?;
        }
        for payload in rx_payloads.drain(..) {
            if let Ok(axdriver_iwx::RxMpduOutcome::Deliver(received)) =
                device.controller.process_rx_mpdu(
                    &payload,
                    false,
                    axdriver_iwx::HardwareDecryptPolicy {
                        receive_protected: false,
                        pairwise_ccmp: false,
                        group_ccmp: false,
                    },
                )
                && received.frame.len() >= 24
                && received.frame[0] & 0x0c == 0
                && received.frame[0] & 0xf0 == subtype
                && received.frame[16..22] == bssid
            {
                return Ok(received.frame);
            }
        }
        device.controller.registers.delay_us(1_000);
    }
    Err(RuntimeStartError::Transmission)
}

const NL80211_AUTH_TYPE_OPEN: u32 = 0;
const NL80211_AUTH_TYPE_SAE: u32 = 4;
const NL80211_AKM_SAE: u32 = 0x000f_ac08;
const NL80211_AKM_PSK: u32 = 0x000f_ac02;
const NL80211_CIPHER_CCMP: u32 = 0x000f_ac04;

fn find_sme_bss(
    device: &AttachedDevice,
    bssid: Option<[u8; 6]>,
    frequency_mhz: Option<u32>,
    ssid: &[u8],
) -> Result<axdriver_net::WirelessBssInfo, RuntimeStartError> {
    device
        .scan_cache
        .as_ref()
        .and_then(|cache| {
            cache.results().iter().find(|bss| {
                bssid.is_none_or(|address| address == bss.bssid)
                    && frequency_mhz.is_none_or(|frequency| frequency == bss.frequency_mhz)
                    && (ssid.is_empty() || bss_ssid(&bss.information_elements) == Some(ssid))
            })
        })
        .cloned()
        .ok_or(RuntimeStartError::FirmwareNotReady)
}

/// `NL80211_CMD_AUTHENTICATE` sends one userspace-SME authentication frame and
/// returns the matching over-the-air response as an nl80211 event payload.
fn build_userspace_auth_body(
    request: &axdriver_net::WirelessAuthenticateRequest,
) -> Result<Vec<u8>, RuntimeStartError> {
    let algorithm = match request.authentication_type {
        NL80211_AUTH_TYPE_OPEN => 0u16,
        NL80211_AUTH_TYPE_SAE => 3u16,
        _ => return Err(RuntimeStartError::InvalidRequest),
    };
    if request.authentication_data.len() > 4096 || request.information_elements.len() > 4096 {
        return Err(RuntimeStartError::InvalidRequest);
    }
    let mut body = Vec::new();
    body.extend_from_slice(&algorithm.to_le_bytes());
    if request.authentication_data.is_empty() {
        if algorithm != 0 {
            return Err(RuntimeStartError::InvalidRequest);
        }
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
    } else {
        if request.authentication_data.len() < 4 {
            return Err(RuntimeStartError::InvalidRequest);
        }
        let transaction = u16::from_le_bytes(request.authentication_data[..2].try_into().unwrap());
        let status = u16::from_le_bytes(request.authentication_data[2..4].try_into().unwrap());
        if !matches!(transaction, 1 | 2) || status != 0 {
            return Err(RuntimeStartError::InvalidRequest);
        }
        body.extend_from_slice(&request.authentication_data);
    }
    validate_ie_sequence(&request.information_elements)?;
    body.extend_from_slice(&request.information_elements);
    Ok(body)
}

fn authenticate_station_sme(
    bdf: Bdf,
    request: &axdriver_net::WirelessAuthenticateRequest,
) -> Result<axdriver_net::WirelessSmeFrame, RuntimeStartError> {
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == bdf)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    if !device.interface_up || !device.runtime_started || device.soft_blocked {
        return Err(RuntimeStartError::FirmwareNotReady);
    }
    if device.station.is_some()
        || request.authentication_type != NL80211_AUTH_TYPE_SAE
            && request.authentication_type != NL80211_AUTH_TYPE_OPEN
        || request.ssid.len() > 32
        || request.information_elements.len() > 4096
        || request.authentication_data.len() > 4096
    {
        return Err(RuntimeStartError::InvalidRequest);
    }
    let pending = if let Some(pending) = device.pending_sme.clone() {
        if request
            .bssid
            .is_some_and(|bssid| bssid != pending.bss.bssid)
            || request
                .frequency_mhz
                .is_some_and(|frequency| frequency != pending.bss.frequency_mhz)
            || !request.ssid.is_empty()
                && bss_ssid(&pending.bss.information_elements) != Some(request.ssid.as_slice())
            || pending.authentication_type != request.authentication_type
        {
            return Err(RuntimeStartError::AlreadyStarted);
        }
        pending
    } else {
        let bss = find_sme_bss(device, request.bssid, request.frequency_mhz, &request.ssid)?;
        let rates = if bss.frequency_mhz < 3000 {
            tk_net80211::STANDARD_RATES_11G
        } else {
            tk_net80211::STANDARD_RATES_11A
        };
        authenticate_station_contexts(device, &bss, &rates.rates[..rates.count])?;
        let pending = PendingSmeStation {
            bss,
            tx_sequence: tk_net80211::ManagementTxSequence::new(0),
            authentication_type: request.authentication_type,
        };
        device.pending_sme = Some(pending.clone());
        pending
    };
    let local_address = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?
        .hardware_address;
    let body = build_userspace_auth_body(request)?;
    let mut tx_sequence = pending.tx_sequence;
    let frame = tx_sequence
        .frame(
            tk_net80211::MGMT_SUBTYPE_AUTH,
            pending.bss.bssid,
            local_address,
            pending.bss.bssid,
            &body,
            false,
            false,
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
    let mut early_responses = Vec::new();
    send_station_management_frame_locked(device, &frame, |packet| {
        if let axdriver_iwx::FirmwareEvent::RxMpdu(payload) =
            axdriver_iwx::decode_firmware_event(packet)
            && early_responses.try_reserve(1).is_ok()
        {
            early_responses.push(payload.to_vec());
        }
    })?;
    let response = station_management_response(device, pending.bss.bssid, 0xb0, early_responses)?;
    if let Some(pending) = device.pending_sme.as_mut() {
        pending.tx_sequence = tx_sequence;
    }
    Ok(axdriver_net::WirelessSmeFrame {
        bssid: pending.bss.bssid,
        frame: response,
    })
}

fn validate_ie_sequence(mut ies: &[u8]) -> Result<(), RuntimeStartError> {
    while !ies.is_empty() {
        if ies.len() < 2 {
            return Err(RuntimeStartError::InvalidRequest);
        }
        let end = 2usize
            .checked_add(usize::from(ies[1]))
            .filter(|end| *end <= ies.len())
            .ok_or(RuntimeStartError::InvalidRequest)?;
        ies = &ies[end..];
    }
    Ok(())
}

fn build_userspace_assoc_body(
    bss: &axdriver_net::WirelessBssInfo,
    ssid: &[u8],
    ies: &[u8],
    security_enabled: bool,
) -> Result<Vec<u8>, RuntimeStartError> {
    validate_ie_sequence(ies)?;
    let rates = if bss.frequency_mhz < 3000 {
        tk_net80211::STANDARD_RATES_11G
    } else {
        tk_net80211::STANDARD_RATES_11A
    };
    let mut body = Vec::new();
    let mut capability = tk_net80211::OUTPUT_CAPINFO_ESS;
    if security_enabled {
        capability |= tk_net80211::OUTPUT_CAPINFO_PRIVACY;
    }
    if bss.capability & 0x0020 != 0 && bss.frequency_mhz < 3000 {
        capability |= tk_net80211::CAPINFO_SHORT_PREAMBLE;
    }
    if bss.capability & 0x0400 != 0 {
        capability |= tk_net80211::CAPINFO_SHORT_SLOTTIME;
    }
    body.extend_from_slice(&capability.to_le_bytes());
    body.extend_from_slice(&10u16.to_le_bytes());
    tk_net80211::append_ssid_ie(&mut body, ssid).map_err(|_| RuntimeStartError::InvalidRequest)?;
    tk_net80211::append_supported_rates_ie(&mut body, &rates)
        .map_err(|_| RuntimeStartError::InvalidRequest)?;
    if rates.count > 8 {
        tk_net80211::append_extended_rates_ie(&mut body, &rates)
            .map_err(|_| RuntimeStartError::InvalidRequest)?;
    }
    body.extend_from_slice(ies);
    Ok(body)
}

fn sme_rsn_valid(
    ies: &[u8],
    use_mfp: u32,
    pairwise_ciphers: &[u32],
    group_cipher: Option<u32>,
    akm_suites: &[u32],
) -> Option<(bool, bool)> {
    let rsn_ie = find_information_element(ies, 48)?;
    let rsn = tk_net80211::parse_rsn(rsn_ie).ok()?;
    let sae = akm_suites.contains(&NL80211_AKM_SAE);
    let psk = akm_suites.contains(&NL80211_AKM_PSK);
    let peer_mfpc = rsn.capabilities & tk_net80211::RSNCAP_MFPC != 0;
    let peer_mfpr = rsn.capabilities & tk_net80211::RSNCAP_MFPR != 0;
    let cipher_ok = rsn.group_cipher == tk_net80211::Cipher::Ccmp
        && rsn.pairwise_ciphers & tk_net80211::Cipher::Ccmp as u32 != 0
        && rsn.group_management_cipher == tk_net80211::Cipher::Bip
        && pairwise_ciphers.contains(&NL80211_CIPHER_CCMP)
        && group_cipher == Some(NL80211_CIPHER_CCMP);
    if !cipher_ok
        || !(sae ^ psk)
        || akm_suites.len() != 1
        || sae && rsn.akms & tk_net80211::Akm::Sae as u32 == 0
        || psk && rsn.akms & tk_net80211::Akm::Psk as u32 == 0
    {
        return None;
    }
    if sae && (use_mfp != 1 || !peer_mfpc || !peer_mfpr) {
        return None;
    }
    if use_mfp == 1 && !peer_mfpc || use_mfp == 0 && peer_mfpr {
        return None;
    }
    Some((true, use_mfp == 1 && peer_mfpc))
}

/// `NL80211_CMD_ASSOCIATE` serializes the userspace RSN IE, completes the
/// on-air exchange, installs the authorized station context, and returns the
/// full response frame to the `mlme` event publisher.
fn associate_station_sme(
    bdf: Bdf,
    request: &axdriver_net::WirelessAssociateRequest,
) -> Result<axdriver_net::WirelessSmeFrame, RuntimeStartError> {
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == bdf)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    if !device.interface_up || !device.runtime_started || device.soft_blocked {
        return Err(RuntimeStartError::FirmwareNotReady);
    }
    if device.station.is_some()
        || request.ssid.is_empty()
        || request.ssid.len() > 32
        || request.information_elements.is_empty()
        || request.information_elements.len() > 4096
        || request.use_mfp > 1
    {
        return Err(RuntimeStartError::InvalidRequest);
    }
    let pending = device
        .pending_sme
        .clone()
        .ok_or(RuntimeStartError::FirmwareNotReady)?;
    if request
        .bssid
        .is_some_and(|bssid| bssid != pending.bss.bssid)
        || request
            .frequency_mhz
            .is_some_and(|frequency| frequency != pending.bss.frequency_mhz)
        || bss_ssid(&pending.bss.information_elements) != Some(request.ssid.as_slice())
    {
        return Err(RuntimeStartError::InvalidRequest);
    }
    let Some((security_enabled, mfp_negotiated)) = sme_rsn_valid(
        &request.information_elements,
        request.use_mfp,
        &request.pairwise_ciphers,
        request.group_cipher,
        &request.akm_suites,
    ) else {
        return Err(RuntimeStartError::UnsupportedSecurity);
    };
    if (request.akm_suites.contains(&NL80211_AKM_SAE)
        && pending.authentication_type != NL80211_AUTH_TYPE_SAE)
        || (request.akm_suites.contains(&NL80211_AKM_PSK)
            && pending.authentication_type != NL80211_AUTH_TYPE_OPEN)
    {
        return Err(RuntimeStartError::InvalidRequest);
    }
    let local_address = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?
        .hardware_address;
    let body = build_userspace_assoc_body(
        &pending.bss,
        &request.ssid,
        &request.information_elements,
        true,
    )?;
    let mut tx_sequence = pending.tx_sequence;
    let frame = tx_sequence
        .frame(
            tk_net80211::MGMT_SUBTYPE_ASSOC_REQ,
            pending.bss.bssid,
            local_address,
            pending.bss.bssid,
            &body,
            false,
            false,
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
    let mut early_responses = Vec::new();
    send_station_management_frame_locked(device, &frame, |packet| {
        if let axdriver_iwx::FirmwareEvent::RxMpdu(payload) =
            axdriver_iwx::decode_firmware_event(packet)
            && early_responses.try_reserve(1).is_ok()
        {
            early_responses.push(payload.to_vec());
        }
    })?;
    let response = station_management_response(device, pending.bss.bssid, 0x10, early_responses)?;
    if response.len() < 30 {
        return Err(RuntimeStartError::Transmission);
    }
    let status = u16::from_le_bytes([response[26], response[27]]);
    if status != 0 {
        if let Some(pending) = device.pending_sme.as_mut() {
            pending.tx_sequence = tx_sequence;
        }
        return Ok(axdriver_net::WirelessSmeFrame {
            bssid: pending.bss.bssid,
            frame: response,
        });
    }
    let response_ies = response[30..].to_vec();
    let Some((_, response_mfp)) = sme_rsn_valid(
        &response_ies,
        request.use_mfp,
        &request.pairwise_ciphers,
        request.group_cipher,
        &request.akm_suites,
    ) else {
        return Err(RuntimeStartError::UnsupportedSecurity);
    };
    if response_mfp != mfp_negotiated {
        return Err(RuntimeStartError::UnsupportedSecurity);
    }
    let association_id = u16::from_le_bytes([response[28], response[29]]) & 0x3fff;
    let rates = if pending.bss.frequency_mhz < 3000 {
        tk_net80211::STANDARD_RATES_11G
    } else {
        tk_net80211::STANDARD_RATES_11A
    };
    let local_phy = tk_net80211::LocalPhyConfig {
        modecaps: 0,
        ht_enabled: false,
        vht_enabled: false,
        he_enabled: false,
        channel_is_2ghz: pending.bss.frequency_mhz < 3000,
        channel_is_5ghz: (3000..=5925).contains(&pending.bss.frequency_mhz),
        channel_supports_ac: false,
        channel_supports_he: false,
        station_mode: true,
        wep_enabled: false,
        rsn_enabled: true,
        ht_caps: tk_net80211::HtCapabilities::default(),
        supported_ht_mcs: [0; 10],
        vht_caps: tk_net80211::VhtCapabilities::default(),
        he_caps: tk_net80211::HeCapabilities::default(),
    };
    let mut nodes = tk_net80211::NodeTable::default();
    let result = tk_net80211::receive_assoc_response(
        &mut nodes,
        &response,
        &tk_net80211::AssocRxPolicy {
            station_mode: true,
            state: tk_net80211::ProtocolState::Assoc,
            reassociation: false,
            local_rates: &rates,
            fixed_rate: None,
            local_phy,
            local_channel_160_allowed: false,
            pairwise_ciphers: tk_net80211::Cipher::Ccmp as u32,
            rsn_enabled: true,
            wep_enabled: false,
            local_qos_enabled: false,
            local_uapsd_enabled: false,
            local_uapsd_access_categories: 0,
            local_uapsd_max_service_period: 0,
        },
    )
    .map_err(|_| RuntimeStartError::Transmission)?;
    if result.status != 0 {
        return Err(RuntimeStartError::Firmware);
    }
    finish_station_association(
        device,
        &pending.bss,
        association_id,
        security_enabled,
        mfp_negotiated,
        body.get(4..).unwrap_or_default().to_vec(),
        response_ies,
        tx_sequence,
        nodes,
    )?;
    device.pending_sme = None;
    Ok(axdriver_net::WirelessSmeFrame {
        bssid: pending.bss.bssid,
        frame: response,
    })
}

fn disconnect_station_sme(
    bdf: Bdf,
    reason: u16,
    disassociate: bool,
) -> Result<(), RuntimeStartError> {
    let subtype = if disassociate {
        tk_net80211::MGMT_SUBTYPE_DISASSOC
    } else {
        tk_net80211::MGMT_SUBTYPE_DEAUTH
    };
    disconnect_station_with_subtype(bdf, reason, subtype)
}

/// Connect to a cached open or WPA2-PSK/CCMP BSS through Open System auth and
/// station association. The userspace supplicant performs the EAPOL handshake.
fn connect_station(
    bdf: Bdf,
    request: &axdriver_net::WirelessConnectRequest,
) -> Result<(), RuntimeStartError> {
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == bdf)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    if !device.interface_up || !device.runtime_started || device.soft_blocked {
        return Err(RuntimeStartError::FirmwareNotReady);
    }
    if device.station.is_some() {
        return Err(RuntimeStartError::AlreadyStarted);
    }
    if request.ssid.is_empty()
        || request.ssid.len() > 32
        || request.authentication_type != 0
        || request.use_mfp > 2
    {
        return Err(RuntimeStartError::InvalidRequest);
    }
    let security_enabled = !is_open_station_request(request);
    let bss = device
        .scan_cache
        .as_ref()
        .and_then(|cache| {
            cache.results().iter().find(|bss| {
                request.bssid.is_none_or(|address| address == bss.bssid)
                    && request
                        .frequency_mhz
                        .is_none_or(|frequency| frequency == bss.frequency_mhz)
                    && bss_ssid(&bss.information_elements) == Some(request.ssid.as_slice())
                    && if security_enabled {
                        bss.capability & 0x0010 != 0
                    } else {
                        bss.capability & 0x0010 == 0
                    }
            })
        })
        .cloned()
        .ok_or(RuntimeStartError::FirmwareNotReady)?;
    let rsn_policy = if security_enabled {
        Some(supplicant_rsn_policy(request, &bss)?)
    } else {
        None
    };
    let rates = if bss.frequency_mhz < 3000 {
        tk_net80211::STANDARD_RATES_11G
    } else {
        tk_net80211::STANDARD_RATES_11A
    };
    authenticate_station_contexts(device, &bss, &rates.rates[..rates.count])?;

    let local_address = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?
        .hardware_address;
    let mut tx_sequence = tk_net80211::ManagementTxSequence::new(0);
    let auth_body = tk_net80211::build_auth_body(1, 0);
    let auth_frame = tx_sequence
        .frame(
            0xb0,
            bss.bssid,
            local_address,
            bss.bssid,
            &auth_body,
            false,
            false,
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
    let mut early_auth_responses = Vec::new();
    send_station_management_frame_locked(device, &auth_frame, |packet| {
        if let axdriver_iwx::FirmwareEvent::RxMpdu(payload) =
            axdriver_iwx::decode_firmware_event(packet)
            && early_auth_responses.try_reserve(1).is_ok()
        {
            early_auth_responses.push(payload.to_vec());
        }
    })?;
    let auth_response = station_management_response(device, bss.bssid, 0xb0, early_auth_responses)?;
    let mut auth_state = tk_net80211::OpenAuthState {
        authentication_state: true,
        rsn_enabled: security_enabled,
        is_bss_node: true,
        sequence: 0,
        status: 0,
        auth_subtype: 0,
        node_failures: 0,
        bad_auth_count: 0,
        auth_fail_count: 0,
    };
    let auth = tk_net80211::receive_auth_response(&auth_response, &mut auth_state)
        .map_err(|_| RuntimeStartError::Transmission)?;
    if auth.sequence != 2 || auth.status != 0 {
        return Err(RuntimeStartError::Firmware);
    }

    let request_body = tk_net80211::build_assoc_request_body(&tk_net80211::AssocRequestConfig {
        ssid: &request.ssid,
        rates: &rates,
        listen_interval: 10,
        reassociation_bssid: None,
        channel_is_2ghz: bss.frequency_mhz < 3000,
        channel_is_5ghz: (3000..=5925).contains(&bss.frequency_mhz),
        channel_is_ac: false,
        channel_is_he: false,
        ht_enabled: false,
        vht_enabled: false,
        he_enabled: false,
        he_mode_supported: false,
        privacy_wep: security_enabled,
        rsn_enabled: security_enabled,
        peer_rsn_protocols: if security_enabled {
            tk_net80211::PROTO_RSN
        } else {
            0
        },
        short_preamble: bss.capability & 0x0020 != 0,
        short_slot: bss.capability & 0x0400 != 0,
        peer_qos: false,
        qos_info: 0,
        rsn: rsn_policy,
        ht_caps: None,
        vht_caps: None,
        he_caps: None,
    })
    .map_err(|_| RuntimeStartError::Firmware)?;
    let request_ies = request_body.get(4..).unwrap_or_default().to_vec();
    let assoc_frame = tx_sequence
        .frame(
            0x00,
            bss.bssid,
            local_address,
            bss.bssid,
            &request_body,
            false,
            false,
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
    let mut early_assoc_responses = Vec::new();
    send_station_management_frame_locked(device, &assoc_frame, |packet| {
        if let axdriver_iwx::FirmwareEvent::RxMpdu(payload) =
            axdriver_iwx::decode_firmware_event(packet)
            && early_assoc_responses.try_reserve(1).is_ok()
        {
            early_assoc_responses.push(payload.to_vec());
        }
    })?;
    let assoc_response =
        station_management_response(device, bss.bssid, 0x10, early_assoc_responses)?;
    let response_ies = assoc_response.get(30..).unwrap_or_default().to_vec();
    let mut nodes = tk_net80211::NodeTable::default();
    let local_phy = tk_net80211::LocalPhyConfig {
        modecaps: 0,
        ht_enabled: false,
        vht_enabled: false,
        he_enabled: false,
        channel_is_2ghz: bss.frequency_mhz < 3000,
        channel_is_5ghz: (3000..=5925).contains(&bss.frequency_mhz),
        channel_supports_ac: false,
        channel_supports_he: false,
        station_mode: true,
        wep_enabled: false,
        rsn_enabled: security_enabled,
        ht_caps: tk_net80211::HtCapabilities::default(),
        supported_ht_mcs: [0; 10],
        vht_caps: tk_net80211::VhtCapabilities::default(),
        he_caps: tk_net80211::HeCapabilities::default(),
    };
    let assoc = tk_net80211::receive_assoc_response(
        &mut nodes,
        &assoc_response,
        &tk_net80211::AssocRxPolicy {
            station_mode: true,
            state: tk_net80211::ProtocolState::Assoc,
            reassociation: false,
            local_rates: &rates,
            fixed_rate: None,
            local_phy,
            local_channel_160_allowed: false,
            pairwise_ciphers: if security_enabled {
                tk_net80211::Cipher::Ccmp as u32
            } else {
                0
            },
            rsn_enabled: security_enabled,
            wep_enabled: false,
            local_qos_enabled: false,
            local_uapsd_enabled: false,
            local_uapsd_access_categories: 0,
            local_uapsd_max_service_period: 0,
        },
    )
    .map_err(|_| RuntimeStartError::Transmission)?;
    if assoc.status != 0 {
        return Err(RuntimeStartError::Firmware);
    }
    finish_station_association(
        device,
        &bss,
        assoc.association_id & 0x3fff,
        security_enabled,
        rsn_policy.is_some_and(|policy| {
            policy.mfp_capable && policy.peer_capabilities & tk_net80211::RSNCAP_MFPC != 0
        }),
        request_ies,
        response_ies,
        tx_sequence,
        nodes,
    )
}

fn finish_station_association(
    device: &mut AttachedDevice,
    bss: &axdriver_net::WirelessBssInfo,
    association_id: u16,
    security_enabled: bool,
    mfp_negotiated: bool,
    request_ies: Vec<u8>,
    response_ies: Vec<u8>,
    tx_sequence: tk_net80211::ManagementTxSequence,
    nodes: tk_net80211::NodeTable,
) -> Result<(), RuntimeStartError> {
    let local_address = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?
        .hardware_address;
    let rates = if bss.frequency_mhz < 3000 {
        tk_net80211::STANDARD_RATES_11G
    } else {
        tk_net80211::STANDARD_RATES_11A
    };
    let mut mac = station_mac_context(
        device.profile,
        local_address,
        &bss,
        &rates.rates[..rates.count],
        true,
        association_id,
    );
    mac.action = axdriver_iwx::MAC_ACTION_MODIFY;
    let bundle = match device.firmware.as_ref() {
        Some(Ok(bundle)) => bundle,
        _ => return Err(RuntimeStartError::FirmwareNotReady),
    };
    let version = bundle
        .image
        .lookup_command_version(0, axdriver_iwx::MAC_CONTEXT_COMMAND as u8);
    let command = axdriver_iwx::mac_context_command(&mac, true, version, 0)
        .map_err(|_| RuntimeStartError::Firmware)?;
    send_context_command(&mut device.controller, &command)?;
    let ring = device
        .controller
        .resources
        .tx_queues
        .get(usize::from(IWX_STATION_DATA_QUEUE))
        .ok_or(RuntimeStartError::Transmission)?;
    let queue_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::SCD_QUEUE_CONFIG_CMD,
    );
    device
        .controller
        .enable_tx_queue(
            axdriver_iwx::QueueConfig {
                station_id: axdriver_iwx::STA_ID_LINK,
                queue_id: IWX_STATION_DATA_QUEUE,
                tid: 0,
                ring_size: axdriver_iwx::DEFAULT_QUEUE_SIZE,
                byte_count_address: ring.byte_counts.device_address(),
                tfd_address: ring.descriptors.device_address(),
            },
            queue_version,
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
    let power = axdriver_iwx::build_power_commands(
        axdriver_iwx::default_station_power_config(0, bss.beacon_interval),
        0,
        0,
    )
    .map_err(|_| RuntimeStartError::Firmware)?;
    if let Some(power) = power.as_ref() {
        device
            .controller
            .send_encoded_command(&power.device, None)
            .map_err(|_| RuntimeStartError::Firmware)?;
        if let Some(mac_power) = &power.mac {
            device
                .controller
                .send_encoded_command(mac_power, None)
                .map_err(|_| RuntimeStartError::Firmware)?;
        }
        axdriver_iwx::update_beacon_abort(
            &mut device.beacon_filter,
            power.beacon_abort_enabled,
            0,
            0,
            |command| {
                device
                    .controller
                    .send_encoded_command(command, None)
                    .map(|_| ())
                    .map_err(|_| RuntimeStartError::Firmware)
            },
        )
        .map_err(|_| RuntimeStartError::Firmware)?;
    }
    device.station = Some(StationConnection {
        bss: bss.clone(),
        bssid: bss.bssid,
        frequency_mhz: bss.frequency_mhz,
        signal_mbm: bss.signal_mbm,
        association_id,
        security_enabled,
        mfp_negotiated,
        disconnected: false,
        request_ies,
        response_ies,
        tx_sequence,
        data_sequence: 0,
        rx_ethernet: VecDeque::new(),
        nodes,
    });
    Ok(())
}

fn disconnect_pending_sme(
    device: &mut AttachedDevice,
    pending: PendingSmeStation,
    reason: u16,
    subtype: u8,
) -> Result<(), RuntimeStartError> {
    if !device.runtime_started || device.soft_blocked {
        device.pending_sme = None;
        device.association = axdriver_iwx::AssociationState::default();
        return Ok(());
    }
    let local_address = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?
        .hardware_address;
    let mut tx_sequence = pending.tx_sequence;
    let body = reason.to_le_bytes();
    let frame = tx_sequence
        .frame(
            subtype,
            pending.bss.bssid,
            local_address,
            pending.bss.bssid,
            &body,
            false,
            false,
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
    send_station_management_frame_locked(device, &frame, |_| {})?;

    let bundle = match device.firmware.as_ref() {
        Some(Ok(bundle)) => bundle,
        _ => return Err(RuntimeStartError::FirmwareNotReady),
    };
    let nvm = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?;
    let channel = axdriver_iwx::init_channel_map(nvm, device.profile.uhb_supported)
        .into_iter()
        .find(|channel| u32::from(channel.frequency_mhz) == pending.bss.frequency_mhz)
        .ok_or(RuntimeStartError::Firmware)?;
    let antennas = nvm.valid_rx_antennas.count_ones().clamp(1, 2) as u8;
    let phy = axdriver_iwx::PhyContextConfig {
        id_and_color: 0,
        action: axdriver_iwx::PHY_CONTEXT_ACTION_REMOVE,
        channel: channel.channel,
        is_24ghz: pending.bss.frequency_mhz < 3000,
        cdb_supported: device.runtime.cdb != 0,
        ultra_high_band_channels: false,
        channel_40mhz: false,
        sco: 0,
        vht_width: 0,
        primary_channel_index: i16::from(channel.channel),
        center_channel_index: i16::from(channel.channel),
        static_chains: antennas,
        dynamic_chains: antennas,
        valid_rx_antennas: nvm.valid_rx_antennas,
        rlc_command_version: bundle.image.lookup_command_version(
            axdriver_iwx::DATA_PATH_GROUP,
            axdriver_iwx::RLC_CONFIG_COMMAND,
        ),
        command_version: bundle
            .image
            .lookup_command_version(0, axdriver_iwx::PHY_CONTEXT_COMMAND as u8),
    };
    let rates = if pending.bss.frequency_mhz < 3000 {
        tk_net80211::STANDARD_RATES_11G
    } else {
        tk_net80211::STANDARD_RATES_11A
    };
    let mut mac = station_mac_context(
        device.profile,
        local_address,
        &pending.bss,
        &rates.rates[..rates.count],
        false,
        0,
    );
    mac.action = axdriver_iwx::MAC_ACTION_REMOVE;
    let mac_version = bundle
        .image
        .lookup_command_version(0, axdriver_iwx::MAC_CONTEXT_COMMAND as u8);
    let binding_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::BINDING_CONTEXT_COMMAND as u8,
    );
    axdriver_iwx::deauthenticate(&mut device.association, |step| match step {
        axdriver_iwx::AssociationStep::UnprotectSession => Ok(()),
        axdriver_iwx::AssociationStep::RemoveStation => {
            let command = axdriver_iwx::remove_station_command(false, 0, 0)
                .map_err(|_| RuntimeStartError::Firmware)?;
            send_context_command(&mut device.controller, &command)?;
            Ok(())
        }
        axdriver_iwx::AssociationStep::RemoveBinding => {
            if let Some(command) = axdriver_iwx::binding_command(
                false,
                axdriver_iwx::CONTEXT_ACTION_REMOVE,
                0,
                0,
                pending.bss.frequency_mhz < 3000,
                device.runtime.cdb != 0,
                binding_version,
                0,
            )
            .map_err(|_| RuntimeStartError::Firmware)?
            {
                let response = send_context_command(&mut device.controller, &command)?
                    .ok_or(RuntimeStartError::Firmware)?;
                if axdriver_iwx::command_response_status(&response, false)
                    .map_err(|_| RuntimeStartError::Firmware)?
                    != 0
                {
                    return Err(RuntimeStartError::Firmware);
                }
            }
            Ok(())
        }
        axdriver_iwx::AssociationStep::RemoveMac => {
            let command = axdriver_iwx::mac_context_command(&mac, true, mac_version, 0)
                .map_err(|_| RuntimeStartError::Firmware)?;
            send_context_command(&mut device.controller, &command)?;
            Ok(())
        }
        axdriver_iwx::AssociationStep::RemovePhy => {
            let command = axdriver_iwx::phy_context_command(phy, 0, 0)
                .map_err(|_| RuntimeStartError::Firmware)?;
            send_context_command(&mut device.controller, &command)?;
            Ok(())
        }
        _ => Err(RuntimeStartError::Firmware),
    })
    .map_err(|_| RuntimeStartError::Firmware)?;
    let queue_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::SCD_QUEUE_CONFIG_CMD,
    );
    device
        .controller
        .disable_management_queue(queue_version)
        .map_err(|_| RuntimeStartError::ManagementQueue)?;
    device.pending_sme = None;
    device.security_keys = IwxSecurityKeys::default();
    device.association = axdriver_iwx::AssociationState::default();
    device.session_protection = axdriver_iwx::SessionProtectionState::default();
    Ok(())
}

fn disconnect_station(bdf: Bdf, reason: u16) -> Result<(), RuntimeStartError> {
    disconnect_station_with_subtype(bdf, reason, tk_net80211::MGMT_SUBTYPE_DEAUTH)
}

fn disconnect_station_with_subtype(
    bdf: Bdf,
    reason: u16,
    subtype: u8,
) -> Result<(), RuntimeStartError> {
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == bdf)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    let Some(station) = device.station.clone() else {
        if let Some(pending) = device.pending_sme.clone() {
            return disconnect_pending_sme(device, pending, reason, subtype);
        }
        return Ok(());
    };
    if !device.runtime_started || device.soft_blocked {
        device.station = None;
        device.security_keys = IwxSecurityKeys::default();
        return Ok(());
    }

    let local_address = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?
        .hardware_address;
    let mut tx_sequence = station.tx_sequence;
    let body = reason.to_le_bytes();
    let protect_management = station.mfp_negotiated
        && device.security_keys.pairwise.is_some()
        && device.security_keys.management.iter().any(Option::is_some);
    if !station.disconnected {
        let frame = tx_sequence
            .frame(
                subtype,
                station.bssid,
                local_address,
                station.bssid,
                &body,
                station.mfp_negotiated,
                protect_management,
            )
            .map_err(|_| RuntimeStartError::Transmission)?;
        send_station_management_frame_locked(device, &frame, |_| {})?;
    }

    let bundle = match device.firmware.as_ref() {
        Some(Ok(bundle)) => bundle,
        _ => return Err(RuntimeStartError::FirmwareNotReady),
    };
    let queue_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::SCD_QUEUE_CONFIG_CMD,
    );
    let queue_ring = device
        .controller
        .resources
        .tx_queues
        .get(usize::from(IWX_STATION_DATA_QUEUE))
        .ok_or(RuntimeStartError::Transmission)?;
    let queue = axdriver_iwx::QueueConfig {
        station_id: axdriver_iwx::STA_ID_LINK,
        queue_id: IWX_STATION_DATA_QUEUE,
        tid: 0,
        ring_size: axdriver_iwx::DEFAULT_QUEUE_SIZE,
        byte_count_address: queue_ring.byte_counts.device_address(),
        tfd_address: queue_ring.descriptors.device_address(),
    };
    device
        .controller
        .disable_tx_queue(queue, queue_version)
        .map_err(|_| RuntimeStartError::Transmission)?;

    let nvm = device
        .nvm
        .as_ref()
        .ok_or(RuntimeStartError::FirmwareNotReady)?;
    let channel = axdriver_iwx::init_channel_map(nvm, device.profile.uhb_supported)
        .into_iter()
        .find(|channel| u32::from(channel.frequency_mhz) == station.frequency_mhz)
        .ok_or(RuntimeStartError::Firmware)?;
    let antenna_chains = nvm.valid_rx_antennas.count_ones().clamp(1, 2) as u8;
    let phy = axdriver_iwx::PhyContextConfig {
        id_and_color: 0,
        action: axdriver_iwx::PHY_CONTEXT_ACTION_REMOVE,
        channel: channel.channel,
        is_24ghz: station.frequency_mhz < 3000,
        cdb_supported: device.runtime.cdb != 0,
        ultra_high_band_channels: false,
        channel_40mhz: false,
        sco: 0,
        vht_width: 0,
        primary_channel_index: i16::from(channel.channel),
        center_channel_index: i16::from(channel.channel),
        static_chains: antenna_chains,
        dynamic_chains: antenna_chains,
        valid_rx_antennas: nvm.valid_rx_antennas,
        rlc_command_version: bundle.image.lookup_command_version(
            axdriver_iwx::DATA_PATH_GROUP,
            axdriver_iwx::RLC_CONFIG_COMMAND,
        ),
        command_version: bundle
            .image
            .lookup_command_version(0, axdriver_iwx::PHY_CONTEXT_COMMAND as u8),
    };
    let rates = if station.frequency_mhz < 3000 {
        tk_net80211::STANDARD_RATES_11G
    } else {
        tk_net80211::STANDARD_RATES_11A
    };
    let mut mac = station_mac_context(
        device.profile,
        local_address,
        &station.bss,
        &rates.rates[..rates.count],
        false,
        0,
    );
    mac.action = axdriver_iwx::MAC_ACTION_REMOVE;
    let mac_version = bundle
        .image
        .lookup_command_version(0, axdriver_iwx::MAC_CONTEXT_COMMAND as u8);
    let binding_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::BINDING_CONTEXT_COMMAND as u8,
    );
    axdriver_iwx::deauthenticate(&mut device.association, |step| match step {
        axdriver_iwx::AssociationStep::UnprotectSession => Ok(()),
        axdriver_iwx::AssociationStep::RemoveStation => {
            let command = axdriver_iwx::remove_station_command(false, 0, 0)
                .map_err(|_| RuntimeStartError::Firmware)?;
            send_context_command(&mut device.controller, &command)?;
            Ok(())
        }
        axdriver_iwx::AssociationStep::RemoveBinding => {
            if let Some(command) = axdriver_iwx::binding_command(
                false,
                axdriver_iwx::CONTEXT_ACTION_REMOVE,
                0,
                0,
                station.frequency_mhz < 3000,
                device.runtime.cdb != 0,
                binding_version,
                0,
            )
            .map_err(|_| RuntimeStartError::Firmware)?
            {
                let response = send_context_command(&mut device.controller, &command)?
                    .ok_or(RuntimeStartError::Firmware)?;
                if axdriver_iwx::command_response_status(&response, false)
                    .map_err(|_| RuntimeStartError::Firmware)?
                    != 0
                {
                    return Err(RuntimeStartError::Firmware);
                }
            }
            Ok(())
        }
        axdriver_iwx::AssociationStep::RemoveMac => {
            let command = axdriver_iwx::mac_context_command(&mac, true, mac_version, 0)
                .map_err(|_| RuntimeStartError::Firmware)?;
            send_context_command(&mut device.controller, &command)?;
            Ok(())
        }
        axdriver_iwx::AssociationStep::RemovePhy => {
            let command = axdriver_iwx::phy_context_command(phy, 0, 0)
                .map_err(|_| RuntimeStartError::Firmware)?;
            send_context_command(&mut device.controller, &command)?;
            Ok(())
        }
        _ => Err(RuntimeStartError::Firmware),
    })
    .map_err(|_| RuntimeStartError::Firmware)?;
    device
        .controller
        .disable_management_queue(queue_version)
        .map_err(|_| RuntimeStartError::ManagementQueue)?;
    device.station = None;
    device.security_keys = IwxSecurityKeys::default();
    device.association = axdriver_iwx::AssociationState::default();
    device.session_protection = axdriver_iwx::SessionProtectionState::default();
    Ok(())
}

fn pump_tx_completions(device: &mut AttachedDevice) -> Result<(), RuntimeStartError> {
    let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
        &mut device.controller.registers,
        &device.controller.interrupt_masks,
        Some(&mut device.controller.resources.ict),
    )
    .map_err(|_| RuntimeStartError::Transmission)?;
    let mut responses = Vec::new();
    device
        .controller
        .process_rx_notifications(|packet, _| {
            if matches!(
                axdriver_iwx::decode_firmware_event(packet),
                axdriver_iwx::FirmwareEvent::TxStatus(_)
            ) && responses.try_reserve(1).is_ok()
            {
                responses.push(packet.payload.to_vec());
            }
            Ok::<_, Infallible>(true)
        })
        .map_err(|_| RuntimeStartError::Transmission)?;
    for response in responses {
        let _ = device
            .controller
            .process_tx_response(
                u16::from(IWX_STATION_DATA_QUEUE),
                u16::from(axdriver_iwx::BGSCAN_FIRST_AGG_TX_QUEUE),
                &response,
                false,
                |_, dma| drop(dma),
            )
            .map_err(|_| RuntimeStartError::Transmission)?;
    }
    Ok(())
}

fn pump_station_rx(device: &mut AttachedDevice) -> Result<(), RuntimeStartError> {
    let family = device.profile.family;
    let mut payloads = Vec::new();
    let mut mfp_disconnect_reason = None;
    let mut sa_query_response = None;
    let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
        &mut device.controller.registers,
        &device.controller.interrupt_masks,
        Some(&mut device.controller.resources.ict),
    )
    .map_err(|_| RuntimeStartError::Transmission)?;
    device
        .controller
        .process_rx_notifications(|packet, _| {
            if let axdriver_iwx::FirmwareEvent::RxPhy(bytes) =
                axdriver_iwx::decode_firmware_event(packet)
            {
                device.scan_phy = axdriver_iwx::parse_rx_phy_info(bytes).ok();
            } else if let axdriver_iwx::FirmwareEvent::RxMpdu(bytes) =
                axdriver_iwx::decode_firmware_event(packet)
                && payloads.try_reserve(1).is_ok()
            {
                payloads.push(bytes.to_vec());
            }
            Ok::<_, Infallible>(true)
        })
        .map_err(|_| RuntimeStartError::Transmission)?;
    for payload in payloads {
        let rssi = axdriver_iwx::signal_strength_dbm(family, &payload)
            .unwrap_or(-127)
            .clamp(-127, 0) as i8;
        let received = match device.controller.process_rx_mpdu(
            &payload,
            false,
            axdriver_iwx::HardwareDecryptPolicy {
                receive_protected: false,
                pairwise_ccmp: false,
                group_ccmp: false,
            },
        ) {
            Ok(axdriver_iwx::RxMpduOutcome::Deliver(frame)) => frame,
            _ => continue,
        };
        let Some(station) = device.station.as_mut() else {
            continue;
        };
        if station.disconnected {
            continue;
        }
        let mut frame = received.frame;
        let was_protected = frame.get(1).is_some_and(|control| control & 0x40 != 0);
        let is_management = frame.first().is_some_and(|control| control & 0x0c == 0);
        let subtype = frame.first().map_or(0, |control| control & 0xf0);
        let robust_management = is_management && matches!(subtype, 0xa0 | 0xc0 | 0xd0);
        if station.mfp_negotiated && robust_management && !was_protected {
            // 802.11w robust management traffic is never accepted in clear.
            continue;
        }
        let mut security_decrypted = false;
        if was_protected {
            if !station.security_enabled {
                continue;
            }
            if is_management && !station.mfp_negotiated {
                continue;
            }
            let Ok(header_len) = tk_net80211::header_length(&frame) else {
                continue;
            };
            let Some(selection) = tk_net80211::select_rx_key(&frame, header_len, true, false)
            else {
                continue;
            };
            let key = match selection {
                tk_net80211::KeySelection::Pairwise => device.security_keys.pairwise.as_mut(),
                tk_net80211::KeySelection::Group(index) if is_management => (4..=7)
                    .contains(&index)
                    .then(|| device.security_keys.management[usize::from(index - 4)].as_mut())
                    .flatten(),
                tk_net80211::KeySelection::Group(index) => device
                    .security_keys
                    .groups
                    .get_mut(usize::from(index))
                    .and_then(Option::as_mut),
            };
            let Some(key) = key else { continue };
            let key_id = match &key.software {
                tk_net80211::SoftwareKey::Bip(key) => key.key_id as u8,
                tk_net80211::SoftwareKey::Ccmp(_) => {
                    let Some(key_id) = frame.get(header_len + 3).map(|byte| byte >> 6) else {
                        continue;
                    };
                    key_id
                }
                _ => continue,
            };
            if key.index != key_id {
                continue;
            }
            let Ok(decrypted) =
                tk_net80211::decrypt_software(&mut key.software, &frame, header_len)
            else {
                continue;
            };
            frame = decrypted;
            security_decrypted = true;
        }
        if is_management {
            if station.mfp_negotiated && robust_management && security_decrypted {
                let policy = tk_net80211::DisconnectPolicy {
                    mode: tk_net80211::RxOperatingMode::Station,
                    state: tk_net80211::ProtocolState::Run,
                    background_scan: false,
                    stay_authenticated: false,
                    peer_is_bss: true,
                    peer_authenticated: true,
                    peer_associated: true,
                };
                let received_disconnect = match subtype {
                    0xc0 => tk_net80211::receive_deauthentication(&frame, policy),
                    0xa0 => tk_net80211::receive_disassociation(&frame, policy),
                    0xd0 => {
                        if let Ok(tk_net80211::SaQueryOutcome::SendResponse { transaction_id }) =
                            tk_net80211::receive_sa_query_request(
                                &frame,
                                &mut tk_net80211::SaQueryState {
                                    station_mode: true,
                                    management_frame_protection: true,
                                    ..Default::default()
                                },
                            )
                        {
                            if frame[4] & 1 == 0 {
                                sa_query_response = Some(transaction_id);
                            }
                        }
                        continue;
                    }
                    _ => continue,
                };
                if let Ok(disconnect) = received_disconnect {
                    mfp_disconnect_reason = Some(disconnect.reason);
                }
            }
            // Management frames are consumed by the state path, never passed
            // to the Ethernet data decapsulator.
            continue;
        }
        if station.security_enabled
            && frame.get(1).is_some_and(|control| control & 0x40 == 0)
            && tk_net80211::header_length(&frame).is_ok_and(|header_len| {
                frame
                    .get(header_len..header_len + 8)
                    .is_some_and(|snap| snap == [0xaa, 0xaa, 0x03, 0, 0, 0, 0x88, 0x8e])
            })
        {
            security_decrypted = true;
        }
        let result = tk_net80211::receive_station_data(
            &mut station.nodes,
            &frame,
            (i16::from(rssi) + 100).clamp(0, 100) as u8,
            u64::from(received.metadata.device_timestamp),
            0,
            tk_net80211::RxDataPolicy {
                state: tk_net80211::ProtocolState::Run,
                monitor_mode: false,
                current_bssid: station.bssid,
                interface_address: device
                    .nvm
                    .as_ref()
                    .map_or([0; 6], |nvm| nvm.hardware_address),
                simplex: false,
                wep_enabled: false,
                rsn_rx_protected: station.security_enabled,
                peer_ht: false,
                ampdu_done: received.metadata.reorder_data != 0,
                hardware_decrypted: security_decrypted,
                same_sequence: received.same_sequence,
                rx_ba_states: [0; 16],
            },
        );
        if let tk_net80211::RxDataResult::Ethernet { frames, .. } = result {
            for frame in frames {
                let mut ethernet = Vec::new();
                if ethernet.try_reserve(14 + frame.payload.len()).is_err() {
                    continue;
                }
                ethernet.extend_from_slice(&frame.destination);
                ethernet.extend_from_slice(&frame.source);
                ethernet.extend_from_slice(&frame.ether_type.to_be_bytes());
                ethernet.extend_from_slice(&frame.payload);
                if station.rx_ethernet.len() < 128 {
                    station.rx_ethernet.push_back(ethernet);
                }
            }
        }
    }
    if mfp_disconnect_reason.is_none()
        && let (Some(transaction_id), Some(address)) = (
            sa_query_response,
            device.nvm.as_ref().map(|nvm| nvm.hardware_address),
        )
        && let Some(station) = device
            .station
            .as_mut()
            .filter(|station| !station.disconnected)
    {
        let body =
            tk_net80211::build_sa_query_body(tk_net80211::ACTION_SA_QUERY_RESPONSE, transaction_id);
        let frame = station
            .tx_sequence
            .frame(
                tk_net80211::MGMT_SUBTYPE_ACTION,
                station.bssid,
                address,
                station.bssid,
                &body,
                true,
                true,
            )
            .map_err(|_| RuntimeStartError::Transmission)?;
        send_station_management_frame_locked(device, &frame, |_| {})?;
    }
    if let Some(reason) = mfp_disconnect_reason {
        if let Some(station) = device.station.as_mut() {
            let bssid = station.bssid;
            station.disconnected = true;
            station.rx_ethernet.clear();
            device.disconnect_event = Some(WirelessDisconnectEvent { bssid, reason });
        }
    }
    Ok(())
}

fn trigger_scan(
    bdf: Bdf,
    request: &axdriver_net::WirelessScanRequest,
) -> Result<(), RuntimeStartError> {
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == bdf)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    if !device.interface_up || !device.runtime_started || device.soft_blocked {
        return Err(RuntimeStartError::FirmwareNotReady);
    }
    if request.ssid.len() > 32 {
        return Err(RuntimeStartError::Firmware);
    }
    let (Some(nvm), Some(Ok(bundle))) = (device.nvm.as_ref(), device.firmware.as_ref()) else {
        return Err(RuntimeStartError::FirmwareNotReady);
    };
    let channels = axdriver_iwx::init_channel_map(nvm, device.profile.uhb_supported)
        .into_iter()
        .filter(|channel| {
            channel.flags != 0
                && (request.frequencies_mhz.is_empty()
                    || request
                        .frequencies_mhz
                        .contains(&u32::from(channel.frequency_mhz)))
        })
        .collect::<Vec<_>>();
    if channels.is_empty() {
        return Err(RuntimeStartError::Firmware);
    }
    let command_version = bundle
        .image
        .lookup_command_version(axdriver_iwx::IWX_LONG_GROUP, axdriver_iwx::UMAC_SCAN_REQ);
    if command_version < 14 {
        return Err(RuntimeStartError::Firmware);
    }
    let probe = axdriver_iwx::ProbeRequestConfig {
        station_address: nvm.hardware_address,
        rates_2ghz: &IWX_SCAN_RATES_2GHZ,
        rates_5ghz: &IWX_SCAN_RATES_5GHZ,
        supports_5ghz: channels
            .iter()
            .any(|channel| channel.flags & axdriver_iwx::CHAN_2GHZ == 0),
        include_ds_parameter: true,
        vht_capabilities_ie: None,
        ht_capabilities_ie: None,
    };
    let command = axdriver_iwx::initiate_scan_command(
        axdriver_iwx::UmacScanConfig {
            version: axdriver_iwx::UmacScanVersion::V14,
            background: false,
            channels: &channels,
            firmware_channel_limit: bundle.image.scan_channels as usize,
            extended_channel_version: command_version >= 17,
            channel_flags: 0,
            probe,
            desired_ssid: &request.ssid,
            slot: 0,
            command_queue: 0,
        },
        command_version,
    )
    .map_err(|_| RuntimeStartError::Firmware)?;
    device.scan_cache = Some(axdriver_iwx::ScanCache::new(channels, 0, 0, false));
    device.scan_phy = None;
    device.scan_complete = false;
    device.scan_event = None;
    let family = device.profile.family;
    let (controller, cache, phy, complete, event) = (
        &mut device.controller,
        device.scan_cache.as_mut().unwrap(),
        &mut device.scan_phy,
        &mut device.scan_complete,
        &mut device.scan_event,
    );
    controller
        .send_encoded_command_wait_allocated(&command, |packet, _| {
            observe_scan_packet(family, cache, phy, complete, event, packet);
            Ok::<_, Infallible>(true)
        })
        .map_err(|_| RuntimeStartError::Firmware)?;
    Ok(())
}

fn pump_scan_events(device: &mut AttachedDevice) -> Result<(), ()> {
    let family = device.profile.family;
    let (controller, cache, phy, complete, event) = (
        &mut device.controller,
        device.scan_cache.as_mut().ok_or(())?,
        &mut device.scan_phy,
        &mut device.scan_complete,
        &mut device.scan_event,
    );
    let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
        &mut controller.registers,
        &controller.interrupt_masks,
        Some(&mut controller.resources.ict),
    )
    .map_err(|_| ())?;
    controller
        .process_rx_notifications(|packet, _| {
            observe_scan_packet(family, cache, phy, complete, event, packet);
            Ok::<_, Infallible>(true)
        })
        .map_err(|_| ())?;
    Ok(())
}

fn abort_scan(bdf: Bdf) -> Result<(), RuntimeStartError> {
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == bdf)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    if !device.runtime_started {
        return Err(RuntimeStartError::FirmwareNotReady);
    }
    if device.scan_cache.is_none() || device.scan_complete {
        return Ok(());
    }
    let command =
        axdriver_iwx::scan_abort_command(0, 0).map_err(|_| RuntimeStartError::Firmware)?;
    let family = device.profile.family;
    let (controller, cache, phy, complete, event) = (
        &mut device.controller,
        device
            .scan_cache
            .as_mut()
            .ok_or(RuntimeStartError::Firmware)?,
        &mut device.scan_phy,
        &mut device.scan_complete,
        &mut device.scan_event,
    );
    controller
        .send_encoded_command_wait(&command, None, |packet, _| {
            observe_scan_packet(family, cache, phy, complete, event, packet);
            Ok::<_, Infallible>(true)
        })
        .map_err(|_| RuntimeStartError::Firmware)?;
    *complete = true;
    *event = Some(WirelessScanEvent::Aborted);
    Ok(())
}

fn observe_scan_packet(
    family: axdriver_iwx::DeviceFamily,
    cache: &mut axdriver_iwx::ScanCache,
    phy: &mut Option<axdriver_iwx::RxPhyInfo>,
    complete: &mut bool,
    scan_event: &mut Option<WirelessScanEvent>,
    packet: &axdriver_iwx::RxPacket<'_>,
) {
    match axdriver_iwx::decode_firmware_event(packet) {
        axdriver_iwx::FirmwareEvent::RxPhy(payload) => {
            *phy = axdriver_iwx::parse_rx_phy_info(payload).ok();
        }
        axdriver_iwx::FirmwareEvent::RxMpdu(payload) => {
            let Some(phy) = *phy else { return };
            let Ok(mpdu) = axdriver_iwx::parse_rx_mpdu(payload, family, false) else {
                return;
            };
            let Ok(frame) = axdriver_iwx::normalize_rx_frame(mpdu) else {
                return;
            };
            let rssi = axdriver_iwx::signal_strength_dbm(family, payload)
                .unwrap_or(-127)
                .clamp(-127, 0) as i8;
            cache.observe(&frame, phy.channel as u8, rssi, phy.timestamp);
        }
        axdriver_iwx::FirmwareEvent::ScanComplete(_) => {
            if !*complete {
                *scan_event = Some(WirelessScanEvent::Results);
            }
            *complete = true;
        }
        _ => {}
    }
}

fn bootstrap_init_firmware(
    controller: &mut IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    profile: AttachProfile,
    runtime: RuntimeConfig,
    hardware_revision: u32,
    bundle: &FirmwareBundle,
) -> Result<NvmInfo, FirmwareBootstrapError> {
    controller
        .start_hardware(profile.integrated)
        .map_err(FirmwareBootstrapError::Hardware)?;
    controller
        .initialize_nic(bundle.image.phy_config.unwrap_or(0), hardware_revision)
        .map_err(FirmwareBootstrapError::Nic)?;

    let alive_version = bundle.image.lookup_notification_version(0, 1);
    controller
        .load_ucode_wait_alive(
            &bundle.image,
            true,
            bundle.pnvm_file.as_deref(),
            runtime.mac_type,
            runtime.rf_type,
            profile.imr_enabled,
            |controller, timeout| {
                let mut elapsed = 0;
                let mut alive = None;
                while elapsed < timeout {
                    let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
                        &mut controller.registers,
                        &controller.interrupt_masks,
                        None,
                    )
                    .map_err(AliveWaitError::Interrupt)?;
                    controller
                        .process_rx_notifications(|packet, _| {
                            if packet.is_notification() && packet.command_id() == 1 {
                                alive = Some(
                                    axdriver_iwx::parse_alive(alive_version, packet.payload)
                                        .map_err(AliveWaitError::Alive),
                                );
                            }
                            Ok::<_, Infallible>(true)
                        })
                        .map_err(AliveWaitError::Receive)?;
                    if let Some(result) = alive.take() {
                        return result.map(Some);
                    }
                    controller.registers.delay_us(1_000);
                    elapsed += 1_000_000;
                }
                Ok(None)
            },
            |controller, timeout| {
                let mut elapsed = 0;
                while elapsed < timeout {
                    let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
                        &mut controller.registers,
                        &controller.interrupt_masks,
                        None,
                    )
                    .map_err(AliveWaitError::Interrupt)?;
                    let mut complete = false;
                    controller
                        .process_rx_notifications(|packet, _| {
                            if packet.is_notification()
                                && matches!(
                                    axdriver_iwx::decode_firmware_event(packet),
                                    axdriver_iwx::FirmwareEvent::PnvmComplete
                                )
                            {
                                complete = true;
                            }
                            Ok::<_, Infallible>(true)
                        })
                        .map_err(AliveWaitError::Receive)?;
                    if complete {
                        return Ok::<_, AliveWaitError>(true);
                    }
                    controller.registers.delay_us(1_000);
                    elapsed += 1_000_000;
                }
                Ok::<_, AliveWaitError>(false)
            },
        )
        .map_err(FirmwareBootstrapError::FirmwareStart)?;

    let init_config = axdriver_iwx::init_extended_config_command(0, 0)
        .map_err(FirmwareBootstrapError::CommandEncoding)?;
    controller
        .send_encoded_command_wait(&init_config, None, |_, _| Ok::<_, Infallible>(true))
        .map_err(FirmwareBootstrapError::Command)?;
    let nvm_complete = axdriver_iwx::nvm_access_complete_command(0, 0)
        .map_err(FirmwareBootstrapError::CommandEncoding)?;
    controller
        .send_encoded_command_wait(&nvm_complete, None, |_, _| Ok::<_, Infallible>(true))
        .map_err(FirmwareBootstrapError::Command)?;

    let mut init_complete = false;
    for _ in 0..2_000 {
        let _ = axdriver_iwx::service_legacy_interrupt(
            &mut controller.registers,
            &controller.interrupt_masks,
            Some(&mut controller.resources.ict),
        )
        .map_err(FirmwareBootstrapError::Interrupt)?;
        controller
            .process_rx_notifications(|packet, _| {
                if packet.is_notification()
                    && matches!(
                        axdriver_iwx::decode_firmware_event(packet),
                        axdriver_iwx::FirmwareEvent::InitComplete
                    )
                {
                    init_complete = true;
                }
                Ok::<_, Infallible>(true)
            })
            .map_err(|error| {
                FirmwareBootstrapError::Command(axdriver_iwx::SyncCommandError::Receive(error))
            })?;
        if init_complete {
            break;
        }
        controller.registers.delay_us(1_000);
    }
    if !init_complete {
        return Err(FirmwareBootstrapError::InitCompleteTimeout);
    }

    let address =
        axdriver_iwx::read_csr_mac_address(&mut controller.registers, profile.mac_addr_from_csr)
            .ok_or(FirmwareBootstrapError::MacAddressUnavailable)?;
    let regulatory_v4 = bundle.image.api_enabled(48);
    let nvm_command = axdriver_iwx::nvm_get_command(regulatory_v4, 0, 0)
        .map_err(FirmwareBootstrapError::CommandEncoding)?;
    let response = controller
        .send_encoded_command_wait(&nvm_command, None, |_, _| Ok::<_, Infallible>(true))
        .map_err(FirmwareBootstrapError::Command)?
        .response
        .ok_or(FirmwareBootstrapError::MissingNvmResponse)?;
    let nvm = axdriver_iwx::parse_nvm_response(
        &response,
        regulatory_v4,
        Some(address),
        &bundle.image.enabled_capabilities,
    )
    .map_err(FirmwareBootstrapError::Nvm)?;
    controller
        .stop_device()
        .map_err(FirmwareBootstrapError::Stop)?;
    Ok(nvm)
}

fn start_regular_firmware(
    controller: &mut IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    profile: AttachProfile,
    runtime: RuntimeConfig,
    hardware_revision: u32,
    bundle: &FirmwareBundle,
    nvm: &NvmInfo,
) -> Result<(), RuntimeStartError> {
    controller
        .start_hardware(profile.integrated)
        .map_err(|error| {
            warn!("iwx: runtime hardware start failed: {error:?}");
            RuntimeStartError::Hardware
        })?;
    controller
        .initialize_nic(bundle.image.phy_config.unwrap_or(0), hardware_revision)
        .map_err(|error| {
            warn!("iwx: runtime NIC initialization failed: {error:?}");
            RuntimeStartError::Nic
        })?;
    let alive_version = bundle.image.lookup_notification_version(0, 1);
    controller
        .load_ucode_wait_alive(
            &bundle.image,
            false,
            bundle.pnvm_file.as_deref(),
            runtime.mac_type,
            runtime.rf_type,
            profile.imr_enabled,
            |controller, timeout| {
                let mut elapsed = 0;
                let mut alive = None;
                while elapsed < timeout {
                    let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
                        &mut controller.registers,
                        &controller.interrupt_masks,
                        None,
                    )
                    .map_err(AliveWaitError::Interrupt)?;
                    controller
                        .process_rx_notifications(|packet, _| {
                            if packet.is_notification() && packet.command_id() == 1 {
                                alive = Some(
                                    axdriver_iwx::parse_alive(alive_version, packet.payload)
                                        .map_err(AliveWaitError::Alive),
                                );
                            }
                            Ok::<_, Infallible>(true)
                        })
                        .map_err(AliveWaitError::Receive)?;
                    if let Some(result) = alive.take() {
                        return result.map(Some);
                    }
                    controller.registers.delay_us(1_000);
                    elapsed += 1_000_000;
                }
                Ok(None)
            },
            |controller, timeout| {
                let mut elapsed = 0;
                while elapsed < timeout {
                    let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
                        &mut controller.registers,
                        &controller.interrupt_masks,
                        None,
                    )
                    .map_err(AliveWaitError::Interrupt)?;
                    let mut complete = false;
                    controller
                        .process_rx_notifications(|packet, _| {
                            complete |= packet.is_notification()
                                && matches!(
                                    axdriver_iwx::decode_firmware_event(packet),
                                    axdriver_iwx::FirmwareEvent::PnvmComplete
                                );
                            Ok::<_, Infallible>(true)
                        })
                        .map_err(AliveWaitError::Receive)?;
                    if complete {
                        return Ok::<_, AliveWaitError>(true);
                    }
                    controller.registers.delay_us(1_000);
                    elapsed += 1_000_000;
                }
                Ok::<_, AliveWaitError>(false)
            },
        )
        .map_err(|error| {
            warn!("iwx: runtime ALIVE/PNVM/post-ALIVE sequence failed: {error:?}");
            RuntimeStartError::Firmware
        })?;
    if bundle
        .image
        .api_enabled(u32::from(axdriver_iwx::REDUCED_SCAN_CONFIG_API))
    {
        let scan_config_version = bundle.image.lookup_command_version(
            axdriver_iwx::IWX_LONG_GROUP,
            axdriver_iwx::SCAN_CONFIG_COMMAND,
        );
        let command = axdriver_iwx::reduced_scan_config_command(
            true,
            scan_config_version,
            nvm.valid_tx_antennas,
            nvm.valid_rx_antennas,
            0,
            0,
        )
        .map_err(|_| RuntimeStartError::Firmware)?;
        controller
            .send_encoded_command_wait(&command, None, |_, _| Ok::<_, Infallible>(true))
            .map_err(|error| {
                warn!("iwx: reduced scan config command failed: {error:?}");
                RuntimeStartError::Firmware
            })?;
    }
    Ok(())
}

/// Start the retained PCI device's regular uCode when the wireless netdev is raised.
pub fn start_runtime(bdf: DeviceFunction) -> Result<(), RuntimeStartError> {
    let key = Bdf(bdf.bus, bdf.device, bdf.function);
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == key)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    if device.runtime_started {
        return Err(RuntimeStartError::AlreadyStarted);
    }
    if device.soft_blocked {
        return Err(RuntimeStartError::SoftBlocked);
    }
    let Some(Ok(bundle)) = device.firmware.as_ref() else {
        return Err(RuntimeStartError::FirmwareNotReady);
    };
    if device.nvm.is_none() || device.preinit.is_none() {
        return Err(RuntimeStartError::FirmwareNotReady);
    }
    match start_regular_firmware(
        &mut device.controller,
        device.profile,
        device.runtime,
        device.hardware_revision,
        bundle,
        device
            .nvm
            .as_ref()
            .ok_or(RuntimeStartError::FirmwareNotReady)?,
    ) {
        Ok(()) => {
            device.runtime_started = true;
            Ok(())
        }
        Err(error) => {
            let _ = device.controller.stop_device();
            Err(error)
        }
    }
}

/// Submit one source-built station management MPDU on iwx's dedicated queue
/// and retire its DMA storage only after the matching firmware TX response.
pub fn send_station_management_frame(
    bdf: DeviceFunction,
    frame: &[u8],
) -> Result<(), RuntimeStartError> {
    send_station_management_frame_with_rx(bdf, frame, |_| {})
}

/// Management transmit variant which hands concurrent RX notifications to
/// the station state-machine waiter while retaining the TX completion gate.
pub fn send_station_management_frame_with_rx(
    bdf: DeviceFunction,
    frame: &[u8],
    receive: impl FnMut(&axdriver_iwx::RxPacket<'_>),
) -> Result<(), RuntimeStartError> {
    let key = Bdf(bdf.bus, bdf.device, bdf.function);
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == key)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    send_station_management_frame_locked(device, frame, receive)
}

fn send_station_management_frame_locked(
    device: &mut AttachedDevice,
    frame: &[u8],
    mut receive: impl FnMut(&axdriver_iwx::RxPacket<'_>),
) -> Result<(), RuntimeStartError> {
    const IEEE80211_HEADER_BYTES: usize = 24;
    const MANAGEMENT_QUEUE: u8 = axdriver_iwx::DQA_CMD_QUEUE + 1;
    const MANAGEMENT_TIMEOUT_US: u32 = 500_000;
    if frame.len() < IEEE80211_HEADER_BYTES {
        return Err(RuntimeStartError::Transmission);
    }
    if !device.runtime_started || device.soft_blocked {
        return Err(RuntimeStartError::FirmwareNotReady);
    }
    let protected_management = frame[1] & 0x40 != 0;
    let transmitted_frame = if protected_management {
        let is_management = frame[0] & 0x0c == 0;
        let is_group = frame[4] & 1 != 0;
        let station = device
            .station
            .as_ref()
            .ok_or(RuntimeStartError::FirmwareNotReady)?;
        if !is_management || is_group || !station.mfp_negotiated {
            // The iwx station path only transmits protected unicast robust
            // management frames with the negotiated pairwise CCMP key; group
            // MFP transmit (IGTK/BIGTK) remains unavailable in this path.
            return Err(RuntimeStartError::UnsupportedSecurity);
        }
        let key = device
            .security_keys
            .pairwise
            .as_mut()
            .ok_or(RuntimeStartError::FirmwareNotReady)?;
        tk_net80211::encrypt_software(&mut key.software, frame, IEEE80211_HEADER_BYTES, 0)
            .map_err(|_| RuntimeStartError::Transmission)?
    } else {
        frame.to_vec()
    };
    let target_slot = device
        .controller
        .submit_data_frame(
            MANAGEMENT_QUEUE,
            axdriver_iwx::STA_ID_LINK,
            &transmitted_frame[..IEEE80211_HEADER_BYTES],
            &transmitted_frame[IEEE80211_HEADER_BYTES..],
            0,
            0,
            false,
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
    let mut elapsed = 0;
    while elapsed < MANAGEMENT_TIMEOUT_US {
        let _ = axdriver_iwx::service_legacy_interrupt::<_, PlatformDmaRegion>(
            &mut device.controller.registers,
            &device.controller.interrupt_masks,
            Some(&mut device.controller.resources.ict),
        )
        .map_err(|_| RuntimeStartError::Transmission)?;
        let mut responses = Vec::new();
        device
            .controller
            .process_rx_notifications(|packet, _| {
                if matches!(
                    axdriver_iwx::decode_firmware_event(packet),
                    axdriver_iwx::FirmwareEvent::TxStatus(_)
                ) {
                    if responses.try_reserve(1).is_ok() {
                        responses.push(packet.payload.to_vec());
                    }
                } else {
                    receive(packet);
                }
                Ok::<_, Infallible>(true)
            })
            .map_err(|_| RuntimeStartError::Transmission)?;
        for response in responses {
            let mut completed = false;
            device
                .controller
                .process_tx_response(
                    u16::from(MANAGEMENT_QUEUE),
                    u16::from(axdriver_iwx::BGSCAN_FIRST_AGG_TX_QUEUE),
                    &response,
                    false,
                    |_, dma| drop(dma),
                )
                .map(|outcome| {
                    completed = outcome.completed_slots.contains(&target_slot);
                })
                .map_err(|_| RuntimeStartError::Transmission)?;
            if completed {
                return Ok(());
            }
        }
        device.controller.registers.delay_us(1_000);
        elapsed += 1_000;
    }
    Err(RuntimeStartError::Transmission)
}

/// Stop regular uCode when the wireless interface is administratively lowered.
pub fn stop_runtime(bdf: DeviceFunction) -> Result<(), RuntimeStartError> {
    let key = Bdf(bdf.bus, bdf.device, bdf.function);
    let mut devices = ATTACHED_DMA.lock();
    let device = devices
        .iter_mut()
        .find(|device| device.bdf == key)
        .ok_or(RuntimeStartError::DeviceNotFound)?;
    if !device.runtime_started {
        return Ok(());
    }
    device
        .controller
        .stop_device()
        .map_err(|_| RuntimeStartError::Hardware)?;
    device.runtime_started = false;
    device.station = None;
    device.security_keys = IwxSecurityKeys::default();
    device.association = axdriver_iwx::AssociationState::default();
    device.session_protection = axdriver_iwx::SessionProtectionState::default();
    Ok(())
}

fn set_soft_blocked(bdf: Bdf, blocked: bool) -> Result<(), RuntimeStartError> {
    let (old, running, interface_up) = {
        let devices = ATTACHED_DMA.lock();
        let device = devices
            .iter()
            .find(|device| device.bdf == bdf)
            .ok_or(RuntimeStartError::DeviceNotFound)?;
        (
            device.soft_blocked,
            device.runtime_started,
            device.interface_up,
        )
    };
    if old == blocked {
        return Ok(());
    }
    let address = DeviceFunction {
        bus: bdf.0,
        device: bdf.1,
        function: bdf.2,
    };
    if blocked {
        if running {
            stop_runtime(address)?;
        }
        let mut devices = ATTACHED_DMA.lock();
        devices
            .iter_mut()
            .find(|device| device.bdf == bdf)
            .ok_or(RuntimeStartError::DeviceNotFound)?
            .soft_blocked = true;
        return Ok(());
    }
    {
        let mut devices = ATTACHED_DMA.lock();
        devices
            .iter_mut()
            .find(|device| device.bdf == bdf)
            .ok_or(RuntimeStartError::DeviceNotFound)?
            .soft_blocked = false;
    }
    if interface_up {
        if let Err(error) = start_runtime(address) {
            if let Some(device) = ATTACHED_DMA
                .lock()
                .iter_mut()
                .find(|device| device.bdf == bdf)
            {
                device.soft_blocked = true;
            }
            return Err(error);
        }
    }
    Ok(())
}

fn log_bootstrap_error(bdf: Bdf, error: FirmwareBootstrapError) {
    use axdriver_iwx::{ControllerError, ControllerUcodeStartError, PnvmLoadError};

    match error {
        FirmwareBootstrapError::Hardware(error) => {
            warn!("iwx: {bdf:?}: hardware/APM startup failed: {error:?}")
        }
        FirmwareBootstrapError::Nic(error) => {
            warn!("iwx: {bdf:?}: NIC initialization failed: {error:?}")
        }
        FirmwareBootstrapError::FirmwareStart(ControllerUcodeStartError::Firmware(
            ControllerError::Wait(error),
        )) => log_alive_wait_error(bdf, error),
        FirmwareBootstrapError::FirmwareStart(ControllerUcodeStartError::Pnvm(
            PnvmLoadError::Wait(error),
        )) => log_alive_wait_error(bdf, error),
        FirmwareBootstrapError::FirmwareStart(error) => {
            warn!("iwx: {bdf:?}: init firmware/PNVM startup failed: {error:?}")
        }
        FirmwareBootstrapError::Interrupt(error) => {
            warn!("iwx: {bdf:?}: init interrupt service failed: {error:?}")
        }
        FirmwareBootstrapError::CommandEncoding(error) => {
            warn!("iwx: {bdf:?}: command encoding failed: {error:?}")
        }
        FirmwareBootstrapError::Command(error) => {
            warn!("iwx: {bdf:?}: command wait failed: {error:?}")
        }
        FirmwareBootstrapError::InitCompleteTimeout => {
            warn!("iwx: {bdf:?}: init ucode did not report INIT_COMPLETE")
        }
        FirmwareBootstrapError::MacAddressUnavailable => {
            warn!("iwx: {bdf:?}: no valid strap/OTP MAC address")
        }
        FirmwareBootstrapError::MissingNvmResponse => {
            warn!("iwx: {bdf:?}: NVM_GET_INFO completed without a response")
        }
        FirmwareBootstrapError::Nvm(error) => {
            warn!("iwx: {bdf:?}: NVM response invalid: {error:?}")
        }
        FirmwareBootstrapError::Stop(error) => {
            warn!("iwx: {bdf:?}: stop after init/NVM bootstrap failed: {error:?}")
        }
    }
}

fn log_alive_wait_error(bdf: Bdf, error: AliveWaitError) {
    match error {
        AliveWaitError::Interrupt(error) => {
            warn!("iwx: {bdf:?}: firmware startup interrupt service failed: {error:?}")
        }
        AliveWaitError::Receive(error) => {
            warn!("iwx: {bdf:?}: firmware startup RX processing failed: {error:?}")
        }
        AliveWaitError::Alive(error) => {
            warn!("iwx: {bdf:?}: firmware ALIVE notification malformed: {error:?}")
        }
    }
}

fn register_rootfs_firmware_callback() {
    if axdriver_base::firmware::rootfs_ready() {
        stage_rootfs_firmware();
        return;
    }
    if ROOTFS_CALLBACK_REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    if !axdriver_base::firmware::on_rootfs_ready(stage_rootfs_firmware) {
        ROOTFS_CALLBACK_REGISTERED.store(false, Ordering::Release);
        warn!("iwx: rootfs-ready callback table is full; firmware was not registered");
    }
}

fn stage_rootfs_firmware() {
    const UC_MAX: usize = 4 * 1024 * 1024;
    const PNVM_MAX: usize = 2 * 1024 * 1024;
    let pending: Vec<(Bdf, AttachProfile)> = {
        let devices = ATTACHED_DMA.lock();
        let mut pending = Vec::new();
        if pending.try_reserve_exact(devices.len()).is_err() {
            warn!("iwx: could not allocate rootfs firmware staging list");
            return;
        }
        pending.extend(
            devices
                .iter()
                .filter(|device| device.firmware.is_none())
                .map(|device| (device.bdf, device.profile)),
        );
        pending
    };
    for (bdf, profile) in pending {
        let firmware = profile.firmware;
        let result = match axdriver_base::firmware::request(firmware.firmware, UC_MAX) {
            None => Err(FirmwareRequestError::UcodeMissing),
            Some(bytes) => match FirmwareImage::parse(&bytes) {
                Err(error) => Err(FirmwareRequestError::UcodeInvalid(error)),
                Ok(image) => {
                    let pnvm = if image.pnvm.is_some() || firmware.pnvm.is_none() {
                        Ok(None)
                    } else {
                        axdriver_base::firmware::request(firmware.pnvm.unwrap(), PNVM_MAX)
                            .map(Some)
                            .ok_or(FirmwareRequestError::PnvmMissing)
                    };
                    pnvm.map(|pnvm_file| FirmwareBundle { image, pnvm_file })
                }
            },
        };
        let mut devices = ATTACHED_DMA.lock();
        if let Some(device) = devices.iter_mut().find(|device| device.bdf == bdf) {
            match &result {
                Ok(bundle) => {
                    info!(
                        "iwx: {bdf:?}: staged firmware version {:?} ({:?}); BAR0 {:#x}+{:#x}, {} \
                         TX queues and {} RX buffers retained",
                        bundle.image.api,
                        device.profile.family,
                        device.bar_base,
                        device.bar_size,
                        device.controller.resources.tx_queues.len(),
                        device.controller.resources.rx_queue.buffers.len(),
                    );
                    match bootstrap_init_firmware(
                        &mut device.controller,
                        device.profile,
                        device.runtime,
                        device.hardware_revision,
                        bundle,
                    ) {
                        Ok(nvm) => {
                            info!(
                                "iwx: {bdf:?}: init ucode and NVM ready; address \
                                 {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, {} channels",
                                nvm.hardware_address[0],
                                nvm.hardware_address[1],
                                nvm.hardware_address[2],
                                nvm.hardware_address[3],
                                nvm.hardware_address[4],
                                nvm.hardware_address[5],
                                nvm.channel_profiles.len(),
                            );
                            device.preinit = Some(preinit_plan(
                                false,
                                nvm.hardware_address,
                                &nvm,
                                device.profile.uhb_supported,
                                false,
                            ));
                            device.nvm = Some(nvm);
                        }
                        Err(error) => log_bootstrap_error(bdf, error),
                    }
                }
                Err(error) => warn!("iwx: {bdf:?}: firmware staging failed: {error:?}"),
            }
            device.firmware = Some(result);
        }
    }
}

/// Apply `iwx_match()` to one PCI function, including the BZ/GF runtime RF check.
pub(crate) fn probe(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
) -> BusProbeResult {
    if !matches_pci_device(info.vendor_id, info.device_id) {
        return BusProbeResult::NotMatched;
    }

    let bar = match root.bar_info(bdf, 0) {
        Ok(BarInfo::Memory { address, size, .. })
            if usize::try_from(size).is_ok_and(|bytes| bytes >= CSR_HW_RF_ID + 4) =>
        {
            (address, size)
        }
        Ok(BarInfo::IO { .. }) | Err(_) => {
            if info.device_id == SPECIAL_BZ_DEVICE {
                return BusProbeResult::NotMatched;
            }
            warn!("iwx: {bdf}: matched PCI ID but BAR0 cannot expose the CSR aperture");
            return BusProbeResult::Claimed;
        }
        Ok(BarInfo::Memory { .. }) => {
            warn!("iwx: {bdf}: matched PCI ID but BAR0 is too short for RF identification");
            return BusProbeResult::Claimed;
        }
    };

    let address = match usize::try_from(bar.0) {
        Ok(address) if address != 0 => address,
        _ => {
            warn!(
                "iwx: {bdf}: BAR0 address {:#x} is not CPU-addressable",
                bar.0
            );
            return BusProbeResult::Claimed;
        }
    };
    // SAFETY: the PCI walk mapped assigned memory BARs into the platform
    // direct map before calling DriverProbe; range is bounded by `bar.1`.
    let base = phys_to_virt(address.into()).as_usize();
    let read_register = |offset: usize| -> u32 {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > bar.1 as usize) {
            u32::MAX
        } else {
            // SAFETY: validated dword inside the mapped device BAR.
            unsafe { ((base + offset) as *const u32).read_volatile() }
        }
    };
    if info.device_id == SPECIAL_BZ_DEVICE {
        let rf_id = read_register(CSR_HW_RF_ID);
        if !pci_match_decision(info.vendor_id, info.device_id, Some(rf_id)) {
            return BusProbeResult::NotMatched;
        }
    }

    let subsystem = root.endpoint_subsystem_ids(bdf).1;
    let hardware_revision = read_register(CSR_HW_REV);
    let rf_id = read_register(CSR_HW_RF_ID);
    if hardware_revision == u32::MAX || rf_id == u32::MAX {
        warn!("iwx: {bdf}: PCI ID matched but hardware identity CSRs are unavailable");
        return BusProbeResult::Claimed;
    }
    let config = RuntimeConfig::from_hardware(
        info.device_id,
        subsystem,
        ((hardware_revision & 0x0000_fff0) >> 4) as u16,
        (hardware_revision & 0x3) as u8,
        ((rf_id & CSR_HW_RFID_TYPE_MASK) >> CSR_HW_RFID_TYPE_SHIFT) as u16,
        ((rf_id >> 28) & 1) as u8,
        ((rf_id >> 29) & 1) as u8,
    );
    let mut publish_device = false;
    match attach_profile(config) {
        Ok(profile) => {
            info!(
                "iwx: {bdf}: PCI {:#06x}:{:#06x} matched {:?} (subsystem {:#06x}:{:#06x})",
                info.vendor_id,
                info.device_id,
                profile.config,
                root.endpoint_subsystem_ids(bdf).0,
                subsystem,
            );
            // Use MMIO and bus mastering for controller DMA; keep INTx disabled
            // and prefer one-vector MSI-X, with task-context polling fallback.
            let Some(command_status) = root.read_config_dword(bdf, 4) else {
                warn!("iwx: {bdf}: could not read PCI command register");
                return BusProbeResult::Claimed;
            };
            let command = command_status as u16 | 0x0006 | 0x0400;
            if !root.write_config_u16(bdf, 4, command) {
                warn!("iwx: {bdf}: could not enable PCI memory/bus-master command bits");
                return BusProbeResult::Claimed;
            }
            let (link_control, device_control2) = pcie_power_registers(root, bdf);
            IwxMsixRoute::disable(root, bdf);
            let msix = IwxMsixRoute::prepare(root, bdf, base, bar.1 as usize);
            if let Err(error) = allocate_resources(
                bdf,
                profile,
                config,
                base,
                bar.1 as usize,
                hardware_revision,
                link_control,
                device_control2,
                msix.as_ref().map(|route| route.vector),
            ) {
                warn!("iwx: {bdf}: attach DMA allocation failed: {error:?}");
                return BusProbeResult::Claimed;
            }
            if let Some(route) = &msix {
                route.enable(root, bdf);
            }
            info!(
                "iwx: {bdf}: receive service={}",
                if msix.is_some() {
                    "MSI-X + 100ms fallback"
                } else {
                    "10ms polling fallback"
                }
            );
            register_rootfs_firmware_callback();
            publish_device = true;
        }
        Err(error) => {
            warn!("iwx: {bdf}: matched PCI function has no usable runtime profile: {error:?}")
        }
    }
    debug_assert_eq!(info.vendor_id, INTEL_VENDOR_ID);
    if info.device_id == AX211_DEVICE_ID {
        info!("iwx: {bdf}: AX211 PCI discovery complete; firmware runtime attach follows");
    }
    if publish_device {
        return match IwxNetDevice::try_new(bdf) {
            #[cfg(any(feature = "dyn", net_dev = "n305-net"))]
            Ok(device) => BusProbeResult::Device(crate::AxDeviceEnum::from_net(device)),
            #[cfg(not(any(feature = "dyn", net_dev = "n305-net")))]
            Ok(_device) => {
                warn!("iwx: {bdf}: this static device set cannot represent the wlan0 link type");
                BusProbeResult::Claimed
            }
            Err(error) => {
                warn!("iwx: {bdf}: could not allocate wlan0 buffers: {error:?}");
                BusProbeResult::Claimed
            }
        };
    }
    // Keep a matched but unusable function claimed so another driver cannot
    // take ownership after this probe has touched its BAR/configuration.
    BusProbeResult::Claimed
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use axdriver_iwx::{INTEL_VENDOR_ID, setup_ht_rate_capabilities, setup_vht_rate_capabilities};
    use axdriver_net::NetDriverOps;

    use super::*;

    fn rsn_ie(akm: u8, capabilities: u16, group_management: bool) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0x00, 0x0f, 0xac, 4]);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0x00, 0x0f, 0xac, 4]);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0x00, 0x0f, 0xac, akm]);
        body.extend_from_slice(&capabilities.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        if group_management {
            body.extend_from_slice(&[0x00, 0x0f, 0xac, 6]);
        }
        let mut ie = vec![48, body.len() as u8];
        ie.extend_from_slice(&body);
        ie
    }

    #[test]
    fn sae_auth_body_translates_nl80211_enum_and_preserves_sae_data() {
        let request = axdriver_net::WirelessAuthenticateRequest {
            authentication_type: NL80211_AUTH_TYPE_SAE,
            authentication_data: vec![1, 0, 0, 0, 0xaa, 0xbb],
            ..Default::default()
        };
        assert_eq!(
            build_userspace_auth_body(&request).unwrap(),
            [3, 0, 1, 0, 0, 0, 0xaa, 0xbb]
        );
        let open = axdriver_net::WirelessAuthenticateRequest::default();
        assert_eq!(
            build_userspace_auth_body(&open).unwrap(),
            [0, 0, 1, 0, 0, 0]
        );
    }

    #[test]
    fn sme_rsn_admission_requires_sae_mfp_and_allows_wpa2_open_auth() {
        let suites = [NL80211_CIPHER_CCMP];
        let sae = rsn_ie(8, tk_net80211::RSNCAP_MFPC | tk_net80211::RSNCAP_MFPR, true);
        assert_eq!(
            sme_rsn_valid(
                &sae,
                1,
                &suites,
                Some(NL80211_CIPHER_CCMP),
                &[NL80211_AKM_SAE]
            ),
            Some((true, true))
        );
        assert_eq!(
            sme_rsn_valid(
                &sae,
                0,
                &suites,
                Some(NL80211_CIPHER_CCMP),
                &[NL80211_AKM_SAE]
            ),
            None
        );
        let psk = rsn_ie(2, 0, false);
        assert_eq!(
            sme_rsn_valid(
                &psk,
                0,
                &suites,
                Some(NL80211_CIPHER_CCMP),
                &[NL80211_AKM_PSK]
            ),
            Some((true, false))
        );
    }

    #[test]
    fn msix_table_layout_checks_bir_and_complete_vector_bounds() {
        let layout = IwxMsixTable::decode(64, 0x2004).unwrap();
        assert_eq!(layout.bir, 4);
        assert_eq!(layout.vectors, 65);
        assert!(!layout.fits(0x2010));
        assert!(layout.fits(0x2410));
        assert!(IwxMsixTable::decode(0, 6).is_none());
    }

    #[test]
    fn openbsd_pci_match_table_and_bz_gf_exception_are_preserved() {
        assert!(pci_match_decision(INTEL_VENDOR_ID, AX211_DEVICE_ID, None));
        assert!(!pci_match_decision(0xffff, AX211_DEVICE_ID, None));
        assert!(!pci_match_decision(INTEL_VENDOR_ID, 0xffff, None));
        assert!(!pci_match_decision(
            INTEL_VENDOR_ID,
            SPECIAL_BZ_DEVICE,
            None
        ));
        assert!(!pci_match_decision(
            INTEL_VENDOR_ID,
            SPECIAL_BZ_DEVICE,
            Some(0)
        ));
        assert!(pci_match_decision(
            INTEL_VENDOR_ID,
            SPECIAL_BZ_DEVICE,
            Some(u32::from(RF_TYPE_GF) << 12)
        ));
    }

    #[test]
    fn published_wireless_netdev_uses_named_ethernet_compatible_contract() {
        let mut device = IwxNetDevice::try_new(DeviceFunction {
            bus: 0,
            device: 1,
            function: 0,
        })
        .unwrap();
        assert_eq!(device.interface_name(), Some("wlan0"));
        assert!(device.is_wireless());
        assert!(!device.can_transmit());
        assert!(!device.can_receive());
        let mut packet = device.alloc_tx_buffer(32).unwrap();
        packet.packet_mut().fill(0x5a);
        assert_eq!(packet.packet(), &[0x5a; 32]);
        device.recycle_rx_buffer(packet).unwrap();
    }

    #[test]
    fn station_join_admission_requires_open_system_and_exact_ssid_element() {
        let mut request = axdriver_net::WirelessConnectRequest {
            ssid: b"open".to_vec(),
            ..Default::default()
        };
        assert!(is_open_station_request(&request));
        request.wpa_versions = 2;
        assert!(!is_open_station_request(&request));

        assert_eq!(
            bss_ssid(&[0, 4, b'o', b'p', b'e', b'n', 1, 1, 2]),
            Some(&b"open"[..])
        );
        assert_eq!(bss_ssid(&[1, 1]), None);
        assert_eq!(bss_ssid(&[0, 3, b'o']), None);
    }

    #[test]
    fn secure_join_admission_selects_only_wpa2_psk_ccmp() {
        let body = tk_net80211::build_rsn_body(&tk_net80211::RsnIePolicy {
            group_cipher: tk_net80211::CIPHER_CCMP,
            pairwise_ciphers: tk_net80211::CIPHER_CCMP,
            akms: tk_net80211::AKM_PSK,
            station_mode: true,
            ..Default::default()
        })
        .unwrap();
        let mut information_elements = vec![0, 4, b't', b'e', b's', b't'];
        information_elements.push(48);
        information_elements.push(body.len() as u8);
        information_elements.extend_from_slice(&body);
        let bss = axdriver_net::WirelessBssInfo {
            bssid: [2, 3, 4, 5, 6, 7],
            frequency_mhz: 2412,
            signal_mbm: -6000,
            timestamp: 1,
            beacon_interval: 100,
            capability: 0x0011,
            information_elements,
            is_probe_response: false,
        };
        let request = axdriver_net::WirelessConnectRequest {
            ssid: b"test".to_vec(),
            wpa_versions: 2,
            pairwise_ciphers: vec![0x000fac04],
            group_cipher: Some(0x000fac04),
            akm_suites: vec![0x000fac02],
            ..Default::default()
        };
        let policy = supplicant_rsn_policy(&request, &bss).unwrap();
        assert_eq!(policy.group_cipher, tk_net80211::CIPHER_CCMP);
        assert_eq!(policy.pairwise_ciphers, tk_net80211::CIPHER_CCMP);
        assert_eq!(policy.akms, tk_net80211::AKM_PSK);

        let mut unsupported = request.clone();
        unsupported.wpa_versions = 1;
        assert_eq!(
            supplicant_rsn_policy(&unsupported, &bss),
            Err(RuntimeStartError::UnsupportedSecurity)
        );

        let mfp_body = tk_net80211::build_rsn_body(&tk_net80211::RsnIePolicy {
            group_cipher: tk_net80211::CIPHER_CCMP,
            pairwise_ciphers: tk_net80211::CIPHER_CCMP,
            akms: tk_net80211::AKM_PSK,
            peer_capabilities: tk_net80211::RSNCAP_MFPC,
            mfp_capable: true,
            station_mode: true,
            group_management_cipher: tk_net80211::IE_CIPHER_BIP,
            ..Default::default()
        })
        .unwrap();
        let mut mfp_bss = bss.clone();
        mfp_bss.information_elements.truncate(6);
        mfp_bss.information_elements.push(48);
        mfp_bss.information_elements.push(mfp_body.len() as u8);
        mfp_bss.information_elements.extend_from_slice(&mfp_body);
        let mut required = request.clone();
        required.use_mfp = 1;
        let required_policy = supplicant_rsn_policy(&required, &mfp_bss).unwrap();
        assert!(required_policy.mfp_capable && required_policy.mfp_required);
        assert_eq!(
            required_policy.group_management_cipher,
            tk_net80211::IE_CIPHER_BIP
        );

        let mut optional = request.clone();
        optional.use_mfp = 2;
        let optional_policy = supplicant_rsn_policy(&optional, &mfp_bss).unwrap();
        assert!(optional_policy.mfp_capable && !optional_policy.mfp_required);

        let mut required_without_peer_mfpc = request;
        required_without_peer_mfpc.use_mfp = 1;
        assert_eq!(
            supplicant_rsn_policy(&required_without_peer_mfpc, &bss),
            Err(RuntimeStartError::UnsupportedSecurity)
        );
    }

    #[test]
    fn source_ht_vht_capabilities_follow_nvm_mimo_and_sku_flags() {
        let nvm = NvmInfo {
            hardware_address: [2, 0, 0, 0, 0, 1],
            nvm_version: 0,
            board_type: 0,
            hardware_address_count: 1,
            empty_otp: false,
            band_24ghz: true,
            band_52ghz: true,
            supports_11n: true,
            supports_11ac: true,
            supports_11ax: true,
            mimo_disabled: false,
            valid_tx_antennas: 0x03,
            valid_rx_antennas: 0x03,
            lar_enabled: false,
            channel_profiles: Vec::new(),
        };
        let ht = encode_ht_capabilities(setup_ht_rate_capabilities(
            nvm.valid_tx_antennas,
            nvm.valid_rx_antennas,
            true,
        ));
        assert_eq!(ht.capability, 0x01ee);
        assert_eq!(&ht.mcs_set[..2], &[0xff, 0xff]);
        assert_eq!(ht.mcs_set[12], 1);
        assert_eq!(ht.ampdu_parameters, 0x17);

        let vht = encode_vht_capabilities(setup_vht_rate_capabilities(
            nvm.valid_tx_antennas,
            nvm.valid_rx_antennas,
            true,
        ));
        assert_eq!(vht.capability, 0x0380_01e4);
        assert_eq!(&vht.mcs_set[..2], &[0xfa, 0xff]);
        assert_eq!(&vht.mcs_set[4..6], &[0xfa, 0xff]);

        let mut single_stream = nvm.clone();
        single_stream.mimo_disabled = true;
        let ht = encode_ht_capabilities(setup_ht_rate_capabilities(
            single_stream.valid_tx_antennas,
            single_stream.valid_rx_antennas,
            false,
        ));
        assert_eq!(&ht.mcs_set[..2], &[0xff, 0]);
        let vht = encode_vht_capabilities(setup_vht_rate_capabilities(
            single_stream.valid_tx_antennas,
            single_stream.valid_rx_antennas,
            false,
        ));
        assert_eq!(&vht.mcs_set[..2], &[0xfe, 0xff]);

        let no_phy = WirelessPhyCapabilities::default();
        assert!(no_phy.vht.is_none());
    }
}
