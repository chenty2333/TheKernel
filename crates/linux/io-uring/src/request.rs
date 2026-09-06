use alloc::vec::Vec;
use core::num::NonZeroU64;

use crate::{IORING_MAX_CQ_ENTRIES, IORING_MAX_ENTRIES, IoUringError, RingId};

/// Operation class retained for cancellation, diagnostics, and close policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestOperation {
    /// `IORING_OP_NOP`.
    Nop,
    /// Positional `IORING_OP_READ`.
    Read,
    /// Positional `IORING_OP_WRITE`.
    Write,
    OpenAt2,
    /// `IORING_OP_FSYNC` / `IORING_OP_FDATASYNC`.
    Fsync,
    /// `IORING_OP_CLOSE`.
    Close,
    /// `IORING_OP_FADVISE`.
    Fadvise,
    /// `IORING_OP_SYNC_FILE_RANGE`.
    SyncFileRange,
    /// `IORING_OP_FALLOCATE`.
    Fallocate,
    /// `IORING_OP_SHUTDOWN`.
    Shutdown,
    /// One-shot `IORING_OP_POLL_ADD`.
    PollAdd,
    /// Relative `IORING_OP_TIMEOUT`.  The executor retains the copied
    /// timespec and can be detached by `ASYNC_CANCEL` before expiry.
    Timeout,
    /// `IORING_OP_TIMEOUT_REMOVE`.
    TimeoutRemove,
    /// `IORING_OP_POLL_REMOVE`.
    PollRemove,
    /// `IORING_OP_ASYNC_CANCEL`.
    AsyncCancel,
    /// `IORING_OP_PROVIDE_BUFFERS` publishes a range into a selected group.
    ProvideBuffers,
    /// `IORING_OP_REMOVE_BUFFERS` retires ready buffers from a selected group.
    RemoveBuffers,
    /// `IORING_OP_ACCEPT`.
    Accept,
    /// Provider-owned `IORING_OP_URING_CMD`.
    UringCmd,
    /// An SQE which must complete with a typed unsupported/invalid result.
    Rejected(u8),
}

impl RequestOperation {
    const fn cancellation_mode(self) -> CancellationMode {
        match self {
            // Read/accept become cancellable only once the adapter retains
            // them as a socket multishot owner.  Ordinary synchronous I/O
            // cannot race a cancellation while its submitter owns execution,
            // whereas the long-lived owner must be selectable by
            // ASYNC_CANCEL.
            Self::PollAdd | Self::Timeout | Self::Read | Self::Accept => {
                CancellationMode::Cancellable
            }
            Self::Nop
            | Self::Write
            | Self::OpenAt2
            | Self::Fsync
            | Self::Close
            | Self::Fadvise
            | Self::SyncFileRange
            | Self::Fallocate
            | Self::Shutdown
            | Self::PollRemove
            | Self::TimeoutRemove
            | Self::AsyncCancel
            | Self::ProvideBuffers
            | Self::RemoveBuffers
            | Self::UringCmd
            | Self::Rejected(_) => CancellationMode::Uncancellable,
        }
    }
}

/// Whether an issued execution mechanism can still honor terminal cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationMode {
    /// The adapter can detach/abort the lower mechanism before completing
    /// `Cancelled` or `Closing`.
    Cancellable,
    /// A provider owns execution from issue through retirement. Only a
    /// consuming provider cancellation control can authorize cancellation;
    /// generic cancel and final-close sweeps must leave it live.
    ProviderControlled,
    /// Execution crossed an irreversible VFS/effect boundary; only its
    /// ordinary executor completion may win terminal ownership.
    Uncancellable,
}

/// Immutable request metadata copied before admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestDescriptor {
    user_data: u64,
    operation: RequestOperation,
}

impl RequestDescriptor {
    /// Builds metadata for one copied SQE.
    pub const fn new(user_data: u64, operation: RequestOperation) -> Self {
        Self {
            user_data,
            operation,
        }
    }

    /// Opaque value copied to the terminal CQE.
    pub const fn user_data(self) -> u64 {
        self.user_data
    }

    /// Operation class used by policy and diagnostics.
    pub const fn operation(self) -> RequestOperation {
        self.operation
    }
}

/// Generation-scoped identity of one accepted request-table slot.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId {
    ring: RingId,
    slot: u32,
    generation: NonZeroU64,
}

impl RequestId {
    const fn new(ring: RingId, slot: u32, generation: NonZeroU64) -> Self {
        Self {
            ring,
            slot,
            generation,
        }
    }

    /// Ring which owns this request identity.
    pub const fn ring(self) -> RingId {
        self.ring
    }

    /// Bounded request-table slot.
    pub const fn slot(self) -> u32 {
        self.slot
    }

    /// Nonzero generation of the occupied slot.
    pub const fn generation(self) -> u64 {
        self.generation.get()
    }
}

/// Externally visible state of one live request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestState {
    /// Terminal CQ credit and a slot are reserved, but SQ consumption has not
    /// yet been committed.
    Reserved,
    /// SQ consumption was committed and execution has not started.
    Prepared,
    /// The adapter handed the request to an execution mechanism with this
    /// cancellation contract.
    Issued(CancellationMode),
    /// A retained asynchronous owner has claimed the request for one
    /// side-effecting execution. Cancellation/close cannot interleave the
    /// kernel action and its CQE publication.
    ShotInFlight(CancellationMode),
    /// Exactly one path owns the request's terminal transition.
    TerminalClaimed,
    /// A complete CQE is waiting for shared-ring publication.
    CompletionPending,
    /// A CQE plan is being written and release-published by the adapter.
    Publishing,
}

/// Ring admission and teardown phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestLifecycle {
    /// New requests may reserve terminal capacity.
    Open,
    /// Admission is closed while existing work reaches a terminal state.
    Closing,
    /// No executor owns work; unpublished and published completions may be
    /// discarded because no userspace mapping can consume them.
    Draining,
    /// All request and completion ownership has ended.
    Closed,
}

/// Why a path won terminal ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalCause {
    /// The execution mechanism produced its ordinary result.
    Completed,
    /// A cancellation request won before ordinary completion.
    Cancelled,
    /// Final-close processing won before ordinary completion.
    Closing,
    /// Admission succeeded but later preparation produced a terminal error.
    PreparationFailed,
}

/// Selector supported by the initial one-shot cancellation contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelSelector {
    /// Select the oldest cancellable request with matching `user_data`.
    UserData(u64),
    /// Select the oldest cancellable timeout with matching `user_data`.
    TimeoutUserData(u64),
    /// Select one exact generation-scoped request.
    Request(RequestId),
}

/// Provider cancellation is resolved only after the provider control has run
/// outside the ring lock.  A terminal CQE is created solely for `Cancelled`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderCancelOutcome {
    /// The provider proved cancellation and retired its resource owners.
    Cancelled,
    /// Release the cancellation fence so the real completion can retire the
    /// request, including a terminal callback already buffered by the adapter.
    InFlight,
}

/// One complete Linux CQE value, independent of shared-memory layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Completion {
    user_data: u64,
    result: i32,
    flags: u32,
}

impl Completion {
    /// Builds a complete terminal CQE value.
    pub const fn new(user_data: u64, result: i32, flags: u32) -> Self {
        Self {
            user_data,
            result,
            flags,
        }
    }

    /// Opaque request value.
    pub const fn user_data(self) -> u64 {
        self.user_data
    }

    /// Linux CQE result, including a negated errno when appropriate.
    pub const fn result(self) -> i32 {
        self.result
    }

    /// Linux CQE flags.
    pub const fn flags(self) -> u32 {
        self.flags
    }
}

/// Reversible pre-admission ownership of one request slot and terminal credit.
#[derive(Debug)]
#[must_use = "a request reservation must be committed or rolled back"]
pub struct RequestReservation {
    id: RequestId,
    descriptor: RequestDescriptor,
}

impl RequestReservation {
    /// Identity reserved for the future accepted request.
    pub const fn id(&self) -> RequestId {
        self.id
    }

    /// Immutable copied request metadata.
    pub const fn descriptor(&self) -> RequestDescriptor {
        self.descriptor
    }
}

/// Proof that SQ consumption was committed for one accepted request.
#[derive(Debug)]
#[must_use = "an accepted request must be issued or completed"]
pub struct PreparedRequest {
    id: RequestId,
    descriptor: RequestDescriptor,
}

impl PreparedRequest {
    /// Exact accepted request identity.
    pub const fn id(&self) -> RequestId {
        self.id
    }

    /// Immutable copied request metadata.
    pub const fn descriptor(&self) -> RequestDescriptor {
        self.descriptor
    }
}

/// Proof that an adapter execution mechanism owns one accepted request.
#[derive(Debug)]
#[must_use = "issued work must eventually claim a terminal transition"]
pub struct IssuedRequest {
    id: RequestId,
    descriptor: RequestDescriptor,
    cancellation_mode: CancellationMode,
}

impl IssuedRequest {
    /// Exact issued request identity suitable for an external completion key.
    pub const fn id(&self) -> RequestId {
        self.id
    }

    /// Immutable copied request metadata.
    pub const fn descriptor(&self) -> RequestDescriptor {
        self.descriptor
    }

    /// Cancellation contract atomically published at execution hand-off.
    pub const fn cancellation_mode(&self) -> CancellationMode {
        self.cancellation_mode
    }
}

/// Failed execution hand-off with the prepared proof returned for cleanup.
#[derive(Debug)]
pub struct RequestIssueError {
    error: IoUringError,
    prepared: PreparedRequest,
}

impl RequestIssueError {
    /// Typed race/stale-state failure.
    pub const fn error(&self) -> IoUringError {
        self.error
    }

    /// Prepared identity and descriptor which were not handed to execution.
    pub const fn prepared(&self) -> &PreparedRequest {
        &self.prepared
    }

    /// Recovers the proof for adapter-side prepared-resource rollback.
    pub fn into_prepared(self) -> PreparedRequest {
        self.prepared
    }
}

/// Unique ownership of one request's terminal transition.
#[derive(Debug)]
#[must_use = "terminal ownership must be converted into a completion"]
pub struct TerminalPermit {
    id: RequestId,
    descriptor: RequestDescriptor,
    cause: TerminalCause,
}

impl TerminalPermit {
    /// Request whose terminal transition is owned.
    pub const fn id(&self) -> RequestId {
        self.id
    }

