use crate::{
    BufferSlot, FileSlot, IoUringError, RawSubmissionEntry, RequestDescriptor, RequestOperation,
    SQE_BYTES,
};

const IOSQE_FIXED_FILE: u8 = 1 << 0;
const IOSQE_IO_DRAIN: u8 = 1 << 1;
const IOSQE_IO_LINK: u8 = 1 << 2;
const IOSQE_IO_HARDLINK: u8 = 1 << 3;
/// The SQE selects one lease from `buf_group` instead of using `addr`.
const IOSQE_BUFFER_SELECT: u8 = 1 << 5;

const IORING_OP_NOP: u8 = 0;
const IORING_OP_READV: u8 = 1;
const IORING_OP_WRITEV: u8 = 2;
const IORING_OP_FSYNC: u8 = 3;
const IORING_OP_CLOSE: u8 = 19;
const IORING_OP_FADVISE: u8 = 24;
const IORING_OP_SYNC_FILE_RANGE: u8 = 8;
const IORING_OP_FALLOCATE: u8 = 17;
const IORING_OP_SHUTDOWN: u8 = 34;
const IORING_OP_POLL_ADD: u8 = 6;
const IORING_OP_POLL_REMOVE: u8 = 7;
const IORING_OP_ASYNC_CANCEL: u8 = 14;
const IORING_OP_TIMEOUT: u8 = 11;
const IORING_OP_ACCEPT: u8 = 13;
const IORING_OP_TIMEOUT_REMOVE: u8 = 12;
const IORING_OP_READ_FIXED: u8 = 4;
const IORING_OP_WRITE_FIXED: u8 = 5;
const IORING_OP_READ: u8 = 22;
const IORING_OP_WRITE: u8 = 23;
const IORING_OP_SEND: u8 = 26;
const IORING_OP_RECV: u8 = 27;
const IORING_OP_OPENAT2: u8 = 28;
const IORING_OP_PROVIDE_BUFFERS: u8 = 31;
const IORING_OP_REMOVE_BUFFERS: u8 = 32;
const IORING_OP_URING_CMD: u8 = 46;
/// First opcode outside the Linux v7.2.3 UAPI enum.
pub const PINNED_IORING_OP_LAST: u8 = 65;

/// Pinned Linux v7.2.3 classification used by parsing and `REGISTER_PROBE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmissionOpcodeSupport {
    /// Implemented by this initial policy core.
    Supported(RequestOperation),
    /// Present in Linux v7.2.3 but not implemented by this profile.
    KnownUnsupported,
    /// Outside the Linux v7.2.3 UAPI enum.
    Unknown,
}

/// Dependency policy carried by the generic IOSQE flag vocabulary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubmissionLink {
    #[default]
    None,
    Soft,
    Hard,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubmissionDependencies {
    drain: bool,
    link: SubmissionLink,
}

impl SubmissionDependencies {
    const fn parse(bits: u8) -> Result<Self, IoUringError> {
        if bits & IOSQE_IO_LINK != 0 && bits & IOSQE_IO_HARDLINK != 0 {
            return Err(IoUringError::UnsupportedSubmissionFlags);
        }
        Ok(Self {
            drain: bits & IOSQE_IO_DRAIN != 0,
            link: if bits & IOSQE_IO_HARDLINK != 0 {
                SubmissionLink::Hard
            } else if bits & IOSQE_IO_LINK != 0 {
                SubmissionLink::Soft
            } else {
                SubmissionLink::None
            },
        })
    }
    pub const fn drain(self) -> bool {
        self.drain
    }
    pub const fn link(self) -> SubmissionLink {
        self.link
    }
}

/// Classifies one raw opcode against Linux v7.2.3.
pub const fn classify_submission_opcode(opcode: u8) -> SubmissionOpcodeSupport {
    match opcode {
        IORING_OP_NOP => SubmissionOpcodeSupport::Supported(RequestOperation::Nop),
        IORING_OP_READV => SubmissionOpcodeSupport::Supported(RequestOperation::Read),
        IORING_OP_WRITEV => SubmissionOpcodeSupport::Supported(RequestOperation::Write),
        IORING_OP_FSYNC => SubmissionOpcodeSupport::Supported(RequestOperation::Fsync),
        IORING_OP_CLOSE => SubmissionOpcodeSupport::Supported(RequestOperation::Close),
        IORING_OP_FADVISE => SubmissionOpcodeSupport::Supported(RequestOperation::Fadvise),
        IORING_OP_SYNC_FILE_RANGE => {
            SubmissionOpcodeSupport::Supported(RequestOperation::SyncFileRange)
        }
        IORING_OP_FALLOCATE => SubmissionOpcodeSupport::Supported(RequestOperation::Fallocate),
        IORING_OP_SHUTDOWN => SubmissionOpcodeSupport::Supported(RequestOperation::Shutdown),
        IORING_OP_POLL_ADD => SubmissionOpcodeSupport::Supported(RequestOperation::PollAdd),
        IORING_OP_POLL_REMOVE => SubmissionOpcodeSupport::Supported(RequestOperation::PollRemove),
        IORING_OP_ASYNC_CANCEL => SubmissionOpcodeSupport::Supported(RequestOperation::AsyncCancel),
        IORING_OP_TIMEOUT => SubmissionOpcodeSupport::Supported(RequestOperation::Timeout),
        IORING_OP_TIMEOUT_REMOVE => {
            SubmissionOpcodeSupport::Supported(RequestOperation::TimeoutRemove)
        }
        IORING_OP_ACCEPT => SubmissionOpcodeSupport::Supported(RequestOperation::Accept),
        IORING_OP_READ_FIXED => SubmissionOpcodeSupport::Supported(RequestOperation::Read),
        IORING_OP_WRITE_FIXED => SubmissionOpcodeSupport::Supported(RequestOperation::Write),
        IORING_OP_READ => SubmissionOpcodeSupport::Supported(RequestOperation::Read),
        IORING_OP_WRITE => SubmissionOpcodeSupport::Supported(RequestOperation::Write),
        IORING_OP_SEND => SubmissionOpcodeSupport::Supported(RequestOperation::Write),
        IORING_OP_RECV => SubmissionOpcodeSupport::Supported(RequestOperation::Read),
        IORING_OP_OPENAT2 => SubmissionOpcodeSupport::Supported(RequestOperation::OpenAt2),
        IORING_OP_PROVIDE_BUFFERS => {
            SubmissionOpcodeSupport::Supported(RequestOperation::ProvideBuffers)
        }
        IORING_OP_REMOVE_BUFFERS => {
            SubmissionOpcodeSupport::Supported(RequestOperation::RemoveBuffers)
        }
        IORING_OP_URING_CMD => SubmissionOpcodeSupport::Supported(RequestOperation::UringCmd),
        opcode if opcode < PINNED_IORING_OP_LAST => SubmissionOpcodeSupport::KnownUnsupported,
        _ => SubmissionOpcodeSupport::Unknown,
    }
}

