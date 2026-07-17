//! Memory mapping backends.
use alloc::{boxed::Box, sync::Arc};
use core::mem::ManuallyDrop;

use axalloc::{UsageKind, global_allocator};
use axerrno::{AxError, AxResult};
use axfs::{CachedFilePagePin, CachedFilePinWindow};
use axhal::{
    mem::{phys_to_virt, virt_to_phys},
    paging::{MappingFlags, PageSize, PageTable, PageTableCursor},
};
use axsync::Mutex;
use enum_dispatch::enum_dispatch;
use memory_addr::{DynPageIter, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};
use memory_set::{DeferredUnmapBackend, MappingBackend};
use thekernel_linux_mm::MappingKind;

mod cow;
mod file;
mod linear;
mod phys_pin;
mod shared;

pub use self::shared::{SharedAtomicU32, SharedPages};
pub(crate) use self::{
    file::WritableMappingAdmission,
    phys_pin::{PhysicalFramePins, PreparedPhysicalFramePins, prepare_physical_pin_registry},
    shared::PreparedFixedSharedMapping,
};
use super::{
    AddrSpace,
    mapping::{FileLikeMappingLease, FileMappingLease, MappingStatus, relocate_affine_origin},
};

fn divide_page(size: usize, page_size: PageSize) -> AxResult<usize> {
    if !page_size.is_aligned(size) {
        return Err(AxError::InvalidInput);
    }
    Ok(size >> (page_size as usize).trailing_zeros())
}

fn alloc_frame(zeroed: bool, size: PageSize) -> AxResult<PhysAddr> {
    let page_size = size as usize;
    let num_pages = page_size / PAGE_SIZE_4K;
    let vaddr =
        VirtAddr::from(global_allocator().alloc_pages(num_pages, page_size, UsageKind::VirtMem)?);
    if zeroed {
        // Zero with a u64 store loop instead of core::ptr::write_bytes. Under
        // QEMU TCG the compiler-emitted memset for write_bytes runs ~4x slower
        // here (it issues ~4x more stores), which made anonymous page faults
        // (stack/heap/malloc) dominate at ~170us/page. The u64 loop cuts that
        // to ~43us. page_size is always a multiple of 8 (4K/2M).
        let p = vaddr.as_mut_ptr() as *mut u64;
        for i in 0..(page_size / 8) {
            unsafe { *p.add(i) = 0 };
        }
    }
    let paddr = virt_to_phys(vaddr);

    Ok(paddr)
}

fn dealloc_frame(frame: PhysAddr, align: PageSize) {
    if phys_pin::defer_frame_dealloc_if_pinned(frame, align) {
        return;
    }
    dealloc_frame_now(frame, align);
}

fn dealloc_frame_now(frame: PhysAddr, align: PageSize) {
    let vaddr = phys_to_virt(frame);
    let page_size: usize = align.into();
    let num_pages = page_size / PAGE_SIZE_4K;
    global_allocator().dealloc_pages(vaddr.as_usize(), num_pages, UsageKind::VirtMem);
}

fn pages_in(range: VirtAddrRange, align: PageSize) -> AxResult<DynPageIter<VirtAddr>> {
    DynPageIter::new(range.start, range.end, align as usize).ok_or(AxError::InvalidInput)
}

fn preflight_sparse_unmap(range: VirtAddrRange, page_size: PageSize, pt: &PageTable) -> AxResult {
    pages_in(range, page_size)?;
    for (_, _, _, mapped_size) in pt.collect_present_leaves(range.start, range.size())? {
        if mapped_size != page_size {
            return Err(AxError::BadAddress);
        }
    }
    Ok(())
}

fn preflight_dense_unmap(range: VirtAddrRange, page_size: PageSize, pt: &PageTable) -> AxResult {
    preflight_sparse_unmap(range, page_size, pt)?;
    for address in pages_in(range, page_size)? {
        let (_, _, mapped_size) = pt.query(address)?;
        if mapped_size != page_size {
            return Err(AxError::BadAddress);
        }
    }
    Ok(())
}

fn page_table_flags(flags: MappingFlags) -> MappingFlags {
    // RISC-V and LoongArch PTEs cannot represent writable-without-readable.
    // Keep VMA flags exact for /proc/maps and mprotect, but normalize the
    // hardware permissions when touching page tables.
    if flags.contains(MappingFlags::WRITE) && !flags.contains(MappingFlags::READ) {
        flags | MappingFlags::READ
    } else {
        flags
    }
}