    /// Immutable metadata used to build its CQE.
    pub const fn descriptor(&self) -> RequestDescriptor {
        self.descriptor
    }

    /// Winning terminal path.
    pub const fn cause(&self) -> TerminalCause {
        self.cause
    }
}

/// Generation-safe handle for one complete but unpublished CQE.
#[derive(Debug)]
#[must_use = "a completed request must be published or explicitly drained"]
pub struct CompletionToken {
    id: RequestId,
}

impl CompletionToken {
    /// Request represented by the pending CQE.
    pub const fn id(&self) -> RequestId {
        self.id
    }
}

/// Lock-external plan for one CQE write followed by CQ-tail release-store.
#[derive(Debug)]
#[must_use = "a CQE publication must be committed after release-storing its tail"]
pub struct CompletionPublication {
    id: RequestId,
    completion: Completion,
    slot: u32,
    new_tail: u32,
    /// A multishot notification leaves its issued owner and terminal credit
    /// live.  Only the final publication clears the request slot.
    terminal: bool,
}

impl CompletionPublication {
    /// Complete value to write before publishing the new tail.
    pub const fn completion(&self) -> Completion {
        self.completion
    }

    /// CQ array slot at which the complete value must be written.
    pub const fn slot(&self) -> u32 {
        self.slot
    }

    /// Monotonic wrapping CQ tail to release-store after the CQE write.
    pub const fn new_tail(&self) -> u32 {
        self.new_tail
    }

    /// Whether committing this publication retires the request.
    pub const fn terminal(&self) -> bool {
        self.terminal
    }
}

/// Bounded snapshot used by close and diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestProgress {
    lifecycle: RequestLifecycle,
    reserved: u32,
    prepared: u32,
    issued: u32,
    uncancellable_issued: u32,
    terminal_claimed: u32,
    completion_pending: u32,
    publishing: u32,
    terminal_credits: u32,
    published: u32,
}

impl RequestProgress {
    /// Current admission/teardown phase.
    pub const fn lifecycle(self) -> RequestLifecycle {
        self.lifecycle
    }

    /// Reversible reservations not yet reflected in SQ head.
    pub const fn reserved(self) -> u32 {
        self.reserved
    }

    /// Accepted requests not handed to execution.
    pub const fn prepared(self) -> u32 {
        self.prepared
    }

    /// Requests owned by an external execution mechanism.
    pub const fn issued(self) -> u32 {
        self.issued
    }

    /// Issued requests which close/cancel must wait for rather than complete.
    pub const fn uncancellable_issued(self) -> u32 {
        self.uncancellable_issued
    }

    /// Requests with a winning terminal path but no complete CQE yet.
    pub const fn terminal_claimed(self) -> u32 {
        self.terminal_claimed
    }

    /// Complete CQEs not yet published to the shared ring.
    pub const fn completion_pending(self) -> u32 {
        self.completion_pending
    }

    /// CQE publication transactions currently outside the policy core.
    pub const fn publishing(self) -> u32 {
        self.publishing
    }

    /// All charged terminal credits, including published CQEs not yet reaped.
    pub const fn terminal_credits(self) -> u32 {
        self.terminal_credits
    }

    /// CQEs visible between the validated CQ head and the core's CQ tail.
    pub const fn published(self) -> u32 {
        self.published
    }

