use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use axdriver::prelude::BlockPhysicalCompletionRoute;
use axerrno::LinuxError;
use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirectIoAlignment, DirEntry, DirEntrySink, DirNode, DirNodeOps,
    FILE_EXTENT_SCAN_CHUNK_BYTES, FileAttr as VfsFileAttr, FileAttrProvider, FileExtent,
    FileExtentMap, FileExtentState, FileIoCancelOutcome, FileIoPublishError, FileIoPublishPayload,
    FileIoRequest, FileIoRequestAccess, FileNode, FileNodeOps,
    FileRangeOperation, FileRangeRequest, FilesystemOps, FsName, FsNameBuf, FsPath,
    ImmediateFileIoResult, Metadata, MetadataCapabilities, MetadataUpdate, NamedCreateOptions, NodeFlags, NodeOps,
    NodePermission, NodeType, NodeUserData, ObjectKey, PhysicalIoAttempt,
    PhysicalIoNotSubmittedReason, PhysicalIoSegment, PreparedFileIo, PreparedFileIoSubmission,
    Reference, RenameRequest, SubmittedFileIo, SubmittedFileIoControl, UnlinkRequest, VfsError,
    VfsResult, WeakDirEntry, WritebackErrorState, XattrProvider, XattrSetMode,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use lwext4_rust::{
    BlockDevice, Ext4Error, FileAttr, InodeToken, InodeType, PhysicalIoCompletion,
    PhysicalIoEffect, PhysicalIoOperation, PhysicalIoPlan, PhysicalIoPublishOutcome,
    PhysicalIoSettlement, Timestamp as Ext4Timestamp,
    ffi::{EEXIST, ENODATA, ENOENT},
};
use spin::Once;

use super::{
    Ext4FileIoPayload, Ext4Filesystem, RuntimeReservation, WritebackErrorReservation,
    util::{into_vfs_err, into_vfs_type},
};
use crate::highlevel::{PreparedProviderIo, ProviderIoCancelOutcome, SubmittedProviderIo};

const EXT4_FILE_IO_SLOTS: usize = 32;
const EXT4_FILE_IO_BOUNCE_BYTES: usize = 256 * 1024;

/// A single allocation made while preparing a VFS request changes role at
/// publish time, so publication can hand ownership to both queue and caller
/// without allocating a control object.
struct Ext4FileIoSubmission {
    prepared: Option<PreparedProviderIo<Ext4FileIoPayload, EXT4_FILE_IO_SLOTS>>,
    submitted: Option<SubmittedProviderIo<Ext4FileIoPayload, EXT4_FILE_IO_SLOTS>>,
    filesystem: Weak<Ext4Filesystem>,
    ino: u32,
    bounce: Box<[u8]>,
}

impl PreparedFileIoSubmission for Ext4FileIoSubmission {
    fn publish(
        mut self: Box<Self>,
        payload: FileIoPublishPayload,
    ) -> Result<SubmittedFileIo, FileIoPublishError> {
        let prepared = self
            .prepared
            .take()
            .expect("ext4 file I/O submission already published");
        match prepared.begin_publish() {
            Ok(permit) => {
                // This is retained for cancellation/teardown before an
                // Append worker has entered its inode serialization domain.
                // The worker overwrites it with the selected EOF before any
                // data write can complete.
                let actual_offset = payload.geometry().offset();
                let value = Ext4FileIoPayload {
                    payload: payload.commit(),
                    filesystem: self.filesystem.clone(),
                    ino: self.ino,
                    bounce: core::mem::take(&mut self.bounce),
                    actual_offset,
                    result: None,
                };
                let submitted = permit.publish(value);
                self.submitted = Some(submitted);
                let control: Box<dyn SubmittedFileIoControl> = self;
                Ok(SubmittedFileIo::new(control))
            }
            Err(_prepared) => Err(payload.fail(VfsError::ResourceBusy)),
        }
    }

    fn try_complete_immediate(
        mut self: Box<Self>,
        _request: &mut dyn FileIoRequestAccess,
    ) -> VfsResult<ImmediateFileIoResult> {
        // The ext4 lock and block completion path can both sleep.  Do not
        // accidentally turn NOWAIT into a blocking operation.
        self.prepared.take();
        Err(VfsError::WouldBlock)
    }
}

impl SubmittedFileIoControl for Ext4FileIoSubmission {
    fn cancel(mut self: Box<Self>) -> FileIoCancelOutcome {
        match self
            .submitted
            .take()
            .expect("ext4 file I/O control not published")
            .cancel()
        {
            ProviderIoCancelOutcome::Cancelled => FileIoCancelOutcome::Cancelled,
            ProviderIoCancelOutcome::InFlight => FileIoCancelOutcome::InFlight,
            ProviderIoCancelOutcome::Terminal => FileIoCancelOutcome::Terminal,
        }
    }
}

const MAX_PHYSICAL_INPUT_SG: usize = 64;
const MAX_PHYSICAL_IO_BYTES: usize = 256 * 1024;
const PHYSICAL_IO_ALIGNMENT: usize = 512;

// Linux v6.18 `FS_XFLAG_*` values. These deliberately are not the ext4
// on-disk `i_flags` bit positions.
const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
const FS_XFLAG_APPEND: u64 = 0x0000_0010;
const FS_XFLAG_SYNC: u64 = 0x0000_0020;
const FS_XFLAG_NOATIME: u64 = 0x0000_0040;
const FS_XFLAG_NODUMP: u64 = 0x0000_0080;
const FS_XFLAG_PROJINHERIT: u64 = 0x0000_0200;
const EXT4_FILEATTR_FLAGS: u32 = 0x0000_0008 // SYNC
    | 0x0000_0010 // IMMUTABLE
    | 0x0000_0020 // APPEND
    | 0x0000_0040 // NODUMP
    | 0x0000_0080 // NOATIME
    | 0x2000_0000; // PROJINHERIT
const FS_XFLAG_SUPPORTED: u64 = FS_XFLAG_IMMUTABLE
    | FS_XFLAG_APPEND
    | FS_XFLAG_SYNC
    | FS_XFLAG_NOATIME
    | FS_XFLAG_NODUMP
    | FS_XFLAG_PROJINHERIT;

fn ext4_flags_to_xflags(flags: u32) -> u64 {
    let mut xflags = 0;
    if flags & 0x0000_0010 != 0 {
        xflags |= FS_XFLAG_IMMUTABLE;
    }
    if flags & 0x0000_0020 != 0 {
        xflags |= FS_XFLAG_APPEND;
    }
    if flags & 0x0000_0008 != 0 {
        xflags |= FS_XFLAG_SYNC;
    }
    if flags & 0x0000_0080 != 0 {
        xflags |= FS_XFLAG_NOATIME;
    }
    if flags & 0x0000_0040 != 0 {
        xflags |= FS_XFLAG_NODUMP;
    }
    if flags & 0x2000_0000 != 0 {
        xflags |= FS_XFLAG_PROJINHERIT;
    }
    xflags
}

fn xflags_to_ext4_flags(xflags: u64) -> u32 {
    let mut flags = 0;
    if xflags & FS_XFLAG_IMMUTABLE != 0 {
        flags |= 0x0000_0010;
    }
    if xflags & FS_XFLAG_APPEND != 0 {
        flags |= 0x0000_0020;
    }
    if xflags & FS_XFLAG_SYNC != 0 {
        flags |= 0x0000_0008;
    }
    if xflags & FS_XFLAG_NOATIME != 0 {
        flags |= 0x0000_0080;
    }
    if xflags & FS_XFLAG_NODUMP != 0 {
        flags |= 0x0000_0040;
    }
    if xflags & FS_XFLAG_PROJINHERIT != 0 {
        flags |= 0x2000_0000;
    }
    flags
}

fn inode_is_immutable(flags: u32) -> bool {
    flags & 0x0000_0010 != 0
}
fn inode_is_append_only(flags: u32) -> bool {
    flags & 0x0000_0020 != 0
}

pub(super) fn admit_inode_mutation<Hal: lwext4_rust::SystemHal>(
    inode: &lwext4_rust::InodeRef<Hal>,
) -> VfsResult<()> {
    if inode_is_immutable(inode.flags()) {
        Err(LinuxError::EPERM.into())
    } else {
        Ok(())
    }
}
fn to_lwext4_physical_segments(
    segments: &[PhysicalIoSegment],
) -> Option<(
    [lwext4_rust::PhysicalIoSegment; MAX_PHYSICAL_INPUT_SG],
    usize,
    usize,
)> {
    if segments.is_empty() || segments.len() > MAX_PHYSICAL_INPUT_SG {
        return None;
    }
    let mut result = [lwext4_rust::PhysicalIoSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_INPUT_SG];
    let mut ranges = [(0usize, 0usize); MAX_PHYSICAL_INPUT_SG];
    let mut total = 0usize;
    let mut count = 0usize;
    for segment in segments.iter().copied() {
        if segment.len == 0
            || segment.paddr % PHYSICAL_IO_ALIGNMENT != 0
            || segment.len % PHYSICAL_IO_ALIGNMENT != 0
        {
            return None;
        }
        let end = segment.paddr.checked_add(segment.len)?;
        total = total.checked_add(segment.len)?;
        if total > MAX_PHYSICAL_IO_BYTES {
            return None;
        }
        if let Some(previous) = count.checked_sub(1).and_then(|index| result.get_mut(index))
            && previous.paddr.checked_add(previous.len) == Some(segment.paddr)
        {
            previous.len = previous.len.checked_add(segment.len)?;
            ranges[count - 1].1 = previous.paddr.checked_add(previous.len)?;
        } else {
            if count == MAX_PHYSICAL_INPUT_SG {
                return None;
            }
            result[count] = lwext4_rust::PhysicalIoSegment {
                paddr: segment.paddr,
                len: segment.len,
            };
            ranges[count] = (segment.paddr, end);
            count += 1;
        }
    }
    if total == 0 || total % PHYSICAL_IO_ALIGNMENT != 0 || count == 0 {
        return None;
    }
    ranges[..count].sort_unstable_by_key(|range| range.0);
    if ranges[..count].windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return None;
    }
    Some((result, count, total))
}

