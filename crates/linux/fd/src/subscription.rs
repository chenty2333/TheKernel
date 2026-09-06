use alloc::vec::Vec;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

fn try_atomic_update(
    counter: &AtomicUsize,
    set_order: Ordering,
    load_order: Ordering,
    mut update: impl FnMut(usize) -> Option<usize>,
) -> Result<usize, usize> {
    let mut current = counter.load(load_order);
    loop {
        let Some(next) = update(current) else {
            return Err(current);
        };
        match counter.compare_exchange_weak(current, next, set_order, load_order) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

/// Shared bounded accounting for persistent watches or ephemeral source slots.
pub struct WatchAccount {
    limit: usize,
    used: AtomicUsize,
}

impl WatchAccount {
    /// Creates an account with an immutable finite limit.
    pub const fn try_new(limit: usize) -> Result<Self, WatchChargeError> {
        if limit == usize::MAX {
            return Err(WatchChargeError::Unbounded);
        }
        Ok(Self {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    /// Returns the configured maximum charge.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Returns currently retained source credits.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Atomically reserves `amount`, refunding it when the returned charge is
    /// dropped.
    pub fn try_charge(&self, amount: usize) -> Result<WatchCharge<'_>, WatchChargeError> {
        try_atomic_update(&self.used, Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(amount).filter(|next| *next <= self.limit)
        })
        .map(|_| WatchCharge {
            account: self,
            amount,
        })
        .map_err(|_| WatchChargeError::Limit)
    }
}

/// Watch-account admission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchChargeError {
    /// The checked charge would exceed the configured owner limit.
    Limit,
    /// `usize::MAX` is rejected rather than exposed as an unbounded policy.
    Unbounded,
}

/// RAII ownership of admitted watch/source credits.
pub struct WatchCharge<'a> {
    account: &'a WatchAccount,
    amount: usize,
}

impl WatchCharge<'_> {
    /// Returns the retained charge.
    pub const fn amount(&self) -> usize {
        self.amount
    }

    fn reduce_to(&mut self, amount: usize) -> Result<(), CommitSubscriptionError> {
        if amount > self.amount {
            return Err(CommitSubscriptionError::InvalidState);
        }
        let refund = self.amount - amount;
        self.amount = amount;
        self.account.used.fetch_sub(refund, Ordering::AcqRel);
        Ok(())
    }
}

impl Drop for WatchCharge<'_> {
    fn drop(&mut self) {
        self.account.used.fetch_sub(self.amount, Ordering::AcqRel);
    }
}

/// Terminal result of cancelling one retained low-level registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelState {
    /// This call detached a live source registration.
    Cancelled,
    /// The source had already closed, fired, or detached the registration.
    AlreadyInactive,
}

/// One owned source registration retained until explicit cancellation/drop.
///
/// `cancel` must leave the value without ownership of a live source slot even
/// when it returns an error. This lets aggregate teardown continue without a
/// hidden retry loop or resource leak.
pub trait RetainedRegistration {
    /// Waker-update error.
    type UpdateError;
    /// Cancellation outcome error.
    type CancelError;

    /// Updates the executor waker without creating another registration.
    fn update(&mut self, waker: &Waker) -> Result<(), Self::UpdateError>;

    /// Detaches the retained source slot and consumes its live ownership.
    fn cancel(&mut self) -> Result<CancelState, Self::CancelError>;
}

/// Aggregate operation failure identifying the source that failed first.
#[derive(Debug, PartialEq, Eq)]
pub struct AggregateError<E> {
    /// Source index in arming order.
    pub index: usize,
    /// Source-specific typed error.
    pub error: E,
}

/// Failure while arming one source in a prepared aggregate.
#[derive(Debug, PartialEq, Eq)]
pub enum ArmError<E> {
    /// The caller attempted to exceed its declared, admitted topology.
    Capacity {
        /// Maximum source count reserved during prepare.
        maximum: usize,
    },
    /// A specific source rejected registration.
    Source(AggregateError<E>),
    /// The prepared owner no longer retains its unpublished storage.
    InvalidState,
}

/// Failure while publishing a fully armed aggregate subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitSubscriptionError {
    /// The prepared owner did not retain a coherent storage/accounting pair.
    InvalidState,
}

/// Failure before any aggregate subscription is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrepareSubscriptionError {
    /// Per-owner/watch accounting rejected the declared maximum source count.
    Quota,
    /// Fallible owned registration storage could not be reserved.
    NoMemory,
}

