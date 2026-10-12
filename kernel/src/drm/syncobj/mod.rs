//! DRM syncobjs: a port of Linux `drivers/gpu/drm/drm_syncobj.c`.
//!
//! A syncobj holds one fence, either a binary payload or the newest node of a
//! dma-fence chain (a timeline, see `chain`). Waiters register on the object
//! and are woken by replace/add operations, exactly as the Linux `cb_list` and
//! `ev_fd_list` do. Lock order is always syncobj lock, then fence internals;
//! no callback re-enters a syncobj.
// Machine-translated Linux C: preserve upstream symbol and type naming conventions
#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
// Machine-translated Linux C: retain original upstream documentation and tab indentation
#![allow(clippy::tabs_in_doc_comments)]
// Machine-translated Linux C: preserve upstream control flow structure for parity audits
#![allow(clippy::collapsible_if)]

mod chain;
mod sync_file;
#[cfg(test)]
mod upstream;

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axhal::time::TimeValue;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axtask::WaitQueue;

pub(crate) use self::sync_file::{export, import};
pub use self::sync_file::SyncFile;
use crate::{
    drm::{fence::Fence, uapi},
    file::{FileHandle, FileLike, IoctlContext, Kstat, event::EventFd},
};

pub type SyncobjHandle = u32;

const WAIT_ALL: u32 = uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL;
const WAIT_FOR_SUBMIT: u32 = uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT;
const WAIT_AVAILABLE: u32 = uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE;

/// `DRM_SYNCOBJ_WAIT_FOR_SUBMIT_TIMEOUT` in `drm_syncobj_find_fence()`.
const WAIT_FOR_SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);

/// A waiting task's wakeup. Callbacks set `pending` and notify the queue, so a
/// waiter that rescans after `arm()` cannot miss a change made before it sleeps.
struct Wakeup {
    pending: AtomicBool,
    queue: WaitQueue,
}

impl Wakeup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicBool::new(false),
            queue: WaitQueue::new(),
        })
    }
    fn arm(&self) {
        self.pending.store(false, Ordering::Release);
    }
    fn wake(&self) {
        self.pending.store(true, Ordering::Release);
        self.queue.notify_all(false);
    }
    /// schedule_timeout(): sleep until woken or `remaining` elapses.
    fn sleep(&self, remaining: Duration) -> AxResult<()> {
        self.queue
            .wait_timeout_until_interruptible(remaining, || self.pending.load(Ordering::Acquire))?;
        Ok(())
    }
}

/// `struct syncobj_wait_entry`: one waiter's view of one (syncobj, point).
struct WaitEntry {
    point: u64,
    wakeup: Arc<Wakeup>,
    fence: spin::Mutex<Option<Arc<Fence>>>,
}

impl WaitEntry {
    fn new(point: u64, wakeup: Arc<Wakeup>) -> Arc<Self> {
        Arc::new(Self {
            point,
            wakeup,
            fence: spin::Mutex::new(None),
        })
    }
    fn fence(&self) -> Option<Arc<Fence>> {
        self.fence.lock().clone()
    }
    fn has_fence(&self) -> bool {
        self.fence.lock().is_some()
    }
    fn set_fence(&self, fence: Arc<Fence>) {
        *self.fence.lock() = Some(fence);
    }
}

/// `struct syncobj_eventfd_entry`: signal `eventfd` once `point` is ready.
struct EventfdEntry {
    point: u64,
    flags: u32,
    // Like Linux's eventfd_ctx reference, this keeps the eventfd open until
    // the entry is retired.
    eventfd: FileHandle<EventFd>,
}

struct Inner {
    fence: Option<Arc<Fence>>,
    cb_list: Vec<Arc<WaitEntry>>,
    ev_fd_list: Vec<Arc<EventfdEntry>>,
}

/// `struct drm_syncobj`.
pub struct Syncobj {
    inner: spin::Mutex<Inner>,
    /// Opaque syncobj descriptors rescan the payload after each change.
    poll_waiters: PollSet,
}

