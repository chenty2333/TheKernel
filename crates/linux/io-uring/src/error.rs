/// Stable policy failures returned by `thekernel-linux-io-uring`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IoUringError {
    /// A stable ring identity used the reserved zero value.
    InvalidIdentity,
    /// A setup field reserved by the supported profile was nonzero.
    ReservedFieldNonZero,
    /// Setup requested a flag outside the supported initial profile.
    UnsupportedSetupFlags,
    /// An adapter attempted to advertise a feature outside the initial core.
    UnsupportedFeatureFlags,
    /// Enter requested flags outside `GETEVENTS` in the initial profile.
    UnsupportedEnterFlags,
    /// A supplied legacy signal-mask size did not match its native ABI size.
    InvalidSignalMaskArgument,
    /// Submission queue depth was zero.
    ZeroEntries,
    /// Submission queue depth exceeded the Linux limit without `CLAMP`.
    SubmissionEntriesTooLarge,
    /// `CQSIZE` selected a zero completion queue depth.
    ZeroCompletionEntries,
    /// Completion queue depth exceeded the Linux limit without `CLAMP`.
    CompletionEntriesTooLarge,
    /// Explicit completion queue depth was smaller than submission depth.
    CompletionEntriesTooSmall,
    /// A size, generation, reference count, or queue counter overflowed.
    Overflow,
    /// A fallible bounded allocation failed.
    AllocationFailed,
    /// No future non-wrapping generation is available.
    GenerationExhausted,
    /// A queue depth required by the contract was not a power of two.
    InvalidQueueGeometry,
    /// An mmap offset does not name an initial ring region.
    InvalidMappingOffset,
    /// A copied shared-ring head advanced beyond its published tail.
    CorruptCompletionHead,
    /// Every terminal CQ credit is already charged to accepted work.
    CompletionQueueFull,
    /// No request slot is available despite otherwise valid admission.
    RequestCapacityExceeded,
    /// A request token belongs to another ring or an older slot generation.
    UnknownRequest,
    /// A request exists but is not in the required lifecycle state.
    InvalidRequestState,
    /// Execution crossed an irreversible boundary and cannot be cancelled or
    /// force-completed by close.
    RequestUncancellable,
    /// The requested terminal transition already has a competing owner.
    TerminalAlreadyClaimed,
    /// No terminal completion is waiting for publication.
    CompletionNotPending,
    /// Another CQE write/tail publication transaction has not committed yet.
    PublicationInFlight,
    /// A consumed CQ count exceeds the completions actually published.
    InvalidCompletionConsumption,
    /// The ring or resource table has stopped admitting new work.
    Closing,
    /// The ring is discarding terminal state before final close.
    Draining,
    /// The ring or resource table is fully closed.
    Closed,
    /// A lifecycle transition skipped a required close or quiescence phase.
    InvalidLifecycleTransition,
    /// Live work prevents the requested lifecycle transition.
    Busy,
    /// The copied SQE uses an opcode known by the pinned Linux UAPI but not
    /// implemented by this initial profile.
    UnsupportedOpcode,
    /// The copied SQE opcode is outside the pinned Linux UAPI range.
    UnknownOpcode,
    /// The copied SQE uses unsupported generic submission flags.
    UnsupportedSubmissionFlags,
    /// The copied SQE requests an operation option not implemented by the core.
    UnsupportedOperationFlags,
    /// Current-file-position I/O was requested without the corresponding feature.
    CurrentPositionUnsupported,
    /// A file descriptor or fixed-file index was negative or otherwise invalid.
    InvalidFileTarget,
    /// A copied SQE contains an invalid required-zero field or arithmetic range.
    InvalidSubmission,
    /// A registered-file table capacity was zero or exceeded the Linux limit.
    InvalidFileTableCapacity,
    /// A registered-file lease budget was zero or exceeded the ring limit.
    InvalidFileLeaseCapacity,
    /// Every configured registered-file execution lease is already charged.
    FileLeaseCapacityExceeded,
    /// A registered-file slot index is outside the table.
    InvalidFileSlot,
    /// A registered-file slot is not empty.
    FileSlotOccupied,
    /// A registered-file slot does not contain a lookup-visible owner.
    FileSlotEmpty,
    /// A file lease token does not name its exact active or retired generation.
    UnknownFileLease,
    /// Registered files are not yet lookup-visible.
    FileTableNotPublished,
    /// No cancellable request matches the supported selector.
    CancellationTargetNotFound,
    /// A registered-buffer table capacity was zero or exceeded the Linux limit.
    InvalidBufferTableCapacity,
    /// A registered-buffer lease budget was zero or exceeded the ring limit.
    InvalidBufferLeaseCapacity,
    /// Every configured registered-buffer execution lease is already charged.
    BufferLeaseCapacityExceeded,
    /// A registered-buffer slot index is outside the table.
    InvalidBufferSlot,
    /// A registered-buffer slot is not empty.
    BufferSlotOccupied,
    /// A registered-buffer slot does not contain a lookup-visible owner.
    BufferSlotEmpty,
    /// A buffer lease token does not name its exact active or retired generation.
    UnknownBufferLease,
    /// Registered buffers are not yet lookup-visible.
    BufferTableNotPublished,
    /// A fixed-buffer request range is outside its registered iovec.
    InvalidBufferRange,
    /// A copied io_uring registration header or argument is malformed.
    InvalidRegistration,
    /// A registration opcode is known but not implemented by this profile.
    UnsupportedRegistration,
    /// A registration opcode is outside the pinned Linux UAPI range.
    UnknownRegistration,
}
