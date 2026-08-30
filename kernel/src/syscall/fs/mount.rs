use alloc::{borrow::Cow, string::String, sync::Arc};
use core::ffi::{c_char, c_void};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{
    FatMountOptions, FsContext, OpenBlockDeviceError, block_device_is_read_only,
    block_device_names, new_block_filesystem, new_block_filesystem_with_fat_options,
    open_block_device,
};
use axfs_ng_vfs::{
    DeviceId, DirEntry, Filesystem, FilesystemOps, NodePermission, NodeType, StatFs, VfsResult,
};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_NO_AUTOMOUNT, AT_RECURSIVE, AT_SYMLINK_NOFOLLOW, CAP_SYS_ADMIN, O_CLOEXEC,
    mount_attr,
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmPtr, vm_load_until_nul};

use crate::{
    file::{
        FileLike, Kstat, get_file_like,
        permission::{SecurityFsContextExt, VfsSecurityContext},
        resolve_at_with_security, with_path_fs,
    },
    mm::map_usercopy_error,
    mounts,
    pseudofs::{MemoryFs, cgroup},
    task::{AsThread, DacCredentialView, current_fs_context, fs_context_publication, try_tasks},
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
const MS_SYNCHRONOUS: u32 = 0x10;
const MS_REMOUNT: u32 = 0x20;
const MS_MANDLOCK: u32 = 0x40;
const MS_DIRSYNC: u32 = 0x80;
const MS_NOSYMFOLLOW: u32 = 0x100;
const MS_NOATIME: u32 = 0x400;
const MS_NODIRATIME: u32 = 0x800;
const MS_BIND: u32 = 0x1000;
const MS_MOVE: u32 = 0x2000;
const MS_REC: u32 = 0x4000;
const MS_SILENT: u32 = 0x8000;
const MS_POSIXACL: u32 = 1 << 16;
const MS_UNBINDABLE: u32 = 1 << 17;
const MS_PRIVATE: u32 = 1 << 18;
const MS_SLAVE: u32 = 1 << 19;
const MS_SHARED: u32 = 1 << 20;
const MS_RELATIME: u32 = 0x20_0000;
const MS_KERNMOUNT: u32 = 1 << 22;
const MS_I_VERSION: u32 = 1 << 23;
const MS_STRICTATIME: u32 = 0x100_0000;
const MS_LAZYTIME: u32 = 1 << 25;
const MS_INTERNAL_FLAGS: u32 = 0xfc00_0000;
const MS_MGC_VAL: u32 = 0xc0ed_0000;
const MS_MGC_MSK: u32 = 0xffff_0000;
const MS_PROPAGATION_FLAGS: u32 = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;
const MS_ATIME_FLAGS: u32 = MS_NOATIME | MS_NODIRATIME | MS_RELATIME | MS_STRICTATIME;
const MS_SUPPORTED_FLAGS: u32 = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_REMOUNT
    | MS_MANDLOCK
    | MS_NOSYMFOLLOW
    | MS_NOATIME
    | MS_NODIRATIME
    | MS_BIND
    | MS_MOVE
    | MS_REC
    | MS_SILENT
    | MS_PROPAGATION_FLAGS
    | MS_RELATIME
    | MS_STRICTATIME;
const MS_UNSUPPORTED_FLAGS: u32 =
    MS_SYNCHRONOUS | MS_DIRSYNC | MS_POSIXACL | MS_I_VERSION | MS_LAZYTIME;
const MS_INHERITED_BIND_FLAGS: u32 = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_NOSYMFOLLOW
    | MS_NOATIME
    | MS_NODIRATIME
    | MS_RELATIME
    | MS_STRICTATIME;
const MS_BIND_REMOUNT_FLAGS: u32 = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_NOSYMFOLLOW
    | MS_NOATIME
    | MS_NODIRATIME
    | MS_RELATIME
    | MS_STRICTATIME;
const MNT_FORCE: i32 = 0x1;
const MNT_DETACH: i32 = 0x2;
const MNT_EXPIRE: i32 = 0x4;
const UMOUNT_NOFOLLOW: i32 = 0x8;
const UMOUNT_FLAGS_VALID: i32 = MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x00000004;
const MOVE_MOUNT_T_SYMLINKS: u32 = 0x00000010;
const MOVE_MOUNT_T_EMPTY_PATH: u32 = 0x00000040;
const MOVE_MOUNT__MASK: u32 = 0x00000077;
const MOUNT_SETATTR_FLAGS: u32 =
    AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW;
const MOUNT_ATTR_SIZE_VER0: usize = core::mem::size_of::<mount_attr>();
const PAGE_SIZE: usize = 4096;

fn load_user_string<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
) -> AxResult<String> {
    String::from_utf8(vm_load_until_nul(memory, ptr.cast::<u8>()).map_err(map_usercopy_error)?)
        .map_err(|_| AxError::IllegalBytes)
}

