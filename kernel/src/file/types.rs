use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::ffi::c_int;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    DeviceId, Filesystem, FsPath, FsPathBuf, Location, MetadataCapabilities, Timestamp, WritebackErrorState,
};
use axio::prelude::*;
use axpoll::Pollable;
use axsync::Mutex;
use axtask::{AxTaskRef, current};
use downcast_rs::{DowncastSync, impl_downcast};
use linux_raw_sys::general::{
    RLIMIT_NOFILE, S_IFDIR, S_IFIFO, S_IFMT, S_IFREG, S_IFSOCK, STATX_BASIC_STATS,
    STATX_BTIME, STATX_DIOALIGN, STATX_MNT_ID, stat, statx, statx_timestamp,
};
use thekernel_linux_io_uring::{IssuedRequest, RequestId, TerminalCause};

use super::{
    FileHandle, OfdIoStatus, add_file_like, current_fd_table, fd_table::FdTable, get_typed_file,
};
pub use crate::mm::SharedPages;
use crate::{
    async_operation::AsyncOperation,
    mm::{AddrSpace, UserMemoryCapability},
    task::{AX_FILE_LIMIT, AsThread, Cred, ProcessData, Session},
};

#[derive(Debug, Clone, Copy)]
pub struct Kstat {
    pub dev: u64,
    pub mnt_id: u64,
    pub ino: u64,
    pub nlink: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub rdev: DeviceId,
    pub attributes: u64,
    pub attributes_mask: u64,
    pub atime: Timestamp,
    pub btime: Timestamp,
    pub metadata_capabilities: MetadataCapabilities,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
}

/// One provider-declared `IORING_OP_URING_CMD` command ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UringCmdManifest {
    command: u32,
    allowed_flags: u32,
    iopoll: bool,
    cancellable: bool,
}

impl UringCmdManifest {
    pub const fn new(command: u32, allowed_flags: u32, iopoll: bool, cancellable: bool) -> Self {
        Self {
            command,
            allowed_flags,
            iopoll,
            cancellable,
        }
    }

    pub const fn command(self) -> u32 {
        self.command
    }
    pub const fn accepts_flags(self, flags: u32) -> bool {
        flags & !self.allowed_flags == 0
    }
    pub const fn iopoll(self) -> bool {
        self.iopoll
    }
    pub const fn cancellable(self) -> bool {
        self.cancellable
    }
}

/// Typed, copied `IORING_OP_URING_CMD` input delivered to an opt-in provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UringCmd {
    command: u32,
    flags: u32,
    payload: [u8; 16],
}

/// Single-use provider completion capability for an asynchronous URING_CMD.
/// The provider owns this value after successful submission; dropping it
/// without a completion fails the exact issued request instead of leaking a
/// terminal credit.
#[must_use]
pub struct UringCmdCompletion {
    ring: Arc<super::io_uring::IoUring>,
    issued: Option<IssuedRequest>,
    /// Retains the submission's exact OFD through provider completion.
    _file: Option<super::io_uring::IoUringFileLease>,
    iopoll: bool,
}

impl UringCmdCompletion {
    pub(crate) fn new(
        ring: Arc<super::io_uring::IoUring>,
        issued: IssuedRequest,
        file: Option<super::io_uring::IoUringFileLease>,
        iopoll: bool,
    ) -> Self {
        Self {
            ring,
            issued: Some(issued),
            _file: file,
            iopoll,
        }
    }

    pub fn complete(mut self, result: i32, flags: u32) -> AxResult<()> {
        let issued = self.issued.take().ok_or(AxError::BadState)?;
        self.ring
            .complete_uring_cmd(issued, TerminalCause::Completed, result, flags, self.iopoll)
    }

    pub fn fail(mut self, error: AxError) -> AxResult<()> {
        let issued = self.issued.take().ok_or(AxError::BadState)?;
        self.ring.complete_uring_cmd(
            issued,
            TerminalCause::PreparationFailed,
            -axerrno::LinuxError::from(error).code(),
            0,
            self.iopoll,
        )
    }
}