fn combine_vfs_cleanup<T>(operation: VfsResult<T>, cleanup: VfsResult<()>) -> VfsResult<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(cleanup_err)) => {
            log::error!("secondary ext4 VFS cleanup failure: {cleanup_err}");
            Err(err)
        }
    }
}

fn try_owned(value: &FsName) -> VfsResult<FsNameBuf> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(value.as_bytes().len())
        .map_err(|_| VfsError::NoMemory)?;
    result.extend_from_slice(value.as_bytes());
    FsNameBuf::from_vec(result)
}

fn admit_xattr_set_mode(mode: XattrSetMode, exists: bool) -> lwext4_rust::Ext4Result<()> {
    match (mode, exists) {
        (XattrSetMode::Create, true) => Err(Ext4Error::new(EEXIST as _, "xattr already exists")),
        (XattrSetMode::Replace, false) => Err(Ext4Error::new(ENODATA as _, "xattr does not exist")),
        (XattrSetMode::CreateAndReplace, true) => {
            Err(Ext4Error::new(EEXIST as _, "xattr already exists"))
        }
        (XattrSetMode::CreateAndReplace, false) => {
            Err(Ext4Error::new(ENODATA as _, "xattr does not exist"))
        }
        _ => Ok(()),
    }
}

const fn ext4_named_create_inode_type(node_type: NodeType) -> Option<InodeType> {
    match node_type {
        NodeType::Fifo => Some(InodeType::Fifo),
        NodeType::CharacterDevice => Some(InodeType::CharacterDevice),
        NodeType::Directory => Some(InodeType::Directory),
        NodeType::BlockDevice => Some(InodeType::BlockDevice),
        NodeType::RegularFile => Some(InodeType::RegularFile),
        NodeType::Socket => Some(InodeType::Socket),
        NodeType::Symlink | NodeType::Unknown => None,
    }
}

struct InodeBinding {
    token: InodeToken,
    namespace_epoch: Arc<AtomicU64>,
}

struct RetainedInodeHandle {
    fs: Arc<Ext4Filesystem>,
    token: Option<InodeToken>,
}

impl RetainedInodeHandle {
    fn new(fs: Arc<Ext4Filesystem>, token: InodeToken) -> Self {
        Self {
            fs,
            token: Some(token),
        }
    }

    fn release_on_success(&mut self) {
        self.token = None;
    }
}

impl Drop for RetainedInodeHandle {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.fs.lock().release_inode_handle(token);
        }
    }
}

pub(crate) struct PreparedInodeEntry {
    inode: Arc<Inode>,
    entry: DirEntry,
    runtime_reservation: Option<(RuntimeReservation, Arc<NodeUserData>)>,
    writeback_error_reservation: WritebackErrorReservation,
}

impl PreparedInodeEntry {
    pub(crate) fn install_initial_data(&mut self, options: &NamedCreateOptions) -> VfsResult<()> {
        let Some(initial_data) = options.initial_data.clone() else {
            return Ok(());
        };
        let (_, runtime) = self.runtime_reservation.as_ref().ok_or(VfsError::InvalidData)?;
        runtime.install_initial_data(initial_data)?;
        Ok(())
    }

    pub(crate) fn bind(mut self, token: InodeToken, namespace_epoch: Arc<AtomicU64>) -> DirEntry {
        if let Some((reservation, runtime)) = self.runtime_reservation.take() {
            let runtime = reservation.commit(token, runtime);
            self.inode.attach_runtime(runtime);
        }
        self.inode.bind(
            token,
            namespace_epoch,
            self.writeback_error_reservation.commit(token),
        );
        self.entry
    }
}

pub struct Inode {
    fs: Arc<Ext4Filesystem>,
    binding: Once<InodeBinding>,
    this: Once<WeakDirEntry>,
    runtime: Once<Arc<NodeUserData>>,
    writeback_errors: Once<Arc<WritebackErrorState>>,
}

impl Inode {
    pub(crate) fn export_handle(&self) -> Option<InodeToken> {
        self.token()
    }

    pub(crate) fn filesystem(&self) -> Arc<Ext4Filesystem> {
        self.fs.clone()
    }

    pub(crate) fn try_finish_exported_entry(
        fs: Arc<Ext4Filesystem>,
        token: InodeToken,
        namespace_epoch: Arc<AtomicU64>,
        inode_type: InodeType,
    ) -> VfsResult<DirEntry> {
        let mut retained = RetainedInodeHandle::new(fs.clone(), token);
        let writeback = fs.reserve_writeback_error_state(Some(token))?;
        match Self::try_prepare_entry_with_writeback_error_reservation(
            fs.clone(),
            inode_type,
            Reference::anonymous(),
            Some(token),
            writeback,
        ) {
            Ok(prepared) => {
                if let Some(runtime) = fs.runtime_attachment(token) {
                    prepared.inode.attach_runtime(runtime);
                }
                let entry = prepared.bind(token, namespace_epoch);
                retained.release_on_success();
                Ok(entry)
            }
            Err(error) => Err(error),
        }
    }
    pub(crate) fn try_prepare_entry(
        fs: Arc<Ext4Filesystem>,
        inode_type: InodeType,
        reference: Reference,
    ) -> VfsResult<PreparedInodeEntry> {
        let writeback_error_reservation = fs.reserve_writeback_error_state(None)?;
        Self::try_prepare_entry_with_writeback_error_reservation(
            fs,
            inode_type,
            reference,
            None,
            writeback_error_reservation,
        )
    }

    fn try_prepare_entry_with_writeback_error_reservation(
        fs: Arc<Ext4Filesystem>,
        inode_type: InodeType,
        reference: Reference,
        token: Option<InodeToken>,
        writeback_error_reservation: WritebackErrorReservation,
    ) -> VfsResult<PreparedInodeEntry> {
        // Every native fileattr provider needs one inode-generation-stable
        // mutation gate, including ordinary files without initial FIFO data.
        // Reopened aliases reuse the registry owner; new identities reserve
        // allocation before any namespace publication.
        let existing_runtime = token.and_then(|token| fs.runtime_attachment(token));
        let runtime_reservation = if existing_runtime.is_some() {
            None
        } else {
            let runtime = Arc::try_new(NodeUserData::new()).map_err(|_| VfsError::NoMemory)?;
            Some((fs.reserve_runtime_attachment()?, runtime))
        };
        let inode = Arc::try_new(Self {
            fs,
            binding: Once::new(),
            this: Once::new(),
            runtime: Once::new(),
            writeback_errors: Once::new(),
        })
        .map_err(|_| VfsError::NoMemory)?;
        if let Some(runtime) = existing_runtime {
            inode.attach_runtime(runtime);
        }
        let entry = if inode_type == InodeType::Directory {
            let entry = DirEntry::try_new_dir(DirNode::new(inode.clone()), reference)?;
            inode.this.call_once(|| entry.downgrade());
            entry
        } else {
            DirEntry::try_new_file(
                FileNode::new(inode.clone()),
                into_vfs_type(inode_type),
                reference,
            )?
        };
        Ok(PreparedInodeEntry {
            inode,
            entry,
            runtime_reservation,
            writeback_error_reservation,
        })
    }

    fn bind(
        &self,
        token: InodeToken,
        namespace_epoch: Arc<AtomicU64>,
        writeback_errors: Arc<WritebackErrorState>,
    ) {
        self.binding.call_once(|| InodeBinding {
            token,
            namespace_epoch,
        });
        let installed = self.writeback_errors.call_once(|| writeback_errors.clone());
        debug_assert!(Arc::ptr_eq(installed, &writeback_errors));
    }

    fn attach_runtime(&self, runtime: Arc<NodeUserData>) {
        let installed = self.runtime.call_once(|| runtime.clone());
        debug_assert!(Arc::ptr_eq(installed, &runtime));
    }

    fn token(&self) -> Option<InodeToken> {
        self.binding.get().map(|binding| binding.token)
    }

    fn ino(&self) -> u32 {
        self.token().map_or(0, InodeToken::ino)
    }

    fn try_prepare_child(
        &self,
        inode_type: InodeType,
        name: FsNameBuf,
    ) -> VfsResult<PreparedInodeEntry> {
        Self::try_prepare_entry(
            self.fs.clone(),
            inode_type,
            Reference::new(self.this.get().and_then(WeakDirEntry::upgrade), name),
        )
    }

    fn try_prepare_retained_child(
        &self,
        token: InodeToken,
        inode_type: InodeType,
        name: FsNameBuf,
    ) -> VfsResult<PreparedInodeEntry> {
        let writeback_error_reservation = self.fs.reserve_writeback_error_state(Some(token))?;
        Self::try_prepare_entry_with_writeback_error_reservation(
            self.fs.clone(),
            inode_type,
            Reference::new(self.this.get().and_then(WeakDirEntry::upgrade), name),
            Some(token),
            writeback_error_reservation,
        )
    }

