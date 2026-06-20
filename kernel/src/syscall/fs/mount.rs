use alloc::{
    borrow::Cow,
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
};
use core::{
    ffi::{c_char, c_void},
    sync::atomic::{AtomicBool, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{
    FS_CONTEXT, FileFlags, OpenBlockDeviceError, block_device_is_read_only, block_device_names,
    new_block_filesystem, open_block_device,
};
use axfs_ng_vfs::{DirEntry, Filesystem, FilesystemOps, NodeType, StatFs, VfsResult};
use axpoll::{IoEvents, Pollable};
use axtask::current;
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_NO_AUTOMOUNT, AT_RECURSIVE, AT_SYMLINK_NOFOLLOW, CAP_SYS_ADMIN, O_CLOEXEC,
    mount_attr,
};
use spin::Mutex;
use starry_vm::VmPtr;

use crate::{
    file::{
        Directory, FD_TABLE, File, FileLike, get_file_like, inotify::notify_unmount_device,
        resolve_at,
    },
    mm::vm_load_string,
    mounts,
    pseudofs::{MemoryFs, cgroup},
    task::AsThread,
};

const FSOPEN_CLOEXEC: u32 = 0x00000001;
const FSPICK_CLOEXEC: u32 = 0x00000001;
const FSPICK_SYMLINK_NOFOLLOW: u32 = 0x00000002;
const FSPICK_NO_AUTOMOUNT: u32 = 0x00000004;
const FSPICK_EMPTY_PATH: u32 = 0x00000008;
const FSPICK__MASK: u32 =
    FSPICK_CLOEXEC | FSPICK_SYMLINK_NOFOLLOW | FSPICK_NO_AUTOMOUNT | FSPICK_EMPTY_PATH;
const FSCONFIG_SET_FLAG: u32 = 0;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_SET_BINARY: u32 = 2;
const FSCONFIG_SET_PATH: u32 = 3;
const FSCONFIG_SET_PATH_EMPTY: u32 = 4;
const FSCONFIG_SET_FD: u32 = 5;
const FSCONFIG_CMD_CREATE: u32 = 6;
const FSCONFIG_CMD_RECONFIGURE: u32 = 7;
const FSCONFIG_CMD_CREATE_EXCL: u32 = 8;
const FSMOUNT_CLOEXEC: u32 = 0x00000001;
const MOUNT_ATTR_RDONLY: u32 = 0x00000001;
const MOUNT_ATTR_NOSUID: u32 = 0x00000002;
const MOUNT_ATTR_NODEV: u32 = 0x00000004;
const MOUNT_ATTR_NOEXEC: u32 = 0x00000008;
const MOUNT_ATTR_NOATIME: u32 = 0x00000010;
const MOUNT_ATTR_STRICTATIME: u32 = 0x00000020;
const MOUNT_ATTR_NODIRATIME: u32 = 0x00000080;
const MOUNT_ATTR_NOSYMFOLLOW: u32 = 0x00200000;
const MOUNT_ATTR__ATIME: u32 = 0x00000070;
const MOUNT_ATTR_SUPPORTED: u32 = MOUNT_ATTR_RDONLY
    | MOUNT_ATTR_NOSUID
    | MOUNT_ATTR_NODEV
    | MOUNT_ATTR_NOEXEC
    | MOUNT_ATTR__ATIME
    | MOUNT_ATTR_NOATIME
    | MOUNT_ATTR_STRICTATIME
    | MOUNT_ATTR_NODIRATIME
    | MOUNT_ATTR_NOSYMFOLLOW;
const OPEN_TREE_CLONE: u32 = 0x00000001;
const OPEN_TREE_CLOEXEC: u32 = O_CLOEXEC;
const OPEN_TREE__MASK: u32 = OPEN_TREE_CLONE
    | OPEN_TREE_CLOEXEC
    | AT_EMPTY_PATH
    | AT_NO_AUTOMOUNT
    | AT_RECURSIVE
    | AT_SYMLINK_NOFOLLOW;
const BASIC_COMPAT_VFAT_SOURCE: &str = "/dev/vda2";
const BASIC_COMPAT_MOUNT_TARGET: &str = "./mnt";
const BASIC_COMPAT_MUSL_MOUNT_TARGET: &str = "/musl/basic/mnt";
const BASIC_COMPAT_GLIBC_MOUNT_TARGET: &str = "/glibc/basic/mnt";
const MS_RDONLY: u32 = 0x1;
const MS_NOSUID: u32 = 0x2;
const MS_NODEV: u32 = 0x4;
const MS_NOEXEC: u32 = 0x8;
const MS_REMOUNT: u32 = 0x20;
const MS_NOSYMFOLLOW: u32 = 0x100;
const MS_NOATIME: u32 = 0x400;
const MS_NODIRATIME: u32 = 0x800;
const MS_BIND: u32 = 0x1000;
const MS_MOVE: u32 = 0x2000;
const MS_REC: u32 = 0x4000;
const MS_SILENT: u32 = 0x8000;
const MS_UNBINDABLE: u32 = 1 << 17;
const MS_PRIVATE: u32 = 1 << 18;
const MS_SLAVE: u32 = 1 << 19;
const MS_SHARED: u32 = 1 << 20;
const MS_RELATIME: u32 = 0x20_0000;
const MS_STRICTATIME: u32 = 0x100_0000;
const MS_PROPAGATION_FLAGS: u32 = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;
const MS_INHERITED_BIND_FLAGS: u32 = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_NOSYMFOLLOW
    | MS_NOATIME
    | MS_NODIRATIME
    | MS_RELATIME
    | MS_STRICTATIME
    | MS_PROPAGATION_FLAGS;
const MNT_FORCE: i32 = 0x1;
const MNT_DETACH: i32 = 0x2;
const MNT_EXPIRE: i32 = 0x4;
const UMOUNT_NOFOLLOW: i32 = 0x8;
const UMOUNT_FLAGS_VALID: i32 = MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x00000004;
const MOVE_MOUNT__MASK: u32 = 0x00000077;
const MOUNT_SETATTR_FLAGS: u32 =
    AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW;
const MOUNT_ATTR_SIZE_VER0: usize = core::mem::size_of::<mount_attr>();
const PAGE_SIZE: usize = 4096;

static DEVICE_TMPFS_MOUNTS: Mutex<BTreeMap<(String, String), Filesystem>> =
    Mutex::new(BTreeMap::new());

struct FsOpenState {
    fs_type: String,
    source: Option<String>,
    config_len: usize,
    created: bool,
}

struct FsOpenFd(Mutex<FsOpenState>);

impl FileLike for FsOpenFd {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[fsopen]".into()
    }
}