impl Drop for UringCmdCompletion {
    fn drop(&mut self) {
        let Some(issued) = self.issued.take() else {
            return;
        };
        let _ = self.ring.complete_uring_cmd(
            issued,
            TerminalCause::PreparationFailed,
            -axerrno::LinuxError::EIO.code(),
            0,
            self.iopoll,
        );
    }
}

impl UringCmd {
    pub const fn new(command: u32, flags: u32, payload: [u8; 16]) -> Self {
        Self {
            command,
            flags,
            payload,
        }
    }

    pub const fn command(self) -> u32 {
        self.command
    }
    pub const fn flags(self) -> u32 {
        self.flags
    }
    pub const fn payload(self) -> [u8; 16] {
        self.payload
    }
}

impl Default for Kstat {
    fn default() -> Self {
        Self {
            dev: 0,
            mnt_id: 0,
            ino: 0,
            nlink: 0,
            mode: 0,
            uid: 0,
            gid: 0,
            size: 0,
            blksize: 4096,
            blocks: 0,
            rdev: DeviceId::default(),
            attributes: 0,
            attributes_mask: 0,
            atime: Timestamp::ZERO,
            btime: Timestamp::ZERO,
            metadata_capabilities: MetadataCapabilities::default(),
            mtime: Timestamp::ZERO,
            ctime: Timestamp::ZERO,
        }
    }
}

impl From<Kstat> for stat {
    fn from(value: Kstat) -> Self {
        // SAFETY: valid for stat
        let mut stat: stat = unsafe { core::mem::zeroed() };
        stat.st_dev = value.dev as _;
        stat.st_ino = value.ino as _;
        stat.st_nlink = value.nlink as _;
        stat.st_mode = value.mode as _;
        stat.st_uid = value.uid as _;
        stat.st_gid = value.gid as _;
        stat.st_size = value.size as _;
        stat.st_blksize = value.blksize as _;
        stat.st_blocks = value.blocks as _;
        stat.st_rdev = value.rdev.0 as _;

        stat.st_atime = value.atime.seconds() as _;
        stat.st_atime_nsec = value.atime.subsec_nanos() as _;
        stat.st_mtime = value.mtime.seconds() as _;
        stat.st_mtime_nsec = value.mtime.subsec_nanos() as _;
        stat.st_ctime = value.ctime.seconds() as _;
        stat.st_ctime_nsec = value.ctime.subsec_nanos() as _;

        stat
    }
}

impl From<Kstat> for statx {
    fn from(value: Kstat) -> Self {
        // SAFETY: valid for statx
        let mut statx: statx = unsafe { core::mem::zeroed() };
        statx.stx_mask = STATX_BASIC_STATS;
        statx.stx_blksize = value.blksize as _;
        statx.stx_attributes = value.attributes;
        statx.stx_attributes_mask = value.attributes_mask;
        statx.stx_nlink = value.nlink as _;
        statx.stx_uid = value.uid as _;
        statx.stx_gid = value.gid as _;
        statx.stx_mode = value.mode as _;
        statx.stx_ino = value.ino as _;
        statx.stx_size = value.size as _;
        statx.stx_blocks = value.blocks as _;
        statx.stx_rdev_major = value.rdev.major();
        statx.stx_rdev_minor = value.rdev.minor();

        fn time_to_statx(time: &Timestamp) -> statx_timestamp {
            statx_timestamp {
                tv_sec: time.seconds() as _,
                tv_nsec: time.subsec_nanos() as _,
                __reserved: 0,
            }
        }
        statx.stx_atime = time_to_statx(&value.atime);
        if value.metadata_capabilities.birth_time {
            statx.stx_mask |= STATX_BTIME;
            statx.stx_btime = time_to_statx(&value.btime);
        }
        statx.stx_ctime = time_to_statx(&value.ctime);
        statx.stx_mtime = time_to_statx(&value.mtime);
        if value.mnt_id != 0 {
            statx.stx_mask |= STATX_MNT_ID;
            statx.stx_mnt_id = value.mnt_id;
        }

        let dev = DeviceId(value.dev);
        statx.stx_dev_major = dev.major();
        statx.stx_dev_minor = dev.minor();
        if let Some(alignment) = value.metadata_capabilities.direct_io_alignment {
            statx.stx_mask |= STATX_DIOALIGN;
            statx.stx_dio_mem_align = alignment.memory;
            statx.stx_dio_offset_align = alignment.offset;
        }

        statx
    }
}