    /// Completes a retained low-level inode identity outside the ext4 spin
    /// lock. If VFS allocation fails, the retained handle is released before
    /// returning to the caller.
    fn try_finish_retained_entry(
        &self,
        token: InodeToken,
        namespace_epoch: Arc<AtomicU64>,
        inode_type: InodeType,
        name: FsNameBuf,
    ) -> VfsResult<DirEntry> {
        match self.try_prepare_retained_child(token, inode_type, name) {
            Ok(prepared) => {
                if let Some(runtime) = self.fs.runtime_attachment(token) {
                    prepared.inode.attach_runtime(runtime);
                }
                Ok(prepared.bind(token, namespace_epoch))
            }
            Err(err) => {
                self.fs.lock().release_inode_handle(token);
                Err(err)
            }
        }
    }

    fn try_finish_retained_name(
        &self,
        token: InodeToken,
        namespace_epoch: Arc<AtomicU64>,
        inode_type: InodeType,
        name: &FsName,
    ) -> VfsResult<DirEntry> {
        match try_owned(name) {
            Ok(name) => self.try_finish_retained_entry(token, namespace_epoch, inode_type, name),
            Err(err) => {
                self.fs.lock().release_inode_handle(token);
                Err(err)
            }
        }
    }

    fn bump_namespace_epoch(&self) {
        if let Some(binding) = self.binding.get() {
            binding.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn namespace_epoch_handle(&self) -> Option<&Arc<AtomicU64>> {
        self.binding.get().map(|binding| &binding.namespace_epoch)
    }

    fn expected_token(&self, expected: &DirEntry) -> Option<InodeToken> {
        expected
            .downcast::<Self>()
            .ok()
            .and_then(|expected| Arc::ptr_eq(&self.fs, &expected.fs).then(|| expected.token()))
            .flatten()
    }
}

impl NodeOps for Inode {
    fn inode(&self) -> u64 {
        self.ino() as _
    }

    fn object_key(&self) -> ObjectKey {
        let token = self.token();
        ObjectKey::new(
            Arc::as_ptr(&self.fs) as usize as u64,
            token.map_or(self.ino() as u64, |token| token.ino() as u64),
            token.map_or(0, |token| token.generation() as u64),
        )
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut attr = FileAttr::default();
        let project_id = self
            .fs
            .lock()
            .with_inode_ref(self.ino(), |inode| {
                inode.get_attr(&mut attr);
                Ok(inode.project_id())
            })
            .map_err(into_vfs_err)?;
        Ok(Metadata {
            inode: self.ino() as _,
            device: attr.device,
            nlink: attr.nlink,
            mode: NodePermission::from_bits_truncate(attr.mode as u16),
            node_type: into_vfs_type(attr.node_type),
            uid: attr.uid,
            gid: attr.gid,
            project_id,
            size: attr.size,
            block_size: attr.block_size,
            blocks: attr.blocks,
            rdev: DeviceId(attr.rdev),
            atime: axfs_ng_vfs::Timestamp::new(attr.atime.seconds(), attr.atime.subsec_nanos()),
            btime: axfs_ng_vfs::Timestamp::new(attr.btime.seconds(), attr.btime.subsec_nanos()),
            mtime: axfs_ng_vfs::Timestamp::new(attr.mtime.seconds(), attr.mtime.subsec_nanos()),
            ctime: axfs_ng_vfs::Timestamp::new(attr.ctime.seconds(), attr.ctime.subsec_nanos()),
        })
    }

    fn metadata_capabilities(&self, metadata: &Metadata) -> MetadataCapabilities {
        MetadataCapabilities {
            // metadata() reads the native ext4 crtime/crtime_extra fields.
            birth_time: true,
            direct_io_alignment: (metadata.node_type == NodeType::RegularFile).then_some(
                DirectIoAlignment {
                    memory: PHYSICAL_IO_ALIGNMENT as u32,
                    offset: PHYSICAL_IO_ALIGNMENT as u32,
                },
            ),
        }
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        // Validate every requested timestamp before touching the inode: an
        // out-of-range ext4 value must fail atomically, never publish a mode
        // or owner change and then discover a time encoding failure.
        for time in [update.atime, update.mtime, update.ctime]
            .into_iter()
            .flatten()
        {
            if !Ext4Timestamp::new(time.seconds(), time.subsec_nanos()).is_ext4_representable() {
                return Err(LinuxError::EOVERFLOW.into());
            }
        }
        let mut fs = self.fs.lock();
        fs.with_inode_ref_mut(self.ino(), |inode| {
            if update.mode.is_some()
                || update.owner.is_some()
                || update.project_id.is_some()
                || update.rdev.is_some()
                || update.atime.is_some()
                || update.mtime.is_some()
                || update.ctime.is_some()
            {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 inode",
                    )
                })?;
                if inode_is_append_only(inode.flags()) {
                    return Err(lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "append-only ext4 inode",
                    ));
                }
            }
            let mut status_changed = false;
            if let Some(mode) = update.mode {
                inode.set_mode((inode.mode() & !0xfff) | (mode.bits() as u32));
                status_changed = true;
            }
            if let Some((uid, gid)) = update.owner {
                inode.set_owner(uid as _, gid as _);
                status_changed = true;
            }
            if let Some(project_id) = update.project_id {
                inode.set_project_id(project_id);
                status_changed = true;
            }
            if let Some(rdev) = update.rdev {
                inode.set_rdev(rdev.0);
                status_changed = true;
            }
            if let Some(atime) = update.atime {
                inode.set_atime(Ext4Timestamp::new(atime.seconds(), atime.subsec_nanos()));
            }
            if let Some(mtime) = update.mtime {
                inode.set_mtime(Ext4Timestamp::new(mtime.seconds(), mtime.subsec_nanos()));
                status_changed = true;
            }
            if let Some(ctime) = update.ctime {
                inode.set_ctime(Ext4Timestamp::new(ctime.seconds(), ctime.subsec_nanos()));
            } else if status_changed {
                inode.update_ctime();
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        Ok(())
    }

    fn len(&self) -> VfsResult<u64> {
        self.fs
            .lock()
            .with_inode_ref(self.ino(), |inode| Ok(inode.size()))
            .map_err(into_vfs_err)
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn sync(&self, data_only: bool) -> VfsResult<()> {
        // lwext4 exposes writeback at the filesystem level. Reopen-after-sync
        // workloads require previous updates to be visible, so route inode sync through the
        // ext4-wide flush path.
        if data_only {
            crate::highlevel::record_file_sync_data_only_metadata_fallback();
        }
        self.fs.lock().flush().map_err(into_vfs_err)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING
    }

    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        self.runtime.get().map(Arc::as_ref)
    }

    fn writeback_error_state(&self) -> VfsResult<Arc<WritebackErrorState>> {
        self.writeback_errors.get().cloned().ok_or(VfsError::Io)
    }

    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }

    fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
        Some(self)
    }
}

impl FileAttrProvider for Inode {
    fn get_file_attr(&self) -> VfsResult<VfsFileAttr> {
        let mut fs = self.fs.lock();
        let (flags, project_id) = fs
            .with_inode_ref(self.ino(), |inode| Ok((inode.flags(), inode.project_id())))
            .map_err(into_vfs_err)?;
        Ok(VfsFileAttr {
            xflags: ext4_flags_to_xflags(flags),
            extsize: 0,
            // Linux ext4 fileattr reports flags and project ID, not FIEMAP.
            nextents: 0,
            project_id,
            cowextsize: 0,
        })
    }

    fn try_get_file_attr(&self) -> VfsResult<VfsFileAttr> {
        let mut fs = self.fs.try_lock().ok_or(VfsError::WouldBlock)?;
        let (flags, project_id) = fs
            .with_inode_ref(self.ino(), |inode| Ok((inode.flags(), inode.project_id())))
            .map_err(into_vfs_err)?;
        Ok(VfsFileAttr {
            xflags: ext4_flags_to_xflags(flags),
            extsize: 0,
            // Match the blocking Linux-compatible fileattr observation.
            nextents: 0,
            project_id,
            cowextsize: 0,
        })
    }

    fn set_file_attr(&self, attr: VfsFileAttr) -> VfsResult<()> {
        // Like Linux ext4, only flags and project ID are consumed. The
        // generic VFS validates extent flags; unflagged numeric hints and
        // the output-only extent count do not change inode allocation.
        if attr.xflags & !FS_XFLAG_SUPPORTED != 0 {
            return Err(VfsError::OperationNotSupported);
        }
        self.fs
            .lock()
            .with_inode_ref_mut(self.ino(), |inode| {
                if attr.xflags & FS_XFLAG_PROJINHERIT != 0 && !inode.is_dir() {
                    return Err(lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EINVAL as _,
                        "project inheritance requires an ext4 directory",
                    ));
                }
                let old_flags = inode.flags();
                inode.set_flags(
                    (old_flags & !EXT4_FILEATTR_FLAGS) | xflags_to_ext4_flags(attr.xflags),
                );
                inode.set_project_id(attr.project_id);
                inode.update_ctime();
                Ok(())
            })
            .map_err(into_vfs_err)
    }

    fn get_legacy_flags(&self) -> VfsResult<u32> {
        const FS_FL_USER_VISIBLE: u32 = 0x0003_dfff;
        self.fs
            .lock()
            .with_inode_ref(self.ino(), |inode| Ok(inode.flags() & FS_FL_USER_VISIBLE))
            .map_err(into_vfs_err)
    }

    fn set_legacy_flags(&self, flags: u32) -> VfsResult<()> {
        const FS_FL_USER_VISIBLE: u32 = 0x0003_dfff;
        const FS_FL_USER_MODIFIABLE: u32 = 0x0003_80ff;
        if flags & !FS_FL_USER_VISIBLE != 0 {
            return Err(VfsError::OperationNotSupported);
        }
        self.fs
            .lock()
            .with_inode_ref_mut(self.ino(), |inode| {
                let old = inode.flags();
                // Preserve read-only visible and private ext4 bits exactly, while
                // applying only the Linux USER_MODIFIABLE subset.
                inode.set_flags((old & !FS_FL_USER_MODIFIABLE) | (flags & FS_FL_USER_MODIFIABLE));
                inode.update_ctime();
                Ok(())
            })
            .map_err(into_vfs_err)
    }
}

