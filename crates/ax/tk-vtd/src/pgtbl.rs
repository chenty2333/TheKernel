//! Intel VT-d second-level four-level page tables (Intel VT-d Architecture
//! Specification, §3.4). This is an original implementation of the format.
use alloc::vec::Vec;
use core::ptr::NonNull;

use crate::Error;

pub const PAGE_SIZE: u64 = 4096;
const ENTRIES: usize = 512;
const READ: u64 = 1;
const WRITE: u64 = 2;
const PRESENT: u64 = READ | WRITE;
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const ADDRESS_BITS: u32 = 48;
const LARGE_PAGE: u64 = 1 << 7;
const PAGE_SIZE_2M: u64 = 1 << 21;

/// Allocator/mapping seam for zeroed, page-aligned VT-d table pages.
///
/// # Safety
/// `page_mut` must return the unique CPU mapping for an owned 4 KiB physical
/// page; `alloc_page` must return a zeroed page aligned to 4 KiB.
/// `free_page` may only receive an owned page after hardware references are
/// invalidated. `flush_range` must publish every modified byte in the owned
/// page to the IOMMU before the caller issues a translation invalidation.
pub unsafe trait PageMemory {
    fn alloc_page(&mut self) -> Result<u64, Error>;
    fn page_mut(&mut self, physical: u64) -> Option<NonNull<[u64; ENTRIES]>>;
    fn flush_range(&mut self, physical: u64, offset: usize, length: usize) -> Result<(), Error>;
    unsafe fn free_page(&mut self, physical: u64);
}

pub struct SecondLevel<M: PageMemory> {
    memory: M,
    root: u64,
    owned_pages: Vec<u64>,
}

impl<M: PageMemory> SecondLevel<M> {
    pub fn new(mut memory: M) -> Result<Self, Error> {
        let root = memory.alloc_page()?;
        if root & (PAGE_SIZE - 1) != 0 || root & !ADDRESS_MASK != 0 {
            // SAFETY: alloc_page returned this page as owned memory.
            unsafe { memory.free_page(root) };
            return Err(Error::InvalidRange);
        }
        let Some(mut pml4) = memory.page_mut(root) else {
            // SAFETY: alloc_page returned this page as owned memory.
            unsafe { memory.free_page(root) };
            return Err(Error::MapFailed);
        };
        // SAFETY: the allocator contract gives exclusive access to this new page.
        unsafe { pml4.as_mut().fill(0) };
        if let Err(error) = memory.flush_range(root, 0, PAGE_SIZE as usize) {
            // No hardware can reference a second-level root before it is
            // installed in a context entry.
            // SAFETY: the page is still owned by this not-yet-constructed table.
            unsafe { memory.free_page(root) };
            return Err(error);
        }
        let mut owned_pages = Vec::new();
        if owned_pages.try_reserve_exact(1).is_err() {
            // SAFETY: this page is still owned by the new table.
            unsafe { memory.free_page(root) };
            return Err(Error::OutOfMemory);
        }
        owned_pages.push(root);
        Ok(Self {
            memory,
            root,
            owned_pages,
        })
    }

    pub const fn root_physical(&self) -> u64 {
        self.root
    }

    fn table_entry(&mut self, parent: u64, index: usize) -> Result<u64, Error> {
        let mut parent_page = self.memory.page_mut(parent).ok_or(Error::MapFailed)?;
        // SAFETY: page_mut uniquely lends this table and index is in 0..512.
        let entry = unsafe { &mut parent_page.as_mut()[index] };
        if *entry & PRESENT == 0 {
            self.owned_pages
                .try_reserve(1)
                .map_err(|_| Error::OutOfMemory)?;
            let child = self.memory.alloc_page()?;
            if child & (PAGE_SIZE - 1) != 0 || child & !ADDRESS_MASK != 0 {
                // SAFETY: allocator returned child as owned memory.
                unsafe { self.memory.free_page(child) };
                return Err(Error::InvalidRange);
            }
            let Some(mut page) = self.memory.page_mut(child) else {
                // SAFETY: allocator returned child as owned memory.
                unsafe { self.memory.free_page(child) };
                return Err(Error::MapFailed);
            };
            // SAFETY: child is freshly allocated and exclusively owned.
            unsafe { page.as_mut().fill(0) };
            if let Err(error) = self.memory.flush_range(child, 0, PAGE_SIZE as usize) {
                // The child is not yet reachable from its parent.
                // SAFETY: the page is still owned and no hardware pointer names it.
                unsafe { self.memory.free_page(child) };
                return Err(error);
            }
            *entry = child | PRESENT;
            self.owned_pages.push(child);
            self.memory
                .flush_range(
                    parent,
                    index * core::mem::size_of::<u64>(),
                    core::mem::size_of::<u64>(),
                )
                .map_err(|_| Error::Quarantined)?;
            Ok(child)
        } else {
            if *entry & (1 << 7) != 0 {
                return Err(Error::InvalidRange);
            }
            Ok(*entry & ADDRESS_MASK)
        }
    }

