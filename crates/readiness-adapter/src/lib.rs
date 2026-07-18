//! TheKernel's object-readiness adapter.
//!
//! `thekernel-axpoll` owns bounded per-source wake registrations. The
//! Linux-ABI FD crate owns bounded, transactional fan-in. This crate connects
//! those contracts for TheKernel object traits without putting Linux policy
//! back into the generic ax mechanism.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::sync::Arc;
use core::{
    convert::Infallible,
    fmt,
    future::{Future, poll_fn},
    pin::Pin,
    task::{Context, Poll},
};

use axerrno::{AxError, AxResult};
pub use axpoll_core::{DEFAULT_CAPACITY, IoEvents, RegisterError, RegistrationToken, UpdateError};
use thekernel_linux_fd::{
    AggregateError, ArmError, CancelState, CommitSubscriptionError, PrepareSubscriptionError,
    PreparedSubscription, RetainedRegistration, Subscription, WatchAccount,
};

/// Hard system-wide ceiling for live readiness source registrations.
///
/// Every registration is also bounded by its source [`PollSet`] capacity.
/// This account prevents a growing number of objects or waits from turning
/// those individually finite sets into an unaccounted global resource.
pub const MAX_LIVE_SOURCE_REGISTRATIONS: usize = 65_536;

static SOURCE_ACCOUNT: WatchAccount = match WatchAccount::try_new(MAX_LIVE_SOURCE_REGISTRATIONS) {
    Ok(account) => account,
    Err(_) => panic!("finite readiness account must be constructible"),
};

/// Returns currently retained readiness topology credits.
///
/// This is intended for bounded diagnostics and leak tests. Direct sources
/// consume one credit; an explicitly nested aggregate also consumes one
/// parent-topology credit in addition to its own children.
pub fn live_registration_charges() -> usize {
    SOURCE_ACCOUNT.used()
}

/// Generic bounded source registry supplied by `thekernel-axpoll`.
pub type PollSet<const CAPACITY: usize = DEFAULT_CAPACITY> = axpoll_core::PollSet<CAPACITY>;

trait ErasedPollSet: Send + Sync {
    fn update(
        &self,
        token: RegistrationToken,
        waker: &core::task::Waker,
    ) -> Result<(), UpdateError>;

    fn cancel(&self, token: RegistrationToken) -> bool;
}

impl<const CAPACITY: usize> ErasedPollSet for PollSet<CAPACITY> {
    fn update(
        &self,
        token: RegistrationToken,
        waker: &core::task::Waker,
    ) -> Result<(), UpdateError> {
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

impl RetainedRegistration for SourceRegistration<'_> {
    type UpdateError = UpdateError;
    type CancelError = Infallible;

    fn update(&mut self, waker: &core::task::Waker) -> Result<(), Self::UpdateError> {
        let token = self.token.ok_or(UpdateError::InvalidToken)?;
        self.source.update(token, waker)
    }

    fn cancel(&mut self) -> Result<CancelState, Self::CancelError> {
        let Some(token) = self.token.take() else {
            return Ok(CancelState::AlreadyInactive);
        };
        Ok(if self.source.cancel(token) {
            CancelState::Cancelled
        } else {
            CancelState::AlreadyInactive
        })
    }
}

impl RetainedRegistration for OwnedSourceRegistration {
    type UpdateError = UpdateError;
    type CancelError = Infallible;

    fn update(&mut self, waker: &core::task::Waker) -> Result<(), Self::UpdateError> {
        let token = self.token.ok_or(UpdateError::InvalidToken)?;
        self.source.update(token, waker)
    }

