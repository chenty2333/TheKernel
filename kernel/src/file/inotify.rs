use alloc::{
    borrow::Cow,
    collections::VecDeque,
    format,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::Location;
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::{
    current,
    future::{block_on, poll_io},
};
use linux_raw_sys::{
    general::{
        DN_ATTRIB, DN_MULTISHOT, DN_RENAME, IN_ACCESS, IN_ALL_EVENTS, IN_ATTRIB, IN_CLOSE_NOWRITE,
        IN_CLOSE_WRITE, IN_DELETE_SELF, IN_EXCL_UNLINK, IN_IGNORED, IN_ISDIR, IN_MASK_ADD,
        IN_MASK_CREATE, IN_MODIFY, IN_MOVE_SELF, IN_ONESHOT, IN_Q_OVERFLOW, IN_UNMOUNT, POLL_MSG,
        SI_SIGIO, inotify_event,
    },
    ioctl::FIONREAD,
};
use spin::Mutex;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use starry_vm::VmMutPtr;

use crate::{
    file::{Directory, File, FileLike, IoDst, get_file_like},
    task::{AsThread, send_signal_to_process},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatchKey {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl WatchKey {
    pub(crate) fn from_location(loc: &Location) -> AxResult<Self> {
        let meta = loc.metadata()?;
        Ok(Self {
            dev: meta.device,
            ino: meta.inode,
        })
    }
}

#[derive(Clone)]
struct Watch {
    wd: i32,
    key: WatchKey,
    mask: u32,
    is_dir: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct QueuedEvent {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

impl QueuedEvent {
    fn encoded_len(&self) -> usize {
        size_of::<inotify_event>() + self.encoded_name_len()
    }

    fn encoded_name_len(&self) -> usize {
        if self.name.is_empty() {
            0
        } else {
            let name_len = self.name.len() + 1;
            (name_len + size_of::<inotify_event>() - 1) & !(size_of::<inotify_event>() - 1)
        }
    }
}

struct InotifyState {
    next_wd: i32,
    watches: Vec<Watch>,
    queue: VecDeque<QueuedEvent>,
}

pub struct InotifyFile {
    state: Mutex<InotifyState>,
    non_blocking: AtomicBool,
    poll_rx: PollSet,
}

static INOTIFY_FILES: Mutex<Vec<Weak<InotifyFile>>> = Mutex::new(Vec::new());
static NEXT_COOKIE: AtomicU32 = AtomicU32::new(1);
pub(crate) const MAX_QUEUED_EVENTS: usize = 16384;
const INOTIFY_PERSISTENT_FLAGS: u32 = IN_EXCL_UNLINK | IN_ONESHOT;

#[derive(Clone)]
struct DnotifyWatch {
    key: WatchKey,
    fd: i32,
    owner: Pid,
    signal: u8,
    mask: u32,
}

static DNOTIFY_WATCHES: Mutex<Vec<DnotifyWatch>> = Mutex::new(Vec::new());

impl InotifyFile {
    pub fn new(non_blocking: bool) -> Arc<Self> {
        let file = Arc::new(Self {
            state: Mutex::new(InotifyState {
                next_wd: 1,
                watches: Vec::new(),
                queue: VecDeque::new(),
            }),
            non_blocking: AtomicBool::new(non_blocking),
            poll_rx: PollSet::new(),
        });
        INOTIFY_FILES.lock().push(Arc::downgrade(&file));
        file
    }

    pub fn add_watch(&self, loc: &Location, mask: u32) -> AxResult<i32> {
        if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
            return Err(AxError::InvalidInput);
        }

        let key = WatchKey::from_location(loc)?;
        let is_dir = loc.is_dir();
        let persistent_mask = mask & (IN_ALL_EVENTS | INOTIFY_PERSISTENT_FLAGS);
        let mut state = self.state.lock();

        if let Some(watch) = state.watches.iter_mut().find(|watch| watch.key == key) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(AxError::AlreadyExists);
            }
            if mask & IN_MASK_ADD != 0 {
                watch.mask |= persistent_mask;
            } else {
                watch.mask = persistent_mask;
            }
            watch.is_dir = is_dir;
            return Ok(watch.wd);
        }

        let wd = state.next_wd;
        state.next_wd += 1;
        state.watches.push(Watch {
            wd,
            key,
            mask: persistent_mask,
            is_dir,
        });
        Ok(wd)
    }

    pub fn remove_watch(&self, wd: i32) -> AxResult<()> {
        let mut state = self.state.lock();
        remove_watch_locked(&mut state, wd)?;
        let wake = InotifyFile::enqueue_locked(
            &mut state,
            QueuedEvent {
                wd,
                mask: IN_IGNORED,
                cookie: 0,
                name: Vec::new(),
            },
        );
        drop(state);
        if wake {
            self.poll_rx.wake();
        }
        Ok(())
    }

    pub fn fdinfo(&self) -> String {
        let state = self.state.lock();
        let mut out = String::new();
        for watch in &state.watches {
            out.push_str(&format!(
                "inotify wd:{} ino:{:x} sdev:{:x} mask:{:x}\n",
                watch.wd, watch.key.ino, watch.key.dev, watch.mask
            ));
        }
        out
    }

    fn enqueue_locked(state: &mut InotifyState, event: QueuedEvent) -> bool {
        if state.queue.back().is_some_and(|last| *last == event) {
            return false;
        }
        if state
            .queue
            .iter()
            .any(|queued| queued.mask == IN_Q_OVERFLOW && queued.wd == -1)
        {
            return false;
        }
        if event.mask != IN_Q_OVERFLOW && state.queue.len() >= MAX_QUEUED_EVENTS {
            state.queue.push_back(QueuedEvent {
                wd: -1,
                mask: IN_Q_OVERFLOW,
                cookie: 0,
                name: Vec::new(),
            });
            return true;
        }
        state.queue.push_back(event);
        true
    }

    fn has_events(&self) -> bool {
        !self.state.lock().queue.is_empty()
    }

    fn queued_bytes(&self) -> usize {
        self.state
            .lock()
            .queue
            .iter()
            .map(QueuedEvent::encoded_len)
            .sum()
    }
}

fn remove_watch_locked(state: &mut InotifyState, wd: i32) -> AxResult<()> {
    let len = state.watches.len();
    state.watches.retain(|watch| watch.wd != wd);
    if state.watches.len() == len {
        Err(AxError::InvalidInput)
    } else {
        Ok(())
    }
}

impl FileLike for InotifyFile {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let mut state = self.state.lock();
            let mut written = 0usize;

            while let Some(event) = state.queue.front() {
                let len = event.encoded_len();
                if written == 0 && dst.remaining_mut() < len {
                    return Err(AxError::InvalidInput);
                }
                if dst.remaining_mut() < len {
                    break;
                }

                let name_len = event.name.len() + usize::from(!event.name.is_empty());
                let padded_name_len = event.encoded_name_len();
                let header = inotify_event {
                    wd: event.wd,
                    mask: event.mask,
                    cookie: event.cookie,
                    len: padded_name_len as u32,
                    name: linux_raw_sys::general::__IncompleteArrayField::new(),
                };
                let header_bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&header as *const inotify_event).cast::<u8>(),
                        size_of::<inotify_event>(),
                    )
                };
                dst.write(header_bytes)?;
                if name_len > 0 {
                    dst.write(&event.name)?;
                    dst.write(&[0])?;
                    if padded_name_len > name_len {
                        dst.write(&vec![0; padded_name_len - name_len])?;
                    }
                }
                written += len;
                state.queue.pop_front();
            }

            if written == 0 {
                Err(AxError::WouldBlock)
            } else {
                Ok(written)
            }
        }))
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> AxResult {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[inotify]".into()
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            FIONREAD => {
                (arg as *mut u32).vm_write(self.queued_bytes() as u32)?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }
}

