//! Intel VT-d DMAR structures and a fail-closed device DMA mapping contract.
//!
//! Translated from FreeBSD `sys/x86/iommu/intel_dmar.h` and the DMAR discovery
//! and DMA address ownership portions of `intel_drv.c` (BSD-2-Clause; FreeBSD
//! source snapshot 2026-10-08). Copyright (c) 2013-2015 The FreeBSD
//! Foundation; developed by Konstantin Belousov under Foundation sponsorship.
//! License text in `LICENSES/BSD-2-Clause.txt`.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::vec::Vec;

pub mod dmar;
pub mod iova;
pub mod pgtbl;
pub mod utils;
pub mod reg;

const DMAR_HEADER_SIZE: usize = 48;
const DRHD: u16 = 0;
const RMRR: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidTable,
    InvalidStructure,
    OutOfMemory,
    NoDomain,
    MapFailed,
    InvalidRange,
    Timeout,
    Unsupported,
}

#[crate_interface::def_interface]
pub trait PlatformDma {
    fn pci_dma_allowed() -> bool;
    fn map(physical: u64, length: usize) -> Result<u64, Error>;
    fn unmap(device_address: u64, length: usize) -> Result<(), Error>;
}

pub fn platform_pci_dma_allowed() -> bool {
    crate_interface::call_interface!(PlatformDma::pci_dma_allowed)
}

/// Map a physical buffer through the installed platform DMA domain.
pub fn platform_map(physical: u64, length: usize) -> Result<u64, Error> {
    crate_interface::call_interface!(PlatformDma::map, physical, length)
}