pub trait WriteBuf: Write + IoBuf + IoBufMut {}
impl<T: Write + IoBuf + IoBufMut> WriteBuf for T {}
pub type IoDst<'a> = dyn WriteBuf + 'a;

pub trait ReadBuf: Read + IoBuf {}
impl<T: Read + IoBuf> ReadBuf for T {}
pub type IoSrc<'a> = dyn ReadBuf + 'a;

/// The immutable syscall-entry view carried through one ioctl operation.
///
/// Ioctl leaves must use this object for all caller-dependent state.  In
/// particular, the user-memory capability and files table are selected once
/// at dispatch and are never re-resolved through `current()` or a scope-local
/// fd lookup while an object implementation is running.
pub struct IoctlContext {
    user_memory: UserMemoryCapability,
    caller_task: AxTaskRef,
    caller_cred: Arc<Cred>,
    caller_process: Arc<ProcessData>,
    caller_session: Arc<Session>,
    files: Arc<FdTable>,
}

impl IoctlContext {
    /// Captures the caller object graph and the explicitly selected address
    /// space exactly once at syscall dispatch.
    pub(crate) fn new(aspace: Arc<Mutex<AddrSpace>>) -> Self {
        let caller_task = current().clone();
        let thread = caller_task.as_thread();
        let caller_process = thread.proc_data.clone();
        let caller_session = caller_process.proc.group().session();
        Self {
            user_memory: UserMemoryCapability::new(aspace),
            caller_cred: thread.current_cred(),
            caller_process,
            caller_session,
            caller_task,
            files: current_fd_table(),
        }
    }

    pub(crate) fn user_memory(&self) -> &UserMemoryCapability {
        &self.user_memory
    }

    pub(crate) fn caller_task(&self) -> &AxTaskRef {
        &self.caller_task
    }

    pub(crate) fn caller_cred(&self) -> &Arc<Cred> {
        &self.caller_cred
    }

    pub(crate) fn caller_process(&self) -> &Arc<ProcessData> {
        &self.caller_process
    }

    pub(crate) fn caller_session(&self) -> &Arc<Session> {
        &self.caller_session
    }

    pub(crate) fn files(&self) -> &Arc<FdTable> {
        &self.files
    }

    pub(crate) fn get_file_like(&self, fd: c_int) -> AxResult<FileHandle<dyn FileLike>> {
        let description = self.files.get_description(fd)?;
        Ok(FileHandle {
            file: description.inner.clone(),
            description,
        })
    }

    pub(crate) fn add_file_like(&self, file: Arc<dyn FileLike>, cloexec: bool) -> AxResult<c_int> {
        let max_nofile = self.caller_process.rlim.read()[RLIMIT_NOFILE]
            .current
            .min(AX_FILE_LIMIT as u64) as usize;
        self.files.add_file_like(file, cloexec, max_nofile)
    }
}

bitflags::bitflags! {
    /// Access requested for one file-owned mapping.
    ///
    /// This intentionally carries only the portable read/write/execute facts.
    /// Linux UAPI parsing remains in the syscall layer and architecture page
    /// table flags remain in the MM layer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FileMmapProtection: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXECUTE = 1 << 2;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMmapSharing {
    Shared,
    Private,
}

/// Normalized, copied mmap input presented to a file-like object.
///
/// Construction proves that the byte geometry is nonempty, page aligned, and
/// cannot overflow. A returned plan therefore never has to reinterpret raw
/// userspace arguments while an address-space or page-table lock is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMmapRequest {
    offset: u64,
    length: usize,
    page_size: usize,
    protection: FileMmapProtection,
    sharing: FileMmapSharing,
}