/// A normal descriptor or generation-checked fixed-file slot reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTarget {
    /// Resolve this process-local descriptor through the caller's FD table.
    Descriptor(u32),
    /// Acquire a lease from this ring's registered-file table.
    Registered(FileSlot),
}

impl FileTarget {
    fn from_sqe(fd: i32, fixed: bool) -> Result<Self, IoUringError> {
        let raw = u32::try_from(fd).map_err(|_| IoUringError::InvalidFileTarget)?;
        if fixed {
            Ok(Self::Registered(FileSlot::new(raw)))
        } else {
            Ok(Self::Descriptor(raw))
        }
    }
}

/// Checked userspace I/O buffer geometry copied from one SQE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoBuffer {
    address: u64,
    length: u32,
}

impl IoBuffer {
    /// Builds a buffer while rejecting address-length overflow.
    pub const fn new(address: u64, length: u32) -> Result<Self, IoUringError> {
        if address.checked_add(length as u64).is_none() {
            return Err(IoUringError::InvalidSubmission);
        }
        Ok(Self { address, length })
    }

    /// Raw userspace start address.
    pub const fn address(self) -> u64 {
        self.address
    }

    /// Requested byte length.
    pub const fn length(self) -> u32 {
        self.length
    }

    /// Exclusive raw end address.
    pub const fn end(self) -> u64 {
        self.address + self.length as u64
    }
}

/// Positional READ or WRITE arguments after strict SQE decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadWriteRequest {
    file: FileTarget,
    offset: u64,
    buffer: IoBuffer,
    buffer_slot: Option<BufferSlot>,
    provided_group: Option<u16>,
    multishot: bool,
    recv_flags: u32,
    rw_flags: u32,
}

/// Stable positional vectored-I/O arguments copied from an SQE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VectoredRequest {
    file: FileTarget,
    iov_address: u64,
    iov_count: u32,
    offset: u64,
    rw_flags: u32,
}

impl VectoredRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }
    pub const fn iov_address(self) -> u64 {
        self.iov_address
    }
    pub const fn iov_count(self) -> u32 {
        self.iov_count
    }
    pub const fn offset(self) -> u64 {
        self.offset
    }
    /// RWF_* flags copied from the SQE's operation-flags field.
    pub const fn rw_flags(self) -> u32 {
        self.rw_flags
    }
}

impl ReadWriteRequest {
    /// File reference selected by `IOSQE_FIXED_FILE`.
    pub const fn file(self) -> FileTarget {
        self.file
    }

    /// Explicit file offset. Current-position I/O is not advertised.
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Checked userspace buffer geometry.
    pub const fn buffer(self) -> IoBuffer {
        self.buffer
    }

    /// Fixed-buffer slot selected by `READ_FIXED`/`WRITE_FIXED`, if any.
    pub const fn fixed_buffer(self) -> Option<BufferSlot> {
        self.buffer_slot
    }

    /// Buffer-group selected through `IOSQE_BUFFER_SELECT`.
    pub const fn provided_buffer_group(self) -> Option<u16> {
        self.provided_group
    }

    /// Whether I/O geometry must be derived from a ring-owned lease.
    pub const fn uses_ring_buffer(self) -> bool {
        self.buffer_slot.is_some() || self.provided_group.is_some()
    }

    /// `IORING_RECV_MULTISHOT`, meaningful only for a socket receive.
    pub const fn multishot(self) -> bool {
        self.multishot
    }

    /// Validated `IORING_OP_RECV` message flags (zero for file reads).
    pub const fn recv_flags(self) -> u32 {
        self.recv_flags
    }

    /// RWF_* flags copied from the SQE's operation-flags field (zero for
    /// socket SEND/RECV, whose flags use a distinct ABI).
    pub const fn rw_flags(self) -> u32 {
        self.rw_flags
    }
}

/// One-shot POLL_ADD arguments after strict SQE decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PollRequest {
    file: FileTarget,
    events: u32,
    multishot: bool,
}