fn try_string(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

struct FsOpenState {
    fs_type: String,
    source: Option<String>,
    data: String,
    config_len: usize,
    created: bool,
}

pub(crate) struct FsOpenFd(Mutex<FsOpenState>);

impl FileLike for FsOpenFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[fsopen]".into())
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // fsconfig operations are synchronous; FileDescription stores the
        // status bit for generic fcntl semantics.
        Ok(())
    }
}

impl Pollable for FsOpenFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut core::task::Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

pub(crate) struct FsMountFd {
    root: axfs_ng_vfs::Location,
}

struct BindFilesystem {
    root: DirEntry,
    name: String,
}

impl BindFilesystem {
    fn try_new(root: DirEntry, name: String, source: &Filesystem) -> AxResult<Filesystem> {
        let ops = Arc::try_new(Self { root, name }).map_err(|_| AxError::NoMemory)?;
        Filesystem::try_new_view(ops, source)
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
    if attr.propagation & !(MS_PROPAGATION_FLAGS as u64) != 0
        || attr.propagation.count_ones() > 1
        || attr.userns_fd != 0
    {
        return Err(AxError::InvalidInput);
    }
    if attr.propagation != 0 {
        return Err(AxError::OperationNotSupported);
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
    current().as_thread().has_effective_capability(cap)
}

fn validate_mount_flags(raw: i32) -> AxResult<u32> {
    let mut flags = raw as u32;
    if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags &= !MS_MGC_MSK;
    }
    if flags & (MS_KERNMOUNT | MS_INTERNAL_FLAGS) != 0
        || flags & !(MS_SUPPORTED_FLAGS | MS_UNSUPPORTED_FLAGS) != 0
    {
        return Err(AxError::InvalidInput);
    }
    if flags & MS_UNSUPPORTED_FLAGS != 0 {
        return Err(AxError::OperationNotSupported);
    }
    Ok(flags)
}

fn normalize_mount_atime(mut requested: u32, current: Option<u32>) -> u32 {
    if requested & MS_REMOUNT != 0
        && requested & MS_ATIME_FLAGS == 0
        && let Some(current) = current
    {
        requested |= current & MS_ATIME_FLAGS;
        return requested;
    }

    if requested & MS_NOATIME != 0 {
        requested &= !MS_RELATIME;
    } else {
        requested |= MS_RELATIME;
    }
    if requested & MS_STRICTATIME != 0 {
        requested &= !(MS_NOATIME | MS_RELATIME);
    }
    requested
}

fn block_device_name_for_rdev(rdev: DeviceId) -> AxResult<Option<String>> {
    if rdev == mounts::ROOT_BLOCK_DEVICE_ID {
        return Ok(Some(try_string(axfs::ROOT_BLOCK_DEVICE_NAME)?));
    }

    Ok(block_device_names()
        .into_iter()
        .enumerate()
        .find_map(|(index, name)| {
            (mounts::extra_block_device_id(index) == Some(rdev)).then_some(name)
        }))
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

fn parse_fat_mount_options(
    data: &str,
    credentials: &DacCredentialView,
    umask: u16,
) -> AxResult<FatMountOptions> {
    parse_fat_mount_options_with_defaults(
        data,
        credentials.uid().into_raw(),
        credentials.gid().into_raw(),
        umask,
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

fn tmpfs_for_mount(data: &str) -> AxResult<Filesystem> {
    MemoryFs::new_with_capacity(parse_tmpfs_size(data))
}

fn bind_mount_flags(
    source: &axfs_ng_vfs::Location,
    target: &axfs_ng_vfs::Location,
    requested: u32,
) -> AxResult<u32> {
    let source_flags = mounts::flags_for_location(source)?;
    let target_flags = mounts::flags_for_location(target)?;
    if (requested | source_flags | target_flags) & MS_PROPAGATION_FLAGS != 0 {
        return Err(AxError::OperationNotSupported);
    }
    Ok(source_flags & MS_INHERITED_BIND_FLAGS)
}

fn bind_filesystem_for(loc: &axfs_ng_vfs::Location, fs_type: &str) -> AxResult<Filesystem> {
    let source = loc.mountpoint().filesystem_handle();
    BindFilesystem::try_new(loc.entry().clone(), try_string(fs_type)?, &source)
}

fn do_bind_mount(
    source: &str,
    target: &axfs_ng_vfs::Location,
    flags: u32,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    if source.is_empty() {
        return Err(AxError::InvalidInput);
    }

    let source_loc = current_fs_context().lock().resolve_security(source, security)?;
    if source_loc.is_dir() != target.is_dir() {
        return Err(AxError::InvalidInput);
    }
    source_loc.check_is_dir()?;
    target.check_is_dir()?;

    let metadata = mounts::clone_metadata_for_bind(&source_loc)?;
    let fs = bind_filesystem_for(&source_loc, &metadata.fs_type)?;
    let mount_flags = bind_mount_flags(&source_loc, target, flags)?;
    let mountpoint = mounts::new_detached_with_flags(&fs, mount_flags, metadata)?;

    if flags & MS_REC != 0 {
        let detached_context = FsContext::new(mountpoint.root_location());
        let children = mounts::recursive_bind_submounts(&source_loc)?;
        for child in children {
            let child_target = detached_context
                .resolve(&child.relative_path)
                .map_err(|_| AxError::Io)?;
            let child_source = child.source;
            if child_source.is_dir() != child_target.is_dir() || !child_source.is_dir() {
                return Err(AxError::Io);
            }
            let child_fs = bind_filesystem_for(&child_source, &child.metadata.fs_type)?;
            let child_flags =
                bind_mount_flags(&child_source, &child_target, child.flags | MS_BIND | MS_REC)?;
            mounts::mount_with_flags(&child_target, &child_fs, child_flags, child.metadata)?;
        }
    }

    mounts::attach_tree_and_record(&mountpoint, target)?;
    Ok(())
}

fn do_move_mount_old(
    source: &str,
    target: &axfs_ng_vfs::Location,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    if source.is_empty() {
        return Err(AxError::InvalidInput);
    }

    let old = current_fs_context().lock().resolve_security(source, security)?;
    if !old.is_root_of_mount() || old.is_root() || old.is_dir() != target.is_dir() {
        return Err(AxError::InvalidInput);
    }
    mounts::move_tree_and_records(&old, target)?;
    Ok(())
}

fn pseudo_fs_for_mount(source: &str, fs_type: &str, data: &str) -> AxResult<Option<Filesystem>> {
    Ok(match fs_type {
        "tmpfs" => Some(tmpfs_for_mount(data)?),
        "cgroup" => Some(cgroup::new_cgroup_v1(cgroup::parse_v1_controllers(
            source, data,
        )?)?),
        "cgroup2" => Some(cgroup::new_cgroup_v2()?),
        _ => None,
    })
}

impl FileLike for FsMountFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[fsmount]".into())
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // This detached mount handle has no blocking data operation.
        Ok(())
    }
}

impl Pollable for FsMountFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut core::task::Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

pub fn sys_fsopen<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fs_name: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let fs_name = load_user_string(memory, fs_name)?;
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

pub fn sys_fsconfig<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
            let key = load_user_string(memory, key)?;
            let value = load_user_string(memory, value as *const c_char)?;
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
        _ => Err(LinuxError::EINVAL.into()),
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
    let source = match state.source.as_deref() {
        Some(source) => try_string(source)?,
        None => try_string("none")?,
    };
    let fs_type = try_string(&state.fs_type)?;
    let data = try_string(&state.data)?;
    drop(state);
    let fs = pseudo_fs_for_mount(&source, &fs_type, &data)?.ok_or(AxError::NoSuchDevice)?;
    let record_data = if fs_type == "cgroup"
        && data.is_empty()
        && !matches!(source.as_str(), "none" | "cgroup")
    {
        try_string(&source)?
    } else {
        data
    };
    let metadata = mounts::MountMetadata::new(source, fs_type, try_string("/")?, record_data);
    let mountpoint =
        mounts::new_detached_with_flags(&fs, mount_attr_to_mount_flags(mount_attrs), metadata)?;
    FsMountFd {
        root: mountpoint.root_location(),
    }
    .add_to_fd_table(flags & FSMOUNT_CLOEXEC != 0)
    .map(|new_fd| new_fd as isize)
}

pub fn sys_fspick(_dirfd: i32, _pathname: *const c_char, _flags: u32) -> AxResult<isize> {
    Err(AxError::OperationNotSupported)
}

pub fn sys_open_tree<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let path = load_user_string(memory, pathname)?;
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
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !security.has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
    }
    if flags & AT_RECURSIVE != 0 {
        return Err(AxError::OperationNotSupported);
    }

    let _mount_operation = mounts::namespace_operation();
    let loc = resolve_at_with_security(dirfd, Some(&path), flags, &security)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    loc.check_is_dir()?;

    let metadata = mounts::clone_metadata_for_bind(&loc)?;
    let filesystem = bind_filesystem_for(&loc, &metadata.fs_type)?;
    let mountpoint =
        mounts::new_detached_with_flags(&filesystem, mounts::flags_for_location(&loc)?, metadata)?;
    FsMountFd {
        root: mountpoint.root_location(),
    }
    .add_to_fd_table(flags & OPEN_TREE_CLOEXEC != 0)
    .map(|new_fd| new_fd as isize)
}