    fn cancel(&mut self) -> Result<CancelState, Self::CancelError> {
        let Some(token) = self.token.take() else {
            return Ok(CancelState::AlreadyInactive);
        };
        Ok(if self.source.cancel(token) {
            CancelState::Cancelled
        } else {
            CancelState::AlreadyInactive
        })
    }
}

enum RegistrationOwner<'a> {
    Source(SourceRegistration<'a>),
    OwnedSource(OwnedSourceRegistration),
    Aggregate(PollRegistration<'a>),
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

impl RetainedRegistration for RegistrationOwner<'_> {
    type UpdateError = RegistrationUpdateError;
    type CancelError = Infallible;

    fn update(&mut self, waker: &core::task::Waker) -> Result<(), Self::UpdateError> {
        match self {
            Self::Source(source) => source
                .update(waker)
                .map_err(RegistrationUpdateError::Source),
            Self::OwnedSource(source) => source
                .update(waker)
                .map_err(RegistrationUpdateError::Source),
            Self::Aggregate(aggregate) => aggregate
                .update(waker)
                .map_err(|_| RegistrationUpdateError::Nested),
        }
    }

    fn cancel(&mut self) -> Result<CancelState, Self::CancelError> {
        match self {
            Self::Source(source) => source.cancel(),
            Self::OwnedSource(source) => source.cancel(),
            Self::Aggregate(aggregate) => {
                aggregate.cancel();
                Ok(CancelState::Cancelled)
            }
        }
    }
}

/// Error while reserving, arming, or publishing an aggregate registration.
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
    /// The global, finite readiness-source account is exhausted.
    Quota,
    /// Storage for the declared topology could not be allocated.
    NoMemory,
    /// More sources were armed than the caller declared before publication.
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
        /// Preserved category from the nested registration.
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
/// armed. Dropping this value rolls every already-armed source back.
#[must_use = "a prepared readiness registration has not been published"]
pub struct PreparedPollRegistration<'a> {
    inner: Option<PreparedSubscription<'static, RegistrationOwner<'a>>>,
}

impl<'a> PreparedPollRegistration<'a> {
    /// Reserves a topology of at most `maximum_sources` registrations.
    pub fn try_new(maximum_sources: usize) -> Result<Self, PollRegistrationError> {
        let inner =
            PreparedSubscription::try_new(&SOURCE_ACCOUNT, maximum_sources).map_err(|error| {
                match error {
                    PrepareSubscriptionError::Quota => PollRegistrationError::Quota,
                    PrepareSubscriptionError::NoMemory => PollRegistrationError::NoMemory,
                    _ => PollRegistrationError::InvalidState,
                }
            })?;
        Ok(Self { inner: Some(inner) })
    }

    /// Arms one direct generic source using already-reserved aggregate storage.
    pub fn arm<const CAPACITY: usize>(
        &mut self,
        source: &'a PollSet<CAPACITY>,
        waker: &core::task::Waker,
    ) -> Result<(), PollRegistrationError> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(PollRegistrationError::InvalidState);
        };
        inner
            .arm_with(|| {
                let prepared = source.prepare(waker);
                source
                    .arm(prepared)
                    .map(|token| {
                        RegistrationOwner::Source(SourceRegistration {
                            source,
                            token: Some(token),
                        })
                    })
                    .map_err(|error| error.kind())
            })
            .map_err(map_source_arm_error)
    }

    /// Arms a source while retaining its `Arc` owner in the published token.
    ///
    /// This is the safe bridge for sources discovered through a locked object
    /// graph: the caller clones the stable source owner under its object lock,
    /// releases that lock, and transfers the owner here. No borrowed lifetime
    /// is extended and no transmute is involved.
    pub fn arm_owned<const CAPACITY: usize>(
        &mut self,
        source: Arc<PollSet<CAPACITY>>,
        waker: &core::task::Waker,
    ) -> Result<(), PollRegistrationError> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(PollRegistrationError::InvalidState);
        };
        inner
            .arm_with(|| {
                let prepared = source.prepare(waker);
                source
                    .arm(prepared)
                    .map(|token| {
                        let source: Arc<dyn ErasedPollSet> = source;
                        RegistrationOwner::OwnedSource(OwnedSourceRegistration {
                            source,
                            token: Some(token),
                        })
                    })
                    .map_err(|error| error.kind())
            })
            .map_err(map_source_arm_error)
    }

    /// Retains one already-published nested object subscription.
    ///
    /// The closure runs only after this aggregate's storage and accounting
    /// were reserved. Failure leaves prior sources owned by the prepare value,
    /// so normal error propagation rolls them back.
    pub fn arm_nested(
        &mut self,
        arm: impl FnOnce() -> Result<PollRegistration<'a>, PollRegistrationError>,
    ) -> Result<(), PollRegistrationError> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(PollRegistrationError::InvalidState);
        };
        inner
            .arm_with(|| arm().map(RegistrationOwner::Aggregate))
            .map_err(map_nested_arm_error)
    }

    /// Publishes the fully armed registration.
    pub fn commit(mut self) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let inner = self
            .inner
            .take()
            .ok_or(PollRegistrationError::InvalidState)?
            .commit()
            .map_err(|_error: CommitSubscriptionError| PollRegistrationError::InvalidState)?;
        Ok(PollRegistration { inner: Some(inner) })
    }
}

