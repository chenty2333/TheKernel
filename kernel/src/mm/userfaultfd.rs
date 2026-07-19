//! Address-space adapter for the bounded generic fault broker.
//!
//! Linux-visible validation remains in `thekernel-linux-mm`; queue identity,
//! coalescing, and waiter ownership remain in `thekernel-axfault`.  This
//! dormant adapter only owns the per-address-space handler registry.  It does
//! not register mappings or route page faults yet.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axfault::FaultBroker;
use axpoll::PollSet;
use thekernel_linux_mm::{
    FaultDisposition, FaultHandlerId, FaultRequest, MmError, UffdRegistrationTable,
};

use super::AddrSpace;

pub(crate) const UFFD_MAX_HANDLERS: usize = 16;
pub(crate) const UFFD_MAX_REGISTRATIONS: usize = 64;
pub(crate) const UFFD_MAX_REQUESTS: usize = 64;
pub(crate) const UFFD_MAX_WAITERS: usize = 128;
pub(crate) const UFFD_POLL_CAPACITY: usize = 256;

pub(crate) type UffdPollSet = PollSet<UFFD_POLL_CAPACITY>;
type UffdBroker = FaultBroker<FaultRequest, FaultHandlerId, FaultDisposition>;
type UffdRegistrations = UffdRegistrationTable<UFFD_MAX_REGISTRATIONS>;

static NEXT_UFFD_HANDLER_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_handler_id() -> AxResult<FaultHandlerId> {
    let raw = NEXT_UFFD_HANDLER_ID
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| AxError::NoMemory)?;
    FaultHandlerId::new(raw).map_err(uffd_policy_error)
}

pub(crate) fn uffd_policy_error(error: MmError) -> AxError {
    match error {
        MmError::CapacityExceeded | MmError::IdExhausted => AxError::NoMemory,
        MmError::Busy | MmError::OwnerBusy | MmError::UffdRegistrationOverlap => {
            AxError::ResourceBusy
        }
        MmError::AccessDenied => AxError::OperationNotPermitted,
        MmError::RangeNotMapped | MmError::StaleGeneration => AxError::BadAddress,
        MmError::Closing | MmError::TearingDown | MmError::Closed => AxError::BadState,
        _ => AxError::InvalidInput,
    }
}

fn broker_config_error(error: axfault::BrokerConfigError) -> AxError {
    match error {
        axfault::BrokerConfigError::NoMemory => AxError::NoMemory,
        axfault::BrokerConfigError::UnboundedCapacity => AxError::InvalidInput,
        axfault::BrokerConfigError::BrokerIdentityExhausted => AxError::NoMemory,
        _ => AxError::InvalidInput,
    }
}

struct UffdHandlerState {
    id: FaultHandlerId,
    readiness: Arc<UffdPollSet>,
}

/// Lazily allocated state for one address space.
///
/// The registration and handler arrays are fixed-size.  The broker reserves
/// its complete request and waiter storage during construction, so observing
/// readiness and claiming an event never allocates.
pub(crate) struct UffdAddressSpaceState {
    #[allow(dead_code)]
    pub(crate) registrations: UffdRegistrations,
    broker: UffdBroker,
    handlers: [Option<UffdHandlerState>; UFFD_MAX_HANDLERS],
}