fn opt_ptr_eq(a: &Option<Arc<Fence>>, b: &Option<Arc<Fence>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

fn stub() -> Arc<Fence> {
    Fence::new(true)
}

/// `drm_syncobj_fence_get()` then `dma_fence_chain_find_seqno()` as used by
/// the wait paths: `None` when the object has no fence or the point is not yet
/// submitted; otherwise the fence to wait on (a signaled stub if the point has
/// completed and the chain has been walked past it).
fn lookup_point(head: Option<Arc<Fence>>, point: u64) -> Option<Arc<Fence>> {
    let head = head?;
    chain::lookup(Some(head), point)
        .ok()
        .map(|fence| fence.unwrap_or_else(stub))
}

/// `drm_syncobj_wait_syncobj_func()`: satisfy a waiter if its point exists.
fn wait_syncobj_func(head: &Option<Arc<Fence>>, entry: &WaitEntry) -> bool {
    match lookup_point(head.clone(), entry.point) {
        Some(fence) => {
            entry.set_fence(fence);
            entry.wakeup.wake();
            true
        }
        None => false,
    }
}

/// `syncobj_eventfd_entry_func()`: returns true once the entry has been
/// retired (its point was found and it no longer belongs on the list).
fn eventfd_entry_func(head: &Option<Arc<Fence>>, entry: &EventfdEntry) -> bool {
    let Some(fence) = lookup_point(head.clone(), entry.point) else {
        return false;
    };
    if entry.flags & WAIT_AVAILABLE != 0 {
        let _ = entry.eventfd.signal(1);
        return true;
    }
    let immediate = entry.eventfd.clone();
    let deferred = entry.eventfd.clone();
    if fence
        .add_callback(alloc::boxed::Box::new(move || {
            let _ = deferred.signal(1);
        }))
        .is_none()
    {
        // Already signaled: Linux -ENOENT path signals at once.
        let _ = immediate.signal(1);
    }
    true
}

/// Run every waiter and eventfd registered on `inner` against the current
/// payload. Called with the syncobj lock held after a payload change.
fn notify(inner: &mut Inner) {
    let Inner {
        fence,
        cb_list,
        ev_fd_list,
    } = inner;
    cb_list.retain(|entry| !wait_syncobj_func(fence, entry));
    ev_fd_list.retain(|entry| !eventfd_entry_func(fence, entry));
}

impl Syncobj {
    fn empty() -> Self {
        Self {
            inner: spin::Mutex::new(Inner {
                fence: None,
                cb_list: Vec::new(),
                ev_fd_list: Vec::new(),
            }),
            poll_waiters: PollSet::new(),
        }
    }

    /// `drm_syncobj_create()`: an optional signaled stub, then an optional
    /// fence replacing it, in the same order as Linux.
    pub(crate) fn create(signaled: bool, fence: Option<Arc<Fence>>) -> AxResult<Arc<Self>> {
        let object = Arc::try_new(Self::empty()).map_err(|_| AxError::NoMemory)?;
        if signaled {
            object.signal();
        }
        if let Some(fence) = fence {
            object.import_fence(fence);
        }
        Ok(object)
    }

    /// Convenience constructor for the DRM file table.
    pub fn new(signaled: bool) -> Arc<Self> {
        let object = Arc::new(Self::empty());
        if signaled {
            object.signal();
        }
        object
    }

    /// `drm_syncobj_replace_fence()`: install `fence` (or NULL) and return the
    /// previous payload, which the caller drops outside the lock.
    fn replace_fence(&self, fence: Option<Arc<Fence>>) -> Option<Arc<Fence>> {
        let (old, changed) = {
            let mut inner = self.inner.lock();
            let changed = !opt_ptr_eq(&inner.fence, &fence);
            let old = core::mem::replace(&mut inner.fence, fence);
            if changed {
                notify(&mut inner);
            }
            (old, changed)
        };
        if changed {
            self.poll_waiters.wake();
        }
        old
    }

    /// `drm_syncobj_replace_fence(syncobj, NULL)`.
    pub fn reset(&self) {
        drop(self.replace_fence(None));
    }

    /// `drm_syncobj_assign_null_handle()`: a fresh signaled stub payload.
    pub fn signal(&self) {
        drop(self.replace_fence(Some(stub())));
    }

    /// `drm_syncobj_replace_fence(syncobj, fence)`.
    pub fn import_fence(&self, fence: Arc<Fence>) {
        drop(self.replace_fence(Some(fence)));
    }

    /// `drm_syncobj_fence_get()`: the current payload, if any.
    pub(crate) fn latest_fence(&self) -> Option<Arc<Fence>> {
        self.inner.lock().fence.clone()
    }

    /// The current payload. Errors when nothing has been installed yet.
    pub fn fence(&self) -> AxResult<Arc<Fence>> {
        self.latest_fence().ok_or(AxError::NotFound)
    }

    /// The fence `drm_syncobj_find_fence()` would return without waiting.
    /// Missing points report `NotFound`.
    pub fn fence_at(&self, point: u64) -> AxResult<Arc<Fence>> {
        let head = self.latest_fence().ok_or(AxError::NotFound)?;
        let mut slot = Some(head);
        chain::find_seqno(&mut slot, point).map_err(|_| AxError::NotFound)?;
        Ok(slot.unwrap_or_else(stub))
    }

    /// `drm_syncobj_add_point()` with the chain node allocated internally.
    /// Returns the previous head, which the caller garbage-collects.
    fn add_point_locked(inner: &mut Inner, point: u64, fence: Arc<Fence>) -> AxResult<Option<Arc<Fence>>> {
        let prev = inner.fence.clone();
        let node = chain::init(prev.clone(), fence, point)?;
        inner.fence = Some(node);
        notify(inner);
        Ok(prev)
    }

    /// `drm_syncobj_add_point()`: append a timeline point.
    pub(crate) fn add_point(&self, point: u64, fence: Arc<Fence>) -> AxResult<()> {
        let prev = {
            let mut inner = self.inner.lock();
            Self::add_point_locked(&mut inner, point, fence)?
        };
        self.poll_waiters.wake();
        chain::collect(prev);
        Ok(())
    }

    /// Exec-output update: a binary payload for point 0, otherwise a timeline
    /// point. A reset is applied under the same lock, so no observer sees the
    /// empty intermediate state.
    pub fn submit_point(&self, point: u64, fence: Arc<Fence>) -> AxResult<()> {
        if point == 0 {
            self.import_fence(fence);
            Ok(())
        } else {
            self.add_point(point, fence)
        }
    }

    /// TIMELINE_SIGNAL: `drm_syncobj_add_point()` with a signaled stub.
    pub fn signal_point(&self, point: u64) -> AxResult<()> {
        self.add_point(point, stub())
    }

    /// Apply one exec output: an optional RESET, then the completion fence at
    /// `point` (binary when zero), as one syncobj-local transition.
    pub(crate) fn apply_exec_output(&self, reset: bool, point: u64, fence: Arc<Fence>) -> AxResult<()> {
        let mut retired = Vec::new();
        let prev = {
            let mut inner = self.inner.lock();
            if reset {
                retired.push(self.replace_locked(&mut inner, None));
            }
            if point == 0 {
                retired.push(self.replace_locked(&mut inner, Some(fence)));
                None
            } else {
                Self::add_point_locked(&mut inner, point, fence)?
            }
        };
        self.poll_waiters.wake();
        drop(retired);
        chain::collect(prev);
        Ok(())
    }

    fn replace_locked(&self, inner: &mut Inner, fence: Option<Arc<Fence>>) -> Option<Arc<Fence>> {
        let old = core::mem::replace(&mut inner.fence, fence);
        notify(inner);
        old
    }

    /// `drm_syncobj_query_ioctl()` for one object: the highest completed
    /// point, or the last submitted point with `last_submitted`.
    pub fn query_point(&self, last_submitted: bool) -> u64 {
        let Some(head) = self.latest_fence() else {
            return 0;
        };
        let Some(head_link) = head.chain() else {
            return 0;
        };
        if last_submitted {
            return head_link.seqno;
        }
        let context = head_link.context;
        let mut last_signaled = head.clone();
        let mut current = Some(head.clone());
        while let Some(node) = current {
            let same_context = node.chain().is_some_and(|link| link.context == context);
            if !same_context {
                break;
            }
            last_signaled = node.clone();
            current = chain::walk(Some(node));
        }
        match last_signaled.chain() {
            Some(link) if last_signaled.is_signaled() => link.seqno,
            Some(link) => link.prev_seqno,
            None => 0,
        }
    }

    fn add_wait(&self, entry: &Arc<WaitEntry>) {
        if entry.has_fence() {
            return;
        }
        let mut inner = self.inner.lock();
        match lookup_point(inner.fence.clone(), entry.point) {
            Some(fence) => entry.set_fence(fence),
            None => inner.cb_list.push(entry.clone()),
        }
    }

    fn remove_wait(&self, entry: &Arc<WaitEntry>) {
        self.inner
            .lock()
            .cb_list
            .retain(|old| !Arc::ptr_eq(old, entry));
    }

    fn add_eventfd(&self, entry: Arc<EventfdEntry>) {
        let mut inner = self.inner.lock();
        inner.ev_fd_list.push(entry.clone());
        let head = inner.fence.clone();
        if eventfd_entry_func(&head, &entry) {
            inner
                .ev_fd_list
                .retain(|old| !Arc::ptr_eq(old, &entry));
        }
    }

    /// `drm_syncobj_eventfd_ioctl()` after its argument checks: signal
    /// `eventfd` once `point` is ready (on submission with WAIT_AVAILABLE).
    pub(crate) fn eventfd(&self, point: u64, flags: u32, eventfd: FileHandle<EventFd>) {
        self.add_eventfd(Arc::new(EventfdEntry {
            point,
            flags,
            eventfd,
        }));
    }
}

/// `drm_syncobj_find_fence()`: the fence for (object, point), waiting for the
/// point to be submitted when `WAIT_FOR_SUBMIT` is set.
pub(crate) fn find_fence(object: &Arc<Syncobj>, point: u64, flags: u32) -> AxResult<Arc<Fence>> {
    if flags & !WAIT_FOR_SUBMIT != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(head) = object.latest_fence() {
        let mut slot = Some(head);
        if chain::find_seqno(&mut slot, point).is_ok() {
            return Ok(slot.unwrap_or_else(stub));
        }
    }
    if flags & WAIT_FOR_SUBMIT == 0 {
        return Err(AxError::InvalidInput);
    }
    let wakeup = Wakeup::new();
    let entry = WaitEntry::new(point, wakeup.clone());
    let expiry = axhal::time::monotonic_time() + WAIT_FOR_SUBMIT_TIMEOUT;
    object.add_wait(&entry);
    let result = loop {
        wakeup.arm();
        if let Some(fence) = entry.fence() {
            break Ok(fence);
        }
        let remaining = remaining_until(expiry);
        if remaining.is_zero() {
            break Err(AxError::TimedOut);
        }
        if let Err(error) = wakeup.sleep(remaining) {
            break Err(error);
        }
    };
    object.remove_wait(&entry);
    result
}

/// Time left until an absolute monotonic `expiry`; zero once it has passed.
fn remaining_until(expiry: TimeValue) -> Duration {
    expiry
        .checked_sub(axhal::time::monotonic_time())
        .unwrap_or(Duration::ZERO)
}

/// `drm_timeout_abs_to_jiffies()`: an absolute `timeout_nsec` becomes a
/// monotonic expiry. Zero, negative and past values poll once.
fn absolute_expiry(timeout_nsec: i64) -> TimeValue {
    if timeout_nsec <= 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(timeout_nsec as u64)
    }
}

