use alloc::{
    alloc::alloc,
    borrow::Cow,
    boxed::Box,
    collections::VecDeque,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    alloc::Layout,
    mem::size_of,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, WeakFilesystemIdentity, path::MAX_NAME_LEN};
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::Mutex as BlockingMutex;
use axtask::{
    current_may_uninit,
    future::{block_on, poll_io},
};
use linux_raw_sys::{
    general::{
        IN_ACCESS, IN_ALL_EVENTS, IN_ATTRIB, IN_CLOSE_NOWRITE, IN_CLOSE_WRITE, IN_DELETE_SELF,
        IN_EXCL_UNLINK, IN_IGNORED, IN_ISDIR, IN_MASK_ADD, IN_MASK_CREATE, IN_MODIFY, IN_MOVE_SELF,
        IN_ONESHOT, IN_Q_OVERFLOW, IN_UNMOUNT, inotify_event,
    },
    ioctl::FIONREAD,
};
use spin::Mutex;
use starry_vm::VmMutPtr;

use crate::{
    deferred_work::DeferredWorkAccount,
    file::{Directory, File, FileLike, IoDst, Kstat, fanotify::FanotifyEventActor, get_file_like},
    task::AsThread,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
    serial: u64,
    key: WatchKey,
    filesystem: WeakFilesystemIdentity,
    mask: u32,
    is_dir: bool,
}

impl Watch {
    fn filesystem_released(&self) -> bool {
        self.filesystem.upgrade().is_none()
    }
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
    next_watch_serial: u64,
    /// Stable reusable slots keep deferred-release removal O(1) and make a
    /// bounded cursor resilient to concurrent add/remove operations.
    watches: Vec<Option<Watch>>,
    queue: VecDeque<QueuedEvent>,
}

pub struct InotifyFile {
    state: Mutex<InotifyState>,
    read_gate: BlockingMutex<()>,
    non_blocking: AtomicBool,
    poll_rx: PollSet,
}

/// Stable, reusable registry slots. Slots are never reordered, so a bounded
/// release scan can drop the registry lock and safely resume by slot number.
static INOTIFY_FILES: Mutex<[Option<Weak<InotifyFile>>; MAX_INOTIFY_INSTANCES]> =
    Mutex::new([const { None }; MAX_INOTIFY_INSTANCES]);
static INOTIFY_SLOT_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);
static NEXT_COOKIE: AtomicU32 = AtomicU32::new(1);
pub(crate) const MAX_QUEUED_EVENTS: usize = 16384;
/// TheKernel does not yet have per-real-UID inotify accounting, so enforce
/// explicit global/per-instance ceilings instead of permitting unbounded
/// kernel memory growth. These can move behind Linux-style sysctls once the
/// credential/accounting layer is ready.
const MAX_INOTIFY_INSTANCES: usize = 8192;
const MAX_WATCHES_PER_INSTANCE: usize = 65_536;
const INOTIFY_PERSISTENT_FLAGS: u32 = IN_EXCL_UNLINK | IN_ONESHOT;
const RELEASE_FILES_PER_BATCH: usize = 16;
const RELEASE_WATCHES_PER_BATCH: usize = 256;
const CLOSE_NOTIFICATIONS_PER_BATCH: usize = 64;
const MAX_INOTIFY_EVENT_SIZE: usize = size_of::<inotify_event>()
    + (MAX_NAME_LEN + 1 + size_of::<inotify_event>() - 1) / size_of::<inotify_event>()
        * size_of::<inotify_event>();

/// Preallocated final-OFD notification. FileDescription::drop only publishes
/// this node to the lock-free stack; filesystem, inotify, fanotify, and
/// dnotify work is deferred to a task-context safe point.
pub(crate) struct CloseWork {
    next: AtomicPtr<CloseWork>,
    location: Location,
    key: WatchKey,
    parent: Option<CloseParent>,
    is_dir: bool,
    mask: u32,
    actor: FanotifyEventActor,
    account: Option<Arc<DeferredWorkAccount>>,
}

struct CloseParent {
    key: WatchKey,
}

