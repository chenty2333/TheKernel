use alloc::{sync::Arc, vec::Vec};
use core::{
    any::Any,
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize, PageTable, PageTableCursor, PagingError};
use axsync::Mutex;
use memory_addr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    super::SharedBackingKey, AddrSpace, Backend, BackendOps, BackendRetirement, FutexBackingId,
    FutexBackingIdentity, FutexWordOffset, MappingStatus, PopulateOutcome, SharedFutexKey,
    alloc_frame, dealloc_frame, divide_page, page_table_flags, pages_in, preflight_sparse_unmap,
    preflight_sparse_leaves,
};
use crate::{
    file::{DeferredFileLease, FileHandle, FileLike, FileMmapProtection, PreparedFileMmap},
    mm::{FileLikeMappingLease, FileMappingSharing},
};

static FIXED_SHARED_MAPPING_ID: AtomicU64 = AtomicU64::new(1);
// Resident anonymous/SysV shared pages. File-backed mappings deliberately do
// not use this path: their cache pages are not anonymous shmem pages.
static SHMEM_RESIDENT_PAGES: AtomicUsize = AtomicUsize::new(0);

const FOLIO_4K_PAGES: usize = PageSize::Size2M as usize / PageSize::Size4K as usize;

/// An owned 2 MiB allocation represented by 512 indexed 4 KiB entries.
///
/// `SharedPageStorage::pages` remains the authoritative indexed lookup table:
/// callers never need to know whether an entry was originally allocated as a
/// base page or as part of a folio.  This side table exists only to give the
/// allocator the correct lifetime and deallocation granularity.
#[derive(Clone)]
struct SharedFolio {
    start_index: usize,
    paddr: PhysAddr,
    /// The exact former base frames stay owned until the folio is demoted.
    /// Keeping them is what makes a failed cross-mm publication reversible:
    /// rolling the PTEs back never revives already-freed physical memory.
    old_pages: Vec<PhysAddr>,
}

struct SharedPageStorage {
    pages: Vec<PhysAddr>,
    folios: Vec<SharedFolio>,
}

pub fn shmem_resident_pages() -> usize {
    SHMEM_RESIDENT_PAGES.load(Ordering::Acquire)
}

/// Converts resident backing frames to Linux `NR_SHMEM` base-page units.
///
/// The backing can use a huge page, but sysinfo's `sharedram` is expressed in
/// bytes from a count of 4 KiB base pages. Keeping the charge in that unit
/// makes allocation, growth, and final drop independent of huge-page
/// promotion or demotion.
fn shmem_base_pages(resident_frames: usize, page_size: PageSize) -> usize {
    resident_frames.saturating_mul(page_size as usize / PAGE_SIZE_4K)
}

fn charge_shmem_pages(charge: &AtomicUsize, resident_frames: usize, page_size: PageSize) {
    charge.fetch_add(
        shmem_base_pages(resident_frames, page_size),
        Ordering::Release,
    );
}

fn uncharge_shmem_pages(charge: &AtomicUsize, resident_frames: usize, page_size: PageSize) {
    let pages = shmem_base_pages(resident_frames, page_size);
    let _ = charge.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        n.checked_sub(pages)
    });
}

pub struct SharedPages {
    backing_key: SharedBackingKey,
    phys_pages: Mutex<SharedPageStorage>,
    // `futex_id` is queried while an IRQ-safe futex queue gate is held. Keep
    // the published length separate from `phys_pages`: taking that mutex in
    // the gate would be a blocking operation. The backing only grows, so a
    // stale (smaller) snapshot can cause a retry but can never expose a word
    // beyond the live allocation.
    published_len: AtomicUsize,
    pub size: PageSize,
    fixed: bool,
    resident_charge: Option<&'static AtomicUsize>,
}
impl SharedPages {
    pub fn new(size: usize, page_size: PageSize) -> AxResult<Self> {
        Self::new_with_growth(size, page_size, true)
    }

    /// Allocates an exact shared backing which can never grow after
    /// publication. All frames and vector capacity are acquired by this call.
    pub fn new_fixed(size: usize, page_size: PageSize) -> AxResult<Self> {
        Self::new_with_growth(size, page_size, false)
    }

    /// Allocates SysV shared pages and charges only frames that were actually
    /// obtained.  The charge lives with the backing Arc through its final drop.
    pub fn new_sysv_charged(size: usize, page_size: PageSize) -> AxResult<Self> {
        // SysV segments have a fixed shm_segsz; unlike anonymous shared
        // mappings they cannot grow through a later range extension.
        Self::new_with_growth_charged(size, page_size, false, Some(&SHMEM_RESIDENT_PAGES))
    }

