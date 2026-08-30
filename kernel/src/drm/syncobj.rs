//! Binary DRM syncobj handles and sync-file OFDs.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{task::Context, time::Duration};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};

use super::fence::Fence;
use crate::file::{FileLike, Kstat};

pub type SyncobjHandle = u32;
pub struct Syncobj {
    fence: spin::Mutex<Arc<Fence>>,
}
impl Syncobj {
    pub fn new(signaled: bool) -> Arc<Self> {
        Arc::new(Self {
            fence: spin::Mutex::new(Fence::new(signaled)),
        })
    }
    pub fn fence(&self) -> Arc<Fence> {
        self.fence.lock().clone()
    }
    pub fn reset(&self) {
        *self.fence.lock() = Fence::new(false);
    }
    pub fn signal(&self) {
        self.fence().signal();
    }
    pub fn import_fence(&self, fence: Arc<Fence>) {
        *self.fence.lock() = fence;
    }
}

pub struct SyncFile {
    fence: Arc<Fence>,
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
    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[sync_file]".into())
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

pub(crate) fn wait(fences: Vec<Arc<Fence>>, wait_all: bool, timeout_nsec: i64) -> AxResult<usize> {
    if fences.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let deadline = timeout_deadline(timeout_nsec);
    if wait_all {
        for fence in &fences {
            fence.wait(remaining_timeout(deadline))?;
        }
        Ok(0)
    } else {
        Fence::wait_any(&fences, remaining_timeout(deadline))
    }
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
        assert_eq!(wait(vec![first, second], false, 0), Ok(1));
    }

    #[test]
    fn binary_wait_zero_timeout_is_nonblocking() {
        assert_eq!(
            wait(vec![Fence::new(false)], false, 0),
            Err(AxError::WouldBlock)
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
}
