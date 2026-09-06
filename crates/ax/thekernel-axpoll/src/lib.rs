//! Bounded I/O readiness registration and wakeup primitives.
//!
//! This crate provides generic mechanism rather than Linux ABI policy. Event
//! flags have crate-owned values; a Linux personality must translate `POLL*`
//! values at its ABI boundary.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::{sync::Arc, task::Wake, vec::Vec};
use core::{
    convert::Infallible,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use bitflags::bitflags;
use kspin::SpinNoIrq as Mutex;

fn try_update_usize<F>(
    atomic: &AtomicUsize,
    set: Ordering,
    fail: Ordering,
    mut update: F,
) -> Result<usize, usize>
where
    F: FnMut(usize) -> Option<usize>,
{
    let mut current = atomic.load(fail);
    loop {
        let Some(next) = update(current) else {
            return Err(current);
        };
        match atomic.compare_exchange_weak(current, next, set, fail) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

bitflags! {
    /// Generic I/O readiness events.
    ///
    /// The numeric values are owned by this crate and are not Linux `POLL*`
    /// constants. ABI-facing callers must translate in both directions.
    #[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash)]
    pub struct IoEvents: u32 {
        /// Data can be read without blocking.
        const READABLE     = 1 << 0;
        /// High-priority data can be read without blocking.
        const PRIORITY     = 1 << 1;
        /// Data can be written without blocking.
        const WRITABLE     = 1 << 2;
        /// An asynchronous error is pending.
        const ERROR        = 1 << 3;
        /// The peer or underlying object has hung up.
        const HANGUP       = 1 << 4;
        /// The requested object or operation is invalid.
        const INVALID      = 1 << 5;
        /// Normal-priority data can be read.
        const READ_NORMAL  = 1 << 6;
        /// Priority-band data can be read.
        const READ_BAND    = 1 << 7;
        /// Normal-priority data can be written.
        const WRITE_NORMAL = 1 << 8;
        /// Priority-band data can be written.
        const WRITE_BAND   = 1 << 9;
        /// A message is available.
        const MESSAGE      = 1 << 10;
        /// The monitored object was removed.
        const REMOVED      = 1 << 11;
        /// The peer closed, or shut down, its writing half.
        const READ_HANGUP  = 1 << 12;

        /// Conditions that should be observed even when not requested.
        const ALWAYS = Self::ERROR.bits() | Self::HANGUP.bits();
    }
}

/// The default number of simultaneous registrations in a [`PollSet`].
pub const DEFAULT_CAPACITY: usize = 64;

/// Generic object that can report its current readiness state.
///
/// This is deliberately independent of file descriptors, filesystems, and
/// operating-system ABI event encodings. Product adapters decide how an
/// object exposes registrations and how an ABI maps [`IoEvents`].
pub trait ReadinessSource {
    /// Returns the object's current generic readiness state.
    fn readiness(&self) -> IoEvents;
}

/// Generic provider of one or more readiness sources.
///
/// A provider is useful for adapters whose one logical wait fans in to
/// several independently registered objects. Registration ownership and
/// cancellation remain adapter policy, so this trait only exposes the source
/// topology.
pub trait ReadinessProvider {
    /// The source type yielded by this provider.
    type Source: ReadinessSource + ?Sized;

    /// Calls `visit` once for every source relevant to a requested event set.
    fn for_each_source(&self, events: IoEvents, visit: &mut dyn FnMut(&Self::Source));
}

/// Hard system-wide ceiling for live aggregate readiness registrations.
///
/// A registration is also limited by each source [`PollSet`] capacity. This
/// account bounds the total retained source topology across all aggregates.
pub const MAX_LIVE_SOURCE_REGISTRATIONS: usize = 65_536;

static LIVE_SOURCE_REGISTRATIONS: AtomicUsize = AtomicUsize::new(0);

/// Returns the number of retained readiness source registrations.
pub fn live_registration_charges() -> usize {
    LIVE_SOURCE_REGISTRATIONS.load(Ordering::Acquire)
}

fn charge_sources(amount: usize) -> Result<(), PollRegistrationError> {
    try_update_usize(
        &LIVE_SOURCE_REGISTRATIONS,
        Ordering::AcqRel,
        Ordering::Acquire,
        |used| {
            used.checked_add(amount)
                .filter(|next| *next <= MAX_LIVE_SOURCE_REGISTRATIONS)
        },
    )
    .map(|_| ())
    .map_err(|_| PollRegistrationError::Quota)
}

fn refund_sources(amount: usize) {
    LIVE_SOURCE_REGISTRATIONS.fetch_sub(amount, Ordering::AcqRel);
}

/// An opaque handle to one live [`PollSet`] registration.
///
/// Tokens are bound to a registry, slot, and generation. Consequently, a token
/// from another registry or from an earlier use of the same slot cannot cancel
/// or update the current registration.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct RegistrationToken {
    registry_id: usize,
    slot: usize,
    generation: usize,
}

impl RegistrationToken {
    const fn new(registry_id: usize, slot: usize, generation: usize) -> Self {
        Self {
            registry_id,
            slot,
            generation,
        }
    }
}

/// Failure returned by [`PollSet::register`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RegisterError {
    /// Every bounded registration slot is occupied.
    Full,
    /// The registry was closed and accepts no new registrations.
    Closed,
    /// The registry or slot generation identifier space was exhausted.
    TokenSpaceExhausted,
}

impl fmt::Display for RegisterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("poll registration set is full"),
            Self::Closed => formatter.write_str("poll registration set is closed"),
            Self::TokenSpaceExhausted => {
                formatter.write_str("poll registration token space is exhausted")
            }
        }
    }
}