impl FileMmapRequest {
    pub fn try_new(
        offset: u64,
        length: usize,
        page_size: usize,
        protection: FileMmapProtection,
        sharing: FileMmapSharing,
    ) -> AxResult<Self> {
        if length == 0
            || page_size == 0
            || !page_size.is_power_of_two()
            || !length.is_multiple_of(page_size)
            || !offset.is_multiple_of(page_size as u64)
        {
            return Err(AxError::InvalidInput);
        }
        offset
            .checked_add(u64::try_from(length).map_err(|_| AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        Ok(Self {
            offset,
            length,
            page_size,
            protection,
            sharing,
        })
    }

    pub const fn offset(self) -> u64 {
        self.offset
    }

    pub const fn length(self) -> usize {
        self.length
    }

    pub const fn page_size(self) -> usize {
        self.page_size
    }

    pub const fn protection(self) -> FileMmapProtection {
        self.protection
    }

    pub const fn sharing(self) -> FileMmapSharing {
        self.sharing
    }
}

/// One exact fixed-size region exported by a file-like object.
///
/// Regions are deliberately non-executable and cannot be resized. A file with
/// multiple disjoint regions keeps one value per accepted file offset and
/// returns the first matching prepared plan from [`Self::prepare`].
#[derive(Clone)]
pub struct FixedSharedMmapRegion {
    file_offset: u64,
    pages: Arc<SharedPages>,
    may_protect: FileMmapProtection,
    retain_description: bool,
}

impl FixedSharedMmapRegion {
    pub fn try_new(
        file_offset: u64,
        pages: Arc<SharedPages>,
        may_protect: FileMmapProtection,
    ) -> AxResult<Self> {
        Self::try_new_with_description_retention(file_offset, pages, may_protect, true)
    }

    /// Builds a region whose VMA retains only the exported shared pages, not
    /// the originating open file description. This is for anonymous control
    /// mappings such as io_uring, where the mapped pages remain valid after
    /// final close and retaining the ring would create a cycle through a
    /// registered buffer's address-space pin.
    pub fn try_new_detached(
        file_offset: u64,
        pages: Arc<SharedPages>,
        may_protect: FileMmapProtection,
    ) -> AxResult<Self> {
        Self::try_new_with_description_retention(file_offset, pages, may_protect, false)
    }

    fn try_new_with_description_retention(
        file_offset: u64,
        pages: Arc<SharedPages>,
        may_protect: FileMmapProtection,
        retain_description: bool,
    ) -> AxResult<Self> {
        let length = pages.total_bytes();
        let page_size = pages.page_size() as usize;
        if !pages.is_fixed()
            || (length == 0 && !pages.is_secret())
            || !file_offset.is_multiple_of(page_size as u64)
            || may_protect.contains(FileMmapProtection::EXECUTE)
        {
            return Err(AxError::InvalidInput);
        }
        file_offset
            .checked_add(u64::try_from(length).map_err(|_| AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        Ok(Self {
            file_offset,
            pages,
            may_protect,
            retain_description,
        })
    }

    /// Validates one request and freezes every mapping fact into an owned plan.
    /// A different offset is not an error so an object can probe several
    /// disjoint regions without weakening validation for the selected region.
    pub fn prepare(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        if request.offset < self.file_offset {
            return Ok(None);
        }
        validate_fixed_shared_request(
            self.file_offset,
            self.pages.total_bytes(),
            self.pages.is_secret(),
            self.pages.page_size() as usize,
            self.may_protect,
            request,
        )?;
        Ok(Some(PreparedFileMmap {
            request,
            region_offset: self.file_offset,
            pages: self.pages.clone(),
            may_protect: self.may_protect,
            retain_description: self.retain_description,
            mapping_lifetime: None,
            excludes_fork_and_dump: false,
        }))
    }
}

fn validate_fixed_shared_request(
    expected_offset: u64,
    expected_length: usize,
    allow_beyond_end: bool,
    expected_page_size: usize,
    may_protect: FileMmapProtection,
    request: FileMmapRequest,
) -> AxResult {
    let Some(relative) = request.offset.checked_sub(expected_offset) else {
        return Err(AxError::InvalidInput);
    };
    // secretmem follows normal file mmap admission: a shared mapping may
    // extend past i_size.  Its fault handler rejects pages beyond EOF instead
    // of turning mmap itself into EINVAL.  Other fixed control mappings keep
    // their exact exported extent.
    if relative
        .checked_add(request.length as u64)
        .is_none_or(|end| end > expected_length as u64 && !allow_beyond_end)
        || request.page_size != expected_page_size
        || request.sharing != FileMmapSharing::Shared
    {
        return Err(AxError::InvalidInput);
    }
    if request.protection.contains(FileMmapProtection::EXECUTE)
        || !may_protect.contains(request.protection)
    {
        return Err(AxError::PermissionDenied);
    }
    Ok(())
}

/// Fully validated and allocation-free-to-bind file mapping plan.
///
/// Its fields are private to prevent a syscall adapter from changing geometry
/// or permissions after the owning [`FileLike`] accepted the request.
pub struct PreparedFileMmap {
    request: FileMmapRequest,
    region_offset: u64,
    pages: Arc<SharedPages>,
    may_protect: FileMmapProtection,
    retain_description: bool,
    mapping_lifetime: Option<Arc<dyn core::any::Any + Send + Sync>>,
    excludes_fork_and_dump: bool,
}

impl PreparedFileMmap {
    /// The plan and live VMA fragments alone retain this lease. Its Drop must
    /// be safe under the address-space lock (spin-only or deferred cleanup).
    pub(crate) fn with_mapping_lifetime(mut self, lease: Arc<dyn core::any::Any + Send + Sync>) -> Self {
        self.mapping_lifetime = Some(lease);
        self
    }
    pub(crate) fn with_excluded_fork_and_dump(mut self) -> Self {
        self.excludes_fork_and_dump = true;
        self
    }
    pub(crate) const fn excludes_fork_and_dump(&self) -> bool { self.excludes_fork_and_dump }
    pub(crate) fn take_mapping_lifetime(&mut self) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        self.mapping_lifetime.take()
    }

    pub(crate) const fn request(&self) -> FileMmapRequest {
        self.request
    }
    pub(crate) const fn region_offset(&self) -> u64 {
        self.region_offset
    }

    pub(crate) const fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    pub(crate) const fn may_protect(&self) -> FileMmapProtection {
        self.may_protect
    }

    pub(crate) const fn retains_description(&self) -> bool {
        self.retain_description
    }

    pub(crate) fn into_pages(self) -> Arc<SharedPages> {
        self.pages
    }
}

#[allow(dead_code)]
pub trait FileLike: Pollable + DowncastSync {
    /// Runs at the task-context last-descriptor boundary, before the final
    /// descriptor reference is retired.  Unlike [`Self::final_close`], this
    /// hook may synchronously quiesce hardware owned by another CPU.  It is
    /// deliberately advisory: callers which cannot provide task context skip
    /// it and `final_close` remains the IRQ-safe fail-closed backstop.
    fn pre_close(&self) {}

    /// Runs exactly once when the final owner of an open file description
    /// releases it, while this object is still alive.
    ///
    /// This is not an fd-close hook: duplicated descriptors, forked fd
    /// tables, transferred descriptor owners, and VMA `vm_file` leases share
    /// one invocation. A mapping may therefore defer this notification until
    /// its final split/fork fragment is released. Implementations run in the
    /// context which drops the final OFD and must neither allocate nor block,
    /// sleep, acquire a sleeping lock, submit I/O, or otherwise depend on
    /// task context; in particular, they must remain safe if that context is
    /// an interrupt path.
    fn final_close(&self) {}

    fn read(&self, _dst: &mut IoDst) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }

    /// Executes one read using a frozen, operation-local OFD status.
    ///
    /// `RWF_NOWAIT` is not an O_NONBLOCK mutation. Providers which can honor
    /// it override this hook with one genuinely nonblocking attempt; the
    /// default must not poll and then call legacy `read`, because readiness
    /// can disappear before that call and re-enter a blocking path.
    fn read_with_operation_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        if status.rwf_nowait() {
            return Err(AxError::OperationNotSupported);
        }
        self.read(dst)
    }

    /// Write-side counterpart of [`Self::read_with_operation_status`].
    fn write_with_operation_status(&self, status: OfdIoStatus, src: &mut IoSrc) -> AxResult<usize> {
        if status.rwf_nowait() {
            return Err(AxError::OperationNotSupported);
        }
        self.write(src)
    }

    fn stat(&self) -> AxResult<Kstat>;

    /// Returns the concrete VFS object retained by this open file
    /// description.  This is deliberately separate from
    /// [`Self::cachestat_location`]: objects such as named FIFOs have stable
    /// mount/idmap provenance but no page-cache mapping for `cachestat(2)`.
    fn vfs_location(&self) -> Option<&Location> {
        None
    }

    /// Linux cachestat(2) observes an object's page-cache mapping.  Objects
    /// without one behave like an empty mapping.
    fn cachestat(&self, _first_page: u64, _last_page: u64) -> AxResult<axfs::CachedFileCacheStat> {
        Ok(axfs::CachedFileCacheStat::default())
    }

    /// Returns the VFS inode checked by `cachestat(2)`.
    fn cachestat_location(&self) -> Option<&Location> {
        None
    }

    /// Classifies a cachestat mapping without filesystem-name matching.
    fn cachestat_is_hugetlbfs(&self) -> bool {
        false
    }

    /// Updates descriptor-owned timestamps for objects which do not have a VFS
    /// location (for example pipes and sockets).  VFS-backed files are updated
    /// through their inode setattr transaction instead.
    fn update_timestamps(
        &self,
        _atime: Option<Timestamp>,
        _mtime: Option<Timestamp>,
        _ctime: Timestamp,
    ) -> AxResult<()> {
        Err(AxError::OperationNotSupported)
    }

    /// Produces a stable byte pathname for procfs and other kernel adapters.
    ///
    /// Dynamic paths must reserve their storage fallibly and report
    /// `NoMemory`; user-triggered path rendering must never rely on
    /// `format!`, `to_string`, or another abort-on-OOM allocation.
    fn path(&self) -> AxResult<Cow<'_, FsPath>>;

    fn ioctl(&self, _context: &IoctlContext, _cmd: u32, _arg: usize) -> AxResult<usize> {
        Err(AxError::NotATty)
    }

    /// Explicit typed contract for `IORING_OP_URING_CMD`.  The empty default
    /// prevents generic file descriptors from accidentally treating a command
    /// SQE as an ioctl.
    fn uring_cmd_manifest(&self) -> &'static [UringCmdManifest] {
        &[]
    }

