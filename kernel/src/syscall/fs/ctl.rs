use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
use core::{
    ffi::{c_char, c_int, c_void},
    mem::offset_of,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FileBackend, FileFlags};
use axfs_ng_vfs::{
    DeviceId, Location, Metadata, MetadataUpdate, NodePermission, NodeType, path::Path,
};
use axhal::power::system_off;
use axtask::current;
use linux_raw_sys::{
    general::*,
    ioctl::{FIONBIO, NS_GET_PARENT, NS_GET_USERNS, TIOCGWINSZ},
};
use starry_vm::{VmPtr, vm_write_slice};

use crate::{
    file::{
        Directory, File, FileLike, add_file_like, get_file_like,
        inotify::location_for_fd,
        is_path_only_fd, namespace_mutation,
        permission::{
            DacFsContextExt, check_open_permissions, check_search_permissions, check_writable_mount,
        },
        resolve_at_with_credentials, with_fs, with_path_fs,
    },
    mm::vm_load_string,
    mounts,
    pseudofs::{
        ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
        namespace_target_from_proc_file, proc_namespace_location_from_object,
    },
    task::{AsThread, DacCredentialView, Kgid, Kuid, PidNamespace},
    time::{TimeValueLike, wall_time},
};

const SUPPORTED_RENAMEAT2_FLAGS: u32 = RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT;
const SUPPORTED_FCHMODAT_FLAGS: u32 = AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;
const SUPPORTED_FCHOWNAT_FLAGS: u32 = AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;

#[derive(Clone, Copy, Eq, PartialEq)]
enum TimeUpdate {
    Omit,
    Now,
    Explicit,
}
const SUPPORTED_UNLINKAT_FLAGS: u32 = AT_REMOVEDIR;
const GETDENTS64_MAX_BUFFER: usize = 16 * 1024;

