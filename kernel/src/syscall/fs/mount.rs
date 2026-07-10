use alloc::{
    borrow::Cow,
    string::{String, ToString},
    sync::Arc,
};
use core::{
    ffi::{c_char, c_void},
    sync::atomic::{AtomicBool, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{
    FS_CONTEXT, FatMountOptions, FileFlags, OpenBlockDeviceError, block_device_is_read_only,
    block_device_names, new_block_filesystem, new_block_filesystem_with_fat_options,
    open_block_device,
};
use axfs_ng_vfs::{
    DirEntry, Filesystem, FilesystemOps, NodePermission, NodeType, StatFs, VfsResult,
};
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
        FD_TABLE, File, FileLike, Kstat, get_file_like, inotify::notify_unmount_device, resolve_at,
    },
    mm::vm_load_string,
    mounts,
    pseudofs::{MemoryFs, cgroup},
    task::AsThread,
};

const FSOPEN_CLOEXEC: u32 = 0x00000001;
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

struct FsOpenState {
    fs_type: String,
    source: Option<String>,
    data: String,
    config_len: usize,
    created: bool,
}

struct FsOpenFd(Mutex<FsOpenState>);

impl FileLike for FsOpenFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }

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
    fn new(root: DirEntry, name: String, device: u64) -> Filesystem {
        Filesystem::new_with_device(Arc::new(Self { root, name }), device)
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

fn current_has_capability(cap: u32) -> bool {
    current()
        .as_thread()
        .proc_data
        .has_effective_capability(cap)
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

fn parse_fat_mask(value: &str) -> Option<u16> {
    let value = value.strip_prefix("0o").unwrap_or(value);
    u16::from_str_radix(value, 8)
        .ok()
        .filter(|mask| *mask & !0o777 == 0)
}

fn parse_fat_mount_options(data: &str) -> AxResult<FatMountOptions> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    parse_fat_mount_options_with_defaults(
        data,
        proc_data.fsuid(),
        proc_data.fsgid(),
        proc_data.umask() as u16,
    )
}

fn parse_fat_mount_options_with_defaults(
    data: &str,
    mut uid: u32,
    mut gid: u32,
    umask: u16,
) -> AxResult<FatMountOptions> {
    let mut file_mask = umask & 0o777;
    let mut dir_mask = file_mask;

    for raw in data.split(',') {
        let option = raw.trim();
        if option.is_empty()
            || matches!(
                option,
                "rw" | "ro"
                    | "suid"
                    | "nosuid"
                    | "dev"
                    | "nodev"
                    | "exec"
                    | "noexec"
                    | "atime"
                    | "noatime"
                    | "diratime"
                    | "nodiratime"
                    | "relatime"
                    | "strictatime"
            )
        {
            continue;
        }

        let Some((key, value)) = option.split_once('=') else {
            return Err(AxError::OperationNotSupported);
        };
        match key {
            "uid" => uid = value.parse().map_err(|_| AxError::InvalidInput)?,
            "gid" => gid = value.parse().map_err(|_| AxError::InvalidInput)?,
            "umask" => {
                let mask = parse_fat_mask(value).ok_or(AxError::InvalidInput)?;
                file_mask = mask;
                dir_mask = mask;
            }
            "fmask" => file_mask = parse_fat_mask(value).ok_or(AxError::InvalidInput)?,
            "dmask" => dir_mask = parse_fat_mask(value).ok_or(AxError::InvalidInput)?,
            _ => return Err(AxError::OperationNotSupported),
        }
    }

    Ok(FatMountOptions {
        uid,
        gid,
        file_mode: NodePermission::from_bits_truncate(0o777 & !file_mask),
        dir_mode: NodePermission::from_bits_truncate(0o777 & !dir_mask),
    })
}

fn tmpfs_for_mount(data: &str) -> Filesystem {
    MemoryFs::new_with_capacity(parse_tmpfs_size(data))
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
    BindFilesystem::new(
        loc.entry().clone(),
        fs_type.to_string(),
        loc.mountpoint().device(),
    )
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
        let parent_id = alias_target.mountpoint().mount_id();
        let mountpoint = alias_target.mount(&fs)?;
        let alias_flags = bind_mount_flags(source_path, &alias, mount_flags);
        mounts::record(
            source_path.to_string(),
            alias,
            fs_type.to_string(),
            source_loc
                .path_in_mount()
                .map_err(|_| AxError::InvalidInput)?
                .to_string(),
            mountpoint.device(),
            mountpoint.mount_id(),
            parent_id,
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
    let parent_id = target.mountpoint().mount_id();
    let mountpoint = target.mount(&fs)?;
    let mount_flags = bind_mount_flags(&source_path, target_path, flags);
    mounts::record(
        source_path.clone(),
        target_path.to_string(),
        fs_type.clone(),
        source_loc
            .path_in_mount()
            .map_err(|_| AxError::InvalidInput)?
            .to_string(),
        mountpoint.device(),
        mountpoint.mount_id(),
        parent_id,
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
        let child_parent_id = child_target.mountpoint().mount_id();
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
            child_source
                .path_in_mount()
                .map_err(|_| AxError::InvalidInput)?
                .to_string(),
            child_mountpoint.device(),
            child_mountpoint.mount_id(),
            child_parent_id,
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
    let new_parent_id = target.mountpoint().mount_id();
    old.move_mount_to(target)?;
    mounts::move_tree(&old_path, target_path, new_parent_id);
    Ok(())
}

fn pseudo_fs_for_mount(source: &str, fs_type: &str, data: &str) -> AxResult<Option<Filesystem>> {
    Ok(match fs_type {
        "tmpfs" => Some(tmpfs_for_mount(data)),
        "cgroup" => Some(cgroup::new_cgroup_v1(cgroup::parse_v1_controllers(
            source, data,
        )?)),
        "cgroup2" => Some(cgroup::new_cgroup_v2()),
        _ => None,
    })
}

impl FileLike for FsMountFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }

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
    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
    }
    if !matches!(fs_name.as_str(), "tmpfs" | "cgroup" | "cgroup2") {
        return Err(AxError::NoSuchDevice);
    }

    FsOpenFd(Mutex::new(FsOpenState {
        fs_type: fs_name,
        source: None,
        data: String::new(),
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

    if state.created
        && matches!(
            cmd,
            FSCONFIG_SET_FLAG
                | FSCONFIG_SET_STRING
                | FSCONFIG_SET_BINARY
                | FSCONFIG_SET_PATH
                | FSCONFIG_SET_PATH_EMPTY
                | FSCONFIG_SET_FD
        )
    {
        return Err(AxError::ResourceBusy);
    }

    match cmd {
        FSCONFIG_SET_FLAG => Err(AxError::OperationNotSupported),
        FSCONFIG_SET_STRING => {
            let key = vm_load_string(key)?;
            let value = vm_load_string(value as *const c_char)?;
            let entry_len = key.len() + value.len() + 2;
            if state.config_len.saturating_add(entry_len) > 4096 {
                return Err(AxError::InvalidInput);
            }
            match (state.fs_type.as_str(), key.as_str()) {
                (_, "source") => state.source = Some(value),
                ("tmpfs", "size") => {
                    if parse_tmpfs_size_component(&value).is_none() {
                        return Err(AxError::InvalidInput);
                    }
                    state.data = alloc::format!("size={value}");
                }
                _ => return Err(AxError::OperationNotSupported),
            }
            state.config_len += entry_len;
            Ok(0)
        }
        FSCONFIG_SET_BINARY | FSCONFIG_SET_PATH | FSCONFIG_SET_PATH_EMPTY | FSCONFIG_SET_FD => {
            Err(AxError::OperationNotSupported)
        }
        FSCONFIG_CMD_CREATE | FSCONFIG_CMD_CREATE_EXCL => {
            if state.created {
                return Err(AxError::ResourceBusy);
            }
            state.created = true;
            Ok(0)
        }
        FSCONFIG_CMD_RECONFIGURE => Err(AxError::OperationNotSupported),
        _ => return Err(LinuxError::EINVAL.into()),
    }
}

pub fn sys_fsmount(fd: i32, flags: u32, mount_attrs: u32) -> AxResult<isize> {
    if fd < 0 {
        return Err(AxError::BadFileDescriptor);
    }
    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
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
    let source = state.source.clone().unwrap_or_else(|| "none".into());
    let fs =
        pseudo_fs_for_mount(&source, &state.fs_type, &state.data)?.ok_or(AxError::NoSuchDevice)?;
    FsMountFd {
        fs,
        source,
        fs_type: state.fs_type.clone(),
        flags: Mutex::new(mount_attr_to_mount_flags(mount_attrs)),
        attached: AtomicBool::new(false),
    }
    .add_to_fd_table(flags & FSMOUNT_CLOEXEC != 0)
    .map(|new_fd| new_fd as isize)
}

pub fn sys_fspick(_dirfd: i32, _pathname: *const c_char, _flags: u32) -> AxResult<isize> {
    Err(AxError::OperationNotSupported)
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
    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
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
    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
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
    if !mounts::update_flags_for_path(path.as_ref(), apply_mount_attr_flags(current, attr)?) {
        return Err(AxError::InvalidInput);
    }
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
    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
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
    let parent_id = target.mountpoint().mount_id();
    let mountpoint = target.mount(&mount_fd.fs)?;
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    mounts::record(
        mount_fd.source.clone(),
        target_path,
        mount_fd.fs_type.clone(),
        "/".to_string(),
        mountpoint.device(),
        mountpoint.mount_id(),
        parent_id,
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
    let data_is_ignored = flags_u32 & (MS_BIND | MS_MOVE | MS_PROPAGATION_FLAGS) != 0;
    let data = if !data_is_ignored && !data.is_null() {
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
        if !data.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        if !mounts::remount_with_data(
            source,
            target_path,
            normalized_fs.to_string(),
            flags_u32,
            data,
        ) {
            return Err(AxError::InvalidInput);
        }
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

    let (fs, linux_device) = if let Some(fs) = pseudo_fs_for_mount(&source, normalized_fs, &data)? {
        (fs, None)
    } else if let Some(dev_name) = source.strip_prefix("/dev/") {
        let device_names = block_device_names();
        debug!("sys_mount: available extra block devices = {device_names:?}");
        let device_index = device_names
            .iter()
            .position(|name| name == dev_name)
            .ok_or(AxError::NoSuchDevice)?;
        let linux_device =
            mounts::extra_block_device_id(device_index).ok_or(AxError::InvalidInput)?;
        if block_device_is_read_only(dev_name).unwrap_or(false) && flags as u32 & MS_RDONLY == 0 {
            return Err(AxError::PermissionDenied);
        }
        match open_block_device(dev_name) {
            Ok(dev) => {
                debug!("sys_mount: opening block device {dev_name}");
                let fs = if normalized_fs == "vfat" {
                    new_block_filesystem_with_fat_options(
                        normalized_fs,
                        dev,
                        parse_fat_mount_options(&data)?,
                    )?
                } else {
                    if !data.is_empty() {
                        return Err(AxError::OperationNotSupported);
                    }
                    new_block_filesystem(normalized_fs, dev)?
                };
                (fs, Some(linux_device))
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

    let parent_id = target.mountpoint().mount_id();
    let mountpoint = target.mount(&fs)?;
    if let Some(linux_device) = linux_device {
        mounts::register_linux_device(
            mountpoint.device(),
            linux_device,
            mountpoint.filesystem_lifetime(),
        );
    }
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
        "/".to_string(),
        mountpoint.device(),
        mountpoint.mount_id(),
        parent_id,
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
    if flags & MNT_EXPIRE != 0 && flags & (MNT_FORCE | MNT_DETACH) != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MNT_FORCE != 0 {
        return Err(AxError::OperationNotSupported);
    }

    let target = if flags & UMOUNT_NOFOLLOW != 0 {
        FS_CONTEXT.lock().resolve_no_follow(&target)?
    } else {
        FS_CONTEXT.lock().resolve(&target)?
    };
    if !target.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    if target.is_root() {
        return Err(AxError::from(LinuxError::EBUSY));
    }
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    if flags & MNT_EXPIRE != 0 && !mounts::mark_expiry(&target_path) {
        return Err(AxError::from(LinuxError::EAGAIN));
    }
    let mount_id = target.mountpoint().mount_id();
    let unmount_devices = target.mountpoint().subtree_devices();
    if flags & MNT_DETACH != 0 {
        target.lazy_unmount()?;
    } else {
        target.unmount()?;
    }
    mounts::remove_subtree(mount_id);
    for device in unmount_devices {
        notify_unmount_device(device);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat_mount_defaults_follow_mounting_process_identity_and_umask() {
        let options = parse_fat_mount_options_with_defaults("", 1000, 100, 0o022).unwrap();
        assert_eq!(options.uid, 1000);
        assert_eq!(options.gid, 100);
        assert_eq!(options.file_mode.bits(), 0o755);
        assert_eq!(options.dir_mode.bits(), 0o755);
    }

    #[test]
    fn fat_mount_masks_are_parsed_independently() {
        let options =
            parse_fat_mount_options_with_defaults("uid=42,gid=7,fmask=0133,dmask=0027", 0, 0, 0)
                .unwrap();
        assert_eq!(options.uid, 42);
        assert_eq!(options.gid, 7);
        assert_eq!(options.file_mode.bits(), 0o644);
        assert_eq!(options.dir_mode.bits(), 0o750);
    }

    #[test]
    fn fat_mount_rejects_unimplemented_or_invalid_options() {
        assert_eq!(
            parse_fat_mount_options_with_defaults("iocharset=utf8", 0, 0, 0).unwrap_err(),
            AxError::OperationNotSupported
        );
        assert_eq!(
            parse_fat_mount_options_with_defaults("umask=0899", 0, 0, 0).unwrap_err(),
            AxError::InvalidInput
        );
    }
}