type PopulateCallback<T = AddrSpace> = Box<dyn FnMut(&mut T)>;

/// Result of populating page-table entries plus cleanup deferred by a backend.
///
/// File-cache eviction listeners cannot re-lock an address space while its
/// populate path already owns that lock. Keep the deferred work attached to
/// the result so callers run it before observing either success or failure.
#[must_use = "deferred population cleanup must be completed"]
pub(crate) struct PopulateOutcome<T = AddrSpace> {
    result: AxResult<usize>,
    callback: Option<ManuallyDrop<PopulateCallback<T>>>,
}

impl<T> PopulateOutcome<T> {
    fn new(result: AxResult<usize>, callback: Option<PopulateCallback<T>>) -> Self {
        Self {
            result,
            callback: callback.map(ManuallyDrop::new),
        }
    }

    fn immediate(result: AxResult<usize>) -> Self {
        Self::new(result, None)
    }

    pub(super) fn finish(mut self, target: &mut T) -> AxResult<usize> {
        if let Some(mut callback) = self.callback.take() {
            // Invoke through a borrow so an unwind leaves the callback and its
            // deferred ownership inside ManuallyDrop. A successful callback is
            // the only path that releases those captures.
            callback(target);
            drop(ManuallyDrop::into_inner(callback));
        }
        self.result
    }
}

impl<T> Drop for PopulateOutcome<T> {
    fn drop(&mut self) {
        assert!(
            self.callback.is_none(),
            "PopulateOutcome with deferred cleanup dropped before finish"
        );
    }
}

#[enum_dispatch]
pub trait BackendOps {
    /// Returns the page size of the backend.
    fn page_size(&self) -> PageSize;

    /// Map a memory region.
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pt: &mut PageTableCursor) -> AxResult;

    /// Unmap a memory region.
    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult<BackendRetirement>;

    /// Validates every recoverable unmap condition without changing PTEs or
    /// backend-owned resources.
    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult;

    /// Validates every recoverable protection condition without changing PTEs
    /// or backend-owned resources.
    fn preflight_protect(
        &self,
        range: VirtAddrRange,
        _new_flags: MappingFlags,
        pt: &PageTable,
    ) -> AxResult {
        preflight_sparse_unmap(range, self.page_size(), pt)
    }

    /// Populate a memory region and return how many pages now satisfy
    /// `access_flags`.
    ///
    /// If another thread has already mapped the page with sufficient permissions,
    /// treat it as populated.
    fn populate(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _access_flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> PopulateOutcome {
        PopulateOutcome::immediate(Ok(0))
    }

    /// Duplicates this mapping for use in a different page table.
    ///
    /// This differs from `clone`, which is designed for splitting a mapping
    /// within the same table.
    ///
    /// [`BackendOps::map`] will be latter called to the returned backend.
    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pt: &mut PageTableCursor,
        new_pt: &mut PageTableCursor,
        new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Backend>;
}

/// A unified enum type for different memory mapping backends.
#[derive(Clone)]
#[enum_dispatch(BackendOps)]
pub enum Backend {
    Linear(linear::LinearBackend),
    Cow(cow::CowBackend),
    Shared(shared::SharedBackend),
    File(file::FileBackend),
}

/// Backend-owned resources detached from PTEs but retained until TLB grace.
#[must_use = "detached mapping resources must remain live until TLB grace"]
pub struct BackendRetirement {
    _cow: Option<cow::CowUnmapRetirement>,
}

impl BackendRetirement {
    const fn empty() -> Self {
        Self { _cow: None }
    }

    fn cow(retirement: cow::CowUnmapRetirement) -> Self {
        Self {
            _cow: Some(retirement),
        }
    }
}

impl MappingBackend for Backend {
    type Addr = VirtAddr;
    type Flags = MappingFlags;
    type PageTable = PageTable;

    fn map(&self, start: VirtAddr, size: usize, flags: MappingFlags, pt: &mut PageTable) -> bool {
        let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        if let Err(err) = BackendOps::map(self, range, flags, &mut pt.cursor()) {
            warn!("Failed to map area: {:?}", err);
            false
        } else {
            true
        }
    }

