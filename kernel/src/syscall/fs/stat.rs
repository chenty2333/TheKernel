use alloc::string::String;
use core::{
    ffi::{c_char, c_int},
    mem::{align_of, offset_of, size_of},
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{DeviceId, Location, NodePermission, NodeType, path::Path};
use axtask::current;
use linux_raw_sys::general::{
    __kernel_fsid_t, AT_EACCESS, AT_EMPTY_PATH, AT_FDCWD, AT_NO_AUTOMOUNT, AT_STATX_SYNC_TYPE,
    AT_SYMLINK_NOFOLLOW, R_OK, S_IFBLK, S_IFMT, S_IFREG, STATX__RESERVED, STATX_ALL, STATX_ATIME,
    STATX_BASIC_STATS, STATX_BLOCKS, STATX_BTIME, STATX_CTIME, STATX_DIOALIGN, STATX_GID,
    STATX_INO, STATX_MNT_ID, STATX_MNT_ID_UNIQUE, STATX_MODE, STATX_MTIME, STATX_NLINK, STATX_SIZE,
    STATX_SUBVOL, STATX_TYPE, STATX_UID, STATX_WRITE_ATOMIC, W_OK, X_OK, stat, statfs, statx,
    statx_timestamp,
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load_until_nul};

use super::ctl::validate_pathname;
use crate::{
    file::{
        AfAlgSocket, Directory, File, FileLike, IoUring, NamedPipe, NetlinkSocket, PacketSocket, Pipe, Socket,
        UserfaultFile, get_file_like, PidFd,
        epoll::Epoll, event::EventFd, fanotify::FanotifyFile, inotify::InotifyFile,
        permission::{
            SecurityFsContextExt, VfsSecurityContext, check_dac_permissions,
            check_dac_permissions_with_security, check_inode_permissions_with_security,
        },
        signalfd::Signalfd, timerfd::TimerFd,
        resolve_at, resolve_at_with_security, resolve_at_with_synthetic_credentials, with_path_fs,
    },
    mm::map_usercopy_error,
    mounts,
    task::AsThread,
    syscall::{MqFd, fs::mount::{FsMountFd, FsOpenFd}},
};
#[cfg(feature = "bpf")]
use crate::file::bpf::{BpfMapFd, BpfProgFd};

const SUPPORTED_FACCESSAT_FLAGS: u32 = AT_EACCESS | AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;
const SUPPORTED_FSTATAT_FLAGS: u32 = AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;
const SUPPORTED_STATX_FLAGS: u32 =
    AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE;
const VALID_STATX_MASK: u32 = STATX_TYPE
    | STATX_MODE
    | STATX_NLINK
    | STATX_UID
    | STATX_GID
    | STATX_ATIME
    | STATX_MTIME
    | STATX_CTIME
    | STATX_INO
    | STATX_SIZE
    | STATX_BLOCKS
    | STATX_BTIME
    | STATX_MNT_ID
    | STATX_DIOALIGN
    | STATX_MNT_ID_UNIQUE
    | STATX_SUBVOL
    | STATX_WRITE_ATOMIC
    | STATX_ALL;
const STATX_CHANGE_COOKIE: u32 = 0x4000_0000;
// Match Linux's regular-file O_DIRECT floor: logical sector alignment, not
// the filesystem's preferred st_blksize.
const REGULAR_FILE_DIO_ALIGNMENT: u32 = 512;
const PIPEFS_MAGIC: i64 = 0x5049_5045;
const SOCKFS_MAGIC: i64 = 0x534f_434b;
const ANON_INODE_FS_MAGIC: i64 = 0x0904_1934;
const MQUEUE_MAGIC: i64 = 0x1980_0202;

/// Native x86_64 `struct ustat`. Although obsolete, Linux still copies the
/// complete 32-byte object, including its ABI padding and obsolete name fields.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ustat {
    f_tfree: i32,
    _padding: u32,
    f_tinode: u64,
    f_fname: [u8; 6],
    f_fpack: [u8; 6],
    _tail_padding: [u8; 4],
}