fn map_source_arm_error(error: ArmError<RegisterError>) -> PollRegistrationError {
    match error {
        ArmError::Capacity { maximum } => PollRegistrationError::TopologyCapacity { maximum },
        ArmError::Source(AggregateError { index, error }) => {
            PollRegistrationError::Source { index, error }
        }
        ArmError::InvalidState => PollRegistrationError::InvalidState,
    }
}

fn map_nested_arm_error(error: ArmError<PollRegistrationError>) -> PollRegistrationError {
    match error {
        ArmError::Capacity { maximum } => PollRegistrationError::TopologyCapacity { maximum },
        ArmError::Source(AggregateError { index, error }) => PollRegistrationError::Nested {
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
                PollRegistrationError::InvalidState => NestedRegistrationError::InvalidState,
            },
        },
        ArmError::InvalidState => PollRegistrationError::InvalidState,
    }
}

/// Published owner of every source token in one object-readiness wait.
///
/// Drop or [`cancel`](Self::cancel) detaches all sources and refunds the exact
/// global charge. Re-polling an unchanged future should call
/// [`update`](Self::update), not publish another subscription.
#[must_use = "dropping the registration immediately cancels its readiness wait"]
pub struct PollRegistration<'a> {
    inner: Option<Subscription<'static, RegistrationOwner<'a>>>,
}

impl<'a> PollRegistration<'a> {
    /// Publishes an object subscription with no wake sources.
    ///
    /// Permanently-ready or permanently-unready objects can return this
    /// without pretending that a source was armed.
    pub fn empty() -> Result<Self, PollRegistrationError> {
        PreparedPollRegistration::try_new(0)?.commit()
    }

    /// Registers one direct source as a complete object subscription.
    pub fn single<const CAPACITY: usize>(
        source: &'a PollSet<CAPACITY>,
        waker: &core::task::Waker,
    ) -> Result<Self, PollRegistrationError> {
        let mut prepared = PreparedPollRegistration::try_new(1)?;
        prepared.arm(source, waker)?;
        prepared.commit()
    }

    /// Registers one `Arc`-owned source as a complete object subscription.
    pub fn single_owned<const CAPACITY: usize>(
        source: Arc<PollSet<CAPACITY>>,
        waker: &core::task::Waker,
    ) -> Result<Self, PollRegistrationError> {
        let mut prepared = PreparedPollRegistration::try_new(1)?;
        prepared.arm_owned(source, waker)?;
        prepared.commit()
    }

    /// Updates every retained source to the current executor waker.
    pub fn update(
        &mut self,
        waker: &core::task::Waker,
    ) -> Result<(), AggregateError<RegistrationUpdateError>> {
        match self.inner.as_mut() {
            Some(inner) => inner.update_all(waker),
            None => Err(AggregateError {
                index: 0,
                error: RegistrationUpdateError::Nested,
            }),
        }
    }

    /// Cancels all retained sources and refunds accounting synchronously.
    pub fn cancel(&mut self) {
        if let Some(inner) = self.inner.take() {
            let result = inner.cancel();
            match result {
                Ok(()) => {}
                Err(error) => match error.error {},
            }
        }
    }

    /// Returns the exact number of retained direct or nested registrations.
    pub fn source_count(&self) -> usize {
        self.inner.as_ref().map_or(0, Subscription::source_count)
    }
}

/// One already-armed readiness wait whose future performs no object operation.
///
/// [`arm`](Self::arm) installs the complete bounded registration with a no-op
/// waker.  A source wake drains that registration, so a later executor-waker
/// update detects a wake that happened between arming and the first poll.  This
/// lets a synchronous consumer execute its potentially sleeping object
/// operation outside its task executor while retaining the usual
/// `check -> arm -> check -> wait` lost-wake protocol.
///
/// Returning `Ready` or dropping this value synchronously cancels every
/// still-live sibling registration and refunds its bounded topology charge.
#[must_use = "dropping an armed wait immediately cancels its readiness registration"]
pub struct ReadinessWait<'a> {
    registration: PollRegistration<'a>,
    has_sources: bool,
}

