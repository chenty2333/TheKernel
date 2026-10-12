//! Small, device-independent completion fences.

use alloc::{sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
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
    // Flattened leaf snapshots only: no recursive chain, producer mutation or
    // reference cycle. Empty for device-owned completion fences.
    dependencies: Vec<Arc<Fence>>,
    // Present for a dma-fence-chain node (a timeline point). Its `dependencies`
    // are the pending leaves of the whole chain, as dma_fence_chain_signaled()
    // observes them, and its error state is never propagated.
    chain: Option<ChainLink>,
    // Set once a composite has subscribed to its leaves' callbacks.
    subscribed: AtomicBool,
    // dma_fence_add_callback() equivalents, run once, outside every lock.
    callbacks: spin::Mutex<Vec<(u64, FenceCallback)>>,
    state: axgpu::Fence,
    // 0 pending, 1 success, 2 error.  A single CAS-owned terminal state
    // makes success/error races deterministic and wakes observers once.
    terminal: AtomicU8,
    deadline: spin::Mutex<Option<axhal::time::TimeValue>>,
    waiters: WaitQueue,
    poll_waiters: PollSet,
}

/// A one-shot completion callback, run after the fence reaches a terminal state.
pub(crate) type FenceCallback = alloc::boxed::Box<dyn FnOnce() + Send>;
static CALLBACK_IDS: AtomicU64 = AtomicU64::new(1);

/// The dma_fence_chain fields of one timeline point. `prev` is replaced only
/// by the chain walk that garbage-collects signaled predecessors.
pub(crate) struct ChainLink {
    pub(crate) context: u64,
    pub(crate) seqno: u64,
    pub(crate) prev_seqno: u64,
    prev: spin::Mutex<Option<Arc<Fence>>>,
    contained: Arc<Fence>,
}