impl CloseWork {
    fn try_new(location: Location, mask: u32) -> AxResult<Box<Self>> {
        let key = WatchKey::from_location(&location)?;
        let parent = location
            .parent()
            .map(|parent| {
                Ok::<CloseParent, AxError>(CloseParent {
                    key: WatchKey::from_location(&parent)?,
                })
            })
            .transpose()?;
        let is_dir = location.is_dir();
        let raw = unsafe { alloc(Layout::new::<Self>()) }.cast::<Self>();
        if raw.is_null() {
            return Err(AxError::NoMemory);
        }
        unsafe {
            raw.write(Self {
                next: AtomicPtr::new(ptr::null_mut()),
                location,
                key,
                parent,
                is_dir,
                mask,
                actor: FanotifyEventActor::default(),
                account: None,
            });
            Ok(Box::from_raw(raw))
        }
    }

    fn run(&self) {
        if let Err(error) =
            notify_exact_prepared(&self.location, self.key, self.is_dir, self.mask, self.actor)
        {
            warn!("deferred close exact notification failed: {error}");
        }
        if let Some(parent) = self.parent.as_ref()
            && let Err(error) = notify_parent_prepared(
                &self.location,
                self.key,
                parent,
                self.is_dir,
                self.mask,
                self.actor,
            )
        {
            warn!("deferred close parent notification failed: {error}");
        }
        if let Some(account) = self.account.as_ref() {
            account.complete();
        }
    }
}

/// Producers publish to an intrusive LIFO stack without allocating or taking
/// a lock. The single drainer atomically snapshots and reverses this stack
/// before consuming it, preserving FIFO order and preventing old work from
/// being starved by a continuous stream of closes.
static CLOSE_WORK_INCOMING: AtomicPtr<CloseWork> = AtomicPtr::new(ptr::null_mut());
static CLOSE_WORK_PENDING: AtomicPtr<CloseWork> = AtomicPtr::new(ptr::null_mut());
static CLOSE_WORK_DRAINING: AtomicBool = AtomicBool::new(false);

struct CloseWorkDrainGuard;

