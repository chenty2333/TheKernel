use alloc::vec::Vec;
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, path::FsPath};
use axtask::current;
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use linux_vfs::{
    LinuxVfsError, StructCopyPlan, XattrArgs, getxattrat_copy_plan, setxattrat_copy_plan,
    validate_file_at_flags, validate_getxattr_flags, validate_setxattr_flags,
};
use thekernel_linux_usercopy::{
    UserCopyError, UserMemory, UserMemoryContext, vm_load, vm_load_until_nul_bounded,
    vm_write_slice,
};

use crate::{
    file::{
        Directory, File, FileLike, ResolveAtResult, get_file_description, get_file_like,
        permission::VfsSecurityContext,
        pipe::NamedPipe,
        resolve_at_with_security, validate_pathname,
        xattr_provider::{
            XATTR_SIZE_MAX, get_xattr_with_security, list_xattrs_with_security,
            remove_xattr_with_security, set_xattr_with_security,
        },
    },
    mm::{copy_struct_from_user, map_usercopy_error},
    task::{AsThread, XATTR_NAME_MAX, security::XattrSetFlags},
};

const PATH_MAX: usize = 4096;

fn map_linux_vfs_error(error: LinuxVfsError) -> AxError {
    match error {
        LinuxVfsError::StructTooSmall | LinuxVfsError::InvalidFlags => LinuxError::EINVAL.into(),
        LinuxVfsError::StructTooLarge | LinuxVfsError::XattrTooLarge => LinuxError::E2BIG.into(),
        _ => AxError::InvalidInput,
    }
}

fn require_xattr_args_plan(
    plan: Result<StructCopyPlan, LinuxVfsError>,
) -> AxResult<StructCopyPlan> {
    plan.map_err(map_linux_vfs_error)
}

fn validate_xattr_name(name: &[u8]) -> AxResult<()> {
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(LinuxError::ERANGE.into());
    }
    Ok(())
}

fn validate_xattr_flags(flags: u32) -> AxResult<XattrSetFlags> {
    XattrSetFlags::try_from_bits(flags).ok_or(AxError::InvalidInput)
}

fn map_xattr_name_load_error(error: UserCopyError) -> AxError {
    match error {
        UserCopyError::TooLong => LinuxError::ERANGE.into(),
        other => map_usercopy_error(other),
    }
}

fn read_xattr_name<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const c_char,
) -> AxResult<Vec<u8>> {
    let name = vm_load_until_nul_bounded(memory, name.cast::<u8>(), XATTR_NAME_MAX + 1)
        .map_err(map_xattr_name_load_error)?;
    validate_xattr_name(&name)?;
    Ok(name)
}

fn read_xattr_value<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    value: *const u8,
    size: usize,
) -> AxResult<Vec<u8>> {
    if size > XATTR_SIZE_MAX {
        return Err(LinuxError::E2BIG.into());
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    vm_load(memory, value, size).map_err(map_usercopy_error)
}

fn current_vfs_security_context() -> VfsSecurityContext {
    VfsSecurityContext::new(current().as_thread().current_cred())
}

/// `AT_EMPTY_PATH` operates on the OFD itself.  If that descriptor survived
/// setns, DAC/idmap must use its opening topology rather than the caller's
/// current namespace topology.
fn empty_path_vfs_security(
    dfd: i32,
) -> AxResult<(
    VfsSecurityContext,
    Option<alloc::sync::Arc<crate::file::FileDescription>>,
)> {
    let snapshot = current().as_thread().namespace_credential_fs_snapshot();
    let description = if dfd == AT_FDCWD {
        None
    } else {
        Some(get_file_description(dfd)?)
    };
    let topology = description
        .as_ref()
        .and_then(|description| description.vfs_mount_topology())
        .unwrap_or_else(|| snapshot.mount_topology.clone());
    Ok((
        VfsSecurityContext::with_execution_authority(
            snapshot.credential,
            topology,
            snapshot.landlock_domain,
        ),
        description,
    ))
}

fn resolve_xattr_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    no_follow: bool,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    let path = vm_load_until_nul_bounded(memory, path.cast::<u8>(), PATH_MAX)
        .map_err(map_usercopy_error)?;
    let path_ref = FsPath::new(&path);
    validate_pathname(path_ref)?;

    match resolve_at_with_security(
        AT_FDCWD,
        Some(path_ref),
        if no_follow { AT_SYMLINK_NOFOLLOW } else { 0 },
        security,
    )? {
        ResolveAtResult::File(loc) => Ok(loc),
        ResolveAtResult::Other(_) => Err(AxError::InvalidInput),
    }
}

