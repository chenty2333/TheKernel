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
use axdriver_iwx::{
    AX211_DEVICE_ID, AttachAllocationError, AttachProfile, CsrAccess, DmaAllocator, DmaError,
    DmaRegion, FirmwareBundle, FirmwareError, FirmwareImage, INTEL_VENDOR_ID, IoBarrier,
    IwxController, NvmInfo, RuntimeConfig, attach_profile, matches_pci_device,
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
    fn drop(&mut self) {
        global_allocator().dealloc_pages(self.cpu.as_ptr() as usize, self.pages, UsageKind::Dma);
    }
}

struct PlatformDmaAllocator;
impl DmaAllocator for PlatformDmaAllocator {
    type Region = PlatformDmaRegion;
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
    bar_base: usize,
    bar_size: usize,
    hardware_revision: u32,
    controller: IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    firmware: Option<Result<FirmwareBundle, FirmwareRequestError>>,
    nvm: Option<NvmInfo>,
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
    Firmware(axdriver_iwx::ControllerError<AliveWaitError>),
    PostAlive(axdriver_iwx::IctError),
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
enum FirmwareRequestError {
    UcodeMissing,
    UcodeInvalid(FirmwareError),
    PnvmMissing,
}

static ATTACHED_DMA: Mutex<Vec<AttachedDevice>> = Mutex::new(Vec::new());
static ROOTFS_CALLBACK_REGISTERED: AtomicBool = AtomicBool::new(false);

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
        bar_base,
        bar_size,
        hardware_revision,
        controller,
        firmware: None,
        nvm: None,
    });
    Ok(())
}

fn bootstrap_init_firmware(
    controller: &mut IwxController<MmioCsrAccess, PlatformDmaAllocator>,
    profile: AttachProfile,
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
        .boot_firmware(
            &bundle.image,
            true,
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
                        return result.map(|info| info.alive_ok);
                    }
                    controller.registers.delay_us(1_000);
                    elapsed += 1_000_000;
                }
                Ok(false)
            },
        )
        .map_err(FirmwareBootstrapError::Firmware)?;
    controller
        .post_alive(&bundle.image)
        .map_err(FirmwareBootstrapError::PostAlive)?;

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

fn log_bootstrap_error(bdf: Bdf, error: FirmwareBootstrapError) {
    use axdriver_iwx::ControllerError;

    match error {
        FirmwareBootstrapError::Hardware(error) => {
            warn!("iwx: {bdf:?}: hardware/APM startup failed: {error:?}")
        }
        FirmwareBootstrapError::Nic(error) => {
            warn!("iwx: {bdf:?}: NIC initialization failed: {error:?}")
        }
        FirmwareBootstrapError::Firmware(error) => match error {
            ControllerError::Attach(error) => warn!("iwx: {bdf:?}: attach failed: {error:?}"),
            ControllerError::Hardware(error) => warn!("iwx: {bdf:?}: APM failed: {error:?}"),
            ControllerError::Dma(error) => warn!("iwx: {bdf:?}: firmware DMA failed: {error:?}"),
            ControllerError::Context(error) => {
                warn!("iwx: {bdf:?}: firmware context failed: {error:?}")
            }
            ControllerError::Register(error) => {
                warn!("iwx: {bdf:?}: firmware register write failed: {error:?}")
            }
            ControllerError::Wait(error) => match error {
                AliveWaitError::Interrupt(error) => {
                    warn!("iwx: {bdf:?}: ALIVE interrupt service failed: {error:?}")
                }
                AliveWaitError::Receive(error) => {
                    warn!("iwx: {bdf:?}: ALIVE RX processing failed: {error:?}")
                }
                AliveWaitError::Alive(error) => {
                    warn!("iwx: {bdf:?}: ALIVE notification malformed: {error:?}")
                }
            },
            ControllerError::FirmwareNotAlive => {
                warn!("iwx: {bdf:?}: firmware did not report ALIVE")
            }
        },
        FirmwareBootstrapError::PostAlive(error) => {
            warn!("iwx: {bdf:?}: post-ALIVE ICT setup failed: {error:?}")
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
        }
        Err(error) => {
            warn!("iwx: {bdf}: matched PCI function has no usable runtime profile: {error:?}")
        }
    }
    debug_assert_eq!(info.vendor_id, INTEL_VENDOR_ID);
    if info.device_id == AX211_DEVICE_ID {
        info!("iwx: {bdf}: AX211 PCI discovery complete; firmware runtime attach follows");
    }
    // The upstream match has claimed this PCI function. Full interface
    // publication is performed by the iwx network/net80211 adapter.
    BusProbeResult::Claimed
}

#[cfg(test)]
mod tests {
    use axdriver_iwx::INTEL_VENDOR_ID;

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
}
