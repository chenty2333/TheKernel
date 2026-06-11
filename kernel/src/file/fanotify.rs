use alloc::{
    borrow::Cow,
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ffi::c_int,
    future::poll_fn,
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FileBackend, FileFlags};
use axfs_ng_vfs::{Location, NodeType};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::{
    current,
    future::{block_on, interruptible, poll_io},
};
use spin::Mutex;

use crate::{
    file::{
        Directory, File, FileLike, IoDst, IoSrc, add_file_like,
        inotify::{WatchKey, location_for_fd},
    },
    task::AsThread,
};

pub const FAN_ACCESS: u64 = 0x0000_0001;
pub const FAN_MODIFY: u64 = 0x0000_0002;
pub const FAN_ATTRIB: u64 = 0x0000_0004;
pub const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
pub const FAN_CLOSE_NOWRITE: u64 = 0x0000_0010;
pub const FAN_OPEN: u64 = 0x0000_0020;
pub const FAN_MOVED_FROM: u64 = 0x0000_0040;
pub const FAN_MOVED_TO: u64 = 0x0000_0080;
pub const FAN_CREATE: u64 = 0x0000_0100;
pub const FAN_DELETE: u64 = 0x0000_0200;
pub const FAN_DELETE_SELF: u64 = 0x0000_0400;
pub const FAN_MOVE_SELF: u64 = 0x0000_0800;
pub const FAN_OPEN_EXEC: u64 = 0x0000_1000;
pub const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
pub const FAN_FS_ERROR: u64 = 0x0000_8000;
pub const FAN_OPEN_PERM: u64 = 0x0001_0000;
pub const FAN_ACCESS_PERM: u64 = 0x0002_0000;
pub const FAN_OPEN_EXEC_PERM: u64 = 0x0004_0000;
pub const FAN_EVENT_ON_CHILD: u64 = 0x0800_0000;
pub const FAN_RENAME: u64 = 0x1000_0000;
pub const FAN_ONDIR: u64 = 0x4000_0000;
pub const FAN_CLOSE: u64 = FAN_CLOSE_WRITE | FAN_CLOSE_NOWRITE;
pub const FAN_MOVE: u64 = FAN_MOVED_FROM | FAN_MOVED_TO;

pub const FAN_CLOEXEC: u32 = 0x0000_0001;
pub const FAN_NONBLOCK: u32 = 0x0000_0002;
pub const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
pub const FAN_CLASS_CONTENT: u32 = 0x0000_0004;
pub const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
pub const FAN_UNLIMITED_QUEUE: u32 = 0x0000_0010;
pub const FAN_UNLIMITED_MARKS: u32 = 0x0000_0020;
pub const FAN_ENABLE_AUDIT: u32 = 0x0000_0040;
pub const FAN_REPORT_PIDFD: u32 = 0x0000_0080;
pub const FAN_REPORT_TID: u32 = 0x0000_0100;
pub const FAN_REPORT_FID: u32 = 0x0000_0200;
pub const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
pub const FAN_REPORT_NAME: u32 = 0x0000_0800;
pub const FAN_REPORT_TARGET_FID: u32 = 0x0000_1000;
pub const FAN_REPORT_DFID_NAME: u32 = FAN_REPORT_DIR_FID | FAN_REPORT_NAME;
pub const FAN_REPORT_DFID_NAME_TARGET: u32 =
    FAN_REPORT_DFID_NAME | FAN_REPORT_FID | FAN_REPORT_TARGET_FID;
pub const FAN_ALLOW: u32 = 0x01;
pub const FAN_DENY: u32 = 0x02;
pub const FAN_AUDIT: u32 = 0x10;
pub const FAN_INFO: u32 = 0x20;

pub const FAN_MARK_ADD: u32 = 0x0000_0001;
pub const FAN_MARK_REMOVE: u32 = 0x0000_0002;
pub const FAN_MARK_DONT_FOLLOW: u32 = 0x0000_0004;
pub const FAN_MARK_ONLYDIR: u32 = 0x0000_0008;
pub const FAN_MARK_MOUNT: u32 = 0x0000_0010;
pub const FAN_MARK_IGNORED_MASK: u32 = 0x0000_0020;
pub const FAN_MARK_IGNORED_SURV_MODIFY: u32 = 0x0000_0040;
pub const FAN_MARK_FLUSH: u32 = 0x0000_0080;
pub const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;
pub const FAN_MARK_EVICTABLE: u32 = 0x0000_0200;
pub const FAN_MARK_IGNORE: u32 = 0x0000_0400;

