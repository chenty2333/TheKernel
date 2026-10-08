//! PCI match and early attach boundary for Intel iwx.
//!
//! The target hardware path is continued in `tk-axdriver-iwx`; this module
//! wires the upstream PCI match decision into TheKernel's PCI driver walk.

use alloc::vec::Vec;
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
    preinit_plan, setup_ht_rate_capabilities, setup_vht_rate_capabilities,
};
use axdriver_net::{
    EthernetAddress, NetBuf, NetBufPool, NetBufPtr, NetDriverOps, WirelessFrequency,
    WirelessHtCapabilities, WirelessPhyCapabilities, WirelessVhtCapabilities,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Bdf(u8, u8, u8);

struct AttachedDevice {
    bdf: Bdf,
    profile: AttachProfile,
    runtime: RuntimeConfig,
    bar_base: usize,
    bar_size: usize,
    hardware_revision: u32,
    controller: IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    firmware: Option<Result<FirmwareBundle, FirmwareRequestError>>,
    nvm: Option<NvmInfo>,
    preinit: Option<PreinitPlan>,
    runtime_started: bool,
    interface_up: bool,
    soft_blocked: bool,
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
        // Management and data queues are not exposed until a net80211 peer
        // and its firmware queue have both been installed.
        false
    }

    fn can_receive(&self) -> bool {
        // Until the RX notification and net80211 conversion worker is
        // attached, polling must not claim pending packets that cannot be
        // retired into the Ethernet receive queue.
        false
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
        Ok(())
    }

    fn transmit(&mut self, tx_buf: NetBufPtr) -> DevResult {
        // This early admission boundary deliberately refuses data until the
        // net80211 association/key state machine is connected to firmware.
        drop(unsafe { NetBuf::from_buf_ptr(tx_buf) });
        Err(DevError::Unsupported)
    }

    fn receive(&mut self) -> DevResult<NetBufPtr> {
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
        Some(10_000)
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
        controller,
        firmware: None,
        nvm: None,
        preinit: None,
        runtime_started: false,
        interface_up: false,
        soft_blocked: false,
    });
    Ok(())
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
    let command_version = bundle.image.lookup_command_version(
        axdriver_iwx::DATA_PATH_GROUP,
        axdriver_iwx::SCD_QUEUE_CONFIG_CMD,
    );
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
    controller
        .enable_management_queue(command_version)
        .map_err(|error| {
            warn!("iwx: runtime management queue enable failed: {error:?}");
            RuntimeStartError::ManagementQueue
        })?;
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
            // Use MMIO and bus mastering for controller DMA, but keep legacy
            // INTx disabled: initialization pumps the source interrupt/RX path
            // synchronously until the later network IRQ worker is installed.
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
            if let Err(error) = allocate_resources(
                bdf,
                profile,
                config,
                base,
                bar.1 as usize,
                hardware_revision,
                link_control,
                device_control2,
            ) {
                warn!("iwx: {bdf}: attach DMA allocation failed: {error:?}");
                return BusProbeResult::Claimed;
            }
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
    use axdriver_iwx::INTEL_VENDOR_ID;
    use axdriver_net::NetDriverOps;

    use super::*;

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