impl XattrProvider for Inode {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<alloc::vec::Vec<u8>> {
        self.fs
            .lock()
            .with_inode_ref(self.ino(), |inode| inode.get_xattr(name))
            .map_err(into_vfs_err)
    }

    fn list_xattrs(&self) -> VfsResult<alloc::vec::Vec<u8>> {
        self.fs
            .lock()
            .with_inode_ref(self.ino(), |inode| inode.list_xattrs())
            .map_err(into_vfs_err)
    }

    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        self.fs
            .lock()
            .with_inode_ref_mut(self.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 inode",
                    )
                })?;
                if inode_is_append_only(inode.flags()) {
                    return Err(lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "append-only ext4 inode",
                    ));
                }
                let exists = inode.has_xattr(name)?;
                admit_xattr_set_mode(mode, exists)?;
                inode.set_xattr(name, value)
            })
            .map_err(into_vfs_err)
    }

    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        self.fs
            .lock()
            .with_inode_ref_mut(self.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 inode",
                    )
                })?;
                if inode_is_append_only(inode.flags()) {
                    return Err(lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "append-only ext4 inode",
                    ));
                }
                inode.remove_xattr(name)
            })
            .map_err(into_vfs_err)
    }
}

impl Drop for Inode {
    fn drop(&mut self) {
        if let Some(binding) = self.binding.get() {
            if let Some(writeback_errors) = self.writeback_errors.get() {
                self.fs
                    .release_writeback_error_state(binding.token, writeback_errors);
            }
            self.fs.lock().release_inode_handle(binding.token);
        }
    }
}

impl Inode {
    /// Builds and publishes one all-extents physical batch without retaining
    /// the ext4 filesystem lock while the device is allowed to run.
    ///
    /// `None` is the only pre-publication fallback result.  A returned
    /// submission, including a terminal partial publication, must be kept by
    /// the caller until its handles have quiesced.
    unsafe fn submit_prepared_physical_effect_route(
        &self,
        effect: &mut PhysicalIoEffect,
        route: BlockPhysicalCompletionRoute,
    ) -> VfsResult<PhysicalIoPublishOutcome> {
        let mut disk = self.fs.physical_disk();
        unsafe {
            effect.publish_with(|requests| match route {
                BlockPhysicalCompletionRoute::Exact => disk.submit_physical_batch(requests),
                BlockPhysicalCompletionRoute::Kernel => disk.submit_physical_batch_kernel(requests),
            })
        }
        .map_err(into_vfs_err)
    }

    unsafe fn submit_prepared_physical_effect(
        &self,
        effect: &mut PhysicalIoEffect,
    ) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe {
            self.submit_prepared_physical_effect_route(effect, BlockPhysicalCompletionRoute::Exact)
        }
    }

    unsafe fn submit_prepared_physical_effect_kernel(
        &self,
        effect: &mut PhysicalIoEffect,
    ) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe {
            self.submit_prepared_physical_effect_route(effect, BlockPhysicalCompletionRoute::Kernel)
        }
    }

    fn finalize_prepared_physical_effect(
        &self,
        effect: &mut PhysicalIoEffect,
        outcome: PhysicalIoPublishOutcome,
    ) -> VfsResult<usize> {
        let publication = match outcome {
            PhysicalIoPublishOutcome::Published(publication)
            | PhysicalIoPublishOutcome::Terminal(publication) => publication,
            PhysicalIoPublishOutcome::NotSubmitted(_) => return Err(VfsError::Io),
        };
        let disk = self.fs.physical_disk();
        let mut completions = [PhysicalIoCompletion {
            handle: 0,
            cookie: 0,
            bytes: 0,
            success: false,
        }; 32];
        let mut retired = 0usize;
        while retired < publication.count() {
            let drain = disk
                .wait_physical_completions_exact(publication, &mut completions)
                .map_err(into_vfs_err)?;
            if drain.completed == 0 || drain.completed > completions.len() {
                return Err(VfsError::Io);
            }
            for completion in completions.iter().copied().take(drain.completed) {
                // The shared device owner has already demultiplexed by this
                // effect's exact raw handle/cookie.  A foreign completion is
                // retained in the device mailbox and never reaches this
                // state machine; status is the concrete lower observation,
                // never a count-only synthetic completion.
                match effect.record_completion(completion) {
                    lwext4_rust::PhysicalIoCompletionOutcome::Accepted => {
                        retired = retired.saturating_add(1);
                    }
                    lwext4_rust::PhysicalIoCompletionOutcome::Retain(_) => {
                        let _ = self.fs.physical_disk().reset_device();
                        return Err(VfsError::Io);
                    }
                }
                if retired == publication.count() {
                    break;
                }
            }
        }
        let settlement = effect.settle();
        let success = match settlement {
            PhysicalIoSettlement::Settled { success, .. } => success,
            PhysicalIoSettlement::Retain(_) => return Err(VfsError::Io),
        };
        let plan = effect.mark_finalized().map_err(into_vfs_err)?;
        if success {
            self.fs.finalize_physical_io_plan(plan, true)?;
            Ok(plan.bytes())
        } else {
            Err(VfsError::Io)
        }
    }

    fn prepare_physical_plan(
        &self,
        operation: PhysicalIoOperation,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<PhysicalIoPlan>> {
        let Some((physical, count, total)) = to_lwext4_physical_segments(segments) else {
            return Ok(None);
        };
        if operation == PhysicalIoOperation::Write {
            self.fs
                .lock()
                .with_inode_ref(self.ino(), |inode| {
                    admit_inode_mutation(inode).map_err(|_| {
                        lwext4_rust::Ext4Error::new(
                            lwext4_rust::ffi::EPERM as _,
                            "immutable ext4 inode",
                        )
                    })?;
                    if inode_is_append_only(inode.flags()) && offset != inode.size() {
                        return Err(lwext4_rust::Ext4Error::new(
                            lwext4_rust::ffi::EPERM as _,
                            "append-only ext4 inode",
                        ));
                    }
                    Ok(())
                })
                .map_err(into_vfs_err)?;
        }
        self.fs
            .prepare_physical_io_plan(self.ino(), operation, offset, total, &physical[..count])
    }

    pub(crate) fn prepare_owned_physical_effect(
        &self,
        operation: PhysicalIoOperation,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<PhysicalIoEffect>> {
        Ok(self
            .prepare_physical_plan(operation, segments, offset)?
            .map(PhysicalIoEffect::new))
    }

    pub(crate) unsafe fn publish_owned_physical_effect(
        &self,
        effect: &mut PhysicalIoEffect,
    ) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe { self.submit_prepared_physical_effect(effect) }
    }

    /// Publishes an io_uring-owned effect to the device-global completion
    /// worker. Synchronous direct effects continue to use the exact route.
    pub(crate) unsafe fn publish_owned_physical_effect_kernel(
        &self,
        effect: &mut PhysicalIoEffect,
    ) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe { self.submit_prepared_physical_effect_kernel(effect) }
    }

    // Physical-I/O submission path in progress.
    #[allow(dead_code)]
    pub(crate) fn finalize_owned_physical_effect(
        &self,
        effect: &mut PhysicalIoEffect,
        completions: &[PhysicalIoCompletion],
    ) -> VfsResult<usize> {
        let settlement = self.settle_owned_physical_effect(effect, completions);
        let PhysicalIoSettlement::Settled { plan, success } = settlement else {
            return Err(VfsError::Io);
        };
        debug_assert_eq!(plan, effect.plan());
        let plan = effect.mark_finalized().map_err(into_vfs_err)?;
        if success {
            self.fs.finalize_physical_io_plan(plan, true)?;
            Ok(plan.bytes())
        } else {
            Err(VfsError::Io)
        }
    }

    /// Applies exact completion observations without releasing the upper
    /// owner's range/cache/pin resources.  A malformed observation is a
    /// quarantine reason; later exact completions may still retire the
    /// published handles, at which point settlement is a typed failure.
    pub(crate) fn settle_owned_physical_effect(
        &self,
        effect: &mut PhysicalIoEffect,
        completions: &[PhysicalIoCompletion],
    ) -> PhysicalIoSettlement {
        for completion in completions.iter().copied() {
            let _ = effect.record_completion(completion);
        }
        effect.settle()
    }

    /// Revalidates a physically retired effect, then marks it finalized.  A
    /// successful physical write keeps its staged cache invalidation committed
    /// even when metadata/mapping revalidation reports an error; restoring
    /// that old cache would be stale.
    pub(crate) fn finalize_settled_physical_effect(
        &self,
        effect: &mut PhysicalIoEffect,
        plan: PhysicalIoPlan,
        success: bool,
    ) -> VfsResult<usize> {
        debug_assert_eq!(plan, effect.plan());
        // Revalidation/commit can report a transient ResourceBusy while a
        // nested filesystem operation is still in flight. Leave the lower
        // effect in Completed/SettledFailure for that typed retry; marking it
        // Finalized before the filesystem call would drop the DMA/cache
        // owners and turn the Busy into a terminal CQE.
        let result = if success {
            self.fs.finalize_physical_io_plan(plan, true)
        } else {
            Err(VfsError::Io)
        };
        if matches!(&result, Err(VfsError::ResourceBusy)) {
            return result.map(|()| plan.bytes());
        }
        let plan = effect.mark_finalized().map_err(into_vfs_err)?;
        result.map(|()| plan.bytes())
    }
}

