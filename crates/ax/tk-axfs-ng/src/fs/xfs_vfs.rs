//! Native VFS projection for verified XFS metadata.
//!
//! Media is first materialized through one claimed-member factory.  It binds
//! the data, optional external-log, and optional realtime queues before any
//! VFS object is published, then takes the sole recovery and write-admission
//! path.  Unsupported media layouts remain an honest read projection rather
//! than an in-memory shadow which could corrupt a real XFS image after
//! remount.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{any::Any, task::Context};

use axerrno::LinuxError;
use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps,
    ExportHandle, FileAttr, FileAttrProvider, FileNode, FileNodeOps, FileRangeOperation,
    FileRangeRequest, Filesystem, FilesystemOps, FsName, Metadata, MetadataUpdate,
    MetadataUpdateCapabilities, NodeFlags, NodeOps, NodePermission, NodeType, NodeUserData,
    ObjectKey, QuotaOps, QuotaUsage, Reference, StatFs, Timestamp, VfsError, VfsResult,
    WeakDirEntry, XattrProvider, XattrSetMode,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use kspin::SpinNoPreempt as SpinMutex;

use super::{
    XFS_ATTR_LOCAL, XFS_ATTR_ROOT, XFS_ATTR_SECURE, XFS_DIFLAG_APPEND, XFS_DIFLAG_EXTSIZE,
    XFS_DIFLAG_EXTSZINHERIT, XFS_DIFLAG_FILESTREAM, XFS_DIFLAG_IMMUTABLE, XFS_DIFLAG_NOATIME,
    XFS_DIFLAG_NODEFRAG, XFS_DIFLAG_NODUMP, XFS_DIFLAG_NOSYMLINKS, XFS_DIFLAG_PREALLOC,
    XFS_DIFLAG_PROJINHERIT, XFS_DIFLAG_REALTIME, XFS_DIFLAG_RTINHERIT, XFS_DIFLAG_SYNC,
    XFS_DIFLAG2_COWEXTSIZE, XFS_DIFLAG2_DAX, XfsError, XfsFileAttr, XfsInode, XfsInodeCoreUpdate,
    XfsMount, XfsNamedInodeOutcome, XfsNewInode, XfsRecoveryJournalCoordinator,
    XfsShortformXattrMode, XfsShortformXattrOutcome, XfsSuperblock, XfsVolume,
};
use crate::MountedBlockDevice;

pub const XFS_SUPER_MAGIC: u32 = 0x5846_5342;

/// Claimed physical members for one XFS durability domain.  This is consumed
/// by the factory, so every failure before VFS publication releases all three
/// claims together.
pub struct XfsMountMembers {
    data: MountedBlockDevice,
    external_log: Option<MountedBlockDevice>,
    realtime: Option<MountedBlockDevice>,
    /// The VFS requested a read-only mount.  This is distinct from the
    /// member snapshots below: a writable device may still be deliberately
    /// mounted read-only, and in that case neither recovery nor a live log
    /// writer may be admitted.
    requested_read_only: bool,
    /// `norecovery` is a stronger form of the read-only request.  It permits
    /// only a verified raw metadata projection and can never gain a writer
    /// through a later remount.
    norecovery: bool,
}

impl XfsMountMembers {
    pub fn new(
        data: MountedBlockDevice,
        external_log: Option<MountedBlockDevice>,
        realtime: Option<MountedBlockDevice>,
    ) -> VfsResult<Self> {
        Self::with_mount_options(data, external_log, realtime, false, false)
    }

    /// Binds physical member capabilities to the mount request.  Claims are
    /// deliberately retained even for a read-only projection so an external
    /// log or realtime member cannot be reconfigured underneath it.
    pub fn with_mount_options(
        data: MountedBlockDevice,
        external_log: Option<MountedBlockDevice>,
        realtime: Option<MountedBlockDevice>,
        requested_read_only: bool,
        norecovery: bool,
    ) -> VfsResult<Self> {
        if norecovery && !requested_read_only {
            return Err(VfsError::InvalidInput);
        }
        let data_identity = data.device().identity_token();
        if external_log
            .as_ref()
            .is_some_and(|member| member.device().identity_token() == data_identity)
            || realtime
                .as_ref()
                .is_some_and(|member| member.device().identity_token() == data_identity)
            || matches!((&external_log, &realtime), (Some(log), Some(rt)) if log.device().identity_token() == rt.device().identity_token())
        {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self {
            data,
            external_log,
            realtime,
            requested_read_only,
            norecovery,
        })
    }

    fn all_members_writable(&self) -> bool {
        !self.data.is_read_only()
            && !self
                .external_log
                .as_ref()
                .is_some_and(MountedBlockDevice::is_read_only)
            && !self
                .realtime
                .as_ref()
                .is_some_and(MountedBlockDevice::is_read_only)
    }

    fn may_recover(&self) -> bool {
        !self.norecovery && self.all_members_writable()
    }

    fn may_start_live_writer(&self) -> bool {
        !self.requested_read_only && !self.norecovery && self.all_members_writable()
    }

    fn volumes(
        &self,
    ) -> VfsResult<(
        axdriver::BlockVolume,
        Option<axdriver::BlockVolume>,
        Option<axdriver::BlockVolume>,
    )> {
        let data = axdriver::BlockVolume::new(vec![self.data.device().clone()])
            .map_err(|error| vfs(XfsError::from(error)))?;
        let external_log = self
            .external_log
            .as_ref()
            .map(|member| {
                axdriver::BlockVolume::new(vec![member.device().clone()])
                    .map_err(|error| vfs(XfsError::from(error)))
            })
            .transpose()?;
        let realtime = self
            .realtime
            .as_ref()
            .map(|member| {
                axdriver::BlockVolume::new(vec![member.device().clone()])
                    .map_err(|error| vfs(XfsError::from(error)))
            })
            .transpose()?;
        Ok((data, external_log, realtime))
    }
}

/// A verified XFS mount.  The successful factory retains every member claim,
/// so device teardown cannot race an open VFS inode or its log/realtime I/O.
pub struct XfsFilesystem {
    volume: Arc<XfsVolume>,
    mount: Option<Arc<XfsMount>>,
    device_id: u64,
    // A root dentry owns an XfsVfsNode, whose stable `filesystem()` reference
    // owns this filesystem.  Cache only the mount-time verified generation;
    // retaining the dentry itself would form a strong mount-claim cycle.
    root: SpinMutex<Option<u32>>,
    self_weak: SpinMutex<Weak<XfsFilesystem>>,
    runtime: SpinMutex<BTreeMap<(u64, u32), Weak<NodeUserData>>>,
    _members: XfsMountMembers,
}

impl XfsFilesystem {
    /// Keeps a VFS observation outside the interval in which the live XFS
    /// coordinator publishes a multi-home-block checkpoint.  Recovered
    /// read-only projections have no live writer and read the verified volume
    /// directly.
    fn coherent_read<T>(
        &self,
        read: impl FnOnce(&XfsVolume) -> super::XfsResult<T>,
    ) -> VfsResult<T> {
        match self.mount.as_ref() {
            Some(mount) => mount.read_coherent(read).map_err(vfs),
            None => read(&self.volume).map_err(vfs),
        }
    }

