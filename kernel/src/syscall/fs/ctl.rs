use alloc::{
    ffi::CString,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    cmp::min,
    ffi::{c_char, c_int, c_void},
    mem::offset_of,
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axfs::FS_CONTEXT;
use axfs_ng_vfs::{
    Location, MetadataUpdate, NodePermission, NodeType,
    path::{MAX_NAME_LEN, Path},
};
use axhal::power::system_off;
use axtask::current;
use linux_raw_sys::{
    general::*,
    ioctl::{FIONBIO, TIOCGWINSZ},
};
use starry_vm::{VmPtr, vm_write_slice};

use crate::{
    file::{
        Directory, FileLike, get_file_like, has_tmpfile_state,
        inotify::location_for_fd,
        permission::{
            check_create_permissions, check_parent_search_permissions, check_remove_permissions,
            check_rename_permissions, check_search_permissions,
        },
        resolve_at, with_fs, with_path_fs,
    },
    mm::vm_load_string,
    task::AsThread,
    time::{TimeValueLike, wall_time},
};

const SUPPORTED_RENAMEAT2_FLAGS: u32 = RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT;
const PATH_MAX: usize = 4096;
const SUPPORTED_FCHMODAT_FLAGS: u32 = AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;
const SUPPORTED_UNLINKAT_FLAGS: u32 = AT_REMOVEDIR;

pub(super) fn validate_pathname(path: &Path) -> AxResult {
    if path.as_str().len() >= PATH_MAX
        || path
            .components()
            .any(|comp| comp.as_str().len() > MAX_NAME_LEN)
    {
        return Err(AxError::NameTooLong);
    }
    Ok(())
}

fn resolve_existing_at(dirfd: i32, path: &Path) -> AxResult<Option<Location>> {
    with_path_fs(dirfd, path, |fs| match fs.resolve_no_follow(path) {
        Ok(loc) => Ok(Some(loc)),
        Err(AxError::NotFound) => Ok(None),
        Err(err) => Err(err),
    })
}

fn proc_self_fd_location(path: &str) -> Option<AxResult<Location>> {
    let fd = path.strip_prefix("/proc/self/fd/")?;
    if fd.is_empty() || fd.as_bytes().iter().any(|byte| !byte.is_ascii_digit()) {
        return Some(Err(AxError::NotFound));
    }

    Some(
        fd.parse::<i32>()
            .ok()
            .and_then(location_for_fd)
            .ok_or(AxError::BadFileDescriptor),
    )
}

fn materialize_tmpfile_link(old: &Location, new_dir: &Location, new_name: &str) -> AxResult<()> {
    if !Arc::ptr_eq(old.mountpoint(), new_dir.mountpoint()) {
        return Err(AxError::CrossesDevices);
    }

    let metadata = old.metadata()?;
    let new = new_dir.create(new_name, NodeType::RegularFile, metadata.mode)?;
    let result = (|| {
        new.update_metadata(MetadataUpdate {
            owner: Some((metadata.uid, metadata.gid)),
            mode: Some(metadata.mode),
            atime: Some(metadata.atime),
            mtime: Some(metadata.mtime),
        })?;

        let old_file = old.entry().as_file()?;
        let new_file = new.entry().as_file()?;
        let mut offset = 0;
        let mut buf = vec![0u8; 4096];

        while offset < metadata.size {
            let len = min(buf.len(), (metadata.size - offset) as usize);
            let read = old_file.read_at(&mut buf[..len], offset)?;
            if read == 0 {
                break;
            }

            let mut written = 0;
            while written < read {
                let count = new_file.write_at(&buf[written..read], offset + written as u64)?;
                if count == 0 {
                    return Err(AxError::WriteZero);
                }
                written += count;
            }

            offset += read as u64;
        }

        Ok(())
    })();

    if result.is_err() {
        let _ = new_dir.unlink(new_name, false);
    }
    result
}

fn same_entry_at(
    old_dirfd: i32,
    old_path: &Path,
    new_dirfd: i32,
    new_path: &Path,
) -> AxResult<bool> {
    let Some(old) = resolve_existing_at(old_dirfd, old_path)? else {
        return Ok(false);
    };
    let Some(new) = resolve_existing_at(new_dirfd, new_path)? else {
        return Ok(false);
    };
    Ok(old.inode() == new.inode() && Arc::ptr_eq(old.mountpoint(), new.mountpoint()))
}

fn path_from_root(mut loc: Location, root: &Location) -> AxResult<String> {
    let mut components: Vec<String> = Vec::new();
    loop {
        if loc.ptr_eq(root) {
            if components.is_empty() {
                return Ok("/".to_string());
            }

            let mut path = String::from("/");
            for (index, component) in components.iter().rev().enumerate() {
                if index > 0 {
                    path.push('/');
                }
                path.push_str(component.as_str());
            }
            return Ok(path);
        }

        let name = loc.name().to_string();
        let Some(parent) = loc.parent() else {
            return Err(AxError::NotFound);
        };
        components.push(name);
        loc = parent;
    }
}

/// The ioctl() system call manipulates the underlying device parameters
/// of special files.
pub fn sys_ioctl(fd: i32, cmd: u32, arg: usize) -> AxResult<isize> {
    debug!("sys_ioctl <= fd: {fd}, cmd: {cmd}, arg: {arg}");
    let f = get_file_like(fd)?;
    if cmd == FIONBIO {
        let val = (arg as *const u8).vm_read()?;
        if val != 0 && val != 1 {
            return Err(AxError::InvalidInput);
        }
        f.set_nonblocking(val != 0)?;
        return Ok(0);
    }
    f.ioctl(cmd, arg)
        .map(|result| result as isize)
        .inspect_err(|err| {
            if *err == AxError::NotATty {
                // glibc likes to call TIOCGWINSZ on non-terminal files, just
                // ignore it
                if cmd == TIOCGWINSZ {
                    return;
                }
                warn!("Unsupported ioctl command: {cmd} for fd: {fd}");
            }
        })
}

pub fn sys_chdir(path: *const c_char) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    debug!("sys_chdir <= path: {path}");

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    let mut fs = FS_CONTEXT.lock();
    let entry = fs.resolve(path)?;
    if entry.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions(
        &entry,
        proc_data.fsuid(),
        proc_data.fsgid(),
        &supplementary_groups,
    )?;
    fs.set_current_dir(entry)?;
    Ok(0)
}

