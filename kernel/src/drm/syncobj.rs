//! DRM syncobj handles, including timeline points, and sync-file OFDs.

use alloc::{borrow::Cow, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{task::Context, time::Duration};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};

use super::fence::Fence;
use crate::file::{FileLike, Kstat};

pub type SyncobjHandle = u32;
pub struct Syncobj {
    // A newly-created unsignaled binary syncobj has no backing fence.  This
    // distinction is observable through WAIT_FOR_SUBMIT and is essential for
    // matching DRM's materialization semantics.
    fence: spin::Mutex<Option<Arc<Fence>>>,
    timeline: spin::Mutex<Timeline>,
    /// Opaque syncobj descriptors rescan the installed binary fence after
    /// an import, signal, or reset transition.
    poll_waiters: axpoll::PollSet,
}

struct Timeline {
    /// Fences explicitly published for future points.  A point at or below
    /// `signaled_through` is implicitly complete even when it is not present
    /// here, so a late waiter never needs a placeholder allocation.
    points: BTreeMap<u64, Arc<Fence>>,
    /// Implicitly successful points. Exact point fences are always consulted
    /// first: a failed fence must never be hidden by this success watermark.
    signaled_through: Option<u64>,
    last_submitted: u64,
    /// A generation fence used only to wait for a missing point to be
    /// submitted.  Publishing a point signals the current generation before
    /// installing the next one, so there is no lost wakeup between lookup and
    /// sleep.
    availability: Arc<Fence>,
}

impl Syncobj {
    pub fn new(signaled: bool) -> Arc<Self> {
        Arc::new(Self {
            fence: spin::Mutex::new(signaled.then(|| Fence::new(true))),
            timeline: spin::Mutex::new(Timeline {
                points: BTreeMap::new(),
                signaled_through: None,
                last_submitted: 0,
                availability: Fence::new(false),
            }),
            poll_waiters: axpoll::PollSet::new(),
        })
    }
    pub fn fence(&self) -> AxResult<Arc<Fence>> {
        self.fence.lock().clone().ok_or(AxError::NotFound)
    }
    pub fn reset(&self) {
        *self.fence.lock() = None;
        let mut timeline = self.timeline.lock();
        timeline.points.clear();
        timeline.signaled_through = None;
        timeline.last_submitted = 0;
        timeline.availability.signal();
        timeline.availability = Fence::new(false);
        self.poll_waiters.wake();
    }
    pub fn signal(&self) {
        let mut fence = self.fence.lock();
        match fence.as_ref() {
            Some(fence) => fence.signal(),
            None => *fence = Some(Fence::new(true)),
        }
        drop(fence);
        self.publish_availability();
        self.poll_waiters.wake();
    }
    pub fn import_fence(&self, fence: Arc<Fence>) {
        *self.fence.lock() = Some(fence);
        self.publish_availability();
        self.poll_waiters.wake();
    }

    /// Apply the EXECBUFFER output update as one syncobj-local transition.
    /// In particular RESET cannot expose an empty object between clearing old
    /// state and installing the completion fence selected by the admitted
    /// GPU job.
    pub(crate) fn apply_exec_output(
        &self,
        reset: bool,
        point: u64,
        fence: Arc<Fence>,
    ) -> AxResult<()> {
        let mut binary = self.fence.lock();
        let mut timeline = self.timeline.lock();
        if reset {
            *binary = None;
            timeline.points.clear();
            timeline.signaled_through = None;
            timeline.last_submitted = 0;
            timeline.availability.signal();
            timeline.availability = Fence::new(false);
        }
        if point == 0 {
            *binary = Some(fence);
            timeline.availability.signal();
            timeline.availability = Fence::new(false);
            return Ok(());
        }
        if timeline.points.contains_key(&point) {
            return Err(AxError::InvalidInput);
        }
        timeline.last_submitted = timeline.last_submitted.max(point);
        if timeline
            .signaled_through
            .is_some_and(|completed| point <= completed)
        {
            fence.signal();
        }
        timeline.points.insert(point, fence);
        timeline.availability.signal();
        timeline.availability = Fence::new(false);
        Ok(())
    }

    fn publish_availability(&self) {
        let mut timeline = self.timeline.lock();
        timeline.availability.signal();
        timeline.availability = Fence::new(false);
    }

    /// Return the fence currently backing `point`.  Point zero is always the
    /// binary syncobj fence; positive points are timeline-only.
    pub fn fence_at(&self, point: u64) -> AxResult<Arc<Fence>> {
        if point == 0 {
            return self.fence();
        }
        let timeline = self.timeline.lock();
        if let Some(fence) = timeline.points.get(&point) {
            return Ok(fence.clone());
        }
        if timeline
            .signaled_through
            .is_some_and(|completed| point <= completed)
        {
            return Ok(Fence::new(true));
        }
        Err(AxError::NotFound)
    }

