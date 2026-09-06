use alloc::vec::Vec;

use crate::{EpollId, FdNumber, InterestMask, InterestMode, OfdId, ReadyMask};

/// Linux epoll interest identity: the shared OFD plus descriptor used by ADD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpollKey {
    /// Stable open-file-description identity.
    pub ofd: OfdId,
    /// Numeric descriptor supplied to `EPOLL_CTL_ADD`.
    pub fd: FdNumber,
}

/// One fully armed interest ready for atomic epoll publication.
pub struct EpollInterest<U, S> {
    key: EpollKey,
    interest: InterestMask,
    mode: InterestMode,
    user_data: U,
    subscription: S,
}

impl<U, S> EpollInterest<U, S> {
    /// Creates an interest after the aggregate source subscription is armed.
    pub const fn new(
        key: EpollKey,
        interest: InterestMask,
        mode: InterestMode,
        user_data: U,
        subscription: S,
    ) -> Self {
        Self {
            key,
            interest,
            mode,
            user_data,
            subscription,
        }
    }

    /// Returns the Linux `(OFD, fd)` identity.
    pub const fn key(&self) -> EpollKey {
        self.key
    }

    /// Returns the normal readiness interest mask.
    pub const fn interest(&self) -> InterestMask {
        self.interest
    }

    /// Returns trigger behavior.
    pub const fn mode(&self) -> InterestMode {
        self.mode
    }

    /// Returns user-provided event data.
    pub const fn user_data(&self) -> &U {
        &self.user_data
    }

    /// Returns the retained aggregate source subscription.
    pub const fn subscription(&self) -> &S {
        &self.subscription
    }

    /// Decomposes ownership for lock-external cancellation/destruction.
    pub fn into_parts(self) -> (EpollKey, InterestMask, InterestMode, U, S) {
        (
            self.key,
            self.interest,
            self.mode,
            self.user_data,
            self.subscription,
        )
    }
}

/// Generation-tagged callback/ctl identity for one epoll interest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EpollToken {
    epoll: EpollId,
    slot: usize,
    generation: u64,
}

impl EpollToken {
    /// Returns the owning epoll instance.
    pub const fn epoll(self) -> EpollId {
        self.epoll
    }

    /// Returns the private slot index for adapter indexes.
    pub const fn slot(self) -> usize {
        self.slot
    }

    /// Returns the opaque generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// In-flight ready event retained across lock-free userspace copyout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryToken {
    interest: EpollToken,
    serial: u64,
    events: ReadyMask,
}

impl DeliveryToken {
    /// Returns the exact interest generation whose delivery is in flight.
    ///
    /// An adapter uses this identity after lock-free userspace copyout to find
    /// its non-authoritative source-control index, recheck level readiness,
    /// and rearm the retained source before finishing the core transaction.
    /// User data is intentionally not an identity because Linux permits
    /// duplicate `epoll_event.data` values.
    pub const fn interest(self) -> EpollToken {
        self.interest
    }
}

/// Stable candidate returned before an adapter prepares userspace event data.
///
/// Preparing this token never clones caller state or mutates a valid ready
/// entry. The adapter may release its IRQ-safe core lock, prepare an owned
/// event payload, then call [`EpollCore::commit_delivery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryPreparation {
    interest: EpollToken,
}

impl DeliveryPreparation {
    /// Returns the exact interest generation that must still be ready at
    /// commit.
    pub const fn interest(self) -> EpollToken {
        self.interest
    }
}

/// Ready event snapshot returned without retaining an epoll-core borrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyEvent<U> {
    /// Token required to finish copied or faulted delivery.
    pub delivery: DeliveryToken,
    /// Coalesced Linux readiness bits.
    pub events: ReadyMask,
    /// User data captured from the interest generation.
    pub user_data: U,
}

/// Result of userspace copyout and the post-copy level recheck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The event was copied. Level-triggered callers provide readiness after
    /// copy; edge-triggered callers normally pass `ReadyMask::EMPTY`.
    Copied {
        /// Readiness observed by the adapter's post-copy level check.
        still_ready: ReadyMask,
    },
    /// The adapter re-polled the queued item immediately before copyout and
    /// found no currently deliverable event.  The stale snapshot is discarded,
    /// but a wake which raced the recheck remains pending and one-shot state is
    /// not consumed because userspace observed no event.
    Suppressed,
    /// Copyout faulted; the same event must remain deliverable.
    Fault,
}

/// Wake callback publication result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// A new ready-queue item was published.
    Enqueued,
    /// Bits were coalesced into an already queued or in-flight item.
    Coalesced,
    /// The interest is disabled or no requested/unconditional bit is ready.
    Ignored,
}