/// `drm_syncobj_array_wait_timeout()` plus its `drm_syncobj_array_wait()`
/// caller: wait for an array of (object, point) pairs. Returns the index that
/// satisfied an any-wait, or `u32::MAX` if none was signaled at entry, which
/// is the value Linux reports in `first_signaled`.
pub(crate) fn array_wait(
    objects: &[Arc<Syncobj>],
    points: &[u64],
    flags: u32,
    timeout_nsec: i64,
    deadline: Option<TimeValue>,
) -> AxResult<u32> {
    let count = objects.len();
    if count == 0 || points.len() != count {
        return Err(AxError::InvalidInput);
    }
    let expiry = absolute_expiry(timeout_nsec);
    let wakeup = Wakeup::new();
    let mut entries = Vec::new();
    entries.try_reserve_exact(count).map_err(|_| AxError::NoMemory)?;
    for &point in points {
        entries.push(WaitEntry::new(point, wakeup.clone()));
    }
    let mut callbacks: Vec<Option<u64>> = alloc::vec![None; count];
    let result = array_wait_entries(objects, &entries, &mut callbacks, &wakeup, flags, expiry, deadline);
    // cleanup_entries: unlink waiters and drop callbacks that did not run.
    for (index, object) in objects.iter().enumerate() {
        object.remove_wait(&entries[index]);
        if let (Some(id), Some(fence)) = (callbacks[index], entries[index].fence()) {
            fence.remove_callback(id);
        }
    }
    result
}

