use alloc::{boxed::Box, vec::Vec};

use axerrno::{AxError, AxResult};

pub(crate) const ENTRIES: usize = 8192;
pub(crate) const BYTES: usize = ENTRIES * 8;

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::AnyBitPattern)]
pub(crate) struct UserDesc {
    pub entry_number: u32,
    pub base_addr: u32,
    pub limit: u32,
    pub flags: u32,
}

impl UserDesc {
    const fn bit(self, n: u32) -> bool {
        self.flags & (1 << n) != 0
    }

    pub(crate) const fn contents(self) -> u32 {
        (self.flags >> 1) & 3
    }

    pub(crate) const fn empty(self) -> bool {
        self.base_addr == 0
            && self.limit == 0
            && self.contents() == 0
            && self.bit(3)
            && !self.bit(0)
            && !self.bit(4)
            && self.bit(5)
            && !self.bit(6)
    }

    pub(crate) const fn old_empty(self) -> bool {
        self.base_addr == 0 && self.limit == 0
    }
}

pub(crate) struct Ldt {
    entries: Box<[u64]>,
}

impl Ldt {
    pub(crate) fn new(n: usize) -> AxResult<Self> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(n)
            .map_err(|_| AxError::NoMemory)?;
        entries.resize(n, 0);
        Ok(Self {
            entries: entries.into_boxed_slice(),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.entries)
    }

    pub(crate) fn copy(&self) -> AxResult<Self> {
        let mut copy = Self::new(self.len())?;
        copy.entries.copy_from_slice(&self.entries);
        Ok(copy)
    }

    pub(crate) fn copy_into(&self, target: &mut Self) {
        target.entries[..self.len()].copy_from_slice(&self.entries)
    }

    pub(crate) fn set(&mut self, index: usize, descriptor: u64) {
        self.entries[index] = descriptor
    }

    pub(crate) fn descriptor(desc: UserDesc, oldmode: bool) -> AxResult<u64> {
        if desc.contents() == 3 && (oldmode || !desc.bit(5)) {
            return Err(AxError::InvalidInput);
        }
        if (oldmode && desc.old_empty()) || desc.empty() {
            return Ok(0);
        }
        // The kernel has no 16-bit compatibility support.
        if !desc.bit(0) {
            return Err(AxError::InvalidInput);
        }

        let base = desc.base_addr as u64;
        let limit = desc.limit as u64;
        let ty = (((desc.bit(3) as u64) ^ 1) << 1) | ((desc.contents() as u64) << 2) | 1;
        Ok((limit & 0xffff)
            | ((base & 0xffff) << 16)
            | (((base >> 16) & 0xff) << 32)
            | (ty << 40)
            | (1 << 44)
            | (3 << 45)
            | ((!desc.bit(5) as u64) << 47)
            | (((limit >> 16) & 0xf) << 48)
            | (((!oldmode && desc.bit(6)) as u64) << 52)
            | (1 << 54)
            | ((desc.bit(4) as u64) << 55)
            | (((base >> 24) & 0xff) << 56))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_NEW: UserDesc = UserDesc {
        entry_number: 0,
        base_addr: 0,
        limit: 0,
        // contents=0, read_exec_only=1, seg_not_present=1.
        flags: (1 << 3) | (1 << 5),
    };

    #[test]
    fn new_and_old_clear_rules_differ() {
        assert_eq!(Ldt::descriptor(EMPTY_NEW, false).unwrap(), 0);
        assert_eq!(Ldt::descriptor(EMPTY_NEW, true).unwrap(), 0);
        let zero = UserDesc::default();
        assert_eq!(Ldt::descriptor(zero, true).unwrap(), 0);
        assert!(Ldt::descriptor(zero, false).is_err());
    }

    #[test]
    fn contents_three_requires_new_absent_segment() {
        let absent = UserDesc {
            flags: (3 << 1) | (1 << 5),
            ..UserDesc::default()
        };
        assert!(Ldt::descriptor(absent, true).is_err());
        assert!(Ldt::descriptor(absent, false).is_err());
        assert!(
            Ldt::descriptor(
                UserDesc {
                    flags: (3 << 1) | (1 << 5) | 1,
                    ..UserDesc::default()
                },
                false,
            )
            .is_ok()
        );
    }

    #[test]
    fn descriptor_encodes_requested_base_limit_and_avl() {
        let descriptor = Ldt::descriptor(
            UserDesc {
                base_addr: 0x1234_5678,
                limit: 0xabcde,
                // seg_32bit, limit_in_pages, useable.
                flags: 1 | (1 << 4) | (1 << 6),
                ..UserDesc::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(descriptor & 0xffff, 0xbcde);
        assert_eq!((descriptor >> 16) & 0xffff, 0x5678);
        assert_eq!((descriptor >> 32) & 0xff, 0x34);
        assert_eq!((descriptor >> 48) & 0xf, 0xa);
        assert_eq!((descriptor >> 52) & 1, 1);
        assert_eq!((descriptor >> 55) & 1, 1);
        assert_eq!((descriptor >> 56) & 0xff, 0x12);
    }
}
