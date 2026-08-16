//! Owned Multiboot handoff data.
//!
//! The platform entry point is reached before the runtime clears `.bss`, while
//! the Multiboot information block is owned by the bootloader and may be
//! reused as soon as normal memory initialization starts.  This module keeps
//! the raw entry record in initialized data, then copies every value needed by
//! the platform into one immutable owner before handing control to the rest of
//! the platform code.

use core::convert::TryFrom;

use axplat::mem::{PhysAddr, RawRange, phys_to_virt};
use lazyinit::LazyInit;

use crate::boot::{EarlyBootRecord, MULTIBOOT_BOOTLOADER_MAGIC, MULTIBOOT2_BOOTLOADER_MAGIC};

pub(crate) const MAX_REGIONS: usize = 16;
const MAX_MB2_INFO_SIZE: usize = 16 * 1024 * 1024;
const MAX_RSDP_LENGTH: usize = 4096;

const MB2_TAG_END: u32 = 0;
const MB2_TAG_MMAP: u32 = 6;
const MB2_TAG_ACPI_OLD: u32 = 14;
const MB2_TAG_ACPI_NEW: u32 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootProtocol {
    Multiboot1,
    Multiboot2,
}

/// The subset of the ACPI RSDP needed to locate the root table.
///
/// The full RSDP is validated while the bootloader-owned bytes are still
/// borrowed.  Only this fixed-size copy is retained, so no platform code ever
/// holds a reference into the Multiboot information block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AcpiRsdp {
    bytes: [u8; 36],
    length: usize,
}

impl AcpiRsdp {
    pub(crate) fn bytes(&self) -> &[u8; 36] {
        &self.bytes
    }

    #[allow(dead_code)]
    pub(crate) fn length(&self) -> usize {
        self.length
    }
}

/// The sole owner of boot protocol data after early handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootInfo {
    protocol: BootProtocol,
    info_paddr: usize,
    rsdp: Option<AcpiRsdp>,
    memory_regions: [RawRange; MAX_REGIONS],
    memory_region_count: usize,
}

impl BootInfo {
    fn empty(protocol: BootProtocol, info_paddr: usize) -> Self {
        Self {
            protocol,
            info_paddr,
            rsdp: None,
            memory_regions: [(0, 0); MAX_REGIONS],
            memory_region_count: 0,
        }
    }

    pub(crate) fn protocol(&self) -> BootProtocol {
        self.protocol
    }

    pub(crate) fn info_paddr(&self) -> usize {
        self.info_paddr
    }

    pub(crate) fn rsdp(&self) -> Option<&AcpiRsdp> {
        self.rsdp.as_ref()
    }

    pub(crate) fn memory_regions(&self) -> &[RawRange] {
        &self.memory_regions[..self.memory_region_count]
    }
}

static BOOT_INFO: LazyInit<BootInfo> = LazyInit::new();

pub(crate) fn get() -> &'static BootInfo {
    BOOT_INFO
        .get()
        .expect("x86 boot handoff has not been finalized")
}

/// Finish the early handoff after `.bss` has been cleared.
pub(crate) fn finish_handoff() {
    let EarlyBootRecord { magic, info_paddr } = crate::boot::early_record();
    let protocol = match magic {
        MULTIBOOT_BOOTLOADER_MAGIC => BootProtocol::Multiboot1,
        MULTIBOOT2_BOOTLOADER_MAGIC => BootProtocol::Multiboot2,
        _ => panic!("unsupported x86 boot magic {magic:#x}"),
    };

    let owner = match protocol {
        BootProtocol::Multiboot1 => BootInfo::empty(protocol, info_paddr),
        BootProtocol::Multiboot2 => {
            let bytes = unsafe { multiboot2_info_bytes(info_paddr) }.unwrap_or_else(|| {
                panic!("invalid Multiboot2 information pointer {info_paddr:#x}")
            });
            parse_multiboot2_info(bytes, info_paddr)
                .unwrap_or_else(|error| panic!("invalid Multiboot2 information: {error:?}"))
        }
    };
    BOOT_INFO.init_once(owner);
}

/// Parse an MB2 information block from a physical address, without retaining
/// a borrow into it.
unsafe fn multiboot2_info_bytes(info_paddr: usize) -> Option<&'static [u8]> {
    if info_paddr & 7 != 0 {
        return None;
    }
    let header = unsafe { physical_bytes(info_paddr, 16)? };
    let total_size = read_u32(header, 0)? as usize;
    if !(16..=MAX_MB2_INFO_SIZE).contains(&total_size) || total_size & 7 != 0 {
        return None;
    }
    unsafe { physical_bytes(info_paddr, total_size) }
}

