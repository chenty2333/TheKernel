use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    cell::Cell,
    ffi::c_int,
    hint::likely,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FsContext, WritePlacement};
use axfs_ng_vfs::{
    DirEntrySink, DirNodeOps, Filesystem, FsPath, Location, Metadata, NodeFlags,
    WritebackErrorState,
    path::{Component, MAX_NAME_LEN},
};
use axio::{IoBuf, Read};
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, Pollable, PreparedPollRegistration,
};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::{
    general::{
        AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, O_APPEND, O_NONBLOCK, RLIM_INFINITY,
        RLIMIT_FSIZE,
    },
    ioctl::{FICLONE, FICLONERANGE, FIDEDUPERANGE},
};
use thekernel_linux_signal::{SignalInfo, Signo};

use super::{
    FileHandle, FileLike, FileMmapRequest, IoctlContext, Kstat, OfdIoStatus, PreparedFileMmap,
    get_file_like, get_typed_file,
    permission::{DacFsContextExt, SecurityFsContextExt, VfsSecurityContext},
    privilege_metadata::{
        ContentWriteCredentialView, ContentWritePrivilegeGuard,
        begin_conservative_content_write_privilege_cleanup, begin_content_write_privilege_cleanup,
    },
    try_owned_path,
};
use crate::{
    async_operation::AsyncOperation,
    file::{IoDst, IoSrc, memfd},
    mounts,
    pseudofs::Device,
    readiness::block_on_poll_io,
    syscall::admit_resize,
    task::{AsThread, DacCredentialView, current_fs_context, send_signal_to_process},
};

const PATH_MAX: usize = 4096;
const FILE_CLONE_RANGE_BYTES: usize = 32;
const FILE_DEDUPE_RANGE_HEADER_BYTES: usize = 24;
const FILE_DEDUPE_RANGE_INFO_BYTES: usize = 32;
const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;
// Legacy FIBMAP still has a real FUSE BMAP backend operation.  Keep the
// numerical UAPI command here rather than treating it as an ordinary device
// ioctl: its argument is a single in/out block index, not an `_IOC` buffer.
const FIBMAP: u32 = 1;

/// Poll adapter which makes the common operation cancellation source part of
/// the provider readiness registration.  Cancellation is therefore a wakeup
/// edge, rather than a check delayed until unrelated file readiness arrives.
struct CancellableFilePoll<'a> {
    file: &'a File,
    operation: &'a AsyncOperation,
}

impl Pollable for CancellableFilePoll<'_> {
    fn poll(&self) -> IoEvents {
        self.file.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let mut prepared = PreparedPollRegistration::try_new(2)?;
        prepared.arm_nested(|| self.file.register(context, events))?;
        prepared.arm(self.operation.waiters(), context.waker())?;
        prepared.commit()
    }
}

fn clone_range_ioctl(
    destination: &axfs::File,
    context: &IoctlContext,
    command: u32,
    argument: usize,
) -> AxResult<usize> {
    let (source_fd, source_offset, source_length, destination_offset) = if command == FICLONE {
        let source_fd = i32::try_from(argument).map_err(|_| AxError::BadFileDescriptor)?;
        (source_fd, 0, 0, 0)
    } else if command == FICLONERANGE {
        let mut raw = [core::mem::MaybeUninit::uninit(); FILE_CLONE_RANGE_BYTES];
        context
            .user_memory()
            .read_bytes(argument, &mut raw)
            .map_err(crate::mm::map_usercopy_error)?;
        // `read_bytes` initializes all elements on success.
        let raw: [u8; FILE_CLONE_RANGE_BYTES] = unsafe { core::mem::transmute(raw) };
        let source_fd = i64::from_ne_bytes(raw[..8].try_into().map_err(|_| AxError::InvalidInput)?);
        let source_fd = i32::try_from(source_fd).map_err(|_| AxError::BadFileDescriptor)?;
        let source_offset =
            u64::from_ne_bytes(raw[8..16].try_into().map_err(|_| AxError::InvalidInput)?);
        let source_length =
            u64::from_ne_bytes(raw[16..24].try_into().map_err(|_| AxError::InvalidInput)?);
        let destination_offset =
            u64::from_ne_bytes(raw[24..32].try_into().map_err(|_| AxError::InvalidInput)?);
        (source_fd, source_offset, source_length, destination_offset)
    } else {
        return Err(AxError::NotATty);
    };
    let source_description = context.files().get_description(source_fd)?;
    let source = source_description
        .inner
        .clone()
        .downcast_arc::<File>()
        .map_err(|_| AxError::InvalidInput)?;
    source.inner().access(axfs::FileFlags::READ)?;
    destination.access(axfs::FileFlags::WRITE)?;
    let source_size = source.inner().location().len()?;
    let length = if source_length == 0 {
        source_size
            .checked_sub(source_offset)
            .ok_or(AxError::InvalidInput)?
    } else {
        source_length
    };
    if length == 0
        || source_offset
            .checked_add(length)
            .map_or(true, |end| end > source_size)
    {
        return Err(AxError::InvalidInput);
    }
    let destination_location = destination.location();
    // Reflink changes the destination inode just as a write does. Keep every
    // content-mutation admission before the provider sees the range.
    super::inode_flags::check_nonappend_content_mutable(destination_location)?;
    let _swap_mutation = crate::mm::admit_mutation(destination_location)?;
    super::executable::check_not_active(destination_location)?;
    let security = VfsSecurityContext::new(context.caller_cred().clone());
    let _privilege_guard = begin_content_write_privilege_cleanup(
        destination_location,
        ContentWriteCredentialView::new(security.actor(), security.filesystem_owner_user_ns()),
    )?;
    let old_size = destination_location.len()?;
    let new_size = old_size.max(
        destination_offset
            .checked_add(length)
            .ok_or(AxError::InvalidInput)?,
    );
    let quota_charge = crate::syscall::admit_resize(destination_location, old_size, new_size)?;
    // Route through File so the destination's stable fileattr gate and page
    // cache invalidation cover the reflink provider commit.
    destination.clone_range_from(
        source.inner().location(),
        source_offset,
        destination_offset,
        length,
    )?;
    quota_charge.commit_actual_blocks(destination_location)?;
    if super::inode_flags::sync_on_content_write(destination_location)? {
        destination.sync(false)?;
    }
    Ok(0)
}

