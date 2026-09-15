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
        // `write_ldt()` gates 16-bit segments on `allow_16bit_segments()`,
        // which is `IS_ENABLED(CONFIG_X86_16BIT)` on a native (non-Xen-PV)
        // x86_64 kernel. That configuration is y here, so a descriptor with
        // `seg_32bit` clear is admitted and stored rather than rejected.
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
            // DB mirrors `seg_32bit`; the long-mode bit stays clear, matching
            // `fill_ldt()`'s deliberate `desc->l = 0`.
            | ((desc.bit(0) as u64) << 54)
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
        // The old mode clears any descriptor with a zero base and limit, so an
        // all-zero request removes the entry.
        let zero = UserDesc::default();
        assert_eq!(Ldt::descriptor(zero, true).unwrap(), 0);
        // The new mode only clears on `LDT_empty()`, which also requires
        // read_exec_only and seg_not_present; an all-zero request is therefore
        // stored as a present 16-bit read/write data segment. Linux 7.2.3
        // produces 0x0000_f300_0000_0000 for it.
        assert_eq!(Ldt::descriptor(zero, false).unwrap(), 0x0000_f300_0000_0000);
    }

    #[test]
    fn contents_three_needs_an_absent_segment_in_the_new_mode() {
        let absent = UserDesc {
            flags: (3 << 1) | (1 << 5),
            ..UserDesc::default()
        };
        // `write_ldt()` rejects contents == 3 outright in the old mode and
        // requires the segment to be marked not-present in the new mode.
        assert!(Ldt::descriptor(absent, true).is_err());
        let stored = Ldt::descriptor(absent, false).unwrap();
        assert_eq!((stored >> 47) & 1, 0, "seg_not_present stores P clear");
        let present = UserDesc {
            flags: 3 << 1,
            ..UserDesc::default()
        };
        assert!(Ldt::descriptor(present, false).is_err());
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
        // fill_ldt() stores `seg_32bit` in DB and never sets the L bit.
        assert_eq!((descriptor >> 54) & 1, 1);
        assert_eq!((descriptor >> 53) & 1, 0);
    }

    #[test]
    fn sixteen_bit_segments_are_stored_with_db_clear() {
        // CONFIG_X86_16BIT=y: a descriptor with `seg_32bit` clear is stored as
        // a 16-bit segment (DB clear) instead of being rejected.
        let sixteen_bit = UserDesc {
            base_addr: 0x1000,
            limit: 0xffff,
            // contents = 0 (data), read_exec_only = 1, present.
            flags: 1 << 3,
            ..UserDesc::default()
        };
        let descriptor = Ldt::descriptor(sixteen_bit, false).unwrap();
        assert_eq!((descriptor >> 54) & 1, 0, "DB mirrors seg_32bit");
        assert_eq!((descriptor >> 53) & 1, 0, "fill_ldt() never sets L");
        assert_eq!((descriptor >> 40) & 0xf, 1);
        // The old mode only differs by clearing AVL.
        assert_eq!((Ldt::descriptor(sixteen_bit, true).unwrap() >> 54) & 1, 0);

        // A 32-bit request keeps DB set, so the two widths stay distinguishable.
        let thirty_two_bit = UserDesc {
            flags: (1 << 3) | 1,
            ..sixteen_bit
        };
        assert_eq!(
            (Ldt::descriptor(thirty_two_bit, false).unwrap() >> 54) & 1,
            1
        );
    }
}