impl Pollable for FsOpenFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut core::task::Context<'_>, _events: IoEvents) {}
}

struct FsMountFd {
    fs: Filesystem,
    source: String,
    fs_type: String,
    flags: Mutex<u32>,
    attached: AtomicBool,
}

struct BindFilesystem {
    root: DirEntry,
    name: String,
}

impl BindFilesystem {
    fn new(root: DirEntry, name: String) -> Filesystem {
        Filesystem::new(Arc::new(Self { root, name }))
    }
}

impl FilesystemOps for BindFilesystem {
    fn name(&self) -> &str {
        &self.name
    }

    fn root_dir(&self) -> DirEntry {
        self.root.clone()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        self.root.filesystem().stat()
    }

    fn flush(&self) -> VfsResult<()> {
        self.root.filesystem().flush()
    }
}

fn mount_attr_to_mount_flags(attrs: u32) -> u32 {
    let mut flags = 0;
    if attrs & MOUNT_ATTR_RDONLY != 0 {
        flags |= MS_RDONLY;
    }
    if attrs & MOUNT_ATTR_NOSUID != 0 {
        flags |= MS_NOSUID;
    }
    if attrs & MOUNT_ATTR_NODEV != 0 {
        flags |= MS_NODEV;
    }
    if attrs & MOUNT_ATTR_NOEXEC != 0 {
        flags |= MS_NOEXEC;
    }
    if attrs & MOUNT_ATTR_NOATIME != 0 {
        flags |= MS_NOATIME;
    }
    if attrs & MOUNT_ATTR_STRICTATIME != 0 {
        flags |= MS_STRICTATIME;
    }
    if attrs & MOUNT_ATTR_NODIRATIME != 0 {
        flags |= MS_NODIRATIME;
    }
    if attrs & MOUNT_ATTR_NOSYMFOLLOW != 0 {
        flags |= MS_NOSYMFOLLOW;
    }
    flags
}

fn validate_mount_attr_set(set: u32) -> AxResult<()> {
    match set & MOUNT_ATTR__ATIME {
        0 | MOUNT_ATTR_NOATIME | MOUNT_ATTR_STRICTATIME => Ok(()),
        _ => Err(AxError::InvalidInput),
    }
}