pub const FANOTIFY_METADATA_VERSION: u8 = 3;
pub const FAN_NOFD: i32 = -1;
pub const MAX_QUEUED_EVENTS: usize = 16384;
pub const MAX_USER_GROUPS: usize = 128;
pub const MAX_USER_MARKS: usize = 1048576;

const FANOTIFY_PERM_EVENTS: u64 = FAN_OPEN_PERM | FAN_ACCESS_PERM | FAN_OPEN_EXEC_PERM;
const FANOTIFY_EVENTS: u64 = FAN_ACCESS
    | FAN_MODIFY
    | FAN_ATTRIB
    | FAN_CLOSE
    | FAN_OPEN
    | FAN_OPEN_EXEC
    | FAN_MOVE
    | FAN_CREATE
    | FAN_DELETE
    | FAN_RENAME
    | FAN_DELETE_SELF
    | FAN_MOVE_SELF
    | FAN_FS_ERROR;
const ALL_FANOTIFY_EVENT_BITS: u64 =
    FANOTIFY_EVENTS | FANOTIFY_PERM_EVENTS | FAN_Q_OVERFLOW | FAN_ONDIR | FAN_EVENT_ON_CHILD;
const FANOTIFY_FID_BITS: u32 = FAN_REPORT_DFID_NAME_TARGET;
const FANOTIFY_ADMIN_INIT_FLAGS: u32 = FAN_CLASS_CONTENT
    | FAN_CLASS_PRE_CONTENT
    | FAN_REPORT_TID
    | FAN_REPORT_PIDFD
    | FAN_UNLIMITED_QUEUE
    | FAN_UNLIMITED_MARKS
    | FAN_ENABLE_AUDIT;
const FANOTIFY_USER_INIT_FLAGS: u32 =
    FAN_CLASS_NOTIF | FANOTIFY_FID_BITS | FAN_CLOEXEC | FAN_NONBLOCK;
const FANOTIFY_INIT_FLAGS: u32 = FANOTIFY_ADMIN_INIT_FLAGS | FANOTIFY_USER_INIT_FLAGS;
const FANOTIFY_MARK_FLAGS: u32 = FAN_MARK_ADD
    | FAN_MARK_REMOVE
    | FAN_MARK_FLUSH
    | FAN_MARK_DONT_FOLLOW
    | FAN_MARK_ONLYDIR
    | FAN_MARK_MOUNT
    | FAN_MARK_FILESYSTEM
    | FAN_MARK_IGNORED_MASK
    | FAN_MARK_IGNORED_SURV_MODIFY
    | FAN_MARK_EVICTABLE
    | FAN_MARK_IGNORE;
const FANOTIFY_RESPONSE_ACCESS: u32 = FAN_ALLOW | FAN_DENY;
const FANOTIFY_RESPONSE_FLAGS: u32 = FAN_AUDIT | FAN_INFO;
const FANOTIFY_RESPONSE_VALID_MASK: u32 = FANOTIFY_RESPONSE_ACCESS | FANOTIFY_RESPONSE_FLAGS;
const FANOTIFY_DIR_ENTRY_EVENTS: u64 = FAN_CREATE | FAN_DELETE | FAN_MOVE | FAN_RENAME;

#[repr(C)]
struct FanotifyEventMetadata {
    event_len: u32,
    vers: u8,
    reserved: u8,
    metadata_len: u16,
    mask: u64,
    fd: c_int,
    pid: c_int,
}

#[repr(C)]
struct FanotifyResponse {
    fd: c_int,
    response: u32,
}