impl UffdAddressSpaceState {
    pub(crate) fn try_new_boxed() -> AxResult<Box<Self>> {
        let broker = UffdBroker::try_new(UFFD_MAX_REQUESTS, UFFD_MAX_WAITERS)
            .map_err(broker_config_error)?;
        let state = Self {
            registrations: UffdRegistrations::new(1).map_err(uffd_policy_error)?,
            broker,
            handlers: core::array::from_fn(|_| None),
        };
        Box::try_new(state).map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn attach_handler(
        &mut self,
        readiness: Arc<UffdPollSet>,
    ) -> AxResult<FaultHandlerId> {
        let slot = self
            .handlers
            .iter()
            .position(Option::is_none)
            .ok_or(AxError::NoMemory)?;
        let id = allocate_handler_id()?;
        self.handlers[slot] = Some(UffdHandlerState { id, readiness });
        Ok(id)
    }

    fn handler(&self, id: FaultHandlerId) -> AxResult<&UffdHandlerState> {
        self.handlers
            .iter()
            .flatten()
            .find(|handler| handler.id == id)
            .ok_or(AxError::BadFileDescriptor)
    }

    pub(crate) fn detach_handler(&mut self, id: FaultHandlerId) -> AxResult<Arc<UffdPollSet>> {
        let slot = self
            .handlers
            .iter()
            .position(|handler| handler.as_ref().is_some_and(|handler| handler.id == id))
            .ok_or(AxError::BadFileDescriptor)?;
        self.registrations
            .detach_handler(id)
            .map_err(uffd_policy_error)?;
        self.broker
            .detach_handler(id, FaultDisposition::HandlerDetached);
        self.handlers[slot]
            .take()
            .map(|handler| handler.readiness)
            .ok_or(AxError::BadState)
    }

    fn has_handlers(&self) -> bool {
        self.handlers.iter().any(Option::is_some)
    }

    pub(crate) fn pending(&self, id: FaultHandlerId) -> AxResult<bool> {
        self.handler(id)?;
        Ok(self.broker.has_pending(id))
    }

    pub(crate) fn claim_next(
        &mut self,
        id: FaultHandlerId,
    ) -> AxResult<Option<DeliveredUffdEvent>> {
        self.handler(id)?;
        Ok(self
            .broker
            .claim_next(id)
            .map(|snapshot| DeliveredUffdEvent {
                request: *snapshot.key(),
            }))
    }

    #[cfg(test)]
    pub(crate) fn admit_test_request(
        &mut self,
        handler: FaultHandlerId,
        request: FaultRequest,
    ) -> axfault::FaultAdmission {
        assert_eq!(request.handler(), handler);
        self.handler(handler).expect("test handler is live");
        self.broker
            .admit(handler, request)
            .expect("test broker has capacity")
    }

    #[cfg(test)]
    pub(crate) fn complete_test_request(&mut self, request: axfault::RequestToken) {
        self.broker
            .complete(
                request,
                FaultDisposition::ZeroFill,
                axfault::CompletionVisibility::Visible,
            )
            .expect("test request is live");
    }
}

impl Drop for UffdAddressSpaceState {
    fn drop(&mut self) {
        // AddrSpace destruction can outlive a non-CLOEXEC userfaultfd OFD.
        // Publish terminal ownership and wake every retained file context;
        // the file then observes an inert old-mm binding rather than an error.
        for handler in self.handlers.iter().flatten() {
            self.broker
                .detach_handler(handler.id, FaultDisposition::HandlerDetached);
            handler.readiness.wake();
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DeliveredUffdEvent {
    request: FaultRequest,
}

impl DeliveredUffdEvent {
    pub(crate) const fn request(self) -> FaultRequest {
        self.request
    }
}

/// Ownership retired by final OFD close.  Wake and destruction happen only
/// after the caller releases the address-space lock.
pub(crate) struct DetachedUffdHandler {
    readiness: Arc<UffdPollSet>,
    retired_state: Option<Box<UffdAddressSpaceState>>,
}

impl DetachedUffdHandler {
    pub(crate) fn finish(self) {
        self.readiness.wake();
        drop(self.retired_state);
    }
}

impl AddrSpace {
    pub(crate) const fn needs_uffd_state(&self) -> bool {
        self.uffd.is_none()
    }

    /// Installs a state candidate prepared outside the address-space lock and
    /// attaches one handler.  `WouldBlock` asks the caller to allocate a
    /// candidate and retry.  A racing winner leaves an unused candidate owned
    /// by the caller for lock-external destruction.
    pub(crate) fn attach_uffd_handler(
        &mut self,
        candidate: &mut Option<Box<UffdAddressSpaceState>>,
        readiness: Arc<UffdPollSet>,
    ) -> AxResult<FaultHandlerId> {
        let installed = self.uffd.is_none();
        if installed {
            self.uffd = Some(candidate.take().ok_or(AxError::WouldBlock)?);
        }
        let attached = self
            .uffd
            .as_mut()
            .expect("userfault state was installed")
            .attach_handler(readiness);
        if attached.is_err()
            && installed
            && self
                .uffd
                .as_ref()
                .is_some_and(|state| !state.has_handlers())
        {
            *candidate = self.uffd.take();
        }
        attached
    }

    pub(crate) fn detach_uffd_handler(
        &mut self,
        handler: FaultHandlerId,
    ) -> AxResult<DetachedUffdHandler> {
        let (readiness, retire_state) = {
            let state = self.uffd.as_mut().ok_or(AxError::BadFileDescriptor)?;
            let readiness = state.detach_handler(handler)?;
            (readiness, !state.has_handlers())
        };
        let retired_state =
            retire_state.then(|| self.uffd.take().expect("empty userfault state disappeared"));
        Ok(DetachedUffdHandler {
            readiness,
            retired_state,
        })
    }

    pub(crate) fn uffd_handler_pending(&self, handler: FaultHandlerId) -> AxResult<bool> {
        self.uffd
            .as_ref()
            .ok_or(AxError::BadFileDescriptor)?
            .pending(handler)
    }

    pub(crate) fn claim_uffd_event(
        &mut self,
        handler: FaultHandlerId,
    ) -> AxResult<Option<DeliveredUffdEvent>> {
        self.uffd
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .claim_next(handler)
    }
}

#[cfg(test)]
mod tests {
    use thekernel_linux_mm::{
        FaultAccess, FaultKey, FaultType, MappingAccess, MappingKind, MappingSnapshot,
    };

    use super::*;

    fn request(handler: FaultHandlerId, page: usize) -> FaultRequest {
        let snapshot = MappingSnapshot::from_raw(
            1,
            2,
            1,
            page,
            4096,
            4096,
            MappingAccess::new(true, true, false).bits(),
            MappingKind::AnonymousPrivate,
            true,
            false,
        )
        .unwrap();
        FaultRequest::new(
            FaultKey::from_address(snapshot, page, FaultAccess::Read).unwrap(),
            handler,
            FaultType::Missing,
        )
    }

    #[test]
    fn pending_is_derived_from_authoritative_broker_phase() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let readiness = Arc::new(UffdPollSet::new());
        let handler = state.attach_handler(readiness).unwrap();
        let admission = state
            .broker
            .admit(handler, request(handler, 0x1000))
            .unwrap();

        assert!(state.pending(handler).unwrap());
        state
            .broker
            .complete(
                admission.request(),
                FaultDisposition::ZeroFill,
                axfault::CompletionVisibility::Visible,
            )
            .unwrap();
        assert!(!state.pending(handler).unwrap());
        assert!(state.claim_next(handler).unwrap().is_none());
    }

    #[test]
    fn claim_moves_event_out_of_readiness_without_a_side_counter() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let readiness = Arc::new(UffdPollSet::new());
        let handler = state.attach_handler(readiness).unwrap();
        state
            .broker
            .admit(handler, request(handler, 0x2000))
            .unwrap();

        assert!(state.pending(handler).unwrap());
        let event = state.claim_next(handler).unwrap().unwrap();
        assert_eq!(event.request().key().page_address().get(), 0x2000);
        assert!(!state.pending(handler).unwrap());
        assert!(state.claim_next(handler).unwrap().is_none());
    }

    #[test]
    fn detach_removes_handler_and_returns_wake_owner() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let readiness = Arc::new(UffdPollSet::new());
        let handler = state.attach_handler(readiness.clone()).unwrap();
        assert!(state.has_handlers());

        let detached_readiness = state.detach_handler(handler).unwrap();
        assert!(Arc::ptr_eq(&detached_readiness, &readiness));
        assert!(!state.has_handlers());
        assert_eq!(state.pending(handler), Err(AxError::BadFileDescriptor));
        detached_readiness.wake();
    }

    #[test]
    fn policy_permission_gate_maps_to_eperm() {
        assert_eq!(
            uffd_policy_error(MmError::AccessDenied),
            AxError::OperationNotPermitted
        );
    }
}