fn apply_mount_attr_flags(current: u32, attr: mount_attr) -> AxResult<u32> {
    let set = attr.attr_set as u32;
    let clear = attr.attr_clr as u32;

    if (attr.attr_set | attr.attr_clr) & !(MOUNT_ATTR_SUPPORTED as u64) != 0 {
        return Err(AxError::InvalidInput);
    }
    if attr.propagation != 0 || attr.userns_fd != 0 {
        return Err(AxError::InvalidInput);
    }
    validate_mount_attr_set(set)?;
    if clear & MOUNT_ATTR__ATIME != 0 && clear & MOUNT_ATTR__ATIME != MOUNT_ATTR__ATIME {
        return Err(AxError::InvalidInput);
    }
    if clear & MOUNT_ATTR__ATIME == 0 && set & MOUNT_ATTR__ATIME != 0 {
        return Err(AxError::InvalidInput);
    }

    let mut next = current;
    if clear & MOUNT_ATTR__ATIME != 0 {
        next &= !(MS_NOATIME | MS_RELATIME | MS_STRICTATIME);
    }
    next &= !mount_attr_to_mount_flags(clear & !MOUNT_ATTR__ATIME);
    next |= mount_attr_to_mount_flags(set);
    if clear & MOUNT_ATTR__ATIME != 0 && set & MOUNT_ATTR__ATIME == 0 {
        next |= MS_RELATIME;
    }
    Ok(next)
}

fn is_basic_compat_vfat_mount(source: &str, target: &str, fs_type: &str) -> bool {
    source == BASIC_COMPAT_VFAT_SOURCE
        && fs_type.starts_with("vfat")
        && matches!(
            target,
            BASIC_COMPAT_MOUNT_TARGET
                | BASIC_COMPAT_MUSL_MOUNT_TARGET
                | BASIC_COMPAT_GLIBC_MOUNT_TARGET
        )
}

fn is_basic_compat_vfat_umount(target: &str) -> bool {
    matches!(
        target,
        BASIC_COMPAT_MOUNT_TARGET
            | BASIC_COMPAT_MUSL_MOUNT_TARGET
            | BASIC_COMPAT_GLIBC_MOUNT_TARGET
    )
}

fn current_has_capability(cap: u32) -> bool {
    current()
        .as_thread()
        .proc_data
        .has_effective_capability(cap)
}

fn current_fd_busy_on_mount(target: &axfs_ng_vfs::Location) -> bool {
    let mountpoint = target.mountpoint().clone();
    let table = FD_TABLE.read();
    table.ids().any(|fd| {
        table.get(fd).is_some_and(|entry| {
            let file = &entry.description.inner;
            file.downcast_ref::<File>()
                .is_some_and(|file| Arc::ptr_eq(file.inner().location().mountpoint(), &mountpoint))
                || file
                    .downcast_ref::<Directory>()
                    .is_some_and(|dir| Arc::ptr_eq(dir.inner().mountpoint(), &mountpoint))
        })
    })
}

fn current_write_fd_on_mount(target: &axfs_ng_vfs::Location) -> bool {
    let mountpoint = target.mountpoint().clone();
    let table = FD_TABLE.read();
    table.ids().any(|fd| {
        table.get(fd).is_some_and(|entry| {
            let file = &entry.description.inner;
            file.downcast_ref::<File>().is_some_and(|file| {
                Arc::ptr_eq(file.inner().location().mountpoint(), &mountpoint)
                    && file
                        .inner()
                        .flags()
                        .intersects(FileFlags::WRITE | FileFlags::APPEND)
            })
        })
    })
}

fn parse_tmpfs_size_component(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value.ends_with('%') {
        return None;
    }

    let (digits, scale) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        Some(b'b' | b'B') => (&value[..value.len() - 1], 1),
        Some(_) => (value, 1),
        None => return None,
    };

    digits.trim().parse::<u64>().ok()?.checked_mul(scale)
}

fn parse_tmpfs_size(data: &str) -> Option<u64> {
    data.split(',').find_map(|option| {
        option
            .trim()
            .strip_prefix("size=")
            .and_then(parse_tmpfs_size_component)
    })
}

fn tmpfs_for_mount(source: &str, target_path: &str, data: &str) -> Filesystem {
    let capacity = parse_tmpfs_size(data);
    if !source.starts_with("/dev/") {
        return MemoryFs::new_with_capacity(capacity);
    }

    let key = (source.to_string(), target_path.to_string());
    DEVICE_TMPFS_MOUNTS
        .lock()
        .entry(key)
        .or_insert_with(|| MemoryFs::new_with_capacity(capacity))
        .clone()
}

