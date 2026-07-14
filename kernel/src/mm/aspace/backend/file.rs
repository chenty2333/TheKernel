use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult};
use axfs::{CachedFile, CachedFilePagePin, CachedFilePinWindow, FileFlags};
use axhal::paging::{MappingFlags, PageSize, PageTableCursor, PagingError};
use axsync::Mutex;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};

use super::{AddrSpace, Backend, BackendOps, PopulateCallback, page_table_flags, pages_in};
use crate::file::{executable, memfd};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct FileFutexKey {
    device: u64,
    inode: u64,
}

#[cfg(not(test))]
type FileFutexHandlesMutex<T> = Mutex<T>;
#[cfg(test)]
type FileFutexHandlesMutex<T> = spin::Mutex<T>;

static FILE_FUTEX_HANDLES: FileFutexHandlesMutex<BTreeMap<FileFutexKey, Weak<FileFutexIdentity>>> =
    FileFutexHandlesMutex::new(BTreeMap::new());
const REGISTERING_LISTENER: usize = usize::MAX;
const WRITABLE_SEGMENTS_TRANSITIONING: usize = usize::MAX;

struct FileFutexIdentity {
    key: FileFutexKey,
    handle: Arc<()>,
}

fn transition_writable_mapping(
    location: &axfs_ng_vfs::Location,
    executable_mapping: Option<&executable::WritableMappingRegistration>,
    writable_mapping: Option<&Arc<memfd::WritableMappingRegistration>>,
    active: bool,
) -> AxResult<()> {
    let was_memfd_mapping_active =
        writable_mapping.is_some_and(|registration| registration.is_active());
    let was_executable_mapping_active =
        executable_mapping.is_some_and(executable::WritableMappingRegistration::is_active);
    if active && let Some(registration) = writable_mapping {
        // This is the shared linearization point with F_ADD_SEALS. Reserve it
        // before killpriv or executable admission so a sealed mapping fails
        // without mutating file metadata.
        registration.set_active(true)?;
    }
    if let Some(registration) = executable_mapping {
        if let Err(error) = registration.set_active(active) {
            if active
                && !was_memfd_mapping_active
                && let Some(registration) = writable_mapping
            {
                let _ = registration.set_active(false);
            }
            return Err(error);
        }
    }
    if active
        && let Err(error) =
            crate::file::xattr_provider::remove_security_capability_if_present(location)
    {
        if !was_executable_mapping_active && let Some(registration) = executable_mapping {
            let _ = registration.set_active(false);
        }
        if !was_memfd_mapping_active && let Some(registration) = writable_mapping {
            let _ = registration.set_active(false);
        }
        return Err(error);
    }
    if !active && let Some(registration) = writable_mapping {
        registration.set_active(false)?;
    }
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
    let loc = cache.location();
    let key = FileFutexKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    };
    let mut handles = FILE_FUTEX_HANDLES.lock();
    if let Some(handle) = handles.get(&key).and_then(Weak::upgrade) {
        return handle;
    }

    let handle = Arc::new(FileFutexIdentity {
        key,
        handle: Arc::new(()),
    });
    handles.insert(key, Arc::downgrade(&handle));
    handle
}

fn new_file_backend_inner(
    start: VirtAddr,
    cache: CachedFile,
    flags: FileFlags,
    offset_page: u32,
    file_end: Option<u64>,
    map_id: Arc<()>,
    futex_handle: Arc<FileFutexIdentity>,
) -> Arc<FileBackendInner> {
    let writable_mapping = memfd::new_writable_mapping_registration(cache.location());
    let executable_mapping =
        executable::WritableMappingRegistration::for_location(cache.location());
    Arc::new(FileBackendInner {
        start,
        cache,
        flags,
        offset_page,
        file_end,
        handle: AtomicUsize::new(0),
        map_id,
        futex_handle,
        writable_segments: AtomicUsize::new(0),
        writable_mapping,
        executable_mapping,
    })
}

fn relocate_backend_start(
    backend_start: VirtAddr,
    old_start: VirtAddr,
    new_start: VirtAddr,
) -> VirtAddr {
    if backend_start >= old_start {
        new_start + backend_start.sub_addr(old_start)
    } else {
        new_start.sub(old_start.sub_addr(backend_start))
    }
}

