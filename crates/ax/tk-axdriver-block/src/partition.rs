//! Bounded, read-only GPT discovery. No filesystem or partition-table writes.
extern crate alloc;
use alloc::{vec, vec::Vec};

use crate::{BlockGeometry, DevError, DevResult};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Partition {
    pub number: usize,
    pub start: u64,
    pub blocks: u64,
}
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
/// Discover GPT primary entries only after checking header/array CRCs and every
/// range. Corrupt/overlapping tables fail closed, leaving the whole disk usable.
pub fn gpt(
    geometry: BlockGeometry,
    mut read: impl FnMut(u64, &mut [u8]) -> DevResult,
) -> DevResult<Vec<Partition>> {
    let sector = geometry.block_size;
    if !(512..=4096).contains(&sector) || !sector.is_power_of_two() || geometry.blocks < 6 {
        return Err(DevError::InvalidParam);
    }
    let mut header = vec![0; sector];
    read(0, &mut header)?;
    if header[510..512] != [0x55, 0xaa] || !(0..4).any(|index| header[446 + index * 16 + 4] == 0xee)
    {
        return Ok(Vec::new());
    }
    read(1, &mut header)?;
    if &header[..8] != b"EFI PART" || u32_at(&header, 8) != 0x10000 {
        return Err(DevError::InvalidParam);
    }
    let size = u32_at(&header, 12) as usize;
    if !(92..=sector).contains(&size) {
        return Err(DevError::InvalidParam);
    }
    let expected = u32_at(&header, 16);
    header[16..20].fill(0);
    if crc32(&header[..size]) != expected || u32_at(&header, 20) != 0 || u64_at(&header, 24) != 1 {
        return Err(DevError::InvalidParam);
    }
    let backup = u64_at(&header, 32);
    let first = u64_at(&header, 40);
    let last = u64_at(&header, 48);
    let table = u64_at(&header, 72);
    let count = u32_at(&header, 80) as usize;
    let stride = u32_at(&header, 84) as usize;
    if !(1..=4096).contains(&count) || !(128..=4096).contains(&stride) || !stride.is_power_of_two()
    {
        return Err(DevError::InvalidParam);
    }
    let bytes = count
        .checked_mul(stride)
        .filter(|bytes| *bytes <= 1024 * 1024)
        .ok_or(DevError::InvalidParam)?;
    let sectors = bytes.div_ceil(sector) as u64;
    if table < 2
        || table.checked_add(sectors).is_none_or(|end| end > first)
        || first > last
        || last >= backup
        || backup >= geometry.blocks
    {
        return Err(DevError::InvalidParam);
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(sectors as usize * sector)
        .map_err(|_| DevError::NoMemory)?;
    entries.resize(sectors as usize * sector, 0);
    read(table, &mut entries)?;
    if crc32(&entries[..bytes]) != u32_at(&header, 88) {
        return Err(DevError::InvalidParam);
    }
    let mut partitions: Vec<Partition> = Vec::new();
    for (index, entry) in entries[..bytes].chunks_exact(stride).enumerate() {
        if entry[..16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let start = u64_at(entry, 32);
        let end = u64_at(entry, 40);
        if entry[16..32].iter().all(|byte| *byte == 0)
            || start < first
            || end > last
            || start > end
            || partitions
                .iter()
                .any(|p| start < p.start + p.blocks && p.start <= end)
        {
            return Err(DevError::InvalidParam);
        }
        partitions.try_reserve(1).map_err(|_| DevError::NoMemory)?;
        partitions.push(Partition {
            number: index + 1,
            start,
            blocks: end - start + 1,
        });
    }
    Ok(partitions)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn image() -> Vec<u8> {
        let mut image = vec![0; 128 * 512];
        image[510..512].copy_from_slice(&[0x55, 0xaa]);
        image[450] = 0xee;
        let entries = &mut image[1024..1536];
        entries[0] = 1;
        entries[16] = 2;
        entries[32..40].copy_from_slice(&8u64.to_le_bytes());
        entries[40..48].copy_from_slice(&99u64.to_le_bytes());
        let array_crc = crc32(entries);
        let header = &mut image[512..1024];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        for (offset, value) in [(24, 1u64), (32, 127), (40, 8), (48, 120), (72, 2)] {
            header[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        header[80..84].copy_from_slice(&4u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        image
    }
    fn scan(image: &[u8]) -> DevResult<Vec<Partition>> {
        gpt(
            BlockGeometry {
                blocks: 128,
                block_size: 512,
            },
            |block, out| {
                let start = block as usize * 512;
                out.copy_from_slice(&image[start..start + out.len()]);
                Ok(())
            },
        )
    }
    #[test]
    fn geometry_and_both_crcs_are_required() {
        let mut bytes = image();
        assert_eq!(
            scan(&bytes).unwrap(),
            vec![Partition {
                number: 1,
                start: 8,
                blocks: 92
            }]
        );
        bytes[600] ^= 1;
        assert!(scan(&bytes).is_err());
        let mut bytes = image();
        bytes[1100] ^= 1;
        assert!(scan(&bytes).is_err());
    }
    #[test]
    fn overlapping_or_out_of_bounds_ranges_fail_closed() {
        let mut bytes = image();
        bytes[1024 + 128..1024 + 256].copy_from_slice(&image()[1024..1024 + 128]);
        let crc = crc32(&bytes[1024..1536]);
        bytes[512 + 88..512 + 92].copy_from_slice(&crc.to_le_bytes());
        bytes[528..532].fill(0);
        let crc = crc32(&bytes[512..604]);
        bytes[528..532].copy_from_slice(&crc.to_le_bytes());
        assert!(scan(&bytes).is_err());
    }
    #[test]
    fn non_gpt_disk_is_not_an_error() {
        assert!(scan(&vec![0; 128 * 512]).unwrap().is_empty());
    }
}