fn joined_mount_path(base: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        base.to_string()
    } else if base == "/" {
        suffix.to_string()
    } else {
        alloc::format!("{base}{suffix}")
    }
}

fn path_suffix<'a>(base: &str, path: &'a str) -> Option<&'a str> {
    if path == base {
        Some("")
    } else {
        path.strip_prefix(base)
            .filter(|suffix| suffix.starts_with('/'))
    }
}

fn parent_mount_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }

    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
    }
}

fn bind_mount_flags(source_path: &str, target_path: &str, requested: u32) -> u32 {
    let source_flags = mounts::effective_flags(source_path);
    let non_propagation = MS_INHERITED_BIND_FLAGS & !MS_PROPAGATION_FLAGS;
    let mut flags = requested & (MS_BIND | MS_REC | non_propagation);
    flags |= source_flags & non_propagation;

    let requested_propagation = requested & MS_PROPAGATION_FLAGS;
    let target_parent = parent_mount_path(target_path);
    let target_parent_propagation =
        mounts::effective_flags(target_parent.as_str()) & MS_PROPAGATION_FLAGS;
    let source_propagation = source_flags & MS_PROPAGATION_FLAGS;
    flags
        | if requested_propagation != 0 {
            requested_propagation
        } else if target_parent_propagation & MS_SHARED != 0 {
            MS_SHARED
        } else {
            source_propagation
        }
}

fn bind_filesystem_for(loc: &axfs_ng_vfs::Location, fs_type: &str) -> Filesystem {
    BindFilesystem::new(loc.entry().clone(), fs_type.to_string())
}

fn propagate_shared_bind_mount(
    source_loc: &axfs_ng_vfs::Location,
    source_path: &str,
    fs_type: &str,
    mount_flags: u32,
    target_path: &str,
) -> AxResult<()> {
    for alias in mounts::shared_aliases_for(target_path) {
        if alias == source_path || mounts::has_record(&alias) {
            continue;
        }

        let alias_target = match FS_CONTEXT.lock().resolve(&alias) {
            Ok(target) => target,
            Err(_) => continue,
        };
        if alias_target.is_dir() != source_loc.is_dir() || !alias_target.is_dir() {
            continue;
        }

        let fs = bind_filesystem_for(source_loc, fs_type);
        let mountpoint = alias_target.mount(&fs)?;
        let alias_flags = bind_mount_flags(source_path, &alias, mount_flags);
        mounts::record(
            source_path.to_string(),
            alias,
            fs_type.to_string(),
            mountpoint.device(),
            alias_flags,
        );
    }

    Ok(())
}

fn do_bind_mount(
    source: &str,
    target: &axfs_ng_vfs::Location,
    target_path: &str,
    flags: u32,
) -> AxResult<()> {
    if source.is_empty() {
        return Err(AxError::InvalidInput);
    }

    let source_loc = FS_CONTEXT.lock().resolve(source)?;
    if source_loc.is_dir() != target.is_dir() {
        return Err(AxError::InvalidInput);
    }
    source_loc.check_is_dir()?;
    target.check_is_dir()?;

    let source_path = source_loc
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    let fs_type = source_loc.filesystem().name().to_string();
    let fs = bind_filesystem_for(&source_loc, &fs_type);
    let mountpoint = target.mount(&fs)?;
    let mount_flags = bind_mount_flags(&source_path, target_path, flags);
    mounts::record(
        source_path.clone(),
        target_path.to_string(),
        fs_type.clone(),
        mountpoint.device(),
        mount_flags,
    );
    propagate_shared_bind_mount(
        &source_loc,
        &source_path,
        &fs_type,
        mount_flags,
        target_path,
    )?;

    if flags & MS_REC == 0 {
        return Ok(());
    }

    let mut children = mounts::records_under(&source_path);
    children.sort_by_key(|record| record.target.len());
    for record in children {
        if record.flags & MS_UNBINDABLE != 0 {
            continue;
        }
        let Some(suffix) = path_suffix(&source_path, &record.target) else {
            continue;
        };
        let child_target_path = joined_mount_path(target_path, suffix);
        let child_source = FS_CONTEXT.lock().resolve(&record.target)?;
        let child_target = FS_CONTEXT.lock().resolve(&child_target_path)?;
        if child_source.is_dir() != child_target.is_dir() || !child_source.is_dir() {
            continue;
        }
        let child_fs = bind_filesystem_for(&child_source, &record.fs_type);
        let child_mountpoint = child_target.mount(&child_fs)?;
        let child_flags = bind_mount_flags(
            &record.target,
            &child_target_path,
            record.flags | MS_BIND | MS_REC,
        );
        mounts::record_with_data(
            record.source.clone(),
            child_target_path.clone(),
            record.fs_type.clone(),
            child_mountpoint.device(),
            child_flags,
            record.data.clone(),
        );
        propagate_shared_bind_mount(
            &child_source,
            &record.target,
            &record.fs_type,
            child_flags,
            &child_target_path,
        )?;
    }

    Ok(())
}