    fn inode_runtime(&self, inode: u64, generation: u32) -> Arc<NodeUserData> {
        let mut states = self.runtime.lock();
        states.retain(|_, state| state.strong_count() != 0);
        if let Some(existing) = states.get(&(inode, generation)).and_then(Weak::upgrade) {
            return existing;
        }
        states.remove(&(inode, generation));
        let created = Arc::new(NodeUserData::new());
        states.insert((inode, generation), Arc::downgrade(&created));
        created
    }

    pub fn new(device: MountedBlockDevice) -> VfsResult<Filesystem> {
        Self::new_with_members(XfsMountMembers::new(device, None, None)?)
    }

    /// The sole claimed-member materialization path.  `XfsVolume::open`
    /// validates superblock/member geometry before recovery; only after it
    /// succeeds is the member set moved into the published filesystem.
    pub fn new_with_members(members: XfsMountMembers) -> VfsResult<Filesystem> {
        let (data, external_log, realtime) = members.volumes()?;
        let volume = XfsVolume::open(data, external_log, realtime).map_err(vfs)?;
        Self::from_volume(volume, members)
    }

    fn from_volume(volume: Arc<XfsVolume>, members: XfsMountMembers) -> VfsResult<Filesystem> {
        let (scan, plan) = volume.physical_recovery_plan().map_err(vfs)?;
        // `norecovery` never manufactures a clean marker, advances the log,
        // or installs a live writer.  It retains only the scanner-validated
        // raw view; ordinary requested RO may replay first, but only if the
        // complete durability domain is writable.
        if members.requested_read_only {
            if scan.clean || members.norecovery {
                return Self::from_recovered_volume(volume, members);
            }
            // Unlike an explicit `norecovery` mount, ordinary RO still
            // requires replay of a dirty journal.  Publishing pre-replay
            // metadata merely because a member is physically RO would expose
            // an inconsistent image, so fail rather than creating a fake
            // read-only recovery result.
            if !members.all_members_writable() {
                return Err(VfsError::ReadOnlyFilesystem);
            }
            return Self::recover_read_only(volume, members, scan, plan);
        }
        // A read-write request cannot be satisfied by a physically read-only
        // data, log, or realtime member.  Report EROFS at the provider
        // boundary after all role validation/claims, rather than converting
        // it into an early VFS permission denial in the syscall layer.
        if !members.may_start_live_writer() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        // The live writer is v5-only.  Quota-bearing v5 media enters the
        // same coordinator after quota roots have been authenticated by
        // XfsVolume::open; native DQUOT items share every metadata commit.
        if !volume.superblock().is_v5() {
            return Err(VfsError::OperationNotSupported);
        }
        if scan.clean {
            // The scanner did not manufacture this state: it established an
            // empty log or authenticated unmount record.  The earlier RW
            // admission gate established v5 writer support, so this cursor
            // can now seed the only live-log coordinator.
            if let Some(cursor) = scan.cursor {
                let mount = Arc::try_new(XfsMount::new(volume, cursor).map_err(vfs)?)
                    .map_err(|_| VfsError::NoMemory)?;
                return Self::from_mount(mount, members);
            }
            return Err(VfsError::Io);
        }
        // Recovery is journaled again before any v5 home image is published.
        // The coordinator admits the whole typed transaction, so a mixed
        // transaction cannot expose a replayed buffer subset.
        let cursor = scan.cursor.ok_or(VfsError::Io)?;
        let mut coordinator = XfsRecoveryJournalCoordinator::new(volume, cursor).map_err(vfs)?;
        coordinator.replay_plan(&plan).map_err(vfs)?;
        let (volume, cursor) = coordinator.finish().map_err(vfs)?;
        let mount = Arc::try_new(XfsMount::new(volume, cursor).map_err(vfs)?)
            .map_err(|_| VfsError::NoMemory)?;
        Self::from_mount(mount, members)
    }

    /// Replays a dirty log only when every member can participate in the
    /// full FUA/flush durability protocol, then publishes a writer-less read
    /// projection.  This path intentionally shares the normal recovery
    /// coordinator: there is no partial or in-memory-only RO replay.
    fn recover_read_only(
        volume: Arc<XfsVolume>,
        members: XfsMountMembers,
        scan: super::XfsPhysicalLogScan,
        plan: super::XfsRecoveryPlan,
    ) -> VfsResult<Filesystem> {
        debug_assert!(members.may_recover());
        if !volume.superblock().is_v5() {
            volume.replay_v4_whole_image_plan(&plan).map_err(vfs)?;
            return Self::from_recovered_volume(volume, members);
        }
        let cursor = scan.cursor.ok_or(VfsError::Io)?;
        let mut coordinator = XfsRecoveryJournalCoordinator::new(volume, cursor).map_err(vfs)?;
        coordinator.replay_plan(&plan).map_err(vfs)?;
        let (volume, _) = coordinator.finish().map_err(vfs)?;
        Self::from_recovered_volume(volume, members)
    }

    /// Publishes an explicitly recovered writable mount.  Recovery and the
    /// physical cursor are established only by `from_volume` above; keeping
    /// this private prevents callers from bypassing that proof.
    fn from_mount(mount: Arc<XfsMount>, members: XfsMountMembers) -> VfsResult<Filesystem> {
        let volume = mount.volume().clone();
        let device_id = u64::from_be_bytes(
            volume.superblock().uuid.0[..8]
                .try_into()
                .map_err(|_| VfsError::Io)?,
        );
        let fs = Arc::try_new(Self {
            volume,
            mount: Some(mount),
            device_id,
            root: SpinMutex::new(None),
            self_weak: SpinMutex::new(Weak::new()),
            runtime: SpinMutex::new(BTreeMap::new()),
            _members: members,
        })
        .map_err(|_| VfsError::NoMemory)?;
        *fs.self_weak.lock() = Arc::downgrade(&fs);
        let filesystem = Filesystem::try_new(fs.clone())?;
        fs.install_root()?;
        Ok(filesystem)
    }

    /// Publication primitive used only after the recovery implementation has
    /// established that committed log items have reached their home blocks.
    /// Keeping this private prevents a mount caller from bypassing that proof.
    #[allow(dead_code)]
    fn from_recovered_volume(
        volume: Arc<XfsVolume>,
        members: XfsMountMembers,
    ) -> VfsResult<Filesystem> {
        if !volume.superblock().has_dirv2() {
            return Err(VfsError::OperationNotSupported);
        }
        let device_id = u64::from_be_bytes(
            volume.superblock().uuid.0[..8]
                .try_into()
                .map_err(|_| VfsError::Io)?,
        );
        let fs = Arc::try_new(Self {
            volume,
            mount: None,
            device_id,
            root: SpinMutex::new(None),
            self_weak: SpinMutex::new(Weak::new()),
            runtime: SpinMutex::new(BTreeMap::new()),
            _members: members,
        })
        .map_err(|_| VfsError::NoMemory)?;
        *fs.self_weak.lock() = Arc::downgrade(&fs);
        let filesystem = Filesystem::try_new(fs.clone())?;
        fs.install_root()?;
        Ok(filesystem)
    }

