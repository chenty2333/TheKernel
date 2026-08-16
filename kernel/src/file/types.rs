use alloc::{borrow::Cow, string::String, sync::Arc, vec::Vec};
use core::{ffi::c_int, time::Duration};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::DeviceId;
use axio::prelude::*;
use axpoll::Pollable;
use axsync::Mutex;
use axtask::{AxTaskRef, current};
use downcast_rs::{DowncastSync, impl_downcast};
use linux_raw_sys::general::{
    RLIMIT_NOFILE, S_IFBLK, S_IFDIR, S_IFIFO, S_IFMT, S_IFREG, S_IFSOCK, STATX_BASIC_STATS,
    STATX_BTIME, STATX_DIOALIGN, STATX_MNT_ID, stat, statx, statx_timestamp,
};

use super::{FileHandle, add_file_like, current_fd_table, fd_table::FdTable, get_typed_file};
pub use crate::mm::SharedPages;
use crate::{
    mm::{AddrSpace, UserMemoryCapability},
    task::{AX_FILE_LIMIT, AsThread, Cred, ProcessData, Session},
};

// Match Linux's regular-file O_DIRECT floor: logical sector alignment, not
// the filesystem's preferred st_blksize.
const REGULAR_FILE_DIO_ALIGNMENT: u32 = 512;

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
    pub atime: Duration,
    pub btime: Duration,
    pub mtime: Duration,
    pub ctime: Duration,
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
            atime: Duration::default(),
            btime: Duration::default(),
            mtime: Duration::default(),
            ctime: Duration::default(),
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

        stat.st_atime = value.atime.as_secs() as _;
        stat.st_atime_nsec = value.atime.subsec_nanos() as _;
        stat.st_mtime = value.mtime.as_secs() as _;
        stat.st_mtime_nsec = value.mtime.subsec_nanos() as _;
        stat.st_ctime = value.ctime.as_secs() as _;
        stat.st_ctime_nsec = value.ctime.subsec_nanos() as _;

        stat
    }
}

impl From<Kstat> for statx {
    fn from(value: Kstat) -> Self {
        // SAFETY: valid for statx
        let mut statx: statx = unsafe { core::mem::zeroed() };
        statx.stx_mask = STATX_BASIC_STATS | STATX_BTIME;
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

        fn time_to_statx(time: &Duration) -> statx_timestamp {
            statx_timestamp {
                tv_sec: time.as_secs() as _,
                tv_nsec: time.subsec_nanos() as _,
                __reserved: 0,
            }
        }
        statx.stx_atime = time_to_statx(&value.atime);
        statx.stx_btime = time_to_statx(&value.btime);
        statx.stx_ctime = time_to_statx(&value.ctime);
        statx.stx_mtime = time_to_statx(&value.mtime);
        if value.mnt_id != 0 {
            statx.stx_mask |= STATX_MNT_ID;
            statx.stx_mnt_id = value.mnt_id;
        }

        let dev = DeviceId(value.dev);
        statx.stx_dev_major = dev.major();
        statx.stx_dev_minor = dev.minor();
        let file_type = value.mode & S_IFMT;
        if file_type == S_IFBLK {
            statx.stx_mask |= STATX_DIOALIGN;
            statx.stx_dio_mem_align = 1;
            statx.stx_dio_offset_align = value.blksize.max(512);
        } else if file_type == S_IFREG {
            statx.stx_mask |= STATX_DIOALIGN;
            statx.stx_dio_mem_align = REGULAR_FILE_DIO_ALIGNMENT;
            statx.stx_dio_offset_align = REGULAR_FILE_DIO_ALIGNMENT;
        }

        statx
    }
}

pub trait WriteBuf: Write + IoBufMut {}
impl<T: Write + IoBufMut> WriteBuf for T {}
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
}

impl FixedSharedMmapRegion {
    pub fn try_new(
        file_offset: u64,
        pages: Arc<SharedPages>,
        may_protect: FileMmapProtection,
    ) -> AxResult<Self> {
        let length = pages.total_bytes();
        let page_size = pages.page_size() as usize;
        if !pages.is_fixed()
            || length == 0
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
        })
    }

    /// Validates one request and freezes every mapping fact into an owned plan.
    /// A different offset is not an error so an object can probe several
    /// disjoint regions without weakening validation for the selected region.
    pub fn prepare(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        if request.offset != self.file_offset {
            return Ok(None);
        }
        validate_fixed_shared_request(
            self.file_offset,
            self.pages.total_bytes(),
            self.pages.page_size() as usize,
            self.may_protect,
            request,
        )?;
        Ok(Some(PreparedFileMmap {
            request,
            pages: self.pages.clone(),
            may_protect: self.may_protect,
        }))
    }
}