#[doc(hidden)]
pub struct FileBackendInner {
    start: VirtAddr,
    cache: CachedFile,
    flags: FileFlags,
    offset_page: u32,
    file_end: Option<u64>,
    handle: AtomicUsize,
    map_id: Arc<()>,
    futex_handle: Arc<FileFutexIdentity>,
    writable_segments: AtomicUsize,
    writable_mapping: Option<Arc<memfd::WritableMappingRegistration>>,
    executable_mapping: Option<executable::WritableMappingRegistration>,
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
        transition_writable_mapping(
            self.cache.location(),
            self.executable_mapping.as_ref(),
            self.writable_mapping.as_ref(),
            active,
        )
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
        if self
            .handle
            .compare_exchange(0, REGISTERING_LISTENER, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AxError::AlreadyExists);
        }
        let aspace = Arc::downgrade(aspace);
        let handle = self.cache.add_evict_listener({
            let this = Arc::downgrade(self);
            move |pn, _page| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                let Some(aspace) = aspace.upgrade() else {
                    // The address space has been dropped, nothing to do.
                    return;
                };
                let Some(mut aspace) = aspace.try_lock() else {
                    // This can happen during the populate process, when new pages
                    // are being populated and old pages are being evicted. In this
                    // case, we delegate the unmapping to the populate process.
                    return;
                };
                this.on_evict(pn, &mut aspace);
            }
        });
        self.handle.store(handle, Ordering::Release);
        Ok(())
    }

    fn on_evict(self: &Arc<Self>, pn: u32, aspace: &mut AddrSpace) {
        let Some(pn) = pn.checked_sub(self.offset_page) else {
            return;
        };
        let vaddr = self.start + pn as usize * PageSize::Size4K as usize;
        if !aspace.find_area(vaddr).is_some_and(
            |it| matches!(it.backend(), Backend::File(file) if Arc::ptr_eq(&file.0, self)),
        ) {
            // Ignore if the page is not controlled by this file mapping.
            return;
        }

        let pt = aspace.page_table_mut();
        match pt.cursor().unmap(vaddr) {
            Ok(_) | Err(PagingError::NotMapped) => {}
            Err(err) => {
                warn!("Failed to unmap page {:?}: {:?}", vaddr, err);
            }
        }
    }
}

/// File-backed mapping backend.
const SEGMENT_INACTIVE: u8 = 0;
const SEGMENT_TRANSITIONING: u8 = 1;
const SEGMENT_ACTIVE: u8 = 2;
const SEGMENT_FAIL_CLOSED: u8 = 3;

pub struct FileBackend(Arc<FileBackendInner>, AtomicU8);