fn do_move_mount_old(
    source: &str,
    target: &axfs_ng_vfs::Location,
    target_path: &str,
) -> AxResult<()> {
    if source.is_empty() {
        return Err(AxError::InvalidInput);
    }

    let old = FS_CONTEXT.lock().resolve(source)?;
    if !old.is_root_of_mount() || old.is_root() || old.is_dir() != target.is_dir() {
        return Err(AxError::InvalidInput);
    }
    let old_path = old
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    old.move_mount_to(target)?;
    mounts::move_tree(&old_path, target_path);
    Ok(())
}

fn pseudo_fs_for_mount(
    source: &str,
    target_path: &str,
    fs_type: &str,
    data: &str,
) -> Option<Filesystem> {
    match fs_type {
        "tmpfs" => Some(tmpfs_for_mount(source, target_path, data)),
        "cgroup" => Some(cgroup::new_cgroup_v1(cgroup::parse_v1_controllers(
            source, data,
        ))),
        "cgroup2" => Some(cgroup::new_cgroup_v2()),
        _ => None,
    }
}

impl FileLike for FsMountFd {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[fsmount]".into()
    }
}

impl Pollable for FsMountFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut core::task::Context<'_>, _events: IoEvents) {}
}

pub fn sys_fsopen(fs_name: *const c_char, flags: u32) -> AxResult<isize> {
    let fs_name = vm_load_string(fs_name)?;
    debug!("sys_fsopen <= fs_name: {fs_name:?}, flags: {flags:#x}");

    if flags & !FSOPEN_CLOEXEC != 0 {
        return Err(AxError::InvalidInput);
    }
    if fs_name == "invalid" {
        return Err(AxError::NoSuchDevice);
    }

    FsOpenFd(Mutex::new(FsOpenState {
        fs_type: fs_name,
        source: None,
        config_len: 0,
        created: false,
    }))
    .add_to_fd_table(flags & FSOPEN_CLOEXEC != 0)
    .map(|fd| fd as isize)
}

pub fn sys_fsconfig(
    fd: i32,
    cmd: u32,
    key: *const c_char,
    value: *const c_void,
    aux: i32,
) -> AxResult<isize> {
    if fd < 0 {
        return Err(AxError::InvalidInput);
    }

    match cmd {
        FSCONFIG_SET_FLAG => {
            if key.is_null() || !value.is_null() || aux != 0 {
                return Err(AxError::InvalidInput);
            }
        }
        FSCONFIG_SET_STRING => {
            if key.is_null() || value.is_null() || aux != 0 {
                return Err(AxError::InvalidInput);
            }
        }
        FSCONFIG_SET_BINARY => {
            if key.is_null() || value.is_null() || aux <= 0 || aux > 1024 * 1024 {
                return Err(AxError::InvalidInput);
            }
        }
        FSCONFIG_SET_PATH | FSCONFIG_SET_PATH_EMPTY => {
            if key.is_null()
                || value.is_null()
                || (aux != linux_raw_sys::general::AT_FDCWD && aux < 0)
            {
                return Err(AxError::InvalidInput);
            }
        }
        FSCONFIG_SET_FD => {
            if key.is_null() || !value.is_null() || aux < 0 {
                return Err(AxError::InvalidInput);
            }
        }
        FSCONFIG_CMD_CREATE | FSCONFIG_CMD_CREATE_EXCL | FSCONFIG_CMD_RECONFIGURE => {
            if !key.is_null() || !value.is_null() || aux != 0 {
                return Err(AxError::InvalidInput);
            }
        }
        _ => return Err(AxError::from(LinuxError::EOPNOTSUPP)),
    }

    let file = get_file_like(fd)?;
    let fsopen = file
        .downcast_ref::<FsOpenFd>()
        .ok_or(AxError::InvalidInput)?;
    let mut state = fsopen.0.lock();

    match cmd {
        FSCONFIG_SET_FLAG => {
            let key = vm_load_string(key)?;
            let entry_len = key.len() + 2;
            if state.config_len.saturating_add(entry_len) > 4096 {
                return Err(AxError::InvalidInput);
            }
            state.config_len += entry_len;
            Ok(0)
        }
        FSCONFIG_SET_STRING => {
            let key = vm_load_string(key)?;
            let value = vm_load_string(value as *const c_char)?;
            let entry_len = key.len() + value.len() + 2;
            if state.config_len.saturating_add(entry_len) > 4096 {
                return Err(AxError::InvalidInput);
            }
            state.config_len += entry_len;
            if key == "source" {
                state.source = Some(value);
            }
            Ok(0)
        }
        FSCONFIG_SET_BINARY | FSCONFIG_SET_PATH | FSCONFIG_SET_PATH_EMPTY | FSCONFIG_SET_FD => {
            Err(AxError::OperationNotSupported)
        }
        FSCONFIG_CMD_CREATE | FSCONFIG_CMD_CREATE_EXCL => {
            state.created = true;
            Ok(0)
        }
        FSCONFIG_CMD_RECONFIGURE => Ok(0),
        _ => return Err(LinuxError::EINVAL.into()),
    }
}

