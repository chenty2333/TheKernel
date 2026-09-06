use core::num::NonZeroU64;

use crate::{MappingSnapshot, MmError};

/// Typed memory access that caused a page fault.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FaultAccess {
    /// Data read.
    Read,
    /// Data write.
    Write,
    /// Instruction fetch.
    Execute,
}

/// Absolute, page-aligned userspace address identifying a delegated fault page.
///
/// This is deliberately part of [`FaultKey`] identity. Unlike a VMA-relative
/// offset, it remains stable when one logical mapping is split or its surviving
/// range acquires a different start address.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FaultPageAddress(usize);

impl FaultPageAddress {
    /// Absolute page-aligned userspace address.
    pub const fn get(self) -> usize {
        self.0
    }
}

macro_rules! nonzero_fault_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Builds an identity, rejecting zero.
            pub const fn new(raw: u64) -> Result<Self, MmError> {
                match NonZeroU64::new(raw) {
                    Some(raw) => Ok(Self(raw)),
                    None => Err(MmError::InvalidIdentity),
                }
            }

            /// Integer identity.
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

nonzero_fault_id!(
    FaultHandlerId,
    "Consumer-owned identity of a fault handler port."
);
nonzero_fault_id!(
    FaultRequestId,
    "Consumer-owned identity of a queued fault request."
);

/// Stable authority key for one delegated fault page.
///
/// Identity is exactly the address space, logical mapping, consumer-supplied
/// fault epoch, absolute page address, and access. For a userfaultfd adapter,
/// the generation is the registration/fault epoch: topology-only VMA splits
/// and `mprotect` preserve it, while mapping replacement allocates a new epoch.
/// Handler detach/close revokes the separate [`FaultHandlerId`] authority at
/// the broker lifecycle boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FaultKey {
    address_space: crate::AddressSpaceId,
    mapping: crate::MappingId,
    generation: crate::MappingGeneration,
    page_address: FaultPageAddress,
    access: FaultAccess,
}

impl FaultKey {
    /// Builds a key for a fault address contained by `snapshot`.
    pub fn from_address(
        snapshot: MappingSnapshot,
        address: usize,
        access: FaultAccess,
    ) -> Result<Self, MmError> {
        if !snapshot.range().user_range().contains_address(address) {
            return Err(MmError::RangeNotMapped);
        }
        Self::check_access(snapshot, access)?;
        Ok(Self {
            address_space: snapshot.address_space(),
            mapping: snapshot.mapping(),
            generation: snapshot.generation(),
            page_address: FaultPageAddress(snapshot.range().page_size().align_down(address)),
            access,
        })
    }

    /// Address-space identity.
    pub const fn address_space(self) -> crate::AddressSpaceId {
        self.address_space
    }

    /// Mapping identity.
    pub const fn mapping(self) -> crate::MappingId {
        self.mapping
    }

    /// Consumer-supplied mapping or registration/fault epoch frozen at admission.
    pub const fn generation(self) -> crate::MappingGeneration {
        self.generation
    }

    /// Absolute page-aligned address frozen at fault admission.
    pub const fn page_address(self) -> FaultPageAddress {
        self.page_address
    }

    /// Fault access.
    pub const fn access(self) -> FaultAccess {
        self.access
    }

    /// Revalidates authority, page coverage/alignment, and current fault access
    /// before admitting a task to the lower broker.
    pub fn revalidate_admission(self, current: MappingSnapshot) -> Result<(), MmError> {
        self.revalidate_completion(current)?;
        Self::check_access(current, self.access)
    }

    /// Revalidates authority and absolute page coverage/alignment before
    /// irreversible resolver publication.
    ///
    /// This deliberately does not recheck current access. A userfaultfd
    /// MISSING request may remain resolvable after `mprotect(PROT_NONE)`; the
    /// blocked fault retries and observes the new protection after resolution.
    pub fn revalidate_completion(self, current: MappingSnapshot) -> Result<(), MmError> {
        if self.address_space != current.address_space()
            || self.mapping != current.mapping()
            || self.generation != current.generation()
        {
            return Err(MmError::StaleGeneration);
        }
        if !current
            .range()
            .user_range()
            .contains_address(self.page_address.get())
        {
            return Err(MmError::RangeNotMapped);
        }
        if !current
            .range()
            .page_size()
            .is_aligned(self.page_address.get())
        {
            return Err(MmError::Unaligned);
        }
        Ok(())
    }

    fn check_access(snapshot: MappingSnapshot, access: FaultAccess) -> Result<(), MmError> {
        let allowed = match access {
            FaultAccess::Read => snapshot.access().readable(),
            FaultAccess::Write => snapshot.access().writable(),
            FaultAccess::Execute => snapshot.access().executable(),
        };
        if !allowed {
            return Err(MmError::AccessDenied);
        }
        Ok(())
    }
}

/// Linux-visible fault classification before a lower mechanism resolves it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FaultType {
    /// Missing anonymous/file page.
    Missing,
    /// Minor fault with backing already resident elsewhere.
    Minor,
    /// Write-protect event.
    WriteProtect,
    /// Private COW break.
    Cow,
    /// File-backed fault not delegated to userspace.
    File,
}