/// Failure returned by [`PollSet::update`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UpdateError {
    /// The registry was closed and no registration can be updated.
    Closed,
    /// The token belongs to another registry or no longer names a live slot.
    InvalidToken,
}

/// An owned waker prepared before a source registration is published.
///
/// Composite waiters can prepare all source wakers and reserve their own token
/// storage before arming the first source. This type does not pretend that one
/// token represents a multi-source wait; every successful [`PollSet::arm`]
/// still returns one independent [`RegistrationToken`].
#[must_use = "a prepared registration has not been armed"]
pub struct PreparedRegistration {
    waker: Waker,
}

impl PreparedRegistration {
    /// Clones a waker before any poll-set lock or source slot is acquired.
    pub fn new(waker: &Waker) -> Self {
        Self {
            waker: waker.clone(),
        }
    }
}

/// Failure to arm one previously prepared source registration.
///
/// The rejected preparation is returned intact so an aggregate owner can
/// release it outside source locks while rolling back earlier tokens.
pub struct ArmRegistrationError {
    kind: RegisterError,
    prepared: PreparedRegistration,
}

impl ArmRegistrationError {
    /// Returns the typed source-admission failure.
    pub const fn kind(&self) -> RegisterError {
        self.kind
    }

    /// Recovers ownership of the unarmed waker.
    pub fn into_prepared(self) -> PreparedRegistration {
        self.prepared
    }
}

impl fmt::Debug for ArmRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArmRegistrationError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ArmRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(formatter)
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("poll registration set is closed"),
            Self::InvalidToken => formatter.write_str("poll registration token is invalid"),
        }
    }
}

static NEXT_REGISTRY_ID: AtomicUsize = AtomicUsize::new(1);

fn allocate_registry_id() -> Option<usize> {
    try_update_usize(
        &NEXT_REGISTRY_ID,
        Ordering::Relaxed,
        Ordering::Relaxed,
        |current| current.checked_add(1),
    )
    .ok()
}

struct Slot {
    generation: usize,
    waker: Option<Waker>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            generation: 0,
            waker: None,
        }
    }
}

struct Inner<const CAPACITY: usize> {
    entries: [Slot; CAPACITY],
    registry_id: usize,
    next: usize,
    len: usize,
    closed: bool,
}

impl<const CAPACITY: usize> Inner<CAPACITY> {
    const fn new() -> Self {
        Self {
            entries: [const { Slot::new() }; CAPACITY],
            registry_id: 0,
            next: 0,
            len: 0,
            closed: false,
        }
    }

