//! Firmware section DMA layout from OpenBSD iwx context-info setup.
//!
//! This is a bus-independent translation of the section-selection and copy
//! order; the PCI adapter supplies the actual DMA allocator and region type.
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230; functions
//! `iwx_alloc_fw_monitor_block()`, `iwx_alloc_fw_monitor()`,
//! `iwx_apply_debug_destination()`, and `iwx_set_ltr()`. ISC.
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>
//!   Author: Stefan Sperling <stsp@openbsd.org>
//! Copyright (c) 2014 Fixup Software Ltd.
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>

use alloc::vec::Vec;

use crate::{FirmwareImage, FirmwareSection, PnvmImage, SectionType};

const PNVM_MAX_SEGMENTS: usize = 64;
const PNVM_INFO_BYTES: usize = PNVM_MAX_SEGMENTS * 8;

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
    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DmaError> {
        if offset == 0 && bytes.len() == self.capacity() {
            self.write(bytes)
        } else {
            Err(DmaError::RegionTooSmall)
        }
    }
    fn read_at(&self, _offset: usize, _bytes: &mut [u8]) -> Result<(), DmaError> {
        Err(DmaError::RegionTooSmall)
    }
}

/// Platform allocator used to stage the firmware sections in device-visible memory.
pub trait DmaAllocator {
    type Region: DmaRegion;
    fn allocate(&mut self, size: usize) -> Result<Self::Region, DmaError>;
    fn allocate_aligned(
        &mut self,
        size: usize,
        alignment: usize,
    ) -> Result<Self::Region, DmaError> {
        let region = self.allocate(size)?;
        if alignment > 1 && region.device_address() % alignment as u64 != 0 {
            return Err(DmaError::RegionTooSmall);
        }
        Ok(region)
    }
}

/// LMAC/UMAC context images and separately-lived paging sections.
pub struct FirmwareDmaImages<R: DmaRegion> {
    pub lmac: Vec<R>,
    pub umac: Vec<R>,
    pub(crate) paging: Vec<R>,
    pub lmac_addresses: Vec<u64>,
    pub umac_addresses: Vec<u64>,
    pub paging_addresses: Vec<u64>,
}

/// Device-visible PNVM storage, retaining all regions referenced by firmware.
pub struct PnvmDmaImage<R: DmaRegion> {
    /// Address written into Gen3 context-info's PNVM base field.
    pub base_address: u64,
    pub total_size: usize,
    /// Present for fragmented PNVM; contains the 64 little-endian image addresses.
    pub info: Option<R>,
    /// Present for a legacy contiguous PNVM image.
    pub contiguous: Option<R>,
    /// Payload segments for fragmented PNVM, held alive until firmware consumes them.
    pub segments: Vec<R>,
}

/// Assemble PNVM payload as one contiguous region or Gen3's address-array layout.
// upstream: if_iwx.c iwx_pnvm_setup_fragmented() and iwx_pnvm_setup()
pub fn setup_pnvm<A: DmaAllocator>(
    allocator: &mut A,
    pnvm: &PnvmImage,
    fragmented: bool,
) -> Result<PnvmDmaImage<A::Region>, DmaError> {
    let total_size = pnvm.segments.iter().try_fold(0usize, |sum, segment| {
        sum.checked_add(segment.len())
            .ok_or(DmaError::RegionTooSmall)
    })?;
    if total_size == 0 || total_size != pnvm.total_size {
        return Err(DmaError::RegionTooSmall);
    }
    if !fragmented {
        let mut image = allocator.allocate(total_size)?;
        if image.capacity() < total_size {
            return Err(DmaError::RegionTooSmall);
        }
        let mut offset = 0usize;
        for segment in &pnvm.segments {
            image.write_at(offset, segment)?;
            offset += segment.len();
        }
        let base_address = image.device_address();
        return Ok(PnvmDmaImage {
            base_address,
            total_size,
            info: None,
            contiguous: Some(image),
            segments: Vec::new(),
        });
    }
    if pnvm.segments.len() > PNVM_MAX_SEGMENTS {
        return Err(DmaError::RegionTooSmall);
    }
    let mut info = allocator.allocate(PNVM_INFO_BYTES)?;
    if info.capacity() < PNVM_INFO_BYTES {
        return Err(DmaError::RegionTooSmall);
    }
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(pnvm.segments.len())
        .map_err(|_| DmaError::AllocationFailed)?;
    for (index, bytes) in pnvm.segments.iter().enumerate() {
        let mut region = allocator.allocate(bytes.len())?;
        if region.capacity() < bytes.len() {
            return Err(DmaError::RegionTooSmall);
        }
        region.write(bytes)?;
        info.write_at(index * 8, &region.device_address().to_le_bytes())?;
        segments.push(region);
    }
    let base_address = info.device_address();
    Ok(PnvmDmaImage {
        base_address,
        total_size,
        info: Some(info),
        contiguous: None,
        segments,
    })
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
    let mut region = allocator.allocate_aligned(section.bytes.len(), 1)?;
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
    initialize_firmware_sections_for_kind(allocator, firmware, SectionType::Regular)
}

