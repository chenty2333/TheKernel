use alloc::{boxed::Box, string::String, sync::Arc};
use core::{
    ffi::{c_char, c_int},
    fmt::Write as _,
    mem::{MaybeUninit, size_of, size_of_val},
    ptr, slice,
};

use axerrno::{AxError, AxResult, LinuxError};
use super::admit_resize;
use axfs::{
    FileBackend, FileFlags, FsContext, OpenOptions, OpenResult, PathwalkComponent,
    PathwalkPolicy,
};
use axfs_ng_vfs::{
    DirEntry, FileNode, Location, MetadataUpdate, NodePermission, NodeType, Reference, path::Path,
};
use axio::{Seek, SeekFrom};
use axtask::current;
use bitflags::bitflags;
use linux_raw_sys::general::*;
use linux_vfs::{
    LimitKind, Openat2Policy, PathContext as LinuxPathContext, PathContextError, PathLimitError,
    PathLimits, ResolveFlags, TopologyEvent, WalkBudget, WalkError,
};
use thekernel_linux_cred::{FileOpenAccess, FileOpenOperation};
use thekernel_linux_signal::Signo;

use crate::{
    file::{
        AsyncIoOwner, AsyncIoOwnerType, DescriptionResource, Directory, File, current_fd_table,
        FileDescription, FileLike, Pipe, ReservedFd, close_file_like, dnotify, executable,
        flock::{self, RecordLockOwner},
        get_file_description, get_file_like, get_typed_file,
        inotify::{
            WatchKey, notify_exact, notify_parent, notify_parent_with_name,
            wait_current_close_notifications,
        },
        lease, memfd,
        permission::{
            VfsSecurityContext, authorize_file_open, authorize_named_inode_create,
            check_create_permissions_with_security, check_open_permissions_with_security,
            check_pathwalk_search_permission_with_security, check_writable_mount,
            initial_named_create_owner_mode_with_security,
        },
        pipe::NamedPipe,
        prepare_file_description_with_open_lease, reserve_fd, resolve_at,
        with_path_fs,
    },
    mm::{UserMemoryCapability, map_usercopy_error},
    pseudofs::{Device, dev::tty},
    syscall::fs::{ctl::validate_pathname, io::check_mandatory_fd_truncate_lock},
    task::{AX_FILE_LIMIT, AsThread, Cred, current_fs_context, ns_capable},
    time::wall_time,
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
            O_RDWR => options.read(true).write(true),
            _ => options.no_data(true),
        };
        // `OpenOptions::append(true)` also grants backend write access. Linux
        // keeps O_APPEND visible in F_GETFL for O_RDONLY and reserved mode-3
        // descriptions but does not turn either writable, so only project
        // append when data writes were actually opened.
        if flags & O_APPEND != 0 && open_has_data_write(flags) {
            options.append(true);
        }
        if flags & O_TRUNC != 0 {
            options.truncate(true);
        }
        if flags & O_CREAT != 0 {
            options.create(true);
            if flags & O_EXCL != 0 {
                options.create_new(true);
            }
        }
        if flags & O_DIRECT != 0 {
            options.direct(true);
        }
        if flags & O_NOATIME != 0 {
            options.no_atime(true);
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

const fn open_has_data_write(flags: u32) -> bool {
    matches!(flags & O_ACCMODE, O_WRONLY | O_RDWR)
}

fn open_status_flags(flags: u32) -> u32 {
    let mut status = flags & O_ACCMODE;
    status |= flags
        & (O_APPEND
            | O_DIRECT
            | O_DSYNC
            | O_SYNC
            | O_NONBLOCK
            | FASYNC
            | O_LARGEFILE
            | O_NOATIME
            | O_PATH);
    status
}

fn file_open_operation(
    flags: u32,
    created: bool,
    unnamed: bool,
) -> AxResult<Option<FileOpenOperation>> {
    // Linux O_PATH returns from do_dentry_open() before security_file_open.
    // Path traversal still receives per-component inode admission, but there
    // is no ordinary file-open hook operation to normalize here.
    if flags & O_PATH != 0 {
        return Ok(None);
    };
    let access = match flags & O_ACCMODE {
        O_RDONLY => FileOpenAccess::Read,
        O_WRONLY => FileOpenAccess::Write,
        O_RDWR => FileOpenAccess::ReadWrite,
        _ => FileOpenAccess::NoData,
    };
    FileOpenOperation::new(
        access,
        flags & O_APPEND != 0 && access.writes(),
        flags & O_TRUNC != 0,
        created,
        unnamed,
    )
    .map(Some)
    .ok_or(AxError::InvalidInput)
}

const FCNTL_SETFL_MUTABLE_FLAGS: u32 = O_APPEND | O_NONBLOCK | FASYNC;

fn fcntl_allowed_on_path_fd(cmd: u32) -> bool {
    matches!(cmd, F_DUPFD | F_DUPFD_CLOEXEC | F_GETFD | F_SETFD | F_GETFL)
}

fn validate_async_signal(sig: c_int) -> AxResult<u8> {
    if sig == 0 {
        return Ok(0);
    }
    if sig < 0 || sig > Signo::SIGRT32 as c_int {
        return Err(AxError::InvalidInput);
    }
    Signo::from_repr(sig as u8)
        .map(|_| sig as u8)
        .ok_or(AxError::InvalidInput)
}

fn sync_async_io_to_file_flags(description: &FileDescription, fd: c_int, flags: u32) {
    let enabled = flags & FASYNC != 0;
    let state = description.async_io_state();
    if let Some(pipe) = description.inner.downcast_ref::<Pipe>() {
        pipe.set_async_io(enabled, state, fd);
    } else if let Some(pipe) = description.inner.downcast_ref::<NamedPipe>() {
        pipe.set_async_io(enabled, state, fd);
    }
}

fn trailing_slash_requires_directory(path: &str) -> bool {
    path.len() > 1 && path.as_bytes().last() == Some(&b'/')
}

fn enforce_trailing_slash_directory(path: &str, loc: &Location) -> AxResult<()> {
    if trailing_slash_requires_directory(path) && !loc.is_dir() {
        return Err(AxError::NotADirectory);
    }
    Ok(())
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

fn open_requires_writable_mount(flags: c_int) -> bool {
    let flags = flags as u32;
    flags & O_PATH == 0 && (open_has_data_write(flags) || flags & O_TRUNC != 0)
}

fn touch_truncated_metadata(loc: &Location) -> AxResult<()> {
    let now = wall_time();
    loc.update_supported_metadata(MetadataUpdate {
        mtime: Some(now.into()),
        ctime: Some(now.into()),
        ..Default::default()
    })?;
    Ok(())
}

fn invalid_directory_open(flags: c_int) -> bool {
    let flags = flags as u32;
    (flags & O_ACCMODE) != O_RDONLY || (flags & (O_CREAT | O_TRUNC)) != 0
}

fn noatime_allowed(flags: u32, owner_match: bool, fowner_capable: bool) -> bool {
    flags & O_NOATIME == 0 || owner_match || fowner_capable
}

fn enforce_special_open_rules(
    loc: &Location,
    flags: c_int,
    security: &OpenPathSecurityContext,
) -> AxResult {
    let flags = flags as u32;
    let metadata = loc.metadata()?;

    let owner_match = security.credentials().uid().into_raw() == metadata.uid;
    let fowner_capable = ns_capable(
        security.actor(),
        security.filesystem_owner_user_ns(),
        CAP_FOWNER,
    );
    if !noatime_allowed(flags, owner_match, fowner_capable) {
        return Err(AxError::OperationNotPermitted);
    }
    if flags & O_NOFOLLOW != 0 && flags & O_PATH == 0 && metadata.node_type == NodeType::Symlink {
        return Err(AxError::from(LinuxError::ELOOP));
    }
    if flags & O_PATH == 0
        && matches!(
            metadata.node_type,
            NodeType::CharacterDevice | NodeType::BlockDevice
        )
        && crate::mounts::is_nodev(loc)?
    {
        return Err(AxError::PermissionDenied);
    }
    if flags & O_TRUNC != 0 {
        drop(memfd::begin_resize(loc, 0)?);
    }

    Ok(())
}

const OPENAT2_HOW_SIZE: usize = size_of::<open_how>();
const OPENAT2_ALLOWED_FLAGS: u64 = (O_ACCMODE
    | O_APPEND
    | FASYNC
    | O_CLOEXEC
    | O_CREAT
    | O_DIRECT
    | O_DIRECTORY
    | O_DSYNC
    | O_EXCL
    | O_LARGEFILE
    | O_NOATIME
    | O_NOCTTY
    | O_NOFOLLOW
    | O_NONBLOCK
    | O_PATH
    | O_SYNC
    | O_TMPFILE
    | O_TRUNC) as u64;
const OPENAT2_PATH_FLAGS: u32 = O_DIRECTORY | O_NOFOLLOW | O_PATH | O_CLOEXEC;
const OPENAT2_ALLOWED_RESOLVE: u64 = (RESOLVE_NO_XDEV
    | RESOLVE_NO_MAGICLINKS
    | RESOLVE_NO_SYMLINKS
    | RESOLVE_BENEATH
    | RESOLVE_IN_ROOT
    | RESOLVE_CACHED) as u64;

const OPEN_NAMESPACE_RESOLVE_FLAGS: u64 =
    (RESOLVE_BENEATH | RESOLVE_IN_ROOT | RESOLVE_NO_XDEV) as u64;

const fn open_requires_namespace_operation(flags: u32, resolve: u64) -> bool {
    open_has_data_write(flags)
        || flags & (O_CREAT | __O_TMPFILE | O_TRUNC) != 0
        || resolve & OPEN_NAMESPACE_RESOLVE_FLAGS != 0
}

const MAX_FILE_HANDLE_SZ: u32 = 128;
const NAME_TO_HANDLE_ALLOWED_FLAGS: i32 = (AT_EMPTY_PATH | AT_SYMLINK_FOLLOW) as i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxFileHandle {
    handle_bytes: u32,
    handle_type: i32,
}

fn validate_openat2_how(how: &open_how) -> AxResult<u32> {
    if how.flags >> 32 != 0
        || how.flags & !OPENAT2_ALLOWED_FLAGS != 0
        || how.resolve & !OPENAT2_ALLOWED_RESOLVE != 0
    {
        return Err(AxError::InvalidInput);
    }
    if how.resolve & RESOLVE_BENEATH as u64 != 0 && how.resolve & RESOLVE_IN_ROOT as u64 != 0 {
        return Err(AxError::InvalidInput);
    }

    let mut flags = how.flags as u32;
    let will_create = flags & (O_CREAT | __O_TMPFILE) != 0;
    if will_create {
        if how.mode & !0o7777 != 0 {
            return Err(AxError::InvalidInput);
        }
    } else if how.mode != 0 {
        return Err(AxError::InvalidInput);
    }

    if flags & (O_DIRECTORY | O_CREAT) == O_DIRECTORY | O_CREAT {
        return Err(AxError::InvalidInput);
    }
    if flags & __O_TMPFILE != 0 && (flags & O_DIRECTORY == 0 || flags & O_ACCMODE == O_RDONLY) {
        return Err(AxError::InvalidInput);
    }
    if flags & O_PATH != 0 && flags & !OPENAT2_PATH_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & __O_SYNC != 0 {
        flags |= O_DSYNC;
    }
    Ok(flags)
}

fn normalize_legacy_open_flags(flags: i32) -> AxResult<i32> {
    let mut flags = flags as u32;

    // Linux masks non-O_PATH status bits for legacy open/openat before
    // interpreting creative flags. In particular O_PATH|O_TMPFILE opens the
    // directory as a path handle instead of attempting anonymous creation.
    if flags & O_PATH != 0 {
        flags &= OPENAT2_PATH_FLAGS;
    }

    if flags & (O_DIRECTORY | O_CREAT) == O_DIRECTORY | O_CREAT {
        return Err(AxError::InvalidInput);
    }
    if flags & __O_TMPFILE != 0
        && (flags & O_TMPFILE != O_TMPFILE || flags & O_CREAT != 0 || flags & O_ACCMODE == O_RDONLY)
    {
        return Err(AxError::InvalidInput);
    }
    if flags & __O_SYNC != 0 {
        flags |= O_DSYNC;
    }

    Ok(flags as i32)
}

fn openat2_context(dirfd: c_int, path: &Path, resolve: u64) -> AxResult<FsContext> {
    let (root, current_dir) = {
        let fs_context = current_fs_context();
        let fs = fs_context.lock();
        (fs.root_dir().clone(), fs.current_dir().clone())
    };

    if resolve & (RESOLVE_IN_ROOT | RESOLVE_BENEATH) as u64 != 0 {
        let base = if dirfd == AT_FDCWD {
            current_dir
        } else {
            Directory::from_fd(dirfd)?.inner().clone()
        };
        return Ok(FsContext::new(base));
    }

    if path.is_absolute() {
        Ok(FsContext::new(root))
    } else {
        let base = if dirfd == AT_FDCWD {
            current_dir
        } else {
            Directory::from_fd(dirfd)?.inner().clone()
        };
        FsContext::new(root).with_current_dir(base)
    }
}

struct Openat2PathwalkPolicy {
    inner: Openat2Policy,
    budget: WalkBudget,
}

#[derive(Clone)]
struct OpenPathSecurityContext {
    vfs: VfsSecurityContext,
    umask: u32,
}

impl OpenPathSecurityContext {
    fn new(actor: Arc<Cred>, umask: u32) -> Self {
        Self {
            vfs: VfsSecurityContext::new(actor),
            umask,
        }
    }

    fn actor(&self) -> &Cred {
        self.vfs.actor()
    }

    fn credentials(&self) -> &crate::task::DacCredentialView {
        self.vfs.credentials()
    }

    fn filesystem_owner_user_ns(&self) -> &Arc<crate::task::UserNamespace> {
        self.vfs.filesystem_owner_user_ns()
    }

    fn vfs_security(&self) -> &VfsSecurityContext {
        &self.vfs
    }
}

impl Openat2PathwalkPolicy {
    fn legacy() -> AxResult<Self> {
        Self::from_parts(
            Openat2Policy::new(ResolveFlags::EMPTY),
            PathLimits::LINUX_DEFAULT,
        )
    }

    fn from_parts(inner: Openat2Policy, limits: PathLimits) -> AxResult<Self> {
        Ok(Self {
            inner,
            budget: WalkBudget::new(limits).map_err(|_| AxError::InvalidInput)?,
        })
    }

    fn authorize(&self, event: TopologyEvent<'_, Location>) -> axfs_ng_vfs::VfsResult<()> {
        self.inner
            .authorize(event)
            .map(|_| ())
            .map_err(Self::map_walk_error)
    }

    fn account(result: Result<(), WalkError>) -> axfs_ng_vfs::VfsResult<()> {
        result.map_err(Self::map_walk_error)
    }

    fn map_walk_error(error: WalkError) -> axfs_ng_vfs::VfsError {
        let linux_error = match error {
            WalkError::CrossDevice => LinuxError::EXDEV,
            WalkError::SymbolicLinkLoop => LinuxError::ELOOP,
            WalkError::RetryWithoutCached => LinuxError::EAGAIN,
            WalkError::Limit(PathLimitError {
                kind: LimitKind::PathBytes | LimitKind::ComponentBytes,
                ..
            }) => LinuxError::ENAMETOOLONG,
            WalkError::Limit(_) => LinuxError::ELOOP,
            _ => LinuxError::EINVAL,
        };
        linux_error.into()
    }
}

impl PathwalkPolicy for Openat2PathwalkPolicy {
    fn component(
        &mut self,
        _directory: &Location,
        component: PathwalkComponent<'_>,
    ) -> axfs_ng_vfs::VfsResult<()> {
        let bytes = match component {
            PathwalkComponent::Root | PathwalkComponent::Current => 1,
            PathwalkComponent::Parent => 2,
            PathwalkComponent::Normal(name) => name.len(),
        };
        Self::account(self.budget.component(bytes))
    }

    fn follow_magic_link(
        &mut self,
        link: &Location,
        final_component: bool,
    ) -> axfs_ng_vfs::VfsResult<()> {
        if link.node_type() != NodeType::Symlink {
            Self::account(self.budget.symlink())?;
        }
        self.authorize(TopologyEvent::FollowMagicLink {
            link,
            final_component,
            // axfs currently does not expose the jump target before follow,
            // so NO_XDEV must conservatively reject instead of fake success.
            target_stays_on_mount: false,
        })
    }

    fn follow_symlink(
        &mut self,
        link: &Location,
        final_component: bool,
    ) -> axfs_ng_vfs::VfsResult<()> {
        Self::account(self.budget.symlink())?;
        self.authorize(TopologyEvent::FollowSymlink {
            link,
            final_component,
        })
    }

    fn cross_mount(&mut self, from: &Location, to: &Location) -> axfs_ng_vfs::VfsResult<()> {
        Self::account(self.budget.mount_crossing())?;
        self.authorize(TopologyEvent::CrossMount { from, to })
    }

    fn absolute_root(&mut self, from: &Location, root: &Location) -> axfs_ng_vfs::VfsResult<()> {
        Self::account(self.budget.restart())?;
        // `openat2_context` installs the dirfd as `FsContext::root_dir` for
        // IN_ROOT, so RestartAtOperationRoot is already the walker's action.
        self.authorize(TopologyEvent::AbsoluteRestart { from, root })
    }

    fn escape_root(&mut self, root: &Location) -> axfs_ng_vfs::VfsResult<()> {
        // FsContext clamps `..` at its root; policy still decides whether the
        // scoped operation permits that action.
        self.authorize(TopologyEvent::EscapeRoot { root })
    }
}

struct ExecutableWriteReservation {
    key: Option<executable::ExecutableKey>,
    persistent: bool,
}

impl ExecutableWriteReservation {
    fn acquire(loc: &Location, flags: u32) -> AxResult<Self> {
        let needs_exclusion =
            flags & O_PATH == 0 && (flags & O_TRUNC != 0 || open_has_data_write(flags));
        let key = if needs_exclusion {
            executable::retain_write_open(loc)?
        } else {
            None
        };
        Ok(Self {
            key,
            persistent: flags & O_PATH == 0 && open_has_data_write(flags),
        })
    }

    fn transfer_persistent(&mut self) -> Option<executable::ExecutableKey> {
        self.persistent.then(|| self.key.take()).flatten()
    }
}

impl Drop for ExecutableWriteReservation {
    fn drop(&mut self) {
        executable::release_write_open(self.key.take());
    }
}

fn prepare_open_description(
    result: OpenResult,
    flags: u32,
    write_open_key: Option<executable::ExecutableKey>,
    open_lease_admission: lease::OpenLeaseAdmission,
    open_credential: Arc<Cred>,
) -> AxResult<Arc<FileDescription>> {
    let mut description_resource: Option<DescriptionResource> = None;
    let f: Arc<dyn FileLike> = match result {
        OpenResult::File(mut file) => {
            if flags & O_PATH == 0 && file.location().metadata()?.node_type == NodeType::Fifo {
                Arc::try_new(crate::file::pipe::NamedPipe::open(
                    file.location().clone(),
                    flags,
                )?)
                .map_err(|_| AxError::NoMemory)?
            } else {
                let mut pty_guard = None;
                // /dev/xx handling
                if flags & O_PATH == 0
                    && let Ok(device) = file.location().entry().downcast::<Device>()
                {
                    let inner = device.inner().as_any();
                    if let Some(ptmx) = inner.downcast_ref::<tty::Ptmx>() {
                        // Opening /dev/ptmx creates a new pseudo-terminal
                        let (master, master_tty, pty_number) = ptmx.create_pty()?;
                        let pts = file
                            .location()
                            .parent()
                            .ok_or(AxError::NotFound)?
                            .lookup_no_follow("pts")?;
                        let pty_name = try_pty_name(pty_number)?;
                        let entry = DirEntry::try_new_file(
                            FileNode::new(master),
                            NodeType::CharacterDevice,
                            Reference::new(Some(pts.entry().clone()), pty_name),
                        )?;
                        let loc = Location::new(file.location().mountpoint().clone(), entry);
                        file = axfs::File::new(FileBackend::Direct(loc), file.flags());
                        pty_guard = Some(master_tty.open_description()?);
                    } else if let Some(pty) = inner.downcast_ref::<tty::PtyDriver>() {
                        if pty.is_locked_pty_slave() {
                            return Err(AxError::Io);
                        }
                        pty_guard = Some(pty.open_description()?);
                    } else if inner.is::<tty::CurrentTty>() {
                        let term = current()
                            .as_thread()
                            .proc_data
                            .proc
                            .group()
                            .session()
                            .terminal()
                            .ok_or(AxError::NotFound)?;
                        let dev_dir = file.location().parent().ok_or(AxError::NotFound)?;
                        let loc = if term.is::<tty::NTtyDriver>() {
                            dev_dir.lookup_no_follow("console")?
                        } else if let Some(pts) = term.downcast_ref::<tty::PtyDriver>() {
                            pty_guard = Some(pts.open_description()?);
                            let pty_name = try_pty_name(pts.pty_number())?;
                            dev_dir
                                .lookup_no_follow("pts")?
                                .lookup_no_follow(&pty_name)?
                        } else {
                            return Err(LinuxError::ENODEV.into());
                        };
                        file = axfs::File::new(FileBackend::Direct(loc), file.flags());
                    }
                }
                let file = File::new(file);
                if let Some(guard) = pty_guard {
                    description_resource =
                        Some(Box::try_new(guard).map_err(|_| AxError::NoMemory)?
                            as DescriptionResource);
                }
                Arc::try_new(file).map_err(|_| AxError::NoMemory)?
            }
        }
        OpenResult::Dir(dir) => Arc::try_new(Directory::new(dir)).map_err(|_| AxError::NoMemory)?,
    };
    if flags & O_NONBLOCK != 0 {
        f.set_nonblocking(true)?;
    }
    prepare_file_description_with_open_lease(
        f,
        open_status_flags(flags),
        write_open_key,
        description_resource,
        open_lease_admission,
        open_credential,
    )
}

/// Runs a fallible policy admission against the exact resolved location before
/// the generic filesystem open can execute `O_TRUNC` or another open-time
/// side effect. The callback remains mechanism-neutral and is also the seam a
/// lower layer could eventually expose between filesystem open and truncate.
fn open_loc_after_admission<R>(
    options: &OpenOptions,
    loc: Location,
    admission: impl FnOnce(&Location) -> AxResult<()>,
    prepare: impl FnOnce(&Location) -> AxResult<R>,
) -> AxResult<(OpenResult, R)> {
    admission(&loc)?;
    let resource = prepare(&loc)?;
    let result = options.open_loc_deferred_truncate(loc)?;
    Ok((result, resource))
}

fn commit_open_truncate(
    file: &File,
    loc: &Location,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    let _memfd_mutation = memfd::begin_resize(loc, 0)?;
    let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
    let quota_charge = admit_resize(loc, loc.len()?, 0)?;
    file.inner().backend()?.set_len(0)?;
    quota_charge.commit_actual_blocks(loc)?;
    // A post-truncate metadata failure cannot be reported without lying that
    // this destructive open left the inode untouched. Filesystems should
    // update truncate timestamps as part of set_len; retain this compatibility
    // update as best effort.
    if let Err(error) = touch_truncated_metadata(loc) {
        warn!("open truncate metadata update failed: {error}");
    }
    Ok(())
}

fn try_pty_name(number: u32) -> AxResult<String> {
    let mut name = String::new();
    name.try_reserve_exact(10).map_err(|_| AxError::NoMemory)?;
    write!(&mut name, "{number}").map_err(|_| AxError::NoMemory)?;
    Ok(name)
}

fn publish_reserved_open(
    result: OpenResult,
    flags: u32,
    reservation: ReservedFd,
    write_open_key: Option<executable::ExecutableKey>,
    open_lease_admission: lease::OpenLeaseAdmission,
    open_credential: Arc<Cred>,
) -> AxResult<i32> {
    let description = prepare_open_description(
        result,
        flags,
        write_open_key,
        open_lease_admission,
        open_credential,
    )?;
    reservation.publish(description)
}

fn name_to_handle_resolve_flags(flags: i32) -> u32 {
    let mut resolve_flags = 0;
    if flags & AT_EMPTY_PATH as i32 != 0 {
        resolve_flags |= AT_EMPTY_PATH;
    }
    if flags & AT_SYMLINK_FOLLOW as i32 == 0 {
        resolve_flags |= AT_SYMLINK_NOFOLLOW;
    }
    resolve_flags
}

fn load_user_string(capability: &UserMemoryCapability, path: *const c_char) -> AxResult<String> {
    String::from_utf8(
        capability
            .load_until_nul(path.cast::<u8>())
            .map_err(map_usercopy_error)?,
    )
    .map_err(|_| AxError::IllegalBytes)
}

pub fn sys_name_to_handle_at(
    capability: UserMemoryCapability,
    dirfd: c_int,
    path: *const c_char,
    _handle: *mut u8,
    _mount_id: *mut i32,
    flags: i32,
) -> AxResult<isize> {
    if flags & !NAME_TO_HANDLE_ALLOWED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    let path = load_user_string(&capability, path)?;
    if !(path.is_empty() && flags & AT_EMPTY_PATH as i32 != 0 && dirfd == AT_FDCWD) {
        let _ = resolve_at(
            dirfd,
            Some(path.as_str()),
            name_to_handle_resolve_flags(flags),
        )?;
    }

    // axfs-ng has no exportable mount/inode/generation handle contract yet.
    Err(LinuxError::EOPNOTSUPP.into())
}

pub fn sys_open_by_handle_at(
    capability: UserMemoryCapability,
    mount_fd: c_int,
    handle: *const u8,
    _flags: i32,
) -> AxResult<isize> {
    let handle_addr = handle as usize;
    let header = unsafe {
        capability
            .read_value_uninit(handle.cast::<LinuxFileHandle>())
            .map_err(map_usercopy_error)?
            .assume_init()
    };

    if header.handle_bytes > MAX_FILE_HANDLE_SZ
        || header.handle_bytes == 0
        || header.handle_type < 0
    {
        return Err(AxError::InvalidInput);
    }

    if mount_fd != AT_FDCWD {
        get_file_like(mount_fd)?;
    }

    let curr = current();
    if !curr
        .as_thread()
        .has_effective_capability(CAP_DAC_READ_SEARCH)
    {
        return Err(LinuxError::EPERM.into());
    }

    let body_addr = handle_addr
        .checked_add(size_of::<LinuxFileHandle>())
        .ok_or(LinuxError::EFAULT)?;
    let mut body = [MaybeUninit::<u8>::uninit(); MAX_FILE_HANDLE_SZ as usize];
    capability
        .read_bytes(body_addr, &mut body[..header.handle_bytes as usize])
        .map_err(map_usercopy_error)?;

    // A well-formed handle cannot be decoded until the VFS exports stable IDs.
    Err(LinuxError::ESTALE.into())
}

fn open_in_fs(
    fs: &mut FsContext,
    path: &str,
    flags: i32,
    mode: __kernel_mode_t,
    security: &OpenPathSecurityContext,
) -> AxResult<isize> {
    let mut policy = Openat2PathwalkPolicy::legacy()?;
    open_in_fs_with_policy(fs, path, flags, mode, security, &mut policy)
}

fn open_in_fs_with_policy<P: PathwalkPolicy + ?Sized>(
    fs: &mut FsContext,
    path: &str,
    flags: i32,
    mode: __kernel_mode_t,
    security: &OpenPathSecurityContext,
    policy: &mut P,
) -> AxResult<isize> {
    validate_pathname(Path::new(path))?;
    debug!("sys_openat <= {path:?} {flags:#o} {mode:#o}");

    let credentials = security.credentials();
    let uid = credentials.uid().into_raw();
    let gid = credentials.gid().into_raw();
    let requested_mode = NodePermission::from_bits_truncate(mode as u16);
    let masked_mode =
        NodePermission::from_bits_truncate(requested_mode.bits() & !(security.umask as u16));
    let resolve_options = flags_to_options(flags, requested_mode.bits() as _, (uid, gid));
    // Linux reserves a numeric slot before path lookup can create a name or
    // truncate an existing inode. The reservation is invisible until the OFD
    // is fully constructed and published below.
    let reservation = reserve_fd((flags as u32) & O_CLOEXEC != 0)?;
    let (loc, created) = resolve_options.resolve_location_with_policy(
        fs,
        path,
        &mut |dir| {
            check_pathwalk_search_permission_with_security(
                dir,
                security.actor(),
                credentials,
                security.filesystem_owner_user_ns(),
            )
        },
        &mut |dir, name, create_options| {
            check_create_permissions_with_security(
                dir,
                security.actor(),
                credentials,
                security.filesystem_owner_user_ns(),
            )?;
            let parent = dir.metadata()?;
            let (final_mode, owner) = initial_named_create_owner_mode_with_security(
                &parent,
                security.vfs_security(),
                create_options.node_type,
                requested_mode,
                security.umask,
            );
            authorize_named_inode_create(
                dir,
                &parent,
                name,
                create_options.node_type,
                final_mode,
                None,
                security.vfs_security(),
            )?;
            create_options.permission = final_mode;
            create_options.user = Some(owner);
            Ok(())
        },
        policy,
    )?;

    if created {
        if let Some(parent) = loc.parent() {
            if let Err(error) =
                notify_parent_with_name(&parent, Some(&loc), loc.name(), IN_CREATE, loc.is_dir(), 0)
            {
                warn!("open create notification failed: {error}");
            }
        } else {
            warn!("created open entry has no parent: {:?}", loc.name());
        }
    }

    let opened_existing = !created;
    let truncates_regular = opened_existing
        && (flags as u32) & O_TRUNC != 0
        && loc.node_type() == NodeType::RegularFile;
    let open_result = (|| {
        enforce_trailing_slash_directory(path, &loc)?;
        enforce_special_open_rules(&loc, flags, security)?;
        if loc.is_dir() && invalid_directory_open(flags) {
            return Err(AxError::IsADirectory);
        }
        if opened_existing {
            check_open_permissions_with_security(
                &loc,
                open_access_mask(flags),
                security.actor(),
                credentials,
                security.filesystem_owner_user_ns(),
            )?;
            if open_requires_writable_mount(flags) {
                check_writable_mount(&loc)?;
            }
        }

        let file_open_operation = file_open_operation(flags as u32, created, false)?;

        let mut effective_flags = flags;
        if created {
            // A new regular inode is already empty. Avoid a redundant truncate
            // after namespace publication, and ensure open_loc cannot create a
            // second object or reinterpret the completed exclusive admission.
            effective_flags &= !((O_CREAT | O_EXCL | O_TRUNC) as i32);
        } else if (flags as u32) & O_CREAT != 0 && (flags as u32) & O_EXCL == 0 {
            effective_flags &= !(O_CREAT as i32);
        }

        let options = flags_to_options(effective_flags, masked_mode.bits() as _, (uid, gid));
        // Linux obtains persistent write access (and therefore reports
        // ETXTBSY/ENFILE) before security_file_open. Keep the RAII token live
        // through every later admission and transfer it into the OFD only on
        // success. Read-only O_TRUNC instead takes a transient token after the
        // ordinary hook/fanotify/lease sequence.
        let mut write_open = if open_has_data_write(flags as u32) {
            Some(ExecutableWriteReservation::acquire(&loc, flags as u32)?)
        } else {
            None
        };
        let (result, (late_write_open, lease_admission)) = open_loc_after_admission(
            &options,
            loc.clone(),
            |candidate| {
                let Some(operation) = file_open_operation else {
                    return Ok(());
                };
                authorize_file_open(
                    candidate,
                    security.actor(),
                    credentials,
                    security.filesystem_owner_user_ns(),
                    operation,
                )
            },
            |candidate| {
                // Match Linux's security_file_open -> fanotify open permission
                // -> lease break ordering. None of these fallible operations
                // runs after the filesystem open or truncate side effect.
                if (flags as u32) & O_PATH == 0 {
                    crate::file::fanotify::permission_check(
                        candidate,
                        candidate,
                        crate::file::fanotify::FAN_OPEN_PERM,
                        candidate.is_dir(),
                        false,
                    )?;
                }
                let lease_admission = lease::admit_open(candidate, flags)?;
                let late_write_open = if write_open.is_none() {
                    Some(ExecutableWriteReservation::acquire(
                        candidate,
                        flags as u32,
                    )?)
                } else {
                    None
                };
                Ok((late_write_open, lease_admission))
            },
        )?;
        if write_open.is_none() {
            write_open = late_write_open;
        }
        let mut write_open = write_open.ok_or(AxError::BadState)?;
        // Construct the complete OFD and reserve its exact table publication
        // before a destructive truncate. Once set_len(0) succeeds, only the
        // infallible Pending -> Visible commit remains.
        let description = prepare_open_description(
            result,
            effective_flags as _,
            write_open.transfer_persistent(),
            lease_admission,
            security.vfs_security().actor_arc().clone(),
        )?;
        let publication = reservation.prepare_publication(description.clone())?;
        if truncates_regular {
            crate::mm::check_not_active(&loc)?;
            let _swap_mutation = crate::mm::admit_mutation(&loc)?;
            let status = description.io_status_snapshot();
            check_mandatory_fd_truncate_lock(
                &loc,
                0,
                description.flock_owner(),
                status.nonblocking(),
            )?;
            let file = description
                .inner
                .downcast_ref::<File>()
                .ok_or(AxError::BadState)?;
            commit_open_truncate(file, &loc, security.vfs_security())?;
        }
        let fd = publication.commit() as isize;
        Ok(fd)
    })();
    let fd = open_result?;
    // The open/truncate notifications below remain after the full open
    // transaction so a failed open has no synthetic open residue. On the
    // successful path preserve Linux's relative ordering: the open event
    // precedes truncate mutation notifications.
    if (flags as u32) & O_PATH == 0 {
        if let Err(error) = notify_parent(&loc, IN_OPEN) {
            warn!("open parent notification failed: {error}");
        }
        if let Err(error) = notify_exact(&loc, IN_OPEN) {
            warn!("open notification failed: {error}");
        }
    }
    if truncates_regular && let Err(error) = notify_exact(&loc, IN_MODIFY | IN_ATTRIB) {
        warn!("open truncate notification failed: {error}");
    }

    Ok(fd)
}

/// Open or create a file.
/// fd: file descriptor
/// filename: file path to be opened or created
/// flags: open flags
/// mode: see man 7 inode
/// return new file descriptor if succeed, or return -1.
pub(crate) fn openat_inner(
    dirfd: c_int,
    path: &str,
    flags: i32,
    mode: __kernel_mode_t,
) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let security = OpenPathSecurityContext::new(thread.current_cred(), current_fs_context().lock().umask());
    openat_inner_with_context(dirfd, path, flags, mode, security)
}

fn openat_inner_with_context(
    dirfd: c_int,
    path: &str,
    flags: i32,
    mode: __kernel_mode_t,
    security: OpenPathSecurityContext,
) -> AxResult<isize> {
    let flags = normalize_legacy_open_flags(flags)?;
    // Named/anonymous creation is a specialized VFS+FD transaction: the FD
    // slot stays private until open construction succeeds, and the namespace
    // guard keeps writable-mount admission stable through inode publication.
    let _mount_namespace =
        open_requires_namespace_operation(flags as u32, 0).then(crate::mounts::namespace_operation);
    if (flags as u32 & O_TMPFILE) == O_TMPFILE {
        let mut policy = Openat2PathwalkPolicy::legacy()?;
        return with_path_fs(dirfd, Path::new(path), |fs| {
            open_tmpfile_in_fs(fs, path, flags, mode, &security, &mut policy)
        });
    }

    with_path_fs(dirfd, Path::new(path), |fs| {
        open_in_fs(fs, path, flags, mode, &security)
    })
}

fn open_tmpfile_in_fs<P: PathwalkPolicy + ?Sized>(
    fs: &mut FsContext,
    path: &str,
    flags: i32,
    mode: __kernel_mode_t,
    security: &OpenPathSecurityContext,
    policy: &mut P,
) -> AxResult<isize> {
    if (flags as u32 & O_ACCMODE) == O_RDONLY {
        return Err(AxError::InvalidInput);
    }

    let path_ref = Path::new(path);
    validate_pathname(path_ref)?;
    if path_ref.as_str().is_empty() {
        return Err(AxError::NotFound);
    }
    let credentials = security.credentials();
    let reservation = reserve_fd((flags as u32) & O_CLOEXEC != 0)?;
    let dir_loc = if flags as u32 & O_NOFOLLOW != 0 {
        fs.resolve_no_follow_with_policy(
            path_ref,
            &mut |dir| {
                check_pathwalk_search_permission_with_security(
                    dir,
                    security.actor(),
                    credentials,
                    security.filesystem_owner_user_ns(),
                )
            },
            policy,
        )
    } else {
        fs.resolve_with_policy(
            path_ref,
            &mut |dir| {
                check_pathwalk_search_permission_with_security(
                    dir,
                    security.actor(),
                    credentials,
                    security.filesystem_owner_user_ns(),
                )
            },
            policy,
        )
    }?;
    dir_loc.check_is_dir()?;
    check_create_permissions_with_security(
        &dir_loc,
        security.actor(),
        credentials,
        security.filesystem_owner_user_ns(),
    )?;

    let parent_meta = dir_loc.metadata()?;
    let (final_mode, owner) = initial_named_create_owner_mode_with_security(
        &parent_meta,
        security.vfs_security(),
        NodeType::RegularFile,
        NodePermission::from_bits_truncate(mode as u16),
        security.umask,
    );

    let open_flags = flags as u32 & !(O_TMPFILE | O_DIRECTORY | O_EXCL);
    let options = flags_to_options(
        open_flags as i32,
        final_mode.bits() as __kernel_mode_t,
        owner,
    );
    let loc = options.create_anonymous_location(&dir_loc, flags as u32 & O_EXCL == 0)?;
    let file_open_operation =
        file_open_operation(open_flags, true, true)?.ok_or(AxError::InvalidInput)?;
    let mut write_open = if open_has_data_write(open_flags) {
        Some(ExecutableWriteReservation::acquire(&loc, open_flags)?)
    } else {
        None
    };
    let (result, (late_write_open, lease_admission)) = open_loc_after_admission(
        &options,
        loc.clone(),
        |candidate| {
            authorize_file_open(
                candidate,
                security.actor(),
                credentials,
                security.filesystem_owner_user_ns(),
                file_open_operation,
            )
        },
        |candidate| {
            crate::file::fanotify::permission_check(
                candidate,
                candidate,
                crate::file::fanotify::FAN_OPEN_PERM,
                false,
                false,
            )?;
            let lease_admission = lease::admit_open(candidate, open_flags as i32)?;
            let late_write_open = if write_open.is_none() {
                Some(ExecutableWriteReservation::acquire(candidate, open_flags)?)
            } else {
                None
            };
            Ok((late_write_open, lease_admission))
        },
    )?;
    if write_open.is_none() {
        write_open = late_write_open;
    }
    let mut write_open = write_open.ok_or(AxError::BadState)?;
    let fd = publish_reserved_open(
        result,
        open_flags,
        reservation,
        write_open.transfer_persistent(),
        lease_admission,
        security.vfs_security().actor_arc().clone(),
    )?;
    if let Err(error) = notify_exact(&loc, IN_OPEN) {
        warn!("tmpfile open notification failed: {error}");
    }
    Ok(fd as isize)
}

pub fn sys_openat(
    capability: UserMemoryCapability,
    dirfd: c_int,
    path: *const c_char,
    flags: i32,
    mode: __kernel_mode_t,
) -> AxResult<isize> {
    let path = load_user_string(&capability, path)?;
    openat_inner(dirfd, &path, flags, mode)
}

// Linux implements creat(2) as openat(AT_FDCWD, path,
// O_CREAT|O_WRONLY|O_TRUNC|O_LARGEFILE, mode) when the native architecture
// uses 64-bit off_t.  Keep the kernel-visible O_LARGEFILE bit even though
// glibc exposes it as zero to x86_64 applications.
#[cfg(target_arch = "x86_64")]
const CREAT_OPEN_FLAGS: i32 = (O_CREAT | O_WRONLY | O_TRUNC | O_LARGEFILE) as i32;

#[cfg(target_arch = "x86_64")]
pub fn sys_creat(
    capability: UserMemoryCapability,
    path: *const c_char,
    mode: __kernel_mode_t,
) -> AxResult<isize> {
    sys_openat(capability, AT_FDCWD as _, path, CREAT_OPEN_FLAGS, mode)
}

pub fn sys_openat2(
    capability: UserMemoryCapability,
    dirfd: c_int,
    path: *const c_char,
    how_ptr: *const u8,
    size: usize,
) -> AxResult<isize> {
    if size < OPENAT2_HOW_SIZE {
        return Err(AxError::InvalidInput);
    }
    if size > 4096 {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    let mut raw = [0u8; 4096];
    let raw_destination =
        unsafe { slice::from_raw_parts_mut(raw.as_mut_ptr().cast::<MaybeUninit<u8>>(), size) };
    capability
        .read_bytes(how_ptr as usize, raw_destination)
        .map_err(map_usercopy_error)?;
    let raw = &raw[..size];
    if size > OPENAT2_HOW_SIZE && raw[OPENAT2_HOW_SIZE..].iter().any(|&byte| byte != 0) {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    let how = unsafe { ptr::read_unaligned(raw.as_ptr().cast::<open_how>()) };
    let flags = validate_openat2_how(&how)? as i32;
    let resolve_cached = how.resolve & RESOLVE_CACHED as u64 != 0;
    if resolve_cached && flags as u32 & (O_TRUNC | O_CREAT | __O_TMPFILE) != 0 {
        return Err(AxError::from(LinuxError::EAGAIN));
    }

    let path = load_user_string(&capability, path)?;
    let path_ref = Path::new(&path);
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    if how.resolve & RESOLVE_BENEATH as u64 != 0 && path_ref.is_absolute() {
        return Err(AxError::from(LinuxError::EXDEV));
    }
    if resolve_cached {
        return Err(AxError::from(LinuxError::EAGAIN));
    }
    let curr = current();
    let thread = curr.as_thread();
    let security = OpenPathSecurityContext::new(thread.current_cred(), current_fs_context().lock().umask());
    // Use one guard for both scoped pathwalk and creation. Combining the
    // predicates before acquisition avoids recursively locking the non-
    // reentrant namespace mutex when openat2 requests both.
    let mount_namespace = open_requires_namespace_operation(flags as u32, how.resolve)
        .then(crate::mounts::namespace_operation);
    let mut fs = openat2_context(dirfd, path_ref, how.resolve)?;
    let context = LinuxPathContext::new(
        security,
        mount_namespace,
        fs.root_dir().clone(),
        fs.current_dir().clone(),
        how.resolve,
        (),
        PathLimits::LINUX_DEFAULT,
    )
    .map_err(|error| match error {
        PathContextError::Resolve(_) | PathContextError::Limits(_) => AxError::InvalidInput,
        _ => AxError::InvalidInput,
    })?;
    let mut policy =
        Openat2PathwalkPolicy::from_parts(*context.resolve_policy(), context.limits())?;
    if (flags as u32 & O_TMPFILE) == O_TMPFILE {
        open_tmpfile_in_fs(
            &mut fs,
            &path,
            flags,
            how.mode as __kernel_mode_t,
            context.credentials(),
            &mut policy,
        )
    } else {
        open_in_fs_with_policy(
            &mut fs,
            &path,
            flags,
            how.mode as __kernel_mode_t,
            context.credentials(),
            &mut policy,
        )
    }
}

/// Open a file by `filename` and insert it into the file descriptor table.
///
/// Return its index in the file table (`fd`). Return `EMFILE` if it already
/// has the maximum number of files open.
#[cfg(target_arch = "x86_64")]
pub fn sys_open(
    capability: UserMemoryCapability,
    path: *const c_char,
    flags: i32,
    mode: __kernel_mode_t,
) -> AxResult<isize> {
    sys_openat(capability, AT_FDCWD as _, path, flags, mode)
}

pub fn sys_close(fd: c_int) -> AxResult<isize> {
    debug!("sys_close <= {fd}");
    close_file_like(fd)?;
    wait_current_close_notifications();
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
        let thread = curr.as_thread();
        if let Some(replacement) = thread.try_clone_fd_table_if_shared()? {
            drop(thread.replace_fd_table(replacement));
        }
    }

    let cloexec = flags.contains(CloseRangeFlags::CLOEXEC);
    if cloexec {
        current_fd_table().mark_cloexec_range(first, last);
    } else {
        drop(current_fd_table().close_range(first, last)?);
        wait_current_close_notifications();
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
    let min_fd = min_fd as usize;
    if min_fd >= max_nofile {
        return Err(AxError::InvalidInput);
    }

    let upper_bound = max_nofile.min(AX_FILE_LIMIT);
    current_fd_table()
        .add_at_least(description, min_fd, upper_bound, cloexec)
        .map(|fd| fd as isize)
}

fn validate_flock(lock: &flock64) -> AxResult<()> {
    match lock.l_whence as i32 {
        0..=2 => Ok(()),
        _ => Err(AxError::InvalidInput),
    }
}

fn read_user_flock64(
    capability: &UserMemoryCapability,
    address: usize,
) -> AxResult<(flock64, [u8; size_of::<flock64>()])> {
    let mut raw = [0u8; size_of::<flock64>()];
    let destination =
        unsafe { slice::from_raw_parts_mut(raw.as_mut_ptr().cast::<MaybeUninit<u8>>(), raw.len()) };
    capability
        .read_bytes(address, destination)
        .map_err(map_usercopy_error)?;
    // SAFETY: `read_bytes` initialized the complete object representation;
    // `read_unaligned` handles the byte-aligned local buffer.
    let lock = unsafe { ptr::read_unaligned(raw.as_ptr().cast::<flock64>()) };
    Ok((lock, raw))
}

fn flock64_bytes(
    lock: &flock64,
    mut raw: [u8; size_of::<flock64>()],
) -> [u8; size_of::<flock64>()] {
    let type_offset = core::mem::offset_of!(flock64, l_type);
    raw[type_offset..type_offset + size_of_val(&lock.l_type)]
        .copy_from_slice(&lock.l_type.to_ne_bytes());
    let whence_offset = core::mem::offset_of!(flock64, l_whence);
    raw[whence_offset..whence_offset + size_of_val(&lock.l_whence)]
        .copy_from_slice(&lock.l_whence.to_ne_bytes());
    let start_offset = core::mem::offset_of!(flock64, l_start);
    raw[start_offset..start_offset + size_of_val(&lock.l_start)]
        .copy_from_slice(&lock.l_start.to_ne_bytes());
    let len_offset = core::mem::offset_of!(flock64, l_len);
    raw[len_offset..len_offset + size_of_val(&lock.l_len)]
        .copy_from_slice(&lock.l_len.to_ne_bytes());
    let pid_offset = core::mem::offset_of!(flock64, l_pid);
    raw[pid_offset..pid_offset + size_of_val(&lock.l_pid)]
        .copy_from_slice(&lock.l_pid.to_ne_bytes());
    raw
}

fn f_owner_ex_bytes(owner: &f_owner_ex) -> [u8; size_of::<f_owner_ex>()] {
    let mut raw = [0u8; size_of::<f_owner_ex>()];
    let type_offset = core::mem::offset_of!(f_owner_ex, type_);
    raw[type_offset..type_offset + size_of_val(&owner.type_)]
        .copy_from_slice(&owner.type_.to_ne_bytes());
    let pid_offset = core::mem::offset_of!(f_owner_ex, pid);
    raw[pid_offset..pid_offset + size_of_val(&owner.pid)].copy_from_slice(&owner.pid.to_ne_bytes());
    raw
}

fn validate_getlk_type(lock: &flock64) -> AxResult<()> {
    match lock.l_type {
        ty if ty == F_RDLCK as i16 || ty == F_WRLCK as i16 => Ok(()),
        _ => Err(AxError::InvalidInput),
    }
}

fn validate_ofd_lock_pid(lock: &flock64) -> AxResult<()> {
    if lock.l_pid != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_record_lock_access(
    description: &crate::file::FileDescription,
    lock: &flock64,
) -> AxResult<()> {
    let flags = description.status_flags();
    if record_lock_access_allowed(flags, lock.l_type) {
        Ok(())
    } else {
        Err(AxError::BadFileDescriptor)
    }
}

fn record_lock_access_allowed(flags: u32, lock_type: i16) -> bool {
    if flags & O_PATH != 0 {
        return lock_type != F_RDLCK as i16 && lock_type != F_WRLCK as i16;
    }
    match lock_type {
        ty if ty == F_RDLCK as i16 => {
            matches!(flags & O_ACCMODE, O_RDONLY | O_RDWR)
        }
        ty if ty == F_WRLCK as i16 => {
            matches!(flags & O_ACCMODE, O_WRONLY | O_RDWR)
        }
        _ => true,
    }
}

fn record_lock_current_offset(
    description: &crate::file::FileDescription,
    lock: &flock64,
) -> AxResult<u64> {
    if lock.l_whence as u32 != SEEK_CUR {
        return Ok(0);
    }

    let Some(file) = description.inner.as_ref().downcast_ref::<File>() else {
        return Ok(0);
    };
    let mut inner = file.inner();
    inner.seek(SeekFrom::Current(0))
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

    let max_nofile = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE].current as usize;
    if new_fd < 0 || new_fd as usize >= max_nofile.min(AX_FILE_LIMIT) {
        return Err(AxError::BadFileDescriptor);
    }
    drop(current_fd_table().dup_replace(old_fd, new_fd, flags.contains(Dup3Flags::O_CLOEXEC))?);
    wait_current_close_notifications();

    Ok(new_fd as _)
}

pub fn sys_fcntl(
    capability: UserMemoryCapability,
    fd: c_int,
    cmd: c_int,
    arg: usize,
) -> AxResult<isize> {
    debug!("sys_fcntl <= fd: {fd} cmd: {cmd} arg: {arg}");
    let cmd = cmd as u32;

    if !fcntl_allowed_on_path_fd(cmd) {
        let description = get_file_description(fd)?;
        if description.status_flags() & O_PATH != 0 {
            return Err(AxError::BadFileDescriptor);
        }
    }

    match cmd {
        F_DUPFD => {
            let description = get_file_description(fd)?;
            dup_fd_at_least(description, arg as c_int, false)
        }
        F_DUPFD_CLOEXEC => {
            let description = get_file_description(fd)?;
            dup_fd_at_least(description, arg as c_int, true)
        }
        F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW => {
            let description = get_file_description(fd)?;
            let stat = description.inner.stat()?;
            let (lock, _) = read_user_flock64(&capability, arg)?;
            validate_flock(&lock)?;
            if matches!(cmd, F_OFD_SETLK | F_OFD_SETLKW) {
                validate_ofd_lock_pid(&lock)?;
            }
            validate_record_lock_access(&description, &lock)?;
            let current_offset = record_lock_current_offset(&description, &lock)?;
            let owner = match cmd {
                F_OFD_SETLK | F_OFD_SETLKW => RecordLockOwner::Ofd(description.flock_owner()),
                _ => RecordLockOwner::Posix(current().as_thread().proc_data.proc.pid()),
            };
            flock::set_record_lock(
                (stat.dev, stat.ino),
                owner,
                stat.size,
                current_offset,
                &lock,
                matches!(cmd, F_SETLKW | F_OFD_SETLKW),
            )?;
            Ok(0)
        }
        F_GETLK | F_OFD_GETLK => {
            let description = get_file_description(fd)?;
            let stat = description.inner.stat()?;
            let (mut lock, original) = read_user_flock64(&capability, arg)?;
            validate_flock(&lock)?;
            validate_getlk_type(&lock)?;
            if cmd == F_OFD_GETLK {
                validate_ofd_lock_pid(&lock)?;
            }
            let current_offset = record_lock_current_offset(&description, &lock)?;
            let owner = if cmd == F_OFD_GETLK {
                RecordLockOwner::Ofd(description.flock_owner())
            } else {
                RecordLockOwner::Posix(current().as_thread().proc_data.proc.pid())
            };
            flock::get_record_lock(
                (stat.dev, stat.ino),
                owner,
                stat.size,
                current_offset,
                &mut lock,
            )?;
            let result = flock64_bytes(&lock, original);
            capability
                .write_bytes(arg, &result)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        F_SETLEASE => {
            let file = File::from_fd(fd)?;
            lease::set_lease(file.as_ref(), file.open_file_description_key(), arg as i32)?;
            Ok(0)
        }
        F_GETLEASE => {
            let file = File::from_fd(fd)?;
            Ok(lease::get_lease(file.as_ref()) as isize)
        }
        F_SETOWN => {
            let description = get_file_description(fd)?;
            let owner = arg as c_int;
            let owner = if owner < 0 {
                AsyncIoOwner::pgrp(owner.checked_neg().ok_or(AxError::InvalidInput)? as _)?
            } else {
                AsyncIoOwner::pid(owner as _)?
            };
            description.set_async_io_owner(owner);
            sync_async_io_to_file_flags(&description, fd, description.io_status_snapshot().raw());
            Ok(0)
        }
        F_GETOWN => {
            let description = get_file_description(fd)?;
            let owner = description.async_io_state().owner;
            if !owner.is_live() {
                return Ok(0);
            }
            let owner = match owner.owner_type() {
                AsyncIoOwnerType::Tid | AsyncIoOwnerType::Pid => owner.id() as c_int,
                AsyncIoOwnerType::Pgrp => -(owner.id() as c_int),
            };
            Ok(owner as isize)
        }
        F_SETOWN_EX => {
            let description = get_file_description(fd)?;
            let owner = unsafe {
                capability
                    .read_value_uninit(arg as *const f_owner_ex)
                    .map_err(map_usercopy_error)?
                    .assume_init()
            };
            if owner.pid < 0 {
                return Err(AxError::NoSuchProcess);
            }
            let owner = match owner.type_ as u32 {
                F_OWNER_TID => AsyncIoOwner::tid(owner.pid as _)?,
                F_OWNER_PID => AsyncIoOwner::pid(owner.pid as _)?,
                F_OWNER_PGRP => AsyncIoOwner::pgrp(owner.pid as _)?,
                _ => return Err(AxError::InvalidInput),
            };
            description.set_async_io_owner(owner);
            sync_async_io_to_file_flags(&description, fd, description.io_status_snapshot().raw());
            Ok(0)
        }
        F_GETOWN_EX => {
            let description = get_file_description(fd)?;
            let state_owner = description.async_io_state().owner;
            let owner = f_owner_ex {
                type_: match state_owner.owner_type() {
                    AsyncIoOwnerType::Tid => F_OWNER_TID as _,
                    AsyncIoOwnerType::Pid => F_OWNER_PID as _,
                    AsyncIoOwnerType::Pgrp => F_OWNER_PGRP as _,
                },
                pid: if state_owner.is_live() {
                    state_owner.id() as _
                } else {
                    0
                },
            };
            let owner_bytes = f_owner_ex_bytes(&owner);
            capability
                .write_bytes(arg, &owner_bytes)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        F_SETSIG => {
            let description = get_file_description(fd)?;
            description.set_async_io_signal(validate_async_signal(arg as c_int)?);
            sync_async_io_to_file_flags(&description, fd, description.io_status_snapshot().raw());
            Ok(0)
        }
        F_GETSIG => {
            let description = get_file_description(fd)?;
            Ok(description.async_io_state().signal as isize)
        }
        F_NOTIFY => {
            let raw_mask = dnotify::mask_from_fcntl_arg(arg);
            let description = get_file_description(fd)?;
            let expected = description.id();
            if dnotify::is_remove_mask(raw_mask) {
                let detached =
                    current_fd_table().with_same_description(fd, expected, |table, _, description| {
                        Ok(dnotify::detach_watch(table, description.id()))
                    })?;
                drop(detached);
                return Ok(0);
            }

            let loc = if let Some(file) = description.inner.downcast_ref::<File>() {
                file.inner().location().clone()
            } else if let Some(dir) = description.inner.downcast_ref::<Directory>() {
                dir.inner().clone()
            } else {
                return Err(AxError::NotADirectory);
            };
            if !loc.is_dir() {
                return Err(AxError::NotADirectory);
            }
            let watch = WatchKey::from_location(&loc)?;
            let mask = dnotify::converted_mask(raw_mask);
            current_fd_table().prepare_dnotify_cleanup()?;
            current_fd_table().with_same_description(fd, expected, |table, fd, description| {
                dnotify::set_watch(table, fd, description, watch, mask)
            })?;
            Ok(0)
        }
        F_SETFL => {
            let description = get_file_description(fd)?;
            let requested = (arg as u32) & FCNTL_SETFL_MUTABLE_FLAGS;
            description.transition_status_flags(
                |old| (old.raw() & !FCNTL_SETFL_MUTABLE_FLAGS) | requested,
                |old, new| {
                    if old.nonblocking() != new.nonblocking() {
                        description.inner.set_nonblocking(new.nonblocking())?;
                    }
                    sync_async_io_to_file_flags(&description, fd, new.raw());
                    Ok(())
                },
            )?;
            Ok(0)
        }
        F_GETFL => {
            let description = get_file_description(fd)?;
            Ok(description.io_status_snapshot().raw() as _)
        }
        F_GETFD => {
            let cloexec = current_fd_table().get_cloexec(fd)?;
            Ok(if cloexec { FD_CLOEXEC as _ } else { 0 })
        }
        F_SETFD => {
            let cloexec = arg & FD_CLOEXEC as usize != 0;
            current_fd_table().set_cloexec(fd, cloexec)?;
            Ok(0)
        }
        F_GETPIPE_SZ => {
            let pipe = Pipe::from_fd(fd)?;
            Ok(pipe.capacity() as _)
        }
        F_SETPIPE_SZ => {
            let pipe = Pipe::from_fd(fd)?;
            Ok(pipe.resize(arg)? as _)
        }
        F_ADD_SEALS => {
            let file = get_typed_file::<File>(fd)?;
            memfd::add_seals(
                file.inner().location(),
                file.inner().flags().contains(FileFlags::WRITE),
                arg as u32,
            )?;
            Ok(0)
        }
        F_GET_SEALS => Ok(memfd::get_seals(get_typed_file::<File>(fd)?.inner().location())? as _),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_flock(fd: c_int, operation: c_int) -> AxResult<isize> {
    debug!("flock <= fd: {fd}, operation: {operation}");

    let description = get_file_description(fd)?;
    let locks_data = flock_operation_locks_data(operation)?;
    let flags = description.status_flags();
    if flags & O_PATH != 0 || (locks_data && flags & O_ACCMODE == O_ACCMODE) {
        return Err(AxError::BadFileDescriptor);
    }
    let stat = description.inner.stat()?;

    crate::file::flock::do_flock((stat.dev, stat.ino), description.flock_owner(), operation)?;
    Ok(0)
}

fn flock_operation_locks_data(operation: c_int) -> AxResult<bool> {
    const LOCK_SH: c_int = 1;
    const LOCK_EX: c_int = 2;
    const LOCK_NB: c_int = 4;
    const LOCK_UN: c_int = 8;

    match operation & !LOCK_NB {
        LOCK_SH | LOCK_EX => Ok(true),
        LOCK_UN => Ok(false),
        _ => Err(AxError::InvalidInput),
    }
}

#[cfg(test)]
mod namespace_operation_tests {
    use core::sync::atomic::{AtomicBool, Ordering};

    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};

    use super::*;
    use crate::pseudofs::tmp::MemoryFs;

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn creat_is_the_x86_64_openat_alias() {
        assert_eq!(
            CREAT_OPEN_FLAGS,
            (O_CREAT | O_WRONLY | O_TRUNC | O_LARGEFILE) as i32
        );
        assert_eq!(O_LARGEFILE, 0o100000);
        assert_ne!(open_status_flags(CREAT_OPEN_FLAGS as u32) & O_LARGEFILE, 0);
    }

    #[test]
    fn creative_open_and_scoped_walk_share_one_namespace_lock_domain() {
        assert!(!open_requires_namespace_operation(O_RDONLY, 0));
        assert!(!open_requires_namespace_operation(O_RDONLY | O_APPEND, 0));
        assert!(!open_requires_namespace_operation(O_ACCMODE, 0));
        assert!(open_requires_namespace_operation(O_WRONLY, 0));
        assert!(open_requires_namespace_operation(O_RDWR, 0));
        assert!(open_requires_namespace_operation(O_CREAT | O_WRONLY, 0));
        assert!(open_requires_namespace_operation(O_TRUNC | O_WRONLY, 0));
        assert!(open_requires_namespace_operation(
            __O_TMPFILE | O_DIRECTORY | O_WRONLY,
            0
        ));

        for resolve in [RESOLVE_BENEATH, RESOLVE_IN_ROOT, RESOLVE_NO_XDEV] {
            assert!(open_requires_namespace_operation(O_RDONLY, resolve as u64));
            assert!(open_requires_namespace_operation(
                O_CREAT | O_WRONLY,
                resolve as u64
            ));
        }
    }

    #[test]
    fn normalized_file_open_operation_preserves_path_truncate_and_tmpfile_facts() {
        assert!(file_open_operation(O_PATH, false, false).unwrap().is_none());

        let truncate = file_open_operation(O_RDONLY | O_TRUNC, false, false)
            .unwrap()
            .unwrap();
        assert_eq!(truncate.access(), FileOpenAccess::Read);
        assert!(truncate.truncate());

        let read_append = file_open_operation(O_RDONLY | O_APPEND, false, false)
            .unwrap()
            .unwrap();
        assert_eq!(read_append.access(), FileOpenAccess::Read);
        assert!(!read_append.append());

        let unnamed = file_open_operation(O_WRONLY, true, true).unwrap().unwrap();
        assert_eq!(unnamed.access(), FileOpenAccess::Write);
        assert!(unnamed.created());
        assert!(unnamed.unnamed());

        let no_data = file_open_operation(O_ACCMODE, false, false)
            .unwrap()
            .unwrap();
        assert_eq!(no_data.access(), FileOpenAccess::NoData);
        assert!(!no_data.access().reads());
        assert!(!no_data.access().writes());

        let unnamed_no_data = file_open_operation(O_ACCMODE, true, true).unwrap().unwrap();
        assert_eq!(unnamed_no_data.access(), FileOpenAccess::NoData);
        assert!(unnamed_no_data.unnamed());
    }

    #[test]
    fn noatime_requires_owner_match_or_namespace_relative_fowner() {
        assert!(noatime_allowed(O_RDONLY, false, false));
        assert!(noatime_allowed(O_NOATIME, true, false));
        assert!(noatime_allowed(O_NOATIME, false, true));
        assert!(!noatime_allowed(O_NOATIME, false, false));
    }

    #[test]
    fn reserved_no_data_mode_cannot_acquire_record_locks() {
        assert!(!record_lock_access_allowed(O_ACCMODE, F_RDLCK as i16));
        assert!(!record_lock_access_allowed(O_ACCMODE, F_WRLCK as i16));
        assert!(record_lock_access_allowed(O_ACCMODE, F_UNLCK as i16));

        assert!(record_lock_access_allowed(O_RDONLY, F_RDLCK as i16));
        assert!(!record_lock_access_allowed(O_RDONLY, F_WRLCK as i16));
        assert!(!record_lock_access_allowed(O_WRONLY, F_RDLCK as i16));
        assert!(record_lock_access_allowed(O_WRONLY, F_WRLCK as i16));
        assert!(record_lock_access_allowed(O_RDWR, F_RDLCK as i16));
        assert!(record_lock_access_allowed(O_RDWR, F_WRLCK as i16));
    }

    #[test]
    fn whole_file_lock_access_distinguishes_no_data_from_path_only() {
        const LOCK_SH: c_int = 1;
        const LOCK_EX: c_int = 2;
        const LOCK_NB: c_int = 4;
        const LOCK_UN: c_int = 8;

        assert_eq!(flock_operation_locks_data(LOCK_SH), Ok(true));
        assert_eq!(flock_operation_locks_data(LOCK_EX | LOCK_NB), Ok(true));
        assert_eq!(flock_operation_locks_data(LOCK_UN), Ok(false));
        assert_eq!(flock_operation_locks_data(0), Err(AxError::InvalidInput));

        let no_data = O_ACCMODE;
        assert!(flock_operation_locks_data(LOCK_SH).unwrap());
        assert_eq!(no_data & O_ACCMODE, O_ACCMODE);
        assert_eq!(O_PATH & O_PATH, O_PATH);
    }

    #[test]
    fn denied_pre_open_admission_cannot_truncate_or_prepare_a_write_lease() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "deny-before-truncate",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let node = loc.entry().as_file().unwrap();
        node.write_at(b"retained", 0).unwrap();

        let options = flags_to_options(O_RDONLY as i32 | O_TRUNC as i32, 0o600, (0, 0));
        let prepared = AtomicBool::new(false);
        let result = open_loc_after_admission(
            &options,
            loc.clone(),
            |_| Err(AxError::PermissionDenied),
            |_| {
                prepared.store(true, Ordering::SeqCst);
                Ok(())
            },
        );

        assert!(matches!(result, Err(AxError::PermissionDenied)));
        assert!(!prepared.load(Ordering::SeqCst));
        assert_eq!(node.len().unwrap(), b"retained".len() as u64);
    }

    #[test]
    fn admitted_open_truncate_revokes_capability_before_size_commit() {
        executable::init().unwrap();
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "open-truncate-killpriv",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let node = loc.entry().as_file().unwrap();
        node.write_at(b"truncate me", 0).unwrap();
        loc.set_xattr(
            crate::task::SECURITY_CAPABILITY_XATTR_NAME,
            &[1, 2, 3],
            axfs_ng_vfs::XattrSetMode::Upsert,
        )
        .unwrap();

        let options = flags_to_options(O_RDONLY as i32 | O_TRUNC as i32, 0o600, (0, 0));
        let result = options.open_loc_deferred_truncate(loc.clone()).unwrap();
        let file = File::new(result.into_file().unwrap());
        let namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let security = VfsSecurityContext::new(Cred::try_root(namespace).unwrap());
        commit_open_truncate(&file, &loc, &security).unwrap();

        assert_eq!(node.len().unwrap(), 0);
        assert_eq!(
            crate::file::xattr_provider::read_security_capability(&loc).unwrap(),
            None
        );
    }

    #[test]
    fn read_only_append_stays_read_only_below_the_ofd_status_layer() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "read-only-append",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let options = flags_to_options(O_RDONLY as i32 | O_APPEND as i32, 0o600, (0, 0));
        let OpenResult::File(file) = options.open_loc(loc).unwrap() else {
            panic!("regular file opened as a directory");
        };
        assert!(file.flags().contains(FileFlags::READ));
        assert!(!file.flags().contains(FileFlags::WRITE));
        assert!(!file.flags().contains(FileFlags::APPEND));
        assert_ne!(open_status_flags(O_RDONLY | O_APPEND) & O_APPEND, 0);
    }

    #[test]
    fn reserved_no_data_mode_runs_open_without_read_or_write_access() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "no-data-open",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let options = flags_to_options(O_ACCMODE as i32 | O_APPEND as i32, 0o600, (0, 0));
        let OpenResult::File(file) = options.open_loc(loc).unwrap() else {
            panic!("regular file opened as a directory");
        };
        assert!(
            !file.flags().intersects(
                FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND | FileFlags::PATH
            )
        );
        assert_ne!(open_status_flags(O_ACCMODE | O_APPEND) & O_APPEND, 0);
    }
}