/// Epoll-core operation failure before errno mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EpollError {
    /// Fallible fixed-capacity storage could not be reserved.
    NoMemory,
    /// Interest or ready capacity is exhausted.
    Capacity,
    /// The `(OFD, fd)` interest already exists.
    Duplicate,
    /// The requested interest does not exist.
    NotFound,
    /// A callback, ctl, or delivery token is stale or foreign.
    StaleToken,
    /// Trigger flags form an unsupported combination.
    UnsupportedMode,
    /// No future unique generation/serial can be allocated.
    GenerationExhausted,
    /// Ready storage violated its admitted one-item-per-interest invariant.
    ReadyQueueFull,
    /// A bounded rescan was requested after an unexpected queue overflow.
    RescanRequired,
}

/// Progress from one explicitly bounded epoll readiness rescan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescanProgress {
    /// Entry slots examined by this call.
    pub scanned: usize,
    /// Previously unqueued ready interests published by this call.
    pub enqueued: usize,
    /// Whether every entry was examined and the rescan request was cleared.
    pub complete: bool,
}

/// Failed ADD/MOD which returns the never-published replacement ownership.
pub struct EpollPublishError<U, S> {
    /// Typed epoll error.
    pub error: EpollError,
    /// Interest and retained subscription not published by the operation.
    pub interest: EpollInterest<U, S>,
}

/// Failed delivery commit which returns the adapter-prepared payload.
pub struct DeliveryCommitError<U> {
    /// Typed epoll-core error.
    pub error: EpollError,
    /// Payload that was never published into a ready event.
    pub user_data: U,
}

impl<U> core::fmt::Debug for DeliveryCommitError<U> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DeliveryCommitError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// Generation-tagged owner of one incremental defensive rescan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RescanToken {
    epoll: EpollId,
    generation: u64,
}

impl RescanToken {
    /// Returns the owning epoll instance.
    pub const fn epoll(self) -> EpollId {
        self.epoll
    }

