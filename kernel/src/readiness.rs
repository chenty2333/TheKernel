//! Kernel-side errno policy for bounded readiness registration.

use core::task::{Context, Poll};

use axerrno::{AxError, AxResult};
use axhal::time::TimeValue;
use axpoll::{
    IoEvents, NestedRegistrationError, PollRegistration, PollRegistrationError, PollSet, Pollable,
    ReadinessWait, RegisterError,
};

struct PollSetSource<'a, const CAPACITY: usize>(&'a PollSet<CAPACITY>);

impl<const CAPACITY: usize> Pollable for PollSetSource<'_, CAPACITY> {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::single(self.0, context.waker())
    }
}

/// Maps a generic readiness-admission failure to the kernel's errno domain.
///
/// Allocation and finite waiter exhaustion are reported as `ENOMEM`, matching
/// Linux's `poll(2)` failure contract. Token exhaustion and closed/incoherent
/// sources remain distinguishable from ordinary memory pressure.
pub(crate) fn registration_error(error: PollRegistrationError) -> AxError {
    match error {
        PollRegistrationError::Quota | PollRegistrationError::NoMemory => AxError::NoMemory,
        PollRegistrationError::TopologyCapacity { .. } | PollRegistrationError::InvalidState => {
            AxError::BadState
        }
        PollRegistrationError::Source { error, .. } => source_registration_error(error),
        PollRegistrationError::Nested { error, .. } => nested_registration_error(error),
        _ => AxError::BadState,
    }
}

fn source_registration_error(error: RegisterError) -> AxError {
    match error {
        RegisterError::Full => AxError::NoMemory,
        RegisterError::Closed => AxError::BadState,
        RegisterError::TokenSpaceExhausted => AxError::OutOfRange,
    }
}

fn nested_registration_error(error: NestedRegistrationError) -> AxError {
    match error {
        NestedRegistrationError::Quota | NestedRegistrationError::NoMemory => AxError::NoMemory,
        NestedRegistrationError::Source(error) => source_registration_error(error),
        NestedRegistrationError::TopologyCapacity { .. }
        | NestedRegistrationError::Nested
        | NestedRegistrationError::InvalidState => AxError::BadState,
        _ => AxError::BadState,
    }
}

fn resolve_interrupt_recheck<T>(
    operation: AxResult<T>,
    interrupted: bool,
    should_interrupt: impl FnOnce() -> bool,
) -> (Poll<AxResult<T>>, bool) {
    match operation {
        Ok(value) => (Poll::Ready(Ok(value)), interrupted),
        Err(error) if error != AxError::WouldBlock => (Poll::Ready(Err(error)), interrupted),
        Err(_) if interrupted && should_interrupt() => {
            (Poll::Ready(Err(AxError::Interrupted)), false)
        }
        Err(_) => (Poll::Pending, false),
    }
}

fn completed_operation<T>(operation: AxResult<T>, nonblocking: bool) -> Option<AxResult<T>> {
    match operation {
        Ok(value) => Some(Ok(value)),
        Err(error) if error != AxError::WouldBlock || nonblocking => Some(Err(error)),
        Err(_) => None,
    }
}

/// Runs one adapter wait with the kernel's task-interruption and synchronous
/// block policy, explicitly flattening every typed failure layer.
pub(crate) fn block_on_poll_io<P, F, T>(
    pollable: &P,
    events: IoEvents,
    nonblocking: bool,
    operation: F,
) -> Result<T, AxError>
where
    P: Pollable + ?Sized,
    F: FnMut() -> Result<T, AxError>,
{
    block_on_poll_io_interruptible_if(pollable, events, nonblocking, operation, || true)
}

