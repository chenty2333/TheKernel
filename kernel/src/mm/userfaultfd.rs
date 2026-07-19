//! Address-space adapter for the bounded generic fault broker.
//!
//! Linux-visible validation remains in `thekernel-linux-mm`; queue identity,
//! coalescing, and waiter ownership remain in `thekernel-axfault`.  This
//! dormant adapter now owns bounded REGISTER/UNREGISTER transactions and the
//! per-address-space handler registry. It still does not route page faults or
//! expose COPY/ZEROPAGE/WAKE resolution.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axfault::{CompletionVisibility, FaultBroker};
use axpoll::PollSet;
use thekernel_linux_mm::{
    AddressSpaceId, FaultDisposition, FaultHandlerId, FaultRequest, MappingGeneration, MappingId,
    MappingSnapshot, MmError, PageRange, UffdApiState, UffdIoctls, UffdRegisterMode,
    UffdRegistrationId, UffdRegistrationIntent, UffdRegistrationReplacement,
    UffdRegistrationRequest, UffdRegistrationTable,
};

use super::AddrSpace;

pub(crate) const UFFD_MAX_HANDLERS: usize = 16;
pub(crate) const UFFD_MAX_REGISTRATIONS: usize = 64;
pub(crate) const UFFD_MAX_REQUESTS: usize = 64;
pub(crate) const UFFD_MAX_WAITERS: usize = 128;
pub(crate) const UFFD_POLL_CAPACITY: usize = 256;
const UFFD_MAX_TXN_FRAGMENTS: usize = UFFD_MAX_REGISTRATIONS;

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

#[derive(Clone, Copy)]
struct UffdWakeSpan {
    handler: FaultHandlerId,
    address_space: AddressSpaceId,
    mapping: MappingId,
    generation: MappingGeneration,
    range: PageRange,
}

impl UffdWakeSpan {
    fn matches(self, handler: FaultHandlerId, request: FaultRequest) -> bool {
        let key = request.key();
        handler == self.handler
            && request.handler() == self.handler
            && key.address_space() == self.address_space
            && key.mapping() == self.mapping
            && key.generation() == self.generation
            && self
                .range
                .user_range()
                .contains_address(key.page_address().get())
    }
}

struct UffdTxnScratch {
    snapshots: Vec<MappingSnapshot>,
    removed: Vec<UffdRegistrationId>,
    register_replacements: Vec<UffdRegistrationRequest>,
    mapping_replacements: Vec<UffdRegistrationReplacement>,
    wake_spans: Vec<UffdWakeSpan>,
}

impl UffdTxnScratch {
    fn try_new() -> AxResult<Self> {
        fn reserved<T>() -> AxResult<Vec<T>> {
            let mut values = Vec::new();
            values
                .try_reserve_exact(UFFD_MAX_TXN_FRAGMENTS)
                .map_err(|_| AxError::NoMemory)?;
            Ok(values)
        }

        Ok(Self {
            snapshots: reserved()?,
            removed: reserved()?,
            register_replacements: reserved()?,
            mapping_replacements: reserved()?,
            wake_spans: reserved()?,
        })
    }

    fn clear_transaction(&mut self) {
        self.removed.clear();
        self.register_replacements.clear();
        self.mapping_replacements.clear();
        self.wake_spans.clear();
    }
}

/// Wake ownership collected while the address-space lock is held.
///
/// The caller must invoke [`Self::finish`] only after releasing that lock.
#[must_use = "release the address-space lock, then finish deferred UFFD wake ownership"]
pub(crate) struct DeferredUffdWake {
    fault_completion: Option<Arc<UffdPollSet>>,
    handlers: [Option<Arc<UffdPollSet>>; UFFD_MAX_HANDLERS],
}

impl DeferredUffdWake {
    fn empty() -> Self {
        Self {
            fault_completion: None,
            handlers: core::array::from_fn(|_| None),
        }
    }