    /// Queues a manifest-validated command.  Successful submission transfers
    /// the sole issued completion token to the provider; it must later call
    /// `complete` or `fail` exactly once.
    fn submit_uring_cmd(
        &self,
        _command: UringCmd,
        completion: UringCmdCompletion,
    ) -> Result<(), (AxError, UringCmdCompletion)> {
        Err((AxError::OperationNotSupported, completion))
    }

    /// Non-blocking IOPOLL harvest hook. Providers queue completion tokens in
    /// `submit_uring_cmd`; this hook retires any completed commands without
    /// requiring an interrupt edge.
    fn harvest_uring_cmd(&self) -> AxResult<()> {
        Ok(())
    }

    /// Requests provider-side cancellation after io_uring has won the
    /// terminal credit. Providers must stop queue ownership and drop their
    /// completion token; a late `complete` then cannot publish a second CQE.
    fn cancel_uring_cmd(&self, _request: RequestId) {}

    /// Synchronizes the object's durable state.  This is a capability, rather
    /// than a classification by object kind: regular files, directories, and
    /// sync-capable pseudo/devices opt in explicitly while pipes and sockets
    /// retain Linux's `EINVAL` result.
    fn sync(&self, _data_only: bool) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    /// Cooperative cancellation boundary for asynchronous sync providers.
    /// Backends that own a deeper request queue may override this to abort an
    /// in-flight flush; the default still makes cancellation visible before
    /// and after the provider boundary without invalidating its resources.
    fn sync_cancellable(&self, data_only: bool, operation: &AsyncOperation) -> AxResult<()> {
        if operation.cancellation_requested() {
            return Err(axerrno::LinuxError::ECANCELED.into());
        }
        self.sync(data_only)?;
        if operation.cancellation_requested() {
            Err(axerrno::LinuxError::ECANCELED.into())
        } else {
            Ok(())
        }
    }

