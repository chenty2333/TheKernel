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

    pub fn send_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> AxResult<T> {
        self.run_poller(pollable, IoEvents::OUT, self.send_timeout(), f)
    }

    pub fn recv_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> AxResult<T> {
        self.run_poller(pollable, IoEvents::IN, self.recv_timeout(), f)
    }

    fn run_poller<P: Pollable, F: FnMut() -> AxResult<T>, T>(
        &self,
        pollable: &P,
        interest: IoEvents,
        timeout: Option<Duration>,
        mut f: F,
    ) -> AxResult<T> {
        let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));

        loop {
            match f() {
                Err(AxError::WouldBlock) if !self.nonblocking() => {}
                other => return other,
            }

            let events = pollable.poll();
            if events.intersects(interest | IoEvents::ALWAYS_POLL) {
                continue;
            }

            if deadline.is_some_and(|end| axhal::time::wall_time() >= end) {
                return Err(AxError::TimedOut);
            }

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
                Err(_) => return Err(AxError::TimedOut),
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
                **error = self.pending_error.swap(0, Ordering::AcqRel);
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
}
