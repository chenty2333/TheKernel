use alloc::{sync::Arc, vec::Vec};
use core::{
    ffi::c_int,
    mem::{MaybeUninit, size_of},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use axtask::future;
use memory_addr::PAGE_SIZE_4K;
use thekernel_linux_io_uring::{
    BufferSlot, EnterFlags, EnterRequest, FeatureFlags, FileTarget, IoUringError, LegacySignalMask,
    PINNED_IORING_OP_LAST, ParsedSubmission, PreparedRequest, ReadWriteRequest,
    RegistrationOperation, RegistrationRequest, SetupRequest, SubmissionOperation, TerminalCause,
};
use thekernel_linux_signal::SignalSet;

use super::{
    IoUringWorkerResult, capture_io_operation_context, io_uring_pread64_submission,
    io_uring_pread64_submission_nonblocking_stream, io_uring_pread64_worker,
    io_uring_pwrite64_submission, io_uring_pwrite64_worker, physical_effect_admission_enabled,
    prepare_physical_io_effect, prepare_physical_io_plan, prepare_physical_io_write_memfd_guard,
    prepare_physical_io_write_privilege_guard,
};
use crate::{
    file::{
        FileDescription, FileHandle, FileLike, FileLikeKind, get_file_description, get_file_like,
        io_uring::{
            IoUring, IoUringBufferLease, IoUringFileLease, PendingStreamAdmissionError,
            PreparedPhysicalIoAdmission, PreparedPhysicalIoOperation, SubmissionStep,
            SubmissionWork,
        },
        prepare_file_description_with_resource, reserve_fd,
    },
    mm::{IoVec, UserMemoryCapability, check_user_writable_with, map_usercopy_error},
    syscall::{io_mpx::wait_io_result, signal::check_sigset_size},
};

mod uapi;

use uapi::{IoUringParams, IoUringProbeHeader, IoUringProbeOp, write_probe};

const THEKERNEL_IO_URING_FEATURES: FeatureFlags = FeatureFlags::SINGLE_MMAP
    .union(FeatureFlags::NODROP)
    .union(FeatureFlags::SUBMIT_STABLE)
    .union(FeatureFlags::POLL_32BITS);

// Linux's io_validate_user_buf_range rejects registrations above SZ_1T before
// checking address arithmetic. Preserve that ordering so SIZE_MAX-sized
// descriptors stay EINVAL, while under-limit address/page-cover overflow is
// reported as EOVERFLOW.
const REGISTERED_BUFFER_MAX_LEN: usize = 1 << 40;

fn validate_registered_buffer_range(address: usize, length: usize) -> AxResult<()> {
    if length == 0 {
        // Linux's io_validate_user_buf_range reports an empty non-NULL
        // registration as EFAULT before it attempts to pin the range.
        return Err(AxError::BadAddress);
    }
    if length > REGISTERED_BUFFER_MAX_LEN {
        return Err(AxError::InvalidInput);
    }
    let end = address
        .checked_add(length)
        .ok_or_else(|| AxError::from(LinuxError::EOVERFLOW))?;
    end.checked_add(PAGE_SIZE_4K - 1)
        .ok_or_else(|| AxError::from(LinuxError::EOVERFLOW))?;
    Ok(())
}

fn map_policy_error(error: IoUringError) -> AxError {
    use IoUringError::*;

    match error {
        AllocationFailed => AxError::NoMemory,
        CompletionQueueFull
        | RequestCapacityExceeded
        | FileLeaseCapacityExceeded
        | BufferLeaseCapacityExceeded
        | Busy => AxError::ResourceBusy,
        Closing | Draining | Closed => AxError::BadFileDescriptor,
        InvalidFileSlot | FileSlotEmpty | UnknownFileLease | FileTableNotPublished => {
            AxError::BadFileDescriptor
        }
        InvalidBufferSlot | BufferSlotEmpty | UnknownBufferLease | BufferTableNotPublished => {
            AxError::BadFileDescriptor
        }
        CancellationTargetNotFound => AxError::NotFound,
        UnsupportedOpcode
        | UnsupportedSubmissionFlags
        | UnsupportedOperationFlags
        | CurrentPositionUnsupported
        | UnsupportedRegistration => AxError::OperationNotSupported,
        Overflow | GenerationExhausted => AxError::OutOfRange,
        _ => AxError::InvalidInput,
    }
}

fn negative_errno(error: AxError) -> i32 {
    -LinuxError::from(error).code()
}

fn negative_policy_errno(error: IoUringError) -> i32 {
    negative_errno(map_policy_error(error))
}

fn ring_from_fd(fd: c_int) -> AxResult<FileHandle<IoUring>> {
    get_file_like(fd)?
        .downcast::<IoUring>()
        .map_err(|_| AxError::OperationNotSupported)
}

pub fn sys_io_uring_setup(
    capability: UserMemoryCapability,
    entries: u32,
    params: *mut IoUringParams,
) -> AxResult<isize> {
    let input = capability
        .read_value(params as *const IoUringParams)
        .map_err(map_usercopy_error)?;
    let layout = SetupRequest::from_raw(
        entries,
        input.cq_entries,
        input.flags,
        input.sq_thread_cpu,
        input.sq_thread_idle,
        input.wq_fd,
        input.resv,
    )
    .and_then(|request| request.resolve(THEKERNEL_IO_URING_FEATURES))
    .map_err(map_policy_error)?;

    let reservation = reserve_fd(true)?;
    let ring = IoUring::try_new(layout)?;
    let finalizer = ring.try_finalizer_resource()?;
    let ring_file: Arc<dyn FileLike> = ring.clone();
    let description = prepare_file_description_with_resource(ring_file, 0, None, Some(finalizer))?;
    let publication = reservation.prepare_publication(description)?;

    // The descriptor is still absent from lookup while userspace receives the
    // exact geometry. A copyout fault therefore rolls every prepared owner
    // back without exposing a partially initialized ring.
    capability
        .write_value(params, IoUringParams::from_layout(ring.layout()))
        .map_err(map_usercopy_error)?;
    Ok(publication.commit() as isize)
}

fn copy_registered_files(
    capability: &UserMemoryCapability,
    argument: u64,
    count: u32,
) -> AxResult<Vec<Option<Arc<FileDescription>>>> {
    let count = usize::try_from(count).map_err(|_| AxError::InvalidInput)?;
    if count > crate::task::AX_FILE_LIMIT {
        return Err(AxError::from(LinuxError::EMFILE));
    }
    let address = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
    let mut descriptors = Vec::<MaybeUninit<i32>>::new();
    descriptors
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    descriptors.resize_with(count, MaybeUninit::uninit);
    capability
        .read_slice(address as *const i32, &mut descriptors)
        .map_err(map_usercopy_error)?;

    let mut files = Vec::new();
    files
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    for descriptor in descriptors {
        // SAFETY: `read_slice` initialized every descriptor before returning.
        let fd = unsafe { descriptor.assume_init() };
        match fd {
            -1 => files.push(None),
            fd if fd >= 0 => files.push(Some(get_file_description(fd)?)),
            _ => return Err(AxError::BadFileDescriptor),
        }
    }
    Ok(files)
}

fn copy_registered_buffers(
    capability: &UserMemoryCapability,
    argument: u64,
    count: u32,
) -> AxResult<Vec<(usize, usize)>> {
    // Linux attempts the userspace iovec access for a NULL argument before it
    // applies the registration count validation. Keep this early return
    // bounded: it performs no allocation and never walks an arbitrary count.
    let count = registered_buffer_count(argument, count)?;
    let address = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
    let mut buffers = Vec::new();
    buffers
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..count {
        let offset = index
            .checked_mul(size_of::<IoVec>())
            .ok_or(AxError::BadAddress)?;
        let descriptor_address = address.checked_add(offset).ok_or(AxError::BadAddress)?;
        // Copy and validate one descriptor at a time. Linux does not first
        // import the complete array and then inspect all lengths; doing so
        // would let a later malformed entry mask an earlier bad userspace
        // range (and would touch more user memory than necessary).
        let descriptor = capability
            .read_value(descriptor_address as *const IoVec)
            .map_err(map_usercopy_error)?;
        if descriptor.iov_len < 0 {
            return Err(AxError::InvalidInput);
        }
        let length = usize::try_from(descriptor.iov_len).map_err(|_| AxError::InvalidInput)?;
        let address = descriptor.iov_base as usize;
        validate_registered_buffer_range(address, length)?;
        // Linux pins/validates each entry before fetching the next one. Keep
        // that order so an inaccessible earlier range cannot be masked by a
        // malformed later descriptor.
        check_user_writable_with(capability, address, length)?;
        buffers.push((address, length));
    }
    Ok(buffers)
}

fn registered_buffer_count(argument: u64, count: u32) -> AxResult<usize> {
    if argument == 0 {
        return Err(AxError::BadAddress);
    }
    let count = usize::try_from(count).map_err(|_| AxError::InvalidInput)?;
    if count == 0 || count > thekernel_linux_io_uring::IORING_MAX_REGISTERED_BUFFERS as usize {
        return Err(AxError::InvalidInput);
    }
    Ok(count)
}

fn register_probe(
    capability: &UserMemoryCapability,
    argument: u64,
    operations: u32,
) -> AxResult<()> {
    let operations = operations.min(PINNED_IORING_OP_LAST as u32);
    let records = usize::try_from(operations).map_err(|_| AxError::InvalidInput)?;
    let bytes = size_of::<IoUringProbeHeader>()
        .checked_add(
            records
                .checked_mul(size_of::<IoUringProbeOp>())
                .ok_or(AxError::InvalidInput)?,
        )
        .ok_or(AxError::InvalidInput)?;
    let address = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(bytes)
        .map_err(|_| AxError::NoMemory)?;
    output.resize(bytes, 0);
    write_probe(&mut output, operations);
    capability
        .write_bytes(address, &output)
        .map_err(map_usercopy_error)
}

pub fn sys_io_uring_register(
    capability: UserMemoryCapability,
    fd: i32,
    opcode: u32,
    arg: usize,
    nr_args: u32,
) -> AxResult<isize> {
    let operation = RegistrationRequest::new(opcode, arg as u64, nr_args)
        .decode()
        .map_err(map_policy_error)?;
    let ring = ring_from_fd(fd)?;
    match operation {
        RegistrationOperation::RegisterBuffers { argument, count } => {
            let buffers = copy_registered_buffers(&capability, argument, count)?;
            ring.register_buffers(&capability, buffers)?;
        }
        RegistrationOperation::UnregisterBuffers => ring.unregister_buffers()?,
        RegistrationOperation::RegisterFiles { argument, count } => {
            let files = copy_registered_files(&capability, argument, count)?;
            ring.register_files(files)?;
        }
        RegistrationOperation::UnregisterFiles => ring.unregister_files()?,
        RegistrationOperation::Probe {
            argument,
            operations,
        } => register_probe(&capability, argument, operations)?,
    }
    Ok(0)
}

fn submission_file(parsed: ParsedSubmission) -> Option<FileTarget> {
    match parsed.operation() {
        SubmissionOperation::Read(request) | SubmissionOperation::Write(request) => {
            Some(request.file())
        }
        SubmissionOperation::PollAdd(request) => Some(request.file()),
        SubmissionOperation::Nop | SubmissionOperation::AsyncCancel { .. } => None,
    }
}

fn submission_buffer(parsed: ParsedSubmission) -> Option<(BufferSlot, u64, u32)> {
    match parsed.operation() {
        SubmissionOperation::Read(request) | SubmissionOperation::Write(request) => request
            .fixed_buffer()
            .map(|slot| (slot, request.buffer().address(), request.buffer().length())),
        SubmissionOperation::Nop
        | SubmissionOperation::PollAdd(_)
        | SubmissionOperation::AsyncCancel { .. } => None,
    }
}

fn retain_submission_file(ring: &IoUring, target: FileTarget) -> AxResult<IoUringFileLease> {
    match target {
        FileTarget::Descriptor(fd) => {
            let fd = i32::try_from(fd).map_err(|_| AxError::BadFileDescriptor)?;
            ring.retain_descriptor(get_file_description(fd)?)
        }
        FileTarget::Registered(slot) => ring.acquire_registered_file(slot),
    }
}

fn retain_submission_buffer(
    ring: &IoUring,
    fixed: (BufferSlot, u64, u32),
) -> AxResult<IoUringBufferLease> {
    let (slot, address, length) = fixed;
    ring.acquire_registered_buffer(slot, address, length)
}

fn submission_io_capability<T: Clone>(
    caller: &T,
    fixed: bool,
    registered: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    if fixed {
        registered()
    } else {
        Ok(caller.clone())
    }
}

fn submission_io_range(
    request: ReadWriteRequest,
    fixed_buffer: Option<&IoUringBufferLease>,
) -> AxResult<(usize, usize)> {
    let (address, length) = if request.fixed_buffer().is_some() {
        fixed_buffer.ok_or(AxError::BadAddress)?.range()?
    } else {
        let buffer = request.buffer();
        (buffer.address(), buffer.length())
    };
    Ok((
        usize::try_from(address).map_err(|_| AxError::BadAddress)?,
        usize::try_from(length).map_err(|_| AxError::BadAddress)?,
    ))
}

fn pending_stream_read_supported(file: &IoUringFileLease, request: ReadWriteRequest) -> bool {
    request.fixed_buffer().is_some()
        && request.offset() == 0
        && file.description().is_ok_and(|description| {
            matches!(
                FileLikeKind::from_file_like(description.file_handle().as_ref()),
                FileLikeKind::Fifo
            )
        })
}

fn issue_prepared(
    ring: &IoUring,
    prepared: PreparedRequest,
) -> AxResult<Option<thekernel_linux_io_uring::IssuedRequest>> {
    let id = prepared.id();
    match ring.issue_request(prepared) {
        Ok(issued) => Ok(Some(issued)),
        Err(error) => {
            let kind = error.error();
            drop(error.into_prepared());
            if kind == IoUringError::TerminalAlreadyClaimed {
                return Ok(None);
            }
            ring.complete_request(
                id,
                TerminalCause::PreparationFailed,
                negative_policy_errno(kind),
                0,
            )?;
            Ok(None)
        }
    }
}

fn complete_preparation_failure(
    ring: &IoUring,
    prepared: PreparedRequest,
    result: i32,
) -> AxResult<()> {
    let id = prepared.id();
    // No execution mechanism owns a submission-preparation failure. Keep the
    // request in Prepared so its typed terminal transition can publish the
    // error CQE without crossing the uncancellable issue boundary.
    drop(prepared);
    ring.complete_request(id, TerminalCause::PreparationFailed, result, 0)
}

fn io_result(result: AxResult<isize>) -> i32 {
    match result {
        Ok(result) => i32::try_from(result).unwrap_or(-LinuxError::EOVERFLOW.code()),
        Err(error) => negative_errno(error),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubmissionOutcome {
    Accepted,
    FailedDuringSubmission,
}

/// Result of the submitter-side physical publication hand-off.  The
/// `NotSubmitted` arm retains both proofs so the caller can execute the
/// existing synchronous fallback while the issued request remains its sole
/// terminal owner.  Published/Terminal effects have already transferred all
/// ownership to the bounded worker queue and cannot fall back.
#[allow(clippy::large_enum_variant)]
enum PhysicalPublishDecision {
    NotSubmitted {
        issued: thekernel_linux_io_uring::IssuedRequest,
        admission: PreparedPhysicalIoAdmission,
    },
    /// The fixed logical owner is queued before lower publication and will
    /// be retried when the device returns completion credit.
    Pending,
    Queued,
    /// Publication returned an error after ownership could no longer prove
    /// that no descriptor was visible. The reservation commit has transferred
    /// the issued request and every physical lease to the fixed worker slot;
    /// reset/quarantine custody, not a CQE or synchronous fallback, owns the
    /// next transition.
    Quarantined,
    /// The effect was not published because the prepared admission was
    /// malformed/internal-invalid.  The issued proof has already been
    /// consumed into a typed CQE; this is not a fallback opportunity.
    Completed,
}

fn publish_physical_admission(
    ring: &IoUring,
    issued: thekernel_linux_io_uring::IssuedRequest,
    mut admission: PreparedPhysicalIoAdmission,
) -> AxResult<PhysicalPublishDecision> {
    let device_identity = admission.plan().device_identity();
    let mut reservation = match ring.reserve_physical_worker_slot_for_device(device_identity) {
        Ok(reservation) => reservation,
        Err(AxError::ResourceBusy) => {
            return Ok(PhysicalPublishDecision::NotSubmitted { issued, admission });
        }
        Err(error) => {
            // No effect has been published, so an internal ring metadata
            // failure may still be represented by the issued request's
            // ordinary terminal CQE. It is not a fallback opportunity after
            // publication, but neither may an accepted SQE be silently
            // dropped when reservation setup itself fails.
            drop(admission);
            ring.complete_issued(issued, TerminalCause::Completed, negative_errno(error), 0)?;
            return Ok(PhysicalPublishDecision::Completed);
        }
    };
    let extent_count = match admission.physical_extent_count() {
        Ok(count) => count,
        Err(error) => {
            drop(admission);
            ring.complete_issued(issued, TerminalCause::Completed, negative_errno(error), 0)?;
            return Ok(PhysicalPublishDecision::Completed);
        }
    };
    if let Err(error) = reservation.bind_admission(&mut admission) {
        drop(admission);
        ring.complete_issued(issued, TerminalCause::Completed, negative_errno(error), 0)?;
        return Ok(PhysicalPublishDecision::Completed);
    }
    match reservation.reserve_completion_routes(extent_count) {
        Ok(()) => {}
        Err(AxError::ResourceBusy) => {
            return match reservation.commit_pending(issued, admission) {
                Ok(()) => Ok(PhysicalPublishDecision::Pending),
                Err((AxError::ResourceBusy, issued, admission)) => {
                    Ok(PhysicalPublishDecision::NotSubmitted { issued, admission })
                }
                Err((error, issued, admission)) => {
                    drop(admission);
                    ring.complete_issued(
                        issued,
                        TerminalCause::Completed,
                        negative_errno(error),
                        0,
                    )?;
                    Ok(PhysicalPublishDecision::Completed)
                }
            };
        }
        Err(error) => {
            drop(admission);
            ring.complete_issued(issued, TerminalCause::Completed, negative_errno(error), 0)?;
            return Ok(PhysicalPublishDecision::Completed);
        }
    }
    let outcome = match reservation.with_physical_publish(|| unsafe { admission.publish() }) {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            // A vendor error is not proof of non-publication. Keep the
            // issued token and all leases in the bounded work owner; the
            // completion/reset worker must quarantine it instead of taking
            // the synchronous fallback.
            reservation.commit(issued, admission)?;
            error!("io_uring physical publication entered quarantine: {error:?}");
            return Ok(PhysicalPublishDecision::Quarantined);
        }
        Err(_) => {
            // The worker/device generation closed before the publication
            // window.  The effect is still unpublished, so release the
            // reservation and use the explicitly proven synchronous path.
            drop(reservation);
            return Ok(PhysicalPublishDecision::NotSubmitted { issued, admission });
        }
    };
    match outcome {
        axfs::PhysicalIoPublishOutcome::NotSubmitted(
            axfs::PhysicalIoNotSubmittedReason::Backpressure,
        ) => match reservation.commit_pending(issued, admission) {
            Ok(()) => Ok(PhysicalPublishDecision::Pending),
            Err((AxError::ResourceBusy, issued, admission)) => {
                Ok(PhysicalPublishDecision::NotSubmitted { issued, admission })
            }
            Err((error, issued, admission)) => {
                drop(admission);
                ring.complete_issued(issued, TerminalCause::Completed, negative_errno(error), 0)?;
                Ok(PhysicalPublishDecision::Completed)
            }
        },
        axfs::PhysicalIoPublishOutcome::NotSubmitted(_) => {
            drop(reservation);
            Ok(PhysicalPublishDecision::NotSubmitted { issued, admission })
        }
        axfs::PhysicalIoPublishOutcome::Published(_)
        | axfs::PhysicalIoPublishOutcome::Terminal(_) => {
            // Reservation commit only moves already-owned values into the
            // preallocated slot.  After this point an enqueue failure cannot
            // select the generic fallback.
            reservation.commit(issued, admission)?;
            Ok(PhysicalPublishDecision::Queued)
        }
    }
}

impl SubmissionOutcome {
    const fn stops_default_batch(self) -> bool {
        matches!(self, Self::FailedDuringSubmission)
    }
}

fn execute_submission(ring: &IoUring, work: SubmissionWork) -> AxResult<SubmissionOutcome> {
    let (prepared, parsed, file, buffer, mut context, physical, admission_error, capability) =
        work.into_parts();
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            complete_preparation_failure(ring, prepared, negative_policy_errno(error))?;
            return Ok(SubmissionOutcome::FailedDuringSubmission);
        }
    };
    let id = prepared.id();
    let Some(issued) = issue_prepared(ring, prepared)? else {
        return Ok(SubmissionOutcome::Accepted);
    };

    if let Some(error) = admission_error {
        drop((issued, file, buffer, context, physical));
        ring.complete_request(id, TerminalCause::Completed, negative_errno(error), 0)?;
        return Ok(SubmissionOutcome::Accepted);
    }

    match parsed.operation() {
        SubmissionOperation::Nop => {
            drop(issued);
            drop((file, buffer));
            ring.complete_request(id, TerminalCause::Completed, 0, 0)
        }
        SubmissionOperation::Read(request) => {
            if let Some(admission) = physical {
                return match publish_physical_admission(ring, issued, admission)? {
                    PhysicalPublishDecision::NotSubmitted { issued, admission } => {
                        let result = match io_uring_pread64_worker(admission)? {
                            IoUringWorkerResult::Completed(result) => result,
                            IoUringWorkerResult::Failed(error) => negative_errno(error) as isize,
                        };
                        ring.complete_issued(issued, TerminalCause::Completed, result as i32, 0)
                            .map(|_| SubmissionOutcome::Accepted)
                    }
                    PhysicalPublishDecision::Pending
                    | PhysicalPublishDecision::Queued
                    | PhysicalPublishDecision::Quarantined
                    | PhysicalPublishDecision::Completed => Ok(SubmissionOutcome::Accepted),
                };
            }
            let Some(file) = file else {
                drop(buffer);
                ring.complete_issued(
                    issued,
                    TerminalCause::PreparationFailed,
                    -LinuxError::EBADF.code(),
                    0,
                )?;
                return Ok(SubmissionOutcome::Accepted);
            };
            if request.fixed_buffer().is_some() && buffer.is_none() {
                drop(file);
                ring.complete_issued(
                    issued,
                    TerminalCause::PreparationFailed,
                    -LinuxError::EFAULT.code(),
                    0,
                )?;
                return Ok(SubmissionOutcome::Accepted);
            }
            let io_capability = match submission_io_capability(
                &capability,
                request.fixed_buffer().is_some(),
                || buffer.as_ref().ok_or(AxError::BadAddress)?.capability(),
            ) {
                Ok(capability) => capability,
                Err(_) => {
                    drop((file, buffer));
                    ring.complete_issued(
                        issued,
                        TerminalCause::PreparationFailed,
                        -LinuxError::EFAULT.code(),
                        0,
                    )?;
                    return Ok(SubmissionOutcome::Accepted);
                }
            };
            let pending_stream = pending_stream_read_supported(&file, request);
            let (io_address, io_length) = match submission_io_range(request, buffer.as_ref()) {
                Ok(range) => range,
                Err(_) => {
                    drop((file, buffer));
                    ring.complete_issued(
                        issued,
                        TerminalCause::PreparationFailed,
                        -LinuxError::EFAULT.code(),
                        0,
                    )?;
                    return Ok(SubmissionOutcome::Accepted);
                }
            };
            let fixed_segments = if request.fixed_buffer().is_some() {
                match buffer
                    .as_ref()
                    .ok_or(AxError::BadAddress)
                    .and_then(|lease| {
                        let (segments, offset, length, disjoint) = lease.physical_range()?;
                        let provenance = lease.physical_provenance()?;
                        Ok((segments, offset, length, disjoint, provenance))
                    }) {
                    Ok(range) => Some(range),
                    Err(_) => {
                        drop((file, buffer));
                        ring.complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::EFAULT.code(),
                            0,
                        )?;
                        return Ok(SubmissionOutcome::Accepted);
                    }
                }
            } else {
                None
            };
            let attempt = file.description().and_then(|description| {
                let context = context.as_ref().ok_or(AxError::BadState)?;
                let submit = if pending_stream {
                    io_uring_pread64_submission_nonblocking_stream
                } else {
                    io_uring_pread64_submission
                };
                submit(
                    &io_capability,
                    description,
                    context,
                    io_address as *mut u8,
                    io_length,
                    request.offset(),
                    fixed_segments,
                )
            });
            if pending_stream && attempt == Err(AxError::WouldBlock) {
                let buffer = buffer.ok_or(AxError::BadAddress)?;
                let context = context.take().ok_or(AxError::BadState)?;
                return match ring
                    .admit_pending_stream(issued, file, buffer, request, context, capability)
                {
                    Ok(()) => Ok(SubmissionOutcome::Accepted),
                    Err(PendingStreamAdmissionError {
                        error,
                        issued,
                        file,
                        buffer,
                        context: _,
                        capability: _,
                    }) => {
                        drop((file, buffer));
                        ring.complete_issued(
                            issued,
                            TerminalCause::Completed,
                            negative_errno(error),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted)
                    }
                };
            }
            let result = io_result(attempt);
            let completed = ring.complete_issued(issued, TerminalCause::Completed, result, 0);
            drop(file);
            drop(buffer);
            completed
        }
        SubmissionOperation::Write(request) => {
            if let Some(admission) = physical {
                return match publish_physical_admission(ring, issued, admission)? {
                    PhysicalPublishDecision::NotSubmitted { issued, admission } => {
                        let result = match io_uring_pwrite64_worker(admission)? {
                            IoUringWorkerResult::Completed(result) => result,
                            IoUringWorkerResult::Failed(error) => negative_errno(error) as isize,
                        };
                        ring.complete_issued(issued, TerminalCause::Completed, result as i32, 0)
                            .map(|_| SubmissionOutcome::Accepted)
                    }
                    PhysicalPublishDecision::Pending
                    | PhysicalPublishDecision::Queued
                    | PhysicalPublishDecision::Quarantined
                    | PhysicalPublishDecision::Completed => Ok(SubmissionOutcome::Accepted),
                };
            }
            drop(issued);
            // Physical writes intentionally remain on the submission task.
            // axfs-ng does not expose a side-effect-free device-admission
            // token, so setid/capability cleanup and RLIMIT_FSIZE handling
            // must retain their Linux task-local ordering here.
            drop(physical);
            let Some(file) = file else {
                drop(buffer);
                ring.complete_request(
                    id,
                    TerminalCause::PreparationFailed,
                    -LinuxError::EBADF.code(),
                    0,
                )?;
                return Ok(SubmissionOutcome::Accepted);
            };
            if request.fixed_buffer().is_some() && buffer.is_none() {
                drop(file);
                ring.complete_request(
                    id,
                    TerminalCause::PreparationFailed,
                    -LinuxError::EFAULT.code(),
                    0,
                )?;
                return Ok(SubmissionOutcome::Accepted);
            }
            let io_capability = match submission_io_capability(
                &capability,
                request.fixed_buffer().is_some(),
                || buffer.as_ref().ok_or(AxError::BadAddress)?.capability(),
            ) {
                Ok(capability) => capability,
                Err(_) => {
                    drop((file, buffer));
                    ring.complete_request(
                        id,
                        TerminalCause::PreparationFailed,
                        -LinuxError::EFAULT.code(),
                        0,
                    )?;
                    return Ok(SubmissionOutcome::Accepted);
                }
            };
            let (io_address, io_length) = match submission_io_range(request, buffer.as_ref()) {
                Ok(range) => range,
                Err(_) => {
                    drop((file, buffer));
                    ring.complete_request(
                        id,
                        TerminalCause::PreparationFailed,
                        -LinuxError::EFAULT.code(),
                        0,
                    )?;
                    return Ok(SubmissionOutcome::Accepted);
                }
            };
            let fixed_segments = if request.fixed_buffer().is_some() {
                match buffer
                    .as_ref()
                    .ok_or(AxError::BadAddress)
                    .and_then(|lease| {
                        let (segments, offset, length, disjoint) = lease.physical_range()?;
                        let provenance = lease.physical_provenance()?;
                        Ok((segments, offset, length, disjoint, provenance))
                    }) {
                    Ok(range) => Some(range),
                    Err(_) => {
                        drop((file, buffer));
                        ring.complete_request(
                            id,
                            TerminalCause::PreparationFailed,
                            -LinuxError::EFAULT.code(),
                            0,
                        )?;
                        return Ok(SubmissionOutcome::Accepted);
                    }
                }
            } else {
                None
            };
            let result = io_result(file.description().and_then(|description| {
                let context = context.as_ref().ok_or(AxError::BadState)?;
                io_uring_pwrite64_submission(
                    &io_capability,
                    description,
                    context,
                    io_address as *const u8,
                    io_length,
                    request.offset(),
                    fixed_segments,
                )
            }));
            let completed = ring.complete_request(id, TerminalCause::Completed, result, 0);
            drop(file);
            drop(buffer);
            completed
        }
        SubmissionOperation::AsyncCancel { target_user_data } => {
            drop((file, buffer));
            ring.cancel_request(issued, target_user_data)
        }
        SubmissionOperation::PollAdd(_) => {
            // POLL_ADD normally commits through SubmissionAdmission::commit_poll,
            // so reaching the ordinary execution path indicates an adapter
            // contract mismatch rather than an unsupported userspace opcode.
            drop((issued, file, buffer));
            ring.complete_request(
                id,
                TerminalCause::PreparationFailed,
                -LinuxError::EIO.code(),
                0,
            )
        }
    }?;
    Ok(SubmissionOutcome::Accepted)
}