impl Pollable for InotifyFile {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.has_events());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
    }
}

pub(crate) fn notify_unmount_device(dev: u64) {
    each_inotify_file(|file| {
        let mut state = file.state.lock();
        let mut wake = false;
        let mut idx = 0;
        while idx < state.watches.len() {
            if state.watches[idx].key.dev != dev {
                idx += 1;
                continue;
            }

            let wd = state.watches[idx].wd;
            state.watches.remove(idx);
            wake |= InotifyFile::enqueue_locked(
                &mut state,
                QueuedEvent {
                    wd,
                    mask: IN_UNMOUNT,
                    cookie: 0,
                    name: Vec::new(),
                },
            );
            wake |= InotifyFile::enqueue_locked(
                &mut state,
                QueuedEvent {
                    wd,
                    mask: IN_IGNORED,
                    cookie: 0,
                    name: Vec::new(),
                },
            );
        }
        drop(state);
        if wake {
            file.poll_rx.wake();
        }
    });
}

fn each_inotify_file(mut f: impl FnMut(&Arc<InotifyFile>)) {
    let mut files = INOTIFY_FILES.lock();
    files.retain(|weak| {
        if let Some(file) = weak.upgrade() {
            f(&file);
            true
        } else {
            false
        }
    });
}

pub(crate) fn set_dnotify_watch(fd: i32, loc: &Location, mask: u32, signal: u8) -> AxResult<()> {
    if !loc.is_dir() {
        return Err(AxError::NotADirectory);
    }

    let owner = current().as_thread().proc_data.proc.pid();
    let key = WatchKey::from_location(loc)?;
    let mut watches = DNOTIFY_WATCHES.lock();
    watches.retain(|watch| watch.owner != owner || watch.fd != fd);
    if mask != 0 {
        watches.push(DnotifyWatch {
            key,
            fd,
            owner,
            signal,
            mask,
        });
    }
    Ok(())
}

