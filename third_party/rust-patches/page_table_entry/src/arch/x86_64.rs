//! x86 page table entries on 64-bit paging.

use core::fmt;

use memory_addr::PhysAddr;
pub use x86_64::structures::paging::page_table::PageTableFlags as PTF;

use crate::{GenericPTE, MappingFlags};

/// An x86 protection key stored in bits 59 through 62 of a leaf PTE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct Pkey(u8);

impl Pkey {
    /// The default protection key.
    pub const DEFAULT: Self = Self(0);
    /// The largest x86 protection-key value.
    pub const MAX: u8 = 15;

    /// Creates a protection key from its architectural four-bit value.
    #[inline]
    pub const fn new(value: u8) -> Option<Self> {
        if value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Returns the architectural protection-key value.
    #[inline]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Page-table entries that carry an x86 protection key.
pub trait PkeyPTE: GenericPTE {
    /// Returns the protection key encoded in this entry.
    fn pkey(&self) -> Pkey;
    /// Replaces the protection key while retaining all other entry bits.
    fn set_pkey(&mut self, pkey: Pkey);
}

impl From<PTF> for MappingFlags {
    fn from(f: PTF) -> Self {
        if !f.contains(PTF::PRESENT) {
            return Self::empty();
        }
        let mut ret = Self::READ;
        if f.contains(PTF::WRITABLE) {
            ret |= Self::WRITE;
        } else if f.contains(PTF::DIRTY) {
            // CET classifies a W=0,D=1 user leaf as a shadow-stack page.
            ret |= Self::SHADOW_STACK;
        }
        if !f.contains(PTF::NO_EXECUTE) {
            ret |= Self::EXECUTE;
        }
        if f.contains(PTF::USER_ACCESSIBLE) {
            ret |= Self::USER;
        }
        if f.contains(PTF::NO_CACHE) {
            ret |= Self::UNCACHED;
        }
        ret
    }
}

impl From<MappingFlags> for PTF {
    fn from(f: MappingFlags) -> Self {
        if f.is_empty() {
            return Self::empty();
        }
        let mut ret = Self::PRESENT;
        if f.contains(MappingFlags::WRITE) {
            ret |= Self::WRITABLE;
        }
        if f.contains(MappingFlags::SHADOW_STACK) {
            debug_assert!(!f.contains(MappingFlags::WRITE));
            ret |= PTF::DIRTY;
        }
        if !f.contains(MappingFlags::EXECUTE) {
            ret |= Self::NO_EXECUTE;
        }
        if f.contains(MappingFlags::USER) {
            ret |= Self::USER_ACCESSIBLE;
        }
        if f.contains(MappingFlags::DEVICE) || f.contains(MappingFlags::UNCACHED) {
            ret |= Self::NO_CACHE | Self::WRITE_THROUGH;
        }
        ret
    }
}

/// An x86_64 page table entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct X64PTE(u64);

impl X64PTE {
    // bits 12..52
    const PHYS_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
    // bits 59..62
    const PKEY_MASK: u64 = 0x7800_0000_0000_0000;
    const PKEY_SHIFT: u32 = 59;

    /// Creates an empty descriptor with all bits set to zero.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns the protection key encoded in this entry.
    #[inline]
    pub const fn pkey(&self) -> Pkey {
        Pkey(((self.0 & Self::PKEY_MASK) >> Self::PKEY_SHIFT) as u8)
    }

    /// Replaces the protection key while retaining all other entry bits.
    #[inline]
    pub fn set_pkey(&mut self, pkey: Pkey) {
        self.0 = (self.0 & !Self::PKEY_MASK) | ((pkey.get() as u64) << Self::PKEY_SHIFT);
    }
}

impl PkeyPTE for X64PTE {
    #[inline]
    fn pkey(&self) -> Pkey {
        self.pkey()
    }

    #[inline]
    fn set_pkey(&mut self, pkey: Pkey) {
        self.set_pkey(pkey)
    }
}

impl GenericPTE for X64PTE {
    fn new_page(paddr: PhysAddr, mapping_flags: MappingFlags, is_huge: bool) -> Self {
        let mut flags = PTF::from(mapping_flags);
        if is_huge {
            flags |= PTF::HUGE_PAGE;
        }
        let mut entry = Self(flags.bits() | (paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK));
        entry.set_pkey(Pkey(mapping_flags.pkey()));
        entry
    }

    fn new_table(paddr: PhysAddr) -> Self {
        let flags = PTF::PRESENT | PTF::WRITABLE | PTF::USER_ACCESSIBLE;
        Self(flags.bits() | (paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK))
    }

    fn paddr(&self) -> PhysAddr {
        PhysAddr::from((self.0 & Self::PHYS_ADDR_MASK) as usize)
    }

    fn flags(&self) -> MappingFlags {
        MappingFlags::from(PTF::from_bits_truncate(self.0)).with_pkey(self.pkey().get())
    }

    fn set_paddr(&mut self, paddr: PhysAddr) {
        self.0 = (self.0 & !Self::PHYS_ADDR_MASK) | (paddr.as_usize() as u64 & Self::PHYS_ADDR_MASK)
    }

    fn set_flags(&mut self, flags: MappingFlags, is_huge: bool) {
        let mut flags = PTF::from(flags);
        if is_huge {
            flags |= PTF::HUGE_PAGE;
        }
        self.0 = (self.0 & (Self::PHYS_ADDR_MASK | Self::PKEY_MASK)) | flags.bits()
    }

    fn bits(self) -> usize {
        self.0 as usize
    }

    fn is_unused(&self) -> bool {
        self.0 == 0
    }

    fn is_present(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::PRESENT)
    }

    fn is_huge(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::HUGE_PAGE)
    }