impl<'a> ReadinessWait<'a> {
    /// Arms every readiness source without depending on an active task block
    /// session.
    ///
    /// The no-op waker is deliberate: source wake consumes the registration,
    /// and the first future poll observes that consumption through the failed
    /// waker update.  Permanently-unready objects may publish an empty
    /// registration; those waits remain pending until an outer interruption or
    /// timeout policy terminates them.
    pub fn arm<P>(pollable: &'a P, events: IoEvents) -> Result<Self, PollRegistrationError>
    where
        P: Pollable + ?Sized,
    {
        let mut context = Context::from_waker(core::task::Waker::noop());
        let registration = pollable.register(&mut context, events)?;
        let has_sources = registration.source_count() != 0;
        Ok(Self {
            registration,
            has_sources,
        })
    }
}

impl Future for ReadinessWait<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.has_sources {
            return Poll::Pending;
        }
        if self.registration.update(context.waker()).is_err() {
            self.registration.cancel();
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for PollRegistration<'_> {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Trait implemented by objects that expose readiness and cancellable source
/// registration to TheKernel's FD and blocking adapters.
pub trait Pollable {
    /// Returns the object's current generic readiness state.
    fn poll(&self) -> IoEvents;

    /// Publishes one bounded registration for all relevant sources.
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError>;
}

/// Typed failure from [`poll_io_nonblocking`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollIoError {
    /// The nonblocking object operation returned an error other than
    /// `WouldBlock`, or nonblocking mode requested that result directly.
    Operation(AxError),
    /// The finite readiness topology could not be registered.
    Registration(PollRegistrationError),
}

impl fmt::Display for PollIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(error) => error.fmt(formatter),
            Self::Registration(error) => error.fmt(formatter),
        }
    }
}

