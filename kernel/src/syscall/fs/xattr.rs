use alloc::{string::String, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType, path::Path};
use axtask::current;
use hashbrown::HashMap;
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, CAP_SYS_ADMIN};
use starry_vm::vm_write_slice;

use super::ctl::validate_pathname;
use crate::{
    file::{ResolveAtResult, is_path_only_fd, permission::check_writable_mount, resolve_at},
    mm::{UserConstPtr, vm_load_string},
    pseudofs::tmp,
    task::AsThread,
};

const XATTR_CREATE: u32 = 0x1;
const XATTR_REPLACE: u32 = 0x2;
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 65536;

fn validate_xattr_name(name: &str) -> AxResult<()> {
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(LinuxError::ERANGE.into());
    }

    let Some((namespace, key)) = name.split_once('.') else {
        return Err(AxError::InvalidInput);
    };
    if namespace.is_empty() || key.is_empty() {
        return Err(AxError::InvalidInput);
    }

    Ok(())
}

fn validate_xattr_flags(flags: u32) -> AxResult<()> {
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 || flags == (XATTR_CREATE | XATTR_REPLACE) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn read_xattr_name(name: *const c_char) -> AxResult<String> {
    let name = vm_load_string(name)?;
    validate_xattr_name(&name)?;
    Ok(name)
}

fn read_xattr_value(value: *const u8, size: usize) -> AxResult<Vec<u8>> {
    if size > XATTR_SIZE_MAX {
        return Err(LinuxError::E2BIG.into());
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    Ok(UserConstPtr::from(value).get_as_slice(size)?.to_vec())
}

fn resolve_xattr_path(path: *const c_char, no_follow: bool) -> AxResult<Location> {
    let path = vm_load_string(path)?;
    let path_ref = Path::new(&path);
    validate_pathname(path_ref)?;

    match resolve_at(
        AT_FDCWD,
        Some(path.as_str()),
        if no_follow { AT_SYMLINK_NOFOLLOW } else { 0 },
    )? {
        ResolveAtResult::File(loc) => Ok(loc),
        ResolveAtResult::Other(_) => Err(AxError::InvalidInput),
    }
}

fn resolve_xattr_fd(fd: i32) -> AxResult<ResolveAtResult> {
    resolve_at(fd, None, AT_EMPTY_PATH)
}

fn check_fd_xattr_access(fd: i32) -> AxResult<()> {
    if is_path_only_fd(fd)? {
        return Err(AxError::BadFileDescriptor);
    }
    Ok(())
}

fn current_can_access_trusted_xattrs() -> bool {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_ADMIN)
}

fn check_namespace_access(loc: &Location, name: &str, write: bool) -> AxResult<()> {
    let (namespace, _) = name.split_once('.').ok_or(AxError::InvalidInput)?;

    match namespace {
        "trusted" if !current_can_access_trusted_xattrs() => {
            if write {
                return Err(LinuxError::EPERM.into());
            }
            return Err(LinuxError::ENODATA.into());
        }
        "user" if !matches!(loc.node_type(), NodeType::RegularFile | NodeType::Directory) => {
            if write {
                return Err(LinuxError::EPERM.into());
            }
            return Err(LinuxError::ENODATA.into());
        }
        _ => {}
    }
    Ok(())
}

fn list_name_visible(loc: &Location, name: &str, can_access_trusted: bool) -> bool {
    if name.starts_with("trusted.") && !can_access_trusted {
        return false;
    }
    if name.starts_with("user.")
        && !matches!(loc.node_type(), NodeType::RegularFile | NodeType::Directory)
    {
        return false;
    }
    true
}

fn set_map_xattr(
    map: &mut HashMap<String, Vec<u8>>,
    name: &str,
    value: Vec<u8>,
    flags: u32,
) -> AxResult<()> {
    match flags {
        XATTR_CREATE => {
            if map.contains_key(name) {
                return Err(LinuxError::EEXIST.into());
            }
        }
        XATTR_REPLACE => {
            if !map.contains_key(name) {
                return Err(LinuxError::ENODATA.into());
            }
        }
        0 => {}
        _ => return Err(AxError::InvalidInput),
    }

    let current = map.get(name).map_or(0, |old| name.len() + 1 + old.len());
    let used = map
        .iter()
        .map(|(name, value)| name.len() + 1 + value.len())
        .sum::<usize>()
        .saturating_sub(current);
    if used.saturating_add(name.len() + 1 + value.len()) > XATTR_SIZE_MAX {
        return Err(LinuxError::ENOSPC.into());
    }

    if let Some(current) = map.get_mut(name) {
        *current = value;
        return Ok(());
    }
    let mut owned_name = String::new();
    owned_name
        .try_reserve_exact(name.len())
        .map_err(|_| AxError::NoMemory)?;
    owned_name.push_str(name);
    map.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    map.insert(owned_name, value);
    Ok(())
}

fn get_map_xattr(map: &HashMap<String, Vec<u8>>, name: &str) -> AxResult<Vec<u8>> {
    let value = map.get(name).ok_or(LinuxError::ENODATA)?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    result.extend_from_slice(value);
    Ok(result)
}

fn list_map_xattrs(
    loc: &Location,
    map: &HashMap<String, Vec<u8>>,
    can_access_trusted: bool,
) -> AxResult<Vec<u8>> {
    let required = map
        .keys()
        .filter(|name| list_name_visible(loc, name, can_access_trusted))
        .try_fold(0usize, |total, name| {
            total.checked_add(name.len().saturating_add(1))
        })
        .ok_or(AxError::NoMemory)?;
    let mut list = Vec::new();
    list.try_reserve_exact(required)
        .map_err(|_| AxError::NoMemory)?;
    for name in map.keys() {
        if !list_name_visible(loc, name, can_access_trusted) {
            continue;
        }
        list.extend_from_slice(name.as_bytes());
        list.push(0);
    }
    Ok(list)
}

fn set_location_xattr(loc: &Location, name: &str, value: Vec<u8>, flags: u32) -> AxResult<()> {
    check_namespace_access(loc, name, true)?;
    check_writable_mount(loc)?;

    if let Some(store) = tmp::xattr_store(loc) {
        let mut map = store.lock();
        return set_map_xattr(&mut map, name, value, flags);
    }

    Err(LinuxError::EOPNOTSUPP.into())
}

fn get_location_xattr(loc: &Location, name: &str) -> AxResult<Vec<u8>> {
    check_namespace_access(loc, name, false)?;

    if let Some(store) = tmp::xattr_store(loc) {
        let map = store.lock();
        return get_map_xattr(&map, name);
    }

    Err(LinuxError::EOPNOTSUPP.into())
}

fn list_location_xattrs(loc: &Location) -> AxResult<Vec<u8>> {
    let can_access_trusted = current_can_access_trusted_xattrs();

    if let Some(store) = tmp::xattr_store(loc) {
        let map = store.lock();
        return list_map_xattrs(loc, &map, can_access_trusted);
    }

    Err(LinuxError::EOPNOTSUPP.into())
}

fn remove_location_xattr(loc: &Location, name: &str) -> AxResult<()> {
    check_namespace_access(loc, name, true)?;
    check_writable_mount(loc)?;

    if let Some(store) = tmp::xattr_store(loc) {
        let mut map = store.lock();
        if map.remove(name).is_none() {
            return Err(LinuxError::ENODATA.into());
        }
        return Ok(());
    }

    Err(LinuxError::EOPNOTSUPP.into())
}

fn write_xattr_value(buf: *mut u8, size: usize, value: &[u8]) -> AxResult<isize> {
    if size == 0 {
        return Ok(value.len() as isize);
    }
    if size < value.len() {
        return Err(LinuxError::ERANGE.into());
    }
    vm_write_slice(buf, value)?;
    Ok(value.len() as isize)
}

fn write_xattr_list(buf: *mut c_char, size: usize, value: &[u8]) -> AxResult<isize> {
    if size == 0 {
        return Ok(value.len() as isize);
    }
    if size < value.len() {
        return Err(LinuxError::ERANGE.into());
    }
    vm_write_slice(buf.cast::<u8>(), value)?;
    Ok(value.len() as isize)
}

fn xattr_set_by_path(
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
    no_follow: bool,
) -> AxResult<isize> {
    validate_xattr_flags(flags)?;
    let name = read_xattr_name(name)?;
    let value = read_xattr_value(value, size)?;
    let loc = resolve_xattr_path(path, no_follow)?;
    set_location_xattr(&loc, &name, value, flags)?;
    Ok(0)
}

fn xattr_set_by_fd(
    fd: i32,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    check_fd_xattr_access(fd)?;
    validate_xattr_flags(flags)?;
    let name = read_xattr_name(name)?;
    let value = read_xattr_value(value, size)?;

    match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => {
            set_location_xattr(&loc, &name, value, flags)?;
            Ok(0)
        }
        ResolveAtResult::Other(_) => {
            let (namespace, _) = name.split_once('.').ok_or(AxError::InvalidInput)?;
            if namespace == "user" {
                Err(LinuxError::EPERM.into())
            } else {
                Err(LinuxError::EOPNOTSUPP.into())
            }
        }
    }
}