fn validate_fixed_shared_request(
    expected_offset: u64,
    expected_length: usize,
    expected_page_size: usize,
    may_protect: FileMmapProtection,
    request: FileMmapRequest,
) -> AxResult {
    if request.offset != expected_offset
        || request.length != expected_length
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
    pages: Arc<SharedPages>,
    may_protect: FileMmapProtection,
}

impl PreparedFileMmap {
    pub(crate) const fn request(&self) -> FileMmapRequest {
        self.request
    }

    pub(crate) const fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    pub(crate) const fn may_protect(&self) -> FileMmapProtection {
        self.may_protect
    }

    pub(crate) fn into_pages(self) -> Arc<SharedPages> {
        self.pages
    }
}

#[allow(dead_code)]
pub trait FileLike: Pollable + DowncastSync {
    fn read(&self, _dst: &mut IoDst) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn stat(&self) -> AxResult<Kstat>;

    /// Produces a stable display path for procfs and other kernel adapters.
    ///
    /// Dynamic paths must reserve their storage fallibly and report
    /// `NoMemory`; user-triggered path rendering must never rely on
    /// `format!`, `to_string`, or another abort-on-OOM allocation.
    fn path(&self) -> AxResult<Cow<'_, str>>;

    fn ioctl(&self, _context: &IoctlContext, _cmd: u32, _arg: usize) -> AxResult<usize> {
        Err(AxError::NotATty)
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

pub(crate) fn try_owned_path(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

pub(crate) fn try_path_into_owned(path: Cow<'_, str>) -> AxResult<String> {
    match path {
        Cow::Owned(path) => Ok(path),
        Cow::Borrowed(path) => try_owned_path(path),
    }
}

pub(crate) fn try_path_into_bytes(path: Cow<'_, str>) -> AxResult<Vec<u8>> {
    match path {
        Cow::Owned(path) => Ok(path.into_bytes()),
        Cow::Borrowed(path) => {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(path.len())
                .map_err(|_| AxError::NoMemory)?;
            bytes.extend_from_slice(path.as_bytes());
            Ok(bytes)
        }
    }
}

/// Builds Linux's anonymous inode display form without an infallible format
/// allocation. Twenty decimal digits cover every `u64` inode value.
pub(crate) fn try_pseudo_inode_path(kind: &str, inode: u64) -> AxResult<Cow<'static, str>> {
    let mut path = String::new();
    path.try_reserve_exact(kind.len().saturating_add(23))
        .map_err(|_| AxError::NoMemory)?;
    path.push_str(kind);
    path.push_str(":[");

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
        path.push(*digit as char);
    }
    path.push(']');
    Ok(Cow::Owned(path))
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
        assert_eq!(try_pseudo_inode_path("socket", 0).unwrap(), "socket:[0]");
        assert_eq!(
            try_pseudo_inode_path("pipe", u64::MAX).unwrap(),
            "pipe:[18446744073709551615]"
        );
    }

    #[test]
    fn borrowed_and_owned_path_snapshots_keep_exact_bytes() {
        assert_eq!(
            try_path_into_bytes(Cow::Borrowed("anon_inode:[eventfd]")).unwrap(),
            b"anon_inode:[eventfd]"
        );
        assert_eq!(
            try_path_into_owned(Cow::Owned(try_owned_path("/tmp/file").unwrap())).unwrap(),
            "/tmp/file"
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
    fn fixed_shared_plan_rejects_private_exec_and_nonexact_requests() {
        let allowed = FileMmapProtection::READ | FileMmapProtection::WRITE;
        let accepted = mmap_request(
            0x20_000,
            0x3000,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
            FileMmapSharing::Shared,
        );
        validate_fixed_shared_request(0x20_000, 0x3000, 0x1000, allowed, accepted).unwrap();

        let private = mmap_request(
            0x20_000,
            0x3000,
            FileMmapProtection::READ,
            FileMmapSharing::Private,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, 0x1000, allowed, private),
            Err(AxError::InvalidInput)
        );

        let executable = mmap_request(
            0x20_000,
            0x3000,
            FileMmapProtection::READ | FileMmapProtection::EXECUTE,
            FileMmapSharing::Shared,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, 0x1000, allowed, executable),
            Err(AxError::PermissionDenied)
        );

        let short = mmap_request(
            0x20_000,
            0x2000,
            FileMmapProtection::READ,
            FileMmapSharing::Shared,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, 0x1000, allowed, short),
            Err(AxError::InvalidInput)
        );

        let wrong_offset = mmap_request(
            0x21_000,
            0x3000,
            FileMmapProtection::READ,
            FileMmapSharing::Shared,
        );
        assert_eq!(
            validate_fixed_shared_request(0x20_000, 0x3000, 0x1000, allowed, wrong_offset),
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
}