/// Two-phase aggregate registration owner.
///
/// The caller declares and pays for the maximum source topology before arming.
/// Any error or dropped prepare value cancels every already armed source.
#[must_use = "dropping a prepared subscription cancels every armed source"]
pub struct PreparedSubscription<'a, R: RetainedRegistration> {
    registrations: Option<Vec<R>>,
    charge: Option<WatchCharge<'a>>,
    maximum: usize,
}

impl<'a, R: RetainedRegistration> PreparedSubscription<'a, R> {
    /// Reserves accounting and owned storage before source publication.
    pub fn try_new(
        account: &'a WatchAccount,
        maximum_sources: usize,
    ) -> Result<Self, PrepareSubscriptionError> {
        let charge = account
            .try_charge(maximum_sources)
            .map_err(|_| PrepareSubscriptionError::Quota)?;
        let mut registrations = Vec::new();
        if registrations.try_reserve_exact(maximum_sources).is_err() {
            drop(charge);
            return Err(PrepareSubscriptionError::NoMemory);
        }
        Ok(Self {
            registrations: Some(registrations),
            charge: Some(charge),
            maximum: maximum_sources,
        })
    }

    /// Returns the declared source maximum.
    pub const fn maximum_sources(&self) -> usize {
        self.maximum
    }

    /// Returns the number of sources armed so far.
    pub fn armed_sources(&self) -> usize {
        self.registrations.as_ref().map_or(0, Vec::len)
    }

    /// Arms and retains one source. If `arm` fails, the prepared owner remains
    /// intact so normal error propagation drops it and rolls back prior slots.
    pub fn arm_with<E>(&mut self, arm: impl FnOnce() -> Result<R, E>) -> Result<(), ArmError<E>> {
        let Some(registrations) = self.registrations.as_mut() else {
            return Err(ArmError::InvalidState);
        };
        let index = registrations.len();
        if index >= self.maximum {
            // The caller violated its declared topology. Do not run `arm`,
            // because there is no admitted storage/accounting for a source.
            return Err(ArmError::Capacity {
                maximum: self.maximum,
            });
        }
        match arm() {
            Ok(registration) => {
                registrations.push(registration);
                Ok(())
            }
            Err(error) => Err(ArmError::Source(AggregateError { index, error })),
        }
    }

    /// Publishes the complete aggregate and refunds unused planned credits.
    pub fn commit(mut self) -> Result<Subscription<'a, R>, CommitSubscriptionError> {
        let Some(mut registrations) = self.registrations.take() else {
            return Err(CommitSubscriptionError::InvalidState);
        };
        let Some(mut charge) = self.charge.take() else {
            cancel_all(&mut registrations);
            return Err(CommitSubscriptionError::InvalidState);
        };
        if let Err(error) = charge.reduce_to(registrations.len()) {
            cancel_all(&mut registrations);
            return Err(error);
        }
        Ok(Subscription {
            registrations: Some(registrations),
            charge: Some(charge),
        })
    }
}

impl<R: RetainedRegistration> Drop for PreparedSubscription<'_, R> {
    fn drop(&mut self) {
        if let Some(registrations) = self.registrations.as_mut() {
            cancel_all(registrations);
        }
    }
}

/// Published aggregate subscription retaining every source token and charge.
#[must_use = "dropping a subscription cancels every retained source"]
pub struct Subscription<'a, R: RetainedRegistration> {
    registrations: Option<Vec<R>>,
    charge: Option<WatchCharge<'a>>,
}

impl<R: RetainedRegistration> Subscription<'_, R> {
    /// Returns the retained source count.
    pub fn source_count(&self) -> usize {
        self.registrations.as_ref().map_or(0, Vec::len)
    }

    /// Updates every retained source and reports the first error after trying
    /// all sources.
    pub fn update_all(&mut self, waker: &Waker) -> Result<(), AggregateError<R::UpdateError>> {
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

    /// Explicitly cancels all sources, still attempting later sources after an
    /// error. Credits are refunded before this function returns.
    pub fn cancel(mut self) -> Result<(), AggregateError<R::CancelError>> {
        let result = self
            .registrations
            .as_mut()
            .map_or(Ok(()), |registrations| cancel_all_result(registrations));
        self.registrations = None;
        self.charge = None;
        result
    }
}

impl<R: RetainedRegistration> Drop for Subscription<'_, R> {
    fn drop(&mut self) {
        if let Some(registrations) = self.registrations.as_mut() {
            cancel_all(registrations);
        }
    }
}