/// Immutable request passed to a consumer-owned bounded broker mechanism.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultRequest {
    key: FaultKey,
    handler: FaultHandlerId,
    fault_type: FaultType,
}

impl FaultRequest {
    /// Builds a typed fault request.
    pub const fn new(key: FaultKey, handler: FaultHandlerId, fault_type: FaultType) -> Self {
        Self {
            key,
            handler,
            fault_type,
        }
    }

    /// Stable generation key.
    pub const fn key(self) -> FaultKey {
        self.key
    }

    /// Target lower handler identity.
    pub const fn handler(self) -> FaultHandlerId {
        self.handler
    }

    /// Fault class.
    pub const fn fault_type(self) -> FaultType {
        self.fault_type
    }
}

/// Stable typed failure disposition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FaultFailure {
    /// Linux-visible protection/address failure, normally mapped to SIGSEGV.
    Segmentation,
    /// Backing-object failure, normally mapped to SIGBUS.
    Bus,
    /// Memory pressure prevented resolution.
    OutOfMemory,
    /// Backing I/O failed.
    Io,
    /// Mapping changed and the caller may retry within a finite budget.
    Retry,
}

/// Typed resolver result; it does not contain a frame or page-table object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FaultDisposition {
    /// Consumer supplies an initialized mechanism-owned page.
    Supply,
    /// Consumer requests a zero-filled page.
    ZeroFill,
    /// Existing backing may continue without supplying a new page.
    Continue,
    /// Consumer requests a write-protection state change.
    WriteProtect,
    /// Typed failure.
    Failure(FaultFailure),
    /// Request was cancelled by its waiter or mapping lifecycle.
    Cancelled,
    /// Handler detached or closed.
    HandlerDetached,
}

/// Explicit finite queue limits consumed by admission policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultCapacity {
    per_address_space: u32,
    per_handler: u32,
    global: u32,
}

impl FaultCapacity {
    /// Builds nonzero finite limits.
    pub const fn new(
        per_address_space: u32,
        per_handler: u32,
        global: u32,
    ) -> Result<Self, MmError> {
        if per_address_space == 0 || per_handler == 0 || global == 0 {
            return Err(MmError::ZeroLength);
        }
        if per_address_space == u32::MAX || per_handler == u32::MAX || global == u32::MAX {
            return Err(MmError::UnboundedLimit);
        }
        Ok(Self {
            per_address_space,
            per_handler,
            global,
        })
    }

    /// Per-address-space request limit.
    pub const fn per_address_space(self) -> u32 {
        self.per_address_space
    }

    /// Per-handler request limit.
    pub const fn per_handler(self) -> u32 {
        self.per_handler
    }

    /// Global request limit.
    pub const fn global(self) -> u32 {
        self.global
    }
}

/// Consumer-supplied current queue load before admitting one new request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultLoad {
    address_space: u32,
    handler: u32,
    global: u32,
}

impl FaultLoad {
    /// Builds exact current counts.
    pub const fn new(address_space: u32, handler: u32, global: u32) -> Self {
        Self {
            address_space,
            handler,
            global,
        }
    }

    /// Current address-space count.
    pub const fn address_space(self) -> u32 {
        self.address_space
    }

    /// Current handler count.
    pub const fn handler(self) -> u32 {
        self.handler
    }

    /// Current global count.
    pub const fn global(self) -> u32 {
        self.global
    }
}

/// Caller-supplied lower-broker facts for one fault admission attempt.
///
/// A new request consumes one request slot and therefore carries exact current
/// load for all three Layer 2 request quotas. An exact request snapshot comes
/// from the lower broker's coalescing lookup and represents only one additional
/// waiter on an already-accounted, still-coalescible request. The policy
/// compares the complete snapshot with the request being admitted; page-only
/// matches are rejected. A visible terminal must not be supplied through this
/// context: its later same-key fault is a new request.
///
/// This value is advisory until the lower broker atomically admits the waiter.
/// The adapter must keep the lookup, policy check, and lower admission in the
/// same broker critical section. Lower waiter capacity and request liveness
/// remain Layer 1 mechanism responsibilities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FaultAdmissionContext {
    /// No exact request exists; admitting this fault would consume a request credit.
    NewRequest {
        /// Finite request limits enforced by Layer 2 policy.
        capacity: FaultCapacity,
        /// Exact current request counts before publication.
        load: FaultLoad,
    },
    /// The lower broker reported one exact non-visible request to coalesce with.
    ExactRequest {
        /// Complete immutable request snapshot returned by the lower broker.
        broker_request: FaultRequest,
    },
}

impl FaultAdmissionContext {
    /// Describes admission of one request which is not present in the lower broker.
    pub const fn new_request(capacity: FaultCapacity, load: FaultLoad) -> Self {
        Self::NewRequest { capacity, load }
    }

    /// Describes one additional waiter on an exact lower-broker request.
    pub const fn exact_request(broker_request: FaultRequest) -> Self {
        Self::ExactRequest { broker_request }
    }
}

