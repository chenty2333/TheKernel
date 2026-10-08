//! PCI binding for the shared Intel e1000/e1000e/igb driver.
//!
//! Adapted from FreeBSD `sys/dev/e1000/if_em.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2001-2024, Intel Corporation.
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2024 Kevin Bowling <kbowling@FreeBSD.org>.
//! SPDX-License-Identifier: BSD-2-Clause

use core::{ptr::NonNull, time::Duration};

use axalloc::{UsageKind, global_allocator};
use axdriver_net::{
    NetDriverOps,
    e1000::{
        E1000Hal, E1000Nic,
        api::{E1000MacType, set_mac_type},
        osdep::{E1000PciConfig, read_pci_cfg, write_pci_cfg},
    },
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use axhal::mem::virt_to_phys;
use log::{info, warn};

use crate::drivers::BusProbeResult;

const INTEL_VENDOR_ID: u16 = 0x8086;
const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 0x0002;
const PCI_COMMAND_MASTER: u16 = 0x0004;
const PCI_CLASS_NETWORK: u8 = 0x02;
const PCI_BAR0: u8 = 0;
const MIN_BAR_BYTES: usize = 0x6000;
const RING_SIZE: usize = 128;

pub struct PlatformHal;

struct E1000PciConfigAdapter<'a> {
    root: &'a mut PciRoot,
    bdf: DeviceFunction,
}

impl E1000PciConfig for E1000PciConfigAdapter<'_> {
    fn read_config_u16(&mut self, register: u32) -> Option<u16> {
        let offset = u8::try_from(register).ok()?;
        if offset & 1 != 0 {
            return None;
        }
        let dword_offset = offset & !3;
        let dword = self.root.read_config_dword(self.bdf, dword_offset)?;
        Some((dword >> (u32::from(offset & 2) * 8)) as u16)
    }

    fn write_config_u16(&mut self, register: u32, value: u16) -> bool {
        let Ok(offset) = u8::try_from(register) else {
            return false;
        };
        self.root.write_config_u16(self.bdf, offset, value)
    }

    fn find_capability(&mut self, capability_id: u8) -> Option<u32> {
        self.root
            .capabilities(self.bdf)
            .find(|capability| capability.id == capability_id)
            .map(|capability| u32::from(capability.offset))
    }
}

impl E1000Hal for PlatformHal {
    fn dma_alloc(pages: usize) -> Option<(u64, NonNull<u8>)> {
        let virtual_address = global_allocator()
            .alloc_pages(pages, 4096, UsageKind::Dma)
            .ok()?;
        let pointer = NonNull::new(virtual_address as *mut u8)?;
        Some((
            virt_to_phys(virtual_address.into()).as_usize() as u64,
            pointer,
        ))
    }

    unsafe fn dma_dealloc(_bus: u64, pointer: NonNull<u8>, pages: usize) {
        global_allocator().dealloc_pages(pointer.as_ptr() as usize, pages, UsageKind::Dma);
    }

    fn delay_us(micros: u32) {
        axhal::time::busy_wait(Duration::from_micros(u64::from(micros)));
    }
}

// upstream: if_em.c em_vendor_info_array[] Intel PCI IDs
const E1000_PCI_IDS: &[u16] = &[
    0x0438, 0x043a, 0x043c, 0x0440, 0x0d4c, 0x0d4d, 0x0d4e, 0x0d4f, 0x0d53, 0x0d55, 0x0dc5, 0x0dc6,
    0x0dc7, 0x0dc8, 0x1000, 0x1001, 0x1004, 0x1008, 0x1009, 0x100c, 0x100d, 0x100e, 0x100f, 0x1010,
    0x1011, 0x1012, 0x1013, 0x1014, 0x1015, 0x1016, 0x1017, 0x1018, 0x1019, 0x101a, 0x101d, 0x101e,
    0x1026, 0x1027, 0x1028, 0x1049, 0x104a, 0x104b, 0x104c, 0x104d, 0x105e, 0x105f, 0x1060, 0x1075,
    0x1076, 0x1077, 0x1078, 0x1079, 0x107a, 0x107b, 0x107c, 0x107d, 0x107e, 0x107f, 0x108a, 0x108b,
    0x108c, 0x1096, 0x1098, 0x1099, 0x109a, 0x10a4, 0x10a5, 0x10a7, 0x10a9, 0x10b5, 0x10b9, 0x10ba,
    0x10bb, 0x10bc, 0x10bd, 0x10bf, 0x10c0, 0x10c2, 0x10c3, 0x10c4, 0x10c5, 0x10c9, 0x10ca, 0x10cb,
    0x10cc, 0x10cd, 0x10ce, 0x10d3, 0x10d5, 0x10d6, 0x10d9, 0x10da, 0x10de, 0x10df, 0x10e5, 0x10e6,
    0x10e7, 0x10e8, 0x10ea, 0x10eb, 0x10ef, 0x10f0, 0x10f5, 0x10f6, 0x1501, 0x1502, 0x1503, 0x150a,
    0x150c, 0x150d, 0x150e, 0x150f, 0x1510, 0x1511, 0x1516, 0x1518, 0x1520, 0x1521, 0x1522, 0x1523,
    0x1524, 0x1525, 0x1526, 0x1527, 0x152d, 0x152f, 0x1533, 0x1534, 0x1535, 0x1536, 0x1537, 0x1538,
    0x1539, 0x153a, 0x153b, 0x1559, 0x155a, 0x156f, 0x1570, 0x157b, 0x157c, 0x15a0, 0x15a1, 0x15a2,
    0x15a3, 0x15b7, 0x15b8, 0x15b9, 0x15bb, 0x15bc, 0x15bd, 0x15be, 0x15d6, 0x15d7, 0x15d8, 0x15df,
    0x15e0, 0x15e1, 0x15e2, 0x15e3, 0x15f4, 0x15f5, 0x15f6, 0x15f9, 0x15fa, 0x15fb, 0x15fc, 0x1a1c,
    0x1a1d, 0x1a1e, 0x1a1f, 0x1f40, 0x1f41, 0x1f45, 0x294c, 0x550a, 0x550b, 0x550c, 0x550d, 0x550e,
    0x550f, 0x5510, 0x5511, 0x57a0, 0x57a1, 0x57b3, 0x57b4, 0x57b5, 0x57b6, 0x57b7, 0x57b8, 0x57b9,
    0x57ba,
];

