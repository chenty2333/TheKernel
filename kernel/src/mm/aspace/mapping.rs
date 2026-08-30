use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axfs::FileFlags;
use axhal::paging::MappingFlags;
use memory_addr::VirtAddr;

use crate::{
    file::{DeferredFileLease, File, FileHandle},
    task::UserNamespace,
};

/// Relocates one affine virtual origin while keeping its backing cursor.
///
/// Moving a suffix below its original prefix can make the old virtual origin
/// unrepresentable. In that case the new virtual origin is rebased to the
/// destination and `backing_advance` identifies the prefix to skip.
pub(super) fn relocate_affine_origin(
    origin: VirtAddr,
    old_start: VirtAddr,
    new_start: VirtAddr,
) -> AxResult<(VirtAddr, usize)> {
    let relocation = thekernel_linux_mm::relocate_affine_origin(
        origin.as_usize(),
        old_start.as_usize(),
        new_start.as_usize(),
    )
    .map_err(super::mm_error)?;
    Ok((
        VirtAddr::from(relocation.origin()),
        relocation.backing_advance(),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileMappingSharing {
    Shared,
    Private,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileMappingIdentity {
    mount_id: u64,
    device: u64,
    inode: u64,
}

#[allow(dead_code)]
impl FileMappingIdentity {
    pub(crate) const fn mount_id(self) -> u64 {
        self.mount_id
    }

    pub(crate) const fn device(self) -> u64 {
        self.device
    }

    pub(crate) const fn inode(self) -> u64 {
        self.inode
    }
}

/// Immutable Linux `vm_file`-style ownership and mapping-time facts.
///
/// Every VMA fragment derived from one file mapping retains a clone of this
/// value. The embedded handle pins the exact open file description after its
/// numeric fd is closed or reused; no later operation needs another fd-table
/// lookup.
#[derive(Clone)]
pub(crate) struct FileMappingLease {
    file: FileHandle<File>,
    ofd_key: u64,
    filesystem_owner_user_ns: Arc<UserNamespace>,
    identity: FileMappingIdentity,
    access_flags: FileFlags,
    status_flags: u32,
    initial_flags: MappingFlags,
    may_protect: MappingFlags,
    sharing: FileMappingSharing,
    mapping_start: VirtAddr,
    file_offset: u64,
    // Keeps the inode's swap admission identity alive across every VMA split,
    // clone and post-close mapping lifetime.
    _swap_mutation_state: Option<Arc<axsync::Mutex<crate::mm::MutationState>>>,
}

#[allow(dead_code)]
impl FileMappingLease {
    pub(crate) fn new(
        file: FileHandle<File>,
        filesystem_owner_user_ns: Arc<UserNamespace>,
        mapping_start: VirtAddr,
        file_offset: u64,
        initial_flags: MappingFlags,
        may_protect: MappingFlags,
        sharing: FileMappingSharing,
    ) -> Self {
        let location = file.inner().location();
        let identity = FileMappingIdentity {
            mount_id: location.mountpoint().mount_id(),
            device: location.mountpoint().device(),
            inode: location.inode(),
        };
        let ofd_key = file.open_file_description_key();
        let access_flags = file.inner().flags();
        let status_flags = file.status_flags();
        let swap_mutation_state = crate::mm::mutation_state_for_mapping(&location);
        Self {
            file,
            ofd_key,
            filesystem_owner_user_ns,
            identity,
            access_flags,
            status_flags,
            initial_flags,
            may_protect,
            sharing,
            mapping_start,
            file_offset,
            _swap_mutation_state: swap_mutation_state,
        }
    }

    pub(crate) const fn file(&self) -> &FileHandle<File> {
        &self.file
    }

    pub(crate) const fn ofd_key(&self) -> u64 {
        self.ofd_key
    }

    pub(crate) const fn filesystem_owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.filesystem_owner_user_ns
    }

    pub(crate) const fn identity(&self) -> FileMappingIdentity {
        self.identity
    }

    pub(crate) const fn access_flags(&self) -> FileFlags {
        self.access_flags
    }

    pub(crate) const fn status_flags(&self) -> u32 {
        self.status_flags
    }

    pub(crate) const fn initial_flags(&self) -> MappingFlags {
        self.initial_flags
    }

    pub(crate) const fn may_protect(&self) -> MappingFlags {
        self.may_protect
    }

    pub(crate) const fn sharing(&self) -> FileMappingSharing {
        self.sharing
    }

    pub(crate) fn file_offset_at(&self, address: VirtAddr) -> Option<u64> {
        let delta = address
            .as_usize()
            .checked_sub(self.mapping_start.as_usize())?;
        self.file_offset.checked_add(delta as u64)
    }

    fn relocated(&self, old_start: VirtAddr, new_start: VirtAddr) -> AxResult<Self> {
        let (mapping_start, backing_advance) =
            relocate_affine_origin(self.mapping_start, old_start, new_start)?;
        let mut relocated = self.clone();
        relocated.mapping_start = mapping_start;
        relocated.file_offset = self
            .file_offset
            .checked_add(u64::try_from(backing_advance).map_err(|_| AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        Ok(relocated)
    }

    pub(super) fn rebased(&self, mapping_start: VirtAddr, file_offset: u64) -> Self {
        let mut rebased = self.clone();
        rebased.mapping_start = mapping_start;
        rebased.file_offset = file_offset;
        rebased
    }

    fn compatible_with(&self, other: &Self) -> bool {
        self.ofd_key == other.ofd_key
            && Arc::ptr_eq(
                &self.filesystem_owner_user_ns,
                &other.filesystem_owner_user_ns,
            )
            && self.identity == other.identity
            && self.access_flags.bits() == other.access_flags.bits()
            && self.status_flags == other.status_flags
            && self.initial_flags == other.initial_flags
            && self.may_protect == other.may_protect
            && self.sharing == other.sharing
            && self.file_offset as u128 + other.mapping_start.as_usize() as u128
                == other.file_offset as u128 + self.mapping_start.as_usize() as u128
    }
}

/// Immutable ownership and permission facts for a non-VFS file-like mapping.
///
/// `owner` is an intrusive deferred lease: VMA cloning is allocation-free and
/// dropping the final fragment only publishes task-context cleanup work.
#[derive(Clone)]
pub(crate) struct FileLikeMappingLease {
    owner: Option<DeferredFileLease>,
    owner_identity: usize,
    ofd_key: u64,
    initial_flags: MappingFlags,
    may_protect: MappingFlags,
    sharing: FileMappingSharing,
    mapping_start: VirtAddr,
    object_offset: u64,
}

#[allow(dead_code)]
impl FileLikeMappingLease {
    pub(crate) fn new(
        owner: DeferredFileLease,
        ofd_key: u64,
        mapping_start: VirtAddr,
        object_offset: u64,
        initial_flags: MappingFlags,
        may_protect: MappingFlags,
        sharing: FileMappingSharing,
    ) -> Self {
        Self {
            owner_identity: owner.identity(),
            owner: Some(owner),
            ofd_key,
            initial_flags,
            may_protect,
            sharing,
            mapping_start,
            object_offset,
        }
    }

    pub(crate) fn new_detached(
        owner_identity: usize,
        ofd_key: u64,
        mapping_start: VirtAddr,
        object_offset: u64,
        initial_flags: MappingFlags,
        may_protect: MappingFlags,
        sharing: FileMappingSharing,
    ) -> Self {
        Self {
            owner: None,
            owner_identity,
            ofd_key,
            initial_flags,
            may_protect,
            sharing,
            mapping_start,
            object_offset,
        }
    }

    pub(crate) const fn ofd_key(&self) -> u64 {
        self.ofd_key
    }

    pub(crate) const fn initial_flags(&self) -> MappingFlags {
        self.initial_flags
    }

    pub(crate) const fn may_protect(&self) -> MappingFlags {
        self.may_protect
    }

    pub(crate) const fn sharing(&self) -> FileMappingSharing {
        self.sharing
    }

    pub(crate) fn object_offset_at(&self, address: VirtAddr) -> Option<u64> {
        let delta = address
            .as_usize()
            .checked_sub(self.mapping_start.as_usize())?;
        self.object_offset.checked_add(delta as u64)
    }

    fn relocated(&self, old_start: VirtAddr, new_start: VirtAddr) -> AxResult<Self> {
        let (mapping_start, backing_advance) =
            relocate_affine_origin(self.mapping_start, old_start, new_start)?;
        let mut relocated = self.clone();
        relocated.mapping_start = mapping_start;
        relocated.object_offset = self
            .object_offset
            .checked_add(u64::try_from(backing_advance).map_err(|_| AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        Ok(relocated)
    }

    fn compatible_with(&self, other: &Self) -> bool {
        matches!(
            (&self.owner, &other.owner),
            (Some(_), Some(_)) | (None, None)
        ) && self.owner_identity == other.owner_identity
            && self.ofd_key == other.ofd_key
            && self.initial_flags == other.initial_flags
            && self.may_protect == other.may_protect
            && self.sharing == other.sharing
            && self.object_offset as u128 + other.mapping_start.as_usize() as u128
                == other.object_offset as u128 + self.mapping_start.as_usize() as u128
    }
}

/// Backend-independent sidecar for Linux-visible VMA ownership.
#[derive(Clone, Default)]
pub(super) struct MappingStatus {
    file: Option<FileMappingLease>,
    file_like: Option<FileLikeMappingLease>,
    sealed: bool,
}

impl MappingStatus {
    pub(super) const fn file_mapping(&self) -> Option<&FileMappingLease> {
        self.file.as_ref()
    }

    pub(super) fn replace_file_mapping(&mut self, file: Option<FileMappingLease>) {
        self.file = file;
    }

    pub(super) const fn file_like_mapping(&self) -> Option<&FileLikeMappingLease> {
        self.file_like.as_ref()
    }

    pub(super) fn replace_file_like_mapping(&mut self, file: Option<FileLikeMappingLease>) {
        self.file_like = file;
    }

    pub(super) const fn has_mapping_owner(&self) -> bool {
        self.file.is_some() || self.file_like.is_some()
    }

    pub(super) const fn is_sealed(&self) -> bool {
        self.sealed
    }

    pub(super) fn set_sealed(&mut self) {
        self.sealed = true;
    }

    /// New mappings derived from an existing backing do not inherit Linux
    /// VM_SEALED unless the operation explicitly preserves VMA metadata.
    pub(super) fn clear_sealed(&mut self) {
        self.sealed = false;
    }

    pub(super) fn relocated(&self, old_start: VirtAddr, new_start: VirtAddr) -> AxResult<Self> {
        Ok(Self {
            file: self
                .file
                .as_ref()
                .map(|file| file.relocated(old_start, new_start))
                .transpose()?,
            file_like: self
                .file_like
                .as_ref()
                .map(|file| file.relocated(old_start, new_start))
                .transpose()?,
            sealed: self.sealed,
        })
    }

    pub(super) fn rebased_file_mapping(
        &self,
        mapping_start: VirtAddr,
        file_offset: u64,
    ) -> AxResult<Self> {
        let file = self.file.as_ref().ok_or(AxError::InvalidInput)?;
        Ok(Self {
            file: Some(file.rebased(mapping_start, file_offset)),
            file_like: self.file_like.clone(),
            sealed: self.sealed,
        })
    }

    pub(super) fn compatible_with(&self, other: &Self) -> bool {
        let file_compatible = match (&self.file, &other.file) {
            (None, None) => true,
            (Some(lhs), Some(rhs)) => lhs.compatible_with(rhs),
            (None, Some(_)) | (Some(_), None) => false,
        };
        let file_like_compatible = match (&self.file_like, &other.file_like) {
            (None, None) => true,
            (Some(lhs), Some(rhs)) => lhs.compatible_with(rhs),
            (None, Some(_)) | (Some(_), None) => false,
        };
        file_compatible && file_like_compatible && self.sealed == other.sealed
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec, vec::Vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Context,
    };

    use axfs::{FileBackend, FileFlags};
    use axfs_ng_vfs::{Location, Mountpoint, NodePermission, NodeType};
    use axhal::paging::MappingFlags;
    use axpoll::{IoEvents, Pollable};
    use memory_addr::{MemoryAddr, VirtAddr};
    use memory_set::{MappingBackend, MappingLineage, MemoryArea, MemorySet};

    use super::{
        super::{PreparedAreaProtect, PreparedProtectRange},
        *,
    };
    use crate::{
        file::{
            FileDescription, FileLike, drain_deferred_description_resource_only_for_test,
            drain_deferred_file_lease_for_test,
        },
        pseudofs::tmp::MemoryFs,
    };

    struct DropCountingMappingOwner {
        drops: Arc<AtomicUsize>,
    }

    impl Drop for DropCountingMappingOwner {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl Pollable for DropCountingMappingOwner {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileLike for DropCountingMappingOwner {
        fn stat(&self) -> AxResult<crate::file::Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<alloc::borrow::Cow<'_, str>> {
            Ok(alloc::borrow::Cow::Borrowed("fixed-mapping-owner"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct LeaseTestBackend {
        mapping_id: u8,
        status: MappingStatus,
    }

    impl MappingBackend for LeaseTestBackend {
        type Addr = VirtAddr;
        type Flags = u8;
        type PageTable = Vec<u8>;

        fn map(
            &self,
            start: VirtAddr,
            size: usize,
            flags: u8,
            page_table: &mut Self::PageTable,
        ) -> bool {
            let range = start.as_usize()..start.as_usize() + size;
            if page_table[range.clone()].iter().any(|entry| *entry != 0) {
                return false;
            }
            page_table[range].fill(flags);
            true
        }

        fn unmap(&self, start: VirtAddr, size: usize, page_table: &mut Self::PageTable) -> bool {
            let range = start.as_usize()..start.as_usize() + size;
            if page_table[range.clone()].contains(&0) {
                return false;
            }
            page_table[range].fill(0);
            true
        }

        fn preflight_unmap(
            &self,
            start: VirtAddr,
            size: usize,
            page_table: &Self::PageTable,
        ) -> bool {
            !page_table[start.as_usize()..start.as_usize() + size].contains(&0)
        }

        fn protect(
            &self,
            start: VirtAddr,
            size: usize,
            new_flags: u8,
            page_table: &mut Self::PageTable,
        ) -> bool {
            let range = start.as_usize()..start.as_usize() + size;
            if page_table[range.clone()].contains(&0) {
                return false;
            }
            page_table[range].fill(new_flags);
            true
        }

        fn can_merge(&self, other: &Self) -> bool {
            self.mapping_id == other.mapping_id && self.status.compatible_with(&other.status)
        }
    }

    fn test_location(name: &str) -> Location {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        mount
            .root_location()
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap()
    }

    fn test_handle(location: Location) -> FileHandle<File> {
        let description = FileDescription::new_with_flags(
            Arc::new(File::new(axfs::File::new(
                FileBackend::Direct(location),
                FileFlags::READ | FileFlags::WRITE,
            ))),
            linux_raw_sys::general::O_APPEND,
        )
        .unwrap();
        FileHandle::<dyn FileLike>::from_description_for_test(description)
            .downcast::<File>()
            .unwrap()
    }

    fn test_lease(
        handle: FileHandle<File>,
        namespace: Arc<UserNamespace>,
        mapping_start: usize,
        file_offset: u64,
    ) -> FileMappingLease {
        FileMappingLease::new(
            handle,
            namespace,
            VirtAddr::from(mapping_start),
            file_offset,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            FileMappingSharing::Private,
        )
    }

    fn status_with(lease: FileMappingLease) -> MappingStatus {
        let mut status = MappingStatus::default();
        status.replace_file_mapping(Some(lease));
        status
    }

    #[test]
    fn sealing_metadata_can_be_cleared_for_a_new_mapping_tail() {
        let mut status = MappingStatus::default();
        status.set_sealed();
        assert!(status.is_sealed());
        status.clear_sealed();
        assert!(!status.is_sealed());
    }

    #[test]
    fn affine_origin_rebases_only_when_the_translated_origin_is_unrepresentable() {
        let origin = VirtAddr::from(0x4000);
        let source = VirtAddr::from(0x8000);

        assert_eq!(
            relocate_affine_origin(origin, source, VirtAddr::from(0x1000)).unwrap(),
            (VirtAddr::from(0x1000), 0x4000)
        );
        assert_eq!(
            relocate_affine_origin(origin, source, VirtAddr::from(0x10_000)).unwrap(),
            (VirtAddr::from(0xc000), 0)
        );
        assert_eq!(
            relocate_affine_origin(
                VirtAddr::from(0x4800),
                VirtAddr::from(0x4000),
                VirtAddr::from(0x1000),
            )
            .unwrap(),
            (VirtAddr::from(0x1800), 0)
        );
    }

    #[test]
    fn split_fragments_retain_exact_ofd_and_restore_merge() {
        let base = VirtAddr::from(0x1000);
        let namespace = UserNamespace::try_new_root().unwrap();
        let lease = test_lease(
            test_handle(test_location("lease-split")),
            namespace,
            base.as_usize(),
            0x8000,
        );
        let expected_ofd = lease.ofd_key();
        let backend = LeaseTestBackend {
            mapping_id: 7,
            status: status_with(lease),
        };
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; 0x6000];
        areas
            .map(
                MemoryArea::new_with_lineage(
                    base,
                    0x3000,
                    1,
                    backend,
                    MappingLineage::new(2).unwrap(),
                ),
                &mut page_table,
                false,
            )
            .unwrap();

        PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: base + 0x1000,
            end: base + 0x2000,
            ranges: vec![PreparedProtectRange {
                start: base + 0x1000,
                end: base + 0x2000,
                flags: 3,
            }],
            max_areas: usize::MAX,
        }
        .commit()
        .unwrap();
        let split: Vec<_> = areas.iter().collect();
        assert_eq!(split.len(), 3);
        for area in split {
            let lease = area.backend().status.file_mapping().unwrap();
            assert_eq!(lease.ofd_key(), expected_ofd);
            assert_eq!(
                lease.file_offset_at(area.start()),
                Some(0x8000 + area.start().sub_addr(base) as u64)
            );
        }

        PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: base + 0x1000,
            end: base + 0x2000,
            ranges: vec![PreparedProtectRange {
                start: base + 0x1000,
                end: base + 0x2000,
                flags: 1,
            }],
            max_areas: usize::MAX,
        }
        .commit()
        .unwrap();
        let merged: Vec<_> = areas.iter().collect();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].backend().status.file_mapping().unwrap().ofd_key(),
            expected_ofd
        );
    }

    #[test]
    fn same_inode_different_ofds_are_not_compatible() {
        let location = test_location("lease-ofd-identity");
        let namespace = UserNamespace::try_new_root().unwrap();
        let first = test_lease(test_handle(location.clone()), namespace.clone(), 0x4000, 0);
        let second = test_lease(test_handle(location), namespace, 0x4000, 0);

        assert_eq!(first.identity(), second.identity());
        assert_ne!(first.ofd_key(), second.ofd_key());
        assert!(!status_with(first).compatible_with(&status_with(second)));
    }

    #[test]
    fn relocation_and_partial_fork_clone_preserve_file_offsets() {
        let base = VirtAddr::from(0x4000);
        let namespace = UserNamespace::try_new_root().unwrap();
        let status = status_with(test_lease(
            test_handle(test_location("lease-relocate")),
            namespace,
            base.as_usize(),
            0x20_000,
        ));

        let forked_suffix = status.clone();
        assert_eq!(
            forked_suffix
                .file_mapping()
                .unwrap()
                .file_offset_at(base + 0x2000),
            Some(0x22_000)
        );

        let relocated_left = status.relocated(base, VirtAddr::from(0x10_000)).unwrap();
        let relocated_right = status
            .relocated(base + 0x1000, VirtAddr::from(0x11_000))
            .unwrap();
        assert!(relocated_left.compatible_with(&relocated_right));
        assert_eq!(
            relocated_right
                .file_mapping()
                .unwrap()
                .file_offset_at(VirtAddr::from(0x11_000)),
            Some(0x21_000)
        );
    }

    #[test]
    fn final_file_like_mapping_release_defers_owner_destruction() {
        let drops = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(DropCountingMappingOwner {
            drops: drops.clone(),
        });
        let description = FileDescription::new(inner.clone()).unwrap();
        let ofd_key = description.id().get();
        let handle = FileHandle::<dyn FileLike>::from_description_for_test(description.clone());
        let retained: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
        let owner = DeferredFileLease::try_new(handle, retained).unwrap();
        assert_eq!(owner.retained_reference_counts(), (2, 1));
        let mapping = FileLikeMappingLease::new(
            owner,
            ofd_key,
            VirtAddr::from(0x4000),
            0,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            FileMappingSharing::Shared,
        );
        let fragment = mapping.clone();

        drop(inner);
        drop(description);
        drop(mapping);
        assert_eq!(drops.load(Ordering::Acquire), 0);

        // The last VMA-side reference only publishes its preallocated node.
        drop(fragment);
        assert_eq!(drops.load(Ordering::Acquire), 0);

        assert!(drain_deferred_file_lease_for_test());
        assert_eq!(drops.load(Ordering::Acquire), 1);
        drain_deferred_description_resource_only_for_test();
    }

    #[test]
    fn detached_file_like_mapping_preserves_sidecar_identity_without_an_ofd_owner() {
        let mapping = FileLikeMappingLease::new_detached(
            17,
            29,
            VirtAddr::from(0x4000),
            0,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            FileMappingSharing::Shared,
        );
        let fragment = mapping.clone();
        let other_mapping = FileLikeMappingLease::new_detached(
            18,
            29,
            VirtAddr::from(0x4000),
            0,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            FileMappingSharing::Shared,
        );

        assert!(mapping.owner.is_none());
        assert!(mapping.compatible_with(&fragment));
        assert!(!mapping.compatible_with(&other_mapping));
    }
}