    fn install_root(&self) -> VfsResult<()> {
        let inode = self.coherent_read(|volume| volume.inode(volume.superblock().root_inode))?;
        if kind_from_mode(inode.mode) != NodeType::Directory {
            return Err(VfsError::Io);
        }
        *self.root.lock() = Some(inode.generation);
        Ok(())
    }

    fn make_root_entry(self: &Arc<Self>, generation: u32) -> DirEntry {
        let fs = self.clone();
        let inode = self.volume.superblock().root_inode;
        let runtime = self.inode_runtime(inode, generation);
        DirEntry::new_dir(
            move |weak| {
                DirNode::new(Arc::new(XfsVfsNode {
                    fs,
                    inode,
                    generation,
                    runtime,
                    weak: Some(weak),
                }))
            },
            Reference::root(),
        )
    }

    fn make_entry(self: &Arc<Self>, inode: u64, reference: Reference) -> VfsResult<DirEntry> {
        let record = self.coherent_read(|volume| volume.inode(inode))?;
        let kind = kind_from_mode(record.mode);
        if kind == NodeType::Directory {
            let fs = self.clone();
            let runtime = self.inode_runtime(inode, record.generation);
            return Ok(DirEntry::new_dir(
                move |weak| {
                    DirNode::new(Arc::new(XfsVfsNode {
                        runtime,
                        fs,
                        inode,
                        generation: record.generation,
                        weak: Some(weak),
                    }))
                },
                reference,
            ));
        }
        let node = Arc::try_new(XfsVfsNode {
            runtime: self.inode_runtime(inode, record.generation),
            fs: self.clone(),
            inode,
            generation: record.generation,
            weak: None,
        })
        .map_err(|_| VfsError::NoMemory)?;
        DirEntry::try_new_file(FileNode::new(node), kind, reference)
    }
}

impl FilesystemOps for XfsFilesystem {
    fn name(&self) -> &str {
        "xfs"
    }

    fn root_dir(&self) -> DirEntry {
        let generation = self
            .root
            .lock()
            .as_ref()
            .copied()
            .expect("XFS root is verified before filesystem publication");
        let fs = self
            .as_arc()
            .expect("a published XFS filesystem retains its self weak reference");
        fs.make_root_entry(generation)
    }

    fn stat(&self) -> VfsResult<StatFs> {
        let sb = self.volume.superblock();
        let counts = self.coherent_read(|volume| volume.stat_counts())?;
        Ok(StatFs {
            fs_type: XFS_SUPER_MAGIC,
            block_size: sb.block_size,
            blocks: counts.total_blocks,
            blocks_free: counts.free_blocks,
            // This is the verified filesystem-wide allocation view, rather
            // than a credential-specific quota promise.  Per-id dquot
            // accounting is intentionally not claimed by this VFS provider.
            blocks_available: counts.free_blocks,
            file_count: counts.total_inodes,
            free_file_count: counts.free_inodes,
            name_length: 255,
            fragment_size: sb.block_size,
            mount_flags: 0,
        })
    }

    fn encode_export_handle(
        &self,
        entry: &DirEntry,
        _mode: axfs_ng_vfs::ExportHandleMode,
    ) -> VfsResult<ExportHandle> {
        let node = entry.downcast::<XfsVfsNode>()?;
        if !Arc::ptr_eq(&node.fs.volume, &self.volume) {
            return Err(VfsError::InvalidInput);
        }
        let handle = self.coherent_read(|volume| volume.export_handle(node.inode))?;
        Ok(ExportHandle {
            handle_type: 0x5846,
            bytes: handle.encode().to_vec(),
        })
    }

    fn decode_export_handle(&self, handle_type: i32, bytes: &[u8]) -> VfsResult<DirEntry> {
        if handle_type != 0x5846 {
            return Err(VfsError::InvalidInput);
        }
        let handle = super::XfsExportHandle::decode(bytes).map_err(vfs)?;
        let inode = self.coherent_read(|volume| volume.resolve_export_handle(handle))?;
        // An export lookup deliberately has no made-up parent/name.  The VFS
        // keeps it as an anonymous reference until a real path lookup binds it.
        let arc = self.as_arc()?;
        arc.make_entry(inode.number, Reference::anonymous())
    }

    fn flush(&self) -> VfsResult<()> {
        match self.mount.as_ref() {
            Some(mount) => mount.flush_live().map_err(vfs),
            None => self.volume.flush().map_err(vfs),
        }
    }

    fn flush_for_unmount(&self) -> VfsResult<()> {
        match self.mount.as_ref() {
            Some(mount) => mount.clean_unmount().map_err(vfs),
            None => self.volume.flush().map_err(vfs),
        }
    }

    fn unmount(&self) {
        self.root.lock().take();
    }

    fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        if self.mount.is_some() && self.volume.superblock().is_v5() {
            MetadataUpdateCapabilities::MODE
                | MetadataUpdateCapabilities::OWNER
                | MetadataUpdateCapabilities::ATIME
                | MetadataUpdateCapabilities::MTIME
                | MetadataUpdateCapabilities::CTIME
        } else {
            MetadataUpdateCapabilities::empty()
        }
    }
}

impl XfsFilesystem {
    fn as_arc(&self) -> VfsResult<Arc<Self>> {
        // `FilesystemOps` is held in an Arc by VFS, but trait callbacks only
        // receive `&self`.  The self weak reference is initialized before the
        // filesystem is published and avoids making the root dentry a strong
        // self-reference solely to recover that Arc.
        self.self_weak.lock().upgrade().ok_or(VfsError::Io)
    }
}

struct XfsVfsNode {
    fs: Arc<XfsFilesystem>,
    inode: u64,
    /// Captured when the dentry is made.  Never derive this from a later
    /// inode-table lookup: inode numbers are reusable after unlink.
    generation: u32,
    runtime: Arc<NodeUserData>,
    weak: Option<WeakDirEntry>,
}

impl XfsVfsNode {
    fn inode_record(&self) -> VfsResult<XfsInode> {
        let inode = self.fs.coherent_read(|volume| volume.inode(self.inode))?;
        if inode.generation != self.generation {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }

    fn entry(&self, name: &FsName, inode: u64) -> VfsResult<DirEntry> {
        let parent = self
            .weak
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::Io)?;
        self.fs
            .make_entry(inode, Reference::try_new(Some(parent), name)?)
    }

    /// A recovered-volume projection has no live-log coordinator, but its
    /// immutable inode images are still a valid xattr read source.  A live
    /// mount takes its coordinator so list/get cannot observe an attribute
    /// tree halfway through an XFS transaction.
    fn xattrs(&self) -> VfsResult<Vec<super::XfsShortformXattr>> {
        match self.fs.mount.as_ref() {
            Some(mount) => mount.xattrs(self.inode).map_err(vfs),
            None => self.fs.coherent_read(|volume| volume.xattrs(self.inode)),
        }
    }
}

impl NodeOps for XfsVfsNode {
    fn inode(&self) -> u64 {
        self.inode
    }

