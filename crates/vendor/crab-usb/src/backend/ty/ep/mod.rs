#[cfg(any(kmod, umod))]
use alloc::vec::Vec;
use alloc::{boxed::Box, sync::Arc};
use core::{
    any::Any,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use ax_sync::SpinLock;
#[cfg(any(kmod, umod))]
use usb_if::endpoint::{IsoPacketResult, TransferStatus};
use usb_if::{
    descriptor::EndpointType,
    endpoint::{EndpointInfo, RequestId, TransferCompletion, TransferRequest},
    err::TransferError,
};

#[cfg(any(kmod, umod))]
use super::transfer::Transfer;

mod ctrl;

pub(crate) trait EndpointOp: Send + Any + 'static {
    fn submit_request(&mut self, request: TransferRequest) -> Result<RequestId, TransferError>;

    fn reclaim_request(
        &mut self,
        id: RequestId,
    ) -> Option<Result<TransferCompletion, TransferError>>;

    fn register_waker(&self, id: RequestId, cx: &mut Context<'_>);

    fn cancel_request(&mut self, _id: RequestId) -> Result<(), TransferError> {
        Err(TransferError::NotSupported)
    }

    fn reset(&mut self) -> EndpointResetFuture {
        Box::pin(async { Err(TransferError::NotSupported) })
    }
}

pub type EndpointResetFuture =
    Pin<Box<dyn Future<Output = Result<(), TransferError>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointLifecycle {
    Active,
    Revoked,
    Disconnected,
}

struct EndpointInner {
    raw: Box<dyn EndpointOp>,
    lifecycle: EndpointLifecycle,
}

#[derive(Clone)]
pub struct EndpointHandle {
    info: EndpointInfo,
    inner: Arc<SpinLock<EndpointInner>>,
}

impl EndpointHandle {
    #[cfg(any(kmod, umod))]
    pub(crate) fn new(info: EndpointInfo, raw: impl EndpointOp) -> Self {
        Self {
            info,
            inner: Arc::new(SpinLock::new(EndpointInner {
                raw: Box::new(raw),
                lifecycle: EndpointLifecycle::Active,
            })),
        }
    }

    pub fn info(&self) -> EndpointInfo {
        self.info
    }

    pub fn submit(&self, request: TransferRequest) -> Result<RequestId, TransferError> {
        self.validate_request(&request)?;
        let mut inner = self.inner.lock();
        match inner.lifecycle {
            EndpointLifecycle::Active => inner.raw.submit_request(request),
            EndpointLifecycle::Revoked => Err(TransferError::EndpointRevoked),
            EndpointLifecycle::Disconnected => Err(TransferError::Disconnected),
        }
    }

    pub fn reclaim(&self, id: RequestId) -> Result<Option<TransferCompletion>, TransferError> {
        match self.inner.lock().raw.reclaim_request(id) {
            Some(result) => result.map(Some),
            None => Ok(None),
        }
    }

    pub fn poll_request(
        &self,
        id: RequestId,
        cx: &mut Context<'_>,
    ) -> Poll<Result<TransferCompletion, TransferError>> {
        let mut inner = self.inner.lock();
        match inner.raw.reclaim_request(id) {
            Some(res) => Poll::Ready(res),
            None => {
                inner.raw.register_waker(id, cx);
                match inner.raw.reclaim_request(id) {
                    Some(res) => Poll::Ready(res),
                    None => Poll::Pending,
                }
            }
        }
    }

    pub fn cancel(&self, id: RequestId) -> Result<(), TransferError> {
        self.inner.lock().raw.cancel_request(id)
    }

    /// Resets host-controller state for this endpoint after a successful
    /// `CLEAR_FEATURE(ENDPOINT_HALT)` request.
    pub fn reset(&self) -> EndpointResetFuture {
        self.inner.lock().raw.reset()
    }

    pub async fn wait(
        &self,
        request: TransferRequest,
    ) -> Result<TransferCompletion, TransferError> {
        let id = self.submit(request)?;
        self.wait_submitted(id).await
    }

    /// Wait for the exact request previously submitted to this endpoint.
    /// This consumes its completion without submitting a second transfer or
    /// cancelling an outstanding IN request which may not receive any data.
    /// Dropping this future does not cancel the request: its submitted buffer
    /// must remain owned until completion or proven controller shutdown.
    pub async fn wait_submitted(
        &self,
        id: RequestId,
    ) -> Result<TransferCompletion, TransferError> {
        EndpointRequestFuture { id, endpoint: self }.await
    }

    #[allow(unused)]
    pub(crate) fn with_raw_mut<T: EndpointOp, R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut inner = self.inner.lock();
        let d = inner.raw.as_mut() as &mut dyn Any;
        f(d.downcast_mut::<T>()
            .expect("EndpointHandle downcast_mut failed"))
    }

    pub(crate) fn revoke(&self) {
        self.inner.lock().lifecycle = EndpointLifecycle::Revoked;
    }

    pub(crate) fn disconnect(&self) {
        self.inner.lock().lifecycle = EndpointLifecycle::Disconnected;
    }

    pub(crate) fn reactivate(&self) {
        self.inner.lock().lifecycle = EndpointLifecycle::Active;
    }

    fn validate_request(&self, request: &TransferRequest) -> Result<(), TransferError> {
        let request_type = match request {
            TransferRequest::Control { .. } => EndpointType::Control,
            TransferRequest::Bulk { .. } => EndpointType::Bulk,
            TransferRequest::Interrupt { .. } => EndpointType::Interrupt,
            TransferRequest::Isochronous { .. } => EndpointType::Isochronous,
        };
        if request_type == self.info.transfer_type {
            Ok(())
        } else {
            Err(TransferError::InvalidEndpoint)
        }
    }
}