fn e1000_family(device_id: u16) -> bool {
    E1000_PCI_IDS.contains(&device_id)
}

#[cfg(test)]
mod tests {
    use super::e1000_family;

    #[test]
    fn probe_table_covers_legacy_e1000e_and_igb_ids() {
        for id in [0x100e, 0x10d3, 0x10c9, 0x1533, 0x15f4] {
            assert!(e1000_family(id), "missing e1000 PCI ID {id:#06x}");
        }
        assert!(!e1000_family(0xdead));
    }
}

/// PCI match and BAR0 resource setup from the if_em probe path.
// upstream: if_em.c em_probe()
pub(crate) fn probe(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
) -> BusProbeResult {
    if info.vendor_id != INTEL_VENDOR_ID || !e1000_family(info.device_id) {
        return BusProbeResult::NotMatched;
    }
    let mac_type = match set_mac_type(info.device_id) {
        Ok(mac_type) => mac_type,
        Err(error) => {
            warn!("e1000: {bdf} cannot select shared MAC type: {error:?}");
            return BusProbeResult::Claimed;
        }
    };
    if info.class != PCI_CLASS_NETWORK {
        return BusProbeResult::Claimed;
    }
    let (address, size) = match root.bar_info(bdf, PCI_BAR0) {
        Ok(BarInfo::Memory { address, size, .. })
            if address != 0 && size as usize >= MIN_BAR_BYTES =>
        {
            (address, size as usize)
        }
        _ => {
            warn!("e1000: {bdf} has no usable MMIO BAR0");
            return BusProbeResult::Claimed;
        }
    };
    {
        let mut pci = E1000PciConfigAdapter { root, bdf };
        let Ok(command) = read_pci_cfg(&mut pci, u32::from(PCI_COMMAND)) else {
            return BusProbeResult::Claimed;
        };
        if write_pci_cfg(
            &mut pci,
            u32::from(PCI_COMMAND),
            command | PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER,
        )
        .is_err()
        {
            warn!("e1000: {bdf} could not enable memory/bus-master PCI command bits");
            return BusProbeResult::Claimed;
        }
    }
    let Ok(mapped) = axklib::mem::iomap((address as usize).into(), size) else {
        warn!("e1000: {bdf} BAR0 mapping failed");
        return BusProbeResult::Claimed;
    };
    let Some(mmio) = NonNull::new(mapped.as_usize() as *mut u8) else {
        return BusProbeResult::Claimed;
    };
    let nic = match E1000Nic::<PlatformHal, RING_SIZE>::new(
        mmio,
        size,
        false,
        matches!(mac_type, E1000MacType::Pch2Lan),
    ) {
        Ok(nic) => nic,
        Err(error) => {
            warn!(
                "e1000: {bdf} {:#06x} ring initialization failed: {error:?}",
                info.device_id
            );
            return BusProbeResult::Claimed;
        }
    };
    let mac = nic.mac_address().0;
    info!(
        "e1000: {bdf} {:04x}:{:04x} {} MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} polling",
        info.vendor_id,
        info.device_id,
        match info.device_id {
            0x100e => "82540EM",
            0x10d3 | 0x10f6 => "82574L",
            0x10c9 => "82576",
            _ => "8254x",
        },
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    #[cfg(not(feature = "dyn"))]
    return BusProbeResult::Device(crate::AxDeviceEnum::from_e1000(nic));
    #[cfg(feature = "dyn")]
    BusProbeResult::Device(crate::AxDeviceEnum::from_net(nic))
}