    fn object_key(&self) -> ObjectKey {
        ObjectKey::new(self.fs.device_id, self.inode, self.generation as u64)
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let inode = self.inode_record()?;
        let rdev = match kind_from_mode(inode.mode) {
            NodeType::CharacterDevice | NodeType::BlockDevice => self
                .fs
                .coherent_read(|volume| volume.inode_rdev(self.inode))?,
            _ => 0,
        };
        metadata(
            inode,
            self.fs.device_id,
            self.fs.volume.superblock().block_size,
            rdev,
        )
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        // Do not silently accept fields whose media transactions do not yet
        // exist.  The mount wrapper normally filters these from setattr, but
        // NodeOps is public and must keep the same fail-closed contract.
        if update.project_id.is_some() || update.rdev.is_some() {
            return Err(VfsError::OperationNotSupported);
        }
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let inode = self.inode_record()?;
        if inode.version < 3 || !self.fs.volume.superblock().is_v5() {
            return Err(VfsError::OperationNotSupported);
        }
        let update = XfsInodeCoreUpdate {
            mode: update.mode.map(|mode| mode.bits()),
            owner: update.owner,
            atime: update
                .atime
                .map(|time| (time.seconds(), time.subsec_nanos())),
            mtime: update
                .mtime
                .map(|time| (time.seconds(), time.subsec_nanos())),
            ctime: update
                .ctime
                .map(|time| (time.seconds(), time.subsec_nanos())),
        };
        mount.update_inode_core(self.inode, update).map_err(vfs)
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        self.fs.flush()
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING
    }
    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }
    fn quota_ops(&self) -> Option<&dyn QuotaOps> {
        Some(self)
    }
    fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
        Some(self)
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(self.runtime.as_ref())
    }
}

impl QuotaOps for XfsVfsNode {
    fn quota_usage(&self) -> VfsResult<QuotaUsage> {
        let inode = self.inode_record()?;
        let state = self.fs.coherent_read(|volume| volume.quota_state())?;
        // Linux's generic object quota query is owner-relative.  The native
        // dquot remains the authority; no synthetic Location ledger is
        // created for an XFS inode.
        let Some(_) = state.roots.user else {
            return Err(VfsError::OperationNotSupported);
        };
        if !self.fs.volume.quota_accounting_enabled(1) {
            return Err(VfsError::OperationNotSupported);
        }
        let dquot = self.fs.coherent_read(|volume| volume.dquot(1, inode.uid))?;
        // xfs_disk_dquot d_bcount is in 512-byte basic blocks, unlike the
        // filesystem's allocation-block geometry.
        let unit = 512u64;
        let used = dquot.blocks.checked_mul(unit).ok_or(VfsError::Io)?;
        let hard_available = if dquot.block_hard == 0 {
            None
        } else {
            Some(
                dquot
                    .block_hard
                    .saturating_sub(dquot.blocks)
                    .checked_mul(unit)
                    .ok_or(VfsError::Io)?,
            )
        };
        let soft_available = if dquot.block_soft == 0 {
            None
        } else {
            Some(
                dquot
                    .block_soft
                    .saturating_sub(dquot.blocks)
                    .checked_mul(unit)
                    .ok_or(VfsError::Io)?,
            )
        };
        Ok(QuotaUsage {
            hard_available,
            soft_available,
            used,
        })
    }
}

impl FileAttrProvider for XfsVfsNode {
    fn get_file_attr(&self) -> VfsResult<FileAttr> {
        let inode = self.inode_record()?;
        Ok(FileAttr {
            // `di_flags` is an on-disk XFS encoding, not the FS_IOC_FSGETXATTR
            // ABI.  Translate explicitly: a few common bits happen to align,
            // but the two flag spaces are not interchangeable as a whole.
            xflags: xfs_xflags(inode.flags, inode.flags2),
            extsize: if inode.flags & (XFS_DIFLAG_EXTSIZE | XFS_DIFLAG_EXTSZINHERIT) != 0 {
                inode
                    .extent_size_hint
                    .checked_mul(self.fs.volume.superblock().block_size)
                    .ok_or(VfsError::Io)?
            } else {
                0
            },
            nextents: inode.data_extents.min(u32::MAX as u64) as u32,
            project_id: inode.project_id,
            cowextsize: if inode.flags2 & XFS_DIFLAG2_COWEXTSIZE != 0 {
                inode
                    .cow_extent_size_hint
                    .checked_mul(self.fs.volume.superblock().block_size)
                    .ok_or(VfsError::Io)?
            } else {
                0
            },
        })
    }

    fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let inode = self.inode_record()?;
        let updated = validate_file_attr(self.fs.volume.superblock(), inode, attr)?;
        let now: Timestamp = wall_time().into();
        mount
            .set_file_attr(self.inode, updated, now.seconds(), now.subsec_nanos())
            .map_err(vfs)
    }

    fn get_legacy_flags(&self) -> VfsResult<u32> {
        Ok(xfs_legacy_flags(self.inode_record()?.flags))
    }
    fn set_legacy_flags(&self, flags: u32) -> VfsResult<()> {
        const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
        const FS_APPEND_FL: u32 = 0x0000_0020;
        const FS_SYNC_FL: u32 = 0x0000_0008;
        const FS_NOATIME_FL: u32 = 0x0000_0080;
        const FS_NODUMP_FL: u32 = 0x0000_0040;
        const FS_PROJINHERIT_FL: u32 = 0x2000_0000;
        const XFS_SETTABLE: u32 = FS_IMMUTABLE_FL
            | FS_APPEND_FL
            | FS_SYNC_FL
            | FS_NOATIME_FL
            | FS_NODUMP_FL
            | FS_PROJINHERIT_FL;
        if flags & !XFS_SETTABLE != 0 {
            return Err(VfsError::OperationNotSupported);
        }
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let inode = self.inode_record()?;
        if flags & FS_PROJINHERIT_FL != 0 && inode.mode & 0o170000 != 0o040000 {
            return Err(LinuxError::EINVAL.into());
        }
        let native = (inode.flags & !(XFS_DIFLAG_LEGACY_MODIFIABLE | XFS_DIFLAG_PROJINHERIT))
            | legacy_flags_to_xfs(flags);
        let now: Timestamp = wall_time().into();
        mount
            .set_file_attr(
                self.inode,
                XfsFileAttr {
                    flags: native,
                    flags2: inode.flags2,
                    project_id: inode.project_id,
                    extent_size_hint: inode.extent_size_hint,
                    cow_extent_size_hint: inode.cow_extent_size_hint,
                },
                now.seconds(),
                now.subsec_nanos(),
            )
            .map_err(vfs)
    }
}