/// Imports a pathname as opaque filesystem bytes.  Unlike the legacy xattr
/// entry points above, the v6.18 `*xattrat` interface must never reinterpret a
/// valid ext4/tmpfs name as UTF-8 before VFS lookup.
fn read_xattr_at_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<Vec<u8>> {
    let bytes = vm_load_until_nul_bounded(memory, path.cast::<u8>(), PATH_MAX)
        .map_err(map_usercopy_error)?;
    if bytes
        .split(|byte| *byte == b'/')
        .any(|name| name.len() > 255)
    {
        return Err(AxError::NameTooLong);
    }
    Ok(bytes)
}

/// Resolves the raw pathname arm of the v6.18 xattr-at syscalls, including
/// `AT_EMPTY_PATH` on an ordinary or `O_PATH` metadata descriptor.
///
/// `resolve_at_with_security` is being migrated with axfs-ng-vfs to accept
/// `FsPath`; retaining the byte view through this boundary is intentional.
fn resolve_xattr_at(
    dfd: i32,
    path: Option<&FsPath>,
    at_flags: u32,
    security: &VfsSecurityContext,
) -> AxResult<ResolveAtResult> {
    validate_file_at_flags(at_flags).map_err(map_linux_vfs_error)?;
    resolve_at_with_security(
        dfd,
        path.filter(|path| !path.as_bytes().is_empty()),
        at_flags,
        security,
    )
}

fn xattr_location_from_at<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    at_flags: u32,
) -> AxResult<(VfsSecurityContext, Location)> {
    // Import the pathname before selecting an OFD-backed authority: Linux
    // reports pathname usercopy faults before it attempts the empty-path fd.
    let bytes = if path.is_null() {
        if at_flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::BadAddress);
        }
        None
    } else {
        Some(read_xattr_at_path(memory, path)?)
    };
    let empty = bytes.as_ref().is_none_or(Vec::is_empty);
    let (security, description) = if empty && at_flags & AT_EMPTY_PATH != 0 {
        let (security, description) = empty_path_vfs_security(dfd)?;
        (security, description)
    } else {
        (current_vfs_security_context(), None)
    };
    if let Some(description) = description {
        let file = description.file_handle();
        return xattr_location(&*file)
            .map(|location| (security, location))
            .ok_or(LinuxError::EOPNOTSUPP.into());
    }
    match resolve_xattr_at(dfd, bytes.as_deref().map(FsPath::new), at_flags, &security)? {
        ResolveAtResult::File(location) => Ok((security, location)),
        ResolveAtResult::Other(_) => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

fn xattr_location(file_like: &dyn FileLike) -> Option<Location> {
    if let Some(file) = file_like.downcast_ref::<File>() {
        Some(file.inner().location().clone())
    } else if let Some(directory) = file_like.downcast_ref::<Directory>() {
        Some(directory.inner().clone())
    } else {
        file_like
            .downcast_ref::<NamedPipe>()
            .map(|pipe| pipe.location().clone())
    }
}

fn resolve_xattr_fd(fd: i32) -> AxResult<ResolveAtResult> {
    let file_like = get_file_like(fd)?;
    // f*xattr operates on inode metadata. An O_PATH description is therefore
    // a valid pinned target and must not pass through the ordinary I/O gate.
    Ok(match xattr_location(&*file_like) {
        Some(location) => ResolveAtResult::File(location),
        None => ResolveAtResult::Other(file_like),
    })
}

fn write_xattr_value<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    size: usize,
    value: &[u8],
) -> AxResult<isize> {
    if size == 0 {
        return Ok(value.len() as isize);
    }
    if size < value.len() {
        return Err(LinuxError::ERANGE.into());
    }
    vm_write_slice(memory, buf, value).map_err(map_usercopy_error)?;
    Ok(value.len() as isize)
}

fn write_xattr_list<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut c_char,
    size: usize,
    value: &[u8],
) -> AxResult<isize> {
    if size == 0 {
        return Ok(value.len() as isize);
    }
    if size < value.len() {
        return Err(LinuxError::ERANGE.into());
    }
    vm_write_slice(memory, buf.cast::<u8>(), value).map_err(map_usercopy_error)?;
    Ok(value.len() as isize)
}

fn xattr_set_by_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
    no_follow: bool,
) -> AxResult<isize> {
    let flags = validate_xattr_flags(flags)?;
    let name = read_xattr_name(memory, name)?;
    let value = read_xattr_value(memory, value, size)?;
    let loc = resolve_xattr_path(memory, path, no_follow, security)?;
    set_xattr_with_security(security, &loc, &name, &value, flags)?;
    Ok(0)
}

