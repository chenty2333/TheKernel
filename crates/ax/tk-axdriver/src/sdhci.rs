//! PCI binding for the SDHCI/MMC block path.
//!
//! Adapted from FreeBSD `sys/dev/sdhci/sdhci_pci.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2008 Alexander Motin <mav@FreeBSD.org>.
//! SPDX-License-Identifier: BSD-2-Clause

use core::{ptr, ptr::NonNull};

use axdriver_base::BaseDriverOps;
use axdriver_block::sdhci::{
    SDHCI_CAPABILITIES, SDHCI_CAPABILITIES2, SDHCI_HOST_VERSION, SDHCI_SPEC_VER_MASK, SdhciDisk,
    SdhciHost, SdhciIo,
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
    SDHCI_PCI_IDS
        .iter()
        .find(|entry| entry.id == id)
        .map_or(0, |entry| entry.quirks)
}

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
    let quirks = quirks_for_device(info.vendor_id, info.device_id);
    let host = SdhciHost::new_with_quirks(io, capabilities, capabilities2, version, quirks);
    let disk = match SdhciDisk::attach(host) {
        Ok(disk) => disk,
        Err(error) => {
            warn!("sdhci: {bdf} card initialization failed: {error:?}");
            return BusProbeResult::Claimed;
        }
    };
    let read_only = info.vendor_id == INTEL_EMMC_VID
        && info.device_id == INTEL_EMMC_DID
        && axhal::boot::command_line_value("mmc.allow_write") != Some("1");
    let partitions = disk.into_partition_devices(read_only);
    info!(
        "sdhci: {bdf} {:04x}:{:04x} published {} MMC block areas read_only={read_only}",
        info.vendor_id,
        info.device_id,
        partitions.len()
    );

    let mut devices = alloc::vec::Vec::new();
    for partition in partitions {
        let _name = alloc::string::String::from(partition.device_name());
        let _user_area = _name == "mmcblk0";
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
    }
}