#[derive(Clone)]
struct FanotifyMark {
    key: WatchKey,
    mask: u64,
    ignored_mask: u64,
    ignored_survives_modify: bool,
    is_dir: bool,
    scope: FanotifyScope,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FanotifyScope {
    Inode,
    Mount(u64),
    Filesystem(u64),
}

struct FanotifyEvent {
    mask: u64,
    fd_loc: Option<Location>,
    permission_id: Option<u64>,
    pid: c_int,
}

struct FanotifyPermissionEvent {
    id: u64,
    fd: Option<c_int>,
    response: Option<u32>,
}

struct FanotifyState {
    marks: Vec<FanotifyMark>,
    queue: VecDeque<FanotifyEvent>,
    pending_permissions: Vec<FanotifyPermissionEvent>,
    overflowed: bool,
    next_permission_id: u64,
    released: bool,
}

pub struct FanotifyFile {
    flags: u32,
    event_f_flags: u32,
    non_blocking: AtomicBool,
    state: Mutex<FanotifyState>,
    poll_rx: PollSet,
}

static FANOTIFY_FILES: Mutex<Vec<Weak<FanotifyFile>>> = Mutex::new(Vec::new());

pub fn validate_init_flags(flags: u32, event_f_flags: u32) -> AxResult<()> {
    if flags & !FANOTIFY_INIT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT)
        == (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT)
    {
        return Err(AxError::InvalidInput);
    }
    if flags & FANOTIFY_FID_BITS != 0 && flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT) != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & FAN_REPORT_NAME != 0 && flags & FAN_REPORT_DIR_FID == 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & FAN_REPORT_TARGET_FID != 0 && flags & FAN_REPORT_DFID_NAME != FAN_REPORT_DFID_NAME {
        return Err(AxError::InvalidInput);
    }
    if event_f_flags & !(linux_raw_sys::general::O_ACCMODE | linux_raw_sys::general::O_CLOEXEC) != 0
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

impl FanotifyFile {
    pub fn new(flags: u32, event_f_flags: u32) -> Arc<Self> {
        let file = Arc::new(Self {
            flags,
            event_f_flags,
            non_blocking: AtomicBool::new(flags & FAN_NONBLOCK != 0),
            state: Mutex::new(FanotifyState {
                marks: Vec::new(),
                queue: VecDeque::new(),
                pending_permissions: Vec::new(),
                overflowed: false,
                next_permission_id: 1,
                released: false,
            }),
            poll_rx: PollSet::new(),
        });
        FANOTIFY_FILES.lock().push(Arc::downgrade(&file));
        file
    }

    pub fn mark(&self, flags: u32, mask: u64, loc: Option<&Location>) -> AxResult<()> {
        validate_mark_flags(flags, mask)?;
        let mut state = self.state.lock();

        if flags & FAN_MARK_FLUSH != 0 {
            flush_marks(&mut state, flags);
            return Ok(());
        }

        let loc = loc.ok_or(AxError::BadFileDescriptor)?;
        validate_mark_target(self.flags, flags, mask, loc)?;
        let key = WatchKey::from_location(loc)?;
        let scope = mark_scope(flags, loc)?;

        if flags & (FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE) != 0 {
            update_ignored_mark(&mut state, key, scope, flags, mask);
        } else if flags & FAN_MARK_ADD != 0 {
            add_mark(&mut state, key, scope, flags, mask, loc)?;
        } else if flags & FAN_MARK_REMOVE != 0 {
            remove_mark(&mut state, key, scope, mask)?;
        }
        Ok(())
    }

    fn has_events(&self) -> bool {
        let state = self.state.lock();
        !state.released && !state.queue.is_empty()
    }

    fn enqueue_locked(
        state: &mut FanotifyState,
        unlimited_queue: bool,
        event: FanotifyEvent,
    ) -> bool {
        if state.released {
            return false;
        }
        if !unlimited_queue && state.queue.len() >= MAX_QUEUED_EVENTS {
            if !state.overflowed {
                state.overflowed = true;
                state.queue.push_back(FanotifyEvent {
                    mask: FAN_Q_OVERFLOW,
                    fd_loc: None,
                    permission_id: None,
                    pid: 0,
                });
                return true;
            }
            return false;
        }
        state.queue.push_back(event);
        true
    }

    fn report_fid(&self) -> bool {
        self.flags & FANOTIFY_FID_BITS != 0
    }

    pub(in crate::file) fn release(&self) {
        let mut state = self.state.lock();
        if state.released {
            return;
        }
        state.released = true;
        state.queue.clear();
        for event in &mut state.pending_permissions {
            if event.response.is_none() {
                event.response = Some(FAN_ALLOW);
            }
        }
        drop(state);
        self.poll_rx.wake();
    }