    /// Install 4 KiB read/write mappings for page-aligned addresses.
    pub fn map(&mut self, physical: u64, iova: u64, length: usize) -> Result<(), Error> {
        self.map_with_flags(physical, iova, length, PRESENT)
    }

    /// Install 4 KiB mappings with the Intel PTE access/snoop/transient bits.
    pub fn map_with_flags(
        &mut self,
        physical: u64,
        iova: u64,
        length: usize,
        flags: u64,
    ) -> Result<(), Error> {
        let permitted = PRESENT | (1 << 11) | (1 << 62);
        if length == 0
            || flags & PRESENT == 0
            || flags & !permitted != 0
            || !physical.is_multiple_of(PAGE_SIZE)
            || !iova.is_multiple_of(PAGE_SIZE)
            || !length.is_multiple_of(PAGE_SIZE as usize)
            || physical
                .checked_add(length as u64)
                .is_none_or(|end| end > 1 << ADDRESS_BITS)
            || iova
                .checked_add(length as u64)
                .is_none_or(|end| end > 1 << ADDRESS_BITS)
        {
            return Err(Error::InvalidRange);
        }
        for offset in (0..length).step_by(PAGE_SIZE as usize) {
            let pa = physical + offset as u64;
            let va = iova + offset as u64;
            let i4 = ((va >> 39) & 0x1ff) as usize;
            let i3 = ((va >> 30) & 0x1ff) as usize;
            let i2 = ((va >> 21) & 0x1ff) as usize;
            let i1 = ((va >> 12) & 0x1ff) as usize;
            let l3 = self.table_entry(self.root, i4)?;
            let l2 = self.table_entry(l3, i3)?;
            let l1 = self.table_entry(l2, i2)?;
            let mut page = self.memory.page_mut(l1).ok_or(Error::MapFailed)?;
            // SAFETY: page_mut uniquely lends the PT and index is in 0..512.
            let entry = unsafe { &mut page.as_mut()[i1] };
            if *entry & PRESENT != 0 {
                return Err(Error::MapFailed);
            }
            *entry = (pa & ADDRESS_MASK) | flags;
            self.memory
                .flush_range(
                    l1,
                    i1 * core::mem::size_of::<u64>(),
                    core::mem::size_of::<u64>(),
                )
                .map_err(|_| Error::Quarantined)?;
        }
        Ok(())
    }

    /// Install supervisor-independent read/write identity mappings using 2 MiB
    /// leaves. This is the legacy-driver identity domain, not a DMA API alias.
    pub fn map_identity_2m(&mut self, end_exclusive: u64) -> Result<(), Error> {
        if end_exclusive == 0
            || !end_exclusive.is_multiple_of(PAGE_SIZE_2M)
            || end_exclusive > 1 << ADDRESS_BITS
        {
            return Err(Error::InvalidRange);
        }
        for physical in (0..end_exclusive).step_by(PAGE_SIZE_2M as usize) {
            let i4 = ((physical >> 39) & 0x1ff) as usize;
            let i3 = ((physical >> 30) & 0x1ff) as usize;
            let i2 = ((physical >> 21) & 0x1ff) as usize;
            let l3 = self.table_entry(self.root, i4)?;
            let l2 = self.table_entry(l3, i3)?;
            let mut page = self.memory.page_mut(l2).ok_or(Error::MapFailed)?;
            // SAFETY: page_mut uniquely lends the PD and index is in 0..512.
            let entry = unsafe { &mut page.as_mut()[i2] };
            if *entry & PRESENT != 0 {
                return Err(Error::MapFailed);
            }
            *entry = physical | PRESENT | LARGE_PAGE;
            self.memory
                .flush_range(
                    l2,
                    i2 * core::mem::size_of::<u64>(),
                    core::mem::size_of::<u64>(),
                )
                .map_err(|_| Error::Quarantined)?;
        }
        Ok(())
    }