impl FileNodeOps for Inode {
    fn prepare_file_io(
        &self,
        request: FileIoRequest,
        completion: Box<dyn axfs_ng_vfs::OwnedFileIoCompletion>,
    ) -> Result<PreparedFileIo, axfs_ng_vfs::FileIoPrepareError> {
        let prepared = match self.fs.file_io_queue().prepare_reserve() {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(axfs_ng_vfs::FileIoPrepareError::new(
                    error, request, completion,
                ));
            }
        };
        let bounce_len = request.len().min(EXT4_FILE_IO_BOUNCE_BYTES);
        let mut bounce = Vec::new();
        if bounce.try_reserve_exact(bounce_len).is_err() {
            return Err(axfs_ng_vfs::FileIoPrepareError::new(
                VfsError::NoMemory,
                request,
                completion,
            ));
        }
        bounce.resize(bounce_len, 0);
        let submission = match Box::try_new(Ext4FileIoSubmission {
            prepared: Some(prepared),
            submitted: None,
            filesystem: Arc::downgrade(&self.fs),
            ino: self.ino(),
            bounce: bounce.into_boxed_slice(),
        }) {
            Ok(submission) => submission,
            Err(_) => {
                return Err(axfs_ng_vfs::FileIoPrepareError::new(
                    VfsError::NoMemory,
                    request,
                    completion,
                ));
            }
        };
        Ok(PreparedFileIo::new(request, completion, submission))
    }

    fn supports_nowait_read(&self) -> bool {
        true
    }
    fn supports_nowait_write(&self) -> bool {
        true
    }

    fn mutate_range(&self, request: FileRangeRequest) -> VfsResult<()> {
        let size = <Self as NodeOps>::len(self)?;
        let end = request.end();
        let mut fs = self.fs.lock();
        match request.operation {
            FileRangeOperation::Allocate { keep_size } => fs
                .allocate_range(self.ino(), request.offset, request.length, keep_size)
                .map_err(into_vfs_err),
            FileRangeOperation::PunchHole => fs
                .punch_hole(self.ino(), request.offset, request.length)
                .map_err(into_vfs_err),
            FileRangeOperation::ZeroRange { keep_size } => fs
                .zero_range(self.ino(), request.offset, request.length, keep_size)
                .map_err(into_vfs_err),
            FileRangeOperation::CollapseRange => {
                if !request.offset.is_multiple_of(4096)
                    || !request.length.is_multiple_of(4096)
                    || end > size
                {
                    return Err(VfsError::InvalidInput);
                }
                fs.collapse_range(self.ino(), request.offset, request.length)
                    .map_err(into_vfs_err)
            }
            FileRangeOperation::InsertRange => {
                if !request.offset.is_multiple_of(4096)
                    || !request.length.is_multiple_of(4096)
                    || request.offset >= size
                {
                    return Err(VfsError::InvalidInput);
                }
                fs.insert_range(self.ino(), request.offset, request.length)
                    .map_err(into_vfs_err)
            }
            FileRangeOperation::UnshareRange => {
                if end > size {
                    return Err(VfsError::InvalidInput);
                }
                // ext4 has no reflink data extents.  A successfully validated
                // range is therefore already exclusively owned.
                Ok(())
            }
        }
    }

    fn syncfs_writeback_error_state(&self) -> Option<Arc<WritebackErrorState>> {
        self.fs.syncfs_writeback_error_state()
    }

    fn max_extent_bytes(&self) -> VfsResult<u64> {
        Ok(self.fs.lock().max_extent_bytes())
    }

    fn supports_extent_mapping(&self) -> bool {
        true
    }

    fn map_extents(&self, start: u64, length: u64, max_extents: usize) -> VfsResult<FileExtentMap> {
        let file_size = <Self as NodeOps>::len(self)?;
        let requested_end = start.checked_add(length).unwrap_or(u64::MAX);
        let end = requested_end.min(file_size);
        if length == 0 || start >= file_size || end <= start {
            return Ok(FileExtentMap::new(Vec::new(), 0, true));
        }

        let mut retained: Vec<FileExtent> = Vec::new();
        let mut count_only_last = None;
        let mut count_only_total = 0u32;
        let mut complete = true;
        let mut reaches_eof = false;
        let mut cursor = start;

        while cursor < end {
            let chunk_end = cursor
                .checked_add(FILE_EXTENT_SCAN_CHUNK_BYTES)
                .unwrap_or(end)
                .min(end);
            let chunk_length = chunk_end - cursor;
            let remaining_capacity = max_extents.saturating_sub(retained.len());
            let chunk_capacity = if max_extents == 0 {
                0
            } else {
                remaining_capacity
            };
            let result = self
                .fs
                .lock()
                .map_extents(self.ino(), cursor, chunk_length, chunk_capacity)
                .map_err(into_vfs_err)?;
            let lwext4_rust::ExtentMap {
                extents: lower_extents,
                mapped_extents: chunk_mapped,
                complete: chunk_complete,
                reaches_eof: chunk_reaches_eof,
                first_extent,
                last_extent,
            } = result;
            reaches_eof = chunk_reaches_eof;

            if max_extents == 0 {
                count_only_total = count_only_total
                    .checked_add(chunk_mapped)
                    .ok_or(VfsError::InvalidInput)?;
            } else if chunk_capacity == 0 {
                if chunk_mapped != 0 {
                    complete = false;
                    break;
                }
            } else {
                for extent in lower_extents {
                    let extent = FileExtent::new(
                        extent.logical,
                        extent.physical,
                        extent.length,
                        match extent.state {
                            lwext4_rust::ExtentState::Written => FileExtentState::Written,
                            lwext4_rust::ExtentState::Unwritten => FileExtentState::Unwritten,
                        },
                    );
                    if let Some(previous) = retained.last_mut()
                        && previous.logical.checked_add(previous.length) == Some(extent.logical)
                        && previous.physical.checked_add(previous.length) == Some(extent.physical)
                        && previous.state == extent.state
                    {
                        previous.length = previous
                            .length
                            .checked_add(extent.length)
                            .ok_or(VfsError::InvalidInput)?;
                    } else if retained.len() < max_extents {
                        retained.push(extent);
                    }
                }
                if !chunk_complete {
                    complete = false;
                    break;
                }
            }

            if max_extents == 0 {
                // Count-only results do not allocate an extent vector. Keep
                // just the previous chunk's tail so a split extent is counted
                // once when the next bounded lock-held chunk starts.
                let previous = count_only_last;
                let first = first_extent.map(|extent| {
                    FileExtent::new(
                        extent.logical,
                        extent.physical,
                        extent.length,
                        match extent.state {
                            lwext4_rust::ExtentState::Written => FileExtentState::Written,
                            lwext4_rust::ExtentState::Unwritten => FileExtentState::Unwritten,
                        },
                    )
                });
                count_only_last = last_extent.map(|extent| {
                    FileExtent::new(
                        extent.logical,
                        extent.physical,
                        extent.length,
                        match extent.state {
                            lwext4_rust::ExtentState::Written => FileExtentState::Written,
                            lwext4_rust::ExtentState::Unwritten => FileExtentState::Unwritten,
                        },
                    )
                });
                if let (Some(previous), Some(first)) = (previous, first)
                    && previous.logical.checked_add(previous.length) == Some(first.logical)
                    && previous.physical.checked_add(previous.length) == Some(first.physical)
                    && previous.state == first.state
                    && chunk_mapped != 0
                {
                    count_only_total = count_only_total.saturating_sub(1);
                }
            }

            cursor = chunk_end;
        }

        if max_extents == 0 {
            let mut result = FileExtentMap::new(Vec::new(), count_only_total, complete);
            result.reaches_eof = complete && cursor == end && reaches_eof;
            return Ok(result);
        }
        let mapped_extents = u32::try_from(retained.len()).map_err(|_| VfsError::InvalidInput)?;
        let mut result = FileExtentMap::new(retained, mapped_extents, complete);
        result.reaches_eof = complete && cursor == end && reaches_eof;
        Ok(result)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if {
            let fs = self.fs.lock();
            fs.is_block_aligned_range(offset, buf.len())
        } {
            self.fs
                .lock()
                .read_at_aligned_hot(self.ino(), buf, offset)
                .map_err(into_vfs_err)
        } else {
            self.fs
                .lock()
                .read_at(self.ino(), buf, offset)
                .map_err(into_vfs_err)
        }
    }

    fn read_at_vectored(&self, bufs: &mut [&mut [u8]], mut offset: u64) -> VfsResult<usize> {
        let len = bufs.iter().map(|buf| buf.len()).sum();
        if {
            let fs = self.fs.lock();
            fs.is_block_aligned_range(offset, len)
        } {
            let mut fs = self.fs.lock();
            if let Some(read) = fs
                .read_at_aligned_hot_vectored(self.ino(), bufs, offset)
                .map_err(into_vfs_err)?
            {
                return Ok(read);
            }
        }

        let mut fs = self.fs.lock();
        let mut total = 0usize;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let read = match fs.read_at(self.ino(), buf, offset).map_err(into_vfs_err) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += read;
            offset = offset
                .checked_add(read as u64)
                .ok_or(VfsError::InvalidInput)?;
            if read < requested || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    fn try_read_at_vectored_async(
        &self,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let submission = {
            let mut fs = self.fs.lock();
            fs.read_at_aligned_hot_vectored_async_submit(self.ino(), bufs, offset)
                .map_err(into_vfs_err)?
        };
        let Some(submission) = submission else {
            return Ok(None);
        };
        self.fs.wait_async_read(&submission)?;
        Ok(Some(submission.bytes))
    }

    unsafe fn try_read_at_physical_with_reason(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        let Some(plan) = self.prepare_physical_plan(PhysicalIoOperation::Read, segments, offset)?
        else {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoNotSubmittedReason::Extent,
            ));
        };
        let mut effect = PhysicalIoEffect::new(plan);
        let outcome = unsafe { self.submit_prepared_physical_effect(&mut effect) }?;
        if matches!(outcome, PhysicalIoPublishOutcome::NotSubmitted(_)) {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoNotSubmittedReason::DeviceAdmission,
            ));
        }
        Ok(PhysicalIoAttempt::Completed(
            self.finalize_prepared_physical_effect(&mut effect, outcome)?,
        ))
    }

    unsafe fn try_read_at_physical(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_read_at_physical_with_reason(segments, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    fn physical_read_eligible(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<bool> {
        Ok(self
            .prepare_physical_plan(PhysicalIoOperation::Read, segments, offset)?
            .is_some())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(lwext4_rust::ffi::EPERM as _, "immutable ext4 inode")
            })?;
            if inode_is_append_only(inode.flags()) && offset != inode.size() {
                return Err(lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "append-only ext4 inode",
                ));
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        if fs.is_block_aligned_range(offset, buf.len()) {
            fs.write_at_aligned_hot(self.ino(), buf, offset)
        } else {
            fs.write_at(self.ino(), buf, offset)
        }
        .map_err(into_vfs_err)
    }

    fn write_at_vectored(&self, bufs: &[&[u8]], mut offset: u64) -> VfsResult<usize> {
        let len = bufs.iter().map(|buf| buf.len()).sum();
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(lwext4_rust::ffi::EPERM as _, "immutable ext4 inode")
            })?;
            if inode_is_append_only(inode.flags()) && offset != inode.size() {
                return Err(lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "append-only ext4 inode",
                ));
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        if fs.is_block_aligned_range(offset, len)
            && let Some(written) = fs
                .write_at_aligned_hot_vectored(self.ino(), bufs, offset)
                .map_err(into_vfs_err)?
        {
            return Ok(written);
        }

        let mut total = 0usize;
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = match fs.write_at(self.ino(), buf, offset).map_err(into_vfs_err) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            offset = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    fn try_write_at_vectored_async(
        &self,
        bufs: &[&[u8]],
        offset: u64,
    ) -> VfsResult<axfs_ng_vfs::AsyncVectoredWriteOutcome> {
        let submission = {
            let mut fs = self.fs.lock();
            fs.with_inode_ref(self.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 inode",
                    )
                })?;
                if inode_is_append_only(inode.flags()) && offset != inode.size() {
                    return Err(lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "append-only ext4 inode",
                    ));
                }
                Ok(())
            })
            .map_err(into_vfs_err)?;
            fs.write_at_aligned_hot_vectored_async_submit(self.ino(), bufs, offset)
                .map_err(into_vfs_err)?
        };
        let Some(submission) = submission else {
            return Ok(axfs_ng_vfs::AsyncVectoredWriteOutcome::NotSubmitted);
        };
        match self.fs.wait_async_write(&submission) {
            Ok(()) => Ok(axfs_ng_vfs::AsyncVectoredWriteOutcome::Completed(
                submission.bytes,
            )),
            Err(error) => Ok(axfs_ng_vfs::AsyncVectoredWriteOutcome::CompletionError(
                error,
            )),
        }
    }

    unsafe fn try_write_at_physical_with_reason(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        let Some(plan) =
            self.prepare_physical_plan(PhysicalIoOperation::Write, segments, offset)?
        else {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoNotSubmittedReason::Extent,
            ));
        };
        let mut effect = PhysicalIoEffect::new(plan);
        let outcome = unsafe { self.submit_prepared_physical_effect(&mut effect) }?;
        if matches!(outcome, PhysicalIoPublishOutcome::NotSubmitted(_)) {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoNotSubmittedReason::DeviceAdmission,
            ));
        }
        Ok(PhysicalIoAttempt::Completed(
            self.finalize_prepared_physical_effect(&mut effect, outcome)?,
        ))
    }

    unsafe fn try_write_at_physical(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_write_at_physical_with_reason(segments, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    fn physical_write_eligible(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<bool> {
        Ok(self
            .prepare_physical_plan(PhysicalIoOperation::Write, segments, offset)?
            .is_some())
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let mut fs = self.fs.lock();
        let length = fs
            .with_inode_ref(self.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 inode",
                    )
                })?;
                Ok(inode.size())
            })
            .map_err(into_vfs_err)?;
        let written = fs.write_at(self.ino(), buf, length).map_err(into_vfs_err)?;
        Ok((written, length + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(lwext4_rust::ffi::EPERM as _, "immutable ext4 inode")
            })?;
            if inode_is_append_only(inode.flags()) {
                return Err(lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "append-only ext4 inode",
                ));
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        fs.set_len(self.ino(), len).map_err(into_vfs_err)
    }

    fn set_len_failure_is_atomic(&self) -> bool {
        // ext4_fs_truncate_inode updates allocation metadata incrementally;
        // Fs::set_len poisons metadata after a failed shrinking truncate.
        false
    }

    fn set_symlink(&self, target: &FsPath) -> VfsResult<()> {
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(lwext4_rust::ffi::EPERM as _, "immutable ext4 symlink")
            })?;
            if inode_is_append_only(inode.flags()) {
                return Err(lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "append-only ext4 symlink",
                ));
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        fs.set_symlink(self.ino(), target.as_bytes())
            .map_err(into_vfs_err)
    }
}

