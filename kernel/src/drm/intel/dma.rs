// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Original, read-only N305 DMA admission. ACPI DMAR/DRHD byte-layout and
//! VT-d GSTS.TES facts are not a translation of GPL IOMMU code. No remapping
//! unit is enabled, disabled or reconfigured. Unknown/active translation refuses
//! physical-address DMA, rather than assuming CPU physical == device address.
use alloc::vec::Vec;

use intel_display::Error;

fn u16_at(b: &[u8], n: usize) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(
        b.get(n..n + 2).ok_or(Error::Truncated)?.try_into().unwrap(),
    ))
}
fn u32_at(b: &[u8], n: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(
        b.get(n..n + 4).ok_or(Error::Truncated)?.try_into().unwrap(),
    ))
}
fn u64_at(b: &[u8], n: usize) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(
        b.get(n..n + 8).ok_or(Error::Truncated)?.try_into().unwrap(),
    ))
}
fn checksum(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}
#[derive(Debug, PartialEq)]
struct Unit {
    base: u64,
    all: bool,
    gpu: bool,
}
/// Only the on-package segment0 bus0 device2 function0 endpoint is admitted.
fn units(table: &[u8]) -> Result<Vec<Unit>, Error> {
    if table.len() < 48
        || &table[..4] != b"DMAR"
        || u32_at(table, 4)? as usize != table.len()
        || !checksum(table)
    {
        return Err(Error::InvalidHeader);
    }
    let mut units = Vec::new();
    let mut n = 48;
    while n < table.len() {
        let kind = u16_at(table, n)?;
        let len = usize::from(u16_at(table, n + 2)?);
        if len < 4 || kind >= 7 {
            return Err(Error::InvalidBlock);
        }
        let b = table
            .get(n..n.checked_add(len).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)?;
        if kind == 0 {
            if len < 16 || b[4] & !1 != 0 {
                return Err(Error::InvalidBlock);
            }
            let segment = u16_at(b, 6)?;
            let base = u64_at(b, 8)?;
            if base == 0 || !base.is_multiple_of(4096) {
                return Err(Error::InvalidBlock);
            }
            let mut gpu = false;
            let mut scope = 16;
            while scope < len {
                let size = usize::from(*b.get(scope + 1).ok_or(Error::Truncated)?);
                if size < 6 || b[scope] > 5 || b[scope] == 0 {
                    return Err(Error::InvalidBlock);
                }
                let descriptor = b
                    .get(scope..scope.checked_add(size).ok_or(Error::Truncated)?)
                    .ok_or(Error::Truncated)?;
                if matches!(descriptor[0], 1 | 2) {
                    if size < 8 || !size.is_multiple_of(2) {
                        return Err(Error::InvalidBlock);
                    }
                    // Integrated GPU has no downstream bridge path. A claimed
                    // GPU path with extra hops is malformed, not permission.
                    if segment == 0
                        && descriptor[5] == 0
                        && descriptor[6] == 2
                        && descriptor[7] == 0
                    {
                        if descriptor[0] != 1 || size != 8 {
                            return Err(Error::Refused);
                        }
                        gpu = true;
                    }
                }
                scope += size;
            }
            if segment == 0 {
                units.try_reserve(1).map_err(|_| Error::Refused)?;
                units.push(Unit {
                    base,
                    all: b[4] & 1 != 0,
                    gpu,
                });
            }
        }
        n += len;
    }
    Ok(units)
}
fn direct(table: &[u8], mut gsts: impl FnMut(u64) -> Result<u32, Error>) -> Result<(), Error> {
    let units = units(table)?;
    let dedicated = units.iter().any(|u| u.gpu);
    for unit in units.iter().filter(|u| u.gpu || (!dedicated && u.all)) {
        let value = gsts(unit.base)?;
        if value == u32::MAX || value & (1 << 31) != 0 {
            return Err(Error::Refused);
        }
    }
    Ok(())
}