fn xattr_get_by_path(
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
    no_follow: bool,
) -> AxResult<isize> {
    let name = read_xattr_name(name)?;
    let loc = resolve_xattr_path(path, no_follow)?;
    let value_bytes = get_location_xattr(&loc, &name)?;
    write_xattr_value(value, size, &value_bytes)
}

fn xattr_get_by_fd(fd: i32, name: *const c_char, value: *mut u8, size: usize) -> AxResult<isize> {
    check_fd_xattr_access(fd)?;
    let name = read_xattr_name(name)?;
    let value_bytes = match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => get_location_xattr(&loc, &name)?,
        ResolveAtResult::Other(_) => return Err(LinuxError::ENODATA.into()),
    };
    write_xattr_value(value, size, &value_bytes)
}

fn xattr_list_by_path(
    path: *const c_char,
    list: *mut c_char,
    size: usize,
    no_follow: bool,
) -> AxResult<isize> {
    let loc = resolve_xattr_path(path, no_follow)?;
    let names = list_location_xattrs(&loc)?;
    write_xattr_list(list, size, &names)
}

fn xattr_list_by_fd(fd: i32, list: *mut c_char, size: usize) -> AxResult<isize> {
    check_fd_xattr_access(fd)?;
    let names = match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => list_location_xattrs(&loc)?,
        ResolveAtResult::Other(_) => Vec::new(),
    };
    write_xattr_list(list, size, &names)
}