impl CloseWorkDrainGuard {
    fn try_enter() -> Option<Self> {
        CLOSE_WORK_DRAINING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for CloseWorkDrainGuard {
    fn drop(&mut self) {
        CLOSE_WORK_DRAINING.store(false, Ordering::Release);
    }
}

fn refill_pending_close_work() {
    if !CLOSE_WORK_PENDING.load(Ordering::Relaxed).is_null() {
        return;
    }

    let mut current = CLOSE_WORK_INCOMING.swap(ptr::null_mut(), Ordering::AcqRel);
    let mut reversed = ptr::null_mut();
    while !current.is_null() {
        let next = unsafe { (*current).next.load(Ordering::Relaxed) };
        unsafe { (*current).next.store(reversed, Ordering::Relaxed) };
        reversed = current;
        current = next;
    }
    CLOSE_WORK_PENDING.store(reversed, Ordering::Relaxed);
}

fn pop_pending_close_work() -> Option<Box<CloseWork>> {
    if CLOSE_WORK_PENDING.load(Ordering::Relaxed).is_null() {
        refill_pending_close_work();
    }
    let head = CLOSE_WORK_PENDING.load(Ordering::Relaxed);
    if head.is_null() {
        return None;
    }
    let next = unsafe { (*head).next.load(Ordering::Relaxed) };
    CLOSE_WORK_PENDING.store(next, Ordering::Relaxed);
    unsafe { (*head).next.store(ptr::null_mut(), Ordering::Relaxed) };
    Some(unsafe { Box::from_raw(head) })
}

pub(crate) fn prepare_description_close(
    inner: &Arc<dyn FileLike>,
) -> AxResult<Option<Box<CloseWork>>> {
    if let Some(file) = inner.downcast_ref::<File>() {
        let location = file.inner().location().clone();
        let flags = file.inner().flags();
        let mask = if flags.intersects(axfs::FileFlags::WRITE | axfs::FileFlags::APPEND) {
            IN_CLOSE_WRITE
        } else {
            IN_CLOSE_NOWRITE
        };
        Ok(Some(CloseWork::try_new(location, mask)?))
    } else if let Some(directory) = inner.downcast_ref::<Directory>() {
        Ok(Some(CloseWork::try_new(
            directory.inner().clone(),
            IN_CLOSE_NOWRITE,
        )?))
    } else {
        Ok(None)
    }
}

/// Publishes already allocated close work without taking a subsystem lock or
/// allocating from FileDescription::drop.
pub(crate) fn defer_description_close(mut work: Box<CloseWork>) {
    work.actor = FanotifyEventActor::current();
    if let Some(account) = current_may_uninit()
        .and_then(|task| {
            task.try_as_thread()
                .map(|thread| thread.deferred_work_account())
        })
        .filter(|account| account.begin())
    {
        work.account = Some(account);
    }
    let work = Box::into_raw(work);
    let mut head = CLOSE_WORK_INCOMING.load(Ordering::Relaxed);
    loop {
        unsafe { (*work).next.store(head, Ordering::Relaxed) };
        match CLOSE_WORK_INCOMING.compare_exchange_weak(
            head,
            work,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => head = actual,
        }
    }
}

pub(crate) fn has_deferred_notification_work() -> bool {
    !CLOSE_WORK_INCOMING.load(Ordering::Acquire).is_null()
        || !CLOSE_WORK_PENDING.load(Ordering::Acquire).is_null()
        || FILESYSTEM_RELEASE_SIGNAL.is_pending()
        || FILESYSTEM_RELEASE_CONTINUATION.load(Ordering::Acquire)
}

/// Waits only for final-OFD work attributed to the current actor. New close
/// work from unrelated tasks cannot starve this syscall/exec/exit boundary.
pub(crate) fn wait_current_close_notifications() {
    if let Some(thread) = current_may_uninit().and_then(|task| {
        task.try_as_thread()
            .map(|thread| thread.deferred_work_account())
    }) {
        while thread.has_pending() {
            super::drain_deferred_description_cleanup();
            drain_close_notifications();
            if thread.has_pending() {
                axtask::yield_now();
            }
        }
    }
}

/// Runs a bounded amount of close-time policy work at a scheduler safe point.
pub(crate) fn drain_close_notifications() {
    let Some(_guard) = CloseWorkDrainGuard::try_enter() else {
        return;
    };
    for _ in 0..CLOSE_NOTIFICATIONS_PER_BATCH {
        let Some(work) = pop_pending_close_work() else {
            return;
        };
        work.run();
    }
}

/// Coalesces final-filesystem-release notifications until a task-context safe
/// point drains them. Its fixed capacity is one bit: additional releases are
/// deliberately coalesced, not dropped, because each watch retains a weak
/// identity that lets the drainer discover every released filesystem exactly.
struct FilesystemReleaseSignal {
    pending: AtomicBool,
}

impl FilesystemReleaseSignal {
    const fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
        }
    }

    /// Returns whether this call changed the signal from idle to pending.
    fn mark(&self) -> bool {
        !self.pending.swap(true, Ordering::AcqRel)
    }

    fn take(&self) -> bool {
        self.pending.load(Ordering::Acquire) && self.pending.swap(false, Ordering::AcqRel)
    }

    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

static FILESYSTEM_RELEASE_SIGNAL: FilesystemReleaseSignal = FilesystemReleaseSignal::new();
static FILESYSTEM_RELEASE_CONTINUATION: AtomicBool = AtomicBool::new(false);
static FILESYSTEM_RELEASE_DRAINING: AtomicBool = AtomicBool::new(false);

struct FileReleaseCursor {
    slot: usize,
    file: Weak<InotifyFile>,
    next_watch_slot: usize,
    watch_slots_remaining: usize,
    end_serial: u64,
}

struct ReleaseDrainCursor {
    active: bool,
    next_slot: usize,
    slots_remaining: usize,
    current: Option<FileReleaseCursor>,
    rescan_requested: bool,
}

impl ReleaseDrainCursor {
    const fn new() -> Self {
        Self {
            active: false,
            next_slot: 0,
            slots_remaining: 0,
            current: None,
            rescan_requested: false,
        }
    }

    fn start(&mut self, slots: usize) {
        self.active = slots != 0;
        self.next_slot = 0;
        self.slots_remaining = slots;
        self.current = None;
        self.rescan_requested = false;
    }

    fn finish(&mut self) -> bool {
        self.active = false;
        self.current = None;
        core::mem::take(&mut self.rescan_requested)
    }

