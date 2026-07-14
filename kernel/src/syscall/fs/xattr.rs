use alloc::{string::String, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType, path::Path};
use axtask::current;
use hashbrown::HashMap;
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, CAP_FOWNER, CAP_SETFCAP, CAP_SYS_ADMIN,
};
use starry_vm::vm_write_slice;

use super::ctl::validate_pathname;
use crate::{
    file::{
        ResolveAtResult, executable, is_path_only_fd, permission::check_writable_mount, resolve_at,
    },
    mm::{UserConstPtr, vm_load_string},
    pseudofs::tmp,
    task::{
        AsThread, Cred, FileCapabilities, Kuid, SECURITY_CAPABILITY_XATTR_NAME,
        parse_file_capabilities,
    },
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

fn security_namespace_write_allowed(name: &str, has_initial_sys_admin: bool) -> bool {
    name == SECURITY_CAPABILITY_XATTR_NAME || has_initial_sys_admin
}

fn file_capability_write_allowed(
    initial_user_namespace: bool,
    has_setfcap: bool,
    owns_inode: bool,
    has_fowner: bool,
) -> bool {
    initial_user_namespace && has_setfcap && (owns_inode || has_fowner)
}

fn credential_can_set_file_capabilities(cred: &Cred, owner: Option<Kuid>) -> bool {
    // Filesystems do not yet publish an owning user namespace or support
    // idmapped mounts. Treat their inodes as initial-user-namespace objects;
    // a CAP_SETFCAP bit held only in a child user namespace is not authority
    // over those objects.
    file_capability_write_allowed(
        cred.user_ns().is_initial(),
        cred.has_effective_capability_in_own_user_ns(CAP_SETFCAP),
        owner == Some(cred.ids().fsuid),
        cred.has_effective_capability_in_own_user_ns(CAP_FOWNER),
    )
}

fn authorized_file_capability_mutation<T>(
    loc: &Location,
    cred: &Cred,
    operation: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    if loc.node_type() != NodeType::RegularFile {
        return Err(LinuxError::EPERM.into());
    }

    let owner = Kuid::from_raw(loc.metadata()?.uid);
    if !credential_can_set_file_capabilities(cred, owner) {
        return Err(LinuxError::EPERM.into());
    }
    operation()
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
        "security"
            if write
                && !security_namespace_write_allowed(
                    name,
                    current()
                        .as_thread()
                        .has_effective_capability(CAP_SYS_ADMIN),
                ) =>
        {
            return Err(LinuxError::EPERM.into());
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
        if name == SECURITY_CAPABILITY_XATTR_NAME {
            let cred = current().as_thread().current_cred();
            // chmod/chown use the same writer gate. Exec credential sampling
            // cannot begin while this authorization and mutation are live.
            return executable::with_file_capability_metadata_unpinned(loc, || {
                let mut map = store.lock();
                set_file_capability_xattr_unpinned(loc, &cred, &mut map, value, flags)
            });
        }
        let mut map = store.lock();
        return set_map_xattr(&mut map, name, value, flags);
    }

    Err(LinuxError::EOPNOTSUPP.into())
}

fn set_file_capability_xattr_unpinned(
    loc: &Location,
    cred: &Cred,
    map: &mut HashMap<String, Vec<u8>>,
    value: Vec<u8>,
    flags: u32,
) -> AxResult<()> {
    authorized_file_capability_mutation(loc, cred, || {
        // Do not retain a payload which exec would later have to interpret
        // leniently. All Linux v1/v2/v3 fields, sizes, flags, capability bits,
        // and the v3 root ID are checked by the shared parser.
        parse_file_capabilities(&value)?;
        set_map_xattr(map, SECURITY_CAPABILITY_XATTR_NAME, value, flags)
    })
}

fn get_location_xattr(loc: &Location, name: &str) -> AxResult<Vec<u8>> {
    check_namespace_access(loc, name, false)?;

    if let Some(store) = tmp::xattr_store(loc) {
        let map = store.lock();
        return get_map_xattr(&map, name);
    }

    Err(LinuxError::EOPNOTSUPP.into())
}

/// Reads the final executable's file capabilities without applying the
/// userspace xattr visibility policy.
///
/// A provider without an honest xattr store and a missing capability xattr
/// both mean "no file capabilities". A stored but malformed capability record
/// remains a hard exec error. Exec callers hold a `CredentialReadLease` from
/// before this read through image publication, so the returned facts cannot
/// race chmod, chown, or another file-capability mutation.
pub(crate) fn security_capabilities_for_exec(loc: &Location) -> AxResult<Option<FileCapabilities>> {
    let Some(store) = tmp::xattr_store(loc) else {
        return stored_file_capabilities(None);
    };
    let map = store.lock();
    stored_file_capabilities(map.get(SECURITY_CAPABILITY_XATTR_NAME).map(Vec::as_slice))
}

fn stored_file_capabilities(value: Option<&[u8]>) -> AxResult<Option<FileCapabilities>> {
    let Some(value) = value else {
        return Ok(None);
    };
    parse_file_capabilities(value).map(Some)
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
        if name == SECURITY_CAPABILITY_XATTR_NAME {
            let cred = current().as_thread().current_cred();
            return executable::with_file_capability_metadata_unpinned(loc, || {
                let mut map = store.lock();
                remove_file_capability_xattr_unpinned(loc, &cred, &mut map)
            });
        }
        let mut map = store.lock();
        if map.remove(name).is_none() {
            return Err(LinuxError::ENODATA.into());
        }
        return Ok(());
    }

    Err(LinuxError::EOPNOTSUPP.into())
}

