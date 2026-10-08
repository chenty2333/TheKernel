//! PCI match and early attach boundary for Intel iwx.
//!
//! The target hardware path is continued in `tk-axdriver-iwx`; this module
//! wires the upstream PCI match decision into TheKernel's PCI driver walk.

use alloc::vec::Vec;
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering, fence},
};

use axalloc::{UsageKind, global_allocator};
use axdriver_iwx::{
    AX211_DEVICE_ID, AttachAllocationError, DeviceConfig, DeviceFamily, DmaAllocator, DmaError,
    DmaRegion, FirmwareBundle, FirmwareError, FirmwareImage, INTEL_VENDOR_ID, IwxAttachResources,
    RuntimeConfig, allocate_attach_resources, lookup_config, matches_pci_device,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Bdf(u8, u8, u8);

struct AttachedDevice {
    bdf: Bdf,
    family: DeviceFamily,
    config: DeviceConfig,
    bar_base: usize,
    bar_size: usize,
    resources: IwxAttachResources<PlatformDmaRegion>,
    firmware: Option<Result<FirmwareBundle, FirmwareRequestError>>,
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

fn device_family(device_id: u16) -> Option<DeviceFamily> {
    match device_id {
        0x2723 | 0x02f0 | 0xa0f0 | 0x34f0 | 0x06f0 | 0x43f0 | 0x3df0 | 0x4df0 => {
            Some(DeviceFamily::Family22000)
        }
        0x2725 | 0x2726 | 0x51f0 | 0x7a70 | 0x51f1 | 0x7af0 | 0x7e40 | 0x54f0 | 0x7f70 => {
            Some(DeviceFamily::Ax210)
        }
        0x7740 => Some(DeviceFamily::Bz),
        _ => None,
    }
}

fn allocate_resources(
    bdf: DeviceFunction,
    family: DeviceFamily,
    config: DeviceConfig,
    bar_base: usize,
    bar_size: usize,
) -> Result<(), AttachAllocationError> {
    let key = Bdf(bdf.bus, bdf.device, bdf.function);
    let mut attached = ATTACHED_DMA.lock();
    if attached.iter().any(|entry| entry.bdf == key) {
        return Ok(());
    }
    let resources = allocate_attach_resources(&mut PlatformDmaAllocator, family)?;
    attached.try_reserve(1).map_err(|_| {
        AttachAllocationError::Allocation(
            axdriver_iwx::AttachAllocationStage::ContextInfo,
            DmaError::AllocationFailed,
        )
    })?;
    attached.push(AttachedDevice {
        bdf: key,
        family,
        config,
        bar_base,
        bar_size,
        resources,
        firmware: None,
    });
    Ok(())
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
    let pending: Vec<(Bdf, DeviceConfig)> = {
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
                .map(|device| (device.bdf, device.config)),
        );
        pending
    };
    for (bdf, config) in pending {
        let firmware = config.firmware();
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
                Ok(bundle) => info!(
                    "iwx: {bdf:?}: staged firmware version {:?} ({:?}); BAR0 {:#x}+{:#x}, {} TX \
                     queues and {} RX buffers retained",
                    bundle.image.api,
                    device.family,
                    device.bar_base,
                    device.bar_size,
                    device.resources.tx_queues.len(),
                    device.resources.rx_queue.buffers.len(),
                ),
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
    match lookup_config(config) {
        Some(selected) => {
            info!(
                "iwx: {bdf}: PCI {:#06x}:{:#06x} matched {selected:?} (subsystem {:#06x}:{:#06x})",
                info.vendor_id,
                info.device_id,
                root.endpoint_subsystem_ids(bdf).0,
                subsystem,
            );
            if let Some(family) = device_family(info.device_id) {
                if let Err(error) = allocate_resources(bdf, family, selected, base, bar.1 as usize)
                {
                    warn!("iwx: {bdf}: attach DMA allocation failed: {error:?}");
                    return BusProbeResult::Claimed;
                }
                register_rootfs_firmware_callback();
            }
        }
        None => warn!("iwx: {bdf}: matched PCI function has no runtime iwx config"),
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