    fn add_handler(&mut self, readiness: Arc<UffdPollSet>) {
        if self
            .handlers
            .iter()
            .flatten()
            .any(|current| Arc::ptr_eq(current, &readiness))
        {
            return;
        }
        let slot = self
            .handlers
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("affected UFFD handlers exceed fixed handler capacity");
        *slot = Some(readiness);
    }

    pub(crate) fn finish(self) {
        if let Some(completion) = self.fault_completion {
            completion.wake();
        }
        for readiness in self.handlers.into_iter().flatten() {
            readiness.wake();
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.fault_completion.is_none() && self.handlers.iter().all(Option::is_none)
    }
}

/// Lazily allocated state for one address space.
///
/// The registration and handler arrays are fixed-size.  The broker reserves
/// its complete request and waiter storage during construction, so observing
/// readiness and claiming an event never allocates.
pub(crate) struct UffdAddressSpaceState {
    pub(crate) registrations: UffdRegistrations,
    broker: UffdBroker,
    handlers: [Option<UffdHandlerState>; UFFD_MAX_HANDLERS],
    fault_completion: Arc<UffdPollSet>,
    scratch: UffdTxnScratch,
}

impl UffdAddressSpaceState {
    pub(crate) fn try_new_boxed() -> AxResult<Box<Self>> {
        let broker = UffdBroker::try_new(UFFD_MAX_REQUESTS, UFFD_MAX_WAITERS)
            .map_err(broker_config_error)?;
        let state = Self {
            registrations: UffdRegistrations::new(1).map_err(uffd_policy_error)?,
            broker,
            handlers: core::array::from_fn(|_| None),
            fault_completion: Arc::try_new(UffdPollSet::new()).map_err(|_| AxError::NoMemory)?,
            scratch: UffdTxnScratch::try_new()?,
        };
        Box::try_new(state).map_err(|_| AxError::NoMemory)
    }

    fn take_snapshot_scratch(&mut self) -> Vec<MappingSnapshot> {
        core::mem::take(&mut self.scratch.snapshots)
    }

    fn restore_snapshot_scratch(&mut self, snapshots: Vec<MappingSnapshot>) {
        debug_assert!(self.scratch.snapshots.is_empty());
        self.scratch.snapshots = snapshots;
    }

    fn project_registration_epochs(
        snapshots: &mut [MappingSnapshot],
        registrations: &UffdRegistrations,
    ) -> AxResult {
        for snapshot in snapshots {
            if let Some(epoch) = registrations
                .epoch_for_mapping(snapshot.address_space(), snapshot.mapping())
                .map_err(uffd_policy_error)?
            {
                *snapshot = snapshot.with_generation(epoch);
            }
        }
        Ok(())
    }

    pub(crate) fn register_range(
        &mut self,
        api: &UffdApiState,
        handler: FaultHandlerId,
        range: PageRange,
        mode: UffdRegisterMode,
        snapshots: &mut [MappingSnapshot],
    ) -> AxResult<UffdIoctls> {
        self.handler(handler)?;
        Self::project_registration_epochs(snapshots, &self.registrations)?;
        let intent =
            UffdRegistrationIntent::new(handler, range, mode).map_err(uffd_policy_error)?;
        let plan = self
            .registrations
            .preflight_register_delta(api, intent, snapshots)
            .map_err(uffd_policy_error)?;
        if plan.is_noop() {
            return Ok(UffdIoctls::MISSING_RANGE_PROFILE);
        }
        if plan.removed() > self.scratch.removed.capacity()
            || plan.replacements() > self.scratch.register_replacements.capacity()
        {
            return Err(AxError::NoMemory);
        }

        self.scratch.clear_transaction();
        let result = (|| {
            self.registrations
                .replay_register_delta(
                    plan,
                    intent,
                    snapshots,
                    |id| self.scratch.removed.push(id),
                    |request| self.scratch.register_replacements.push(request),
                )
                .map_err(uffd_policy_error)?;

            let commit = if self.scratch.removed.is_empty() {
                let plan = self
                    .registrations
                    .preflight_register(api, &self.scratch.register_replacements)
                    .map_err(uffd_policy_error)?;
                self.registrations.commit_register(
                    plan,
                    &self.scratch.register_replacements,
                    |_| {},
                )
            } else {
                let plan = self
                    .registrations
                    .preflight_replace(
                        api,
                        &self.scratch.removed,
                        &self.scratch.register_replacements,
                    )
                    .map_err(uffd_policy_error)?;
                self.registrations.commit_replace(
                    plan,
                    &self.scratch.removed,
                    &self.scratch.register_replacements,
                    |_| {},
                )
            };
            commit
                .map(|_| UffdIoctls::MISSING_RANGE_PROFILE)
                .map_err(uffd_policy_error)
        })();
        self.scratch.clear_transaction();
        result
    }