#[cfg(target_os = "none")]
pub(super) fn firmware_bytes(physical: u64, len: usize) -> Result<Vec<u8>, Error> {
    // Observed firmware pointers only, bounded before dereference, no alias of
    // usable RAM (including kernel/modules). Boot API doesn't retain all NVS.
    use axhal::mem::{PhysAddr, phys_ram_ranges};
    let end = physical.checked_add(len as u64).ok_or(Error::Refused)?;
    if physical < 0x1000
        || end > 1 << 52
        || len > 1 << 20
        || phys_ram_ranges().iter().any(|&(base, size)| {
            physical < (base as u64).saturating_add(size as u64) && end > base as u64
        })
    {
        return Err(Error::Refused);
    }
    let mut data = Vec::new();
    data.try_reserve_exact(len).map_err(|_| Error::Refused)?;
    data.resize(len, 0);
    let physical = usize::try_from(physical).map_err(|_| Error::Refused)?;
    let mapping = axmm::iomap(PhysAddr::from_usize(physical), len).map_err(|_| Error::Refused)?;
    for (i, b) in data.iter_mut().enumerate() {
        // SAFETY: checked extent at observed firmware pointer; live UC mapping.
        *b = unsafe { mapping.as_ptr().add(i).read_volatile() };
    }
    Ok(data)
}
#[cfg(target_os = "none")]
fn table(physical: u64) -> Result<Vec<u8>, Error> {
    let header = firmware_bytes(physical, 36)?;
    let length = u32_at(&header, 4)? as usize;
    if !(36..=1 << 20).contains(&length) {
        return Err(Error::InvalidHeader);
    }
    let data = firmware_bytes(physical, length)?;
    if !checksum(&data) {
        return Err(Error::InvalidHeader);
    }
    Ok(data)
}
#[cfg(target_os = "none")]
pub(super) fn require_direct(bdf: super::pci::Bdf) -> Result<(), Error> {
    if (bdf.bus, bdf.device, bdf.function) != (0, 2, 0) {
        return Err(Error::Refused);
    }
    let rsdp = axhal::kexec::boot_rsdp().ok_or(Error::Refused)?;
    if &rsdp[..8] != b"RSD PTR " || !checksum(&rsdp[..20]) {
        return Err(Error::InvalidHeader);
    }
    let (address, width, signature) = if rsdp[15] >= 2 {
        if u32_at(rsdp, 20)? != 36 || !checksum(rsdp) {
            return Err(Error::InvalidHeader);
        }
        (u64_at(rsdp, 24)?, 8, b"XSDT")
    } else {
        (u64::from(u32_at(rsdp, 16)?), 4, b"RSDT")
    };
    let root = table(address)?;
    if &root[..4] != signature || !(root.len() - 36).is_multiple_of(width) {
        return Err(Error::InvalidHeader);
    }
    let mut dmar = None;
    for entry in root[36..].chunks_exact(width) {
        let address = if width == 8 {
            u64_at(entry, 0)?
        } else {
            u64::from(u32_at(entry, 0)?)
        };
        let header = firmware_bytes(address, 36)?;
        if &header[..4] == b"DMAR" {
            if dmar.is_some() {
                return Err(Error::Refused);
            }
            dmar = Some(table(address)?);
        }
    }
    // A complete valid root with no DMAR has no advertised Intel remapper.
    if let Some(dmar) = dmar {
        direct(&dmar, |base| {
            let status = firmware_bytes(base.checked_add(0x1c).ok_or(Error::Refused)?, 4)?;
            u32_at(&status, 0)
        })?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn dmar(records: &[(bool, bool, u16, u64)]) -> Vec<u8> {
        let mut b = alloc::vec![0;48];
        b[..4].copy_from_slice(b"DMAR");
        for &(all, gpu, segment, base) in records {
            b.extend_from_slice(&[0, 0, if gpu { 24 } else { 16 }, 0, u8::from(all), 0]);
            b.extend_from_slice(&segment.to_le_bytes());
            b.extend_from_slice(&base.to_le_bytes());
            if gpu {
                b.extend_from_slice(&[1, 8, 0, 0, 0, 0, 2, 0]);
            }
        }
        let len = b.len() as u32;
        b[4..8].copy_from_slice(&len.to_le_bytes());
        fix(&mut b);
        b
    }
    fn fix(b: &mut [u8]) {
        b[9] = 0;
        b[9] = 0u8.wrapping_sub(b.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    }
    #[test]
    fn active_graphics_translation_never_authorizes_physical_dma() {
        let b = dmar(&[(false, true, 0, 0xfed90000)]);
        assert_eq!(direct(&b, |_| Ok(1 << 31)), Err(Error::Refused));
        assert_eq!(direct(&b, |_| Ok(u32::MAX)), Err(Error::Refused));
        assert!(direct(&b, |_| Ok(1 << 30)).is_ok());
    }
    #[test]
    fn dedicated_graphics_unit_takes_precedence_without_touching_other_dma_domains() {
        let b = dmar(&[(true, false, 0, 0xfed91000), (false, true, 0, 0xfed90000)]);
        let mut reads = Vec::new();
        direct(&b, |r| {
            reads.push(r);
            Ok(0)
        })
        .unwrap();
        assert_eq!(reads, alloc::vec![0xfed90000]);
        let b = dmar(&[(true, false, 0, 0xfed91000)]);
        assert_eq!(direct(&b, |_| Ok(1 << 31)), Err(Error::Refused));
    }
    #[test]
    fn malformed_or_truncated_records_are_not_absent_remappers() {
        let b = dmar(&[(false, true, 0, 0xfed90000)]);
        for i in 0..b.len() {
            assert!(units(&b[..i]).is_err());
        }
        for (index, value) in [(48 + 2, 0), (48 + 8, 1), (48 + 16 + 1, 0)] {
            let mut bad = b.clone();
            bad[index] = value;
            fix(&mut bad);
            assert!(units(&bad).is_err());
        }
    }
}
