use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ops::Range,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::{
    CachedFile, CachedFileEvictionOwner, CachedFileIdentity, CachedFilePagePin,
    CachedFilePinWindow, EvictedPage, FileFlags,
};
use axhal::paging::{MappingFlags, PageSize, PageTable, PageTableCursor, PagingError};
use axsync::Mutex;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, Backend, BackendOps, BackendRetirement, FutexBackingId, FutexBackingIdentity,
    FutexWordOffset, MappingStatus, PopulateCallback, PopulateOutcome, page_table_flags, pages_in,
    preflight_sparse_unmap,
};
use crate::file::{executable, memfd};

type FileFutexKey = CachedFileIdentity;

#[cfg(not(test))]
type FileFutexHandlesMutex<T> = Mutex<T>;
#[cfg(test)]
type FileFutexHandlesMutex<T> = spin::Mutex<T>;

static FILE_FUTEX_HANDLES: FileFutexHandlesMutex<BTreeMap<FileFutexKey, Weak<FileFutexIdentity>>> =
    FileFutexHandlesMutex::new(BTreeMap::new());
const REGISTERING_LISTENER: usize = usize::MAX;
const WRITABLE_SEGMENTS_TRANSITIONING: usize = usize::MAX;

#[cfg(test)]
const DEFERRED_EVICTION_FAIL_RESERVE: u8 = 1;
#[cfg(test)]
const DEFERRED_EVICTION_FAIL_STATE: u8 = 2;
#[cfg(test)]
const DEFERRED_EVICTION_FAIL_CALLBACK: u8 = 3;
#[cfg(test)]
static DEFERRED_EVICTION_PREPARE_FAILPOINT: AtomicU8 = AtomicU8::new(0);

pub(crate) struct FileFutexIdentity {
    key: FileFutexKey,
    /// Keep the real cache alive for the complete futex identity lease.  The
    /// old implementation retained only a marker `Arc<()>`, which allowed a
    /// closed/reopened inode to reuse the same numeric key while waiters from
    /// the old mapping were still queued.
    cache: CachedFile,
}

struct PreparedPopulateEvictions {
    state: Arc<spin::Mutex<Vec<EvictedPage>>>,
    callback: PopulateCallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FilePopulatePlan {
    page_count: usize,
    missing_count: usize,
    already_accessible: bool,
}

fn inspect_file_populate(
    range: VirtAddrRange,
    access_flags: MappingFlags,
    mut query: impl FnMut(VirtAddr) -> Result<(MappingFlags, PageSize), PagingError>,
) -> AxResult<FilePopulatePlan> {
    let mut page_count = 0usize;
    let mut missing_count = 0usize;
    let mut already_accessible = true;

    for addr in pages_in(range, PageSize::Size4K)? {
        page_count = page_count.checked_add(1).ok_or(AxError::InvalidInput)?;
        match query(addr) {
            Ok((page_flags, page_size)) => {
                if page_size != PageSize::Size4K {
                    return Err(AxError::BadAddress);
                }
                if !page_flags.contains(access_flags) {
                    already_accessible = false;
                }
            }
            Err(PagingError::NotMapped) => {
                missing_count = missing_count.checked_add(1).ok_or(AxError::InvalidInput)?;
                already_accessible = false;
            }
            Err(_) => return Err(AxError::BadAddress),
        }
    }

    Ok(FilePopulatePlan {
        page_count,
        missing_count,
        already_accessible,
    })
}

impl PreparedPopulateEvictions {
    fn try_new(inner: Arc<FileBackendInner>, page_count: usize) -> AxResult<Self> {
        #[cfg(test)]
        if take_deferred_eviction_prepare_failpoint(DEFERRED_EVICTION_FAIL_RESERVE) {
            return Err(AxError::NoMemory);
        }

        let mut evictions = Vec::new();
        evictions
            .try_reserve_exact(page_count)
            .map_err(|_| AxError::NoMemory)?;

        #[cfg(test)]
        if take_deferred_eviction_prepare_failpoint(DEFERRED_EVICTION_FAIL_STATE) {
            return Err(AxError::NoMemory);
        }

        let state = Arc::try_new(spin::Mutex::new(evictions)).map_err(|_| AxError::NoMemory)?;
        let callback_state = state.clone();

        #[cfg(test)]
        if take_deferred_eviction_prepare_failpoint(DEFERRED_EVICTION_FAIL_CALLBACK) {
            return Err(AxError::NoMemory);
        }

        let callback: PopulateCallback = Box::try_new(move |aspace: &mut AddrSpace| {
            loop {
                let pn = {
                    let evictions = callback_state.lock();
                    evictions.last().map(EvictedPage::page_number)
                };
                let Some(pn) = pn else {
                    break;
                };
                assert!(
                    inner.on_evict_from_locked_aspace(pn, aspace),
                    "failed to detach aliases for deferred cache eviction"
                );
                // The old cache frame remains owned until every PTE in this
                // address space has been detached above.
                let evicted = callback_state
                    .lock()
                    .pop()
                    .expect("deferred eviction disappeared during cleanup");
                drop(evicted);
            }
        })
        .map_err(|_| AxError::NoMemory)?;

        Ok(Self { state, callback })
    }

    fn push(&self, evicted: EvictedPage) {
        let mut evictions = self.state.lock();
        debug_assert!(
            evictions.len() < evictions.capacity(),
            "deferred eviction count exceeded the populated page bound"
        );
        // Capacity for one possible eviction per populated page was reserved
        // before cache mutation, so this push cannot allocate.
        evictions.push(evicted);
    }