impl Pollable for Inode {
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

impl DirNodeOps for Inode {
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        ext4_named_create_inode_type(node_type).is_some()
    }

    fn supports_symlink(&self) -> bool {
        true
    }

    fn supports_hard_links(&self) -> bool {
        true
    }

    fn supports_unlink(&self) -> bool {
        true
    }

    fn supports_rmdir(&self) -> bool {
        true
    }

    fn supports_rename(&self) -> bool {
        true
    }

    fn namespace_epoch(&self) -> u64 {
        self.binding
            .get()
            .map_or(0, |binding| binding.namespace_epoch.load(Ordering::Acquire))
    }

    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        let mut reader = fs.read_dir(self.ino(), offset).map_err(into_vfs_err)?;
        let operation = (|| {
            let mut count = 0;
            while let Some(entry) = reader.current() {
                // `entry` borrows the iterator's current record.  Advance the
                // iterator only after copying every value the VFS callback
                // needs, so the callback observes the post-entry directory
                // cursor without retaining a borrow into `reader`.
                let name = entry.name().to_vec();
                let ino = entry.ino() as u64;
                let node_type = into_vfs_type(entry.inode_type());
                reader.step().map_err(into_vfs_err)?;
                if !sink.accept(FsName::new(&name), ino, node_type, reader.offset()) {
                    break;
                }
                count += 1;
            }
            Ok(count)
        })();
        combine_vfs_cleanup(operation, reader.finish().map_err(into_vfs_err))
    }

    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        let entry_name = try_owned(name)?;
        let (token, namespace_epoch, inode_type) = {
            let mut fs = self.fs.lock();
            let (ino, inode_type) = fs
                .lookup_inode(self.ino(), name.as_bytes())
                .map_err(into_vfs_err)?;
            let (token, namespace_epoch) = fs.retain_inode_handle(ino).map_err(into_vfs_err)?;
            (token, namespace_epoch, inode_type)
        };
        self.try_finish_retained_entry(token, namespace_epoch, inode_type, entry_name)
    }

    fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        let inode_type = match ext4_named_create_inode_type(options.node_type) {
            Some(inode_type) => inode_type,
            None if options.node_type == NodeType::Symlink => {
                return Err(VfsError::OperationNotSupported);
            }
            None => return Err(VfsError::InvalidData),
        };

        // Fast existing-name path: retain the stable identity while serialized,
        // then build its VFS node after releasing the ext4 spin lock.
        let existing = {
            let mut fs = self.fs.lock();
            fs.with_inode_ref(self.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 directory",
                    )
                })
            })
            .map_err(into_vfs_err)?;
            match fs.lookup_inode(self.ino(), name.as_bytes()) {
                Ok((ino, existing_type)) => {
                    if disposition == CreateDisposition::Exclusive {
                        return Err(VfsError::AlreadyExists);
                    }
                    let (token, namespace_epoch) =
                        fs.retain_inode_handle(ino).map_err(into_vfs_err)?;
                    Some((token, namespace_epoch, existing_type))
                }
                Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => None,
                Err(err) => return Err(into_vfs_err(err)),
            }
        };
        if let Some((token, namespace_epoch, existing_type)) = existing {
            let entry =
                self.try_finish_retained_name(token, namespace_epoch, existing_type, name)?;
            return Ok(CreateOutcome {
                entry,
                created: false,
            });
        }

        let entry_name = try_owned(name)?;
        let mut prepared = self.try_prepare_child(inode_type, entry_name)?;
        prepared.install_initial_data(options)?;
        let mut fs = self.fs.lock();
        // Allocation above opened a race window. Revalidate under the same
        // serialization used by the backend create before committing anything.
        match fs.lookup_inode(self.ino(), name.as_bytes()) {
            Ok((ino, existing_type)) => {
                if disposition == CreateDisposition::Exclusive {
                    return Err(VfsError::AlreadyExists);
                }
                let (token, namespace_epoch) = fs.retain_inode_handle(ino).map_err(into_vfs_err)?;
                drop(fs);
                drop(prepared);
                let entry =
                    self.try_finish_retained_name(token, namespace_epoch, existing_type, name)?;
                return Ok(CreateOutcome {
                    entry,
                    created: false,
                });
            }
            Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
            Err(err) => return Err(into_vfs_err(err)),
        }
        let inherited_project_id = fs
            .with_inode_ref(self.ino(), |inode| {
                Ok((inode.flags() & 0x2000_0000 != 0).then_some(inode.project_id()))
            })
            .map_err(into_vfs_err)?;
        let inherited_project_id = options
            .initial_attributes
            .project_id
            .or(inherited_project_id);
        self.bump_namespace_epoch();
        let (token, namespace_epoch) = fs
            .create_prepared(
                self.ino(),
                name.as_bytes(),
                inode_type,
                options.permission.bits() as _,
                inherited_project_id,
                options.initial_attributes.project_inherit,
                options.initial_attributes.access_acl.as_deref(),
                options.initial_attributes.default_acl.as_deref(),
                options.owner,
                options.rdev.map(|rdev| rdev.0),
                Some(wall_time()),
            )
            .map_err(into_vfs_err)?;
        Ok(CreateOutcome {
            entry: prepared.bind(token, namespace_epoch),
            created: true,
        })
    }

    fn create_symlink(
        &self,
        name: &FsName,
        target: &FsPath,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        {
            let mut fs = self.fs.lock();
            fs.with_inode_ref(self.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 directory",
                    )
                })
            })
            .map_err(into_vfs_err)?;
            match fs.lookup_inode(self.ino(), name.as_bytes()) {
                Ok(_) => return Err(VfsError::AlreadyExists),
                Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
                Err(err) => return Err(into_vfs_err(err)),
            }
        }
        let prepared = self.try_prepare_child(InodeType::Symlink, try_owned(name)?)?;
        let mut fs = self.fs.lock();
        match fs.lookup_inode(self.ino(), name.as_bytes()) {
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
            Err(err) => return Err(into_vfs_err(err)),
        }
        let inherited_project_id = fs
            .with_inode_ref(self.ino(), |inode| {
                Ok((inode.flags() & 0x2000_0000 != 0).then_some(inode.project_id()))
            })
            .map_err(into_vfs_err)?;
        self.bump_namespace_epoch();
        let (token, namespace_epoch) = fs
            .create_symlink(
                self.ino(),
                name.as_bytes(),
                target.as_bytes(),
                permission.bits() as _,
                inherited_project_id,
                user,
                Some(wall_time()),
            )
            .map_err(into_vfs_err)?;
        Ok(prepared.bind(token, namespace_epoch))
    }

    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &FsPath,
        options: &NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        if options.node_type != NodeType::Symlink
            || options.initial_attributes.project_inherit
            || options.initial_attributes.default_acl.is_some()
        {
            return Err(VfsError::InvalidInput);
        }
        {
            let mut fs = self.fs.lock();
            fs.with_inode_ref(self.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 directory",
                    )
                })
            })
            .map_err(into_vfs_err)?;
            match fs.lookup_inode(self.ino(), name.as_bytes()) {
                Ok(_) => return Err(VfsError::AlreadyExists),
                Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
                Err(err) => return Err(into_vfs_err(err)),
            }
        }
        let prepared = self.try_prepare_child(InodeType::Symlink, try_owned(name)?)?;
        let mut fs = self.fs.lock();
        match fs.lookup_inode(self.ino(), name.as_bytes()) {
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
            Err(err) => return Err(into_vfs_err(err)),
        }
        let inherited_project_id = options.initial_attributes.project_id.or(fs
            .with_inode_ref(self.ino(), |inode| {
                Ok((inode.flags() & 0x2000_0000 != 0).then_some(inode.project_id()))
            })
            .map_err(into_vfs_err)?);
        self.bump_namespace_epoch();
        let (token, namespace_epoch) = fs
            .create_symlink_prepared(
                self.ino(),
                name.as_bytes(),
                target.as_bytes(),
                options.permission.bits() as _,
                inherited_project_id,
                options.initial_attributes.access_acl.as_deref(),
                options.initial_attributes.default_acl.as_deref(),
                options.owner,
                Some(wall_time()),
            )
            .map_err(into_vfs_err)?;
        Ok(prepared.bind(token, namespace_epoch))
    }

    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        let target = node
            .downcast::<Self>()
            .map_err(|_| VfsError::CrossesDevices)?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let target_token = target.token().ok_or(VfsError::NotFound)?;
        let inode_type = match node.node_type() {
            NodeType::Fifo => InodeType::Fifo,
            NodeType::CharacterDevice => InodeType::CharacterDevice,
            NodeType::Directory => return Err(VfsError::OperationNotPermitted),
            NodeType::BlockDevice => InodeType::BlockDevice,
            NodeType::RegularFile => InodeType::RegularFile,
            NodeType::Symlink => InodeType::Symlink,
            NodeType::Socket => InodeType::Socket,
            NodeType::Unknown => return Err(VfsError::InvalidData),
        };
        let prepared = self.try_prepare_child(inode_type, try_owned(name)?)?;
        if let Some(runtime) = target.runtime.get() {
            prepared.inode.attach_runtime(runtime.clone());
        }
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "immutable ext4 directory",
                )
            })
        })
        .map_err(into_vfs_err)?;
        fs.with_inode_ref(target_token.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(lwext4_rust::ffi::EPERM as _, "immutable ext4 inode")
            })?;
            if inode_is_append_only(inode.flags()) {
                return Err(lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "append-only ext4 inode",
                ));
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        let (retained_token, namespace_epoch) = fs
            .retain_inode_handle(target_token.ino())
            .map_err(into_vfs_err)?;
        if retained_token != target_token {
            fs.release_inode_handle(retained_token);
            return Err(VfsError::NotFound);
        }
        self.bump_namespace_epoch();
        if let Err(err) = fs.link(
            self.ino(),
            name.as_bytes(),
            target_token.ino(),
            Some(wall_time()),
        ) {
            fs.release_inode_handle(retained_token);
            return Err(into_vfs_err(err));
        }
        Ok(prepared.bind(retained_token, namespace_epoch))
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let expected = match request.expected {
            Some(expected) => Some(self.expected_token(expected).ok_or(VfsError::NotFound)?),
            None => None,
        };
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "immutable ext4 directory",
                )
            })
        })
        .map_err(into_vfs_err)?;
        let (victim, _) = fs
            .lookup_inode(self.ino(), request.name.as_bytes())
            .map_err(into_vfs_err)?;
        fs.with_inode_ref(victim, |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(lwext4_rust::ffi::EPERM as _, "immutable ext4 inode")
            })?;
            if inode_is_append_only(inode.flags()) {
                return Err(lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "append-only ext4 inode",
                ));
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        self.bump_namespace_epoch();
        let now = wall_time();
        fs.unlink_checked(
            self.ino(),
            request.name.as_bytes(),
            expected,
            Some(request.is_dir),
            Some(now),
        )
        .map_err(into_vfs_err)
    }

    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst_dir: Arc<Self> = request
            .dst_dir
            .downcast()
            .map_err(|_| VfsError::InvalidInput)?;
        if !Arc::ptr_eq(&self.fs, &dst_dir.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let src = self.expected_token(request.src).ok_or(VfsError::NotFound)?;
        let dst = request
            .dst
            .map(|expected| self.expected_token(expected).ok_or(VfsError::NotFound))
            .transpose()?;
        let src_epoch = self.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let dst_epoch = dst_dir.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "immutable ext4 source directory",
                )
            })
        })
        .map_err(into_vfs_err)?;
        fs.with_inode_ref(dst_dir.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "immutable ext4 destination directory",
                )
            })
        })
        .map_err(into_vfs_err)?;
        fs.with_inode_ref(src.ino(), |inode| {
            admit_inode_mutation(inode).map_err(|_| {
                lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "immutable ext4 source inode",
                )
            })?;
            if inode_is_append_only(inode.flags()) {
                return Err(lwext4_rust::Ext4Error::new(
                    lwext4_rust::ffi::EPERM as _,
                    "append-only ext4 source inode",
                ));
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        if let Some(dst) = dst {
            fs.with_inode_ref(dst.ino(), |inode| {
                admit_inode_mutation(inode).map_err(|_| {
                    lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "immutable ext4 destination inode",
                    )
                })?;
                if inode_is_append_only(inode.flags()) {
                    return Err(lwext4_rust::Ext4Error::new(
                        lwext4_rust::ffi::EPERM as _,
                        "append-only ext4 destination inode",
                    ));
                }
                Ok(())
            })
            .map_err(into_vfs_err)?;
        }
        self.bump_namespace_epoch();
        if !Arc::ptr_eq(src_epoch, dst_epoch) {
            dst_dir.bump_namespace_epoch();
        }
        let now = wall_time();
        fs.rename(
            self.ino(),
            request.src_name.as_bytes(),
            dst_dir.ino(),
            request.dst_name.as_bytes(),
            src,
            dst,
            Some(now),
        )
        .map_err(into_vfs_err)
    }

    fn supports_rename_exchange(&self) -> bool {
        self.fs.lock().supports_atomic_renameat2()
    }

    fn rename_exchange(&self, request: axfs_ng_vfs::RenameExchangeRequest<'_>) -> VfsResult<()> {
        let dst_dir: Arc<Self> = request
            .dst_dir
            .downcast()
            .map_err(|_| VfsError::InvalidInput)?;
        if !Arc::ptr_eq(&self.fs, &dst_dir.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let src = self.expected_token(request.src).ok_or(VfsError::NotFound)?;
        let dst = self.expected_token(request.dst).ok_or(VfsError::NotFound)?;
        let src_epoch = self.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let dst_epoch = dst_dir.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let mut fs = self.fs.lock();
        fs.rename_exchange(
            self.ino(),
            request.src_name.as_bytes(),
            dst_dir.ino(),
            request.dst_name.as_bytes(),
            src,
            dst,
            Some(wall_time()),
        )
        .map_err(into_vfs_err)?;
        self.bump_namespace_epoch();
        if !Arc::ptr_eq(src_epoch, dst_epoch) {
            dst_dir.bump_namespace_epoch();
        }
        Ok(())
    }

    fn supports_rename_whiteout(&self) -> bool {
        self.fs.lock().supports_atomic_renameat2()
    }

    fn rename_whiteout(&self, request: axfs_ng_vfs::RenameWhiteoutRequest<'_>) -> VfsResult<()> {
        let dst_dir: Arc<Self> = request
            .dst_dir
            .downcast()
            .map_err(|_| VfsError::InvalidInput)?;
        if !Arc::ptr_eq(&self.fs, &dst_dir.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let src = self.expected_token(request.src).ok_or(VfsError::NotFound)?;
        let dst = request
            .dst
            .map(|entry| self.expected_token(entry).ok_or(VfsError::NotFound))
            .transpose()?;
        let src_epoch = self.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let dst_epoch = dst_dir.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let mut fs = self.fs.lock();
        fs.rename_whiteout(
            self.ino(),
            request.src_name.as_bytes(),
            dst_dir.ino(),
            request.dst_name.as_bytes(),
            src,
            dst,
            Some(wall_time()),
        )
        .map_err(into_vfs_err)?;
        self.bump_namespace_epoch();
        if !Arc::ptr_eq(src_epoch, dst_epoch) {
            dst_dir.bump_namespace_epoch();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "test-ramdisk")]
    use core::sync::atomic::AtomicBool;
    #[cfg(feature = "test-ramdisk")]
    use std::{
        fs::{self, OpenOptions},
        process::Command,
    };

    #[cfg(feature = "test-ramdisk")]
    use axdriver::{AxBlockDevice, SharedBlockDevice};
    #[cfg(feature = "test-ramdisk")]
    use axfs_ng_vfs::Mountpoint;

    use super::*;

    #[cfg(feature = "test-ramdisk")]
    fn formatted_ext4_device() -> crate::MountedBlockDevice {
        let path = std::env::temp_dir().join(format!(
            "axfs-ext4-xattr-{}-{}.img",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        drop(file);
        let status = Command::new("mke2fs")
            .args([
                "-q",
                "-t",
                "ext4",
                "-F",
                "-b",
                "4096",
                "-O",
                "none,has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,\
                 large_file,huge_file,dir_nlink,extra_isize,metadata_csum",
            ])
            .arg(&path)
            .status()
            .expect("mke2fs is required for the axfs ext4 provider test");
        assert!(status.success());
        let image = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();

        crate::MountedBlockDevice {
            device: SharedBlockDevice::new(AxBlockDevice::Existing(axdriver_block::ramdisk::RamDisk::from(image.as_slice()))),
            mounted: Arc::new(AtomicBool::new(true)),
            read_only: false,
        }
    }

    #[test]
    fn ext4_named_create_capabilities_match_backend_primitives() {
        for node_type in [
            NodeType::Fifo,
            NodeType::CharacterDevice,
            NodeType::Directory,
            NodeType::BlockDevice,
            NodeType::RegularFile,
            NodeType::Socket,
        ] {
            assert!(ext4_named_create_inode_type(node_type).is_some());
        }
        assert!(ext4_named_create_inode_type(NodeType::Symlink).is_none());
        assert!(ext4_named_create_inode_type(NodeType::Unknown).is_none());
    }

    #[test]
    fn ext4_xattr_modes_preserve_linux_existence_errors() {
        for (mode, exists) in [
            (XattrSetMode::Upsert, false),
            (XattrSetMode::Upsert, true),
            (XattrSetMode::Create, false),
            (XattrSetMode::Replace, true),
        ] {
            assert!(admit_xattr_set_mode(mode, exists).is_ok());
        }
        assert_eq!(
            admit_xattr_set_mode(XattrSetMode::Create, true)
                .unwrap_err()
                .code,
            EEXIST as i32
        );
        assert_eq!(
            admit_xattr_set_mode(XattrSetMode::Replace, false)
                .unwrap_err()
                .code,
            ENODATA as i32
        );
    }

    #[test]
    fn physical_sg_coalesces_adjacent_ranges_before_the_four_entry_cap() {
        let segments = [
            PhysicalIoSegment::new(0x20_000, 64 * 1024),
            PhysicalIoSegment::new(0x30_000, 64 * 1024),
            PhysicalIoSegment::new(0x40_000, 64 * 1024),
            PhysicalIoSegment::new(0x50_000, 64 * 1024 - 512),
            PhysicalIoSegment::new(0x5f_c00, 512),
        ];
        let (_, count, total) = to_lwext4_physical_segments(&segments).unwrap();
        assert_eq!(count, 1);
        assert_eq!(total, 256 * 1024);
    }

    #[test]
    fn physical_sg_rejects_more_than_the_bounded_request_size() {
        let segments = [PhysicalIoSegment::new(0x20_000, 256 * 1024 + 512)];
        assert!(to_lwext4_physical_segments(&segments).is_none());
    }

    #[cfg(feature = "test-ramdisk")]
    #[test]
    fn ordinary_ext4_file_resize_has_shared_native_mutation_runtime() {
        let filesystem = Ext4Filesystem::new_for_sync_test(formatted_ext4_device()).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        let root = mount.root_location();
        let location = root.create(
            FsName::new(b"resize-runtime"),
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ).unwrap();
        // No initial_data was supplied: native mutation still needs a stable
        // per-inode gate, and a separately materialized alias must share it.
        let alias = root.entry().downcast::<Inode>().unwrap()
            .lookup(FsName::new(b"resize-runtime")).unwrap();
        assert!(core::ptr::eq(
            location.entry().persistent_user_data().unwrap(),
            alias.persistent_user_data().unwrap(),
        ));
        let mut options = crate::OpenOptions::new();
        options.read(true).write(true);
        let file = options.open_loc(location.clone()).unwrap().into_file().unwrap();
        file.set_len(16384).unwrap();
        assert_eq!(location.len().unwrap(), 16384);
        file.set_len(4096).unwrap();
        assert_eq!(location.len().unwrap(), 4096);
        drop(file);
        drop(alias);
        drop(location);
        drop(root);
        drop(mount);
        drop(filesystem);
        while axfs_ng_vfs::drain_deferred_dentry_cache_cleanup() {}
        crate::drain_deferred_filesystem_finalizers(|| {});
    }

    #[cfg(feature = "test-ramdisk")]
    #[test]
    fn ext4_xattr_provider_preserves_raw_name_bytes() {
        let filesystem = Ext4Filesystem::new_for_sync_test(formatted_ext4_device()).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        let root = mount.root_location();
        let file = root
            .create(
                FsName::new(b"first-xattr"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let raw_name = b"user.raw-\xff-name";
        let mut boundary_name = b"user.".to_vec();
        boundary_name.resize(255, 0xfe);
        assert_eq!(boundary_name.len(), 255);

        file.set_xattr(b"user.first", b"provider", XattrSetMode::Create)
            .unwrap();
        file.set_xattr(raw_name, b"raw", XattrSetMode::Create)
            .unwrap();
        file.set_xattr(&boundary_name, b"boundary", XattrSetMode::Create)
            .unwrap();
        assert_eq!(file.get_xattr(b"user.first").unwrap(), b"provider");
        assert_eq!(file.get_xattr(raw_name).unwrap(), b"raw");
        assert_eq!(file.get_xattr(&boundary_name).unwrap(), b"boundary");
        let listed = file.list_xattrs().unwrap();
        let names = listed
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .collect::<alloc::vec::Vec<_>>();
        assert!(names.contains(&b"user.first".as_slice()));
        assert!(names.contains(&raw_name.as_slice()));
        assert!(names.contains(&boundary_name.as_slice()));

        file.remove_xattr(raw_name).unwrap();
        file.remove_xattr(&boundary_name).unwrap();
        assert_eq!(file.list_xattrs().unwrap(), b"user.first\0");

        drop(file);
        drop(root);
        drop(mount);
        drop(filesystem);
        while axfs_ng_vfs::drain_deferred_dentry_cache_cleanup() {}
        crate::drain_deferred_filesystem_finalizers(|| {});
    }
}
