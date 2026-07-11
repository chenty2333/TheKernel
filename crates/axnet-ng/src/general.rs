use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    task::{Poll, Waker},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use axtask::future::{block_on, interruptible, timeout_at};

use crate::{
    net_stack::NetStack,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

const READY_RETRY_BUDGET: usize = 16;

const fn receive_timeout_error() -> AxError {
    // Linux reports expiry of SO_RCVTIMEO as EAGAIN/EWOULDBLOCK. ETIMEDOUT is
    // reserved for protocol-level timeout failures, not an exhausted receive
    // wait budget.
    AxError::WouldBlock
}

#[derive(Clone, Copy)]
struct PollBehavior {
    timeout_error: AxError,
    per_call_nonblocking: bool,
    consume_pending_error: bool,
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
            events | IoEvents::ERR
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

    pub fn register_waker(&self, stack: &NetStack, waker: &Waker) {
        stack
            .get_service()
            .register_waker(self.device_mask(), waker);
    }

    pub fn connect_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> AxResult<T> {
        self.run_poller(
            pollable,
            IoEvents::OUT,
            self.send_timeout(),
            PollBehavior {
                timeout_error: AxError::TimedOut,
                per_call_nonblocking: false,
                consume_pending_error: false,
            },
            f,
        )
    }

    pub fn send_poller_with_nonblocking<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        per_call_nonblocking: bool,
        f: F,
    ) -> AxResult<T> {
        // Linux's data-send paths consume sk_err before attempting to publish
        // any new payload. Connect completion uses `connect_poller` instead so
        // a failed nonblocking connect remains observable through SO_ERROR.
        self.run_poller(
            pollable,
            IoEvents::OUT,
            self.send_timeout(),
            PollBehavior {
                timeout_error: AxError::TimedOut,
                per_call_nonblocking,
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
        self.run_poller(
            pollable,
            IoEvents::IN,
            self.recv_timeout(),
            PollBehavior {
                timeout_error: receive_timeout_error(),
                per_call_nonblocking,
                consume_pending_error: true,
            },
            f,
        )
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
        let mut ready_retries = 0usize;

        loop {
            // Check on every retry, not only at entry. Another thread sharing
            // the socket can defer an error while this operation is blocked;
            // its ERR readiness must terminate the operation rather than make
            // the ALWAYS_POLL retry path spin forever.
            if behavior.consume_pending_error {
                self.consume_pending_error()?;
            }
            match f() {
                Err(AxError::WouldBlock)
                    if !self.nonblocking() && !behavior.per_call_nonblocking => {}
                other => return other,
            }

            // A stale or overly broad readiness indication must neither hide
            // timeout expiry nor monopolize a CPU forever. A small immediate
            // retry window preserves the common race-closing fast path; a
            // persistently ready source yields at a fixed budget.
            if deadline.is_some_and(|end| axhal::time::wall_time() >= end) {
                return Err(behavior.timeout_error);
            }
            let events = pollable.poll();
            if events.intersects(interest | IoEvents::ALWAYS_POLL) {
                ready_retries += 1;
                if ready_retries >= READY_RETRY_BUDGET {
                    axtask::yield_now();
                    ready_retries = 0;
                }
                continue;
            }
            ready_retries = 0;

            match block_on(timeout_at(
                deadline,
                interruptible(poll_fn(|cx| {
                    if pollable.poll().intersects(interest | IoEvents::ALWAYS_POLL) {
                        return Poll::Ready(());
                    }

                    pollable.register(cx, interest);
                    if pollable.poll().intersects(interest | IoEvents::ALWAYS_POLL) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })),
            )) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(AxError::Interrupted),
                Err(_) => return Err(behavior.timeout_error),
            }
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
    use super::*;
    use crate::options::{Configurable, GetSocketOption};

    struct Ready;

    impl Pollable for Ready {
        fn poll(&self) -> IoEvents {
            IoEvents::OUT
        }

        fn register(&self, _context: &mut core::task::Context<'_>, _events: IoEvents) {}
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
    fn pending_error_sets_readiness_and_precedes_send_attempt() {
        let options = GeneralOptions::new();
        options.set_pending_error(LinuxError::ECONNREFUSED);
        let events = options.add_pending_error_event(IoEvents::OUT);
        assert!(events.contains(IoEvents::OUT));
        assert!(events.contains(IoEvents::ERR));

        let mut attempted = false;
        let error = options
            .send_poller_with_nonblocking(&Ready, false, || {
                attempted = true;
                Ok(1usize)
            })
            .unwrap_err();
        assert_eq!(LinuxError::from(error), LinuxError::ECONNREFUSED);
        assert!(!attempted);
        let events = options.add_pending_error_event(IoEvents::OUT);
        assert!(events.contains(IoEvents::OUT));
        assert!(!events.contains(IoEvents::ERR));

        assert_eq!(
            options.send_poller_with_nonblocking(&Ready, false, || Ok(1usize)),
            Ok(1)
        );
    }

    #[test]
    fn pending_error_during_blocked_send_terminates_retry() {
        let options = GeneralOptions::new();
        let mut attempts = 0;
        let error = options
            .send_poller_with_nonblocking(&Ready, false, || {
                attempts += 1;
                options.set_pending_error(LinuxError::ECONNREFUSED);
                Err::<usize, _>(AxError::WouldBlock)
            })
            .unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::ECONNREFUSED);
        assert_eq!(attempts, 1);
    }
}