    fn into_callback(self) -> Option<PopulateCallback> {
        let Self { state, callback } = self;
        let has_evictions = !state.lock().is_empty();
        drop(state);
        has_evictions.then_some(callback)
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.state.lock().capacity()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.state.lock().is_empty()
    }
}

#[cfg(test)]
fn take_deferred_eviction_prepare_failpoint(stage: u8) -> bool {
    DEFERRED_EVICTION_PREPARE_FAILPOINT
        .compare_exchange(stage, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

#[derive(Clone, Copy)]
struct WritableMappingActivation {
    memfd: bool,
    executable: bool,
    swap: bool,
}

fn activate_writable_mapping(
    executable_mapping: Option<&executable::WritableMappingRegistration>,
    writable_mapping: Option<&Arc<memfd::WritableMappingRegistration>>,
    swap_mapping: Option<&crate::mm::WritableMappingRegistration>,
) -> AxResult<WritableMappingActivation> {
    let was_memfd_mapping_active =
        writable_mapping.is_some_and(|registration| registration.is_active());
    let was_executable_mapping_active =
        executable_mapping.is_some_and(executable::WritableMappingRegistration::is_active);
    let was_swap_mapping_active = swap_mapping.is_some_and(crate::mm::WritableMappingRegistration::is_active);
    if let Some(registration) = swap_mapping { registration.set_active(true)?; }
    if let Some(registration) = writable_mapping {
        // This is the shared linearization point with F_ADD_SEALS. Reserve it
        // before executable admission so a sealed mapping publishes nothing.
        registration.set_active(true)?;
    }
    if let Some(registration) = executable_mapping
        && let Err(error) = registration.set_active(true)
    {
        if !was_memfd_mapping_active && let Some(registration) = writable_mapping {
            let _ = registration.set_active(false);
        }
        if !was_swap_mapping_active && let Some(registration) = swap_mapping { let _ = registration.set_active(false); }
        return Err(error);
    }

    Ok(WritableMappingActivation {
        memfd: writable_mapping.is_some() && !was_memfd_mapping_active,
        executable: executable_mapping.is_some() && !was_executable_mapping_active,
        swap: swap_mapping.is_some() && !was_swap_mapping_active,
    })
}

fn deactivate_writable_mapping(
    executable_mapping: Option<&executable::WritableMappingRegistration>,
    writable_mapping: Option<&Arc<memfd::WritableMappingRegistration>>,
    swap_mapping: Option<&crate::mm::WritableMappingRegistration>,
) -> AxResult<()> {
    if let Some(registration) = executable_mapping {
        registration.set_active(false)?;
    }
    if let Some(registration) = writable_mapping {
        registration.set_active(false)?;
    }
    if let Some(registration) = swap_mapping { registration.set_active(false)?; }
    Ok(())
}

fn rollback_writable_mapping_activation(
    executable_mapping: Option<&executable::WritableMappingRegistration>,
    writable_mapping: Option<&Arc<memfd::WritableMappingRegistration>>,
    swap_mapping: Option<&crate::mm::WritableMappingRegistration>,
    activation: WritableMappingActivation,
) -> AxResult<()> {
    if activation.executable
        && let Some(registration) = executable_mapping
    {
        registration.set_active(false)?;
    }
    if activation.memfd
        && let Some(registration) = writable_mapping
    {
        registration.set_active(false)?;
    }
    if activation.swap && let Some(registration) = swap_mapping { registration.set_active(false)?; }
    Ok(())
}

impl Drop for FileFutexIdentity {
    fn drop(&mut self) {
        let mut handles = FILE_FUTEX_HANDLES.lock();
        if handles
            .get(&self.key)
            .is_some_and(|weak| core::ptr::eq(weak.as_ptr(), self))
        {
            handles.remove(&self.key);
        }
    }
}

fn file_futex_handle(cache: &CachedFile) -> Arc<FileFutexIdentity> {
    let key = cache.identity();
    let mut handles = FILE_FUTEX_HANDLES.lock();
    if let Some(handle) = handles.get(&key).and_then(Weak::upgrade) {
        return handle;
    }

    let handle = Arc::new(FileFutexIdentity {
        key,
        cache: cache.clone(),
    });
    handles.insert(key, Arc::downgrade(&handle));
    handle
}

fn new_file_backend_inner(
    start: VirtAddr,
    cache: CachedFile,
    owner: CachedFileEvictionOwner,
    flags: FileFlags,
    offset_page: u32,
    file_end: Option<u64>,
    map_id: Arc<()>,
    futex_handle: Arc<FileFutexIdentity>,
) -> Arc<FileBackendInner> {
    let writable_mapping = memfd::new_writable_mapping_registration(cache.location());
    let executable_mapping =
        executable::WritableMappingRegistration::for_location(cache.location());
    let swap_mapping = crate::mm::WritableMappingRegistration::for_location(cache.location());
    Arc::new(FileBackendInner {
        start,
        cache,
        owner,
        flags,
        offset_page,
        file_end,
        handle: AtomicUsize::new(0),
        map_id,
        futex_handle,
        writable_segments: AtomicUsize::new(0),
        writable_mapping,
        executable_mapping,
        swap_mapping,
    })
}

fn eviction_owner(aspace: &Arc<Mutex<AddrSpace>>) -> CachedFileEvictionOwner {
    CachedFileEvictionOwner::new(Arc::as_ptr(aspace) as usize)
        .expect("address-space Arc pointers are nonzero")
}

fn advance_offset_page(offset_page: u32, backing_advance: usize) -> AxResult<u32> {
    if !backing_advance.is_multiple_of(PAGE_SIZE_4K) {
        return Err(AxError::InvalidInput);
    }
    let backing_pages =
        u32::try_from(backing_advance / PAGE_SIZE_4K).map_err(|_| AxError::InvalidInput)?;
    offset_page
        .checked_add(backing_pages)
        .ok_or(AxError::InvalidInput)
}

#[doc(hidden)]
pub struct FileBackendInner {
    start: VirtAddr,
    cache: CachedFile,
    owner: CachedFileEvictionOwner,
    flags: FileFlags,
    offset_page: u32,
    file_end: Option<u64>,
    handle: AtomicUsize,
    map_id: Arc<()>,
    futex_handle: Arc<FileFutexIdentity>,
    writable_segments: AtomicUsize,
    writable_mapping: Option<Arc<memfd::WritableMappingRegistration>>,
    executable_mapping: Option<executable::WritableMappingRegistration>,
    swap_mapping: Option<crate::mm::WritableMappingRegistration>,
}
impl Drop for FileBackendInner {
    fn drop(&mut self) {
        assert_eq!(
            self.writable_segments.load(Ordering::Acquire),
            0,
            "dropping file backend with active writable segments"
        );
        let handle = self.handle.load(Ordering::Acquire);
        if handle != 0 && handle != REGISTERING_LISTENER {
            unsafe {
                self.cache.remove_evict_listener(handle);
            }
        }
    }
}
impl FileBackendInner {
    fn transition_writable_mapping(&self, active: bool) -> AxResult<()> {
        if active {
            activate_writable_mapping(
                self.executable_mapping.as_ref(),
                self.writable_mapping.as_ref(),
                self.swap_mapping.as_ref(),
            )?;
            Ok(())
        } else {
            deactivate_writable_mapping(
                self.executable_mapping.as_ref(),
                self.writable_mapping.as_ref(),
                self.swap_mapping.as_ref(),
            )
        }
    }

    fn stable_writable_segments(&self) -> usize {
        loop {
            let current = self.writable_segments.load(Ordering::Acquire);
            if current != WRITABLE_SEGMENTS_TRANSITIONING {
                return current;
            }
            core::hint::spin_loop();
        }
    }

    fn acquire_writable_segment(&self) -> AxResult<()> {
        loop {
            let current = self.writable_segments.load(Ordering::Acquire);
            if current == WRITABLE_SEGMENTS_TRANSITIONING {
                core::hint::spin_loop();
                continue;
            }
            if current == 0 {
                if self
                    .writable_segments
                    .compare_exchange(
                        0,
                        WRITABLE_SEGMENTS_TRANSITIONING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }
                match self.transition_writable_mapping(true) {
                    Ok(()) => {
                        self.writable_segments.store(1, Ordering::Release);
                        return Ok(());
                    }
                    Err(error) => {
                        self.writable_segments.store(0, Ordering::Release);
                        return Err(error);
                    }
                }
            }
            let next = current
                .checked_add(1)
                .filter(|next| *next != WRITABLE_SEGMENTS_TRANSITIONING)
                .ok_or(AxError::NoMemory)?;
            if self
                .writable_segments
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// Retains an already-active segment for infallible `Backend::clone()`.
    /// It never performs a 0->1 transition, so splitting cannot newly fail.
    fn retain_writable_segment(&self) -> bool {
        loop {
            let current = self.writable_segments.load(Ordering::Acquire);
            if current == WRITABLE_SEGMENTS_TRANSITIONING {
                core::hint::spin_loop();
                continue;
            }
            if current == 0 {
                return false;
            }
            let next = current
                .checked_add(1)
                .filter(|next| *next != WRITABLE_SEGMENTS_TRANSITIONING)
                .expect("file writable-segment count overflow");
            if self
                .writable_segments
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn release_writable_segment(&self) -> AxResult<()> {
        loop {
            let current = self.writable_segments.load(Ordering::Acquire);
            if current == WRITABLE_SEGMENTS_TRANSITIONING {
                core::hint::spin_loop();
                continue;
            }
            assert!(current != 0, "file writable-segment count underflow");
            if current > 1 {
                if self
                    .writable_segments
                    .compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(());
                }
                continue;
            }
            if self
                .writable_segments
                .compare_exchange(
                    1,
                    WRITABLE_SEGMENTS_TRANSITIONING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            match self.transition_writable_mapping(false) {
                Ok(()) => {
                    self.writable_segments.store(0, Ordering::Release);
                    return Ok(());
                }
                Err(error) => {
                    self.writable_segments.store(1, Ordering::Release);
                    return Err(error);
                }
            }
        }
    }

    pub fn register_listener(self: &Arc<Self>, aspace: &Arc<Mutex<AddrSpace>>) -> AxResult {
        debug_assert_eq!(self.owner, eviction_owner(aspace));
        if self
            .handle
            .compare_exchange(0, REGISTERING_LISTENER, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AxError::AlreadyExists);
        }
        let aspace = Arc::downgrade(aspace);
        let handle = self.cache.add_evict_listener(self.owner, {
            let this = Arc::downgrade(self);
            move |pn, _page| {
                let Some(this) = this.upgrade() else {
                    return true;
                };
                let Some(aspace) = aspace.upgrade() else {
                    // The address space has been dropped, nothing to do.
                    return true;
                };
                let Some(mut aspace) = aspace.try_lock() else {
                    // This can happen during the populate process, when new pages
                    // are being populated and old pages are being evicted. In this
                    // case, we delegate the unmapping to the populate process.
                    return false;
                };
                this.on_evict(pn, &mut aspace)
            }
        });
        self.handle.store(handle, Ordering::Release);
        Ok(())
    }

    fn on_evict(self: &Arc<Self>, pn: u32, aspace: &mut AddrSpace) -> bool {
        let Some(pn) = pn.checked_sub(self.offset_page) else {
            return true;
        };
        let vaddr = self.start + pn as usize * PageSize::Size4K as usize;
        if !aspace.find_area(vaddr).is_some_and(
            |it| matches!(it.backend(), Backend::File(file) if Arc::ptr_eq(&file.0, self)),
        ) {
            // Ignore if the page is not controlled by this file mapping.
            return true;
        }

        // A cache eviction must never turn an mlocked mapping into a missing
        // PTE.  Returning false keeps the cache page resident; PAGEOUT then
        // treats this page as advisory work it could not reclaim.
        if aspace.range_is_locked(vaddr, PageSize::Size4K as usize) {
            return false;
        }

        // An alias-preserving COLLAPSE may have installed one PDE over these
        // cache pages.  Cache eviction still has 4 KiB ownership, so expand
        // that PDE before detaching this one cache-page alias.
        if aspace
            .page_table()
            .query(vaddr)
            .is_ok_and(|(_, _, size)| size == PageSize::Size2M)
            && aspace
                .demote_alias_preserving_2m(VirtAddr::from(
                    vaddr.as_usize() & !(super::super::COLLAPSE_2M_SIZE - 1),
                ))
                .is_err()
        {
            return false;
        }

        let result = aspace.page_table_mut().cursor().unmap(vaddr);
        match result {
            Ok(_) => {
                drop(crate::mm::synchronize_tlb());
                true
            }
            Err(PagingError::NotMapped) => true,
            Err(err) => {
                warn!("Failed to unmap page {vaddr:?}: {err:?}");
                false
            }
        }
    }

    fn on_evict_from_locked_aspace(self: &Arc<Self>, pn: u32, aspace: &mut AddrSpace) -> bool {
        // Every listener associated with this address space failed try_lock()
        // while populate owned the lock. Walk all mappings of the same cache,
        // including aliases, before releasing the evicted cache page.
        let mut cursor = aspace.base();
        let mut detached = true;
        loop {
            let next = aspace
                .areas
                .iter()
                .find(|area| area.end() > cursor)
                .map(|area| {
                    let inner = match area.backend() {
                        Backend::File(file)
                            if file.0.owner == self.owner && file.0.cache.ptr_eq(&self.cache) =>
                        {
                            Some(file.0.clone())
                        }
                        _ => None,
                    };
                    (area.end(), inner)
                });
            let Some((next_cursor, inner)) = next else {
                break;
            };
            cursor = next_cursor;
            if let Some(inner) = inner {
                detached &= inner.on_evict(pn, aspace);
            }
        }
        detached
    }
}

/// Pre-commit registration for one new shared-writable VMA grant.
///
/// This owns only registrations which were inactive at admission. A successful
/// VMA commit explicitly hands them to the backend's writable-segment count;
/// failure refunds them unless a partial page-table transition must retain the
/// exclusion fail-closed.
#[must_use = "writable mapping admission must be completed or rolled back"]
pub(crate) struct WritableMappingAdmission {
    inner: Arc<FileBackendInner>,
    activation: Option<WritableMappingActivation>,
}

impl WritableMappingAdmission {
    fn begin(backend: &FileBackend) -> AxResult<Self> {
        let activation = activate_writable_mapping(
            backend.0.executable_mapping.as_ref(),
            backend.0.writable_mapping.as_ref(),
            backend.0.swap_mapping.as_ref(),
        )?;
        Ok(Self {
            inner: backend.0.clone(),
            activation: Some(activation),
        })
    }

    pub(crate) fn complete(mut self) -> AxResult<()> {
        if self.inner.stable_writable_segments() == 0 {
            return Err(AxError::BadState);
        }
        self.activation = None;
        Ok(())
    }
}

impl Drop for WritableMappingAdmission {
    fn drop(&mut self) {
        let Some(activation) = self.activation.take() else {
            return;
        };
        if self.inner.stable_writable_segments() != 0 {
            // A partially failed PTE transition owns the registrations now and
            // must retain them fail-closed.
            return;
        }
        if let Err(error) = rollback_writable_mapping_activation(
            self.inner.executable_mapping.as_ref(),
            self.inner.writable_mapping.as_ref(),
            self.inner.swap_mapping.as_ref(),
            activation,
        ) {
            error!(
                "failed to roll back writable-mapping admission: {error}; retaining exclusion \
                 fail-closed"
            );
        }
    }
}

/// File-backed mapping backend.
const SEGMENT_INACTIVE: u8 = 0;
const SEGMENT_TRANSITIONING: u8 = 1;
const SEGMENT_ACTIVE: u8 = 2;
const SEGMENT_FAIL_CLOSED: u8 = 3;

pub struct FileBackend(Arc<FileBackendInner>, AtomicU8, MappingStatus);

impl Clone for FileBackend {
    fn clone(&self) -> Self {
        loop {
            match self.1.load(Ordering::Acquire) {
                SEGMENT_INACTIVE => {
                    return Self(
                        self.0.clone(),
                        AtomicU8::new(SEGMENT_INACTIVE),
                        self.2.clone(),
                    );
                }
                SEGMENT_TRANSITIONING => core::hint::spin_loop(),
                SEGMENT_ACTIVE => {
                    if self.0.retain_writable_segment() {
                        return Self(
                            self.0.clone(),
                            AtomicU8::new(SEGMENT_ACTIVE),
                            self.2.clone(),
                        );
                    }
                    // A concurrent deactivation may have completed after the
                    // state load but before the retain. Re-read the handle and
                    // clone the state at a valid linearization point.
                    continue;
                }
                SEGMENT_FAIL_CLOSED => {
                    assert!(
                        self.0.retain_writable_segment(),
                        "fail-closed file segment lost its inner ownership"
                    );
                    return Self(
                        self.0.clone(),
                        AtomicU8::new(SEGMENT_ACTIVE),
                        self.2.clone(),
                    );
                }
                _ => panic!("invalid file writable-segment state"),
            }
        }
    }
}

impl Drop for FileBackend {
    fn drop(&mut self) {
        loop {
            match self.1.load(Ordering::Acquire) {
                SEGMENT_INACTIVE => return,
                SEGMENT_TRANSITIONING => core::hint::spin_loop(),
                SEGMENT_ACTIVE => {
                    if self
                        .1
                        .compare_exchange(
                            SEGMENT_ACTIVE,
                            SEGMENT_TRANSITIONING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    if let Err(error) = self.0.release_writable_segment() {
                        error!(
                            "failed to release file writable segment: {error}; retaining its \
                             exclusion fail-closed"
                        );
                        self.1.store(SEGMENT_FAIL_CLOSED, Ordering::Release);
                        core::mem::forget(self.0.clone());
                        return;
                    }
                    self.1.store(SEGMENT_INACTIVE, Ordering::Release);
                    return;
                }
                SEGMENT_FAIL_CLOSED => {
                    // A failed page-table mutation may have exposed writable
                    // PTEs after memory_set already detached the temporary
                    // backend. Keep the inner and its registrations alive
                    // permanently rather than creating an untracked writer.
                    core::mem::forget(self.0.clone());
                    return;
                }
                _ => panic!("invalid file writable-segment state"),
            }
        }
    }
}

impl FileBackend {
    pub(super) const fn mapping_status(&self) -> &MappingStatus {
        &self.2
    }

    pub(super) fn mapping_status_mut(&mut self) -> &mut MappingStatus {
        &mut self.2
    }

    pub(crate) fn location(&self) -> &axfs_ng_vfs::Location {
        self.0.cache.location()
    }

    pub(crate) fn begin_writable_mapping_admission(&self) -> AxResult<WritableMappingAdmission> {
        WritableMappingAdmission::begin(self)
    }

    fn inactive(inner: Arc<FileBackendInner>) -> Self {
        Self::inactive_with_status(inner, MappingStatus::default())
    }

    fn inactive_with_status(inner: Arc<FileBackendInner>, status: MappingStatus) -> Self {
        Self(inner, AtomicU8::new(SEGMENT_INACTIVE), status)
    }

    fn writable_segment_active(&self) -> bool {
        loop {
            match self.1.load(Ordering::Acquire) {
                SEGMENT_INACTIVE => return false,
                SEGMENT_TRANSITIONING => core::hint::spin_loop(),
                SEGMENT_ACTIVE | SEGMENT_FAIL_CLOSED => return true,
                _ => panic!("invalid file writable-segment state"),
            }
        }
    }

    fn activate_writable_segment(&self) -> AxResult<()> {
        loop {
            match self.1.load(Ordering::Acquire) {
                SEGMENT_ACTIVE => return Ok(()),
                SEGMENT_FAIL_CLOSED => return Ok(()),
                SEGMENT_TRANSITIONING => core::hint::spin_loop(),
                SEGMENT_INACTIVE => {
                    if self
                        .1
                        .compare_exchange(
                            SEGMENT_INACTIVE,
                            SEGMENT_TRANSITIONING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    match self.0.acquire_writable_segment() {
                        Ok(()) => {
                            self.1.store(SEGMENT_ACTIVE, Ordering::Release);
                            return Ok(());
                        }
                        Err(error) => {
                            self.1.store(SEGMENT_INACTIVE, Ordering::Release);
                            return Err(error);
                        }
                    }
                }
                _ => panic!("invalid file writable-segment state"),
            }
        }
    }

    fn deactivate_writable_segment(&self) -> AxResult<()> {
        loop {
            match self.1.load(Ordering::Acquire) {
                SEGMENT_INACTIVE => return Ok(()),
                SEGMENT_TRANSITIONING => core::hint::spin_loop(),
                SEGMENT_ACTIVE => {
                    if self
                        .1
                        .compare_exchange(
                            SEGMENT_ACTIVE,
                            SEGMENT_TRANSITIONING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    match self.0.release_writable_segment() {
                        Ok(()) => {
                            self.1.store(SEGMENT_INACTIVE, Ordering::Release);
                            return Ok(());
                        }
                        Err(error) => {
                            self.1.store(SEGMENT_ACTIVE, Ordering::Release);
                            return Err(error);
                        }
                    }
                }
                SEGMENT_FAIL_CLOSED => return Err(AxError::BadState),
                _ => panic!("invalid file writable-segment state"),
            }
        }
    }

    fn retain_writable_exclusion_fail_closed(&self) {
        loop {
            match self.1.load(Ordering::Acquire) {
                SEGMENT_TRANSITIONING => core::hint::spin_loop(),
                SEGMENT_ACTIVE => {
                    if self
                        .1
                        .compare_exchange(
                            SEGMENT_ACTIVE,
                            SEGMENT_FAIL_CLOSED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                SEGMENT_FAIL_CLOSED => return,
                SEGMENT_INACTIVE => {
                    panic!("writable page-table failure without an active file exclusion")
                }
                _ => panic!("invalid file writable-segment state"),
            }
        }
    }
    fn page_number_for(&self, vaddr: VirtAddr) -> AxResult<u32> {
        let delta = vaddr
            .as_usize()
            .checked_sub(self.0.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        if !delta.is_multiple_of(PAGE_SIZE_4K) {
            return Err(AxError::InvalidInput);
        }
        u32::try_from(delta / PAGE_SIZE_4K)
            .ok()
            .and_then(|delta| self.0.offset_page.checked_add(delta))
            .ok_or(AxError::InvalidInput)
    }

    fn validate_range(&self, range: VirtAddrRange) -> AxResult {
        pages_in(range, PageSize::Size4K)?;
        if !range.is_empty() {
            self.page_number_for(range.end - PAGE_SIZE_4K)?;
        }
        Ok(())
    }

    fn cache_page_range(&self, range: VirtAddrRange) -> AxResult<Range<u32>> {
        self.validate_range(range)?;
        if range.is_empty() {
            return Ok(0..0);
        }
        let start = self.page_number_for(range.start)?;
        let count =
            u32::try_from(range.size() / PAGE_SIZE_4K).map_err(|_| AxError::InvalidInput)?;
        let end = start.checked_add(count).ok_or(AxError::InvalidInput)?;
        Ok(start..end)
    }

    pub(crate) fn check_flags(&self, flags: MappingFlags) -> AxResult {
        let mut required_flags = FileFlags::empty();
        if flags.contains(MappingFlags::READ) {
            required_flags |= FileFlags::READ;
        }
        if flags.contains(MappingFlags::WRITE) {
            required_flags |= FileFlags::WRITE;
        }

        if !self.0.flags.contains(required_flags) {
            return Err(AxError::PermissionDenied);
        }
        if flags.contains(MappingFlags::WRITE) {
            memfd::check_writable_shared_mapping(self.0.cache.location())?;
        }
        Ok(())
    }

    /// Publishes a writable-mapping exclusion before granting PTE write
    /// access, and revokes it only after write access has been removed. This
    /// ordering also keeps a failed page-table update from losing the token.
    pub(crate) fn protect_range(
        &self,
        range: VirtAddrRange,
        new_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult<()> {
        self.check_flags(new_flags)?;
        let writable = new_flags.contains(MappingFlags::WRITE);

        if writable {
            self.activate_writable_segment()?;
        }
        // `protect_region` is not promised to be failure-atomic. If granting
        // write access fails after touching a prefix, retain the token with the
        // backend; if removing write access fails, likewise do not release it.
        if let Err(error) =
            pt.protect_region(range.start, range.size(), page_table_flags(new_flags))
        {
            if self.writable_segment_active() {
                self.retain_writable_exclusion_fail_closed();
            }
            return Err(error.into());
        }
        if !writable {
            pt.flush();
            drop(crate::mm::synchronize_tlb());
            self.deactivate_writable_segment()?;
        }
        Ok(())
    }

    pub(crate) fn futex_backing_identity(&self) -> FutexBackingIdentity {
        FutexBackingIdentity::File(self.0.futex_handle.clone())
    }

    pub(crate) fn futex_key(
        &self,
        address: usize,
    ) -> Option<(FutexBackingIdentity, FutexWordOffset)> {
        let relative = address.checked_sub(self.0.start.as_usize())?;
        let offset = (self.0.offset_page as usize)
            .checked_mul(PAGE_SIZE_4K)?
            .checked_add(relative)?;
        Some((self.futex_backing_identity(), FutexWordOffset::new(offset)))
    }

    pub(crate) fn futex_id(&self, address: usize) -> Option<(FutexBackingId, FutexWordOffset)> {
        let relative = address.checked_sub(self.0.start.as_usize())?;
        let offset = (self.0.offset_page as usize)
            .checked_mul(PAGE_SIZE_4K)?
            .checked_add(relative)?;
        Some((
            FutexBackingId::file(Arc::as_ptr(&self.0.futex_handle) as usize),
            FutexWordOffset::new(offset),
        ))
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0.map_id, &other.0.map_id)
            && self.0.start == other.0.start
            && self.0.offset_page == other.0.offset_page
            && self.0.file_end == other.0.file_end
            && self.0.flags.bits() == other.0.flags.bits()
            && self.0.cache.ptr_eq(&other.0.cache)
    }

    pub(crate) fn faults_with_sigbus(&self, vaddr: VirtAddr) -> bool {
        let Some(file_end) = self.0.file_end else {
            return false;
        };
        let page_start = vaddr.align_down_4k();
        let page_delta = page_start
            .as_usize()
            .saturating_sub(self.0.start.as_usize()) as u64;
        let file_offset = self.0.offset_page as u64 * PAGE_SIZE_4K as u64 + page_delta;
        let current_end = self.0.cache.location().len().unwrap_or(file_end);
        file_offset >= current_end
    }

    pub(crate) fn cached_page_resident(&self, vaddr: VirtAddr) -> bool {
        let page_start = vaddr.align_down_4k();
        if page_start < self.0.start {
            return false;
        }

        let Ok(pn) = self.page_number_for(page_start) else {
            return false;
        };

        let mut resident = false;
        self.0.cache.with_page(pn, |page| {
            resident = page.is_some();
        });
        resident
    }

    /// Demotes already resident file-cache pages without faulting them in.
    pub(crate) fn cold_pages(&self, range: VirtAddrRange) -> AxResult<usize> {
        Ok(self.0.cache.cold_pages(self.cache_page_range(range)?)?)
    }

    /// Writes back and evicts resident file-cache pages for this mapping.
    /// Missing, pinned, writeback, and in-memory pages are left in place by
    /// the cache layer, matching the advisory nature of pageout.
    pub(crate) fn pageout_pages(&self, range: VirtAddrRange) -> AxResult<usize> {
        Ok(self.0.cache.pageout_pages(self.cache_page_range(range)?)?)
    }

    /// Reads file-backed pages into the inode cache without installing PTEs.
    ///
    /// A caller holds `aspace`'s lock while this runs.  Cache replacement can
    /// therefore make this mapping's eviction listener defer PTE detachment;
    /// use the owner-aware cache insertion path and drain that deferred
    /// eviction before releasing its retained page.  The ordinary insertion
    /// path cannot do this: it would either reject the locked listener or
    /// release an evicted page before its aliases had been detached.
    pub(crate) fn prefetch(&self, range: VirtAddrRange, aspace: &mut AddrSpace) -> AxResult<usize> {
        self.validate_range(range)?;

        self.0.cache.with_direct_io_excluded(|| {
            let mut prefetched = 0usize;
            for vaddr in pages_in(range, PageSize::Size4K)? {
                // A mapping wholly beyond its current file size faults with
                // SIGBUS.  It has no backing page to bring into cache.
                if self.faults_with_sigbus(vaddr) {
                    continue;
                }
                let pn = self.page_number_for(vaddr)?;
                let evicted = self.0.cache.with_page_or_insert_for_owner(
                    pn,
                    self.0.owner,
                    |_, evicted| Ok(evicted),
                )?;
                if let Some(evicted) = evicted {
                    if let Some(owner) = evicted.deferred_owner() {
                        assert_eq!(owner, self.0.owner);
                        assert!(
                            self.0
                                .on_evict_from_locked_aspace(evicted.page_number(), aspace),
                            "failed to detach aliases for deferred cache eviction"
                        );
                    }
                    // Keep a deferred page alive until every alias was
                    // detached above; non-deferred pages were already
                    // acknowledged by their listeners.
                    drop(evicted);
                }
                prefetched = prefetched.checked_add(1).ok_or(AxError::InvalidInput)?;
            }
            Ok(prefetched)
        })
    }

    pub(crate) fn begin_user_io_pin_window(&self) -> AxResult<CachedFilePinWindow> {
        self.0.cache.begin_user_io_pin_window()
    }

    pub(crate) fn pin_user_io_page(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        dirty_on_release: bool,
    ) -> AxResult<CachedFilePagePin> {
        let page_start = vaddr.align_down_4k();
        if page_start < self.0.start {
            return Err(AxError::BadAddress);
        }

        let pn = self
            .page_number_for(page_start)
            .map_err(|_| AxError::BadAddress)?;

        self.0
            .cache
            .pin_cached_page_by_paddr(pn, paddr, dirty_on_release)
    }

    fn clone_for_range_with_id(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
        map_id: Arc<()>,
    ) -> AxResult<Self> {
        let (start, backing_advance) =
            super::relocate_affine_origin(self.0.start, old_start, new_start)?;
        let offset_page = advance_offset_page(self.0.offset_page, backing_advance)?;
        let inner = new_file_backend_inner(
            start,
            self.0.cache.clone(),
            eviction_owner(aspace),
            self.0.flags,
            offset_page,
            self.0.file_end,
            map_id,
            self.0.futex_handle.clone(),
        );
        inner.register_listener(aspace)?;
        let status = self.2.relocated(old_start, new_start)?;
        let backend = Self::inactive_with_status(inner, status);
        if self.writable_segment_active() {
            backend.activate_writable_segment()?;
        }
        Ok(backend)
    }

    pub(crate) fn clone_for_range(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Self> {
        self.clone_for_range_with_id(old_start, new_start, aspace, self.0.map_id.clone())
    }

    /// Clone the same cached file object at an explicit file-page cursor for
    /// `remap_file_pages`.  The new listener is registered before publication;
    /// mapping status is rebased so VMA ownership and futex/file offsets agree.
    pub(crate) fn clone_rebased(
        &self,
        start: VirtAddr,
        offset_page: u32,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Self> {
        let inner = new_file_backend_inner(
            start,
            self.0.cache.clone(),
            eviction_owner(aspace),
            self.0.flags,
            offset_page,
            self.0.file_end,
            self.0.map_id.clone(),
            self.0.futex_handle.clone(),
        );
        inner.register_listener(aspace)?;
        let byte_offset = u64::from(offset_page)
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(AxError::InvalidInput)?;
        let status = self.2.rebased_file_mapping(start, byte_offset)?;
        let backend = Self::inactive_with_status(inner, status);
        if self.writable_segment_active() {
            backend.activate_writable_segment()?;
        }
        Ok(backend)
    }

    pub(crate) fn duplicate_mapping(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Self> {
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        self.clone_for_range_with_id(old_start, new_start, aspace, map_id)
    }

    fn clone_map_with_registration(
        &self,
        flags: MappingFlags,
        owner: CachedFileEvictionOwner,
        register: impl FnOnce(&Arc<FileBackendInner>) -> AxResult,
    ) -> AxResult<Backend> {
        let inner = new_file_backend_inner(
            self.0.start,
            self.0.cache.clone(),
            owner,
            self.0.flags,
            self.0.offset_page,
            self.0.file_end,
            self.0.map_id.clone(),
            self.0.futex_handle.clone(),
        );
        register(&inner)?;
        let backend = FileBackend::inactive_with_status(inner, self.2.clone());
        if flags.contains(MappingFlags::WRITE) {
            backend.activate_writable_segment()?;
        }
        Ok(Backend::File(backend))
    }

    pub(crate) fn clone_materialized_pages(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        size: usize,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        let old_range =
            VirtAddrRange::try_from_start_size(old_start, size).ok_or(AxError::InvalidInput)?;
        let new_range =
            VirtAddrRange::try_from_start_size(new_start, size).ok_or(AxError::InvalidInput)?;
        let new_pages = pages_in(new_range, PageSize::Size4K)?;
        for (old_addr, new_addr) in pages_in(old_range, PageSize::Size4K)?.zip(new_pages) {
            match pt.query(old_addr) {
                Ok((paddr, flags, page_size)) => {
                    if page_size != PageSize::Size4K {
                        return Err(AxError::BadAddress);
                    }
                    pt.map(new_addr, paddr, PageSize::Size4K, page_table_flags(flags))?;
                }
                Err(PagingError::NotMapped) => {}
                Err(_) => return Err(AxError::BadAddress),
            }
        }
        Ok(())
    }

    pub(crate) fn sync(&self, data_only: bool) -> AxResult {
        self.0.cache.sync(data_only)?;
        Ok(())
    }
}

impl BackendOps for FileBackend {
    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        self.validate_range(range)?;
        self.check_flags(flags)?;
        if flags.contains(MappingFlags::WRITE) {
            self.activate_writable_segment()?;
        } else {
            self.deactivate_writable_segment()?;
        }
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult<BackendRetirement> {
        for addr in pages_in(range, PageSize::Size4K)? {
            match pt.unmap(addr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => {
                    warn!("Failed to unmap page {addr:?}: {err:?}");
                    if self.writable_segment_active() {
                        self.retain_writable_exclusion_fail_closed();
                    }
                    return Err(err.into());
                }
            }
        }
        Ok(BackendRetirement::empty())
    }

    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult {
        self.validate_range(range)?;
        preflight_sparse_unmap(range, PageSize::Size4K, pt)
    }

    fn preflight_protect(
        &self,
        range: VirtAddrRange,
        new_flags: MappingFlags,
        pt: &PageTable,
    ) -> AxResult {
        self.validate_range(range)?;
        self.check_flags(new_flags)?;
        preflight_sparse_unmap(range, PageSize::Size4K, pt)
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> PopulateOutcome {
        let (outcome, needs_tlb_sync) = self.0.cache.with_direct_io_excluded(|| {
            let plan = match inspect_file_populate(range, access_flags, |addr| {
                pt.query(addr)
                    .map(|(_, page_flags, page_size)| (page_flags, page_size))
            }) {
                Ok(plan) => plan,
                Err(error) => return (PopulateOutcome::immediate(Err(error)), false),
            };
            let start_page = match self.page_number_for(range.start) {
                Ok(start_page) => start_page,
                Err(error) => return (PopulateOutcome::immediate(Err(error)), false),
            };
            if let Some(last_index) = plan.page_count.checked_sub(1)
                && u32::try_from(last_index)
                    .ok()
                    .and_then(|last_index| start_page.checked_add(last_index))
                    .is_none()
            {
                return (
                    PopulateOutcome::immediate(Err(AxError::InvalidInput)),
                    false,
                );
            }
            if plan.already_accessible {
                return (PopulateOutcome::immediate(Ok(plan.page_count)), false);
            }
            let deferred_evictions = if plan.missing_count == 0 {
                None
            } else {
                match PreparedPopulateEvictions::try_new(self.0.clone(), plan.missing_count) {
                    Ok(evictions) => Some(evictions),
                    Err(error) => return (PopulateOutcome::immediate(Err(error)), false),
                }
            };

            let owner = self.0.owner;
            let mut needs_tlb_sync = false;
            let result = (|| {
                let mut pages = 0;
                for (i, addr) in pages_in(range, PageSize::Size4K)?.enumerate() {
                    let pn = u32::try_from(i)
                        .ok()
                        .and_then(|i| start_page.checked_add(i))
                        .ok_or(AxError::InvalidInput)?;
                    match pt.query(addr) {
                        Ok((paddr, page_flags, page_size)) => {
                            if page_size != PageSize::Size4K {
                                return Err(AxError::BadAddress);
                            }
                            if access_flags.contains(MappingFlags::WRITE)
                                && !page_flags.contains(MappingFlags::WRITE)
                            {
                                self.0.cache.with_page(pn, |page| {
                                    let page = page.ok_or(AxError::BadAddress)?;
                                    if page.paddr() != paddr {
                                        return Err(AxError::BadAddress);
                                    }
                                    page.mark_dirty();
                                    pt.remap(addr, paddr, page_table_flags(flags))?;
                                    needs_tlb_sync = true;
                                    pages += 1;
                                    AxResult::Ok(())
                                })?;
                            } else if page_flags.contains(access_flags) {
                                pages += 1;
                            }
                        }
                        // If the page is not mapped, try map it.
                        Err(PagingError::NotMapped) => {
                            let Some(deferred_evictions) = deferred_evictions.as_ref() else {
                                // The address-space lock makes page-table
                                // residency stable between inspection and
                                // mutation. Refuse an impossible late hole
                                // rather than insert without preallocated
                                // eviction ownership.
                                return Err(AxError::BadAddress);
                            };
                            let map_flags = flags - MappingFlags::WRITE;
                            self.0.cache.with_page_or_insert_for_owner(
                                pn,
                                owner,
                                |page, evicted| {
                                    if let Some(evicted) = evicted
                                        && let Some(deferred_owner) = evicted.deferred_owner()
                                    {
                                        assert_eq!(
                                            deferred_owner, owner,
                                            "foreign address-space owner deferred cache eviction"
                                        );
                                        deferred_evictions.push(evicted);
                                    }
                                    pt.map(
                                        addr,
                                        page.paddr(),
                                        PageSize::Size4K,
                                        page_table_flags(map_flags),
                                    )?;
                                    pages += 1;
                                    Ok(())
                                },
                            )?;
                        }
                        Err(_) => return Err(AxError::BadAddress),
                    }
                }
                Ok(pages)
            })();
            (
                PopulateOutcome::new(
                    result,
                    deferred_evictions.and_then(PreparedPopulateEvictions::into_callback),
                ),
                needs_tlb_sync,
            )
        });
        if needs_tlb_sync {
            pt.flush();
            drop(crate::mm::synchronize_tlb());
        }
        outcome
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        flags: MappingFlags,
        _old_pt: &mut PageTableCursor,
        _new_pt: &mut PageTableCursor,
        new_aspace: &Arc<Mutex<AddrSpace>>,
        _active_long_term_cow_frames: &[PhysAddr],
        _share_shadow_stack: bool,
    ) -> AxResult<Backend> {
        self.clone_map_with_registration(flags, eviction_owner(new_aspace), |inner| {
            inner.register_listener(new_aspace)
        })
    }
}

impl Backend {
    pub fn new_file(
        start: VirtAddr,
        cache: CachedFile,
        flags: FileFlags,
        offset: usize,
        file_end: Option<u64>,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Self> {
        let offset_page =
            u32::try_from(offset / PAGE_SIZE_4K).map_err(|_| AxError::InvalidInput)?;
        let futex_handle = file_futex_handle(&cache);
        let inner = new_file_backend_inner(
            start,
            cache,
            eviction_owner(aspace),
            flags,
            offset_page,
            file_end,
            Arc::new(()),
            futex_handle,
        );
        inner.register_listener(aspace)?;
        Ok(Self::File(FileBackend::inactive(inner)))
    }

    #[cfg(test)]
    pub(super) fn new_file_for_mapping_status_test(
        start: VirtAddr,
        cache: CachedFile,
        flags: FileFlags,
    ) -> Self {
        let futex_handle = file_futex_handle(&cache);
        Self::File(FileBackend::inactive(new_file_backend_inner(
            start,
            cache,
            CachedFileEvictionOwner::new(1).unwrap(),
            flags,
            0,
            None,
            Arc::new(()),
            futex_handle,
        )))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::AtomicBool;
    use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};

    use axfs_ng_vfs::{Location, Mountpoint, NodePermission, NodeType};
    use linux_raw_sys::general::F_SEAL_WRITE;
    use memory_set::{MappingBackend, MappingLineage, MemoryArea, MemorySet};

    use super::*;
    use crate::{
        file::{File as KernelFile, FileDescription, FileHandle, FileLike},
        mm::{FileMappingLease, FileMappingSharing},
        pseudofs::tmp::MemoryFs,
        task::UserNamespace,
    };

    static TEST_SERIAL: StdMutex<()> = StdMutex::new(());

    fn test_context() -> StdMutexGuard<'static, ()> {
        TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn test_location(name: &str) -> Location {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        mount
            .root_location()
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap()
    }

    fn test_backend(loc: &Location, map_id: Arc<()>) -> FileBackend {
        let cache = CachedFile::get_or_create(loc.clone());
        let futex_handle = file_futex_handle(&cache);
        FileBackend::inactive(new_file_backend_inner(
            VirtAddr::from(0x1000),
            cache,
            CachedFileEvictionOwner::new(1).unwrap(),
            FileFlags::READ | FileFlags::WRITE,
            0,
            None,
            map_id,
            futex_handle,
        ))
    }

    fn test_mapping_lease(loc: &Location) -> FileMappingLease {
        let description = FileDescription::new(Arc::new(KernelFile::new(axfs::File::new(
            axfs::FileBackend::Direct(loc.clone()),
            FileFlags::READ | FileFlags::WRITE,
        ))))
        .unwrap();
        let handle = FileHandle::<dyn FileLike>::from_description_for_test(description)
            .downcast::<KernelFile>()
            .unwrap();
        FileMappingLease::new(
            handle,
            UserNamespace::try_new_root().unwrap(),
            VirtAddr::from(0x1000),
            0,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            FileMappingSharing::Shared,
        )
    }

    #[test]
    fn resident_file_populate_plan_needs_no_eviction_preparation() {
        let range = VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x3000));
        let flags = MappingFlags::USER | MappingFlags::READ;
        let mut queries = 0;

        let plan = inspect_file_populate(range, MappingFlags::READ, |_| {
            queries += 1;
            Ok((flags, PageSize::Size4K))
        })
        .unwrap();

        assert_eq!(queries, 2);
        assert_eq!(
            plan,
            FilePopulatePlan {
                page_count: 2,
                missing_count: 0,
                already_accessible: true,
            }
        );
    }

    #[test]
    fn file_populate_plan_reserves_only_missing_page_evictions() {
        let range = VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x4000));
        let flags = MappingFlags::USER | MappingFlags::READ;

        let plan = inspect_file_populate(range, MappingFlags::WRITE, |addr| {
            if addr == VirtAddr::from(0x2000) {
                Err(PagingError::NotMapped)
            } else {
                Ok((flags, PageSize::Size4K))
            }
        })
        .unwrap();

        assert_eq!(
            plan,
            FilePopulatePlan {
                page_count: 3,
                missing_count: 1,
                already_accessible: false,
            }
        );
    }

    #[test]
    fn file_populate_plan_rejects_non_4k_entries_before_mutation() {
        let range = VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x2000));

        assert_eq!(
            inspect_file_populate(range, MappingFlags::READ, |_| {
                Ok((MappingFlags::READ, PageSize::Size2M))
            }),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn prefetch_populates_file_cache_without_installing_a_pte() {
        let _context = test_context();
        let location = test_location("prefetch-without-pte");
        let backend = test_backend(&location, Arc::new(()));
        let address = VirtAddr::from(0x1000);
        let range = VirtAddrRange::new(address, address + PAGE_SIZE_4K);
        let mut aspace = AddrSpace::new_empty(address, PAGE_SIZE_4K).unwrap();

        assert!(!backend.cached_page_resident(address));
        assert_eq!(backend.prefetch(range, &mut aspace), Ok(1));
        assert!(backend.cached_page_resident(address));
        assert!(matches!(
            aspace.page_table().query(address),
            Err(PagingError::NotMapped)
        ));
    }

    #[test]
    fn deferred_eviction_preparation_failpoints_leave_no_callback_owner() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("deferred-eviction-prepare-failpoints");
        let backend = test_backend(&loc, Arc::new(()));
        let baseline_owners = Arc::strong_count(&backend.0);

        for stage in [
            DEFERRED_EVICTION_FAIL_RESERVE,
            DEFERRED_EVICTION_FAIL_STATE,
            DEFERRED_EVICTION_FAIL_CALLBACK,
        ] {
            DEFERRED_EVICTION_PREPARE_FAILPOINT.store(stage, Ordering::Release);
            assert_eq!(
                PreparedPopulateEvictions::try_new(backend.0.clone(), 4).err(),
                Some(AxError::NoMemory)
            );
            assert_eq!(
                DEFERRED_EVICTION_PREPARE_FAILPOINT.load(Ordering::Acquire),
                0
            );
            assert_eq!(Arc::strong_count(&backend.0), baseline_owners);
        }
    }

    #[test]
    fn deferred_eviction_preparation_reserves_the_full_page_bound() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("deferred-eviction-page-bound");
        let backend = test_backend(&loc, Arc::new(()));
        let baseline_owners = Arc::strong_count(&backend.0);

        assert_eq!(
            PreparedPopulateEvictions::try_new(backend.0.clone(), usize::MAX).err(),
            Some(AxError::NoMemory)
        );
        assert_eq!(Arc::strong_count(&backend.0), baseline_owners);

        let prepared = PreparedPopulateEvictions::try_new(backend.0.clone(), 4).unwrap();
        assert!(prepared.capacity() >= 4);
        assert!(prepared.is_empty());
        assert!(prepared.into_callback().is_none());
        assert_eq!(Arc::strong_count(&backend.0), baseline_owners);
    }

    #[test]
    fn low_address_file_suffix_advances_the_page_cursor() {
        let (start, backing_advance) = super::super::relocate_affine_origin(
            VirtAddr::from(0x4000),
            VirtAddr::from(0x8000),
            VirtAddr::from(0x1000),
        )
        .unwrap();

        assert_eq!(start, VirtAddr::from(0x1000));
        assert_eq!(backing_advance, 0x4000);
        assert_eq!(advance_offset_page(7, backing_advance), Ok(11));
        assert_eq!(
            advance_offset_page(7, backing_advance + 1),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            advance_offset_page(u32::MAX - 1, backing_advance),
            Err(AxError::InvalidInput)
        );
    }

    #[derive(Clone)]
    struct TestFileMappingBackend(FileBackend);

    impl MappingBackend for TestFileMappingBackend {
        type Addr = VirtAddr;
        type Flags = MappingFlags;
        type PageTable = ();

        fn map(
            &self,
            start: VirtAddr,
            size: usize,
            flags: MappingFlags,
            _page_table: &mut (),
        ) -> bool {
            let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
                return false;
            };
            if self.0.validate_range(range).is_err() || self.0.check_flags(flags).is_err() {
                return false;
            }
            if flags.contains(MappingFlags::WRITE) {
                self.0.activate_writable_segment().is_ok()
            } else {
                self.0.deactivate_writable_segment().is_ok()
            }
        }

        fn unmap(&self, _start: VirtAddr, _size: usize, _page_table: &mut ()) -> bool {
            true
        }

        fn preflight_unmap(&self, _start: VirtAddr, _size: usize, _page_table: &()) -> bool {
            true
        }

        fn protect(
            &self,
            _start: VirtAddr,
            _size: usize,
            new_flags: MappingFlags,
            _page_table: &mut (),
        ) -> bool {
            if self.0.check_flags(new_flags).is_err() {
                return false;
            }
            if new_flags.contains(MappingFlags::WRITE) {
                self.0.activate_writable_segment().is_ok()
            } else {
                self.0.deactivate_writable_segment().is_ok()
            }
        }

        fn can_merge(&self, other: &Self) -> bool {
            self.0.compatible_with(&other.0)
                && self
                    .0
                    .mapping_status()
                    .compatible_with(other.0.mapping_status())
        }
    }

    fn install_capability(loc: &Location) {
        loc.set_xattr(
            b"security.capability",
            &[1, 2, 3],
            axfs_ng_vfs::XattrSetMode::Upsert,
        )
        .unwrap();
    }

    fn has_capability(loc: &Location) -> bool {
        crate::file::xattr_provider::read_security_capability(loc)
            .unwrap()
            .is_some()
    }

    #[test]
    fn shared_writable_mapping_registers_exec_and_setcap_exclusion() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("mapped-capability");
        install_capability(&loc);

        let executable_mapping =
            executable::WritableMappingRegistration::for_location(&loc).unwrap();

        activate_writable_mapping(Some(&executable_mapping), None, None).unwrap();
        assert!(has_capability(&loc));
        assert!(matches!(
            executable::CredentialReadLease::acquire(&loc),
            Err(error) if error == axerrno::LinuxError::ETXTBSY.into()
        ));
        assert!(matches!(
            executable::with_file_capability_metadata_unpinned(&loc, || Ok(())),
            Err(error) if error == axerrno::LinuxError::ETXTBSY.into()
        ));

        deactivate_writable_mapping(Some(&executable_mapping), None, None).unwrap();
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
    }

    #[test]
    fn dropped_writable_mapping_admission_refunds_fresh_registrations() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("dropped-writable-admission");
        memfd::install_memfd_state(&loc, true).unwrap();
        let backend = test_backend(&loc, Arc::new(()));

        let admission = backend.begin_writable_mapping_admission().unwrap();
        assert!(backend.0.writable_mapping.as_ref().unwrap().is_active());
        assert!(backend.0.executable_mapping.as_ref().unwrap().is_active());
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);

        drop(admission);
        assert!(!backend.0.writable_mapping.as_ref().unwrap().is_active());
        assert!(!backend.0.executable_mapping.as_ref().unwrap().is_active());
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap(),
            F_SEAL_WRITE
        );
    }

    #[test]
    fn completed_writable_mapping_admission_transfers_registration_to_segment() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("completed-writable-admission");
        memfd::install_memfd_state(&loc, true).unwrap();
        let backend = test_backend(&loc, Arc::new(()));

        let admission = backend.begin_writable_mapping_admission().unwrap();
        backend.activate_writable_segment().unwrap();
        admission.complete().unwrap();

        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 1);
        assert!(backend.0.writable_mapping.as_ref().unwrap().is_active());
        assert!(backend.0.executable_mapping.as_ref().unwrap().is_active());
        backend.deactivate_writable_segment().unwrap();
        assert!(!backend.0.writable_mapping.as_ref().unwrap().is_active());
        assert!(!backend.0.executable_mapping.as_ref().unwrap().is_active());
    }

    #[test]
    fn split_segments_keep_exclusions_until_the_final_writable_handle() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("split-writable-memfd");
        memfd::install_memfd_state(&loc, true).unwrap();
        install_capability(&loc);
        let backend = test_backend(&loc, Arc::new(()));

        backend.activate_writable_segment().unwrap();
        assert!(has_capability(&loc));
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 1);

        let split = backend.clone();
        assert!(Arc::ptr_eq(&backend.0, &split.0));
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 2);

        // Simulate mprotect(PROT_READ) on one of two split VMAs.
        backend.deactivate_writable_segment().unwrap();
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 1);
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE),
            Err(AxError::ResourceBusy)
        );
        assert!(matches!(
            executable::CredentialReadLease::acquire(&loc),
            Err(error) if error == axerrno::LinuxError::ETXTBSY.into()
        ));

        // Dropping/merging the final writable segment releases both kinds of
        // exclusion exactly once.
        drop(split);
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap(),
            F_SEAL_WRITE
        );
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
    }

    #[test]
    fn memory_set_split_merge_and_unmap_keep_exact_file_backend_ownership() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("real-memory-set-split");
        memfd::install_memfd_state(&loc, true).unwrap();
        let base = VirtAddr::from(0x2000_0000);
        let mut file_backend = test_backend(&loc, Arc::new(()));
        let lease = test_mapping_lease(&loc);
        let expected_ofd = lease.ofd_key();
        file_backend
            .mapping_status_mut()
            .replace_file_mapping(Some(lease));
        let backend = TestFileMappingBackend(file_backend);
        let writable = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
        let readonly = MappingFlags::READ | MappingFlags::USER;
        let mut set = MemorySet::new();
        let mut page_table = ();

        set.map(
            MemoryArea::new_with_lineage(
                base,
                3 * PAGE_SIZE_4K,
                writable,
                backend,
                MappingLineage::new(2).unwrap(),
            ),
            &mut page_table,
            false,
        )
        .unwrap();
        set.protect(
            base + PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            |_| Some(readonly),
            &mut page_table,
        )
        .unwrap();

        {
            let areas = set.iter().collect::<Vec<_>>();
            assert_eq!(areas.len(), 3);
            assert_eq!(
                (areas[0].start(), areas[0].end()),
                (base, base + PAGE_SIZE_4K)
            );
            assert_eq!(
                (areas[1].start(), areas[1].end()),
                (base + PAGE_SIZE_4K, base + 2 * PAGE_SIZE_4K)
            );
            assert_eq!(
                (areas[2].start(), areas[2].end()),
                (base + 2 * PAGE_SIZE_4K, base + 3 * PAGE_SIZE_4K)
            );
            let left = &areas[0].backend().0;
            let middle = &areas[1].backend().0;
            let right = &areas[2].backend().0;
            assert_eq!(
                left.mapping_status().file_mapping().unwrap().ofd_key(),
                expected_ofd
            );
            assert_eq!(
                middle.mapping_status().file_mapping().unwrap().ofd_key(),
                expected_ofd
            );
            assert_eq!(
                right.mapping_status().file_mapping().unwrap().ofd_key(),
                expected_ofd
            );
            assert!(left.writable_segment_active());
            assert!(!middle.writable_segment_active());
            assert!(right.writable_segment_active());
            assert!(Arc::ptr_eq(&left.0, &middle.0));
            assert!(Arc::ptr_eq(&left.0, &right.0));
            assert_eq!(left.0.writable_segments.load(Ordering::Acquire), 2);
        }
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE),
            Err(AxError::ResourceBusy)
        );
        assert!(matches!(
            executable::CredentialReadLease::acquire(&loc),
            Err(error) if error == axerrno::LinuxError::ETXTBSY.into()
        ));

        // Restoring WRITE merges the three areas. The two retired backend
        // handles refund their ownership, leaving one segment registration.
        set.protect(
            base + PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            |_| Some(writable),
            &mut page_table,
        )
        .unwrap();
        {
            let areas = set.iter().collect::<Vec<_>>();
            assert_eq!(areas.len(), 1);
            let file = &areas[0].backend().0;
            assert_eq!(
                file.mapping_status().file_mapping().unwrap().ofd_key(),
                expected_ofd
            );
            assert!(file.writable_segment_active());
            assert_eq!(file.0.writable_segments.load(Ordering::Acquire), 1);
        }

        set.unmap(base, 3 * PAGE_SIZE_4K, &mut page_table).unwrap();
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap(),
            F_SEAL_WRITE
        );
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
    }

    #[test]
    fn inactive_clone_and_drop_do_not_change_segment_accounting() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("inactive-split");
        memfd::install_memfd_state(&loc, true).unwrap();
        let backend = test_backend(&loc, Arc::new(()));

        let split = backend.clone();
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);
        assert!(!split.writable_segment_active());
        drop(split);
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap(),
            F_SEAL_WRITE
        );
    }

    #[test]
    fn fresh_mapping_inners_publish_independent_registrations() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("independent-mappings");
        memfd::install_memfd_state(&loc, true).unwrap();
        let map_id = Arc::new(());
        let first = test_backend(&loc, map_id.clone());
        let second = test_backend(&loc, map_id);

        assert!(!Arc::ptr_eq(&first.0, &second.0));
        assert!(!Arc::ptr_eq(
            first.0.writable_mapping.as_ref().unwrap(),
            second.0.writable_mapping.as_ref().unwrap()
        ));
        first.activate_writable_segment().unwrap();
        second.activate_writable_segment().unwrap();
        assert_eq!(first.0.writable_segments.load(Ordering::Acquire), 1);
        assert_eq!(second.0.writable_segments.load(Ordering::Acquire), 1);

        first.deactivate_writable_segment().unwrap();
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE),
            Err(AxError::ResourceBusy)
        );
        drop(second);
        assert_eq!(
            memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap(),
            F_SEAL_WRITE
        );
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
    }

    #[test]
    fn sealed_memfd_admission_does_not_change_mode_or_capability() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("sealed-mapping");
        memfd::install_memfd_state(&loc, true).unwrap();
        memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap();
        install_capability(&loc);
        loc.update_metadata(axfs_ng_vfs::MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(0o6755)),
            ..Default::default()
        })
        .unwrap();
        let backend = test_backend(&loc, Arc::new(()));

        assert_eq!(
            backend.begin_writable_mapping_admission().err(),
            Some(AxError::OperationNotPermitted)
        );
        assert_eq!(
            backend.activate_writable_segment(),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(loc.metadata().unwrap().mode.bits(), 0o6755);
        assert!(has_capability(&loc));
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);
        assert!(!backend.0.executable_mapping.as_ref().unwrap().is_active());
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
    }

    #[test]
    fn executable_admission_failure_refunds_memfd_and_preserves_capability() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("leased-executable");
        memfd::install_memfd_state(&loc, true).unwrap();
        install_capability(&loc);
        let lease = executable::CredentialReadLease::acquire(&loc).unwrap();
        let backend = test_backend(&loc, Arc::new(()));

        assert!(matches!(
            backend.activate_writable_segment(),
            Err(error) if error == axerrno::LinuxError::ETXTBSY.into()
        ));
        assert!(has_capability(&loc));
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);
        assert!(!backend.0.writable_mapping.as_ref().unwrap().is_active());

        drop(lease);
        backend.activate_writable_segment().unwrap();
        assert!(has_capability(&loc));
        backend.deactivate_writable_segment().unwrap();
    }

    #[test]
    fn clone_map_post_registration_failure_refunds_fresh_resources() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("clone-map-refund");
        memfd::install_memfd_state(&loc, true).unwrap();
        memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap();
        install_capability(&loc);
        let source = test_backend(&loc, Arc::new(()));
        let registration_ran = AtomicBool::new(false);

        let result = source.clone_map_with_registration(
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
            CachedFileEvictionOwner::new(2).unwrap(),
            |_| {
                registration_ran.store(true, Ordering::Release);
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(error) if error == AxError::OperationNotPermitted
        ));
        assert!(registration_ran.load(Ordering::Acquire));
        assert_eq!(source.0.writable_segments.load(Ordering::Acquire), 0);
        assert!(!source.0.writable_mapping.as_ref().unwrap().is_active());
        assert!(has_capability(&loc));
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
    }

    #[test]
    fn fork_clone_preserves_exact_file_mapping_lease() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("fork-file-mapping-lease");
        let mut source = test_backend(&loc, Arc::new(()));
        let lease = test_mapping_lease(&loc);
        let expected_ofd = lease.ofd_key();
        source
            .mapping_status_mut()
            .replace_file_mapping(Some(lease));

        let child = source
            .clone_map_with_registration(
                MappingFlags::READ | MappingFlags::USER,
                CachedFileEvictionOwner::new(2).unwrap(),
                |_| Ok(()),
            )
            .unwrap();
        let Backend::File(child) = child else {
            panic!("file fork clone changed backend kind");
        };

        assert!(!Arc::ptr_eq(&source.0, &child.0));
        assert_eq!(
            source.mapping_status().file_mapping().unwrap().ofd_key(),
            expected_ofd
        );
        assert_eq!(
            child.mapping_status().file_mapping().unwrap().ofd_key(),
            expected_ofd
        );
        assert!(
            source
                .mapping_status()
                .compatible_with(child.mapping_status())
        );
    }

    #[test]
    fn file_futex_identity_tracks_cached_file_generation() {
        let _context = test_context();
        let loc = test_location("futex-file-generation");
        let cache = CachedFile::get_or_create(loc);
        let first = file_futex_handle(&cache);
        let second = file_futex_handle(&cache);

        assert_eq!(first.key, cache.identity());
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn segment_activation_and_deactivation_are_idempotent() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("idempotent-segment");
        memfd::install_memfd_state(&loc, true).unwrap();
        let backend = test_backend(&loc, Arc::new(()));

        backend.activate_writable_segment().unwrap();
        backend.activate_writable_segment().unwrap();
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 1);
        backend.deactivate_writable_segment().unwrap();
        backend.deactivate_writable_segment().unwrap();
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);
    }

    #[test]
    fn segment_count_underflow_and_overflow_fail_closed() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("segment-count-bounds");
        let backend = test_backend(&loc, Arc::new(()));

        let underflow = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = backend.0.release_writable_segment();
        }));
        assert!(underflow.is_err());
        assert_eq!(backend.0.writable_segments.load(Ordering::Acquire), 0);

        backend
            .0
            .writable_segments
            .store(WRITABLE_SEGMENTS_TRANSITIONING - 1, Ordering::Release);
        assert_eq!(backend.0.acquire_writable_segment(), Err(AxError::NoMemory));
        assert_eq!(
            backend.0.writable_segments.load(Ordering::Acquire),
            WRITABLE_SEGMENTS_TRANSITIONING - 1
        );
        backend.0.writable_segments.store(0, Ordering::Release);
    }
}
