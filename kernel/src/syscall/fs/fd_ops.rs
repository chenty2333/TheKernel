use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
};
use core::{
    ffi::{c_char, c_int},
    ops::Deref,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FileBackend, OpenOptions, OpenResult};
use axfs_ng_vfs::{
    DirEntry, FileNode, Location, MetadataUpdate, NodePermission, NodeType, Reference, path::Path,
};
use axtask::current;
use bitflags::bitflags;
use linux_raw_sys::general::*;
use spin::RwLock;

use crate::{
    file::{
        Directory, FD_TABLE, File, FileDescriptor, FileLike, Pipe, add_file_like, close_file_like,
        get_file_description, get_file_like,
        inotify::{
            location_for_fd, notify_close, notify_exact, notify_parent, notify_parent_with_name,
        },
        lease,
        permission::{check_create_permissions, check_open_permissions},
        with_path_fs,
    },
    mm::{UserConstPtr, UserPtr, vm_load_string},
    pseudofs::{Device, dev::tty},
    syscall::{
        fs::ctl::validate_pathname,
        sys::{sys_getegid, sys_geteuid},
    },
    task::{AX_FILE_LIMIT, AsThread},
};

/// Convert open flags to [`OpenOptions`].
fn flags_to_options(flags: c_int, mode: __kernel_mode_t, (uid, gid): (u32, u32)) -> OpenOptions {
    let flags = flags as u32;
    let mut options = OpenOptions::new();
    options.mode(mode).user(uid, gid);
    if flags & O_PATH != 0 {
        options.path(true);
    } else {
        match flags & 0b11 {
            O_RDONLY => options.read(true),
            O_WRONLY => options.write(true),
            _ => options.read(true).write(true),
        };
        if flags & O_APPEND != 0 {
            options.append(true);
        }
        if flags & O_TRUNC != 0 {
            options.truncate(true);
        }
        if flags & O_CREAT != 0 {
            options.create(true);
        }
        if flags & O_EXCL != 0 {
            options.create_new(true);
        }
        if flags & O_DIRECT != 0 {
            options.direct(true);
        }
    }
    if flags & O_DIRECTORY != 0 {
        options.directory(true);
    }
    if flags & O_NOFOLLOW != 0 {
        options.no_follow(true);
    }
    options
}

fn open_access_mask(flags: c_int) -> u32 {
    if flags as u32 & O_PATH != 0 {
        return 0;
    }
    let mut mask = match flags as u32 & O_ACCMODE {
        O_RDONLY => R_OK,
        O_WRONLY => W_OK,
        _ => R_OK | W_OK,
    };
    if flags as u32 & O_TRUNC != 0 {
        mask |= W_OK;
    }
    mask
}

fn invalid_directory_open(flags: c_int) -> bool {
    let flags = flags as u32;
    (flags & O_ACCMODE) != O_RDONLY || (flags & (O_CREAT | O_TRUNC)) != 0
}

fn enforce_special_open_rules(loc: &Location, flags: c_int, uid: u32) -> AxResult {
    let flags = flags as u32;
    let metadata = loc.metadata()?;

    if flags & O_NOATIME != 0 && uid != 0 && uid != metadata.uid {
        return Err(AxError::OperationNotPermitted);
    }
    if flags & O_NOFOLLOW != 0 && flags & O_PATH == 0 && metadata.node_type == NodeType::Symlink {
        return Err(AxError::from(LinuxError::ELOOP));
    }

    Ok(())
}

static TMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_tmpfile_path(dir: &str) -> String {
    let counter = TMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let suffix = format!(".tmpfile-{counter:016x}");
    if dir.starts_with('/') && dir.trim_matches('/').is_empty() {
        return format!("/{suffix}");
    }
    match dir.trim_end_matches('/') {
        "" | "." => format!("./{suffix}"),
        root => format!("{root}/{suffix}"),
    }
}