    /// Constructs a growable anonymous MAP_SHARED backing accounted as shmem.
    pub fn new_shmem(size: usize, page_size: PageSize) -> AxResult<Self> {
        Self::new_with_growth_charged(size, page_size, true, Some(&SHMEM_RESIDENT_PAGES))
    }

    fn new_with_growth(size: usize, page_size: PageSize, growable: bool) -> AxResult<Self> {
        Self::new_with_growth_charged(size, page_size, growable, None)
    }

    fn new_with_growth_charged(
        size: usize,
        page_size: PageSize,
        growable: bool,
        resident_charge: Option<&'static AtomicUsize>,
    ) -> AxResult<Self> {
        if !page_size.is_aligned(size) {
            return Err(AxError::InvalidInput);
        }
        // Reserve identity before acquiring frames so key exhaustion leaves no
        // allocation or resident charge to unwind.
        let backing_key = SharedBackingKey::allocate()?;
        let num_pages = size / page_size as usize;
        let mut phys_pages = Vec::new();
        phys_pages
            .try_reserve_exact(num_pages)
            .map_err(|_| AxError::NoMemory)?;
        for _ in 0..num_pages {
            match alloc_frame(true, page_size) {
                Ok(frame) => phys_pages.push(frame),
                Err(err) => {
                    for frame in phys_pages {
                        dealloc_frame(frame, page_size);
                    }
                    return Err(err);
                }
            }
        }
        if let Some(charge) = resident_charge {
            charge_shmem_pages(charge, num_pages, page_size);
        }
        Ok(Self {
            backing_key,
            phys_pages: Mutex::new(SharedPageStorage {
                pages: phys_pages,
                folios: Vec::new(),
            }),
            published_len: AtomicUsize::new(num_pages),
            size: page_size,
            fixed: !growable,
            resident_charge,
        })
    }

    /// Stable reverse-map identity for this backing's complete lifetime.
    pub(crate) const fn backing_key(&self) -> SharedBackingKey {
        self.backing_key
    }

    pub const fn page_size(&self) -> PageSize {
        self.size
    }

    pub const fn is_fixed(&self) -> bool {
        self.fixed
    }

    pub fn len(&self) -> usize {
        self.phys_pages.lock().pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.phys_pages.lock().pages.is_empty()
    }