/// `IORING_OP_FSYNC` arguments copied from a single SQE.
///
/// The Linux opcode shares the `len` field with `fsync_flags`; only
/// `IORING_FSYNC_DATASYNC` is presently meaningful.  Keeping the retained
/// file target here makes close/reuse unable to redirect a submitted sync.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FsyncRequest {
    file: FileTarget,
    datasync: bool,
}

impl FsyncRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }

    pub const fn datasync(self) -> bool {
        self.datasync
    }
}

/// `IORING_OP_CLOSE` descriptor selected at SQE copy time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CloseRequest {
    fd: i32,
}

impl CloseRequest {
    pub const fn fd(self) -> i32 {
        self.fd
    }
}

/// `IORING_OP_FADVISE` arguments.  `offset` and `length` retain their Linux
/// signed `loff_t` interpretation at execution time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadviseRequest {
    file: FileTarget,
    offset: i64,
    length: i64,
    advice: u32,
}

impl FadviseRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }
    pub const fn offset(self) -> i64 {
        self.offset
    }
    pub const fn length(self) -> i64 {
        self.length
    }
    pub const fn advice(self) -> u32 {
        self.advice
    }
}

/// `IORING_OP_SYNC_FILE_RANGE` arguments copied without a later fd lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncFileRangeRequest {
    file: FileTarget,
    offset: i64,
    length: i64,
    flags: u32,
}

impl SyncFileRangeRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }
    pub const fn offset(self) -> i64 {
        self.offset
    }
    pub const fn length(self) -> i64 {
        self.length
    }
    pub const fn flags(self) -> u32 {
        self.flags
    }
}

/// `IORING_OP_FALLOCATE` mutation parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FallocateRequest {
    file: FileTarget,
    offset: i64,
    length: i64,
    mode: u32,
}

impl FallocateRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }
    pub const fn offset(self) -> i64 {
        self.offset
    }
    pub const fn length(self) -> i64 {
        self.length
    }
    pub const fn mode(self) -> u32 {
        self.mode
    }
}

/// `IORING_OP_SHUTDOWN` request selected by a retained socket description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownRequest {
    file: FileTarget,
    how: u32,
}

impl ShutdownRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }
    pub const fn how(self) -> u32 {
        self.how
    }
}

impl PollRequest {
    /// File reference selected by `IOSQE_FIXED_FILE`.
    pub const fn file(self) -> FileTarget {
        self.file
    }

    /// Native-endian Linux poll event bits copied from `poll32_events`.
    pub const fn events(self) -> u32 {
        self.events
    }

    /// Whether `IORING_POLL_ADD_MULTI` was requested.
    pub const fn multishot(self) -> bool {
        self.multishot
    }
}

/// `IORING_OP_TIMEOUT` fields copied from an SQE.  The timespec itself stays
/// in user memory until submission admission, where the kernel copies it once
/// before handing the request to the timer worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeoutRequest {
    timespec_address: u64,
    count: u32,
}

/// `IORING_OP_PROVIDE_BUFFERS` wire arguments.  The operation describes
/// `count` equally sized, consecutive userspace buffers beginning at `address`
/// whose ids begin at `first_id` in `group`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProvideBuffersRequest {
    address: u64,
    length: u32,
    count: u16,
    group: u16,
    first_id: u16,
}

impl ProvideBuffersRequest {
    pub const fn address(self) -> u64 {
        self.address
    }
    pub const fn length(self) -> u32 {
        self.length
    }
    pub const fn count(self) -> u16 {
        self.count
    }
    pub const fn group(self) -> u16 {
        self.group
    }
    pub const fn first_id(self) -> u16 {
        self.first_id
    }
}

/// `IORING_OP_REMOVE_BUFFERS` retires up to `count` ready buffers from one
/// group.  In-flight leases deliberately remain valid until their CQE path
/// releases them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoveBuffersRequest {
    count: u16,
    group: u16,
}

/// A retained-listener accept request.  Multishot is selected by the Linux
/// `IORING_ACCEPT_MULTISHOT` bit in `ioprio` and is deliberately represented
/// separately from the socket flags carried in `operation_flags`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptRequest {
    file: FileTarget,
    flags: u32,
    multishot: bool,
}

impl AcceptRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }
    pub const fn flags(self) -> u32 {
        self.flags
    }
    pub const fn multishot(self) -> bool {
        self.multishot
    }
}

impl RemoveBuffersRequest {
    pub const fn count(self) -> u16 {
        self.count
    }
    pub const fn group(self) -> u16 {
        self.group
    }
}

impl TimeoutRequest {
    /// Userspace address of `__kernel_timespec`.
    pub const fn timespec_address(self) -> u64 {
        self.timespec_address
    }
    /// Number of completions that would satisfy the timeout.  The initial
    /// implementation supports the normal zero-count deadline form.
    pub const fn count(self) -> u32 {
        self.count
    }
}