fn add_to_fd(result: OpenResult, flags: u32) -> AxResult<i32> {
    let f: Arc<dyn FileLike> = match result {
        OpenResult::File(mut file) => {
            if flags & O_PATH == 0 && file.location().metadata()?.node_type == NodeType::Fifo {
                Arc::new(crate::file::pipe::NamedPipe::open(
                    file.location().clone(),
                    flags,
                )?)
            } else {
            // /dev/xx handling
                if let Ok(device) = file.location().entry().downcast::<Device>() {
                    let inner = device.inner().as_any();
                    if let Some(ptmx) = inner.downcast_ref::<tty::Ptmx>() {
                        // Opening /dev/ptmx creates a new pseudo-terminal
                        let (master, pty_number) = ptmx.create_pty()?;
                        // TODO: this is cursed
                        let pts = FS_CONTEXT.lock().resolve("/dev/pts")?;
                        let entry = DirEntry::new_file(
                            FileNode::new(master),
                            NodeType::CharacterDevice,
                            Reference::new(Some(pts.entry().clone()), pty_number.to_string()),
                        );
                        let loc = Location::new(file.location().mountpoint().clone(), entry);
                        file = axfs::File::new(FileBackend::Direct(loc), file.flags());
                    } else if inner.is::<tty::CurrentTty>() {
                        let term = current()
                            .as_thread()
                            .proc_data
                            .proc
                            .group()
                            .session()
                            .terminal()
                            .ok_or(AxError::NotFound)?;
                        let path = if term.is::<tty::NTtyDriver>() {
                            "/dev/console".to_string()
                        } else if let Some(pts) = term.downcast_ref::<tty::PtyDriver>() {
                            format!("/dev/pts/{}", pts.pty_number())
                        } else {
                            panic!("unknown terminal type")
                        };
                        let loc = FS_CONTEXT.lock().resolve(&path)?;
                        file = axfs::File::new(FileBackend::Direct(loc), file.flags());
                    }
                }
                Arc::new(File::new(file))
            }
        }
        OpenResult::Dir(dir) => Arc::new(Directory::new(dir)),
    };
    if flags & O_NONBLOCK != 0 {
        f.set_nonblocking(true)?;
    }
    add_file_like(f, flags & O_CLOEXEC != 0)
}

/// Open or create a file.
/// fd: file descriptor
/// filename: file path to be opened or created
/// flags: open flags
/// mode: see man 7 inode
/// return new file descriptor if succeed, or return -1.
fn openat_inner(dirfd: c_int, path: &str, flags: i32, mode: __kernel_mode_t) -> AxResult<isize> {
    validate_pathname(Path::new(path))?;
    debug!("sys_openat <= {dirfd} {path:?} {flags:#o} {mode:#o}");

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    let uid = proc_data.euid();
    let gid = proc_data.egid();
    let mode = mode & !current().as_thread().proc_data.umask();
    let created_parent = if (flags as u32) & O_CREAT != 0 {
        with_path_fs(dirfd, Path::new(path), |fs| {
            match fs.resolve_no_follow(path) {
                Ok(loc) => {
                    enforce_special_open_rules(&loc, flags, uid)?;
                    if loc.is_dir() && invalid_directory_open(flags) {
                        return Err(AxError::IsADirectory);
                    }
                    check_open_permissions(
                        &loc,
                        open_access_mask(flags),
                        uid,
                        gid,
                        &supplementary_groups,
                    )?;
                    Ok(None)
                }
                Err(AxError::NotFound) => {
                    let (parent, name) = fs.resolve_nonexistent(Path::new(path))?;
                    check_create_permissions(&parent, uid, gid, &supplementary_groups)?;
                    Ok(Some((parent, name.to_string())))
                }
                Err(err) => Err(err),
            }
        })?
    } else {
        with_path_fs(dirfd, Path::new(path), |fs| {
            let loc = if (flags as u32) & O_NOFOLLOW != 0 {
                fs.resolve_no_follow(path)?
            } else {
                fs.resolve(path)?
            };
            enforce_special_open_rules(&loc, flags, uid)?;
            if loc.is_dir() && invalid_directory_open(flags) {
                return Err(AxError::IsADirectory);
            }
            check_open_permissions(
                &loc,
                open_access_mask(flags),
                uid,
                gid,
                &supplementary_groups,
            )
        })?;
        None
    };

    if created_parent.is_none() {
        let existing = with_path_fs(dirfd, Path::new(path), |fs| {
            if (flags as u32) & O_NOFOLLOW != 0 {
                fs.resolve_no_follow(path)
            } else {
                fs.resolve(path)
            }
        })?;
        enforce_special_open_rules(&existing, flags, uid)?;
        lease::wait_for_open(&existing, flags)?;
    }

    let options = flags_to_options(flags, mode, (sys_geteuid()? as _, sys_getegid()? as _));
    let fd = with_path_fs(dirfd, Path::new(path), |fs| options.open(fs, path))
        .and_then(|it| add_to_fd(it, flags as _))
        .map(|fd| fd as isize)?;

    if let Some(loc) = location_for_fd(fd as i32) {
        if let Some((parent, name)) = created_parent {
            let mut final_mode = NodePermission::from_bits_truncate(mode as u16);
            let mut owner_gid = proc_data.egid();
            let parent_meta = parent.metadata()?;
            if parent_meta.mode.contains(NodePermission::SET_GID) {
                owner_gid = parent_meta.gid;
            }
            if proc_data.euid() != 0 && !proc_data.is_in_group(owner_gid) {
                final_mode.remove(NodePermission::SET_GID);
            }
            loc.update_metadata(MetadataUpdate {
                owner: Some((proc_data.euid(), owner_gid)),
                mode: Some(final_mode),
                ..Default::default()
            })?;
            let _ = notify_parent_with_name(&parent, &name, IN_CREATE, loc.is_dir(), 0);
        }
        let _ = notify_parent(&loc, IN_OPEN);
        let _ = notify_exact(&loc, IN_OPEN);
    }

    Ok(fd)
}