    fn arm(&mut self, owned: Waker) -> Result<RegistrationToken, (RegisterError, Waker)> {
        if self.closed {
            return Err((RegisterError::Closed, owned));
        }

        if self.len == CAPACITY {
            return Err((RegisterError::Full, owned));
        }

        let Some(slot) = (self.next..CAPACITY).chain(0..self.next).find(|&slot| {
            self.entries[slot].waker.is_none() && self.entries[slot].generation < usize::MAX
        }) else {
            return Err((RegisterError::TokenSpaceExhausted, owned));
        };

        let registry_id = if self.registry_id == 0 {
            let Some(registry_id) = allocate_registry_id() else {
                return Err((RegisterError::TokenSpaceExhausted, owned));
            };
            self.registry_id = registry_id;
            registry_id
        } else {
            self.registry_id
        };

        let entry = &mut self.entries[slot];
        entry.generation += 1;
        entry.waker = Some(owned);
        self.len += 1;
        self.next = if slot + 1 == CAPACITY { 0 } else { slot + 1 };

        Ok(RegistrationToken::new(registry_id, slot, entry.generation))
    }

    fn update(
        &mut self,
        token: RegistrationToken,
        candidate: &Waker,
        owned: Waker,
    ) -> (Result<(), UpdateError>, Option<Waker>) {
        if self.closed {
            return (Err(UpdateError::Closed), Some(owned));
        }
        if token.registry_id == 0 || token.registry_id != self.registry_id {
            return (Err(UpdateError::InvalidToken), Some(owned));
        }

        let Some(entry) = self.entries.get_mut(token.slot) else {
            return (Err(UpdateError::InvalidToken), Some(owned));
        };
        if entry.generation != token.generation || entry.waker.is_none() {
            return (Err(UpdateError::InvalidToken), Some(owned));
        }

        if entry
            .waker
            .as_ref()
            .is_some_and(|registered| registered.will_wake(candidate))
        {
            return (Ok(()), Some(owned));
        }

        let replaced = entry.waker.replace(owned);
        (Ok(()), replaced)
    }

    fn cancel(&mut self, token: RegistrationToken) -> Option<Waker> {
        if token.registry_id == 0 || token.registry_id != self.registry_id {
            return None;
        }

        let entry = self.entries.get_mut(token.slot)?;
        if entry.generation != token.generation {
            return None;
        }

        let removed = entry.waker.take();
        if removed.is_some() {
            self.len -= 1;
            self.next = token.slot;
        }
        removed
    }

    fn drain(&mut self, pending: &mut [Option<Waker>; CAPACITY]) -> usize {
        let len = self.len;
        for (destination, entry) in pending.iter_mut().zip(&mut self.entries) {
            *destination = entry.waker.take();
        }
        self.next = 0;
        self.len = 0;
        len
    }
}

/// A bounded registry for tasks waiting on I/O readiness.
///
/// `CAPACITY` is a hard upper bound: registration never allocates a growing
/// collection and never silently overwrites an existing waiter. `wake()` drains
/// the current registrations but leaves the set open. `close()` drains and
/// wakes them while permanently rejecting future registration.
///
/// Registration, update, cancellation, wake, and close races are linearized by
/// a short IRQ-safe lock. Waker clone, destruction, and wake callbacks occur
/// outside that lock so custom RawWaker implementations may safely re-enter the
/// registry.
pub struct PollSet<const CAPACITY: usize = DEFAULT_CAPACITY>(Mutex<Inner<CAPACITY>>);

impl<const CAPACITY: usize> Default for PollSet<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> PollSet<CAPACITY> {
    /// Creates an empty, open registry.
    pub const fn new() -> Self {
        Self(Mutex::new(Inner::new()))
    }

    /// Returns the compile-time registration capacity.
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    /// Returns the number of live registrations.
    pub fn len(&self) -> usize {
        self.0.lock().len
    }

    /// Returns `true` when there are no live registrations.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` after the registry has been closed.
    pub fn is_closed(&self) -> bool {
        self.0.lock().closed
    }