/// Operations implemented by the first pure core slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmissionOperation {
    /// `IORING_OP_NOP` with no result injection.
    Nop,
    /// `IORING_OP_READV`.
    Readv(VectoredRequest),
    /// `IORING_OP_WRITEV`.
    Writev(VectoredRequest),
    /// Positional `IORING_OP_READ`.
    Read(ReadWriteRequest),
    /// Positional `IORING_OP_WRITE`.
    Write(ReadWriteRequest),
    /// `IORING_OP_OPENAT2`; path/how are copied by the kernel before issue.
    OpenAt2(OpenAt2Request),
    /// `IORING_OP_FSYNC`, optionally data-only.
    Fsync(FsyncRequest),
    /// `IORING_OP_CLOSE` for a process descriptor. Fixed-file close has
    /// distinct table-slot semantics and is deliberately not conflated with
    /// ordinary fd-table removal.
    Close(CloseRequest),
    /// `IORING_OP_FADVISE` on a retained OFD.
    Fadvise(FadviseRequest),
    /// `IORING_OP_SYNC_FILE_RANGE` on a retained OFD.
    SyncFileRange(SyncFileRangeRequest),
    /// `IORING_OP_FALLOCATE` on a retained OFD and credential snapshot.
    Fallocate(FallocateRequest),
    /// `IORING_OP_SHUTDOWN` on a retained socket OFD.
    Shutdown(ShutdownRequest),
    /// One-shot `IORING_OP_POLL_ADD`.
    PollAdd(PollRequest),
    /// Relative `IORING_OP_TIMEOUT`.
    Timeout(TimeoutRequest),
    /// Cancels a pending timeout selected by user data.
    TimeoutRemove {
        target_user_data: u64,
    },
    /// Remove the pending one-shot poll whose user-data is `target_user_data`.
    PollRemove {
        target_user_data: u64,
    },
    /// Default single-target `IORING_OP_ASYNC_CANCEL`, matched by user data.
    AsyncCancel {
        /// `user_data` of the request to cancel.
        target_user_data: u64,
    },
    /// Supply a contiguous range of userspace buffers to a buffer group.
    ProvideBuffers(ProvideBuffersRequest),
    /// Retire ready buffers from a buffer group.
    RemoveBuffers(RemoveBuffersRequest),
    /// Accept one connection, optionally retaining the listener for shots.
    Accept(AcceptRequest),
    UringCmd(UringCmdRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenAt2Request {
    dirfd: i32,
    path_address: u64,
    how_address: u64,
    how_size: u32,
}
impl OpenAt2Request {
    pub const fn dirfd(self) -> i32 {
        self.dirfd
    }
    pub const fn path_address(self) -> u64 {
        self.path_address
    }
    pub const fn how_address(self) -> u64 {
        self.how_address
    }
    pub const fn how_size(self) -> u32 {
        self.how_size
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UringCmdRequest {
    file: FileTarget,
    command: u32,
    flags: u32,
    payload: [u8; 16],
}
impl UringCmdRequest {
    pub const fn file(self) -> FileTarget {
        self.file
    }
    pub const fn command(self) -> u32 {
        self.command
    }
    pub const fn flags(self) -> u32 {
        self.flags
    }
    pub const fn payload(self) -> [u8; 16] {
        self.payload
    }
}

/// One 64-byte SQE copied once and decoded into kernel-neutral values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParsedSubmission {
    user_data: u64,
    operation: SubmissionOperation,
    dependencies: SubmissionDependencies,
}

/// One private 64-byte SQE copy which preserves identity even when decoding
/// produces an operation-level error completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopiedSubmission {
    bytes: [u8; SQE_BYTES as usize],
}

impl CopiedSubmission {
    /// Takes ownership of the adapter's single stable SQE copy.
    pub const fn new(bytes: [u8; SQE_BYTES as usize]) -> Self {
        Self { bytes }
    }

    /// Raw operation byte used by probe/error classification.
    pub const fn opcode(&self) -> u8 {
        self.bytes[0]
    }

    /// Opaque user value available even if full operation decoding fails.
    pub fn user_data(&self) -> u64 {
        RawSubmissionEntry::new(self.bytes).decode().user_data
    }

    /// Descriptor used to reserve terminal capacity before full decode.
    pub fn descriptor(&self) -> RequestDescriptor {
        let operation = match classify_submission_opcode(self.opcode()) {
            SubmissionOpcodeSupport::Supported(operation) => operation,
            SubmissionOpcodeSupport::KnownUnsupported | SubmissionOpcodeSupport::Unknown => {
                RequestOperation::Rejected(self.opcode())
            }
        };
        RequestDescriptor::new(self.user_data(), operation)
    }

    /// Strictly decodes the private copy.
    pub fn parse(self) -> Result<ParsedSubmission, IoUringError> {
        ParsedSubmission::parse_copied(self.bytes)
    }
}

impl ParsedSubmission {
    /// Parses an already copied SQE without constructing a raw C union or enum.
    ///
    /// Integer fields use the little-endian Linux ABI of supported x86_64
    /// consumers. The parser accepts only the operation and flag subset that
    /// this crate can model.
    pub fn parse(bytes: [u8; SQE_BYTES as usize]) -> Result<Self, IoUringError> {
        CopiedSubmission::new(bytes).parse()
    }

    fn parse_copied(bytes: [u8; SQE_BYTES as usize]) -> Result<Self, IoUringError> {
        let raw = RawSubmissionEntry::new(bytes).decode();
        let opcode = raw.opcode;
        let original_sqe_flags = raw.flags;
        let dependencies = SubmissionDependencies::parse(original_sqe_flags)?;
        let sqe_flags = original_sqe_flags & !(IOSQE_IO_DRAIN | IOSQE_IO_LINK | IOSQE_IO_HARDLINK);
        let ioprio = raw.ioprio;
        let fd = raw.fd;
        let offset = raw.offset;
        let address = raw.address;
        let length = raw.len;
        let operation_flags = raw.operation_flags;
        let user_data = raw.user_data;
        let buffer_index = raw.buffer_index;
        let personality = raw.personality;
        let splice_fd_in = raw.file_index;

        let operation = match opcode {
            IORING_OP_NOP => {
                require_submission_flags(sqe_flags, 0)?;
                if ioprio != 0 || personality != 0 || operation_flags != 0 {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::Nop
            }
            IORING_OP_READV | IORING_OP_WRITEV => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                const RWF_SUPPORTED: u32 = 0x0000_0001
                    | 0x0000_0002
                    | 0x0000_0004
                    | 0x0000_0008
                    | 0x0000_0010
                    | 0x0000_0020
                    | 0x0000_0040
                    | 0x0000_0080
                    | 0x0000_0100
                    | 0x0000_0200;
                if ioprio != 0
                    || personality != 0
                    || buffer_index != 0
                    || operation_flags & !RWF_SUPPORTED != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                if offset == u64::MAX {
                    return Err(IoUringError::CurrentPositionUnsupported);
                }
                let request = VectoredRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    iov_address: address,
                    iov_count: length,
                    offset,
                    rw_flags: operation_flags,
                };
                if opcode == IORING_OP_READV {
                    SubmissionOperation::Readv(request)
                } else {
                    SubmissionOperation::Writev(request)
                }
            }
            IORING_OP_FSYNC => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                // IORING_FSYNC_DATASYNC is the complete v6.18 generic flag
                // vocabulary for this opcode.  fsync has no address, offset,
                // ioprio, buffer-index, personality, or splice input.
                const IORING_FSYNC_DATASYNC: u32 = 1;
                if ioprio != 0
                    || personality != 0
                    || buffer_index != 0
                    || offset != 0
                    || address != 0
                    || splice_fd_in != 0
                    || length & !IORING_FSYNC_DATASYNC != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::Fsync(FsyncRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    datasync: length & IORING_FSYNC_DATASYNC != 0,
                })
            }
            IORING_OP_CLOSE => {
                require_submission_flags(sqe_flags, 0)?;
                if ioprio != 0
                    || personality != 0
                    || offset != 0
                    || address != 0
                    || length != 0
                    || operation_flags != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                if fd < 0 {
                    return Err(IoUringError::InvalidFileTarget);
                }
                SubmissionOperation::Close(CloseRequest { fd })
            }
            IORING_OP_FADVISE => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                if ioprio != 0
                    || personality != 0
                    || address != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::Fadvise(FadviseRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    offset: offset as i64,
                    length: i64::from(length),
                    advice: operation_flags,
                })
            }
            IORING_OP_SYNC_FILE_RANGE => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                if ioprio != 0
                    || personality != 0
                    || address != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::SyncFileRange(SyncFileRangeRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    offset: offset as i64,
                    length: i64::from(length),
                    flags: operation_flags,
                })
            }
            IORING_OP_FALLOCATE => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                if ioprio != 0
                    || personality != 0
                    || address != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::Fallocate(FallocateRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    offset: offset as i64,
                    length: i64::from(length),
                    mode: operation_flags,
                })
            }
            IORING_OP_SHUTDOWN => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                if ioprio != 0
                    || personality != 0
                    || offset != 0
                    || address != 0
                    || operation_flags != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::Shutdown(ShutdownRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    how: length,
                })
            }
            IORING_OP_READ_FIXED
            | IORING_OP_WRITE_FIXED
            | IORING_OP_READ
            | IORING_OP_WRITE
            | IORING_OP_SEND
            | IORING_OP_RECV => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE | IOSQE_BUFFER_SELECT)?;
                let fixed_buffer = matches!(opcode, IORING_OP_READ_FIXED | IORING_OP_WRITE_FIXED);
                let buffer_select = sqe_flags & IOSQE_BUFFER_SELECT != 0;
                const IORING_RECV_MULTISHOT: u16 = 1 << 1;
                let recv_multishot =
                    opcode == IORING_OP_RECV && ioprio & IORING_RECV_MULTISHOT != 0;
                // The io_uring receive operation has no control/name output,
                // so accept only payload-receive flags that its retained
                // socket primitive can carry without a user msghdr.
                const IOURING_RECV_FLAGS: u32 = 0x2 | 0x20 | 0x40 | 0x100;
                if ioprio
                    & !(if opcode == IORING_OP_RECV {
                        IORING_RECV_MULTISHOT
                    } else {
                        0
                    })
                    != 0
                    || personality != 0
                    || (!fixed_buffer && !buffer_select && buffer_index != 0)
                    || (matches!(
                        opcode,
                        IORING_OP_READ_FIXED
                            | IORING_OP_WRITE_FIXED
                            | IORING_OP_READ
                            | IORING_OP_WRITE
                    ) && operation_flags
                        & !(0x0000_0001
                            | 0x0000_0002
                            | 0x0000_0004
                            | 0x0000_0008
                            | 0x0000_0010
                            | 0x0000_0020
                            | 0x0000_0040
                            | 0x0000_0080
                            | 0x0000_0100
                            | 0x0000_0200)
                        != 0)
                    || (opcode == IORING_OP_RECV && operation_flags & !IOURING_RECV_FLAGS != 0)
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                if fixed_buffer && buffer_select {
                    return Err(IoUringError::UnsupportedSubmissionFlags);
                }
                if buffer_select && !matches!(opcode, IORING_OP_READ | IORING_OP_RECV) {
                    return Err(IoUringError::UnsupportedSubmissionFlags);
                }
                if recv_multishot && !buffer_select {
                    return Err(IoUringError::UnsupportedSubmissionFlags);
                }
                if offset == u64::MAX {
                    return Err(IoUringError::CurrentPositionUnsupported);
                }
                if opcode == IORING_OP_RECV && offset != 0 {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                // The generic stream engine owns ordinary READ/WRITE and
                // SEND/RECV alike.  Socket message flags need their own
                // ancillary/credential contract and are therefore not
                // silently treated as file rw_flags here.
                if opcode == IORING_OP_SEND && operation_flags != 0 {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                let request = ReadWriteRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    offset,
                    buffer: IoBuffer::new(address, length)?,
                    buffer_slot: fixed_buffer.then(|| BufferSlot::new(u32::from(buffer_index))),
                    provided_group: buffer_select.then_some(personality),
                    multishot: recv_multishot,
                    recv_flags: if opcode == IORING_OP_RECV {
                        operation_flags
                    } else {
                        0
                    },
                    rw_flags: if matches!(
                        opcode,
                        IORING_OP_READ_FIXED
                            | IORING_OP_WRITE_FIXED
                            | IORING_OP_READ
                            | IORING_OP_WRITE
                    ) {
                        operation_flags
                    } else {
                        0
                    },
                };
                if matches!(
                    opcode,
                    IORING_OP_READ_FIXED | IORING_OP_READ | IORING_OP_RECV
                ) {
                    SubmissionOperation::Read(request)
                } else {
                    SubmissionOperation::Write(request)
                }
            }
            IORING_OP_OPENAT2 => {
                require_submission_flags(sqe_flags, 0)?;
                if ioprio != 0
                    || personality != 0
                    || operation_flags != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::OpenAt2(OpenAt2Request {
                    dirfd: fd,
                    path_address: address,
                    how_address: raw.offset,
                    how_size: length,
                })
            }
            IORING_OP_PROVIDE_BUFFERS => {
                require_submission_flags(sqe_flags, 0)?;
                let count = u16::try_from(fd).map_err(|_| IoUringError::InvalidSubmission)?;
                if ioprio != 0
                    || offset != 0
                    || operation_flags != 0
                    || splice_fd_in != 0
                    || address == 0
                    || length == 0
                    || count == 0
                {
                    return Err(IoUringError::InvalidSubmission);
                }
                // The id range is part of the ABI, so reject wrap before any
                // adapter state is touched.
                buffer_index
                    .checked_add(count - 1)
                    .ok_or(IoUringError::InvalidSubmission)?;
                let bytes = u64::from(length)
                    .checked_mul(u64::from(count))
                    .ok_or(IoUringError::InvalidSubmission)?;
                address
                    .checked_add(bytes)
                    .ok_or(IoUringError::InvalidSubmission)?;
                SubmissionOperation::ProvideBuffers(ProvideBuffersRequest {
                    address,
                    length,
                    count,
                    group: personality,
                    first_id: buffer_index,
                })
            }
            IORING_OP_ACCEPT => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                // Linux v6.18 keeps ACCEPT_MULTISHOT in ioprio.  Accept4
                // flags are the opcode flags and no user sockaddr may be
                // retained by a multishot owner.
                const IORING_ACCEPT_MULTISHOT: u16 = 1;
                if ioprio & !IORING_ACCEPT_MULTISHOT != 0
                    || personality != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                    || offset != 0
                    || address != 0
                    || length != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::Accept(AcceptRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    flags: operation_flags,
                    multishot: ioprio & IORING_ACCEPT_MULTISHOT != 0,
                })
            }
            IORING_OP_REMOVE_BUFFERS => {
                require_submission_flags(sqe_flags, 0)?;
                let count = u16::try_from(fd).map_err(|_| IoUringError::InvalidSubmission)?;
                if ioprio != 0
                    || offset != 0
                    || address != 0
                    || length != 0
                    || operation_flags != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                    || count == 0
                {
                    return Err(IoUringError::InvalidSubmission);
                }
                SubmissionOperation::RemoveBuffers(RemoveBuffersRequest {
                    count,
                    group: personality,
                })
            }
            IORING_OP_URING_CMD => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                if ioprio != 0
                    || personality != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                    || offset >> 32 != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                let mut payload = [0_u8; 16];
                payload.copy_from_slice(&bytes[48..64]);
                SubmissionOperation::UringCmd(UringCmdRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    // `cmd_op` overlays the low word of `off` in the Linux
                    // SQE union.  The high word is reserved.
                    command: offset as u32,
                    flags: operation_flags,
                    payload,
                })
            }
            IORING_OP_POLL_ADD => {
                require_submission_flags(sqe_flags, IOSQE_FIXED_FILE)?;
                if ioprio != 0
                    || personality != 0
                    || buffer_index != 0
                    || offset != 0
                    || address != 0
                    || length & !1 != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::PollAdd(PollRequest {
                    file: FileTarget::from_sqe(fd, sqe_flags & IOSQE_FIXED_FILE != 0)?,
                    events: operation_flags,
                    multishot: length & 1 != 0,
                })
            }
            IORING_OP_TIMEOUT => {
                require_submission_flags(sqe_flags, 0)?;
                // Timeout flags (absolute, boottime, realtime, link-timeout
                // and ETIME-success) select distinct clock/link semantics.
                // Do not accept them until those semantics are installed;
                // the unflagged relative form is fully self-contained.
                if ioprio != 0
                    || personality != 0
                    || fd != -1
                    || offset != 0
                    || operation_flags != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                if address == 0 || length != 0 {
                    return Err(IoUringError::InvalidSubmission);
                }
                SubmissionOperation::Timeout(TimeoutRequest {
                    timespec_address: address,
                    count: length,
                })
            }
            IORING_OP_TIMEOUT_REMOVE => {
                require_submission_flags(sqe_flags, 0)?;
                if ioprio != 0
                    || personality != 0
                    || fd != -1
                    || offset != 0
                    || length != 0
                    || operation_flags != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::TimeoutRemove {
                    target_user_data: address,
                }
            }
            IORING_OP_POLL_REMOVE => {
                require_submission_flags(sqe_flags, 0)?;
                if ioprio != 0
                    || personality != 0
                    || fd != -1
                    || offset != 0
                    || length != 0
                    || operation_flags != 0
                    || buffer_index != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::PollRemove {
                    target_user_data: address,
                }
            }
            IORING_OP_ASYNC_CANCEL => {
                require_submission_flags(sqe_flags, 0)?;
                if ioprio != 0
                    || personality != 0
                    || offset != 0
                    || operation_flags != 0
                    || splice_fd_in != 0
                {
                    return Err(IoUringError::UnsupportedOperationFlags);
                }
                SubmissionOperation::AsyncCancel {
                    target_user_data: address,
                }
            }
            opcode if opcode < PINNED_IORING_OP_LAST => {
                return Err(IoUringError::UnsupportedOpcode);
            }
            _ => return Err(IoUringError::UnknownOpcode),
        };

        Ok(Self {
            user_data,
            operation,
            dependencies,
        })
    }

    /// Opaque userspace value copied into the terminal CQE.
    pub const fn user_data(self) -> u64 {
        self.user_data
    }

    /// Strictly decoded operation arguments.
    pub const fn operation(self) -> SubmissionOperation {
        self.operation
    }
    pub const fn dependencies(self) -> SubmissionDependencies {
        self.dependencies
    }

    /// Produces the request-table descriptor used for cancellation matching.
    pub const fn descriptor(self) -> RequestDescriptor {
        RequestDescriptor::new(
            self.user_data,
            match self.operation {
                SubmissionOperation::Nop => RequestOperation::Nop,
                SubmissionOperation::Readv(_) => RequestOperation::Read,
                SubmissionOperation::Writev(_) => RequestOperation::Write,
                SubmissionOperation::Read(_) => RequestOperation::Read,
                SubmissionOperation::Write(_) => RequestOperation::Write,
                SubmissionOperation::OpenAt2(_) => RequestOperation::OpenAt2,
                SubmissionOperation::Fsync(_) => RequestOperation::Fsync,
                SubmissionOperation::Close(_) => RequestOperation::Close,
                SubmissionOperation::Fadvise(_) => RequestOperation::Fadvise,
                SubmissionOperation::SyncFileRange(_) => RequestOperation::SyncFileRange,
                SubmissionOperation::Fallocate(_) => RequestOperation::Fallocate,
                SubmissionOperation::Shutdown(_) => RequestOperation::Shutdown,
                SubmissionOperation::PollAdd(_) => RequestOperation::PollAdd,
                SubmissionOperation::Timeout(_) => RequestOperation::Timeout,
                SubmissionOperation::TimeoutRemove { .. } => RequestOperation::TimeoutRemove,
                SubmissionOperation::PollRemove { .. } => RequestOperation::PollRemove,
                SubmissionOperation::AsyncCancel { .. } => RequestOperation::AsyncCancel,
                SubmissionOperation::ProvideBuffers(_) => RequestOperation::ProvideBuffers,
                SubmissionOperation::RemoveBuffers(_) => RequestOperation::RemoveBuffers,
                SubmissionOperation::Accept(_) => RequestOperation::Accept,
                SubmissionOperation::UringCmd(_) => RequestOperation::UringCmd,
            },
        )
    }
}

