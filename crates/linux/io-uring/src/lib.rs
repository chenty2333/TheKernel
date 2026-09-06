#![no_std]
#![forbid(unsafe_code)]

//! Pure Linux-visible `io_uring` policy and lifecycle contracts.
//!
//! This crate validates initial setup, enter, registration, and shared-ring
//! values, parses copied SQE bytes, reserves terminal completion capacity
//! before submission admission, and owns generation-safe request and
//! registered-file state. It deliberately does not dereference userspace
//! memory, perform atomic access to mapped pages, own a file descriptor, spawn
//! workers, register concrete readiness callbacks, execute VFS I/O, install a
//! signal mask, or map Linux errors to syscall return values.
//!
//! A kernel adapter must copy each 64-byte SQE exactly once after an acquire
//! load of the userspace SQ tail. It then parses that byte array here. On the
//! completion side, the adapter writes the complete CQE returned by the sole
//! publication plan, release-stores the plan's new CQ tail, and commits that
//! transaction back to the core before another publication or reap.

extern crate alloc;

mod buffer;
mod enter;
mod error;
mod registration;
mod request;
mod resource;
mod setup;
mod sqe;
mod uapi;

pub use buffer::{
    BufferInstallError, BufferLeaseRelease, BufferLeaseReleaseError, BufferSlot, BufferTableId,
    BufferTableLifecycle, BufferTableProgress, RegisteredBufferLease, RegisteredBufferRange,
    RegisteredBufferTable, RegisteredBufferToken, RetiredBuffer,
};
pub use enter::{EnterArgument, EnterFlags, EnterRequest, IoUringGeteventsArg, LegacySignalMask};
pub use error::IoUringError;
pub use registration::{
    IORING_MAX_PROBE_OPERATIONS, IORING_MAX_REGISTERED_BUFFERS, PINNED_IORING_REGISTER_LAST,
    RegistrationOperation, RegistrationRequest,
};
pub use request::{
    CancelSelector, CancellationMode, Completion, CompletionPublication, CompletionToken,
    IssuedRequest, PreparedRequest, ProviderCancelOutcome, RequestDescriptor, RequestId,
    RequestIssueError, RequestLifecycle, RequestOperation, RequestProgress, RequestRegistry,
    RequestReservation, RequestState, RequestTraceEvent, TerminalCause, TerminalPermit,
};
pub use resource::{
    FileInstallError, FileSlot, FileTableId, FileTableLifecycle, FileTableProgress, LeaseRelease,
    LeaseReleaseError, RegisteredFileLease, RegisteredFileTable, RegisteredFileToken, RetiredFile,
};
pub use setup::{
    CQE_BYTES, CompletionQueueOffsets, FeatureFlags, IORING_MAX_CQ_ENTRIES, IORING_MAX_ENTRIES,
    IORING_MAX_FIXED_FILES, IORING_OFF_CQ_RING, IORING_OFF_SQ_RING, IORING_OFF_SQES, MappingRegion,
    RING_HEADER_BYTES, RingId, RingLayout, SQE_BYTES, SetupFlags, SetupRequest,
    SubmissionQueueOffsets,
};
pub use sqe::{
    AcceptRequest, CloseRequest, CopiedSubmission, FadviseRequest, FallocateRequest, FileTarget,
    FsyncRequest, IoBuffer, PINNED_IORING_OP_LAST, ParsedSubmission, PollRequest,
    ProvideBuffersRequest, ReadWriteRequest, RemoveBuffersRequest, ShutdownRequest,
    SubmissionDependencies, SubmissionLink, SubmissionOpcodeSupport, SubmissionOperation,
    SyncFileRangeRequest, TimeoutRequest, UringCmdRequest, classify_submission_opcode,
};
pub use uapi::{
    DecodedSubmissionEntry, IO_URING_CQE_BYTES, IO_URING_PARAMS_BYTES, IO_URING_PROBE_HEADER_BYTES,
    IO_URING_PROBE_OP_BYTES, IO_URING_RING_OFFSETS_BYTES, IO_URING_SQE_BYTES, IORING_OP_SUPPORTED,
    IoUringParams, RawSubmissionEntry, RingOffsets, encode_probe, probe_operation_count,
    probe_output_bytes,
};