impl XattrProvider for XfsVfsNode {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        let (flags, suffix) = xattr_namespace(name)?;
        self.xattrs()?
            .into_iter()
            .find(|attribute| attribute.flags == flags && attribute.name == suffix)
            .map(|attribute| attribute.value)
            .ok_or(VfsError::NotFound)
    }

    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        let attributes = self.xattrs()?;
        let mut result = Vec::new();
        for attribute in attributes {
            let prefix = xattr_prefix(attribute.flags).ok_or(VfsError::OperationNotSupported)?;
            result
                .try_reserve(prefix.len() + attribute.name.len() + 1)
                .map_err(|_| VfsError::NoMemory)?;
            result.extend_from_slice(prefix);
            result.extend_from_slice(&attribute.name);
            result.push(0);
        }
        Ok(result)
    }

    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let (flags, suffix) = xattr_namespace(name)?;
        let mode = match mode {
            XattrSetMode::Upsert => XfsShortformXattrMode::Upsert,
            XattrSetMode::Create => XfsShortformXattrMode::Create,
            XattrSetMode::Replace => XfsShortformXattrMode::Replace,
            XattrSetMode::CreateAndReplace => XfsShortformXattrMode::CreateAndReplace,
        };
        match mount
            .mutate_xattr(self.inode, flags, suffix, Some(value), mode)
            .map_err(vfs)?
        {
            XfsShortformXattrOutcome::Applied => Ok(()),
            XfsShortformXattrOutcome::Exists => Err(LinuxError::EEXIST.into()),
            XfsShortformXattrOutcome::Missing => Err(VfsError::NotFound),
        }
    }
    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let (flags, suffix) = xattr_namespace(name)?;
        match mount
            .mutate_xattr(
                self.inode,
                flags,
                suffix,
                None,
                XfsShortformXattrMode::Upsert,
            )
            .map_err(vfs)?
        {
            XfsShortformXattrOutcome::Applied => Ok(()),
            XfsShortformXattrOutcome::Exists => Err(VfsError::Io),
            XfsShortformXattrOutcome::Missing => Err(VfsError::NotFound),
        }
    }
}

impl Pollable for XfsVfsNode {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

impl FileNodeOps for XfsVfsNode {
    fn supports_nowait_read(&self) -> bool {
        true
    }
    fn supports_nowait_write(&self) -> bool {
        true
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.fs
            .coherent_read(|volume| volume.read_inode_at(self.inode, offset, buf))
    }
    fn clone_range_from(
        &self,
        source: &dyn NodeOps,
        source_offset: u64,
        destination_offset: u64,
        length: u64,
    ) -> VfsResult<()> {
        if source.object_key().filesystem != self.fs.device_id {
            return Err(VfsError::CrossesDevices);
        }
        let now: Timestamp = wall_time().into();
        self.fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .clone_range(
                source.inode(),
                source_offset,
                self.inode,
                destination_offset,
                length,
                now.seconds(),
                now.subsec_nanos(),
            )
            .map_err(vfs)
    }
    fn dedupe_range_from(
        &self,
        source: &dyn NodeOps,
        source_offset: u64,
        destination_offset: u64,
        length: u64,
    ) -> VfsResult<bool> {
        if source.object_key().filesystem != self.fs.device_id {
            return Err(VfsError::CrossesDevices);
        }
        let now: Timestamp = wall_time().into();
        self.fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .dedupe_range(
                source.inode(),
                source_offset,
                self.inode,
                destination_offset,
                length,
                now.seconds(),
                now.subsec_nanos(),
            )
            .map_err(vfs)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .write_at(self.inode, offset, buf)
            .map_err(vfs)
    }
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        self.fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .append(self.inode, buf)
            .map_err(vfs)
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .truncate(self.inode, len)
            .map_err(vfs)
    }
    fn mutate_range(&self, request: FileRangeRequest) -> VfsResult<()> {
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        match request.operation {
            FileRangeOperation::Allocate { keep_size } => mount
                .fallocate(self.inode, request.offset, request.length, keep_size)
                .map_err(vfs),
            FileRangeOperation::ZeroRange { keep_size } => mount
                .zero_range(self.inode, request.offset, request.length, keep_size)
                .map_err(vfs),
            FileRangeOperation::UnshareRange => mount
                .unshare_range(self.inode, request.offset, request.length)
                .map_err(vfs),
            FileRangeOperation::PunchHole => mount
                .punch_hole(self.inode, request.offset, request.length)
                .map_err(vfs),
            FileRangeOperation::CollapseRange => mount
                .collapse_range(self.inode, request.offset, request.length)
                .map_err(vfs),
            FileRangeOperation::InsertRange => mount
                .insert_range(self.inode, request.offset, request.length)
                .map_err(vfs),
        }
    }
    fn set_symlink(&self, target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        let bytes = target.as_bytes();
        if bytes.iter().any(|byte| *byte == 0) {
            return Err(VfsError::InvalidInput);
        }
        let inode = self.inode_record()?;
        if inode.mode & 0o170000 != 0o120000
            || inode.version < 3
            || !self.fs.volume.superblock().is_v5()
        {
            return Err(VfsError::OperationNotSupported);
        }
        let now: Timestamp = wall_time().into();
        match self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .replace_symlink(self.inode, bytes, now.seconds(), now.subsec_nanos())
        {
            Ok(()) => Ok(()),
            // The target has already passed VFS pathname validation; this
            // remaining range error is the native allocator's inability to
            // reserve a replacement extent, not a missing inode.
            Err(XfsError::AddressOutOfRange) => Err(VfsError::StorageFull),
            Err(error) => Err(vfs(error)),
        }
    }
}