    pub fn ensure_len(&self, len: usize) -> AxResult {
        let current_len = self.phys_pages.lock().pages.len();
        if current_len >= len {
            return Ok(());
        }
        if self.fixed {
            return Err(AxError::InvalidInput);
        }

        let mut new_pages = Vec::new();
        new_pages
            .try_reserve_exact(len - current_len)
            .map_err(|_| AxError::NoMemory)?;
        for _ in current_len..len {
            match alloc_frame(true, self.size) {
                Ok(frame) => new_pages.push(frame),
                Err(err) => {
                    for frame in new_pages {
                        dealloc_frame(frame, self.size);
                    }
                    return Err(err);
                }
            }
        }

        let mut pages = self.phys_pages.lock();
        if pages.pages.len() >= len {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Ok(());
        }
        let needed = len - pages.pages.len();
        if pages.pages.try_reserve_exact(needed).is_err() {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Err(AxError::NoMemory);
        }
        let unused = new_pages.split_off(needed);
        pages.pages.extend(new_pages);
        if let Some(charge) = self.resident_charge {
            charge_shmem_pages(charge, needed, self.size);
        }
        let published_len = pages.pages.len();
        drop(pages);
        self.published_len.store(published_len, Ordering::Release);
        for frame in unused {
            dealloc_frame(frame, self.size);
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> usize {
        self.len() * self.size as usize
    }

    fn total_bytes_snapshot(&self) -> usize {
        self.published_len.load(Ordering::Acquire) * self.size as usize
    }

    pub fn read_bytes(&self, offset: usize, mut buf: &mut [u8]) -> AxResult {
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)? > self.total_bytes() {
            return Err(AxError::InvalidInput);
        }

        let page_bytes = self.size as usize;
        let pages = self.phys_pages.lock();
        let mut page_index = offset / page_bytes;
        let mut page_offset = offset % page_bytes;

        while !buf.is_empty() {
            let phys = pages.pages[page_index];
            let src = axhal::mem::phys_to_virt(phys).as_usize() + page_offset;
            let chunk_len = (page_bytes - page_offset).min(buf.len());
            unsafe {
                core::ptr::copy_nonoverlapping(src as *const u8, buf.as_mut_ptr(), chunk_len);
            }
            let (_, rest) = buf.split_at_mut(chunk_len);
            buf = rest;
            page_index += 1;
            page_offset = 0;
        }

        Ok(())
    }

    pub fn write_bytes(&self, offset: usize, mut buf: &[u8]) -> AxResult {
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)? > self.total_bytes() {
            return Err(AxError::InvalidInput);
        }

        let page_bytes = self.size as usize;
        let pages = self.phys_pages.lock();
        let mut page_index = offset / page_bytes;
        let mut page_offset = offset % page_bytes;

        while !buf.is_empty() {
            let phys = pages.pages[page_index];
            let dst = axhal::mem::phys_to_virt(phys).as_usize() + page_offset;
            let chunk_len = (page_bytes - page_offset).min(buf.len());
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, chunk_len);
            }
            buf = &buf[chunk_len..];
            page_index += 1;
            page_offset = 0;
        }

        Ok(())
    }

    fn with_pages_range<T>(
        &self,
        start_index: usize,
        count: usize,
        use_pages: impl FnOnce(&[PhysAddr]) -> AxResult<T>,
    ) -> AxResult<T> {
        let pages = self.phys_pages.lock();
        let end = start_index
            .checked_add(count)
            .ok_or(AxError::InvalidInput)?;
        if end > pages.pages.len() {
            return Err(AxError::NoMemory);
        }
        use_pages(&pages.pages[start_index..end])
    }

    /// Returns the physical address for a 4 KiB indexed backing entry.
    ///
    /// A promoted folio still exposes all 512 entries through this interface,
    /// as consecutive physical addresses derived from its 2 MiB base.
    pub fn paddr_at(&self, index: usize) -> AxResult<PhysAddr> {
        self.phys_pages
            .lock()
            .pages
            .get(index)
            .copied()
            .ok_or(AxError::InvalidInput)
    }

    /// Snapshots the source 4 KiB frames retained by a promoted folio.
    ///
    /// Callers use this before changing any alias PTE so all allocation can
    /// fail before publication.  The returned frames stay owned by the folio
    /// until [`Self::demote_4k_folio`] commits the ownership transition.
    pub fn demote_4k_folio_frames(&self, start_index: usize) -> AxResult<Vec<PhysAddr>> {
        if self.size != PageSize::Size4K || !start_index.is_multiple_of(FOLIO_4K_PAGES) {
            return Err(AxError::InvalidInput);
        }
        let storage = self.phys_pages.lock();
        let folio = storage
            .folios
            .iter()
            .find(|folio| folio.start_index == start_index)
            .ok_or(AxError::InvalidInput)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(FOLIO_4K_PAGES)
            .map_err(|_| AxError::NoMemory)?;
        frames.extend_from_slice(&folio.old_pages);
        Ok(frames)
    }

    pub fn has_4k_folio(&self, start_index: usize) -> bool {
        self.size == PageSize::Size4K
            && self
                .phys_pages
                .lock()
                .folios
                .iter()
                .any(|folio| folio.start_index == start_index)
    }

    fn physical_page(&self, index: usize) -> AxResult<PhysAddr> {
        self.paddr_at(index)
    }

    /// Transactionally replace 512 base-page entries with one 2 MiB folio.
    ///
    /// Allocation, metadata reservation, and copying complete before the
    /// indexed table is published.  Consequently a failure leaves the old
    /// backing untouched; after publication the old frames are released.
    pub fn promote_4k_folio(&self, start_index: usize) -> AxResult<PhysAddr> {
        if self.size != PageSize::Size4K || !start_index.is_multiple_of(FOLIO_4K_PAGES) {
            return Err(AxError::InvalidInput);
        }
        let end = start_index
            .checked_add(FOLIO_4K_PAGES)
            .ok_or(AxError::InvalidInput)?;
        let mut storage = self.phys_pages.lock();
        if end > storage.pages.len()
            || storage.folios.iter().any(|folio| {
                let folio_end = folio.start_index + FOLIO_4K_PAGES;
                start_index < folio_end && folio.start_index < end
            })
        {
            return Err(AxError::InvalidInput);
        }
        storage
            .folios
            .try_reserve_exact(1)
            .map_err(|_| AxError::NoMemory)?;
        let mut old_pages = Vec::new();
        old_pages
            .try_reserve_exact(FOLIO_4K_PAGES)
            .map_err(|_| AxError::NoMemory)?;
        old_pages.extend_from_slice(&storage.pages[start_index..end]);

        let folio = alloc_frame(false, PageSize::Size2M)?;
        for (offset, &source) in old_pages.iter().enumerate() {
            let destination = axhal::mem::phys_to_virt(PhysAddr::from(
                folio.as_usize() + offset * PageSize::Size4K as usize,
            ));
            unsafe {
                core::ptr::copy_nonoverlapping(
                    axhal::mem::phys_to_virt(source).as_ptr(),
                    destination.as_mut_ptr(),
                    PageSize::Size4K as usize,
                );
            }
        }

        for (offset, entry) in storage.pages[start_index..end].iter_mut().enumerate() {
            *entry = PhysAddr::from(folio.as_usize() + offset * PageSize::Size4K as usize);
        }
        storage.folios.push(SharedFolio {
            start_index,
            paddr: folio,
            old_pages,
        });
        Ok(folio)
    }

    /// Transactionally restore the base-page backing for a promoted folio.
    ///
    /// This is the inverse ownership transition.  The exact source 4 KiB
    /// frames are retained by the promoted folio, so no allocation is needed
    /// and a transaction can restore the original backing identity exactly.
    pub fn demote_4k_folio(&self, start_index: usize) -> AxResult {
        if self.size != PageSize::Size4K || !start_index.is_multiple_of(FOLIO_4K_PAGES) {
            return Err(AxError::InvalidInput);
        }
        let end = start_index
            .checked_add(FOLIO_4K_PAGES)
            .ok_or(AxError::InvalidInput)?;
        let mut storage = self.phys_pages.lock();
        let folio_index = storage
            .folios
            .iter()
            .position(|folio| folio.start_index == start_index)
            .ok_or(AxError::InvalidInput)?;
        if end > storage.pages.len() {
            return Err(AxError::BadState);
        }
        let folio = &storage.folios[folio_index];
        if folio.old_pages.len() != FOLIO_4K_PAGES {
            return Err(AxError::BadState);
        }
        for (offset, &destination) in folio.old_pages.iter().enumerate() {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    axhal::mem::phys_to_virt(PhysAddr::from(
                        folio.paddr.as_usize() + offset * PageSize::Size4K as usize,
                    ))
                    .as_ptr(),
                    axhal::mem::phys_to_virt(destination).as_mut_ptr(),
                    PageSize::Size4K as usize,
                );
            }
        }
        let folio = storage.folios.remove(folio_index);
        storage.pages[start_index..end].copy_from_slice(&folio.old_pages);
        dealloc_frame(folio.paddr, PageSize::Size2M);
        Ok(())
    }

    /// Returns an aligned, bounds-checked atomic view into a fixed backing.
    /// The handle exposes only the memory order required by shared producer /
    /// consumer rings; callers cannot accidentally perform relaxed accesses.
    pub fn atomic_u32(self: &Arc<Self>, offset: usize) -> AxResult<SharedAtomicU32> {
        validate_atomic_u32_offset(self.total_bytes(), self.size as usize, offset)?;
        if !self.fixed {
            return Err(AxError::InvalidInput);
        }
        let page_size = self.size as usize;
        let page = self.physical_page(offset / page_size)?;
        let in_page = offset % page_size;
        let virtual_address = axhal::mem::phys_to_virt(page)
            .as_usize()
            .checked_add(in_page)
            .ok_or(AxError::InvalidInput)?;
        let address =
            NonNull::new(virtual_address as *mut AtomicU32).ok_or(AxError::InvalidInput)?;
        Ok(SharedAtomicU32 {
            address,
            pages: self.clone(),
        })
    }
}

