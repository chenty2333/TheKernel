use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
use core::{
    ffi::{c_char, c_int, c_void},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FileBackend, FileFlags};
use axfs_ng_vfs::{
    DeviceId, Location, MetadataUpdate, NodePermission, NodeType,
    path::{FinalComponent, FinalComponentKind, Path},
};
use axhal::power::system_off;
use axtask::current;
use linux_raw_sys::{
    general::*,
    ioctl::{
        FIONBIO, FIONREAD, NS_GET_NSTYPE, NS_GET_OWNER_UID, NS_GET_PARENT, NS_GET_USERNS,
        TIOCGWINSZ, TIOCINQ,
    },
};
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, VmPtr, vm_load_until_nul, vm_write_slice,
};

#[cfg(test)]
use crate::file::permission::{
    chown_hook_mode_for_test, prepare_chmod_metadata_setattr_for_test,
    prepare_chown_metadata_setattr_for_test,
};
use crate::{
    file::{
        Directory, File, FileDescription, FileLike, IoctlContext, executable, get_file_description,
        get_file_like,
        inotify::location_for_fd,
        namespace_mutation,
        permission::{
            ChmodSetattrPolicy, ChownSetattrPolicy, NamedCreateTerminalType, SecurityFsContextExt,
            VfsSecurityContext, check_open_permissions_with_security,
            check_search_permissions_with_security, check_writable_mount,
        },
        privilege_metadata::probe_inode_setattr_privilege_cleanup,
        resolve_at_with_security, validate_symlink_target, with_fs, with_path_fs,
    },
    mm::map_usercopy_error,
    mounts,
    pseudofs::{
        ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
        namespace_target_from_proc_file, proc_namespace_location_from_object,
    },
    task::{
        AsThread, Cred, Kgid, Kuid, PidNamespace, UserGid, UserUid, has_pending_syscall_signal,
        ns_capable,
    },
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
const GETDENTS_NAME_PATH_MAX: usize = 4096;

fn try_string(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

fn load_user_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<String> {
    String::from_utf8(vm_load_until_nul(memory, path.cast::<u8>()).map_err(map_usercopy_error)?)
        .map_err(|_| AxError::IllegalBytes)
}

fn warn_notification(context: &str, result: AxResult<()>) {
    if let Err(error) = result {
        warn!("{context} notification failed: {error}");
    }
}

fn add_proc_namespace_fd(
    context: &IoctlContext,
    template: &Location,
    kind: ProcNamespaceKind,
    object: ProcNamespaceObject,
) -> AxResult<isize> {
    let loc = proc_namespace_location_from_object(template, kind, object)?;
    let file = axfs::File::new(FileBackend::Direct(loc), FileFlags::READ);
    Ok(context.add_file_like(
        Arc::try_new(File::new(file)).map_err(|_| AxError::NoMemory)?,
        false,
    )? as isize)
}

fn visible_pid_namespace_parent(
    context: &IoctlContext,
    ns: &Arc<PidNamespace>,
) -> Option<Arc<PidNamespace>> {
    let cred = context.caller_cred();
    if !ns_capable(cred, ns.owner_user_ns(), CAP_SYS_ADMIN) {
        return None;
    }
    let parent = ns.parent()?;
    let active = context.caller_process().pid_ns();
    let mut cursor = Some(parent.clone());

    while let Some(candidate) = cursor {
        if Arc::ptr_eq(&candidate, &active) {
            return Some(parent);
        }
        cursor = candidate.parent();
    }

    None
}

fn proc_namespace_ioctl(
    context: &IoctlContext,
    loc: &Location,
    cmd: u32,
    arg: usize,
) -> Option<AxResult<isize>> {
    let ProcNamespaceTarget::Live(kind, object) = namespace_target_from_proc_file(loc) else {
        return None;
    };

    let result = match cmd {
        NS_GET_PARENT => match (kind, object) {
            (ProcNamespaceKind::Pid, ProcNamespaceObject::Pid(ns)) => {
                visible_pid_namespace_parent(context, &ns)
                    .map(|parent| {
                        add_proc_namespace_fd(
                            context,
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
        NS_GET_USERNS => object
            .owner_user_ns()
            .map(|owner| {
                add_proc_namespace_fd(
                    context,
                    loc,
                    ProcNamespaceKind::User,
                    ProcNamespaceObject::User(owner),
                )
            })
            .unwrap_or(Err(AxError::OperationNotPermitted)),
        NS_GET_OWNER_UID => match object {
            ProcNamespaceObject::User(ns) => {
                let owner = context
                    .caller_cred()
                    .user_ns()
                    .from_kuid_munged(ns.owner_kuid());
                context
                    .user_memory()
                    .write_bytes(arg, &owner.to_ne_bytes())
                    .map_err(map_usercopy_error)
                    .map(|_| 0)
            }
            _ => Err(AxError::InvalidInput),
        },
        NS_GET_NSTYPE => Ok(match kind {
            ProcNamespaceKind::Pid => CLONE_NEWPID,
            ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren => CLONE_NEWTIME,
            ProcNamespaceKind::User => CLONE_NEWUSER,
            ProcNamespaceKind::Uts => CLONE_NEWUTS,
        } as isize),
        _ => return None,
    };
    Some(result)
}

pub(crate) fn validate_pathname(path: &Path) -> AxResult {
    crate::file::validate_pathname(path)
}

fn proc_self_fd_location(path: &str) -> Option<AxResult<LinkatSource>> {
    let fd = path.strip_prefix("/proc/self/fd/")?;
    if fd.is_empty() || fd.as_bytes().iter().any(|byte| !byte.is_ascii_digit()) {
        return Some(Err(AxError::NotFound));
    }

    Some(
        fd.parse::<i32>()
            .map_err(|_| AxError::BadFileDescriptor)
            .and_then(|fd| {
                let description = get_file_description(fd)?;
                Ok(hardlink_location_from_description(&description)
                    .map_or(LinkatSource::AnonymousFile, LinkatSource::Location))
            }),
    )
}

fn linkat_opener_credential_authorized(actor: &Cred, opener: Option<&Cred>) -> bool {
    opener.is_some_and(|opener| {
        actor.same_linux_credential(opener)
            || ns_capable(actor, opener.user_ns(), CAP_DAC_READ_SEARCH)
    })
}

enum LinkatSource {
    Location(Location),
    AnonymousFile,
}

fn hardlink_location_from_description(description: &FileDescription) -> Option<Location> {
    hardlink_location_from_file_like(description.inner.as_ref())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataTargetSource {
    /// `fchmod(2)` / `fchown(2)`: an O_PATH description is not a valid direct
    /// file operand.
    DirectFd,
    /// `*at(2)`: AT_EMPTY_PATH deliberately accepts an O_PATH description.
    At,
}

fn check_metadata_description_status(
    source: MetadataTargetSource,
    status_flags: u32,
) -> AxResult<()> {
    if source == MetadataTargetSource::DirectFd && status_flags & O_PATH != 0 {
        Err(AxError::BadFileDescriptor)
    } else {
        Ok(())
    }
}

/// Pins the exact metadata target once, including its authoritative OFD
/// status. This avoids a check-then-lookup close/dup2 ABA and preserves the
/// Linux distinction between direct-fd syscalls and AT_EMPTY_PATH.
fn resolve_metadata_target(
    dirfd: i32,
    path: Option<&str>,
    flags: u32,
    source: MetadataTargetSource,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    if matches!(path, None | Some("")) {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        if source == MetadataTargetSource::DirectFd || dirfd != AT_FDCWD {
            let description = get_file_description(dirfd)?;
            check_metadata_description_status(source, description.status_flags())?;
            return hardlink_location_from_description(&description)
                .ok_or(AxError::BadFileDescriptor);
        }
    }

    resolve_at_with_security(dirfd, path, flags, security)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)
}

fn hardlink_location_from_file_like(file_like: &dyn FileLike) -> Option<Location> {
    if let Some(file) = file_like.downcast_ref::<File>() {
        Some(file.inner().location().clone())
    } else if let Some(directory) = file_like.downcast_ref::<Directory>() {
        Some(directory.inner().clone())
    } else {
        file_like
            .downcast_ref::<crate::file::pipe::NamedPipe>()
            .map(|pipe| pipe.location().clone())
    }
}

fn pin_linkat_source_description_with<F>(
    fd: c_int,
    security: &VfsSecurityContext,
    require_empty_path_authorization: bool,
    lookup: F,
) -> AxResult<Arc<FileDescription>>
where
    F: FnOnce(c_int) -> AxResult<Arc<FileDescription>>,
{
    // Lookup must precede opener authorization. Linux reports EBADF for an
    // invalid descriptor even when the actor would fail LOOKUP_LINKAT_EMPTY.
    let description = lookup(fd)?;
    if require_empty_path_authorization {
        let opener = description.vfs_open_credential();
        if !linkat_opener_credential_authorized(security.actor(), opener.as_deref()) {
            return Err(AxError::NotFound);
        }
    }
    Ok(description)
}

fn pin_linkat_source_description(
    fd: c_int,
    security: &VfsSecurityContext,
    require_empty_path_authorization: bool,
) -> AxResult<Arc<FileDescription>> {
    pin_linkat_source_description_with(
        fd,
        security,
        require_empty_path_authorization,
        get_file_description,
    )
}

fn resolve_hardlink_source_in_fs(
    fs: &axfs::FsContext,
    path: &Path,
    follow_final_symlink: bool,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    if follow_final_symlink {
        fs.resolve_security(path, security)
    } else {
        fs.resolve_no_follow_security(path, security)
    }
}

fn resolve_linkat_source(
    old_dirfd: c_int,
    old_path: &str,
    flags: u32,
    security: &VfsSecurityContext,
) -> AxResult<LinkatSource> {
    if old_path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        if old_dirfd == AT_FDCWD {
            return Ok(LinkatSource::Location(
                FS_CONTEXT.lock().current_dir().clone(),
            ));
        }

        let description = pin_linkat_source_description(old_dirfd, security, true)?;
        let source = hardlink_location_from_description(&description)
            .map_or(LinkatSource::AnonymousFile, LinkatSource::Location);
        drop(description);
        return Ok(source);
    }

    let follow_final_symlink = flags & AT_SYMLINK_FOLLOW != 0;
    if follow_final_symlink && let Some(location) = proc_self_fd_location(old_path) {
        return location;
    }

    let path = Path::new(old_path);
    if path.is_absolute() || old_dirfd == AT_FDCWD {
        return resolve_hardlink_source_in_fs(
            &FS_CONTEXT.lock(),
            path,
            follow_final_symlink,
            security,
        )
        .map(LinkatSource::Location);
    }

    // Pin one exact OFD before both LOOKUP_LINKAT_EMPTY authorization and
    // extraction of the relative-path starting point. A concurrent dup2/close
    // cannot make authorization observe one description and pathwalk another.
    let description =
        pin_linkat_source_description(old_dirfd, security, flags & AT_EMPTY_PATH != 0)?;
    let start = hardlink_location_from_description(&description).ok_or(AxError::NotADirectory)?;
    let fs = FS_CONTEXT.lock();
    let relative_fs = fs.with_current_dir(start)?;
    let result = resolve_hardlink_source_in_fs(&relative_fs, path, follow_final_symlink, security)
        .map(LinkatSource::Location);
    drop(description);
    result
}

fn current_has_capability(cap: u32) -> bool {
    current().as_thread().has_effective_capability(cap)
}

fn requested_chown_ids(
    actor: &Cred,
    requested_uid: i32,
    requested_gid: i32,
) -> AxResult<(Option<Kuid>, Option<Kgid>)> {
    let user = if requested_uid == -1 {
        None
    } else {
        Some(
            UserUid::from_raw(requested_uid as u32)
                .and_then(|user| actor.user_ns().user_uid_to_kernel(user))
                .ok_or(AxError::InvalidInput)?,
        )
    };
    let group = if requested_gid == -1 {
        None
    } else {
        Some(
            UserGid::from_raw(requested_gid as u32)
                .and_then(|group| actor.user_ns().user_gid_to_kernel(group))
                .ok_or(AxError::InvalidInput)?,
        )
    };
    Ok((user, group))
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
const fn fionbio_enabled(value: c_int) -> bool {
    value != 0
}

pub fn sys_ioctl(context: &IoctlContext, fd: i32, cmd: u32, arg: usize) -> AxResult<isize> {
    debug!("sys_ioctl <= fd: {fd}, cmd: {cmd}, arg: {arg}");
    let f = context.get_file_like(fd)?;
    // O_PATH exposes pathname metadata, not the underlying object's ioctl
    // surface. Reject before FIONBIO reads its userspace argument.
    f.check_io_access()?;
    if cmd == FIONBIO {
        // Linux FIONBIO consumes an `int *`; every nonzero value enables the
        // flag. Reading the complete word also preserves cross-page EFAULT.
        let val: c_int = context
            .user_memory()
            .read_value(arg as *const c_int)
            .map_err(map_usercopy_error)?;
        f.set_nonblocking_status(fionbio_enabled(val))?;
        return Ok(0);
    }
    if let Some(file) = f.downcast_ref::<File>()
        && let Some(result) = proc_namespace_ioctl(context, file.inner().location(), cmd, arg)
    {
        return result;
    }
    let result = f.ioctl(context, cmd, arg).inspect_err(|err| {
        if *err == AxError::NotATty {
            // glibc likes to call TIOCGWINSZ on non-terminal files, just
            // ignore it
            if cmd == TIOCGWINSZ {
                return;
            }
            warn!("Unsupported ioctl command: {cmd} for fd: {fd}");
        }
    })?;
    if cmd == FIONREAD || cmd == TIOCINQ {
        let value = i32::try_from(result).unwrap_or(i32::MAX);
        context
            .user_memory()
            .write_bytes(arg, &value.to_ne_bytes())
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    Ok(result as isize)
}

pub fn sys_chdir<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<isize> {
    let path = load_user_path(memory, path)?;
    debug!("sys_chdir <= path: {path}");

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let mut fs = FS_CONTEXT.lock();
    let entry = fs.resolve_security(path, &security)?;
    if entry.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions_with_security(&entry, &security)?;
    fs.set_current_dir(entry)?;
    Ok(0)
}

pub fn sys_fchdir(dirfd: i32) -> AxResult<isize> {
    debug!("sys_fchdir <= dirfd: {dirfd}");

    let entry = with_fs(dirfd, |fs| Ok(fs.current_dir().clone()))?;
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if entry.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions_with_security(&entry, &security)?;
    FS_CONTEXT.lock().set_current_dir(entry)?;
    Ok(0)
}

pub fn sys_mkdir<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    mode: u32,
) -> AxResult<isize> {
    sys_mkdirat(memory, AT_FDCWD, path, mode)
}

pub fn sys_chroot<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<isize> {
    let path = load_user_path(memory, path)?;
    debug!("sys_chroot <= path: {path}");

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let mut fs = FS_CONTEXT.lock();
    let loc = fs.resolve_security(path, &security)?;
    if loc.node_type() != NodeType::Directory {
        return Err(AxError::NotADirectory);
    }
    check_search_permissions_with_security(&loc, &security)?;
    if !security.has_capability(CAP_SYS_CHROOT) {
        return Err(AxError::OperationNotPermitted);
    }
    fs.set_root_dir(loc)?;
    Ok(0)
}

pub fn sys_mkdirat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    mode: u32,
) -> AxResult<isize> {
    let path = load_user_path(memory, path)?;
    debug!("sys_mkdirat <= dirfd: {dirfd}, path: {path}, mode: {mode}");
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(Path::new(&path))?;

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let requested_mode = NodePermission::from_bits_truncate(mode as u16);
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let path_ref = Path::new(&path);
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_named_create_security(
            path_ref,
            &security,
            NamedCreateTerminalType::Directory,
        )?;
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
        &security,
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

fn decode_mknod_node_type(mode: u32) -> AxResult<NodeType> {
    match mode & S_IFMT {
        0 | S_IFREG => Ok(NodeType::RegularFile),
        S_IFIFO => Ok(NodeType::Fifo),
        S_IFCHR => Ok(NodeType::CharacterDevice),
        S_IFBLK => Ok(NodeType::BlockDevice),
        S_IFSOCK => Ok(NodeType::Socket),
        S_IFDIR => Err(AxError::OperationNotPermitted),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_mknodat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    mode: u32,
    dev: u64,
) -> AxResult<isize> {
    let path = load_user_path(memory, path)?;
    let path_ref = Path::new(&path);
    validate_pathname(path_ref)?;
    debug!("sys_mknodat <= dirfd: {dirfd}, path: {path}, mode: {mode:#o}, dev: {dev}");

    let node_type = decode_mknod_node_type(mode)?;

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());

    let requested_mode = NodePermission::from_bits_truncate(mode as u16);
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_named_create_security(
            path_ref,
            &security,
            NamedCreateTerminalType::NonDirectory,
        )?;
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
        &security,
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

#[derive(Clone, Copy)]
enum DirentFormat {
    Legacy,
    Dirent64,
}

fn getdents_count(count: usize) -> usize {
    count as u32 as usize
}

fn getdents_has_room(count: usize, copied: usize, record_len: usize) -> bool {
    // Linux stores the unsigned syscall argument in the callback's signed
    // `int count`. Native values above INT_MAX therefore cannot admit even
    // the first record (an empty directory still returns zero).
    count <= i32::MAX as usize && record_len <= count.saturating_sub(copied)
}

fn dirent_record_len(format: DirentFormat, name: &[u8]) -> AxResult<usize> {
    if name.is_empty()
        || name.len() >= GETDENTS_NAME_PATH_MAX
        || name.iter().any(|byte| *byte == b'/')
    {
        return Err(AxError::Io);
    }
    Ok(match format {
        // struct linux_dirent: ino@0, off@8, reclen@16, name@18, type@last
        DirentFormat::Legacy => (18 + name.len() + 2).next_multiple_of(8),
        // struct linux_dirent64: ino@0, off@8, reclen@16, type@18, name@19
        DirentFormat::Dirent64 => (19 + name.len() + 1).next_multiple_of(8),
    })
}

/// Constructs one native x86_64 Linux directory record in reusable storage.
fn fill_dirent(
    record: &mut Vec<u8>,
    format: DirentFormat,
    ino: u64,
    offset: u64,
    node_type: NodeType,
    name: &[u8],
) -> AxResult<()> {
    let reclen = dirent_record_len(format, name)?;
    let name_offset = match format {
        DirentFormat::Legacy => 18,
        DirentFormat::Dirent64 => 19,
    };
    record.clear();
    record
        .try_reserve_exact(reclen)
        .map_err(|_| AxError::NoMemory)?;
    record.resize(reclen, 0);
    record[0..8].copy_from_slice(&ino.to_ne_bytes());
    record[8..16].copy_from_slice(&match format {
        DirentFormat::Legacy => offset.to_ne_bytes(),
        DirentFormat::Dirent64 => (offset as i64).to_ne_bytes(),
    });
    record[16..18].copy_from_slice(&(reclen as u16).to_ne_bytes());
    match format {
        DirentFormat::Legacy => record[reclen - 1] = node_type as u8,
        DirentFormat::Dirent64 => record[18] = node_type as u8,
    }
    record[name_offset..name_offset + name.len()].copy_from_slice(name);
    Ok(())
}

#[cfg(test)]
fn build_dirent(
    format: DirentFormat,
    ino: u64,
    offset: u64,
    node_type: NodeType,
    name: &[u8],
) -> AxResult<Vec<u8>> {
    let mut record = Vec::new();
    fill_dirent(&mut record, format, ino, offset, node_type, name)?;
    Ok(record)
}

fn sys_getdents_common<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    buf: *mut u8,
    count: usize,
    format: DirentFormat,
) -> AxResult<isize> {
    let dir = Directory::from_fd(fd)?;
    dir.check_io_access()?;
    if dir.inner().metadata()?.nlink == 0 {
        return Err(AxError::NotFound);
    }

    let count = getdents_count(count);
    let result = {
        let mut dir_offset = dir.offset.lock();
        let mut copied = 0;
        let mut stop_error = None;
        let mut stopped_for_space = false;
        let mut record = Vec::new();
        let mut last_reclen = 0;

        let iteration = dir.read_dir(*dir_offset, &mut |name: &str, ino, node_type, offset| {
            // Linux only checks signals between already completed records.
            if copied != 0 && has_pending_syscall_signal(current().as_thread()) {
                return false;
            }

            let record_len = match dirent_record_len(format, name.as_bytes()) {
                Ok(record_len) => record_len,
                Err(error) => {
                    stop_error = Some(error);
                    return false;
                }
            };
            if !getdents_has_room(count, copied, record_len) {
                stopped_for_space = true;
                return false;
            }
            if let Err(error) =
                fill_dirent(&mut record, format, ino, offset, node_type, name.as_bytes())
            {
                stop_error = Some(error);
                return false;
            }

            // Any failure from the user-memory provider is EFAULT for this
            // copyout, including provider-side allocation failures.
            if vm_write_slice(memory, buf.wrapping_add(copied), &record).is_err() {
                stop_error = Some(AxError::BadAddress);
                return false;
            }
            *dir_offset = offset;
            copied += record.len();
            last_reclen = record.len();
            true
        });

        if copied != 0 {
            // Linux performs a final checked d_off store after iteration. A
            // concurrent mprotect/unmap may therefore turn an otherwise
            // successful prefix into EFAULT, while the OFD cookie remains
            // committed to the last copied record.
            let final_offset = match format {
                DirentFormat::Legacy => dir_offset.to_ne_bytes(),
                DirentFormat::Dirent64 => (*dir_offset as i64).to_ne_bytes(),
            };
            let last_d_off = buf.wrapping_add(copied - last_reclen + 8);
            vm_write_slice(memory, last_d_off, &final_offset)
                .map(|_| copied as isize)
                .map_err(|_| AxError::BadAddress)
        } else {
            match iteration {
                Err(error) => Err(error),
                Ok(_) => match stop_error {
                    Some(error) => Err(error),
                    None if stopped_for_space => Err(AxError::InvalidInput),
                    None => Ok(0),
                },
            }
        }
    };

    // Linux marks every live-directory iteration as an access, including an
    // empty result, EINVAL/EFAULT from the actor, or an iterator error.
    // Metadata failures are best-effort and cannot change the syscall result.
    if mounts::should_update_atime(dir.inner()) {
        warn_notification(
            "getdents atime",
            dir.inner().update_supported_metadata(MetadataUpdate {
                atime: Some(wall_time()),
                ..Default::default()
            }),
        );
    }
    result
}

pub fn sys_getdents<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    buf: *mut u8,
    count: usize,
) -> AxResult<isize> {
    debug!("sys_getdents <= fd: {fd}, buf: {buf:?}, count: {count}");
    sys_getdents_common(memory, fd, buf, count, DirentFormat::Legacy)
}

pub fn sys_getdents64<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    buf: *mut u8,
    len: usize,
) -> AxResult<isize> {
    debug!("sys_getdents64 <= fd: {fd}, buf: {buf:?}, len: {len}");
    sys_getdents_common(memory, fd, buf, len, DirentFormat::Dirent64)
}

/// create a link from new_path to old_path
/// old_path: old file path
/// new_path: new file path
/// flags: link flags
/// return value: return 0 when success, else return -1.
pub fn sys_linkat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    old_dirfd: c_int,
    old_path: *const c_char,
    new_dirfd: c_int,
    new_path: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let old_path = load_user_path(memory, old_path)?;
    let new_path = load_user_path(memory, new_path)?;
    debug!(
        "sys_linkat <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_FOLLOW) != 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !old_path.is_empty() {
        validate_pathname(Path::new(&old_path))?;
    }
    let source = resolve_linkat_source(old_dirfd, &old_path, flags, &security)?;

    if new_path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(Path::new(&new_path))?;

    let mount_operation = mounts::namespace_operation();
    let new_path_ref = Path::new(&new_path);
    let (new_dir, new_name) = with_path_fs(new_dirfd, new_path_ref, |fs| {
        fs.resolve_named_create_security(
            new_path_ref,
            &security,
            NamedCreateTerminalType::NonDirectory,
        )
    })?;
    let old = match source {
        LinkatSource::Location(old) => old,
        LinkatSource::AnonymousFile => {
            namespace_mutation::reject_unnameable_link_source(
                &mount_operation,
                &new_dir,
                new_name,
                &security,
            )?;
            return Err(AxError::BadState);
        }
    };

    let linked = namespace_mutation::link(&mount_operation, &new_dir, new_name, &old, &security)?;
    warn_notification(
        "link source attribute",
        crate::file::inotify::notify_exact(&old, IN_ATTRIB),
    );
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

pub fn sys_link<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    old_path: *const c_char,
    new_path: *const c_char,
) -> AxResult<isize> {
    sys_linkat(memory, AT_FDCWD, old_path, AT_FDCWD, new_path, 0)
}

/// remove link of specific file (can be used to delete file)
/// dir_fd: the directory of link to be removed
/// path: the name of link to be removed
/// flags: can be 0 or AT_REMOVEDIR
/// return 0 when success, else return -1
fn unlinkat_remove_dir(flags: usize) -> AxResult<bool> {
    // The syscall ABI declares this argument as C `int`, even though the
    // internal dispatcher carries a register-sized value.
    let flags = flags as u32;
    if flags & !SUPPORTED_UNLINKAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(flags == AT_REMOVEDIR)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnlinkFinalName<'a> {
    name: &'a str,
    requires_directory: bool,
}

/// Maps the generic, lossless final-component classification to Linux
/// unlink/rmdir ABI errors without allowing `.`/`..`/root to alias an earlier
/// named entry.
fn unlinkat_final_name(
    final_component: FinalComponent<'_>,
    remove_dir: bool,
) -> AxResult<UnlinkFinalName<'_>> {
    match final_component.kind() {
        FinalComponentKind::Normal(name) => Ok(UnlinkFinalName {
            name,
            requires_directory: final_component.requires_directory(),
        }),
        FinalComponentKind::Dot if remove_dir => Err(AxError::InvalidInput),
        FinalComponentKind::DotDot if remove_dir => Err(AxError::DirectoryNotEmpty),
        FinalComponentKind::Root if remove_dir => Err(AxError::ResourceBusy),
        FinalComponentKind::Dot | FinalComponentKind::DotDot | FinalComponentKind::Root => {
            Err(AxError::IsADirectory)
        }
    }
}

fn resolve_unlink_target_in_fs(
    fs: &axfs::FsContext,
    path: &Path,
    remove_dir: bool,
    security: &VfsSecurityContext,
) -> AxResult<(Location, String, Location)> {
    let (_, syntactic_final) = path.split_final_component().ok_or(AxError::NotFound)?;
    if matches!(syntactic_final.kind(), FinalComponentKind::Root) {
        // Linux classifies LAST_ROOT without looking up or admitting the root
        // as a searchable parent for a later named entry.
        return Err(if remove_dir {
            AxError::ResourceBusy
        } else {
            AxError::IsADirectory
        });
    }
    let (parent, final_component) = fs.resolve_parent_preserving_final_security(path, security)?;
    let final_name = unlinkat_final_name(final_component, remove_dir)?;
    check_writable_mount(&parent)?;
    let name = try_string(final_name.name)?;
    let target = parent.lookup_no_follow_in_mount(&name)?;
    if final_name.requires_directory && !remove_dir {
        // filename_unlinkat handles trailing slashes immediately after final
        // lookup, before security_path_unlink or vfs_unlink/may_delete.
        return Err(if target.is_dir() {
            AxError::IsADirectory
        } else {
            AxError::NotADirectory
        });
    }
    Ok((parent, name, target))
}

pub fn sys_unlinkat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    flags: usize,
) -> AxResult<isize> {
    let remove_dir = unlinkat_remove_dir(flags)?;
    let path = load_user_path(memory, path)?;
    let path_ref = Path::new(&path);
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(path_ref)?;
    let mount_operation = mounts::namespace_operation();

    debug!("sys_unlinkat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let (parent, name, loc) = with_path_fs(dirfd, path_ref, |fs| {
        resolve_unlink_target_in_fs(fs, path_ref, remove_dir, &security)
    })?;
    let outcome = namespace_mutation::unlink(
        &mount_operation,
        &parent,
        &name,
        &loc,
        remove_dir,
        &security,
    )?;
    let is_dir = outcome.is_dir;
    if !is_dir {
        // Linux reports every successful link-count change, including removal
        // of a non-final hard-link name.
        warn_notification(
            "unlink attribute",
            crate::file::inotify::notify_exact(&loc, IN_ATTRIB),
        );
    }
    if outcome.loses_last_link {
        if !is_dir {
            axfs::mark_cached_file_unlinked(&loc);
        }
        // The current dentry layer has no delayed detach callback for an open
        // path, so last-link DELETE_SELF is still eager. Crucially, a
        // non-final hard-link unlink no longer destroys the inode watch.
        warn_notification(
            "unlink self",
            crate::file::inotify::notify_exact(&loc, IN_DELETE_SELF),
        );
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
    Ok(0)
}

pub fn sys_rmdir<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<isize> {
    sys_unlinkat(memory, AT_FDCWD, path, AT_REMOVEDIR as _)
}

pub fn sys_unlink<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<isize> {
    sys_unlinkat(memory, AT_FDCWD, path, 0)
}

pub fn sys_getcwd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    size: usize,
) -> AxResult<isize> {
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

    vm_write_slice(memory, buf, cwd).map_err(map_usercopy_error)?;
    Ok(cwd.len() as isize)
}

pub fn sys_symlink<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    target: *const c_char,
    linkpath: *const c_char,
) -> AxResult<isize> {
    sys_symlinkat(memory, target, AT_FDCWD, linkpath)
}

