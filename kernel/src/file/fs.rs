use alloc::{borrow::Cow, vec};
use core::{
    ffi::c_int,
    hint::likely,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FsContext};
use axfs_ng_vfs::{
    Location, Metadata, NodeFlags,
    path::{MAX_NAME_LEN, Path},
};
use axio::{Cursor, IoBuf, Seek, SeekFrom};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, RLIM_INFINITY, RLIMIT_FSIZE,
};
use starry_signal::{SignalInfo, Signo};

use super::{
    FileHandle, FileLike, Kstat, get_file_description, get_file_like, get_typed_file,
    permission::DacFsContextExt, try_owned_path,
};
use crate::{
    file::{IoDst, IoSrc, memfd},
    mounts,
    readiness::block_on_poll_io,
    task::{AsThread, DacCredentialView, send_signal_to_process},
};

const O_PATH_STATUS_FLAG: u32 = linux_raw_sys::general::O_PATH;
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
    let credentials = current.as_thread().fs_dac_credentials();
    resolve_at_with_credentials(dirfd, path, flags, &credentials)
}

pub fn resolve_at_with_credentials(
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

pub fn is_path_only_fd(fd: c_int) -> AxResult<bool> {
    if get_file_description(fd)?.status_flags() & O_PATH_STATUS_FLAG != 0 {
        return Ok(true);
    }

    if let Ok(file) = get_typed_file::<File>(fd) {
        return Ok(file.inner().flags().contains(axfs::FileFlags::PATH));
    }

    Ok(false)
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

    fn is_blocking(&self) -> bool {
        self.inner.location().flags().contains(NodeFlags::BLOCKING)
    }
}

fn path_for(loc: &Location) -> AxResult<Cow<'static, str>> {
    let path = loc.absolute_path()?;
    Ok(Cow::Owned(try_owned_path(path.as_str())?))
}

impl FileLike for File {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let inner = self.inner();
        if likely(self.is_blocking()) {
            inner.read(dst)
        } else {
            block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || {
                inner.read(&mut *dst)
            })
        }
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let inner = self.inner();
        let len = src.remaining();
        let mut limited = None;
        if len != 0 {
            let appending = inner.flags().contains(axfs::FileFlags::APPEND);
            let inode_append = appending
                && !inner
                    .location()
                    .flags()
                    .contains(NodeFlags::POSITIONED_APPEND);
            let offset = if inode_append {
                inner.location().len()?
            } else {
                let mut file = inner;
                file.seek(SeekFrom::Current(0))?
            };
            super::executable::check_not_active(inner.location())?;
            let allowed = allowed_write_len(offset, len)?;
            if allowed == 0 {
                return Ok(0);
            }
            memfd::check_write(inner.location(), offset, allowed)?;
            if allowed < len {
                let mut buf = vec![0u8; allowed];
                let read = src.read(&mut buf)?;
                limited = Some(buf[..read].to_vec());
            }
        }

        if let Some(buf) = limited {
            let mut cursor = Cursor::new(buf.as_slice());
            if likely(self.is_blocking()) {
                inner.write(&mut cursor)
            } else {
                block_on_poll_io(self, IoEvents::WRITABLE, self.nonblocking(), || {
                    inner.write(&mut cursor)
                })
            }
        } else if likely(self.is_blocking()) {
            inner.write(src)
        } else {
            block_on_poll_io(self, IoEvents::WRITABLE, self.nonblocking(), || {
                inner.write(&mut *src)
            })
        }
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
