use alloc::{collections::VecDeque, sync::Arc};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use axsync::Mutex;

struct QueueState<T> {
    items: VecDeque<T>,
    reserved: usize,
    send_closed: bool,
    receive_closed: bool,
}

struct Shared<T> {
    capacity: usize,
    state: Mutex<QueueState<T>>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
    readers: PollSet,
    writers: PollSet,
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
    let shared = Arc::try_new(Shared {
        capacity,
        state: Mutex::new(QueueState {
            items,
            reserved: 0,
            send_closed: false,
            receive_closed: false,
        }),
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
        readers: PollSet::new(),
        writers: PollSet::new(),
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

    pub(super) fn register_write(&self, waker: &Waker) {
        self.shared.writers.register(waker);
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
        Recv { receiver: self }
    }

    pub(super) fn close(&self) {
        let changed = {
            let mut state = self.shared.state.lock();
            let changed = !state.receive_closed;
            state.receive_closed = true;
            changed
        };
        if changed {
            self.shared.readers.wake();
            self.shared.writers.wake();
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.shared.state.lock().items.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.shared.state.lock().items.len()
    }

    pub(super) fn register_read(&self, waker: &Waker) {
        self.shared.readers.register(waker);
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
}

impl<T> Future for Recv<'_, T> {
    type Output = Result<T, TryRecvError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match self.receiver.try_recv() {
            Ok(item) => Poll::Ready(Ok(item)),
            Err(TryRecvError::Closed) => Poll::Ready(Err(TryRecvError::Closed)),
            Err(TryRecvError::Empty) => {
                self.receiver.register_read(context.waker());
                match self.receiver.try_recv() {
                    Ok(item) => Poll::Ready(Ok(item)),
                    Err(TryRecvError::Closed) => Poll::Ready(Err(TryRecvError::Closed)),
                    Err(TryRecvError::Empty) => Poll::Pending,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