    /// Returns the recovery generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

impl<U, S> core::fmt::Debug for EpollPublishError<U, S> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("EpollPublishError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

struct Entry<U, S> {
    generation: u64,
    interest: EpollInterest<U, S>,
    enabled: bool,
    queued: bool,
    in_delivery: Option<u64>,
    ready: ReadyMask,
    during_delivery: ReadyMask,
}

struct ReadyQueue {
    items: Vec<EpollToken>,
    head: usize,
    len: usize,
}

#[derive(Debug, Clone, Copy)]
struct RescanState {
    generation: u64,
    cursor: usize,
    remaining: usize,
}

impl ReadyQueue {
    fn try_new(capacity: usize, filler: EpollToken) -> Result<Self, EpollError> {
        let mut items = Vec::new();
        items
            .try_reserve_exact(capacity)
            .map_err(|_| EpollError::NoMemory)?;
        items.resize(capacity, filler);
        Ok(Self {
            items,
            head: 0,
            len: 0,
        })
    }

    fn capacity(&self) -> usize {
        self.items.len()
    }

    fn push(&mut self, token: EpollToken) -> Result<(), EpollError> {
        if self.len == self.capacity() {
            return Err(EpollError::ReadyQueueFull);
        }
        let index = (self.head + self.len) % self.capacity();
        self.items[index] = token;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<EpollToken> {
        if self.len == 0 {
            return None;
        }
        let token = self.items[self.head];
        self.head = (self.head + 1) % self.capacity();
        self.len -= 1;
        Some(token)
    }

    fn peek(&self) -> Option<EpollToken> {
        (self.len != 0).then(|| self.items[self.head])
    }

    fn remove(&mut self, token: EpollToken) {
        if self.len == 0 {
            return;
        }
        let capacity = self.capacity();
        let mut retained = 0;
        for read in 0..self.len {
            let item = self.items[(self.head + read) % capacity];
            if item != token {
                let write = (self.head + retained) % capacity;
                self.items[write] = item;
                retained += 1;
            }
        }
        self.len = retained;
    }
}

/// Bounded allocation-free-after-construction epoll interest/ready core.
///
/// The consumer protects it with its chosen short IRQ-safe lock. Methods never
/// call source registrations, wakers, destructors, or usercopy; removed and
/// replaced interests are returned for lock-external teardown.
pub struct EpollCore<U, S> {
    id: EpollId,
    entries: Vec<Option<Entry<U, S>>>,
    ready: ReadyQueue,
    next_generation: u64,
    next_delivery: u64,
    next_rescan: u64,
    rescan: Option<RescanState>,
}

impl<U, S> EpollCore<U, S> {
    /// Fallibly reserves all interest and ready storage up front.
    pub fn try_new(id: EpollId, capacity: usize) -> Result<Self, EpollError> {
        if capacity == usize::MAX {
            return Err(EpollError::Capacity);
        }
        let filler = EpollToken {
            epoll: id,
            slot: 0,
            generation: 0,
        };
        let ready = ReadyQueue::try_new(capacity, filler)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| EpollError::NoMemory)?;
        entries.resize_with(capacity, || None);
        Ok(Self {
            id,
            entries,
            ready,
            next_generation: 1,
            next_delivery: 1,
            next_rescan: 1,
            rescan: None,
        })
    }

    /// Returns the instance identity.
    pub const fn id(&self) -> EpollId {
        self.id
    }

    /// Returns admitted interest capacity.
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// Returns the number of published interests.
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }

    /// Returns whether no interest is published.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns whether an unexpected ready-queue overflow requested a bounded
    /// adapter-driven rescan.
    pub const fn needs_rescan(&self) -> bool {
        self.rescan.is_some()
    }

    /// Returns the current recovery token, if an invariant failure requested
    /// an incremental rescan.
    pub fn rescan_token(&self) -> Option<RescanToken> {
        self.rescan.map(|state| RescanToken {
            epoll: self.id,
            generation: state.generation,
        })
    }

    fn allocate_generation(&mut self) -> Result<u64, EpollError> {
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .ok_or(EpollError::GenerationExhausted)?;
        Ok(generation)
    }

    fn allocate_delivery(&mut self) -> Result<u64, EpollError> {
        let serial = self.next_delivery;
        self.next_delivery = serial
            .checked_add(1)
            .ok_or(EpollError::GenerationExhausted)?;
        Ok(serial)
    }

    fn start_rescan(&mut self) -> Result<(), EpollError> {
        let generation = self.next_rescan;
        self.next_rescan = generation
            .checked_add(1)
            .ok_or(EpollError::GenerationExhausted)?;
        self.rescan = Some(RescanState {
            generation,
            cursor: 0,
            remaining: self.entries.len(),
        });
        Ok(())
    }

    fn validate_mode(mode: InterestMode) -> Result<(), EpollError> {
        // Exclusive selection requires coordination across multiple epoll
        // instances at the source registry. This single-instance core cannot
        // honestly provide it until that lower-layer contract is supplied.
        if mode.exclusive {
            Err(EpollError::UnsupportedMode)
        } else {
            Ok(())
        }
    }

    fn entry(&self, token: EpollToken) -> Result<&Entry<U, S>, EpollError> {
        if token.epoll != self.id {
            return Err(EpollError::StaleToken);
        }
        match self.entries.get(token.slot).and_then(Option::as_ref) {
            Some(entry) if entry.generation == token.generation => Ok(entry),
            _ => Err(EpollError::StaleToken),
        }
    }

    fn entry_mut(&mut self, token: EpollToken) -> Result<&mut Entry<U, S>, EpollError> {
        if token.epoll != self.id {
            return Err(EpollError::StaleToken);
        }
        match self.entries.get_mut(token.slot).and_then(Option::as_mut) {
            Some(entry) if entry.generation == token.generation => Ok(entry),
            _ => Err(EpollError::StaleToken),
        }
    }

    /// Atomically publishes an already armed interest.
    pub fn add(
        &mut self,
        interest: EpollInterest<U, S>,
    ) -> Result<EpollToken, EpollPublishError<U, S>> {
        if let Err(error) = Self::validate_mode(interest.mode) {
            return Err(EpollPublishError { error, interest });
        }
        if self
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.interest.key == interest.key)
        {
            return Err(EpollPublishError {
                error: EpollError::Duplicate,
                interest,
            });
        }
        let Some(slot) = self.entries.iter().position(Option::is_none) else {
            return Err(EpollPublishError {
                error: EpollError::Capacity,
                interest,
            });
        };
        let generation = match self.allocate_generation() {
            Ok(generation) => generation,
            Err(error) => return Err(EpollPublishError { error, interest }),
        };
        self.entries[slot] = Some(Entry {
            generation,
            interest,
            enabled: true,
            queued: false,
            in_delivery: None,
            ready: ReadyMask::EMPTY,
            during_delivery: ReadyMask::EMPTY,
        });
        Ok(EpollToken {
            epoll: self.id,
            slot,
            generation,
        })
    }

    /// Replaces one exact interest generation and returns the old subscription
    /// owner for lock-external cancellation.
    pub fn modify(
        &mut self,
        token: EpollToken,
        replacement: EpollInterest<U, S>,
    ) -> Result<(EpollToken, EpollInterest<U, S>), EpollPublishError<U, S>> {
        if let Err(error) = Self::validate_mode(replacement.mode) {
            return Err(EpollPublishError {
                error,
                interest: replacement,
            });
        }
        let slot = token.slot;
        let key_matches = self
            .entry(token)
            .is_ok_and(|entry| entry.interest.key == replacement.key);
        if !key_matches {
            return Err(EpollPublishError {
                error: EpollError::StaleToken,
                interest: replacement,
            });
        }
        let generation = match self.allocate_generation() {
            Ok(generation) => generation,
            Err(error) => {
                return Err(EpollPublishError {
                    error,
                    interest: replacement,
                });
            }
        };
        self.ready.remove(token);
        let Some(old) = self.entries[slot].take() else {
            return Err(EpollPublishError {
                error: EpollError::StaleToken,
                interest: replacement,
            });
        };
        self.entries[slot] = Some(Entry {
            generation,
            interest: replacement,
            enabled: true,
            queued: false,
            in_delivery: None,
            ready: ReadyMask::EMPTY,
            during_delivery: ReadyMask::EMPTY,
        });
        Ok((
            EpollToken {
                epoll: self.id,
                slot,
                generation,
            },
            old.interest,
        ))
    }

