// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_opregion.c:
// intel_opregion_setup, opregion_header/opregion_asle, VBT discovery only.
// Copyright 2008 Intel Corporation <hong.liu@intel.com>
// Copyright 2008 Red Hat <mjg@redhat.com>. MIT permission text: ../LICENSE-MIT.
// All SWSCI/ASLE/ACPI writes, DMI quirks, panel lookup and lifecycle omitted.
use crate::{Error, bios::Vbt, bytes, le32, le64};

pub const ASLS: u16 = 0xfc;
pub const SIZE: usize = 8192;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalVbt {
    pub physical: u64,
    pub size: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct OpRegion<'a> {
    data: &'a [u8],
    physical: u64,
    pub major: u8,
    pub minor: u8,
    pub revision: u8,
    pub mailboxes: u32,
}
impl<'a> OpRegion<'a> {
    pub fn parse(data: &'a [u8], physical: u64) -> Result<Self, Error> {
        let data = bytes(data, 0, SIZE)?;
        if physical == 0
            || physical == u64::MAX
            || &data[..16] != b"IntelGraphicsMem"
            || le32(data, 16)? != 8
        {
            return Err(Error::InvalidHeader);
        }
        Ok(Self {
            data,
            physical,
            major: data[23],
            minor: data[22],
            revision: data[21],
            mailboxes: le32(data, 88)?,
        })
    }
    /// Caller maps the returned extent read-only; never dereference a BIOS
    /// physical pointer inside this pure parser. Linux >=2.1 uses unsigned
    /// offsets, not absolute addresses. Reject overlap/overflow and oversize
    /// before even requesting a mapping (stricter than Linux's warning).
    pub fn external_vbt(&self) -> Result<Option<ExternalVbt>, Error> {
        if self.major < 2 || self.mailboxes & 4 == 0 {
            return Ok(None);
        }
        let rvda = le64(self.data, 0x3ba)?;
        let size = usize::try_from(le32(self.data, 0x3c2)?).map_err(|_| Error::Truncated)?;
        if rvda == 0 || size == 0 {
            return Ok(None);
        }
        if !(48..=65536).contains(&size) {
            return Err(Error::InvalidHeader);
        }
        let physical = if self.major > 2 || self.minor >= 1 {
            if rvda < SIZE as u64 {
                return Err(Error::InvalidHeader);
            }
            self.physical
                .checked_add(rvda)
                .ok_or(Error::InvalidHeader)?
        } else {
            rvda
        };
        physical
            .checked_add(size as u64)
            .ok_or(Error::InvalidHeader)?;
        Ok(Some(ExternalVbt { physical, size }))
    }
    pub fn mailbox_vbt(&self) -> Result<Vbt<'a>, Error> {
        let end = if self.mailboxes & 16 != 0 {
            0x1c00
        } else {
            SIZE
        };
        Vbt::parse(&self.data[0x400..end])
    }
    /// Linux tries RVDA before mailbox 4 and falls back when the RVDA VBT is
    /// invalid. An external slice must have the exact requested mapping length.
    /// Returning a mailbox VBT does not hide why external_vbt mapping failed;
    /// callers should report that error separately.
    pub fn vbt(&self, external: Option<&'a [u8]>) -> Result<Vbt<'a>, Error> {
        if let (Ok(Some(region)), Some(data)) = (self.external_vbt(), external)
            && data.len() == region.size
            && let Ok(vbt) = Vbt::parse(data)
        {
            return Ok(vbt);
        }
        self.mailbox_vbt()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn region(major: u8, minor: u8, rvda: u64, size: u32) -> [u8; SIZE] {
        let mut data = [0; SIZE];
        data[..16].copy_from_slice(b"IntelGraphicsMem");
        data[16..20].copy_from_slice(&8u32.to_le_bytes());
        data[23] = major;
        data[22] = minor;
        data[88] = 4;
        data[0x3ba..0x3c2].copy_from_slice(&rvda.to_le_bytes());
        data[0x3c2..0x3c6].copy_from_slice(&size.to_le_bytes());
        data
    }
    #[test]
    fn external_20_absolute_21_and_30_relative() {
        for (major, minor, expected) in [(2, 0, 8192), (2, 1, 0x102000), (3, 0, 0x102000)] {
            let data = region(major, minor, 8192, 8704);
            let op = OpRegion::parse(&data, 0x100000).unwrap();
            assert_eq!(
                op.external_vbt().unwrap(),
                Some(ExternalVbt {
                    physical: expected,
                    size: 8704
                })
            );
        }
    }
    #[test]
    fn bad_ranges_and_missing_asle_are_not_mapping_requests() {
        for (addr, size) in [(4, 8704), (8192, 0xffffffff), (u64::MAX, 8704), (8192, 47)] {
            let data = region(2, 1, addr, size);
            assert!(
                OpRegion::parse(&data, 0x100000)
                    .unwrap()
                    .external_vbt()
                    .is_err()
            );
        }
        let mut data = region(2, 1, 8192, 8704);
        data[88] = 0;
        assert_eq!(
            OpRegion::parse(&data, 0x100000)
                .unwrap()
                .external_vbt()
                .unwrap(),
            None
        );
        assert!(OpRegion::parse(&data[..100], 0x100000).is_err());
        assert!(OpRegion::parse(&data, 0).is_err());
    }
}
