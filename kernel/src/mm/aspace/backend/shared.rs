use alloc::{sync::Arc, vec::Vec};
use core::{
    any::Any,
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize, PageTable, PageTableCursor, PagingError};
use axsync::Mutex;
use memory_addr::{PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, Backend, BackendOps, BackendRetirement, FutexBackingId, FutexBackingIdentity,
    FutexWordOffset, MappingStatus, PopulateOutcome, SharedFutexKey, alloc_frame, dealloc_frame,
    divide_page, page_table_flags, pages_in, preflight_sparse_unmap,
};
use crate::{
    file::{DeferredFileLease, FileHandle, FileLike, FileMmapProtection, PreparedFileMmap},
    mm::{FileLikeMappingLease, FileMappingSharing},
};

static FIXED_SHARED_MAPPING_ID: AtomicU64 = AtomicU64::new(1);

pub struct SharedPages {
    phys_pages: Mutex<Vec<PhysAddr>>,
    // `futex_id` is queried while an IRQ-safe futex queue gate is held. Keep
    // the published length separate from `phys_pages`: taking that mutex in
    // the gate would be a blocking operation. The backing only grows, so a
    // stale (smaller) snapshot can cause a retry but can never expose a word
    // beyond the live allocation.
    published_len: AtomicUsize,
    pub size: PageSize,
    fixed: bool,
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

    fn new_with_growth(size: usize, page_size: PageSize, growable: bool) -> AxResult<Self> {
        if !page_size.is_aligned(size) {
            return Err(AxError::InvalidInput);
        }
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
        Ok(Self {
            phys_pages: Mutex::new(phys_pages),
            published_len: AtomicUsize::new(num_pages),
            size: page_size,
            fixed: !growable,
        })
    }

    pub const fn page_size(&self) -> PageSize {
        self.size
    }

    pub const fn is_fixed(&self) -> bool {
        self.fixed
    }

    pub fn len(&self) -> usize {
        self.phys_pages.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.phys_pages.lock().is_empty()
    }

    pub fn ensure_len(&self, len: usize) -> AxResult {
        let current_len = self.phys_pages.lock().len();
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
        if pages.len() >= len {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Ok(());
        }
        let needed = len - pages.len();
        if pages.try_reserve_exact(needed).is_err() {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Err(AxError::NoMemory);
        }
        let unused = new_pages.split_off(needed);
        pages.extend(new_pages);
        let published_len = pages.len();
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
            let phys = pages[page_index];
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
            let phys = pages[page_index];
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
        if end > pages.len() {
            return Err(AxError::NoMemory);
        }
        use_pages(&pages[start_index..end])
    }

    fn physical_page(&self, index: usize) -> AxResult<PhysAddr> {
        self.phys_pages
            .lock()
            .get(index)
            .copied()
            .ok_or(AxError::InvalidInput)
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
        for frame in self.phys_pages.lock().iter() {
            dealloc_frame(*frame, self.size);
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
        for vaddr in pages_in(range, self.pages.size)? {
            match pt.unmap(vaddr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(BackendRetirement::empty())
    }

    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult {
        preflight_sparse_unmap(range, self.pages.size, pt)
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
    ) -> AxResult<Backend> {
        Ok(Backend::Shared(self.clone()))
    }
}

impl Backend {
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
    owner: DeferredFileLease,
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
        let retained: Arc<dyn Any + Send + Sync> = pages.clone();
        let owner = DeferredFileLease::try_new(handle, retained)?;
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
        let mapping = FileLikeMappingLease::new(
            owner,
            ofd_key,
            start,
            object_offset,
            initial_flags,
            may_protect,
            FileMappingSharing::Shared,
        );
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