struct EndpointRequestFuture<'a> {
    id: RequestId,
    endpoint: &'a EndpointHandle,
}

impl Future for EndpointRequestFuture<'_> {
    type Output = Result<TransferCompletion, TransferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.endpoint.poll_request(this.id, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use core::task::Waker;
    use usb_if::endpoint::{EndpointAddress, TransferStatus};

    struct PendingIn {
        submissions: Arc<AtomicUsize>,
        ready: Arc<AtomicBool>,
        pending: Option<RequestId>,
    }

    impl EndpointOp for PendingIn {
        fn submit_request(&mut self, _: TransferRequest) -> Result<RequestId, TransferError> {
            assert!(self.pending.is_none(), "a second IN request was submitted");
            let id = RequestId::new(self.submissions.fetch_add(1, Ordering::SeqCst) as u64 + 1);
            self.pending = Some(id);
            Ok(id)
        }

        fn reclaim_request(
            &mut self,
            id: RequestId,
        ) -> Option<Result<TransferCompletion, TransferError>> {
            if self.pending != Some(id) || !self.ready.load(Ordering::SeqCst) {
                return None;
            }
            self.pending = None;
            Some(Ok(TransferCompletion {
                request_id: id,
                status: TransferStatus::Completed,
                actual_length: 3,
                iso_packets: Vec::new(),
            }))
        }

        fn register_waker(&self, _: RequestId, _: &mut Context<'_>) {}

        fn cancel_request(&mut self, _: RequestId) -> Result<(), TransferError> {
            panic!("a quiet event-IN request must not be cancelled before sending the command")
        }
    }

    #[test]
    fn wait_submitted_reuses_pending_in_and_reclaims_each_response_once() {
        let submissions = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let endpoint = EndpointHandle::new(
            EndpointInfo {
                address: EndpointAddress::new(0x81),
                transfer_type: EndpointType::Interrupt,
                direction: usb_if::transfer::Direction::In,
                max_packet_size: 16,
                packets_per_microframe: 1,
                interval: 1,
            },
            PendingIn {
                submissions: submissions.clone(),
                ready: ready.clone(),
                pending: None,
            },
        );
        let mut context = Context::from_waker(Waker::noop());
        for expected in 1..=2 {
            ready.store(false, Ordering::SeqCst);
            let mut buffer = [0; 16];
            let id = endpoint.submit(TransferRequest::interrupt_in(&mut buffer)).unwrap();
            let mut wait = core::pin::pin!(endpoint.wait_submitted(id));
            assert!(wait.as_mut().poll(&mut context).is_pending());
            assert_eq!(submissions.load(Ordering::SeqCst), expected);
            ready.store(true, Ordering::SeqCst);
            let Poll::Ready(Ok(completion)) = wait.as_mut().poll(&mut context) else {
                panic!("the exact pending response was not reclaimed");
            };
            assert_eq!(completion.request_id, id);
            assert_eq!(completion.actual_length, 3);
            assert!(endpoint.reclaim(id).unwrap().is_none());
        }
    }
}

#[cfg(any(kmod, umod))]
pub(crate) fn transfer_to_completion(id: RequestId, transfer: Transfer) -> TransferCompletion {
    let iso_packets = match &transfer.kind {
        usb_if::endpoint::TransferKind::Isochronous { packet_lengths } => packet_lengths
            .iter()
            .copied()
            .zip(transfer.iso_packet_actual_lengths.iter().copied())
            .map(|(requested_length, actual_length)| IsoPacketResult {
                requested_length,
                actual_length,
                status: TransferStatus::Completed,
            })
            .collect(),
        _ => Vec::new(),
    };

    TransferCompletion {
        request_id: id,
        status: TransferStatus::Completed,
        actual_length: transfer.transfer_len,
        iso_packets,
    }
}