    /// Prepares a source registration without acquiring a source slot.
    ///
    /// Aggregate owners should first reserve their bounded token storage, then
    /// prepare every source waker, and only then call [`Self::arm`] per source.
    pub fn prepare(&self, waker: &Waker) -> PreparedRegistration {
        PreparedRegistration::new(waker)
    }

    /// Arms one prepared source and returns its opaque cancellation token.
    ///
    /// Capacity, closure, or token-space failure returns the preparation to the
    /// caller without replacing or waking an existing registration.
    pub fn arm(
        &self,
        prepared: PreparedRegistration,
    ) -> Result<RegistrationToken, ArmRegistrationError> {
        match self.0.lock().arm(prepared.waker) {
            Ok(token) => Ok(token),
            Err((kind, waker)) => Err(ArmRegistrationError {
                kind,
                prepared: PreparedRegistration { waker },
            }),
        }
    }

    /// Registers a waker and returns its opaque cancellation token.
    ///
    /// Every call creates an independent registration, even if another slot has
    /// an equivalent waker. A full registry returns [`RegisterError::Full`]
    /// without replacing or waking another waiter. A logical waiter that is
    /// polled again must retain its token and call [`Self::update`] instead of
    /// registering a second time.
    pub fn register(&self, waker: &Waker) -> Result<RegistrationToken, RegisterError> {
        self.arm(self.prepare(waker)).map_err(|error| error.kind())
    }

    /// Replaces the waker associated with a live token.
    ///
    /// The token and its generation remain unchanged. The replaced or rejected
    /// waker is destroyed after the registry lock is released.
    pub fn update(&self, token: RegistrationToken, waker: &Waker) -> Result<(), UpdateError> {
        let owned = waker.clone();
        let (result, deferred_drop) = self.0.lock().update(token, waker, owned);
        drop(deferred_drop);
        result
    }

    /// Cancels a live registration.
    ///
    /// Returns `false` for stale tokens, foreign-registry tokens, and tokens
    /// already consumed by another cancel, wake, or close operation.
    pub fn cancel(&self, token: RegistrationToken) -> bool {
        let removed = self.0.lock().cancel(token);
        let cancelled = removed.is_some();
        drop(removed);
        cancelled
    }

    /// Drains and wakes all current registrations while leaving the set open.
    ///
    /// Returns the number of registrations consumed by this operation. Every
    /// callback runs after the registry lock has been released.
    pub fn wake(&self) -> usize {
        let mut pending = [const { None }; CAPACITY];
        let len = self.0.lock().drain(&mut pending);
        for waker in pending.into_iter().flatten() {
            waker.wake();
        }
        len
    }

    /// Permanently closes the registry, drains it, and wakes its waiters.
    ///
    /// The state transition and drain are atomic with respect to registration,
    /// update, cancellation, and wake. Repeated calls are harmless and return
    /// zero after the first close.
    pub fn close(&self) -> usize {
        let mut pending = [const { None }; CAPACITY];
        let len = {
            let mut inner = self.0.lock();
            if inner.closed {
                return 0;
            }
            inner.closed = true;
            inner.drain(&mut pending)
        };
        for waker in pending.into_iter().flatten() {
            waker.wake();
        }
        len
    }
}

impl<const CAPACITY: usize> Drop for PollSet<CAPACITY> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<const CAPACITY: usize> Wake for PollSet<CAPACITY> {
    fn wake(self: Arc<Self>) {
        self.as_ref().wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.as_ref().wake();
    }
}

trait ErasedPollSet: Send + Sync {
    fn update(&self, token: RegistrationToken, waker: &Waker) -> Result<(), UpdateError>;
    fn cancel(&self, token: RegistrationToken) -> bool;
}

impl<const CAPACITY: usize> ErasedPollSet for PollSet<CAPACITY> {
    fn update(&self, token: RegistrationToken, waker: &Waker) -> Result<(), UpdateError> {
        Self::update(self, token, waker)
    }

    fn cancel(&self, token: RegistrationToken) -> bool {
        Self::cancel(self, token)
    }
}

struct SourceRegistration<'a> {
    source: &'a dyn ErasedPollSet,
    token: Option<RegistrationToken>,
}

struct OwnedSourceRegistration {
    source: Arc<dyn ErasedPollSet>,
    token: Option<RegistrationToken>,
}

