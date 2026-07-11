use alloc::{borrow::Cow, string::String, sync::Arc, vec::Vec};
use core::{ffi::c_int, time::Duration};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::DeviceId;
use axio::prelude::*;
use axpoll::Pollable;
use downcast_rs::{DowncastSync, impl_downcast};
use linux_raw_sys::general::{
    S_IFBLK, S_IFDIR, S_IFIFO, S_IFMT, S_IFREG, S_IFSOCK, STATX_BASIC_STATS, STATX_BTIME,
    STATX_DIOALIGN, STATX_MNT_ID, stat, statx, statx_timestamp,
};

use super::{FileHandle, add_file_like, get_typed_file};

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

    fn ioctl(&self, _cmd: u32, _arg: usize) -> AxResult<usize> {
        Err(AxError::NotATty)
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
}
