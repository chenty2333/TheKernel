use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ops::Deref;

use axpoll::Pollable;

use super::NodeOps;
use crate::{VfsError, VfsResult, WritebackErrorState, path::FsPath};

/// Maximum logical range mapped while a filesystem-specific spin lock is
/// held.  Callers release and reacquire the backend lock between chunks.
pub const FILE_EXTENT_SCAN_CHUNK_BYTES: u64 = 16 * 1024 * 1024;

/// Provider verdict for an `RWF_NOWAIT` admission probe.  Cache residency,
/// remote RPC slots and daemon queue capacity are provider facts, not generic
/// poll readiness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NowaitAdmission {
    Ready,
    WouldBlock,
}

/// Filesystem-native range mutation requested by the Linux VFS adapter.
///
/// Keeping the operation typed prevents individual providers from decoding
/// Linux `fallocate(2)` flag combinations (and accidentally accepting a
/// combination with different semantics).  `keep_size` is meaningful only
/// for Allocate and ZeroRange; PunchHole always preserves the logical size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRangeOperation {
    Allocate { keep_size: bool },
    PunchHole,
    ZeroRange { keep_size: bool },
    CollapseRange,
    InsertRange,
    UnshareRange,
}

/// One overflow-checked file range operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileRangeRequest {
    pub operation: FileRangeOperation,
    pub offset: u64,
    pub length: u64,
    // Prevent callers in provider crates from bypassing `try_new` with a
    // struct literal.  The visible fields remain readable so this invariant
    // can be introduced without duplicating accessor boilerplate throughout
    // every filesystem adapter.
    _validated: (),
}

impl FileRangeRequest {
    pub fn try_new(operation: FileRangeOperation, offset: u64, length: u64) -> VfsResult<Self> {
        if length == 0 || offset.checked_add(length).is_none() {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self {
            operation,
            offset,
            length,
            _validated: (),
        })
    }

    pub fn end(self) -> u64 {
        self.offset
            .checked_add(self.length)
            .expect("validated FileRangeRequest overflowed")
    }
}

/// One allocated file extent returned by a filesystem mapping query.  This
/// includes unwritten allocations; usercopy happens only after the filesystem
/// lock has been released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileExtentState {
    Written,
    Unwritten,
}

/// One allocated file extent returned by a filesystem mapping query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileExtent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub state: FileExtentState,
}

impl FileExtent {
    pub const fn new(logical: u64, physical: u64, length: u64, state: FileExtentState) -> Self {
        Self {
            logical,
            physical,
            length,
            state,
        }
    }
}

/// Typed result of one file extent mapping query.
#[derive(Debug, Eq, PartialEq)]
pub struct FileExtentMap {
    /// The retained prefix.  It is empty for a count-only query.
    pub extents: Vec<FileExtent>,
    /// For a non-zero capacity this is the number of retained extents, not
    /// the total discovered count.  A zero-capacity query returns the exact
    /// discovered count here without allocating `extents`.
    pub mapped_extents: u32,
    /// Whether the complete range was scanned and all mapped extents were
    /// retained.  Count-only scans are complete by definition.
    pub complete: bool,
    /// Whether the scan reached the current end of the file.
    pub reaches_eof: bool,
}

impl FileExtentMap {
    pub fn new(extents: Vec<FileExtent>, mapped_extents: u32, complete: bool) -> Self {
        Self {
            extents,
            mapped_extents,
            complete,
            reaches_eof: false,
        }
    }
}

/// One caller-owned physical-memory range used by a synchronous direct I/O
/// request.
///
/// The range must remain pinned, DMA-accessible, and disjoint from every other
/// range for the complete call. This descriptor deliberately carries no
/// allocator, address-space, or driver dependency; the owner of the range is
/// responsible for its lifetime, access permissions, and any content race
/// between CPU access and device DMA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoSegment {
    /// Physical address of the first byte in the range.
    pub paddr: usize,
    /// Number of bytes in the range.
    pub len: usize,
}