    /// Wait until a point has a backing fence.  Missing points normally fail
    /// with ENOENT; WAIT_FOR_SUBMIT instead waits for a producer to publish
    /// the point, sharing the caller's absolute timeout budget.
    pub fn wait_fence_for_submit(
        &self,
        point: u64,
        wait_for_submit: bool,
        deadline: Option<axhal::time::TimeValue>,
    ) -> AxResult<Arc<Fence>> {
        if point == 0 {
            loop {
                let availability = match self.fence.lock().clone() {
                    Some(fence) => return Ok(fence),
                    None if !wait_for_submit => return Err(AxError::NotFound),
                    None => self.timeline.lock().availability.clone(),
                };
                availability.wait(remaining_timeout(deadline))?;
            }
        }
        loop {
            let availability = {
                let timeline = self.timeline.lock();
                if let Some(fence) = timeline.points.get(&point) {
                    return Ok(fence.clone());
                }
                if timeline
                    .signaled_through
                    .is_some_and(|completed| point <= completed)
                {
                    return Ok(Fence::new(true));
                }
                if !wait_for_submit {
                    return Err(AxError::NotFound);
                }
                timeline.availability.clone()
            };
            availability.wait(remaining_timeout(deadline))?;
        }
    }

    fn fence_or_availability(
        &self,
        point: u64,
        wait_for_submit: bool,
    ) -> AxResult<(Arc<Fence>, bool)> {
        if point == 0 {
            return match self.fence.lock().clone() {
                Some(fence) => Ok((fence, true)),
                None if wait_for_submit => Ok((self.timeline.lock().availability.clone(), false)),
                None => Err(AxError::NotFound),
            };
        }
        let timeline = self.timeline.lock();
        if let Some(fence) = timeline.points.get(&point) {
            return Ok((fence.clone(), true));
        }
        if timeline
            .signaled_through
            .is_some_and(|completed| point <= completed)
        {
            return Ok((Fence::new(true), true));
        }
        if wait_for_submit {
            Ok((timeline.availability.clone(), false))
        } else {
            Err(AxError::NotFound)
        }
    }

    /// Publish a fence at a timeline point. Timeline points may be submitted
    /// in any order, but one published point retains its fence identity.
    pub fn submit_point(&self, point: u64, fence: Arc<Fence>) -> AxResult<()> {
        if point == 0 {
            self.import_fence(fence);
            return Ok(());
        }
        let mut timeline = self.timeline.lock();
        if timeline.points.contains_key(&point) {
            return Err(AxError::InvalidInput);
        }
        timeline.last_submitted = timeline.last_submitted.max(point);
        if timeline
            .signaled_through
            .is_some_and(|completed| point <= completed)
        {
            fence.signal();
        }
        timeline.points.insert(point, fence);
        timeline.availability.signal();
        timeline.availability = Fence::new(false);
        Ok(())
    }

    /// Signal all timeline points up to and including `point`.  This models
    /// the monotonic completion guarantee of a timeline semaphore.
    pub fn signal_point(&self, point: u64) -> AxResult<()> {
        if point == 0 {
            self.signal();
            return Ok(());
        }
        let mut timeline = self.timeline.lock();
        if timeline
            .signaled_through
            .is_some_and(|completed| point < completed)
        {
            return Err(AxError::InvalidInput);
        }
        timeline.last_submitted = timeline.last_submitted.max(point);
        timeline.signaled_through = Some(point);
        for (_, fence) in timeline.points.range(..=point) {
            fence.signal();
        }
        timeline.availability.signal();
        timeline.availability = Fence::new(false);
        Ok(())
    }

    pub fn query_point(&self, last_submitted: bool) -> u64 {
        let timeline = self.timeline.lock();
        // Driver fences may complete without a userspace TIMELINE_SIGNAL.
        // Report their highest terminal point, including failure.  Do not fold
        // those points into `signaled_through`: an error fence remains an
        // exact dependency that transfer/export/GPU submission must observe.
        let completed = timeline
            .points
            .iter()
            .filter_map(|(&point, fence)| fence.is_signaled().then_some(point))
            .max()
            .map_or(timeline.signaled_through.unwrap_or(0), |point| {
                timeline.signaled_through.unwrap_or(0).max(point)
            });
        if last_submitted {
            timeline.last_submitted
        } else {
            completed
        }
    }
}

pub struct SyncFile {
    fence: Arc<Fence>,
}