    fn handle_permission_response(&self, fd: c_int, response: u32) -> AxResult<()> {
        if response & !FANOTIFY_RESPONSE_VALID_MASK != 0 {
            return Err(AxError::InvalidInput);
        }
        match response & FANOTIFY_RESPONSE_ACCESS {
            FAN_ALLOW | FAN_DENY => {}
            _ => return Err(AxError::InvalidInput),
        }
        if response & FAN_AUDIT != 0 && self.flags & FAN_ENABLE_AUDIT == 0 {
            return Err(AxError::InvalidInput);
        }
        if response & FAN_INFO != 0 {
            return Err(AxError::InvalidInput);
        }
        if fd < 0 {
            return Err(AxError::InvalidInput);
        }

        let mut state = self.state.lock();
        let Some(event) = state
            .pending_permissions
            .iter_mut()
            .find(|event| event.fd == Some(fd) && event.response.is_none())
        else {
            return Err(LinuxError::ENOENT.into());
        };
        event.response = Some(response);
        drop(state);
        self.poll_rx.wake();
        Ok(())
    }
}

impl FileLike for FanotifyFile {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let mut state = self.state.lock();
            let metadata_len = size_of::<FanotifyEventMetadata>();
            if dst.remaining_mut() < metadata_len {
                return Err(AxError::InvalidInput);
            }
            let mut written = 0usize;

            while let Some(event) = state.queue.front() {
                if dst.remaining_mut() < metadata_len {
                    break;
                }
                let fd = event
                    .fd_loc
                    .as_ref()
                    .map_or(FAN_NOFD, |loc| opened_event_fd(self, loc));
                let metadata = FanotifyEventMetadata {
                    event_len: metadata_len as u32,
                    vers: FANOTIFY_METADATA_VERSION,
                    reserved: 0,
                    metadata_len: metadata_len as u16,
                    mask: event.mask,
                    fd,
                    pid: event.pid,
                };
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&metadata as *const FanotifyEventMetadata).cast::<u8>(),
                        metadata_len,
                    )
                };
                dst.write(bytes)?;
                let event = state.queue.pop_front().expect("queue front disappeared");
                if let Some(id) = event.permission_id
                    && let Some(pending) =
                        state.pending_permissions.iter_mut().find(|it| it.id == id)
                {
                    pending.fd = Some(fd);
                    if fd == FAN_NOFD && pending.response.is_none() {
                        pending.response = Some(FAN_ALLOW);
                    }
                }
                written += metadata_len;
            }

            if written == 0 {
                Err(AxError::WouldBlock)
            } else {
                Ok(written)
            }
        }))
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let len = src.remaining();
        if len < size_of::<FanotifyResponse>() {
            return Err(AxError::InvalidInput);
        }
        let mut response = [0_u8; size_of::<FanotifyResponse>()];
        src.read(&mut response)?;
        let fd = c_int::from_ne_bytes([response[0], response[1], response[2], response[3]]);
        let response = u32::from_ne_bytes([response[4], response[5], response[6], response[7]]);
        let mut discard = [0_u8; size_of::<FanotifyEventMetadata>()];
        while src.remaining() != 0 {
            let chunk = src.remaining().min(discard.len());
            src.read(&mut discard[..chunk])?;
        }
        self.handle_permission_response(fd, response)?;
        Ok(size_of::<FanotifyResponse>().min(len))
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> AxResult {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[fanotify]".into()
    }
}

impl Pollable for FanotifyFile {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.has_events());
        events.set(IoEvents::OUT, true);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
    }
}