fn submit_entries(
    ring: &IoUring,
    requested: u32,
    capability: &UserMemoryCapability,
) -> AxResult<(u32, bool)> {
    let mut examined = 0;
    let mut submitted = 0;
    while examined < requested {
        let step = match ring.prepare_submission() {
            Ok(step) => step,
            Err(error) if submitted != 0 => {
                error!("io_uring stopped after {submitted} accepted SQEs: {error:?}");
                return Ok((submitted, false));
            }
            Err(error) => return Err(error),
        };
        match step {
            SubmissionStep::Empty
            | SubmissionStep::CompletionQueueFull
            | SubmissionStep::AdmissionBusy => return Ok((submitted, false)),
            SubmissionStep::Dropped => {
                examined += 1;
            }
            SubmissionStep::Admission(admission) => {
                examined += 1;
                let parsed = admission.parsed();
                let mut lease = match parsed.ok().and_then(submission_file) {
                    Some(target) => match retain_submission_file(ring, target) {
                        Ok(lease) => Some(lease),
                        Err(error) => {
                            let work = match admission.commit(
                                None,
                                None,
                                None,
                                None,
                                None,
                                capability.clone(),
                            ) {
                                Ok(work) => work,
                                Err(commit_error) if submitted != 0 => {
                                    error!(
                                        "io_uring stopped before committing a file-binding error: \
                                         {commit_error:?}"
                                    );
                                    return Ok((submitted, false));
                                }
                                Err(commit_error) => return Err(commit_error),
                            };
                            submitted += 1;
                            let (prepared, ..) = work.into_parts();
                            if let Err(completion_error) =
                                complete_preparation_failure(ring, prepared, negative_errno(error))
                            {
                                error!(
                                    "io_uring failed to publish file-binding error after \
                                     acceptance: {completion_error:?}"
                                );
                                return Ok((submitted, false));
                            }
                            // Linux resolves ordinary and fixed files at issue
                            // time. An EBADF CQE is therefore not a submission
                            // preparation failure and does not stop the batch.
                            continue;
                        }
                    },
                    None => None,
                };

                let mut buffer = match parsed.ok().and_then(submission_buffer) {
                    Some(fixed) => match retain_submission_buffer(ring, fixed) {
                        Ok(buffer) => Some(buffer),
                        Err(error) => {
                            drop(lease);
                            let work = match admission.commit(
                                None,
                                None,
                                None,
                                None,
                                None,
                                capability.clone(),
                            ) {
                                Ok(work) => work,
                                Err(commit_error) if submitted != 0 => {
                                    error!(
                                        "io_uring stopped before committing a buffer-binding \
                                         error: {commit_error:?}"
                                    );
                                    return Ok((submitted, false));
                                }
                                Err(commit_error) => return Err(commit_error),
                            };
                            submitted += 1;
                            let (prepared, ..) = work.into_parts();
                            if let Err(completion_error) =
                                complete_preparation_failure(ring, prepared, negative_errno(error))
                            {
                                error!(
                                    "io_uring failed to publish buffer-binding error after \
                                     acceptance: {completion_error:?}"
                                );
                                return Ok((submitted, false));
                            }
                            // A bad fixed-buffer index/range is an operation
                            // error; later SQEs in the default batch remain
                            // eligible for admission.
                            continue;
                        }
                    },
                    None => None,
                };

                // Capture the immutable actor/OFD snapshot while the SQE is
                // still being admitted.  The retained lease supplies the
                // exact description; execution must not recreate this from
                // whichever task happens to drain a future work item.
                let mut context = match parsed.ok().map(ParsedSubmission::operation) {
                    Some(SubmissionOperation::Read(_)) | Some(SubmissionOperation::Write(_)) => {
                        lease
                            .as_ref()
                            .map(|lease| lease.description().map(capture_io_operation_context))
                            .transpose()?
                    }
                    _ => None,
                };

                // Only a fixed-buffer regular ext4 O_DIRECT request can
                // receive an owned worker token. Policy failures are carried
                // into the accepted CQE so execution does not repeat a
                // fanotify wait or RLIMIT_FSIZE signal.
                let mut physical = None;
                let mut admission_error = None;
                if physical_effect_admission_enabled()
                    && let Ok(parsed) = parsed
                {
                    let operation = match parsed.operation() {
                        SubmissionOperation::Read(request) if request.fixed_buffer().is_some() => {
                            Some((PreparedPhysicalIoOperation::Read, request))
                        }
                        SubmissionOperation::Write(request) if request.fixed_buffer().is_some() => {
                            Some((PreparedPhysicalIoOperation::Write, request))
                        }
                        _ => None,
                    };
                    if let Some((operation, request)) = operation
                        && let (Some(file_lease), Some(buffer_lease), Some(context_ref)) =
                            (lease.as_ref(), buffer.as_ref(), context.as_ref())
                    {
                        match prepare_physical_io_plan(
                            file_lease,
                            buffer_lease,
                            context_ref,
                            operation,
                            request.offset(),
                        ) {
                            Ok(Some(plan)) => {
                                match prepare_physical_io_write_memfd_guard(
                                    file_lease,
                                    context_ref,
                                    &plan,
                                ) {
                                    Ok(memfd) => {
                                        match prepare_physical_io_effect(file_lease, &plan) {
                                            Ok(Some(effect)) => {
                                                match prepare_physical_io_write_privilege_guard(
                                                    file_lease,
                                                    context_ref,
                                                    &plan,
                                                ) {
                                                    Ok(privilege) => {
                                                        let file_lease = lease
                                                            .take()
                                                            .ok_or(AxError::BadState)?;
                                                        let buffer_lease = buffer
                                                            .take()
                                                            .ok_or(AxError::BadState)?;
                                                        let prepared =
                                                            PreparedPhysicalIoAdmission::new(
                                                                file_lease,
                                                                buffer_lease,
                                                                context
                                                                    .take()
                                                                    .ok_or(AxError::BadState)?,
                                                                plan,
                                                                memfd,
                                                                privilege,
                                                                effect,
                                                            );
                                                        match prepared {
                                                            Ok(prepared) => {
                                                                physical = Some(prepared)
                                                            }
                                                            Err(error) => {
                                                                admission_error = Some(error)
                                                            }
                                                        }
                                                    }
                                                    Err(error) => admission_error = Some(error),
                                                }
                                            }
                                            Ok(None) => {}
                                            Err(error) => admission_error = Some(error),
                                        }
                                    }
                                    Err(error) => admission_error = Some(error),
                                }
                            }
                            Ok(None) => {}
                            Err(error) => admission_error = Some(error),
                        }
                    }
                }

                if let Ok(parsed) = parsed
                    && let SubmissionOperation::PollAdd(request) = parsed.operation()
                {
                    let lease = lease.ok_or(AxError::BadState)?;
                    if let Err(error) =
                        admission.commit_poll(lease, request.events(), capability.clone())
                    {
                        if submitted != 0 {
                            error!("io_uring stopped before POLL admission: {error:?}");
                            return Ok((submitted, false));
                        }
                        return Err(error);
                    }
                    submitted += 1;
                    continue;
                }

                let work = match admission.commit(
                    lease,
                    buffer,
                    context,
                    physical,
                    admission_error,
                    capability.clone(),
                ) {
                    Ok(work) => work,
                    Err(error) if submitted != 0 => {
                        error!("io_uring stopped before SQ admission commit: {error:?}");
                        return Ok((submitted, false));
                    }
                    Err(error) => return Err(error),
                };
                submitted += 1;
                match execute_submission(ring, work) {
                    Ok(outcome) if outcome.stops_default_batch() => {
                        return Ok((submitted, false));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        error!("io_uring completion failed after acceptance: {error:?}");
                        return Ok((submitted, false));
                    }
                }
            }
        }
    }
    Ok((submitted, true))
}