    fn snapshot_for_fragment(
        snapshots: &[MappingSnapshot],
        address_space: AddressSpaceId,
        mapping: MappingId,
        range: PageRange,
    ) -> AxResult<MappingSnapshot> {
        snapshots
            .iter()
            .copied()
            .find(|snapshot| {
                snapshot.address_space() == address_space
                    && snapshot.mapping() == mapping
                    && snapshot.range().contains(range)
            })
            .ok_or(AxError::BadState)
    }

    pub(crate) fn unregister_range(
        &mut self,
        api: &UffdApiState,
        range: PageRange,
        snapshots: &mut [MappingSnapshot],
    ) -> AxResult<DeferredUffdWake> {
        let address_space = self
            .registrations
            .validate_unregister_vmas(api, range, snapshots)
            .map_err(uffd_policy_error)?;
        Self::project_registration_epochs(snapshots, &self.registrations)?;

        self.scratch.clear_transaction();
        let result = (|| {
            for registration in self.registrations.intersecting(address_space, range) {
                self.handler(registration.handler())?;
                if self.scratch.removed.len() == self.scratch.removed.capacity()
                    || self.scratch.wake_spans.len() == self.scratch.wake_spans.capacity()
                {
                    return Err(AxError::NoMemory);
                }
                let wake_start = registration.range().start().max(range.start());
                let wake_end = registration.range().end().min(range.end());
                let wake_range = PageRange::with_page_size(
                    wake_start,
                    wake_end.checked_sub(wake_start).ok_or(AxError::BadState)?,
                    range.page_size(),
                )
                .map_err(uffd_policy_error)?;
                self.scratch.removed.push(registration.id());
                self.scratch.wake_spans.push(UffdWakeSpan {
                    handler: registration.handler(),
                    address_space: registration.address_space(),
                    mapping: registration.mapping(),
                    generation: registration.generation(),
                    range: wake_range,
                });

                for survivor in [
                    (registration.range().start(), wake_start),
                    (wake_end, registration.range().end()),
                ] {
                    let (start, end) = survivor;
                    if start == end {
                        continue;
                    }
                    if self.scratch.mapping_replacements.len()
                        == self.scratch.mapping_replacements.capacity()
                    {
                        return Err(AxError::NoMemory);
                    }
                    let survivor = PageRange::with_page_size(
                        start,
                        end.checked_sub(start).ok_or(AxError::BadState)?,
                        range.page_size(),
                    )
                    .map_err(uffd_policy_error)?;
                    let current = Self::snapshot_for_fragment(
                        snapshots,
                        registration.address_space(),
                        registration.mapping(),
                        survivor,
                    )?;
                    let request = registration
                        .refreshed_fragment(current, survivor)
                        .map_err(uffd_policy_error)?;
                    self.scratch
                        .mapping_replacements
                        .push(UffdRegistrationReplacement::new(registration.id(), request));
                }
            }

            if self.scratch.removed.is_empty() {
                return Ok(DeferredUffdWake::empty());
            }
            let plan = self
                .registrations
                .preflight_mapping_replace(
                    &self.scratch.removed,
                    &self.scratch.mapping_replacements,
                )
                .map_err(uffd_policy_error)?;
            self.registrations
                .commit_mapping_replace(
                    plan,
                    &self.scratch.removed,
                    &self.scratch.mapping_replacements,
                    |_| {},
                )
                .map_err(uffd_policy_error)?;

            let mut deferred = DeferredUffdWake::empty();
            let (completed, released) = {
                let wake_spans = &self.scratch.wake_spans;
                let completed = self.broker.complete_where(
                    |snapshot| {
                        wake_spans
                            .iter()
                            .copied()
                            .any(|span| span.matches(*snapshot.handler(), *snapshot.key()))
                    },
                    FaultDisposition::Cancelled,
                    CompletionVisibility::Visible,
                );
                let released = self.broker.release_where(|snapshot| {
                    wake_spans
                        .iter()
                        .copied()
                        .any(|span| span.matches(*snapshot.handler(), *snapshot.key()))
                });
                (completed, released)
            };
            if completed.requests_completed() != 0 {
                // Readiness is a hint. Waking every affected handler keeps the
                // hot broker path to one bounded scan and DeferredUffdWake
                // deduplicates shared handler readiness objects.
                for index in 0..self.scratch.wake_spans.len() {
                    let span = self.scratch.wake_spans[index];
                    deferred.add_handler(
                        self.handler(span.handler)
                            .expect("registered UFFD handler disappeared under address-space lock")
                            .readiness
                            .clone(),
                    );
                }
            }
            if completed.waiters_released() + released.waiters_released() != 0 {
                deferred.fault_completion = Some(self.fault_completion.clone());
            }
            Ok(deferred)
        })();
        self.scratch.clear_transaction();
        result
    }