impl ChainLink {
    pub(crate) fn prev(&self) -> Option<Arc<Fence>> {
        self.prev.lock().clone()
    }
    /// cmpxchg(&chain->prev, prev, replacement): install only if `prev` is
    /// still the current predecessor.
    pub(crate) fn replace_prev_if(&self, prev: &Arc<Fence>, replacement: Option<Arc<Fence>>) {
        let mut current = self.prev.lock();
        if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, prev)) {
            *current = replacement;
        }
    }
    /// dma_fence_chain_contained(): the device or software fence of this point.
    pub(crate) fn contained(&self) -> &Arc<Fence> {
        &self.contained
    }
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
    pub fn replace(&self, fence: Arc<Fence>) -> AxResult<Option<Arc<Fence>>> {
        let upstream = crate::drm::intel::upstream_sync::prepare_native_reservation_update(
            &[self],
            fence.clone(),
        )?;
        let previous = self.exclusive.lock().replace(fence.clone());
        let changed = previous
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &fence));
        if changed {
            if let Some(upstream) = upstream {
                upstream.publish();
            }
        }
        Ok(previous)
    }

    /// Add an external synchronization dependency without discarding a
    /// dependency already owned by this reservation. Unlike `replace`, this
    /// does not wait: future implicit consumers wait for both fence leaves.
    pub fn append(&self, fence: Arc<Fence>) -> AxResult<()> {
        for _ in 0..8 {
            let previous = self.predecessor();
            let replacement = match &previous {
                Some(previous) => Fence::join(&[previous.clone(), fence.clone()])?,
                None => fence.clone(),
            };
            if previous
                .as_ref()
                .is_some_and(|previous| Arc::ptr_eq(previous, &replacement))
            {
                return Ok(());
            }
            let upstream = crate::drm::intel::upstream_sync::prepare_native_reservation_update(
                &[self],
                fence.clone(),
            )?;
            let mut exclusive = self.exclusive.lock();
            let unchanged = match (exclusive.as_ref(), previous.as_ref()) {
                (None, None) => true,
                (Some(current), Some(previous)) => Arc::ptr_eq(current, previous),
                _ => false,
            };
            if !unchanged {
                drop(exclusive);
                drop(upstream);
                continue;
            }
            *exclusive = Some(replacement);
            drop(exclusive);
            if let Some(upstream) = upstream {
                upstream.publish();
            }
            return Ok(());
        }
        Err(AxError::WouldBlock)
    }

    /// Import an upstream dma-fence edge without publishing it back into the
    /// same Linux reservation object. The upstream side owns that edge already.
    pub(crate) fn append_from_upstream(&self, fence: Arc<Fence>) -> AxResult<()> {
        for _ in 0..8 {
            let previous = self.exclusive.lock().clone();
            if previous
                .as_ref()
                .is_some_and(|previous| Arc::ptr_eq(previous, &fence))
            {
                return Ok(());
            }
            let replacement = match &previous {
                Some(previous) => Fence::join(&[previous.clone(), fence.clone()])?,
                None => fence.clone(),
            };
            let mut exclusive = self.exclusive.lock();
            let unchanged = match (exclusive.as_ref(), previous.as_ref()) {
                (None, None) => true,
                (Some(current), Some(previous)) => Arc::ptr_eq(current, previous),
                _ => false,
            };
            if unchanged {
                *exclusive = Some(replacement);
                return Ok(());
            }
        }
        Err(AxError::WouldBlock)
    }

    /// Install the initial resource-ready dependency. Resource construction
    /// has no predecessor; later GPU/KMS submissions use `replace_many`.
    pub fn publish(&self, fence: Arc<Fence>) -> AxResult<()> {
        debug_assert!(self.exclusive.lock().is_none());
        self.replace(fence)?;
        Ok(())
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

        let upstream = crate::drm::intel::upstream_sync::prepare_native_reservation_update(
            reservations,
            fence.clone(),
        )?;

        for reservation in reservations.iter() {
            guards.push(reservation.exclusive.lock());
        }
        for guard in &mut guards {
            if let Some(predecessor) = guard.replace(fence.clone()) {
                predecessors.push(predecessor);
            }
        }
        drop(guards);
        if let Some(upstream) = upstream {
            upstream.publish();
        }
        Ok(predecessors)
    }

    /// Replace a set of GEM reservations while honoring per-object opt-outs
    /// from implicit synchronization. For opted-out objects the previous
    /// fence is chained with the new completion fence in the reservation, so
    /// a later non-async operation still observes both; only non-async
    /// predecessors are returned for the caller to wait.
    ///
    /// Build replacement chains outside the reservation locks, then validate
    /// that their predecessor snapshot is still current before publishing the
    /// complete set. Under contention this retries a bounded number of times
    /// and fails without publishing any completion fence.
    pub fn replace_many_with_async(
        reservations: &mut [(&Reservation, bool)],
        fence: Arc<Fence>,
    ) -> AxResult<Vec<Arc<Fence>>> {
        reservations
            .sort_unstable_by_key(|(reservation, _)| (*reservation as *const Reservation).addr());

        if reservations
            .iter()
            .all(|(_, skip_implicit)| !*skip_implicit)
        {
            let mut ordinary = Vec::new();
            ordinary
                .try_reserve_exact(reservations.len())
                .map_err(|_| AxError::NoMemory)?;
            ordinary.extend(reservations.iter().map(|(reservation, _)| *reservation));
            return Self::replace_many(&mut ordinary, fence);
        }

        let mut snapshots = Vec::new();
        snapshots
            .try_reserve_exact(reservations.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut replacements = Vec::new();
        replacements
            .try_reserve_exact(reservations.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut guards = Vec::new();
        guards
            .try_reserve_exact(reservations.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut predecessors = Vec::new();
        predecessors
            .try_reserve_exact(reservations.len())
            .map_err(|_| AxError::NoMemory)?;

        let mut reservation_refs = Vec::new();
        {
            reservation_refs
                .try_reserve_exact(reservations.len())
                .map_err(|_| AxError::NoMemory)?;
            reservation_refs.extend(reservations.iter().map(|(reservation, _)| *reservation));
        }

        for _ in 0..8 {
            snapshots.clear();
            for (reservation, _) in reservations.iter() {
                snapshots.push(reservation.exclusive.lock().clone());
            }

            replacements.clear();
            for ((_, skip_implicit), previous) in reservations.iter().zip(&snapshots) {
                let replacement = if *skip_implicit {
                    if let Some(previous) = previous {
                        Fence::join(&[previous.clone(), fence.clone()])?
                    } else {
                        fence.clone()
                    }
                } else {
                    fence.clone()
                };
                replacements.push(replacement);
            }

            let upstream = crate::drm::intel::upstream_sync::prepare_native_reservation_update(
                &reservation_refs,
                fence.clone(),
            )?;

            guards.clear();
            for (reservation, _) in reservations.iter() {
                guards.push(reservation.exclusive.lock());
            }
            let unchanged = guards.iter().zip(&snapshots).all(|(current, expected)| {
                match (current.as_ref(), expected.as_ref()) {
                    (None, None) => true,
                    (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
                    _ => false,
                }
            });
            if !unchanged {
                guards.clear();
                drop(upstream);
                continue;
            }

            predecessors.clear();
            for (index, (_, skip_implicit)) in reservations.iter().enumerate() {
                if !skip_implicit && let Some(previous) = &snapshots[index] {
                    predecessors.push(previous.clone());
                }
                *guards[index] = Some(replacements[index].clone());
            }
            let predecessors = core::mem::take(&mut predecessors);
            drop(guards);
            if let Some(upstream) = upstream {
                upstream.publish();
            }
            return Ok(predecessors);
        }

        Err(AxError::WouldBlock)
    }
}

impl Fence {
    pub fn new(signaled: bool) -> Arc<Self> {
        if !signaled {
            PENDING_FENCES.fetch_add(1, Ordering::Relaxed);
        }
        Arc::new(Self {
            dependencies: Vec::new(),
            chain: None,
            subscribed: AtomicBool::new(false),
            callbacks: spin::Mutex::new(Vec::new()),
            state: axgpu::Fence::new(signaled),
            terminal: AtomicU8::new(if signaled { 1 } else { 0 }),
            deadline: spin::Mutex::new(None),
            waiters: WaitQueue::new(),
            poll_waiters: PollSet::new(),
        })
    }
    /// Consumer-only timeline view. Source dma-fence-chain waits all older
    /// producers; software SIGNAL cannot complete one of those producers.
    /// Flatten/deduplicate pending leaves to bound call-stack depth. One
    /// completed error is enough to retain conservative error propagation.
    pub(crate) fn join(fences: &[Arc<Self>]) -> AxResult<Arc<Self>> {
        let mut dependencies = Vec::new();
        let mut error = false;
        for fence in fences {
            let leaves = if fence.dependencies.is_empty() {
                core::slice::from_ref(fence)
            } else {
                &fence.dependencies
            };
            dependencies
                .try_reserve(leaves.len())
                .map_err(|_| AxError::NoMemory)?;
            for leaf in leaves {
                if leaf.is_signaled() {
                    if !leaf.is_failed() || error {
                        continue;
                    }
                    error = true;
                }
                if !dependencies.iter().any(|old| Arc::ptr_eq(old, leaf)) {
                    dependencies.push(leaf.clone());
                }
            }
        }
        if dependencies.is_empty() {
            return Ok(Self::new(true));
        }
        if dependencies.len() == 1 {
            return Ok(dependencies.pop().unwrap());
        }
        PENDING_FENCES.fetch_add(1, Ordering::Relaxed);
        // A failed allocation drops this pending value and its accounting,
        // exactly as dropping a never-submitted ordinary fence does.
        Arc::try_new(Self {
            dependencies,
            chain: None,
            subscribed: AtomicBool::new(false),
            callbacks: spin::Mutex::new(Vec::new()),
            state: axgpu::Fence::new(false),
            terminal: AtomicU8::new(0),
            deadline: spin::Mutex::new(None),
            waiters: WaitQueue::new(),
            poll_waiters: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Collect the not-yet-signaled leaves of `fence` into `out`, the set a
    /// chain or composite must still wait for. Error state is not inspected.
    pub(crate) fn collect_pending(
        fence: &Arc<Self>,
        out: &mut Vec<Arc<Self>>,
    ) -> AxResult<()> {
        if fence.is_signaled() {
            return Ok(());
        }
        let leaves = if fence.dependencies.is_empty() {
            core::slice::from_ref(fence)
        } else {
            &fence.dependencies
        };
        out.try_reserve(leaves.len()).map_err(|_| AxError::NoMemory)?;
        for leaf in leaves {
            if !leaf.is_signaled() && !out.iter().any(|old| Arc::ptr_eq(old, leaf)) {
                out.push(leaf.clone());
            }
        }
        Ok(())
    }

    /// Build one dma-fence-chain node for timeline `seqno` whose predecessor is
    /// `prev`. Its pending leaves are the predecessor's plus `contained`'s.
    pub(crate) fn chain_node(
        prev: Option<Arc<Self>>,
        contained: Arc<Self>,
        context: u64,
        seqno: u64,
        prev_seqno: u64,
    ) -> AxResult<Arc<Self>> {
        let mut pending = Vec::new();
        if let Some(prev) = &prev {
            Self::collect_pending(prev, &mut pending)?;
        }
        Self::collect_pending(&contained, &mut pending)?;
        let signaled = pending.is_empty();
        if !signaled {
            PENDING_FENCES.fetch_add(1, Ordering::Relaxed);
        }
        let link = ChainLink {
            context,
            seqno,
            prev_seqno,
            prev: spin::Mutex::new(prev),
            contained,
        };
        Arc::try_new(Self {
            dependencies: pending,
            chain: Some(link),
            subscribed: AtomicBool::new(false),
            callbacks: spin::Mutex::new(Vec::new()),
            state: axgpu::Fence::new(signaled),
            terminal: AtomicU8::new(u8::from(signaled)),
            deadline: spin::Mutex::new(None),
            waiters: WaitQueue::new(),
            poll_waiters: PollSet::new(),
        })
        .map_err(|_| {
            if !signaled {
                PENDING_FENCES.fetch_sub(1, Ordering::Relaxed);
            }
            AxError::NoMemory
        })
    }

    /// The dma-fence-chain fields, or None for an ordinary fence.
    pub(crate) fn chain(&self) -> Option<&ChainLink> {
        self.chain.as_ref()
    }

    /// dma_fence_add_callback(): `None` means the fence is already signaled
    /// and the callback was not registered (Linux -ENOENT).
    pub(crate) fn add_callback(self: &Arc<Self>, callback: FenceCallback) -> Option<u64> {
        if self.is_signaled() {
            return None;
        }
        let id = CALLBACK_IDS.fetch_add(1, Ordering::Relaxed);
        {
            let mut callbacks = self.callbacks.lock();
            // signal() publishes the terminal state before draining under this
            // same lock, so a callback is either refused here or drained there.
            if self.terminal.load(Ordering::Acquire) != 0 {
                return None;
            }
            callbacks.push((id, callback));
        }
        if !self.dependencies.is_empty() && !self.subscribed.swap(true, Ordering::AcqRel) {
            for leaf in &self.dependencies {
                let weak = Arc::downgrade(self);
                let _ = leaf.add_callback(alloc::boxed::Box::new(move || {
                    if let Some(composite) = weak.upgrade() {
                        composite.refresh_dependencies();
                    }
                }));
            }
            self.refresh_dependencies();
        }
        Some(id)
    }

    /// dma_fence_remove_callback(): drop a registration that has not run.
    pub(crate) fn remove_callback(&self, id: u64) {
        self.callbacks.lock().retain(|(old, _)| *old != id);
    }

    fn run_callbacks(&self) {
        let callbacks = core::mem::take(&mut *self.callbacks.lock());
        for (_, callback) in callbacks {
            callback();
        }
    }
    fn refresh_dependencies(&self) {
        if self.terminal.load(Ordering::Acquire) != 0 || self.dependencies.is_empty() {
            return;
        }
        if self.dependencies.iter().all(|leaf| leaf.is_signaled()) {
            // Deliberate deviation from Linux dma_fence_chain: a failed contained
            // submission must stay visible on the timeline point, since exec
            // outputs and sync_file consumers rely on it. Waits still complete.
            if self.dependencies.iter().any(|leaf| leaf.is_failed()) {
                self.signal_error();
            } else {
                self.signal();
            }
        }
    }
    pub fn is_signaled(&self) -> bool {
        self.refresh_dependencies();
        self.terminal.load(Ordering::Acquire) != 0
    }
    pub fn is_failed(&self) -> bool {
        self.refresh_dependencies();
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
            self.run_callbacks();
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
            self.run_callbacks();
        }
    }
    /// A DRM deadline is a scheduling hint, not a second completion state.
    /// Preserve it on the fence so a transport scheduler can consume it
    /// without changing the direct-fence wait/error contract.
    pub fn set_deadline(&self, deadline: axhal::time::TimeValue) {
        *self.deadline.lock() = Some(deadline);
        for leaf in &self.dependencies {
            leaf.set_deadline(deadline);
        }
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
        let count = if self.dependencies.is_empty() {
            1
        } else {
            self.dependencies.len()
        };
        let mut prepared =
            axpoll::PreparedPollRegistration::try_new(if readable { count } else { 0 })?;
        if readable {
            if self.dependencies.is_empty() {
                prepared.arm(&self.poll_waiters, context.waker())?;
            } else {
                for leaf in &self.dependencies {
                    prepared.arm(&leaf.poll_waiters, context.waker())?;
                }
            }
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
        let waiters = if self.dependencies.is_empty() {
            &self.waiters
        } else {
            &FENCE_SET_WAITERS
        };
        match timeout {
            Some(duration) if duration.is_zero() => Err(AxError::WouldBlock),
            Some(duration) => {
                if waiters.wait_timeout_until(duration, || self.is_signaled())?
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
                waiters
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
    fn joined_consumer_views_wait_all_leaves_keep_errors_and_flatten_long_chains() {
        let first = Fence::new(false);
        let second = Fence::new(false);
        let mut view = Fence::join(&[first.clone(), second.clone()]).unwrap();
        for _ in 0..10_000 {
            view = Fence::join(&[view, Fence::new(true)]).unwrap();
        }
        assert_eq!(view.dependencies.len(), 2);
        assert_eq!(view.wait(Some(Duration::ZERO)), Err(AxError::WouldBlock));
        first.signal_error();
        assert!(!view.is_signaled());
        assert!(!second.is_signaled());
        second.signal();
        assert!(view.is_signaled());
        assert!(view.is_failed());
        assert_eq!(view.wait(Some(Duration::ZERO)), Err(AxError::Io));
    }
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
        first.replace(predecessor.clone()).unwrap();
        second.replace(predecessor.clone()).unwrap();
        let completion = Fence::new(false);
        let mut reservations = [&second, &first];

        let prior = Reservation::replace_many(&mut reservations, completion.clone()).unwrap();
        assert_eq!(prior.len(), 2);
        assert!(prior.iter().all(|fence| Arc::ptr_eq(fence, &predecessor)));
        assert!(Arc::ptr_eq(&first.predecessor().unwrap(), &completion));
        assert!(Arc::ptr_eq(&second.predecessor().unwrap(), &completion));
    }

    #[test]
    fn async_reservation_skips_wait_but_preserves_prior_and_current_edges() {
        let asynchronous = Reservation::new();
        let synchronous = Reservation::new();
        let async_prior = Fence::new(false);
        let sync_prior = Fence::new(false);
        asynchronous.replace(async_prior.clone()).unwrap();
        synchronous.replace(sync_prior.clone()).unwrap();
        let completion = Fence::new(false);
        let mut reservations = [(&synchronous, false), (&asynchronous, true)];

        let waits =
            Reservation::replace_many_with_async(&mut reservations, completion.clone()).unwrap();
        assert_eq!(waits.len(), 1);
        assert!(Arc::ptr_eq(&waits[0], &sync_prior));
        assert_eq!(
            sync_prior.wait(Some(Duration::ZERO)),
            Err(AxError::WouldBlock)
        );
        assert_eq!(
            async_prior.wait(Some(Duration::ZERO)),
            Err(AxError::WouldBlock)
        );

        completion.signal();
        assert!(synchronous.predecessor().unwrap().is_signaled());
        let joined = asynchronous.predecessor().unwrap();
        assert!(!joined.is_signaled(), "the async predecessor remains live");
        async_prior.signal();
        assert!(joined.is_signaled());
    }

    #[test]
    fn failed_admission_restores_the_exact_predecessor() {
        let reservation = Reservation::new();
        let predecessor = Fence::new(true);
        reservation.replace(predecessor.clone()).unwrap();
        let rejected = Fence::new(false);

        let prior = reservation.replace(rejected.clone()).unwrap();
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
        let prior = reservation.replace(rejected.clone()).unwrap();
        let later = Fence::new(false);
        reservation.replace(later.clone()).unwrap();

        reservation.restore_if_current(&rejected, prior);

        assert!(Arc::ptr_eq(&reservation.predecessor().unwrap(), &later));
    }
}