impl DirNodeOps for XfsVfsNode {
    // XFS namespace changes are committed by XfsMount and can also arrive
    // through another VFS alias.  Until the mount exposes a shared native
    // directory change epoch, do not attach the VFS dentry cache to a
    // fabricated per-entry counter.
    fn is_cacheable(&self) -> bool {
        false
    }

    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let mut count = 0;
        if offset == 0 && sink.accept(FsName::new(b"."), self.inode, NodeType::Directory, 1) {
            count += 1;
        }
        let snapshot = self.fs.coherent_read(|volume| {
            let parent = volume.directory_parent(self.inode)?;
            let entries = volume.directory_entries(self.inode)?;
            let mut records = Vec::new();
            records
                .try_reserve_exact(entries.len())
                .map_err(|_| XfsError::NoMemory)?;
            for entry in entries {
                records.push((
                    entry.name,
                    entry.inode,
                    kind_from_mode(volume.inode(entry.inode)?.mode),
                ));
            }
            Ok((parent, records))
        })?;
        if offset <= 1 {
            let parent = snapshot.0;
            if sink.accept(FsName::new(b".."), parent, NodeType::Directory, 2) {
                count += 1;
            }
        }
        for (index, (name, inode, node_type)) in snapshot
            .1
            .into_iter()
            .enumerate()
            .skip(offset.saturating_sub(2) as usize)
        {
            if !sink.accept(FsName::new(&name), inode, node_type, index as u64 + 3) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        let current = self
            .weak
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::Io)?;
        if name.as_bytes() == b"." {
            return Ok(current);
        }
        if name.as_bytes() == b".." {
            return Ok(current.parent().unwrap_or(current));
        }
        let inode = self
            .fs
            .coherent_read(|volume| volume.lookup_directory(self.inode, name.as_bytes()))?;
        self.entry(name, inode.number)
    }

    fn supports_named_create(&self, node_type: NodeType) -> bool {
        self.fs.mount.is_some() && matches!(node_type, NodeType::RegularFile | NodeType::Directory)
    }

    fn supports_symlink(&self) -> bool {
        self.fs.mount.is_some()
    }
    fn supports_hard_links(&self) -> bool {
        self.fs.mount.is_some()
    }
    fn supports_unlink(&self) -> bool {
        self.fs.mount.is_some()
    }
    fn supports_rmdir(&self) -> bool {
        self.fs.mount.is_some()
    }
    fn supports_rename(&self) -> bool {
        self.fs.mount.is_some()
    }

    fn create_named(
        &self,
        name: &FsName,
        options: &axfs_ng_vfs::NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        if !self.supports_named_create(options.node_type) {
            return Err(VfsError::OperationNotSupported);
        }
        validate_xfs_name(name)?;

        // `create_named_inode` commits an inode core and its directory item
        // together, but it has no native ACL/xattr or persistent VFS initial
        // data input.  Reject those requests before entering its transaction;
        // installing them after the name is visible would be a namespace
        // publication bug rather than a partial feature.
        if options.initial_data.is_some()
            || options.initial_attributes.project_inherit
            || options.initial_attributes.access_acl.is_some()
            || options.initial_attributes.default_acl.is_some()
        {
            return Err(VfsError::OperationNotSupported);
        }
        if options.rdev.is_some() {
            return Err(VfsError::InvalidInput);
        }

        let parent = self.inode_record()?;
        let (uid, gid) = options.owner.unwrap_or((parent.uid, parent.gid));
        let project_id = options
            .initial_attributes
            .project_id
            .unwrap_or(parent.project_id);
        let (type_mode, directory_parent) = match options.node_type {
            NodeType::RegularFile => (0o100000, None),
            NodeType::Directory => (0o040000, Some(self.inode)),
            _ => return Err(VfsError::OperationNotSupported),
        };
        let initial = XfsNewInode {
            mode: type_mode | options.permission.bits(),
            uid,
            gid,
            project_id,
            parent: directory_parent,
            symlink_target: None,
        };
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let outcome = match mount.create_named_inode(
            self.inode,
            name.as_bytes(),
            initial,
            disposition == CreateDisposition::Exclusive,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(match error {
                    XfsError::AddressOutOfRange if disposition == CreateDisposition::Exclusive => {
                        VfsError::AlreadyExists
                    }
                    XfsError::AddressOutOfRange => VfsError::StorageFull,
                    error => vfs(error),
                });
            }
        };
        match outcome {
            XfsNamedInodeOutcome::Created(inode) => Ok(CreateOutcome {
                entry: self.entry(name, inode)?,
                created: true,
            }),
            XfsNamedInodeOutcome::Existing(inode) => Ok(CreateOutcome {
                entry: self.entry(name, inode)?,
                created: false,
            }),
        }
    }

    fn create_symlink(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        validate_xfs_name(name)?;
        let bytes = target.as_bytes();
        if bytes.iter().any(|byte| *byte == 0) {
            return Err(VfsError::InvalidInput);
        }
        let parent = self.inode_record()?;
        let (uid, gid) = user.unwrap_or((parent.uid, parent.gid));
        let initial = XfsNewInode {
            mode: 0o120000 | permission.bits(),
            uid,
            gid,
            project_id: parent.project_id,
            parent: None,
            symlink_target: Some(bytes.to_vec()),
        };
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        match mount
            .create_named_inode(self.inode, name.as_bytes(), initial, true)
            .map_err(vfs)?
        {
            XfsNamedInodeOutcome::Created(inode) => self.entry(name, inode),
            XfsNamedInodeOutcome::Existing(_) => Err(VfsError::AlreadyExists),
        }
    }
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        validate_xfs_name(name)?;
        let target = node
            .downcast::<XfsVfsNode>()
            .map_err(|_| VfsError::CrossesDevices)?;
        if !Arc::ptr_eq(&self.fs.volume, &target.fs.volume) {
            return Err(VfsError::CrossesDevices);
        }
        if node.node_type() == NodeType::Directory {
            return Err(VfsError::OperationNotPermitted);
        }
        // The coordinator re-reads the target under its one live-log lock;
        // this preflight only rejects an already stale exported dentry before
        // it can be mistaken for a reusable inode number.
        let current = target.inode_record()?;
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        mount
            .link_named(
                self.inode,
                name.as_bytes(),
                target.inode,
                current.generation,
            )
            .map_err(vfs)?;
        self.entry(name, target.inode)
    }

    fn unlink(&self, request: axfs_ng_vfs::UnlinkRequest<'_>) -> VfsResult<()> {
        validate_xfs_name(request.name)?;
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let observed = self
            .fs
            .coherent_read(|volume| volume.lookup_directory(self.inode, request.name.as_bytes()))?;
        if let Some(expected) = request.expected {
            let expected = expected
                .downcast::<XfsVfsNode>()
                .map_err(|_| VfsError::NotFound)?;
            expected.inode_record()?;
            if !Arc::ptr_eq(&self.fs.volume, &expected.fs.volume)
                || expected.inode != observed.number
                || expected.object_key().generation != observed.generation as u64
            {
                return Err(VfsError::NotFound);
            }
        }
        let is_dir = observed.mode & 0o170000 == 0o040000;
        if is_dir != request.is_dir {
            return Err(VfsError::InvalidInput);
        }
        let expected = request
            .expected
            .map(|_| (observed.number, observed.generation));
        if is_dir {
            mount
                .rmdir_named(self.inode, request.name.as_bytes(), expected)
                .map_err(vfs)
        } else {
            mount
                .unlink_named(self.inode, request.name.as_bytes(), expected)
                .map_err(vfs)
        }
    }

    fn rename(&self, request: axfs_ng_vfs::RenameRequest<'_>) -> VfsResult<()> {
        validate_xfs_name(request.src_name)?;
        validate_xfs_name(request.dst_name)?;
        let destination = request.dst_dir.downcast::<XfsVfsNode>()?;
        if !Arc::ptr_eq(&self.fs.volume, &destination.fs.volume) {
            return Err(VfsError::CrossesDevices);
        }
        let source = request
            .src
            .downcast::<XfsVfsNode>()
            .map_err(|_| VfsError::NotFound)?;
        if !Arc::ptr_eq(&self.fs.volume, &source.fs.volume) {
            return Err(VfsError::CrossesDevices);
        }
        source.inode_record()?;
        let now_source = self.fs.coherent_read(|volume| {
            volume.lookup_directory(self.inode, request.src_name.as_bytes())
        })?;
        if now_source.number != source.inode
            || now_source.generation as u64 != source.object_key().generation
        {
            return Err(VfsError::NotFound);
        }
        let now_destination = self.fs.coherent_read(|volume| {
            volume.lookup_directory(destination.inode, request.dst_name.as_bytes())
        });
        let destination_expected = match (request.dst, now_destination) {
            (None, Err(VfsError::NotFound)) => None,
            (Some(expected), Ok(actual)) => {
                let expected = expected
                    .downcast::<XfsVfsNode>()
                    .map_err(|_| VfsError::NotFound)?;
                expected.inode_record()?;
                if !Arc::ptr_eq(&self.fs.volume, &expected.fs.volume)
                    || actual.number != expected.inode
                    || actual.generation as u64 != expected.object_key().generation
                {
                    return Err(VfsError::NotFound);
                }
                Some((actual.number, actual.generation))
            }
            _ => return Err(VfsError::NotFound),
        };
        let mount = self
            .fs
            .mount
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        mount
            .rename_named(
                self.inode,
                request.src_name.as_bytes(),
                (now_source.number, now_source.generation),
                destination.inode,
                request.dst_name.as_bytes(),
                destination_expected,
            )
            .map_err(vfs)
    }
}

