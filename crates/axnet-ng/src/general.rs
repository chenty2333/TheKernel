use core::{
    sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    task::Waker,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
#[cfg(test)]
use axpoll::PollRegistration;
use axpoll::{
    IoEvents, NestedRegistrationError, PollRegistrationError, Pollable, PreparedPollRegistration,
    ReadinessWait, RegisterError,
};
use axtask::future::{DeadlineReservation, block_on, interruptible};

use crate::{
    SocketTransferDirection,
    net_stack::NetStack,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

const READY_RETRY_BUDGET: usize = 16;

pub(crate) fn poll_registration_error(error: PollRegistrationError) -> AxError {
    match error {
        PollRegistrationError::NoMemory => AxError::NoMemory,
        PollRegistrationError::Quota
        | PollRegistrationError::Source {
            error: RegisterError::Full,
            ..
        } => AxError::ResourceBusy,
        PollRegistrationError::Source {
            error: RegisterError::TokenSpaceExhausted,
            ..
        } => AxError::OutOfRange,
        PollRegistrationError::Source {
            error: RegisterError::Closed,
            ..
        }
        | PollRegistrationError::TopologyCapacity { .. }
        | PollRegistrationError::InvalidState => AxError::BadState,
        PollRegistrationError::Nested { error, .. } => nested_registration_error(error),
        _ => AxError::BadState,
    }
}

fn nested_registration_error(error: NestedRegistrationError) -> AxError {
    match error {
        NestedRegistrationError::NoMemory => AxError::NoMemory,
        NestedRegistrationError::Quota | NestedRegistrationError::Source(RegisterError::Full) => {
            AxError::ResourceBusy
        }
        NestedRegistrationError::Source(RegisterError::TokenSpaceExhausted) => AxError::OutOfRange,
        NestedRegistrationError::Source(RegisterError::Closed)
        | NestedRegistrationError::TopologyCapacity { .. }
        | NestedRegistrationError::Nested
        | NestedRegistrationError::InvalidState => AxError::BadState,
        _ => AxError::BadState,
    }
}

const fn receive_timeout_error() -> AxError {
    // Linux reports expiry of SO_RCVTIMEO as EAGAIN/EWOULDBLOCK. ETIMEDOUT is
    // reserved for protocol-level timeout failures, not an exhausted receive
    // wait budget.
    AxError::WouldBlock
}

const fn send_timeout_error() -> AxError {
    // SO_SNDTIMEO expiry is reported like a nonblocking data operation when
    // no bytes were transferred, not as a protocol-level ETIMEDOUT.
    AxError::WouldBlock
}

const fn connect_timeout_error() -> AxError {
    // Linux reports SO_SNDTIMEO expiry from connect(2) as EINPROGRESS. A
    // protocol-owned connection timeout remains free to report ETIMEDOUT.
    AxError::InProgress
}

#[derive(Clone, Copy)]
struct PollBehavior {
    timeout_error: AxError,
    effective_nonblocking: bool,
    consume_pending_error: bool,
}

#[derive(Clone, Copy)]
enum PollWaitFailure {
    Interrupted,
    Error(AxError),
}

impl PollWaitFailure {
    const fn error(self) -> AxError {
        match self {
            Self::Interrupted => AxError::Interrupted,
            Self::Error(error) => error,
        }
    }
}

fn resolve_wait_failure<T>(
    completed: Option<AxResult<T>>,
    failure: PollWaitFailure,
) -> (AxResult<T>, bool) {
    match completed {
        Some(result) => (result, matches!(failure, PollWaitFailure::Interrupted)),
        None => (Err(failure.error()), false),
    }
}

/// General options for all sockets.
pub(crate) struct GeneralOptions {
    /// Whether the socket is non-blocking.
    nonblock: AtomicBool,
    /// Whether the socket should reuse the address.
    reuse_address: AtomicBool,
    /// Whether SO_DONTROUTE is enabled.
    dont_route: AtomicBool,

    send_timeout_nanos: AtomicU64,
    recv_timeout_nanos: AtomicU64,

    pending_error: AtomicI32,

    device_mask: AtomicU64,
}
impl Default for GeneralOptions {
    fn default() -> Self {
        Self::new()
    }
}
impl GeneralOptions {
    pub fn new() -> Self {
        Self {
            nonblock: AtomicBool::new(false),
            reuse_address: AtomicBool::new(false),
            dont_route: AtomicBool::new(false),

            send_timeout_nanos: AtomicU64::new(0),
            recv_timeout_nanos: AtomicU64::new(0),

            pending_error: AtomicI32::new(0),

            device_mask: AtomicU64::new(0),
        }
    }

    pub fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    pub fn reuse_address(&self) -> bool {
        self.reuse_address.load(Ordering::Relaxed)
    }

    pub fn dont_route(&self) -> bool {
        self.dont_route.load(Ordering::Relaxed)
    }

    pub fn send_timeout(&self) -> Option<Duration> {
        let nanos = self.send_timeout_nanos.load(Ordering::Relaxed);
        (nanos > 0).then(|| Duration::from_nanos(nanos))
    }

    pub fn recv_timeout(&self) -> Option<Duration> {
        let nanos = self.recv_timeout_nanos.load(Ordering::Relaxed);
        (nanos > 0).then(|| Duration::from_nanos(nanos))
    }

    pub fn set_pending_error(&self, error: LinuxError) {
        self.pending_error.store(error.code(), Ordering::Release);
    }

    pub fn clear_pending_error(&self) {
        self.pending_error.store(0, Ordering::Release);
    }

    fn take_pending_error(&self) -> Option<LinuxError> {
        let code = self.pending_error.swap(0, Ordering::AcqRel);
        LinuxError::try_from(code).ok()
    }

    /// Adds the generic error readiness bit while a deferred socket error is
    /// pending. Merely recording an error intentionally does not wake poll
    /// waiters; callers that defer an error after partial progress must not
    /// manufacture a wakeup that Linux does not provide.
    pub fn add_pending_error_event(&self, events: IoEvents) -> IoEvents {
        if self.pending_error.load(Ordering::Acquire) != 0 {
            events | IoEvents::ERROR
        } else {
            events
        }
    }

    /// Consumes and returns the one-shot deferred socket error, if any.
    pub fn consume_pending_error(&self) -> AxResult {
        match self.take_pending_error() {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    pub fn set_device_mask(&self, mask: u64) {
        self.device_mask.store(mask, Ordering::Release);
    }

    pub fn device_mask(&self) -> u64 {
        self.device_mask.load(Ordering::Acquire)
    }

    pub fn arm_waker<'a>(
        &self,
        stack: &'a NetStack,
        prepared: &mut PreparedPollRegistration<'a>,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        stack.arm_readiness(prepared, self.device_mask(), waker)
    }

    pub fn connect_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> AxResult<T> {
        self.run_poller(
            pollable,
            IoEvents::WRITABLE,
            self.send_timeout(),
            PollBehavior {
                timeout_error: connect_timeout_error(),
                effective_nonblocking: self.nonblocking(),
                consume_pending_error: false,
            },
            f,
        )
    }

    pub fn send_poller_with_effective_nonblocking<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        effective_nonblocking: bool,
        f: F,
    ) -> AxResult<T> {
        // Linux's data-send paths consume sk_err before attempting to publish
        // any new payload. Connect completion uses `connect_poller` instead so
        // a failed nonblocking connect remains observable through SO_ERROR.
        self.run_poller(
            pollable,
            IoEvents::WRITABLE,
            self.send_timeout(),
            PollBehavior {
                timeout_error: send_timeout_error(),
                effective_nonblocking,
                consume_pending_error: true,
            },
            f,
        )
    }

    pub fn recv_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> AxResult<T> {
        self.recv_poller_with_nonblocking(pollable, false, f)
    }

    pub fn recv_poller_with_nonblocking<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        per_call_nonblocking: bool,
        f: F,
    ) -> AxResult<T> {
        self.recv_poller_with_effective_nonblocking(
            pollable,
            self.nonblocking() || per_call_nonblocking,
            f,
        )
    }

    pub fn recv_poller_with_effective_nonblocking<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        effective_nonblocking: bool,
        f: F,
    ) -> AxResult<T> {
        self.run_poller(
            pollable,
            IoEvents::READABLE,
            self.recv_timeout(),
            PollBehavior {
                timeout_error: receive_timeout_error(),
                effective_nonblocking,
                consume_pending_error: true,
            },
            f,
        )
    }

    pub fn transfer_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        direction: SocketTransferDirection,
        effective_nonblocking: bool,
        f: F,
    ) -> AxResult<T> {
        match direction {
            SocketTransferDirection::Receive => {
                self.recv_poller_with_effective_nonblocking(pollable, effective_nonblocking, f)
            }
            SocketTransferDirection::Send => {
                self.send_poller_with_effective_nonblocking(pollable, effective_nonblocking, f)
            }
        }
    }

    fn completed_operation<F: FnMut() -> AxResult<T>, T>(
        &self,
        behavior: PollBehavior,
        nonblocking: bool,
        operation: &mut F,
    ) -> Option<AxResult<T>> {
        if behavior.consume_pending_error
            && let Err(error) = self.consume_pending_error()
        {
            return Some(Err(error));
        }
        match operation() {
            Ok(value) => Some(Ok(value)),
            Err(error) if error != AxError::WouldBlock || nonblocking => Some(Err(error)),
            Err(_) => None,
        }
    }

    fn run_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        interest: IoEvents,
        timeout: Option<Duration>,
        behavior: PollBehavior,
        mut f: F,
    ) -> AxResult<T> {
        let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
        let mut deadline_reservation = None;
        let mut ready_retries = 0usize;

        loop {
            // Object operations and deferred-error consumption can acquire
            // sleeping socket/backend locks. Keep every attempt outside the
            // synchronous task block session.
            if let Some(result) =
                self.completed_operation(behavior, behavior.effective_nonblocking, &mut f)
            {
                return result;
            }

            // A stale or overly broad readiness indication must neither hide
            // timeout expiry nor monopolize a CPU forever. A small immediate
            // retry window preserves the common race-closing fast path; a
            // persistently ready source yields at a fixed budget.
            if deadline.is_some_and(|end| axhal::time::wall_time() >= end) {
                return resolve_wait_failure(
                    self.completed_operation(behavior, false, &mut f),
                    PollWaitFailure::Error(behavior.timeout_error),
                )
                .0;
            }
            let events = pollable.poll();
            if events.intersects(interest | IoEvents::ALWAYS) {
                ready_retries += 1;
                if ready_retries >= READY_RETRY_BUDGET {
                    axtask::yield_now();
                    ready_retries = 0;
                }
                continue;
            }
            ready_retries = 0;

            // Publish the complete bounded topology with a no-op waker before
            // entering block_on. ReadinessWait's first executor-waker update
            // detects a wake in either check/arm gap without re-entering the
            // Pollable object graph from Future::poll.
            let wait = match ReadinessWait::arm(pollable, interest) {
                Ok(wait) => wait,
                Err(error) => {
                    let completed = self.completed_operation(behavior, false, &mut f);
                    return resolve_wait_failure(
                        completed,
                        PollWaitFailure::Error(poll_registration_error(error)),
                    )
                    .0;
                }
            };

            // Complete check -> arm -> check with the authoritative operation,
            // not merely a potentially stale readiness bit.
            if let Some(result) = self.completed_operation(behavior, false, &mut f) {
                return result;
            }
            if pollable.poll().intersects(interest | IoEvents::ALWAYS) {
                drop(wait);
                ready_retries += 1;
                if ready_retries >= READY_RETRY_BUDGET {
                    axtask::yield_now();
                    ready_retries = 0;
                }
                continue;
            }
            if deadline.is_some_and(|end| axhal::time::wall_time() >= end) {
                return resolve_wait_failure(
                    self.completed_operation(behavior, false, &mut f),
                    PollWaitFailure::Error(behavior.timeout_error),
                )
                .0;
            }

            // Only the already-published readiness token, task interrupt, and
            // a previously admitted deadline are polled inside the block
            // session. Reserve the absolute deadline lazily, after every
            // authoritative operation recheck, and retain it across spurious
            // readiness sessions. This keeps one socket call to one bounded
            // timer admission without charging immediately completed calls.
            let failure = if let Some(end) = deadline {
                if deadline_reservation.is_none() {
                    match DeadlineReservation::reserve(end) {
                        Ok(reservation) => deadline_reservation = Some(reservation),
                        Err(error) => {
                            let completed = self.completed_operation(behavior, false, &mut f);
                            return resolve_wait_failure(
                                completed,
                                PollWaitFailure::Error(error.into()),
                            )
                            .0;
                        }
                    }
                }

                let reservation = deadline_reservation
                    .as_mut()
                    .expect("deadline reservation was initialized above");
                match block_on(reservation.race(interruptible(wait))) {
                    Ok(Ok(Ok(()))) => continue,
                    Ok(Ok(Err(_))) => PollWaitFailure::Interrupted,
                    Ok(Err(_)) => PollWaitFailure::Error(behavior.timeout_error),
                    Err(error) => PollWaitFailure::Error(error.into()),
                }
            } else {
                match block_on(interruptible(wait)) {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => PollWaitFailure::Interrupted,
                    Err(error) => PollWaitFailure::Error(error.into()),
                }
            };

            // A completed operation wins a simultaneous interrupt, timeout,
            // timer-admission failure, or block-session failure. When it wins
            // an interrupt race, preserve that interrupt for the caller's next
            // interruption boundary.
            let completed = self.completed_operation(behavior, false, &mut f);
            let (result, restore_interrupt) = resolve_wait_failure(completed, failure);
            if restore_interrupt {
                axtask::current().interrupt();
            }
            return result;
        }
    }
}
impl Configurable for GeneralOptions {
    fn nonblocking(&self) -> bool {
        self.nonblocking()
    }