fn array_wait_entries(
    objects: &[Arc<Syncobj>],
    entries: &[Arc<WaitEntry>],
    callbacks: &mut [Option<u64>],
    wakeup: &Arc<Wakeup>,
    flags: u32,
    expiry: TimeValue,
    deadline: Option<TimeValue>,
) -> AxResult<u32> {
    let count = objects.len();
    let relaxed = flags & (WAIT_FOR_SUBMIT | WAIT_AVAILABLE) != 0;
    let mut first = u32::MAX;
    let mut signaled = 0usize;
    for (index, object) in objects.iter().enumerate() {
        let Some(fence) = lookup_point(object.latest_fence(), entries[index].point) else {
            if relaxed {
                continue;
            }
            return Err(AxError::InvalidInput);
        };
        let counted = flags & WAIT_AVAILABLE != 0 || fence.is_signaled();
        entries[index].set_fence(fence);
        if counted {
            if signaled == 0 {
                first = index as u32;
            }
            signaled += 1;
        }
    }
    if signaled == count || (signaled > 0 && flags & WAIT_ALL == 0) {
        return Ok(first);
    }
    if relaxed {
        for (index, object) in objects.iter().enumerate() {
            object.add_wait(&entries[index]);
        }
    }
    if let Some(deadline) = deadline {
        for entry in entries {
            if let Some(fence) = entry.fence() {
                fence.set_deadline(deadline);
            }
        }
    }
    loop {
        wakeup.arm();
        let mut done_count = 0usize;
        for index in 0..count {
            let Some(fence) = entries[index].fence() else {
                continue;
            };
            let done = if flags & WAIT_AVAILABLE != 0 || fence.is_signaled() {
                true
            } else if callbacks[index].is_none() {
                let wake = wakeup.clone();
                match fence.add_callback(alloc::boxed::Box::new(move || wake.wake())) {
                    Some(id) => {
                        callbacks[index] = Some(id);
                        false
                    }
                    None => true,
                }
            } else {
                false
            };
            if done {
                if flags & WAIT_ALL != 0 {
                    done_count += 1;
                } else {
                    return Ok(index as u32);
                }
            }
        }
        if done_count == count {
            return Ok(first);
        }
        let remaining = remaining_until(expiry);
        if remaining.is_zero() {
            return Err(AxError::TimedOut);
        }
        wakeup.sleep(remaining)?;
    }
}