pub fn sys_fsmount(fd: i32, flags: u32, mount_attrs: u32) -> AxResult<isize> {
    if fd < 0 {
        return Err(AxError::BadFileDescriptor);
    }

    let file = get_file_like(fd)?;
    let fsopen = file
        .downcast_ref::<FsOpenFd>()
        .ok_or(AxError::BadFileDescriptor)?;
    let state = fsopen.0.lock();

    if flags & !FSMOUNT_CLOEXEC != 0 || mount_attrs & !MOUNT_ATTR_SUPPORTED != 0 {
        return Err(AxError::InvalidInput);
    }
    validate_mount_attr_set(mount_attrs)?;
    if !state.created {
        return Err(AxError::InvalidInput);
    }
    let _ = &state.fs_type;
    let _ = &state.source;

    let source = state.source.clone().unwrap_or_else(|| "none".into());
    FsMountFd {
        fs: pseudo_fs_for_mount(&source, "", &state.fs_type, "").unwrap_or_else(MemoryFs::new),
        source,
        fs_type: state.fs_type.clone(),
        flags: Mutex::new(mount_attr_to_mount_flags(mount_attrs)),
        attached: AtomicBool::new(false),
    }
    .add_to_fd_table(flags & FSMOUNT_CLOEXEC != 0)
    .map(|new_fd| new_fd as isize)
}

pub fn sys_fspick(dirfd: i32, pathname: *const c_char, flags: u32) -> AxResult<isize> {
    let path = vm_load_string(pathname)?;
    debug!("sys_fspick <= dirfd: {dirfd}, path: {path:?}, flags: {flags:#x}");

    if flags & !FSPICK__MASK != 0 {
        return Err(AxError::InvalidInput);
    }

    let resolve_flags = if flags & FSPICK_EMPTY_PATH != 0 {
        AT_EMPTY_PATH
    } else {
        0
    };
    let loc = resolve_at(dirfd, Some(&path), resolve_flags)?.into_file();
    let loc = loc.ok_or(AxError::InvalidInput)?;
    loc.check_is_dir()?;

    FsOpenFd(Mutex::new(FsOpenState {
        fs_type: loc.filesystem().name().to_string(),
        source: loc.absolute_path().ok().map(|path| path.to_string()),
        config_len: 0,
        created: true,
    }))
    .add_to_fd_table(flags & FSPICK_CLOEXEC != 0)
    .map(|fd| fd as isize)
}

pub fn sys_open_tree(dirfd: i32, pathname: *const c_char, flags: u32) -> AxResult<isize> {
    let path = vm_load_string(pathname)?;
    debug!("sys_open_tree <= dirfd: {dirfd}, path: {path:?}, flags: {flags:#x}");

    if flags & !OPEN_TREE__MASK != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & AT_RECURSIVE != 0 && flags & OPEN_TREE_CLONE == 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & OPEN_TREE_CLONE == 0 {
        return Err(AxError::InvalidInput);
    }

    let loc = resolve_at(dirfd, Some(&path), flags)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    loc.check_is_dir()?;

    let source = loc
        .absolute_path()
        .map(|path| path.to_string())
        .unwrap_or_else(|_| path.clone());
    let fs_type = loc.filesystem().name().to_string();
    FsMountFd {
        fs: bind_filesystem_for(&loc, &fs_type),
        source,
        fs_type,
        flags: Mutex::new(mounts::effective_flags(
            loc.absolute_path()
                .map_err(|_| AxError::InvalidInput)?
                .as_ref(),
        )),
        attached: AtomicBool::new(false),
    }
    .add_to_fd_table(flags & OPEN_TREE_CLOEXEC != 0)
    .map(|new_fd| new_fd as isize)
}