fn copied_signal_mask(
    capability: &UserMemoryCapability,
    request: EnterRequest,
) -> AxResult<Option<SignalSet>> {
    match request.signal_mask() {
        LegacySignalMask::None => Ok(None),
        LegacySignalMask::Address(address) => {
            let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
            let value = capability
                .read_value_uninit::<SignalSet>(address as *const SignalSet)
                .map_err(map_usercopy_error)?;
            // SAFETY: the explicit capability read initialized the complete
            // signal-set object before returning success.
            Ok(Some(unsafe { value.assume_init() }))
        }
    }
}

pub fn sys_io_uring_enter(
    capability: UserMemoryCapability,
    uctx: &mut UserContext,
    fd: i32,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    sig: usize,
    argsz: usize,
) -> AxResult<isize> {
    let request = EnterRequest::from_raw(
        to_submit,
        min_complete,
        flags,
        sig as u64,
        // The ABI size is meaningful only if this invocation reaches an
        // actual GETEVENTS wait with a non-NULL mask. Defer validation until
        // after submissions and the immediate-completion check; otherwise a
        // caller that never waits would observe an incorrect EINVAL.
        size_of::<SignalSet>() as u64,
        size_of::<SignalSet>() as u64,
    )
    .map_err(map_policy_error)?;
    let ring = ring_from_fd(fd)?;
    ring.observe_completion_head()?;

    let (submitted, complete_batch) =
        submit_entries(ring.as_ref(), request.to_submit(), &capability)?;
    if !request.flags().contains(EnterFlags::GETEVENTS)
        || !complete_batch
        || submitted != request.to_submit()
    {
        return Ok(submitted as isize);
    }

    let minimum = request.minimum_complete().min(ring.layout().cq_entries());
    let immediately_ready = ring
        .observe_completion_head()
        .and_then(|_| ring.available_completions())
        .map(|available| available >= minimum);
    match immediately_ready {
        Ok(true) => return Ok(submitted as isize),
        Ok(false) => {}
        Err(_) if submitted != 0 => return Ok(submitted as isize),
        Err(error) => return Err(error),
    }

    // Only this path is an actual GETEVENTS wait. A legacy mask is optional;
    // when present, Linux validates its exact size before copying it in.
    let sigmask = match request.signal_mask() {
        LegacySignalMask::None => None,
        LegacySignalMask::Address(_) => {
            check_sigset_size(argsz)?;
            copied_signal_mask(&capability, request)?
        }
    };

    let mut wait_once = || {
        Ok::<AxResult<isize>, future::Elapsed>(crate::readiness::block_on_poll_io(
            ring.as_ref(),
            IoEvents::READABLE,
            false,
            || {
                ring.observe_completion_head()?;
                if ring.available_completions()? >= minimum {
                    Ok(submitted as isize)
                } else {
                    Err(AxError::WouldBlock)
                }
            },
        ))
    };
    if submitted == 0 {
        wait_io_result(Some(uctx), sigmask, &mut wait_once)
    } else {
        // Linux returns the accepted submission count when the later wait is
        // interrupted or otherwise fails. Leave signal dispatch to the normal
        // return-to-userspace path so its frame observes that successful count,
        // while wait_io_result still restores the temporary mask on every exit.
        match wait_io_result(None, sigmask, &mut wait_once) {
            Ok(result) => Ok(result),
            Err(_) => Ok(submitted as isize),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use spin::Mutex;

    use super::*;

    #[derive(Clone)]
    struct TestIoCapability {
        address_space: Arc<Mutex<[u8; 1]>>,
    }

    impl TestIoCapability {
        fn new() -> Self {
            Self {
                address_space: Arc::new(Mutex::new([0])),
            }
        }

        fn address_space(&self) -> &Arc<Mutex<[u8; 1]>> {
            &self.address_space
        }

        fn read_byte(&self) -> u8 {
            self.address_space.lock()[0]
        }

        fn write_byte(&self, value: u8) {
            self.address_space.lock()[0] = value;
        }
    }

    #[test]
    fn policy_failures_keep_malformed_and_buffer_range_distinct() {
        assert_eq!(
            LinuxError::from(map_policy_error(IoUringError::InvalidRegistration)),
            LinuxError::EINVAL
        );
        assert_eq!(
            LinuxError::from(map_policy_error(IoUringError::InvalidBufferRange)),
            LinuxError::EINVAL
        );
        assert_eq!(
            LinuxError::from(map_policy_error(IoUringError::UnknownOpcode)),
            LinuxError::EINVAL
        );
        assert_eq!(
            LinuxError::from(map_policy_error(IoUringError::CurrentPositionUnsupported)),
            LinuxError::EOPNOTSUPP
        );
    }

    #[test]
    fn registered_buffer_range_overflow_errno_matches_linux_ordering() {
        let overflow = |address, length| {
            LinuxError::from(validate_registered_buffer_range(address, length).unwrap_err())
        };

        assert_eq!(overflow(0x1000, 0), LinuxError::EFAULT);

        // Both the byte end and its page cover must be checked before the
        // user-memory access check, and both are EOVERFLOW on Linux.
        assert_eq!(overflow(usize::MAX - 1, 2), LinuxError::EOVERFLOW);
        assert_eq!(
            overflow(usize::MAX - PAGE_SIZE_4K, 2),
            LinuxError::EOVERFLOW
        );

        // Linux rejects an over-SZ_1T descriptor as EINVAL before considering
        // a potentially overflowing address expression.
        assert_eq!(
            overflow(0x1000, REGISTERED_BUFFER_MAX_LEN + 1),
            LinuxError::EINVAL
        );
    }

    #[test]
    fn registered_buffer_count_keeps_null_usercopy_precedence_bounded() {
        assert_eq!(
            LinuxError::from(registered_buffer_count(0, 0).unwrap_err()),
            LinuxError::EFAULT
        );
        assert_eq!(
            LinuxError::from(registered_buffer_count(0, u32::MAX).unwrap_err()),
            LinuxError::EFAULT
        );
        assert_eq!(
            LinuxError::from(registered_buffer_count(1, 0).unwrap_err()),
            LinuxError::EINVAL
        );
        assert_eq!(
            LinuxError::from(
                registered_buffer_count(
                    1,
                    thekernel_linux_io_uring::IORING_MAX_REGISTERED_BUFFERS + 1,
                )
                .unwrap_err(),
            ),
            LinuxError::EINVAL
        );
    }

    #[test]
    fn default_batch_stops_after_a_submission_failure() {
        assert!(SubmissionOutcome::FailedDuringSubmission.stops_default_batch());
        assert!(!SubmissionOutcome::Accepted.stops_default_batch());
    }

    #[test]
    fn fixed_submission_keeps_registration_address_space() {
        let registered = TestIoCapability::new();
        let caller = TestIoCapability::new();

        let selected = submission_io_capability(&caller, true, || Ok(registered.clone()))
            .expect("fixed operation must use its registration capability");
        assert!(Arc::ptr_eq(
            selected.address_space(),
            registered.address_space()
        ));
        assert!(!Arc::ptr_eq(
            selected.address_space(),
            caller.address_space()
        ));

        selected.write_byte(0x5a);
        assert_eq!(registered.read_byte(), 0x5a);
        assert_eq!(caller.read_byte(), 0);

        let ordinary = submission_io_capability(&caller, false, || {
            panic!("ordinary I/O must not ask for a registered capability")
        })
        .unwrap();
        assert!(Arc::ptr_eq(
            ordinary.address_space(),
            caller.address_space()
        ));
    }
}
