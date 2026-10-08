//! PCI binding for the SDHCI/MMC block path.
//!
//! Adapted from FreeBSD `sys/dev/sdhci/sdhci_pci.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2008 Alexander Motin <mav@FreeBSD.org>.
//! SPDX-License-Identifier: BSD-2-Clause

use core::{ptr, ptr::NonNull};

use axdriver_base::BaseDriverOps;
use axdriver_block::{
    BlockDriverOps,
    sdhci::{
        SDHCI_CAPABILITIES, SDHCI_CAPABILITIES2, SDHCI_HOST_VERSION, SDHCI_SPEC_VER_MASK,
        SdhciDisk, SdhciHost, SdhciIo,
    },
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use log::{info, warn};

const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 0x0002;
const PCI_CLASS_SYSTEM_PERIPHERAL: u8 = 0x08;
const PCI_SUBCLASS_SD_HOST: u8 = 0x05;
const SDHCI_BAR: u8 = 0;
const SDHCI_BAR_MIN_BYTES: usize = 0x100;
const INTEL_EMMC_VID: u16 = 0x8086;
const INTEL_EMMC_DID: u16 = 0x54c4;

struct SdhciWindow {
    base: NonNull<u8>,
    size: usize,
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
        // SAFETY: `base` is a mapped PCI BAR and register widths/alignment follow SDHCI.
        unsafe { ptr::read_volatile(self.base.as_ptr().add(offset).cast::<T>()) }
    }

    fn write<T: Copy>(&mut self, offset: usize, value: T) {
        assert!(
            offset
                .checked_add(core::mem::size_of::<T>())
                .is_some_and(|end| end <= self.size)
        );
        // SAFETY: `base` is a mapped PCI BAR and register widths/alignment follow SDHCI.
        unsafe { ptr::write_volatile(self.base.as_ptr().add(offset).cast::<T>(), value) }
    }
}

impl SdhciIo for SdhciWindow {
    fn read8(&mut self, offset: usize) -> u8 {
        self.read(offset)
    }
    fn read16(&mut self, offset: usize) -> u16 {
        self.read(offset)
    }
    fn read32(&mut self, offset: usize) -> u32 {
        self.read(offset)
    }
    fn write8(&mut self, offset: usize, value: u8) {
        self.write(offset, value)
    }
    fn write16(&mut self, offset: usize, value: u16) {
        self.write(offset, value)
    }
    fn write32(&mut self, offset: usize, value: u32) {
        self.write(offset, value)
    }
    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
}

/// FreeBSD `sdhci_pci_attach()` resource/slot adaptation: map BAR0, enable MMIO,
/// initialize the host and card, then return the block device and GPT children.
// upstream: sdhci_pci.c sdhci_pci_attach()
pub(crate) fn probe(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
) -> super::drivers::BusProbeResult {
    use super::drivers::BusProbeResult;
    if info.class != PCI_CLASS_SYSTEM_PERIPHERAL || info.subclass != PCI_SUBCLASS_SD_HOST {
        return BusProbeResult::NotMatched;
    }
    let (address, size) = match root.bar_info(bdf, SDHCI_BAR) {
        Ok(BarInfo::Memory { address, size, .. })
            if address != 0 && size as usize >= SDHCI_BAR_MIN_BYTES =>
        {
            (address, size as usize)
        }
        _ => {
            warn!("sdhci: {bdf} has no usable MMIO BAR0");
            return BusProbeResult::Claimed;
        }
    };
    let Some(command) = root.read_config_dword(bdf, PCI_COMMAND) else {
        return BusProbeResult::Claimed;
    };
    if !root.write_config_u16(bdf, PCI_COMMAND, command as u16 | PCI_COMMAND_MEMORY) {
        warn!("sdhci: {bdf} could not enable memory decoding");
        return BusProbeResult::Claimed;
    }
    let Ok(mapped) = axklib::mem::iomap((address as usize).into(), size) else {
        warn!("sdhci: {bdf} BAR0 mapping failed");
        return BusProbeResult::Claimed;
    };
    let Some(base) = NonNull::new(mapped.as_usize() as *mut u8) else {
        return BusProbeResult::Claimed;
    };
    let mut io = SdhciWindow { base, size };
    let capabilities = io.read32(SDHCI_CAPABILITIES as usize);
    let capabilities2 = io.read32(SDHCI_CAPABILITIES2 as usize);
    let version = (io.read16(SDHCI_HOST_VERSION as usize) & SDHCI_SPEC_VER_MASK as u16) as u8;
    let host = SdhciHost::new(io, capabilities, capabilities2, version);
    let mut disk = match SdhciDisk::attach(host) {
        Ok(disk) => disk,
        Err(error) => {
            warn!("sdhci: {bdf} card initialization failed: {error:?}");
            return BusProbeResult::Claimed;
        }
    };
    let read_only = info.vendor_id == INTEL_EMMC_VID
        && info.device_id == INTEL_EMMC_DID
        && axhal::boot::command_line_value("mmc.allow_write") != Some("1");
    disk.set_read_only(read_only);
    info!(
        "sdhci: {bdf} {:04x}:{:04x} /dev/mmcblk0 blocks={} read_only={read_only}",
        info.vendor_id,
        info.device_id,
        disk.num_blocks()
    );

    let mut devices = alloc::vec::Vec::new();
    #[cfg(feature = "shared-block")]
    {
        let name = alloc::string::String::from(disk.device_name());
        #[cfg(feature = "dyn")]
        let parent = crate::SharedBlockDevice::new(disk);
        #[cfg(not(feature = "dyn"))]
        let parent = crate::SharedBlockDevice::new(crate::StaticBlockDevice::Sdhci(
            alloc::boxed::Box::new(disk),
        ));
        #[cfg(feature = "dyn")]
        devices.push(crate::AxDeviceEnum::from_block(parent.clone()));
        #[cfg(not(feature = "dyn"))]
        devices.push(crate::AxDeviceEnum::Block(crate::StaticBlockDevice::Sdhci(
            alloc::boxed::Box::new(parent.clone()),
        )));
        match crate::discover_gpt_partitions(&parent, &name, read_only) {
            Ok(partitions) => {
                devices.extend(partitions.into_iter().map(crate::AxDeviceEnum::Block))
            }
            Err(error) => log::debug!("sdhci: no accepted GPT on {name}: {error:?}"),
        }
    }
    #[cfg(not(feature = "shared-block"))]
    {
        #[cfg(feature = "dyn")]
        devices.push(crate::AxDeviceEnum::from_block(disk));
        #[cfg(not(feature = "dyn"))]
        devices.push(crate::AxDeviceEnum::Block(crate::StaticBlockDevice::Sdhci(
            alloc::boxed::Box::new(disk),
        )));
    }
    if devices.is_empty() {
        BusProbeResult::Claimed
    } else {
        BusProbeResult::Devices(devices)
    }
}
