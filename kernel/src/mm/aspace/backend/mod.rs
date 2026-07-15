//! Memory mapping backends.
use alloc::{boxed::Box, sync::Arc};

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
use memory_set::MappingBackend;

mod cow;
mod file;
mod linear;
mod phys_pin;
mod shared;

pub(crate) use self::phys_pin::{PhysicalFramePin, pin_frame};
pub use self::shared::SharedPages;
use super::{
    AddrSpace,
    mapping::{FileMappingLease, MappingStatus, relocate_affine_origin},
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

type PopulateCallback = Box<dyn FnOnce(&mut AddrSpace)>;

#[enum_dispatch]
pub trait BackendOps {
    /// Returns the page size of the backend.
    fn page_size(&self) -> PageSize;

    /// Map a memory region.
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pt: &mut PageTableCursor) -> AxResult;

    /// Unmap a memory region.
    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult;

    /// Called before a memory region is protected.
    fn on_protect(
        &self,
        _range: VirtAddrRange,
        _new_flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        Ok(())
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
    ) -> AxResult<(usize, Option<PopulateCallback>)> {
        Ok((0, None))
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
        if let Err(err) = BackendOps::unmap(self, range, &mut pt.cursor()) {
            warn!("Failed to unmap area: {:?}", err);
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
        if let Err(err) = BackendOps::on_protect(self, range, new_flags, &mut cursor) {
            warn!("Failed to protect area: {:?}", err);
            return false;
        }
        cursor
            .protect_region(start, size, page_table_flags(new_flags))
            .is_ok()
    }

    fn can_merge(&self, other: &Self) -> bool {
        self.mergeable_with(other)
    }
}

impl Backend {
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
        self.file_mapping().is_none()
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

    pub(crate) fn replace_file_mapping(&mut self, file: Option<FileMappingLease>) {
        self.mapping_status_mut().replace_file_mapping(file);
    }

    pub(crate) fn with_file_mapping(mut self, file: FileMappingLease) -> Self {
        self.replace_file_mapping(Some(file));
        self
    }

    pub fn supports_user_io_frame_pin(&self) -> bool {
        matches!(self, Backend::Cow(_) | Backend::Shared(_))
    }

    pub fn begin_user_io_pin_window(&self) -> Option<CachedFilePinWindow> {
        match self {
            Backend::File(backend) => Some(backend.begin_user_io_pin_window()),
            Backend::Linear(_) | Backend::Cow(_) | Backend::Shared(_) => None,
        }
    }

    pub fn pin_user_io_page_cache(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
    ) -> AxResult<Option<CachedFilePagePin>> {
        match self {
            Backend::File(backend) => Ok(Some(backend.pin_user_io_page(vaddr, paddr)?)),
            Backend::Linear(_) | Backend::Cow(_) | Backend::Shared(_) => Ok(None),
        }
    }

    pub fn check_protect_flags(&self, flags: MappingFlags) -> AxResult {
        let requested = flags & (MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE);
        if self
            .file_mapping()
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