/// Deadline-aware synchronous readiness wait.
///
/// Object attempts execute outside the task's synchronous block session. Only
/// the already-armed readiness token, timer, and interrupt futures are polled
/// by `axtask::block_on`. The absolute deadline survives spurious readiness and
/// ignored task interrupts, so retrying cannot extend the caller's timeout.
pub(crate) fn block_on_poll_io_until<P, F, T>(
    pollable: &P,
    events: IoEvents,
    nonblocking: bool,
    deadline: Option<TimeValue>,
    mut operation: F,
) -> Result<AxResult<T>, axtask::future::Elapsed>
where
    P: Pollable + ?Sized,
    F: FnMut() -> AxResult<T>,
{
    let mut deadline_reservation = None;
    loop {
        if let Some(result) = completed_operation(operation(), nonblocking) {
            return Ok(result);
        }

        let wait = match ReadinessWait::arm(pollable, events) {
            Ok(wait) => wait,
            Err(error) => {
                return Ok(completed_operation(operation(), false)
                    .unwrap_or_else(|| Err(registration_error(error))));
            }
        };
        if let Some(result) = completed_operation(operation(), false) {
            return Ok(result);
        }

        let waited = if let Some(end) = deadline {
            if deadline_reservation.is_none() {
                match axtask::future::DeadlineReservation::reserve(end) {
                    Ok(reservation) => deadline_reservation = Some(reservation),
                    Err(error) => {
                        return Ok(completed_operation(operation(), false)
                            .unwrap_or_else(|| Err(error.into())));
                    }
                }
            }

            let reservation = deadline_reservation
                .as_mut()
                .expect("deadline reservation was initialized above");
            match axtask::future::block_on(reservation.race(axtask::future::interruptible(wait))) {
                Err(error) => return Ok(Err(error.into())),
                Ok(Err(elapsed)) => {
                    return match completed_operation(operation(), false) {
                        Some(result) => Ok(result),
                        None => Err(elapsed),
                    };
                }
                Ok(Ok(waited)) => waited,
            }
        } else {
            match axtask::future::block_on(axtask::future::interruptible(wait)) {
                Err(error) => return Ok(Err(error.into())),
                Ok(waited) => waited,
            }
        };

        match waited {
            Ok(()) => {}
            Err(_) => {
                let current = axtask::current();
                let (decision, restore_interrupt) =
                    resolve_interrupt_recheck(operation(), true, || true);
                if restore_interrupt {
                    current.interrupt();
                }
                match decision {
                    Poll::Ready(result) => return Ok(result),
                    Poll::Pending => {}
                }
            }
        }
    }
}