    fn unmap(&self, start: VirtAddr, size: usize, pt: &mut PageTable) -> bool {
        let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        let mut cursor = pt.cursor();
        let result = BackendOps::unmap(self, range, &mut cursor);
        drop(cursor);
        match result {
            Ok(retired) => {
                super::super::retire_after_local_flush(retired);
                true
            }
            Err(err) => {
                warn!("Failed to unmap area: {:?}", err);
                false
            }
        }
    }

    fn preflight_unmap(&self, start: VirtAddr, size: usize, pt: &PageTable) -> bool {
        let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        if let Err(err) = BackendOps::preflight_unmap(self, range, pt) {
            warn!("Failed to preflight area unmap: {:?}", err);
            false
        } else {
            true
        }
    }

    fn preflight_protect(
        &self,
        start: Self::Addr,
        size: usize,
        new_flags: Self::Flags,
        pt: &Self::PageTable,
    ) -> bool {
        let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        if let Err(err) = BackendOps::preflight_protect(self, range, new_flags, pt) {
            warn!("Failed to preflight area protection: {:?}", err);
            false
        } else {
            true
        }
    }

    fn protect(
        &self,
        start: Self::Addr,
        size: usize,
        new_flags: Self::Flags,
        pt: &mut Self::PageTable,
    ) -> bool {
        let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        let mut cursor = pt.cursor();
        if let Backend::File(file) = self {
            if let Err(err) = file.protect_range(range, new_flags, &mut cursor) {
                warn!("Failed to protect file area: {:?}", err);
                return false;
            }
            return true;
        }
        cursor
            .protect_region(start, size, page_table_flags(new_flags))
            .is_ok()
    }

    fn can_merge(&self, other: &Self) -> bool {
        self.mergeable_with(other)
    }
}

impl DeferredUnmapBackend for Backend {
    type Retirement = BackendRetirement;

    fn unmap_deferred(
        &self,
        start: VirtAddr,
        size: usize,
        pt: &mut PageTable,
    ) -> Option<Self::Retirement> {
        let range = VirtAddrRange::try_from_start_size(start, size)?;
        let mut cursor = pt.cursor();
        let result = BackendOps::unmap(self, range, &mut cursor);
        drop(cursor);
        match result {
            Ok(retired) => Some(retired),
            Err(err) => {
                warn!("Failed to defer area unmap: {:?}", err);
                None
            }
        }
    }
}

impl Backend {
    pub(crate) fn linux_mapping_kind(&self) -> MappingKind {
        match self {
            Backend::Cow(_) if self.file_mapping().is_some() => MappingKind::FilePrivate,
            Backend::Cow(_) => MappingKind::AnonymousPrivate,
            Backend::Shared(_) if self.mapping_status().has_mapping_owner() => {
                MappingKind::FileShared
            }
            Backend::Shared(_) => MappingKind::AnonymousShared,
            Backend::File(_) => MappingKind::FileShared,
            Backend::Linear(_) => MappingKind::Device,
        }
    }

    pub fn is_shareable(&self) -> bool {
        matches!(
            self,
            Backend::Linear(_) | Backend::Shared(_) | Backend::File(_)
        )
    }

    pub fn ensure_range_covered(&self, start: VirtAddr, size: usize) -> AxResult {
        match self {
            Backend::Linear(backend) => backend.ensure_range_covered(start, size),
            Backend::Cow(_) | Backend::File(_) => Ok(()),
            Backend::Shared(backend) => backend.ensure_range_covered(start, size),
        }
    }

    pub fn faults_with_sigbus(&self, vaddr: VirtAddr) -> bool {
        match self {
            Backend::Cow(backend) => backend.faults_with_sigbus(vaddr),
            Backend::File(backend) => backend.faults_with_sigbus(vaddr),
            Backend::Linear(_) | Backend::Shared(_) => false,
        }
    }

    pub fn cached_page_resident(&self, vaddr: VirtAddr) -> bool {
        match self {
            Backend::Cow(backend) => backend.cached_page_resident(vaddr),
            Backend::File(backend) => backend.cached_page_resident(vaddr),
            Backend::Linear(_) | Backend::Shared(_) => false,
        }
    }