fn validate_mark_flags(flags: u32, mask: u64) -> AxResult<()> {
    if flags & !FANOTIFY_MARK_FLAGS != 0 || mask & !ALL_FANOTIFY_EVENT_BITS != 0 {
        return Err(AxError::InvalidInput);
    }
    let commands = u32::from(flags & FAN_MARK_ADD != 0)
        + u32::from(flags & FAN_MARK_REMOVE != 0)
        + u32::from(flags & FAN_MARK_FLUSH != 0);
    if commands != 1 {
        return Err(AxError::InvalidInput);
    }
    if flags & FAN_MARK_FLUSH != 0 && mask != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & FAN_MARK_IGNORED_MASK != 0 && flags & FAN_MARK_IGNORE != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & FAN_MARK_IGNORE != 0 && flags & FAN_MARK_IGNORED_SURV_MODIFY == 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & FAN_MARK_MOUNT != 0 && flags & FAN_MARK_FILESYSTEM != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_mark_target(group_flags: u32, flags: u32, mask: u64, loc: &Location) -> AxResult<()> {
    if flags & FAN_MARK_ONLYDIR != 0 && !loc.is_dir() {
        return Err(AxError::NotADirectory);
    }
    if mask & FANOTIFY_PERM_EVENTS != 0
        && group_flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT) == 0
    {
        return Err(AxError::InvalidInput);
    }
    if group_flags & FANOTIFY_FID_BITS == 0
        && mask & (FAN_ATTRIB | FANOTIFY_DIR_ENTRY_EVENTS | FAN_DELETE_SELF | FAN_MOVE_SELF) != 0
    {
        return Err(AxError::InvalidInput);
    }
    if group_flags & FAN_REPORT_NAME == 0 && mask & FAN_RENAME != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & FAN_MARK_MOUNT != 0 && mask & FANOTIFY_DIR_ENTRY_EVENTS != 0 {
        return Err(AxError::InvalidInput);
    }
    if group_flags & FAN_REPORT_TARGET_FID != 0
        && !loc.is_dir()
        && mask & (FAN_DELETE | FAN_RENAME | FAN_ONDIR | FAN_EVENT_ON_CHILD) != 0
    {
        return Err(AxError::NotADirectory);
    }
    if flags & FAN_MARK_IGNORE != 0
        && !loc.is_dir()
        && mask & (FANOTIFY_DIR_ENTRY_EVENTS | FAN_ONDIR | FAN_EVENT_ON_CHILD) != 0
    {
        return Err(AxError::NotADirectory);
    }
    if flags & FAN_MARK_IGNORE != 0 && flags & FAN_MARK_IGNORED_SURV_MODIFY == 0 && loc.is_dir() {
        return Err(AxError::IsADirectory);
    }
    Ok(())
}

fn mark_scope(flags: u32, loc: &Location) -> AxResult<FanotifyScope> {
    let meta = loc.metadata()?;
    if flags & FAN_MARK_MOUNT != 0 {
        Ok(FanotifyScope::Mount(meta.device))
    } else if flags & FAN_MARK_FILESYSTEM != 0 {
        Ok(FanotifyScope::Filesystem(meta.device))
    } else {
        Ok(FanotifyScope::Inode)
    }
}

fn flush_marks(state: &mut FanotifyState, flags: u32) {
    state.marks.retain(|mark| match mark.scope {
        FanotifyScope::Inode => flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0,
        FanotifyScope::Mount(_) => flags & FAN_MARK_MOUNT == 0,
        FanotifyScope::Filesystem(_) => flags & FAN_MARK_FILESYSTEM == 0,
    });
}

fn add_mark(
    state: &mut FanotifyState,
    key: WatchKey,
    scope: FanotifyScope,
    flags: u32,
    mask: u64,
    loc: &Location,
) -> AxResult<()> {
    if let Some(mark) = state
        .marks
        .iter_mut()
        .find(|mark| mark.key == key && mark.scope == scope)
    {
        mark.mask |= mask & ALL_FANOTIFY_EVENT_BITS;
        mark.is_dir = loc.is_dir();
        return Ok(());
    }
    state.marks.push(FanotifyMark {
        key,
        mask: mask & ALL_FANOTIFY_EVENT_BITS,
        ignored_mask: 0,
        ignored_survives_modify: flags & FAN_MARK_IGNORED_SURV_MODIFY != 0,
        is_dir: loc.is_dir(),
        scope,
    });
    Ok(())
}

fn update_ignored_mark(
    state: &mut FanotifyState,
    key: WatchKey,
    scope: FanotifyScope,
    flags: u32,
    mask: u64,
) {
    if let Some(mark) = state
        .marks
        .iter_mut()
        .find(|mark| mark.key == key && mark.scope == scope)
    {
        if flags & FAN_MARK_REMOVE != 0 {
            mark.ignored_mask &= !mask;
        } else {
            mark.ignored_mask |= mask;
            mark.ignored_survives_modify = flags & FAN_MARK_IGNORED_SURV_MODIFY != 0;
        }
    }
}