// `linux_raw_sys` exposes these UAPI records without bytemuck's `NoUninit`
// marker.  The x86_64 Linux layouts contain ABI padding/tail storage that is
// part of the object copied by stat-family syscalls, so keep the unchecked
// copyout below tied to the generated layouts instead of dropping the check.
const _: () = {
    assert!(align_of::<stat>() == 8);
    assert!(size_of::<stat>() == 144);
    assert!(align_of::<statx>() == 8);
    assert!(size_of::<statx>() == 256);
    assert!(align_of::<statfs>() == 8);
    assert!(size_of::<statfs>() == 120);
    assert!(align_of::<Ustat>() == 8);
    assert!(size_of::<Ustat>() == 32);
    assert!(offset_of!(Ustat, f_tfree) == 0);
    assert!(offset_of!(Ustat, f_tinode) == 8);
    assert!(offset_of!(Ustat, f_fname) == 16);
    assert!(offset_of!(Ustat, f_fpack) == 22);
};

fn load_user_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<String> {
    String::from_utf8(vm_load_until_nul(memory, path.cast::<u8>()).map_err(map_usercopy_error)?)
        .map_err(|_| AxError::IllegalBytes)
}

fn write_stat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    statbuf: *mut stat,
    value: stat,
) -> AxResult<()> {
    // SAFETY: `stat` is an integer-only x86_64 UAPI record.  The layout
    // assertions above cover its complete initialized object representation,
    // including ABI padding and tail bytes.
    unsafe { VmMutPtr::vm_write_unchecked(statbuf, memory, value) }.map_err(map_usercopy_error)
}

fn write_statx<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    statxbuf: *mut statx,
    value: statx,
) -> AxResult<()> {
    // SAFETY: `statx` is a fully initialized integer-only x86_64 UAPI record;
    // its complete object extent is checked above before this raw copyout.
    unsafe { VmMutPtr::vm_write_unchecked(statxbuf, memory, value) }.map_err(map_usercopy_error)
}

fn write_statfs<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut statfs,
    value: statfs,
) -> AxResult<()> {
    // SAFETY: `statfs` is integer-only on the supported x86_64 ABI and its
    // complete object size/alignment are asserted above.
    unsafe { VmMutPtr::vm_write_unchecked(buf, memory, value) }.map_err(map_usercopy_error)
}

fn write_ustat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut Ustat,
    value: Ustat,
) -> AxResult<()> {
    // SAFETY: `Ustat` is integer-only and initialized from a zeroed complete
    // object representation before its counters are filled in.
    unsafe { VmMutPtr::vm_write_unchecked(buf, memory, value) }.map_err(|_| AxError::BadAddress)
}

#[inline]
fn decode_ustat_device(raw: u64) -> DeviceId {
    let raw = raw as u32;
    let major = (raw & 0x0fff00) >> 8;
    let minor = (raw & 0x0000ff) | ((raw >> 12) & 0x0fff00);
    DeviceId::new(major, minor)
}

fn ustat_from_counts(blocks_free: u64, free_file_count: u64) -> Ustat {
    // SAFETY: every field, explicit padding, and tail padding is zeroed before
    // the two Linux-defined counters are populated.
    let mut result: Ustat = unsafe { core::mem::zeroed() };
    result.f_tfree = blocks_free as i32;
    result.f_tinode = free_file_count;
    result
}

fn node_type_from_mode(mode: u32) -> NodeType {
    NodeType::from(((mode & S_IFMT) >> 12) as u8)
}

#[cfg(test)]
fn uses_empty_path_fd(path: Option<&str>, flags: u32) -> bool {
    flags & AT_EMPTY_PATH != 0 && path.is_none_or(str::is_empty)
}

fn readonly_access_check_applies(node_type: NodeType) -> bool {
    !matches!(
        node_type,
        NodeType::CharacterDevice | NodeType::BlockDevice | NodeType::Fifo | NodeType::Socket
    )
}

fn statx_timestamp_from_duration(time: axfs_ng_vfs::Timestamp) -> statx_timestamp {
    statx_timestamp {
        tv_sec: time.seconds() as _,
        tv_nsec: time.subsec_nanos() as _,
        __reserved: 0,
    }
}

