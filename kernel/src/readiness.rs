//! Kernel-side errno policy for bounded readiness registration.

use core::{
    future::poll_fn,
    task::{Context, Poll},
};

use axerrno::{AxError, AxResult};
use axpoll::{
    IoEvents, NestedRegistrationError, PollIoError, PollRegistration, PollRegistrationError,
    PollSet, Pollable, RegisterError,
};

struct PollSetSource<'a>(&'a PollSet);

impl Pollable for PollSetSource<'_> {
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

/// Flattens the adapter's operation/registration split without losing either
/// failure class.
pub(crate) fn poll_io_error(error: PollIoError) -> AxError {
    match error {
        PollIoError::Operation(error) => error,
        PollIoError::Registration(error) => registration_error(error),
        _ => AxError::BadState,
    }
}

/// Runs one readiness operation with condition-first signal interruption.
///
/// `axtask::future::interruptible` polls the operation before consuming a task
/// interrupt and re-polls it after installing the interrupt waker. This keeps
/// ready work ahead of `EINTR` while preserving the adapter's complete
/// check-arm-check registration protocol.
pub(crate) async fn interruptible_poll_io<P, F, T>(
    pollable: &P,
    events: IoEvents,
    nonblocking: bool,
    operation: F,
) -> AxResult<T>
where
    P: Pollable + ?Sized,
    F: FnMut() -> AxResult<T>,
{
    axtask::future::interruptible(axpoll::poll_io(pollable, events, nonblocking, operation))
        .await
        .map_err(AxError::from)?
        .map_err(poll_io_error)
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
    axtask::future::block_on(interruptible_poll_io(
        pollable,
        events,
        nonblocking,
        operation,
    ))
    .map_err(AxError::from)?
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
}

/// Async check -> arm -> check wait for one raw generic source.
pub(crate) async fn poll_set_io<F, T>(source: &PollSet, operation: F) -> Result<T, PollIoError>
where
    F: FnMut() -> Result<T, AxError>,
{
    axpoll::poll_io(&PollSetSource(source), IoEvents::empty(), false, operation).await
}

/// Interruptible synchronous form of [`poll_set_io`].
pub(crate) fn block_on_poll_set<F, T>(source: &PollSet, operation: F) -> Result<T, AxError>
where
    F: FnMut() -> Result<T, AxError>,
{
    block_on_poll_io(&PollSetSource(source), IoEvents::empty(), false, operation)
}

/// Non-interruptible synchronous wait for kernel lifecycle handshakes such as
/// vfork publication. Typed block and registration failures still propagate.
pub(crate) fn block_on_poll_set_uninterruptible<F, T>(
    source: &PollSet,
    operation: F,
) -> Result<T, AxError>
where
    F: FnMut() -> Result<T, AxError>,
{
    axtask::future::block_on(poll_set_io(source, operation))
        .map_err(AxError::from)?
        .map_err(poll_io_error)
}

/// Waits on one source while consuming task interrupts only when the caller's
/// Linux-visible predicate says that interrupt should terminate the syscall.
pub(crate) fn block_on_poll_set_interruptible_if<F, I, T>(
    source: &PollSet,
    mut operation: F,
    mut should_interrupt: I,
) -> Result<T, AxError>
where
    F: FnMut() -> Result<T, AxError>,
    I: FnMut() -> bool,
{
    let mut registration: Option<PollRegistration<'_>> = None;
    axtask::future::block_on(poll_fn(|context| {
        if let Some(retained) = registration.as_mut()
            && retained.update(context.waker()).is_err()
        {
            registration = None;
        }
        match operation() {
            Ok(value) => return Poll::Ready(Ok(value)),
            Err(error) if error != AxError::WouldBlock => return Poll::Ready(Err(error)),
            Err(_) => {}
        }
        if registration.is_none() {
            match PollRegistration::single(source, context.waker()) {
                Ok(retained) => registration = Some(retained),
                Err(error) => return Poll::Ready(Err(registration_error(error))),
            }
            match operation() {
                Ok(value) => return Poll::Ready(Ok(value)),
                Err(error) if error != AxError::WouldBlock => return Poll::Ready(Err(error)),
                Err(_) => {}
            }
        }

        let current = axtask::current();
        let interrupted = current.poll_interrupt(context).is_ready();
        let (decision, restore_interrupt) =
            resolve_interrupt_recheck(operation(), interrupted, &mut should_interrupt);
        if restore_interrupt {
            current.interrupt();
        }
        decision
    }))
    .map_err(AxError::from)?
}