pub fn sys_fchdir(dirfd: i32) -> AxResult<isize> {
    debug!("sys_fchdir <= dirfd: {dirfd}");

    let entry = with_fs(dirfd, |fs| Ok(fs.current_dir().clone()))?;
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    if entry.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions(
        &entry,
        proc_data.fsuid(),
        proc_data.fsgid(),
        &supplementary_groups,
    )?;
    FS_CONTEXT.lock().set_current_dir(entry)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_mkdir(path: *const c_char, mode: u32) -> AxResult<isize> {
    sys_mkdirat(AT_FDCWD, path, mode)
}

pub fn sys_chroot(path: *const c_char) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    debug!("sys_chroot <= path: {path}");

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    let mut fs = FS_CONTEXT.lock();
    let loc = fs.resolve(path)?;
    if loc.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions(
        &loc,
        proc_data.fsuid(),
        proc_data.fsgid(),
        &supplementary_groups,
    )?;
    if proc_data.euid() != 0 {
        return Err(AxError::OperationNotPermitted);
    }
    fs.set_root_dir(loc)?;
    Ok(0)
}

pub fn sys_mkdirat(dirfd: i32, path: *const c_char, mode: u32) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    debug!("sys_mkdirat <= dirfd: {dirfd}, path: {path}, mode: {mode}");
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(Path::new(&path))?;

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let requested_mode = NodePermission::from_bits_truncate((mode & !proc_data.umask()) as u16);
    let supplementary_groups = proc_data.supplementary_groups();
    let path_ref = Path::new(&path);
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_nonexistent(path_ref)?;
        check_create_permissions(
            &parent,
            proc_data.fsuid(),
            proc_data.fsgid(),
            &supplementary_groups,
        )?;
        Ok((parent, name.to_string()))
    })?;
    let parent_meta = parent.metadata()?;
    let loc = parent.create(&name, NodeType::Directory, requested_mode)?;
    let mut final_mode = requested_mode;
    let mut owner_gid = proc_data.fsgid();
    if parent_meta.mode.contains(NodePermission::SET_GID) {
        owner_gid = parent_meta.gid;
        final_mode.insert(NodePermission::SET_GID);
    }
    loc.update_metadata(MetadataUpdate {
        owner: Some((proc_data.fsuid(), owner_gid)),
        mode: Some(final_mode),
        ..Default::default()
    })?;
    let _ = crate::file::inotify::notify_parent_with_name(&parent, loc.name(), IN_CREATE, true, 0);
    Ok(0)
}