fn statx_from_kstat(value: crate::file::Kstat, request_mask: u32) -> statx {
    let mut result: statx = unsafe { core::mem::zeroed() };
    result.stx_mask = STATX_BASIC_STATS | STATX_BTIME;
    result.stx_blksize = value.blksize as _;
    result.stx_attributes = value.attributes;
    result.stx_attributes_mask = value.attributes_mask;
    result.stx_nlink = value.nlink as _;
    result.stx_uid = value.uid as _;
    result.stx_gid = value.gid as _;
    result.stx_mode = value.mode as _;
    result.stx_ino = value.ino as _;
    result.stx_size = value.size as _;
    result.stx_blocks = value.blocks as _;
    result.stx_atime = statx_timestamp_from_duration(value.atime);
    result.stx_btime = statx_timestamp_from_duration(value.btime);
    result.stx_ctime = statx_timestamp_from_duration(value.ctime);
    result.stx_mtime = statx_timestamp_from_duration(value.mtime);
    result.stx_rdev_major = value.rdev.major();
    result.stx_rdev_minor = value.rdev.minor();
    let dev = DeviceId(value.dev);
    result.stx_dev_major = dev.major();
    result.stx_dev_minor = dev.minor();
    if value.mnt_id != 0 {
        result.stx_mask |= if request_mask & STATX_MNT_ID_UNIQUE != 0 {
            STATX_MNT_ID_UNIQUE
        } else {
            STATX_MNT_ID
        };
        result.stx_mnt_id = if request_mask & STATX_MNT_ID_UNIQUE != 0 {
            value.mnt_id
        } else {
            mounts::statx_mount_id(value.mnt_id).unwrap_or(value.mnt_id as u32) as u64
        };
    }

    let request_mask = request_mask & !STATX_CHANGE_COOKIE;
    let file_type = value.mode & S_IFMT;
    if request_mask & STATX_DIOALIGN != 0 && file_type == S_IFBLK {
        result.stx_mask |= STATX_DIOALIGN;
        result.stx_dio_mem_align = 1;
        result.stx_dio_offset_align = value.blksize.max(512);
    } else if request_mask & STATX_DIOALIGN != 0 && file_type == S_IFREG {
        result.stx_mask |= STATX_DIOALIGN;
        result.stx_dio_mem_align = REGULAR_FILE_DIO_ALIGNMENT;
        result.stx_dio_offset_align = REGULAR_FILE_DIO_ALIGNMENT;
    }

    result
}

/// Get the file metadata by `path` and write into `statbuf`.
///
/// Return 0 if success.
#[cfg(target_arch = "x86_64")]
pub fn sys_stat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    statbuf: *mut stat,
) -> AxResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_fstatat(memory, AT_FDCWD, path, statbuf, 0)
}

/// Get file metadata by `fd` and write into `statbuf`.
///
/// Return 0 if success.
pub fn sys_fstat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    statbuf: *mut stat,
) -> AxResult<isize> {
    sys_fstatat(memory, fd, core::ptr::null(), statbuf, AT_EMPTY_PATH)
}