impl Clone for FileBackend {
    fn clone(&self) -> Self {
        loop {
            match self.1.load(Ordering::Acquire) {
                SEGMENT_INACTIVE => {
                    return Self(self.0.clone(), AtomicU8::new(SEGMENT_INACTIVE));
                }
                SEGMENT_TRANSITIONING => core::hint::spin_loop(),
                SEGMENT_ACTIVE => {
                    if self.0.retain_writable_segment() {
                        return Self(self.0.clone(), AtomicU8::new(SEGMENT_ACTIVE));
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
                    return Self(self.0.clone(), AtomicU8::new(SEGMENT_ACTIVE));
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
    fn inactive(inner: Arc<FileBackendInner>) -> Self {
        Self(inner, AtomicU8::new(SEGMENT_INACTIVE))
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
            self.deactivate_writable_segment()?;
        }
        Ok(())
    }

    pub fn futex_handle(&self) -> Weak<()> {
        Arc::downgrade(&self.0.futex_handle.handle)
    }

    pub fn futex_key(&self, address: usize) -> (Weak<()>, usize) {
        let offset = (self.0.offset_page as usize * PAGE_SIZE_4K)
            .saturating_add(address.saturating_sub(self.0.start.as_usize()));
        (self.futex_handle(), offset)
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

    pub(crate) fn begin_user_io_pin_window(&self) -> CachedFilePinWindow {
        self.0.cache.begin_user_io_pin_window()
    }

    pub(crate) fn pin_user_io_page(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
    ) -> AxResult<CachedFilePagePin> {
        let page_start = vaddr.align_down_4k();
        if page_start < self.0.start {
            return Err(AxError::BadAddress);
        }

        let pn = self
            .page_number_for(page_start)
            .map_err(|_| AxError::BadAddress)?;

        self.0.cache.pin_cached_page_by_paddr(pn, paddr)
    }

    fn clone_for_range_with_id(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
        map_id: Arc<()>,
    ) -> AxResult<Self> {
        let start = relocate_backend_start(self.0.start, old_start, new_start);
        let inner = new_file_backend_inner(
            start,
            self.0.cache.clone(),
            self.0.flags,
            self.0.offset_page,
            self.0.file_end,
            map_id,
            self.0.futex_handle.clone(),
        );
        inner.register_listener(aspace)?;
        let backend = Self::inactive(inner);
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

    pub(crate) fn duplicate_mapping(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Self> {
        self.clone_for_range_with_id(old_start, new_start, aspace, Arc::new(()))
    }

    fn clone_map_with_registration(
        &self,
        flags: MappingFlags,
        register: impl FnOnce(&Arc<FileBackendInner>) -> AxResult,
    ) -> AxResult<Backend> {
        let inner = new_file_backend_inner(
            self.0.start,
            self.0.cache.clone(),
            self.0.flags,
            self.0.offset_page,
            self.0.file_end,
            self.0.map_id.clone(),
            self.0.futex_handle.clone(),
        );
        register(&inner)?;
        let backend = FileBackend::inactive(inner);
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

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        for addr in pages_in(range, PageSize::Size4K)? {
            match pt.unmap(addr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => {
                    warn!("Failed to unmap page {:?}: {:?}", addr, err);
                    if self.writable_segment_active() {
                        self.retain_writable_exclusion_fail_closed();
                    }
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }

    fn on_protect(
        &self,
        _range: VirtAddrRange,
        new_flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        self.check_flags(new_flags)?;
        Ok(())
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult<(usize, Option<PopulateCallback>)> {
        self.0.cache.with_direct_io_excluded(|| {
            let mut pages = 0;
            let mut to_be_evicted = Vec::new();
            let start_page = self.page_number_for(range.start)?;
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
                                pages += 1;
                                AxResult::Ok(())
                            })?;
                        } else if page_flags.contains(access_flags) {
                            pages += 1;
                        }
                    }
                    // If the page is not mapped, try map it.
                    Err(PagingError::NotMapped) => {
                        let map_flags = flags - MappingFlags::WRITE;
                        self.0.cache.with_page_or_insert(pn, |page, evicted| {
                            let evicted = evicted;
                            if let Some(evicted) = evicted.as_ref() {
                                to_be_evicted.push(evicted.page_number());
                            }
                            pt.map(
                                addr,
                                page.paddr(),
                                PageSize::Size4K,
                                page_table_flags(map_flags),
                            )?;
                            pages += 1;
                            Ok(())
                        })?;
                    }
                    Err(_) => return Err(AxError::BadAddress),
                }
            }
            let callback: Option<PopulateCallback> = if to_be_evicted.is_empty() {
                None
            } else {
                let inner = self.0.clone();
                Some(Box::new(move |aspace: &mut AddrSpace| {
                    for pn in to_be_evicted {
                        inner.on_evict(pn, aspace);
                    }
                }))
            };
            Ok((pages, callback))
        })
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        flags: MappingFlags,
        _old_pt: &mut PageTableCursor,
        _new_pt: &mut PageTableCursor,
        new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Backend> {
        self.clone_map_with_registration(flags, |inner| inner.register_listener(new_aspace))
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
            flags,
            offset_page,
            file_end,
            Arc::new(()),
            futex_handle,
        );
        inner.register_listener(aspace)?;
        Ok(Self::File(FileBackend::inactive(inner)))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::AtomicBool;
    use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};

    use axfs_ng_vfs::{Location, Mountpoint, NodePermission, NodeType};
    use linux_raw_sys::general::F_SEAL_WRITE;
    use memory_set::{MappingBackend, MemoryArea, MemorySet};

    use super::*;
    use crate::pseudofs::tmp::MemoryFs;

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
            FileFlags::READ | FileFlags::WRITE,
            0,
            None,
            map_id,
            futex_handle,
        ))
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
        }
    }

    fn install_capability(loc: &Location) {
        loc.set_xattr(
            "security.capability",
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
    fn shared_writable_mapping_kills_caps_and_excludes_exec_and_setcap() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("mapped-capability");
        install_capability(&loc);

        let executable_mapping =
            executable::WritableMappingRegistration::for_location(&loc).unwrap();

        transition_writable_mapping(&loc, Some(&executable_mapping), None, true).unwrap();
        assert!(!has_capability(&loc));
        assert!(matches!(
            executable::CredentialReadLease::acquire(&loc),
            Err(error) if error == axerrno::LinuxError::ETXTBSY.into()
        ));
        assert!(matches!(
            executable::with_file_capability_metadata_unpinned(&loc, || Ok(())),
            Err(error) if error == axerrno::LinuxError::ETXTBSY.into()
        ));

        transition_writable_mapping(&loc, Some(&executable_mapping), None, false).unwrap();
        drop(executable::CredentialReadLease::acquire(&loc).unwrap());
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
        assert!(!has_capability(&loc));
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
        let backend = TestFileMappingBackend(test_backend(&loc, Arc::new(())));
        let writable = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
        let readonly = MappingFlags::READ | MappingFlags::USER;
        let mut set = MemorySet::new();
        let mut page_table = ();

        set.map(
            MemoryArea::new(base, 3 * PAGE_SIZE_4K, writable, backend),
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
    fn sealed_memfd_rejection_does_not_kill_capability_or_publish_exec_exclusion() {
        let _context = test_context();
        executable::init().unwrap();
        let loc = test_location("sealed-mapping");
        memfd::install_memfd_state(&loc, true).unwrap();
        memfd::add_seals(&loc, true, F_SEAL_WRITE).unwrap();
        install_capability(&loc);
        let backend = test_backend(&loc, Arc::new(()));

        assert_eq!(
            backend.activate_writable_segment(),
            Err(AxError::OperationNotPermitted)
        );
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
        assert!(!has_capability(&loc));
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