enum RegistrationOwner<'a> {
    Source(SourceRegistration<'a>),
    OwnedSource(OwnedSourceRegistration),
    Aggregate(PollRegistration<'a>),
}

impl RegistrationOwner<'_> {
    fn update(&mut self, waker: &Waker) -> Result<(), RegistrationUpdateError> {
        let (source, token) = match self {
            Self::Source(source) => (source.source, &mut source.token),
            Self::OwnedSource(source) => (&*source.source, &mut source.token),
            Self::Aggregate(aggregate) => {
                return aggregate
                    .update(waker)
                    .map_err(|_| RegistrationUpdateError::Nested);
            }
        };
        let token = token.ok_or(RegistrationUpdateError::Source(UpdateError::InvalidToken))?;
        source
            .update(token, waker)
            .map_err(RegistrationUpdateError::Source)
    }

    fn cancel(&mut self) -> Result<(), Infallible> {
        match self {
            Self::Source(source) => {
                if let Some(token) = source.token.take() {
                    source.source.cancel(token);
                }
            }
            Self::OwnedSource(source) => {
                if let Some(token) = source.token.take() {
                    source.source.cancel(token);
                }
            }
            Self::Aggregate(aggregate) => aggregate.cancel(),
        }
        Ok(())
    }
}

/// Failure while updating a retained object-readiness subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistrationUpdateError {
    /// A direct source token was closed or invalidated.
    Source(UpdateError),
    /// A nested object subscription contained an invalidated source.
    Nested,
}

/// Aggregate operation failure identifying the first source that failed.
#[derive(Debug, PartialEq, Eq)]
pub struct AggregateError<E> {
    /// Source index in arming order.
    pub index: usize,
    /// Source-specific typed error.
    pub error: E,
}

/// Error category preserved from a nested readiness registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NestedRegistrationError {
    /// The nested finite source account was exhausted.
    Quota,
    /// The nested topology could not reserve storage.
    NoMemory,
    /// The nested object exceeded its predeclared topology.
    TopologyCapacity {
        /// Maximum admitted nested source count.
        maximum: usize,
    },
    /// A nested concrete source rejected admission.
    Source(RegisterError),
    /// A deeper object layer rejected its nested registration.
    Nested,
    /// The nested two-phase owner was no longer coherent.
    InvalidState,
}

/// Error while reserving, arming, or publishing an aggregate registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollRegistrationError {
    /// The global finite readiness-source account is exhausted.
    Quota,
    /// Storage for the declared topology could not be allocated.
    NoMemory,
    /// More sources were armed than declared before publication.
    TopologyCapacity {
        /// Maximum admitted source count.
        maximum: usize,
    },
    /// A concrete source rejected registration.
    Source {
        /// Source index in arming order.
        index: usize,
        /// Typed generic-source admission failure.
        error: RegisterError,
    },
    /// A nested object registration failed before publication.
    Nested {
        /// Source index in arming order.
        index: usize,
        /// Preserved category from nested registration.
        error: NestedRegistrationError,
    },
    /// The two-phase aggregate owner was used after publication.
    InvalidState,
}

impl fmt::Display for PollRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Quota => formatter.write_str("readiness registration quota exhausted"),
            Self::NoMemory => formatter.write_str("readiness registration allocation failed"),
            Self::TopologyCapacity { .. } => {
                formatter.write_str("readiness source topology exceeded its reservation")
            }
            Self::Source { error, .. } => error.fmt(formatter),
            Self::Nested { .. } => formatter.write_str("nested readiness registration failed"),
            Self::InvalidState => formatter.write_str("readiness registration state is invalid"),
        }
    }
}

/// Unpublished owner of one bounded multi-source registration.
///
/// Storage and global accounting are admitted before the first source is
/// armed. Dropping this value cancels every already-armed source.
#[must_use = "a prepared readiness registration has not been published"]
pub struct PreparedPollRegistration<'a> {
    registrations: Option<Vec<RegistrationOwner<'a>>>,
    reserved: usize,
}