pub fn sys_openat(
    dirfd: c_int,
    path: *const c_char,
    flags: i32,
    mode: __kernel_mode_t,
) -> AxResult<isize> {
    let path = vm_load_string(path)?;

    if (flags as u32 & O_TMPFILE) == O_TMPFILE {
        if (flags as u32 & O_ACCMODE) == O_RDONLY {
            return Err(AxError::InvalidInput);
        }
        let tmp_flags = ((flags as u32) & !(O_TMPFILE | O_DIRECTORY)) | O_CREAT | O_EXCL;
        for _ in 0..64 {
            let tmp_path = next_tmpfile_path(&path);
            match openat_inner(dirfd, &tmp_path, tmp_flags as i32, mode) {
                Ok(fd) => return Ok(fd),
                Err(AxError::AlreadyExists) => continue,
                Err(err) => return Err(err),
            }
        }
        return Err(AxError::AlreadyExists);
    }

    openat_inner(dirfd, &path, flags, mode)
}

/// Open a file by `filename` and insert it into the file descriptor table.
///
/// Return its index in the file table (`fd`). Return `EMFILE` if it already
/// has the maximum number of files open.
#[cfg(target_arch = "x86_64")]
pub fn sys_open(path: *const c_char, flags: i32, mode: __kernel_mode_t) -> AxResult<isize> {
    sys_openat(AT_FDCWD as _, path, flags, mode)
}

pub fn sys_close(fd: c_int) -> AxResult<isize> {
    debug!("sys_close <= {fd}");
    notify_close(fd);
    close_file_like(fd)?;
    Ok(0)
}

bitflags! {
    #[derive(Debug, Clone, Copy)]
    struct CloseRangeFlags: u32 {
        const UNSHARE = 1 << 1;
        const CLOEXEC = 1 << 2;
    }
}

pub fn sys_close_range(first: u32, last: u32, flags: u32) -> AxResult<isize> {
    if last < first {
        return Err(AxError::InvalidInput);
    }
    let flags = CloseRangeFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    debug!("sys_close_range <= fds: [{first}, {last}], flags: {flags:?}");
    if flags.contains(CloseRangeFlags::UNSHARE) {
        let curr = current();
        curr.as_thread().with_mut_scope(|scope| {
            let mut guard = FD_TABLE.scope_mut(scope);
            if Arc::strong_count(guard.deref()) > 1 {
                let cloned = guard.read().clone();
                *guard = Arc::new(RwLock::new(cloned));
            }
        });
    }

    let cloexec = flags.contains(CloseRangeFlags::CLOEXEC);
    let mut fd_table = FD_TABLE.write();
    if let Some(max_index) = fd_table.ids().next_back() {
        let last = last.min(max_index as u32);
        for fd in first..=last {
            if cloexec {
                if let Some(f) = fd_table.get_mut(fd as _) {
                    f.cloexec = true;
                }
            } else {
                fd_table.remove(fd as _);
            }
        }
    }

    Ok(0)
}

fn dup_fd(old_fd: c_int, cloexec: bool) -> AxResult<isize> {
    let description = get_file_description(old_fd)?;
    dup_fd_at_least(description, 0, cloexec)
}

fn dup_fd_at_least(
    description: Arc<crate::file::FileDescription>,
    min_fd: c_int,
    cloexec: bool,
) -> AxResult<isize> {
    if min_fd < 0 {
        return Err(AxError::InvalidInput);
    }

    let max_nofile = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE].current as usize;
    let upper_bound = max_nofile.min(AX_FILE_LIMIT);
    let mut fd_table = FD_TABLE.write();
    for new_fd in min_fd as usize..upper_bound {
        if fd_table.get(new_fd).is_some() {
            continue;
        }
        fd_table
            .add_at(
                new_fd,
                FileDescriptor {
                    description: description.clone(),
                    cloexec,
                },
            )
            .map_err(|_| AxError::TooManyOpenFiles)?;
        return Ok(new_fd as isize);
    }

    Err(AxError::TooManyOpenFiles)
}