    pub fn is_private_anonymous(&self) -> bool {
        !self.mapping_status().has_mapping_owner()
            && matches!(self, Backend::Cow(backend) if backend.is_private_anonymous())
    }

    fn mapping_status(&self) -> &MappingStatus {
        match self {
            Backend::Linear(backend) => backend.mapping_status(),
            Backend::Cow(backend) => backend.mapping_status(),
            Backend::Shared(backend) => backend.mapping_status(),
            Backend::File(backend) => backend.mapping_status(),
        }
    }

    fn mapping_status_mut(&mut self) -> &mut MappingStatus {
        match self {
            Backend::Linear(backend) => backend.mapping_status_mut(),
            Backend::Cow(backend) => backend.mapping_status_mut(),
            Backend::Shared(backend) => backend.mapping_status_mut(),
            Backend::File(backend) => backend.mapping_status_mut(),
        }
    }

    pub(crate) fn file_mapping(&self) -> Option<&FileMappingLease> {
        self.mapping_status().file_mapping()
    }

    pub(crate) fn file_like_mapping(&self) -> Option<&FileLikeMappingLease> {
        self.mapping_status().file_like_mapping()
    }

    pub(crate) fn shared_file_location(&self) -> Option<&axfs_ng_vfs::Location> {
        match self {
            Self::File(backend) => Some(backend.location()),
            Self::Linear(_) | Self::Cow(_) | Self::Shared(_) => None,
        }
    }

    pub(crate) fn begin_shared_writable_mapping_admission(
        &self,
    ) -> AxResult<Option<file::WritableMappingAdmission>> {
        match self {
            Self::File(backend) => backend.begin_writable_mapping_admission().map(Some),
            Self::Linear(_) | Self::Cow(_) | Self::Shared(_) => Ok(None),
        }
    }

    pub(crate) fn replace_file_mapping(&mut self, file: Option<FileMappingLease>) {
        self.mapping_status_mut().replace_file_mapping(file);
    }

    pub(crate) fn with_file_mapping(mut self, file: FileMappingLease) -> Self {
        self.replace_file_mapping(Some(file));
        self
    }

    pub(crate) fn with_file_like_mapping(mut self, file: FileLikeMappingLease) -> Self {
        self.mapping_status_mut()
            .replace_file_like_mapping(Some(file));
        self
    }

    pub fn supports_user_io_frame_pin(&self) -> bool {
        matches!(self, Backend::Cow(_) | Backend::Shared(_))
    }

    pub fn begin_user_io_pin_window(&self) -> AxResult<Option<CachedFilePinWindow>> {
        match self {
            Backend::File(backend) => backend.begin_user_io_pin_window().map(Some),
            Backend::Linear(_) | Backend::Cow(_) | Backend::Shared(_) => Ok(None),
        }
    }

    pub fn pin_user_io_page_cache(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        dirty_on_release: bool,
    ) -> AxResult<Option<CachedFilePagePin>> {
        match self {
            Backend::File(backend) => Ok(Some(backend.pin_user_io_page(
                vaddr,
                paddr,
                dirty_on_release,
            )?)),
            Backend::Linear(_) | Backend::Cow(_) | Backend::Shared(_) => Ok(None),
        }
    }

    pub fn check_protect_flags(&self, flags: MappingFlags) -> AxResult {
        let requested = flags & (MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE);
        if self
            .file_mapping()
            .is_some_and(|mapping| !mapping.may_protect().contains(requested))
            || self
                .file_like_mapping()
                .is_some_and(|mapping| !mapping.may_protect().contains(requested))
        {
            return Err(AxError::PermissionDenied);
        }
        match self {
            Backend::File(backend) => backend.check_flags(flags),
            Backend::Shared(backend) => backend.check_protect_flags(flags),
            Backend::Linear(_) | Backend::Cow(_) => Ok(()),
        }
    }