fn dedupe_range_ioctl(
    source: &axfs::File,
    context: &IoctlContext,
    argument: usize,
) -> AxResult<usize> {
    let mut header = [core::mem::MaybeUninit::uninit(); FILE_DEDUPE_RANGE_HEADER_BYTES];
    context
        .user_memory()
        .read_bytes(argument, &mut header)
        .map_err(crate::mm::map_usercopy_error)?;
    // `read_bytes` initializes all elements on success.
    let header: [u8; FILE_DEDUPE_RANGE_HEADER_BYTES] = unsafe { core::mem::transmute(header) };
    let source_offset =
        u64::from_ne_bytes(header[..8].try_into().map_err(|_| AxError::InvalidInput)?);
    let length = u64::from_ne_bytes(
        header[8..16]
            .try_into()
            .map_err(|_| AxError::InvalidInput)?,
    );
    let destinations = u16::from_ne_bytes(
        header[16..18]
            .try_into()
            .map_err(|_| AxError::InvalidInput)?,
    ) as usize;
    if length == 0
        || destinations == 0
        || destinations > 1024
        || header[18..].iter().any(|byte| *byte != 0)
    {
        return Err(AxError::InvalidInput);
    }
    source.access(axfs::FileFlags::READ)?;
    let source_size = source.location().len()?;
    if source_offset
        .checked_add(length)
        .map_or(true, |end| end > source_size)
    {
        return Err(AxError::InvalidInput);
    }
    let source_location = source.location().clone();
    for index in 0..destinations {
        let address = argument
            .checked_add(FILE_DEDUPE_RANGE_HEADER_BYTES)
            .and_then(|base| base.checked_add(index.checked_mul(FILE_DEDUPE_RANGE_INFO_BYTES)?))
            .ok_or(AxError::BadAddress)?;
        let mut info = [core::mem::MaybeUninit::uninit(); FILE_DEDUPE_RANGE_INFO_BYTES];
        context
            .user_memory()
            .read_bytes(address, &mut info)
            .map_err(crate::mm::map_usercopy_error)?;
        // `read_bytes` initializes all elements on success.
        let mut info: [u8; FILE_DEDUPE_RANGE_INFO_BYTES] = unsafe { core::mem::transmute(info) };
        if info[28..].iter().any(|byte| *byte != 0) {
            return Err(AxError::InvalidInput);
        }
        let destination_offset =
            u64::from_ne_bytes(info[8..16].try_into().map_err(|_| AxError::InvalidInput)?);
        // Linux reports candidate failures in each `file_dedupe_range_info`
        // and continues with later destinations.  Only malformed outer ABI
        // or a fault copying an info record is syscall-fatal.
        let result = (|| -> AxResult<bool> {
            let fd = i64::from_ne_bytes(info[..8].try_into().map_err(|_| AxError::InvalidInput)?);
            let fd = i32::try_from(fd).map_err(|_| AxError::BadFileDescriptor)?;
            let description = context.files().get_description(fd)?;
            let destination = description
                .inner
                .clone()
                .downcast_arc::<File>()
                .map_err(|_| AxError::InvalidInput)?;
            destination.inner().access(axfs::FileFlags::WRITE)?;
            let destination_location = destination.inner().location();
            // Each FIDEDUPERANGE entry is a destination mutation. Do not let
            // immutable/append, executable/swap, killpriv, quota or FS_SYNC
            // be bypassed merely because the provider shares extents.
            super::inode_flags::check_nonappend_content_mutable(destination_location)?;
            let _swap_mutation = crate::mm::admit_mutation(destination_location)?;
            super::executable::check_not_active(destination_location)?;
            let security = VfsSecurityContext::new(context.caller_cred().clone());
            let _privilege_guard = begin_content_write_privilege_cleanup(
                destination_location,
                ContentWriteCredentialView::new(
                    security.actor(),
                    security.filesystem_owner_user_ns(),
                ),
            )?;
            let old_size = destination_location.len()?;
            let new_size = old_size.max(
                destination_offset
                    .checked_add(length)
                    .ok_or(AxError::InvalidInput)?,
            );
            let quota_charge =
                crate::syscall::admit_resize(destination_location, old_size, new_size)?;
            let same = destination.inner().dedupe_range_from(
                &source_location,
                source_offset,
                destination_offset,
                length,
            )?;
            quota_charge.commit_actual_blocks(destination_location)?;
            if super::inode_flags::sync_on_content_write(destination_location)? {
                destination.inner().sync(false)?;
            }
            Ok(same)
        })();
        match result {
            Ok(true) => {
                info[16..24].copy_from_slice(&length.to_ne_bytes());
                info[24..28].copy_from_slice(&0i32.to_ne_bytes());
            }
            Ok(false) => {
                info[16..24].copy_from_slice(&0u64.to_ne_bytes());
                info[24..28].copy_from_slice(&FILE_DEDUPE_RANGE_DIFFERS.to_ne_bytes());
            }
            Err(error) => {
                info[16..24].copy_from_slice(&0u64.to_ne_bytes());
                let status = -(LinuxError::from(error) as i32);
                info[24..28].copy_from_slice(&status.to_ne_bytes());
            }
        }
        context
            .user_memory()
            .write_bytes(address, &info)
            .map_err(crate::mm::map_usercopy_error)?;
    }
    Ok(0)
}

pub(crate) fn validate_pathname(path: &FsPath) -> AxResult {
    if path.as_bytes().len() >= PATH_MAX
        || path
            .components()
            .any(|component| matches!(component, Component::Normal(name) if name.as_bytes().len() > MAX_NAME_LEN))
    {
        Err(AxError::NameTooLong)
    } else {
        Ok(())
    }
}

/// Validates the uninterpreted target accepted by Linux `symlink(2)`.
///
/// Unlike a destination pathname, the target is stored verbatim and its
/// components are not subject to `NAME_MAX` during creation.
pub(crate) fn validate_symlink_target(target: &FsPath) -> AxResult {
    if target.as_bytes().is_empty() {
        Err(AxError::NotFound)
    } else if target.as_bytes().len() >= PATH_MAX {
        Err(AxError::NameTooLong)
    } else {
        Ok(())
    }
}

fn fsize_limit() -> Option<u64> {
    let limit = current().as_thread().proc_data.rlim.read()[RLIMIT_FSIZE].current;
    (limit != RLIM_INFINITY as i64 as u64).then_some(limit)
}

fn raise_sigxfsz() {
    let curr = current();
    let pid = curr.as_thread().proc_data.proc.pid();
    let _ = send_signal_to_process(pid, Some(SignalInfo::new_kernel(Signo::SIGXFSZ)));
}

pub(crate) fn allowed_write_len(offset: u64, len: usize) -> AxResult<usize> {
    if len == 0 {
        return Ok(0);
    }

    let Some(limit) = fsize_limit() else {
        return Ok(len);
    };

    if offset >= limit {
        raise_sigxfsz();
        return Err(AxError::from(LinuxError::EFBIG));
    }

    Ok(len.min(limit.saturating_sub(offset) as usize))
}

pub(crate) fn check_resize_limit(new_len: u64) -> AxResult<()> {
    let Some(limit) = fsize_limit() else {
        return Ok(());
    };

    if new_len > limit {
        raise_sigxfsz();
        return Err(AxError::from(LinuxError::EFBIG));
    }

    Ok(())
}

pub fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsContext) -> AxResult<R>) -> AxResult<R> {
    let fs_context = current_fs_context();
    let mut fs = fs_context.lock();
    if dirfd == AT_FDCWD {
        f(&mut fs)
    } else {
        let dir = Directory::from_fd(dirfd)?.inner.clone();
        f(&mut fs.with_current_dir(dir)?)
    }
}

pub fn with_path_fs<R>(
    dirfd: c_int,
    path: &FsPath,
    f: impl FnOnce(&mut FsContext) -> AxResult<R>,
) -> AxResult<R> {
    let fs_context = current_fs_context();
    let mut fs = fs_context.lock();
    if dirfd == AT_FDCWD || path.is_absolute() {
        f(&mut fs)
    } else {
        let dir = Directory::from_fd(dirfd)?.inner.clone();
        f(&mut fs.with_current_dir(dir)?)
    }
}

pub enum ResolveAtResult {
    File(Location),
    Other(FileHandle<dyn FileLike>),
}

impl ResolveAtResult {
    pub fn into_file(self) -> Option<Location> {
        match self {
            Self::File(file) => Some(file),
            Self::Other(_) => None,
        }
    }

    pub fn stat(&self) -> AxResult<Kstat> {
        match self {
            Self::File(file) => location_to_kstat(file),
            Self::Other(file_like) => file_like.stat(),
        }
    }
}

pub fn resolve_at(dirfd: c_int, path: Option<&FsPath>, flags: u32) -> AxResult<ResolveAtResult> {
    let current = current();
    let security = VfsSecurityContext::new(current.as_thread().current_cred());
    resolve_at_with_security(dirfd, path, flags, &security)
}