    /// Deletes one exact interest and returns retained ownership.
    pub fn remove(&mut self, token: EpollToken) -> Result<EpollInterest<U, S>, EpollError> {
        self.entry(token)?;
        self.ready.remove(token);
        self.entries[token.slot]
            .take()
            .map(|entry| entry.interest)
            .ok_or(EpollError::StaleToken)
    }

    /// Publishes bounded readiness from a source callback.
    pub fn notify(
        &mut self,
        token: EpollToken,
        ready: ReadyMask,
    ) -> Result<NotifyOutcome, EpollError> {
        let entry = self.entry_mut(token)?;
        if !entry.enabled {
            return Ok(NotifyOutcome::Ignored);
        }
        let deliverable = ready.deliverable(entry.interest.interest);
        if deliverable.is_empty() {
            return Ok(NotifyOutcome::Ignored);
        }
        if entry.in_delivery.is_some() {
            entry.during_delivery |= deliverable;
            return Ok(NotifyOutcome::Coalesced);
        }
        entry.ready |= deliverable;
        if entry.queued {
            return Ok(NotifyOutcome::Coalesced);
        }
        if let Err(error) = self.ready.push(token) {
            self.start_rescan()?;
            return Err(error);
        }
        self.entry_mut(token)?.queued = true;
        Ok(NotifyOutcome::Enqueued)
    }

    /// Returns one exact ready candidate without cloning caller state.
    ///
    /// Stale queue records are removed defensively. A valid candidate remains
    /// queued until [`commit_delivery`](Self::commit_delivery) revalidates it,
    /// so an adapter can prepare arbitrary owned user data outside its
    /// IRQ-safe core lock.
    pub fn prepare_delivery(&mut self) -> Result<Option<DeliveryPreparation>, EpollError> {
        while let Some(token) = self.ready.peek() {
            let valid = self.entry(token).is_ok_and(|entry| entry.queued);
            if !valid {
                self.ready.pop();
                continue;
            }
            return Ok(Some(DeliveryPreparation { interest: token }));
        }
        if self.rescan.is_some() {
            Err(EpollError::RescanRequired)
        } else {
            Ok(None)
        }
    }

    /// Commits a prepared candidate and moves caller-prepared event data into
    /// the ready snapshot.
    ///
    /// No `Clone`, callback, allocation, or destructor runs in the core. A
    /// stale candidate returns `user_data` unchanged for lock-external cleanup.
    pub fn commit_delivery<V>(
        &mut self,
        preparation: DeliveryPreparation,
        user_data: V,
    ) -> Result<ReadyEvent<V>, DeliveryCommitError<V>> {
        let token = preparation.interest;
        let valid =
            self.ready.peek() == Some(token) && self.entry(token).is_ok_and(|entry| entry.queued);
        if !valid {
            return Err(DeliveryCommitError {
                error: EpollError::StaleToken,
                user_data,
            });
        }
        let serial = match self.allocate_delivery() {
            Ok(serial) => serial,
            Err(error) => return Err(DeliveryCommitError { error, user_data }),
        };
        self.ready.pop();
        let entry = match self.entry_mut(token) {
            Ok(entry) => entry,
            Err(error) => return Err(DeliveryCommitError { error, user_data }),
        };
        entry.queued = false;
        entry.in_delivery = Some(serial);
        let events = entry.ready;
        entry.ready = ReadyMask::EMPTY;
        Ok(ReadyEvent {
            delivery: DeliveryToken {
                interest: token,
                serial,
                events,
            },
            events,
            user_data,
        })
    }

    /// Convenience delivery for bitwise-copyable event data.
    ///
    /// Kernels with owned or fallible event payloads should use
    /// [`prepare_delivery`](Self::prepare_delivery) and
    /// [`commit_delivery`](Self::commit_delivery) explicitly.
    pub fn begin_delivery(&mut self) -> Result<Option<ReadyEvent<U>>, EpollError>
    where
        U: Copy,
    {
        let Some(preparation) = self.prepare_delivery()? else {
            return Ok(None);
        };
        let user_data = self
            .entry(preparation.interest)
            .map(|entry| entry.interest.user_data)?;
        self.commit_delivery(preparation, user_data)
            .map(Some)
            .map_err(|error| error.error)
    }