pub fn sys_mknodat(dirfd: i32, path: *const c_char, mode: u32, _dev: u64) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    let path_ref = Path::new(&path);
    validate_pathname(path_ref)?;
    debug!("sys_mknodat <= dirfd: {dirfd}, path: {path}, mode: {mode:#o}, dev: {_dev}");

    let node_type = match mode & S_IFMT {
        S_IFREG => NodeType::RegularFile,
        S_IFIFO => NodeType::Fifo,
        S_IFCHR => NodeType::CharacterDevice,
        S_IFBLK => NodeType::BlockDevice,
        S_IFSOCK => NodeType::Socket,
        _ => return Err(AxError::InvalidInput),
    };

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice)
        && proc_data.euid() != 0
    {
        return Err(AxError::OperationNotPermitted);
    }

    let requested_mode = NodePermission::from_bits_truncate((mode & !proc_data.umask()) as u16);
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_nonexistent(path_ref)?;
        check_create_permissions(
            &parent,
            proc_data.fsuid(),
            proc_data.fsgid(),
            &proc_data.supplementary_groups(),
        )?;
        Ok((parent, name.to_string()))
    })?;

    let loc = parent.create(&name, node_type, requested_mode)?;
    let mut final_mode = requested_mode;
    let mut owner_gid = proc_data.fsgid();
    let parent_meta = parent.metadata()?;
    if parent_meta.mode.contains(NodePermission::SET_GID) {
        owner_gid = parent_meta.gid;
    }
    if proc_data.fsuid() != 0 && !proc_data.is_in_fs_group(owner_gid) {
        final_mode.remove(NodePermission::SET_GID);
    }
    loc.update_metadata(MetadataUpdate {
        owner: Some((proc_data.fsuid(), owner_gid)),
        mode: Some(final_mode),
        ..Default::default()
    })?;
    let _ = crate::file::inotify::notify_parent_with_name(&parent, &name, IN_CREATE, false, 0);
    Ok(0)
}

// Directory buffer for getdents64 syscall
struct DirBuffer {
    buf: Vec<u8>,
    offset: usize,
}