    pub fn relocate(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Self> {
        match self {
            Backend::Linear(backend) => backend.relocate(old_start, new_start),
            Backend::Cow(backend) => backend
                .clone_for_range(old_start, new_start)
                .map(Backend::Cow),
            Backend::Shared(backend) => backend
                .clone_for_range(old_start, new_start)
                .map(Backend::Shared),
            Backend::File(backend) => backend
                .clone_for_range(old_start, new_start, aspace)
                .map(Backend::File),
        }
    }

    pub fn duplicate_mapping(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Self> {
        match self {
            Backend::Linear(backend) => backend.duplicate_mapping(old_start, new_start),
            Backend::Cow(backend) => backend
                .duplicate_mapping(old_start, new_start)
                .map(Backend::Cow),
            Backend::Shared(backend) => backend
                .duplicate_mapping(old_start, new_start)
                .map(Backend::Shared),
            Backend::File(backend) => backend
                .duplicate_mapping(old_start, new_start, aspace)
                .map(Backend::File),
        }
    }

    pub fn migrate_present_pages(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        size: usize,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        match self {
            Backend::Linear(_) | Backend::Shared(_) => Ok(()),
            Backend::Cow(backend) => {
                backend.clone_materialized_pages(old_start, new_start, size, pt)
            }
            Backend::File(backend) => {
                backend.clone_materialized_pages(old_start, new_start, size, pt)
            }
        }
    }

    pub fn compatible_with(&self, other: &Self) -> bool {
        let backend_compatible = match (self, other) {
            (Backend::Linear(lhs), Backend::Linear(rhs)) => lhs.compatible_with(rhs),
            (Backend::Cow(lhs), Backend::Cow(rhs)) => lhs.compatible_with(rhs),
            (Backend::Shared(lhs), Backend::Shared(rhs)) => lhs.compatible_with(rhs),
            (Backend::File(lhs), Backend::File(rhs)) => lhs.compatible_with(rhs),
            _ => false,
        };
        backend_compatible
            && self
                .mapping_status()
                .compatible_with(other.mapping_status())
    }

    pub fn mergeable_with(&self, other: &Self) -> bool {
        let backend_mergeable = match (self, other) {
            (Backend::Linear(lhs), Backend::Linear(rhs)) => lhs.compatible_with(rhs),
            (Backend::Cow(lhs), Backend::Cow(rhs)) => lhs.mergeable_with(rhs),
            (Backend::Shared(lhs), Backend::Shared(rhs)) => lhs.compatible_with(rhs),
            (Backend::File(lhs), Backend::File(rhs)) => lhs.compatible_with(rhs),
            _ => false,
        };
        backend_mergeable
            && self
                .mapping_status()
                .compatible_with(other.mapping_status())
    }

    pub fn fault_around_size(&self, access_flags: MappingFlags) -> usize {
        match self {
            Backend::Cow(backend) => backend.fault_around_size(access_flags),
            _ => self.page_size() as usize,
        }
    }

    pub fn sync(&self, data_only: bool) -> AxResult {
        match self {
            Backend::File(backend) => backend.sync(data_only),
            Backend::Linear(_) | Backend::Cow(_) | Backend::Shared(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use axfs::{CachedFile, FileBackend, FileFlags};
    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};

    use super::*;
    use crate::{
        file::{File, FileDescription, FileHandle, FileLike},
        mm::{FileMappingLease, FileMappingSharing},
        pseudofs::tmp::MemoryFs,
        task::UserNamespace,
    };

    #[test]
    fn populate_outcome_runs_deferred_cleanup_on_error() {
        let mut cleanup_calls = 0;
        let outcome = PopulateOutcome::<usize>::new(
            Err(AxError::BadAddress),
            Some(Box::new(|calls| *calls += 1)),
        );

        assert_eq!(outcome.finish(&mut cleanup_calls), Err(AxError::BadAddress));
        assert_eq!(cleanup_calls, 1);
    }

    #[test]
    fn populate_outcome_fails_stop_without_dropping_deferred_ownership() {
        struct DropProbe(Arc<core::sync::atomic::AtomicUsize>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            }
        }

        let drops = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let probe = DropProbe(drops.clone());
        let outcome = PopulateOutcome::<usize>::new(
            Ok(0),
            Some(Box::new(move |_| {
                let _ = &probe;
            })),
        );

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(outcome)));
        assert!(panic.is_err());
        assert_eq!(drops.load(core::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn populate_outcome_fails_stop_if_deferred_cleanup_unwinds() {
        struct DropProbe(Arc<core::sync::atomic::AtomicUsize>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            }
        }

        let drops = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let probe = DropProbe(drops.clone());
        let outcome = PopulateOutcome::<usize>::new(
            Ok(0),
            Some(Box::new(move |_| {
                let _ = &probe;
                panic!("deferred cleanup failed");
            })),
        );

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = outcome.finish(&mut 0);
        }));
        assert!(panic.is_err());
        assert_eq!(drops.load(core::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn every_file_origin_backend_retains_one_mapping_status() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                "mapping-status-backends",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let flags = FileFlags::READ | FileFlags::WRITE;
        let description = FileDescription::new(Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(location.clone()),
            flags,
        ))))
        .unwrap();
        let handle = FileHandle::<dyn FileLike>::from_description_for_test(description)
            .downcast::<File>()
            .unwrap();
        let namespace = UserNamespace::try_new_root().unwrap();
        let lease = FileMappingLease::new(
            handle,
            namespace.clone(),
            VirtAddr::from(0x4000),
            0,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            FileMappingSharing::Private,
        );
        let ofd_key = lease.ofd_key();
        let file = Backend::new_file_for_mapping_status_test(
            VirtAddr::from(0x4000),
            CachedFile::get_or_create(location.clone()),
            flags,
        );
        let shared_pages = Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap());
        let backends = [
            Backend::new_alloc(VirtAddr::from(0x4000), PageSize::Size4K),
            Backend::new_shared(VirtAddr::from(0x4000), shared_pages.clone()),
            Backend::new_linear(VirtAddr::from(0x4000), PhysAddr::from(0x8000), PAGE_SIZE_4K),
            file,
        ];
        // SharedPages uses the kernel mutex in Drop; host unit tests have no
        // current task to acquire it even though this zero-page fixture owns no
        // frames.
        core::mem::forget(shared_pages);

        assert!(backends[0].is_private_anonymous());
        for backend in backends {
            let backend = backend.with_file_mapping(lease.clone());
            assert_eq!(backend.file_mapping().unwrap().ofd_key(), ofd_key);
            assert!(!backend.is_private_anonymous());

            let cloned = backend.clone();
            assert_eq!(cloned.file_mapping().unwrap().ofd_key(), ofd_key);
            assert!(backend.mergeable_with(&cloned));
        }

        let second_description = FileDescription::new(Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(location),
            flags,
        ))))
        .unwrap();
        let second_handle =
            FileHandle::<dyn FileLike>::from_description_for_test(second_description)
                .downcast::<File>()
                .unwrap();
        let second_lease = FileMappingLease::new(
            second_handle,
            namespace,
            VirtAddr::from(0x4000),
            0,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            FileMappingSharing::Private,
        );
        let first =
            Backend::new_alloc(VirtAddr::from(0x4000), PageSize::Size4K).with_file_mapping(lease);
        let second = first.clone().with_file_mapping(second_lease);
        assert!(!first.mergeable_with(&second));
    }

    #[test]
    fn file_mapping_lease_enforces_vm_may_flags_for_cow_and_linear_backends() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                "mapping-protect-lease",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let description = FileDescription::new(Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(location),
            FileFlags::READ,
        ))))
        .unwrap();
        let handle = FileHandle::<dyn FileLike>::from_description_for_test(description)
            .downcast::<File>()
            .unwrap();
        let namespace = UserNamespace::try_new_root().unwrap();
        let start = VirtAddr::from(0x4000);
        let initial = MappingFlags::USER | MappingFlags::READ;
        let shared_read_only = FileMappingLease::new(
            handle.clone(),
            namespace.clone(),
            start,
            0,
            initial,
            MappingFlags::READ,
            FileMappingSharing::Shared,
        );

        for backend in [
            Backend::new_alloc(start, PageSize::Size4K).with_file_mapping(shared_read_only.clone()),
            Backend::new_linear(start, PhysAddr::from(0x8000), PAGE_SIZE_4K)
                .with_file_mapping(shared_read_only),
        ] {
            assert_eq!(
                backend.check_protect_flags(MappingFlags::USER | MappingFlags::WRITE),
                Err(AxError::PermissionDenied)
            );
        }

        let private_cow = FileMappingLease::new(
            handle,
            namespace,
            start,
            0,
            initial,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            FileMappingSharing::Private,
        );
        let private_cow =
            Backend::new_alloc(start, PageSize::Size4K).with_file_mapping(private_cow);
        private_cow
            .check_protect_flags(MappingFlags::USER | MappingFlags::WRITE)
            .unwrap();
    }
}