fn cancel_all<R: RetainedRegistration>(registrations: &mut [R]) {
    for registration in registrations {
        let _ = registration.cancel();
    }
}

fn cancel_all_result<R: RetainedRegistration>(
    registrations: &mut [R],
) -> Result<(), AggregateError<R::CancelError>> {
    let mut first = None;
    for (index, registration) in registrations.iter_mut().enumerate() {
        if let Err(error) = registration.cancel() {
            if first.is_none() {
                first = Some(AggregateError { index, error });
            }
        }
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{RawWaker, RawWakerVTable},
    };

    use super::*;

    struct Registration {
        cancels: Arc<AtomicUsize>,
        updates: Arc<AtomicUsize>,
        cancel_error: bool,
    }

    impl RetainedRegistration for Registration {
        type UpdateError = ();
        type CancelError = ();

        fn update(&mut self, _waker: &Waker) -> Result<(), Self::UpdateError> {
            self.updates.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn cancel(&mut self) -> Result<CancelState, Self::CancelError> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            if self.cancel_error {
                Err(())
            } else {
                Ok(CancelState::Cancelled)
            }
        }
    }

    fn registration(cancels: &Arc<AtomicUsize>, updates: &Arc<AtomicUsize>) -> Registration {
        Registration {
            cancels: cancels.clone(),
            updates: updates.clone(),
            cancel_error: false,
        }
    }

    fn noop_waker() -> Waker {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        unsafe fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        // SAFETY: every vtable operation accepts the null data pointer and
        // performs no dereference or ownership operation.
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    #[test]
    fn partial_arm_failure_rolls_back_every_prior_source_and_charge() {
        let account = WatchAccount::try_new(4).unwrap();
        let cancels = Arc::new(AtomicUsize::new(0));
        let updates = Arc::new(AtomicUsize::new(0));
        let result: Result<(), ArmError<&str>> = (|| {
            let mut prepared = PreparedSubscription::try_new(&account, 3).unwrap();
            prepared.arm_with(|| Ok(registration(&cancels, &updates)))?;
            prepared.arm_with(|| Ok(registration(&cancels, &updates)))?;
            prepared.arm_with(|| Err("closed"))?;
            Ok(())
        })();
        assert_eq!(
            result,
            Err(ArmError::Source(AggregateError {
                index: 2,
                error: "closed",
            }))
        );
        assert_eq!(cancels.load(Ordering::SeqCst), 2);
        assert_eq!(account.used(), 0);
    }

    #[test]
    fn commit_refunds_unused_plan_and_drop_cancels_all() {
        let account = WatchAccount::try_new(8).unwrap();
        let cancels = Arc::new(AtomicUsize::new(0));
        let updates = Arc::new(AtomicUsize::new(0));
        {
            let mut prepared = PreparedSubscription::try_new(&account, 6).unwrap();
            prepared
                .arm_with::<()>(|| Ok(registration(&cancels, &updates)))
                .unwrap();
            prepared
                .arm_with::<()>(|| Ok(registration(&cancels, &updates)))
                .unwrap();
            let mut subscription = prepared.commit().unwrap();
            assert_eq!(account.used(), 2);
            subscription.update_all(&noop_waker()).unwrap();
            assert_eq!(updates.load(Ordering::SeqCst), 2);
        }
        assert_eq!(cancels.load(Ordering::SeqCst), 2);
        assert_eq!(account.used(), 0);
    }

    #[test]
    fn quota_is_atomic_under_concurrency() {
        let account = Arc::new(WatchAccount::try_new(4).unwrap());
        let start = Arc::new(std::sync::Barrier::new(9));
        let admitted = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut threads = alloc::vec::Vec::new();
        for _ in 0..8 {
            let account = account.clone();
            let start = start.clone();
            let admitted = admitted.clone();
            let peak = peak.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                if let Ok(charge) = account.try_charge(1) {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    peak.fetch_max(account.used(), Ordering::SeqCst);
                    std::thread::yield_now();
                    drop(charge);
                }
            }));
        }
        start.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(admitted.load(Ordering::SeqCst) >= 1);
        assert!(peak.load(Ordering::SeqCst) <= 4);
        assert_eq!(account.used(), 0);
    }

    #[test]
    fn account_rejects_an_effectively_unbounded_limit() {
        assert!(matches!(
            WatchAccount::try_new(usize::MAX),
            Err(WatchChargeError::Unbounded)
        ));
    }
}
