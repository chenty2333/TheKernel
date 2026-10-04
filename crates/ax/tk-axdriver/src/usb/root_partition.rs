//! Bounded primary GPT reader for an explicit USB-root boot. Original code;
//! N305 USB-root boot has not been validated on hardware.
//! GPT layout: UEFI 2.11 section 5.3. We never inspect an internal disk here.
use axdriver_base::{DevError, DevResult};

const ROOT_X86_64: [u8; 16] = [
    0xe3, 0xbc, 0x68, 0x4f, 0xcd, 0xe8, 0xb1, 0x4d, 0x96, 0xe7, 0xfb, 0xca, 0xf9, 0x84, 0xb7, 0x09,
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Header {
    pub table: u64,
    pub count: usize,
    pub crc: u32,
    first: u64,
    last: u64,
}
fn u32_at(b: &[u8], at: usize) -> DevResult<u32> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4)
            .ok_or(DevError::InvalidParam)?
            .try_into()
            .map_err(|_| DevError::InvalidParam)?,
    ))
}
fn u64_at(b: &[u8], at: usize) -> DevResult<u64> {
    Ok(u64::from_le_bytes(
        b.get(at..at + 8)
            .ok_or(DevError::InvalidParam)?
            .try_into()
            .map_err(|_| DevError::InvalidParam)?,
    ))
}
fn crc(b: &[u8]) -> u32 {
    let mut sum = !0u32;
    for &byte in b {
        sum ^= u32::from(byte);
        for _ in 0..8 {
            sum = (sum >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(sum & 1));
        }
    }
    !sum
}
pub(super) fn header(b: &[u8; 512], blocks: u64) -> DevResult<Header> {
    if &b[..8] != b"EFI PART" || u32_at(b, 8)? != 0x10000 {
        return Err(DevError::InvalidParam);
    }
    let size = u32_at(b, 12)? as usize;
    if !(92..=512).contains(&size) || u32_at(b, 20)? != 0 {
        return Err(DevError::InvalidParam);
    }
    let mut copy = *b;
    copy[16..20].fill(0);
    if crc(&copy[..size]) != u32_at(b, 16)? || u64_at(b, 24)? != 1 {
        return Err(DevError::InvalidParam);
    }
    let other = u64_at(b, 32)?;
    let h = Header {
        first: u64_at(b, 40)?,
        last: u64_at(b, 48)?,
        table: u64_at(b, 72)?,
        count: u32_at(b, 80)? as usize,
        crc: u32_at(b, 88)?,
    };
    let sectors = (h.count * 128).div_ceil(512) as u64;
    if h.count == 0
        || h.count > 128
        || u32_at(b, 84)? != 128
        || h.first < 34
        || h.first > h.last
        || h.last >= blocks
        || other <= h.last
        || other >= blocks
        || h.table < 2
        || h.table.checked_add(sectors).is_none_or(|end| end > h.first)
    {
        return Err(DevError::InvalidParam);
    }
    Ok(h)
}
pub(super) fn partition(h: Header, entries: &[u8]) -> DevResult<(u64, u64)> {
    if entries.len() != h.count * 128 || crc(entries) != h.crc {
        return Err(DevError::InvalidParam);
    }
    let mut root = None;
    for entry in entries.as_chunks::<128>().0 {
        if entry[..16] == [0; 16] {
            continue;
        }
        let first = u64_at(entry, 32)?;
        let last = u64_at(entry, 40)?;
        if first < h.first || first > last || last > h.last {
            return Err(DevError::InvalidParam);
        }
        if entry[..16] == ROOT_X86_64 {
            if root.is_some() {
                return Err(DevError::InvalidParam);
            }
            root = Some((first, last));
        }
    }
    let (start, end) = root.ok_or(DevError::Unsupported)?;
    for entry in entries.as_chunks::<128>().0 {
        if entry[..16] == [0; 16] || entry[..16] == ROOT_X86_64 {
            continue;
        }
        if u64_at(entry, 32)? <= end && u64_at(entry, 40)? >= start {
            return Err(DevError::InvalidParam);
        }
    }
    Ok((start, end - start + 1))
}
/// Translate a partition-relative transfer without permitting overflow or escape.
pub(super) fn physical(start: u64, blocks: u64, at: u64, count: u64) -> DevResult<u64> {
    if at.checked_add(count).is_none_or(|end| end > blocks) {
        return Err(DevError::InvalidParam);
    }
    start.checked_add(at).ok_or(DevError::InvalidParam)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn entries() -> [u8; 256] {
        let mut b = [0; 256];
        b[..16].copy_from_slice(&ROOT_X86_64);
        b[32..40].copy_from_slice(&2048u64.to_le_bytes());
        b[40..48].copy_from_slice(&4095u64.to_le_bytes());
        b
    }
    #[test]
    fn validates_crc_unique_partition_and_ranges() {
        let mut b = entries();
        let h = Header {
            table: 2,
            count: 2,
            crc: crc(&b),
            first: 34,
            last: 8190,
        };
        assert_eq!(partition(h, &b).unwrap(), (2048, 2048));
        b[50] ^= 1;
        assert!(partition(h, &b).is_err());
        b = entries();
        b.copy_within(0..128, 128);
        let mut h = h;
        h.crc = crc(&b);
        assert!(partition(h, &b).is_err());
        b[128..144].fill(1);
        h.crc = crc(&b);
        assert!(partition(h, &b).is_err());
        assert_eq!(physical(2048, 2048, 2047, 1).unwrap(), 4095);
        assert!(physical(2048, 2048, 2047, 2).is_err());
        assert!(physical(1, u64::MAX, u64::MAX, 1).is_err());
    }
    #[test]
    fn malformed_headers_and_random_bytes_are_rejected() {
        let mut b = [0u8; 512];
        b[..8].copy_from_slice(b"EFI PART");
        b[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        b[12..16].copy_from_slice(&92u32.to_le_bytes());
        b[24..32].copy_from_slice(&1u64.to_le_bytes());
        b[32..40].copy_from_slice(&8191u64.to_le_bytes());
        b[40..48].copy_from_slice(&34u64.to_le_bytes());
        b[48..56].copy_from_slice(&8190u64.to_le_bytes());
        b[72..80].copy_from_slice(&2u64.to_le_bytes());
        b[80..84].copy_from_slice(&2u32.to_le_bytes());
        b[84..88].copy_from_slice(&128u32.to_le_bytes());
        let sum = crc(&b[..92]);
        b[16..20].copy_from_slice(&sum.to_le_bytes());
        assert!(header(&b, 8192).is_ok());
        b[40] ^= 1;
        assert!(header(&b, 8192).is_err());
        for x in 0..=255 {
            assert!(header(&[x; 512], 8192).is_err());
        }
    }
}
