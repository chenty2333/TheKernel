use alloc::{
    borrow::Cow,
    collections::VecDeque,
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
use axtask::future::{block_on, poll_io};
use linux_raw_sys::general::{
    IN_ACCESS, IN_ALL_EVENTS, IN_CLOSE_NOWRITE, IN_CLOSE_WRITE, IN_DELETE_SELF, IN_ISDIR,
    IN_MODIFY, IN_MOVE_SELF, inotify_event,
};
use spin::Mutex;

use crate::file::{Directory, File, FileLike, IoDst, get_file_like};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatchKey {
    dev: u64,
    ino: u64,
}

impl WatchKey {
    fn from_location(loc: &Location) -> AxResult<Self> {
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
        let name_len = if self.name.is_empty() {
            0
        } else {
            self.name.len() + 1
        };
        let name_len = (name_len + 3) & !3;
        size_of::<inotify_event>() + name_len
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
        let key = WatchKey::from_location(loc)?;
        let is_dir = loc.is_dir();
        let mut state = self.state.lock();
        let wd = state.next_wd;
        state.next_wd += 1;
        state.watches.push(Watch {
            wd,
            key,
            mask,
            is_dir,
        });
        Ok(wd)
    }

    pub fn remove_watch(&self, wd: i32) -> AxResult<()> {
        let mut state = self.state.lock();
        let len = state.watches.len();
        state.watches.retain(|watch| watch.wd != wd);
        if state.watches.len() == len {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }

    fn enqueue(&self, event: QueuedEvent) {
        let mut state = self.state.lock();
        if state.queue.back().is_some_and(|last| *last == event) {
            return;
        }
        state.queue.push_back(event);
        self.poll_rx.wake();
    }

    fn has_events(&self) -> bool {
        !self.state.lock().queue.is_empty()
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

                let name_len = if event.name.is_empty() {
                    0
                } else {
                    event.name.len() + 1
                };
                let padded_name_len = (name_len + 3) & !3;
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

fn emit_to_matching_watches(key: WatchKey, mask: u32, cookie: u32, name: &[u8], require_dir: bool) {
    let interest = mask & IN_ALL_EVENTS;
    each_inotify_file(|file| {
        let watches = file.state.lock().watches.clone();
        for watch in watches {
            if watch.key != key {
                continue;
            }
            if require_dir && !watch.is_dir {
                continue;
            }
            if interest != 0 && watch.mask & interest == 0 {
                continue;
            }
            file.enqueue(QueuedEvent {
                wd: watch.wd,
                mask,
                cookie,
                name: name.to_vec(),
            });
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
    emit_to_matching_watches(WatchKey::from_location(loc)?, mask, 0, &[], false);
    Ok(())
}

pub(crate) fn notify_exact_with_cookie(loc: &Location, mut mask: u32, cookie: u32) -> AxResult<()> {
    mask = exact_dir_mask(mask, loc.is_dir());
    emit_to_matching_watches(WatchKey::from_location(loc)?, mask, cookie, &[], false);
    Ok(())
}

pub(crate) fn notify_parent(loc: &Location, mut mask: u32) -> AxResult<()> {
    let Some(parent) = loc.parent() else {
        return Ok(());
    };
    if loc.is_dir() {
        mask |= IN_ISDIR;
    }
    emit_to_matching_watches(
        WatchKey::from_location(&parent)?,
        mask,
        0,
        loc.name().as_bytes(),
        true,
    );
    Ok(())
}

pub(crate) fn notify_parent_with_name(
    parent: &Location,
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
    );
    Ok(())
}

pub(crate) fn notify_read(fd: i32) {
    if let Ok(file) = File::from_fd(fd) {
        let loc = file.inner().location().clone();
        let _ = notify_exact(&loc, IN_ACCESS);
    }
}

pub(crate) fn notify_write(fd: i32) {
    if let Ok(file) = File::from_fd(fd) {
        let loc = file.inner().location().clone();
        let _ = notify_exact(&loc, IN_MODIFY);
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