/// Retire a platform DMA mapping after the device has stopped accessing it.
pub fn platform_unmap(device_address: u64, length: usize) -> Result<(), Error> {
    crate_interface::call_interface!(PlatformDma::unmap, device_address, length)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unit {
    pub segment: u16,
    pub register_base: u64,
    pub include_all: bool,
    pub scopes: Vec<OwnedScope>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedScope {
    pub scope_type: u8,
    pub enumeration_id: u8,
    pub start_bus: u8,
    pub path: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservedRegion {
    pub segment: u16,
    pub base: u64,
    pub end_inclusive: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmarTable {
    pub host_address_width: u8,
    pub interrupt_remapping: bool,
    pub units: Vec<Unit>,
    pub reserved_regions: Vec<ReservedRegion>,
}

fn le16(b: &[u8], n: usize) -> Result<u16, Error> {
    let v = b.get(n..n + 2).ok_or(Error::InvalidTable)?;
    Ok(u16::from_le_bytes([v[0], v[1]]))
}
fn le32(b: &[u8], n: usize) -> Result<u32, Error> {
    let v = b.get(n..n + 4).ok_or(Error::InvalidTable)?;
    Ok(u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
}
fn le64(b: &[u8], n: usize) -> Result<u64, Error> {
    let v = b.get(n..n + 8).ok_or(Error::InvalidTable)?;
    Ok(u64::from_le_bytes([
        v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7],
    ]))
}

impl DmarTable {
    /// Parse ACPI DMAR's fixed header and remapping-structure list. Unknown
    /// structure types are skipped only after validating their length.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < DMAR_HEADER_SIZE || bytes.get(..4) != Some(b"DMAR") {
            return Err(Error::InvalidTable);
        }
        let table_len = le32(bytes, 4)? as usize;
        if table_len < DMAR_HEADER_SIZE || table_len > bytes.len() {
            return Err(Error::InvalidTable);
        }
        let bytes = &bytes[..table_len];
        let host_address_width = bytes[36].checked_add(1).ok_or(Error::InvalidTable)?;
        if host_address_width == 0 || host_address_width > 64 {
            return Err(Error::InvalidTable);
        }
        let flags = bytes[37];
        let mut table = Self {
            host_address_width,
            interrupt_remapping: flags & 1 != 0,
            units: Vec::new(),
            reserved_regions: Vec::new(),
        };
        let mut at = DMAR_HEADER_SIZE;
        while at < bytes.len() {
            let kind = le16(bytes, at)?;
            let len = usize::from(le16(bytes, at + 2)?);
            if len < 4 || at.checked_add(len).is_none_or(|end| end > bytes.len()) {
                return Err(Error::InvalidStructure);
            }
            let record = &bytes[at..at + len];
            match kind {
                DRHD => {
                    if len < 16 {
                        return Err(Error::InvalidStructure);
                    }
                    let mut unit = Unit {
                        segment: le16(record, 6)?,
                        register_base: le64(record, 8)?,
                        include_all: record[4] & 1 != 0,
                        scopes: Vec::new(),
                    };
                    if unit.register_base == 0 || unit.register_base & 0xfff != 0 {
                        return Err(Error::InvalidStructure);
                    }
                    Self::parse_scopes(&record[16..], &mut unit.scopes)?;
                    table.units.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
                    table.units.push(unit);
                }
                RMRR => {
                    if len < 24 {
                        return Err(Error::InvalidStructure);
                    }
                    let base = le64(record, 8)?;
                    let end = le64(record, 16)?;
                    if base > end {
                        return Err(Error::InvalidStructure);
                    }
                    table
                        .reserved_regions
                        .try_reserve(1)
                        .map_err(|_| Error::OutOfMemory)?;
                    table.reserved_regions.push(ReservedRegion {
                        segment: le16(record, 6)?,
                        base,
                        end_inclusive: end,
                    });
                }
                _ => {}
            }
            at += len;
        }
        if table.units.is_empty() {
            return Err(Error::InvalidStructure);
        }
        Ok(table)
    }

    fn parse_scopes(mut bytes: &[u8], out: &mut Vec<OwnedScope>) -> Result<(), Error> {
        while !bytes.is_empty() {
            if bytes.len() < 6 {
                return Err(Error::InvalidStructure);
            }
            let len = usize::from(bytes[1]);
            if len < 8 || len > bytes.len() || (len - 6) % 2 != 0 {
                return Err(Error::InvalidStructure);
            }
            let path = &bytes[6..len];
            out.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
            let mut copied = Vec::new();
            copied
                .try_reserve_exact(path.len())
                .map_err(|_| Error::OutOfMemory)?;
            copied.extend_from_slice(path);
            out.push(OwnedScope {
                scope_type: bytes[0],
                enumeration_id: bytes[4],
                start_bus: bytes[5],
                path: copied,
            });
            bytes = &bytes[len..];
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciRequester {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// Select the most specific segment-matching DRHD. An include-all unit is
/// fallback only; an explicit endpoint/bridge scope wins over it.
// upstream: intel_drv.c dmar_find_by_scope()
pub fn select_unit(table: &DmarTable, requester: PciRequester) -> Option<&Unit> {
    let fallback = table
        .units
        .iter()
        .find(|unit| unit.segment == requester.segment && unit.include_all);
    table
        .units
        .iter()
        .find(|unit| {
            unit.segment == requester.segment
                && unit.scopes.iter().any(|scope| {
                    scope.scope_type == 1
                        && scope.start_bus == requester.bus
                        && scope.path.len() == 2
                        && scope.path[0] == requester.device
                        && scope.path[1] & 7 == requester.function
                })
        })
        .or(fallback)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    ToDevice,
    FromDevice,
    Bidirectional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mapping {
    pub physical: u64,
    pub device_address: u64,
    pub length: usize,
    pub direction: Direction,
}

pub trait Backend {
    fn map(
        &mut self,
        requester: PciRequester,
        physical: u64,
        length: usize,
        direction: Direction,
    ) -> Result<u64, Error>;
    fn unmap(
        &mut self,
        requester: PciRequester,
        device_address: u64,
        length: usize,
    ) -> Result<(), Error>;
}

/// DMA facade: disabled VT-d uses identity, while enabled VT-d requires a
/// valid requester domain and never falls back to identity after map failure.
pub struct Dma<B> {
    backend: B,
    enabled: bool,
}
impl<B: Backend> Dma<B> {
    pub const fn new(backend: B, enabled: bool) -> Self {
        Self { backend, enabled }
    }
    pub fn map(
        &mut self,
        requester: PciRequester,
        physical: u64,
        length: usize,
        direction: Direction,
    ) -> Result<Mapping, Error> {
        if length == 0 || physical.checked_add(length as u64).is_none() {
            return Err(Error::InvalidRange);
        }
        let device_address = if self.enabled {
            self.backend.map(requester, physical, length, direction)?
        } else {
            physical
        };
        if device_address.checked_add(length as u64).is_none() {
            return Err(Error::InvalidRange);
        }
        Ok(Mapping {
            physical,
            device_address,
            length,
            direction,
        })
    }
    pub fn unmap(&mut self, requester: PciRequester, mapping: Mapping) -> Result<(), Error> {
        if mapping.length == 0 {
            return Err(Error::InvalidRange);
        }
        if self.enabled {
            self.backend
                .unmap(requester, mapping.device_address, mapping.length)?;
        }
        Ok(())
    }
    pub fn into_backend(self) -> B {
        self.backend
    }
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;
    fn dmar(record: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; DMAR_HEADER_SIZE];
        b[..4].copy_from_slice(b"DMAR");
        b[36] = 47;
        b[37] = 1;
        b.extend_from_slice(record);
        let n = b.len() as u32;
        b[4..8].copy_from_slice(&n.to_le_bytes());
        b
    }
    fn drhd(base: u64) -> Vec<u8> {
        let mut r = vec![0u8; 16];
        r[0..2].copy_from_slice(&DRHD.to_le_bytes());
        r[2..4].copy_from_slice(&16u16.to_le_bytes());
        r[8..16].copy_from_slice(&base.to_le_bytes());
        r
    }
    fn endpoint_drhd(base: u64, bus: u8, device: u8, function: u8) -> Vec<u8> {
        let mut record = drhd(base);
        record[2..4].copy_from_slice(&24u16.to_le_bytes());
        // DMAR device scope: type/length/reserved/enumeration/start-bus,
        // then one PCI path element (device,function).
        record.extend_from_slice(&[1, 8, 0, 0, 0, bus, device, function]);
        record
    }
    #[test]
    fn parses_dmar_remapping_unit() {
        let t = DmarTable::parse(&dmar(&drhd(0xfed9_0000))).unwrap();
        assert_eq!(t.host_address_width, 48);
        assert!(t.interrupt_remapping);
        assert_eq!(t.units[0].register_base, 0xfed9_0000);
    }
    #[test]
    fn malformed_structure_is_rejected() {
        let mut r = drhd(0xfed9_0000);
        r[2..4].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(DmarTable::parse(&dmar(&r)), Err(Error::InvalidStructure));
    }
    #[test]
    fn parses_endpoint_scope_and_selects_only_its_segment_requester() {
        let table = DmarTable::parse(&dmar(&endpoint_drhd(0xfed9_0000, 0, 2, 0))).unwrap();
        let requester = PciRequester {
            segment: 0,
            bus: 0,
            device: 2,
            function: 0,
        };
        assert!(select_unit(&table, requester).is_some());
        assert!(
            select_unit(
                &table,
                PciRequester {
                    device: 3,
                    ..requester
                }
            )
            .is_none()
        );
        assert!(
            select_unit(
                &table,
                PciRequester {
                    segment: 1,
                    ..requester
                }
            )
            .is_none()
        );
    }
    struct Fake;
    impl Backend for Fake {
        fn map(&mut self, _: PciRequester, p: u64, _: usize, _: Direction) -> Result<u64, Error> {
            Ok(p + 0x1000)
        }
        fn unmap(&mut self, _: PciRequester, _: u64, _: usize) -> Result<(), Error> {
            Ok(())
        }
    }
    #[test]
    fn dma_mapping_is_identity_only_when_disabled() {
        let id = PciRequester {
            segment: 0,
            bus: 0,
            device: 1,
            function: 0,
        };
        let mut off = Dma::new(Fake, false);
        assert_eq!(
            off.map(id, 0x2000, 4096, Direction::Bidirectional)
                .unwrap()
                .device_address,
            0x2000
        );
        let mut on = Dma::new(Fake, true);
        assert_eq!(
            on.map(id, 0x2000, 4096, Direction::ToDevice)
                .unwrap()
                .device_address,
            0x3000
        );
    }
}