fn validate_xfs_name(name: &FsName) -> VfsResult<()> {
    let bytes = name.as_bytes();
    if bytes.len() > 255 {
        return Err(VfsError::NameTooLong);
    }
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.iter().any(|byte| *byte == 0 || *byte == b'/')
    {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

fn kind_from_mode(mode: u16) -> NodeType {
    NodeType::from(((mode >> 12) & 0xf) as u8)
}

fn xattr_namespace(name: &[u8]) -> VfsResult<(u8, &[u8])> {
    let (flags, suffix) = if let Some(suffix) = name.strip_prefix(b"user.") {
        (XFS_ATTR_LOCAL, suffix)
    } else if let Some(suffix) = name.strip_prefix(b"trusted.") {
        (XFS_ATTR_LOCAL | XFS_ATTR_ROOT, suffix)
    } else if let Some(suffix) = name.strip_prefix(b"security.") {
        (XFS_ATTR_LOCAL | XFS_ATTR_SECURE, suffix)
    } else {
        return Err(VfsError::OperationNotSupported);
    };
    if suffix.is_empty() || suffix.iter().any(|byte| *byte == 0) {
        return Err(VfsError::InvalidInput);
    }
    Ok((flags, suffix))
}

fn xattr_prefix(flags: u8) -> Option<&'static [u8]> {
    match flags & !XFS_ATTR_LOCAL {
        0 => Some(b"user."),
        XFS_ATTR_ROOT => Some(b"trusted."),
        XFS_ATTR_SECURE => Some(b"security."),
        _ => None,
    }
}

/// Translate Linux XFS `di_flags`/`di_flags2` to the `fsxattr.fsx_xflags`
/// ABI.  These bit spaces are deliberately similar rather than identical;
/// keeping the translation at the provider boundary means generic VFS policy
/// sees one Linux ABI regardless of the backing filesystem.
const XFS_DIFLAG_LEGACY_MODIFIABLE: u16 = XFS_DIFLAG_IMMUTABLE
    | XFS_DIFLAG_APPEND
    | XFS_DIFLAG_SYNC
    | XFS_DIFLAG_NOATIME
    | XFS_DIFLAG_NODUMP;
const XFS_DIFLAG_FILEATTR_MUTABLE: u16 = XFS_DIFLAG_LEGACY_MODIFIABLE
    | XFS_DIFLAG_PROJINHERIT
    | XFS_DIFLAG_EXTSIZE
    | XFS_DIFLAG_EXTSZINHERIT;

const FS_XFLAG_IMMUTABLE: u64 = 1 << 3;
const FS_XFLAG_APPEND: u64 = 1 << 4;
const FS_XFLAG_SYNC: u64 = 1 << 5;
const FS_XFLAG_NOATIME: u64 = 1 << 6;
const FS_XFLAG_NODUMP: u64 = 1 << 7;
const FS_XFLAG_PROJINHERIT: u64 = 1 << 9;
const FS_XFLAG_EXTSIZE: u64 = 1 << 11;
const FS_XFLAG_EXTSZINHERIT: u64 = 1 << 12;
const FS_XFLAG_DAX: u64 = 1 << 15;
const FS_XFLAG_COWEXTSIZE: u64 = 1 << 16;
const FS_XFLAG_SETTABLE: u64 = FS_XFLAG_IMMUTABLE
    | FS_XFLAG_APPEND
    | FS_XFLAG_SYNC
    | FS_XFLAG_NOATIME
    | FS_XFLAG_NODUMP
    | FS_XFLAG_PROJINHERIT
    | FS_XFLAG_EXTSIZE
    | FS_XFLAG_EXTSZINHERIT
    | FS_XFLAG_COWEXTSIZE;
const FS_XFLAG_KNOWN: u64 = 0x0001_ffff;

fn xfs_xflags(flags: u16, flags2: u64) -> u64 {
    const FS_XFLAG_REALTIME: u64 = 1 << 0;
    const FS_XFLAG_PREALLOC: u64 = 1 << 1;
    const FS_XFLAG_RTINHERIT: u64 = 1 << 8;
    const FS_XFLAG_NOSYMLINKS: u64 = 1 << 10;
    const FS_XFLAG_NODEFRAG: u64 = 1 << 13;
    const FS_XFLAG_FILESTREAM: u64 = 1 << 14;

    let mut result = 0;
    for (native, abi) in [
        (XFS_DIFLAG_REALTIME, FS_XFLAG_REALTIME),
        (XFS_DIFLAG_PREALLOC, FS_XFLAG_PREALLOC),
        (XFS_DIFLAG_IMMUTABLE, FS_XFLAG_IMMUTABLE),
        (XFS_DIFLAG_APPEND, FS_XFLAG_APPEND),
        (XFS_DIFLAG_SYNC, FS_XFLAG_SYNC),
        (XFS_DIFLAG_NOATIME, FS_XFLAG_NOATIME),
        (XFS_DIFLAG_NODUMP, FS_XFLAG_NODUMP),
        (XFS_DIFLAG_RTINHERIT, FS_XFLAG_RTINHERIT),
        (XFS_DIFLAG_PROJINHERIT, FS_XFLAG_PROJINHERIT),
        (XFS_DIFLAG_NOSYMLINKS, FS_XFLAG_NOSYMLINKS),
        (XFS_DIFLAG_EXTSIZE, FS_XFLAG_EXTSIZE),
        (XFS_DIFLAG_EXTSZINHERIT, FS_XFLAG_EXTSZINHERIT),
        (XFS_DIFLAG_NODEFRAG, FS_XFLAG_NODEFRAG),
        (XFS_DIFLAG_FILESTREAM, FS_XFLAG_FILESTREAM),
    ] {
        if flags & native != 0 {
            result |= abi;
        }
    }
    if flags2 & XFS_DIFLAG2_COWEXTSIZE != 0 {
        result |= FS_XFLAG_COWEXTSIZE;
    }
    if flags2 & XFS_DIFLAG2_DAX != 0 {
        result |= FS_XFLAG_DAX;
    }
    result
}

fn validate_file_attr(
    sb: XfsSuperblock,
    inode: XfsInode,
    mut attr: FileAttr,
) -> VfsResult<XfsFileAttr> {
    let current = xfs_xflags(inode.flags, inode.flags2);
    if attr.xflags & !FS_XFLAG_KNOWN != 0 {
        return Err(VfsError::OperationNotSupported);
    }
    if (attr.xflags ^ current) & !FS_XFLAG_SETTABLE != 0 {
        return Err(VfsError::OperationNotSupported);
    }
    if attr.nextents != 0 && attr.nextents != inode.data_extents.min(u32::MAX as u64) as u32 {
        return Err(LinuxError::EINVAL.into());
    }

    // These are VFS's zero-value normalizations.  Keep them here as well so
    // direct provider callers cannot create an extsize flag with no hint.
    if attr.extsize == 0 {
        attr.xflags &= !(FS_XFLAG_EXTSIZE | FS_XFLAG_EXTSZINHERIT);
    }
    if attr.cowextsize == 0 {
        attr.xflags &= !FS_XFLAG_COWEXTSIZE;
    }
    let mode = inode.mode & 0o170000;
    let regular = mode == 0o100000;
    let directory = mode == 0o040000;
    if attr.xflags & FS_XFLAG_PROJINHERIT != 0 && !directory {
        return Err(LinuxError::EINVAL.into());
    }
    if attr.xflags & FS_XFLAG_EXTSIZE != 0 && !regular {
        return Err(LinuxError::EINVAL.into());
    }
    if attr.xflags & FS_XFLAG_EXTSZINHERIT != 0 && !directory {
        return Err(LinuxError::EINVAL.into());
    }
    if attr.xflags & FS_XFLAG_COWEXTSIZE != 0
        && (!regular && !directory || !sb.features.has_reflink())
    {
        return Err(LinuxError::EINVAL.into());
    }
    let block = sb.block_size;
    let max_hint = sb
        .ag_blocks
        .checked_div(2)
        .and_then(|blocks| blocks.checked_mul(block))
        .ok_or(VfsError::Io)?;
    if attr.extsize != 0 && (attr.extsize % block != 0 || attr.extsize > max_hint) {
        return Err(LinuxError::EINVAL.into());
    }
    if attr.cowextsize != 0 && attr.cowextsize % block != 0 {
        return Err(LinuxError::EINVAL.into());
    }
    let extsize_blocks = if attr.xflags & (FS_XFLAG_EXTSIZE | FS_XFLAG_EXTSZINHERIT) != 0 {
        attr.extsize / block
    } else {
        0
    };
    let cowextsize_blocks = if attr.xflags & FS_XFLAG_COWEXTSIZE != 0 {
        attr.cowextsize / block
    } else {
        0
    };
    if regular && inode.data_extents != 0 && extsize_blocks != inode.extent_size_hint {
        return Err(LinuxError::EINVAL.into());
    }

    let mut flags = inode.flags & !XFS_DIFLAG_FILEATTR_MUTABLE;
    for (xflag, diflag) in [
        (FS_XFLAG_IMMUTABLE, XFS_DIFLAG_IMMUTABLE),
        (FS_XFLAG_APPEND, XFS_DIFLAG_APPEND),
        (FS_XFLAG_SYNC, XFS_DIFLAG_SYNC),
        (FS_XFLAG_NOATIME, XFS_DIFLAG_NOATIME),
        (FS_XFLAG_NODUMP, XFS_DIFLAG_NODUMP),
        (FS_XFLAG_PROJINHERIT, XFS_DIFLAG_PROJINHERIT),
        (FS_XFLAG_EXTSIZE, XFS_DIFLAG_EXTSIZE),
        (FS_XFLAG_EXTSZINHERIT, XFS_DIFLAG_EXTSZINHERIT),
    ] {
        if attr.xflags & xflag != 0 {
            flags |= diflag;
        }
    }
    let mut flags2 = inode.flags2 & !XFS_DIFLAG2_COWEXTSIZE;
    if attr.xflags & FS_XFLAG_COWEXTSIZE != 0 {
        flags2 |= XFS_DIFLAG2_COWEXTSIZE;
    }
    Ok(XfsFileAttr {
        flags,
        flags2,
        project_id: attr.project_id,
        extent_size_hint: extsize_blocks,
        cow_extent_size_hint: cowextsize_blocks,
    })
}

fn legacy_flags_to_xfs(flags: u32) -> u16 {
    const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
    const FS_APPEND_FL: u32 = 0x0000_0020;
    const FS_SYNC_FL: u32 = 0x0000_0008;
    const FS_NOATIME_FL: u32 = 0x0000_0080;
    const FS_NODUMP_FL: u32 = 0x0000_0040;
    const FS_PROJINHERIT_FL: u32 = 0x2000_0000;
    let mut native = 0;
    for (legacy, diflag) in [
        (FS_IMMUTABLE_FL, XFS_DIFLAG_IMMUTABLE),
        (FS_APPEND_FL, XFS_DIFLAG_APPEND),
        (FS_SYNC_FL, XFS_DIFLAG_SYNC),
        (FS_NOATIME_FL, XFS_DIFLAG_NOATIME),
        (FS_NODUMP_FL, XFS_DIFLAG_NODUMP),
    ] {
        if flags & legacy != 0 {
            native |= diflag;
        }
    }
    if flags & FS_PROJINHERIT_FL != 0 {
        native |= XFS_DIFLAG_PROJINHERIT;
    }
    native
}

/// `FS_IOC_GETFLAGS` also differs from `di_flags`: only the common legacy
/// subset belongs in this ABI, and SYNC/NODUMP have different bit positions.
fn xfs_legacy_flags(flags: u16) -> u32 {
    const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
    const FS_APPEND_FL: u32 = 0x0000_0020;
    const FS_SYNC_FL: u32 = 0x0000_0008;
    const FS_NOATIME_FL: u32 = 0x0000_0080;
    const FS_NODUMP_FL: u32 = 0x0000_0040;
    const FS_PROJINHERIT_FL: u32 = 0x2000_0000;
    let mut result = 0;
    if flags & XFS_DIFLAG_IMMUTABLE != 0 {
        result |= FS_IMMUTABLE_FL;
    }
    if flags & XFS_DIFLAG_APPEND != 0 {
        result |= FS_APPEND_FL;
    }
    if flags & XFS_DIFLAG_SYNC != 0 {
        result |= FS_SYNC_FL;
    }
    if flags & XFS_DIFLAG_NOATIME != 0 {
        result |= FS_NOATIME_FL;
    }
    if flags & XFS_DIFLAG_NODUMP != 0 {
        result |= FS_NODUMP_FL;
    }
    if flags & XFS_DIFLAG_PROJINHERIT != 0 {
        result |= FS_PROJINHERIT_FL;
    }
    result
}

fn metadata(inode: XfsInode, device: u64, block_size: u32, rdev: u32) -> VfsResult<Metadata> {
    Ok(Metadata {
        device,
        inode: inode.number,
        nlink: inode.nlink as u64,
        mode: NodePermission::from_bits_truncate(inode.mode),
        node_type: kind_from_mode(inode.mode),
        uid: inode.uid,
        gid: inode.gid,
        project_id: inode.project_id,
        size: inode.size,
        block_size: block_size as u64,
        blocks: inode.blocks,
        rdev: DeviceId(rdev as u64),
        atime: Timestamp::try_new(inode.atime_seconds, inode.atime_nanoseconds)
            .ok_or(VfsError::Io)?,
        btime: Timestamp::try_new(inode.crtime_seconds, inode.crtime_nanoseconds)
            .ok_or(VfsError::Io)?,
        mtime: Timestamp::try_new(inode.mtime_seconds, inode.mtime_nanoseconds)
            .ok_or(VfsError::Io)?,
        ctime: Timestamp::try_new(inode.ctime_seconds, inode.ctime_nanoseconds)
            .ok_or(VfsError::Io)?,
    })
}

fn vfs(error: XfsError) -> VfsError {
    match error {
        XfsError::QuotaExceeded => LinuxError::EDQUOT.into(),
        XfsError::AddressOutOfRange => VfsError::NotFound,
        XfsError::NotEmpty => VfsError::DirectoryNotEmpty,
        XfsError::NoMemory => VfsError::NoMemory,
        XfsError::UnsupportedFeature => VfsError::OperationNotSupported,
        XfsError::Io => VfsError::Io,
        XfsError::InvalidSuperblock | XfsError::CorruptMetadata => VfsError::Io,
    }
}