fn xattr_remove_by_path(
    path: *const c_char,
    name: *const c_char,
    no_follow: bool,
) -> AxResult<isize> {
    let name = read_xattr_name(name)?;
    let loc = resolve_xattr_path(path, no_follow)?;
    remove_location_xattr(&loc, &name)?;
    Ok(0)
}

fn xattr_remove_by_fd(fd: i32, name: *const c_char) -> AxResult<isize> {
    check_fd_xattr_access(fd)?;
    let name = read_xattr_name(name)?;
    match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => {
            remove_location_xattr(&loc, &name)?;
            Ok(0)
        }
        ResolveAtResult::Other(_) => Err(LinuxError::ENODATA.into()),
    }
}

pub fn sys_setxattr(
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    xattr_set_by_path(path, name, value, size, flags, false)
}

pub fn sys_lsetxattr(
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    xattr_set_by_path(path, name, value, size, flags, true)
}

pub fn sys_fsetxattr(
    fd: i32,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    xattr_set_by_fd(fd, name, value, size, flags)
}

pub fn sys_getxattr(
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    xattr_get_by_path(path, name, value, size, false)
}

pub fn sys_lgetxattr(
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    xattr_get_by_path(path, name, value, size, true)
}

pub fn sys_fgetxattr(fd: i32, name: *const c_char, value: *mut u8, size: usize) -> AxResult<isize> {
    xattr_get_by_fd(fd, name, value, size)
}

pub fn sys_listxattr(path: *const c_char, list: *mut c_char, size: usize) -> AxResult<isize> {
    xattr_list_by_path(path, list, size, false)
}

pub fn sys_llistxattr(path: *const c_char, list: *mut c_char, size: usize) -> AxResult<isize> {
    xattr_list_by_path(path, list, size, true)
}

pub fn sys_flistxattr(fd: i32, list: *mut c_char, size: usize) -> AxResult<isize> {
    xattr_list_by_fd(fd, list, size)
}

pub fn sys_removexattr(path: *const c_char, name: *const c_char) -> AxResult<isize> {
    xattr_remove_by_path(path, name, false)
}

pub fn sys_lremovexattr(path: *const c_char, name: *const c_char) -> AxResult<isize> {
    xattr_remove_by_path(path, name, true)
}

pub fn sys_fremovexattr(fd: i32, name: *const c_char) -> AxResult<isize> {
    xattr_remove_by_fd(fd, name)
}
