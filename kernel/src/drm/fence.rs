//! Small, device-independent completion fences.

use alloc::{sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
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
static PENDING_FENCES: AtomicU64 = AtomicU64::new(0);
static ERROR_FENCES: AtomicU64 = AtomicU64::new(0);

/// Global fence accounting for the one supported DRM device.  It intentionally
/// observes only fence state transitions; reading it neither waits nor drains
/// a completion queue.
pub(crate) fn metrics() -> (u64, u64) {
    (
        PENDING_FENCES.load(Ordering::Acquire),
        ERROR_FENCES.load(Ordering::Acquire),
    )
}

/// One-shot fence.  Signalling is monotonic and never runs callbacks under a
/// DRM object lock.
pub struct Fence {
    state: axgpu::Fence,
    // 0 pending, 1 success, 2 error.  A single CAS-owned terminal state
    // makes success/error races deterministic and wakes observers once.
    terminal: AtomicU8,
    deadline: spin::Mutex<Option<axhal::time::TimeValue>>,
    waiters: WaitQueue,
    poll_waiters: PollSet,
}

/// Per-GEM reservation state.  Submissions replace the exclusive completion
/// fence only after callers have obtained any predecessor to wait on.
///
/// axgpu's reservation is only a sequence allocator, whereas DRM needs to
/// retain the predecessor fence identity for future submission ordering.
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
    /// Replace one reservation edge and return the exact predecessor under
    /// the same lock. KMS uses this at commit admission so render submission
    /// can observe neither an unqueued scanout nor a queued scanout without
    /// its completion dependency.
    pub fn replace(&self, fence: Arc<Fence>) -> Option<Arc<Fence>> {
        self.exclusive.lock().replace(fence)
    }

    /// Install the initial resource-ready dependency. Resource construction
    /// has no predecessor; later GPU/KMS submissions use `replace_many`.
    pub fn publish(&self, fence: Arc<Fence>) {
        debug_assert!(self.exclusive.lock().is_none());
        let _ = self.replace(fence);
    }

    /// Roll back a failed admission only if no later submission has already
    /// replaced this fence. A later publisher owns the dependency chain and
    /// must observe the failed fence rather than losing its predecessor.
    pub fn restore_if_current(&self, installed: &Arc<Fence>, predecessor: Option<Arc<Fence>>) {
        let mut exclusive = self.exclusive.lock();
        if exclusive
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, installed))
        {
            *exclusive = predecessor;
        }
    }

    /// Atomically replace the exclusive fence for a set of distinct GEM
    /// reservations and return their prior fences.  Callers must wait the
    /// returned fences only after this function returns: sleeping while a
    /// reservation lock is held would prevent dependent submissions from
    /// publishing their own completion fence.
    ///
    /// Sorting by reservation address establishes one lock order for
    /// overlapping EXECBUFFER object sets.  This makes the predecessor
    /// snapshot and publication indivisible across the entire set, rather
    /// than allowing per-object replacement to form a dependency cycle.
    pub fn replace_many(
        reservations: &mut [&Reservation],
        fence: Arc<Fence>,
    ) -> AxResult<Vec<Arc<Fence>>> {
        reservations
            .sort_unstable_by_key(|reservation| (*reservation as *const Reservation).addr());

        let mut guards = Vec::new();
        guards
            .try_reserve_exact(reservations.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut predecessors = Vec::new();
        predecessors
            .try_reserve_exact(reservations.len())
            .map_err(|_| AxError::NoMemory)?;

        for reservation in reservations {
            guards.push(reservation.exclusive.lock());
        }
        for guard in &mut guards {
            if let Some(predecessor) = guard.replace(fence.clone()) {
                predecessors.push(predecessor);
            }
        }
        drop(guards);
        Ok(predecessors)
    }
}