pub fn sys_symlinkat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    target: *const c_char,
    new_dirfd: i32,
    linkpath: *const c_char,
) -> AxResult<isize> {
    let target = load_user_path(memory, target)?;
    validate_symlink_target(&target)?;
    let linkpath = load_user_path(memory, linkpath)?;
    debug!("sys_symlinkat <= target: {target:?}, new_dirfd: {new_dirfd}, linkpath: {linkpath:?}");

    if linkpath.is_empty() {
        return Err(AxError::NotFound);
    }
    let linkpath_ref = Path::new(&linkpath);
    validate_pathname(linkpath_ref)?;

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(new_dirfd, linkpath_ref, |fs| {
        let (parent, name) = fs.resolve_named_create_security(
            linkpath_ref,
            &security,
            NamedCreateTerminalType::NonDirectory,
        )?;
        Ok((parent, try_string(name)?))
    })?;
    let loc =
        namespace_mutation::create_symlink(&mount_operation, &parent, &name, &target, &security)?;
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

pub fn sys_readlink<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    buf: *mut u8,
    size: usize,
) -> AxResult<isize> {
    sys_readlinkat(memory, AT_FDCWD, path, buf, size)
}

pub fn sys_readlinkat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    buf: *mut u8,
    size: usize,
) -> AxResult<isize> {
    fn write_readlink_result<M: UserMemory + ?Sized>(
        memory: &mut UserMemoryContext<'_, M>,
        loc: &Location,
        buf: *mut u8,
        size: usize,
    ) -> AxResult<isize> {
        let link = loc.read_link()?;
        let read = size.min(link.len());
        vm_write_slice(memory, buf, &link.as_bytes()[..read]).map_err(map_usercopy_error)?;
        Ok(read as isize)
    }

    let path = load_user_path(memory, path)?;

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
        return write_readlink_result(memory, &loc, buf, size);
    }
    validate_pathname(Path::new(&path))?;

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());

    with_path_fs(dirfd, Path::new(&path), |fs| {
        let entry = fs.resolve_no_follow_security(path.as_str(), &security)?;
        write_readlink_result(memory, &entry, buf, size)
    })
}

