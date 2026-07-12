use alloc::{collections::VecDeque, sync::Arc};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};

use axerrno::{AxError, AxResult};
use axpoll::{PollRegistration, PollSet};
use axsync::spin::SpinNoIrq;

use crate::general::poll_registration_error;

struct QueueState<T> {
    items: VecDeque<T>,
    reserved: usize,
    send_closed: bool,
    receive_closed: bool,
}

struct Shared<T> {
    capacity: usize,
    // Every operation is bounded and allocation-free after construction.
    // A non-sleeping lock lets final endpoint Drop publish queue closure
    // without entering the scheduler or invoking a waker.
    state: SpinNoIrq<QueueState<T>>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
    readers: Arc<PollSet>,
    writers: Arc<PollSet>,
}

pub(super) struct Sender<T> {
    shared: Arc<Shared<T>>,
}

pub(super) struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

#[derive(Debug)]
#[cfg(test)]
pub(super) enum TrySendError<T> {
    Full(T),
    Closed(T),
}

#[derive(Debug)]
pub(super) enum PermitSendError<T> {
    Closed(T),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TryRecvError {
    Empty,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReserveError {
    Full,
    Closed,
}

pub(super) struct SendPermit<T> {
    sender: Sender<T>,
    active: bool,
}

/// Completes the externally accounted portion of a successful receive before
/// making blocked writers runnable again.
#[must_use = "receive accounting must be completed before waking writers"]
pub(super) struct RecvCompletion<T> {
    shared: Arc<Shared<T>>,
}

pub(super) fn try_bounded<T>(capacity: usize) -> AxResult<(Sender<T>, Receiver<T>)> {
    if capacity == 0 {
        return Err(AxError::InvalidInput);
    }
    let mut items = VecDeque::new();
    items
        .try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    let readers = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
    let writers = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
    let shared = Arc::try_new(Shared {
        capacity,
        state: SpinNoIrq::new(QueueState {
            items,
            reserved: 0,
            send_closed: false,
            receive_closed: false,
        }),
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
        readers,
        writers,
    })
    .map_err(|_| AxError::NoMemory)?;
    Ok((
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared },
    ))
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Sender<T> {
    #[cfg(test)]
    pub(super) fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        let result = {
            let mut state = self.shared.state.lock();
            if state.receive_closed {
                Err(TrySendError::Closed(item))
            } else if state.items.len() + state.reserved >= self.shared.capacity {
                Err(TrySendError::Full(item))
            } else {
                // try_bounded() reserved the complete logical capacity, so a
                // push below that limit cannot enter the allocator.
                state.items.push_back(item);
                Ok(())
            }
        };
        if result.is_ok() {
            self.shared.readers.wake();
        }
        result
    }

    pub(super) fn try_reserve(&self, limit: usize) -> Result<SendPermit<T>, ReserveError> {
        let effective_limit = limit.min(self.shared.capacity);
        let result = {
            let mut state = self.shared.state.lock();
            if state.receive_closed {
                Err(ReserveError::Closed)
            } else if effective_limit == 0 || state.items.len() + state.reserved >= effective_limit
            {
                Err(ReserveError::Full)
            } else {
                state.reserved += 1;
                Ok(())
            }
        };
        result.map(|()| SendPermit {
            sender: self.clone(),
            active: true,
        })
    }

    pub(super) fn is_full(&self) -> bool {
        let state = self.shared.state.lock();
        state.items.len() + state.reserved >= self.shared.capacity
    }

    pub(super) fn is_closed(&self) -> bool {
        self.shared.state.lock().receive_closed
    }

    pub(super) fn write_poll_source(&self) -> Arc<PollSet> {
        self.shared.writers.clone()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.state.lock().send_closed = true;
            self.shared.readers.wake();
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.shared.receivers.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Receiver<T> {
    fn try_recv_inner(&self) -> Result<T, TryRecvError> {
        {
            let mut state = self.shared.state.lock();
            match state.items.pop_front() {
                Some(item) => Ok(item),
                None if state.send_closed || state.receive_closed => Err(TryRecvError::Closed),
                None => Err(TryRecvError::Empty),
            }
        }
    }

    pub(super) fn try_recv(&self) -> Result<T, TryRecvError> {
        let result = self.try_recv_inner();
        if result.is_ok() {
            self.shared.writers.wake();
        }
        result
    }

    pub(super) fn try_recv_deferred_wake(&self) -> Result<(T, RecvCompletion<T>), TryRecvError> {
        self.try_recv_inner().map(|item| {
            (
                item,
                RecvCompletion {
                    shared: self.shared.clone(),
                },
            )
        })
    }

    pub(super) fn recv(&self) -> Recv<'_, T> {
        Recv {
            receiver: self,
            registration: None,
        }
    }

    pub(super) fn close(&self) {
        {
            let mut state = self.shared.state.lock();
            state.receive_closed = true;
        }
        // The deferred finalizer calls this after Drop may already have
        // published receive_closed without waking. Always emit the eventual
        // wake edge, even when the state transition itself is idempotent.
        self.shared.readers.wake();
        self.shared.writers.wake();
    }

    /// Publishes receive closure and visits every already queued item without
    /// removing, dropping, allocating, sleeping, or invoking a waker.
    ///
    /// The visitor runs under the bounded queue's non-sleeping state lock and
    /// therefore must perform only fixed-cost close-state publication.
    pub(super) fn close_without_wake_and_visit(&self, mut visit: impl FnMut(&T)) {
        let mut state = self.shared.state.lock();
        state.receive_closed = true;
        for item in &state.items {
            visit(item);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.shared.state.lock().items.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.shared.state.lock().items.len()
    }

    pub(super) fn read_poll_source(&self) -> Arc<PollSet> {
        self.shared.readers.clone()
    }

    pub(super) fn wake_writers(&self) {
        self.shared.writers.wake();
    }
}

impl<T> RecvCompletion<T> {
    pub(super) fn complete(self) {
        self.shared.writers.wake();
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.shared.receivers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.close();
        }
    }
}

impl<T> SendPermit<T> {
    pub(super) fn send(mut self, item: T) -> Result<(), PermitSendError<T>> {
        let result = {
            let mut state = self.sender.shared.state.lock();
            state.reserved -= 1;
            if state.receive_closed {
                Err(PermitSendError::Closed(item))
            } else {
                state.items.push_back(item);
                Ok(())
            }
        };
        self.active = false;
        self.sender.shared.writers.wake();
        if result.is_ok() {
            self.sender.shared.readers.wake();
        }
        result
    }
}

impl<T> Drop for SendPermit<T> {
    fn drop(&mut self) {
        if self.active {
            self.sender.shared.state.lock().reserved -= 1;
            self.sender.shared.writers.wake();
        }
    }
}

pub(super) struct Recv<'a, T> {
    receiver: &'a Receiver<T>,
    registration: Option<PollRegistration<'a>>,
}

impl<T> Future for Recv<'_, T> {
    type Output = AxResult<T>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match self.receiver.try_recv() {
            Ok(item) => {
                self.registration.take();
                Poll::Ready(Ok(item))
            }
            Err(TryRecvError::Closed) => {
                self.registration.take();
                Poll::Ready(Err(AxError::ConnectionReset))
            }
            Err(TryRecvError::Empty) => {
                let needs_registration = if let Some(registration) = self.registration.as_mut() {
                    if registration.update(context.waker()).is_ok() {
                        false
                    } else {
                        registration.cancel();
                        true
                    }
                } else {
                    true
                };
                if needs_registration {
                    let source = self.receiver.read_poll_source();
                    match PollRegistration::single_owned(source, context.waker()) {
                        Ok(registration) => self.registration = Some(registration),
                        Err(error) => {
                            return Poll::Ready(Err(poll_registration_error(error)));
                        }
                    }
                }
                match self.receiver.try_recv() {
                    Ok(item) => {
                        self.registration.take();
                        Poll::Ready(Ok(item))
                    }
                    Err(TryRecvError::Closed) => {
                        self.registration.take();
                        Poll::Ready(Err(AxError::ConnectionReset))
                    }
                    Err(TryRecvError::Empty) => Poll::Pending,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::task::Waker;

    use super::*;

    #[test]
    fn recv_future_retains_updates_and_releases_one_source_token() {
        let (sender, receiver) = try_bounded(1).unwrap();
        let readers = receiver.read_poll_source();
        let mut recv = receiver.recv();
        let mut context = Context::from_waker(Waker::noop());

        assert!(Pin::new(&mut recv).poll(&mut context).is_pending());
        assert_eq!(readers.len(), 1);
        assert!(Pin::new(&mut recv).poll(&mut context).is_pending());
        assert_eq!(readers.len(), 1);

        sender.try_send(7).unwrap();
        assert!(readers.is_empty());
        assert_eq!(Pin::new(&mut recv).poll(&mut context), Poll::Ready(Ok(7)));
        assert!(readers.is_empty());
    }

    #[test]
    fn queue_is_bounded_and_receiver_close_is_observable() {
        let (sender, receiver) = try_bounded(2).unwrap();
        sender.try_send(1).unwrap();
        sender.try_send(2).unwrap();
        assert!(matches!(sender.try_send(3), Err(TrySendError::Full(3))));
        assert_eq!(receiver.try_recv(), Ok(1));
        receiver.close();
        assert!(matches!(sender.try_send(4), Err(TrySendError::Closed(4))));
        assert_eq!(receiver.try_recv(), Ok(2));
    }

    #[test]
    fn reservation_accounts_before_publication_and_rolls_back() {
        let (sender, receiver) = try_bounded::<usize>(1).unwrap();
        let permit = sender.try_reserve(1).unwrap();
        assert!(matches!(sender.try_reserve(1), Err(ReserveError::Full)));
        drop(permit);
        sender.try_reserve(1).unwrap().send(7).unwrap();
        assert_eq!(receiver.try_recv(), Ok(7));
    }

    #[test]
    fn close_without_wake_rejects_an_already_reserved_publication_and_visits_all_items() {
        let (sender, receiver) = try_bounded(4).unwrap();
        sender.try_send(1).unwrap();
        sender.try_send(2).unwrap();
        let permit = sender.try_reserve(4).unwrap();
        let mut visited = 0;
        receiver.close_without_wake_and_visit(|_| visited += 1);
        assert_eq!(visited, 2);
        assert!(matches!(permit.send(3), Err(PermitSendError::Closed(3))));
        assert!(matches!(sender.try_reserve(4), Err(ReserveError::Closed)));
    }
}