fn validate_flock(lock: &flock64) -> AxResult<()> {
    match lock.l_whence as i32 {
        0..=2 => Ok(()),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_dup(old_fd: c_int) -> AxResult<isize> {
    debug!("sys_dup <= {old_fd}");
    dup_fd(old_fd, false)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_dup2(old_fd: c_int, new_fd: c_int) -> AxResult<isize> {
    if old_fd == new_fd {
        get_file_like(new_fd)?;
        return Ok(new_fd as _);
    }
    sys_dup3(old_fd, new_fd, 0)
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Dup3Flags: c_int {
        const O_CLOEXEC = O_CLOEXEC as _; // Close on exec
    }
}

pub fn sys_dup3(old_fd: c_int, new_fd: c_int, flags: c_int) -> AxResult<isize> {
    let flags = Dup3Flags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    debug!("sys_dup3 <= old_fd: {old_fd}, new_fd: {new_fd}, flags: {flags:?}");

    if old_fd == new_fd {
        return Err(AxError::InvalidInput);
    }

    let mut fd_table = FD_TABLE.write();
    let mut f = fd_table
        .get(old_fd as _)
        .cloned()
        .ok_or(AxError::BadFileDescriptor)?;
    f.cloexec = flags.contains(Dup3Flags::O_CLOEXEC);

    fd_table.remove(new_fd as _);
    fd_table
        .add_at(new_fd as _, f)
        .map_err(|_| AxError::BadFileDescriptor)?;

    Ok(new_fd as _)
}

pub fn sys_fcntl(fd: c_int, cmd: c_int, arg: usize) -> AxResult<isize> {
    debug!("sys_fcntl <= fd: {fd} cmd: {cmd} arg: {arg}");

    match cmd as u32 {
        F_DUPFD => {
            let description = get_file_description(fd)?;
            dup_fd_at_least(description, arg as c_int, false)
        }
        F_DUPFD_CLOEXEC => {
            let description = get_file_description(fd)?;
            dup_fd_at_least(description, arg as c_int, true)
        }
        F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW => {
            let _ = get_file_like(fd)?;
            let lock = UserConstPtr::<flock64>::from(arg as *const flock64).get_as_ref()?;
            validate_flock(lock)?;
            Ok(0)
        }
        F_GETLK | F_OFD_GETLK => {
            let _ = get_file_like(fd)?;
            let lock = UserPtr::<flock64>::from(arg).get_as_mut()?;
            validate_flock(lock)?;
            lock.l_type = F_UNLCK as _;
            Ok(0)
        }
        F_SETLEASE => {
            let file = File::from_fd(fd)?;
            lease::set_lease(
                file.as_ref(),
                get_file_description(fd)?.flock_owner(),
                arg as i32,
            )?;
            Ok(0)
        }
        F_GETLEASE => {
            let file = File::from_fd(fd)?;
            Ok(lease::get_lease(file.as_ref()) as isize)
        }
        F_SETFL => {
            get_file_like(fd)?.set_nonblocking(arg & (O_NONBLOCK as usize) > 0)?;
            Ok(0)
        }
        F_GETFL => {
            let f = get_file_like(fd)?;

            let mut ret = 0;
            if f.nonblocking() {
                ret |= O_NONBLOCK;
            }

            let perm = NodePermission::from_bits_truncate(f.stat()?.mode as _);
            if perm.contains(NodePermission::OWNER_WRITE) {
                if perm.contains(NodePermission::OWNER_READ) {
                    ret |= O_RDWR;
                } else {
                    ret |= O_WRONLY;
                }
            }

            Ok(ret as _)
        }
        F_GETFD => {
            let cloexec = FD_TABLE
                .read()
                .get(fd as _)
                .ok_or(AxError::BadFileDescriptor)?
                .cloexec;
            Ok(if cloexec { FD_CLOEXEC as _ } else { 0 })
        }
        F_SETFD => {
            let cloexec = arg & FD_CLOEXEC as usize != 0;
            FD_TABLE
                .write()
                .get_mut(fd as _)
                .ok_or(AxError::BadFileDescriptor)?
                .cloexec = cloexec;
            Ok(0)
        }
        F_GETPIPE_SZ => {
            let pipe = Pipe::from_fd(fd)?;
            Ok(pipe.capacity() as _)
        }
        F_SETPIPE_SZ => {
            let pipe = Pipe::from_fd(fd)?;
            pipe.resize(arg)?;
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_flock(fd: c_int, operation: c_int) -> AxResult<isize> {
    debug!("flock <= fd: {fd}, operation: {operation}");

    let description = get_file_description(fd)?;
    let stat = description.inner.stat()?;

    crate::file::flock::do_flock((stat.dev, stat.ino), description.flock_owner(), operation)?;
    Ok(0)
}