    fn take_next_slot(&mut self) -> Option<usize> {
        if self.slots_remaining == 0 {
            return None;
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        self.slots_remaining -= 1;
        Some(slot)
    }
}

static FILESYSTEM_RELEASE_CURSOR: Mutex<ReleaseDrainCursor> = Mutex::new(ReleaseDrainCursor::new());

struct ReleaseDrainGuard;

impl ReleaseDrainGuard {
    fn try_enter() -> Option<Self> {
        FILESYSTEM_RELEASE_DRAINING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for ReleaseDrainGuard {
    fn drop(&mut self) {
        FILESYSTEM_RELEASE_DRAINING.store(false, Ordering::Release);
    }
}

fn register_inotify_file(file: &Arc<InotifyFile>) -> AxResult<()> {
    let weak = Arc::downgrade(file);
    let mut files = INOTIFY_FILES.lock();
    let reusable = files.iter().position(|slot| {
        slot.as_ref()
            .is_none_or(|registered| registered.strong_count() == 0)
    });
    let Some(slot) = reusable else {
        return Err(AxError::TooManyOpenFiles);
    };
    let retired = files[slot].replace(weak);
    drop(files);
    drop(retired);
    INOTIFY_SLOT_HIGH_WATER.fetch_max(slot + 1, Ordering::AcqRel);
    Ok(())
}

impl InotifyFile {
    pub fn new(non_blocking: bool) -> AxResult<Arc<Self>> {
        // Keep one allocation-free slot available for an IN_Q_OVERFLOW marker
        // even when subsequent event allocation fails under memory pressure.
        let mut queue = VecDeque::new();
        queue.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        let file = Arc::try_new(Self {
            state: Mutex::new(InotifyState {
                next_wd: 1,
                next_watch_serial: 1,
                watches: Vec::new(),
                queue,
            }),
            read_gate: BlockingMutex::new(()),
            non_blocking: AtomicBool::new(non_blocking),
            poll_rx: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        register_inotify_file(&file)?;
        Ok(file)
    }

    pub fn add_watch(&self, loc: &Location, mask: u32) -> AxResult<i32> {
        if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
            return Err(AxError::InvalidInput);
        }

        let key = WatchKey::from_location(loc)?;
        let filesystem = loc.mountpoint().filesystem_identity_weak();
        let is_dir = loc.is_dir();
        let persistent_mask = mask & (IN_ALL_EVENTS | INOTIFY_PERSISTENT_FLAGS);
        let mut state = self.state.lock();

        if let Some(watch) = state
            .watches
            .iter_mut()
            .flatten()
            .find(|watch| watch.key == key)
        {
            if mask & IN_MASK_CREATE != 0 {
                return Err(AxError::AlreadyExists);
            }
            if mask & IN_MASK_ADD != 0 {
                watch.mask |= persistent_mask;
            } else {
                watch.mask = persistent_mask;
            }
            watch.filesystem = filesystem;
            watch.is_dir = is_dir;
            return Ok(watch.wd);
        }

        let vacant_slot = state.watches.iter().position(Option::is_none);
        if vacant_slot.is_none() {
            if state.watches.len() >= MAX_WATCHES_PER_INSTANCE {
                return Err(AxError::StorageFull);
            }
            state
                .watches
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
        }

        let wd = state.next_wd;
        let serial = state.next_watch_serial;
        let next_wd = state.next_wd.checked_add(1).ok_or(AxError::OutOfRange)?;
        let next_watch_serial = state
            .next_watch_serial
            .checked_add(1)
            .ok_or(AxError::OutOfRange)?;
        state.next_wd = next_wd;
        state.next_watch_serial = next_watch_serial;
        let watch = Watch {
            wd,
            serial,
            key,
            filesystem,
            mask: persistent_mask,
            is_dir,
        };
        if let Some(slot) = vacant_slot {
            state.watches[slot] = Some(watch);
        } else {
            state.watches.push(Some(watch));
        }
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
        for watch in state.watches.iter().flatten() {
            out.push_str(&format!(
                "inotify wd:{} ino:{:x} sdev:{:x} mask:{:x}\n",
                watch.wd, watch.key.ino, watch.key.dev, watch.mask
            ));
        }
        out
    }

    fn enqueue_overflow_locked(state: &mut InotifyState) -> bool {
        if state
            .queue
            .iter()
            .any(|queued| queued.mask == IN_Q_OVERFLOW && queued.wd == -1)
        {
            return false;
        }
        // Construction reserves at least one slot. If all current capacity is
        // occupied and growth failed, sacrifice the newest ordinary event so
        // userspace still receives an honest loss marker.
        if state.queue.len() == state.queue.capacity() {
            state.queue.pop_back();
        }
        state.queue.push_back(QueuedEvent {
            wd: -1,
            mask: IN_Q_OVERFLOW,
            cookie: 0,
            name: Vec::new(),
        });
        true
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
        if event.mask == IN_Q_OVERFLOW || state.queue.len() >= MAX_QUEUED_EVENTS {
            return Self::enqueue_overflow_locked(state);
        }
        if state.queue.try_reserve(1).is_err() {
            return Self::enqueue_overflow_locked(state);
        }
        state.queue.push_back(event);
        true
    }

    fn enqueue_named_locked(
        state: &mut InotifyState,
        wd: i32,
        mask: u32,
        cookie: u32,
        name: &[u8],
    ) -> bool {
        let mut owned_name = Vec::new();
        if owned_name.try_reserve_exact(name.len()).is_err() {
            return Self::enqueue_overflow_locked(state);
        }
        owned_name.extend_from_slice(name);
        Self::enqueue_locked(
            state,
            QueuedEvent {
                wd,
                mask,
                cookie,
                name: owned_name,
            },
        )
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

    /// Drains records which are ready now. Linux removes an inotify event
    /// before copying it to userspace and destroys it even when that copy
    /// faults; in particular, EFAULT is not converted to a successful short
    /// read after earlier records were copied.
    fn read_ready(&self, dst: &mut IoDst) -> AxResult<usize> {
        let mut written = 0usize;

        loop {
            let Some(len) = self
                .state
                .lock()
                .queue
                .front()
                .map(QueuedEvent::encoded_len)
            else {
                break;
            };
            if written == 0 && dst.remaining_mut() < len {
                return Err(AxError::InvalidInput);
            }
            if dst.remaining_mut() < len {
                break;
            }
            let Some(event) = self.state.lock().queue.pop_front() else {
                continue;
            };
            if len > MAX_INOTIFY_EVENT_SIZE {
                // The VFS name limit makes this unreachable for admitted
                // events. Consume a corrupt record instead of permanently
                // wedging the queue on it.
                return if written == 0 {
                    Err(AxError::InvalidInput)
                } else {
                    Ok(written)
                };
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
            let mut encoded = [0_u8; MAX_INOTIFY_EVENT_SIZE];
            let header_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&header as *const inotify_event).cast::<u8>(),
                    size_of::<inotify_event>(),
                )
            };
            encoded[..size_of::<inotify_event>()].copy_from_slice(header_bytes);
            if name_len > 0 {
                let name_start = size_of::<inotify_event>();
                encoded[name_start..name_start + event.name.len()].copy_from_slice(&event.name);
                // The stack buffer is already zeroed, including the NUL
                // terminator and ABI padding after the name.
            }
            match dst.write(&encoded[..len]) {
                Ok(copied) if copied == len => written += len,
                Ok(_) => return Err(AxError::BadAddress),
                Err(error) => return Err(error),
            }
        }

        if written == 0 {
            Err(AxError::WouldBlock)
        } else {
            Ok(written)
        }
    }
}

fn remove_watch_locked(state: &mut InotifyState, wd: i32) -> AxResult<()> {
    let watch = state
        .watches
        .iter_mut()
        .find(|watch| watch.as_ref().is_some_and(|watch| watch.wd == wd))
        .ok_or(AxError::InvalidInput)?;
    *watch = None;
    Ok(())
}

impl FileLike for InotifyFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(super::anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        axtask::run_deferred_work();
        let _reader = self.read_gate.lock();
        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            self.read_ready(dst)
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
        axtask::run_deferred_work();
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.has_events());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
            // Register first so a release racing with poll cannot be missed.
            axtask::run_deferred_work();
        }
    }
}

/// Installs the VFS final-release callback.
pub(crate) fn init_filesystem_release_notifications() {
    axfs_ng_vfs::set_filesystem_release_hook(defer_filesystem_release);
}

fn defer_filesystem_release(_dev: u64) {
    // Do not wake a task here: the current no_std wait implementations can
    // allocate under contention. Task-context syscall and inotify safe points
    // observe this coalesced signal instead.
    FILESYSTEM_RELEASE_SIGNAL.mark();
}

/// Processes final filesystem releases from an explicit task-context safe
/// point. Each invocation has hard file/watch visit budgets. A release racing
/// with an active scan requests a complete follow-up pass, while unfinished
/// work retains a separate continuation bit.
pub(crate) fn drain_filesystem_release_notifications() {
    if !FILESYSTEM_RELEASE_SIGNAL.is_pending()
        && !FILESYSTEM_RELEASE_CONTINUATION.load(Ordering::Acquire)
    {
        return;
    }
    let Some(_guard) = ReleaseDrainGuard::try_enter() else {
        FILESYSTEM_RELEASE_CONTINUATION.store(true, Ordering::Release);
        return;
    };

    let release_arrived = FILESYSTEM_RELEASE_SIGNAL.take();
    let continuation = FILESYSTEM_RELEASE_CONTINUATION.swap(false, Ordering::AcqRel);
    if !release_arrived && !continuation {
        return;
    }

    let mut cursor = FILESYSTEM_RELEASE_CURSOR.lock();
    if cursor.active {
        cursor.rescan_requested |= release_arrived;
    } else {
        cursor.start(INOTIFY_SLOT_HIGH_WATER.load(Ordering::Acquire));
    }
    if !cursor.active {
        return;
    }

    if notify_released_filesystems_batch(&mut cursor) {
        let rescan = cursor.finish();
        // A real release racing with this batch remains in RELEASE_SIGNAL.
        // `rescan` covers releases consumed by an earlier continuation.
        if rescan {
            FILESYSTEM_RELEASE_CONTINUATION.store(true, Ordering::Release);
        }
    } else {
        FILESYSTEM_RELEASE_CONTINUATION.store(true, Ordering::Release);
    }
}

fn registered_inotify_file(slot: usize) -> Option<Weak<InotifyFile>> {
    INOTIFY_FILES
        .lock()
        .get(slot)
        .and_then(|file| file.as_ref().cloned())
}

fn clear_dead_inotify_file(slot: usize, expected: &Weak<InotifyFile>) {
    let mut files = INOTIFY_FILES.lock();
    let Some(registered) = files.get_mut(slot).and_then(Option::as_mut) else {
        return;
    };
    let retired = (Weak::ptr_eq(registered, expected) && registered.strong_count() == 0)
        .then(|| files[slot].take())
        .flatten();
    drop(files);
    drop(retired);
}

fn select_next_release_file(cursor: &mut ReleaseDrainCursor) {
    let Some(slot) = cursor.take_next_slot() else {
        return;
    };

    let Some(weak) = registered_inotify_file(slot) else {
        return;
    };
    let Some(file) = weak.upgrade() else {
        clear_dead_inotify_file(slot, &weak);
        return;
    };
    let state = file.state.lock();
    let end_serial = state.next_watch_serial;
    let watch_slots_remaining = state.watches.len();
    drop(state);
    cursor.current = Some(FileReleaseCursor {
        slot,
        file: weak,
        next_watch_slot: 0,
        watch_slots_remaining,
        end_serial,
    });
}

fn scan_current_release_file(cursor: &mut ReleaseDrainCursor, budget: usize) -> usize {
    let Some(file_cursor) = cursor.current.as_ref() else {
        return 0;
    };
    let slot = file_cursor.slot;
    let weak = file_cursor.file.clone();
    let start_slot = file_cursor.next_watch_slot;
    let slots_remaining = file_cursor.watch_slots_remaining;
    let end_serial = file_cursor.end_serial;
    let Some(file) = weak.upgrade() else {
        clear_dead_inotify_file(slot, &weak);
        cursor.current = None;
        return 0;
    };

    let mut state = file.state.lock();
    let mut visited = 0;
    let mut wake = false;
    let visits = budget.min(slots_remaining);
    while visited < visits {
        let slot = start_slot + visited;
        visited += 1;
        let Some(watch) = state.watches.get_mut(slot).and_then(Option::as_mut) else {
            continue;
        };
        if watch.serial >= end_serial || !watch.filesystem_released() {
            continue;
        }
        let wd = watch.wd;
        state.watches[slot] = None;
        for mask in [IN_UNMOUNT, IN_IGNORED] {
            wake |= InotifyFile::enqueue_locked(
                &mut state,
                QueuedEvent {
                    wd,
                    mask,
                    cookie: 0,
                    name: Vec::new(),
                },
            );
        }
    }
    let complete = visited == slots_remaining;
    drop(state);
    if wake {
        file.poll_rx.wake();
    }
    if complete {
        cursor.current = None;
    } else if let Some(current) = cursor.current.as_mut() {
        current.next_watch_slot += visited;
        current.watch_slots_remaining -= visited;
    }
    visited
}

fn notify_released_filesystems_batch(cursor: &mut ReleaseDrainCursor) -> bool {
    let mut files_visited = 0;
    let mut watches_visited = 0;
    loop {
        if cursor.current.is_none() {
            if cursor.slots_remaining == 0 {
                return true;
            }
            if files_visited == RELEASE_FILES_PER_BATCH {
                return false;
            }
            files_visited += 1;
            select_next_release_file(cursor);
            if cursor.current.is_none() {
                continue;
            }
        }

        if watches_visited == RELEASE_WATCHES_PER_BATCH {
            return false;
        }
        watches_visited +=
            scan_current_release_file(cursor, RELEASE_WATCHES_PER_BATCH - watches_visited);
        if cursor.current.is_some() {
            return false;
        }
    }
}

fn each_inotify_file(mut f: impl FnMut(&Arc<InotifyFile>)) {
    let slots = INOTIFY_SLOT_HIGH_WATER.load(Ordering::Acquire);
    for slot in 0..slots {
        let Some(weak) = registered_inotify_file(slot) else {
            continue;
        };
        if let Some(file) = weak.upgrade() {
            f(&file);
        } else {
            clear_dead_inotify_file(slot, &weak);
        }
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
            let Some(watch) = state.watches[idx].clone() else {
                idx += 1;
                continue;
            };
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
            wake |= InotifyFile::enqueue_named_locked(&mut state, watch.wd, mask, cookie, name);
            let remove_watch =
                watch.mask & IN_ONESHOT != 0 || mask & (IN_DELETE_SELF | IN_UNMOUNT) != 0;
            if remove_watch {
                state.watches[idx] = None;
                wake |= InotifyFile::enqueue_locked(
                    &mut state,
                    QueuedEvent {
                        wd: watch.wd,
                        mask: IN_IGNORED,
                        cookie: 0,
                        name: Vec::new(),
                    },
                );
            }
            idx += 1;
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

pub(crate) fn notify_exact(loc: &Location, mask: u32) -> AxResult<()> {
    notify_exact_with_actor(loc, mask, FanotifyEventActor::current())
}

pub(crate) fn notify_exact_with_actor(
    loc: &Location,
    mask: u32,
    actor: FanotifyEventActor,
) -> AxResult<()> {
    notify_exact_prepared(
        loc,
        WatchKey::from_location(loc)?,
        loc.is_dir(),
        mask,
        actor,
    )
}

fn notify_exact_prepared(
    loc: &Location,
    key: WatchKey,
    is_dir: bool,
    mut mask: u32,
    actor: FanotifyEventActor,
) -> AxResult<()> {
    mask = exact_dir_mask(mask, is_dir);
    emit_to_matching_watches(key, mask, 0, &[], false, false);
    crate::file::fanotify::notify_with_keys_and_actor(
        loc,
        key,
        loc.mountpoint().mount_id(),
        key,
        inotify_to_fanotify(mask),
        is_dir,
        false,
        actor,
    );
    crate::file::dnotify::emit_inotify(key, mask)?;
    Ok(())
}

pub(crate) fn notify_parent(loc: &Location, mask: u32) -> AxResult<()> {
    notify_parent_with_actor(loc, mask, FanotifyEventActor::current())
}

pub(crate) fn notify_parent_with_actor(
    loc: &Location,
    mask: u32,
    actor: FanotifyEventActor,
) -> AxResult<()> {
    let Some(parent) = loc.parent() else {
        return Ok(());
    };
    let parent = CloseParent {
        key: WatchKey::from_location(&parent)?,
    };
    notify_parent_prepared(
        loc,
        WatchKey::from_location(loc)?,
        &parent,
        loc.is_dir(),
        mask,
        actor,
    )
}

fn notify_parent_prepared(
    loc: &Location,
    event_key: WatchKey,
    parent: &CloseParent,
    is_dir: bool,
    mut mask: u32,
    actor: FanotifyEventActor,
) -> AxResult<()> {
    if is_dir {
        mask |= IN_ISDIR;
    }
    let unlinked_child = match loc.metadata() {
        Ok(meta) => meta.nlink == 0,
        Err(error) => {
            warn!("deferred close link-state lookup failed: {error}");
            true
        }
    };
    emit_to_matching_watches(
        parent.key,
        mask,
        0,
        loc.name().as_bytes(),
        true,
        unlinked_child,
    );
    crate::file::fanotify::notify_with_keys_and_actor(
        loc,
        event_key,
        loc.mountpoint().mount_id(),
        parent.key,
        inotify_to_fanotify(mask),
        is_dir,
        true,
        actor,
    );
    crate::file::dnotify::emit_inotify(parent.key, mask)?;
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
    let key = WatchKey::from_location(parent)?;
    emit_to_matching_watches(key, mask, cookie, child_name.as_bytes(), true, false);
    crate::file::fanotify::notify(
        child.unwrap_or(parent),
        parent,
        inotify_to_fanotify(mask),
        is_dir,
        true,
    );
    crate::file::dnotify::emit_inotify(key, mask)?;
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
    let new_key = WatchKey::from_location(new_parent)?;
    crate::file::dnotify::emit_rename(old_key, new_key)
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

#[cfg(test)]
mod tests {
    use axerrno::{AxError, AxResult};
    use axio::{IoBufMut, Write};

    use super::{
        FilesystemReleaseSignal, InotifyFile, QueuedEvent, RELEASE_FILES_PER_BATCH,
        ReleaseDrainCursor,
    };

    struct FaultAfterWrites {
        remaining: usize,
        successful_writes: usize,
    }

    impl Write for FaultAfterWrites {
        fn write(&mut self, buf: &[u8]) -> AxResult<usize> {
            if self.successful_writes == 0 {
                return Err(AxError::BadAddress);
            }
            self.successful_writes -= 1;
            self.remaining -= buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> AxResult<()> {
            Ok(())
        }
    }

    impl IoBufMut for FaultAfterWrites {
        fn remaining_mut(&self) -> usize {
            self.remaining
        }
    }

    #[test]
    fn filesystem_release_signal_coalesces_until_drained() {
        let signal = FilesystemReleaseSignal::new();

        assert!(signal.mark());
        assert!(!signal.mark());
        assert!(signal.take());
        assert!(!signal.take());
        assert!(signal.mark());
        assert!(signal.take());
    }

    #[test]
    fn release_cursor_preserves_stable_slot_progress_across_batches() {
        let mut cursor = ReleaseDrainCursor::new();
        cursor.start(RELEASE_FILES_PER_BATCH + 3);

        for expected in 0..RELEASE_FILES_PER_BATCH {
            assert_eq!(cursor.take_next_slot(), Some(expected));
        }
        assert_eq!(cursor.slots_remaining, 3);
        assert_eq!(cursor.take_next_slot(), Some(RELEASE_FILES_PER_BATCH));

        cursor.rescan_requested = true;
        assert!(cursor.finish());
        assert!(!cursor.active);
        assert!(cursor.current.is_none());
    }

    #[test]
    fn read_fault_consumes_event_and_overrides_prior_short_success() {
        let file = InotifyFile::new(true).unwrap();
        let event = QueuedEvent {
            wd: 1,
            mask: linux_raw_sys::general::IN_ACCESS,
            cookie: 0,
            name: alloc::vec::Vec::new(),
        };
        let event_len = event.encoded_len();
        {
            let mut state = file.state.lock();
            state.queue.try_reserve(2).unwrap();
            state.queue.push_back(event.clone());
            state.queue.push_back(event);
        }
        let mut dst = FaultAfterWrites {
            remaining: event_len * 2,
            successful_writes: 1,
        };

        assert_eq!(file.read_ready(&mut dst), Err(AxError::BadAddress));
        assert!(file.state.lock().queue.is_empty());
    }
}