impl<'a> PreparedPollRegistration<'a> {
    /// Reserves a topology of at most `maximum_sources` registrations.
    pub fn try_new(maximum_sources: usize) -> Result<Self, PollRegistrationError> {
        charge_sources(maximum_sources)?;
        let mut registrations = Vec::new();
        if registrations.try_reserve_exact(maximum_sources).is_err() {
            refund_sources(maximum_sources);
            return Err(PollRegistrationError::NoMemory);
        }
        Ok(Self {
            registrations: Some(registrations),
            reserved: maximum_sources,
        })
    }

    /// Arms one borrowed poll source using already-reserved aggregate storage.
    pub fn arm<const CAPACITY: usize>(
        &mut self,
        source: &'a PollSet<CAPACITY>,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        self.arm_owner(|| {
            source
                .arm(source.prepare(waker))
                .map(|token| {
                    RegistrationOwner::Source(SourceRegistration {
                        source,
                        token: Some(token),
                    })
                })
                .map_err(|error| error.kind())
        })
    }

    /// Arms one `Arc`-owned source while retaining its owner in the token.
    pub fn arm_owned<const CAPACITY: usize>(
        &mut self,
        source: Arc<PollSet<CAPACITY>>,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        self.arm_owner(|| {
            source
                .arm(source.prepare(waker))
                .map(|token| {
                    let source: Arc<dyn ErasedPollSet> = source;
                    RegistrationOwner::OwnedSource(OwnedSourceRegistration {
                        source,
                        token: Some(token),
                    })
                })
                .map_err(|error| error.kind())
        })
    }

    /// Retains one already-published nested object registration.
    pub fn arm_nested(
        &mut self,
        arm: impl FnOnce() -> Result<PollRegistration<'a>, PollRegistrationError>,
    ) -> Result<(), PollRegistrationError> {
        let Some(registrations) = self.registrations.as_mut() else {
            return Err(PollRegistrationError::InvalidState);
        };
        let index = registrations.len();
        if index >= self.reserved {
            return Err(PollRegistrationError::TopologyCapacity {
                maximum: self.reserved,
            });
        }
        match arm() {
            Ok(registration) => {
                registrations.push(RegistrationOwner::Aggregate(registration));
                Ok(())
            }
            Err(error) => Err(PollRegistrationError::Nested {
                index,
                error: match error {
                    PollRegistrationError::Quota => NestedRegistrationError::Quota,
                    PollRegistrationError::NoMemory => NestedRegistrationError::NoMemory,
                    PollRegistrationError::TopologyCapacity { maximum } => {
                        NestedRegistrationError::TopologyCapacity { maximum }
                    }
                    PollRegistrationError::Source { error, .. } => {
                        NestedRegistrationError::Source(error)
                    }
                    PollRegistrationError::Nested { error, .. } => error,
                    PollRegistrationError::InvalidState => {
                        return Err(PollRegistrationError::InvalidState);
                    }
                },
            }),
        }
    }

    fn arm_owner(
        &mut self,
        arm: impl FnOnce() -> Result<RegistrationOwner<'a>, RegisterError>,
    ) -> Result<(), PollRegistrationError> {
        let Some(registrations) = self.registrations.as_mut() else {
            return Err(PollRegistrationError::InvalidState);
        };
        let index = registrations.len();
        if index >= self.reserved {
            return Err(PollRegistrationError::TopologyCapacity {
                maximum: self.reserved,
            });
        }
        match arm() {
            Ok(registration) => {
                registrations.push(registration);
                Ok(())
            }
            Err(error) => Err(PollRegistrationError::Source { index, error }),
        }
    }

    /// Publishes the fully armed registration and refunds unused credits.
    pub fn commit(mut self) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let registrations = self
            .registrations
            .take()
            .ok_or(PollRegistrationError::InvalidState)?;
        let source_count = registrations.len();
        refund_sources(self.reserved - source_count);
        Ok(PollRegistration {
            registrations: Some(registrations),
            charges: source_count,
        })
    }
}

impl Drop for PreparedPollRegistration<'_> {
    fn drop(&mut self) {
        if let Some(registrations) = self.registrations.as_mut() {
            for registration in registrations {
                let _ = registration.cancel();
            }
            refund_sources(self.reserved);
        }
    }
}