/// Get the metadata of the symbolic link and write into `buf`.
///
/// Return 0 if success.
#[cfg(target_arch = "x86_64")]
pub fn sys_lstat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    statbuf: *mut stat,
) -> AxResult<isize> {
    use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

    sys_fstatat(memory, AT_FDCWD, path, statbuf, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fstatat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    statbuf: *mut stat,
    flags: u32,
) -> AxResult<isize> {
    if flags & !SUPPORTED_FSTATAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if path.is_null() && flags & AT_EMPTY_PATH == 0 {
        return Err(AxError::BadAddress);
    }
    let path = path
        .nullable()
        .map(|path| load_user_path(memory, path))
        .transpose()?;
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }

    debug!("sys_fstatat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let loc = resolve_at(dirfd, path.as_deref(), flags)?;
    write_stat(memory, statbuf, loc.stat()?.into())?;

    Ok(0)
}

pub fn sys_statx<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: c_int,
    path: *const c_char,
    flags: u32,
    mask: u32,
    statxbuf: *mut statx,
) -> AxResult<isize> {
    // `statx()` uses pathname, dirfd, and flags to identify the target
    // file in one of the following ways:

    // An absolute pathname(situation 1)
    //        If pathname begins with a slash, then it is an absolute
    //        pathname that identifies the target file.  In this case,
    //        dirfd is ignored.

    // A relative pathname(situation 2)
    //        If pathname is a string that begins with a character other
    //        than a slash and dirfd is AT_FDCWD, then pathname is a
    //        relative pathname that is interpreted relative to the
    //        process's current working directory.

    // A directory-relative pathname(situation 3)
    //        If pathname is a string that begins with a character other
    //        than a slash and dirfd is a file descriptor that refers to
    //        a directory, then pathname is a relative pathname that is
    //        interpreted relative to the directory referred to by dirfd.
    //        (See openat(2) for an explanation of why this is useful.)

    // By file descriptor(situation 4)
    //        If pathname is an empty string (or NULL since Linux 6.11)
    //        and the AT_EMPTY_PATH flag is specified in flags (see
    //        below), then the target file is the one referred to by the
    //        file descriptor dirfd.

    let path = path
        .nullable()
        .map(|path| load_user_path(memory, path))
        .transpose()?;
    debug!("sys_statx <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");
    if flags & !SUPPORTED_STATX_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE {
        return Err(AxError::InvalidInput);
    }
    if mask & STATX__RESERVED != 0 {
        return Err(AxError::InvalidInput);
    }
    if mask & !(VALID_STATX_MASK | STATX__RESERVED | STATX_CHANGE_COOKIE) != 0 {
        return Err(AxError::InvalidInput);
    }
    if path.is_none() && flags & AT_EMPTY_PATH == 0 {
        return Err(AxError::BadAddress);
    }
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }

    let loc = resolve_at(dirfd, path.as_deref(), flags)?;
    write_statx(memory, statxbuf, statx_from_kstat(loc.stat()?, mask))?;

    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_access<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    mode: u32,
) -> AxResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_faccessat2(memory, AT_FDCWD, path, mode, 0)
}

pub fn sys_faccessat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: c_int,
    path: *const c_char,
    mode: u32,
) -> AxResult<isize> {
    sys_faccessat2(memory, dirfd, path, mode, 0)
}

fn check_readonly_write_access(loc: &Location) -> AxResult {
    if crate::mounts::is_readonly(loc)? {
        Err(AxError::ReadOnlyFilesystem)
    } else {
        Ok(())
    }
}

pub fn sys_faccessat2<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: c_int,
    path: *const c_char,
    mode: u32,
    flags: u32,
) -> AxResult<isize> {
    let path = path
        .nullable()
        .map(|path| load_user_path(memory, path))
        .transpose()?;
    debug!("sys_faccessat2 <= dirfd: {dirfd}, path: {path:?}, mode: {mode}, flags: {flags}");

    if mode & !(R_OK | W_OK | X_OK) != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & !SUPPORTED_FACCESSAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }

    let curr = current();
    let actor = curr.as_thread().current_cred();
    let effective = flags & AT_EACCESS != 0;
    let security = effective.then(|| VfsSecurityContext::new(actor.clone()));
    let synthetic = (!effective).then(|| actor.real_id_access_dac_credentials());
    let file = if let Some(security) = security.as_ref() {
        resolve_at_with_security(dirfd, path.as_deref(), flags, security)?
    } else {
        let credentials = synthetic.as_ref().ok_or(AxError::BadState)?;
        resolve_at_with_synthetic_credentials(dirfd, path.as_deref(), flags, credentials)?
    };
    let stat = file.stat()?;
    let perm = stat.mode & NodePermission::all().bits() as u32;
    let node_type = node_type_from_mode(stat.mode);

    let loc = match &file {
        crate::file::ResolveAtResult::File(loc) => Some(loc),
        crate::file::ResolveAtResult::Other(_) => None,
    };
    if let Some(loc) = loc
        && mode & X_OK != 0
        && node_type == NodeType::RegularFile
        && crate::mounts::is_noexec(loc)?
    {
        return Err(AxError::PermissionDenied);
    }

    if mode == 0 {
        return Ok(0);
    }

    if let Some(security) = security.as_ref() {
        if let Some(loc) = loc {
            let metadata = loc.metadata()?;
            // faccessat(2) checks discretionary access using real/effective
            // IDs; it is not an operation on the object and must not consume
            // Landlock rights.
            check_dac_permissions_with_security(
                perm, metadata.uid, metadata.gid, node_type, mode, security,
            )?;
        } else {
            check_dac_permissions_with_security(
                perm, stat.uid, stat.gid, node_type, mode, security,
            )?;
        }
    } else {
        let credentials = synthetic.as_ref().ok_or(AxError::BadState)?;
        check_dac_permissions(perm, stat.uid, stat.gid, node_type, mode, credentials)?;
    }
    if mode & W_OK != 0
        && readonly_access_check_applies(node_type)
        && let Some(loc) = loc
    {
        check_readonly_write_access(loc)?;
    }

    Ok(0)
}

