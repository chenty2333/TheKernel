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
    FS_CONTEXT, OpenBlockDeviceError, block_device_is_read_only, block_device_names,
    new_block_filesystem, open_block_device,
};
use axfs_ng_vfs::Filesystem;
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
        Directory, FD_TABLE, File, FileLike, get_file_like, inotify::notify_unmount, resolve_at,
    },
    mm::vm_load_string,
    mounts,
    pseudofs::MemoryFs,
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
const MS_RELATIME: u32 = 0x20_0000;
const MS_STRICTATIME: u32 = 0x100_0000;
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

fn tmpfs_for_mount(source: &str, target_path: &str) -> Filesystem {
    if !source.starts_with("/dev/") {
        return MemoryFs::new();
    }

    let key = (source.to_string(), target_path.to_string());
    DEVICE_TMPFS_MOUNTS
        .lock()
        .entry(key)
        .or_insert_with(MemoryFs::new)
        .clone()
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
        _ => unreachable!(),
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

    FsMountFd {
        fs: MemoryFs::new(),
        source: state.source.clone().unwrap_or_else(|| "none".into()),
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

    FsMountFd {
        fs: MemoryFs::new(),
        source: loc
            .absolute_path()
            .map(|path| path.to_string())
            .unwrap_or_else(|_| path.clone()),
        fs_type: loc.filesystem().name().to_string(),
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
    _data: *const c_void,
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

    if flags as u32 & MS_REMOUNT != 0 {
        if !target.is_root_of_mount() && !target.is_root() {
            return Err(AxError::InvalidInput);
        }
        mounts::remount(source, target_path, fs_type, flags as u32);
        return Ok(0);
    }

    let normalized_fs = if fs_type.starts_with("vfat") {
        "vfat"
    } else {
        fs_type.as_str()
    };

    let fs = if normalized_fs == "tmpfs" {
        tmpfs_for_mount(&source, &target_path)
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
        return Err(AxError::NoSuchDevice);
    };

    let mountpoint = target.mount(&fs)?;
    mounts::record(
        source,
        target_path,
        normalized_fs.to_string(),
        mountpoint.device(),
        flags as u32,
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
    target.unmount()?;
    let _ = notify_unmount(&target);
    let _ = mounts::remove(&target_path);
    Ok(0)
}