    /// Every request-table slot has reached its terminal publication/drain.
    pub const fn requests_empty(self) -> bool {
        self.reserved == 0
            && self.prepared == 0
            && self.issued == 0
            && self.terminal_claimed == 0
            && self.completion_pending == 0
            && self.publishing == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryState {
    Reserved,
    Prepared,
    Issued(CancellationMode),
    ShotInFlight(CancellationMode),
    TerminalClaimed(TerminalCause),
    CompletionPending(Completion),
    Publishing(Completion),
}

impl EntryState {
    const fn public(self) -> RequestState {
        match self {
            Self::Reserved => RequestState::Reserved,
            Self::Prepared => RequestState::Prepared,
            Self::Issued(mode) => RequestState::Issued(mode),
            Self::ShotInFlight(mode) => RequestState::ShotInFlight(mode),
            Self::TerminalClaimed(_) => RequestState::TerminalClaimed,
            Self::CompletionPending(_) => RequestState::CompletionPending,
            Self::Publishing(_) => RequestState::Publishing,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestEntry {
    descriptor: RequestDescriptor,
    sequence: NonZeroU64,
    state: EntryState,
}

#[derive(Debug)]
struct RequestSlot {
    generation: Option<NonZeroU64>,
    entry: Option<RequestEntry>,
}

/// Committed lifecycle transitions. CQ head observations are aggregate: a
/// head advance proves slot reclamation, not per-request userspace processing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestTraceEvent {
    Reserved {
        id: RequestId,
        descriptor: RequestDescriptor,
    },
    Submitted {
        id: RequestId,
    },
    Issued {
        id: RequestId,
    },
    RolledBack {
        id: RequestId,
    },
    CompletionAccepted {
        id: RequestId,
        cause: TerminalCause,
        result: i32,
        flags: u32,
    },
    PublicationStarted {
        id: RequestId,
        terminal: bool,
    },
    PublicationRolledBack {
        id: RequestId,
        terminal: bool,
    },
    ProviderCancelSelected {
        id: RequestId,
    },
    ProviderCancelResolved {
        id: RequestId,
        outcome: ProviderCancelOutcome,
    },
    Published {
        id: RequestId,
        completion: Completion,
        tail: u32,
        terminal: bool,
    },
    Discarded {
        id: RequestId,
    },
    HeadReclaimed {
        ring: RingId,
        head: u32,
        count: u32,
    },
}

/// Fixed-capacity, generation-safe request and terminal-CQ policy registry.
///
/// The embedding kernel supplies external synchronization. Construction is the
/// only allocation point; all admission, completion, cancellation, reaping,
/// and close operations are allocation-free.
#[derive(Debug)]
pub struct RequestRegistry {
    observer: Option<fn(RequestTraceEvent)>,
    ring: RingId,
    capacity: u32,
    slots: Vec<RequestSlot>,
    free_slots: Vec<u32>,
    next_sequence: Option<NonZeroU64>,
    lifecycle: RequestLifecycle,
    cq_entries: u32,
    cq_head: u32,
    cq_tail: u32,
    terminal_credits: u32,
    publication_in_flight: Option<RequestId>,
    nonterminal_reservations: u32,
    nonterminal_reservation: Option<RequestId>,
    /// Whether each published CQ slot consumes a terminal request credit.
    /// MORE notifications occupy CQ space but must never refund the retained
    /// owner's final credit when userspace advances CQ head.
    published_terminal: Vec<bool>,
}

impl RequestRegistry {
    /// Allocates a bounded request table and configures terminal CQ capacity.
    pub fn new(ring: RingId, request_capacity: u32, cq_entries: u32) -> Result<Self, IoUringError> {
        if request_capacity == 0 || request_capacity > IORING_MAX_ENTRIES {
            return Err(IoUringError::RequestCapacityExceeded);
        }
        if cq_entries == 0 || !cq_entries.is_power_of_two() || cq_entries > IORING_MAX_CQ_ENTRIES {
            return Err(IoUringError::InvalidQueueGeometry);
        }
        let capacity = usize::try_from(request_capacity).map_err(|_| IoUringError::Overflow)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| IoUringError::AllocationFailed)?;
        for _ in 0..capacity {
            slots.push(RequestSlot {
                generation: NonZeroU64::new(1),
                entry: None,
            });
        }
        let mut free_slots = Vec::new();
        free_slots
            .try_reserve_exact(capacity)
            .map_err(|_| IoUringError::AllocationFailed)?;
        for slot in (0..request_capacity).rev() {
            free_slots.push(slot);
        }
        let cq_capacity = usize::try_from(cq_entries).map_err(|_| IoUringError::Overflow)?;
        let mut published_terminal = Vec::new();
        published_terminal
            .try_reserve_exact(cq_capacity)
            .map_err(|_| IoUringError::AllocationFailed)?;
        published_terminal.resize(cq_capacity, false);
        Ok(Self {
            observer: None,
            ring,
            capacity: request_capacity,
            slots,
            free_slots,
            next_sequence: NonZeroU64::new(1),
            lifecycle: RequestLifecycle::Open,
            cq_entries,
            cq_head: 0,
            cq_tail: 0,
            terminal_credits: 0,
            publication_in_flight: None,
            nonterminal_reservations: 0,
            nonterminal_reservation: None,
            published_terminal,
        })
    }

    /// Installs an embedding-kernel diagnostic observer. It runs under the
    /// caller's registry synchronization and must not block, allocate, or
    /// reenter the registry. Existing requests are not replayed.
    pub fn set_observer(&mut self, observer: Option<fn(RequestTraceEvent)>) {
        self.observer = observer;
    }

    fn observe(&self, event: RequestTraceEvent) {
        if let Some(observer) = self.observer {
            observer(event);
        }
    }

    /// Ring scope carried by every generated token.
    pub const fn ring(&self) -> RingId {
        self.ring
    }

    /// Fixed request-slot capacity allocated at construction.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Maximum simultaneously published completion entries.
    pub const fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// Last validated userspace CQ head.
    pub const fn completion_head(&self) -> u32 {
        self.cq_head
    }

    /// Core-owned CQ tail represented by returned publication plans.
    pub const fn completion_tail(&self) -> u32 {
        self.cq_tail
    }

    /// Reserves a request slot and terminal CQ credit before SQ admission.
    ///
    /// `CompletionQueueFull` is retryable without advancing the shared SQ
    /// head. The returned reservation remains reversible until committed.
    pub fn reserve(
        &mut self,
        descriptor: RequestDescriptor,
    ) -> Result<RequestReservation, IoUringError> {
        if self.lifecycle != RequestLifecycle::Open {
            return Err(match self.lifecycle {
                RequestLifecycle::Open => unreachable!(),
                RequestLifecycle::Closing => IoUringError::Closing,
                RequestLifecycle::Draining => IoUringError::Draining,
                RequestLifecycle::Closed => IoUringError::Closed,
            });
        }
        if self.terminal_credits >= self.cq_entries {
            return Err(IoUringError::CompletionQueueFull);
        }
        let sequence = self
            .next_sequence
            .ok_or(IoUringError::GenerationExhausted)?;
        let slot_index = match self.free_slots.last() {
            Some(slot) => *slot,
            None => {
                return Err(if self.slots.iter().any(|slot| slot.entry.is_some()) {
                    IoUringError::RequestCapacityExceeded
                } else {
                    IoUringError::GenerationExhausted
                });
            }
        };
        let slot = self
            .slots
            .get(usize::try_from(slot_index).map_err(|_| IoUringError::Overflow)?)
            .ok_or(IoUringError::RequestCapacityExceeded)?;
        if slot.entry.is_some() {
            return Err(IoUringError::InvalidRequestState);
        }
        let generation = slot.generation.ok_or(IoUringError::GenerationExhausted)?;
        let id = RequestId::new(self.ring, slot_index, generation);

        self.terminal_credits = self
            .terminal_credits
            .checked_add(1)
            .ok_or(IoUringError::Overflow)?;
        self.next_sequence = sequence.get().checked_add(1).and_then(NonZeroU64::new);
        let popped = self
            .free_slots
            .pop()
            .ok_or(IoUringError::RequestCapacityExceeded)?;
        if popped != slot_index {
            return Err(IoUringError::InvalidRequestState);
        }
        let slot = self
            .slots
            .get_mut(usize::try_from(slot_index).map_err(|_| IoUringError::Overflow)?)
            .ok_or(IoUringError::RequestCapacityExceeded)?;
        slot.entry = Some(RequestEntry {
            descriptor,
            sequence,
            state: EntryState::Reserved,
        });
        self.observe(RequestTraceEvent::Reserved { id, descriptor });
        Ok(RequestReservation { id, descriptor })
    }

    /// Rolls back admission before the adapter advances shared SQ head.
    pub fn rollback(&mut self, reservation: RequestReservation) -> Result<(), IoUringError> {
        self.require_state(reservation.id, RequestState::Reserved)?;
        self.clear_slot(reservation.id)?;
        self.refund_credits(1)?;
        self.observe(RequestTraceEvent::RolledBack { id: reservation.id });
        Ok(())
    }

    /// Commits SQ consumption for a previously reserved request.
    pub fn commit(
        &mut self,
        reservation: RequestReservation,
    ) -> Result<PreparedRequest, IoUringError> {
        let entry = self.entry_mut(reservation.id)?;
        if entry.state != EntryState::Reserved {
            return Err(IoUringError::InvalidRequestState);
        }
        entry.state = EntryState::Prepared;
        let descriptor = entry.descriptor;
        self.observe(RequestTraceEvent::Submitted { id: reservation.id });
        Ok(PreparedRequest {
            id: reservation.id,
            descriptor,
        })
    }

    /// Transfers a prepared request to the adapter's execution mechanism.
    pub fn issue(&mut self, prepared: PreparedRequest) -> Result<IssuedRequest, RequestIssueError> {
        self.issue_with_cancellation_mode(prepared, None)
    }

    /// Transfers a prepared request with an adapter-resolved cancellation
    /// contract.  URING_CMD derives this from its provider manifest after
    /// descriptor/OFD resolution, not from untrusted SQE syntax.
    pub fn issue_with_cancellation_mode(
        &mut self,
        prepared: PreparedRequest,
        override_mode: Option<CancellationMode>,
    ) -> Result<IssuedRequest, RequestIssueError> {
        let entry = match self.entry_mut(prepared.id) {
            Ok(entry) => entry,
            Err(error) => return Err(RequestIssueError { error, prepared }),
        };
        if entry.state != EntryState::Prepared {
            let error = if matches!(
                entry.state,
                EntryState::TerminalClaimed(_)
                    | EntryState::CompletionPending(_)
                    | EntryState::Publishing(_)
                    | EntryState::ShotInFlight(_)
            ) {
                IoUringError::TerminalAlreadyClaimed
            } else {
                IoUringError::InvalidRequestState
            };
            return Err(RequestIssueError { error, prepared });
        }
        let cancellation_mode =
            override_mode.unwrap_or_else(|| entry.descriptor.operation.cancellation_mode());
        entry.state = EntryState::Issued(cancellation_mode);
        let descriptor = entry.descriptor;
        self.observe(RequestTraceEvent::Issued { id: prepared.id });
        Ok(IssuedRequest {
            id: prepared.id,
            descriptor,
            cancellation_mode,
        })
    }

    /// Returns an immutable snapshot of one exact live request.
    pub fn request(
        &self,
        id: RequestId,
    ) -> Result<(RequestDescriptor, RequestState), IoUringError> {
        let entry = self.entry(id)?;
        Ok((entry.descriptor, entry.state.public()))
    }

    /// Whether every request admitted before `id` has reached a terminal
    /// transition.  IOSQE_IO_DRAIN orders execution, not CQE publication.
    pub fn prior_requests_terminal(&self, id: RequestId) -> Result<bool, IoUringError> {
        let sequence = self.entry(id)?.sequence;
        Ok(self.slots.iter().all(|slot| {
            let Some(entry) = slot.entry else {
                return true;
            };
            entry.sequence >= sequence
                || matches!(
                    entry.state,
                    EntryState::TerminalClaimed(_)
                        | EntryState::CompletionPending(_)
                        | EntryState::Publishing(_)
                )
        }))
    }

    /// Generation-safe liveness check used by retained executors before
    /// putting an owner back into a readiness table after `WouldBlock`.
    pub fn issued_is_live(&self, issued: &IssuedRequest) -> bool {
        self.entry(issued.id).is_ok_and(|entry| {
            entry.descriptor == issued.descriptor
                && entry.state == EntryState::Issued(issued.cancellation_mode)
        })
    }

    /// Returns whether one retained multishot owner is awaiting deferred CQE
    /// publication.  It remains owned by the same issued token but cannot
    /// begin a second shot until the task-work publication commits it back to
    /// `Issued`.
    pub fn issued_shot_publication_pending(&self, issued: &IssuedRequest) -> bool {
        self.entry(issued.id).is_ok_and(|entry| {
            entry.descriptor == issued.descriptor
                && entry.state == EntryState::ShotInFlight(issued.cancellation_mode)
                && self.publication_in_flight == Some(issued.id)
        })
    }

    /// Atomically excludes cancellation before a multishot executor performs
    /// an externally visible socket action.
    pub fn begin_nonterminal_shot(&mut self, issued: &IssuedRequest) -> Result<(), IoUringError> {
        if self.nonterminal_reservation.is_some() {
            return Err(IoUringError::Busy);
        }
        // Borrow this issued request's own eventual-final credit for the
        // intermediate MORE CQE. The credit is never taken from another
        // request, and remains charged in `terminal_credits` for its later
        // final transition after userspace reaps the MORE entry.
        let visible_terminal_credits = self
            .published_terminal
            .iter()
            .filter(|terminal| **terminal)
            .count();
        let visible_terminal_credits =
            u32::try_from(visible_terminal_credits).map_err(|_| IoUringError::Overflow)?;
        // `published_count` already includes visible terminal CQEs. Only
        // terminal credits whose CQE is not yet visible consume additional
        // future capacity here; then borrow this owner's own final credit.
        let other_terminal_credits = self
            .terminal_credits
            .checked_sub(visible_terminal_credits)
            .and_then(|unpublished| unpublished.checked_sub(1))
            .ok_or(IoUringError::InvalidRequestState)?;
        if self
            .published_count()?
            .checked_add(other_terminal_credits)
            .and_then(|used| used.checked_add(self.nonterminal_reservations))
            .ok_or(IoUringError::Overflow)?
            >= self.cq_entries
        {
            return Err(IoUringError::CompletionQueueFull);
        }
        let entry = self.entry_mut(issued.id)?;
        if entry.descriptor != issued.descriptor
            || entry.state != EntryState::Issued(issued.cancellation_mode)
        {
            return Err(IoUringError::InvalidRequestState);
        }
        entry.state = EntryState::ShotInFlight(issued.cancellation_mode);
        self.nonterminal_reservations = self
            .nonterminal_reservations
            .checked_add(1)
            .ok_or(IoUringError::Overflow)?;
        self.nonterminal_reservation = Some(issued.id);
        Ok(())
    }

    /// Excludes cancellation for one side-effecting operation which will
    /// publish only its final CQE.  Unlike a multishot MORE publication it
    /// consumes no extra CQ reservation; it exists for poll-driven work whose
    /// owner is temporarily removed from the readiness table while I/O runs.
    pub fn begin_side_effect(&mut self, issued: &IssuedRequest) -> Result<(), IoUringError> {
        let entry = self.entry_mut(issued.id)?;
        if entry.descriptor != issued.descriptor
            || entry.state != EntryState::Issued(issued.cancellation_mode)
        {
            return Err(IoUringError::InvalidRequestState);
        }
        entry.state = EntryState::ShotInFlight(issued.cancellation_mode);
        Ok(())
    }

    /// Releases a claimed shot without a MORE publication (would-block,
    /// terminal EOF/error).  The caller then either rearms or claims the
    /// ordinary terminal transition.
    pub fn abort_nonterminal_shot(&mut self, issued: &IssuedRequest) -> Result<(), IoUringError> {
        let entry = self.entry_mut(issued.id)?;
        if entry.descriptor != issued.descriptor
            || entry.state != EntryState::ShotInFlight(issued.cancellation_mode)
        {
            return Err(IoUringError::InvalidRequestState);
        }
        entry.state = EntryState::Issued(issued.cancellation_mode);
        if self.nonterminal_reservation == Some(issued.id) {
            self.nonterminal_reservation = None;
            self.nonterminal_reservations = self
                .nonterminal_reservations
                .checked_sub(1)
                .ok_or(IoUringError::InvalidRequestState)?;
        }
        Ok(())
    }

    /// Converts an in-flight side-effect shot directly into the sole terminal
    /// claimant.  This is deliberately not an abort followed by
    /// `claim_terminal`: that intermediate Issued state would permit
    /// ASYNC_CANCEL to publish `-ECANCELED` after the operation had already
    /// consumed input or written a supplied buffer.
    pub fn claim_terminal_after_nonterminal_shot(
        &mut self,
        issued: &IssuedRequest,
        cause: TerminalCause,
    ) -> Result<TerminalPermit, IoUringError> {
        let entry = self.entry_mut(issued.id)?;
        if entry.descriptor != issued.descriptor
            || entry.state != EntryState::ShotInFlight(issued.cancellation_mode)
        {
            return Err(IoUringError::InvalidRequestState);
        }
        let descriptor = entry.descriptor;
        entry.state = EntryState::TerminalClaimed(cause);
        if self.nonterminal_reservation == Some(issued.id) {
            self.nonterminal_reservation = None;
            self.nonterminal_reservations = self
                .nonterminal_reservations
                .checked_sub(1)
                .ok_or(IoUringError::InvalidRequestState)?;
        }
        Ok(TerminalPermit {
            id: issued.id,
            descriptor,
            cause,
        })
    }

    /// Claims one cancellable issued request selected by its bounded slot.
    /// Final-close code walks the preallocated slot range, so it never needs
    /// to allocate a side list just to detach timer-like executors.
    pub fn claim_cancellable_at_slot(
        &mut self,
        slot_index: u32,
        cause: TerminalCause,
    ) -> Result<Option<TerminalPermit>, IoUringError> {
        let slot = self
            .slots
            .get(usize::try_from(slot_index).map_err(|_| IoUringError::UnknownRequest)?)
            .ok_or(IoUringError::UnknownRequest)?;
        let Some(entry) = slot.entry else {
            return Ok(None);
        };
        let Some(generation) = slot.generation else {
            return Ok(None);
        };
        if entry.state != EntryState::Issued(CancellationMode::Cancellable) {
            return Ok(None);
        }
        self.claim_terminal(RequestId::new(self.ring, slot_index, generation), cause)
            .map(Some)
    }

    /// Claims the sole terminal transition for prepared or issued work.
    pub fn claim_terminal(
        &mut self,
        id: RequestId,
        cause: TerminalCause,
    ) -> Result<TerminalPermit, IoUringError> {
        let entry = self.entry_mut(id)?;
        match entry.state {
            EntryState::Prepared => {
                entry.state = EntryState::TerminalClaimed(cause);
                Ok(TerminalPermit {
                    id,
                    descriptor: entry.descriptor,
                    cause,
                })
            }
            EntryState::Issued(
                CancellationMode::Uncancellable | CancellationMode::ProviderControlled,
            ) if cause != TerminalCause::Completed => Err(IoUringError::RequestUncancellable),
            EntryState::Issued(_) => {
                entry.state = EntryState::TerminalClaimed(cause);
                Ok(TerminalPermit {
                    id,
                    descriptor: entry.descriptor,
                    cause,
                })
            }
            EntryState::TerminalClaimed(_)
            | EntryState::CompletionPending(_)
            | EntryState::Publishing(_)
            | EntryState::ShotInFlight(_) => Err(IoUringError::TerminalAlreadyClaimed),
            EntryState::Reserved => Err(IoUringError::InvalidRequestState),
        }
    }

    /// Atomically selects and claims one cancellable request.
    ///
    /// `exclude` is normally the `ASYNC_CANCEL` request itself. Duplicate user
    /// data values select the oldest still-cancellable admission.
    pub fn claim_cancel(
        &mut self,
        selector: CancelSelector,
        exclude: Option<RequestId>,
    ) -> Result<TerminalPermit, IoUringError> {
        if let CancelSelector::Request(id) = selector {
            if Some(id) == exclude {
                return Err(IoUringError::CancellationTargetNotFound);
            }
            return match self.claim_terminal(id, TerminalCause::Cancelled) {
                Err(
                    IoUringError::UnknownRequest
                    | IoUringError::TerminalAlreadyClaimed
                    | IoUringError::RequestUncancellable,
                ) => Err(IoUringError::CancellationTargetNotFound),
                result => result,
            };
        }

        let mut candidate: Option<(NonZeroU64, RequestId)> = None;
        for (slot_index, slot) in self.slots.iter().enumerate() {
            let Some(entry) = slot.entry else {
                continue;
            };
            let generation = slot.generation.ok_or(IoUringError::GenerationExhausted)?;
            let id = RequestId::new(
                self.ring,
                u32::try_from(slot_index).map_err(|_| IoUringError::Overflow)?,
                generation,
            );
            if Some(id) == exclude || !selector.matches(entry.descriptor) {
                continue;
            }
            match entry.state {
                EntryState::Prepared | EntryState::Issued(CancellationMode::Cancellable) => {
                    if candidate
                        .map(|(sequence, _)| entry.sequence < sequence)
                        .unwrap_or(true)
                    {
                        candidate = Some((entry.sequence, id));
                    }
                }
                EntryState::TerminalClaimed(_)
                | EntryState::CompletionPending(_)
                | EntryState::Publishing(_)
                | EntryState::ShotInFlight(_) => {}
                EntryState::Reserved => {}
                EntryState::Issued(
                    CancellationMode::Uncancellable | CancellationMode::ProviderControlled,
                ) => {}
            }
        }
        if let Some((_, id)) = candidate {
            self.claim_terminal(id, TerminalCause::Cancelled)
        } else {
            Err(IoUringError::CancellationTargetNotFound)
        }
    }

    /// Selects and fences one provider-owned request without terminalizing
    /// it.  The returned generation-scoped id is in `ShotInFlight` until
    /// [`finish_provider_cancel`] resolves the provider's outcome.
    pub fn select_cancel_candidate(
        &mut self,
        selector: CancelSelector,
        exclude: Option<RequestId>,
    ) -> Result<RequestId, IoUringError> {
        self.select_provider_cancel_candidate(selector, exclude, |_| true)
    }

    /// Selects a provider-owned cancellable request whose adapter-side owner
    /// still exists.  The predicate is evaluated while the registry lock is
    /// held, so a reusable slot cannot be selected after its provider owner
    /// has moved to a new generation.
    pub fn select_provider_cancel_candidate(
        &mut self,
        selector: CancelSelector,
        exclude: Option<RequestId>,
        mut owned: impl FnMut(RequestId) -> bool,
    ) -> Result<RequestId, IoUringError> {
        let mut candidate = None;
        for (slot_index, slot) in self.slots.iter().enumerate() {
            let (Some(entry), Some(generation)) = (slot.entry, slot.generation) else {
                continue;
            };
            let id = RequestId::new(
                self.ring,
                u32::try_from(slot_index).map_err(|_| IoUringError::Overflow)?,
                generation,
            );
            let matches = match selector {
                CancelSelector::Request(expected) => id == expected,
                CancelSelector::UserData(data) => entry.descriptor.user_data == data,
                CancelSelector::TimeoutUserData(data) => {
                    entry.descriptor.user_data == data
                        && entry.descriptor.operation == RequestOperation::Timeout
                }
            };
            if Some(id) != exclude
                && matches
                && owned(id)
                && matches!(
                    entry.state,
                    EntryState::Issued(
                        CancellationMode::Cancellable | CancellationMode::ProviderControlled
                    )
                )
                && candidate
                    .map(|(_, sequence): (RequestId, NonZeroU64)| entry.sequence < sequence)
                    .unwrap_or(true)
            {
                candidate = Some((id, entry.sequence));
            }
        }
        let Some((id, _)) = candidate else {
            return Err(IoUringError::CancellationTargetNotFound);
        };
        self.begin_provider_cancel(id)?;
        Ok(id)
    }

    pub fn begin_provider_cancel(&mut self, id: RequestId) -> Result<(), IoUringError> {
        let entry = self.entry_mut(id)?;
        let EntryState::Issued(
            mode @ (CancellationMode::Cancellable | CancellationMode::ProviderControlled),
        ) = entry.state
        else {
            return Err(IoUringError::CancellationTargetNotFound);
        };
        entry.state = EntryState::ShotInFlight(mode);
        self.observe(RequestTraceEvent::ProviderCancelSelected { id });
        Ok(())
    }

    pub fn finish_provider_cancel(
        &mut self,
        id: RequestId,
        outcome: ProviderCancelOutcome,
    ) -> Result<Option<TerminalPermit>, IoUringError> {
        let EntryState::ShotInFlight(
            mode @ (CancellationMode::Cancellable | CancellationMode::ProviderControlled),
        ) = self.entry(id)?.state
        else {
            return Err(IoUringError::TerminalAlreadyClaimed);
        };
        let result = match outcome {
            ProviderCancelOutcome::Cancelled => self
                .claim_terminal_after_nonterminal_shot(
                    &IssuedRequest {
                        id,
                        descriptor: self.entry(id)?.descriptor,
                        cancellation_mode: mode,
                    },
                    TerminalCause::Cancelled,
                )
                .map(Some),
            ProviderCancelOutcome::InFlight => {
                let entry = self.entry_mut(id)?;
                entry.state = EntryState::Issued(mode);
                Ok(None)
            }
        };
        if result.is_ok() {
            self.observe(RequestTraceEvent::ProviderCancelResolved { id, outcome });
        }
        result
    }

    /// Converts unique terminal ownership into a complete pending CQE.
    pub fn finish_terminal(
        &mut self,
        permit: TerminalPermit,
        result: i32,
        flags: u32,
    ) -> Result<CompletionToken, IoUringError> {
        let entry = self.entry_mut(permit.id)?;
        if entry.state != EntryState::TerminalClaimed(permit.cause) {
            return Err(IoUringError::InvalidRequestState);
        }
        entry.state = EntryState::CompletionPending(Completion::new(
            entry.descriptor.user_data,
            result,
            flags,
        ));
        self.observe(RequestTraceEvent::CompletionAccepted {
            id: permit.id,
            cause: permit.cause,
            result,
            flags,
        });
        Ok(CompletionToken { id: permit.id })
    }

    /// Starts the sole CQE write/tail publication transaction.
    ///
    /// The adapter must write `publication.completion()` completely at
    /// `publication.slot()`, release-store `publication.new_tail()`, and then
    /// call `commit_publication`. Other publication and reap operations remain
    /// blocked while the plan is outside this core.
    pub fn publish(
        &mut self,
        token: &CompletionToken,
    ) -> Result<CompletionPublication, IoUringError> {
        if self.lifecycle == RequestLifecycle::Draining {
            return Err(IoUringError::Draining);
        }
        if self.lifecycle == RequestLifecycle::Closed {
            return Err(IoUringError::Closed);
        }
        if self.publication_in_flight.is_some() {
            return Err(IoUringError::PublicationInFlight);
        }
        let entry = self.entry(token.id)?;
        let EntryState::CompletionPending(completion) = entry.state else {
            return Err(IoUringError::CompletionNotPending);
        };
        if self
            .published_count()?
            .checked_add(self.nonterminal_reservations)
            .ok_or(IoUringError::Overflow)?
            >= self.cq_entries
        {
            return Err(IoUringError::CompletionQueueFull);
        }
        let slot = self.cq_tail & (self.cq_entries - 1);
        let new_tail = self.cq_tail.wrapping_add(1);
        self.entry_mut(token.id)?.state = EntryState::Publishing(completion);
        self.publication_in_flight = Some(token.id);
        self.observe(RequestTraceEvent::PublicationStarted {
            id: token.id,
            terminal: true,
        });
        Ok(CompletionPublication {
            id: token.id,
            completion,
            slot,
            new_tail,
            terminal: true,
        })
    }

    /// Produces one `IORING_CQE_F_MORE`-style CQE without consuming the
    /// issued request.  The original terminal credit remains reserved, so a
    /// later cancellation/error/close retains one unique final transition.
    pub fn publish_nonterminal(
        &mut self,
        issued: &IssuedRequest,
        result: i32,
        flags: u32,
    ) -> Result<CompletionPublication, IoUringError> {
        if self.lifecycle == RequestLifecycle::Draining {
            return Err(IoUringError::Draining);
        }
        if self.lifecycle == RequestLifecycle::Closed {
            return Err(IoUringError::Closed);
        }
        // `begin_nonterminal_shot` owns this exact reservation before the
        // socket side effect. A different request remains excluded, while
        // its holder may turn the reservation into the CQ publication.
        if self.nonterminal_reservation != Some(issued.id) || self.publication_in_flight.is_some() {
            return Err(IoUringError::PublicationInFlight);
        }
        let entry = self.entry(issued.id)?;
        if entry.descriptor != issued.descriptor
            || !matches!(entry.state, EntryState::ShotInFlight(mode) if mode == issued.cancellation_mode)
        {
            return Err(IoUringError::InvalidRequestState);
        }
        let completion = Completion::new(entry.descriptor.user_data, result, flags);
        let slot = self.cq_tail & (self.cq_entries - 1);
        let new_tail = self.cq_tail.wrapping_add(1);
        self.publication_in_flight = Some(issued.id);
        self.nonterminal_reservation = None;
        self.nonterminal_reservations = self
            .nonterminal_reservations
            .checked_sub(1)
            .ok_or(IoUringError::InvalidRequestState)?;
        self.observe(RequestTraceEvent::PublicationStarted {
            id: issued.id,
            terminal: false,
        });
        Ok(CompletionPublication {
            id: issued.id,
            completion,
            slot,
            new_tail,
            terminal: false,
        })
    }

    /// Commits core accounting after the adapter release-stored the plan tail.
    pub fn commit_publication(
        &mut self,
        publication: CompletionPublication,
    ) -> Result<(), IoUringError> {
        if self.publication_in_flight != Some(publication.id) {
            return Err(IoUringError::PublicationInFlight);
        }
        let expected_slot = self.cq_tail & (self.cq_entries - 1);
        let expected_tail = self.cq_tail.wrapping_add(1);
        if publication.slot != expected_slot || publication.new_tail != expected_tail {
            return Err(IoUringError::InvalidQueueGeometry);
        }
        let entry = self.entry(publication.id)?;
        if publication.terminal {
            if entry.state != EntryState::Publishing(publication.completion) {
                return Err(IoUringError::InvalidRequestState);
            }
            self.clear_slot(publication.id)?;
        } else if let EntryState::ShotInFlight(mode) = entry.state {
            self.entry_mut(publication.id)?.state = EntryState::Issued(mode);
        } else {
            return Err(IoUringError::InvalidRequestState);
        }
        let published = self
            .published_terminal
            .get_mut(publication.slot as usize)
            .ok_or(IoUringError::InvalidQueueGeometry)?;
        *published = publication.terminal;
        self.cq_tail = publication.new_tail;
        self.publication_in_flight = None;
        self.observe(RequestTraceEvent::Published {
            id: publication.id,
            completion: publication.completion,
            tail: publication.new_tail,
            terminal: publication.terminal,
        });
        Ok(())
    }

    /// Rolls a plan back before any shared CQ tail release-store occurred.
    pub fn rollback_publication(
        &mut self,
        publication: CompletionPublication,
    ) -> Result<CompletionToken, IoUringError> {
        if self.publication_in_flight != Some(publication.id) {
            return Err(IoUringError::PublicationInFlight);
        }
        if !publication.terminal {
            return Err(IoUringError::InvalidRequestState);
        }
        let entry = self.entry_mut(publication.id)?;
        if entry.state != EntryState::Publishing(publication.completion) {
            return Err(IoUringError::InvalidRequestState);
        }
        entry.state = EntryState::CompletionPending(publication.completion);
        self.publication_in_flight = None;
        self.observe(RequestTraceEvent::PublicationRolledBack {
            id: publication.id,
            terminal: publication.terminal,
        });
        Ok(CompletionToken { id: publication.id })
    }

    /// Clears a failed nonterminal publication plan.  Since nonterminal
    /// publication never changes the issued entry state, there is no token to
    /// restore and no request credit to refund.
    pub fn rollback_nonterminal_publication(
        &mut self,
        publication: CompletionPublication,
    ) -> Result<(), IoUringError> {
        if publication.terminal || self.publication_in_flight != Some(publication.id) {
            return Err(IoUringError::PublicationInFlight);
        }
        let mode = match self.entry(publication.id)?.state {
            EntryState::ShotInFlight(mode) => mode,
            _ => return Err(IoUringError::InvalidRequestState),
        };
        self.entry_mut(publication.id)?.state = EntryState::Issued(mode);
        self.publication_in_flight = None;
        self.observe(RequestTraceEvent::PublicationRolledBack {
            id: publication.id,
            terminal: publication.terminal,
        });
        Ok(())
    }

    /// Validates userspace CQ consumption and refunds exactly those credits.
    ///
    /// Backward, forged-forward, and over-consuming heads are rejected without
    /// changing accounting. Credits for unpublished completions are never
    /// refunded here.
    pub fn observe_completion_head(&mut self, user_head: u32) -> Result<u32, IoUringError> {
        if self.publication_in_flight.is_some() {
            return Err(IoUringError::PublicationInFlight);
        }
        let consumed = user_head.wrapping_sub(self.cq_head);
        if consumed > self.published_count()? {
            return Err(IoUringError::CorruptCompletionHead);
        }
        let mut terminal = 0_u32;
        for offset in 0..consumed {
            let slot = self.cq_head.wrapping_add(offset) & (self.cq_entries - 1);
            let terminal_slot = self
                .published_terminal
                .get_mut(slot as usize)
                .ok_or(IoUringError::InvalidQueueGeometry)?;
            if *terminal_slot {
                terminal = terminal.checked_add(1).ok_or(IoUringError::Overflow)?;
            }
            *terminal_slot = false;
        }
        self.cq_head = user_head;
        self.refund_credits(terminal)?;
        if consumed != 0 {
            self.observe(RequestTraceEvent::HeadReclaimed {
                ring: self.ring,
                head: user_head,
                count: consumed,
            });
        }
        Ok(consumed)
    }

    /// Stops new reservations while preserving all existing terminal owners.
    pub fn begin_close(&mut self) -> Result<RequestProgress, IoUringError> {
        match self.lifecycle {
            RequestLifecycle::Open => self.lifecycle = RequestLifecycle::Closing,
            RequestLifecycle::Closing | RequestLifecycle::Draining | RequestLifecycle::Closed => {}
        }
        self.progress()
    }

    /// Enters explicit discard mode after every executor/terminal owner ended.
    pub fn begin_draining(&mut self) -> Result<RequestProgress, IoUringError> {
        if self.lifecycle == RequestLifecycle::Draining {
            return self.progress();
        }
        if self.lifecycle != RequestLifecycle::Closing {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        if self.slots.iter().any(|slot| {
            slot.entry
                .is_some_and(|entry| !matches!(entry.state, EntryState::CompletionPending(_)))
        }) {
            return Err(IoUringError::Busy);
        }
        self.lifecycle = RequestLifecycle::Draining;
        self.progress()
    }

    /// Discards one unpublished terminal CQE after userspace lost access.
    pub fn discard_completion(&mut self, token: CompletionToken) -> Result<(), IoUringError> {
        if self.lifecycle != RequestLifecycle::Draining {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        self.require_state(token.id, RequestState::CompletionPending)?;
        self.clear_slot(token.id)?;
        self.refund_credits(1)?;
        self.observe(RequestTraceEvent::Discarded { id: token.id });
        Ok(())
    }

    /// Discards all published but unconsumed CQEs after mappings are quiescent.
    pub fn discard_published(&mut self) -> Result<u32, IoUringError> {
        if self.lifecycle != RequestLifecycle::Draining {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        if self.publication_in_flight.is_some() {
            return Err(IoUringError::PublicationInFlight);
        }
        let published = self.published_count()?;
        let mut terminal = 0_u32;
        for offset in 0..published {
            let slot = self.cq_head.wrapping_add(offset) & (self.cq_entries - 1);
            let terminal_slot = self
                .published_terminal
                .get_mut(slot as usize)
                .ok_or(IoUringError::InvalidQueueGeometry)?;
            if *terminal_slot {
                terminal = terminal.checked_add(1).ok_or(IoUringError::Overflow)?;
            }
            *terminal_slot = false;
        }
        self.cq_head = self.cq_tail;
        self.refund_credits(terminal)?;
        Ok(published)
    }

    /// Finishes close only after every request and terminal credit is gone.
    pub fn finish_close(&mut self) -> Result<(), IoUringError> {
        match self.lifecycle {
            RequestLifecycle::Open => return Err(IoUringError::InvalidLifecycleTransition),
            RequestLifecycle::Closed => return Ok(()),
            RequestLifecycle::Closing | RequestLifecycle::Draining => {}
        }
        if self.publication_in_flight.is_some()
            || self.slots.iter().any(|slot| slot.entry.is_some())
            || self.terminal_credits != 0
        {
            return Err(IoUringError::Busy);
        }
        self.lifecycle = RequestLifecycle::Closed;
        Ok(())
    }

    /// Returns a finite snapshot without exposing internal storage.
    pub fn progress(&self) -> Result<RequestProgress, IoUringError> {
        let mut progress = RequestProgress {
            lifecycle: self.lifecycle,
            reserved: 0,
            prepared: 0,
            issued: 0,
            uncancellable_issued: 0,
            terminal_claimed: 0,
            completion_pending: 0,
            publishing: 0,
            terminal_credits: self.terminal_credits,
            published: self.published_count()?,
        };
        for slot in &self.slots {
            match slot.entry.map(|entry| entry.state) {
                Some(EntryState::Reserved) => progress.reserved += 1,
                Some(EntryState::Prepared) => progress.prepared += 1,
                Some(EntryState::Issued(mode)) => {
                    progress.issued += 1;
                    if mode == CancellationMode::Uncancellable {
                        progress.uncancellable_issued += 1;
                    }
                }
                Some(EntryState::ShotInFlight(mode)) => {
                    progress.issued += 1;
                    if mode == CancellationMode::Uncancellable {
                        progress.uncancellable_issued += 1;
                    }
                }
                Some(EntryState::TerminalClaimed(_)) => progress.terminal_claimed += 1,
                Some(EntryState::CompletionPending(_)) => progress.completion_pending += 1,
                Some(EntryState::Publishing(_)) => progress.publishing += 1,
                None => {}
            }
        }
        Ok(progress)
    }

    fn entry(&self, id: RequestId) -> Result<&RequestEntry, IoUringError> {
        if id.ring != self.ring {
            return Err(IoUringError::UnknownRequest);
        }
        let slot = self
            .slots
            .get(usize::try_from(id.slot).map_err(|_| IoUringError::UnknownRequest)?)
            .ok_or(IoUringError::UnknownRequest)?;
        if slot.generation != Some(id.generation) {
            return Err(IoUringError::UnknownRequest);
        }
        slot.entry.as_ref().ok_or(IoUringError::UnknownRequest)
    }

    fn entry_mut(&mut self, id: RequestId) -> Result<&mut RequestEntry, IoUringError> {
        if id.ring != self.ring {
            return Err(IoUringError::UnknownRequest);
        }
        let slot = self
            .slots
            .get_mut(usize::try_from(id.slot).map_err(|_| IoUringError::UnknownRequest)?)
            .ok_or(IoUringError::UnknownRequest)?;
        if slot.generation != Some(id.generation) {
            return Err(IoUringError::UnknownRequest);
        }
        slot.entry.as_mut().ok_or(IoUringError::UnknownRequest)
    }

    fn require_state(&self, id: RequestId, state: RequestState) -> Result<(), IoUringError> {
        if self.entry(id)?.state.public() == state {
            Ok(())
        } else {
            Err(IoUringError::InvalidRequestState)
        }
    }

    fn clear_slot(&mut self, id: RequestId) -> Result<(), IoUringError> {
        if id.ring != self.ring {
            return Err(IoUringError::UnknownRequest);
        }
        let slot = self
            .slots
            .get_mut(usize::try_from(id.slot).map_err(|_| IoUringError::UnknownRequest)?)
            .ok_or(IoUringError::UnknownRequest)?;
        if slot.generation != Some(id.generation) || slot.entry.is_none() {
            return Err(IoUringError::UnknownRequest);
        }
        slot.entry = None;
        let next_generation = id.generation.get().checked_add(1).and_then(NonZeroU64::new);
        slot.generation = next_generation;
        if next_generation.is_some() {
            if self.free_slots.len() >= self.slots.len() {
                return Err(IoUringError::InvalidRequestState);
            }
            self.free_slots.push(id.slot);
        }
        Ok(())
    }

    fn published_count(&self) -> Result<u32, IoUringError> {
        let published = self.cq_tail.wrapping_sub(self.cq_head);
        if published <= self.cq_entries {
            Ok(published)
        } else {
            Err(IoUringError::InvalidQueueGeometry)
        }
    }

    fn refund_credits(&mut self, count: u32) -> Result<(), IoUringError> {
        self.terminal_credits = self
            .terminal_credits
            .checked_sub(count)
            .ok_or(IoUringError::InvalidCompletionConsumption)?;
        Ok(())
    }

    #[cfg(test)]
    fn force_empty_slot_generation(&mut self, slot: u32, generation: u64) {
        let slot = &mut self.slots[slot as usize];
        assert!(slot.entry.is_none());
        slot.generation = NonZeroU64::new(generation);
    }

    #[cfg(test)]
    fn force_next_sequence(&mut self, sequence: u64) {
        self.next_sequence = NonZeroU64::new(sequence);
    }

    #[cfg(test)]
    fn force_empty_cq_counters(&mut self, counter: u32) {
        assert_eq!(self.terminal_credits, 0);
        assert!(self.publication_in_flight.is_none());
        self.cq_head = counter;
        self.cq_tail = counter;
    }
}

impl CancelSelector {
    fn matches(self, descriptor: RequestDescriptor) -> bool {
        match self {
            Self::UserData(user_data) => descriptor.user_data == user_data,
            Self::TimeoutUserData(user_data) => {
                descriptor.user_data == user_data
                    && descriptor.operation == RequestOperation::Timeout
            }
            Self::Request(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    std::thread_local! {
        static OBSERVED: std::cell::RefCell<Vec<RequestTraceEvent>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    fn observe(event: RequestTraceEvent) {
        OBSERVED.with(|events| events.borrow_mut().push(event));
    }
    fn take_events() -> Vec<RequestTraceEvent> {
        OBSERVED.with(|events| core::mem::take(&mut *events.borrow_mut()))
    }

    #[test]
    fn observer_orders_cancel_acceptance_and_actual_publication() {
        take_events();
        let mut registry = RequestRegistry::new(ring(51), 2, 2).unwrap();
        registry.set_observer(Some(observe));
        let descriptor = RequestDescriptor::new(77, RequestOperation::PollAdd);
        let reserved = registry.reserve(descriptor).unwrap();
        let id = reserved.id();
        let prepared = registry.commit(reserved).unwrap();
        let _issued = registry.issue(prepared).unwrap();
        let permit = registry
            .claim_cancel(CancelSelector::Request(id), None)
            .unwrap();
        let token = registry.finish_terminal(permit, -125, 0).unwrap();
        let publication = registry.publish(&token).unwrap();
        // Neither a plan nor its rollback means the CQE became visible.
        let token = registry.rollback_publication(publication).unwrap();
        assert_eq!(
            take_events(),
            alloc::vec![
                RequestTraceEvent::Reserved { id, descriptor },
                RequestTraceEvent::Submitted { id },
                RequestTraceEvent::Issued { id },
                RequestTraceEvent::CompletionAccepted {
                    id,
                    cause: TerminalCause::Cancelled,
                    result: -125,
                    flags: 0
                },
                RequestTraceEvent::PublicationStarted { id, terminal: true },
                RequestTraceEvent::PublicationRolledBack { id, terminal: true },
            ]
        );
        assert!(
            registry
                .claim_terminal(id, TerminalCause::Completed)
                .is_err()
        );
        let publication = registry.publish(&token).unwrap();
        registry.commit_publication(publication).unwrap();
        registry.observe_completion_head(1).unwrap();
        registry.observe_completion_head(1).unwrap();
        assert_eq!(
            take_events(),
            alloc::vec![
                RequestTraceEvent::PublicationStarted { id, terminal: true },
                RequestTraceEvent::Published {
                    id,
                    completion: Completion::new(77, -125, 0),
                    tail: 1,
                    terminal: true
                },
                RequestTraceEvent::HeadReclaimed {
                    ring: ring(51),
                    head: 1,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn observer_keeps_pending_terminal_distinct_from_full_cq_publication() {
        take_events();
        let mut registry = RequestRegistry::new(ring(52), 1, 1).unwrap();
        registry.set_observer(Some(observe));
        let reservation = registry
            .reserve(RequestDescriptor::new(88, RequestOperation::PollAdd))
            .unwrap();
        let id = reservation.id();
        let prepared = registry.commit(reservation).unwrap();
        let issued = registry.issue(prepared).unwrap();
        registry.begin_nonterminal_shot(&issued).unwrap();
        let shot = registry.publish_nonterminal(&issued, 1, 2).unwrap();
        registry.commit_publication(shot).unwrap();
        let permit = registry
            .claim_terminal(id, TerminalCause::Cancelled)
            .unwrap();
        let token = registry.finish_terminal(permit, -125, 0).unwrap();
        assert_eq!(
            registry.publish(&token).unwrap_err(),
            IoUringError::CompletionQueueFull
        );
        let before = take_events();
        assert_eq!(
            before
                .iter()
                .filter(|event| matches!(event, RequestTraceEvent::Published { .. }))
                .count(),
            1
        );
        registry.observe_completion_head(1).unwrap();
        let final_cqe = registry.publish(&token).unwrap();
        registry.commit_publication(final_cqe).unwrap();
        assert_eq!(
            take_events(),
            alloc::vec![
                RequestTraceEvent::HeadReclaimed {
                    ring: ring(52),
                    head: 1,
                    count: 1
                },
                RequestTraceEvent::PublicationStarted { id, terminal: true },
                RequestTraceEvent::Published {
                    id,
                    completion: Completion::new(88, -125, 0),
                    tail: 2,
                    terminal: true
                },
            ]
        );
    }

    #[test]
    fn observer_reports_provider_rearm_and_nonterminal_publication_rollback() {
        take_events();
        let mut registry = RequestRegistry::new(ring(54), 1, 1).unwrap();
        registry.set_observer(Some(observe));
        let reserved = registry
            .reserve(RequestDescriptor::new(9, RequestOperation::PollAdd))
            .unwrap();
        let prepared = registry.commit(reserved).unwrap();
        let issued = registry.issue(prepared).unwrap();
        let id = issued.id();
        take_events();
        registry.begin_provider_cancel(id).unwrap();
        assert!(
            registry
                .finish_provider_cancel(id, ProviderCancelOutcome::InFlight)
                .unwrap()
                .is_none()
        );
        assert!(registry.issued_is_live(&issued));
        registry.begin_nonterminal_shot(&issued).unwrap();
        let publication = registry.publish_nonterminal(&issued, 1, 2).unwrap();
        registry
            .rollback_nonterminal_publication(publication)
            .unwrap();
        assert_eq!(
            take_events(),
            alloc::vec![
                RequestTraceEvent::ProviderCancelSelected { id },
                RequestTraceEvent::ProviderCancelResolved {
                    id,
                    outcome: ProviderCancelOutcome::InFlight
                },
                RequestTraceEvent::PublicationStarted {
                    id,
                    terminal: false
                },
                RequestTraceEvent::PublicationRolledBack {
                    id,
                    terminal: false
                },
            ]
        );
    }

    #[test]
    fn observer_disabled_and_reservation_rollback_are_explicit() {
        take_events();
        let mut registry = RequestRegistry::new(ring(53), 1, 1).unwrap();
        let reservation = registry.reserve(descriptor(1)).unwrap();
        registry.rollback(reservation).unwrap();
        assert!(take_events().is_empty());
        registry.set_observer(Some(observe));
        let reservation = registry.reserve(descriptor(1)).unwrap();
        let id = reservation.id();
        registry.rollback(reservation).unwrap();
        assert_eq!(
            take_events(),
            alloc::vec![
                RequestTraceEvent::Reserved {
                    id,
                    descriptor: descriptor(1)
                },
                RequestTraceEvent::RolledBack { id },
            ]
        );
    }

    fn ring(raw: u64) -> RingId {
        RingId::new(raw).unwrap()
    }

    fn descriptor(user_data: u64) -> RequestDescriptor {
        RequestDescriptor::new(user_data, RequestOperation::Nop)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Published {
        completion: Completion,
        slot: u32,
        new_tail: u32,
    }

    fn complete(
        registry: &mut RequestRegistry,
        prepared: PreparedRequest,
        result: i32,
    ) -> Published {
        let permit = registry
            .claim_terminal(prepared.id(), TerminalCause::Completed)
            .unwrap();
        let token = registry.finish_terminal(permit, result, 0).unwrap();
        let publication = registry.publish(&token).unwrap();
        let published = Published {
            completion: publication.completion(),
            slot: publication.slot(),
            new_tail: publication.new_tail(),
        };
        registry.commit_publication(publication).unwrap();
        published
    }

    #[test]
    fn terminal_credit_is_reserved_before_commit_and_rollback_refunds_it() {
        let mut registry = RequestRegistry::new(ring(1), 2, 1).unwrap();
        let reservation = registry.reserve(descriptor(7)).unwrap();
        assert_eq!(registry.progress().unwrap().terminal_credits(), 1);
        assert!(matches!(
            registry.reserve(descriptor(8)),
            Err(IoUringError::CompletionQueueFull)
        ));
        registry.rollback(reservation).unwrap();
        assert_eq!(registry.progress().unwrap().terminal_credits(), 0);
        assert!(registry.reserve(descriptor(8)).is_ok());
    }

    #[test]
    fn request_capacity_is_bounded_by_the_pinned_linux_sq_limit() {
        assert!(matches!(
            RequestRegistry::new(ring(1), IORING_MAX_ENTRIES + 1, 1),
            Err(IoUringError::RequestCapacityExceeded)
        ));
        assert_eq!(RequestRegistry::new(ring(1), 3, 4).unwrap().capacity(), 3);
    }

    #[test]
    fn published_credit_is_refunded_only_by_valid_user_head() {
        let mut registry = RequestRegistry::new(ring(1), 2, 2).unwrap();
        let reservation = registry.reserve(descriptor(11)).unwrap();
        let prepared = registry.commit(reservation).unwrap();
        let publication = complete(&mut registry, prepared, 42);
        assert_eq!(publication.slot, 0);
        assert_eq!(publication.new_tail, 1);
        assert_eq!(publication.completion, Completion::new(11, 42, 0));
        assert_eq!(registry.progress().unwrap().terminal_credits(), 1);
        assert_eq!(
            registry.observe_completion_head(2),
            Err(IoUringError::CorruptCompletionHead)
        );
        assert_eq!(registry.progress().unwrap().terminal_credits(), 1);
        assert_eq!(registry.observe_completion_head(1).unwrap(), 1);
        assert_eq!(registry.progress().unwrap().terminal_credits(), 0);
    }

    #[test]
    fn request_slot_reuse_changes_generation_before_cq_reap() {
        let mut registry = RequestRegistry::new(ring(1), 1, 2).unwrap();
        let reservation = registry.reserve(descriptor(1)).unwrap();
        let first = registry.commit(reservation).unwrap();
        let first_id = first.id();
        complete(&mut registry, first, 0);

        let reservation = registry.reserve(descriptor(2)).unwrap();
        let second = registry.commit(reservation).unwrap();
        assert_eq!(first_id.slot(), second.id().slot());
        assert_ne!(first_id.generation(), second.id().generation());
        assert_eq!(
            registry.request(first_id),
            Err(IoUringError::UnknownRequest)
        );
    }

    #[test]
    fn generation_and_admission_sequence_never_wrap_into_aba() {
        let mut generation_registry = RequestRegistry::new(ring(1), 1, 1).unwrap();
        generation_registry.force_empty_slot_generation(0, u64::MAX);
        let reservation = generation_registry.reserve(descriptor(1)).unwrap();
        let prepared = generation_registry.commit(reservation).unwrap();
        complete(&mut generation_registry, prepared, 0);
        generation_registry.observe_completion_head(1).unwrap();
        assert!(matches!(
            generation_registry.reserve(descriptor(2)),
            Err(IoUringError::GenerationExhausted)
        ));

        let mut sequence_registry = RequestRegistry::new(ring(2), 1, 1).unwrap();
        sequence_registry.force_next_sequence(u64::MAX);
        let reservation = sequence_registry.reserve(descriptor(1)).unwrap();
        let prepared = sequence_registry.commit(reservation).unwrap();
        complete(&mut sequence_registry, prepared, 0);
        sequence_registry.observe_completion_head(1).unwrap();
        assert!(matches!(
            sequence_registry.reserve(descriptor(2)),
            Err(IoUringError::GenerationExhausted)
        ));
    }

    #[test]
    fn cq_counters_wrap_monotonically_without_refunding_early() {
        let mut registry = RequestRegistry::new(ring(1), 1, 2).unwrap();
        registry.force_empty_cq_counters(u32::MAX);
        let reservation = registry.reserve(descriptor(1)).unwrap();
        let prepared = registry.commit(reservation).unwrap();
        let publication = complete(&mut registry, prepared, 0);
        assert_eq!(publication.new_tail, 0);
        assert_eq!(publication.slot, 1);
        assert_eq!(registry.observe_completion_head(0), Ok(1));
        assert_eq!(registry.progress().unwrap().terminal_credits(), 0);
    }

    #[test]
    fn terminal_race_has_exactly_one_owner() {
        let mut registry = RequestRegistry::new(ring(1), 1, 2).unwrap();
        let reservation = registry.reserve(descriptor(1)).unwrap();
        let prepared = registry.commit(reservation).unwrap();
        let issued = registry.issue(prepared).unwrap();
        let permit = registry
            .claim_terminal(issued.id(), TerminalCause::Completed)
            .unwrap();
        assert!(matches!(
            registry.claim_terminal(issued.id(), TerminalCause::Cancelled),
            Err(IoUringError::TerminalAlreadyClaimed)
        ));
        assert!(registry.finish_terminal(permit, 0, 0).is_ok());
    }

    #[test]
    fn publication_is_serialized_until_tail_commit_or_rollback() {
        let mut registry = RequestRegistry::new(ring(1), 2, 2).unwrap();
        let first_reservation = registry.reserve(descriptor(1)).unwrap();
        let first = registry.commit(first_reservation).unwrap();
        let first_permit = registry
            .claim_terminal(first.id(), TerminalCause::Completed)
            .unwrap();
        let first_token = registry.finish_terminal(first_permit, 10, 0).unwrap();

        let second_reservation = registry.reserve(descriptor(2)).unwrap();
        let second = registry.commit(second_reservation).unwrap();
        let second_permit = registry
            .claim_terminal(second.id(), TerminalCause::Completed)
            .unwrap();
        let second_token = registry.finish_terminal(second_permit, 20, 0).unwrap();

        let first_publication = registry.publish(&first_token).unwrap();
        assert_eq!(registry.progress().unwrap().publishing(), 1);
        assert!(matches!(
            registry.publish(&second_token),
            Err(IoUringError::PublicationInFlight)
        ));
        assert_eq!(
            registry.observe_completion_head(0),
            Err(IoUringError::PublicationInFlight)
        );
        let first_token = registry.rollback_publication(first_publication).unwrap();
        assert_eq!(registry.progress().unwrap().publishing(), 0);

        let first_publication = registry.publish(&first_token).unwrap();
        registry.commit_publication(first_publication).unwrap();
        let second_publication = registry.publish(&second_token).unwrap();
        assert_eq!(second_publication.slot(), 1);
        assert_eq!(second_publication.new_tail(), 2);
        registry.commit_publication(second_publication).unwrap();
    }

    #[test]
    fn duplicate_user_data_cancellation_selects_oldest_live_request() {
        let mut registry = RequestRegistry::new(ring(1), 3, 4).unwrap();
        let reservation = registry.reserve(descriptor(9)).unwrap();
        let first = registry.commit(reservation).unwrap();
        let reservation = registry.reserve(descriptor(9)).unwrap();
        let second = registry.commit(reservation).unwrap();
        let permit = registry
            .claim_cancel(CancelSelector::UserData(9), None)
            .unwrap();
        assert_eq!(permit.id(), first.id());
        assert_eq!(
            registry.request(second.id()).unwrap().1,
            RequestState::Prepared
        );
    }

    #[test]
    fn cancellation_exclusion_prevents_self_match() {
        let mut registry = RequestRegistry::new(ring(1), 1, 1).unwrap();
        let reservation = registry
            .reserve(RequestDescriptor::new(9, RequestOperation::AsyncCancel))
            .unwrap();
        let cancel = registry.commit(reservation).unwrap();
        assert!(matches!(
            registry.claim_cancel(CancelSelector::UserData(9), Some(cancel.id())),
            Err(IoUringError::CancellationTargetNotFound)
        ));
    }

    #[test]
    fn repeated_cancel_observes_no_cancellable_target() {
        let mut registry = RequestRegistry::new(ring(1), 1, 1).unwrap();
        let reservation = registry.reserve(descriptor(9)).unwrap();
        let target = registry.commit(reservation).unwrap();
        let permit = registry
            .claim_cancel(CancelSelector::UserData(9), None)
            .unwrap();
        assert_eq!(permit.id(), target.id());
        assert!(matches!(
            registry.claim_cancel(CancelSelector::UserData(9), None),
            Err(IoUringError::CancellationTargetNotFound)
        ));
    }

    #[test]
    fn irreversible_rw_handoff_blocks_cancel_and_forced_close_completion() {
        let mut registry = RequestRegistry::new(ring(1), 1, 1).unwrap();
        let reservation = registry
            .reserve(RequestDescriptor::new(9, RequestOperation::Write))
            .unwrap();
        let prepared = registry.commit(reservation).unwrap();
        let issued = registry.issue(prepared).unwrap();
        assert_eq!(issued.cancellation_mode(), CancellationMode::Uncancellable);
        assert!(matches!(
            registry.claim_cancel(CancelSelector::UserData(9), None),
            Err(IoUringError::CancellationTargetNotFound)
        ));
        assert_eq!(registry.progress().unwrap().uncancellable_issued(), 1);
        assert!(matches!(
            registry.claim_terminal(issued.id(), TerminalCause::Closing),
            Err(IoUringError::RequestUncancellable)
        ));
        let completion = registry
            .claim_terminal(issued.id(), TerminalCause::Completed)
            .unwrap();
        let completion = registry.finish_terminal(completion, 0, 0).unwrap();
        registry.begin_close().unwrap();
        registry.begin_draining().unwrap();
        registry.discard_completion(completion).unwrap();
        registry.finish_close().unwrap();
    }

    #[test]
    fn poll_handoff_remains_cancellable_and_issue_race_returns_proof() {
        let mut registry = RequestRegistry::new(ring(1), 2, 2).unwrap();
        let reservation = registry
            .reserve(RequestDescriptor::new(7, RequestOperation::PollAdd))
            .unwrap();
        let poll = registry.commit(reservation).unwrap();
        let issued = registry.issue(poll).unwrap();
        assert_eq!(issued.cancellation_mode(), CancellationMode::Cancellable);
        let first_cancel = registry
            .claim_cancel(CancelSelector::UserData(7), None)
            .unwrap();
        let first_completion = registry.finish_terminal(first_cancel, -1, 0).unwrap();

        let reservation = registry
            .reserve(RequestDescriptor::new(8, RequestOperation::PollAdd))
            .unwrap();
        let prepared = registry.commit(reservation).unwrap();
        let id = prepared.id();
        let second_cancel = registry
            .claim_cancel(CancelSelector::UserData(8), None)
            .unwrap();
        let issue_error = registry
            .issue(prepared)
            .expect_err("cancel must win before external hand-off");
        assert_eq!(issue_error.error(), IoUringError::TerminalAlreadyClaimed);
        assert_eq!(issue_error.prepared().id(), id);
        let second_completion = registry.finish_terminal(second_cancel, -1, 0).unwrap();
        registry.begin_close().unwrap();
        registry.begin_draining().unwrap();
        registry.discard_completion(first_completion).unwrap();
        registry.discard_completion(second_completion).unwrap();
        registry.finish_close().unwrap();
    }

    #[test]
    fn cross_ring_request_identity_is_rejected() {
        let mut first = RequestRegistry::new(ring(1), 1, 1).unwrap();
        let second = RequestRegistry::new(ring(2), 1, 1).unwrap();
        let reservation = first.reserve(descriptor(1)).unwrap();
        let prepared = first.commit(reservation).unwrap();
        assert_eq!(
            second.request(prepared.id()),
            Err(IoUringError::UnknownRequest)
        );
    }

    #[test]
    fn provider_handoff_cannot_be_cancelled_or_reused_before_retirement() {
        // These are the two submit exits: a synchronous provider completion,
        // and a failed publication after its prepared resources are retired.
        for result in [4096, -5] {
            let mut registry = RequestRegistry::new(ring(1), 1, 2).unwrap();
            let reservation = registry.reserve(descriptor(7)).unwrap();
            let prepared = registry.commit(reservation).unwrap();
            let issued = registry
                .issue_with_cancellation_mode(prepared, Some(CancellationMode::ProviderControlled))
                .unwrap();
            let id = issued.id();
            // Interleave cancel at issue -> owner installation and again
            // while submit runs outside the adapter lock (Publishing).
            for _ in 0..2 {
                assert!(matches!(
                    registry.select_provider_cancel_candidate(
                        CancelSelector::UserData(7),
                        None,
                        |_| false,
                    ),
                    Err(IoUringError::CancellationTargetNotFound)
                ));
                assert!(matches!(
                    registry.claim_cancel(CancelSelector::UserData(7), None),
                    Err(IoUringError::CancellationTargetNotFound)
                ));
                assert!(matches!(
                    registry.claim_cancel(CancelSelector::Request(id), None),
                    Err(IoUringError::CancellationTargetNotFound)
                ));
                assert!(
                    registry
                        .claim_cancellable_at_slot(id.slot(), TerminalCause::Closing)
                        .unwrap()
                        .is_none()
                );
                assert!(matches!(
                    registry.claim_terminal(id, TerminalCause::Closing),
                    Err(IoUringError::RequestUncancellable)
                ));
                assert!(registry.reserve(descriptor(8)).is_err());
                assert!(registry.issued_is_live(&issued));
                assert_eq!(registry.progress().unwrap().completion_pending(), 0);
            }
            let permit = registry
                .claim_terminal(id, TerminalCause::Completed)
                .unwrap();
            let token = registry.finish_terminal(permit, result, 0).unwrap();
            let publication = registry.publish(&token).unwrap();
            assert_eq!(publication.completion(), Completion::new(7, result, 0));
            registry.commit_publication(publication).unwrap();
            let replacement = registry.reserve(descriptor(8)).unwrap();
            assert_eq!(replacement.id().slot(), id.slot());
            assert_ne!(replacement.id().generation(), id.generation());
            assert!(matches!(
                registry.claim_terminal(id, TerminalCause::Completed),
                Err(IoUringError::UnknownRequest)
            ));
            registry.rollback(replacement).unwrap();
            registry.observe_completion_head(1).unwrap();
            assert_eq!(registry.progress().unwrap().terminal_credits(), 0);
        }
    }

    #[test]
    fn provider_inflight_cancel_keeps_exclusive_retirement_and_close_progress() {
        let mut registry = RequestRegistry::new(ring(1), 1, 1).unwrap();
        let reservation = registry.reserve(descriptor(7)).unwrap();
        let prepared = registry.commit(reservation).unwrap();
        let issued = registry
            .issue_with_cancellation_mode(prepared, Some(CancellationMode::ProviderControlled))
            .unwrap();
        let id = registry
            .select_provider_cancel_candidate(CancelSelector::UserData(7), None, |_| true)
            .unwrap();
        // A synchronous callback during control.cancel must wait for the
        // adapter to resolve its fence, retaining the real callback result.
        assert!(matches!(
            registry.claim_terminal(id, TerminalCause::Completed),
            Err(IoUringError::TerminalAlreadyClaimed)
        ));
        registry
            .finish_provider_cancel(id, ProviderCancelOutcome::InFlight)
            .unwrap();
        assert!(registry.issued_is_live(&issued));
        assert!(matches!(
            registry.claim_cancel(CancelSelector::UserData(7), None),
            Err(IoUringError::CancellationTargetNotFound)
        ));
        registry.begin_close().unwrap();
        assert!(
            registry
                .claim_cancellable_at_slot(id.slot(), TerminalCause::Closing)
                .unwrap()
                .is_none()
        );
        assert_eq!(registry.begin_draining(), Err(IoUringError::Busy));
        let permit = registry
            .claim_terminal(id, TerminalCause::Completed)
            .unwrap();
        let token = registry.finish_terminal(permit, 4096, 0).unwrap();
        registry.begin_draining().unwrap();
        registry.discard_completion(token).unwrap();
        registry.finish_close().unwrap();
    }

    #[test]
    fn provider_control_can_confirm_cancellation_exactly_once() {
        let mut registry = RequestRegistry::new(ring(1), 1, 1).unwrap();
        let reservation = registry.reserve(descriptor(7)).unwrap();
        let prepared = registry.commit(reservation).unwrap();
        let issued = registry
            .issue_with_cancellation_mode(prepared, Some(CancellationMode::ProviderControlled))
            .unwrap();
        registry.begin_provider_cancel(issued.id()).unwrap();
        let permit = registry
            .finish_provider_cancel(issued.id(), ProviderCancelOutcome::Cancelled)
            .unwrap()
            .unwrap();
        let token = registry.finish_terminal(permit, -125, 0).unwrap();
        assert!(matches!(
            registry.claim_terminal(issued.id(), TerminalCause::Completed),
            Err(IoUringError::TerminalAlreadyClaimed)
        ));
        let publication = registry.publish(&token).unwrap();
        assert_eq!(publication.completion(), Completion::new(7, -125, 0));
        registry.commit_publication(publication).unwrap();
        registry.observe_completion_head(1).unwrap();
        assert_eq!(registry.progress().unwrap().terminal_credits(), 0);
    }

    #[test]
    fn close_preserves_terminal_credit_until_reap() {
        let mut registry = RequestRegistry::new(ring(1), 1, 1).unwrap();
        let reservation = registry.reserve(descriptor(1)).unwrap();
        let prepared = registry.commit(reservation).unwrap();
        registry.begin_close().unwrap();
        assert!(matches!(
            registry.reserve(descriptor(2)),
            Err(IoUringError::Closing)
        ));
        complete(&mut registry, prepared, 0);
        assert_eq!(registry.finish_close(), Err(IoUringError::Busy));
        registry.observe_completion_head(1).unwrap();
        registry.finish_close().unwrap();
        registry.begin_close().unwrap();
        registry.finish_close().unwrap();
        assert_eq!(
            registry.progress().unwrap().lifecycle(),
            RequestLifecycle::Closed
        );
    }

    #[test]
    fn draining_requires_quiescent_execution_and_explicitly_refunds() {
        let mut registry = RequestRegistry::new(ring(1), 2, 2).unwrap();
        let reservation = registry.reserve(descriptor(1)).unwrap();
        let first = registry.commit(reservation).unwrap();
        let reservation = registry.reserve(descriptor(2)).unwrap();
        let second = registry.commit(reservation).unwrap();
        let first_token = {
            let permit = registry
                .claim_terminal(first.id(), TerminalCause::Closing)
                .unwrap();
            registry.finish_terminal(permit, -1, 0).unwrap()
        };
        registry.begin_close().unwrap();
        assert_eq!(registry.begin_draining(), Err(IoUringError::Busy));
        let second_token = {
            let permit = registry
                .claim_terminal(second.id(), TerminalCause::Closing)
                .unwrap();
            registry.finish_terminal(permit, -1, 0).unwrap()
        };
        registry.begin_draining().unwrap();
        registry.discard_completion(first_token).unwrap();
        registry.discard_completion(second_token).unwrap();
        registry.finish_close().unwrap();
    }
}