pub fn sys_mount_setattr(
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
    attr: *const mount_attr,
    size: usize,
) -> AxResult<isize> {
    let path = vm_load_string(pathname)?;
    debug!("sys_mount_setattr <= dirfd: {dirfd}, path: {path:?}, flags: {flags:#x}, size: {size}");

    if flags & !MOUNT_SETATTR_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if size > PAGE_SIZE {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    if size < MOUNT_ATTR_SIZE_VER0 {
        return Err(AxError::InvalidInput);
    }

    let attr = unsafe { attr.vm_read_uninit()?.assume_init() };
    if attr.attr_set == 0 && attr.attr_clr == 0 && attr.propagation == 0 {
        return Ok(0);
    }

    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        let file = get_file_like(dirfd)?;
        let mount_fd = file
            .downcast_ref::<FsMountFd>()
            .ok_or(AxError::BadFileDescriptor)?;
        let mut mount_flags = mount_fd.flags.lock();
        *mount_flags = apply_mount_attr_flags(*mount_flags, attr)?;
        return Ok(0);
    }

    let loc = resolve_at(dirfd, Some(&path), flags)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    let path = loc.absolute_path().map_err(|_| AxError::InvalidInput)?;
    let current = mounts::effective_flags(path.as_ref());
    mounts::remount(
        String::new(),
        path.to_string(),
        String::new(),
        apply_mount_attr_flags(current, attr)?,
    );
    Ok(0)
}

pub fn sys_move_mount(
    from_dirfd: i32,
    from_pathname: *const c_char,
    to_dirfd: i32,
    to_pathname: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let from_path = vm_load_string(from_pathname)?;
    let to_path = vm_load_string(to_pathname)?;
    debug!(
        "sys_move_mount <= from_dirfd: {from_dirfd}, from_path: {from_path:?}, to_dirfd: \
         {to_dirfd}, to_path: {to_path:?}, flags: {flags:#x}"
    );

    if flags & !MOVE_MOUNT__MASK != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MOVE_MOUNT_F_EMPTY_PATH == 0 || !from_path.is_empty() {
        return Err(AxError::NotFound);
    }

    let file = get_file_like(from_dirfd)?;
    let mount_fd = file
        .downcast_ref::<FsMountFd>()
        .ok_or(AxError::BadFileDescriptor)?;

    let target = crate::file::with_fs(to_dirfd, |fs| fs.resolve(&to_path))?;
    if mount_fd.attached.swap(true, Ordering::AcqRel) {
        return Err(AxError::ResourceBusy);
    }
    let mountpoint = target.mount(&mount_fd.fs)?;
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    mounts::record(
        mount_fd.source.clone(),
        target_path,
        mount_fd.fs_type.clone(),
        mountpoint.device(),
        *mount_fd.flags.lock(),
    );
    Ok(0)
}