    fn clear(&mut self) {
        self.0 = 0
    }
}

impl fmt::Debug for X64PTE {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut f = f.debug_struct("X64PTE");
        f.field("raw", &self.0)
            .field("paddr", &self.paddr())
            .field("flags", &self.flags())
            .field("pkey", &self.pkey())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protection_key_uses_only_pte_bits_59_through_62() {
        let mut entry = X64PTE::new_page(
            PhysAddr::from(0x1234_5000usize),
            MappingFlags::READ | MappingFlags::WRITE,
            false,
        );
        let key = Pkey::new(11).unwrap();
        entry.set_pkey(key);

        assert_eq!(entry.pkey(), key);
        assert_eq!(
            entry.bits() as u64 & X64PTE::PKEY_MASK,
            11 << X64PTE::PKEY_SHIFT
        );
        assert_eq!(entry.paddr(), PhysAddr::from(0x1234_5000usize));
        assert_eq!(
            entry.flags() - MappingFlags::PKEY_MASK,
            MappingFlags::READ | MappingFlags::WRITE
        );
    }

    #[test]
    fn changing_flags_preserves_protection_key() {
        let mut entry = X64PTE::new_page(
            PhysAddr::from(0x1234_5000usize),
            MappingFlags::READ | MappingFlags::WRITE,
            false,
        );
        let key = Pkey::new(7).unwrap();
        entry.set_pkey(key);
        entry.set_flags(MappingFlags::READ | MappingFlags::USER, false);

        assert_eq!(entry.pkey(), key);
        assert_eq!(
            entry.flags() - MappingFlags::PKEY_MASK,
            MappingFlags::READ | MappingFlags::USER
        );
    }

    #[test]
    fn flags_round_trip_the_protection_key_for_remap_paths() {
        let mut entry = X64PTE::new_page(
            PhysAddr::from(0x1234_5000usize),
            MappingFlags::READ | MappingFlags::WRITE,
            false,
        );
        entry.set_pkey(Pkey::new(13).unwrap());

        assert_eq!(entry.flags().pkey(), 13);
    }
}
