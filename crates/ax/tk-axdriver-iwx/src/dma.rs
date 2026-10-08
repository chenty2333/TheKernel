//! Firmware section DMA layout from OpenBSD iwx context-info setup.
//!
//! This is a bus-independent translation of the section-selection and copy
//! order; the PCI adapter supplies the actual DMA allocator and region type.

use alloc::vec::Vec;

use crate::{FirmwareImage, FirmwareSection, SectionType};

/// DMA allocation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    AllocationFailed,
    RegionTooSmall,
}

/// One allocator-owned DMA region. Implementations must keep `device_address`
/// stable until the region is dropped and make `write` visible to the device.
pub trait DmaRegion {
    fn device_address(&self) -> u64;
    fn capacity(&self) -> usize;
    fn write(&mut self, bytes: &[u8]) -> Result<(), DmaError>;
}

/// Platform allocator used to stage the firmware sections in device-visible memory.
pub trait DmaAllocator {
    type Region: DmaRegion;
    fn allocate(&mut self, size: usize) -> Result<Self::Region, DmaError>;
}

/// LMAC/UMAC context images and separately-lived paging sections.
pub struct FirmwareDmaImages<R: DmaRegion> {
    pub lmac: Vec<R>,
    pub umac: Vec<R>,
    paging: Vec<R>,
    pub lmac_addresses: Vec<u64>,
    pub umac_addresses: Vec<u64>,
    pub paging_addresses: Vec<u64>,
}

impl<R: DmaRegion> FirmwareDmaImages<R> {
    /// Retire paging memory when the device is stopped; firmware context may
    /// remain allocated until its ALIVE notification.
    // upstream: if_iwx.c iwx_ctxt_info_free_paging()
    pub fn free_paging(&mut self) {
        self.paging.clear();
        self.paging_addresses.clear();
    }

    /// Release firmware image sections after firmware ALIVE.
    // upstream: if_iwx.c iwx_ctxt_info_free_fw_img()
    pub fn free_firmware_sections(&mut self) {
        self.lmac.clear();
        self.umac.clear();
        self.lmac_addresses.clear();
        self.umac_addresses.clear();
    }
}

/// Copy one firmware section into device-visible DMA memory.
// upstream: if_iwx.c iwx_ctxt_info_alloc_dma()
fn allocate_section<A: DmaAllocator>(
    allocator: &mut A,
    section: &FirmwareSection,
) -> Result<A::Region, DmaError> {
    let mut region = allocator.allocate(section.bytes.len())?;
    if region.capacity() < section.bytes.len() {
        return Err(DmaError::RegionTooSmall);
    }
    region.write(&section.bytes)?;
    Ok(region)
}