fn statfs(loc: &Location) -> AxResult<statfs> {
    let stat = loc.filesystem().stat()?;
    // FIXME: Zeroable
    let mut result: statfs = unsafe { core::mem::zeroed() };
    result.f_type = stat.fs_type as _;
    result.f_bsize = stat.block_size as _;
    result.f_blocks = stat.blocks as _;
    result.f_bfree = stat.blocks_free as _;
    result.f_bavail = stat.blocks_available as _;
    result.f_files = stat.file_count as _;
    result.f_ffree = stat.free_file_count as _;
    let device = mounts::linux_device_id(loc.mountpoint().device()).0;
    result.f_fsid = __kernel_fsid_t {
        val: [device as _, (device >> 32) as _],
    };
    result.f_namelen = stat.name_length as _;
    result.f_frsize = stat.fragment_size as _;
    result.f_flags = crate::mounts::statfs_mount_flags(loc, stat.mount_flags)? as _;
    Ok(result)
}

enum SpecialFdFilesystem {
    Pipe,
    Socket,
    AnonymousInode,
    Mqueue,
}

/// Explicit filesystem capability classification for every non-location fd
/// family the kernel exports. Unknown `FileLike` implementations deliberately
/// receive no synthetic statfs result.
fn special_fd_filesystem(fd: &dyn FileLike) -> Option<SpecialFdFilesystem> {
    if fd.downcast_ref::<Pipe>().is_some() { return Some(SpecialFdFilesystem::Pipe); }
    if fd.downcast_ref::<Socket>().is_some()
        || fd.downcast_ref::<NetlinkSocket>().is_some()
        || fd.downcast_ref::<PacketSocket>().is_some()
        || fd.downcast_ref::<AfAlgSocket>().is_some()
    { return Some(SpecialFdFilesystem::Socket); }
    if fd.downcast_ref::<MqFd>().is_some() { return Some(SpecialFdFilesystem::Mqueue); }
    #[cfg(feature = "bpf")]
    let bpf_anon_inode = fd.downcast_ref::<BpfMapFd>().is_some()
        || fd.downcast_ref::<BpfProgFd>().is_some();
    #[cfg(not(feature = "bpf"))]
    let bpf_anon_inode = false;
    if fd.downcast_ref::<EventFd>().is_some()
        || fd.downcast_ref::<Signalfd>().is_some()
        || fd.downcast_ref::<TimerFd>().is_some()
        || fd.downcast_ref::<Epoll>().is_some()
        || fd.downcast_ref::<InotifyFile>().is_some()
        || fd.downcast_ref::<FanotifyFile>().is_some()
        || fd.downcast_ref::<PidFd>().is_some()
        || fd.downcast_ref::<IoUring>().is_some()
        || fd.downcast_ref::<UserfaultFile>().is_some()
        || fd.downcast_ref::<FsOpenFd>().is_some()
        || fd.downcast_ref::<FsMountFd>().is_some()
        || bpf_anon_inode
    { return Some(SpecialFdFilesystem::AnonymousInode); }
    None
}

fn special_fd_statfs(fd: &dyn FileLike) -> Option<AxResult<statfs>> {
    let mut result: statfs = unsafe { core::mem::zeroed() };
    let kind = special_fd_filesystem(fd)?;
    result.f_type = match kind {
        SpecialFdFilesystem::Pipe => PIPEFS_MAGIC,
        SpecialFdFilesystem::Socket => SOCKFS_MAGIC,
        SpecialFdFilesystem::AnonymousInode => ANON_INODE_FS_MAGIC,
        SpecialFdFilesystem::Mqueue => MQUEUE_MAGIC,
    };
    result.f_bsize = 4096;
    result.f_namelen = 255;
    result.f_frsize = 4096;
    Some(Ok(result))
}