/// `drm_syncobj_transfer_ioctl()` after lookup: move the payload of
/// `source` at `src_point` into `destination`, as a timeline point when
/// `dst_point` is nonzero and as the binary payload otherwise.
pub(crate) fn transfer(
    source: &Arc<Syncobj>,
    destination: &Arc<Syncobj>,
    src_point: u64,
    dst_point: u64,
    flags: u32,
) -> AxResult<()> {
    let fence = find_fence(source, src_point, flags)?;
    if dst_point != 0 {
        // dma_fence_unwrap_merge(): the timeline point carries only the
        // pending contained fences of the source.
        destination.add_point(dst_point, chain::unwrap_merge(fence)?)
    } else {
        destination.import_fence(fence);
        Ok(())
    }
}

/// `drm_syncobj_export_sync_file()`: a sync_file for (object, point).
/// The descriptor is reserved first, so a failed lookup leaves no fd behind.
pub(crate) fn export_sync_file(
    object: &Arc<Syncobj>,
    point: u64,
    context: &IoctlContext,
) -> AxResult<i32> {
    let slot = sync_file::reserve_fd(context, true)?;
    let fence = find_fence(object, point, 0)?;
    Ok(sync_file::publish(fence, slot)?.commit())
}

/// `drm_syncobj_get_fd()`: an O_CLOEXEC descriptor that shares the object.
pub(crate) fn export_syncobj(object: Arc<Syncobj>, context: &IoctlContext) -> AxResult<i32> {
    context.add_file_like(SyncobjFile::new(object), true)
}

/// Resolve a descriptor created by `export_syncobj()`.
pub(crate) fn import_syncobj(context: &IoctlContext, fd: i32) -> AxResult<Arc<Syncobj>> {
    context
        .get_file_like(fd)?
        .downcast::<SyncobjFile>()
        .map(|file| file.object())
}

/// An opaque syncobj descriptor shares the object itself, unlike a sync_file
/// which snapshots one fence.
struct SyncobjFile {
    object: Arc<Syncobj>,
}

impl SyncobjFile {
    fn new(object: Arc<Syncobj>) -> Arc<Self> {
        Arc::new(Self { object })
    }
    fn object(&self) -> Arc<Syncobj> {
        self.object.clone()
    }
}

impl FileLike for SyncobjFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[syncobj_file]",
        )))
    }
    fn set_nonblocking(&self, _: bool) -> AxResult<()> {
        Ok(())
    }
}