    #[cfg(test)]
    pub(crate) fn set_test_snapshots(&mut self, snapshots: &[MappingSnapshot]) -> AxResult {
        if snapshots.len() > self.scratch.snapshots.capacity() {
            return Err(AxError::NoMemory);
        }
        self.scratch.snapshots.clear();
        self.scratch.snapshots.extend_from_slice(snapshots);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn register_test_range(
        &mut self,
        api: &UffdApiState,
        handler: FaultHandlerId,
        range: PageRange,
        mode: UffdRegisterMode,
    ) -> AxResult<UffdIoctls> {
        let mut snapshots = self.take_snapshot_scratch();
        let result = self.register_range(api, handler, range, mode, &mut snapshots);
        self.restore_snapshot_scratch(snapshots);
        result
    }

    #[cfg(test)]
    pub(crate) fn unregister_test_range(
        &mut self,
        api: &UffdApiState,
        range: PageRange,
    ) -> AxResult<DeferredUffdWake> {
        let mut snapshots = self.take_snapshot_scratch();
        let result = self.unregister_range(api, range, &mut snapshots);
        self.restore_snapshot_scratch(snapshots);
        result
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

    pub(crate) fn detach_handler(&mut self, id: FaultHandlerId) -> AxResult<DeferredUffdWake> {
        let slot = self
            .handlers
            .iter()
            .position(|handler| handler.as_ref().is_some_and(|handler| handler.id == id))
            .ok_or(AxError::BadFileDescriptor)?;
        self.registrations
            .detach_handler(id)
            .map_err(uffd_policy_error)?;
        let completed = self
            .broker
            .detach_handler(id, FaultDisposition::HandlerDetached);
        let readiness = self.handlers[slot]
            .take()
            .map(|handler| handler.readiness)
            .ok_or(AxError::BadState)?;
        let mut deferred = DeferredUffdWake::empty();
        deferred.add_handler(readiness);
        if completed.waiters_released() != 0 {
            deferred.fault_completion = Some(self.fault_completion.clone());
        }
        Ok(deferred)
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

    #[cfg(test)]
    pub(crate) fn observe_test_waiter(
        &self,
        waiter: axfault::WaiterToken,
    ) -> Result<axfault::WaiterObservation<FaultDisposition>, axfault::WaiterError> {
        self.broker.waiter(waiter)
    }

    #[cfg(test)]
    pub(crate) fn fault_completion_for_test(&self) -> Arc<UffdPollSet> {
        self.fault_completion.clone()
    }
}

impl Drop for UffdAddressSpaceState {
    fn drop(&mut self) {
        // AddrSpace destruction can outlive a non-CLOEXEC userfaultfd OFD.
        // Publish terminal ownership and wake every retained file context;
        // the file then observes an inert old-mm binding rather than an error.
        let mut wake_completion = false;
        for handler in self.handlers.iter().flatten() {
            let completed = self
                .broker
                .detach_handler(handler.id, FaultDisposition::HandlerDetached);
            handler.readiness.wake();
            wake_completion |= completed.waiters_released() != 0;
        }
        if wake_completion {
            self.fault_completion.wake();
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
    wake: DeferredUffdWake,
    retired_state: Option<Box<UffdAddressSpaceState>>,
}

impl DetachedUffdHandler {
    pub(crate) fn finish(self) {
        self.wake.finish();
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

    pub(crate) fn register_uffd_range(
        &mut self,
        api: &UffdApiState,
        handler: FaultHandlerId,
        range: PageRange,
        mode: UffdRegisterMode,
    ) -> AxResult<UffdIoctls> {
        let mut snapshots = self
            .uffd
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .take_snapshot_scratch();
        snapshots.clear();
        let scanned = self.append_uffd_mapping_snapshots(range, &mut snapshots);
        let result = match scanned {
            Ok(()) => self
                .uffd
                .as_mut()
                .expect("UFFD state disappeared under address-space lock")
                .register_range(api, handler, range, mode, &mut snapshots),
            Err(error) => Err(error),
        };
        self.uffd
            .as_mut()
            .expect("UFFD state disappeared under address-space lock")
            .restore_snapshot_scratch(snapshots);
        result
    }

    pub(crate) fn unregister_uffd_range(
        &mut self,
        api: &UffdApiState,
        range: PageRange,
    ) -> AxResult<DeferredUffdWake> {
        let mut snapshots = self
            .uffd
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .take_snapshot_scratch();
        snapshots.clear();
        let scanned = self.append_uffd_mapping_snapshots(range, &mut snapshots);
        let result = match scanned {
            Ok(()) => self
                .uffd
                .as_mut()
                .expect("UFFD state disappeared under address-space lock")
                .unregister_range(api, range, &mut snapshots),
            Err(error) => Err(error),
        };
        self.uffd
            .as_mut()
            .expect("UFFD state disappeared under address-space lock")
            .restore_snapshot_scratch(snapshots);
        result
    }

    pub(crate) fn detach_uffd_handler(
        &mut self,
        handler: FaultHandlerId,
    ) -> AxResult<DetachedUffdHandler> {
        let (wake, retire_state) = {
            let state = self.uffd.as_mut().ok_or(AxError::BadFileDescriptor)?;
            let wake = state.detach_handler(handler)?;
            (wake, !state.has_handlers())
        };
        let retired_state =
            retire_state.then(|| self.uffd.take().expect("empty userfault state disappeared"));
        Ok(DetachedUffdHandler {
            wake,
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
    use memory_addr::PAGE_SIZE_4K;
    use thekernel_linux_mm::{
        FaultAccess, FaultKey, FaultType, MappingAccess, MappingKind, MappingSnapshot, UFFD_API,
    };

    use super::*;

    fn snapshot(
        mapping: u64,
        generation: u64,
        start: usize,
        length: usize,
        kind: MappingKind,
    ) -> MappingSnapshot {
        MappingSnapshot::from_raw(
            1,
            mapping,
            generation,
            start,
            length,
            PAGE_SIZE_4K,
            MappingAccess::new(true, true, false).bits(),
            kind,
            true,
            false,
        )
        .unwrap()
    }

    fn page_range(start: usize, length: usize) -> PageRange {
        PageRange::new(start, length, PAGE_SIZE_4K).unwrap()
    }

    fn initialized_api() -> UffdApiState {
        let mut api = UffdApiState::new();
        let negotiation = api.prepare_raw(UFFD_API, 0).unwrap();
        api.commit(negotiation).unwrap();
        api
    }

    fn request_for(
        snapshot: MappingSnapshot,
        handler: FaultHandlerId,
        page: usize,
    ) -> FaultRequest {
        FaultRequest::new(
            FaultKey::from_address(snapshot, page, FaultAccess::Read).unwrap(),
            handler,
            FaultType::Missing,
        )
    }

    fn request(handler: FaultHandlerId, page: usize) -> FaultRequest {
        request_for(
            snapshot(2, 1, page, PAGE_SIZE_4K, MappingKind::AnonymousPrivate),
            handler,
            page,
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
    fn register_allows_holes_and_publishes_only_mapped_fragments() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [
            snapshot(2, 1, 0x1000, 0x2000, MappingKind::AnonymousPrivate),
            snapshot(3, 1, 0x5000, 0x2000, MappingKind::AnonymousPrivate),
        ];

        let ioctls = state
            .register_range(
                &api,
                handler,
                page_range(0x1000, 0x6000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            )
            .unwrap();

        assert_eq!(ioctls, UffdIoctls::MISSING_RANGE_PROFILE);
        let mut registered: Vec<_> = state.registrations.iter().collect();
        registered.sort_by_key(|registration| registration.range().start());
        assert_eq!(registered.len(), 2);
        assert_eq!(registered[0].range(), page_range(0x1000, 0x2000));
        assert_eq!(registered[1].range(), page_range(0x5000, 0x2000));
    }

    #[test]
    fn register_late_incompatible_vma_is_atomic() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [
            snapshot(2, 1, 0x1000, 0x2000, MappingKind::AnonymousPrivate),
            snapshot(3, 1, 0x5000, 0x2000, MappingKind::FilePrivate),
        ];

        assert_eq!(
            state.register_range(
                &api,
                handler,
                page_range(0x1000, 0x6000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            ),
            Err(AxError::InvalidInput)
        );
        assert!(state.registrations.is_empty());
    }

    #[test]
    fn register_foreign_overlap_is_busy_without_mutation() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let first = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let second = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [snapshot(
            2,
            1,
            0x1000,
            0x8000,
            MappingKind::AnonymousPrivate,
        )];
        state
            .register_range(
                &api,
                first,
                page_range(0x2000, 0x2000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            )
            .unwrap();

        assert_eq!(
            state.register_range(
                &api,
                second,
                page_range(0x3000, 0x2000),
                UffdRegisterMode::MISSING,
                &mut mappings,
            ),
            Err(AxError::ResourceBusy)
        );
        let registered: Vec<_> = state.registrations.iter().collect();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].handler(), first);
        assert_eq!(registered[0].range(), page_range(0x2000, 0x2000));
    }

    #[test]
    fn same_handler_subset_extension_and_bridge_stay_canonical() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mut mappings = [snapshot(
            2,
            1,
            0x1000,
            0x8000,
            MappingKind::AnonymousPrivate,
        )];
        for range in [
            page_range(0x2000, 0x2000),
            page_range(0x3000, 0x1000),
            page_range(0x1000, 0x3000),
            page_range(0x6000, 0x1000),
            page_range(0x4000, 0x2000),
        ] {
            state
                .register_range(
                    &api,
                    handler,
                    range,
                    UffdRegisterMode::MISSING,
                    &mut mappings,
                )
                .unwrap();
        }
        let registered: Vec<_> = state.registrations.iter().collect();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].range(), page_range(0x1000, 0x6000));
    }

    #[test]
    fn registration_capacity_failure_leaves_existing_table_unchanged() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        for index in 0..UFFD_MAX_REGISTRATIONS {
            let start = 0x1000 + index * 0x2000;
            let mut mapping = [snapshot(
                10 + index as u64,
                1,
                start,
                PAGE_SIZE_4K,
                MappingKind::AnonymousPrivate,
            )];
            state
                .register_range(
                    &api,
                    handler,
                    page_range(start, PAGE_SIZE_4K),
                    UffdRegisterMode::MISSING,
                    &mut mapping,
                )
                .unwrap();
        }
        assert_eq!(state.registrations.len(), UFFD_MAX_REGISTRATIONS);

        let start = 0x1000 + UFFD_MAX_REGISTRATIONS * 0x2000;
        let mut overflow = [snapshot(
            1000,
            1,
            start,
            PAGE_SIZE_4K,
            MappingKind::AnonymousPrivate,
        )];
        assert_eq!(
            state.register_range(
                &api,
                handler,
                page_range(start, PAGE_SIZE_4K),
                UffdRegisterMode::MISSING,
                &mut overflow,
            ),
            Err(AxError::NoMemory)
        );
        assert_eq!(state.registrations.len(), UFFD_MAX_REGISTRATIONS);
    }

    #[test]
    fn unregister_trims_mixed_owners_and_wakes_only_exact_registered_faults() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let first = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let second = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let first_mapping = snapshot(2, 7, 0x1000, 0x3000, MappingKind::AnonymousPrivate);
        let second_mapping = snapshot(3, 9, 0x6000, 0x3000, MappingKind::AnonymousPrivate);
        for (handler, mapping) in [(first, first_mapping), (second, second_mapping)] {
            let mut current = [mapping];
            state
                .register_range(
                    &api,
                    handler,
                    mapping.range(),
                    UffdRegisterMode::MISSING,
                    &mut current,
                )
                .unwrap();
        }

        let first_fault =
            state.admit_test_request(first, request_for(first_mapping, first, 0x1000));
        let second_fault =
            state.admit_test_request(second, request_for(second_mapping, second, 0x6000));
        let gap_mapping = snapshot(99, 1, 0x4000, 0x2000, MappingKind::AnonymousPrivate);
        let gap_fault = state.admit_test_request(first, request_for(gap_mapping, first, 0x4000));

        let mut current = [first_mapping, second_mapping];
        let deferred = state
            .unregister_range(&api, page_range(0x1000, 0x8000), &mut current)
            .unwrap();

        assert!(state.registrations.is_empty());
        assert_eq!(
            state.observe_test_waiter(first_fault.waiter()).unwrap(),
            axfault::WaiterObservation::Ready(FaultDisposition::Cancelled)
        );
        assert_eq!(
            state.observe_test_waiter(second_fault.waiter()).unwrap(),
            axfault::WaiterObservation::Ready(FaultDisposition::Cancelled)
        );
        assert_eq!(
            state.observe_test_waiter(gap_fault.waiter()).unwrap(),
            axfault::WaiterObservation::Pending
        );
        assert!(!deferred.is_empty());
        deferred.finish();
    }

    #[test]
    fn unregister_middle_preserves_left_and_right_registration_epoch() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let api = initialized_api();
        let mapping = snapshot(2, 17, 0x1000, 0x8000, MappingKind::AnonymousPrivate);
        let mut current = [mapping];
        state
            .register_range(
                &api,
                handler,
                mapping.range(),
                UffdRegisterMode::MISSING,
                &mut current,
            )
            .unwrap();

        let deferred = state
            .unregister_range(&api, page_range(0x4000, 0x2000), &mut current)
            .unwrap();
        assert!(deferred.is_empty());
        let mut fragments: Vec<_> = state.registrations.iter().collect();
        fragments.sort_by_key(|registration| registration.range().start());
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].range(), page_range(0x1000, 0x3000));
        assert_eq!(fragments[1].range(), page_range(0x6000, 0x3000));
        assert_eq!(fragments[0].generation(), fragments[1].generation());
        assert_eq!(fragments[0].generation().get(), 17);
        deferred.finish();
    }

    #[test]
    fn detach_removes_handler_and_returns_wake_owner() {
        let mut state = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = state.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        assert!(state.has_handlers());

        let detached = state.detach_handler(handler).unwrap();
        assert!(!detached.is_empty());
        assert!(!state.has_handlers());
        assert_eq!(state.pending(handler), Err(AxError::BadFileDescriptor));
        detached.finish();
    }

    #[test]
    fn policy_permission_gate_maps_to_eperm() {
        assert_eq!(
            uffd_policy_error(MmError::AccessDenied),
            AxError::OperationNotPermitted
        );
    }
}