pub fn sys_mount_setattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
    attr: *const mount_attr,
    size: usize,
) -> AxResult<isize> {
    let path = load_user_string(memory, pathname)?;
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
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !security.has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
    }

    let attr = unsafe {
        attr.vm_read_uninit(memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    if attr.attr_set == 0 && attr.attr_clr == 0 && attr.propagation == 0 {
        return Ok(0);
    }

    let _mount_operation = mounts::namespace_operation();
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        let file = get_file_like(dirfd)?;
        if let Some(mount_fd) = file.downcast_ref::<FsMountFd>() {
            if mount_fd.root.mountpoint().is_attached() {
                if !mounts::try_update_flags_for_mounts(
                    mount_fd.root.mountpoint().mount_id(),
                    flags & AT_RECURSIVE != 0,
                    |current| apply_mount_attr_flags(current, attr),
                )? {
                    return Err(AxError::InvalidInput);
                }
            } else {
                mounts::update_detached_mount_flags(
                    mount_fd.root.mountpoint(),
                    flags & AT_RECURSIVE != 0,
                    |current| apply_mount_attr_flags(current, attr),
                )?;
            }
            return Ok(0);
        }
    }

    let loc = resolve_at_with_security(dirfd, Some(&path), flags, &security)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    if !loc.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    if !mounts::try_update_flags_for_mounts(
        loc.mountpoint().mount_id(),
        flags & AT_RECURSIVE != 0,
        |current| apply_mount_attr_flags(current, attr),
    )? {
        return Err(AxError::InvalidInput);
    }
    Ok(0)
}