impl DirBuffer {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0; len],
            offset: 0,
        }
    }

    fn remaining_space(&self) -> usize {
        self.buf.len().saturating_sub(self.offset)
    }

    fn write_entry(&mut self, d_ino: u64, d_off: i64, d_type: NodeType, name: &[u8]) -> bool {
        const NAME_OFFSET: usize = offset_of!(linux_dirent64, d_name);

        let len = NAME_OFFSET + name.len() + 1;
        // alignment
        let len = len.next_multiple_of(align_of::<linux_dirent64>());
        if self.remaining_space() < len {
            return false;
        }

        // FIXME: safety
        unsafe {
            let entry_ptr = self.buf.as_mut_ptr().add(self.offset);
            entry_ptr.cast::<linux_dirent64>().write(linux_dirent64 {
                d_ino,
                d_off,
                d_reclen: len as _,
                d_type: d_type as _,
                d_name: Default::default(),
            });

            let name_ptr = entry_ptr.add(NAME_OFFSET);
            name_ptr.copy_from_nonoverlapping(name.as_ptr(), name.len());
            name_ptr.add(name.len()).write(0);
        }

        self.offset += len;
        true
    }
}

pub fn sys_getdents64(fd: i32, buf: *mut u8, len: usize) -> AxResult<isize> {
    debug!("sys_getdents64 <= fd: {fd}, buf: {buf:?}, len: {len}");

    let dir = Directory::from_fd(fd)?;
    if dir.inner().metadata()?.nlink == 0 {
        return Err(AxError::NotFound);
    }

    let mut buffer = DirBuffer::new(len);
    let mut dir_offset = dir.offset.lock();

    let mut has_remaining = false;

    dir.inner()
        .read_dir(*dir_offset, &mut |name: &str, ino, node_type, offset| {
            has_remaining = true;
            if !buffer.write_entry(ino, offset as _, node_type, name.as_bytes()) {
                return false;
            }
            *dir_offset = offset;
            true
        })?;

    if has_remaining && buffer.offset == 0 {
        return Err(AxError::InvalidInput);
    }

    vm_write_slice(buf, &buffer.buf)?;

    Ok(buffer.offset as _)
}