fn remove_mark(
    state: &mut FanotifyState,
    key: WatchKey,
    scope: FanotifyScope,
    mask: u64,
) -> AxResult<()> {
    let Some(idx) = state
        .marks
        .iter()
        .position(|mark| mark.key == key && mark.scope == scope)
    else {
        return Err(AxError::InvalidInput);
    };
    state.marks[idx].mask &= !mask;
    state.marks[idx].ignored_mask &= !mask;
    if state.marks[idx].mask == 0 && state.marks[idx].ignored_mask == 0 {
        state.marks.remove(idx);
    }
    Ok(())
}

fn live_fanotify_files() -> Vec<Arc<FanotifyFile>> {
    let mut files = FANOTIFY_FILES.lock();
    let mut live = Vec::new();
    files.retain(|weak| {
        if let Some(file) = weak.upgrade() {
            live.push(file);
            true
        } else {
            false
        }
    });
    live
}

fn each_fanotify_file(mut f: impl FnMut(&Arc<FanotifyFile>)) {
    for file in live_fanotify_files() {
        f(&file);
    }
}

fn clone_readonly_fd(loc: &Location, cloexec: bool) -> AxResult<c_int> {
    if loc.metadata()?.node_type == NodeType::Directory {
        return add_file_like(Arc::new(Directory::new(loc.clone())), cloexec);
    }
    let file = axfs::File::new(FileBackend::Direct(loc.clone()), FileFlags::READ);
    add_file_like(Arc::new(File::new(file)), cloexec)
}

fn opened_event_fd(file: &FanotifyFile, loc: &Location) -> c_int {
    if file.report_fid() {
        return FAN_NOFD;
    }
    clone_readonly_fd(
        loc,
        file.event_f_flags & linux_raw_sys::general::O_CLOEXEC != 0,
    )
    .unwrap_or(FAN_NOFD)
}

fn fanotify_pid(file: &FanotifyFile) -> c_int {
    let thread = current();
    if file.flags & FAN_REPORT_TID != 0 {
        thread.as_thread().tid() as c_int
    } else {
        thread.as_thread().proc_data.proc.pid() as c_int
    }
}

fn fanotify_mark_matches(
    mark: &FanotifyMark,
    event_key: WatchKey,
    watch_key: WatchKey,
    is_dir: bool,
    parent_event: bool,
) -> bool {
    let matched = if parent_event {
        mark.key == watch_key && mark.is_dir && mark.mask & FAN_EVENT_ON_CHILD != 0
    } else {
        mark.key == event_key || scope_matches(mark.scope, event_key)
    };
    matched && (!is_dir || mark.mask & FAN_ONDIR != 0 || parent_event)
}

fn scope_matches(scope: FanotifyScope, key: WatchKey) -> bool {
    match scope {
        FanotifyScope::Inode => false,
        FanotifyScope::Mount(dev) | FanotifyScope::Filesystem(dev) => dev == key.dev,
    }
}

fn enqueue_permission_event(
    file: &Arc<FanotifyFile>,
    state: &mut FanotifyState,
    event_loc: &Location,
    mask: u64,
) -> AxResult<u64> {
    if state.released {
        return Err(AxError::Interrupted);
    }
    if file.flags & FAN_UNLIMITED_QUEUE == 0 && state.queue.len() >= MAX_QUEUED_EVENTS {
        return Err(LinuxError::EPERM.into());
    }
    let id = state.next_permission_id;
    state.next_permission_id = state.next_permission_id.wrapping_add(1).max(1);
    state.pending_permissions.push(FanotifyPermissionEvent {
        id,
        fd: None,
        response: None,
    });
    state.queue.push_back(FanotifyEvent {
        mask,
        fd_loc: Some(event_loc.clone()),
        permission_id: Some(id),
        pid: fanotify_pid(file),
    });
    Ok(id)
}

fn wait_for_permission_response(file: &Arc<FanotifyFile>, id: u64) -> AxResult<()> {
    let response = block_on(interruptible(poll_fn(|cx| {
        let mut state = file.state.lock();
        if let Some(idx) = state
            .pending_permissions
            .iter()
            .position(|event| event.id == id && event.response.is_some())
        {
            let event = state.pending_permissions.remove(idx);
            return Poll::Ready(event.response.unwrap_or(FAN_ALLOW));
        }
        if !state.pending_permissions.iter().any(|event| event.id == id) || state.released {
            return Poll::Ready(FAN_ALLOW);
        }
        file.poll_rx.register(cx.waker());
        if let Some(idx) = state
            .pending_permissions
            .iter()
            .position(|event| event.id == id && event.response.is_some())
        {
            let event = state.pending_permissions.remove(idx);
            Poll::Ready(event.response.unwrap_or(FAN_ALLOW))
        } else {
            Poll::Pending
        }
    })))
    .map_err(|_| AxError::Interrupted)?;

    match response & FANOTIFY_RESPONSE_ACCESS {
        FAN_ALLOW => Ok(()),
        FAN_DENY => Err(LinuxError::EPERM.into()),
        _ => Err(LinuxError::EPERM.into()),
    }
}