    fn get_option_inner(&self, option: &mut GetSocketOption) -> AxResult<bool> {
        use GetSocketOption as O;

        match option {
            O::Error(error) => {
                **error = self.take_pending_error().map_or(0, LinuxError::code);
            }
            O::NonBlocking(nonblock) => {
                **nonblock = self.nonblocking();
            }
            O::ReuseAddress(reuse) => {
                **reuse = self.reuse_address();
            }
            O::DontRoute(dont_route) => {
                **dont_route = self.dont_route();
            }
            O::SendTimeout(timeout) => {
                **timeout = Duration::from_nanos(self.send_timeout_nanos.load(Ordering::Relaxed));
            }
            O::ReceiveTimeout(timeout) => {
                **timeout = Duration::from_nanos(self.recv_timeout_nanos.load(Ordering::Relaxed));
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> AxResult<bool> {
        use SetSocketOption as O;

        match option {
            O::NonBlocking(nonblock) => {
                self.nonblock.store(*nonblock, Ordering::Relaxed);
            }
            O::ReuseAddress(reuse) => {
                self.reuse_address.store(*reuse, Ordering::Relaxed);
            }
            O::DontRoute(dont_route) => {
                self.dont_route.store(*dont_route, Ordering::Relaxed);
            }
            O::SendTimeout(timeout) => {
                self.send_timeout_nanos
                    .store(timeout.as_nanos() as u64, Ordering::Relaxed);
            }
            O::ReceiveTimeout(timeout) => {
                self.recv_timeout_nanos
                    .store(timeout.as_nanos() as u64, Ordering::Relaxed);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use core::{
        sync::atomic::{AtomicBool, AtomicUsize},
        task::Context,
    };
    use std::sync::{Mutex, Once};

    use axpoll::PollSet;

    use super::*;
    use crate::options::{Configurable, GetSocketOption, SetSocketOption};

    static TASK_INIT: Once = Once::new();
    static TASK_SERIAL: Mutex<()> = Mutex::new(());

    fn init_task_runtime() {
        axhal::percpu::init_primary(0);
        TASK_INIT.call_once(|| axtask::init_scheduler().unwrap());
    }

    struct Ready;

    impl Pollable for Ready {
        fn poll(&self) -> IoEvents {
            IoEvents::WRITABLE
        }

        fn register<'a>(
            &'a self,
            _context: &mut core::task::Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            PollRegistration::empty()
        }
    }

    struct ArmedPollable {
        source: PollSet<2>,
        armed: AtomicBool,
        wake_while_arming: bool,
    }

    impl ArmedPollable {
        const fn new(wake_while_arming: bool) -> Self {
            Self {
                source: PollSet::new(),
                armed: AtomicBool::new(false),
                wake_while_arming,
            }
        }
    }

    impl Pollable for ArmedPollable {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            let registration = PollRegistration::single(&self.source, context.waker())?;
            self.armed.store(true, Ordering::Release);
            if self.wake_while_arming {
                self.source.wake();
            }
            Ok(registration)
        }
    }

    struct RejectingPollable;

    impl Pollable for RejectingPollable {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            Err(PollRegistrationError::NoMemory)
        }
    }

    struct OneSpuriousReady {
        polls: AtomicUsize,
    }

    impl Pollable for OneSpuriousReady {
        fn poll(&self) -> IoEvents {
            if self.polls.fetch_add(1, Ordering::AcqRel) == 0 {
                IoEvents::WRITABLE
            } else {
                IoEvents::empty()
            }
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            PollRegistration::empty()
        }
    }

    #[test]
    fn socket_error_is_reported_once() {
        let options = GeneralOptions::new();
        options.set_pending_error(LinuxError::ECONNREFUSED);

        let mut error = 0;
        options
            .get_option(GetSocketOption::Error(&mut error))
            .unwrap();
        assert_eq!(error, LinuxError::ECONNREFUSED.code());

        options
            .get_option(GetSocketOption::Error(&mut error))
            .unwrap();
        assert_eq!(error, 0);
    }

    #[test]
    fn receive_timeout_maps_to_linux_eagain() {
        assert_eq!(
            LinuxError::from(receive_timeout_error()),
            LinuxError::EAGAIN
        );
    }

    #[test]
    fn send_timeout_maps_to_linux_eagain() {
        assert_eq!(LinuxError::from(send_timeout_error()), LinuxError::EAGAIN);
    }

    #[test]
    fn connect_timeout_maps_to_linux_einprogress() {
        assert_eq!(
            LinuxError::from(connect_timeout_error()),
            LinuxError::EINPROGRESS
        );
    }

    #[test]
    fn operation_recheck_wins_an_already_elapsed_socket_timeout() {
        let options = GeneralOptions::new();
        let mut attempts = 0;

        let result = options.run_poller(
            &Ready,
            IoEvents::WRITABLE,
            Some(Duration::ZERO),
            PollBehavior {
                timeout_error: send_timeout_error(),
                effective_nonblocking: false,
                consume_pending_error: false,
            },
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(AxError::WouldBlock)
                } else {
                    Ok(61usize)
                }
            },
        );

        assert_eq!(result, Ok(61));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn nested_registration_keeps_resource_failure_category() {
        assert_eq!(
            poll_registration_error(PollRegistrationError::Nested {
                index: 4,
                error: NestedRegistrationError::NoMemory,
            }),
            AxError::NoMemory
        );
        assert_eq!(
            poll_registration_error(PollRegistrationError::Nested {
                index: 4,
                error: NestedRegistrationError::Source(RegisterError::Full),
            }),
            AxError::ResourceBusy
        );
        assert_eq!(
            poll_registration_error(PollRegistrationError::Nested {
                index: 4,
                error: NestedRegistrationError::Source(RegisterError::TokenSpaceExhausted),
            }),
            AxError::OutOfRange
        );
    }

    #[test]
    fn pending_error_sets_readiness_and_precedes_send_attempt() {
        let options = GeneralOptions::new();
        options.set_pending_error(LinuxError::ECONNREFUSED);
        let events = options.add_pending_error_event(IoEvents::WRITABLE);
        assert!(events.contains(IoEvents::WRITABLE));
        assert!(events.contains(IoEvents::ERROR));

        let mut attempted = false;
        let error = options
            .send_poller_with_effective_nonblocking(&Ready, false, || {
                attempted = true;
                Ok(1usize)
            })
            .unwrap_err();
        assert_eq!(LinuxError::from(error), LinuxError::ECONNREFUSED);
        assert!(!attempted);
        let events = options.add_pending_error_event(IoEvents::WRITABLE);
        assert!(events.contains(IoEvents::WRITABLE));
        assert!(!events.contains(IoEvents::ERROR));

        assert_eq!(
            options.send_poller_with_effective_nonblocking(&Ready, false, || Ok(1usize)),
            Ok(1)
        );
    }

    #[test]
    fn pending_error_during_blocked_send_terminates_retry() {
        let options = GeneralOptions::new();
        let mut attempts = 0;
        let error = options
            .send_poller_with_effective_nonblocking(&Ready, false, || {
                attempts += 1;
                options.set_pending_error(LinuxError::ECONNREFUSED);
                Err::<usize, _>(AxError::WouldBlock)
            })
            .unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::ECONNREFUSED);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn armed_wait_is_rechecked_and_refunded_before_task_blocking() {
        let options = GeneralOptions::new();
        let pollable = ArmedPollable::new(false);
        let mut attempts = 0;

        let result = options.send_poller_with_effective_nonblocking(&pollable, false, || {
            attempts += 1;
            if attempts == 1 {
                return Err(AxError::WouldBlock);
            }
            assert!(pollable.armed.load(Ordering::Acquire));
            assert_eq!(pollable.source.len(), 1);
            Ok(17usize)
        });

        assert_eq!(result, Ok(17));
        assert_eq!(attempts, 2);
        assert!(pollable.source.is_empty());
    }

    #[test]
    fn wake_during_arm_cannot_lose_the_final_operation() {
        let options = GeneralOptions::new();
        let pollable = ArmedPollable::new(true);
        let mut attempts = 0;

        let result = options.recv_poller(&pollable, || {
            attempts += 1;
            if attempts == 1 {
                Err(AxError::WouldBlock)
            } else {
                Ok(23usize)
            }
        });

        assert_eq!(result, Ok(23));
        assert_eq!(attempts, 2);
        assert!(pollable.source.is_empty());
    }

    #[test]
    fn final_operation_beats_registration_failure() {
        let options = GeneralOptions::new();
        let mut attempts = 0;

        let result = options.recv_poller(&RejectingPollable, || {
            attempts += 1;
            if attempts == 1 {
                Err(AxError::WouldBlock)
            } else {
                Ok(29usize)
            }
        });

        assert_eq!(result, Ok(29));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn one_spurious_ready_edge_retries_without_extending_the_operation() {
        let options = GeneralOptions::new();
        let pollable = OneSpuriousReady {
            polls: AtomicUsize::new(0),
        };
        let mut attempts = 0;

        let result = options.send_poller_with_effective_nonblocking(&pollable, false, || {
            attempts += 1;
            if attempts < 2 {
                Err(AxError::WouldBlock)
            } else {
                Ok(31usize)
            }
        });

        assert_eq!(result, Ok(31));
        assert_eq!(attempts, 2);
        assert_eq!(pollable.polls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn terminal_recheck_precedes_interrupt_timeout_and_block_errors() {
        for failure in [
            PollWaitFailure::Interrupted,
            PollWaitFailure::Error(AxError::TimedOut),
            PollWaitFailure::Error(AxError::BadState),
        ] {
            let (result, restore_interrupt) = resolve_wait_failure(Some(Ok(37usize)), failure);
            assert_eq!(result, Ok(37));
            assert_eq!(
                restore_interrupt,
                matches!(failure, PollWaitFailure::Interrupted)
            );

            let (result, restore_interrupt) =
                resolve_wait_failure(Some(Err::<usize, _>(AxError::BrokenPipe)), failure);
            assert_eq!(result, Err(AxError::BrokenPipe));
            assert_eq!(
                restore_interrupt,
                matches!(failure, PollWaitFailure::Interrupted)
            );
        }

        assert_eq!(
            resolve_wait_failure::<usize>(None, PollWaitFailure::Interrupted),
            (Err(AxError::Interrupted), false)
        );
        assert_eq!(
            resolve_wait_failure::<usize>(None, PollWaitFailure::Error(AxError::ResourceBusy)),
            (Err(AxError::ResourceBusy), false)
        );
    }

    #[test]
    fn socket_operations_can_start_their_own_block_session() {
        let _serial = TASK_SERIAL.lock().unwrap();
        init_task_runtime();
        let options = GeneralOptions::new();
        let pollable = ArmedPollable::new(false);
        let mut attempts = 0;

        let result = options.recv_poller(&pollable, || {
            attempts += 1;
            if attempts == 1 {
                return Err(AxError::WouldBlock);
            }
            assert_eq!(axtask::future::block_on(async { 41usize }), Ok(41));
            Ok(43usize)
        });

        assert_eq!(result, Ok(43));
        assert_eq!(attempts, 2);
        assert!(pollable.source.is_empty());
    }

    #[test]
    fn completed_operation_wins_interrupt_and_restores_it() {
        let _serial = TASK_SERIAL.lock().unwrap();
        init_task_runtime();
        let current = axtask::current();
        current.clear_interrupt();
        current.interrupt();
        let options = GeneralOptions::new();
        let pollable = ArmedPollable::new(false);
        let mut attempts = 0;

        let result = options.recv_poller(&pollable, || {
            attempts += 1;
            if attempts < 3 {
                Err(AxError::WouldBlock)
            } else {
                Ok(47usize)
            }
        });

        assert_eq!(result, Ok(47));
        assert_eq!(attempts, 3);
        assert!(current.is_interrupted());
        current.clear_interrupt();
        assert!(pollable.source.is_empty());
    }

    #[test]
    fn nonzero_deadline_survives_repeated_wakes_and_refunds_readiness() {
        let _serial = TASK_SERIAL.lock().unwrap();
        init_task_runtime();
        let options = GeneralOptions::new();
        options
            .set_option(SetSocketOption::ReceiveTimeout(&Duration::from_secs(60)))
            .unwrap();
        let pollable = ArmedPollable::new(true);
        let mut attempts = 0;

        let result = options.recv_poller(&pollable, || {
            attempts += 1;
            if attempts < 3 {
                Err(AxError::WouldBlock)
            } else {
                Ok(53usize)
            }
        });

        assert_eq!(result, Ok(53));
        assert_eq!(attempts, 3);
        assert!(pollable.source.is_empty());
    }

    #[test]
    fn exact_nonblocking_override_is_not_resampled_from_backend() {
        let options = GeneralOptions::new();
        options
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();
        let mut attempts = 0;
        assert_eq!(
            options.send_poller_with_effective_nonblocking(&Ready, false, || {
                attempts += 1;
                if attempts == 1 {
                    Err(AxError::WouldBlock)
                } else {
                    Ok(7usize)
                }
            }),
            Ok(7)
        );
        assert_eq!(attempts, 2);

        options
            .set_option(SetSocketOption::NonBlocking(&false))
            .unwrap();
        attempts = 0;
        assert_eq!(
            options.send_poller_with_effective_nonblocking(&Ready, true, || {
                attempts += 1;
                Err::<usize, _>(AxError::WouldBlock)
            }),
            Err(AxError::WouldBlock)
        );
        assert_eq!(attempts, 1);
    }

    #[derive(Debug, Eq, PartialEq)]
    enum TransferAttempt {
        OppositeEndpointBlocked,
    }

    #[test]
    fn transfer_poller_returns_wrapped_opposite_endpoint_block() {
        let options = GeneralOptions::new();
        let mut attempts = 0;

        let outcome = options
            .transfer_poller(&Ready, SocketTransferDirection::Send, false, || {
                attempts += 1;
                Ok(TransferAttempt::OppositeEndpointBlocked)
            })
            .unwrap();

        assert_eq!(outcome, TransferAttempt::OppositeEndpointBlocked);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn transfer_poller_consumes_pending_error_for_both_directions() {
        for direction in [
            SocketTransferDirection::Receive,
            SocketTransferDirection::Send,
        ] {
            let options = GeneralOptions::new();
            options.set_pending_error(LinuxError::ECONNRESET);
            let mut attempted = false;

            let error = options
                .transfer_poller(&Ready, direction, false, || {
                    attempted = true;
                    Ok(())
                })
                .unwrap_err();

            assert_eq!(LinuxError::from(error), LinuxError::ECONNRESET);
            assert!(!attempted);
        }
    }
}