impl PhysicalIoSegment {
    pub const fn new(paddr: usize, len: usize) -> Self {
        Self { paddr, len }
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Result of attempting a synchronous physical direct-I/O request.
///
/// `NotSubmitted` is deliberately typed: callers may use their pre-publish
/// fallback only for an operation which never reached the device.  Once a
/// lower layer returns `Completed`, later validation errors remain terminal
/// and must not be retried through a bounce buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoAttempt {
    Completed(usize),
    NotSubmitted(PhysicalIoNotSubmittedReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoNotSubmittedReason {
    /// The filesystem mapping or direct-I/O preflight did not admit the
    /// request (for example a hole, fragmented extent, or EOF range).
    Extent,
    /// The request was eligible in the filesystem, but device admission did
    /// not publish a descriptor (for example queue capacity or unsupported
    /// physical SG geometry).
    DeviceAdmission,
}

/// Result of an asynchronous vectored write attempt.
///
/// Submission/admission failures remain the outer [`VfsResult`] error: no
/// device request was accepted, so callers may retain their dirty state and
/// must not report a writeback completion error.  Once a request is accepted,
/// implementations return one of the two terminal outcomes below only after
/// it has completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsyncVectoredWriteOutcome {
    /// No asynchronous request was accepted and the caller may use its
    /// synchronous fallback path.
    NotSubmitted,
    /// An accepted request completed successfully.
    Completed(usize),
    /// An accepted request completed with this error.
    CompletionError(VfsError),
}

/// Direction of an owned direct-I/O operation.
///
/// This deliberately has no catch-all integer form.  A provider must make an
/// explicit decision before it accepts a new operation class into its
/// asynchronous execution domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIoOpcode {
    Read,
    Write,
}

/// Immutable placement selected before an owned request reaches a cache or
/// provider queue. `Append` is an inode-serialized operation, never a saved
/// EOF offset captured at submission time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIoWritePlacement {
    Positioned,
    Append,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIoSyncMode {
    None,
    Data,
    Full,
}

/// Immutable per-operation policy; Linux RWF bits are translated at the
/// syscall boundary and never reach a provider as an untyped integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIoPolicy {
    pub nowait: bool,
    pub sync: FileIoSyncMode,
    pub dontcache: bool,
    /// Required logical offset alignment for a direct operation.  The kernel
    /// validates buffer and length before reservation; an `Append` request
    /// validates this final component only after it owns the append domain
    /// and has selected the real EOF.
    pub direct_offset_alignment: Option<usize>,
}

impl FileIoPolicy {
    pub const DEFAULT: Self = Self {
        nowait: false,
        sync: FileIoSyncMode::None,
        dontcache: false,
        direct_offset_alignment: None,
    };
}

/// Caller-owned storage for one direct or asynchronous file operation.
///
/// There is intentionally no blanket implementation for byte containers.  A
/// buffer owner is responsible for pinning, DMA/cache maintenance, and for
/// keeping the allocation alive until it is returned in a terminal
/// completion.  This keeps the VFS layer independent of the kernel's page
/// pinning and usercopy machinery.
pub trait OwnedFileIoBuffer: Send + Sync {
    /// Number of bytes available to this request.
    fn len(&self) -> usize;

    /// Whether this owner can source writes or receive reads.
    fn supports(&self, access: FileIoBufferAccess) -> bool;

    /// Copies from this owner at `offset` into `destination`.  `offset` and
    /// the returned count are relative to `len`; returning more than
    /// `destination.len()` or the remaining owner range is a provider error.
    /// A short result is permitted only as the ordinary read short-copy
    /// result, never as an implicit bounds escape.
    fn source_copy_at(&self, offset: usize, destination: &mut [u8]) -> VfsResult<usize>;

    /// Copies `source` into this owner at `offset`.  The same strict bounds
    /// and short-copy rules apply.  No Rust slice of pinned user/DMA memory
    /// crosses this ABI.
    fn destination_copy_at(&mut self, offset: usize, source: &[u8]) -> VfsResult<usize>;