/// Published owner of every source token in one object-readiness wait.
#[must_use = "dropping the registration immediately cancels its readiness wait"]
pub struct PollRegistration<'a> {
    registrations: Option<Vec<RegistrationOwner<'a>>>,
    charges: usize,
}

impl<'a> PollRegistration<'a> {
    /// Publishes an object subscription with no wake sources.
    pub fn empty() -> Result<Self, PollRegistrationError> {
        PreparedPollRegistration::try_new(0)?.commit()
    }
    /// Registers one borrowed source as a complete object subscription.
    pub fn single<const CAPACITY: usize>(
        source: &'a PollSet<CAPACITY>,
        waker: &Waker,
    ) -> Result<Self, PollRegistrationError> {
        let mut prepared = PreparedPollRegistration::try_new(1)?;
        prepared.arm(source, waker)?;
        prepared.commit()
    }
    /// Registers one `Arc`-owned source as a complete object subscription.
    pub fn single_owned<const CAPACITY: usize>(
        source: Arc<PollSet<CAPACITY>>,
        waker: &Waker,
    ) -> Result<Self, PollRegistrationError> {
        let mut prepared = PreparedPollRegistration::try_new(1)?;
        prepared.arm_owned(source, waker)?;
        prepared.commit()
    }
    /// Updates every retained source to the current executor waker.
    pub fn update(&mut self, waker: &Waker) -> Result<(), AggregateError<RegistrationUpdateError>> {
        if self.registrations.is_none() {
            return Err(AggregateError {
                index: 0,
                error: RegistrationUpdateError::Nested,
            });
        }
        let mut first = None;
        if let Some(registrations) = self.registrations.as_mut() {
            for (index, registration) in registrations.iter_mut().enumerate() {
                if let Err(error) = registration.update(waker) {
                    if first.is_none() {
                        first = Some(AggregateError { index, error });
                    }
                }
            }
        }
        first.map_or(Ok(()), Err)
    }
    /// Cancels all retained sources and refunds accounting synchronously.
    pub fn cancel(&mut self) {
        if let Some(mut registrations) = self.registrations.take() {
            for registration in &mut registrations {
                let _ = registration.cancel();
            }
            refund_sources(self.charges);
            self.charges = 0;
        }
    }
    /// Returns the number of retained direct or nested registrations.
    pub fn source_count(&self) -> usize {
        self.registrations.as_ref().map_or(0, Vec::len)
    }
}

impl Drop for PollRegistration<'_> {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Trait implemented by OS-neutral objects that expose bounded readiness registration.
pub trait Pollable {
    /// Returns this object's current generic readiness state.
    fn poll(&self) -> IoEvents;

    /// Publishes one bounded registration for all sources relevant to `events`.
    fn register<'a>(
        &'a self,
        context: &mut core::task::Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError>;
}

/// Preferred descriptive name for the OS-neutral object readiness trait.
pub trait ObjectReadiness: Pollable {}

impl<T: Pollable + ?Sized> ObjectReadiness for T {}

/// One already-armed readiness wait whose future performs no object operation.
#[must_use = "dropping an armed wait immediately cancels its readiness registration"]
pub struct ReadinessWait<'a> {
    registration: PollRegistration<'a>,
    has_sources: bool,
}

impl<'a> ReadinessWait<'a> {
    /// Arms every source using a no-op waker outside an executor poll.
    pub fn arm<P: Pollable + ?Sized>(
        pollable: &'a P,
        events: IoEvents,
    ) -> Result<Self, PollRegistrationError> {
        let mut context = core::task::Context::from_waker(Waker::noop());
        let registration = pollable.register(&mut context, events)?;
        let has_sources = registration.source_count() != 0;
        Ok(Self {
            registration,
            has_sources,
        })
    }
}

impl core::future::Future for ReadinessWait<'_> {
    type Output = ();

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        context: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        if !self.has_sources {
            return core::task::Poll::Pending;
        }
        if self.registration.update(context.waker()).is_err() {
            self.registration.cancel();
            core::task::Poll::Ready(())
        } else {
            core::task::Poll::Pending
        }
    }
}