fn xattr_set_by_fd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    fd: i32,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    let flags = validate_xattr_flags(flags)?;
    let name = read_xattr_name(memory, name)?;
    let value = read_xattr_value(memory, value, size)?;

    match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => {
            set_xattr_with_security(security, &loc, &name, &value, flags)?;
            Ok(0)
        }
        ResolveAtResult::Other(_) => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

fn xattr_get_by_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
    no_follow: bool,
) -> AxResult<isize> {
    let name = read_xattr_name(memory, name)?;
    let loc = resolve_xattr_path(memory, path, no_follow, security)?;
    let value_bytes = get_xattr_with_security(security, &loc, &name)?;
    write_xattr_value(memory, value, size, &value_bytes)
}

fn xattr_get_by_fd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    fd: i32,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    let name = read_xattr_name(memory, name)?;
    let value_bytes = match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => get_xattr_with_security(security, &loc, &name)?,
        ResolveAtResult::Other(_) => return Err(LinuxError::EOPNOTSUPP.into()),
    };
    write_xattr_value(memory, value, size, &value_bytes)
}

fn xattr_list_by_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    path: *const c_char,
    list: *mut c_char,
    size: usize,
    no_follow: bool,
) -> AxResult<isize> {
    let loc = resolve_xattr_path(memory, path, no_follow, security)?;
    let names = list_xattrs_with_security(security, &loc)?;
    write_xattr_list(memory, list, size, &names)
}

fn xattr_list_by_fd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    fd: i32,
    list: *mut c_char,
    size: usize,
) -> AxResult<isize> {
    let names = match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => list_xattrs_with_security(security, &loc)?,
        ResolveAtResult::Other(_) => return Err(LinuxError::EOPNOTSUPP.into()),
    };
    write_xattr_list(memory, list, size, &names)
}

fn xattr_remove_by_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    path: *const c_char,
    name: *const c_char,
    no_follow: bool,
) -> AxResult<isize> {
    let name = read_xattr_name(memory, name)?;
    let loc = resolve_xattr_path(memory, path, no_follow, security)?;
    remove_xattr_with_security(security, &loc, &name)?;
    Ok(0)
}

fn xattr_remove_by_fd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    security: &VfsSecurityContext,
    fd: i32,
    name: *const c_char,
) -> AxResult<isize> {
    let name = read_xattr_name(memory, name)?;
    match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => {
            remove_xattr_with_security(security, &loc, &name)?;
            Ok(0)
        }
        ResolveAtResult::Other(_) => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

pub fn sys_setxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_set_by_path(memory, &security, path, name, value, size, flags, false)
}

pub fn sys_lsetxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_set_by_path(memory, &security, path, name, value, size, flags, true)
}

pub fn sys_fsetxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_set_by_fd(memory, &security, fd, name, value, size, flags)
}

pub fn sys_getxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_get_by_path(memory, &security, path, name, value, size, false)
}

pub fn sys_lgetxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_get_by_path(memory, &security, path, name, value, size, true)
}

pub fn sys_fgetxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_get_by_fd(memory, &security, fd, name, value, size)
}

pub fn sys_listxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    list: *mut c_char,
    size: usize,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_list_by_path(memory, &security, path, list, size, false)
}

pub fn sys_llistxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    list: *mut c_char,
    size: usize,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_list_by_path(memory, &security, path, list, size, true)
}

pub fn sys_flistxattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    list: *mut c_char,
    size: usize,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_list_by_fd(memory, &security, fd, list, size)
}

pub fn sys_removexattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    name: *const c_char,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_remove_by_path(memory, &security, path, name, false)
}

pub fn sys_lremovexattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    name: *const c_char,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_remove_by_path(memory, &security, path, name, true)
}

pub fn sys_fremovexattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    name: *const c_char,
) -> AxResult<isize> {
    let security = current_vfs_security_context();
    xattr_remove_by_fd(memory, &security, fd, name)
}

/// Linux v6.18 `setxattrat(2)` (syscall 463).
pub fn sys_setxattrat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    at_flags: u32,
    name: *const c_char,
    uargs: *const XattrArgs,
    usize: usize,
) -> AxResult<isize> {
    let plan = require_xattr_args_plan(setxattrat_copy_plan(usize))?;
    let args: XattrArgs = copy_struct_from_user(memory, uargs.cast(), plan.user_size)?;

    // path_setxattrat() checks the AT mask before importing the xattr name
    // and value.  Preserve that ordering after the outer structure copy.
    validate_file_at_flags(at_flags).map_err(map_linux_vfs_error)?;
    validate_setxattr_flags(args.flags).map_err(map_linux_vfs_error)?;
    let flags = validate_xattr_flags(args.flags)?;
    let name = read_xattr_name(memory, name)?;
    let value = read_xattr_value(memory, args.value as *const u8, args.size as usize)?;
    let (security, location) = xattr_location_from_at(memory, dfd, path, at_flags)?;
    set_xattr_with_security(&security, &location, &name, &value, flags)?;
    Ok(0)
}