/// Resource class admitted by [`FaultAdmission`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultAdmissionKind {
    /// The lower broker must publish and account one new request plus one waiter.
    NewRequest,
    /// The lower broker must add only one waiter to an exact live request.
    CoalescedWaiter,
}

impl FaultAdmissionKind {
    /// Whether this admission must acquire a new request credit.
    pub const fn consumes_request_credit(self) -> bool {
        matches!(self, Self::NewRequest)
    }
}

/// Consumer-owned broker lifecycle observed by policy admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultLifecycleState {
    /// Requests may be admitted.
    Open,
    /// Existing requests drain, but new requests are rejected.
    Closing,
    /// Handler has detached and requests must fail/wake.
    Detached,
    /// Broker/port is fully closed.
    Closed,
}

/// Stateless admission policy for a typed fault request.
pub struct FaultAdmission;

impl FaultAdmission {
    /// Checks lifecycle and revalidates the complete request before a task can
    /// sleep. All three finite request quotas apply unless the lower broker
    /// reports this exact request as still coalescible.
    pub fn check(
        request: FaultRequest,
        current: MappingSnapshot,
        context: FaultAdmissionContext,
        lifecycle: FaultLifecycleState,
    ) -> Result<FaultAdmissionPermit, MmError> {
        match lifecycle {
            FaultLifecycleState::Open => {}
            FaultLifecycleState::Closing => return Err(MmError::Closing),
            FaultLifecycleState::Detached => return Err(MmError::TearingDown),
            FaultLifecycleState::Closed => return Err(MmError::Closed),
        }
        request.key.revalidate_admission(current)?;
        let kind = match context {
            FaultAdmissionContext::NewRequest { capacity, load } => {
                if load.address_space >= capacity.per_address_space
                    || load.handler >= capacity.per_handler
                    || load.global >= capacity.global
                {
                    return Err(MmError::QuotaExceeded);
                }
                FaultAdmissionKind::NewRequest
            }
            FaultAdmissionContext::ExactRequest { broker_request } => {
                if broker_request != request {
                    return Err(MmError::FaultRequestMismatch);
                }
                FaultAdmissionKind::CoalescedWaiter
            }
        };
        Ok(FaultAdmissionPermit { request, kind })
    }
}

/// Opaque proof that Layer 2 lifecycle, identity, access, and applicable
/// request-quota checks admitted one fault attempt.
///
/// This permit does not prove that the lower broker successfully published a
/// request or waiter. In particular, a coalesced permit remains subject to an
/// atomic exact-match recheck and finite waiter capacity in Layer 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "an admitted fault must be submitted or cancelled by the adapter"]
pub struct FaultAdmissionPermit {
    request: FaultRequest,
    kind: FaultAdmissionKind,
}

impl FaultAdmissionPermit {
    /// Admitted request.
    pub const fn request(self) -> FaultRequest {
        self.request
    }

    /// Request-credit class selected from the lower-broker facts.
    pub const fn kind(self) -> FaultAdmissionKind {
        self.kind
    }
}

/// Opaque proof that a reply still targets the exact mapping generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "a revalidated fault completion must be committed through the lower port"]
pub struct FaultCompletionPermit {
    request: FaultRequest,
    disposition: FaultDisposition,
}

impl FaultCompletionPermit {
    /// Revalidated request.
    pub const fn request(self) -> FaultRequest {
        self.request
    }

    /// Typed resolver disposition.
    pub const fn disposition(self) -> FaultDisposition {
        self.disposition
    }
}

/// Rejects a late reply to a replaced mapping before mechanism publication.
pub fn validate_fault_completion(
    request: FaultRequest,
    current: MappingSnapshot,
    disposition: FaultDisposition,
) -> Result<FaultCompletionPermit, MmError> {
    request.key.revalidate_completion(current)?;
    Ok(FaultCompletionPermit {
        request,
        disposition,
    })
}

/// Port implemented by a lower generic bounded fault broker.
///
/// The trait deliberately leaves storage, locks, waiter ownership, readiness,
/// coalescing, cancellation, and wakeup mechanics to Layer 1. Linux MM policy
/// passes only typed admission/completion values across this seam.
pub trait FaultPort {
    /// Lower broker ticket/handle type.
    type Ticket;
    /// Lower mechanism error type.
    type Error;

    /// Atomically publishes one previously admitted request/waiter pair.
    ///
    /// [`FaultAdmissionKind::CoalescedWaiter`] must still match one exact
    /// non-visible lower request and acquire an independent waiter slot; the
    /// Layer 2 permit is not mechanism-level proof that either condition still
    /// holds.
    fn submit(&mut self, permit: FaultAdmissionPermit) -> Result<Self::Ticket, Self::Error>;

    /// Completes one lower ticket after generation revalidation.
    fn complete(
        &mut self,
        ticket: Self::Ticket,
        permit: FaultCompletionPermit,
    ) -> Result<(), Self::Error>;

    /// Cancels one independently owned waiter/request ticket.
    fn cancel(&mut self, ticket: Self::Ticket) -> Result<(), Self::Error>;

    /// Detaches one handler and makes lower storage wake/cancel its requests.
    fn detach(&mut self, handler: FaultHandlerId) -> Result<(), Self::Error>;
}
