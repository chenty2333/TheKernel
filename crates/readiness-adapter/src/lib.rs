//! TheKernel errno and product-future bridge for generic readiness objects.
//!
//! Bounded source registration, aggregate token ownership, and object
//! readiness traits are defined by `thekernel-axpoll`. This crate deliberately
//! contains only TheKernel product policy built on that neutral contract.

#![no_std]
#![deny(missing_docs)]

use core::{fmt, future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult};
pub use axpoll_core::{
    AggregateError, DEFAULT_CAPACITY, IoEvents, MAX_LIVE_SOURCE_REGISTRATIONS,
    NestedRegistrationError, ObjectReadiness, PollRegistration, PollRegistrationError, PollSet,
    Pollable, PreparedPollRegistration, ReadinessProvider, ReadinessSource, ReadinessWait,
    RegisterError, RegistrationToken, RegistrationUpdateError, UpdateError,
    live_registration_charges,
};

/// Typed failure from [`poll_io_nonblocking`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollIoError {
    /// The nonblocking object operation returned an error other than `WouldBlock`.
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
/// aggregate while pending, updates wakers on re-poll, and cancels on every
/// completion or drop path. Linux errno translation remains caller policy.
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
            Err(error) if nonblocking => return Poll::Ready(Err(PollIoError::Operation(error))),
            Err(_) => {}
        }
        if registration.is_none() {
            registration = Some(match pollable.register(context, events) {
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
            });
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