    /// Non-VFS objects receive a private sequence.  Sync-capable VFS objects
    /// override this with their inode-owned source.
    fn writeback_error_state(&self) -> AxResult<Arc<WritebackErrorState>> {
        Arc::try_new(WritebackErrorState::default()).map_err(|_| AxError::NoMemory)
    }

    /// A filesystem anchor is intentionally independent of ordinary I/O
    /// permission.  syncfs accepts an O_PATH fd when its object belongs to a
    /// mounted filesystem, while anonymous and special descriptors have none.
    fn syncfs_filesystem(&self) -> Option<Filesystem> {
        None
    }

    /// Prepares an object-owned mapping without holding address-space or page
    /// table locks. Implementations must return a plan only after every
    /// fallible allocation and all object-specific validation have completed.
    fn prepare_mmap(&self, _request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        Ok(None)
    }

    fn nonblocking(&self) -> bool {
        false
    }

    /// Applies the object-specific part of an `O_NONBLOCK` transition.
    ///
    /// The open-file-description owns the status flag. Implementations whose
    /// I/O semantics do not depend on it must still opt in explicitly instead
    /// of inheriting a silent-success default.
    fn set_nonblocking(&self, nonblocking: bool) -> AxResult;

    fn from_fd(fd: c_int) -> AxResult<FileHandle<Self>>
    where
        Self: Sized + 'static,
    {
        get_typed_file(fd)
    }