/// Resolves with an explicit synthetic DAC projection. This exists for the
/// non-AT_EACCESS access(2) real-ID/permitted-capability view, which cannot be
/// rebound to the live effective actor's typed security state.
pub(crate) fn resolve_at_with_synthetic_credentials(
    dirfd: c_int,
    path: Option<&FsPath>,
    flags: u32,
    credentials: &DacCredentialView,
) -> AxResult<ResolveAtResult> {
    match path {
        Some(path) if !path.as_bytes().is_empty() => with_path_fs(dirfd, path, |fs| {
            if flags & AT_SYMLINK_NOFOLLOW != 0 {
                fs.resolve_no_follow_dac(path, credentials)
            } else {
                fs.resolve_dac(path, credentials)
            }
            .map(ResolveAtResult::File)
        }),
        _ => {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(AxError::NotFound);
            }
            if dirfd == AT_FDCWD {
                return Ok(ResolveAtResult::File(
                    current_fs_context().lock().current_dir().clone(),
                ));
            }
            let file_like = get_file_like(dirfd)?;
            let f = file_like.clone();
            Ok(if let Some(file) = f.downcast_ref::<File>() {
                ResolveAtResult::File(file.inner().location().clone())
            } else if let Some(dir) = f.downcast_ref::<Directory>() {
                ResolveAtResult::File(dir.inner().clone())
            } else {
                ResolveAtResult::Other(file_like)
            })
        }
    }
}

/// Resolves one path or metadata fd with a single frozen composite actor.
///
/// Pathname traversal runs both Linux DAC and the typed inode-permission hook
/// stack for every searched directory. The final inode is deliberately left
/// to the operation-specific hook (for example `inode_setattr`) so callers do
/// not manufacture a generic read/write permission request that Linux never
/// performs for that operation.
pub(crate) fn resolve_at_with_security(
    dirfd: c_int,
    path: Option<&FsPath>,
    flags: u32,
    security: &VfsSecurityContext,
) -> AxResult<ResolveAtResult> {
    match path {
        Some(path) if !path.as_bytes().is_empty() => with_path_fs(dirfd, path, |fs| {
            if flags & AT_SYMLINK_NOFOLLOW != 0 {
                fs.resolve_no_follow_security(path, security)
            } else {
                fs.resolve_security(path, security)
            }
            .map(ResolveAtResult::File)
        }),
        _ => {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(AxError::NotFound);
            }
            if dirfd == AT_FDCWD {
                return Ok(ResolveAtResult::File(
                    current_fs_context().lock().current_dir().clone(),
                ));
            }
            let file_like = get_file_like(dirfd)?;
            let file = file_like.clone();
            Ok(if let Some(file) = file.downcast_ref::<File>() {
                ResolveAtResult::File(file.inner().location().clone())
            } else if let Some(directory) = file.downcast_ref::<Directory>() {
                ResolveAtResult::File(directory.inner().clone())
            } else {
                ResolveAtResult::Other(file_like)
            })
        }
    }
}

pub fn metadata_to_kstat(metadata: &Metadata) -> Kstat {
    let ty = metadata.node_type as u8;
    let perm = metadata.mode.bits() as u32;
    let mode = ((ty as u32) << 12) | perm;
    Kstat {
        dev: mounts::linux_device_id(metadata.device).0,
        mnt_id: 0,
        ino: metadata.inode,
        mode,
        nlink: metadata.nlink as _,
        uid: metadata.uid,
        gid: metadata.gid,
        size: metadata.size,
        blksize: metadata.block_size as _,
        blocks: metadata.blocks,
        rdev: metadata.rdev,
        attributes: 0,
        attributes_mask: 0,
        atime: metadata.atime,
        btime: metadata.btime,
        metadata_capabilities: Default::default(),
        mtime: metadata.mtime,
        ctime: metadata.ctime,
    }
}

pub fn location_to_kstat(loc: &Location) -> AxResult<Kstat> {
    let idmap = if let Some(task) = axtask::current_may_uninit()
        && let Some(thread) = task.try_as_thread()
    {
        thread
            .mount_ns()
            .topology()
            .idmap_for_mount(loc.mountpoint().mount_id())?
    } else {
        None
    };
    location_to_kstat_with_idmap(loc, idmap.as_deref())
}

pub(crate) fn location_to_kstat_with_idmap(
    loc: &Location,
    idmap: Option<&crate::mounts::MountIdmap>,
) -> AxResult<Kstat> {
    let metadata = loc.metadata()?;
    let mut stat = metadata_to_kstat(&metadata);
    stat.metadata_capabilities = loc.metadata_capabilities(&metadata);
    if let Some(idmap) = idmap {
        let project = |id: u32, rows: &[crate::mounts::MountIdmapRange]| {
            rows.iter()
                .find_map(|row| {
                    let end = row.outside.checked_add(row.length)?;
                    (id >= row.outside && id < end)
                        .then_some(row.inside.checked_add(id - row.outside))
                        .flatten()
                })
                .unwrap_or(u32::MAX)
        };
        stat.uid = project(stat.uid, &idmap.uid);
        stat.gid = project(stat.gid, &idmap.gid);
    }
    stat.mnt_id = loc.mountpoint().mount_id();
    let (attributes, attributes_mask) = super::inode_flags::statx_attributes(loc)?;
    stat.attributes = attributes;
    stat.attributes_mask = attributes_mask;
    Ok(stat)
}

/// File wrapper for `axfs::fops::File`.
pub struct File {
    inner: axfs::File,
    nonblock: AtomicBool,
    landlock_ioctl_dev_allowed: bool,
    landlock_truncate_allowed: bool,
}

struct ReplayableWriteSource<'a, S: ?Sized> {
    source: &'a mut S,
    cache: Vec<u8>,
    requested: usize,
    position: usize,
    admitted: &'a Cell<Option<usize>>,
}

impl<'a, S> ReplayableWriteSource<'a, S>
where
    S: Read + IoBuf + ?Sized,
{
    fn new(source: &'a mut S, admitted: &'a Cell<Option<usize>>) -> Self {
        Self {
            requested: source.remaining(),
            source,
            cache: Vec::new(),
            position: 0,
            admitted,
        }
    }

    fn begin_attempt(&mut self) {
        self.position = 0;
        self.admitted.set(None);
    }
}

impl<S> Read for ReplayableWriteSource<'_, S>
where
    S: Read + IoBuf + ?Sized,
{
    fn read(&mut self, dst: &mut [u8]) -> axio::Result<usize> {
        let limit = dst.len().min(self.remaining());
        if limit == 0 {
            return Ok(0);
        }

        let cached = self.cache.len().saturating_sub(self.position).min(limit);
        if cached != 0 {
            dst[..cached].copy_from_slice(&self.cache[self.position..self.position + cached]);
            self.position += cached;
            return Ok(cached);
        }

        let start = self.cache.len();
        self.cache
            .try_reserve_exact(limit)
            .map_err(|_| AxError::NoMemory)?;
        self.cache.resize(start + limit, 0);
        let read = match self.source.read(&mut self.cache[start..]) {
            Ok(read) if read <= limit => read,
            Ok(_) => {
                self.cache.truncate(start);
                return Err(AxError::InvalidInput);
            }
            Err(error) => {
                self.cache.truncate(start);
                return Err(error);
            }
        };
        self.cache.truncate(start + read);
        dst[..read].copy_from_slice(&self.cache[start..]);
        self.position += read;
        Ok(read)
    }
}

impl<S> IoBuf for ReplayableWriteSource<'_, S>
where
    S: Read + IoBuf + ?Sized,
{
    fn remaining(&self) -> usize {
        self.admitted
            .get()
            .unwrap_or(self.requested)
            .saturating_sub(self.position)
    }
}