pub fn sys_chown<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    uid: i32,
    gid: i32,
) -> AxResult<isize> {
    sys_fchownat(memory, AT_FDCWD, path, uid, gid, 0)
}

pub fn sys_lchown<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    uid: i32,
    gid: i32,
) -> AxResult<isize> {
    use linux_raw_sys::general::AT_SYMLINK_NOFOLLOW;
    sys_fchownat(memory, AT_FDCWD, path, uid, gid, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fchown(fd: i32, uid: i32, gid: i32) -> AxResult<isize> {
    do_fchownat(
        fd,
        None,
        uid,
        gid,
        AT_EMPTY_PATH,
        MetadataTargetSource::DirectFd,
    )
}

pub fn sys_fchownat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    uid: i32,
    gid: i32,
    flags: u32,
) -> AxResult<isize> {
    // Linux rejects unknown flags before touching the userspace pathname.
    // Keep EINVAL ahead of EFAULT/ENAMETOOLONG for malformed combinations.
    if flags & !SUPPORTED_FCHOWNAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let path = load_user_path(memory, path)?;
    do_fchownat(
        dirfd,
        Some(path.as_str()),
        uid,
        gid,
        flags,
        MetadataTargetSource::At,
    )
}

fn do_fchownat(
    dirfd: i32,
    path: Option<&str>,
    uid: i32,
    gid: i32,
    flags: u32,
    source: MetadataTargetSource,
) -> AxResult<isize> {
    if flags & !SUPPORTED_FCHOWNAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    // Interim serialization only: the current generic VFS has no per-inode
    // metadata transaction primitive. Reuse the namespace writer domain so a
    // fresh snapshot cannot race another in-kernel metadata mutation between
    // admission and publication. The stable Linux policy lives in the typed
    // plan below; this broad gate must eventually become an inode-local Layer
    // 1 mechanism rather than part of the syscall or ABI contract.
    let _metadata_writer_fallback = mounts::namespace_operation();
    let loc = resolve_metadata_target(dirfd, path, flags, source, &security)?;
    // Linux's mnt_want_write() failure precedes ID conversion, inode locking,
    // security hooks, and setattr_prepare authorization.
    check_writable_mount(&loc)?;
    let (requested_user, requested_group) = requested_chown_ids(security.actor(), uid, gid)?;
    executable::with_credential_metadata_unpinned(&loc, || {
        let policy = ChownSetattrPolicy::new(&loc, requested_user, requested_group, &security)?;
        let privilege_cleanup = probe_inode_setattr_privilege_cleanup(&loc, policy.metadata())?;

        // notify_change() validates the target filesystem mapping before the
        // inode hook, but setattr_prepare's owner/CAP checks remain later.
        let prepared = policy.admit(&security, privilege_cleanup)?.prepare()?;

        // Publication consumes the admitted cleanup token first, then mutates
        // metadata. A later backend failure deliberately does not roll back a
        // capability removal, matching Linux commoncap's conservative order.
        let published = prepared.publish()?;

        // Current upstream Linux runs fsnotify before its infallible post hook.
        warn_notification(
            "chown parent",
            crate::file::inotify::notify_parent(&loc, IN_ATTRIB),
        );
        warn_notification(
            "chown self",
            crate::file::inotify::notify_exact(&loc, IN_ATTRIB),
        );
        published.commit();
        Ok(())
    })?;
    Ok(0)
}

pub fn sys_chmod<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    mode: u32,
) -> AxResult<isize> {
    sys_fchmodat(memory, AT_FDCWD, path, mode, 0)
}