pub fn sys_move_mount<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    from_dirfd: i32,
    from_pathname: *const c_char,
    to_dirfd: i32,
    to_pathname: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let from_path = load_user_string(memory, from_pathname)?;
    let to_path = load_user_string(memory, to_pathname)?;
    debug!(
        "sys_move_mount <= from_dirfd: {from_dirfd}, from_path: {from_path:?}, to_dirfd: \
         {to_dirfd}, to_path: {to_path:?}, flags: {flags:#x}"
    );

    if flags & !MOVE_MOUNT__MASK != 0 {
        return Err(AxError::InvalidInput);
    }
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !security.has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
    }
    if !from_path.is_empty() {
        return Err(AxError::OperationNotSupported);
    }
    if flags & MOVE_MOUNT_F_EMPTY_PATH == 0 {
        return Err(AxError::NotFound);
    }

    let _mount_operation = mounts::namespace_operation();
    let file = get_file_like(from_dirfd)?;
    let mount_fd = file
        .downcast_ref::<FsMountFd>()
        .ok_or(AxError::BadFileDescriptor)?;

    let target = if to_path.is_empty() {
        if flags & MOVE_MOUNT_T_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        resolve_at_with_security(to_dirfd, Some(""), AT_EMPTY_PATH, &security)?
            .into_file()
            .ok_or(AxError::InvalidInput)?
    } else {
        with_path_fs(to_dirfd, axfs_ng_vfs::path::Path::new(&to_path), |fs| {
            if flags & MOVE_MOUNT_T_SYMLINKS != 0 {
                fs.resolve_security(&to_path, &security)
            } else {
                fs.resolve_no_follow_security(&to_path, &security)
            }
        })?
    };
    if mount_fd.root.mountpoint().is_attached() {
        mounts::move_tree_and_records(&mount_fd.root, &target)?;
    } else {
        mounts::attach_tree_and_record(mount_fd.root.mountpoint(), &target)?;
    }
    Ok(0)
}