/// Drives a synchronous nonblocking operation through one cancellable wait.
///
/// The future performs `check -> arm every source -> check`, retains the
/// complete aggregate while pending, updates its wakers on a normal re-poll,
/// and cancels it on every completion or drop path. Interruption, timeout, and
/// the final errno mapping remain caller policy and therefore wrap this future
/// outside the adapter.
///
/// Because `operation` is invoked from [`Future::poll`], it must be a truly
/// non-blocking attempt: it may return [`AxError::WouldBlock`], but it must not
/// acquire a sleeping lock or enter a synchronous task block session.  A
/// synchronous consumer with a potentially sleeping operation should execute
/// its attempts outside the executor and use [`ReadinessWait`] for the armed
/// wait phase.
pub async fn poll_io_nonblocking<P, F, T>(
    pollable: &P,
    events: IoEvents,
    nonblocking: bool,
    mut operation: F,
) -> Result<T, PollIoError>
where
    P: Pollable + ?Sized,
    F: FnMut() -> AxResult<T>,
{
    let mut registration: Option<PollRegistration<'_>> = None;
    poll_fn(move |context| {
        if let Some(retained) = registration.as_mut()
            && retained.update(context.waker()).is_err()
        {
            registration = None;
        }

        match operation() {
            Ok(value) => return Poll::Ready(Ok(value)),
            Err(error) if error != AxError::WouldBlock => {
                return Poll::Ready(Err(PollIoError::Operation(error)));
            }
            Err(error) if nonblocking => {
                return Poll::Ready(Err(PollIoError::Operation(error)));
            }
            Err(_) => {}
        }

        if registration.is_none() {
            let retained = match pollable.register(context, events) {
                Ok(retained) => retained,
                Err(error) => {
                    return match operation() {
                        Ok(value) => Poll::Ready(Ok(value)),
                        Err(operation_error) if operation_error != AxError::WouldBlock => {
                            Poll::Ready(Err(PollIoError::Operation(operation_error)))
                        }
                        Err(_) => Poll::Ready(Err(PollIoError::Registration(error))),
                    };
                }
            };
            registration = Some(retained);

            match operation() {
                Ok(value) => return Poll::Ready(Ok(value)),
                Err(error) if error != AxError::WouldBlock => {
                    return Poll::Ready(Err(PollIoError::Operation(error)));
                }
                Err(_) => {}
            }
        }

        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake};
    use core::{
        future::Future,
        pin::pin,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };

    use super::*;

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct TestPollable<'a, const CAPACITY: usize> {
        source: &'a PollSet<CAPACITY>,
    }

    struct TwoSourcePollable<'a> {
        first: &'a PollSet<1>,
        second: &'a PollSet<1>,
    }

    struct EmptyPollable;

    impl Pollable for EmptyPollable {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            PollRegistration::empty()
        }
    }

    impl<'source, const CAPACITY: usize> Pollable for TestPollable<'source, CAPACITY> {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            PollRegistration::single(self.source, context.waker())
        }
    }

    impl Pollable for TwoSourcePollable<'_> {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            let mut prepared = PreparedPollRegistration::try_new(2)?;
            prepared.arm(self.first, context.waker())?;
            prepared.arm(self.second, context.waker())?;
            prepared.commit()
        }
    }

    #[test]
    fn readiness_wait_observes_a_wake_before_its_first_task_poll() {
        let source = PollSet::<1>::new();
        let pollable = TestPollable { source: &source };
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut context = Context::from_waker(&waker);
        let mut wait = pin!(ReadinessWait::arm(&pollable, IoEvents::READABLE).unwrap());

        assert_eq!(source.len(), 1);
        assert_eq!(source.wake(), 1);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(()));
        assert!(source.is_empty());
    }

    #[test]
    fn readiness_wait_updates_the_executor_waker_then_completes_on_wake() {
        let source = PollSet::<1>::new();
        let pollable = TestPollable { source: &source };
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut context = Context::from_waker(&waker);
        let mut wait = pin!(ReadinessWait::arm(&pollable, IoEvents::READABLE).unwrap());

        assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(source.wake(), 1);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(()));
        assert!(source.is_empty());
    }

    #[test]
    fn readiness_wait_cancels_sibling_sources_before_returning_ready() {
        let first = PollSet::<1>::new();
        let second = PollSet::<1>::new();
        let pollable = TwoSourcePollable {
            first: &first,
            second: &second,
        };
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);
        let mut context = Context::from_waker(&waker);
        let mut wait = pin!(ReadinessWait::arm(&pollable, IoEvents::READABLE).unwrap());

        assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(first.wake(), 1);
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(()));
        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(wait.registration.source_count(), 0);
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(()));
    }

    #[test]
    fn empty_readiness_wait_remains_pending_for_outer_policy() {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);
        let mut context = Context::from_waker(&waker);
        let mut wait = pin!(ReadinessWait::arm(&EmptyPollable, IoEvents::empty()).unwrap());

        assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
    }

    #[test]
    fn dropping_readiness_wait_cancels_its_registration() {
        let source = PollSet::<1>::new();
        let pollable = TestPollable { source: &source };

        let wait = ReadinessWait::arm(&pollable, IoEvents::READABLE).unwrap();
        assert_eq!(source.len(), 1);
        drop(wait);
        assert!(source.is_empty());
    }

    #[test]
    fn aggregate_arms_updates_and_cancels_every_source() {
        let first = PollSet::<2>::new();
        let second = PollSet::<2>::new();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut prepared = PreparedPollRegistration::try_new(2).unwrap();
        prepared.arm(&first, &waker).unwrap();
        prepared.arm(&second, &waker).unwrap();
        let mut registration = prepared.commit().unwrap();

        assert_eq!(registration.source_count(), 2);
        registration.update(&waker).unwrap();
        registration.cancel();
        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dropped_prepare_rolls_back_partial_arm() {
        let source = PollSet::<1>::new();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);
        let mut prepared = PreparedPollRegistration::try_new(2).unwrap();
        prepared.arm(&source, &waker).unwrap();
        drop(prepared);
        assert!(source.is_empty());
    }

    #[test]
    fn nested_registration_preserves_deep_failure_category() {
        let mapped = map_nested_arm_error(ArmError::Source(AggregateError {
            index: 5,
            error: PollRegistrationError::Nested {
                index: 2,
                error: NestedRegistrationError::Source(RegisterError::Full),
            },
        }));

        assert_eq!(
            mapped,
            PollRegistrationError::Nested {
                index: 5,
                error: NestedRegistrationError::Source(RegisterError::Full),
            }
        );
    }

    #[test]
    fn owned_source_registration_keeps_discovered_source_alive() {
        let source = Arc::new(PollSet::<1>::new());
        let weak = Arc::downgrade(&source);
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);
        let mut registration = PollRegistration::single_owned(source, &waker).unwrap();

        let retained = weak.upgrade().unwrap();
        assert_eq!(retained.len(), 1);
        drop(retained);
        registration.cancel();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn poll_io_nonblocking_retains_one_token_updates_waker_and_cancels_on_drop() {
        let source = PollSet::<2>::new();
        let pollable = TestPollable { source: &source };
        let ready = AtomicBool::new(false);
        let first_counter = Arc::new(Counter(AtomicUsize::new(0)));
        let second_counter = Arc::new(Counter(AtomicUsize::new(0)));
        let first_waker = Waker::from(first_counter.clone());
        let second_waker = Waker::from(second_counter.clone());

        {
            let mut future = pin!(poll_io_nonblocking(
                &pollable,
                IoEvents::READABLE,
                false,
                || {
                    if ready.load(Ordering::Acquire) {
                        Ok(7_u32)
                    } else {
                        Err(AxError::WouldBlock)
                    }
                },
            ));
            let mut first_context = Context::from_waker(&first_waker);
            assert_eq!(future.as_mut().poll(&mut first_context), Poll::Pending);
            assert_eq!(source.len(), 1);

            let mut second_context = Context::from_waker(&second_waker);
            assert_eq!(future.as_mut().poll(&mut second_context), Poll::Pending);
            assert_eq!(source.len(), 1);
            assert_eq!(source.wake(), 1);
            assert_eq!(first_counter.0.load(Ordering::SeqCst), 0);
            assert_eq!(second_counter.0.load(Ordering::SeqCst), 1);

            ready.store(true, Ordering::Release);
            assert_eq!(
                future.as_mut().poll(&mut second_context),
                Poll::Ready(Ok(7))
            );
        }
        assert!(source.is_empty());
    }

    #[test]
    fn poll_io_nonblocking_closes_arm_race_with_a_second_operation_check() {
        let source = PollSet::<1>::new();
        let pollable = TestPollable { source: &source };
        let calls = AtomicUsize::new(0);
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);

        {
            let mut future = pin!(poll_io_nonblocking(
                &pollable,
                IoEvents::READABLE,
                false,
                || {
                    if calls.fetch_add(1, Ordering::AcqRel) == 0 {
                        Err(AxError::WouldBlock)
                    } else {
                        Ok(11_u32)
                    }
                },
            ));
            let mut context = Context::from_waker(&waker);
            assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(Ok(11)));
            assert_eq!(calls.load(Ordering::Acquire), 2);
        }
        assert!(source.is_empty());
    }

    #[test]
    fn poll_io_nonblocking_reports_source_capacity_instead_of_overwriting_a_waiter() {
        let source = PollSet::<1>::new();
        let pollable = TestPollable { source: &source };
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);
        let occupied = source.register(&waker).unwrap();

        let mut future = pin!(poll_io_nonblocking(
            &pollable,
            IoEvents::READABLE,
            false,
            || Err::<u32, _>(AxError::WouldBlock),
        ));
        let mut context = Context::from_waker(&waker);
        assert_eq!(
            future.as_mut().poll(&mut context),
            Poll::Ready(Err(PollIoError::Registration(
                PollRegistrationError::Source {
                    index: 0,
                    error: RegisterError::Full,
                }
            )))
        );
        assert_eq!(source.len(), 1);
        assert!(source.cancel(occupied));
    }

    #[test]
    fn poll_io_nonblocking_rechecks_the_operation_when_a_source_closes_before_arm() {
        let source = PollSet::<1>::new();
        let pollable = TestPollable { source: &source };
        let calls = AtomicUsize::new(0);
        let mut future = pin!(poll_io_nonblocking(
            &pollable,
            IoEvents::READABLE,
            false,
            || match calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    source.close();
                    Err(AxError::WouldBlock)
                }
                _ => Ok(41_u32),
            },
        ));
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);
        let mut context = Context::from_waker(&waker);

        assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(Ok(41)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