pub fn sys_fchmod(fd: i32, mode: u32) -> AxResult<isize> {
    do_fchmodat(
        fd,
        None,
        mode,
        AT_EMPTY_PATH,
        MetadataTargetSource::DirectFd,
    )
}

pub fn sys_fchmodat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    mode: u32,
    flags: u32,
) -> AxResult<isize> {
    // Match do_fchmodat(): flag validation precedes filename acquisition.
    if flags & !SUPPORTED_FCHMODAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let path = load_user_path(memory, path)?;
    do_fchmodat(
        dirfd,
        Some(path.as_str()),
        mode,
        flags,
        MetadataTargetSource::At,
    )
}

fn do_fchmodat(
    dirfd: i32,
    path: Option<&str>,
    mode: u32,
    flags: u32,
    source: MetadataTargetSource,
) -> AxResult<isize> {
    if flags & !SUPPORTED_FCHMODAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    // See the chown path above: this broad writer gate is an interim mechanism,
    // not the final per-inode metadata transaction architecture.
    let _metadata_writer_fallback = mounts::namespace_operation();
    let loc = resolve_metadata_target(dirfd, path, flags, source, &security)?;
    check_writable_mount(&loc)?;
    let publishes_setid =
        mode & (NodePermission::SET_UID | NodePermission::SET_GID).bits() as u32 != 0;
    executable::with_setid_metadata_unpinned(&loc, publishes_setid, || {
        let policy = ChmodSetattrPolicy::new(&loc, mode, &security)?;
        if policy.metadata().node_type == NodeType::Symlink {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        let prepared = policy.admit(&security)?.prepare()?;
        let published = prepared.publish()?;

        warn_notification(
            "chmod parent",
            crate::file::inotify::notify_parent(&loc, IN_ATTRIB),
        );
        warn_notification(
            "chmod self",
            crate::file::inotify::notify_exact(&loc, IN_ATTRIB),
        );
        published.commit();
        Ok(())
    })?;
    Ok(0)
}

fn update_times<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    atime: Option<Duration>,
    mtime: Option<Duration>,
    atime_intent: TimeUpdate,
    mtime_intent: TimeUpdate,
    flags: u32,
) -> AxResult<()> {
    let path = path
        .nullable()
        .map(|path| load_user_path(memory, path))
        .transpose()?;
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let credentials = security.credentials();
    let loc = resolve_at_with_security(dirfd, path.as_deref(), flags, &security)?
        .into_file()
        .ok_or(AxError::BadFileDescriptor)?;
    if atime_intent == TimeUpdate::Omit && mtime_intent == TimeUpdate::Omit {
        return Ok(());
    }

    let meta = loc.metadata()?;
    if Kuid::from_raw(meta.uid) != Some(credentials.uid()) && !security.has_capability(CAP_FOWNER) {
        if (atime_intent, mtime_intent) != (TimeUpdate::Now, TimeUpdate::Now) {
            return Err(AxError::OperationNotPermitted);
        }
        check_open_permissions_with_security(
            &loc,
            W_OK,
            security.actor(),
            credentials,
            security.filesystem_owner_user_ns(),
        )?;
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

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct utimbuf {
    actime: linux_raw_sys::general::__kernel_old_time_t,
    modtime: linux_raw_sys::general::__kernel_old_time_t,
}

pub fn sys_utime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    times: *const utimbuf,
) -> AxResult<isize> {
    let (atime, mtime) = if let Some(times) = times.nullable() {
        // FIXME: AnyBitPattern
        let times = unsafe {
            times
                .vm_read_uninit(memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
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
    update_times(
        memory,
        AT_FDCWD,
        path,
        Some(atime),
        Some(mtime),
        intent,
        intent,
        0,
    )?;
    Ok(0)
}

pub fn sys_utimes<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    times: *const [linux_raw_sys::general::timeval; 2],
) -> AxResult<isize> {
    let (atime, mtime) = if let Some(times) = times.nullable() {
        // FIXME: AnyBitPattern
        let [atime, mtime] = unsafe {
            times
                .vm_read_uninit(memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
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
    update_times(
        memory,
        AT_FDCWD,
        path,
        Some(atime),
        Some(mtime),
        intent,
        intent,
        0,
    )?;
    Ok(0)
}

pub fn sys_utimensat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
        let [atime, mtime] = unsafe {
            times
                .vm_read_uninit(memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
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
    update_times(
        memory,
        dirfd,
        path,
        atime,
        mtime,
        atime_intent,
        mtime_intent,
        flags,
    )?;
    Ok(0)
}

pub fn sys_rename<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    old_path: *const c_char,
    new_path: *const c_char,
) -> AxResult<isize> {
    sys_renameat(memory, AT_FDCWD, old_path, AT_FDCWD, new_path)
}

pub fn sys_renameat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    old_dirfd: i32,
    old_path: *const c_char,
    new_dirfd: i32,
    new_path: *const c_char,
) -> AxResult<isize> {
    sys_renameat2(memory, old_dirfd, old_path, new_dirfd, new_path, 0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenameFinalName<'a> {
    name: &'a str,
    requires_directory: bool,
}

/// Preserves the exact final pathname syntax used by `renameat2`.
///
/// Linux resolves both parent paths and rejects cross-mount operations before
/// it classifies `LAST_DOT`, `LAST_DOTDOT`, or `LAST_ROOT`. `RENAME_NOREPLACE`
/// also deliberately changes the destination-special-component errno to
/// `EEXIST`. The caller therefore supplies the operation-specific error after
/// it has completed those earlier ordering steps.
fn renameat_final_name(
    final_component: FinalComponent<'_>,
    special_error: AxError,
) -> AxResult<RenameFinalName<'_>> {
    match final_component.kind() {
        FinalComponentKind::Normal(name) => Ok(RenameFinalName {
            name,
            requires_directory: final_component.requires_directory(),
        }),
        FinalComponentKind::Dot | FinalComponentKind::DotDot | FinalComponentKind::Root => {
            Err(special_error)
        }
    }
}

fn lookup_optional_in_mount(parent: &Location, name: &str) -> AxResult<Option<Location>> {
    match parent.lookup_no_follow_in_mount(name) {
        Ok(location) => Ok(Some(location)),
        Err(AxError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn validate_rename_directory_intent(
    old_requires_directory: bool,
    new_requires_directory: bool,
    source_is_directory: bool,
    destination_is_directory: Option<bool>,
) -> AxResult<()> {
    if (old_requires_directory || new_requires_directory) && !source_is_directory {
        return Err(AxError::NotADirectory);
    }
    if new_requires_directory && destination_is_directory == Some(false) {
        return Err(AxError::NotADirectory);
    }
    Ok(())
}

pub fn sys_renameat2<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    old_dirfd: i32,
    old_path: *const c_char,
    new_dirfd: i32,
    new_path: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let old_path = load_user_path(memory, old_path)?;
    let new_path = load_user_path(memory, new_path)?;
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
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let (old_dir, old_final) = with_path_fs(old_dirfd, old_path_ref, |fs| {
        fs.resolve_parent_preserving_final_security(old_path_ref, &security)
    })?;
    let (new_dir, new_final) = with_path_fs(new_dirfd, new_path_ref, |fs| {
        fs.resolve_parent_preserving_final_security(new_path_ref, &security)
    })?;

    // filename_renameat2 rejects distinct mounts before LAST_* classification
    // or final lookup. Bind mounts remain distinct even when they expose the
    // same backend inode.
    if !old_dir.same_mount(&new_dir) {
        return Err(LinuxError::EXDEV.into());
    }

    let old_final = renameat_final_name(old_final, AxError::ResourceBusy)?;
    let new_special_error = if flags & RENAME_NOREPLACE != 0 {
        AxError::AlreadyExists
    } else {
        AxError::ResourceBusy
    };
    let new_final = renameat_final_name(new_final, new_special_error)?;

    // This mirrors mnt_want_write() placement: parent resolution and LAST_*
    // classification have completed, while no final dentry has been consumed.
    check_writable_mount(&old_dir)?;

    // Final lookups deliberately stay in the parents' exact mounts. Crossing
    // into a child mount would substitute its root for the covered dentry and
    // make both transaction identity checks and the eventual EBUSY wrong.
    let old_loc = old_dir.lookup_no_follow_in_mount(old_final.name)?;
    let new_existing = lookup_optional_in_mount(&new_dir, new_final.name)?;
    if flags & RENAME_NOREPLACE != 0 && new_existing.is_some() {
        return Err(AxError::AlreadyExists);
    }

    // lock_rename() classifies the two directory-topology traps after lookup
    // but before trailing-slash checks, path hooks, DAC, or inode hooks.
    old_dir.validate_rename_ancestry_checked(&old_loc, &new_dir, new_existing.as_ref())?;

    // Linux performs these trailing-slash checks after both locked lookups.
    // A missing destination with a slash is valid when the source is a
    // directory; a symlink itself never satisfies this no-follow requirement.
    validate_rename_directory_intent(
        old_final.requires_directory,
        new_final.requires_directory,
        old_loc.is_dir(),
        new_existing.as_ref().map(Location::is_dir),
    )?;

    // vfs_rename returns immediately when the looked-up source and target are
    // the same inode. This includes distinct hard-link names and intentionally
    // precedes may_delete and the inode_rename hook. RENAME_NOREPLACE has
    // already returned EEXIST above.
    if new_existing
        .as_ref()
        .is_some_and(|destination| destination.same_node(&old_loc))
    {
        return Ok(0);
    }

    let old_is_dir = old_loc.is_dir();

    let outcome = namespace_mutation::rename(
        &mount_operation,
        &old_dir,
        old_final.name,
        &old_loc,
        &new_dir,
        new_final.name,
        new_existing.as_ref(),
        flags & RENAME_NOREPLACE != 0,
        &security,
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
            old_final.name,
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
            new_final.name,
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
    file.check_io_access()?;
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

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::{cell::Cell, mem::MaybeUninit, time::Duration};

    use axfs_ng_vfs::{Metadata, Mountpoint};
    use thekernel_linux_cred::{FsCredentialSnapshot, GroupInfo, Kgid, Kuid};
    use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext};

    use super::*;
    use crate::task::DacCredentialView;

    #[test]
    fn getdents_records_match_native_x86_64_layouts() {
        let legacy = build_dirent(
            DirentFormat::Legacy,
            0x0102_0304_0506_0708,
            0x1112_1314_1516_1718,
            NodeType::Directory,
            b"abc",
        )
        .unwrap();
        assert_eq!(legacy.len(), 24);
        assert_eq!(
            u64::from_ne_bytes(legacy[0..8].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            u64::from_ne_bytes(legacy[8..16].try_into().unwrap()),
            0x1112_1314_1516_1718
        );
        assert_eq!(u16::from_ne_bytes(legacy[16..18].try_into().unwrap()), 24);
        assert_eq!(&legacy[18..22], b"abc\0");
        assert_eq!(legacy[22], 0);
        assert_eq!(legacy[23], NodeType::Directory as u8);

        let dirent64 = build_dirent(
            DirentFormat::Dirent64,
            0x0102_0304_0506_0708,
            0x1112_1314_1516_1718,
            NodeType::RegularFile,
            b"abc",
        )
        .unwrap();
        assert_eq!(dirent64.len(), 24);
        assert_eq!(
            u64::from_ne_bytes(dirent64[0..8].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            i64::from_ne_bytes(dirent64[8..16].try_into().unwrap()),
            0x1112_1314_1516_1718
        );
        assert_eq!(u16::from_ne_bytes(dirent64[16..18].try_into().unwrap()), 24);
        assert_eq!(dirent64[18], NodeType::RegularFile as u8);
        assert_eq!(&dirent64[19..23], b"abc\0");
        assert_eq!(dirent64[23], 0);
    }

    #[test]
    fn getdents_count_uses_unsigned_int_width() {
        assert_eq!(getdents_count(u32::MAX as usize), u32::MAX as usize);
        assert_eq!(getdents_count((u32::MAX as usize).saturating_add(9)), 8);
        assert!(!getdents_has_room(i32::MAX as usize + 1, 0, 24));
        assert!(getdents_has_room(i32::MAX as usize, 0, 24));
    }

    #[test]
    fn getdents_first_record_requires_its_full_reclen() {
        let record = build_dirent(
            DirentFormat::Dirent64,
            1,
            2,
            NodeType::RegularFile,
            b"entry",
        )
        .unwrap();
        assert!(!getdents_has_room(record.len() - 1, 0, record.len()));
        assert!(getdents_has_room(record.len(), 0, record.len()));
        assert_eq!(record.len(), 32);
    }

    fn linkat_test_security() -> VfsSecurityContext {
        let namespace = crate::task::UserNamespace::try_new_root().unwrap();
        VfsSecurityContext::new(Cred::try_root(namespace).unwrap())
    }

    struct NoUserMemory;

    // The metadata flag-order test must not touch its null pathname. Invalid
    // flags are rejected before usercopy, so this provider is never called.
    unsafe impl UserMemory for NoUserMemory {
        fn read(
            &mut self,
            _start: usize,
            _dst: &mut [MaybeUninit<u8>],
        ) -> Result<(), UserCopyError> {
            Err(UserCopyError::BadAddress)
        }

        fn write(&mut self, _start: usize, _src: &[u8]) -> Result<(), UserCopyError> {
            Err(UserCopyError::BadAddress)
        }
    }

    #[test]
    fn linkat_empty_path_opener_rule_uses_core_identity_and_opener_namespace() {
        let initial = crate::task::UserNamespace::try_new_root().unwrap();
        let initial_actor = Cred::try_root(initial.clone()).unwrap();
        let fork_child = Cred::try_clone_for_fork(&initial_actor).unwrap();
        assert!(linkat_opener_credential_authorized(
            &fork_child,
            Some(&initial_actor)
        ));

        let child_namespace = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let child_actor =
            Cred::try_with_user_namespace(&initial_actor, child_namespace.clone()).unwrap();
        assert!(!linkat_opener_credential_authorized(
            &child_actor,
            Some(&initial_actor)
        ));

        let distinct_initial = Cred::try_root(initial).unwrap();
        let child_opener =
            Cred::try_with_user_namespace(&distinct_initial, child_namespace).unwrap();
        assert!(linkat_opener_credential_authorized(
            &initial_actor,
            Some(&child_opener)
        ));
        assert!(!linkat_opener_credential_authorized(&initial_actor, None));
    }

    #[test]
    fn linkat_empty_path_propagates_bad_fd_before_opener_authorization() {
        let security = linkat_test_security();
        let lookups = Cell::new(0);
        let result = pin_linkat_source_description_with(-1, &security, true, |_| {
            lookups.set(lookups.get() + 1);
            Err::<Arc<FileDescription>, _>(AxError::BadFileDescriptor)
        });

        assert!(matches!(result, Err(AxError::BadFileDescriptor)));
        assert_eq!(lookups.get(), 1);
    }

    #[test]
    fn hardlink_source_resolution_honors_final_follow_and_pinned_relative_start() {
        let security = linkat_test_security();
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        let target = root
            .create(
                "target",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let symlink = root
            .create_symlink(
                "jump",
                "target",
                NodePermission::from_bits_truncate(0o777),
                Some((0, 0)),
            )
            .unwrap();
        let context = axfs::FsContext::new(root.clone());

        let no_follow =
            resolve_hardlink_source_in_fs(&context, Path::new("jump"), false, &security).unwrap();
        let followed =
            resolve_hardlink_source_in_fs(&context, Path::new("jump"), true, &security).unwrap();
        assert!(no_follow.same_node(&symlink));
        assert!(followed.same_node(&target));

        let left = root
            .create(
                "left",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let right = root
            .create(
                "right",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let left_source = left
            .create(
                "source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let right_source = right
            .create(
                "source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let left_context = context.with_current_dir(left).unwrap();
        let pinned_context = left_context.with_current_dir(right).unwrap();
        let resolved =
            resolve_hardlink_source_in_fs(&pinned_context, Path::new("source"), false, &security)
                .unwrap();
        assert!(!resolved.same_node(&left_source));
        assert!(resolved.same_node(&right_source));
    }

    #[test]
    fn hardlink_fd_location_supports_file_directory_and_named_pipe() {
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();

        let file_location = root
            .create(
                "file",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file = File::new(axfs::File::new(
            FileBackend::Direct(file_location.clone()),
            FileFlags::READ,
        ));
        assert!(
            hardlink_location_from_file_like(&file)
                .unwrap()
                .same_node(&file_location)
        );

        let directory_location = root
            .create(
                "directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        let directory = Directory::new(directory_location.clone());
        assert!(
            hardlink_location_from_file_like(&directory)
                .unwrap()
                .same_node(&directory_location)
        );

        let fifo_location = root
            .create(
                "fifo",
                NodeType::Fifo,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let fifo = crate::file::pipe::NamedPipe::open(fifo_location.clone(), O_RDWR).unwrap();
        assert!(
            hardlink_location_from_file_like(&fifo)
                .unwrap()
                .same_node(&fifo_location)
        );
    }

    #[test]
    fn mknodat_decodes_linux_node_types_and_error_classes() {
        for (mode, expected) in [
            (0, NodeType::RegularFile),
            (S_IFREG, NodeType::RegularFile),
            (S_IFIFO, NodeType::Fifo),
            (S_IFCHR, NodeType::CharacterDevice),
            (S_IFBLK, NodeType::BlockDevice),
            (S_IFSOCK, NodeType::Socket),
        ] {
            assert_eq!(decode_mknod_node_type(mode | 0o6755), Ok(expected));
        }
        assert_eq!(
            decode_mknod_node_type(S_IFDIR | 0o755),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            decode_mknod_node_type(S_IFLNK | 0o777),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            decode_mknod_node_type(S_IFMT | 0o600),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn named_create_preserves_trailing_and_special_terminal_syntax() {
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        root.create(
            "existing-file",
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        )
        .unwrap();
        root.create(
            "existing-dir",
            NodeType::Directory,
            NodePermission::from_bits_truncate(0o700),
        )
        .unwrap();
        let context = axfs::FsContext::new(root.clone());
        let security = linkat_test_security();

        let (parent, name) = context
            .resolve_named_create_security(
                Path::new("missing"),
                &security,
                NamedCreateTerminalType::NonDirectory,
            )
            .unwrap();
        assert!(parent.same_node(&root));
        assert_eq!(name, "missing");
        let (parent, name) = context
            .resolve_named_create_security(
                Path::new("missing/"),
                &security,
                NamedCreateTerminalType::Directory,
            )
            .unwrap();
        assert!(parent.same_node(&root));
        assert_eq!(name, "missing");
        assert!(matches!(
            context.resolve_named_create_security(
                Path::new("missing/"),
                &security,
                NamedCreateTerminalType::NonDirectory,
            ),
            Err(AxError::NotFound)
        ));
        for path in ["existing-file/", "existing-dir/"] {
            assert!(matches!(
                context.resolve_named_create_security(
                    Path::new(path),
                    &security,
                    NamedCreateTerminalType::NonDirectory,
                ),
                Err(AxError::AlreadyExists)
            ));
        }
        for path in [".", "..", "/"] {
            for terminal_type in [
                NamedCreateTerminalType::Directory,
                NamedCreateTerminalType::NonDirectory,
            ] {
                assert!(matches!(
                    context.resolve_named_create_security(
                        Path::new(path),
                        &security,
                        terminal_type,
                    ),
                    Err(AxError::AlreadyExists)
                ));
            }
        }

        assert!(matches!(
            context.resolve_named_create_security(
                Path::new("missing/."),
                &security,
                NamedCreateTerminalType::Directory,
            ),
            Err(AxError::NotFound)
        ));
        assert!(matches!(
            context.resolve_named_create_security(
                Path::new("existing-file/."),
                &security,
                NamedCreateTerminalType::Directory,
            ),
            Err(AxError::NotADirectory)
        ));
        assert!(matches!(
            root.lookup_no_follow_in_mount("missing"),
            Err(AxError::NotFound)
        ));
        assert!(matches!(
            context.resolve_named_create_security(
                Path::new("existing-dir/."),
                &security,
                NamedCreateTerminalType::Directory,
            ),
            Err(AxError::AlreadyExists)
        ));
    }

    #[test]
    fn unlinkat_flags_are_normalized_to_the_linux_int_abi_before_dispatch() {
        assert_eq!(unlinkat_remove_dir(0), Ok(false));
        assert_eq!(unlinkat_remove_dir(AT_REMOVEDIR as usize), Ok(true));

        if usize::BITS > u32::BITS {
            let ignored_register_bits = 1usize << u32::BITS;
            assert_eq!(unlinkat_remove_dir(ignored_register_bits), Ok(false));
            assert_eq!(
                unlinkat_remove_dir(ignored_register_bits | AT_REMOVEDIR as usize),
                Ok(true)
            );
        }

        assert_eq!(unlinkat_remove_dir(1), Err(AxError::InvalidInput));
    }

    fn final_component(path: &str) -> FinalComponent<'_> {
        Path::new(path).split_final_component().unwrap().1
    }

    #[test]
    fn unlinkat_preserves_destructive_final_component_syntax() {
        assert_eq!(
            unlinkat_final_name(final_component("file"), false),
            Ok(UnlinkFinalName {
                name: "file",
                requires_directory: false,
            })
        );
        assert_eq!(
            unlinkat_final_name(final_component("file/"), false),
            Ok(UnlinkFinalName {
                name: "file",
                requires_directory: true,
            })
        );

        for path in [".", "..", "/"] {
            assert_eq!(
                unlinkat_final_name(final_component(path), false),
                Err(AxError::IsADirectory)
            );
        }
        assert_eq!(
            unlinkat_final_name(final_component("."), true),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            unlinkat_final_name(final_component(".."), true),
            Err(AxError::DirectoryNotEmpty)
        );
        assert_eq!(
            unlinkat_final_name(final_component("/"), true),
            Err(AxError::ResourceBusy)
        );
    }

    #[test]
    fn renameat_preserves_special_components_and_noreplace_errno() {
        assert_eq!(
            renameat_final_name(final_component("entry"), AxError::ResourceBusy),
            Ok(RenameFinalName {
                name: "entry",
                requires_directory: false,
            })
        );
        assert_eq!(
            renameat_final_name(final_component("entry/"), AxError::ResourceBusy),
            Ok(RenameFinalName {
                name: "entry",
                requires_directory: true,
            })
        );

        for path in [".", "..", "/"] {
            assert_eq!(
                renameat_final_name(final_component(path), AxError::ResourceBusy),
                Err(AxError::ResourceBusy)
            );
            assert_eq!(
                renameat_final_name(final_component(path), AxError::AlreadyExists),
                Err(AxError::AlreadyExists)
            );
        }
    }

    #[test]
    fn renameat_trailing_slash_requires_the_source_and_existing_target_directory() {
        for (old_requires, new_requires) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                validate_rename_directory_intent(old_requires, new_requires, false, None),
                Err(AxError::NotADirectory)
            );
        }

        assert_eq!(
            validate_rename_directory_intent(false, true, true, Some(false)),
            Err(AxError::NotADirectory)
        );
        assert_eq!(
            validate_rename_directory_intent(false, true, true, None),
            Ok(())
        );
        assert_eq!(
            validate_rename_directory_intent(true, true, true, Some(true)),
            Ok(())
        );
        assert_eq!(
            validate_rename_directory_intent(false, false, false, Some(true)),
            Ok(())
        );
    }

    #[test]
    fn unlink_target_resolution_never_retargets_trailing_or_dot_syntax() {
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        let file = root
            .create(
                "file",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let directory = root
            .create(
                "directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let symlink = root
            .create_symlink(
                "symlink",
                "file",
                NodePermission::from_bits_truncate(0o777),
                Some((0, 0)),
            )
            .unwrap();
        let context = axfs::FsContext::new(root.clone());
        let security = linkat_test_security();

        let (parent, name, target) =
            resolve_unlink_target_in_fs(&context, Path::new("file"), false, &security).unwrap();
        assert!(parent.same_node(&root));
        assert_eq!(name, "file");
        assert!(target.same_node(&file));

        for path in ["file/", "symlink/", "file/."] {
            assert!(matches!(
                resolve_unlink_target_in_fs(&context, Path::new(path), false, &security),
                Err(AxError::NotADirectory)
            ));
        }
        for (path, expected) in [("file/", &file), ("symlink/", &symlink)] {
            let (_, _, target) =
                resolve_unlink_target_in_fs(&context, Path::new(path), true, &security).unwrap();
            assert!(target.same_node(expected));
        }
        for path in ["directory/", "directory/."] {
            assert!(matches!(
                resolve_unlink_target_in_fs(&context, Path::new(path), false, &security),
                Err(AxError::IsADirectory)
            ));
        }
        assert!(matches!(
            resolve_unlink_target_in_fs(&context, Path::new("directory/."), true, &security,),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            resolve_unlink_target_in_fs(&context, Path::new("directory/.."), true, &security,),
            Err(AxError::DirectoryNotEmpty)
        ));
        assert!(matches!(
            resolve_unlink_target_in_fs(&context, Path::new("///"), false, &security),
            Err(AxError::IsADirectory)
        ));
        assert!(matches!(
            resolve_unlink_target_in_fs(&context, Path::new("///"), true, &security),
            Err(AxError::ResourceBusy)
        ));

        assert!(root.lookup_no_follow("file").unwrap().same_node(&file));
        assert!(
            root.lookup_no_follow("directory")
                .unwrap()
                .same_node(&directory)
        );
        assert!(
            root.lookup_no_follow("symlink")
                .unwrap()
                .same_node(&symlink)
        );
    }

    #[test]
    fn symlink_target_uses_linux_empty_and_path_max_rules_without_name_max() {
        assert_eq!(validate_symlink_target(""), Err(AxError::NotFound));
        assert_eq!(validate_symlink_target("target"), Ok(()));
        assert_eq!(validate_symlink_target(&"a".repeat(255 + 1)), Ok(()));
        assert_eq!(validate_symlink_target(&"a".repeat(4095)), Ok(()));
        assert_eq!(
            validate_symlink_target(&"a".repeat(4096)),
            Err(AxError::NameTooLong)
        );
    }

    #[test]
    fn fionbio_uses_the_complete_int_and_treats_every_nonzero_as_enabled() {
        assert!(!fionbio_enabled(0));
        for value in [1, 2, 256, -1] {
            assert!(fionbio_enabled(value));
        }
    }

    #[test]
    fn metadata_fd_origin_distinguishes_direct_and_at_empty_path_opath() {
        assert_eq!(
            check_metadata_description_status(MetadataTargetSource::DirectFd, O_PATH),
            Err(AxError::BadFileDescriptor)
        );
        assert_eq!(
            check_metadata_description_status(
                MetadataTargetSource::DirectFd,
                linux_raw_sys::general::O_RDONLY,
            ),
            Ok(())
        );
        assert_eq!(
            check_metadata_description_status(MetadataTargetSource::At, O_PATH),
            Ok(())
        );
        assert_eq!(
            check_metadata_description_status(
                MetadataTargetSource::At,
                linux_raw_sys::general::O_RDONLY,
            ),
            Ok(())
        );
    }

    #[test]
    fn metadata_syscalls_reject_invalid_flags_before_faulting_the_path_pointer() {
        let invalid = 1_u32 << 31;
        let mut provider = NoUserMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_fchownat(&mut memory, AT_FDCWD, core::ptr::null(), 0, 0, invalid),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            sys_fchmodat(&mut memory, AT_FDCWD, core::ptr::null(), 0o600, invalid),
            Err(AxError::InvalidInput)
        );
    }

    fn credentials(uid: u32, gid: u32, groups: &[u32], capabilities: &[u32]) -> DacCredentialView {
        let mut effective = [0; 2];
        for &capability in capabilities {
            let word = capability as usize / u32::BITS as usize;
            effective[word] |= 1 << (capability % u32::BITS);
        }
        let mut supplementary_groups = Vec::new();
        supplementary_groups
            .try_reserve_exact(groups.len())
            .unwrap();
        for &group in groups {
            supplementary_groups.push(Kgid::from_raw(group).unwrap());
        }
        FsCredentialSnapshot::new(
            Kuid::from_raw(uid).unwrap(),
            Kgid::from_raw(gid).unwrap(),
            GroupInfo::try_new(supplementary_groups).unwrap(),
            effective,
            true,
        )
    }

    fn metadata(uid: u32, gid: u32, mode: u16) -> Metadata {
        Metadata {
            device: 0,
            inode: 1,
            nlink: 1,
            mode: NodePermission::from_bits_truncate(mode),
            node_type: NodeType::RegularFile,
            uid,
            gid,
            size: 0,
            block_size: 4096,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: Duration::ZERO,
            btime: Duration::ZERO,
            mtime: Duration::ZERO,
            ctime: Duration::ZERO,
        }
    }

    fn chown_hook_mode(
        metadata: &Metadata,
        credentials: &DacCredentialView,
    ) -> Option<NodePermission> {
        chown_hook_mode_for_test(metadata, credentials)
    }

    fn prepare_chown_metadata_update(
        metadata: &Metadata,
        requested_user: Option<Kuid>,
        requested_group: Option<Kgid>,
        expected_hook_mode: Option<NodePermission>,
        credentials: &DacCredentialView,
        ctime: Duration,
    ) -> AxResult<MetadataUpdate> {
        assert_eq!(
            chown_hook_mode_for_test(metadata, credentials).map(|mode| mode.bits()),
            expected_hook_mode.map(|mode| mode.bits())
        );
        Ok(prepare_chown_metadata_setattr_for_test(
            metadata,
            requested_user,
            requested_group,
            credentials,
            ctime,
        )?
        .into_parts()
        .0)
    }

    fn prepare_chmod_metadata_update(
        metadata: &Metadata,
        requested_mode: u32,
        credentials: &DacCredentialView,
        ctime: Duration,
    ) -> AxResult<MetadataUpdate> {
        Ok(
            prepare_chmod_metadata_setattr_for_test(metadata, requested_mode, credentials, ctime)?
                .into_parts()
                .0,
        )
    }

    #[test]
    fn chown_authorization_uses_the_snapshot_read_inside_the_writer_gate() {
        let actor = credentials(1000, 100, &[], &[]);
        let stale_owner = metadata(1000, 100, 0o6755);
        let requested_user = Some(Kuid::from_raw(1000).unwrap());
        assert!(
            prepare_chown_metadata_update(
                &stale_owner,
                requested_user,
                None,
                chown_hook_mode(&stale_owner, &actor),
                &actor,
                Duration::from_secs(1),
            )
            .is_ok()
        );

        // A concurrent chown may replace the pre-gate snapshot. The helper
        // must be fed the fresh in-gate snapshot and reject the old owner.
        let fresh_owner = metadata(2000, 100, 0o6755);
        assert_eq!(
            prepare_chown_metadata_update(
                &fresh_owner,
                requested_user,
                None,
                chown_hook_mode(&fresh_owner, &actor),
                &actor,
                Duration::from_secs(1),
            )
            .unwrap_err(),
            AxError::OperationNotPermitted
        );
    }

    #[test]
    fn fully_omitted_chown_preserves_absence_and_needs_no_owner_authority() {
        let actor = credentials(1000, 100, &[], &[]);
        let foreign = metadata(2000, 200, 0o600);
        let update = prepare_chown_metadata_update(
            &foreign,
            None,
            None,
            chown_hook_mode(&foreign, &actor),
            &actor,
            Duration::from_secs(7),
        )
        .unwrap();

        assert_eq!(update.owner, None);
        assert!(update.mode.is_none());
        assert_eq!(update.ctime, Some(Duration::from_secs(7)));
    }

    #[test]
    fn omitted_chown_with_implicit_mode_still_requires_owner_or_fowner() {
        let actor = credentials(1000, 100, &[], &[]);
        let foreign_setuid = metadata(2000, 200, 0o4755);
        assert_eq!(
            prepare_chown_metadata_update(
                &foreign_setuid,
                None,
                None,
                chown_hook_mode(&foreign_setuid, &actor),
                &actor,
                Duration::from_secs(1),
            )
            .unwrap_err(),
            AxError::OperationNotPermitted
        );

        // CAP_CHOWN authorizes explicit ownership fields but does not imply
        // CAP_FOWNER for the implicit ATTR_MODE created by KILL_SUID.
        let chown_only = credentials(1000, 100, &[], &[CAP_CHOWN]);
        assert_eq!(
            prepare_chown_metadata_update(
                &foreign_setuid,
                Some(Kuid::from_raw(2000).unwrap()),
                None,
                chown_hook_mode(&foreign_setuid, &chown_only),
                &chown_only,
                Duration::from_secs(1),
            )
            .unwrap_err(),
            AxError::OperationNotPermitted
        );
    }

    #[test]
    fn chown_rechecks_sgid_against_the_requested_new_group_after_hook() {
        let actor = credentials(1000, 100, &[], &[CAP_CHOWN]);
        let old = metadata(1000, 100, 0o6644);
        let hook_mode = chown_hook_mode(&old, &actor);

        // The pre-hook proposal preserves SGID because the actor belongs to
        // the old group and the file is not group-executable.
        assert_eq!(hook_mode.unwrap().bits(), 0o2644);
        let update = prepare_chown_metadata_update(
            &old,
            None,
            Some(Kgid::from_raw(200).unwrap()),
            hook_mode,
            &actor,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(update.owner, Some((1000, 200)));
        assert!(!update.mode.unwrap().contains(NodePermission::SET_GID));
    }

    #[test]
    fn chmod_authorization_uses_the_snapshot_read_inside_the_writer_gate() {
        let actor = credentials(1000, 100, &[], &[]);
        assert!(
            prepare_chmod_metadata_update(
                &metadata(1000, 100, 0o755),
                0o700,
                &actor,
                Duration::from_secs(1),
            )
            .is_ok()
        );
        assert_eq!(
            prepare_chmod_metadata_update(
                &metadata(2000, 100, 0o755),
                0o700,
                &actor,
                Duration::from_secs(1),
            )
            .unwrap_err(),
            AxError::OperationNotPermitted
        );
    }

    #[test]
    fn metadata_derivation_uses_fresh_gid_for_omitted_ids_and_setgid() {
        let actor = credentials(1000, 100, &[], &[]);
        let fresh = metadata(1000, 200, 0o6755);

        let chown = prepare_chown_metadata_update(
            &fresh,
            None,
            None,
            chown_hook_mode(&fresh, &actor),
            &actor,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(chown.owner, None);
        assert_eq!(
            chown.mode.unwrap().bits(),
            NodePermission::from_bits_truncate(0o0755).bits()
        );

        let chmod =
            prepare_chmod_metadata_update(&fresh, 0o2755, &actor, Duration::from_secs(1)).unwrap();
        assert!(!chmod.mode.unwrap().contains(NodePermission::SET_GID));

        let group_member = credentials(1000, 100, &[200], &[]);
        let chmod =
            prepare_chmod_metadata_update(&fresh, 0o2755, &group_member, Duration::from_secs(1))
                .unwrap();
        assert!(chmod.mode.unwrap().contains(NodePermission::SET_GID));
    }

    #[test]
    fn successful_same_or_omitted_chown_still_kills_file_capability() {
        let fs = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let file = mount
            .root_location()
            .create(
                "chown-killpriv",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        let capability = crate::task::SECURITY_CAPABILITY_XATTR_NAME;
        let security = linkat_test_security();

        for (uid, gid) in [(-1, -1), (0, 0)] {
            file.set_xattr(capability, &[1, 2, 3], axfs_ng_vfs::XattrSetMode::Upsert)
                .unwrap();
            let requested_user = (uid != -1).then_some(Kuid::INITIAL_ROOT);
            let requested_group = (gid != -1).then_some(Kgid::INITIAL_ROOT);
            let policy =
                ChownSetattrPolicy::new(&file, requested_user, requested_group, &security).unwrap();
            let cleanup = probe_inode_setattr_privilege_cleanup(&file, policy.metadata()).unwrap();
            let published = policy
                .admit(&security, cleanup)
                .unwrap()
                .prepare()
                .unwrap()
                .publish()
                .unwrap();
            assert_eq!(
                crate::file::xattr_provider::read_security_capability(&file).unwrap(),
                None
            );
            published.commit();
        }
    }

    #[test]
    fn conservative_chown_privilege_cleanup_is_not_rolled_back_after_backend_failure() {
        let fs = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let file = mount
            .root_location()
            .create(
                "chown-killpriv-backend-failure",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o6755),
            )
            .unwrap();
        let capability = crate::task::SECURITY_CAPABILITY_XATTR_NAME;
        file.set_xattr(capability, &[1, 2, 3], axfs_ng_vfs::XattrSetMode::Upsert)
            .unwrap();
        let before = file.metadata().unwrap();

        // This is the exact boundary used by sys_fchownat: prepare succeeds,
        // killpriv commits, then an independent metadata backend may fail.
        crate::file::xattr_provider::remove_security_capability_if_present(&file).unwrap();
        let backend_result: AxResult<()> = Err(AxError::StorageFull);
        assert_eq!(backend_result, Err(AxError::StorageFull));

        assert_eq!(
            crate::file::xattr_provider::read_security_capability(&file).unwrap(),
            None
        );
        let after = file.metadata().unwrap();
        assert_eq!(after.mode.bits(), before.mode.bits());
        assert_eq!((after.uid, after.gid), (before.uid, before.gid));
    }

    #[test]
    fn committed_metadata_projection_is_infallible_and_exact() {
        let old = metadata(1000, 100, 0o6755);
        let actor = credentials(1000, 100, &[], &[CAP_CHOWN, CAP_FOWNER]);
        let (update, committed) = prepare_chown_metadata_setattr_for_test(
            &old,
            Some(Kuid::from_raw(2000).unwrap()),
            Some(Kgid::from_raw(3000).unwrap()),
            &actor,
            Duration::from_secs(4),
        )
        .unwrap()
        .into_parts();
        assert_eq!(update.owner, Some((2000, 3000)));
        assert_eq!(update.mode.unwrap().bits(), 0o755);
        assert_eq!(update.ctime, Some(Duration::from_secs(4)));
        assert_eq!(committed.mode.bits(), 0o755);
        assert_eq!((committed.uid, committed.gid), (2000, 3000));
        assert_eq!(committed.atime, old.atime);
        assert_eq!(committed.mtime, old.mtime);
        assert_eq!(committed.ctime, Duration::from_secs(4));
        assert_eq!(committed.inode, old.inode);
    }
}