/// Runs every object attempt outside `axtask::block_on`; only the already
/// armed, operation-free readiness wait may own the task's block session.
///
/// The two outside attempts form `check -> arm -> check`.  A wake drains the
/// adapter registration, so [`ReadinessWait`] also detects a wake in either
/// gap before it installs the task waker.  On interruption, one final outside
/// attempt gives completed work priority and restores the interrupt exactly as
/// `axtask::future::interruptible` does for a non-blocking future.
fn block_on_poll_io_interruptible_if<P, F, I, T>(
    pollable: &P,
    events: IoEvents,
    nonblocking: bool,
    mut operation: F,
    mut should_interrupt: I,
) -> Result<T, AxError>
where
    P: Pollable + ?Sized,
    F: FnMut() -> Result<T, AxError>,
    I: FnMut() -> bool,
{
    loop {
        if let Some(result) = completed_operation(operation(), nonblocking) {
            return result;
        }

        let wait = match ReadinessWait::arm(pollable, events) {
            Ok(wait) => wait,
            Err(error) => {
                return completed_operation(operation(), false)
                    .unwrap_or_else(|| Err(registration_error(error)));
            }
        };
        if let Some(result) = completed_operation(operation(), false) {
            return result;
        }

        match axtask::future::block_on(axtask::future::interruptible(wait))
            .map_err(AxError::from)?
        {
            Ok(()) => {}
            Err(_) => {
                let current = axtask::current();
                let (decision, restore_interrupt) =
                    resolve_interrupt_recheck(operation(), true, &mut should_interrupt);
                if restore_interrupt {
                    current.interrupt();
                }
                match decision {
                    Poll::Ready(result) => return result,
                    Poll::Pending => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn nested_admission_errors_keep_linux_errno_categories() {
        assert_eq!(
            registration_error(PollRegistrationError::Nested {
                index: 3,
                error: NestedRegistrationError::NoMemory,
            }),
            AxError::NoMemory
        );
        assert_eq!(
            registration_error(PollRegistrationError::Nested {
                index: 7,
                error: NestedRegistrationError::Source(RegisterError::TokenSpaceExhausted),
            }),
            AxError::OutOfRange
        );
    }

    #[test]
    fn interrupt_recheck_restores_only_when_completed_work_wins() {
        let predicate_called = Cell::new(false);
        let (decision, restore_interrupt) = resolve_interrupt_recheck(Ok(17_u32), true, || {
            predicate_called.set(true);
            true
        });
        assert_eq!(decision, Poll::Ready(Ok(17)));
        assert!(restore_interrupt);
        assert!(!predicate_called.get());

        let (decision, restore_interrupt) =
            resolve_interrupt_recheck(Err::<u32, _>(AxError::WouldBlock), true, || false);
        assert_eq!(decision, Poll::Pending);
        assert!(!restore_interrupt);

        let (decision, restore_interrupt) =
            resolve_interrupt_recheck(Err::<u32, _>(AxError::WouldBlock), true, || true);
        assert_eq!(decision, Poll::Ready(Err(AxError::Interrupted)));
        assert!(!restore_interrupt);
    }

    #[test]
    fn closed_registration_rechecks_condition_before_returning_an_error() {
        let source: PollSet = PollSet::new();
        let calls = Cell::new(0);

        let result = block_on_poll_set(&source, || {
            let call = calls.get();
            calls.set(call + 1);
            if call == 0 {
                source.close();
                Err(AxError::WouldBlock)
            } else {
                Ok(29_u32)
            }
        });

        assert_eq!(result, Ok(29));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn closed_registration_recheck_is_bounded_when_condition_stays_blocked() {
        let source = PollSet::new();
        source.close();
        let calls = Cell::new(0);

        let result = block_on_poll_set(&source, || {
            calls.set(calls.get() + 1);
            Err::<u32, _>(AxError::WouldBlock)
        });

        assert_eq!(result, Err(AxError::BadState));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn nonblocking_attempt_never_arms_a_readiness_source() {
        let source: PollSet = PollSet::new();
        let calls = Cell::new(0);

        let result = block_on_poll_io(&PollSetSource(&source), IoEvents::empty(), true, || {
            calls.set(calls.get() + 1);
            Err::<u32, _>(AxError::WouldBlock)
        });

        assert_eq!(result, Err(AxError::WouldBlock));
        assert_eq!(calls.get(), 1);
        assert!(source.is_empty());
    }
}

/// Interruptible synchronous wait for one raw generic source.
pub(crate) fn block_on_poll_set<F, T>(source: &PollSet, operation: F) -> Result<T, AxError>
where
    F: FnMut() -> Result<T, AxError>,
{
    block_on_poll_io(&PollSetSource(source), IoEvents::empty(), false, operation)
}

/// Deadline-aware synchronous form for one raw generic source.
pub(crate) fn block_on_poll_set_until<F, T>(
    source: &PollSet,
    deadline: Option<TimeValue>,
    operation: F,
) -> Result<AxResult<T>, axtask::future::Elapsed>
where
    F: FnMut() -> AxResult<T>,
{
    block_on_poll_io_until(
        &PollSetSource(source),
        IoEvents::empty(),
        false,
        deadline,
        operation,
    )
}

/// Non-interruptible synchronous wait for kernel lifecycle handshakes such as
/// vfork publication. Typed block and registration failures still propagate.
pub(crate) fn block_on_poll_set_uninterruptible<const CAPACITY: usize, F, T>(
    source: &PollSet<CAPACITY>,
    mut operation: F,
) -> Result<T, AxError>
where
    F: FnMut() -> Result<T, AxError>,
{
    let pollable = PollSetSource(source);
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error != AxError::WouldBlock => return Err(error),
            Err(_) => {}
        }

        let wait = match ReadinessWait::arm(&pollable, IoEvents::empty()) {
            Ok(wait) => wait,
            Err(error) => {
                return completed_operation(operation(), false)
                    .unwrap_or_else(|| Err(registration_error(error)));
            }
        };
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error != AxError::WouldBlock => return Err(error),
            Err(_) => {}
        }

        axtask::future::block_on(wait).map_err(AxError::from)?;
    }
}

/// Waits on one source while consuming task interrupts only when the caller's
/// Linux-visible predicate says that interrupt should terminate the syscall.
pub(crate) fn block_on_poll_set_interruptible_if<F, I, T>(
    source: &PollSet,
    operation: F,
    should_interrupt: I,
) -> Result<T, AxError>
where
    F: FnMut() -> Result<T, AxError>,
    I: FnMut() -> bool,
{
    block_on_poll_io_interruptible_if(
        &PollSetSource(source),
        IoEvents::empty(),
        false,
        operation,
        should_interrupt,
    )
}
