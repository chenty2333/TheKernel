use alloc::{sync::Arc, vec::Vec};
use core::ffi::{c_char, c_int, c_void};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FileBackend, FileFlags};
use axfs_ng_vfs::{
    DeviceId, FsName, FsNameBuf, FsPath, FsPathBuf, Location, MetadataUpdate, NodePermission,
    NodeType, Timestamp,
    path::{FinalComponent, FinalComponentKind},
};
use axhal::power::system_off;
use axtask::current;
use memory_addr::PAGE_SIZE_4K;
use linux_raw_sys::{
    general::*,
    ioctl::{
        FICLONE, FICLONERANGE, FIGETBSZ, FIFREEZE, FIOASYNC, FIOCLEX, FIONBIO, FIONCLEX, FIONREAD,
        FIOQSIZE, FITHAW, FS_IOC_FIEMAP, FS_IOC_FSGETXATTR, FS_IOC_GETFLAGS, NS_GET_NSTYPE,
        NS_GET_OWNER_UID, NS_GET_PARENT, NS_GET_USERNS, TIOCGWINSZ, TIOCINQ,
    },
};
use tk_linux_cred::{InodeSetattrProposal, InodeTimestampIntent, InodeTimestampValue};
use tk_linux_usercopy::{
    UserCopyError, UserMemory, UserMemoryContext, VmPtr, vm_load_until_nul,
    vm_load_until_nul_bounded, vm_write_slice,
};

use super::admit_chown;
#[cfg(test)]
use crate::file::permission::{
    chown_hook_mode_for_test, prepare_chmod_metadata_setattr_for_test,
    prepare_chown_metadata_setattr_for_test,
};
use crate::{
    file::{
        Directory, File, FileDescription, FileLike, IoctlContext, executable,
        filesystem_type_catalog, get_file_description, inode_flags,
        inotify::location_for_fd,
        namespace_mutation,
        permission::{
            ChmodSetattrPolicy, ChownSetattrPolicy, NamedCreateTerminalType, SecurityFsContextExt,
            TimestampSetattrPolicy, VfsSecurityContext, check_fchdir_permissions_with_security,
            check_pseudo_inode_permissions_with_security, check_search_permissions_with_security,
            check_writable_mount, idmapped_chown_ids,
        },
        posix_acl,
        privilege_metadata::probe_inode_setattr_privilege_cleanup,
        resolve_at_with_security, validate_symlink_target, with_path_fs,
    },
    mm::map_usercopy_error,
    mounts,
    pseudofs::{
        ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
        dev::tty::vhangup_controlling_session, namespace_target_from_proc_file,
        proc_namespace_location_from_object,
    },
    task::{
        AsThread, Cred, Kgid, Kuid, PidNamespace, UserGid, UserUid, current_fs_context,
        fs_context_publication, has_pending_syscall_signal, ns_capable,
        security::InodeSetattrCommittedSecurityRef,
    },
    time::{timestamp_from_seconds, timestamp_from_timespec, timestamp_from_timeval, wall_time},
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
const SYSFS_NAME_PATH_MAX: usize = 4096;

/// `_IOR(0x15, 0, struct fsuuid2)` from include/uapi/linux/fs.h, where
/// `struct fsuuid2 { __u8 len; __u8 uuid[16]; }` is 17 bytes: type `0x15`,
/// sequence 0, size 17.
const FS_IOC_GETFSUUID: u32 = 0x8011_1500;
/// `_IOR(0x15, 1, struct fs_sysfs_path)` from include/uapi/linux/fs.h, where
/// `struct fs_sysfs_path { __u8 len; char name[128]; }` is 129 bytes.
const FS_IOC_GETFSSYSFSPATH: u32 = 0x8081_1501;

/// Implements the obsolete `sysfs(2)` filesystem-type catalog syscall.
pub fn sys_sysfs<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    option: usize,
    arg1: *const c_char,
    arg2: *mut c_char,
) -> AxResult<isize> {
    match option as u32 as i32 {
        1 => {
            let name = vm_load_until_nul_bounded(memory, arg1.cast::<u8>(), SYSFS_NAME_PATH_MAX)
                .map_err(map_usercopy_error)?;
            if name.is_empty() {
                return Err(LinuxError::ENOENT.into());
            }
            filesystem_type_catalog()
                .iter()
                .position(|entry| entry[..entry.len() - 1] == name)
                .map(|index| index as isize)
                .ok_or(AxError::InvalidInput)
        }
        2 => {
            let index = arg1 as usize as u32 as usize;
            let name = filesystem_type_catalog()
                .get(index)
                .ok_or(AxError::InvalidInput)?;
            vm_write_slice(memory, arg2.cast::<u8>(), name).map_err(|_| AxError::BadAddress)?;
            Ok(0)
        }
        3 => Ok(filesystem_type_catalog().len() as isize),
        _ => Err(AxError::InvalidInput),
    }
}

fn try_name(value: &FsName) -> AxResult<FsNameBuf> {
    FsNameBuf::from_vec({
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(value.as_bytes().len())
            .map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(value.as_bytes());
        bytes
    })
    .map_err(Into::into)
}

fn load_user_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<FsPathBuf> {
    Ok(FsPathBuf::from_vec(
        vm_load_until_nul(memory, path.cast::<u8>()).map_err(map_usercopy_error)?,
    ))
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
            (
                ProcNamespaceKind::Cgroup
                | ProcNamespaceKind::Ipc
                | ProcNamespaceKind::Mount
                | ProcNamespaceKind::Net
                | ProcNamespaceKind::User
                | ProcNamespaceKind::Uts,
                _,
            ) => Err(AxError::InvalidInput),
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
            ProcNamespaceKind::Cgroup => CLONE_NEWCGROUP,
            ProcNamespaceKind::Ipc => CLONE_NEWIPC,
            ProcNamespaceKind::Mount => CLONE_NEWNS,
            ProcNamespaceKind::Net => CLONE_NEWNET,
            ProcNamespaceKind::Pid => CLONE_NEWPID,
            ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren => CLONE_NEWTIME,
            ProcNamespaceKind::User => CLONE_NEWUSER,
            ProcNamespaceKind::Uts => CLONE_NEWUTS,
        } as isize),
        _ => return None,
    };
    Some(result)
}

pub(crate) fn validate_pathname(path: &FsPath) -> AxResult {
    crate::file::validate_pathname(path)
}