    /// Completes copyout, preserving faulted events and wakeups racing copy.
    pub fn finish_delivery(
        &mut self,
        delivery: DeliveryToken,
        outcome: DeliveryOutcome,
    ) -> Result<(), EpollError> {
        let token = delivery.interest;
        let should_enqueue = {
            let entry = self.entry_mut(token)?;
            if entry.in_delivery != Some(delivery.serial) {
                return Err(EpollError::StaleToken);
            }
            entry.in_delivery = None;
            match outcome {
                DeliveryOutcome::Fault => {
                    entry.ready |= delivery.events | entry.during_delivery;
                }
                DeliveryOutcome::Suppressed => {
                    entry.ready |= entry.during_delivery;
                }
                DeliveryOutcome::Copied { still_ready } => {
                    entry.ready |= entry.during_delivery;
                    if !entry.interest.mode.edge {
                        entry.ready |= still_ready.deliverable(entry.interest.interest);
                    }
                    if entry.interest.mode.one_shot {
                        entry.enabled = false;
                    }
                }
            }
            entry.during_delivery = ReadyMask::EMPTY;
            entry.enabled && !entry.ready.is_empty() && !entry.queued
        };
        if should_enqueue {
            if let Err(error) = self.ready.push(token) {
                self.start_rescan()?;
                return Err(error);
            }
            self.entry_mut(token)?.queued = true;
        }
        Ok(())
    }

    /// Examines at most `max_entries` slots after an unexpected queue overflow.
    ///
    /// Normal operation admits one ready item per interest, so this is a
    /// defensive recovery seam rather than a periodic scan path. A zero budget
    /// performs no work and never clears a pending request.
    pub fn rescan_ready(
        &mut self,
        token: RescanToken,
        max_entries: usize,
    ) -> Result<RescanProgress, EpollError> {
        let Some(mut state) = self.rescan else {
            return Err(EpollError::StaleToken);
        };
        if token.epoll != self.id || token.generation != state.generation {
            return Err(EpollError::StaleToken);
        }
        if self.entries.is_empty() || state.remaining == 0 {
            self.rescan = None;
            return Ok(RescanProgress {
                scanned: 0,
                enqueued: 0,
                complete: true,
            });
        }
        if max_entries == 0 {
            return Ok(RescanProgress {
                scanned: 0,
                enqueued: 0,
                complete: false,
            });
        }

        let target = max_entries.min(state.remaining);
        let mut scanned = 0usize;
        let mut enqueued = 0usize;
        while scanned < target {
            let slot = state.cursor;
            let ready_token = self.entries[slot].as_ref().and_then(|entry| {
                (entry.enabled
                    && !entry.ready.is_empty()
                    && !entry.queued
                    && entry.in_delivery.is_none())
                .then_some(EpollToken {
                    epoll: self.id,
                    slot,
                    generation: entry.generation,
                })
            });
            if let Some(ready_token) = ready_token {
                if let Err(error) = self.ready.push(ready_token) {
                    self.rescan = Some(state);
                    return Err(error);
                }
                self.entry_mut(ready_token)?.queued = true;
                enqueued += 1;
            }

            state.cursor = (state.cursor + 1) % self.entries.len();
            state.remaining -= 1;
            scanned += 1;
        }

        let complete = state.remaining == 0;
        self.rescan = if complete { None } else { Some(state) };
        Ok(RescanProgress {
            scanned,
            enqueued,
            complete,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Subscription(Arc<AtomicUsize>);

    impl Drop for Subscription {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct OwnedPayload(u64);

    fn id(raw: u64) -> EpollId {
        EpollId::new(raw).unwrap()
    }

    fn ofd(raw: u64) -> OfdId {
        OfdId::new(raw).unwrap()
    }

    fn interest(
        key: EpollKey,
        mode: InterestMode,
        drops: &Arc<AtomicUsize>,
    ) -> EpollInterest<u64, Subscription> {
        EpollInterest::new(key, InterestMask::IN, mode, 99, Subscription(drops.clone()))
    }

    #[test]
    fn duplicate_and_capacity_failures_return_subscription_ownership() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        core.add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        let duplicate = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap_err();
        assert_eq!(duplicate.error, EpollError::Duplicate);
        drop(duplicate.interest);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exclusive_mode_is_rejected_until_a_cross_epoll_selector_is_supplied() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let error = core
            .add(interest(
                key,
                InterestMode {
                    exclusive: true,
                    ..InterestMode::default()
                },
                &drops,
            ))
            .unwrap_err();
        assert_eq!(error.error, EpollError::UnsupportedMode);
        drop(error.interest);
        assert!(core.is_empty());
    }

    #[test]
    fn level_delivery_requeues_only_after_post_copy_recheck() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Ok(NotifyOutcome::Enqueued)
        );
        let event = core.begin_delivery().unwrap().unwrap();
        assert_eq!(event.events, ReadyMask::IN);
        assert_eq!(event.delivery.interest(), token);
        core.finish_delivery(
            event.delivery,
            DeliveryOutcome::Copied {
                still_ready: ReadyMask::IN,
            },
        )
        .unwrap();
        assert!(core.begin_delivery().unwrap().is_some());
    }

    #[test]
    fn suppressed_delivery_preserves_racing_wake_without_consuming_one_shot() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(
                key,
                InterestMode {
                    one_shot: true,
                    ..InterestMode::default()
                },
                &drops,
            ))
            .unwrap();