/// Linux v6.18 `getxattrat(2)` (syscall 464).
pub fn sys_getxattrat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    at_flags: u32,
    name: *const c_char,
    uargs: *const XattrArgs,
    usize: usize,
) -> AxResult<isize> {
    let plan = require_xattr_args_plan(getxattrat_copy_plan(usize))?;
    let args: XattrArgs = copy_struct_from_user(memory, uargs.cast(), plan.user_size)?;
    validate_getxattr_flags(args.flags).map_err(map_linux_vfs_error)?;

    // path_getxattrat() validates the AT mask before importing the xattr
    // name, so a bad AT flag takes precedence over a bad name pointer.
    validate_file_at_flags(at_flags).map_err(map_linux_vfs_error)?;
    let name = read_xattr_name(memory, name)?;
    let (security, location) = xattr_location_from_at(memory, dfd, path, at_flags)?;
    let value = get_xattr_with_security(&security, &location, &name)?;
    write_xattr_value(memory, args.value as *mut u8, args.size as usize, &value)
}

/// Linux v6.18 `listxattrat(2)` (syscall 465).
pub fn sys_listxattrat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    at_flags: u32,
    list: *mut c_char,
    size: usize,
) -> AxResult<isize> {
    validate_file_at_flags(at_flags).map_err(map_linux_vfs_error)?;
    let (security, location) = xattr_location_from_at(memory, dfd, path, at_flags)?;
    let names = list_xattrs_with_security(&security, &location)?;
    write_xattr_list(memory, list, size, &names)
}

/// Linux v6.18 `removexattrat(2)` (syscall 466).
pub fn sys_removexattrat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    at_flags: u32,
    name: *const c_char,
) -> AxResult<isize> {
    validate_file_at_flags(at_flags).map_err(map_linux_vfs_error)?;
    let name = read_xattr_name(memory, name)?;
    let (security, location) = xattr_location_from_at(memory, dfd, path, at_flags)?;
    remove_xattr_with_security(&security, &location, &name)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axfs::{FileBackend, FileFlags};
    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};
    use linux_raw_sys::general::O_PATH;

    use super::*;
    use crate::{file::FileDescription, pseudofs::tmp::MemoryFs};

    #[test]
    fn xattr_flags_accept_linux_set_bit_domain() {
        assert_eq!(validate_xattr_flags(0), Ok(XattrSetFlags::NONE));
        assert_eq!(validate_xattr_flags(1), Ok(XattrSetFlags::CREATE));
        assert_eq!(validate_xattr_flags(2), Ok(XattrSetFlags::REPLACE));
        assert_eq!(
            validate_xattr_flags(3),
            Ok(XattrSetFlags::CREATE_AND_REPLACE)
        );
        assert_eq!(validate_xattr_flags(4), Err(AxError::InvalidInput));
    }

    #[test]
    fn xattr_name_import_requires_only_nonempty_bounded_bytes() {
        assert_eq!(validate_xattr_name(b"user.key"), Ok(()));
        assert_eq!(validate_xattr_name(b""), Err(LinuxError::ERANGE.into()));
        assert_eq!(validate_xattr_name(b"user"), Ok(()));
        assert_eq!(validate_xattr_name(b".key"), Ok(()));
        assert_eq!(validate_xattr_name(b"user."), Ok(()));
        assert_eq!(validate_xattr_name(b"user.\xff"), Ok(()));
        assert_eq!(validate_xattr_name(&[0xff; XATTR_NAME_MAX]), Ok(()));
        let oversized = alloc::vec![b'a'; 256];
        assert_eq!(
            validate_xattr_name(&oversized),
            Err(LinuxError::ERANGE.into())
        );
        assert_eq!(
            map_xattr_name_load_error(UserCopyError::TooLong),
            LinuxError::ERANGE.into()
        );
        assert_eq!(
            map_xattr_name_load_error(UserCopyError::BadAddress),
            AxError::BadAddress
        );
    }

    #[test]
    fn fd_xattr_targeting_is_independent_of_opath_io_access() {
        let filesystem = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let location = mount
            .root_location()
            .create(
                "opath-xattr-target",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(location),
            FileFlags::PATH,
        )));
        let description = FileDescription::new_with_flags(file.clone(), O_PATH).unwrap();
        assert_eq!(
            description.check_io_access(),
            Err(AxError::BadFileDescriptor)
        );
        let resolved = xattr_location(file.as_ref()).unwrap();
        assert_eq!(
            resolved.metadata().unwrap().node_type,
            NodeType::RegularFile
        );
    }
}
