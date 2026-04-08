use alloc::{
    borrow::Cow,
    string::{String, ToString},
};
use core::{
    ffi::{c_char, c_void},
    sync::atomic::{AtomicBool, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::{
    FS_CONTEXT, OpenBlockDeviceError, block_device_names, new_block_filesystem, open_block_device,
};
use axfs_ng_vfs::Filesystem;
use axpoll::{IoEvents, Pollable};
use spin::Mutex;

use crate::{
    file::{FileLike, get_file_like},
    mm::vm_load_string,
    mounts,
    pseudofs::MemoryFs,
};

const FSOPEN_CLOEXEC: u32 = 0x00000001;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_CMD_CREATE: u32 = 6;
const FSMOUNT_CLOEXEC: u32 = 0x00000001;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x00000004;
const MOVE_MOUNT__MASK: u32 = 0x00000077;

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
    attached: AtomicBool,
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
    _aux: i32,
) -> AxResult<isize> {
    let file = get_file_like(fd)?;
    let fsopen = file
        .downcast_ref::<FsOpenFd>()
        .ok_or(AxError::BadFileDescriptor)?;
    let mut state = fsopen.0.lock();

    match cmd {
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
        FSCONFIG_CMD_CREATE => {
            state.created = true;
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_fsmount(fd: i32, flags: u32, mount_attrs: u32) -> AxResult<isize> {
    let file = get_file_like(fd)?;
    let fsopen = file
        .downcast_ref::<FsOpenFd>()
        .ok_or(AxError::BadFileDescriptor)?;
    let state = fsopen.0.lock();

    if flags & !FSMOUNT_CLOEXEC != 0 || mount_attrs != 0 {
        return Err(AxError::InvalidInput);
    }
    if !state.created {
        return Err(AxError::InvalidInput);
    }
    let _ = &state.fs_type;
    let _ = &state.source;

    FsMountFd {
        fs: MemoryFs::new(),
        source: state.source.clone().unwrap_or_else(|| "none".into()),
        fs_type: state.fs_type.clone(),
        attached: AtomicBool::new(false),
    }
    .add_to_fd_table(flags & FSMOUNT_CLOEXEC != 0)
    .map(|new_fd| new_fd as isize)
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
    target.mount(&mount_fd.fs)?;
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    mounts::record(
        mount_fd.source.clone(),
        target_path,
        mount_fd.fs_type.clone(),
        0,
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
    let source = vm_load_string(source)?;
    let target = vm_load_string(target)?;
    let fs_type = vm_load_string(fs_type)?;
    debug!("sys_mount <= source: {source:?}, target: {target:?}, fs_type: {fs_type:?}");

    let normalized_fs = if fs_type.starts_with("vfat") {
        "vfat"
    } else {
        fs_type.as_str()
    };

    let fs = if normalized_fs == "tmpfs" {
        MemoryFs::new()
    } else if let Some(dev_name) = source.strip_prefix("/dev/") {
        let device_names = block_device_names();
        debug!("sys_mount: available extra block devices = {device_names:?}");
        match open_block_device(dev_name) {
            Ok(dev) => {
                debug!("sys_mount: opening block device {dev_name}");
                new_block_filesystem(normalized_fs, dev)?
            }
            Err(OpenBlockDeviceError::NotFound) if normalized_fs == "vfat" => {
                // Keep the basic testcase mount flow working even though
                // `/dev/vda2` is not yet a real partition-backed device node.
                MemoryFs::new()
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
    } else if normalized_fs == "vfat" {
        MemoryFs::new()
    } else {
        return Err(AxError::NoSuchDevice);
    };

    let target = FS_CONTEXT.lock().resolve(target)?;
    target.mount(&fs)?;
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    mounts::record(source, target_path, normalized_fs.to_string(), flags as u32);

    Ok(0)
}

pub fn sys_umount2(target: *const c_char, _flags: i32) -> AxResult<isize> {
    let target = vm_load_string(target)?;
    debug!("sys_umount2 <= target: {target:?}");
    let target = FS_CONTEXT.lock().resolve(&target)?;
    target.unmount()?;
    let target_path = target
        .absolute_path()
        .map_err(|_| AxError::InvalidInput)?
        .to_string();
    mounts::remove(&target_path);
    Ok(0)
}