    /// Remove 4 KiB mappings. Table pages are retained until the domain is
    /// destroyed so hardware can never observe a freed paging structure.
    pub fn unmap(&mut self, iova: u64, length: usize) -> Result<(), Error> {
        if length == 0
            || !iova.is_multiple_of(PAGE_SIZE)
            || !length.is_multiple_of(PAGE_SIZE as usize)
            || iova
                .checked_add(length as u64)
                .is_none_or(|end| end > 1 << ADDRESS_BITS)
        {
            return Err(Error::InvalidRange);
        }
        for offset in (0..length).step_by(PAGE_SIZE as usize) {
            let va = iova + offset as u64;
            let i4 = ((va >> 39) & 0x1ff) as usize;
            let i3 = ((va >> 30) & 0x1ff) as usize;
            let i2 = ((va >> 21) & 0x1ff) as usize;
            let i1 = ((va >> 12) & 0x1ff) as usize;
            let l3 = self.existing_table(self.root, i4)?;
            let l2 = self.existing_table(l3, i3)?;
            let l1 = self.existing_table(l2, i2)?;
            let mut page = self.memory.page_mut(l1).ok_or(Error::MapFailed)?;
            // SAFETY: page_mut uniquely lends the PT and index is in 0..512.
            let entry = unsafe { &mut page.as_mut()[i1] };
            if *entry & PRESENT == 0 {
                return Err(Error::MapFailed);
            }
            *entry = 0;
            self.memory
                .flush_range(
                    l1,
                    i1 * core::mem::size_of::<u64>(),
                    core::mem::size_of::<u64>(),
                )
                .map_err(|_| Error::Quarantined)?;
        }
        Ok(())
    }

    fn existing_table(&mut self, parent: u64, index: usize) -> Result<u64, Error> {
        let mut page = self.memory.page_mut(parent).ok_or(Error::MapFailed)?;
        // SAFETY: page_mut uniquely lends table page and index is bounded.
        let entry = unsafe { page.as_mut()[index] };
        if entry & PRESENT == 0 || entry & (1 << 7) != 0 {
            return Err(Error::MapFailed);
        }
        Ok(entry & ADDRESS_MASK)
    }

    pub fn translate(&mut self, iova: u64) -> Option<u64> {
        let i4 = ((iova >> 39) & 0x1ff) as usize;
        let i3 = ((iova >> 30) & 0x1ff) as usize;
        let i2 = ((iova >> 21) & 0x1ff) as usize;
        let i1 = ((iova >> 12) & 0x1ff) as usize;
        let l3 = self.existing_table(self.root, i4).ok()?;
        let l2 = self.existing_table(l3, i3).ok()?;
        let mut directory = self.memory.page_mut(l2)?;
        // SAFETY: page_mut uniquely lends the PD and index is bounded.
        let directory_entry = unsafe { directory.as_mut()[i2] };
        if directory_entry & PRESENT != 0 && directory_entry & LARGE_PAGE != 0 {
            return Some((directory_entry & ADDRESS_MASK) | (iova & (PAGE_SIZE_2M - 1)));
        }
        let l1 = self.existing_table(l2, i2).ok()?;
        let mut page = self.memory.page_mut(l1)?;
        // SAFETY: page_mut uniquely lends table and index is bounded.
        let entry = unsafe { page.as_mut()[i1] };
        (entry & PRESENT != 0).then_some((entry & ADDRESS_MASK) | (iova & 0xfff))
    }
}

