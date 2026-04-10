use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType, path::Path};
use axsync::Mutex;
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use starry_vm::vm_write_slice;

use super::ctl::validate_pathname;
use crate::{
    file::{ResolveAtResult, resolve_at},
    mm::{UserConstPtr, vm_load_string},
    pseudofs::tmp,
};

const XATTR_CREATE: u32 = 0x1;
const XATTR_REPLACE: u32 = 0x2;
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 65536;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct XattrKey {
    device: u64,
    inode: u64,
}

static GENERIC_XATTRS: Mutex<BTreeMap<XattrKey, BTreeMap<String, Vec<u8>>>> =
    Mutex::new(BTreeMap::new());

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

fn namespace_allows_set(loc: &Location, name: &str) -> AxResult<()> {
    let (namespace, _) = name.split_once('.').unwrap();
    if namespace == "user"
        && !matches!(loc.node_type(), NodeType::RegularFile | NodeType::Directory)
    {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

fn set_map_xattr(
    map: &mut BTreeMap<String, Vec<u8>>,
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

    map.insert(name.into(), value);
    Ok(())
}

fn get_map_xattr(map: &BTreeMap<String, Vec<u8>>, name: &str) -> AxResult<Vec<u8>> {
    map.get(name)
        .cloned()
        .ok_or_else(|| LinuxError::ENODATA.into())
}

fn list_map_xattrs(map: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut list = Vec::new();
    for name in map.keys() {
        list.extend_from_slice(name.as_bytes());
        list.push(0);
    }
    list
}

fn set_location_xattr(loc: &Location, name: &str, value: Vec<u8>, flags: u32) -> AxResult<()> {
    namespace_allows_set(loc, name)?;

    if let Some(store) = tmp::xattr_store(loc) {
        let mut map = store.lock();
        return set_map_xattr(&mut map, name, value, flags);
    }

    let key = XattrKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    };
    let mut stores = GENERIC_XATTRS.lock();
    {
        let map = stores.entry(key).or_default();
        set_map_xattr(map, name, value, flags)?;
    }
    Ok(())
}

fn get_location_xattr(loc: &Location, name: &str) -> AxResult<Vec<u8>> {
    if let Some(store) = tmp::xattr_store(loc) {
        let map = store.lock();
        return get_map_xattr(&map, name);
    }

    let key = XattrKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    };
    let stores = GENERIC_XATTRS.lock();
    let map = stores
        .get(&key)
        .ok_or_else(|| AxError::from(LinuxError::ENODATA))?;
    get_map_xattr(map, name)
}

fn list_location_xattrs(loc: &Location) -> Vec<u8> {
    if let Some(store) = tmp::xattr_store(loc) {
        let map = store.lock();
        return list_map_xattrs(&map);
    }

    let key = XattrKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    };
    let stores = GENERIC_XATTRS.lock();
    stores.get(&key).map_or_else(Vec::new, list_map_xattrs)
}

fn remove_location_xattr(loc: &Location, name: &str) -> AxResult<()> {
    if let Some(store) = tmp::xattr_store(loc) {
        let mut map = store.lock();
        if map.remove(name).is_none() {
            return Err(LinuxError::ENODATA.into());
        }
        return Ok(());
    }

    let key = XattrKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    };
    let mut stores = GENERIC_XATTRS.lock();
    let should_remove_key = {
        let map = stores
            .get_mut(&key)
            .ok_or_else(|| AxError::from(LinuxError::ENODATA))?;
        if map.remove(name).is_none() {
            return Err(LinuxError::ENODATA.into());
        }
        map.is_empty()
    };
    if should_remove_key {
        stores.remove(&key);
    }
    Ok(())
}

pub(crate) fn clear_location_xattrs(loc: &Location) {
    if let Some(store) = tmp::xattr_store(loc) {
        store.lock().clear();
        return;
    }

    let key = XattrKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    };
    GENERIC_XATTRS.lock().remove(&key);
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
    validate_xattr_flags(flags)?;
    let name = read_xattr_name(name)?;
    let value = read_xattr_value(value, size)?;

    match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => {
            set_location_xattr(&loc, &name, value, flags)?;
            Ok(0)
        }
        ResolveAtResult::Other(_) => {
            let (namespace, _) = name.split_once('.').unwrap();
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
    let names = list_location_xattrs(&loc);
    write_xattr_list(list, size, &names)
}

fn xattr_list_by_fd(fd: i32, list: *mut c_char, size: usize) -> AxResult<isize> {
    let names = match resolve_xattr_fd(fd)? {
        ResolveAtResult::File(loc) => list_location_xattrs(&loc),
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