pub fn sys_ustat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dev: u64,
    ubuf: *mut Ustat,
) -> AxResult<isize> {
    let loc = mounts::mounted_root_location(decode_ustat_device(dev))?;
    let stat = loc.filesystem().stat()?;
    write_ustat(
        memory,
        ubuf,
        ustat_from_counts(stat.blocks_free, stat.free_file_count),
    )?;
    Ok(0)
}

pub fn sys_statfs<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    buf: *mut statfs,
) -> AxResult<isize> {
    let path = load_user_path(memory, path)?;
    debug!("sys_statfs <= path: {path:?}");
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    let path_ref = Path::new(&path);
    validate_pathname(path_ref)?;

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let loc = with_path_fs(AT_FDCWD, path_ref, |fs| {
        fs.resolve_security(path_ref, &security)
    })?;

    write_statfs(memory, buf, statfs(&loc.mountpoint().root_location())?)?;
    Ok(0)
}

pub fn sys_fstatfs<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    buf: *mut statfs,
) -> AxResult<isize> {
    debug!("sys_fstatfs <= fd: {fd}");

    let file = get_file_like(fd)?;
    if let Some(file) = file.downcast_ref::<File>() {
        write_statfs(memory, buf, statfs(file.inner().location())?)?;
    } else if let Some(dir) = file.downcast_ref::<Directory>() {
        write_statfs(memory, buf, statfs(dir.inner())?)?;
    } else if let Some(pipe) = file.downcast_ref::<NamedPipe>() {
        write_statfs(memory, buf, statfs(pipe.location())?)?;
    } else if let Some(result) = special_fd_statfs(file.as_ref()) {
        write_statfs(memory, buf, result?)?;
    } else {
        return Err(AxError::InvalidInput);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ustat_decodes_legacy_linux_device_numbers() {
        assert_eq!(decode_ustat_device(0x800).major(), 8);
        assert_eq!(decode_ustat_device(0x800).minor(), 0);
        assert_eq!(
            decode_ustat_device(0xdead_beef_0000_0800).0,
            DeviceId::new(8, 0).0
        );
    }

    #[test]
    fn ustat_has_complete_zeroed_native_layout() {
        let record = ustat_from_counts(0x1_0000_0001, u64::MAX);
        assert_eq!(record.f_tfree, 1);
        assert_eq!(record.f_tinode, u64::MAX);
        assert_eq!(record.f_fname, [0; 6]);
        assert_eq!(record.f_fpack, [0; 6]);
        // SAFETY: `Ustat` has a complete initialized representation.
        let bytes = unsafe {
            core::slice::from_raw_parts((&raw const record).cast::<u8>(), size_of::<Ustat>())
        };
        assert_eq!(&bytes[4..8], &[0; 4]);
        assert_eq!(&bytes[28..32], &[0; 4]);
    }

    #[test]
    fn ustat_unknown_device_is_rejected_before_any_user_copy() {
        assert!(matches!(
            mounts::mounted_root_location(DeviceId::new(u32::MAX, u32::MAX)),
            Err(AxError::InvalidInput)
        ));
    }

    #[test]
    fn empty_path_fd_detection_requires_the_flag_and_an_empty_path() {
        assert!(uses_empty_path_fd(None, AT_EMPTY_PATH));
        assert!(uses_empty_path_fd(Some(""), AT_EMPTY_PATH));
        assert!(!uses_empty_path_fd(Some("file"), AT_EMPTY_PATH));
        assert!(!uses_empty_path_fd(None, 0));
    }

    #[test]
    fn readonly_access_check_excludes_linux_special_files() {
        for node_type in [
            NodeType::CharacterDevice,
            NodeType::BlockDevice,
            NodeType::Fifo,
            NodeType::Socket,
        ] {
            assert!(!readonly_access_check_applies(node_type));
        }
        assert!(readonly_access_check_applies(NodeType::RegularFile));
        assert!(readonly_access_check_applies(NodeType::Directory));
        assert!(readonly_access_check_applies(NodeType::Symlink));
    }
}