#[derive(Clone, Copy)]
enum ContentWriteSecurity<'a> {
    Exact(&'a VfsSecurityContext),
    Conservative,
}

impl ContentWriteSecurity<'_> {
    fn begin(self, location: &Location) -> AxResult<ContentWritePrivilegeGuard> {
        match self {
            Self::Exact(security) => begin_content_write_privilege_cleanup(
                location,
                ContentWriteCredentialView::new(
                    security.actor(),
                    security.filesystem_owner_user_ns(),
                ),
            ),
            Self::Conservative => begin_conservative_content_write_privilege_cleanup(location),
        }
    }
}

impl File {
    pub(crate) fn supports_rwf_nowait_read(&self) -> AxResult<bool> {
        self.inner().supports_nowait_read().map_err(Into::into)
    }

    pub(crate) fn supports_rwf_nowait_write(&self) -> AxResult<bool> {
        self.inner().supports_nowait_write().map_err(Into::into)
    }

    fn nowait_read_admitted(&self, offset: u64, length: usize) -> AxResult<bool> {
        if !self.supports_rwf_nowait_read()? {
            return Err(AxError::OperationNotSupported);
        }
        // High-level File owns the cache range proof and invokes NodeOps only
        // for direct/uncached OFDs.  A cached miss must remain a miss here;
        // falling back at this layer could issue provider I/O for NOWAIT.
        self.inner()
            .nowait_read_admit(offset, length)
            .map_err(Into::into)
    }

    fn nowait_write_admitted(&self, offset: u64, length: usize) -> AxResult<bool> {
        if !self.supports_rwf_nowait_write()? {
            return Err(AxError::OperationNotSupported);
        }
        self.inner()
            .nowait_write_admit(offset, length)
            .map_err(Into::into)
    }
    pub(crate) fn sync_range(&self, offset: u64, len: u64) -> AxResult<()> {
        self.inner.sync_range(offset, len, true)
    }
    pub fn new(inner: axfs::File) -> Self {
        Self {
            inner,
            nonblock: AtomicBool::new(false),
            landlock_ioctl_dev_allowed: true,
            landlock_truncate_allowed: true,
        }
    }

    pub(crate) fn with_landlock_permissions(
        inner: axfs::File,
        allowed: bool,
        truncate_allowed: bool,
    ) -> Self {
        Self {
            inner,
            nonblock: AtomicBool::new(false),
            landlock_ioctl_dev_allowed: allowed,
            landlock_truncate_allowed: truncate_allowed,
        }
    }

    pub(crate) const fn landlock_ioctl_dev_allowed(&self) -> bool {
        self.landlock_ioctl_dev_allowed
    }
    pub(crate) const fn landlock_truncate_allowed(&self) -> bool {
        self.landlock_truncate_allowed
    }

    pub fn inner(&self) -> &axfs::File {
        &self.inner
    }

    /// Begins Linux privilege cleanup using the exact actor and filesystem
    /// owner namespace frozen by the syscall. The returned guard must cover the
    /// complete backend content mutation.
    pub(crate) fn begin_content_write_privilege_cleanup(
        &self,
        security: &VfsSecurityContext,
    ) -> AxResult<ContentWritePrivilegeGuard> {
        // File attributes are VFS invariants.  Check them here, at the one
        // admission point shared by ordinary I/O, io_uring, splice/copy and
        // writeback-assisted operations, before killpriv or backend mutation.
        super::inode_flags::check_content_mutable(self.inner.location())?;
        ContentWriteSecurity::Exact(security).begin(self.inner.location())
    }

    fn is_blocking(&self) -> bool {
        self.inner.location().flags().contains(NodeFlags::BLOCKING)
    }

    fn is_stream_or_no_seek(&self) -> bool {
        self.inner
            .location()
            .flags()
            .intersects(NodeFlags::STREAM | NodeFlags::NO_SEEK)
    }