impl<M: PageMemory> Drop for SecondLevel<M> {
    fn drop(&mut self) {
        for physical in self.owned_pages.drain(..).rev() {
            // SAFETY: these are all pages allocated and retained by this table;
            // the owner must have quiesced hardware before dropping the domain.
            unsafe { self.memory.free_page(physical) };
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
    use core::ptr::NonNull;

    use super::*;

    struct FakeMemory {
        next: u64,
        pages: BTreeMap<u64, Box<[u64; ENTRIES]>>,
        flushes: Vec<(u64, usize, usize, u64)>,
    }
    unsafe impl PageMemory for FakeMemory {
        fn alloc_page(&mut self) -> Result<u64, Error> {
            let physical = self.next;
            self.next += PAGE_SIZE;
            self.pages.insert(physical, Box::new([0; ENTRIES]));
            Ok(physical)
        }
        fn page_mut(&mut self, physical: u64) -> Option<NonNull<[u64; ENTRIES]>> {
            self.pages
                .get_mut(&physical)
                .map(|page| NonNull::from(page.as_mut()))
        }
        fn flush_range(
            &mut self,
            physical: u64,
            offset: usize,
            length: usize,
        ) -> Result<(), Error> {
            if !self.pages.contains_key(&physical)
                || offset
                    .checked_add(length)
                    .is_none_or(|end| end > PAGE_SIZE as usize)
            {
                return Err(Error::InvalidRange);
            }
            let value = if length == core::mem::size_of::<u64>()
                && offset.is_multiple_of(core::mem::size_of::<u64>())
            {
                self.pages[&physical][offset / core::mem::size_of::<u64>()]
            } else {
                0
            };
            self.flushes.push((physical, offset, length, value));
            Ok(())
        }
        unsafe fn free_page(&mut self, physical: u64) {
            self.pages.remove(&physical);
        }
    }
    fn fake() -> FakeMemory {
        FakeMemory {
            next: 0x1000,
            pages: BTreeMap::new(),
            flushes: Vec::new(),
        }
    }

    #[test]
    fn maps_and_unmaps_four_level_second_level_pages() {
        let mut page_table = SecondLevel::new(fake()).unwrap();
        let initial_flushes = page_table.memory.flushes.len();
        page_table.map(0x8000, 0x4000_0000, 0x3000).unwrap();
        assert!(page_table.memory.flushes.len() > initial_flushes);
        assert!(
            page_table
                .memory
                .flushes
                .iter()
                .all(|(physical, offset, length, _)| {
                    page_table.memory.pages.contains_key(physical)
                        && offset
                            .checked_add(*length)
                            .is_some_and(|end| end <= PAGE_SIZE as usize)
                })
        );
        assert_ne!(
            page_table.memory.flushes.last().unwrap().3 & PRESENT,
            0,
            "the mapped leaf value is written before its flush hook"
        );
        assert_eq!(page_table.translate(0x4000_0000), Some(0x8000));
        assert_eq!(page_table.translate(0x4000_2abc), Some(0xaabc));
        page_table.unmap(0x4000_0000, 0x3000).unwrap();
        assert_eq!(page_table.memory.flushes.last().unwrap().3, 0);
        assert_eq!(page_table.translate(0x4000_0000), None);
    }

    #[test]
    fn mapping_refuses_unaligned_and_colliding_iovas() {
        let mut page_table = SecondLevel::new(fake()).unwrap();
        assert_eq!(page_table.map(1, 0x1000, 4096), Err(Error::InvalidRange));
        page_table.map(0x1000, 0x2000, 4096).unwrap();
        assert_eq!(page_table.map(0x3000, 0x2000, 4096), Err(Error::MapFailed));
    }

    #[test]
    fn identity_domain_uses_two_megabyte_leaves_across_gigabyte_boundaries() {
        let mut page_table = SecondLevel::new(fake()).unwrap();
        page_table
            .map_identity_2m((1 << 30) + PAGE_SIZE_2M)
            .unwrap();
        assert_eq!(page_table.translate(0x1fff), Some(0x1fff));
        assert_eq!(page_table.translate(0x1234_5678), Some(0x1234_5678));
        assert_eq!(
            page_table.translate((1 << 30) + 0xabc),
            Some((1 << 30) + 0xabc)
        );
        assert_eq!(page_table.translate((1 << 30) + PAGE_SIZE_2M), None);
    }

    #[test]
    fn translated_iova_can_be_added_just_above_the_identity_aperture() {
        let mut page_table = SecondLevel::new(fake()).unwrap();
        page_table.map_identity_2m(1 << 30).unwrap();
        page_table.map(0x8000, 1 << 30, 4096).unwrap();
        assert_eq!(page_table.translate(0x1234), Some(0x1234));
        assert_eq!(page_table.translate((1 << 30) + 0xabc), Some(0x8abc));
    }
}
