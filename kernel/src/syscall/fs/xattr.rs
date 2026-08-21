use alloc::{string::String, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, path::Path};
use axtask::current;
use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use thekernel_linux_usercopy::{
    UserCopyError, UserMemory, UserMemoryContext, vm_load, vm_load_until_nul,
    vm_load_until_nul_bounded, vm_write_slice,
};

use super::ctl::validate_pathname;
use crate::{
    file::{
        Directory, File, FileLike, ResolveAtResult, get_file_like,
        permission::VfsSecurityContext,
        pipe::NamedPipe,
        resolve_at_with_security,
        xattr_provider::{
            XATTR_SIZE_MAX, get_xattr_with_security, list_xattrs_with_security,
            remove_xattr_with_security, set_xattr_with_security,
        },
    },
    mm::map_usercopy_error,
    task::{AsThread, XATTR_NAME_MAX, security::XattrSetFlags},
};

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

fn resolve_xattr_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    no_follow: bool,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    let path = String::from_utf8(
        vm_load_until_nul(memory, path.cast::<u8>()).map_err(map_usercopy_error)?,
    )
    .map_err(|_| AxError::IllegalBytes)?;
    let path_ref = Path::new(&path);
    validate_pathname(path_ref)?;

    match resolve_at_with_security(
        AT_FDCWD,
        Some(path.as_str()),
        if no_follow { AT_SYMLINK_NOFOLLOW } else { 0 },
        security,
    )? {
        ResolveAtResult::File(loc) => Ok(loc),
        ResolveAtResult::Other(_) => Err(AxError::InvalidInput),
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

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axfs::{FileBackend, FileFlags};
    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};
    use linux_raw_sys::general::O_PATH;

    use super::*;
    use crate::{file::FileDescription, pseudofs::tmp::MemoryFs};

    #[test]
    fn xattr_flags_accept_only_linux_set_modes() {
        assert_eq!(validate_xattr_flags(0), Ok(XattrSetFlags::NONE));
        assert_eq!(validate_xattr_flags(1), Ok(XattrSetFlags::CREATE));
        assert_eq!(validate_xattr_flags(2), Ok(XattrSetFlags::REPLACE));
        assert_eq!(validate_xattr_flags(3), Err(AxError::InvalidInput));
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