/// An opaque syncobj file shares the object itself, unlike a sync_file which
/// snapshots one backing fence.
pub struct SyncobjFile {
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
            b"anon_inode:[drm_syncobj]",
        )))
    }
    fn set_nonblocking(&self, _: bool) -> AxResult<()> {
        Ok(())
    }
}
impl Pollable for SyncobjFile {
    fn poll(&self) -> IoEvents {
        self.object
            .fence()
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
impl SyncFile {
    pub fn new(fence: Arc<Fence>) -> Arc<Self> {
        Arc::new(Self { fence })
    }
    pub fn fence(&self) -> Arc<Fence> {
        self.fence.clone()
    }
}
impl FileLike for SyncFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[sync_file]",
        )))
    }
    fn set_nonblocking(&self, _: bool) -> AxResult<()> {
        Ok(())
    }
}
impl Pollable for SyncFile {
    fn poll(&self) -> IoEvents {
        self.fence.poll_events()
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.fence.register_events(context, events)
    }
}
pub(crate) fn export(
    fence: Arc<Fence>,
    context: &crate::file::IoctlContext,
    cloexec: bool,
) -> AxResult<i32> {
    context.add_file_like(SyncFile::new(fence), cloexec)
}
pub(crate) fn import(context: &crate::file::IoctlContext, fd: i32) -> AxResult<Arc<Fence>> {
    context
        .get_file_like(fd)?
        .downcast::<SyncFile>()
        .map(|f| f.fence())
}
pub(crate) fn export_syncobj(
    object: Arc<Syncobj>,
    context: &crate::file::IoctlContext,
    cloexec: bool,
) -> AxResult<i32> {
    context.add_file_like(SyncobjFile::new(object), cloexec)
}
pub(crate) fn import_syncobj(
    context: &crate::file::IoctlContext,
    fd: i32,
) -> AxResult<Arc<Syncobj>> {
    context
        .get_file_like(fd)?
        .downcast::<SyncobjFile>()
        .map(|file| file.object())
}

fn terminal_wait(fence: &Fence, timeout: Option<Duration>) -> AxResult<()> {
    match fence.wait(timeout) {
        // A fence error is terminal for syncobj ordering.  Keep the error on
        // the Fence itself for direct GPU users, but do not make DRM syncobj
        // waits wait forever behind a failed submission.
        Err(AxError::Io) => Ok(()),
        result => result,
    }
}

fn terminal_wait_any(fences: &[Arc<Fence>], timeout: Option<Duration>) -> AxResult<usize> {
    match Fence::wait_any(fences, timeout) {
        Err(AxError::Io) => fences
            .iter()
            .position(|fence| fence.is_signaled())
            .ok_or(AxError::Io),
        result => result,
    }
}

fn timeout_result<T>(result: AxResult<T>) -> AxResult<T> {
    result.map_err(|error| {
        if error == AxError::WouldBlock {
            AxError::TimedOut
        } else {
            error
        }
    })
}

pub(crate) fn wait(
    fences: Vec<Arc<Fence>>,
    wait_all: bool,
    timeout_nsec: i64,
    deadline_hint: Option<axhal::time::TimeValue>,
) -> AxResult<usize> {
    if fences.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let deadline = timeout_deadline(timeout_nsec);
    if let Some(deadline_hint) = deadline_hint {
        for fence in &fences {
            fence.set_deadline(deadline_hint);
        }
    }
    timeout_result(if wait_all {
        for fence in &fences {
            terminal_wait(fence, remaining_timeout(deadline))?;
        }
        Ok(0)
    } else {
        terminal_wait_any(&fences, remaining_timeout(deadline))
    })
}

/// `drm_syncobj_wait.timeout_nsec` is an absolute CLOCK_MONOTONIC deadline,
/// not a relative duration.  Negative values retain Linux's infinite-wait
/// sentinel.
fn timeout_deadline(timeout_nsec: i64) -> Option<axhal::time::TimeValue> {
    if timeout_nsec < 0 {
        None
    } else {
        Some(Duration::from_nanos(timeout_nsec as u64))
    }
}

fn remaining_timeout(deadline: Option<axhal::time::TimeValue>) -> Option<Duration> {
    deadline.map(|deadline| {
        deadline
            .checked_sub(axhal::time::monotonic_time())
            .unwrap_or(Duration::ZERO)
    })
}