fn require_submission_flags(bits: u8, allowed: u8) -> Result<(), IoUringError> {
    if bits & !allowed == 0 {
        Ok(())
    } else {
        Err(IoUringError::UnsupportedSubmissionFlags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqe(opcode: u8, user_data: u64) -> [u8; SQE_BYTES as usize] {
        let mut bytes = [0; SQE_BYTES as usize];
        bytes[0] = opcode;
        bytes[32..40].copy_from_slice(&user_data.to_le_bytes());
        bytes
    }

    #[test]
    fn copied_submission_preserves_identity_before_decode() {
        let copied = CopiedSubmission::new(sqe(1, 0x1122_3344_5566_7788));
        assert_eq!(copied.opcode(), 1);
        assert_eq!(copied.user_data(), 0x1122_3344_5566_7788);
        assert_eq!(copied.descriptor().operation(), RequestOperation::Read);
        assert!(matches!(
            copied.parse().unwrap().operation(),
            SubmissionOperation::Readv(_)
        ));
    }

    #[test]
    fn nop_and_positioned_write_decode_without_hidden_state() {
        let nop = ParsedSubmission::parse(sqe(IORING_OP_NOP, 5)).unwrap();
        assert_eq!(nop.operation(), SubmissionOperation::Nop);

        let mut bytes = sqe(IORING_OP_WRITE, 6);
        bytes[4..8].copy_from_slice(&4_i32.to_le_bytes());
        bytes[8..16].copy_from_slice(&9_u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0x2000_u64.to_le_bytes());
        bytes[24..28].copy_from_slice(&32_u32.to_le_bytes());
        let write = ParsedSubmission::parse(bytes).unwrap();
        let SubmissionOperation::Write(write) = write.operation() else {
            panic!("expected write request");
        };
        assert_eq!(write.file(), FileTarget::Descriptor(4));
        assert_eq!(write.offset(), 9);
        assert_eq!(write.buffer(), IoBuffer::new(0x2000, 32).unwrap());
    }

    #[test]
    fn pointer_overflow_and_unimplemented_flags_are_distinct() {
        let mut bytes = sqe(IORING_OP_READ, 1);
        bytes[4..8].copy_from_slice(&4_i32.to_le_bytes());
        bytes[16..24].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
        bytes[24..28].copy_from_slice(&4_u32.to_le_bytes());
        assert_eq!(
            ParsedSubmission::parse(bytes),
            Err(IoUringError::InvalidSubmission)
        );

        bytes[16..24].copy_from_slice(&0x1000_u64.to_le_bytes());
        bytes[1] = 1 << 4;
        assert_eq!(
            ParsedSubmission::parse(bytes),
            Err(IoUringError::UnsupportedSubmissionFlags)
        );
        bytes[1] = 0;
        // RWF_HIPRI is a supported wire flag and must reach the adapter.
        bytes[28..32].copy_from_slice(&1_u32.to_le_bytes());
        let parsed = ParsedSubmission::parse(bytes).unwrap();
        let SubmissionOperation::Read(request) = parsed.operation() else {
            panic!("expected read request");
        };
        assert_eq!(request.rw_flags(), 1);

        // An unknown operation bit must not be masked away during decoding.
        bytes[28..32].copy_from_slice(&(1_u32 << 31).to_le_bytes());
        assert_eq!(
            ParsedSubmission::parse(bytes),
            Err(IoUringError::UnsupportedOperationFlags)
        );
    }

    #[test]
    fn pinned_known_and_unknown_opcodes_are_distinct() {
        assert_eq!(
            ParsedSubmission::parse(sqe(PINNED_IORING_OP_LAST - 1, 1)),
            Err(IoUringError::UnsupportedOpcode)
        );
        assert_eq!(
            ParsedSubmission::parse(sqe(PINNED_IORING_OP_LAST, 1)),
            Err(IoUringError::UnknownOpcode)
        );
        assert_eq!(
            classify_submission_opcode(PINNED_IORING_OP_LAST),
            SubmissionOpcodeSupport::Unknown
        );
    }

    #[test]
    fn poll_add_reads_all_32_event_bits() {
        let mut bytes = sqe(IORING_OP_POLL_ADD, 9);
        bytes[4..8].copy_from_slice(&3_i32.to_le_bytes());
        bytes[28..32].copy_from_slice(&0x8000_0001_u32.to_le_bytes());
        let parsed = ParsedSubmission::parse(bytes).unwrap();
        let SubmissionOperation::PollAdd(poll) = parsed.operation() else {
            panic!("expected poll request");
        };
        assert_eq!(poll.file(), FileTarget::Descriptor(3));
        assert_eq!(poll.events(), 0x8000_0001);
    }

    #[test]
    fn positioned_io_rejects_current_position_and_accepts_fixed_file() {
        let mut bytes = sqe(IORING_OP_READ, 9);
        bytes[1] = IOSQE_FIXED_FILE;
        bytes[4..8].copy_from_slice(&2_i32.to_le_bytes());
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            ParsedSubmission::parse(bytes),
            Err(IoUringError::CurrentPositionUnsupported)
        );
        bytes[8..16].copy_from_slice(&7_u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0x1000_u64.to_le_bytes());
        bytes[24..28].copy_from_slice(&16_u32.to_le_bytes());
        let parsed = ParsedSubmission::parse(bytes).unwrap();
        let SubmissionOperation::Read(read) = parsed.operation() else {
            panic!("expected read request");
        };
        assert_eq!(read.file(), FileTarget::Registered(FileSlot::new(2)));
        assert_eq!(read.offset(), 7);
        assert_eq!(read.buffer().end(), 0x1010);
    }

    #[test]
    fn fixed_buffer_io_decodes_slot_and_exact_range() {
        let mut bytes = sqe(IORING_OP_WRITE_FIXED, 10);
        bytes[1] = IOSQE_FIXED_FILE;
        bytes[4..8].copy_from_slice(&3_i32.to_le_bytes());
        bytes[8..16].copy_from_slice(&11_u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0x4010_u64.to_le_bytes());
        bytes[24..28].copy_from_slice(&24_u32.to_le_bytes());
        bytes[40..42].copy_from_slice(&7_u16.to_le_bytes());
        let parsed = ParsedSubmission::parse(bytes).unwrap();
        let SubmissionOperation::Write(write) = parsed.operation() else {
            panic!("expected fixed-buffer write request");
        };
        assert_eq!(write.file(), FileTarget::Registered(FileSlot::new(3)));
        assert_eq!(write.fixed_buffer(), Some(BufferSlot::new(7)));
        assert_eq!(write.buffer(), IoBuffer::new(0x4010, 24).unwrap());
        assert_eq!(
            classify_submission_opcode(IORING_OP_READ_FIXED),
            SubmissionOpcodeSupport::Supported(RequestOperation::Read)
        );
    }

    #[test]
    fn async_cancel_decodes_default_user_data_selector() {
        let mut bytes = sqe(IORING_OP_ASYNC_CANCEL, 11);
        bytes[4..8].copy_from_slice(&(-1_i32).to_le_bytes());
        bytes[16..24].copy_from_slice(&77_u64.to_le_bytes());
        let parsed = ParsedSubmission::parse(bytes).unwrap();
        assert_eq!(
            parsed.operation(),
            SubmissionOperation::AsyncCancel {
                target_user_data: 77
            }
        );
    }
}