    /// Reads using one immutable open-file-description status snapshot.
    pub(crate) fn read_with_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        if let Some(handle) = self.inner().open_handle()
            && let Ok(pipe) = handle
                .clone()
                .into_any()
                .downcast::<crate::pseudofs::rpc_pipefs::RpcPipeOpenHandle>()
        {
            return block_on_poll_io(
                self,
                IoEvents::READABLE | IoEvents::HANGUP,
                status.nonblocking() || status.rwf_nowait(),
                || pipe.read_user(dst),
            );
        }
        let inner = self.inner();
        // Stream/no-seek VFS adapters do not have a positioned regular-file
        // NOWAIT admission.  Their operation-local readiness is the provider
        // admission: preserve the frozen status and never let a NOWAIT call
        // enter the BLOCKING fast path.
        if status.rwf_nowait() && self.is_stream_or_no_seek() {
            return block_on_poll_io(self, IoEvents::READABLE | IoEvents::HANGUP, true, || {
                inner.read(&mut *dst)
            });
        }
        if likely(self.is_blocking()) {
            inner.read(dst)
        } else {
            block_on_poll_io(
                self,
                IoEvents::READABLE,
                status.nonblocking() || status.rwf_nowait(),
                || inner.read(&mut *dst),
            )
        }
    }

    /// Reads at a caller-frozen open-file-description position.
    ///
    /// The caller owns any current-position transaction; this method must not
    /// reacquire it while providing the same blocking semantics as `read`.
    pub(crate) fn read_at_with_status(
        &self,
        status: OfdIoStatus,
        dst: &mut IoDst,
        offset: u64,
    ) -> AxResult<usize> {
        let inner = self.inner();
        if status.rwf_nowait() && !self.nowait_read_admitted(offset, dst.remaining())? {
            return Err(AxError::WouldBlock);
        }
        if likely(self.is_blocking()) {
            inner.read_at(dst, offset)
        } else {
            block_on_poll_io(
                self,
                IoEvents::READABLE,
                status.nonblocking() || status.rwf_nowait(),
                || inner.read_at(&mut *dst, offset),
            )
        }
    }

    /// Cancellable positioned-read provider entry point used by classic AIO.
    /// Filesystems with an operation engine can override the lower open
    /// handle; this common VFS boundary guarantees that cancellation is
    /// observed on both sides of provider admission.
    pub(crate) fn read_at_with_status_cancellable(
        &self,
        status: OfdIoStatus,
        dst: &mut IoDst,
        offset: u64,
        operation: &AsyncOperation,
    ) -> AxResult<usize> {
        if operation.cancellation_requested() {
            return Err(LinuxError::ECANCELED.into());
        }
        let inner = self.inner();
        if status.rwf_nowait() && !self.nowait_read_admitted(offset, dst.remaining())? {
            return Err(AxError::WouldBlock);
        }
        if likely(self.is_blocking()) {
            inner.read_at(dst, offset)
        } else {
            let source = CancellableFilePoll {
                file: self,
                operation,
            };
            block_on_poll_io(
                &source,
                IoEvents::READABLE,
                status.nonblocking() || status.rwf_nowait(),
                || {
                    if operation.cancellation_requested() {
                        Err(LinuxError::ECANCELED.into())
                    } else {
                        inner.read_at(&mut *dst, offset)
                    }
                },
            )
        }
    }

    /// Writes using one immutable open-file-description status snapshot.
    ///
    /// Every status-sensitive decision for this operation, including append
    /// placement and poll behavior, is derived from `status`. Backend status
    /// mirrors are deliberately not consulted by this path.
    pub(crate) fn write_with_status(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        security: &VfsSecurityContext,
    ) -> AxResult<usize> {
        self.write_with_status_and_direct_validation(status, src, security, |_offset, _allowed| {
            Ok(())
        })
    }

    /// Writes using one status snapshot and validates the exact admitted
    /// offset/prefix before the backend can mutate the inode.
    ///
    /// Ordinary append admission runs inside axfs-ng's inode append domain, so
    /// Linux policy never reasons from a stale EOF. The validator is supplied
    /// by syscall glue because user-buffer alignment is an ABI concern rather
    /// than a VFS mechanism.
    pub(crate) fn write_with_status_and_direct_validation(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        security: &VfsSecurityContext,
        mut validate_direct: impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<usize> {
        self.write_with_status_and_direct_validation_inner(
            status,
            src,
            ContentWriteSecurity::Exact(security),
            &mut validate_direct,
        )
    }

    fn write_with_status_and_direct_validation_inner(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        security: ContentWriteSecurity<'_>,
        validate_direct: &mut impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<usize> {
        if let Some(handle) = self.inner().open_handle()
            && let Ok(pipe) = handle
                .clone()
                .into_any()
                .downcast::<crate::pseudofs::rpc_pipefs::RpcPipeOpenHandle>()
        {
            return block_on_poll_io(
                self,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                || pipe.write_user(src),
            );
        }
        let inner = self.inner();
        let memfd_mutation = memfd::begin_write(inner.location(), src.remaining())?;
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let placement = if status.append() {
            WritePlacement::End
        } else {
            WritePlacement::Current
        };
        let inode_append = placement == WritePlacement::End
            && inner.has_current_position()
            && !inner
                .location()
                .flags()
                .contains(NodeFlags::POSITIONED_APPEND);
        let mut write = || {
            replay.begin_attempt();
            let mut privilege_guard = None;
            let mut quota_charge = None;
            let result = inner.write_with_placement_and_admission(
                &mut replay,
                placement,
                |offset, requested| {
                    let file_len = if inode_append {
                        offset
                    } else {
                        inner.location().len()?
                    };
                    let (allowed, guard) = self.admit_content_write(
                        offset,
                        requested,
                        file_len,
                        placement == WritePlacement::End,
                        &memfd_mutation,
                        security,
                        validate_direct,
                    )?;
                    privilege_guard = guard;
                    quota_charge = Some(admit_resize(
                        inner.location(),
                        file_len,
                        file_len.max(offset.saturating_add(allowed as u64)),
                    )?);
                    admitted.set(Some(allowed));
                    Ok(allowed)
                },
            );
            drop(privilege_guard);
            if result.is_ok()
                && let Some(charge) = quota_charge
            {
                charge.commit_actual_blocks(inner.location())?;
            }
            result
        };
        if status.rwf_nowait() && self.is_stream_or_no_seek() {
            block_on_poll_io(self, IoEvents::WRITABLE | IoEvents::HANGUP, true, write)
        } else if likely(self.is_blocking()) {
            write()
        } else {
            block_on_poll_io(
                self,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                write,
            )
        }
    }

    fn admit_content_write(
        &self,
        offset: u64,
        requested: usize,
        file_len: u64,
        append: bool,
        memfd_mutation: &memfd::MemfdMutationGuard,
        security: ContentWriteSecurity<'_>,
        validate_direct: &mut impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<(
        usize,
        Option<(ContentWritePrivilegeGuard, crate::mm::MutationAdmission)>,
    )> {
        if requested == 0 {
            return Ok((0, None));
        }

        let location = self.inner.location();
        super::inode_flags::check_data_write(location, offset, append)?;
        // This guard survives through the actual backend write, closing the
        // swapon check-to-effect race for write/pwrite/vector/direct paths.
        let swap_mutation = crate::mm::admit_mutation(location)?;
        super::executable::check_not_active(location)?;
        let allowed = allowed_write_len(offset, requested)?;
        validate_direct(offset, allowed)?;
        memfd_mutation.admit_write(location, file_len, offset, allowed)?;
        let privilege_guard = (allowed != 0)
            .then(|| security.begin(location))
            .transpose()?
            .map(|guard| (guard, swap_mutation));
        Ok((allowed, privilege_guard))
    }

    /// Appends without changing the open-file-description position.
    ///
    /// This is the `pwrite*` counterpart of
    /// [`write_with_status_and_direct_validation`](Self::write_with_status_and_direct_validation).
    /// The exact EOF and admitted prefix are protected by the same axfs-ng
    /// append transaction used for the lower inode operation.
    pub(crate) fn write_at_end_with_status_and_direct_validation(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        security: &VfsSecurityContext,
        mut validate_direct: impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<usize> {
        let inner = self.inner();
        let memfd_mutation = memfd::begin_write(inner.location(), src.remaining())?;
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let mut write = || {
            replay.begin_attempt();
            let mut privilege_guard = None;
            let result = inner.write_at_end_with_admission(&mut replay, |offset, requested| {
                if status.rwf_nowait() && !self.nowait_write_admitted(offset, requested)? {
                    return Err(AxError::WouldBlock);
                }
                let (allowed, guard) = self.admit_content_write(
                    offset,
                    requested,
                    offset,
                    true,
                    &memfd_mutation,
                    ContentWriteSecurity::Exact(security),
                    &mut validate_direct,
                )?;
                privilege_guard = guard;
                admitted.set(Some(allowed));
                Ok(allowed)
            });
            drop(privilege_guard);
            result
        };
        if likely(self.is_blocking()) {
            write()
        } else {
            block_on_poll_io(
                self,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                write,
            )
        }
    }

    /// The append transaction returns the exact start selected under its
    /// inode append lock.  RWF_DONTCACHE uses this to evict precisely the
    /// written range without racing a subsequent append.
    pub(crate) fn write_at_end_with_status_and_direct_validation_and_start(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        security: &VfsSecurityContext,
        mut validate_direct: impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<(usize, u64)> {
        let inner = self.inner();
        // Reject a known cache/provider miss before acquiring the shared
        // current-position/append transaction. The exact EOF is rechecked by
        // the callback below after its nonblocking cursor admission.
        if status.rwf_nowait()
            && !self.nowait_write_admitted(inner.location().len()?, src.remaining())?
        {
            return Err(AxError::WouldBlock);
        }
        let memfd_mutation = memfd::begin_write(inner.location(), src.remaining())?;
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let mut write = || {
            replay.begin_attempt();
            let mut privilege_guard = None;
            let result = if status.rwf_nowait() {
                inner
                    .try_write_at_end_with_admission_and_start(&mut replay, |offset, requested| {
                        if !self.nowait_write_admitted(offset, requested)? {
                            return Err(AxError::WouldBlock);
                        }
                        let (allowed, guard) = self.admit_content_write(
                            offset,
                            requested,
                            offset,
                            true,
                            &memfd_mutation,
                            ContentWriteSecurity::Exact(security),
                            &mut validate_direct,
                        )?;
                        privilege_guard = guard;
                        admitted.set(Some(allowed));
                        Ok(allowed)
                    })?
                    .ok_or(AxError::WouldBlock)
            } else {
                inner.write_at_end_with_admission_and_start(&mut replay, |offset, requested| {
                    if status.rwf_nowait() && !self.nowait_write_admitted(offset, requested)? {
                        return Err(AxError::WouldBlock);
                    }
                    let (allowed, guard) = self.admit_content_write(
                        offset,
                        requested,
                        offset,
                        true,
                        &memfd_mutation,
                        ContentWriteSecurity::Exact(security),
                        &mut validate_direct,
                    )?;
                    privilege_guard = guard;
                    admitted.set(Some(allowed));
                    Ok(allowed)
                })
            };
            drop(privilege_guard);
            result
        };
        if likely(self.is_blocking()) {
            write()
        } else {
            block_on_poll_io(
                self,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                write,
            )
        }
    }

    /// `pwritev2(offset=-1)` append: append and advance the frozen OFD cursor
    /// to the committed EOF while returning the exact append start.
    pub(crate) fn write_at_current_append_with_status_and_direct_validation_and_start(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        security: &VfsSecurityContext,
        mut validate_direct: impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<(usize, u64)> {
        let inner = self.inner();
        // Keep the provider/cache admission outside the shared cursor and
        // append transactions. The exact EOF is checked again after the
        // nonblocking cursor acquisition below.
        if status.rwf_nowait()
            && !self.nowait_write_admitted(inner.location().len()?, src.remaining())?
        {
            return Err(AxError::WouldBlock);
        }
        let memfd_mutation = memfd::begin_write(inner.location(), src.remaining())?;
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let mut write = || {
            replay.begin_attempt();
            let mut privilege_guard = None;
            let result = if status.rwf_nowait() {
                inner
                    .try_write_at_current_append_with_admission_and_start(
                        &mut replay,
                        |offset, requested| {
                            if !self.nowait_write_admitted(offset, requested)? {
                                return Err(AxError::WouldBlock);
                            }
                            let (allowed, guard) = self.admit_content_write(
                                offset,
                                requested,
                                offset,
                                true,
                                &memfd_mutation,
                                ContentWriteSecurity::Exact(security),
                                &mut validate_direct,
                            )?;
                            privilege_guard = guard;
                            admitted.set(Some(allowed));
                            Ok(allowed)
                        },
                    )?
                    .ok_or(AxError::WouldBlock)
            } else {
                inner.write_at_current_append_with_admission_and_start(
                    &mut replay,
                    |offset, requested| {
                        if status.rwf_nowait() && !self.nowait_write_admitted(offset, requested)? {
                            return Err(AxError::WouldBlock);
                        }
                        let (allowed, guard) = self.admit_content_write(
                            offset,
                            requested,
                            offset,
                            true,
                            &memfd_mutation,
                            ContentWriteSecurity::Exact(security),
                            &mut validate_direct,
                        )?;
                        privilege_guard = guard;
                        admitted.set(Some(allowed));
                        Ok(allowed)
                    },
                )
            };
            drop(privilege_guard);
            result
        };
        if likely(self.is_blocking()) {
            write()
        } else {
            block_on_poll_io(
                self,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                write,
            )
        }
    }

    pub(crate) fn write_at_end_with_status_and_direct_validation_cancellable(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        security: &VfsSecurityContext,
        operation: &AsyncOperation,
        mut validate_direct: impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<usize> {
        if operation.cancellation_requested() {
            return Err(LinuxError::ECANCELED.into());
        }
        let inner = self.inner();
        let memfd_mutation = memfd::begin_write(inner.location(), src.remaining())?;
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let mut write = || {
            if operation.cancellation_requested() {
                return Err(LinuxError::ECANCELED.into());
            }
            replay.begin_attempt();
            let mut privilege_guard = None;
            let result = if status.rwf_nowait() {
                inner
                    .try_write_at_end_with_admission_and_start(&mut replay, |offset, requested| {
                        // Cancellation wins until this prepared transaction
                        // reaches its provider commit point.
                        if operation.cancellation_requested() {
                            return Err(LinuxError::ECANCELED.into());
                        }
                        if !self.nowait_write_admitted(offset, requested)? {
                            return Err(AxError::WouldBlock);
                        }
                        let (allowed, guard) = self.admit_content_write(
                            offset,
                            requested,
                            offset,
                            true,
                            &memfd_mutation,
                            ContentWriteSecurity::Exact(security),
                            &mut validate_direct,
                        )?;
                        privilege_guard = guard;
                        admitted.set(Some(allowed));
                        Ok(allowed)
                    })?
                    .map(|(written, _)| written)
                    .ok_or(AxError::WouldBlock)
            } else {
                inner.write_at_end_with_admission(&mut replay, |offset, requested| {
                    if operation.cancellation_requested() {
                        return Err(LinuxError::ECANCELED.into());
                    }
                    if status.rwf_nowait() && !self.nowait_write_admitted(offset, requested)? {
                        return Err(AxError::WouldBlock);
                    }
                    let (allowed, guard) = self.admit_content_write(
                        offset,
                        requested,
                        offset,
                        true,
                        &memfd_mutation,
                        ContentWriteSecurity::Exact(security),
                        &mut validate_direct,
                    )?;
                    privilege_guard = guard;
                    admitted.set(Some(allowed));
                    Ok(allowed)
                })
            };
            drop(privilege_guard);
            result
        };
        if likely(self.is_blocking()) {
            write()
        } else {
            let source = CancellableFilePoll {
                file: self,
                operation,
            };
            block_on_poll_io(
                &source,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                || write(),
            )
        }
    }

    /// Writes at a caller-frozen open-file-description position.
    ///
    /// RLIMIT_FSIZE and memfd-seal admission use exactly `offset`, and the
    /// backend operation remains positioned so an outer current-position
    /// transaction can commit the accepted prefix once without recursion.
    pub(crate) fn write_at_with_status_and_direct_validation(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        offset: u64,
        security: &VfsSecurityContext,
        mut validate_direct: impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<usize> {
        let inner = self.inner();
        let memfd_mutation = memfd::begin_write(inner.location(), src.remaining())?;
        let requested = src.remaining();
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let mut write = || {
            replay.begin_attempt();
            let requested = replay.remaining();
            let (allowed, privilege_guard) = self.admit_content_write(
                offset,
                requested,
                inner.location().len()?,
                false,
                &memfd_mutation,
                ContentWriteSecurity::Exact(security),
                &mut validate_direct,
            )?;
            admitted.set(Some(allowed));
            let result = inner.write_at(&mut replay, offset);
            drop(privilege_guard);
            result
        };
        if status.rwf_nowait() && !self.nowait_write_admitted(offset, requested)? {
            return Err(AxError::WouldBlock);
        }
        if likely(self.is_blocking()) {
            write()
        } else {
            block_on_poll_io(
                self,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                write,
            )
        }
    }

    pub(crate) fn write_at_with_status_and_direct_validation_cancellable(
        &self,
        status: OfdIoStatus,
        src: &mut IoSrc,
        offset: u64,
        security: &VfsSecurityContext,
        operation: &AsyncOperation,
        mut validate_direct: impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<usize> {
        if operation.cancellation_requested() {
            return Err(LinuxError::ECANCELED.into());
        }
        let inner = self.inner();
        let memfd_mutation = memfd::begin_write(inner.location(), src.remaining())?;
        let requested = src.remaining();
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let mut write = || {
            if operation.cancellation_requested() {
                return Err(LinuxError::ECANCELED.into());
            }
            replay.begin_attempt();
            let requested = replay.remaining();
            let (allowed, privilege_guard) = self.admit_content_write(
                offset,
                requested,
                inner.location().len()?,
                false,
                &memfd_mutation,
                ContentWriteSecurity::Exact(security),
                &mut validate_direct,
            )?;
            admitted.set(Some(allowed));
            let result = if operation.cancellation_requested() {
                Err(LinuxError::ECANCELED.into())
            } else {
                inner.write_at(&mut replay, offset)
            };
            drop(privilege_guard);
            result
        };
        if status.rwf_nowait() && !self.nowait_write_admitted(offset, requested)? {
            return Err(AxError::WouldBlock);
        }
        if likely(self.is_blocking()) {
            write()
        } else {
            let source = CancellableFilePoll {
                file: self,
                operation,
            };
            block_on_poll_io(
                &source,
                IoEvents::WRITABLE,
                status.nonblocking() || status.rwf_nowait(),
                || write(),
            )
        }
    }
}

fn path_for(loc: &Location) -> AxResult<Cow<'static, FsPath>> {
    let path = loc.absolute_path()?;
    Ok(Cow::Owned(try_owned_path(&path)?))
}

impl FileLike for File {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let result = self.read_with_status(
            OfdIoStatus::new(if self.nonblocking() { O_NONBLOCK } else { 0 }),
            dst,
        );
        #[cfg(feature = "bpf")]
        if let Ok(count) = &result {
            let mut context = [0u8; 16];
            context[..8].copy_from_slice(&1u64.to_ne_bytes());
            context[8..].copy_from_slice(&(*count as u64).to_ne_bytes());
            crate::bpf::run_struct_ops(&mut context);
        }
        result
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let raw_status = if self.inner().flags().contains(axfs::FileFlags::APPEND) {
            O_APPEND
        } else {
            0
        } | if self.nonblocking() { O_NONBLOCK } else { 0 };
        // Syscall paths downcast regular files and supply an exact
        // VfsSecurityContext. Keep this generic trait fallback safe for
        // inherited or kernel-internal handles without sampling current().
        let mut validate = |_offset, _allowed| Ok(());
        let result = self.write_with_status_and_direct_validation_inner(
            OfdIoStatus::new(raw_status),
            src,
            ContentWriteSecurity::Conservative,
            &mut validate,
        );
        #[cfg(feature = "bpf")]
        if let Ok(count) = &result {
            let mut context = [0u8; 16];
            context[..8].copy_from_slice(&2u64.to_ne_bytes());
            context[8..].copy_from_slice(&(*count as u64).to_ne_bytes());
            crate::bpf::run_struct_ops(&mut context);
        }
        result
    }

    fn stat(&self) -> AxResult<Kstat> {
        location_to_kstat(self.inner().location())
    }

    fn vfs_location(&self) -> Option<&Location> {
        Some(self.inner().location())
    }

    fn cachestat(&self, first_page: u64, last_page: u64) -> AxResult<axfs::CachedFileCacheStat> {
        Ok(self.inner().cachestat(first_page, last_page))
    }

    fn cachestat_location(&self) -> Option<&Location> {
        Some(self.inner().location())
    }

    fn cachestat_is_hugetlbfs(&self) -> bool {
        // This is a superblock property, not a pathname convention: bind
        // mounts and renamed files retain the same hugetlbfs admission rule.
        self.inner().location().filesystem().name() == "hugetlbfs"
    }

    /// Regular files delegate object-owned fixed mappings to their VFS
    /// provider.  hugetlbfs uses this typed boundary to export the inode's
    /// exact `SharedPages` backing; ordinary files return `None` and continue
    /// through the normal cached/direct mmap path.
    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        crate::pseudofs::hugetlb::prepare_mmap(self.inner().location(), request)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if cmd == FIBMAP {
            let handle = self.inner().open_handle().ok_or(AxError::NotATty)?;
            let fuse = handle
                .clone()
                .into_any()
                .downcast::<crate::pseudofs::dev::fuse::FuseOpenFile>()
                .map_err(|_| AxError::NotATty)?;
            let block = context
                .user_memory()
                .read_value::<i32>(arg as *const i32)
                .map_err(crate::mm::map_usercopy_error)?;
            if block < 0 {
                return Err(AxError::InvalidInput);
            }
            let block_size = self.inner().location().metadata()?.block_size;
            let block_size = u32::try_from(block_size).map_err(|_| AxError::InvalidInput)?;
            let mapped = fuse.bmap(block as u64, block_size)?;
            let mapped = i32::try_from(mapped).map_err(|_| AxError::InvalidInput)?;
            context
                .user_memory()
                .write_value(arg as *mut i32, mapped)
                .map_err(crate::mm::map_usercopy_error)?;
            return Ok(0);
        }
        if super::fiemap::is_fiemap_command(cmd) {
            return super::fiemap::ioctl(self.inner(), context, arg);
        }
        if matches!(cmd, FICLONE | FICLONERANGE) {
            return clone_range_ioctl(self.inner(), context, cmd, arg);
        }
        if cmd == FIDEDUPERANGE {
            return dedupe_range_ioctl(self.inner(), context, arg);
        }
        if let Some(result) = super::inode_flags::ioctl(self.inner().location(), context, cmd, arg)
        {
            return result;
        }
        if let Some(handle) = self.inner().open_handle()
            && let Some(fuse) = handle
                .clone()
                .into_any()
                .downcast::<crate::pseudofs::dev::fuse::FuseOpenFile>()
                .ok()
            && let Some(result) = fuse.ioctl(context, cmd, arg)
        {
            return result;
        }
        let location = self.inner().backend()?.location();
        let device = location
            .entry()
            .downcast::<Device>()
            .map_err(|_| AxError::NotATty)?;
        device.inner().ioctl(context, cmd, arg)
    }

    fn sync(&self, data_only: bool) -> AxResult<()> {
        self.inner.sync(data_only)
    }

    fn writeback_error_state(&self) -> AxResult<Arc<WritebackErrorState>> {
        self.inner.location().writeback_error_state()
    }

    fn syncfs_filesystem(&self) -> Option<Filesystem> {
        Some(self.inner().location().mountpoint().filesystem_handle())
    }

    fn set_nonblocking(&self, flag: bool) -> AxResult {
        self.nonblock.store(flag, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Acquire)
    }

    fn path(&self) -> AxResult<Cow<'_, FsPath>> {
        path_for(self.inner.location())
    }

    fn from_fd(fd: c_int) -> AxResult<FileHandle<Self>>
    where
        Self: Sized + 'static,
    {
        match get_typed_file(fd) {
            Ok(file) => Ok(file),
            Err(AxError::InvalidInput) => {
                let file = get_file_like(fd)?;
                if file.downcast_ref::<Directory>().is_some() {
                    Err(AxError::IsADirectory)
                } else {
                    Err(AxError::BrokenPipe)
                }
            }
            Err(err) => Err(err),
        }
    }
}
impl Pollable for File {
    fn poll(&self) -> IoEvents {
        self.inner()
            .open_handle()
            .map_or_else(|| self.inner().location().poll(), |handle| handle.poll())
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        match self.inner().open_handle() {
            Some(handle) => handle.register(context, events),
            None => self.inner().location().register(context, events),
        }
    }
}

/// Directory wrapper for `axfs::fops::Directory`.
pub struct Directory {
    inner: Location,
    open_handle: Option<Arc<dyn DirNodeOps>>,
    pub offset: Mutex<u64>,
}

impl Directory {
    pub fn new(inner: Location) -> Self {
        Self {
            inner,
            open_handle: None,
            offset: Mutex::new(0),
        }
    }

    pub fn from_opened(inner: axfs::OpenedDirectory) -> Self {
        let (inner, open_handle) = inner.into_parts();
        Self {
            inner,
            open_handle,
            offset: Mutex::new(0),
        }
    }

    /// Get the inner node of the directory.
    pub fn inner(&self) -> &Location {
        &self.inner
    }
}

impl FileHandle<Directory> {
    /// Enumerates directory data through this exact open file description.
    ///
    /// Path walking may legitimately use an `O_PATH` directory handle, while
    /// `getdents64` may not. Keeping enumeration on the handle preserves that
    /// distinction without putting Linux open flags into the generic VFS
    /// `Location`.
    pub(crate) fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> AxResult<usize> {
        self.check_io_access()?;
        if let Some(handle) = &self.open_handle {
            handle.read_dir(offset, sink).map_err(AxError::from)
        } else {
            self.inner.read_dir(offset, sink)
        }
    }
}

impl FileLike for Directory {
    fn read(&self, _dst: &mut IoDst) -> AxResult<usize> {
        Err(AxError::IsADirectory)
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::BadFileDescriptor)
    }

    fn stat(&self) -> AxResult<Kstat> {
        location_to_kstat(&self.inner)
    }

    fn vfs_location(&self) -> Option<&Location> {
        Some(&self.inner)
    }

    fn cachestat_location(&self) -> Option<&Location> {
        Some(&self.inner)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        super::inode_flags::ioctl(&self.inner, context, cmd, arg).unwrap_or(Err(AxError::NotATty))
    }

    fn sync(&self, data_only: bool) -> AxResult<()> {
        // A directory sync is a metadata durability request.  Preserve the
        // caller's data-only bit for filesystems which distinguish it, but do
        // not route through a regular-file handle (directories have none).
        self.inner.entry().sync(data_only)
    }

    fn writeback_error_state(&self) -> AxResult<Arc<WritebackErrorState>> {
        self.inner.writeback_error_state()
    }

    fn syncfs_filesystem(&self) -> Option<Filesystem> {
        Some(self.inner.mountpoint().filesystem_handle())
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // Directories never block in this implementation. FileDescription
        // still records O_NONBLOCK for F_GETFL and dup-shared OFD semantics.
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, FsPath>> {
        path_for(&self.inner)
    }

    fn from_fd(fd: c_int) -> AxResult<FileHandle<Self>> {
        match get_typed_file(fd) {
            Ok(file) => Ok(file),
            Err(AxError::InvalidInput) => Err(AxError::NotADirectory),
            Err(err) => Err(err),
        }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        if let Some(handle) = &self.open_handle {
            let _ = handle.release_handle();
        }
    }
}
impl Pollable for Directory {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axfs_ng_vfs::{DirEntrySink, Mountpoint, NodePermission, NodeType};
    use linux_raw_sys::general::{O_DIRECTORY, O_PATH};

    use super::*;
    use crate::{file::FileDescription, pseudofs::tmp};

    struct RecordingDirSink {
        called: bool,
    }

    struct CountingSource {
        bytes: Vec<u8>,
        position: usize,
        reads: usize,
    }

    impl Read for CountingSource {
        fn read(&mut self, dst: &mut [u8]) -> axio::Result<usize> {
            self.reads += 1;
            let read = dst.len().min(self.remaining());
            dst[..read].copy_from_slice(&self.bytes[self.position..self.position + read]);
            self.position += read;
            Ok(read)
        }
    }

    impl IoBuf for CountingSource {
        fn remaining(&self) -> usize {
            self.bytes.len() - self.position
        }
    }

    impl DirEntrySink for RecordingDirSink {
        fn accept(
            &mut self,
            _name: &axfs_ng_vfs::FsName,
            _ino: u64,
            _node_type: NodeType,
            _offset: u64,
        ) -> bool {
            self.called = true;
            true
        }
    }

    #[test]
    fn search_only_opath_directory_rejects_getdents_and_data_callbacks() {
        let fs = tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                axfs_ng_vfs::FsName::new(b"search-only-opath"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o111),
            )
            .unwrap();
        let directory = Arc::new(Directory::new(loc));
        let description =
            FileDescription::new_with_flags(directory.clone(), O_PATH | O_DIRECTORY).unwrap();
        let handle = FileHandle {
            description,
            file: directory,
        };

        assert!(handle.is_path_only());
        // ioctl/FIONBIO and sync-family syscalls share this exact OFD gate.
        assert_eq!(handle.check_io_access(), Err(AxError::BadFileDescriptor));
        assert_eq!(handle.poll_events_for_poll(), IoEvents::INVALID);
        // select deliberately polls the inner object and applies its separate
        // O_PATH ready-set rule instead of inheriting poll's POLLNVAL result.
        assert_eq!(handle.poll(), IoEvents::READABLE | IoEvents::WRITABLE);

        let mut sink = RecordingDirSink { called: false };
        let result = handle.read_dir(0, &mut sink);
        assert_eq!(result, Err(AxError::BadFileDescriptor));
        assert!(!sink.called);

        let mut read_called = false;
        let result = handle.with_read_credentials(|| {
            read_called = true;
            Ok(())
        });
        assert_eq!(result, Err(AxError::BadFileDescriptor));
        assert!(!read_called);

        let mut write_called = false;
        let result = handle.with_write_credentials(|_status| {
            write_called = true;
            Ok(())
        });
        assert_eq!(result, Err(AxError::BadFileDescriptor));
        assert!(!write_called);
    }

    #[test]
    fn replayable_write_source_replays_and_only_copies_admitted_growth() {
        let mut source = CountingSource {
            bytes: b"abcdef".to_vec(),
            position: 0,
            reads: 0,
        };
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(&mut source, &admitted);
        let mut output = [0u8; 8];

        replay.begin_attempt();
        assert_eq!(replay.remaining(), 6);
        admitted.set(Some(4));
        assert_eq!(replay.read(&mut output), Ok(4));
        assert_eq!(&output[..4], b"abcd");
        assert_eq!(replay.source.reads, 1);

        replay.begin_attempt();
        admitted.set(Some(2));
        output.fill(0);
        assert_eq!(replay.read(&mut output), Ok(2));
        assert_eq!(&output[..2], b"ab");
        assert_eq!(replay.source.reads, 1);

        replay.begin_attempt();
        admitted.set(Some(6));
        output.fill(0);
        assert_eq!(replay.read(&mut output), Ok(4));
        assert_eq!(replay.read(&mut output[4..]), Ok(2));
        assert_eq!(&output[..6], b"abcdef");
        assert_eq!(replay.source.reads, 2);
    }

    #[test]
    fn cached_write_hook_kills_capability_before_mutation_but_empty_write_does_not() {
        super::super::executable::init().unwrap();
        let fs = tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                axfs_ng_vfs::FsName::new(b"cached-killpriv"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        loc.entry()
            .as_file()
            .unwrap()
            .write_at(b"original", 0)
            .unwrap();
        let node = loc.entry().as_file().unwrap();
        loc.set_xattr(
            b"security.capability",
            &[1, 2, 3],
            axfs_ng_vfs::XattrSetMode::Upsert,
        )
        .unwrap();
        // Layer 1 stores generic xattrs and must not interpret privilege names.
        node.write_at(b"provider-neutral", 0).unwrap();
        node.set_len(node.len().unwrap()).unwrap();
        assert!(
            crate::file::xattr_provider::read_security_capability(&loc)
                .unwrap()
                .is_some()
        );

        let namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let security = VfsSecurityContext::new(crate::task::Cred::try_root(namespace).unwrap());
        let mut options = axfs::OpenOptions::new();
        options.write(true);
        let file = File::new(options.open_loc(loc.clone()).unwrap().into_file().unwrap());
        // A zero-length operation never begins a content mutation.
        assert!(
            crate::file::xattr_provider::read_security_capability(&loc)
                .unwrap()
                .is_some()
        );

        {
            let _privilege_guard = file
                .begin_content_write_privilege_cleanup(&security)
                .unwrap();
            node.write_at(b"attacker", 0).unwrap();
        }
        assert_eq!(
            crate::file::xattr_provider::read_security_capability(&loc).unwrap(),
            None
        );

        loc.set_xattr(
            b"security.capability",
            &[1, 2, 3],
            axfs_ng_vfs::XattrSetMode::Upsert,
        )
        .unwrap();
        {
            let _privilege_guard = file
                .begin_content_write_privilege_cleanup(&security)
                .unwrap();
            file.inner().set_len(0).unwrap();
        }
        assert_eq!(node.len().unwrap(), 0);
        assert_eq!(
            crate::file::xattr_provider::read_security_capability(&loc).unwrap(),
            None
        );
    }
}
