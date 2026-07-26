use alloc::{borrow::Cow, vec::Vec};
use core::{
    cell::Cell,
    ffi::c_int,
    hint::likely,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FsContext, WritePlacement};
use axfs_ng_vfs::{
    DirEntrySink, Location, Metadata, NodeFlags,
    path::{MAX_NAME_LEN, Path},
};
use axio::{IoBuf, Read};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, O_APPEND, O_NONBLOCK, RLIM_INFINITY, RLIMIT_FSIZE,
};
use starry_signal::{SignalInfo, Signo};

use super::{
    FileHandle, FileLike, Kstat, OfdIoStatus, get_file_like, get_typed_file,
    permission::{DacFsContextExt, SecurityFsContextExt, VfsSecurityContext},
    privilege_metadata::{
        ContentWriteCredentialView, ContentWritePrivilegeGuard,
        begin_conservative_content_write_privilege_cleanup, begin_content_write_privilege_cleanup,
    },
    try_owned_path,
};
use crate::{
    file::{IoDst, IoSrc, memfd},
    mounts,
    readiness::block_on_poll_io,
    task::{AsThread, DacCredentialView, send_signal_to_process},
};

const PATH_MAX: usize = 4096;

pub(crate) fn validate_pathname(path: &Path) -> AxResult {
    if path.as_str().len() >= PATH_MAX
        || path
            .components()
            .any(|component| component.as_str().len() > MAX_NAME_LEN)
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
pub(crate) fn validate_symlink_target(target: &str) -> AxResult {
    if target.is_empty() {
        Err(AxError::NotFound)
    } else if target.len() >= PATH_MAX {
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
    let mut fs = FS_CONTEXT.lock();
    if dirfd == AT_FDCWD {
        f(&mut fs)
    } else {
        let dir = Directory::from_fd(dirfd)?.inner.clone();
        f(&mut fs.with_current_dir(dir)?)
    }
}

pub fn with_path_fs<R>(
    dirfd: c_int,
    path: &Path,
    f: impl FnOnce(&mut FsContext) -> AxResult<R>,
) -> AxResult<R> {
    let mut fs = FS_CONTEXT.lock();
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

pub fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> AxResult<ResolveAtResult> {
    let current = current();
    let security = VfsSecurityContext::new(current.as_thread().current_cred());
    resolve_at_with_security(dirfd, path, flags, &security)
}

/// Resolves with an explicit synthetic DAC projection. This exists for the
/// non-AT_EACCESS access(2) real-ID/permitted-capability view, which cannot be
/// rebound to the live effective actor's typed security state.
pub(crate) fn resolve_at_with_synthetic_credentials(
    dirfd: c_int,
    path: Option<&str>,
    flags: u32,
    credentials: &DacCredentialView,
) -> AxResult<ResolveAtResult> {
    match path {
        Some("") | None => {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(AxError::NotFound);
            }
            if dirfd == AT_FDCWD {
                return Ok(ResolveAtResult::File(
                    FS_CONTEXT.lock().current_dir().clone(),
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
        Some(path) => with_path_fs(dirfd, Path::new(path), |fs| {
            if flags & AT_SYMLINK_NOFOLLOW != 0 {
                fs.resolve_no_follow_dac(path, credentials)
            } else {
                fs.resolve_dac(path, credentials)
            }
            .map(ResolveAtResult::File)
        }),
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
    path: Option<&str>,
    flags: u32,
    security: &VfsSecurityContext,
) -> AxResult<ResolveAtResult> {
    match path {
        Some("") | None => {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(AxError::NotFound);
            }
            if dirfd == AT_FDCWD {
                return Ok(ResolveAtResult::File(
                    FS_CONTEXT.lock().current_dir().clone(),
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
        Some(path) => with_path_fs(dirfd, Path::new(path), |fs| {
            if flags & AT_SYMLINK_NOFOLLOW != 0 {
                fs.resolve_no_follow_security(path, security)
            } else {
                fs.resolve_security(path, security)
            }
            .map(ResolveAtResult::File)
        }),
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
        mtime: metadata.mtime,
        ctime: metadata.ctime,
    }
}

pub fn location_to_kstat(loc: &Location) -> AxResult<Kstat> {
    let mut stat = metadata_to_kstat(&loc.metadata()?);
    stat.mnt_id = loc.mountpoint().mount_id();
    let (attributes, attributes_mask) = super::inode_flags::statx_attributes(loc);
    stat.attributes = attributes;
    stat.attributes_mask = attributes_mask;
    Ok(stat)
}

/// File wrapper for `axfs::fops::File`.
pub struct File {
    inner: axfs::File,
    nonblock: AtomicBool,
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
    pub fn new(inner: axfs::File) -> Self {
        Self {
            inner,
            nonblock: AtomicBool::new(false),
        }
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
        ContentWriteSecurity::Exact(security).begin(self.inner.location())
    }

    fn is_blocking(&self) -> bool {
        self.inner.location().flags().contains(NodeFlags::BLOCKING)
    }

    /// Reads using one immutable open-file-description status snapshot.
    pub(crate) fn read_with_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        let inner = self.inner();
        if likely(self.is_blocking()) {
            inner.read(dst)
        } else {
            block_on_poll_io(self, IoEvents::READABLE, status.nonblocking(), || {
                inner.read(&mut *dst)
            })
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
        if likely(self.is_blocking()) {
            inner.read_at(dst, offset)
        } else {
            block_on_poll_io(self, IoEvents::READABLE, status.nonblocking(), || {
                inner.read_at(&mut *dst, offset)
            })
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
                        &memfd_mutation,
                        security,
                        validate_direct,
                    )?;
                    privilege_guard = guard;
                    admitted.set(Some(allowed));
                    Ok(allowed)
                },
            );
            drop(privilege_guard);
            result
        };
        if likely(self.is_blocking()) {
            write()
        } else {
            block_on_poll_io(self, IoEvents::WRITABLE, status.nonblocking(), write)
        }
    }

    fn admit_content_write(
        &self,
        offset: u64,
        requested: usize,
        file_len: u64,
        memfd_mutation: &memfd::MemfdMutationGuard,
        security: ContentWriteSecurity<'_>,
        validate_direct: &mut impl FnMut(u64, usize) -> AxResult<()>,
    ) -> AxResult<(usize, Option<ContentWritePrivilegeGuard>)> {
        if requested == 0 {
            return Ok((0, None));
        }

        let location = self.inner.location();
        super::executable::check_not_active(location)?;
        let allowed = allowed_write_len(offset, requested)?;
        validate_direct(offset, allowed)?;
        memfd_mutation.admit_write(location, file_len, offset, allowed)?;
        let privilege_guard = (allowed != 0)
            .then(|| security.begin(location))
            .transpose()?;
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
                let (allowed, guard) = self.admit_content_write(
                    offset,
                    requested,
                    offset,
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
            block_on_poll_io(self, IoEvents::WRITABLE, status.nonblocking(), write)
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
        let admitted = Cell::new(None);
        let mut replay = ReplayableWriteSource::new(src, &admitted);
        let mut write = || {
            replay.begin_attempt();
            let requested = replay.remaining();
            let (allowed, privilege_guard) = self.admit_content_write(
                offset,
                requested,
                inner.location().len()?,
                &memfd_mutation,
                ContentWriteSecurity::Exact(security),
                &mut validate_direct,
            )?;
            admitted.set(Some(allowed));
            let result = inner.write_at(&mut replay, offset);
            drop(privilege_guard);
            result
        };
        if likely(self.is_blocking()) {
            write()
        } else {
            block_on_poll_io(self, IoEvents::WRITABLE, status.nonblocking(), write)
        }
    }
}

fn path_for(loc: &Location) -> AxResult<Cow<'static, str>> {
    let path = loc.absolute_path()?;
    Ok(Cow::Owned(try_owned_path(path.as_str())?))
}

impl FileLike for File {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_with_status(
            OfdIoStatus::new(if self.nonblocking() { O_NONBLOCK } else { 0 }),
            dst,
        )
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
        self.write_with_status_and_direct_validation_inner(
            OfdIoStatus::new(raw_status),
            src,
            ContentWriteSecurity::Conservative,
            &mut validate,
        )
    }

    fn stat(&self) -> AxResult<Kstat> {
        location_to_kstat(self.inner().location())
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        if let Some(result) = super::inode_flags::ioctl(self.inner().location(), cmd, arg) {
            return result;
        }
        self.inner().backend()?.location().ioctl(cmd, arg)
    }

    fn set_nonblocking(&self, flag: bool) -> AxResult {
        self.nonblock.store(flag, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Acquire)
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
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
        self.inner().location().poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        self.inner().location().register(context, events)
    }
}

/// Directory wrapper for `axfs::fops::Directory`.
pub struct Directory {
    inner: Location,
    pub offset: Mutex<u64>,
}

impl Directory {
    pub fn new(inner: Location) -> Self {
        Self {
            inner,
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
        self.inner.read_dir(offset, sink)
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

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        super::inode_flags::ioctl(&self.inner, cmd, arg).unwrap_or(Err(AxError::NotATty))
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // Directories never block in this implementation. FileDescription
        // still records O_NONBLOCK for F_GETFL and dup-shared OFD semantics.
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
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
        fn accept(&mut self, _name: &str, _ino: u64, _node_type: NodeType, _offset: u64) -> bool {
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
                "search-only-opath",
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
                "cached-killpriv",
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
            file.inner().backend().unwrap().set_len(0).unwrap();
        }
        assert_eq!(node.len().unwrap(), 0);
        assert_eq!(
            crate::file::xattr_provider::read_security_capability(&loc).unwrap(),
            None
        );
    }
}