unsafe fn physical_bytes(address: usize, length: usize) -> Option<&'static [u8]> {
    address.checked_add(length)?;
    let ptr = phys_to_virt(PhysAddr::from_usize(address)).as_ptr();
    Some(unsafe { core::slice::from_raw_parts(ptr, length) })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseError {
    InfoTooSmall,
    InfoTooLarge,
    InfoNotAligned,
    ReservedHeader,
    TagHeaderTruncated,
    TagTooSmall,
    TagOutOfBounds,
    TagAlignment,
    EndTagMissing,
    EndTagMalformed,
    DuplicateMemoryMap,
    MemoryMapMissing,
    MemoryMapMalformed,
    MemoryMapCapacity,
    MemoryRangeOverflow,
    MemoryRangeConversion,
    AcpiTagTruncated,
    AcpiTagMalformed,
    AcpiSignature,
    AcpiChecksum,
    AcpiLength,
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let bytes = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let bytes = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn checksum_is_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn align_tag_size(size: usize) -> Option<usize> {
    size.checked_add(7).map(|size| size & !7)
}

fn parse_acpi_tag(payload: &[u8], tag_type: u32) -> Result<AcpiRsdp, ParseError> {
    let minimum = if tag_type == MB2_TAG_ACPI_NEW { 36 } else { 20 };
    if payload.len() < minimum {
        return Err(ParseError::AcpiTagTruncated);
    }
    if &payload[..8] != b"RSD PTR " {
        return Err(ParseError::AcpiSignature);
    }
    if !checksum_is_valid(&payload[..20]) {
        return Err(ParseError::AcpiChecksum);
    }

    let revision = payload[15];
    if tag_type == MB2_TAG_ACPI_NEW && revision < 2 {
        return Err(ParseError::AcpiTagMalformed);
    }
    let length = if revision >= 2 {
        if payload.len() < 36 {
            return Err(ParseError::AcpiLength);
        }
        let length = read_u32(payload, 20).ok_or(ParseError::AcpiLength)? as usize;
        if !(36..=MAX_RSDP_LENGTH).contains(&length) || length > payload.len() {
            return Err(ParseError::AcpiLength);
        }
        if !checksum_is_valid(&payload[..length]) {
            return Err(ParseError::AcpiChecksum);
        }
        length
    } else {
        20
    };

    let mut bytes = [0; 36];
    bytes[..36.min(payload.len())].copy_from_slice(&payload[..36.min(payload.len())]);
    Ok(AcpiRsdp { bytes, length })
}

fn parse_memory_map(
    tag: &[u8],
    regions: &mut [RawRange; MAX_REGIONS],
) -> Result<usize, ParseError> {
    if tag.len() < 16 {
        return Err(ParseError::MemoryMapMalformed);
    }
    let entry_size = read_u32(tag, 8).ok_or(ParseError::MemoryMapMalformed)? as usize;
    if entry_size < 24 || entry_size & 7 != 0 {
        return Err(ParseError::MemoryMapMalformed);
    }
    let payload_len = tag
        .len()
        .checked_sub(16)
        .ok_or(ParseError::MemoryMapMalformed)?;
    if payload_len % entry_size != 0 {
        return Err(ParseError::MemoryMapMalformed);
    }

    let mut count = 0;
    let mut offset = 16;
    while offset < tag.len() {
        let entry = tag
            .get(
                offset
                    ..offset
                        .checked_add(entry_size)
                        .ok_or(ParseError::MemoryMapMalformed)?,
            )
            .ok_or(ParseError::MemoryMapMalformed)?;
        let base = read_u64(entry, 0).ok_or(ParseError::MemoryMapMalformed)?;
        let length = read_u64(entry, 8).ok_or(ParseError::MemoryMapMalformed)?;
        let kind = read_u32(entry, 16).ok_or(ParseError::MemoryMapMalformed)?;
        if kind == 1 && length != 0 {
            let base = usize::try_from(base).map_err(|_| ParseError::MemoryRangeConversion)?;
            let length = usize::try_from(length).map_err(|_| ParseError::MemoryRangeConversion)?;
            base.checked_add(length)
                .ok_or(ParseError::MemoryRangeOverflow)?;
            if count == regions.len() {
                return Err(ParseError::MemoryMapCapacity);
            }
            regions[count] = (base, length);
            count += 1;
        }
        offset += entry_size;
    }
    Ok(count)
}

/// Parse a complete Multiboot2 information block into an owned `BootInfo`.
///
/// This function is intentionally independent of the physical-memory access
/// helper so host tests can exercise every boundary and malformed-tag case.
fn parse_multiboot2_info(bytes: &[u8], info_paddr: usize) -> Result<BootInfo, ParseError> {
    if info_paddr & 7 != 0 {
        return Err(ParseError::InfoNotAligned);
    }
    if bytes.len() < 8 {
        return Err(ParseError::InfoTooSmall);
    }
    let total_size = read_u32(bytes, 0).ok_or(ParseError::InfoTooSmall)? as usize;
    if total_size < 16 {
        return Err(ParseError::InfoTooSmall);
    }
    if total_size > MAX_MB2_INFO_SIZE {
        return Err(ParseError::InfoTooLarge);
    }
    if total_size & 7 != 0 {
        return Err(ParseError::InfoNotAligned);
    }
    if total_size > bytes.len() {
        return Err(ParseError::InfoTooSmall);
    }
    let bytes = &bytes[..total_size];
    if read_u32(bytes, 4) != Some(0) {
        return Err(ParseError::ReservedHeader);
    }

    let mut owner = BootInfo::empty(BootProtocol::Multiboot2, info_paddr);
    let mut cursor = 8;
    let mut saw_end = false;
    let mut saw_mmap = false;
    let mut acpi_old = None;
    let mut acpi_new = None;

    while cursor < total_size {
        if cursor & 7 != 0 {
            return Err(ParseError::TagAlignment);
        }
        let remaining = total_size - cursor;
        if remaining < 8 {
            return Err(ParseError::TagHeaderTruncated);
        }
        let tag_type = read_u32(bytes, cursor).ok_or(ParseError::TagHeaderTruncated)?;
        let tag_size = read_u32(bytes, cursor + 4).ok_or(ParseError::TagHeaderTruncated)? as usize;
        if tag_size < 8 {
            return Err(ParseError::TagTooSmall);
        }
        if tag_size > remaining {
            return Err(ParseError::TagOutOfBounds);
        }
        let aligned_size = align_tag_size(tag_size).ok_or(ParseError::TagAlignment)?;
        if aligned_size > remaining {
            return Err(ParseError::TagAlignment);
        }
        let tag = &bytes[cursor..cursor + tag_size];

        match tag_type {
            MB2_TAG_END => {
                if tag_size != 8 || read_u32(tag, 0) != Some(0) || cursor + 8 != total_size {
                    return Err(ParseError::EndTagMalformed);
                }
                saw_end = true;
                break;
            }
            MB2_TAG_MMAP => {
                if saw_mmap {
                    return Err(ParseError::DuplicateMemoryMap);
                }
                saw_mmap = true;
                owner.memory_region_count = parse_memory_map(tag, &mut owner.memory_regions)?;
            }
            MB2_TAG_ACPI_OLD | MB2_TAG_ACPI_NEW => {
                // Firmware occasionally leaves a stale/partial ACPI tag next
                // to a valid one.  Keep validating each candidate, but let a
                // valid new tag win over a valid old tag without allowing a
                // malformed candidate to discard the other valid copy.
                if let Ok(parsed) = parse_acpi_tag(&tag[8..], tag_type) {
                    if tag_type == MB2_TAG_ACPI_NEW {
                        acpi_new = Some(parsed);
                    } else {
                        acpi_old = Some(parsed);
                    }
                }
            }
            _ => {}
        }

        cursor += aligned_size;
    }

    if !saw_end {
        return Err(ParseError::EndTagMissing);
    }
    if !saw_mmap || owner.memory_region_count == 0 {
        return Err(ParseError::MemoryMapMissing);
    }
    owner.rsdp = acpi_new.or(acpi_old);
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::{AcpiRsdp, BootProtocol, MAX_REGIONS, ParseError, parse_multiboot2_info};

    fn push_u32(bytes: &mut std::vec::Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_tag(bytes: &mut std::vec::Vec<u8>, tag_type: u32, payload: &[u8]) {
        let size = 8 + payload.len();
        push_u32(bytes, tag_type);
        push_u32(bytes, size as u32);
        bytes.extend_from_slice(payload);
        while bytes.len() & 7 != 0 {
            bytes.push(0);
        }
    }

    fn set_total_size(bytes: &mut std::vec::Vec<u8>) {
        let size = bytes.len() as u32;
        bytes[..4].copy_from_slice(&size.to_le_bytes());
    }

    fn valid_rsdp(seed: u8) -> [u8; 36] {
        let mut rsdp = [0; 36];
        rsdp[..8].copy_from_slice(b"RSD PTR ");
        rsdp[9] = seed;
        rsdp[15] = 2;
        rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
        let checksum20 = (0u8).wrapping_sub(
            rsdp[..20]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        );
        rsdp[8] = checksum20;
        let checksum36 =
            (0u8).wrapping_sub(rsdp.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
        rsdp[32] = checksum36;
        rsdp
    }

    fn valid_mmap_tag_payload() -> [u8; 32] {
        let mut payload = [0; 32];
        payload[..4].copy_from_slice(&24u32.to_le_bytes());
        payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        payload[16..24].copy_from_slice(&0x0800_0000u64.to_le_bytes());
        payload[24..28].copy_from_slice(&1u32.to_le_bytes());
        payload
    }

    fn valid_info() -> std::vec::Vec<u8> {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 15, &valid_rsdp(2));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        bytes
    }

    #[test]
    fn parser_copies_mmap_and_new_rsdp() {
        let mut bytes = valid_info();
        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        // The Multiboot information block (and its ACPI tag) is only borrowed
        // during parsing.  The platform must retain the copied RSDP rather
        // than relying on the bootloader-owned bytes after the handoff.
        bytes.fill(0);
        assert_eq!(info.protocol(), BootProtocol::Multiboot2);
        assert_eq!(info.info_paddr(), 0x1000);
        assert_eq!(info.memory_regions(), &[(0, 0x0800_0000)]);
        assert_eq!(info.rsdp().unwrap().bytes()[9], 2);
    }

    #[test]
    fn new_acpi_tag_wins_over_old_tag() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 14, &valid_rsdp(1));
        push_tag(&mut bytes, 15, &valid_rsdp(2));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.rsdp().unwrap().bytes()[9], 2);
    }

    #[test]
    fn malformed_new_acpi_tag_falls_back_to_valid_old_tag() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &valid_mmap_tag_payload());
        push_tag(&mut bytes, 14, &valid_rsdp(1));
        let mut malformed_new = valid_rsdp(2);
        malformed_new[8] ^= 1;
        push_tag(&mut bytes, 15, &malformed_new);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.rsdp().unwrap().bytes()[9], 1);
    }

    #[test]
    fn malformed_and_missing_memory_maps_are_rejected() {
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 15, &valid_rsdp(2));
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::MemoryMapMissing)
        );

        let mut bytes = vec![0; 8];
        let mut payload = valid_mmap_tag_payload();
        payload[..4].copy_from_slice(&23u32.to_le_bytes());
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::MemoryMapMalformed)
        );
    }

    #[test]
    fn zero_length_available_entries_are_skipped() {
        let mut payload = vec![0; 8 + 24 * 2];
        payload[..4].copy_from_slice(&24u32.to_le_bytes());
        let second = 8 + 24;
        payload[second..second + 8].copy_from_slice(&0x2000u64.to_le_bytes());
        payload[second + 8..second + 16].copy_from_slice(&0x2000u64.to_le_bytes());
        payload[second + 16..second + 20].copy_from_slice(&1u32.to_le_bytes());
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.memory_regions(), &[(0x2000, 0x2000)]);
    }

    #[test]
    fn extended_mmap_entries_must_be_eight_byte_aligned_and_are_supported() {
        let mut payload = vec![0; 8 + 32];
        payload[..4].copy_from_slice(&32u32.to_le_bytes());
        payload[8..16].copy_from_slice(&0x3000u64.to_le_bytes());
        payload[16..24].copy_from_slice(&0x3000u64.to_le_bytes());
        payload[24..28].copy_from_slice(&1u32.to_le_bytes());
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);

        let info = parse_multiboot2_info(&bytes, 0x1000).unwrap();
        assert_eq!(info.memory_regions(), &[(0x3000, 0x3000)]);

        let mut malformed = bytes;
        malformed[16..20].copy_from_slice(&31u32.to_le_bytes());
        assert_eq!(
            parse_multiboot2_info(&malformed, 0x1000),
            Err(ParseError::MemoryMapMalformed)
        );
    }

    #[test]
    fn end_tag_must_be_the_last_aligned_tag() {
        let mut bytes = valid_info();
        bytes.extend_from_slice(&[0; 8]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::EndTagMalformed)
        );
    }

    #[test]
    fn mmap_capacity_is_fatal_instead_of_truncating() {
        let mut payload = vec![0; 8 + 24 * (MAX_REGIONS + 1)];
        payload[..4].copy_from_slice(&24u32.to_le_bytes());
        for index in 0..=MAX_REGIONS {
            let offset = 8 + index * 24;
            payload[offset..offset + 8].copy_from_slice(&((index * 0x1000) as u64).to_le_bytes());
            payload[offset + 8..offset + 16].copy_from_slice(&0x1000u64.to_le_bytes());
            payload[offset + 16..offset + 20].copy_from_slice(&1u32.to_le_bytes());
        }
        let mut bytes = vec![0; 8];
        push_tag(&mut bytes, 6, &payload);
        push_tag(&mut bytes, 0, &[]);
        set_total_size(&mut bytes);
        assert_eq!(
            parse_multiboot2_info(&bytes, 0x1000),
            Err(ParseError::MemoryMapCapacity)
        );
    }

    #[test]
    fn rsdp_copy_is_fixed_size_and_owned() {
        let rsdp = AcpiRsdp {
            bytes: valid_rsdp(7),
            length: 36,
        };
        let mut bytes = *rsdp.bytes();
        bytes[9] = 9;
        assert_eq!(bytes[9], 9);
        assert_eq!(rsdp.bytes()[9], 7);
    }
}