fn proc_self_fd_location(path: &FsPath) -> Option<AxResult<LinkatSource>> {
    let fd = path.as_bytes().strip_prefix(b"/proc/self/fd/")?;
    if fd.is_empty() || fd.iter().any(|byte| !byte.is_ascii_digit()) {
        return Some(Err(AxError::NotFound));
    }

    Some(
        core::str::from_utf8(fd)
            .ok()?
            .parse::<i32>()
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
    path: Option<&FsPath>,
    flags: u32,
    source: MetadataTargetSource,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    if path.is_none_or(|path| path.as_bytes().is_empty()) {
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
    path: &FsPath,
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
    old_path: &FsPath,
    flags: u32,
    security: &VfsSecurityContext,
) -> AxResult<LinkatSource> {
    if old_path.as_bytes().is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        if old_dirfd == AT_FDCWD {
            return Ok(LinkatSource::Location(
                current_fs_context().lock().current_dir().clone(),
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

    let path = old_path;
    if path.is_absolute() || old_dirfd == AT_FDCWD {
        return resolve_hardlink_source_in_fs(
            &current_fs_context().lock(),
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
    let fs_context = current_fs_context();
    let fs = fs_context.lock();
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

fn path_from_root(loc: Location, root: &Location) -> AxResult<FsPathBuf> {
    if loc.metadata()?.nlink == 0 {
        return Err(AxError::NotFound);
    }
    let (path, reachable) = loc.path_relative_to(root)?;
    if reachable {
        return Ok(path);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(b"(unreachable)".len().saturating_add(path.as_bytes().len()))
        .map_err(|_| AxError::NoMemory)?;
    bytes.extend_from_slice(b"(unreachable)");
    bytes.extend_from_slice(path.as_bytes());
    Ok(FsPathBuf::from_vec(bytes))
}

/// The ioctl() system call manipulates the underlying device parameters
/// of special files.
const fn fionbio_enabled(value: c_int) -> bool {
    value != 0
}

/// `FIBMAP` — `_IO(0x00, 1)`, answered by `file_ioctl()` (fs/ioctl.c:325-326)
/// for a regular, non-anonymous inode only.
const FIBMAP: u32 = 0x0000_0001;

/// The legacy XFS space-reservation commands that `file_ioctl()`
/// (fs/ioctl.c:328-336) forwards to `ioctl_preallocate()`:
///
///     case FS_IOC_RESVSP:
///     case FS_IOC_RESVSP64:
///             return ioctl_preallocate(filp, 0, p);
///     case FS_IOC_UNRESVSP:
///     case FS_IOC_UNRESVSP64:
///             return ioctl_preallocate(filp, FALLOC_FL_PUNCH_HOLE, p);
///     case FS_IOC_ZERO_RANGE:
///             return ioctl_preallocate(filp, FALLOC_FL_ZERO_RANGE, p);
///
/// `include/linux/falloc.h:22-26` encodes all five with `struct space_resv`,
/// and on x86_64 every one of them uses the native 48-byte layout; only the
/// 32-bit compat entry point (fs/ioctl.c:295-317) takes `struct space_resv_32`.
const FS_IOC_RESVSP: u32 = 0x4030_5828;
const FS_IOC_UNRESVSP: u32 = 0x4030_5829;
const FS_IOC_RESVSP64: u32 = 0x4030_582a;
const FS_IOC_UNRESVSP64: u32 = 0x4030_582b;
const FS_IOC_ZERO_RANGE: u32 = 0x4030_5839;

/// `SEEK_SET`, `SEEK_CUR` and `SEEK_END` as `struct space_resv` carries them:
/// `l_whence` is `__s16`, and `ioctl_preallocate()` resolves `l_start` against
/// it (fs/ioctl.c:272-282).
const SPACE_RESV_SEEK_SET: i32 = SEEK_SET as i32;
const SPACE_RESV_SEEK_CUR: i32 = SEEK_CUR as i32;
const SPACE_RESV_SEEK_END: i32 = SEEK_END as i32;

/// Size of `struct space_resv` (include/linux/falloc.h:12-20) as the x86_64
/// kernel reads it: two `__s16` fields, two `__s64` fields, an `__s32`, a
/// `__u32` and four reserved `__s32` words.
const SPACE_RESV_SIZE: usize = 48;

/// The `l_whence`/`l_start`/`l_len` fields of a `struct space_resv`, which is
/// all `ioctl_preallocate()` uses (fs/ioctl.c:261-266).
#[derive(Clone, Copy)]
struct SpaceResv {
    whence: i32,
    start: i64,
    len: i64,
}

impl SpaceResv {
    /// Parses the three used fields out of the 48 user bytes.
    fn parse(raw: &[u8; SPACE_RESV_SIZE]) -> Self {
        let mut start = [0u8; 8];
        start.copy_from_slice(&raw[8..16]);
        let mut len = [0u8; 8];
        len.copy_from_slice(&raw[16..24]);
        Self {
            whence: i16::from_ne_bytes([raw[2], raw[3]]) as i32,
            start: i64::from_ne_bytes(start),
            len: i64::from_ne_bytes(len),
        }
    }
}

/// Returns the `vfs_fallocate()` mode that `ioctl_preallocate()` derives from
/// a legacy space-reservation command (fs/ioctl.c:328-336), or `None` when the
/// command is not one of them.
///
/// `ioctl_preallocate()` always adds `FALLOC_FL_KEEP_SIZE` before calling
/// `vfs_fallocate()`, which is why these commands never change `i_size`.
fn preallocate_mode(cmd: u32) -> Option<u32> {
    match cmd {
        FS_IOC_RESVSP | FS_IOC_RESVSP64 => Some(0),
        FS_IOC_UNRESVSP | FS_IOC_UNRESVSP64 => Some(FALLOC_FL_PUNCH_HOLE),
        FS_IOC_ZERO_RANGE => Some(FALLOC_FL_ZERO_RANGE),
        _ => None,
    }
}

/// Returns the VFS inode type of the object behind a descriptor.
///
/// Linux's generic ioctl layer classifies the *inode* (`file_inode(filp)`),
/// not the `file_operations` table, which is why commands such as `FIOQSIZE`
/// or `FIBMAP` answer differently for a directory and for a pipe even though
/// both enter `do_vfs_ioctl()`.  Objects with no VFS inode are the
/// pipefs/sockfs/anon_inodefs/pidfs pseudo files, whose `inode->i_op` carries
/// no `fiemap` and whose superblock has no block size.
fn ioctl_inode_type(f: &crate::file::FileHandle<dyn FileLike>) -> Option<NodeType> {
    f.vfs_location().map(Location::node_type)
}

pub fn sys_ioctl(context: &IoctlContext, fd: i32, cmd: u32, arg: usize) -> AxResult<isize> {
    debug!("sys_ioctl <= fd: {fd}, cmd: {cmd}, arg: {arg}");
    let f = context.get_file_like(fd)?;
    // `sys_ioctl()` reaches the LSM hook `security_file_ioctl()` before
    // `do_vfs_ioctl()` and therefore before every generic command below.
    if let Some(file) = f.downcast_ref::<File>()
        && matches!(
            file.inner().location().node_type(),
            NodeType::CharacterDevice | NodeType::BlockDevice
        )
        && !file.landlock_ioctl_dev_allowed()
    {
        crate::file::permission::report_cached_landlock_denial(
            file.inner().location(),
            crate::task::security::LANDLOCK_ACCESS_FS_IOCTL_DEV,
        );
        return Err(AxError::PermissionDenied);
    }
    // ------------------------------------------------------------------
    // Commands `do_vfs_ioctl()` answers without consulting the provider
    // (fs/ioctl.c:492-581).
    //
    // `sys_ioctl()` runs `do_vfs_ioctl()` before it ever calls
    // `->unlocked_ioctl`, and `do_dentry_open()` installs an empty
    // `file_operations` table for every `O_PATH` description
    // (fs/open.c:888-901).  These commands are therefore available on an
    // `O_PATH` descriptor: only a command that falls through to the provider
    // is refused by that empty table.  They are reproduced here at the same
    // point of the generic layer, ahead of `check_io_access()`.
    // ------------------------------------------------------------------
    let inode_type = ioctl_inode_type(&f);
    if cmd == FIOCLEX || cmd == FIONCLEX {
        // fs/ioctl.c:
        //     case FIOCLEX:
        //             set_close_on_exec(fd, 1);
        //             return 0;
        //
        //     case FIONCLEX:
        //             set_close_on_exec(fd, 0);
        //             return 0;
        context.files().set_cloexec(fd, cmd == FIOCLEX)?;
        return Ok(0);
    }
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
    if cmd == FIOASYNC {
        // fs/ioctl.c `ioctl_fioasync()`:
        //     error = get_user(on, argp);
        //     flag = on ? FASYNC : 0;
        //     if ((flag ^ filp->f_flags) & FASYNC) {
        //             if (filp->f_op->fasync)
        //                     error = filp->f_op->fasync(fd, filp, on);
        //             else
        //                     error = -ENOTTY;
        //     }
        //     return error < 0 ? error : 0;
        let val: c_int = context
            .user_memory()
            .read_value(arg as *const c_int)
            .map_err(map_usercopy_error)?;
        super::fd_ops::ioctl_fioasync(context, fd, fionbio_enabled(val))?;
        return Ok(0);
    }
    if cmd == FIOQSIZE {
        // fs/ioctl.c:
        //     case FIOQSIZE:                           /* fs/ioctl.c:513-522 */
        //             if (S_ISDIR(inode->i_mode) ||
        //                 (S_ISREG(inode->i_mode) && !IS_ANON_FILE(inode)) ||
        //                 S_ISLNK(inode->i_mode)) {
        //                     loff_t res = inode_get_bytes(inode);
        //                     return copy_to_user(argp, &res, sizeof(res)) ?
        //                                 -EFAULT : 0;
        //             }
        //
        //             return -ENOTTY;
        // The anonymous-regular exclusion is not reproduced: this kernel has no
        // `IS_ANON_FILE` predicate for the descriptors that reach here.
        // `inode_get_bytes()` is `(i_blocks << 9) + i_bytes` (fs/stat.c), and
        // the remaining byte count `i_bytes` is part of the inode size that
        // `i_blocks` already covers for every layout this kernel implements.
        match inode_type {
            Some(NodeType::Directory | NodeType::RegularFile | NodeType::Symlink) => {
                let blocks = match f.vfs_location() {
                    Some(location) => location.metadata()?.blocks,
                    None => 0,
                };
                let bytes = blocks
                    .checked_mul(512)
                    .and_then(|bytes| i64::try_from(bytes).ok())
                    .ok_or(AxError::InvalidInput)?;
                context
                    .user_memory()
                    .write_bytes(arg, &bytes.to_ne_bytes())
                    .map_err(map_usercopy_error)?;
                return Ok(0);
            }
            _ => return Err(AxError::NotATty),
        }
    }
    if cmd == FIGETBSZ {
        // fs/ioctl.c:
        //     case FIGETBSZ:
        //             /* anon_bdev filesystems may not have a block size */
        //             if (!inode->i_sb->s_blocksize)
        //                     return -EINVAL;
        //             return put_user(inode->i_sb->s_blocksize,
        //                             (int __user *)argp);
        // Objects without a VFS location live on the pipefs/sockfs/
        // anon_inodefs/pidfs pseudo-superblocks.  Those four are all built by
        // `init_pseudo()` (fs/pipe.c:1564-1572, net/socket.c:477,
        // fs/anon_inodes.c:86, fs/pidfs.c:1120), and `pseudo_fs_fill_super()`
        // sets
        //     s->s_blocksize = PAGE_SIZE;
        //     s->s_blocksize_bits = PAGE_SHIFT;
        // (fs/libfs.c:681-682), so the "anon_bdev filesystems may not have a
        // block size" guard below is *not* what such an inode hits: it reports
        // the page size.  The guard only fires for a location whose provider
        // genuinely exposes no block size.
        let block_size = match f.vfs_location() {
            Some(location) => location.metadata()?.block_size,
            None => PAGE_SIZE_4K as u64,
        };
        if block_size == 0 {
            return Err(AxError::InvalidInput);
        }
        let block_size = i32::try_from(block_size).unwrap_or(i32::MAX);
        context
            .user_memory()
            .write_bytes(arg, &block_size.to_ne_bytes())
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if cmd == FIFREEZE || cmd == FITHAW {
        // fs/ioctl.c `ioctl_fsfreeze()` / `ioctl_fsthaw()` both open with
        //     if (!ns_capable(sb->s_user_ns, CAP_SYS_ADMIN))
        //             return -EPERM;
        // `ioctl_fsfreeze()` then answers -EOPNOTSUPP when the superblock
        // provides neither `->freeze_super` nor `->freeze_fs`, and
        // `ioctl_fsthaw()` reaches `thaw_super_locked()`, whose
        //     if (sb->s_writers.frozen != SB_FREEZE_COMPLETE)
        //             goto out_unlock;          /* error = -EINVAL */
        // yields -EINVAL for a superblock that was never frozen.  This kernel
        // has no freeze machinery at all, so those two verdicts are exact.
        let security = VfsSecurityContext::new(context.caller_cred().clone());
        if !ns_capable(
            security.actor(),
            security.filesystem_owner_user_ns(),
            CAP_SYS_ADMIN,
        ) {
            return Err(AxError::OperationNotPermitted);
        }
        return Err(if cmd == FIFREEZE {
            LinuxError::EOPNOTSUPP.into()
        } else {
            AxError::InvalidInput
        });
    }
    if cmd == FS_IOC_GETFSUUID || cmd == FS_IOC_GETFSSYSFSPATH {
        // fs/ioctl.c:455-466 `ioctl_getfsuuid()` returns
        //     if (!sb->s_uuid_len)
        //             return -ENOTTY;
        // and fs/ioctl.c:468-480 `ioctl_get_fs_sysfs_path()` returns
        //     if (!strlen(sb->s_sysfs_name))
        //             return -ENOTTY;
        // This kernel maintains neither superblock attribute, so both commands
        // are unknown here exactly as they are for a Linux superblock that
        // carries neither.  A provider-visible `s_uuid`/`s_sysfs_name` pair is
        // the missing primitive for the ext4 case.
        return Err(AxError::NotATty);
    }
    if cmd == FS_IOC_FIEMAP
        && matches!(
            inode_type,
            None | Some(
                NodeType::Fifo
                    | NodeType::Socket
                    | NodeType::CharacterDevice
                    | NodeType::BlockDevice
            )
        )
    {
        // fs/ioctl.c `ioctl_fiemap()` starts with (fs/ioctl.c:206-207)
        //     if (!inode->i_op->fiemap)
        //             return -EOPNOTSUPP;
        // A pipe, socket or anonymous inode lives on a pseudo-superblock whose
        // `i_op` table carries no `fiemap` at all, and the special inodes of a
        // disk filesystem do not either (`ext4_special_inode_operations`,
        // fs/ext4/namei.c:4243-4249), so those commands are unsupported rather
        // than unknown.  Directories are deliberately left to the provider:
        // ext4 does register `->fiemap` for them (fs/ext4/namei.c:4238), which
        // this kernel's directory provider cannot serve yet.
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if matches!(cmd, FICLONE | FICLONERANGE) {
        // `do_vfs_ioctl()` reaches `ioctl_file_clone()` (fs/ioctl.c:230-248)
        // for every descriptor, and `vfs_clone_file_range()` then classifies
        // the two inodes in `generic_file_rw_checks()`
        // (fs/read_write.c:1785-1801):
        //     if (S_ISDIR(inode_in->i_mode) || S_ISDIR(inode_out->i_mode))
        //             return -EISDIR;
        //     if (!S_ISREG(inode_in->i_mode) || !S_ISREG(inode_out->i_mode))
        //             return -EINVAL;
        // The source side of that test needs the descriptor named by `arg` and
        // stays with the provider; the destination side is decided here so a
        // directory or a FIFO destination is no longer reported as an unknown
        // command.
        match inode_type {
            Some(NodeType::Directory) => return Err(AxError::IsADirectory),
            Some(NodeType::RegularFile) | None => {}
            Some(_) => return Err(AxError::InvalidInput),
        }
    }
    if inode_type == Some(NodeType::RegularFile) {
        // `do_vfs_ioctl()`'s default arm only reaches `file_ioctl()` for
        // `S_ISREG(inode->i_mode) && !IS_ANON_FILE(inode)`.
        if cmd == FIBMAP {
            // fs/ioctl.c `ioctl_fibmap()` opens with (fs/ioctl.c:58-63)
            //     if (!capable(CAP_SYS_RAWIO))
            //             return -EPERM;
            //     error = get_user(ur_block, p);
            if !current().as_thread().has_effective_capability(CAP_SYS_RAWIO) {
                return Err(AxError::OperationNotPermitted);
            }
        } else if let Some(mode) = preallocate_mode(cmd) {
            // fs/ioctl.c `ioctl_preallocate()` (fs/ioctl.c:268-289) copies a
            // `struct space_resv`, resolves its offset against
            // `SEEK_SET`/`SEEK_CUR`/`SEEK_END` and forwards to
            //     return vfs_fallocate(filp, mode | FALLOC_FL_KEEP_SIZE, sr.l_start,
            //                     sr.l_len);
            let raw: [u8; SPACE_RESV_SIZE] = context
                .user_memory()
                .read_value(arg as *const [u8; SPACE_RESV_SIZE])
                .map_err(map_usercopy_error)?;
            let sr = SpaceResv::parse(&raw);
            let metadata = f
                .vfs_location()
                .ok_or(AxError::InvalidInput)?
                .metadata()?;
            let base = match sr.whence {
                SPACE_RESV_SEEK_SET => 0,
                SPACE_RESV_SEEK_CUR => {
                    super::io::seek_file_like(&f, 0, SPACE_RESV_SEEK_CUR)? as i64
                }
                SPACE_RESV_SEEK_END => {
                    i64::try_from(metadata.size).map_err(|_| AxError::InvalidInput)?
                }
                _ => return Err(AxError::InvalidInput),
            };
            let offset = base.checked_add(sr.start).ok_or(AxError::InvalidInput)?;
            let security = VfsSecurityContext::new(context.caller_cred().clone());
            super::io::fallocate_file_like(
                &f,
                &security,
                mode | FALLOC_FL_KEEP_SIZE,
                offset,
                sr.len,
            )?;
            return Ok(0);
        }
    }
    // A command that `do_vfs_ioctl()` answers itself has already returned.  The
    // remaining cases of its default arm enter the provider or an inode
    // operation through the generic layer, so the empty `O_PATH` table does not
    // stop all of them: `ioctl_fiemap()` and `ioctl_getflags()` never test
    // `f_mode`, while `ioctl_file_clone()` and `ioctl_preallocate()` fail with
    // their own `-EBADF` (`generic_file_rw_checks()`, `vfs_fallocate()`), which
    // `check_io_access()` reports here as well.  Every other command is a
    // provider command and is refused on an `O_PATH` descriptor exactly like
    // Linux's empty `file_operations`.
    let reaches_object = matches!(cmd, FS_IOC_FIEMAP | FS_IOC_GETFLAGS | FS_IOC_FSGETXATTR)
        || (inode_type == Some(NodeType::RegularFile)
            && (cmd == FIBMAP || preallocate_mode(cmd).is_some()));
    if !reaches_object {
        f.check_io_access()?;
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
    debug!("sys_chdir <= path: {path:?}");

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let _fs_context_publication = fs_context_publication();
    let fs_context = current_fs_context();
    let mut fs = fs_context.lock();
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

    let _fs_context_publication = fs_context_publication();

    // fdget_raw-equivalent: retain this exact open file description once and
    // keep it live through the permission decision and cwd publication.
    // Directory::from_fd also preserves Linux's EBADF-before-ENOTDIR order.
    let directory = Directory::from_fd(dirfd)?;
    let entry = directory.inner();
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    check_fchdir_permissions_with_security(entry, &security)?;
    // One FS-context critical section publishes the new pwd only after all
    // fallible fd/type/permission stages succeeded, preserving CLONE_FS.
    current_fs_context().lock().set_current_dir(entry.clone())?;
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
    debug!("sys_chroot <= path: {path:?}");

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let _fs_context_publication = fs_context_publication();
    let fs_context = current_fs_context();
    let mut fs = fs_context.lock();
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
    debug!("sys_mkdirat <= dirfd: {dirfd}, path: {path:?}, mode: {mode}");
    if path.as_bytes().is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(&path)?;

    let curr = current();
    let requested_mode = NodePermission::from_bits_truncate(mode as u16);
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let path_ref = &*path;
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_named_create_security(
            path_ref,
            &security,
            NamedCreateTerminalType::Directory,
        )?;
        Ok((parent, try_name(name)?))
    })?;
    inode_flags::check_content_mutable(&parent)?;
    let loc = namespace_mutation::create_named(
        &mount_operation,
        &parent,
        &name,
        NodeType::Directory,
        requested_mode,
        current_fs_context().lock().umask(),
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
    let path_ref = &*path;
    validate_pathname(path_ref)?;
    debug!("sys_mknodat <= dirfd: {dirfd}, path: {path:?}, mode: {mode:#o}, dev: {dev}");

    let node_type = decode_mknod_node_type(mode)?;

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());

    let requested_mode = NodePermission::from_bits_truncate(mode as u16);
    let mount_operation = mounts::namespace_operation();
    let (parent, name) = with_path_fs(dirfd, path_ref, |fs| {
        let (parent, name) = fs.resolve_named_create_security(
            path_ref,
            &security,
            NamedCreateTerminalType::NonDirectory,
        )?;
        Ok((parent, try_name(name)?))
    })?;
    inode_flags::check_content_mutable(&parent)?;

    let rdev = matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice)
        .then_some(DeviceId(dev));
    let loc = namespace_mutation::create_named(
        &mount_operation,
        &parent,
        &name,
        node_type,
        requested_mode,
        current_fs_context().lock().umask(),
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
    if name.is_empty() || name.len() >= GETDENTS_NAME_PATH_MAX || name.contains(&b'/') {
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

fn copy_dirents<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    count: usize,
    format: DirentFormat,
    dir_offset: &mut u64,
    mut read_dir: impl FnMut(u64, &mut dyn axfs_ng_vfs::DirEntrySink) -> AxResult<usize>,
    mut has_signal: impl FnMut() -> bool,
) -> AxResult<isize> {
    // Filesystems may hold their own locks while invoking the sink. Usercopy
    // acquires AddrSpace and may fault a file-backed mapping, so it must run
    // after read_dir returns. Bound staging independently of the user's count.
    const BATCH_ENTRIES: usize = 32;
    let mut batch = Vec::new();
    batch
        .try_reserve_exact(BATCH_ENTRIES)
        .map_err(|_| AxError::NoMemory)?;
    let mut copied = 0;
    let mut stop_error = None;
    let mut stopped_for_space = false;
    let mut last_reclen = 0;

    let iteration = 'batches: loop {
        batch.clear();
        let mut staged = 0;
        let mut full_batch = false;
        let iteration = read_dir(*dir_offset, &mut |name: &FsName, ino, node_type, offset| {
            let record_len = match dirent_record_len(format, name.as_bytes()) {
                Ok(record_len) => record_len,
                Err(error) => {
                    stop_error = Some(error);
                    return false;
                }
            };
            if !getdents_has_room(count, copied + staged, record_len) {
                stopped_for_space = true;
                return false;
            }
            let mut record = Vec::new();
            if let Err(error) =
                fill_dirent(&mut record, format, ino, offset, node_type, name.as_bytes())
            {
                stop_error = Some(error);
                return false;
            }
            staged += record.len();
            batch.push((offset, record));
            full_batch = batch.len() == BATCH_ENTRIES;
            !full_batch
        });

        for (offset, record) in &batch {
            // Linux only checks signals between already completed records.
            if copied != 0 && has_signal() {
                break 'batches iteration;
            }
            // Any failure from the user-memory provider is EFAULT for this
            // copyout, including provider-side allocation failures. Commit
            // only copied records, not the iterator's speculative batch end.
            if vm_write_slice(memory, buf.wrapping_add(copied), record).is_err() {
                stop_error = Some(AxError::BadAddress);
                break 'batches iteration;
            }
            *dir_offset = *offset;
            copied += record.len();
            last_reclen = record.len();
        }
        if iteration.is_err() || stop_error.is_some() || stopped_for_space || !full_batch {
            break iteration;
        }
    };

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

    let result = copy_dirents(
        memory,
        buf,
        getdents_count(count),
        format,
        &mut dir.offset.lock(),
        |offset, sink| dir.read_dir(offset, sink),
        || has_pending_syscall_signal(current().as_thread()),
    );

    // Linux marks every live-directory iteration as an access, including an
    // empty result, EINVAL/EFAULT from the actor, or an iterator error.
    // Metadata failures are best-effort and cannot change the syscall result.
    if mounts::should_update_atime(dir.inner()) {
        warn_notification(
            "getdents atime",
            dir.inner().update_supported_metadata(MetadataUpdate {
                atime: Some(wall_time().into()),
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
         new_path: {new_path:?}, flags: {flags}"
    );

    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_FOLLOW) != 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !old_path.as_bytes().is_empty() {
        validate_pathname(&old_path)?;
    }
    let source = resolve_linkat_source(old_dirfd, &old_path, flags, &security)?;

    if new_path.as_bytes().is_empty() {
        return Err(AxError::NotFound);
    }
    validate_pathname(&new_path)?;

    let mount_operation = mounts::namespace_operation();
    let new_path_ref = &*new_path;
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
    inode_flags::check_content_mutable(&new_dir)?;
    // Linking changes the source inode's link count/ctime. Linux rejects this
    // metadata mutation for immutable and append-only inodes even though no
    // file data is written.
    inode_flags::check_nonappend_content_mutable(&old)?;
    // bpffs object dentries are not ordinary hard-linkable files.  In
    // particular, accepting a second link would create another pathname with
    // no independent object-pin lifetime.
    if mounts::metadata_for_location(&old)?.fs_type == "bpf"
        || mounts::metadata_for_location(&new_dir)?.fs_type == "bpf"
    {
        return Err(LinuxError::EPERM.into());
    }

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
    name: &'a FsName,
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
    path: &FsPath,
    remove_dir: bool,
    security: &VfsSecurityContext,
) -> AxResult<(Location, FsNameBuf, Location)> {
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
    let name = try_name(final_name.name)?;
    let target = parent.lookup_no_follow_in_mount(&name)?;
    // Immutable directories reject namespace changes and immutable targets
    // cannot lose a name.  Do this after lookup, matching vfs_unlink's inode
    // admission placement rather than turning a missing name into EPERM.
    // Removing a name is forbidden from an append-only directory, and an
    // append-only inode may not lose one of its names.
    inode_flags::check_nonappend_content_mutable(&parent)?;
    inode_flags::check_nonappend_content_mutable(&target)?;
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
    let path_ref = &*path;
    if path.as_bytes().is_empty() {
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
    let _swap_mutation = crate::mm::admit_mutation(&loc)?;
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
        // bpffs owns a strong BPF-object reference at its final dentry. Drop
        // that reference in the same successful last-link transition; a
        // rename never reaches this path and non-final hard-link removal must
        // not unpin the object.
        #[cfg(feature = "bpf")]
        crate::bpf::forget_pinned_object(&loc);
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
        let fs_context = current_fs_context();
        let fs = fs_context.lock();
        path_from_root(fs.current_dir().clone(), fs.root_dir())?
    };
    debug!("sys_getcwd => cwd: {cwd:?}");

    let mut cwd = cwd.into_vec();
    cwd.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
    cwd.push(0);

    // Linux `SYSCALL_DEFINE2(getcwd, ...)` in fs/d_path.c renders the current
    // directory into a fixed kmalloc(PATH_MAX) scratch buffer (`__getname()`)
    // and then decides:
    //   len = PATH_MAX - b.len;
    //   if (unlikely(len > PATH_MAX))      error = -ENAMETOOLONG;
    //   else if (unlikely(len > size))     error = -ERANGE;
    //   else if (copy_to_user(buf, b.buf, len)) error = -EFAULT;
    //   else                               error = len;
    // `len` includes the trailing NUL, and there is no NULL test before the
    // length comparison, so getcwd(NULL, 0) is -ERANGE and getcwd(NULL, n) is
    // -EFAULT. The only place a NULL buf is ever dereferenced is the copy.
    let len = linux_vfs::getcwd_user_len(cwd.len(), size).map_err(|error| match error {
        linux_vfs::GetcwdError::NameTooLong => AxError::from(LinuxError::ENAMETOOLONG),
        linux_vfs::GetcwdError::Range => AxError::OutOfRange,
    })?;

    if buf.is_null() {
        return Err(AxError::BadAddress);
    }

    vm_write_slice(memory, buf, &cwd).map_err(map_usercopy_error)?;
    Ok(len as isize)
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

    if linkpath.as_bytes().is_empty() {
        return Err(AxError::NotFound);
    }
    let linkpath_ref = &*linkpath;
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
        Ok((parent, try_name(name)?))
    })?;
    inode_flags::check_content_mutable(&parent)?;
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
        let read = size.min(link.as_bytes().len());
        vm_write_slice(memory, buf, &link.as_bytes()[..read]).map_err(map_usercopy_error)?;
        Ok(read as isize)
    }

    let path = load_user_path(memory, path)?;

    debug!("sys_readlinkat <= dirfd: {dirfd}, path: {path:?}");
    if size == 0 {
        return Err(AxError::InvalidInput);
    }
    if path.as_bytes().is_empty() {
        if dirfd == AT_FDCWD {
            return Err(AxError::NotFound);
        }
        let loc = location_for_fd(dirfd).ok_or(AxError::BadFileDescriptor)?;
        if loc.node_type() != NodeType::Symlink {
            return Err(AxError::NotFound);
        }
        return write_readlink_result(memory, &loc, buf, size);
    }
    validate_pathname(&path)?;

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());

    with_path_fs(dirfd, &path, |fs| {
        let entry = fs.resolve_no_follow_security(&path, &security)?;
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
        Some(&path),
        uid,
        gid,
        flags,
        MetadataTargetSource::At,
    )
}

fn do_fchownat(
    dirfd: i32,
    path: Option<&FsPath>,
    uid: i32,
    gid: i32,
    flags: u32,
    source: MetadataTargetSource,
) -> AxResult<isize> {
    if flags & !SUPPORTED_FCHOWNAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path {
        if path.as_bytes().is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(path)?;
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
    inode_flags::check_nonappend_content_mutable(&loc)?;
    let (requested_user, requested_group) = requested_chown_ids(security.actor(), uid, gid)?;
    let (requested_user, requested_group) =
        idmapped_chown_ids(&loc, &security, requested_user, requested_group)?;
    executable::with_credential_metadata_unpinned(&loc, || {
        let policy = ChownSetattrPolicy::new(&loc, requested_user, requested_group, &security)?;
        let privilege_cleanup = probe_inode_setattr_privilege_cleanup(&loc, policy.metadata())?;

        // notify_change() validates the target filesystem mapping before the
        // inode hook, but setattr_prepare's owner/CAP checks remain later.
        let old_metadata = loc.metadata()?;
        let transfer = admit_chown(&loc, &old_metadata, policy.metadata())?;
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
        transfer.0.commit();
        transfer.1.commit();
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
    do_fchmodat(dirfd, Some(&path), mode, flags, MetadataTargetSource::At)
}

fn do_fchmodat(
    dirfd: i32,
    path: Option<&FsPath>,
    mode: u32,
    flags: u32,
    source: MetadataTargetSource,
) -> AxResult<isize> {
    if flags & !SUPPORTED_FCHMODAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path {
        if path.as_bytes().is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(path)?;
    }
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    // See the chown path above: this broad writer gate is an interim mechanism,
    // not the final per-inode metadata transaction architecture.
    let _metadata_writer_fallback = mounts::namespace_operation();
    let loc = resolve_metadata_target(dirfd, path, flags, source, &security)?;
    check_writable_mount(&loc)?;
    inode_flags::check_nonappend_content_mutable(&loc)?;
    let publishes_setid =
        mode & (NodePermission::SET_UID | NodePermission::SET_GID).bits() as u32 != 0;
    executable::with_setid_metadata_unpinned(&loc, publishes_setid, || {
        let policy = ChmodSetattrPolicy::new(&loc, mode, &security)?;
        if policy.metadata().node_type == NodeType::Symlink {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        let prepared = policy.admit(&security)?.prepare()?;
        let acl = posix_acl::prepare_chmod(&loc, prepared.committed_mode())?;
        let (published, _) = if let Some(acl) = acl {
            prepared.publish_with_staged(|| acl.stage(&loc), |_| acl.rollback(&loc))?
        } else {
            (prepared.publish()?, ())
        };

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
    atime: Option<Timestamp>,
    mtime: Option<Timestamp>,
    atime_intent: TimeUpdate,
    mtime_intent: TimeUpdate,
    flags: u32,
) -> AxResult<()> {
    let path = path
        .nullable()
        .map(|path| load_user_path(memory, path))
        .transpose()?;
    if atime_intent == TimeUpdate::Omit && mtime_intent == TimeUpdate::Omit {
        return Ok(());
    }
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    // Keep resolution, mount admission, DAC, security hooks, and publication
    // in one metadata-writer transaction.  In particular, a concurrent chown
    // cannot change the owner between the timestamp permission check and the
    // metadata update.
    let _metadata_writer = mounts::namespace_operation();
    match resolve_at_with_security(dirfd, path.as_deref(), flags, &security)? {
        crate::file::ResolveAtResult::File(loc) => {
            // Linux's mount-write admission precedes notify_change's DAC and
            // setattr path, so a read-only mount wins over EPERM/EACCES.
            check_writable_mount(&loc)?;
            inode_flags::check_nonappend_content_mutable(&loc)?;
            let intent = InodeTimestampIntent::new(
                timestamp_value(atime_intent),
                timestamp_value(mtime_intent),
            );
            let published = TimestampSetattrPolicy::new(&loc, atime, mtime, intent)?
                .admit(&security)?
                .publish()?;
            published.commit();
            Ok(())
        }
        crate::file::ResolveAtResult::Other(file) => {
            // A NULL pathname plus a descriptor denotes that exact struct
            // file; pipes and sockets have mutable pseudo-inode timestamps.
            let stat = file.stat()?;
            let metadata = pseudo_metadata(&stat);
            let credentials = security.credentials();
            if Kuid::from_raw(metadata.uid) != Some(credentials.uid())
                && !security.has_capability(CAP_FOWNER)
            {
                if (atime_intent, mtime_intent) != (TimeUpdate::Now, TimeUpdate::Now) {
                    return Err(AxError::OperationNotPermitted);
                }
                check_pseudo_inode_permissions_with_security(&metadata, W_OK, &security)?;
            }
            let intent = InodeTimestampIntent::new(
                timestamp_value(atime_intent),
                timestamp_value(mtime_intent),
            );
            let admission = security
                .begin_pseudo_inode_setattr(&metadata, InodeSetattrProposal::timestamps(intent))?;
            let ctime = wall_time().into();
            file.update_timestamps(atime, mtime, ctime)?;
            let mut committed = metadata;
            if let Some(atime) = atime {
                committed.atime = atime;
            }
            if let Some(mtime) = mtime {
                committed.mtime = mtime;
            }
            committed.ctime = ctime;
            admission.committed(InodeSetattrCommittedSecurityRef::new_pseudo(&committed));
            Ok(())
        }
    }
}

fn pseudo_metadata(stat: &crate::file::Kstat) -> axfs_ng_vfs::Metadata {
    axfs_ng_vfs::Metadata {
        device: stat.dev,
        inode: stat.ino,
        nlink: stat.nlink as u64,
        mode: NodePermission::from_bits_truncate((stat.mode & 0o7777) as u16),
        node_type: NodeType::from((stat.mode >> 12) as u8),
        uid: stat.uid,
        gid: stat.gid,
        project_id: 0,
        size: stat.size,
        block_size: stat.blksize as u64,
        blocks: stat.blocks,
        rdev: stat.rdev,
        atime: stat.atime,
        btime: stat.btime,
        mtime: stat.mtime,
        ctime: stat.ctime,
    }
}

const fn timestamp_value(update: TimeUpdate) -> InodeTimestampValue {
    match update {
        TimeUpdate::Omit => InodeTimestampValue::Omit,
        TimeUpdate::Now => InodeTimestampValue::Now,
        TimeUpdate::Explicit => InodeTimestampValue::Explicit,
    }
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
            timestamp_from_seconds(times.actime),
            timestamp_from_seconds(times.modtime),
        )
    } else {
        let time = wall_time();
        (time.into(), time.into())
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
        (
            timestamp_from_timeval(atime.tv_sec, atime.tv_usec)?,
            timestamp_from_timeval(mtime.tv_sec, mtime.tv_usec)?,
        )
    } else {
        let time = wall_time();
        (time.into(), time.into())
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

/// Legacy timeval front-end shared by `futimesat(2)`.
///
/// Unlike `utimensat`, this ABI never accepts special nanosecond values: the
/// microseconds must be checked before conversion so invalid values cannot be
/// hidden by the `* 1000` conversion.
fn legacy_futimesat_pair(
    times: [linux_raw_sys::general::__kernel_old_timeval; 2],
) -> AxResult<(Timestamp, Timestamp)> {
    fn convert(time: linux_raw_sys::general::__kernel_old_timeval) -> AxResult<Timestamp> {
        timestamp_from_timeval(time.tv_sec, time.tv_usec)
    }
    Ok((convert(times[0])?, convert(times[1])?))
}

/// Linux's legacy `do_futimesat` wrapper: copy and validate the old timeval
/// pair first, then delegate pathname/dirfd resolution and setattr policy to
/// the common utimensat path with no flags.
pub fn sys_futimesat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    times: *const [linux_raw_sys::general::__kernel_old_timeval; 2],
) -> AxResult<isize> {
    let (atime, mtime, intent) = if let Some(times) = times.nullable() {
        // SAFETY: the x86_64 legacy ABI has two initialized integer words per
        // timeval; usercopy establishes the complete pair before conversion.
        let times = unsafe {
            times
                .vm_read_uninit(memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
        let (atime, mtime) = legacy_futimesat_pair(times)?;
        (Some(atime), Some(mtime), TimeUpdate::Explicit)
    } else {
        let now = wall_time();
        (Some(now.into()), Some(now.into()), TimeUpdate::Now)
    };

    // `do_utimes` treats a null pathname as an fd target only when dfd is a
    // real descriptor. A null pathname with AT_FDCWD is still a bad user
    // pathname, not an empty-path request.
    if path.is_null() {
        if dirfd == AT_FDCWD {
            return Err(AxError::BadAddress);
        }
        update_times(
            memory,
            dirfd,
            path,
            atime,
            mtime,
            intent,
            intent,
            AT_EMPTY_PATH,
        )?;
    } else {
        update_times(memory, dirfd, path, atime, mtime, intent, intent, 0)?;
    }
    Ok(0)
}

pub fn sys_utimensat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    path: *const c_char,
    times: *const [timespec; 2],
    mut flags: u32,
) -> AxResult<isize> {
    fn utime_to_timestamp(time: &timespec) -> (Option<AxResult<Timestamp>>, TimeUpdate) {
        match time.tv_nsec {
            val if val == UTIME_OMIT as _ => (None, TimeUpdate::Omit),
            val if val == UTIME_NOW as _ => (Some(Ok(wall_time().into())), TimeUpdate::Now),
            _ => (
                Some(timestamp_from_timespec(time.tv_sec, time.tv_nsec)),
                TimeUpdate::Explicit,
            ),
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
        if atime.tv_nsec == UTIME_OMIT as _ && mtime.tv_nsec == UTIME_OMIT as _ {
            return Ok(0);
        }
        let (atime, atime_intent) = utime_to_timestamp(&atime);
        let (mtime, mtime_intent) = utime_to_timestamp(&mtime);
        (
            atime.transpose()?,
            mtime.transpose()?,
            atime_intent,
            mtime_intent,
        )
    } else {
        let time = wall_time();
        (
            Some(time.into()),
            Some(time.into()),
            TimeUpdate::Now,
            TimeUpdate::Now,
        )
    };
    if path.is_null() {
        if flags != 0 {
            return Err(AxError::InvalidInput);
        }
        flags |= AT_EMPTY_PATH;
    }
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
    name: &'a FsName,
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

fn lookup_optional_in_mount(parent: &Location, name: &FsName) -> AxResult<Option<Location>> {
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
    let old_path_ref = &*old_path;
    let new_path_ref = &*new_path;
    debug!(
        "sys_renameat2 <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path:?}, flags: {flags}"
    );

    if flags & !SUPPORTED_RENAMEAT2_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & RENAME_EXCHANGE != 0 && flags & (RENAME_NOREPLACE | RENAME_WHITEOUT) != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & RENAME_WHITEOUT != 0 && flags & RENAME_NOREPLACE != 0 {
        return Err(AxError::InvalidInput);
    }
    if old_path.as_bytes().is_empty() || new_path.as_bytes().is_empty() {
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
    if flags & RENAME_EXCHANGE != 0 && new_existing.is_none() {
        // Exchange has no create half: both names must have been resolved
        // under their exact parent mounts before any permission or backend
        // admission is attempted.
        return Err(AxError::NotFound);
    }
    // A no-replace request reports the existing destination before ordinary
    // rename's same-inode no-op.  All other modes short-circuit here, before
    // immutable/append and active-swap admission can reject a mutation that
    // will not occur.
    if flags & RENAME_NOREPLACE != 0 && new_existing.is_some() {
        return Err(AxError::AlreadyExists);
    }
    if new_existing
        .as_ref()
        .is_some_and(|destination| destination.same_node(&old_loc))
    {
        return Ok(0);
    }
    inode_flags::check_nonappend_content_mutable(&old_dir)?;
    if new_existing.is_some() {
        // Replacing a destination removes a name from this directory. Merely
        // adding a new name remains permitted for an append-only directory.
        inode_flags::check_nonappend_content_mutable(&new_dir)?;
    } else {
        inode_flags::check_content_mutable(&new_dir)?;
    }
    inode_flags::check_nonappend_content_mutable(&old_loc)?;
    if let Some(destination) = new_existing.as_ref() {
        inode_flags::check_nonappend_content_mutable(destination)?;
    }
    // Swap backing is identified by inode, not pathname: prohibit both
    // renaming the active backing and replacing it through the destination.
    crate::mm::check_not_active(&old_loc)?;
    let _old_swap_mutation = crate::mm::admit_mutation(&old_loc)?;
    if let Some(destination) = new_existing.as_ref() {
        crate::mm::check_not_active(destination)?;
    }
    let _destination_swap_mutation = new_existing
        .as_ref()
        .map(crate::mm::admit_mutation)
        .transpose()?;

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
    let old_is_dir = old_loc.is_dir();

    let outcome = if flags & RENAME_EXCHANGE != 0 {
        namespace_mutation::rename_exchange(
            &mount_operation,
            &old_dir,
            old_final.name,
            &old_loc,
            &new_dir,
            new_final.name,
            new_existing.as_ref().ok_or(AxError::NotFound)?,
            &security,
        )?
    } else if flags & RENAME_WHITEOUT != 0 {
        namespace_mutation::rename_whiteout(
            &mount_operation,
            &old_dir,
            old_final.name,
            &old_loc,
            &new_dir,
            new_final.name,
            new_existing.as_ref(),
            &security,
        )?
    } else {
        namespace_mutation::rename(
            &mount_operation,
            &old_dir,
            old_final.name,
            &old_loc,
            &new_dir,
            new_final.name,
            new_existing.as_ref(),
            flags & RENAME_NOREPLACE != 0,
            &security,
        )?
    };
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
    if flags & RENAME_EXCHANGE != 0 {
        // An exchange is two correlated moves, each with its own cookie; the
        // paired notifications preserve the identities observed before the
        // backend's single swap transaction.
        let destination = new_existing.as_ref().ok_or(AxError::NotFound)?;
        let exchange_cookie = crate::file::inotify::next_rename_cookie();
        warn_notification(
            "rename exchange destination source",
            crate::file::inotify::notify_parent_with_name(
                &new_dir,
                Some(destination),
                new_final.name,
                IN_MOVED_FROM,
                destination.is_dir(),
                exchange_cookie,
            ),
        );
        warn_notification(
            "rename exchange destination target",
            crate::file::inotify::notify_parent_with_name(
                &old_dir,
                Some(destination),
                old_final.name,
                IN_MOVED_TO,
                destination.is_dir(),
                exchange_cookie,
            ),
        );
        warn_notification(
            "rename exchange self",
            crate::file::inotify::notify_exact(destination, IN_MOVE_SELF),
        );
    }
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
    let mount = current_fs_context().lock().root_dir().mountpoint().clone();
    mount.flush_all_filesystems()?;
    Ok(0)
}

pub fn sys_syncfs(fd: i32) -> AxResult<isize> {
    // Retain this exact OFD: close/reuse must not redirect its errseq cursor
    // or filesystem anchor.  Do not apply I/O access checks: Linux permits
    // O_PATH and read-only descriptors for syncfs.
    get_file_description(fd)?.sync_filesystem()?;
    Ok(0)
}

static CAD_REBOOT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Called by the physical keyboard worker, never under an input/VT lock.
pub(crate) fn ctrl_alt_delete() {
    if CAD_REBOOT.load(core::sync::atomic::Ordering::Acquire) {
        let mount = axfs::FS_CONTEXT.lock().root_dir().mountpoint().clone();
        let _ = mount.flush_all_filesystems();
        axhal::power::system_reset();
    } else {
        // The initial PID namespace owns CAD even if a keyboard ioctl caller
        // happens to reside in a nested namespace.
        let _ = crate::task::send_signal_to_process(
            1,
            Some(tk_linux_signal::SignalInfo::new_kernel(
                tk_linux_signal::Signo::SIGINT,
            )),
        );
    }
}

/// `strncpy_from_user(&buffer[0], arg, sizeof(buffer) - 1)` in
/// `SYSCALL_DEFINE4(reboot, ...)` (kernel/reboot.c), where `char buffer[256]`.
const REBOOT_RESTART2_SCAN: usize = 255;

pub fn sys_reboot<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    magic1: i32,
    magic2: i32,
    cmd: i32,
    arg: *const c_void,
) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    // Namespace-local CAP_SYS_BOOT is not authority over physical power.
    if !thread.current_cred().user_ns().is_initial() || !current_has_capability(CAP_SYS_BOOT) {
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

    // `reboot_pid_ns()` (kernel/pid_namespace.c) runs before the command
    // switch: outside the initial PID namespace only RESTART, RESTART2, HALT
    // and POWER_OFF are recognised and everything else is -EINVAL.  The
    // recognised four are supposed to set `pid_ns->reboot` and SIGKILL the
    // namespace's child reaper, which needs a namespace-reaper transaction
    // rather than platform power I/O, so they stay -EOPNOTSUPP here.
    if thread.pid_ns().parent().is_some() {
        return Err(
            if matches!(
                cmd as u32,
                LINUX_REBOOT_CMD_RESTART
                    | LINUX_REBOOT_CMD_RESTART2
                    | LINUX_REBOOT_CMD_HALT
                    | LINUX_REBOOT_CMD_POWER_OFF
            ) {
                LinuxError::EOPNOTSUPP.into()
            } else {
                AxError::InvalidInput
            },
        );
    }

    // RESTART2 hands the restart command to the platform restart handler, so
    // Linux copies it out of userspace first:
    //     case LINUX_REBOOT_CMD_RESTART2:
    //             ret = strncpy_from_user(&buffer[0], arg, sizeof(buffer) - 1);
    //             if (ret < 0) { ret = -EFAULT; break; }
    //             buffer[sizeof(buffer) - 1] = '\0';
    //             kernel_restart(buffer);
    // A NULL or unreadable `arg` is therefore -EFAULT before anything is
    // restarted, while a command at least 255 bytes long is truncated to 255
    // bytes and still restarts (strncpy_from_user reports the truncation as a
    // byte count, not an error).
    let restart_command = if cmd as u32 == LINUX_REBOOT_CMD_RESTART2 {
        match vm_load_until_nul_bounded(memory, arg.cast::<u8>(), REBOOT_RESTART2_SCAN) {
            Ok(command) => command,
            Err(UserCopyError::TooLong) => Vec::new(),
            Err(error) => return Err(map_usercopy_error(error)),
        }
    } else {
        Vec::new()
    };

    match cmd as u32 {
        LINUX_REBOOT_CMD_RESTART | LINUX_REBOOT_CMD_RESTART2 => {
            sys_sync()?;
            if restart_command.is_empty() {
                ax_println!("System is restarting");
            } else {
                ax_println!(
                    "System is restarting with command: {}",
                    core::str::from_utf8(&restart_command).unwrap_or("<invalid utf-8>")
                );
            }
            axhal::power::system_reset();
        }
        LINUX_REBOOT_CMD_HALT => {
            // Do not strand only the calling CPU while other CPUs continue
            // running userspace. A fleet-wide terminal stop protocol is needed
            // before SMP HALT can be offered honestly.
            if axhal::cpu_num() > 1 {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            sys_sync()?;
            ax_println!("System is halted");
            axhal::power::system_halt();
        }
        LINUX_REBOOT_CMD_POWER_OFF => {
            sys_sync()?;
            ax_println!("System is powering off");
            system_off();
        }
        LINUX_REBOOT_CMD_KEXEC => {
            sys_sync()?;
            crate::syscall::task::execute_loaded()
        }
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => {
            CAD_REBOOT.store(
                cmd as u32 == LINUX_REBOOT_CMD_CAD_ON,
                core::sync::atomic::Ordering::Release,
            );
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_vhangup() -> AxResult<isize> {
    if !current_has_capability(CAP_SYS_TTY_CONFIG) {
        return Err(AxError::OperationNotPermitted);
    }

    // Linux treats the absence of a controlling terminal as a successful
    // no-op. The terminal state machine serializes this with PTY last-close
    // and only the winner signals the foreground process group.
    let session = current().as_thread().proc_data.proc.group().session();
    vhangup_controlling_session(&session);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::{cell::Cell, mem::MaybeUninit, time::Duration};

    use axfs_ng_vfs::{Metadata, Mountpoint};
    use tk_linux_cred::{FsCredentialSnapshot, GroupInfo, Kgid, Kuid};
    use tk_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext};

    use super::*;
    use crate::task::DacCredentialView;

    #[test]
    fn legacy_futimesat_rejects_invalid_microseconds_before_conversion() {
        let valid = [
            __kernel_old_timeval {
                tv_sec: 7,
                tv_usec: 999_999,
            },
            __kernel_old_timeval {
                tv_sec: 9,
                tv_usec: 0,
            },
        ];
        assert_eq!(
            legacy_futimesat_pair(valid),
            Ok((Timestamp::new(7, 999_999_000), Timestamp::new(9, 0)))
        );

        for usec in [-1, 1_000_000] {
            let invalid = [
                __kernel_old_timeval {
                    tv_sec: 0,
                    tv_usec: usec,
                },
                valid[1],
            ];
            assert_eq!(legacy_futimesat_pair(invalid), Err(AxError::InvalidInput));
        }

        let negative_seconds = [
            __kernel_old_timeval {
                tv_sec: -1,
                tv_usec: 0,
            },
            valid[1],
        ];
        assert!(legacy_futimesat_pair(negative_seconds).is_ok());
    }

    struct GetdentsMemory<'a> {
        iterating: &'a Cell<bool>,
        bytes: Vec<u8>,
        calls: usize,
        fail_call: Option<usize>,
    }

    // SAFETY: The fixture accesses only its owned byte vector after checking
    // the requested range, and never dereferences a userspace address.
    unsafe impl UserMemory for GetdentsMemory<'_> {
        fn read(
            &mut self,
            _start: usize,
            _dst: &mut [MaybeUninit<u8>],
        ) -> Result<(), UserCopyError> {
            Err(UserCopyError::BadAddress)
        }

        fn write(&mut self, start: usize, src: &[u8]) -> Result<(), UserCopyError> {
            self.calls += 1;
            // Usercopy may fault a file-backed mapping, requiring the same
            // filesystem lock held by the directory iterator.
            if self.iterating.get() || self.fail_call == Some(self.calls) {
                return Err(UserCopyError::BadAddress);
            }
            let end = start
                .checked_add(src.len())
                .ok_or(UserCopyError::BadAddress)?;
            self.bytes
                .get_mut(start..end)
                .ok_or(UserCopyError::BadAddress)?
                .copy_from_slice(src);
            Ok(())
        }
    }

    fn getdents_test_directory(
        iterating: &Cell<bool>,
        offset: u64,
        entries: u64,
        sink: &mut dyn axfs_ng_vfs::DirEntrySink,
    ) -> AxResult<usize> {
        iterating.set(true);
        let mut accepted = 0;
        for entry in offset..entries {
            if !sink.accept(
                FsName::new(b"entry"),
                entry + 1,
                NodeType::RegularFile,
                entry + 1,
            ) {
                break;
            }
            accepted += 1;
        }
        iterating.set(false);
        Ok(accepted)
    }

    #[test]
    fn getdents_copyout_runs_after_filesystem_iteration_unlocks() {
        for format in [DirentFormat::Legacy, DirentFormat::Dirent64] {
            let iterating = Cell::new(false);
            let mut provider = GetdentsMemory {
                iterating: &iterating,
                bytes: vec![0; 4096],
                calls: 0,
                fail_call: None,
            };
            let mut memory = UserMemoryContext::new(&mut provider);
            let mut offset = 0;
            let result = copy_dirents(
                &mut memory,
                core::ptr::null_mut(),
                4096,
                format,
                &mut offset,
                |offset, sink| getdents_test_directory(&iterating, offset, 40, sink),
                || false,
            );
            let reclen = dirent_record_len(format, b"entry").unwrap();
            assert_eq!(result, Ok((40 * reclen) as isize));
            assert_eq!(offset, 40);
            assert_eq!(
                u64::from_ne_bytes(provider.bytes[8..16].try_into().unwrap()),
                1
            );
            assert_eq!(
                u64::from_ne_bytes(
                    provider.bytes[39 * reclen + 8..39 * reclen + 16]
                        .try_into()
                        .unwrap()
                ),
                40
            );
        }
    }

    #[test]
    fn getdents_staged_copy_preserves_partial_fault_and_cookie_semantics() {
        for format in [DirentFormat::Legacy, DirentFormat::Dirent64] {
            for (fail_call, count, signal, expected, expected_offset) in [
                (Some(1), 4096, false, Err(AxError::BadAddress), 0),
                (Some(2), 4096, false, Ok(32), 1),
                (Some(4), 4096, false, Err(AxError::BadAddress), 3),
                (None, 31, false, Err(AxError::InvalidInput), 0),
                (None, 64, false, Ok(64), 2),
                (None, 4096, true, Ok(32), 1),
            ] {
                let iterating = Cell::new(false);
                let mut provider = GetdentsMemory {
                    iterating: &iterating,
                    bytes: vec![0; 4096],
                    calls: 0,
                    fail_call,
                };
                let mut memory = UserMemoryContext::new(&mut provider);
                let mut offset = 0;
                let result = copy_dirents(
                    &mut memory,
                    core::ptr::null_mut(),
                    count,
                    format,
                    &mut offset,
                    |offset, sink| getdents_test_directory(&iterating, offset, 3, sink),
                    || signal,
                );
                assert_eq!(result, expected);
                assert_eq!(offset, expected_offset);
            }
        }
    }

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

    struct SysfsMemory {
        bytes: Vec<u8>,
        readable: bool,
        writable: bool,
    }

    impl SysfsMemory {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                readable: true,
                writable: true,
            }
        }
    }

    unsafe impl UserMemory for SysfsMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> Result<(), UserCopyError> {
            if !self.readable {
                return Err(UserCopyError::BadAddress);
            }
            let end = start
                .checked_add(dst.len())
                .ok_or(UserCopyError::BadAddress)?;
            let source = self
                .bytes
                .get(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            for (destination, source) in dst.iter_mut().zip(source) {
                destination.write(*source);
            }
            Ok(())
        }

        fn write(&mut self, start: usize, src: &[u8]) -> Result<(), UserCopyError> {
            if !self.writable {
                return Err(UserCopyError::BadAddress);
            }
            let end = start
                .checked_add(src.len())
                .ok_or(UserCopyError::BadAddress)?;
            let destination = self
                .bytes
                .get_mut(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            destination.copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn sysfs_decodes_option_as_a_low_32_bit_signed_int_before_usercopy() {
        let mut provider = NoUserMemory;
        let mut memory = UserMemoryContext::new(&mut provider);

        assert_eq!(
            sys_sysfs(
                &mut memory,
                (1usize << 32) | u32::MAX as usize,
                usize::MAX as *const c_char,
                usize::MAX as *mut c_char,
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            sys_sysfs(
                &mut memory,
                4,
                usize::MAX as *const c_char,
                usize::MAX as *mut c_char,
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn sysfs_option_one_uses_raw_bounded_bytes() {
        let mut provider = SysfsMemory::new(vec![0; SYSFS_NAME_PATH_MAX + 1]);
        provider.bytes[..5].copy_from_slice(b"vfat\0");
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_sysfs(&mut memory, 1, core::ptr::null(), core::ptr::null_mut()),
            Ok(1)
        );

        memory.memory_mut().bytes[..2].copy_from_slice(&[0xff, 0]);
        assert_eq!(
            sys_sysfs(&mut memory, 1, core::ptr::null(), core::ptr::null_mut()),
            Err(AxError::InvalidInput)
        );

        memory.memory_mut().bytes[0] = 0;
        assert_eq!(
            sys_sysfs(&mut memory, 1, core::ptr::null(), core::ptr::null_mut()),
            Err(LinuxError::ENOENT.into())
        );

        memory.memory_mut().readable = false;
        assert_eq!(
            sys_sysfs(&mut memory, 1, core::ptr::null(), core::ptr::null_mut()),
            Err(AxError::BadAddress)
        );
        memory.memory_mut().readable = true;

        memory.memory_mut().bytes[..SYSFS_NAME_PATH_MAX].fill(b'x');
        assert_eq!(
            sys_sysfs(&mut memory, 1, core::ptr::null(), core::ptr::null_mut()),
            Err(AxError::NameTooLong)
        );
    }

    #[test]
    fn sysfs_option_two_writes_nul_and_validates_index_before_destination() {
        let mut provider = SysfsMemory::new(vec![0; 64]);
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_sysfs(
                &mut memory,
                2,
                ((1usize << 32) | 1) as *const c_char,
                16usize as *mut c_char,
            ),
            Ok(0)
        );
        assert_eq!(&memory.memory_mut().bytes[16..21], b"vfat\0");

        memory.memory_mut().writable = false;
        assert_eq!(
            sys_sysfs(
                &mut memory,
                2,
                usize::MAX as *const c_char,
                core::ptr::null_mut(),
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            sys_sysfs(&mut memory, 2, core::ptr::null(), core::ptr::null_mut(),),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn sysfs_option_three_ignores_both_pointers() {
        let mut provider = NoUserMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_sysfs(
                &mut memory,
                3,
                usize::MAX as *const c_char,
                usize::MAX as *mut c_char,
            ),
            Ok(filesystem_type_catalog().len() as isize)
        );
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
        let _context = crate::test_support::scheduler_test_context();
        let security = linkat_test_security();
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        let target = root
            .create(
                axfs_ng_vfs::FsName::new(b"target"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let symlink = root
            .create_symlink(
                axfs_ng_vfs::FsName::new(b"jump"),
                axfs_ng_vfs::FsPath::new(b"target"),
                NodePermission::from_bits_truncate(0o777),
                Some((0, 0)),
            )
            .unwrap();
        let context = axfs::FsContext::new(root.clone());

        let no_follow = resolve_hardlink_source_in_fs(
            &context,
            axfs_ng_vfs::FsPath::new(b"jump"),
            false,
            &security,
        )
        .unwrap();
        let followed = resolve_hardlink_source_in_fs(
            &context,
            axfs_ng_vfs::FsPath::new(b"jump"),
            true,
            &security,
        )
        .unwrap();
        assert!(no_follow.same_node(&symlink));
        assert!(followed.same_node(&target));

        let left = root
            .create(
                axfs_ng_vfs::FsName::new(b"left"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let right = root
            .create(
                axfs_ng_vfs::FsName::new(b"right"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let left_source = left
            .create(
                axfs_ng_vfs::FsName::new(b"source"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let right_source = right
            .create(
                axfs_ng_vfs::FsName::new(b"source"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let left_context = context.with_current_dir(left).unwrap();
        let pinned_context = left_context.with_current_dir(right).unwrap();
        let resolved = resolve_hardlink_source_in_fs(
            &pinned_context,
            axfs_ng_vfs::FsPath::new(b"source"),
            false,
            &security,
        )
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
                axfs_ng_vfs::FsName::new(b"file"),
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
                axfs_ng_vfs::FsName::new(b"directory"),
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
                axfs_ng_vfs::FsName::new(b"fifo"),
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
        let _context = crate::test_support::scheduler_test_context();
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        root.create(
            axfs_ng_vfs::FsName::new(b"existing-file"),
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        )
        .unwrap();
        root.create(
            axfs_ng_vfs::FsName::new(b"existing-dir"),
            NodeType::Directory,
            NodePermission::from_bits_truncate(0o700),
        )
        .unwrap();
        let context = axfs::FsContext::new(root.clone());
        let security = linkat_test_security();

        let (parent, name) = context
            .resolve_named_create_security(
                axfs_ng_vfs::FsPath::new(b"missing"),
                &security,
                NamedCreateTerminalType::NonDirectory,
            )
            .unwrap();
        assert!(parent.same_node(&root));
        assert_eq!(name.as_bytes(), b"missing");
        let (parent, name) = context
            .resolve_named_create_security(
                axfs_ng_vfs::FsPath::new(b"missing/"),
                &security,
                NamedCreateTerminalType::Directory,
            )
            .unwrap();
        assert!(parent.same_node(&root));
        assert_eq!(name.as_bytes(), b"missing");
        assert!(matches!(
            context.resolve_named_create_security(
                axfs_ng_vfs::FsPath::new(b"missing/"),
                &security,
                NamedCreateTerminalType::NonDirectory,
            ),
            Err(AxError::NotFound)
        ));
        for path in ["existing-file/", "existing-dir/"] {
            assert!(matches!(
                context.resolve_named_create_security(
                    axfs_ng_vfs::FsPath::new(path.as_bytes()),
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
                        axfs_ng_vfs::FsPath::new(path.as_bytes()),
                        &security,
                        terminal_type,
                    ),
                    Err(AxError::AlreadyExists)
                ));
            }
        }

        assert!(matches!(
            context.resolve_named_create_security(
                axfs_ng_vfs::FsPath::new(b"missing/."),
                &security,
                NamedCreateTerminalType::Directory,
            ),
            Err(AxError::NotFound)
        ));
        assert!(matches!(
            context.resolve_named_create_security(
                axfs_ng_vfs::FsPath::new(b"existing-file/."),
                &security,
                NamedCreateTerminalType::Directory,
            ),
            Err(AxError::NotADirectory)
        ));
        assert!(matches!(
            root.lookup_no_follow_in_mount(axfs_ng_vfs::FsName::new(b"missing")),
            Err(AxError::NotFound)
        ));
        assert!(matches!(
            context.resolve_named_create_security(
                axfs_ng_vfs::FsPath::new(b"existing-dir/."),
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
        axfs_ng_vfs::FsPath::new(path.as_bytes())
            .split_final_component()
            .unwrap()
            .1
    }

    #[test]
    fn unlinkat_preserves_destructive_final_component_syntax() {
        assert_eq!(
            unlinkat_final_name(final_component("file"), false),
            Ok(UnlinkFinalName {
                name: axfs_ng_vfs::FsName::new(b"file"),
                requires_directory: false,
            })
        );
        assert_eq!(
            unlinkat_final_name(final_component("file/"), false),
            Ok(UnlinkFinalName {
                name: axfs_ng_vfs::FsName::new(b"file"),
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
                name: axfs_ng_vfs::FsName::new(b"entry"),
                requires_directory: false,
            })
        );
        assert_eq!(
            renameat_final_name(final_component("entry/"), AxError::ResourceBusy),
            Ok(RenameFinalName {
                name: axfs_ng_vfs::FsName::new(b"entry"),
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
        let _context = crate::test_support::scheduler_test_context();
        let filesystem = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        let file = root
            .create(
                axfs_ng_vfs::FsName::new(b"file"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            )
            .unwrap();
        let directory = root
            .create(
                axfs_ng_vfs::FsName::new(b"directory"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let symlink = root
            .create_symlink(
                axfs_ng_vfs::FsName::new(b"symlink"),
                axfs_ng_vfs::FsPath::new(b"file"),
                NodePermission::from_bits_truncate(0o777),
                Some((0, 0)),
            )
            .unwrap();
        let context = axfs::FsContext::new(root.clone());
        let security = linkat_test_security();

        let (parent, name, target) = resolve_unlink_target_in_fs(
            &context,
            axfs_ng_vfs::FsPath::new(b"file"),
            false,
            &security,
        )
        .unwrap();
        assert!(parent.same_node(&root));
        assert_eq!(name.as_bytes(), b"file");
        assert!(target.same_node(&file));

        for path in ["file/", "symlink/", "file/."] {
            assert!(matches!(
                resolve_unlink_target_in_fs(
                    &context,
                    axfs_ng_vfs::FsPath::new(path.as_bytes()),
                    false,
                    &security
                ),
                Err(AxError::NotADirectory)
            ));
        }
        for (path, expected) in [("file/", &file), ("symlink/", &symlink)] {
            let (_, _, target) = resolve_unlink_target_in_fs(
                &context,
                axfs_ng_vfs::FsPath::new(path.as_bytes()),
                true,
                &security,
            )
            .unwrap();
            assert!(target.same_node(expected));
        }
        for path in ["directory/", "directory/."] {
            assert!(matches!(
                resolve_unlink_target_in_fs(
                    &context,
                    axfs_ng_vfs::FsPath::new(path.as_bytes()),
                    false,
                    &security
                ),
                Err(AxError::IsADirectory)
            ));
        }
        assert!(matches!(
            resolve_unlink_target_in_fs(
                &context,
                axfs_ng_vfs::FsPath::new(b"directory/."),
                true,
                &security,
            ),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            resolve_unlink_target_in_fs(
                &context,
                axfs_ng_vfs::FsPath::new(b"directory/.."),
                true,
                &security,
            ),
            Err(AxError::DirectoryNotEmpty)
        ));
        assert!(matches!(
            resolve_unlink_target_in_fs(
                &context,
                axfs_ng_vfs::FsPath::new(b"///"),
                false,
                &security
            ),
            Err(AxError::IsADirectory)
        ));
        assert!(matches!(
            resolve_unlink_target_in_fs(
                &context,
                axfs_ng_vfs::FsPath::new(b"///"),
                true,
                &security
            ),
            Err(AxError::ResourceBusy)
        ));

        assert!(
            root.lookup_no_follow(axfs_ng_vfs::FsName::new(b"file"))
                .unwrap()
                .same_node(&file)
        );
        assert!(
            root.lookup_no_follow(axfs_ng_vfs::FsName::new(b"directory"))
                .unwrap()
                .same_node(&directory)
        );
        assert!(
            root.lookup_no_follow(axfs_ng_vfs::FsName::new(b"symlink"))
                .unwrap()
                .same_node(&symlink)
        );
    }

    #[test]
    fn symlink_target_uses_linux_empty_and_path_max_rules_without_name_max() {
        assert_eq!(
            validate_symlink_target(axfs_ng_vfs::FsPath::new(b"")),
            Err(AxError::NotFound)
        );
        assert_eq!(
            validate_symlink_target(axfs_ng_vfs::FsPath::new(b"target")),
            Ok(())
        );
        assert_eq!(
            validate_symlink_target(axfs_ng_vfs::FsPath::new("a".repeat(255 + 1).as_bytes())),
            Ok(())
        );
        assert_eq!(
            validate_symlink_target(axfs_ng_vfs::FsPath::new("a".repeat(4095).as_bytes())),
            Ok(())
        );
        assert_eq!(
            validate_symlink_target(axfs_ng_vfs::FsPath::new("a".repeat(4096).as_bytes())),
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
            project_id: 0,
            size: 0,
            block_size: 4096,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: Timestamp::ZERO,
            btime: Timestamp::ZERO,
            mtime: Timestamp::ZERO,
            ctime: Timestamp::ZERO,
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
            ctime.into(),
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
        Ok(prepare_chmod_metadata_setattr_for_test(
            metadata,
            requested_mode,
            credentials,
            ctime.into(),
        )?
        .into_parts()
        .0)
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
        assert_eq!(update.ctime, Some(Duration::from_secs(7).into()));
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
                axfs_ng_vfs::FsName::new(b"chown-killpriv"),
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
                axfs_ng_vfs::FsName::new(b"chown-killpriv-backend-failure"),
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
            Duration::from_secs(4).into(),
        )
        .unwrap()
        .into_parts();
        assert_eq!(update.owner, Some((2000, 3000)));
        assert_eq!(update.mode.unwrap().bits(), 0o755);
        assert_eq!(update.ctime, Some(Duration::from_secs(4).into()));
        assert_eq!(committed.mode.bits(), 0o755);
        assert_eq!((committed.uid, committed.gid), (2000, 3000));
        assert_eq!(committed.atime, old.atime);
        assert_eq!(committed.mtime, old.mtime);
        assert_eq!(committed.ctime, Duration::from_secs(4).into());
        assert_eq!(committed.inode, old.inode);
    }
}