    /// Optional physical-SG inspection for a future zero-copy provider.
    /// Implementations may retain physical pin metadata without constructing
    /// a CPU slice; the default makes zero-copy explicitly unsupported.
    fn visit_physical_segments(
        &self,
        _visitor: &mut dyn FileIoPhysicalSegmentVisitor,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIoBufferAccess {
    Source,
    Destination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIoPhysicalSegment {
    pub paddr: usize,
    pub len: usize,
}

pub trait FileIoPhysicalSegmentVisitor {
    fn segment(&mut self, segment: FileIoPhysicalSegment) -> VfsResult<()>;
}

/// An owned request which can outlive the syscall stack and may be moved to a
/// provider worker without borrowing user memory.
pub struct FileIoRequest {
    opcode: FileIoOpcode,
    placement: FileIoWritePlacement,
    policy: FileIoPolicy,
    offset: u64,
    len: usize,
    buffer: Box<dyn OwnedFileIoBuffer>,
}

impl FileIoRequest {
    /// Validates the fixed request geometry and the buffer's exposed view
    /// before a provider may reserve an asynchronous operation.
    pub fn try_new(
        opcode: FileIoOpcode,
        offset: u64,
        buffer: Box<dyn OwnedFileIoBuffer>,
    ) -> VfsResult<Self> {
        Self::try_new_with_policy_and_placement(
            opcode,
            FileIoPolicy::DEFAULT,
            FileIoWritePlacement::Positioned,
            offset,
            buffer,
        )
    }

    pub fn try_new_with_placement(
        opcode: FileIoOpcode,
        placement: FileIoWritePlacement,
        offset: u64,
        buffer: Box<dyn OwnedFileIoBuffer>,
    ) -> VfsResult<Self> {
        Self::try_new_with_policy_and_placement(
            opcode,
            FileIoPolicy::DEFAULT,
            placement,
            offset,
            buffer,
        )
    }

    pub fn try_new_with_policy_and_placement(
        opcode: FileIoOpcode,
        policy: FileIoPolicy,
        placement: FileIoWritePlacement,
        offset: u64,
        buffer: Box<dyn OwnedFileIoBuffer>,
    ) -> VfsResult<Self> {
        let len = buffer.len();
        let len_u64 = u64::try_from(len).map_err(|_| VfsError::InvalidInput)?;
        if opcode == FileIoOpcode::Read && placement != FileIoWritePlacement::Positioned {
            return Err(VfsError::InvalidInput);
        }
        // An append request's supplied offset is intentionally non-geometric:
        // Linux ignores it and the provider selects EOF later while holding
        // its append domain.  Do not turn a harmless stale/overflowing value
        // into a submission failure or accidentally validate it as EOF.
        if placement == FileIoWritePlacement::Positioned {
            offset.checked_add(len_u64).ok_or(VfsError::InvalidInput)?;
        }
        if let Some(alignment) = policy.direct_offset_alignment
            && (alignment == 0 || !alignment.is_power_of_two() || len % alignment != 0)
        {
            return Err(VfsError::InvalidInput);
        }
        if placement == FileIoWritePlacement::Positioned
            && let Some(alignment) = policy.direct_offset_alignment
            && !offset.is_multiple_of(alignment as u64)
        {
            return Err(VfsError::InvalidInput);
        }
        let required = match opcode {
            FileIoOpcode::Read => FileIoBufferAccess::Destination,
            FileIoOpcode::Write => FileIoBufferAccess::Source,
        };
        if len != 0 && !buffer.supports(required) {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self {
            opcode,
            placement,
            policy,
            offset,
            len,
            buffer,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn opcode(&self) -> FileIoOpcode {
        self.opcode
    }

    pub fn placement(&self) -> FileIoWritePlacement {
        self.placement
    }

    pub fn policy(&self) -> FileIoPolicy {
        self.policy
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn source_copy_at(&self, offset: usize, destination: &mut [u8]) -> VfsResult<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        if self.opcode != FileIoOpcode::Write || !self.buffer.supports(FileIoBufferAccess::Source) {
            return Err(VfsError::InvalidInput);
        }
        let remaining = self.len.checked_sub(offset).ok_or(VfsError::InvalidInput)?;
        if destination.len() > remaining {
            return Err(VfsError::InvalidInput);
        }
        let copied = self.buffer.source_copy_at(offset, destination)?;
        if copied > destination.len() || copied > remaining {
            return Err(VfsError::Io);
        }
        Ok(copied)
    }

    pub fn destination_copy_at(&mut self, offset: usize, source: &[u8]) -> VfsResult<usize> {
        if source.is_empty() {
            return Ok(0);
        }
        if self.opcode != FileIoOpcode::Read
            || !self.buffer.supports(FileIoBufferAccess::Destination)
        {
            return Err(VfsError::InvalidInput);
        }
        let remaining = self.len.checked_sub(offset).ok_or(VfsError::InvalidInput)?;
        if source.len() > remaining {
            return Err(VfsError::InvalidInput);
        }
        let copied = self.buffer.destination_copy_at(offset, source)?;
        if copied > source.len() || copied > remaining {
            return Err(VfsError::Io);
        }
        Ok(copied)
    }

    pub fn into_buffer(self) -> Box<dyn OwnedFileIoBuffer> {
        self.buffer
    }
}

/// Immediate terminal result for an accepted owned operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImmediateFileIoResult {
    Completed(usize),
    Failed(VfsError),
    /// The provider removed this accepted request before its lower I/O became
    /// irrevocable.  Kernel adapters map this distinct terminal state to
    /// ECANCELED rather than conflating it with signal interruption.
    Cancelled,
}

/// The one terminal message delivered to the owner of an accepted request.
/// The buffer returns with the completion regardless of success or failure.
pub struct FileIoCompletion {
    pub result: ImmediateFileIoResult,
    /// Actual start offset. Append providers set this after acquiring their
    /// inode append domain; positioned requests retain `request.offset()`.
    pub actual_offset: u64,
    pub request: FileIoRequest,
}

/// A one-shot completion sink for an owned file-I/O request.
///
/// Consuming the boxed sink makes duplicate completion impossible at the API
/// boundary.  The VFS wrapper synthesizes a terminal EIO if a prepared
/// operation is abandoned; after publish, the provider queue owns terminal
/// delivery and a dropped submitted control handle has no data side effect.
pub trait OwnedFileIoCompletion: Send + Sync {
    fn complete(self: Box<Self>, completion: FileIoCompletion);

    /// Removes VFS-owned pre-publication adapters before returning a request
    /// to its caller for retry.  Plain completion sinks are already retry
    /// safe; wrappers which add terminal metadata work override this to avoid
    /// stacking their effects across failed prepare/publish attempts.
    fn into_retry_completion(self: Box<Self>) -> Box<dyn OwnedFileIoCompletion>;
}

/// Restricted provider view of an owned request.  It deliberately cannot move
/// or replace the request/buffer; the VFS wrapper retains that ownership.
pub trait FileIoRequestAccess {
    fn opcode(&self) -> FileIoOpcode;
    fn placement(&self) -> FileIoWritePlacement;
    fn policy(&self) -> FileIoPolicy;
    fn offset(&self) -> u64;
    fn len(&self) -> usize;
    fn source_copy_at(&self, offset: usize, destination: &mut [u8]) -> VfsResult<usize>;
    fn destination_copy_at(&mut self, offset: usize, source: &[u8]) -> VfsResult<usize>;
}

/// Immutable request geometry available before publication only.
pub trait FileIoRequestGeometry {
    fn opcode(&self) -> FileIoOpcode;
    fn placement(&self) -> FileIoWritePlacement;
    fn policy(&self) -> FileIoPolicy;
    fn offset(&self) -> u64;
    fn len(&self) -> usize;
}
impl FileIoRequestGeometry for FileIoRequest {
    fn opcode(&self) -> FileIoOpcode {
        self.opcode()
    }
    fn placement(&self) -> FileIoWritePlacement {
        self.placement()
    }
    fn policy(&self) -> FileIoPolicy {
        self.policy()
    }
    fn offset(&self) -> u64 {
        self.offset()
    }
    fn len(&self) -> usize {
        self.len()
    }
}

impl FileIoRequestAccess for FileIoRequest {
    fn opcode(&self) -> FileIoOpcode {
        FileIoRequest::opcode(self)
    }
    fn placement(&self) -> FileIoWritePlacement {
        FileIoRequest::placement(self)
    }
    fn policy(&self) -> FileIoPolicy {
        FileIoRequest::policy(self)
    }
    fn offset(&self) -> u64 {
        FileIoRequest::offset(self)
    }
    fn len(&self) -> usize {
        FileIoRequest::len(self)
    }
    fn source_copy_at(&self, offset: usize, destination: &mut [u8]) -> VfsResult<usize> {
        FileIoRequest::source_copy_at(self, offset, destination)
    }
    fn destination_copy_at(&mut self, offset: usize, source: &[u8]) -> VfsResult<usize> {
        FileIoRequest::destination_copy_at(self, offset, source)
    }
}

/// Opaque payload passed to an async publication hook.  It can only be
/// returned unchanged through `fail`, or consumed into the queue's owned
/// request/sink pair on successful publication.
pub struct FileIoPublishPayload {
    request: Option<FileIoRequest>,
    completion: Option<Box<dyn OwnedFileIoCompletion>>,
}

impl FileIoPublishPayload {
    fn new(request: FileIoRequest, completion: Box<dyn OwnedFileIoCompletion>) -> Self {
        Self {
            request: Some(request),
            completion: Some(completion),
        }
    }
    /// Pre-publication inspection cannot read/write buffer contents.
    pub fn geometry(&self) -> &dyn FileIoRequestGeometry {
        self.request
            .as_ref()
            .expect("file I/O publish request missing")
    }
    /// This consuming transition is valid only at the provider's final,
    /// infallible queue publication boundary.
    pub fn commit(mut self) -> PublishedFileIoPayload {
        PublishedFileIoPayload {
            request: self.request.take(),
            completion: self.completion.take(),
        }
    }
    fn into_parts(mut self) -> (FileIoRequest, Box<dyn OwnedFileIoCompletion>) {
        (
            self.request
                .take()
                .expect("file I/O publish request missing"),
            self.completion
                .take()
                .expect("file I/O publish completion missing"),
        )
    }
    pub fn fail(self, error: VfsError) -> FileIoPublishError {
        FileIoPublishError {
            error,
            payload: self,
        }
    }
}

/// Accepted queue payload.  Only this post-publication type may copy buffer
/// bytes or emit a terminal completion.
pub struct PublishedFileIoPayload {
    request: Option<FileIoRequest>,
    completion: Option<Box<dyn OwnedFileIoCompletion>>,
}
impl PublishedFileIoPayload {
    pub fn with_request<R>(
        &mut self,
        use_request: impl FnOnce(&mut dyn FileIoRequestAccess) -> R,
    ) -> R {
        use_request(
            self.request
                .as_mut()
                .expect("published file I/O request missing"),
        )
    }
    pub fn complete(self, result: ImmediateFileIoResult) {
        let actual_offset = self
            .request
            .as_ref()
            .expect("published file I/O request missing")
            .offset();
        self.complete_at(result, actual_offset);
    }
    pub fn complete_at(mut self, result: ImmediateFileIoResult, actual_offset: u64) {
        let request = self
            .request
            .take()
            .expect("published file I/O request missing");
        let completion = self
            .completion
            .take()
            .expect("published file I/O completion missing");
        completion.complete(FileIoCompletion {
            result: normalize_file_io_result(result, request.len()),
            actual_offset,
            request,
        });
    }
}

impl Drop for FileIoPublishPayload {
    fn drop(&mut self) {
        abandon_owned_file_io(&mut self.request, &mut self.completion);
    }
}
impl Drop for PublishedFileIoPayload {
    fn drop(&mut self) {
        abandon_owned_file_io(&mut self.request, &mut self.completion);
    }
}

pub struct FileIoPublishError {
    error: VfsError,
    payload: FileIoPublishPayload,
}
impl FileIoPublishError {
    fn into_parts(self) -> (VfsError, FileIoRequest, Box<dyn OwnedFileIoCompletion>) {
        let (request, completion) = self.payload.into_parts();
        (self.error, request, completion)
    }
}

/// Provider-owned state reserved during `FileNodeOps::prepare_file_io`.
///
/// `publish` consumes the exact reservation and receives both owned request
/// objects, so direct NOWAIT and deferred AIO/io_uring retain their routing
/// rather than being collapsed into the legacy borrowed I/O methods.
pub trait PreparedFileIoSubmission: Send + Sync {
    fn publish(
        self: Box<Self>,
        payload: FileIoPublishPayload,
    ) -> Result<SubmittedFileIo, FileIoPublishError>;

    /// Executes a prepared NOWAIT operation in the caller context without
    /// publishing it to a worker or device queue.  The provider returns the
    /// terminal result and the same owned request; the VFS wrapper validates
    /// it and delivers the completion sink exactly once.
    fn try_complete_immediate(
        self: Box<Self>,
        request: &mut dyn FileIoRequestAccess,
    ) -> VfsResult<ImmediateFileIoResult>;
}

/// A prepare failure which returns every object the caller still owns.
pub struct FileIoPrepareError {
    pub error: VfsError,
    pub request: FileIoRequest,
    pub completion: Box<dyn OwnedFileIoCompletion>,
}

impl FileIoPrepareError {
    pub fn new(
        error: VfsError,
        request: FileIoRequest,
        completion: Box<dyn OwnedFileIoCompletion>,
    ) -> Self {
        Self {
            error,
            request,
            completion,
        }
    }
}

/// A provider-approved request which has not yet been published to hardware
/// or a worker.  Dropping it before returning ownership emits exactly one
/// terminal EIO completion.
pub struct PreparedFileIo {
    request: Option<FileIoRequest>,
    completion: Option<Box<dyn OwnedFileIoCompletion>>,
    submission: Option<Box<dyn PreparedFileIoSubmission>>,
}

impl PreparedFileIo {
    pub fn new(
        request: FileIoRequest,
        completion: Box<dyn OwnedFileIoCompletion>,
        submission: Box<dyn PreparedFileIoSubmission>,
    ) -> Self {
        Self {
            request: Some(request),
            completion: Some(completion),
            submission: Some(submission),
        }
    }

    pub fn request(&self) -> &FileIoRequest {
        self.request
            .as_ref()
            .expect("prepared file I/O request missing")
    }

    /// Returns ownership before publication.  No completion is emitted.
    pub fn abort(mut self) -> (FileIoRequest, Box<dyn OwnedFileIoCompletion>) {
        let request = self
            .request
            .take()
            .expect("prepared file I/O request missing");
        let completion = self
            .completion
            .take()
            .expect("prepared file I/O completion missing");
        (request, completion)
    }

    /// Publishes this already-prepared request.  This wrapper performs no
    /// allocation; providers may move it directly into a pre-reserved slot.
    pub fn submit(mut self) -> Result<SubmittedFileIo, FileIoPrepareError> {
        let request = self
            .request
            .take()
            .expect("prepared file I/O request missing");
        let completion = self
            .completion
            .take()
            .expect("prepared file I/O completion missing");
        match self
            .submission
            .take()
            .expect("prepared file I/O submission missing")
            .publish(FileIoPublishPayload::new(request, completion))
        {
            Ok(submitted) => Ok(submitted),
            Err(error) => {
                let (error, request, completion) = error.into_parts();
                Err(FileIoPrepareError::new(error, request, completion))
            }
        }
    }

    /// Consumes the provider reservation through its caller-context NOWAIT
    /// hook.  No request is ever published on this route.
    pub fn try_complete_immediate(mut self) -> Result<ImmediateFileIoResult, FileIoPrepareError> {
        let request = self
            .request
            .take()
            .expect("prepared file I/O request missing");
        let completion = self
            .completion
            .take()
            .expect("prepared file I/O completion missing");
        let submission = self
            .submission
            .take()
            .expect("prepared file I/O submission missing");
        let mut request = request;
        match submission.try_complete_immediate(&mut request) {
            Ok(result) => {
                let result = normalize_file_io_result(result, request.len());
                let actual_offset = request.offset();
                completion.complete(FileIoCompletion {
                    result,
                    actual_offset,
                    request,
                });
                Ok(result)
            }
            Err(error) => Err(FileIoPrepareError::new(error, request, completion)),
        }
    }
}

/// An accepted request awaiting its unique terminal completion.
pub struct SubmittedFileIo {
    control: Option<Box<dyn SubmittedFileIoControl>>,
}

/// Consuming control for a queue-owned asynchronous operation.
pub trait SubmittedFileIoControl: Send + Sync {
    fn cancel(self: Box<Self>) -> FileIoCancelOutcome;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIoCancelOutcome {
    /// The queue removed the request and delivered its unique terminal error.
    Cancelled,
    /// A worker already owns the request and will deliver completion.
    InFlight,
    /// The request was already completed, failed, or cancelled.
    Terminal,
}

impl SubmittedFileIo {
    pub fn new(control: Box<dyn SubmittedFileIoControl>) -> Self {
        Self {
            control: Some(control),
        }
    }

    pub fn cancel(mut self) -> FileIoCancelOutcome {
        self.control
            .take()
            .expect("submitted file I/O control missing")
            .cancel()
    }
}

fn normalize_file_io_result(result: ImmediateFileIoResult, len: usize) -> ImmediateFileIoResult {
    match result {
        ImmediateFileIoResult::Completed(bytes) if bytes > len => {
            ImmediateFileIoResult::Failed(VfsError::Io)
        }
        result => result,
    }
}

fn abandon_owned_file_io(
    request: &mut Option<FileIoRequest>,
    completion: &mut Option<Box<dyn OwnedFileIoCompletion>>,
) {
    if let (Some(request), Some(completion)) = (request.take(), completion.take()) {
        completion.complete(FileIoCompletion {
            result: ImmediateFileIoResult::Failed(VfsError::Io),
            actual_offset: request.offset(),
            request,
        });
    }
}

impl Drop for PreparedFileIo {
    fn drop(&mut self) {
        abandon_owned_file_io(&mut self.request, &mut self.completion);
    }
}

pub trait FileNodeOps: NodeOps + Pollable {
    /// Prepares a direct or asynchronous operation using caller-owned,
    /// lifetime-safe storage.  Legacy `read_at`/`write_at` methods are not an
    /// implementation of this effect: their borrowed buffers cannot be
    /// published after the call returns.
    ///
    /// Providers which have not implemented the full prepare/publish/cancel
    /// protocol return `OperationNotSupported` and return the request and
    /// sink unchanged.  They must never claim support merely because their
    /// synchronous path happens to be pollable.
    fn prepare_file_io(
        &self,
        request: FileIoRequest,
        completion: Box<dyn OwnedFileIoCompletion>,
    ) -> Result<PreparedFileIo, FileIoPrepareError> {
        Err(FileIoPrepareError::new(
            VfsError::OperationNotSupported,
            request,
            completion,
        ))
    }

    /// Whether this concrete provider implements Linux's `FMODE_NOWAIT`
    /// read contract.  Being a regular inode or merely pollable is not an
    /// assertion of this capability.
    fn supports_nowait_read(&self) -> bool {
        false
    }

    /// Whether this concrete provider implements Linux's `FMODE_NOWAIT`
    /// write contract.
    fn supports_nowait_write(&self) -> bool {
        false
    }

    /// Probes whether a read can issue without waiting.  The conservative
    /// default must not claim readiness for an unconverted provider.
    fn nowait_read_admit(&self, _offset: u64, _length: usize) -> VfsResult<NowaitAdmission> {
        Ok(NowaitAdmission::WouldBlock)
    }

    /// Probes nonblocking write admission before beginning any mutation.
    fn nowait_write_admit(&self, _offset: u64, _length: usize) -> VfsResult<NowaitAdmission> {
        Ok(NowaitAdmission::WouldBlock)
    }
    /// Builds the backend object for one successful non-`O_PATH` open.
    ///
    /// Most native filesystems have stateless file I/O and retain the default
    /// `None`.  Stateful remote filesystems (notably FUSE and NFS) return an
    /// opaque per-open object here so an inode-level node can never leak a
    /// daemon file handle between independent open file descriptions.
    ///
    /// The returned object must retain all state needed through final close;
    /// callers route read, write, readdir, locking and release through this
    /// object rather than reconstructing an inode-global handle.
    fn open_handle(
        &self,
        _read: bool,
        _write: bool,
        _flags: u32,
    ) -> VfsResult<Option<Arc<dyn FileNodeOps>>> {
        Ok(None)
    }

    /// Releases one per-open backend handle.  This is separate from `Drop`:
    /// a provider may be retained by cached dentries after the OFD closes,
    /// while a FUSE RELEASE must be emitted at the OFD lifetime boundary.
    fn release_handle(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Returns the superblock errseq source when this node participates in
    /// asynchronous filesystem writeback.  It is distinct from the inode
    /// errseq returned through `NodeOps::writeback_error_state`.
    fn syncfs_writeback_error_state(&self) -> Option<Arc<WritebackErrorState>> {
        None
    }

    /// Returns the exclusive upper bound for extent queries on this file's
    /// filesystem. This is the filesystem's representable file-size limit,
    /// not a Linux ABI policy.
    fn max_extent_bytes(&self) -> VfsResult<u64> {
        Err(VfsError::OperationNotSupported)
    }

    /// Whether this file can provide allocated extent mappings.
    ///
    /// This capability is intentionally queryable without user input so a
    /// Linux adapter can reject an unsupported ioctl before touching user
    /// memory.
    fn supports_extent_mapping(&self) -> bool {
        false
    }

    /// Collects allocated file extents intersecting `[start, start + length)`.
    /// Holes are omitted. A zero capacity is a complete count-only scan and
    /// must not allocate an extent buffer; a non-zero capacity retains only a
    /// prefix and reports the retained count in `mapped_extents`.
    ///
    /// No userspace pointers or Linux ABI structs cross this VFS boundary.
    fn map_extents(&self, start: u64, length: u64, max_extents: usize) -> VfsResult<FileExtentMap> {
        let _ = (start, length, max_extents);
        Err(VfsError::OperationNotSupported)
    }

    /// Applies an allocation/extents mutation to a regular file.
    ///
    /// The high-level file layer has already serialized the operation with
    /// page-cache writeback and invalidated aliases.  Providers must commit
    /// data and allocation metadata atomically according to their native
    /// transaction model; unsupported filesystems return the conservative
    /// default without changing data.
    fn mutate_range(&self, _request: FileRangeRequest) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    /// Reads a number of bytes starting from a given offset.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize>;

    /// Reads data into a scatter list starting from a given offset.
    fn read_at_vectored(&self, bufs: &mut [&mut [u8]], mut offset: u64) -> VfsResult<usize> {
        let mut total = 0usize;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let read = match self.read_at(buf, offset) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += read;
            offset = offset
                .checked_add(read as u64)
                .ok_or(VfsError::InvalidInput)?;
            if read < requested || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Attempts to read data into a scatter list through an asynchronous
    /// lower-device path.
    ///
    /// Implementations must return only after accepted device requests have
    /// completed, but may split submit and wait internally to avoid holding
    /// filesystem locks across a blocking wait. `Ok(None)` means the caller
    /// should use [`read_at_vectored`](Self::read_at_vectored).
    fn try_read_at_vectored_async(
        &self,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let _ = bufs;
        let _ = offset;
        Ok(None)
    }

    /// Attempts a synchronous direct read into caller-pinned physical memory.
    ///
    /// The implementation must not construct a Rust slice from a physical
    /// address. `Ok(None)` means that the caller may use its ordinary fallback
    /// path; an error is terminal for this request and must not be treated as
    /// permission to retry through a bounce buffer.
    ///
    /// # Safety
    ///
    /// Every non-empty segment must remain pinned, DMA-accessible, writable,
    /// and disjoint from all other segments until this method returns. The
    /// caller is responsible for content races caused by concurrent CPU access
    /// to the DMA range; such races do not create Rust references from paddr.
    unsafe fn try_read_at_physical(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let _ = (segments, offset);
        Ok(None)
    }

    /// Typed form of [`Self::try_read_at_physical`].  The default adapter is
    /// intentionally conservative: an implementation which has no physical
    /// hook reports device admission failure, which is still unpublished and
    /// therefore safe for the caller's fallback path.
    unsafe fn try_read_at_physical_with_reason(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        Ok(
            match unsafe { self.try_read_at_physical(segments, offset)? } {
                Some(bytes) => PhysicalIoAttempt::Completed(bytes),
                None => {
                    PhysicalIoAttempt::NotSubmitted(PhysicalIoNotSubmittedReason::DeviceAdmission)
                }
            },
        )
    }

    /// Performs a side-effect-free capability and mapping preflight for a
    /// physical read.  The high-level direct backend runs this while holding
    /// its direct-I/O exclusion, before it invalidates cached pages; a true
    /// result therefore remains eligible until the matching unsafe hook call.
    fn physical_read_eligible(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<bool> {
        let _ = (segments, offset);
        Ok(false)
    }

    /// Writes a number of bytes starting from a given offset.
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize>;

    /// Shares a source range with this destination when the filesystem owns a
    /// native reflink transaction.  The source is exposed as `NodeOps` rather
    /// than a concrete file type so cross-filesystem attempts can be rejected
    /// from stable object identity without downcasting another provider.
    fn clone_range_from(
        &self,
        _source: &dyn NodeOps,
        _source_offset: u64,
        _destination_offset: u64,
        _len: u64,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    /// Verifies and shares a range.  `Ok(false)` means byte mismatch with no
    /// namespace/data mutation; unsupported providers use the fail-closed
    /// default rather than silently copying through a dedupe request.
    fn dedupe_range_from(
        &self,
        _source: &dyn NodeOps,
        _source_offset: u64,
        _destination_offset: u64,
        _len: u64,
    ) -> VfsResult<bool> {
        Err(VfsError::OperationNotSupported)
    }

    /// Writes data from a scatter list starting from a given offset.
    fn write_at_vectored(&self, bufs: &[&[u8]], mut offset: u64) -> VfsResult<usize> {
        let mut total = 0usize;
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = match self.write_at(buf, offset) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            offset = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Attempts to write data from a scatter list through an asynchronous
    /// lower-device path.
    ///
    /// Implementations must return only after accepted device requests have
    /// completed, but may split submit and wait internally to avoid holding
    /// filesystem locks across a blocking wait. [`AsyncVectoredWriteOutcome::NotSubmitted`]
    /// means the caller should use [`write_at_vectored`](Self::write_at_vectored).
    /// Errors in the outer result occurred before a request was accepted.
    fn try_write_at_vectored_async(
        &self,
        bufs: &[&[u8]],
        offset: u64,
    ) -> VfsResult<AsyncVectoredWriteOutcome> {
        let _ = bufs;
        let _ = offset;
        Ok(AsyncVectoredWriteOutcome::NotSubmitted)
    }

    /// Attempts a synchronous direct overwrite from caller-pinned physical
    /// memory. The operation must not extend the file.
    ///
    /// # Safety
    ///
    /// Every non-empty segment must remain pinned, DMA-accessible, readable,
    /// and disjoint from all other segments until this method returns. The
    /// caller is responsible for content races caused by concurrent CPU access
    /// to the DMA range; such races do not create Rust references from paddr.
    unsafe fn try_write_at_physical(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let _ = (segments, offset);
        Ok(None)
    }

    /// Typed form of [`Self::try_write_at_physical`]; see the read-side
    /// contract for the publish boundary and fallback rule.
    unsafe fn try_write_at_physical_with_reason(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        Ok(
            match unsafe { self.try_write_at_physical(segments, offset)? } {
                Some(bytes) => PhysicalIoAttempt::Completed(bytes),
                None => {
                    PhysicalIoAttempt::NotSubmitted(PhysicalIoNotSubmittedReason::DeviceAdmission)
                }
            },
        )
    }

    /// Performs a side-effect-free capability and mapping preflight for a
    /// physical overwrite.  It must not publish a descriptor or touch file
    /// data; the high-level direct backend calls the unsafe hook only after a
    /// true result and cache invalidation under the same direct-I/O lock.
    fn physical_write_eligible(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<bool> {
        let _ = (segments, offset);
        Ok(false)
    }

    /// Appends data to the file.
    ///
    /// Returns `(written, offset)` where `written` is the number of bytes
    /// written and `offset` is the new file size.
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)>;

    /// Sets the size of the file.
    ///
    /// Unless [`set_len_failure_is_atomic`](Self::set_len_failure_is_atomic)
    /// returns `true`, an error may be reported after the implementation has
    /// changed file data, allocation metadata, or the visible length. Cache
    /// users must therefore invalidate any pages that could have become stale.
    fn set_len(&self, len: u64) -> VfsResult<()>;

    /// Whether a failed [`set_len`](Self::set_len) leaves all file data,
    /// allocation metadata, and the visible length unchanged.
    ///
    /// Implementations must return `true` only when this is a stable guarantee
    /// for every error path. Callers may retain and restore pre-operation cache
    /// pages based on this contract. The conservative default is `false`.
    fn set_len_failure_is_atomic(&self) -> bool {
        false
    }

    /// Sets the file's symlink target.
    fn set_symlink(&self, target: &FsPath) -> VfsResult<()>;
}

#[repr(transparent)]
pub struct FileNode(Arc<dyn FileNodeOps>);

impl Deref for FileNode {
    type Target = dyn FileNodeOps;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl From<FileNode> for Arc<dyn NodeOps> {
    fn from(node: FileNode) -> Self {
        node.0.clone()
    }
}

impl FileNode {
    pub fn new(ops: Arc<dyn FileNodeOps>) -> Self {
        Self(ops)
    }

    pub fn inner(&self) -> &Arc<dyn FileNodeOps> {
        &self.0
    }

    /// Borrows the type-erased base node implemented by this file inode.
    ///
    /// Keeping the trait upcast at the VFS wrapper boundary avoids relying on
    /// deref coercion from `FileNode` at provider call sites.
    pub fn as_node_ops(&self) -> &dyn NodeOps {
        self.0.as_ref()
    }

    pub fn downcast<T: FileNodeOps>(self: &Arc<Self>) -> VfsResult<Arc<T>> {
        self.0
            .clone()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }

    /// Clones the node's owned trait object and downcasts it without requiring
    /// an `Arc<FileNode>` at the call site.  The returned `Arc` is an owned
    /// worker-safe inode reference; no VFS borrow crosses an await boundary.
    pub fn downcast_owned<T: FileNodeOps>(&self) -> VfsResult<Arc<T>> {
        self.0
            .clone()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }
}