fn try_string(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

fn warn_notification(context: &str, result: AxResult<()>) {
    if let Err(error) = result {
        warn!("{context} notification failed: {error}");
    }
}

fn add_proc_namespace_fd(
    template: &Location,
    kind: ProcNamespaceKind,
    object: ProcNamespaceObject,
) -> AxResult<isize> {
    let loc = proc_namespace_location_from_object(template, kind, object)?;
    let file = axfs::File::new(FileBackend::Direct(loc), FileFlags::READ);
    Ok(add_file_like(
        Arc::try_new(File::new(file)).map_err(|_| AxError::NoMemory)?,
        false,
    )? as isize)
}

fn visible_pid_namespace_parent(ns: &Arc<PidNamespace>) -> Option<Arc<PidNamespace>> {
    let parent = ns.parent()?;
    let active = current().as_thread().proc_data.pid_ns();
    let mut cursor = Some(parent.clone());

    while let Some(candidate) = cursor {
        if Arc::ptr_eq(&candidate, &active) {
            return Some(parent);
        }
        cursor = candidate.parent();
    }

    None
}

fn proc_namespace_ioctl(loc: &Location, cmd: u32) -> Option<AxResult<isize>> {
    let ProcNamespaceTarget::Live(kind, object) = namespace_target_from_proc_file(loc) else {
        return None;
    };

    let result = match cmd {
        NS_GET_PARENT => match (kind, object) {
            (ProcNamespaceKind::Pid, ProcNamespaceObject::Pid(ns)) => {
                visible_pid_namespace_parent(&ns)
                    .map(|parent| {
                        add_proc_namespace_fd(
                            loc,
                            ProcNamespaceKind::Pid,
                            ProcNamespaceObject::Pid(parent),
                        )
                    })
                    .unwrap_or(Err(AxError::OperationNotPermitted))
            }
            (ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren, _) => {
                Err(AxError::InvalidInput)
            }
            (ProcNamespaceKind::User | ProcNamespaceKind::Uts, _) => Err(AxError::InvalidInput),
            _ => Err(AxError::InvalidInput),
        },
        NS_GET_USERNS => match object {
            ProcNamespaceObject::User(ns) => ns
                .parent()
                .map(|parent| {
                    add_proc_namespace_fd(
                        loc,
                        ProcNamespaceKind::User,
                        ProcNamespaceObject::User(parent),
                    )
                })
                .unwrap_or(Err(AxError::OperationNotPermitted)),
            ProcNamespaceObject::Pid(_)
            | ProcNamespaceObject::Time(_)
            | ProcNamespaceObject::Uts(_) => Err(AxError::OperationNotPermitted),
        },
        _ => return None,
    };
    Some(result)
}

pub(crate) fn validate_pathname(path: &Path) -> AxResult {
    crate::file::validate_pathname(path)
}

fn resolve_existing_at(
    dirfd: i32,
    path: &Path,
    credentials: &DacCredentialView,
) -> AxResult<Option<Location>> {
    with_path_fs(dirfd, path, |fs| {
        match fs.resolve_no_follow_dac(path, credentials) {
            Ok(loc) => Ok(Some(loc)),
            Err(AxError::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
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

fn proc_self_fd_number(path: &str) -> Option<AxResult<i32>> {
    let fd = path.strip_prefix("/proc/self/fd/")?;
    if fd.is_empty() || fd.as_bytes().iter().any(|byte| !byte.is_ascii_digit()) {
        return Some(Err(AxError::NotFound));
    }

    Some(fd.parse::<i32>().map_err(|_| AxError::NotFound))
}

fn check_empty_fd_metadata_access(dirfd: i32, path: Option<&str>, flags: u32) -> AxResult<()> {
    if matches!(path, None | Some("")) && flags & AT_EMPTY_PATH != 0 && is_path_only_fd(dirfd)? {
        return Err(AxError::BadFileDescriptor);
    }
    Ok(())
}

fn check_proc_fd_metadata_access(path: Option<&str>) -> AxResult<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(fd) = proc_self_fd_number(path) {
        let fd = fd?;
        if is_path_only_fd(fd)? {
            return Err(AxError::BadFileDescriptor);
        }
    }
    Ok(())
}

fn current_has_capability(cap: u32) -> bool {
    current().as_thread().has_effective_capability(cap)
}

fn current_can_preserve_setgid(gid: u32) -> bool {
    let Some(gid) = Kgid::from_raw(gid) else {
        return false;
    };
    let curr = current();
    let cred = curr.as_thread().current_cred();
    cred.ids().fsgid == gid
        || cred.groups().contains(gid)
        || cred.has_effective_capability(CAP_FSETID)
}

fn chown_mode_after_update(meta: &Metadata) -> NodePermission {
    let mut mode = meta.mode;
    if meta.node_type == NodeType::Directory {
        return mode;
    }
    mode.remove(NodePermission::SET_UID);
    if mode.contains(NodePermission::GROUP_EXEC) || !current_can_preserve_setgid(meta.gid) {
        mode.remove(NodePermission::SET_GID);
    }
    mode
}

fn check_chown_permission(meta: &Metadata, uid: u32, gid: u32) -> AxResult<()> {
    let curr = current();
    let cred = curr.as_thread().current_cred();
    if cred.has_effective_capability(CAP_CHOWN) {
        return Ok(());
    }
    let ids = cred.ids();
    if uid != meta.uid {
        return Err(AxError::OperationNotPermitted);
    }
    if Kuid::from_raw(meta.uid) != Some(ids.fsuid) {
        return Err(AxError::OperationNotPermitted);
    }
    let target_gid = Kgid::from_raw(gid);
    if gid != meta.gid
        && target_gid != Some(ids.fsgid)
        && !target_gid.is_some_and(|gid| cred.groups().contains(gid))
    {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

fn check_chmod_permission(meta: &Metadata) -> AxResult<()> {
    let curr = current();
    let cred = curr.as_thread().current_cred();
    if Kuid::from_raw(meta.uid) == Some(cred.ids().fsuid)
        || cred.has_effective_capability(CAP_FOWNER)
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn path_from_root(loc: Location, root: &Location) -> AxResult<String> {
    if loc.ptr_eq(root) {
        return try_string("/");
    }

    let loc_path = try_string(loc.absolute_path()?.as_str())?;
    if root.is_root() {
        return Ok(loc_path);
    }

    let root_path = try_string(root.absolute_path()?.as_str())?;
    if loc_path == root_path {
        return try_string("/");
    }

    let prefix = if root_path == "/" {
        try_string("/")?
    } else {
        let mut prefix = try_string(&root_path)?;
        prefix.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        prefix.push('/');
        prefix
    };
    let rest = loc_path.strip_prefix(&prefix).ok_or(AxError::NotFound)?;
    let mut result = String::new();
    result
        .try_reserve_exact(rest.len().checked_add(1).ok_or(AxError::NoMemory)?)
        .map_err(|_| AxError::NoMemory)?;
    result.push('/');
    result.push_str(rest);
    Ok(result)
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
    if let Some(file) = f.downcast_ref::<File>()
        && let Some(result) = proc_namespace_ioctl(file.inner().location(), cmd)
    {
        return result;
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
    let credentials = curr.as_thread().fs_dac_credentials();
    let mut fs = FS_CONTEXT.lock();
    let entry = fs.resolve_dac(path, &credentials)?;
    if entry.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions(&entry, &credentials)?;
    fs.set_current_dir(entry)?;
    Ok(0)
}

pub fn sys_fchdir(dirfd: i32) -> AxResult<isize> {
    debug!("sys_fchdir <= dirfd: {dirfd}");

    let entry = with_fs(dirfd, |fs| Ok(fs.current_dir().clone()))?;
    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    if entry.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions(&entry, &credentials)?;
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
    let credentials = curr.as_thread().fs_dac_credentials();
    let mut fs = FS_CONTEXT.lock();
    let loc = fs.resolve_dac(path, &credentials)?;
    if loc.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions(&loc, &credentials)?;
    if !current_has_capability(CAP_SYS_CHROOT) {
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
    let requested_mode = NodePermission::from_bits_truncate(mode as u16);
    let credentials = curr.as_thread().fs_dac_credentials();
    let path_ref = Path::new(&path);
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_nonexistent_dac(path_ref, &credentials)?;
        Ok((parent, try_string(name)?))
    })?;
    let loc = namespace_mutation::create_named(
        &mount_operation,
        &parent,
        &name,
        NodeType::Directory,
        requested_mode,
        proc_data.umask(),
        None,
        &credentials,
    )?;
    warn_notification(
        "mkdir create",
        crate::file::inotify::notify_parent_with_name(
            &parent,
            Some(&loc),
            loc.name(),
            IN_CREATE,
            true,
            0,
        ),
    );
    Ok(0)
}

pub fn sys_mknodat(dirfd: i32, path: *const c_char, mode: u32, dev: u64) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    let path_ref = Path::new(&path);
    validate_pathname(path_ref)?;
    debug!("sys_mknodat <= dirfd: {dirfd}, path: {path}, mode: {mode:#o}, dev: {dev}");

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
        && !curr.as_thread().has_effective_capability(CAP_MKNOD)
    {
        return Err(AxError::OperationNotPermitted);
    }

    let requested_mode = NodePermission::from_bits_truncate(mode as u16);
    let credentials = curr.as_thread().fs_dac_credentials();
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_nonexistent_dac(path_ref, &credentials)?;
        Ok((parent, try_string(name)?))
    })?;

    let rdev = matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice)
        .then_some(DeviceId(dev));
    let loc = namespace_mutation::create_named(
        &mount_operation,
        &parent,
        &name,
        node_type,
        requested_mode,
        proc_data.umask(),
        rdev,
        &credentials,
    )?;
    warn_notification(
        "mknod create",
        crate::file::inotify::notify_parent_with_name(
            &parent,
            Some(&loc),
            &name,
            IN_CREATE,
            false,
            0,
        ),
    );
    Ok(0)
}

// Directory buffer for getdents64 syscall
struct DirBuffer {
    buf: Vec<u8>,
    offset: usize,
}

impl DirBuffer {
    fn try_new(len: usize) -> AxResult<Self> {
        let len = len.min(GETDENTS64_MAX_BUFFER);
        let mut buf = Vec::new();
        buf.try_reserve_exact(len).map_err(|_| AxError::NoMemory)?;
        buf.resize(len, 0);
        Ok(Self { buf, offset: 0 })
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

    let mut buffer = DirBuffer::try_new(len)?;
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
    if buffer.offset > 0 && mounts::should_update_atime(dir.inner()) {
        dir.inner().update_supported_metadata(MetadataUpdate {
            atime: Some(wall_time()),
            ..Default::default()
        })?;
    }

    vm_write_slice(buf, &buffer.buf[..buffer.offset])?;

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
    let old_path = vm_load_string(old_path)?;
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
    let credentials = curr.as_thread().fs_dac_credentials();
    if old_path.is_empty()
        && (flags & AT_EMPTY_PATH == 0 || !credentials.has_capability(CAP_DAC_READ_SEARCH))
    {
        return Err(AxError::NotFound);
    }
    if !old_path.is_empty() {
        validate_pathname(Path::new(&old_path))?;
    }

    let mount_operation = mounts::namespace_operation();
    let old = match old_path.as_str() {
        path if flags & AT_SYMLINK_FOLLOW != 0 => {
            proc_self_fd_location(path).unwrap_or_else(|| {
                resolve_at_with_credentials(old_dirfd, Some(path), flags, &credentials)?
                    .into_file()
                    .ok_or(AxError::BadFileDescriptor)
            })?
        }
        path if !path.is_empty() => with_path_fs(old_dirfd, Path::new(path), |fs| {
            fs.resolve_no_follow_dac(path, &credentials)
        })?,
        _ => resolve_at_with_credentials(old_dirfd, Some(&old_path), flags, &credentials)?
            .into_file()
            .ok_or(AxError::BadFileDescriptor)?,
    };
    let (new_dir, new_name) = with_path_fs(new_dirfd, Path::new(&new_path), |fs| {
        if fs.resolve_dac(Path::new(&new_path), &credentials).is_ok() {
            return Err(AxError::AlreadyExists);
        }
        let (new_dir, new_name) = fs.resolve_nonexistent_dac(Path::new(&new_path), &credentials)?;
        Ok((new_dir, new_name))
    })?;

    if old.is_dir() {
        return Err(AxError::OperationNotPermitted);
    }
    let linked =
        namespace_mutation::link(&mount_operation, &new_dir, new_name, &old, &credentials)?;
    warn_notification(
        "link create",
        crate::file::inotify::notify_parent_with_name(
            &new_dir,
            Some(&linked),
            new_name,
            IN_CREATE,
            linked.is_dir(),
            0,
        ),
    );
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
    let mount_operation = mounts::namespace_operation();

    debug!("sys_unlinkat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    let (parent_hint, name_hint) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_nonexistent_dac(path_ref, &credentials)?;
        check_writable_mount(&parent)?;
        Ok((parent, try_string(name)?))
    })?;
    let loc = parent_hint.lookup_no_follow(&name_hint)?;
    let parent = parent_hint;
    let name = name_hint;
    let outcome = namespace_mutation::unlink(
        &mount_operation,
        &parent,
        &name,
        &loc,
        flags == AT_REMOVEDIR as _,
        &credentials,
    )?;
    let is_dir = outcome.is_dir;
    if !is_dir && outcome.loses_last_link {
        axfs::mark_cached_file_unlinked(&loc);
    }
    warn_notification(
        "unlink parent",
        crate::file::inotify::notify_parent_with_name(
            &parent,
            Some(&loc),
            &name,
            IN_DELETE,
            is_dir,
            0,
        ),
    );
    if !is_dir && outcome.loses_last_link {
        warn_notification(
            "unlink attribute",
            crate::file::inotify::notify_exact(&loc, IN_ATTRIB),
        );
    }
    warn_notification(
        "unlink self",
        crate::file::inotify::notify_exact(&loc, IN_DELETE_SELF),
    );
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

    if linkpath.is_empty() {
        return Err(AxError::NotFound);
    }
    let linkpath_ref = Path::new(&linkpath);
    validate_pathname(linkpath_ref)?;

    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(new_dirfd, linkpath_ref, |fs| {
        let (parent, name) = fs.resolve_nonexistent_dac(linkpath_ref, &credentials)?;
        Ok((parent, try_string(name)?))
    })?;
    let loc = namespace_mutation::create_symlink(
        &mount_operation,
        &parent,
        &name,
        &target,
        &credentials,
    )?;
    warn_notification(
        "symlink create",
        crate::file::inotify::notify_parent_with_name(
            &parent,
            Some(&loc),
            &name,
            IN_CREATE,
            false,
            0,
        ),
    );
    Ok(0)
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
    fn write_readlink_result(loc: &Location, buf: *mut u8, size: usize) -> AxResult<isize> {
        let link = loc.read_link()?;
        let read = size.min(link.len());
        vm_write_slice(buf, &link.as_bytes()[..read])?;
        Ok(read as isize)
    }

    let path = vm_load_string(path)?;

    debug!("sys_readlinkat <= dirfd: {dirfd}, path: {path:?}");
    if size == 0 {
        return Err(AxError::InvalidInput);
    }
    if path.is_empty() {
        if dirfd == AT_FDCWD {
            return Err(AxError::NotFound);
        }
        let loc = location_for_fd(dirfd).ok_or(AxError::BadFileDescriptor)?;
        if loc.node_type() != NodeType::Symlink {
            return Err(AxError::NotFound);
        }
        return write_readlink_result(&loc, buf, size);
    }
    validate_pathname(Path::new(&path))?;

    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();

    with_path_fs(dirfd, Path::new(&path), |fs| {
        let entry = fs.resolve_no_follow_dac(path.as_str(), &credentials)?;
        write_readlink_result(&entry, buf, size)
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
    if flags & !SUPPORTED_FCHOWNAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }
    check_empty_fd_metadata_access(dirfd, path.as_deref(), flags)?;
    check_proc_fd_metadata_access(path.as_deref())?;
    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    let loc = resolve_at_with_credentials(dirfd, path.as_deref(), flags, &credentials)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)?;
    let meta = loc.metadata()?;

    let uid = if uid == -1 { meta.uid } else { uid as _ };
    let gid = if gid == -1 { meta.gid } else { gid as _ };
    check_writable_mount(&loc)?;
    check_chown_permission(&meta, uid, gid)?;
    let mode = chown_mode_after_update(&meta);
    loc.update_metadata(MetadataUpdate {
        owner: Some((uid, gid)),
        mode: Some(mode),
        ..Default::default()
    })?;
    warn_notification(
        "chown parent",
        crate::file::inotify::notify_parent(&loc, IN_ATTRIB),
    );
    warn_notification(
        "chown self",
        crate::file::inotify::notify_exact(&loc, IN_ATTRIB),
    );
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
    check_empty_fd_metadata_access(dirfd, path.as_deref(), flags)?;
    check_proc_fd_metadata_access(path.as_deref())?;
    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    let loc = resolve_at_with_credentials(dirfd, path.as_deref(), flags, &credentials)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)?;
    let meta = loc.metadata()?;
    check_writable_mount(&loc)?;
    let mut mode = NodePermission::from_bits_truncate(mode as u16);
    check_chmod_permission(&meta)?;
    if !current_can_preserve_setgid(meta.gid) {
        mode.remove(NodePermission::SET_GID);
    }
    loc.update_metadata(MetadataUpdate {
        mode: Some(mode),
        ..Default::default()
    })?;
    warn_notification(
        "chmod parent",
        crate::file::inotify::notify_parent(&loc, IN_ATTRIB),
    );
    warn_notification(
        "chmod self",
        crate::file::inotify::notify_exact(&loc, IN_ATTRIB),
    );
    Ok(0)
}

fn update_times(
    dirfd: i32,
    path: *const c_char,
    atime: Option<Duration>,
    mtime: Option<Duration>,
    atime_intent: TimeUpdate,
    mtime_intent: TimeUpdate,
    flags: u32,
) -> AxResult<()> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    let loc = resolve_at_with_credentials(dirfd, path.as_deref(), flags, &credentials)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)?;
    if atime_intent == TimeUpdate::Omit && mtime_intent == TimeUpdate::Omit {
        return Ok(());
    }

    let meta = loc.metadata()?;
    if Kuid::from_raw(meta.uid) != Some(credentials.uid())
        && !credentials.has_capability(CAP_FOWNER)
    {
        if (atime_intent, mtime_intent) != (TimeUpdate::Now, TimeUpdate::Now) {
            return Err(AxError::OperationNotPermitted);
        }
        check_open_permissions(&loc, W_OK, &credentials)?;
    }
    check_writable_mount(&loc)?;
    loc.update_metadata(MetadataUpdate {
        atime,
        mtime,
        ..Default::default()
    })?;
    loc.update_supported_metadata(MetadataUpdate {
        ctime: Some(wall_time()),
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
    let intent = if times.is_null() {
        TimeUpdate::Now
    } else {
        TimeUpdate::Explicit
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), intent, intent, 0)?;
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
    let intent = if times.is_null() {
        TimeUpdate::Now
    } else {
        TimeUpdate::Explicit
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), intent, intent, 0)?;
    Ok(0)
}

pub fn sys_utimensat(
    dirfd: i32,
    path: *const c_char,
    times: *const [timespec; 2],
    mut flags: u32,
) -> AxResult<isize> {
    if path.is_null() {
        if flags != 0 {
            return Err(AxError::InvalidInput);
        }
        flags |= AT_EMPTY_PATH;
    }
    fn utime_to_duration(time: &timespec) -> (Option<AxResult<Duration>>, TimeUpdate) {
        match time.tv_nsec {
            val if val == UTIME_OMIT as _ => (None, TimeUpdate::Omit),
            val if val == UTIME_NOW as _ => (Some(Ok(wall_time())), TimeUpdate::Now),
            _ => (Some(time.try_into_time_value()), TimeUpdate::Explicit),
        }
    }

    let (atime, mtime, atime_intent, mtime_intent) = if let Some(times) = times.nullable() {
        // FIXME: AnyBitPattern
        let [atime, mtime] = unsafe { times.vm_read_uninit()?.assume_init() };
        let (atime, atime_intent) = utime_to_duration(&atime);
        let (mtime, mtime_intent) = utime_to_duration(&mtime);
        (
            atime.transpose()?,
            mtime.transpose()?,
            atime_intent,
            mtime_intent,
        )
    } else {
        let time = wall_time();
        (Some(time), Some(time), TimeUpdate::Now, TimeUpdate::Now)
    };
    update_times(dirfd, path, atime, mtime, atime_intent, mtime_intent, flags)?;
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
    if flags & RENAME_EXCHANGE != 0 && flags & (RENAME_NOREPLACE | RENAME_WHITEOUT) != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & (RENAME_EXCHANGE | RENAME_WHITEOUT) != 0 {
        return Err(AxError::OperationNotSupported);
    }
    if old_path.is_empty() || new_path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(old_path_ref)?;
    validate_pathname(new_path_ref)?;
    let mount_operation = mounts::namespace_operation();

    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    let (old_loc, old_is_root) = with_path_fs(old_dirfd, old_path_ref, |fs| {
        let loc = fs.resolve_no_follow_dac(&old_path, &credentials)?;
        let is_root = loc.ptr_eq(fs.root_dir());
        Ok((loc, is_root))
    })?;
    let old_is_dir = old_loc.is_dir();
    let new_is_root = with_path_fs(new_dirfd, new_path_ref, |fs| {
        match fs.resolve_no_follow_dac(new_path_ref, &credentials) {
            Ok(loc) => Ok(loc.ptr_eq(fs.root_dir())),
            Err(AxError::NotFound) => Ok(false),
            Err(err) => Err(err),
        }
    })?;

    if old_is_root {
        if new_is_root {
            return Err(AxError::ResourceBusy);
        }
        with_path_fs(new_dirfd, new_path_ref, |fs| {
            fs.resolve_parent_dac(new_path_ref, &credentials)?;
            Err(AxError::ResourceBusy)
        })?;
    }

    if new_is_root {
        return Err(AxError::ResourceBusy);
    }

    let (old_dir, old_name) = with_path_fs(old_dirfd, old_path_ref, |fs| {
        fs.resolve_parent_dac(old_path_ref, &credentials)
    })?;
    let (new_dir, new_name) = with_path_fs(new_dirfd, new_path_ref, |fs| {
        fs.resolve_parent_dac(new_path_ref, &credentials)
    })?;
    let new_existing = resolve_existing_at(new_dirfd, new_path_ref, &credentials)?;

    let outcome = namespace_mutation::rename(
        &mount_operation,
        &old_dir,
        &old_name,
        &old_loc,
        &new_dir,
        &new_name,
        new_existing.as_ref(),
        flags & RENAME_NOREPLACE != 0,
        &credentials,
    )?;
    if outcome.replaced_loses_last_link
        && let Some(replaced) = outcome.replaced.as_ref()
    {
        axfs::mark_cached_file_unlinked(replaced);
    }
    let cookie = crate::file::inotify::next_rename_cookie();
    warn_notification(
        "rename source",
        crate::file::inotify::notify_parent_with_name(
            &old_dir,
            Some(&old_loc),
            &old_name,
            IN_MOVED_FROM,
            old_is_dir,
            cookie,
        ),
    );
    warn_notification(
        "rename destination",
        crate::file::inotify::notify_parent_with_name(
            &new_dir,
            Some(&old_loc),
            &new_name,
            IN_MOVED_TO,
            old_is_dir,
            cookie,
        ),
    );
    warn_notification(
        "rename self",
        crate::file::inotify::notify_exact(&old_loc, IN_MOVE_SELF),
    );
    warn_notification(
        "rename dnotify",
        crate::file::inotify::notify_dnotify_rename(&old_dir, &new_dir),
    );
    Ok(0)
}

pub fn sys_sync() -> AxResult<isize> {
    FS_CONTEXT
        .lock()
        .root_dir()
        .mountpoint()
        .flush_all_filesystems()?;
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
    if !current_has_capability(CAP_SYS_BOOT) {
        return Err(AxError::OperationNotPermitted);
    }
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
            ax_println!("System is shutting down");
            system_off();
        }
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => Err(LinuxError::EOPNOTSUPP.into()),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_vhangup() -> AxResult<isize> {
    if !current_has_capability(CAP_SYS_TTY_CONFIG) {
        return Err(AxError::OperationNotPermitted);
    }
    Err(LinuxError::EOPNOTSUPP.into())
}