/// Allocate all regular-runtime LMAC, UMAC and paging images in source order.
///
/// The section counts and separator arithmetic are the same as
/// `iwx_init_fw_sec()`: one separator follows LMAC and two precede paging.
// upstream: if_iwx.c iwx_init_fw_sec()
pub fn initialize_firmware_sections<A: DmaAllocator>(
    allocator: &mut A,
    firmware: &FirmwareImage,
) -> Result<FirmwareDmaImages<A::Region>, DmaError> {
    let mut sections: Vec<&FirmwareSection> = Vec::new();
    sections
        .try_reserve(firmware.sections.len())
        .map_err(|_| DmaError::AllocationFailed)?;
    sections.extend(
        firmware
            .sections
            .iter()
            .filter(|section| section.kind == SectionType::Regular),
    );
    let (lmac_count, umac_count, paging_count) = firmware.section_counts_by_layout();
    let umac_start = lmac_count.saturating_add(1);
    let paging_start = lmac_count.saturating_add(umac_count).saturating_add(2);
    if umac_start > sections.len()
        || paging_start > sections.len()
        || lmac_count > sections.len()
        || umac_count > sections.len().saturating_sub(umac_start)
        || paging_count > sections.len().saturating_sub(paging_start)
    {
        return Err(DmaError::RegionTooSmall);
    }

    let mut lmac = Vec::new();
    let mut lmac_addresses = Vec::new();
    lmac.try_reserve(lmac_count)
        .map_err(|_| DmaError::AllocationFailed)?;
    lmac_addresses
        .try_reserve(lmac_count)
        .map_err(|_| DmaError::AllocationFailed)?;
    for section in &sections[..lmac_count] {
        let region = allocate_section(allocator, section)?;
        lmac_addresses.push(region.device_address());
        lmac.push(region);
    }

    let mut umac = Vec::new();
    let mut umac_addresses = Vec::new();
    umac.try_reserve(umac_count)
        .map_err(|_| DmaError::AllocationFailed)?;
    umac_addresses
        .try_reserve(umac_count)
        .map_err(|_| DmaError::AllocationFailed)?;
    for section in &sections[umac_start..umac_start + umac_count] {
        let region = allocate_section(allocator, section)?;
        umac_addresses.push(region.device_address());
        umac.push(region);
    }

    let mut paging = Vec::new();
    let mut paging_addresses = Vec::new();
    paging
        .try_reserve(paging_count)
        .map_err(|_| DmaError::AllocationFailed)?;
    paging_addresses
        .try_reserve(paging_count)
        .map_err(|_| DmaError::AllocationFailed)?;
    for section in &sections[paging_start..paging_start + paging_count] {
        let region = allocate_section(allocator, section)?;
        paging_addresses.push(region.device_address());
        paging.push(region);
    }

    Ok(FirmwareDmaImages {
        lmac,
        umac,
        paging,
        lmac_addresses,
        umac_addresses,
        paging_addresses,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::cell::Cell;

    use super::*;
    use crate::firmware::test_image;

    struct TestRegion {
        address: u64,
        bytes: Vec<u8>,
    }

    impl DmaRegion for TestRegion {
        fn device_address(&self) -> u64 {
            self.address
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), DmaError> {
            self.bytes.copy_from_slice(bytes);
            Ok(())
        }
    }

    struct TestAllocator(Cell<u64>);

    impl DmaAllocator for TestAllocator {
        type Region = TestRegion;
        fn allocate(&mut self, size: usize) -> Result<Self::Region, DmaError> {
            let address = self.0.get();
            self.0.set(address + 0x1000);
            Ok(TestRegion {
                address,
                bytes: vec![0; size],
            })
        }
    }

    const TLV_SEC_RT: u32 = 19;

    fn section(offset: u32, bytes: &[u8]) -> Vec<u8> {
        let mut value = offset.to_le_bytes().to_vec();
        value.extend_from_slice(bytes);
        value
    }

    #[test]
    fn allocates_lmac_umac_then_paging_and_copies_image_bytes() {
        let lmac = section(0x1000, &[1, 2]);
        let separator = section(0xffff_cccc, &[]);
        let umac = section(0x2000, &[3]);
        let paging_separator = section(0xaaaa_bbbb, &[]);
        let paging = section(0x3000, &[4, 5, 6]);
        let firmware = FirmwareImage::parse(&test_image(&[
            (TLV_SEC_RT, &lmac),
            (TLV_SEC_RT, &separator),
            (TLV_SEC_RT, &umac),
            (TLV_SEC_RT, &paging_separator),
            (TLV_SEC_RT, &paging),
        ]))
        .unwrap();
        let mut allocator = TestAllocator(Cell::new(0x100000));
        let mut images = initialize_firmware_sections(&mut allocator, &firmware).unwrap();
        assert_eq!(images.lmac_addresses, [0x100000]);
        assert_eq!(images.umac_addresses, [0x101000]);
        assert_eq!(images.paging_addresses, [0x102000]);
        assert_eq!(images.lmac[0].bytes, [1, 2]);
        assert_eq!(images.umac[0].bytes, [3]);
        assert_eq!(images.paging[0].bytes, [4, 5, 6]);
        images.free_paging();
        assert!(images.paging_addresses.is_empty());
        assert_eq!(images.lmac.len(), 1);
    }
}