    fn add_to_fd_table(self, cloexec: bool) -> AxResult<c_int>
    where
        Self: Sized + 'static,
    {
        add_file_like(Arc::try_new(self).map_err(|_| AxError::NoMemory)?, cloexec)
    }
}
impl_downcast!(sync FileLike);

pub(crate) fn try_owned_path(value: &FsPath) -> AxResult<FsPathBuf> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(value.as_bytes().len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(value.as_bytes());
    Ok(FsPathBuf::from_vec(owned))
}

pub(crate) fn try_path_into_owned(path: Cow<'_, FsPath>) -> AxResult<FsPathBuf> {
    match path {
        Cow::Owned(path) => Ok(path),
        Cow::Borrowed(path) => try_owned_path(path),
    }
}

pub(crate) fn try_path_into_bytes(path: Cow<'_, FsPath>) -> AxResult<Vec<u8>> {
    match path {
        Cow::Owned(path) => Ok(path.into_vec()),
        Cow::Borrowed(path) => {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(path.as_bytes().len())
                .map_err(|_| AxError::NoMemory)?;
            bytes.extend_from_slice(path.as_bytes());
            Ok(bytes)
        }
    }
}

/// Builds Linux's anonymous inode display form without an infallible format
/// allocation. Twenty decimal digits cover every `u64` inode value.
pub(crate) fn try_pseudo_inode_path(kind: &str, inode: u64) -> AxResult<Cow<'static, FsPath>> {
    let mut path = Vec::new();
    path.try_reserve_exact(kind.len().saturating_add(23))
        .map_err(|_| AxError::NoMemory)?;
    path.extend_from_slice(kind.as_bytes());
    path.extend_from_slice(b":[");

    let mut digits = [0u8; 20];
    let mut start = digits.len();
    let mut remaining = inode;
    loop {
        start -= 1;
        digits[start] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    for digit in &digits[start..] {
        path.push(*digit);
    }
    path.push(b']');
    Ok(Cow::Owned(FsPathBuf::from_vec(path)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileLikeKind {
    Regular,
    Directory,
    Fifo,
    Socket,
    Other,
}

impl FileLikeKind {
    pub fn from_mode(mode: u32) -> Self {
        match mode & S_IFMT {
            S_IFREG => Self::Regular,
            S_IFDIR => Self::Directory,
            S_IFIFO => Self::Fifo,
            S_IFSOCK => Self::Socket,
            _ => Self::Other,
        }
    }

    pub fn from_file_like(file: &dyn FileLike) -> Self {
        file.stat()
            .map(|stat| Self::from_mode(stat.mode))
            .unwrap_or(Self::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mmap_request(
        offset: u64,
        length: usize,
        protection: FileMmapProtection,
        sharing: FileMmapSharing,
    ) -> FileMmapRequest {
        FileMmapRequest::try_new(offset, length, 0x1000, protection, sharing).unwrap()
    }

    #[test]
    fn pseudo_inode_paths_cover_decimal_boundaries_without_formatting() {
        assert_eq!(try_pseudo_inode_path("socket", 0).unwrap().as_bytes(), b"socket:[0]");
        assert_eq!(
            try_pseudo_inode_path("pipe", u64::MAX).unwrap().as_bytes(),
            b"pipe:[18446744073709551615]"
        );
    }

    #[test]
    fn borrowed_and_owned_path_snapshots_keep_exact_bytes() {
        assert_eq!(
            try_path_into_bytes(Cow::Borrowed(axfs_ng_vfs::FsPath::new(b"anon_inode:[eventfd]"))).unwrap(),
            b"anon_inode:[eventfd]"
        );
        assert_eq!(
            try_path_into_owned(Cow::Owned(try_owned_path(axfs_ng_vfs::FsPath::new(b"/tmp/file")).unwrap())).unwrap().as_bytes(),
            b"/tmp/file"
        );
    }

    #[test]
    fn file_mmap_request_rejects_unaligned_and_overflowing_geometry() {
        assert_eq!(
            FileMmapRequest::try_new(
                1,
                0x1000,
                0x1000,
                FileMmapProtection::READ,
                FileMmapSharing::Shared,
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            FileMmapRequest::try_new(
                0,
                0x1001,
                0x1000,
                FileMmapProtection::READ,
                FileMmapSharing::Shared,
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            FileMmapRequest::try_new(
                u64::MAX - 0xfff,
                0x2000,
                0x1000,
                FileMmapProtection::READ,
                FileMmapSharing::Shared,
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn fixed_shared_plan_rejects_private_exec_and_out_of_range_requests() {
        let allowed = FileMmapProtection::READ | FileMmapProtection::WRITE;
        let accepted = mmap_request(
            0x20_000,
            0x3000,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
            FileMmapSharing::Shared,
        );
        validate_fixed_shared_request(0x20_000, 0x3000, false, 0x1000, allowed, accepted).unwrap();

        let private = mmap_request(
            0x20_000,
            0x3000,
            FileMmapProtection::READ,
            FileMmapSharing::Private,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, false, 0x1000, allowed, private),
            Err(AxError::InvalidInput)
        );

        let executable = mmap_request(
            0x20_000,
            0x3000,
            FileMmapProtection::READ | FileMmapProtection::EXECUTE,
            FileMmapSharing::Shared,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, false, 0x1000, allowed, executable),
            Err(AxError::PermissionDenied)
        );

        let short = mmap_request(
            0x20_000,
            0x2000,
            FileMmapProtection::READ,
            FileMmapSharing::Shared,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, false, 0x1000, allowed, short),
            Ok(())
        );

        let wrong_offset = mmap_request(
            0x21_000,
            0x3000,
            FileMmapProtection::READ,
            FileMmapSharing::Shared,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, false, 0x1000, allowed, wrong_offset),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn fixed_shared_plan_accepts_the_page_rounded_backing_length() {
        let structure_end = 0x2345usize;
        let backing_length = structure_end.next_multiple_of(0x1000);
        let request = mmap_request(
            0,
            backing_length,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
            FileMmapSharing::Shared,
        );
        validate_fixed_shared_request(
            0,
            backing_length,
            false,
            0x1000,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
            request,
        )
        .unwrap();
        assert_eq!(
            FileMmapRequest::try_new(
                0,
                structure_end,
                0x1000,
                FileMmapProtection::READ,
                FileMmapSharing::Shared,
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn secret_shared_plan_allows_mapping_past_logical_end() {
        let request = mmap_request(
            0x1000,
            0x3000,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
            FileMmapSharing::Shared,
        );
        validate_fixed_shared_request(
            0,
            0x1000,
            true,
            0x1000,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
            request,
        )
        .unwrap();
    }
}