fn remove_file_capability_xattr_unpinned(
    loc: &Location,
    cred: &Cred,
    map: &mut HashMap<String, Vec<u8>>,
) -> AxResult<()> {
    authorized_file_capability_mutation(loc, cred, || {
        if map.remove(SECURITY_CAPABILITY_XATTR_NAME).is_none() {
            return Err(LinuxError::ENODATA.into());
        }
        Ok(())
    })
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

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};

    use axerrno::{AxError, LinuxError};
    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};

    use super::*;
    use crate::task::{Kgid, UserNamespace};

    fn valid_v2_capability() -> Vec<u8> {
        vec![
            0x01, 0x00, 0x00, 0x02, // revision 2, effective
            0x01, 0x00, 0x00, 0x00, // permitted word 0
            0x00, 0x00, 0x00, 0x00, // inheritable word 0
            0x00, 0x00, 0x00, 0x00, // permitted word 1
            0x00, 0x00, 0x00, 0x00, // inheritable word 1
        ]
    }

    fn memory_node(node_type: NodeType) -> Location {
        let fs = tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        mount
            .root_location()
            .create(
                "capability-target",
                node_type,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap()
    }

    fn initial_root() -> Arc<Cred> {
        Cred::try_root(UserNamespace::try_new_root().unwrap()).unwrap()
    }

    #[test]
    fn file_capability_authority_requires_every_independent_gate() {
        assert!(file_capability_write_allowed(true, true, true, false));
        assert!(file_capability_write_allowed(true, true, false, true));
        assert!(!file_capability_write_allowed(false, true, true, true));
        assert!(!file_capability_write_allowed(true, false, true, true));
        assert!(!file_capability_write_allowed(true, true, false, false));
    }

    #[test]
    fn non_capability_security_writes_require_initial_sys_admin() {
        assert!(!security_namespace_write_allowed("security.ima", false));
        assert!(security_namespace_write_allowed("security.ima", true));
        // security.capability has its stricter CAP_SETFCAP + owner gate below.
        assert!(security_namespace_write_allowed(
            SECURITY_CAPABILITY_XATTR_NAME,
            false
        ));
    }

    #[test]
    fn child_user_namespace_setfcap_is_not_host_filesystem_authority() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        assert!(credential_can_set_file_capabilities(
            &root,
            Some(Kuid::INITIAL_ROOT)
        ));

        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let child = Cred::try_with_user_namespace(&root, child_namespace).unwrap();
        assert!(!credential_can_set_file_capabilities(
            &child,
            Some(Kuid::INITIAL_ROOT)
        ));
    }

    #[test]
    fn setting_capabilities_rejects_non_regular_and_malformed_targets() {
        let root = initial_root();
        let directory = memory_node(NodeType::Directory);
        let directory_store = tmp::xattr_store(&directory).unwrap();
        let mut directory_map = directory_store.lock();
        assert_eq!(
            set_file_capability_xattr_unpinned(
                &directory,
                &root,
                &mut directory_map,
                valid_v2_capability(),
                0,
            ),
            Err(LinuxError::EPERM.into())
        );
        directory_map.insert(SECURITY_CAPABILITY_XATTR_NAME.into(), valid_v2_capability());
        assert_eq!(
            remove_file_capability_xattr_unpinned(&directory, &root, &mut directory_map),
            Err(LinuxError::EPERM.into())
        );
        assert!(directory_map.contains_key(SECURITY_CAPABILITY_XATTR_NAME));
        drop(directory_map);

        let file = memory_node(NodeType::RegularFile);
        let file_store = tmp::xattr_store(&file).unwrap();
        let mut file_map = file_store.lock();
        assert_eq!(
            set_file_capability_xattr_unpinned(&file, &root, &mut file_map, vec![1, 2, 3], 0),
            Err(AxError::InvalidInput)
        );
        assert!(!file_map.contains_key(SECURITY_CAPABILITY_XATTR_NAME));
    }

    #[test]
    fn set_and_remove_helpers_preserve_the_stored_record_on_error() {
        let root = initial_root();
        let file = memory_node(NodeType::RegularFile);
        let store = tmp::xattr_store(&file).unwrap();
        let mut map = store.lock();

        assert_eq!(
            remove_file_capability_xattr_unpinned(&file, &root, &mut map),
            Err(LinuxError::ENODATA.into())
        );
        assert!(!map.contains_key(SECURITY_CAPABILITY_XATTR_NAME));

        set_file_capability_xattr_unpinned(&file, &root, &mut map, valid_v2_capability(), 0)
            .unwrap();
        assert_eq!(
            set_file_capability_xattr_unpinned(
                &file,
                &root,
                &mut map,
                valid_v2_capability(),
                XATTR_CREATE,
            ),
            Err(LinuxError::EEXIST.into())
        );
        assert!(map.contains_key(SECURITY_CAPABILITY_XATTR_NAME));

        remove_file_capability_xattr_unpinned(&file, &root, &mut map).unwrap();
        assert!(!map.contains_key(SECURITY_CAPABILITY_XATTR_NAME));
    }

    #[test]
    fn exec_reader_distinguishes_absent_unsupported_and_malformed_xattrs() {
        let file = memory_node(NodeType::RegularFile);
        let store = tmp::xattr_store(&file).unwrap();
        assert!(security_capabilities_for_exec(&file).unwrap().is_none());

        store
            .lock()
            .insert(SECURITY_CAPABILITY_XATTR_NAME.into(), valid_v2_capability());
        assert!(security_capabilities_for_exec(&file).unwrap().is_some());

        store
            .lock()
            .insert(SECURITY_CAPABILITY_XATTR_NAME.into(), vec![1, 2, 3]);
        assert_eq!(
            security_capabilities_for_exec(&file),
            Err(AxError::InvalidInput)
        );

        assert!(stored_file_capabilities(None).unwrap().is_none());
    }
}