/// Allocate Init ucode sections for the INIT_MVM bootstrap image.
// upstream: if_iwx.c iwx_init_fw_sec()
pub fn initialize_init_firmware_sections<A: DmaAllocator>(
    allocator: &mut A,
    firmware: &FirmwareImage,
) -> Result<FirmwareDmaImages<A::Region>, DmaError> {
    initialize_firmware_sections_for_kind(allocator, firmware, SectionType::Init)
}

fn initialize_firmware_sections_for_kind<A: DmaAllocator>(
    allocator: &mut A,
    firmware: &FirmwareImage,
    section_type: SectionType,
) -> Result<FirmwareDmaImages<A::Region>, DmaError> {
    let mut sections: Vec<&FirmwareSection> = Vec::new();
    sections
        .try_reserve(firmware.sections.len())
        .map_err(|_| DmaError::AllocationFailed)?;
    sections.extend(
        firmware
            .sections
            .iter()
            .filter(|section| section.kind == section_type),
    );
    let (lmac_count, umac_count, paging_count) =
        firmware.section_counts_by_layout_for(section_type);
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
        fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DmaError> {
            let dst = self
                .bytes
                .get_mut(offset..offset + bytes.len())
                .ok_or(DmaError::RegionTooSmall)?;
            dst.copy_from_slice(bytes);
            Ok(())
        }
        fn read_at(&self, offset: usize, bytes: &mut [u8]) -> Result<(), DmaError> {
            let src = self
                .bytes
                .get(offset..offset + bytes.len())
                .ok_or(DmaError::RegionTooSmall)?;
            bytes.copy_from_slice(src);
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
    const TLV_SEC_INIT: u32 = 20;

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

    #[test]
    fn init_firmware_allocator_selects_only_init_ucode_sections() {
        let regular = section(0x1000, &[0xaa]);
        let init = section(0x2000, &[0xbb, 0xcc]);
        let separator = section(0xffff_cccc, &[]);
        let paging_separator = section(0xaaaa_bbbb, &[]);
        let firmware = FirmwareImage::parse(&test_image(&[
            (TLV_SEC_RT, &regular),
            (TLV_SEC_RT, &separator),
            (TLV_SEC_RT, &regular),
            (TLV_SEC_RT, &paging_separator),
            (TLV_SEC_RT, &regular),
            (TLV_SEC_INIT, &init),
            (TLV_SEC_INIT, &separator),
            (TLV_SEC_INIT, &init),
            (TLV_SEC_INIT, &paging_separator),
        ]))
        .unwrap();
        let mut allocator = TestAllocator(Cell::new(0x400000));

        let regular_images = initialize_firmware_sections(&mut allocator, &firmware).unwrap();
        assert_eq!(regular_images.lmac[0].bytes, [0xaa]);
        let init_images = initialize_init_firmware_sections(&mut allocator, &firmware).unwrap();
        assert_eq!(init_images.lmac_addresses, [0x403000]);
        assert_eq!(init_images.lmac[0].bytes, [0xbb, 0xcc]);
        assert_eq!(init_images.umac.len(), 1);
        assert!(init_images.paging.is_empty());
    }

    #[test]
    fn assembles_contiguous_and_fragmented_pnvm_layouts() {
        let pnvm = PnvmImage {
            version: 7,
            total_size: 5,
            segments: vec![vec![1, 2], vec![3, 4, 5]],
        };
        let mut allocator = TestAllocator(Cell::new(0x200000));
        let contiguous = setup_pnvm(&mut allocator, &pnvm, false).unwrap();
        assert_eq!(contiguous.base_address, 0x200000);
        assert_eq!(contiguous.total_size, 5);
        assert_eq!(
            contiguous.contiguous.as_ref().unwrap().bytes,
            [1, 2, 3, 4, 5]
        );

        let fragmented = setup_pnvm(&mut allocator, &pnvm, true).unwrap();
        assert_eq!(fragmented.base_address, 0x201000);
        assert_eq!(fragmented.total_size, 5);
        let info = &fragmented.info.as_ref().unwrap().bytes;
        assert_eq!(u64::from_le_bytes(info[0..8].try_into().unwrap()), 0x202000);
        assert_eq!(
            u64::from_le_bytes(info[8..16].try_into().unwrap()),
            0x203000
        );
        assert_eq!(fragmented.segments[0].bytes, [1, 2]);
        assert_eq!(fragmented.segments[1].bytes, [3, 4, 5]);
    }

    struct FallbackAllocator {
        inner: TestAllocator,
        fail_above: usize,
    }
    impl DmaAllocator for FallbackAllocator {
        type Region = TestRegion;
        fn allocate(&mut self, size: usize) -> Result<Self::Region, DmaError> {
            if size > self.fail_above {
                return Err(DmaError::AllocationFailed);
            }
            self.inner.allocate(size)
        }
    }

    #[test]
    fn firmware_monitor_allocation_falls_back_to_largest_supported_power() {
        let mut allocator = FallbackAllocator {
            inner: TestAllocator(Cell::new(0x800000)),
            fail_above: 1 << 12,
        };
        let monitor = allocate_monitor(&mut allocator, 15, None).unwrap().unwrap();
        assert_eq!(monitor.size_power, 12);
        assert_eq!(monitor.region.capacity(), 1 << 12);
        assert!(
            allocate_monitor(&mut allocator, 16, None)
                .unwrap()
                .is_none()
        );
        assert!(
            allocate_monitor(&mut allocator, 0, Some(monitor))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn ltr_value_matches_source_fields_and_enable_predicates() {
        assert_eq!(ltr_long_value(), 0x80fa_80fa);
        struct Writes(Vec<(bool, u32, u32)>);
        impl LtrRegisterAccess for Writes {
            fn write_csr(&mut self, address: u32, value: u32) {
                self.0.push((false, address, value));
            }
            fn write_peripheral(&mut self, address: u32, value: u32) {
                self.0.push((true, address, value));
            }
        }
        let mut writes = Writes(Vec::new());
        set_ltr(&mut writes, false, 0, 0x10);
        assert_eq!(writes.0, [(false, 0xd4, ltr_long_value())]);
        writes.0.clear();
        set_ltr(&mut writes, true, 0x10, 0x10);
        assert_eq!(
            writes.0,
            [(true, 0xa0348c, 0xf), (true, 0xa03480, ltr_long_value())]
        );
        writes.0.clear();
        set_ltr(&mut writes, true, 0x11, 0x10);
        assert!(writes.0.is_empty());
    }
}

/// Allocated monitor buffer and the exponent of its actual size.
pub struct MonitorBuffer<R: DmaRegion> {
    pub region: R,
    pub size_power: u8,
}

/// Try the requested external monitor-buffer size, descending to the minimum.
// upstream: if_iwx.c iwx_alloc_fw_monitor_block()
pub fn allocate_monitor_block<A: DmaAllocator>(
    allocator: &mut A,
    max_power: u8,
    min_power: u8,
) -> Result<MonitorBuffer<A::Region>, DmaError> {
    let mut last_error = DmaError::AllocationFailed;
    for power in (min_power..=max_power).rev() {
        let Some(size) = 1usize.checked_shl(u32::from(power)) else {
            last_error = DmaError::RegionTooSmall;
            continue;
        };
        match allocator.allocate(size) {
            Ok(region) if region.capacity() >= size => {
                return Ok(MonitorBuffer {
                    region,
                    size_power: power,
                });
            }
            Ok(_) => last_error = DmaError::RegionTooSmall,
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// Allocate an external firmware monitor as `iwx_alloc_fw_monitor()` does.
// upstream: if_iwx.c iwx_alloc_fw_monitor()
pub fn allocate_monitor<A: DmaAllocator>(
    allocator: &mut A,
    firmware_size_power: u8,
    existing: Option<MonitorBuffer<A::Region>>,
) -> Result<Option<MonitorBuffer<A::Region>>, DmaError> {
    if existing.is_some() {
        return Ok(existing);
    }
    let max_power = if firmware_size_power == 0 {
        26
    } else {
        firmware_size_power.saturating_add(11)
    };
    if max_power > 26 {
        return Ok(None);
    }
    allocate_monitor_block(allocator, max_power, 11).map(Some)
}

/// Register operations used by firmware debug-destination metadata.
pub trait DebugRegisterTransaction {
    type Error;
    fn read_csr(&mut self, address: u32) -> u32;
    fn write_csr(&mut self, address: u32, value: u32);
    fn read_peripheral(&mut self, address: u32) -> u32;
    fn write_peripheral(&mut self, address: u32, value: u32);
    fn set_peripheral_bits(&mut self, address: u32, mask: u32) -> Result<(), Self::Error>;
    fn clear_peripheral_bits(&mut self, address: u32, mask: u32) -> Result<(), Self::Error>;
}

/// Register bus that serializes peripheral-register access with the NIC lock.
pub trait DebugRegisterAccess {
    type Error;
    type Transaction<'a>: DebugRegisterTransaction<Error = Self::Error>
    where
        Self: 'a;
    fn lock_nic(&mut self) -> Option<Self::Transaction<'_>>;
    fn write_peripheral(&mut self, address: u32, value: u32);
}

/// Debug destination application failure.
#[derive(Debug, PartialEq, Eq)]
pub enum DebugDestinationError<E> {
    Busy,
    Dma(DmaError),
    Register(E),
    InvalidTlv,
}

const DEBUG_DEST_HEADER_BYTES: usize = 22;
const CSR_ASSIGN: u8 = 0;
const CSR_SETBIT: u8 = 1;
const CSR_CLEARBIT: u8 = 2;
const PRPH_ASSIGN: u8 = 3;
const PRPH_SETBIT: u8 = 4;
const PRPH_CLEARBIT: u8 = 5;
const PRPH_BLOCKBIT: u8 = 9;
const EXTERNAL_MODE: u8 = 1;

/// Apply `FW_DBG_DEST` CSR/peripheral operations and attach the external buffer.
// upstream: if_iwx.c iwx_apply_debug_destination()
pub fn apply_debug_destination<A, R>(
    allocator: &mut A,
    registers: &mut R,
    destination: &[u8],
    monitor: &mut Option<MonitorBuffer<A::Region>>,
) -> Result<(), DebugDestinationError<R::Error>>
where
    A: DmaAllocator,
    R: DebugRegisterAccess,
{
    if destination.len() < DEBUG_DEST_HEADER_BYTES || destination[0] != 0 {
        return Err(DebugDestinationError::InvalidTlv);
    }
    let monitor_mode = destination[1];
    let size_power = destination[2];
    let base_reg = read_le_u32(destination, 4).ok_or(DebugDestinationError::InvalidTlv)?;
    let end_reg = read_le_u32(destination, 8).ok_or(DebugDestinationError::InvalidTlv)?;
    let base_shift = destination[20];
    let end_shift = destination[21];
    if monitor_mode == EXTERNAL_MODE && monitor.is_none() {
        *monitor =
            allocate_monitor(allocator, size_power, None).map_err(DebugDestinationError::Dma)?;
    }
    let operations = &destination[DEBUG_DEST_HEADER_BYTES..];
    // The C loop uses integer division for n_dest_reg; any trailing partial
    // operation bytes are not visited.
    let operations = &operations[..(operations.len() / 12) * 12];
    let mut nic = registers.lock_nic().ok_or(DebugDestinationError::Busy)?;
    for op in operations.as_chunks::<12>().0 {
        let operation = op[0];
        let address = read_le_u32(op, 4).ok_or(DebugDestinationError::InvalidTlv)?;
        let value = read_le_u32(op, 8).ok_or(DebugDestinationError::InvalidTlv)?;
        let mask = 1u32.checked_shl(value).unwrap_or(0);
        match operation {
            CSR_ASSIGN => nic.write_csr(address, value),
            CSR_SETBIT => {
                let current = nic.read_csr(address);
                nic.write_csr(address, current | mask);
            }
            CSR_CLEARBIT => {
                let current = nic.read_csr(address);
                nic.write_csr(address, current & !mask);
            }
            PRPH_ASSIGN => nic.write_peripheral(address, value),
            PRPH_SETBIT => nic
                .set_peripheral_bits(address, mask)
                .map_err(DebugDestinationError::Register)?,
            PRPH_CLEARBIT => nic
                .clear_peripheral_bits(address, mask)
                .map_err(DebugDestinationError::Register)?,
            PRPH_BLOCKBIT if nic.read_peripheral(address) & mask != 0 => break,
            PRPH_BLOCKBIT => {}
            _ => {}
        }
    }
    drop(nic);
    if monitor_mode == EXTERNAL_MODE
        && let Some(buffer) = monitor.as_ref()
    {
        let address = buffer.region.device_address();
        let size = buffer.region.capacity();
        let end = address
            .checked_add(size as u64)
            .and_then(|end| end.checked_sub(256))
            .ok_or(DebugDestinationError::InvalidTlv)?;
        registers.write_peripheral(base_reg, (address >> base_shift) as u32);
        registers.write_peripheral(end_reg, (end >> end_shift) as u32);
    }
    Ok(())
}

fn read_le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let word = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(word.try_into().ok()?))
}

/// Program the source's long-latency LTR workaround for non-integrated devices.
// upstream: if_iwx.c iwx_set_ltr()
#[allow(clippy::identity_op)] // keep the upstream scale-mask expressions exactly, even where they evaluate to zero
pub fn ltr_long_value() -> u32 {
    0x8000_0000
        | ((2 << 24) & 0x1c00_0000)
        | ((250 << 16) & 0x03ff_0000)
        | 0x0000_8000
        | ((2 << 8) & 0x0000_1c00)
        | (250 & 0x0000_03ff)
}

/// LTR CSR/peripheral locations for the So-family workaround.
pub trait LtrRegisterAccess {
    fn write_csr(&mut self, address: u32, value: u32);
    fn write_peripheral(&mut self, address: u32, value: u32);
}

/// Apply LTR only where the upstream iwx silicon-family predicates allow it.
// upstream: if_iwx.c iwx_set_ltr()
pub fn set_ltr<R: LtrRegisterAccess>(
    registers: &mut R,
    integrated: bool,
    device_family: u8,
    family_22000: u8,
) {
    const CSR_LTR_LONG_VAL_AD: u32 = 0x0d4;
    const HPM_MAC_LTR_CSR: u32 = 0x00a0_348c;
    const HPM_MAC_LTR_ENABLE_ALL: u32 = 0xf;
    const HPM_UMAC_LTR: u32 = 0x00a0_3480;
    if !integrated {
        registers.write_csr(CSR_LTR_LONG_VAL_AD, ltr_long_value());
    } else if device_family == family_22000 {
        registers.write_peripheral(HPM_MAC_LTR_CSR, HPM_MAC_LTR_ENABLE_ALL);
        registers.write_peripheral(HPM_UMAC_LTR, ltr_long_value());
    }
}