/// create a link from new_path to old_path
/// old_path: old file path
/// new_path: new file path
/// flags: link flags
/// return value: return 0 when success, else return -1.
pub fn sys_linkat(
    old_dirfd: c_int,
    old_path: *const c_char,
    new_dirfd: c_int,
    new_path: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let old_path = old_path.nullable().map(vm_load_string).transpose()?;
    let new_path = vm_load_string(new_path)?;
    debug!(
        "sys_linkat <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_FOLLOW) != 0 {
        return Err(AxError::InvalidInput);
    }
    if new_path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(Path::new(&new_path))?;

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    let old = match old_path.as_deref() {
        Some(path) if flags & AT_SYMLINK_FOLLOW != 0 => proc_self_fd_location(path)
            .unwrap_or_else(|| {
                resolve_at(old_dirfd, Some(path), flags)?
                    .into_file()
                    .ok_or(AxError::BadFileDescriptor)
            })?,
        _ => resolve_at(old_dirfd, old_path.as_deref(), flags)?
            .into_file()
            .ok_or(AxError::BadFileDescriptor)?,
    };
    if old.is_dir() {
        return Err(AxError::OperationNotPermitted);
    }
    check_search_permissions(
        &old,
        proc_data.fsuid(),
        proc_data.fsgid(),
        &supplementary_groups,
    )?;
    let (new_dir, new_name) = with_path_fs(new_dirfd, Path::new(&new_path), |fs| {
        if fs.resolve(Path::new(&new_path)).is_ok() {
            return Err(AxError::AlreadyExists);
        }
        let (new_dir, new_name) = fs.resolve_nonexistent(Path::new(&new_path))?;
        check_create_permissions(
            &new_dir,
            proc_data.fsuid(),
            proc_data.fsgid(),
            &supplementary_groups,
        )?;
        Ok((new_dir, new_name))
    })?;

    if has_tmpfile_state(&old) && old.filesystem().name() != "tmpfs" {
        materialize_tmpfile_link(&old, &new_dir, new_name)?;
    } else {
        new_dir.link(new_name, &old)?;
    }
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_link(old_path: *const c_char, new_path: *const c_char) -> AxResult<isize> {
    sys_linkat(AT_FDCWD, old_path, AT_FDCWD, new_path, 0)
}

/// remove link of specific file (can be used to delete file)
/// dir_fd: the directory of link to be removed
/// path: the name of link to be removed
/// flags: can be 0 or AT_REMOVEDIR
/// return 0 when success, else return -1
pub fn sys_unlinkat(dirfd: i32, path: *const c_char, flags: usize) -> AxResult<isize> {
    if (flags as u32) & !SUPPORTED_UNLINKAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let path = vm_load_string(path)?;
    let path_ref = Path::new(&path);
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(path_ref)?;

    debug!("sys_unlinkat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    let loc = with_path_fs(dirfd, path_ref, |fs| fs.resolve_no_follow(path_ref))?;
    let parent = loc.parent();
    let name = loc.name().to_string();
    let is_dir = loc.is_dir();
    let clear_xattrs = is_dir || loc.metadata()?.nlink <= 1;
    if let Some(parent) = parent.as_ref() {
        check_remove_permissions(
            parent,
            &loc,
            proc_data.fsuid(),
            proc_data.fsgid(),
            &supplementary_groups,
        )?;
    }
    with_path_fs(dirfd, Path::new(&path), |fs| {
        if flags == AT_REMOVEDIR as _ {
            fs.remove_dir(&path)?;
        } else {
            fs.remove_file(&path)?;
        }
        Ok(0)
    })?;
    if clear_xattrs {
        super::clear_location_xattrs(&loc);
    }
    if let Some(parent) = parent {
        let _ = crate::file::inotify::notify_parent_with_name(&parent, &name, IN_DELETE, is_dir, 0);
    }
    let _ = crate::file::inotify::notify_exact(&loc, IN_DELETE_SELF);
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_rmdir(path: *const c_char) -> AxResult<isize> {
    sys_unlinkat(AT_FDCWD, path, AT_REMOVEDIR as _)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_unlink(path: *const c_char) -> AxResult<isize> {
    sys_unlinkat(AT_FDCWD, path, 0)
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> AxResult<isize> {
    let cwd = {
        let fs = FS_CONTEXT.lock();
        path_from_root(fs.current_dir().clone(), fs.root_dir())?
    };
    debug!("sys_getcwd => cwd: {cwd}");

    let cwd = CString::new(cwd.as_str()).map_err(|_| AxError::InvalidInput)?;
    let cwd = cwd.as_bytes_with_nul();

    if cwd.len() > size {
        return Err(AxError::OutOfRange);
    }

    if buf.is_null() {
        return Err(AxError::BadAddress);
    }

    vm_write_slice(buf, cwd)?;
    Ok(cwd.len() as isize)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_symlink(target: *const c_char, linkpath: *const c_char) -> AxResult<isize> {
    sys_symlinkat(target, AT_FDCWD, linkpath)
}

pub fn sys_symlinkat(
    target: *const c_char,
    new_dirfd: i32,
    linkpath: *const c_char,
) -> AxResult<isize> {
    let target = vm_load_string(target)?;
    let linkpath = vm_load_string(linkpath)?;
    debug!("sys_symlinkat <= target: {target:?}, new_dirfd: {new_dirfd}, linkpath: {linkpath:?}");

    with_path_fs(new_dirfd, Path::new(&linkpath), |fs| {
        fs.symlink(target.as_str(), linkpath.as_str())?;
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_readlink(path: *const c_char, buf: *mut u8, size: usize) -> AxResult<isize> {
    sys_readlinkat(AT_FDCWD, path, buf, size)
}

pub fn sys_readlinkat(
    dirfd: i32,
    path: *const c_char,
    buf: *mut u8,
    size: usize,
) -> AxResult<isize> {
    let path = vm_load_string(path)?;

    debug!("sys_readlinkat <= dirfd: {dirfd}, path: {path:?}");
    if size == 0 {
        return Err(AxError::InvalidInput);
    }
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(Path::new(&path))?;

    with_path_fs(dirfd, Path::new(&path), |fs| {
        let entry = fs.resolve_no_follow(path.as_str())?;
        let curr = current();
        let proc_data = &curr.as_thread().proc_data;
        check_parent_search_permissions(
            &entry,
            proc_data.fsuid(),
            proc_data.fsgid(),
            &proc_data.supplementary_groups(),
        )?;
        let link = entry.read_link()?;
        let read = size.min(link.len());
        vm_write_slice(buf, &link.as_bytes()[..read])?;
        Ok(read as isize)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_chown(path: *const c_char, uid: i32, gid: i32) -> AxResult<isize> {
    sys_fchownat(AT_FDCWD, path, uid, gid, 0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_lchown(path: *const c_char, uid: i32, gid: i32) -> AxResult<isize> {
    use linux_raw_sys::general::AT_SYMLINK_NOFOLLOW;
    sys_fchownat(AT_FDCWD, path, uid, gid, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fchown(fd: i32, uid: i32, gid: i32) -> AxResult<isize> {
    sys_fchownat(fd, core::ptr::null(), uid, gid, AT_EMPTY_PATH)
}

pub fn sys_fchownat(
    dirfd: i32,
    path: *const c_char,
    uid: i32,
    gid: i32,
    flags: u32,
) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    let loc = resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)?;
    let meta = loc.metadata()?;

    let mut mode = meta.mode;
    if meta.node_type == NodeType::RegularFile
        && mode.intersects(
            NodePermission::OWNER_EXEC | NodePermission::GROUP_EXEC | NodePermission::OTHER_EXEC,
        )
    {
        mode.remove(NodePermission::SET_UID);
        if mode.contains(NodePermission::GROUP_EXEC) {
            mode.remove(NodePermission::SET_GID);
        }
    }

    let uid = if uid == -1 { meta.uid } else { uid as _ };
    let gid = if gid == -1 { meta.gid } else { gid as _ };
    loc.update_metadata(MetadataUpdate {
        owner: Some((uid, gid)),
        mode: Some(mode),
        ..Default::default()
    })?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_chmod(path: *const c_char, mode: u32) -> AxResult<isize> {
    sys_fchmodat(AT_FDCWD, path, mode, 0)
}

pub fn sys_fchmod(fd: i32, mode: u32) -> AxResult<isize> {
    sys_fchmodat(fd, core::ptr::null(), mode, AT_EMPTY_PATH)
}

pub fn sys_fchmodat(dirfd: i32, path: *const c_char, mode: u32, flags: u32) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    if flags & !SUPPORTED_FCHMODAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }
    let loc = resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)?;
    let meta = loc.metadata()?;
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if proc_data.fsuid() != 0 && proc_data.fsuid() != meta.uid {
        return Err(AxError::OperationNotPermitted);
    }
    let mut mode = NodePermission::from_bits_truncate(mode as u16);
    if proc_data.fsuid() != 0 && !proc_data.is_in_fs_group(meta.gid) {
        mode.remove(NodePermission::SET_GID);
    }
    loc.update_metadata(MetadataUpdate {
        mode: Some(mode),
        ..Default::default()
    })?;
    let _ = crate::file::inotify::notify_parent(&loc, IN_ATTRIB);
    let _ = crate::file::inotify::notify_exact(&loc, IN_ATTRIB);
    Ok(0)
}

fn update_times(
    dirfd: i32,
    path: *const c_char,
    atime: Option<Duration>,
    mtime: Option<Duration>,
    flags: u32,
) -> AxResult<()> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)?
        .update_metadata(MetadataUpdate {
            atime,
            mtime,
            ..Default::default()
        })?;
    Ok(())
}

#[cfg(target_arch = "x86_64")]
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct utimbuf {
    actime: linux_raw_sys::general::__kernel_old_time_t,
    modtime: linux_raw_sys::general::__kernel_old_time_t,
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utime(path: *const c_char, times: *const utimbuf) -> AxResult<isize> {
    let (atime, mtime) = if let Some(times) = times.nullable() {
        // FIXME: AnyBitPattern
        let times = unsafe { times.vm_read_uninit()?.assume_init() };
        (
            Duration::from_secs(times.actime as _),
            Duration::from_secs(times.modtime as _),
        )
    } else {
        let time = wall_time();
        (time, time)
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utimes(
    path: *const c_char,
    times: *const [linux_raw_sys::general::timeval; 2],
) -> AxResult<isize> {
    let (atime, mtime) = if let Some(times) = times.nullable() {
        // FIXME: AnyBitPattern
        let [atime, mtime] = unsafe { times.vm_read_uninit()?.assume_init() };
        (atime.try_into_time_value()?, mtime.try_into_time_value()?)
    } else {
        let time = wall_time();
        (time, time)
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0)?;
    Ok(0)
}

pub fn sys_utimensat(
    dirfd: i32,
    path: *const c_char,
    times: *const [timespec; 2],
    mut flags: u32,
) -> AxResult<isize> {
    if path.is_null() {
        flags |= AT_EMPTY_PATH;
    }
    fn utime_to_duration(time: &timespec) -> Option<AxResult<Duration>> {
        match time.tv_nsec {
            val if val == UTIME_OMIT as _ => None,
            val if val == UTIME_NOW as _ => Some(Ok(wall_time())),
            _ => Some(time.try_into_time_value()),
        }
    }

    let (atime, mtime) = if let Some(times) = times.nullable() {
        // FIXME: AnyBitPattern
        let [atime, mtime] = unsafe { times.vm_read_uninit()?.assume_init() };
        (
            utime_to_duration(&atime).transpose()?,
            utime_to_duration(&mtime).transpose()?,
        )
    } else {
        let time = wall_time();
        (Some(time), Some(time))
    };
    if atime.is_none() && mtime.is_none() {
        return Ok(0);
    }

    update_times(dirfd, path, atime, mtime, flags)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_rename(old_path: *const c_char, new_path: *const c_char) -> AxResult<isize> {
    sys_renameat(AT_FDCWD, old_path, AT_FDCWD, new_path)
}

#[cfg(not(target_arch = "riscv64"))]
pub fn sys_renameat(
    old_dirfd: i32,
    old_path: *const c_char,
    new_dirfd: i32,
    new_path: *const c_char,
) -> AxResult<isize> {
    sys_renameat2(old_dirfd, old_path, new_dirfd, new_path, 0)
}

pub fn sys_renameat2(
    old_dirfd: i32,
    old_path: *const c_char,
    new_dirfd: i32,
    new_path: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let old_path = vm_load_string(old_path)?;
    let new_path = vm_load_string(new_path)?;
    let old_path_ref = Path::new(&old_path);
    let new_path_ref = Path::new(&new_path);
    debug!(
        "sys_renameat2 <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    if flags & !SUPPORTED_RENAMEAT2_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & RENAME_EXCHANGE != 0 && flags & RENAME_NOREPLACE != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & RENAME_EXCHANGE != 0 {
        return Err(AxError::Unsupported);
    }
    if flags & RENAME_WHITEOUT != 0 {
        return Err(AxError::Unsupported);
    }
    if old_path.is_empty() || new_path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(old_path_ref)?;
    validate_pathname(new_path_ref)?;

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    let old_loc = with_path_fs(old_dirfd, old_path_ref, |fs| {
        fs.resolve_no_follow(&old_path)
    })?;
    let old_is_dir = old_loc.is_dir();
    let old_is_root = with_path_fs(old_dirfd, old_path_ref, |fs| {
        Ok(fs.path_refers_to_root(old_path_ref))
    })?;
    let new_is_root = with_path_fs(new_dirfd, new_path_ref, |fs| {
        Ok(fs.path_refers_to_root(new_path_ref))
    })?;

    if old_is_root {
        if new_is_root {
            return Err(AxError::ResourceBusy);
        }
        with_path_fs(new_dirfd, new_path_ref, |fs| {
            fs.resolve_parent(new_path_ref)?;
            Err(AxError::ResourceBusy)
        })?;
    }

    if new_is_root {
        with_path_fs(old_dirfd, old_path_ref, |fs| {
            fs.resolve_no_follow(old_path_ref)?;
            Ok(())
        })?;
        return Err(AxError::ResourceBusy);
    }

    if same_entry_at(old_dirfd, old_path_ref, new_dirfd, new_path_ref)? {
        return Ok(0);
    }

    let (old_dir, old_name) = with_path_fs(old_dirfd, old_path_ref, |fs| {
        fs.resolve_parent(old_path_ref)
    })?;
    let (new_dir, new_name) = with_path_fs(new_dirfd, new_path_ref, |fs| {
        fs.resolve_parent(new_path_ref)
    })?;
    let new_existing = resolve_existing_at(new_dirfd, new_path_ref)?;

    if flags & RENAME_NOREPLACE != 0 && new_existing.is_some() {
        return Err(AxError::AlreadyExists);
    }
    if let Some(existing) = new_existing.as_ref() {
        match (old_is_dir, existing.is_dir()) {
            (true, false) => return Err(AxError::NotADirectory),
            (false, true) => return Err(AxError::IsADirectory),
            _ => {}
        }
    }

    check_rename_permissions(
        &old_dir,
        &old_loc,
        &new_dir,
        new_existing.as_ref(),
        proc_data.fsuid(),
        proc_data.fsgid(),
        &supplementary_groups,
    )?;

    old_dir.rename(&old_name, &new_dir, &new_name)?;
    let cookie = crate::file::inotify::next_rename_cookie();
    let _ = crate::file::inotify::notify_parent_with_name(
        &old_dir,
        &old_name,
        IN_MOVED_FROM,
        old_is_dir,
        cookie,
    );
    let _ = crate::file::inotify::notify_parent_with_name(
        &new_dir,
        &new_name,
        IN_MOVED_TO,
        old_is_dir,
        cookie,
    );
    let _ = crate::file::inotify::notify_exact(&old_loc, IN_MOVE_SELF);
    Ok(0)
}

pub fn sys_sync() -> AxResult<isize> {
    FS_CONTEXT.lock().root_dir().filesystem().flush()?;
    Ok(0)
}

pub fn sys_syncfs(fd: i32) -> AxResult<isize> {
    let file = get_file_like(fd)?;
    if let Some(file) = file.downcast_ref::<crate::file::File>() {
        file.inner().location().filesystem().flush()?;
        return Ok(0);
    }
    if let Some(dir) = file.downcast_ref::<Directory>() {
        dir.inner().filesystem().flush()?;
        return Ok(0);
    }
    Err(AxError::InvalidInput)
}

pub fn sys_reboot(magic1: i32, magic2: i32, cmd: i32, _arg: *const c_void) -> AxResult<isize> {
    if magic1 as u32 != LINUX_REBOOT_MAGIC1 {
        return Err(AxError::InvalidInput);
    }
    match magic2 as u32 {
        LINUX_REBOOT_MAGIC2 | LINUX_REBOOT_MAGIC2A | LINUX_REBOOT_MAGIC2B
        | LINUX_REBOOT_MAGIC2C => {}
        _ => return Err(AxError::InvalidInput),
    }

    match cmd as u32 {
        LINUX_REBOOT_CMD_RESTART
        | LINUX_REBOOT_CMD_HALT
        | LINUX_REBOOT_CMD_POWER_OFF
        | LINUX_REBOOT_CMD_RESTART2 => {
            sys_sync()?;
            system_off();
        }
        _ => Err(AxError::Unsupported),
    }
}