impl Pollable for SyncobjFile {
    fn poll(&self) -> IoEvents {
        self.object
            .latest_fence()
            .map_or(IoEvents::empty(), |fence| fence.poll_events())
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let interested = events.intersects(IoEvents::READABLE | IoEvents::ERROR);
        let mut prepared = axpoll::PreparedPollRegistration::try_new(interested as usize)?;
        if interested {
            prepared.arm(&self.object.poll_waiters, context.waker())?;
        }
        prepared.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_all_points(objects: &[Arc<Syncobj>], points: &[u64]) -> AxResult<u32> {
        array_wait(objects, points, 0, 0, None)
    }

    #[test]
    fn binary_reset_and_signal_match_linux_payload_transitions() {
        let object = Syncobj::new(false);
        assert_eq!(object.fence().err(), Some(AxError::NotFound));
        object.signal();
        assert!(object.fence().unwrap().is_signaled());
        object.reset();
        assert!(object.fence().is_err());
        let gpu = Fence::new(false);
        object.import_fence(gpu.clone());
        object.reset();
        assert!(!gpu.is_signaled());
    }

    #[test]
    fn binary_wait_any_reports_first_signaled_and_all_needs_every_member() {
        let signaled = Syncobj::new(true);
        let pending = Syncobj::new(false);
        pending.import_fence(Fence::new(false));
        assert_eq!(
            wait_all_points(&[pending.clone(), signaled.clone()], &[0, 0]),
            Ok(1)
        );
        assert_eq!(
            array_wait(&[pending.clone(), signaled.clone()], &[0, 0], WAIT_ALL, 0, None),
            Err(AxError::TimedOut)
        );
        pending.signal();
        assert_eq!(
            array_wait(&[pending, signaled], &[0, 0], WAIT_ALL, 0, None),
            Ok(0)
        );
    }

    #[test]
    fn unsubmitted_binary_fence_is_invalid_without_wait_for_submit() {
        let object = Syncobj::new(false);
        assert_eq!(
            wait_all_points(&[object], &[0]),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn timeline_point_wait_and_query_follow_completion() {
        let object = Syncobj::new(false);
        let a = Fence::new(false);
        object.submit_point(2, a.clone()).unwrap();
        assert_eq!(object.query_point(true), 2);
        assert_eq!(object.query_point(false), 0);
        assert_eq!(wait_all_points(&[object.clone()], &[2]), Err(AxError::TimedOut));
        a.signal();
        assert_eq!(wait_all_points(&[object.clone()], &[2]), Ok(0));
        assert_eq!(object.query_point(false), 2);
        // A point never submitted cannot be waited on without WAIT_FOR_SUBMIT.
        assert_eq!(
            wait_all_points(&[object.clone()], &[5]),
            Err(AxError::InvalidInput)
        );
        object.signal_point(4).unwrap();
        assert_eq!(object.query_point(true), 4);
        assert_eq!(wait_all_points(&[object.clone()], &[4]), Ok(0));
        assert!(object.fence_at(3).unwrap().is_signaled());
    }

    #[test]
    fn wait_for_submit_returns_the_published_point() {
        let object = Syncobj::new(false);
        let late = Fence::new(false);
        object.submit_point(1, late.clone()).unwrap();
        let fence = find_fence(&object, 1, WAIT_FOR_SUBMIT).unwrap();
        assert!(!fence.is_signaled());
        late.signal();
        assert!(fence.is_signaled());
        assert_eq!(
            find_fence(&object, 9, 0).err(),
            Some(AxError::InvalidInput)
        );
        assert_eq!(
            find_fence(&object, 0, 0x80).err(),
            Some(AxError::InvalidInput)
        );
    }

    #[test]
    fn transfer_timeline_to_binary_and_binary_to_timeline() {
        let timeline = Syncobj::new(false);
        let binary = Syncobj::new(false);
        let gpu = Fence::new(false);
        timeline.submit_point(3, gpu.clone()).unwrap();
        transfer(&timeline, &binary, 3, 0, 0).unwrap();
        assert!(!binary.fence().unwrap().is_signaled());
        gpu.signal();
        assert!(binary.fence().unwrap().is_signaled());
        transfer(&binary, &timeline, 0, 7, 0).unwrap();
        assert_eq!(timeline.query_point(true), 7);
        assert_eq!(wait_all_points(&[timeline.clone()], &[7]), Ok(0));
    }

    #[test]
    fn reset_under_exec_output_keeps_prior_point_dependency_pending() {
        let object = Syncobj::new(false);
        let first = Fence::new(false);
        object.submit_point(1, first.clone()).unwrap();
        object
            .apply_exec_output(true, 0, Fence::new(false))
            .unwrap();
        assert_eq!(object.query_point(true), 0);
        assert!(object.fence().is_ok());
        assert!(!first.is_signaled());
    }
}