        core.notify(token, ReadyMask::IN).unwrap();
        let stale = core.begin_delivery().unwrap().unwrap();
        core.finish_delivery(stale.delivery, DeliveryOutcome::Suppressed)
            .unwrap();
        assert!(core.begin_delivery().unwrap().is_none());
        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Ok(NotifyOutcome::Enqueued)
        );

        let racing = core.begin_delivery().unwrap().unwrap();
        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Ok(NotifyOutcome::Coalesced)
        );
        core.finish_delivery(racing.delivery, DeliveryOutcome::Suppressed)
            .unwrap();
        assert_eq!(
            core.begin_delivery().unwrap().unwrap().events,
            ReadyMask::IN
        );
    }

    #[test]
    fn wake_during_edge_copyout_is_not_lost() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(
                key,
                InterestMode {
                    edge: true,
                    ..InterestMode::default()
                },
                &drops,
            ))
            .unwrap();
        core.notify(token, ReadyMask::IN).unwrap();
        let event = core.begin_delivery().unwrap().unwrap();
        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Ok(NotifyOutcome::Coalesced)
        );
        core.finish_delivery(
            event.delivery,
            DeliveryOutcome::Copied {
                still_ready: ReadyMask::EMPTY,
            },
        )
        .unwrap();
        assert!(core.begin_delivery().unwrap().is_some());
    }

    #[test]
    fn copyout_fault_requeues_the_same_event() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();

        core.notify(token, ReadyMask::IN).unwrap();
        let event = core.begin_delivery().unwrap().unwrap();
        assert_eq!(event.events, ReadyMask::IN);
        core.finish_delivery(event.delivery, DeliveryOutcome::Fault)
            .unwrap();

        let retried = core.begin_delivery().unwrap().unwrap();
        assert_eq!(retried.events, ReadyMask::IN);
    }

    #[test]
    fn copyout_fault_merges_a_concurrent_wake() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(EpollInterest::new(
                key,
                InterestMask::IN | InterestMask::OUT,
                InterestMode::default(),
                99,
                Subscription(drops.clone()),
            ))
            .unwrap();

        core.notify(token, ReadyMask::IN).unwrap();
        let event = core.begin_delivery().unwrap().unwrap();
        assert_eq!(
            core.notify(token, ReadyMask::OUT),
            Ok(NotifyOutcome::Coalesced)
        );
        core.finish_delivery(event.delivery, DeliveryOutcome::Fault)
            .unwrap();

        let retried = core.begin_delivery().unwrap().unwrap();
        assert_eq!(retried.events, ReadyMask::IN | ReadyMask::OUT);
    }

    #[test]
    fn remove_and_modify_invalidate_in_flight_delivery_tokens() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut removed = EpollCore::try_new(id(1), 1).unwrap();
        let removed_token = removed
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        removed.notify(removed_token, ReadyMask::IN).unwrap();
        let old_delivery = removed.begin_delivery().unwrap().unwrap().delivery;
        drop(removed.remove(removed_token).unwrap());
        assert_eq!(
            removed.finish_delivery(old_delivery, DeliveryOutcome::Fault),
            Err(EpollError::StaleToken)
        );

        let mut modified = EpollCore::try_new(id(2), 1).unwrap();
        let modified_token = modified
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        modified.notify(modified_token, ReadyMask::IN).unwrap();
        let old_delivery = modified.begin_delivery().unwrap().unwrap().delivery;
        let (_new_token, old) = modified
            .modify(
                modified_token,
                interest(key, InterestMode::default(), &drops),
            )
            .unwrap();
        drop(old);
        assert_eq!(
            modified.finish_delivery(old_delivery, DeliveryOutcome::Fault),
            Err(EpollError::StaleToken)
        );
    }

    #[test]
    fn ready_queue_holds_at_most_one_item_per_interest() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut core = EpollCore::try_new(id(1), 2).unwrap();
        let first = core
            .add(interest(
                EpollKey {
                    ofd: ofd(1),
                    fd: FdNumber::new(3),
                },
                InterestMode::default(),
                &drops,
            ))
            .unwrap();
        let second = core
            .add(interest(
                EpollKey {
                    ofd: ofd(2),
                    fd: FdNumber::new(4),
                },
                InterestMode::default(),
                &drops,
            ))
            .unwrap();

        assert_eq!(
            core.notify(first, ReadyMask::IN),
            Ok(NotifyOutcome::Enqueued)
        );
        assert_eq!(
            core.notify(first, ReadyMask::IN),
            Ok(NotifyOutcome::Coalesced)
        );
        assert_eq!(
            core.notify(second, ReadyMask::IN),
            Ok(NotifyOutcome::Enqueued)
        );
        assert_eq!(core.ready.len, 2);
    }

    #[test]
    fn zero_capacity_is_finite_and_rejects_publication() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 0).unwrap();
        assert_eq!(core.capacity(), 0);
        let error = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap_err();
        assert_eq!(error.error, EpollError::Capacity);
        drop(error.interest);
    }

    #[test]
    fn delivery_generation_exhaustion_leaves_the_ready_item_queued() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        core.notify(token, ReadyMask::IN).unwrap();
        core.next_delivery = u64::MAX;

        assert_eq!(core.begin_delivery(), Err(EpollError::GenerationExhausted));
        assert_eq!(core.ready.len, 1);
        assert!(core.entry(token).unwrap().queued);
    }

    #[test]
    fn externally_prepared_non_clone_payload_commits_without_core_cloning() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        core.notify(token, ReadyMask::IN).unwrap();

        let preparation = core.prepare_delivery().unwrap().unwrap();
        assert_eq!(preparation.interest(), token);
        let event = core.commit_delivery(preparation, OwnedPayload(7)).unwrap();

        assert_eq!(event.events, ReadyMask::IN);
        assert_eq!(event.user_data, OwnedPayload(7));
    }

    #[test]
    fn stale_preparations_return_unpublished_payload_ownership() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };

        let mut removed = EpollCore::try_new(id(1), 1).unwrap();
        let removed_token = removed
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        removed.notify(removed_token, ReadyMask::IN).unwrap();
        let removed_preparation = removed.prepare_delivery().unwrap().unwrap();
        drop(removed.remove(removed_token).unwrap());
        let error = removed
            .commit_delivery(removed_preparation, OwnedPayload(11))
            .unwrap_err();
        assert_eq!(error.error, EpollError::StaleToken);
        assert_eq!(error.user_data, OwnedPayload(11));

        let mut modified = EpollCore::try_new(id(2), 1).unwrap();
        let modified_token = modified
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        modified.notify(modified_token, ReadyMask::IN).unwrap();
        let modified_preparation = modified.prepare_delivery().unwrap().unwrap();
        let (_, old) = modified
            .modify(
                modified_token,
                interest(key, InterestMode::default(), &drops),
            )
            .unwrap();
        drop(old);
        let error = modified
            .commit_delivery(modified_preparation, OwnedPayload(13))
            .unwrap_err();
        assert_eq!(error.error, EpollError::StaleToken);
        assert_eq!(error.user_data, OwnedPayload(13));
    }

    #[test]
    fn delivery_exhaustion_returns_payload_and_preserves_queue_item() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        core.notify(token, ReadyMask::IN).unwrap();
        let preparation = core.prepare_delivery().unwrap().unwrap();
        core.next_delivery = u64::MAX;

        let error = core
            .commit_delivery(preparation, OwnedPayload(17))
            .unwrap_err();
        assert_eq!(error.error, EpollError::GenerationExhausted);
        assert_eq!(error.user_data, OwnedPayload(17));
        assert_eq!(core.ready.len, 1);
        assert!(core.entry(token).unwrap().queued);
    }

    #[test]
    fn unexpected_queue_overflow_requests_only_an_explicit_bounded_rescan() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        let stale = EpollToken {
            epoll: id(99),
            slot: 0,
            generation: 1,
        };
        core.ready.push(stale).unwrap();

        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Err(EpollError::ReadyQueueFull)
        );
        assert!(core.needs_rescan());
        assert_eq!(core.begin_delivery(), Err(EpollError::RescanRequired));
        let rescan = core.rescan_token().unwrap();
        assert_eq!(
            core.rescan_ready(rescan, 0).unwrap(),
            RescanProgress {
                scanned: 0,
                enqueued: 0,
                complete: false,
            }
        );
        assert_eq!(
            core.rescan_ready(rescan, 1).unwrap(),
            RescanProgress {
                scanned: 1,
                enqueued: 1,
                complete: true,
            }
        );
        assert_eq!(
            core.begin_delivery().unwrap().unwrap().events,
            ReadyMask::IN
        );
    }

    #[test]
    fn bounded_rescan_persists_progress_until_every_slot_is_examined() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut core = EpollCore::try_new(id(1), 3).unwrap();
        let tokens = [
            core.add(interest(
                EpollKey {
                    ofd: ofd(1),
                    fd: FdNumber::new(3),
                },
                InterestMode::default(),
                &drops,
            ))
            .unwrap(),
            core.add(interest(
                EpollKey {
                    ofd: ofd(2),
                    fd: FdNumber::new(4),
                },
                InterestMode::default(),
                &drops,
            ))
            .unwrap(),
            core.add(interest(
                EpollKey {
                    ofd: ofd(3),
                    fd: FdNumber::new(5),
                },
                InterestMode::default(),
                &drops,
            ))
            .unwrap(),
        ];
        let stale = EpollToken {
            epoll: id(99),
            slot: 0,
            generation: 1,
        };
        for _ in 0..3 {
            core.ready.push(stale).unwrap();
        }
        for token in tokens {
            assert_eq!(
                core.notify(token, ReadyMask::IN),
                Err(EpollError::ReadyQueueFull)
            );
        }
        assert_eq!(core.prepare_delivery(), Err(EpollError::RescanRequired));
        let rescan = core.rescan_token().unwrap();

        assert_eq!(
            core.rescan_ready(rescan, 1).unwrap(),
            RescanProgress {
                scanned: 1,
                enqueued: 1,
                complete: false,
            }
        );
        assert_eq!(
            core.rescan_ready(rescan, 1).unwrap(),
            RescanProgress {
                scanned: 1,
                enqueued: 1,
                complete: false,
            }
        );
        assert_eq!(
            core.rescan_ready(rescan, 1).unwrap(),
            RescanProgress {
                scanned: 1,
                enqueued: 1,
                complete: true,
            }
        );
        assert!(!core.needs_rescan());
        assert_eq!(core.ready.len, 3);
    }

    #[test]
    fn rescan_zero_budget_does_not_advance_and_queue_full_retries_same_slot() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(key, InterestMode::default(), &drops))
            .unwrap();
        let stale = EpollToken {
            epoll: id(99),
            slot: 0,
            generation: 1,
        };
        core.ready.push(stale).unwrap();
        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Err(EpollError::ReadyQueueFull)
        );
        let rescan = core.rescan_token().unwrap();

        assert_eq!(
            core.rescan_ready(rescan, 0).unwrap(),
            RescanProgress {
                scanned: 0,
                enqueued: 0,
                complete: false,
            }
        );
        assert_eq!(
            core.rescan_ready(rescan, 1),
            Err(EpollError::ReadyQueueFull)
        );
        assert_eq!(core.prepare_delivery(), Err(EpollError::RescanRequired));
        assert_eq!(
            core.rescan_ready(rescan, 1).unwrap(),
            RescanProgress {
                scanned: 1,
                enqueued: 1,
                complete: true,
            }
        );
    }

    #[test]
    fn new_overflow_restarts_rescan_and_stales_the_old_token() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut core = EpollCore::try_new(id(1), 2).unwrap();
        let first = core
            .add(interest(
                EpollKey {
                    ofd: ofd(1),
                    fd: FdNumber::new(3),
                },
                InterestMode::default(),
                &drops,
            ))
            .unwrap();
        let second = core
            .add(interest(
                EpollKey {
                    ofd: ofd(2),
                    fd: FdNumber::new(4),
                },
                InterestMode::default(),
                &drops,
            ))
            .unwrap();
        let stale = EpollToken {
            epoll: id(99),
            slot: 0,
            generation: 1,
        };
        core.ready.push(stale).unwrap();
        core.ready.push(stale).unwrap();

        assert_eq!(
            core.notify(first, ReadyMask::IN),
            Err(EpollError::ReadyQueueFull)
        );
        let old = core.rescan_token().unwrap();
        assert_eq!(
            core.notify(second, ReadyMask::IN),
            Err(EpollError::ReadyQueueFull)
        );
        let current = core.rescan_token().unwrap();
        assert_ne!(old, current);
        assert_eq!(core.rescan_ready(old, 1), Err(EpollError::StaleToken));

        assert_eq!(core.prepare_delivery(), Err(EpollError::RescanRequired));
        assert_eq!(
            core.rescan_ready(current, 2).unwrap(),
            RescanProgress {
                scanned: 2,
                enqueued: 2,
                complete: true,
            }
        );
    }

    #[test]
    fn one_shot_disables_until_modify_and_old_tokens_go_stale() {
        let drops = Arc::new(AtomicUsize::new(0));
        let key = EpollKey {
            ofd: ofd(1),
            fd: FdNumber::new(3),
        };
        let mut core = EpollCore::try_new(id(1), 1).unwrap();
        let token = core
            .add(interest(
                key,
                InterestMode {
                    one_shot: true,
                    ..InterestMode::default()
                },
                &drops,
            ))
            .unwrap();
        core.notify(token, ReadyMask::IN).unwrap();
        let event = core.begin_delivery().unwrap().unwrap();
        core.finish_delivery(
            event.delivery,
            DeliveryOutcome::Copied {
                still_ready: ReadyMask::IN,
            },
        )
        .unwrap();
        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Ok(NotifyOutcome::Ignored)
        );

        let (rearmed, old) = core
            .modify(token, interest(key, InterestMode::default(), &drops))
            .unwrap();
        drop(old);
        assert_eq!(
            core.notify(token, ReadyMask::IN),
            Err(EpollError::StaleToken)
        );
        assert_eq!(
            core.notify(rearmed, ReadyMask::IN),
            Ok(NotifyOutcome::Enqueued)
        );
    }
}
