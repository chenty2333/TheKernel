//! Small, device-independent completion fences.

use alloc::sync::Arc;
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet};
use axtask::WaitQueue;

// This is only a wakeup fan-in: callers always rescan their own fence list
// under acquire loads.  It lets a binary syncobj WAIT(any) sleep until *any*
// member changes without introducing a descriptor table or callback lifetime.
static FENCE_SET_WAITERS: WaitQueue = WaitQueue::new();

/// One-shot fence.  Signalling is monotonic and never runs callbacks under a
/// DRM object lock.
pub struct Fence {
    signaled: AtomicBool,
    waiters: WaitQueue,
    poll_waiters: PollSet,
}

impl Fence {
    pub fn new(signaled: bool) -> Arc<Self> {
        Arc::new(Self {
            signaled: AtomicBool::new(signaled),
            waiters: WaitQueue::new(),
            poll_waiters: PollSet::new(),
        })
    }
    pub fn is_signaled(&self) -> bool {
        self.signaled.load(Ordering::Acquire)
    }
    pub fn signal(&self) {
        if !self.signaled.swap(true, Ordering::AcqRel) {
            self.waiters.notify_all(false);
            FENCE_SET_WAITERS.notify_all(false);
            self.poll_waiters.wake();
        }
    }

    pub(crate) fn poll_events(&self) -> IoEvents {
        self.is_signaled()
            .then_some(IoEvents::READABLE)
            .unwrap_or_default()
    }

    pub(crate) fn register_events<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let readable = events.intersects(IoEvents::READABLE | IoEvents::ERROR);
        let mut prepared = axpoll::PreparedPollRegistration::try_new(readable as usize)?;
        if readable {
            prepared.arm(&self.poll_waiters, context.waker())?;
        }
        prepared.commit()
    }
    /// `None` waits indefinitely; a zero duration is a non-blocking probe.
    pub fn wait(&self, timeout: Option<Duration>) -> AxResult<()> {
        if self.is_signaled() {
            return Ok(());
        }
        match timeout {
            Some(duration) if duration.is_zero() => Err(AxError::WouldBlock),
            Some(duration) => {
                if self
                    .waiters
                    .wait_timeout_until(duration, || self.is_signaled())?
                    && !self.is_signaled()
                {
                    Err(AxError::WouldBlock)
                } else {
                    Ok(())
                }
            }
            None => {
                self.waiters
                    .wait_until(|| self.is_signaled())
                    .map_err(AxError::from)?;
                Ok(())
            }
        }
    }

    pub(crate) fn wait_any(fences: &[Arc<Self>], timeout: Option<Duration>) -> AxResult<usize> {
        if let Some(index) = fences.iter().position(|fence| fence.is_signaled()) {
            return Ok(index);
        }
        match timeout {
            Some(duration) if duration.is_zero() => Err(AxError::WouldBlock),
            Some(duration) => {
                FENCE_SET_WAITERS.wait_timeout_until(duration, || {
                    fences.iter().any(|fence| fence.is_signaled())
                })?;
                fences
                    .iter()
                    .position(|fence| fence.is_signaled())
                    .ok_or(AxError::WouldBlock)
            }
            None => {
                FENCE_SET_WAITERS
                    .wait_until(|| fences.iter().any(|fence| fence.is_signaled()))
                    .map_err(AxError::from)?;
                fences
                    .iter()
                    .position(|fence| fence.is_signaled())
                    .ok_or(AxError::WouldBlock)
            }
        }
    }
}

/// Per-GEM reservation state.  Submissions replace the exclusive completion
/// fence only after callers have obtained any predecessor to wait on.
pub struct Reservation {
    exclusive: spin::Mutex<Option<Arc<Fence>>>,
}
impl Reservation {
    pub const fn new() -> Self {
        Self {
            exclusive: spin::Mutex::new(None),
        }
    }
    pub fn predecessor(&self) -> Option<Arc<Fence>> {
        self.exclusive.lock().clone()
    }
    pub fn publish(&self, fence: Arc<Fence>) {
        *self.exclusive.lock() = Some(fence);
    }
}