pub fn sys_mount<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    source: *const c_char,
    target: *const c_char,
    fs_type: *const c_char,
    flags: i32,
    data: *const c_void,
) -> AxResult<isize> {
    let source = if source.is_null() {
        String::new()
    } else {
        load_user_string(memory, source)?
    };
    let target = load_user_string(memory, target)?;
    let fs_type = if fs_type.is_null() {
        String::new()
    } else {
        load_user_string(memory, fs_type)?
    };
    debug!("sys_mount <= source: {source:?}, target: {target:?}, fs_type: {fs_type:?}");

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let credentials = security.credentials();
    let umask = current_fs_context().lock().umask() as u16;
    if !security.has_capability(CAP_SYS_ADMIN) {
        return Err(AxError::from(LinuxError::EPERM));
    }
    let flags_u32 = validate_mount_flags(flags)?;
    let _mount_operation = mounts::namespace_operation();
    let target = current_fs_context().lock().resolve_security(&target, &security)?;
    let normalized_fs = if fs_type.starts_with("vfat") {
        "vfat"
    } else {
        fs_type.as_str()
    };
    let data_is_ignored = flags_u32 & (MS_BIND | MS_MOVE | MS_PROPAGATION_FLAGS) != 0;
    let data = if !data_is_ignored && !data.is_null() {
        load_user_string(memory, data as *const c_char)?
    } else {
        String::new()
    };

    if flags_u32 & MS_REMOUNT != 0 {
        if !target.is_root_of_mount() {
            return Err(AxError::InvalidInput);
        }
        if flags_u32 & (MS_MOVE | MS_PROPAGATION_FLAGS) != 0 {
            return Err(AxError::OperationNotSupported);
        }
        let current_flags = mounts::flags_for_location(&target)?;
        if flags_u32 & MS_BIND != 0 {
            if flags_u32 & MS_REC != 0 {
                return Err(AxError::OperationNotSupported);
            }
            let allowed = MS_REMOUNT | MS_BIND | MS_SILENT | MS_BIND_REMOUNT_FLAGS;
            if flags_u32 & !allowed != 0 || !data.is_empty() {
                return Err(AxError::OperationNotSupported);
            }
            let bind_flags =
                normalize_mount_atime(flags_u32, Some(current_flags)) & MS_BIND_REMOUNT_FLAGS;
            mounts::remount_with_data(
                &target,
                source,
                try_string(normalized_fs)?,
                bind_flags,
                data,
            )?;
            return Ok(0);
        }
        let mut remount_flags = normalize_mount_atime(flags_u32, Some(current_flags));
        remount_flags &= !(MS_REMOUNT | MS_SILENT | MS_REC);
        if (remount_flags ^ current_flags) & MS_RDONLY != 0 {
            return Err(AxError::OperationNotSupported);
        }
        if !data.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        mounts::remount_with_data(
            &target,
            source,
            try_string(normalized_fs)?,
            remount_flags,
            data,
        )?;
        return Ok(0);
    }

    if flags_u32 & MS_BIND != 0 {
        do_bind_mount(&source, &target, flags_u32, &security)?;
        return Ok(0);
    }

    if flags_u32 & MS_PROPAGATION_FLAGS != 0 {
        let allowed = MS_PROPAGATION_FLAGS | MS_REC | MS_SILENT;
        if flags_u32 & !allowed != 0 || (flags_u32 & MS_PROPAGATION_FLAGS).count_ones() != 1 {
            return Err(AxError::InvalidInput);
        }
        return Err(AxError::OperationNotSupported);
    }

    if flags_u32 & MS_MOVE != 0 {
        do_move_mount_old(&source, &target, &security)?;
        return Ok(0);
    }

    if source.is_empty() || fs_type.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let mount_flags = normalize_mount_atime(flags_u32, None) & !(MS_REC | MS_SILENT);

    let (fs, linux_device) = if let Some(fs) = pseudo_fs_for_mount(&source, normalized_fs, &data)? {
        (fs, None)
    } else {
        let source_loc = current_fs_context().lock().resolve_security(&source, &security)?;
        let metadata = source_loc.metadata()?;
        if metadata.node_type != NodeType::BlockDevice {
            return Err(AxError::from(LinuxError::ENOTBLK));
        }

        if mounts::is_nodev(&source_loc)? {
            return Err(AxError::PermissionDenied);
        }

        let linux_device = metadata.rdev;
        let dev_name = block_device_name_for_rdev(linux_device)?.ok_or(AxError::NoSuchDevice)?;
        match open_block_device(&dev_name) {
            Ok(dev) => {
                debug!("sys_mount: opening block device {dev_name}");
                let read_only =
                    block_device_is_read_only(&dev_name).ok_or(AxError::NoSuchDevice)?;
                if read_only && mount_flags & MS_RDONLY == 0 {
                    return Err(AxError::PermissionDenied);
                }
                let fs = if normalized_fs == "vfat" {
                    new_block_filesystem_with_fat_options(
                        normalized_fs,
                        dev,
                        parse_fat_mount_options(&data, credentials, umask)?,
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
    };

    let record_data = if normalized_fs == "cgroup" && data.is_empty() {
        match source.as_str() {
            "none" | "cgroup" => String::new(),
            _ => try_string(&source)?,
        }
    } else {
        data
    };
    let metadata = mounts::MountMetadata::new(
        source,
        try_string(normalized_fs)?,
        try_string("/")?,
        record_data,
    );
    let mountpoint = mounts::new_detached_with_flags(&fs, mount_flags, metadata)?;
    if let Some(linux_device) = linux_device {
        mounts::register_linux_device(mountpoint.filesystem_identity(), linux_device)?;
    }
    mounts::attach_tree_and_record(&mountpoint, &target)?;

    Ok(0)
}

pub fn sys_pivot_root<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    new_root: *const c_char,
    put_old: *const c_char,
) -> AxResult<isize> {
    // Linux checks namespace authority before either pathname copy.  Besides
    // matching the observable EPERM/EFAULT order, this avoids user-memory
    // access for callers that cannot mount in the first place.
    if !current_has_capability(CAP_SYS_ADMIN) {
        return Err(LinuxError::EPERM.into());
    }
    let new_root = load_user_string(memory, new_root)?;
    if new_root.is_empty() {
        return Err(AxError::NotFound);
    }

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let _mount_operation = mounts::namespace_operation();
    // Freeze fs_struct cloning/replacement before resolving the transaction
    // and retain it until every live old-root reference has moved.
    let _fs_context_publication = fs_context_publication();
    // Resolve in Linux order while the namespace topology is frozen.  The
    // security-aware walk applies DAC and registered inode security hooks to
    // both paths before the topology commit.
    let fs_context = current_fs_context();
    let fs = fs_context.lock();
    let old_root = fs.root_dir().clone();
    let new_root_loc = fs.resolve_security(&new_root, &security)?;
    // LOOKUP_DIRECTORY belongs to the new-root walk. Linux returns ENOTDIR
    // here before it reads or resolves put_old.
    new_root_loc.check_is_dir()?;
    drop(fs);

    let put_old = load_user_string(memory, put_old)?;
    if put_old.is_empty() {
        return Err(AxError::NotFound);
    }
    debug!("sys_pivot_root <= new_root: {new_root:?}, put_old: {put_old:?}");
    let put_old_loc = fs_context.lock().resolve_security(&put_old, &security)?;
    put_old_loc.check_is_dir()?;
    // Keep every live task pinned before the irreversible topology commit.
    // The subsequent per-context updates are allocation-free, matching
    // chroot_fs_refs(): only root/cwd references exactly at the old root move.
    let tasks = try_tasks()?;
    mounts::pivot_root_and_records(&old_root, &new_root_loc, &put_old_loc)?;
    for task in tasks {
        if let Some(thread) = task.try_as_thread() {
            // A task may have completed exit after its weak registry entry was
            // snapshotted. The publication gate prevents retirement after a
            // successful acquisition; an already-retired exiting task has no
            // user-visible fs references left to update.
            if let Some(fs_context) = thread.try_fs_context() {
                fs_context
                    .lock()
                    .pivot_root_refs(&old_root, &new_root_loc);
            }
        }
    }
    Ok(0)
}

pub fn sys_umount2<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    target: *const c_char,
    flags: i32,
) -> AxResult<isize> {
    if flags & !UMOUNT_FLAGS_VALID != 0 {
        return Err(AxError::InvalidInput);
    }
    let target = load_user_string(memory, target)?;
    debug!("sys_umount2 <= target: {target:?}, flags: {flags:#x}");
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !security.has_capability(CAP_SYS_ADMIN) {
        return Err(AxError::from(LinuxError::EPERM));
    }
    if flags & MNT_EXPIRE != 0 && flags & (MNT_FORCE | MNT_DETACH) != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MNT_FORCE != 0 {
        return Err(AxError::OperationNotSupported);
    }

    let _mount_operation = mounts::namespace_operation();
    let target = if flags & UMOUNT_NOFOLLOW != 0 {
        current_fs_context()
            .lock()
            .resolve_no_follow_security_unobserved(&target, &security)?
    } else {
        current_fs_context()
            .lock()
            .resolve_security_unobserved(&target, &security)?
    };
    if !target.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    if target.is_root() {
        return Err(AxError::from(LinuxError::EBUSY));
    }
    mounts::unmount_and_remove_records(target, flags & MNT_DETACH != 0, flags & MNT_EXPIRE != 0)?;
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

    #[test]
    fn mount_flag_validation_separates_invalid_and_unsupported_bits() {
        assert_eq!(
            validate_mount_flags(MS_KERNMOUNT as i32).unwrap_err(),
            AxError::InvalidInput
        );
        assert_eq!(
            validate_mount_flags(MS_SYNCHRONOUS as i32).unwrap_err(),
            AxError::OperationNotSupported
        );
        assert_eq!(
            validate_mount_flags((MS_MGC_VAL | MS_RDONLY) as i32).unwrap(),
            MS_RDONLY
        );
    }

    #[test]
    fn mount_atime_normalization_defaults_preserves_and_prioritizes_strict() {
        assert_eq!(
            normalize_mount_atime(MS_NODEV, None) & MS_RELATIME,
            MS_RELATIME
        );
        assert_eq!(
            normalize_mount_atime(MS_REMOUNT | MS_NODEV, Some(MS_NOATIME)) & MS_ATIME_FLAGS,
            MS_NOATIME
        );
        let strict = normalize_mount_atime(MS_NOATIME | MS_RELATIME | MS_STRICTATIME, None);
        assert_ne!(strict & MS_STRICTATIME, 0);
        assert_eq!(strict & (MS_NOATIME | MS_RELATIME), 0);
    }
}