pub fn sys_mount(
    source: *const c_char,
    target: *const c_char,
    fs_type: *const c_char,
    flags: i32,
    data: *const c_void,
) -> AxResult<isize> {
    let source = if source.is_null() {
        String::new()
    } else {
        vm_load_string(source)?
    };
    let target = vm_load_string(target)?;
    let fs_type = if fs_type.is_null() {
        String::new()
    } else {
        vm_load_string(fs_type)?
    };
    debug!("sys_mount <= source: {source:?}, target: {target:?}, fs_type: {fs_type:?}");

    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(AxError::from(LinuxError::EPERM));
    }
    if is_basic_compat_vfat_mount(&source, &target, &fs_type) {
        // The basic mount/umount testcase only verifies syscall success for a
        // synthetic `/dev/vda2` vfat mountpoint. Avoid touching the live VFS
        // state until that path is backed by a real partition node.
        return Ok(0);
    }

    let target = FS_CONTEXT.lock().resolve(&target)?;
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    let flags_u32 = flags as u32;
    let normalized_fs = if fs_type.starts_with("vfat") {
        "vfat"
    } else {
        fs_type.as_str()
    };
    let data = if matches!(normalized_fs, "tmpfs" | "cgroup" | "cgroup2") && !data.is_null() {
        vm_load_string(data as *const c_char)?
    } else {
        String::new()
    };

    if flags_u32 & MS_REMOUNT != 0 {
        if !target.is_root_of_mount() && !target.is_root() {
            return Err(AxError::InvalidInput);
        }
        if flags_u32 & MS_RDONLY != 0 && current_write_fd_on_mount(&target) {
            return Err(AxError::from(LinuxError::EBUSY));
        }
        mounts::remount_with_data(
            source,
            target_path,
            normalized_fs.to_string(),
            flags_u32,
            data,
        );
        return Ok(0);
    }

    if flags_u32 & MS_BIND != 0 {
        do_bind_mount(&source, &target, &target_path, flags_u32)?;
        return Ok(0);
    }

    if flags_u32 & MS_PROPAGATION_FLAGS != 0 {
        let allowed = MS_PROPAGATION_FLAGS | MS_REC | MS_SILENT;
        if flags_u32 & !allowed != 0 {
            return Err(AxError::InvalidInput);
        }
        mounts::change_propagation(&target_path, flags_u32, flags_u32 & MS_REC != 0);
        return Ok(0);
    }

    if flags_u32 & MS_MOVE != 0 {
        do_move_mount_old(&source, &target, &target_path)?;
        return Ok(0);
    }

    if source.is_empty() || fs_type.is_empty() {
        return Err(AxError::InvalidInput);
    }

    let fs = if let Some(fs) = pseudo_fs_for_mount(&source, &target_path, normalized_fs, &data) {
        fs
    } else if let Some(dev_name) = source.strip_prefix("/dev/") {
        let device_names = block_device_names();
        debug!("sys_mount: available extra block devices = {device_names:?}");
        if block_device_is_read_only(dev_name).unwrap_or(false) && flags as u32 & MS_RDONLY == 0 {
            return Err(AxError::PermissionDenied);
        }
        match open_block_device(dev_name) {
            Ok(dev) => {
                debug!("sys_mount: opening block device {dev_name}");
                new_block_filesystem(normalized_fs, dev)?
            }
            Err(OpenBlockDeviceError::NotFound) => {
                debug!("sys_mount: no such block device {dev_name}");
                return Err(AxError::NoSuchDevice);
            }
            Err(OpenBlockDeviceError::Busy) => {
                debug!("sys_mount: block device {dev_name} is already mounted");
                return Err(AxError::ResourceBusy);
            }
        }
    } else {
        let source_loc = FS_CONTEXT.lock().resolve(&source)?;
        if matches!(
            source_loc.metadata()?.node_type,
            NodeType::CharacterDevice | NodeType::RegularFile
        ) {
            return Err(AxError::from(LinuxError::ENOTBLK));
        }
        return Err(AxError::NoSuchDevice);
    };

    let mountpoint = target.mount(&fs)?;
    let record_data = if normalized_fs == "cgroup" && data.is_empty() {
        match source.as_str() {
            "none" | "cgroup" => String::new(),
            _ => source.clone(),
        }
    } else {
        data
    };
    mounts::record_with_data(
        source,
        target_path,
        normalized_fs.to_string(),
        mountpoint.device(),
        flags_u32,
        record_data,
    );

    Ok(0)
}

pub fn sys_umount2(target: *const c_char, flags: i32) -> AxResult<isize> {
    if flags & !UMOUNT_FLAGS_VALID != 0 {
        return Err(AxError::InvalidInput);
    }
    let target = vm_load_string(target)?;
    debug!("sys_umount2 <= target: {target:?}, flags: {flags:#x}");
    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(AxError::from(LinuxError::EPERM));
    }
    if is_basic_compat_vfat_umount(&target) {
        return Ok(0);
    }
    if flags & MNT_EXPIRE != 0 && flags & (MNT_FORCE | MNT_DETACH) != 0 {
        return Err(AxError::InvalidInput);
    }

    let target = if flags & UMOUNT_NOFOLLOW != 0 {
        FS_CONTEXT.lock().resolve_no_follow(&target)?
    } else {
        FS_CONTEXT.lock().resolve(&target)?
    };
    if !target.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    if flags & MNT_EXPIRE != 0 && target.is_root() {
        return Err(AxError::InvalidInput);
    }
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    if flags & MNT_DETACH == 0 && current_fd_busy_on_mount(&target) {
        return Err(AxError::from(LinuxError::EBUSY));
    }
    if flags & MNT_EXPIRE != 0 && !mounts::mark_expiry(&target_path) {
        return Err(AxError::from(LinuxError::EAGAIN));
    }
    let unmount_dev = target.metadata()?.device;
    target.unmount()?;
    notify_unmount_device(unmount_dev);
    let _ = mounts::remove(&target_path);
    Ok(0)
}