fn validate_atomic_u32_offset(total_bytes: usize, page_size: usize, offset: usize) -> AxResult {
    if page_size < core::mem::size_of::<AtomicU32>()
        || !offset.is_multiple_of(core::mem::align_of::<AtomicU32>())
        || offset
            .checked_add(core::mem::size_of::<AtomicU32>())
            .is_none_or(|end| end > total_bytes)
        || offset % page_size > page_size - core::mem::size_of::<AtomicU32>()
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

/// A lifetime-pinned atomic word stored in a fixed shared-page backing.
pub struct SharedAtomicU32 {
    address: NonNull<AtomicU32>,
    pages: Arc<SharedPages>,
}

// The target is naturally aligned, points into immutable backing storage, and
// is accessed exclusively through AtomicU32 operations.
unsafe impl Send for SharedAtomicU32 {}
unsafe impl Sync for SharedAtomicU32 {}

impl Clone for SharedAtomicU32 {
    fn clone(&self) -> Self {
        Self {
            address: self.address,
            pages: self.pages.clone(),
        }
    }
}

impl SharedAtomicU32 {
    pub fn load_acquire(&self) -> u32 {
        // SAFETY: construction validated alignment and bounds, while `pages`
        // pins the physical frame for the complete handle lifetime.
        atomic_load_acquire(self.address)
    }

    pub fn store_release(&self, value: u32) {
        // SAFETY: see load_acquire; AtomicU32 supplies the shared mutation law.
        atomic_store_release(self.address, value);
    }
}

fn atomic_load_acquire(address: NonNull<AtomicU32>) -> u32 {
    // SAFETY: callers supply a live, naturally aligned AtomicU32.
    unsafe { address.as_ref() }.load(Ordering::Acquire)
}

fn atomic_store_release(address: NonNull<AtomicU32>, value: u32) {
    // SAFETY: callers supply a live, naturally aligned AtomicU32.
    unsafe { address.as_ref() }.store(value, Ordering::Release);
}

impl Drop for SharedPages {
    fn drop(&mut self) {
        let storage = self.phys_pages.lock();
        for (index, &frame) in storage.pages.iter().enumerate() {
            if !storage.folios.iter().any(|folio| {
                (folio.start_index..folio.start_index + FOLIO_4K_PAGES).contains(&index)
            }) {
                dealloc_frame(frame, self.size);
            }
        }
        for folio in &storage.folios {
            dealloc_frame(folio.paddr, PageSize::Size2M);
            for &frame in &folio.old_pages {
                dealloc_frame(frame, PageSize::Size4K);
            }
        }
        if let Some(charge) = self.resident_charge {
            uncharge_shmem_pages(
                charge,
                self.published_len.load(Ordering::Relaxed),
                self.size,
            );
        }
    }
}

// FIXME: This implementation does not allow map or unmap partial ranges.
#[derive(Clone)]
pub struct SharedBackend {
    start: VirtAddr,
    page_offset: usize,
    pages: Arc<SharedPages>,
    may_protect: MappingFlags,
    map_id: SharedMapId,
    status: MappingStatus,
}

#[derive(Clone)]
enum SharedMapId {
    Dynamic(Arc<()>),
    Fixed(u64),
}

impl SharedMapId {
    fn same_mapping(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Dynamic(lhs), Self::Dynamic(rhs)) => Arc::ptr_eq(lhs, rhs),
            (Self::Fixed(lhs), Self::Fixed(rhs)) => lhs == rhs,
            (Self::Dynamic(_), Self::Fixed(_)) | (Self::Fixed(_), Self::Dynamic(_)) => false,
        }
    }
}