impl Fence {
    pub fn new(signaled: bool) -> Arc<Self> {
        if !signaled {
            PENDING_FENCES.fetch_add(1, Ordering::Relaxed);
        }
        Arc::new(Self {
            state: axgpu::Fence::new(signaled),
            terminal: AtomicU8::new(if signaled { 1 } else { 0 }),
            deadline: spin::Mutex::new(None),
            waiters: WaitQueue::new(),
            poll_waiters: PollSet::new(),
        })
    }
    pub fn is_signaled(&self) -> bool {
        self.terminal.load(Ordering::Acquire) != 0
    }
    pub fn is_failed(&self) -> bool {
        self.terminal.load(Ordering::Acquire) == 2
    }
    pub fn signal(&self) {
        if self
            .terminal
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.state.signal();
            PENDING_FENCES.fetch_sub(1, Ordering::Relaxed);
            self.waiters.notify_all(false);
            FENCE_SET_WAITERS.notify_all(false);
            self.poll_waiters.wake();
        }
    }
    /// Terminally fail a fence. Failure is also a wakeup, but waiters and
    /// pollers can distinguish it from successful GPU completion.
    pub fn signal_error(&self) {
        if self
            .terminal
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.state.signal();
            ERROR_FENCES.fetch_add(1, Ordering::Relaxed);
            PENDING_FENCES.fetch_sub(1, Ordering::Relaxed);
            self.waiters.notify_all(false);
            FENCE_SET_WAITERS.notify_all(false);
            self.poll_waiters.wake();
        }
    }
    /// A DRM deadline is a scheduling hint, not a second completion state.
    /// Preserve it on the fence so a transport scheduler can consume it
    /// without changing the direct-fence wait/error contract.
    pub fn set_deadline(&self, deadline: axhal::time::TimeValue) {
        *self.deadline.lock() = Some(deadline);
    }
    pub fn deadline(&self) -> Option<axhal::time::TimeValue> {
        *self.deadline.lock()
    }

    pub(crate) fn poll_events(&self) -> IoEvents {
        if self.is_signaled() {
            if self.is_failed() {
                IoEvents::ERROR
            } else {
                IoEvents::READABLE
            }
        } else {
            Default::default()
        }
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
            return if self.is_failed() {
                Err(AxError::Io)
            } else {
                Ok(())
            };
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
                    if self.is_failed() {
                        Err(AxError::Io)
                    } else {
                        Ok(())
                    }
                }
            }
            None => {
                self.waiters
                    .wait_until(|| self.is_signaled())
                    .map_err(AxError::from)?;
                if self.is_failed() {
                    Err(AxError::Io)
                } else {
                    Ok(())
                }
            }
        }
    }

    pub(crate) fn wait_any(fences: &[Arc<Self>], timeout: Option<Duration>) -> AxResult<usize> {
        if let Some(index) = fences.iter().position(|fence| fence.is_signaled()) {
            return if fences[index].is_failed() {
                Err(AxError::Io)
            } else {
                Ok(index)
            };
        }
        match timeout {
            Some(duration) if duration.is_zero() => Err(AxError::WouldBlock),
            Some(duration) => {
                FENCE_SET_WAITERS.wait_timeout_until(duration, || {
                    fences.iter().any(|fence| fence.is_signaled())
                })?;
                let index = fences
                    .iter()
                    .position(|fence| fence.is_signaled())
                    .ok_or(AxError::WouldBlock)?;
                if fences[index].is_failed() {
                    Err(AxError::Io)
                } else {
                    Ok(index)
                }
            }
            None => {
                FENCE_SET_WAITERS
                    .wait_until(|| fences.iter().any(|fence| fence.is_signaled()))
                    .map_err(AxError::from)?;
                let index = fences
                    .iter()
                    .position(|fence| fence.is_signaled())
                    .ok_or(AxError::WouldBlock)?;
                if fences[index].is_failed() {
                    Err(AxError::Io)
                } else {
                    Ok(index)
                }
            }
        }
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        // A cancelled owner may discard a never-submitted fence. It is no
        // longer observable or pending once its final reference goes away.
        if !self.is_signaled() {
            PENDING_FENCES.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_publishes_a_predecessor_that_waits_for_completion() {
        let reservation = Reservation::new();
        let fence = Fence::new(false);
        let mut reservations = [&reservation];

        assert!(
            Reservation::replace_many(&mut reservations, fence.clone())
                .unwrap()
                .is_empty()
        );
        let predecessor = reservation.predecessor().unwrap();
        assert!(Arc::ptr_eq(&predecessor, &fence));
        assert_eq!(
            predecessor.wait(Some(Duration::ZERO)),
            Err(AxError::WouldBlock)
        );

        fence.signal();
        assert_eq!(predecessor.wait(Some(Duration::ZERO)), Ok(()));
    }

    #[test]
    fn reservation_replaces_an_overlapping_set_as_one_snapshot() {
        let first = Reservation::new();
        let second = Reservation::new();
        let predecessor = Fence::new(false);
        first.replace(predecessor.clone());
        second.replace(predecessor.clone());
        let completion = Fence::new(false);
        let mut reservations = [&second, &first];

        let prior = Reservation::replace_many(&mut reservations, completion.clone()).unwrap();
        assert_eq!(prior.len(), 2);
        assert!(prior.iter().all(|fence| Arc::ptr_eq(fence, &predecessor)));
        assert!(Arc::ptr_eq(&first.predecessor().unwrap(), &completion));
        assert!(Arc::ptr_eq(&second.predecessor().unwrap(), &completion));
    }

    #[test]
    fn failed_admission_restores_the_exact_predecessor() {
        let reservation = Reservation::new();
        let predecessor = Fence::new(true);
        reservation.replace(predecessor.clone());
        let rejected = Fence::new(false);

        let prior = reservation.replace(rejected.clone());
        reservation.restore_if_current(&rejected, prior);

        assert!(Arc::ptr_eq(
            &reservation.predecessor().unwrap(),
            &predecessor
        ));
    }

    #[test]
    fn failed_admission_does_not_clobber_a_later_publisher() {
        let reservation = Reservation::new();
        let rejected = Fence::new(false);
        let prior = reservation.replace(rejected.clone());
        let later = Fence::new(false);
        reservation.replace(later.clone());

        reservation.restore_if_current(&rejected, prior);

        assert!(Arc::ptr_eq(&reservation.predecessor().unwrap(), &later));
    }
}