pub(crate) fn remove_dnotify_watch(fd: i32) {
    let owner = current().as_thread().proc_data.proc.pid();
    DNOTIFY_WATCHES
        .lock()
        .retain(|watch| watch.owner != owner || watch.fd != fd);
}

fn dnotify_siginfo(signal: u8, fd: i32) -> SignalInfo {
    let signo = if signal == 0 {
        Signo::SIGIO
    } else {
        Signo::from_repr(signal).unwrap_or(Signo::SIGIO)
    };
    let mut info = SignalInfo::new_kernel(signo);
    info.set_code(SI_SIGIO);
    unsafe {
        let sigpoll = &mut info.0.__bindgen_anon_1.__bindgen_anon_1._sifields._sigpoll;
        sigpoll._fd = fd;
        sigpoll._band = POLL_MSG as _;
    }
    info
}

fn emit_dnotify(key: WatchKey, event: u32) {
    let mut signals = Vec::new();
    {
        let mut watches = DNOTIFY_WATCHES.lock();
        watches.retain(|watch| {
            if watch.key != key || watch.mask & event == 0 {
                return true;
            }
            signals.push((watch.owner, dnotify_siginfo(watch.signal, watch.fd)));
            watch.mask & DN_MULTISHOT != 0
        });
    }

    for (pid, signal) in signals {
        let _ = send_signal_to_process(pid, Some(signal));
    }
}

fn emit_to_matching_watches(
    key: WatchKey,
    mask: u32,
    cookie: u32,
    name: &[u8],
    require_dir: bool,
    unlinked_child: bool,
) {
    let interest = mask & IN_ALL_EVENTS;
    each_inotify_file(|file| {
        let mut state = file.state.lock();
        let mut idx = 0;
        let mut wake = false;
        while idx < state.watches.len() {
            let watch = state.watches[idx].clone();
            if watch.key != key {
                idx += 1;
                continue;
            }
            if require_dir && !watch.is_dir {
                idx += 1;
                continue;
            }
            if unlinked_child && watch.mask & IN_EXCL_UNLINK != 0 {
                idx += 1;
                continue;
            }
            if interest != 0 && watch.mask & interest == 0 {
                idx += 1;
                continue;
            }
            wake |= InotifyFile::enqueue_locked(
                &mut state,
                QueuedEvent {
                    wd: watch.wd,
                    mask,
                    cookie,
                    name: name.to_vec(),
                },
            );
            let remove_watch =
                watch.mask & IN_ONESHOT != 0 || mask & (IN_DELETE_SELF | IN_UNMOUNT) != 0;
            if remove_watch {
                state.watches.remove(idx);
                wake |= InotifyFile::enqueue_locked(
                    &mut state,
                    QueuedEvent {
                        wd: watch.wd,
                        mask: IN_IGNORED,
                        cookie: 0,
                        name: Vec::new(),
                    },
                );
            } else {
                idx += 1;
            }
        }
        drop(state);
        if wake {
            file.poll_rx.wake();
        }
    });
}

fn exact_dir_mask(mask: u32, is_dir: bool) -> u32 {
    if is_dir && mask != IN_MOVE_SELF && mask != IN_DELETE_SELF {
        mask | IN_ISDIR
    } else {
        mask
    }
}