pub(crate) fn permission_check(
    event_loc: &Location,
    watch_loc: &Location,
    mask: u64,
    is_dir: bool,
    parent_event: bool,
) -> AxResult<()> {
    let event_key = WatchKey::from_location(event_loc)?;
    let watch_key = WatchKey::from_location(watch_loc)?;
    let mut waits = Vec::new();

    for file in live_fanotify_files() {
        let mut state = file.state.lock();
        if state.released {
            continue;
        }
        let should_queue = state.marks.clone().into_iter().any(|mark| {
            fanotify_mark_matches(&mark, event_key, watch_key, is_dir, parent_event)
                && mask & mark.mask != 0
                && mark.ignored_mask & mask == 0
        });
        if should_queue {
            let id = enqueue_permission_event(&file, &mut state, event_loc, mask)?;
            waits.push((file.clone(), id));
        }
        drop(state);
        if should_queue {
            file.poll_rx.wake();
        }
    }

    let mut denied = false;
    for (file, id) in waits {
        match wait_for_permission_response(&file, id) {
            Ok(()) => {}
            Err(err) if err == AxError::from(LinuxError::EPERM) => denied = true,
            Err(err) => return Err(err),
        }
    }
    if denied {
        Err(LinuxError::EPERM.into())
    } else {
        Ok(())
    }
}

pub(crate) fn permission_check_fd(fd: c_int, mask: u64) -> AxResult<()> {
    if let Some(loc) = location_for_fd(fd) {
        permission_check(&loc, &loc, mask, loc.is_dir(), false)?;
    }
    Ok(())
}

pub(crate) fn notify(
    event_loc: &Location,
    watch_loc: &Location,
    mask: u64,
    is_dir: bool,
    parent_event: bool,
) {
    let Ok(event_key) = WatchKey::from_location(event_loc) else {
        return;
    };
    let Ok(watch_key) = WatchKey::from_location(watch_loc) else {
        return;
    };
    each_fanotify_file(|file| {
        let mut state = file.state.lock();
        if state.released {
            return;
        }
        let mut wake = false;
        for mark in state.marks.clone() {
            if !fanotify_mark_matches(&mark, event_key, watch_key, is_dir, parent_event) {
                continue;
            }
            let event_mask = mask & mark.mask & !FAN_EVENT_ON_CHILD;
            if event_mask == 0 || mark.ignored_mask & event_mask != 0 {
                if event_mask & FAN_MODIFY != 0 && !mark.ignored_survives_modify {
                    if let Some(current_mark) = state
                        .marks
                        .iter_mut()
                        .find(|it| it.key == mark.key && it.scope == mark.scope)
                    {
                        current_mark.ignored_mask = 0;
                    }
                }
                continue;
            }
            let fd_loc = if event_mask
                & (FAN_ACCESS | FAN_MODIFY | FAN_CLOSE | FAN_OPEN | FAN_OPEN_EXEC)
                != 0
            {
                Some(event_loc.clone())
            } else {
                None
            };
            wake |= FanotifyFile::enqueue_locked(
                &mut state,
                file.flags & FAN_UNLIMITED_QUEUE != 0,
                FanotifyEvent {
                    mask: event_mask,
                    fd_loc,
                    permission_id: None,
                    pid: fanotify_pid(file),
                },
            );
            if event_mask & FAN_MODIFY != 0 && !mark.ignored_survives_modify {
                if let Some(current_mark) = state
                    .marks
                    .iter_mut()
                    .find(|it| it.key == mark.key && it.scope == mark.scope)
                {
                    current_mark.ignored_mask = 0;
                }
            }
        }
        drop(state);
        if wake {
            file.poll_rx.wake();
        }
    });
}