impl SharedBackend {
    /// Clone this shared backing at a different page cursor while retaining
    /// its physical-object identity.  `remap_file_pages` uses this only while
    /// an AddrSpace replacement transaction owns the old VMA, so futex keys
    /// continue to name the same `SharedPages` object at the rebased offset.
    pub(crate) fn clone_rebased(
        &self,
        start: VirtAddr,
        page_offset: usize,
    ) -> AxResult<Self> {
        let byte_offset = page_offset
            .checked_mul(self.pages.size as usize)
            .ok_or(AxError::InvalidInput)?;
        if byte_offset >= self.pages.total_bytes() {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            start,
            page_offset,
            pages: self.pages.clone(),
            may_protect: self.may_protect,
            map_id: self.map_id.clone(),
            status: self.status.clone(),
        })
    }

    pub(super) const fn mapping_status(&self) -> &MappingStatus {
        &self.status
    }

    pub(super) fn mapping_status_mut(&mut self) -> &mut MappingStatus {
        &mut self.status
    }

    pub fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    pub(crate) fn backing_offset(&self, address: usize) -> Option<usize> {
        let relative = address.checked_sub(self.start.as_usize())?;
        self.page_offset
            .checked_mul(self.pages.size as usize)?
            .checked_add(relative)
    }

    pub(crate) fn futex_key(&self, address: usize) -> Option<SharedFutexKey> {
        let offset = self.backing_offset(address)?;
        let end = offset.checked_add(size_of::<u32>())?;
        (end <= self.pages.total_bytes()).then(|| {
            SharedFutexKey::new(
                FutexBackingIdentity::Shared(self.pages.clone()),
                FutexWordOffset::new(offset),
            )
        })
    }

    pub(crate) fn futex_id(&self, address: usize) -> Option<(FutexBackingId, FutexWordOffset)> {
        let offset = self.backing_offset(address)?;
        let end = offset.checked_add(size_of::<u32>())?;
        (end <= self.pages.total_bytes_snapshot()).then(|| {
            (
                FutexBackingId::shared(Arc::as_ptr(&self.pages) as usize),
                FutexWordOffset::new(offset),
            )
        })
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        self.map_id.same_mapping(&other.map_id)
            && self.start == other.start
            && self.page_offset == other.page_offset
            && Arc::ptr_eq(&self.pages, &other.pages)
    }

    fn clone_for_range_with_id(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        map_id: SharedMapId,
    ) -> AxResult<Self> {
        let (start, backing_advance) =
            super::relocate_affine_origin(self.start, old_start, new_start)?;
        let backing_pages = divide_page(backing_advance, self.pages.size)?;
        let page_offset = self
            .page_offset
            .checked_add(backing_pages)
            .ok_or(AxError::InvalidInput)?;
        Ok(Self {
            start,
            page_offset,
            pages: self.pages.clone(),
            may_protect: self.may_protect,
            map_id,
            status: self.status.relocated(old_start, new_start)?,
        })
    }

    pub(crate) fn clone_for_range(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> AxResult<Self> {
        self.clone_for_range_with_id(old_start, new_start, self.map_id.clone())
    }

    pub(crate) fn duplicate_mapping(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> AxResult<Self> {
        if self.pages.is_fixed() {
            return Err(AxError::OperationNotSupported);
        }
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        self.clone_for_range_with_id(old_start, new_start, SharedMapId::Dynamic(map_id))
    }

    pub(crate) fn ensure_range_covered(&self, start: VirtAddr, size: usize) -> AxResult {
        let offset = start
            .as_usize()
            .checked_sub(self.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        let start_index = self
            .page_offset
            .checked_add(divide_page(offset, self.pages.size)?)
            .ok_or(AxError::InvalidInput)?;
        let count = divide_page(size, self.pages.size)?;
        self.pages.ensure_len(
            start_index
                .checked_add(count)
                .ok_or(AxError::InvalidInput)?,
        )
    }

    pub(crate) fn check_protect_flags(&self, flags: MappingFlags) -> AxResult {
        let requested = flags & access_flags();
        if !self.may_protect.contains(requested) {
            return Err(AxError::PermissionDenied);
        }
        Ok(())
    }
}

impl BackendOps for SharedBackend {
    fn page_size(&self) -> PageSize {
        self.pages.size
    }

    fn map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        debug!("Shared::map: {range:?} {flags:?}");
        pages_in(range, self.pages.size)?;
        self.check_protect_flags(flags)?;
        Ok(())
    }

    fn preflight_protect(
        &self,
        range: VirtAddrRange,
        new_flags: MappingFlags,
        pt: &PageTable,
    ) -> AxResult {
        self.check_protect_flags(new_flags)?;
        preflight_sparse_unmap(range, self.pages.size, pt)
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult<BackendRetirement> {
        debug!("Shared::unmap: {range:?}");
        // `drain_present_leaves` handles both original folio-sized leaves and
        // the P1 children published by a prepared pkey demotion.
        let _ = pt.drain_present_leaves(range.start, range.size())?;
        Ok(BackendRetirement::empty())
    }

    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult {
        preflight_sparse_leaves(range, pt)
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> PopulateOutcome {
        let result = (|| {
            let offset = range
                .start
                .as_usize()
                .checked_sub(self.start.as_usize())
                .ok_or(AxError::InvalidInput)?;
            let start_index = self
                .page_offset
                .checked_add(divide_page(offset, self.pages.size)?)
                .ok_or(AxError::InvalidInput)?;
            let count = divide_page(range.size(), self.pages.size)?;
            let mut needs_tlb_sync = false;
            let result = self
                .pages
                .with_pages_range(start_index, count, |physical_pages| {
                    let mut populated = 0;
                    for (vaddr, &paddr) in
                        pages_in(range, self.pages.size)?.zip(physical_pages.iter())
                    {
                        match pt.query(vaddr) {
                            Ok((mapped_paddr, page_flags, page_size)) => {
                                if page_size != self.pages.size || mapped_paddr != paddr {
                                    return Err(AxError::BadAddress);
                                }
                                if access_flags.contains(MappingFlags::WRITE)
                                    && !page_flags.contains(MappingFlags::WRITE)
                                {
                                    pt.remap(vaddr, paddr, page_table_flags(flags))?;
                                    needs_tlb_sync = true;
                                    populated += 1;
                                } else if page_flags.contains(access_flags) {
                                    populated += 1;
                                }
                            }
                            Err(PagingError::NotMapped) => {
                                pt.map(vaddr, paddr, self.pages.size, page_table_flags(flags))?;
                                populated += 1;
                            }
                            Err(_) => return Err(AxError::BadAddress),
                        }
                    }
                    Ok(populated)
                });
            if needs_tlb_sync {
                pt.flush();
                drop(crate::mm::synchronize_tlb());
            }
            result
        })();
        PopulateOutcome::immediate(result)
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pt: &mut PageTableCursor,
        _new_pt: &mut PageTableCursor,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
        _active_long_term_cow_frames: &[PhysAddr],
    ) -> AxResult<Backend> {
        Ok(Backend::Shared(self.clone()))
    }
}

impl Backend {
    pub(crate) fn shared_pages(&self) -> Option<&Arc<SharedPages>> {
        match self {
            Self::Shared(shared) => Some(shared.pages()),
            Self::Linear(_) | Self::Cow(_) | Self::File(_) => None,
        }
    }

    /// Stable reverse-map identity for an anonymous/shared-memory backing.
    /// File mappings have their own cache identity and are deliberately not
    /// folded into this shmem alias registry.
    pub(crate) fn shared_backing_key(&self) -> Option<super::super::SharedBackingKey> {
        match self {
            Self::Shared(shared) => Some(shared.pages.backing_key()),
            Self::Linear(_) | Self::Cow(_) | Self::File(_) => None,
        }
    }

    pub fn new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> Self {
        Self::new_shared_with_may_protect(start, pages, access_flags())
    }

    pub fn try_new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> AxResult<Self> {
        Self::try_new_shared_with_may_protect(start, pages, access_flags())
    }

    pub fn try_new_shared_with_may_protect(
        start: VirtAddr,
        pages: Arc<SharedPages>,
        may_protect: MappingFlags,
    ) -> AxResult<Self> {
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        Ok(Self::Shared(SharedBackend {
            start,
            page_offset: 0,
            pages,
            may_protect: may_protect & access_flags(),
            map_id: SharedMapId::Dynamic(map_id),
            status: MappingStatus::default(),
        }))
    }

    pub fn new_shared_with_may_protect(
        start: VirtAddr,
        pages: Arc<SharedPages>,
        may_protect: MappingFlags,
    ) -> Self {
        Self::Shared(SharedBackend {
            start,
            page_offset: 0,
            pages,
            may_protect: may_protect & access_flags(),
            map_id: SharedMapId::Dynamic(Arc::new(())),
            status: MappingStatus::default(),
        })
    }
}

/// Every fallible resource needed to bind a fixed object mapping.
///
/// Construction runs before the address-space lock. `into_backend` only moves
/// prevalidated ownership into the VMA backend and performs no allocation.
pub(crate) struct PreparedFixedSharedMapping {
    pages: Arc<SharedPages>,
    owner: Option<DeferredFileLease>,
    ofd_key: u64,
    object_offset: u64,
    initial_flags: MappingFlags,
    may_protect: MappingFlags,
    map_id: u64,
}

impl PreparedFixedSharedMapping {
    pub(crate) fn try_new(
        handle: FileHandle<dyn FileLike>,
        plan: PreparedFileMmap,
    ) -> AxResult<Self> {
        let request = plan.request();
        let pages = plan.pages().clone();
        if !pages.is_fixed()
            || request.length() != pages.total_bytes()
            || request.page_size() != pages.page_size() as usize
        {
            return Err(AxError::InvalidInput);
        }

        let map_id = FIXED_SHARED_MAPPING_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| AxError::TooManyOpenFiles)?;
        let ofd_key = handle.open_file_description_key();
        let owner = if plan.retains_description() {
            let retained: Arc<dyn Any + Send + Sync> = pages.clone();
            Some(DeferredFileLease::try_new(handle, retained)?)
        } else {
            None
        };
        let may_protect = mapping_flags(plan.may_protect());
        Ok(Self {
            pages: plan.into_pages(),
            owner,
            ofd_key,
            object_offset: request.offset(),
            initial_flags: mapping_flags(request.protection()),
            may_protect,
            map_id,
        })
    }

    pub(crate) fn into_backend(self, start: VirtAddr) -> Backend {
        let Self {
            pages,
            owner,
            ofd_key,
            object_offset,
            initial_flags,
            may_protect,
            map_id,
        } = self;
        let mapping = match owner {
            Some(owner) => FileLikeMappingLease::new(
                owner,
                ofd_key,
                start,
                object_offset,
                initial_flags,
                may_protect,
                FileMappingSharing::Shared,
            ),
            None => FileLikeMappingLease::new_detached(
                map_id as usize,
                ofd_key,
                start,
                object_offset,
                initial_flags,
                may_protect,
                FileMappingSharing::Shared,
            ),
        };
        Backend::Shared(SharedBackend {
            start,
            page_offset: 0,
            pages,
            may_protect: may_protect & access_flags(),
            map_id: SharedMapId::Fixed(map_id),
            status: MappingStatus::default(),
        })
        .with_file_like_mapping(mapping)
    }
}

fn mapping_flags(protection: FileMmapProtection) -> MappingFlags {
    let mut flags = MappingFlags::USER;
    if protection.contains(FileMmapProtection::READ) {
        flags |= MappingFlags::READ;
    }
    if protection.contains(FileMmapProtection::WRITE) {
        flags |= MappingFlags::WRITE;
    }
    if protection.contains(FileMmapProtection::EXECUTE) {
        flags |= MappingFlags::EXECUTE;
    }
    flags
}

fn access_flags() -> MappingFlags {
    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shmem_charge_is_in_base_page_equivalents_for_all_backing_sizes() {
        assert_eq!(shmem_base_pages(1, PageSize::Size4K), 1);
        assert_eq!(shmem_base_pages(1, PageSize::Size2M), 512);
        assert_eq!(shmem_base_pages(1, PageSize::Size1G), 262_144);

        // The same resident byte range has one NR_SHMEM charge regardless of
        // whether its backing is represented as base pages or huge pages.
        assert_eq!(
            shmem_base_pages(512, PageSize::Size4K),
            shmem_base_pages(1, PageSize::Size2M)
        );
        assert_eq!(
            shmem_base_pages(512, PageSize::Size2M),
            shmem_base_pages(1, PageSize::Size1G)
        );
    }

    #[test]
    fn shmem_huge_backing_growth_and_drop_are_symmetric() {
        let _context = crate::test_support::scheduler_test_context();
        let baseline = shmem_resident_pages();
        let pages = SharedPages::new_shmem(PageSize::Size2M as usize, PageSize::Size2M).unwrap();
        assert_eq!(shmem_resident_pages(), baseline + 512);

        pages.ensure_len(2).unwrap();
        assert_eq!(shmem_resident_pages(), baseline + 1024);
        drop(pages);
        assert_eq!(shmem_resident_pages(), baseline);
    }

    #[test]
    fn shared_atomic_offsets_are_aligned_bounded_and_page_local() {
        assert!(validate_atomic_u32_offset(0x2000, 0x1000, 0).is_ok());
        assert!(validate_atomic_u32_offset(0x2000, 0x1000, 0xffc).is_ok());
        assert_eq!(
            validate_atomic_u32_offset(0x2000, 0x1000, 1),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_atomic_u32_offset(0x2000, 0x1000, 0x2000),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_atomic_u32_offset(3, 3, 0),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn shared_atomic_handle_uses_release_store_and_acquire_load() {
        let atomic = AtomicU32::new(7);
        let address = NonNull::from(&atomic);
        assert_eq!(atomic_load_acquire(address), 7);
        atomic_store_release(address, 29);
        assert_eq!(atomic.load(Ordering::Acquire), 29);
    }

    #[test]
    fn futex_id_uses_published_length_without_taking_pages_lock() {
        let _context = crate::test_support::scheduler_test_context();
        let pages = Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap());
        let backend = SharedBackend {
            start: VirtAddr::from(0x4000),
            page_offset: 0,
            pages: pages.clone(),
            may_protect: access_flags(),
            map_id: SharedMapId::Dynamic(Arc::new(())),
            status: MappingStatus::default(),
        };
        // Holding the vector mutex proves that the gate-safe identity query
        // does not fall back to `total_bytes()` and block on the same lock.
        let pages_guard = pages.phys_pages.lock();
        assert_eq!(backend.futex_id(0x4000), None);
        drop(pages_guard);
        // SharedPages teardown needs task context in host tests; this test is
        // only about the nonblocking identity query.
        core::mem::forget(backend);
        core::mem::forget(pages);
    }

    #[test]
    fn low_address_shared_suffix_preserves_page_and_futex_cursors() {
        let origin = VirtAddr::from(0x4000);
        let source = VirtAddr::from(0x8000);
        let destination = VirtAddr::from(0x1000);
        let pages = Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap());
        let map_id = SharedMapId::Dynamic(Arc::new(()));
        let backend = SharedBackend {
            start: origin,
            page_offset: 0,
            pages: pages.clone(),
            may_protect: access_flags(),
            map_id: map_id.clone(),
            status: MappingStatus::default(),
        };
        // Keep one zero-page owner alive: dropping the final kernel mutex has
        // no current task in host tests.
        core::mem::forget(pages);

        let first = backend
            .clone_for_range_with_id(source, destination, map_id.clone())
            .unwrap();
        let second_fragment = backend
            .clone_for_range_with_id(source, destination, map_id)
            .unwrap();

        assert_eq!(first.start, destination);
        assert_eq!(first.page_offset, 4);
        assert_eq!(first.backing_offset(destination.as_usize()), Some(0x4000));
        assert_eq!(
            first.backing_offset((destination + 0x1000).as_usize()),
            Some(0x5000)
        );
        assert!(first.compatible_with(&second_fragment));
    }
}