pub(crate) fn next_rename_cookie() -> u32 {
    NEXT_COOKIE.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn notify_exact(loc: &Location, mut mask: u32) -> AxResult<()> {
    mask = exact_dir_mask(mask, loc.is_dir());
    let key = WatchKey::from_location(loc)?;
    emit_to_matching_watches(key, mask, 0, &[], false, false);
    crate::file::fanotify::notify(loc, loc, inotify_to_fanotify(mask), loc.is_dir(), false);
    if mask & IN_ATTRIB != 0 {
        emit_dnotify(key, DN_ATTRIB);
    }
    Ok(())
}

pub(crate) fn notify_exact_with_cookie(loc: &Location, mut mask: u32, cookie: u32) -> AxResult<()> {
    mask = exact_dir_mask(mask, loc.is_dir());
    emit_to_matching_watches(
        WatchKey::from_location(loc)?,
        mask,
        cookie,
        &[],
        false,
        false,
    );
    Ok(())
}

pub(crate) fn notify_parent(loc: &Location, mut mask: u32) -> AxResult<()> {
    let Some(parent) = loc.parent() else {
        return Ok(());
    };
    if loc.is_dir() {
        mask |= IN_ISDIR;
    }
    let key = WatchKey::from_location(&parent)?;
    let unlinked_child = matches!(loc.metadata(), Ok(meta) if meta.nlink == 0);
    emit_to_matching_watches(key, mask, 0, loc.name().as_bytes(), true, unlinked_child);
    crate::file::fanotify::notify(loc, &parent, inotify_to_fanotify(mask), loc.is_dir(), true);
    if mask & IN_ATTRIB != 0 {
        emit_dnotify(key, DN_ATTRIB);
    }
    Ok(())
}

pub(crate) fn notify_parent_with_name(
    parent: &Location,
    child: Option<&Location>,
    child_name: &str,
    mut mask: u32,
    is_dir: bool,
    cookie: u32,
) -> AxResult<()> {
    if is_dir {
        mask |= IN_ISDIR;
    }
    emit_to_matching_watches(
        WatchKey::from_location(parent)?,
        mask,
        cookie,
        child_name.as_bytes(),
        true,
        false,
    );
    crate::file::fanotify::notify(
        child.unwrap_or(parent),
        parent,
        inotify_to_fanotify(mask),
        is_dir,
        true,
    );
    Ok(())
}

fn inotify_to_fanotify(mask: u32) -> u64 {
    let mut out = 0;
    if mask & IN_ACCESS != 0 {
        out |= crate::file::fanotify::FAN_ACCESS;
    }
    if mask & IN_MODIFY != 0 {
        out |= crate::file::fanotify::FAN_MODIFY;
    }
    if mask & IN_ATTRIB != 0 {
        out |= crate::file::fanotify::FAN_ATTRIB;
    }
    if mask & IN_CLOSE_WRITE != 0 {
        out |= crate::file::fanotify::FAN_CLOSE_WRITE;
    }
    if mask & IN_CLOSE_NOWRITE != 0 {
        out |= crate::file::fanotify::FAN_CLOSE_NOWRITE;
    }
    if mask & linux_raw_sys::general::IN_OPEN != 0 {
        out |= crate::file::fanotify::FAN_OPEN;
    }
    if mask & linux_raw_sys::general::IN_MOVED_FROM != 0 {
        out |= crate::file::fanotify::FAN_MOVED_FROM;
    }
    if mask & linux_raw_sys::general::IN_MOVED_TO != 0 {
        out |= crate::file::fanotify::FAN_MOVED_TO;
    }
    if mask & linux_raw_sys::general::IN_CREATE != 0 {
        out |= crate::file::fanotify::FAN_CREATE;
    }
    if mask & linux_raw_sys::general::IN_DELETE != 0 {
        out |= crate::file::fanotify::FAN_DELETE;
    }
    if mask & IN_DELETE_SELF != 0 {
        out |= crate::file::fanotify::FAN_DELETE_SELF;
    }
    if mask & IN_MOVE_SELF != 0 {
        out |= crate::file::fanotify::FAN_MOVE_SELF;
    }
    if mask & IN_ISDIR != 0 {
        out |= crate::file::fanotify::FAN_ONDIR;
    }
    out
}

pub(crate) fn notify_dnotify_rename(old_parent: &Location, new_parent: &Location) -> AxResult<()> {
    let old_key = WatchKey::from_location(old_parent)?;
    if old_key == WatchKey::from_location(new_parent)? {
        emit_dnotify(old_key, DN_RENAME);
    }
    Ok(())
}

pub(crate) fn notify_read(fd: i32) {
    if let Ok(file) = File::from_fd(fd) {
        let loc = file.inner().location().clone();
        let _ = notify_exact(&loc, IN_ACCESS);
        let _ = notify_parent(&loc, IN_ACCESS);
    }
}

pub(crate) fn notify_write(fd: i32) {
    if let Ok(file) = File::from_fd(fd) {
        let loc = file.inner().location().clone();
        let _ = notify_exact(&loc, IN_MODIFY);
        let _ = notify_parent(&loc, IN_MODIFY);
    }
}

pub(crate) fn notify_close(fd: i32) {
    if let Ok(file_like) = get_file_like(fd)
        && let Some(file) = file_like.downcast_ref::<File>()
    {
        let loc = file.inner().location().clone();
        let flags = file.inner().flags();
        let close_mask = if flags.intersects(axfs::FileFlags::WRITE | axfs::FileFlags::APPEND) {
            IN_CLOSE_WRITE
        } else {
            IN_CLOSE_NOWRITE
        };
        let _ = notify_exact(&loc, close_mask);
        let _ = notify_parent(&loc, close_mask);
    }
}

pub(crate) fn location_for_fd(fd: i32) -> Option<Location> {
    if let Ok(file_like) = get_file_like(fd) {
        if let Some(file) = file_like.downcast_ref::<File>() {
            return Some(file.inner().location().clone());
        }
        if let Some(dir) = file_like.downcast_ref::<Directory>() {
            return Some(dir.inner().clone());
        }
    }
    None
}