pub(crate) fn timeline_wait(
    objects: &[(Arc<Syncobj>, u64)],
    wait_all: bool,
    wait_for_submit: bool,
    wait_available: bool,
    timeout_nsec: i64,
    deadline_hint: Option<axhal::time::TimeValue>,
) -> AxResult<usize> {
    if objects.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let deadline = timeout_deadline(timeout_nsec);
    timeout_result(if wait_all {
        for (object, point) in objects {
            let fence = object.wait_fence_for_submit(
                *point,
                wait_for_submit || wait_available,
                deadline,
            )?;
            if let Some(deadline_hint) = deadline_hint {
                fence.set_deadline(deadline_hint);
            }
            if !wait_available {
                terminal_wait(&fence, remaining_timeout(deadline))?;
            }
        }
        Ok(0)
    } else {
        // WAIT(any) must not let an unavailable first point hide a completed
        // later point.  Availability generations are included in the same
        // wait set; waking on one merely causes a rescan.
        loop {
            let mut fences = Vec::new();
            let mut completions = Vec::new();
            fences
                .try_reserve_exact(objects.len())
                .map_err(|_| AxError::NoMemory)?;
            completions
                .try_reserve_exact(objects.len())
                .map_err(|_| AxError::NoMemory)?;
            for (object, point) in objects {
                let (fence, completion) =
                    object.fence_or_availability(*point, wait_for_submit || wait_available)?;
                if completion {
                    if let Some(deadline_hint) = deadline_hint {
                        fence.set_deadline(deadline_hint);
                    }
                }
                fences.push(fence);
                completions.push(completion);
            }
            if let Some(index) = fences.iter().enumerate().find_map(|(index, fence)| {
                (completions[index] && (wait_available || fence.is_signaled())).then_some(index)
            }) {
                return Ok(index);
            }
            let index = terminal_wait_any(&fences, remaining_timeout(deadline))?;
            if completions[index] {
                return Ok(index);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake, vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use super::*;

    #[test]
    fn binary_wait_any_reports_the_signaled_member() {
        let first = Fence::new(false);
        let second = Fence::new(true);
        assert_eq!(wait(vec![first, second], false, 0, None), Ok(1));
    }

    #[test]
    fn binary_wait_zero_timeout_is_nonblocking() {
        assert_eq!(
            wait(vec![Fence::new(false)], false, 0, None),
            Err(AxError::TimedOut)
        );
    }

    #[test]
    fn wait_all_two_unfinished_fences_share_one_expired_deadline() {
        let fences = vec![Fence::new(false), Fence::new(false)];
        let deadline = Some(Duration::ZERO);
        for fence in fences {
            assert_eq!(
                fence.wait(remaining_timeout(deadline)),
                Err(AxError::WouldBlock)
            );
        }
    }

    #[test]
    fn absolute_monotonic_deadlines_are_not_rebased_at_wait_entry() {
        let now = axhal::time::monotonic_time();
        let deadline = now.checked_add(Duration::from_millis(1)).unwrap();
        let remaining = remaining_timeout(Some(deadline)).unwrap();
        assert!(remaining <= Duration::from_millis(1));
        assert_eq!(timeout_deadline(123), Some(Duration::from_nanos(123)));
        assert_eq!(timeout_deadline(-1), None);
    }

    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn sync_file_poll_registration_wakes_when_its_fence_signals() {
        let fence = Fence::new(false);
        let sync_file = SyncFile::new(fence.clone());
        assert_eq!(sync_file.poll(), IoEvents::empty());

        let wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let _registration = sync_file
            .register(&mut context, IoEvents::READABLE)
            .unwrap();

        fence.signal();
        assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        assert_eq!(sync_file.poll(), IoEvents::READABLE);
    }

    #[test]
    fn timeline_signals_prior_points_and_rejects_a_backward_completion() {
        let object = Syncobj::new(false);
        let submitted = Fence::new(false);
        object.submit_point(4, submitted.clone()).unwrap();

        object.signal_point(5).unwrap();
        assert!(submitted.is_signaled());
        assert!(object.fence_at(3).unwrap().is_signaled());
        assert_eq!(object.query_point(false), 5);
        assert_eq!(object.query_point(true), 5);
        assert_eq!(object.signal_point(4), Err(AxError::InvalidInput));
    }

    #[test]
    fn timeline_submission_never_replaces_an_existing_dependency() {
        let object = Syncobj::new(false);
        object.submit_point(7, Fence::new(false)).unwrap();
        assert_eq!(
            object.submit_point(7, Fence::new(false)),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            object.submit_point(6, Fence::new(false)),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn timeline_success_watermark_never_hides_an_exact_error_fence() {
        let object = Syncobj::new(false);
        let failed = Fence::new(false);
        object.submit_point(3, failed.clone()).unwrap();
        failed.signal_error();
        object.signal_point(5).unwrap();

        let exact = object.fence_at(3).unwrap();
        assert!(Arc::ptr_eq(&exact, &failed));
        assert!(exact.is_failed());
        assert_eq!(object.query_point(false), 5);
        assert!(object.fence_at(4).unwrap().is_signaled());
    }
}
