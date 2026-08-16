use alloc::{sync::Arc, vec::Vec};
use core::{
    ffi::c_int,
    mem::{MaybeUninit, size_of},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use axtask::future;
use thekernel_linux_io_uring::{
    BufferSlot, EnterFlags, EnterRequest, FeatureFlags, FileTarget, IoUringError, LegacySignalMask,
    PINNED_IORING_OP_LAST, ParsedSubmission, PreparedRequest, ReadWriteRequest,
    RegistrationOperation, RegistrationRequest, SetupRequest, SubmissionOperation, TerminalCause,
};
use thekernel_linux_signal::SignalSet;

use super::{io_uring_pread64, io_uring_pwrite64};
use crate::{
    file::{
        FileDescription, FileHandle, FileLike, get_file_description, get_file_like,
        io_uring::{IoUring, IoUringBufferLease, IoUringFileLease, SubmissionStep, SubmissionWork},
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
    let count = usize::try_from(count).map_err(|_| AxError::InvalidInput)?;
    if count == 0 || count > thekernel_linux_io_uring::IORING_MAX_REGISTERED_BUFFERS as usize {
        return Err(AxError::InvalidInput);
    }
    if argument == 0 {
        // Linux reaches the iovec copy for a valid count and reports a bad
        // userspace address; NULL is not a malformed registration header.
        return Err(AxError::BadAddress);
    }
    let address = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
    let bytes = count
        .checked_mul(size_of::<IoVec>())
        .ok_or(AxError::BadAddress)?;
    address.checked_add(bytes).ok_or(AxError::BadAddress)?;
    let mut descriptors = Vec::<MaybeUninit<IoVec>>::new();
    descriptors
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    descriptors.resize_with(count, MaybeUninit::uninit);
    capability
        .read_slice(address as *const IoVec, &mut descriptors)
        .map_err(map_usercopy_error)?;

    let mut buffers = Vec::new();
    buffers
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    for descriptor in descriptors {
        // SAFETY: `read_slice` initialized every descriptor before returning.
        let descriptor = unsafe { descriptor.assume_init() };
        if descriptor.iov_len <= 0 {
            return Err(AxError::InvalidInput);
        }
        let length = usize::try_from(descriptor.iov_len).map_err(|_| AxError::InvalidInput)?;
        let address = descriptor.iov_base as usize;
        address.checked_add(length).ok_or(AxError::BadAddress)?;
        buffers.push((address, length));
    }
    // Validate/populate all ranges before any long-lived pin is installed.
    // This keeps malformed or inaccessible later iovecs from publishing a
    // partially visible table while retaining NoMemory versus bad-address
    // errors from the explicit user-memory capability.
    for &(address, length) in &buffers {
        check_user_writable_with(capability, address, length)?;
    }
    Ok(buffers)
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

fn submission_io_capability(
    caller: &UserMemoryCapability,
    fixed: bool,
    registered: impl FnOnce() -> AxResult<UserMemoryCapability>,
) -> AxResult<UserMemoryCapability> {
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

impl SubmissionOutcome {
    const fn stops_default_batch(self) -> bool {
        matches!(self, Self::FailedDuringSubmission)
    }
}

fn execute_submission(ring: &IoUring, work: SubmissionWork) -> AxResult<SubmissionOutcome> {
    let (prepared, parsed, file, buffer, capability) = work.into_parts();
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

    match parsed.operation() {
        SubmissionOperation::Nop => {
            drop(issued);
            drop((file, buffer));
            ring.complete_request(id, TerminalCause::Completed, 0, 0)
        }
        SubmissionOperation::Read(request) => {
            drop(issued);
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
            let result = io_result(file.description().and_then(|description| {
                io_uring_pread64(
                    &io_capability,
                    description,
                    io_address as *mut u8,
                    io_length,
                    request.offset(),
                )
            }));
            let completed = ring.complete_request(id, TerminalCause::Completed, result, 0);
            drop(file);
            drop(buffer);
            completed
        }
        SubmissionOperation::Write(request) => {
            drop(issued);
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
            let result = io_result(file.description().and_then(|description| {
                io_uring_pwrite64(
                    &io_capability,
                    description,
                    io_address as *const u8,
                    io_length,
                    request.offset(),
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
                let lease = match parsed.ok().and_then(submission_file) {
                    Some(target) => match retain_submission_file(ring, target) {
                        Ok(lease) => Some(lease),
                        Err(error) => {
                            let work = match admission.commit(None, None, capability.clone()) {
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

                let buffer = match parsed.ok().and_then(submission_buffer) {
                    Some(fixed) => match retain_submission_buffer(ring, fixed) {
                        Ok(buffer) => Some(buffer),
                        Err(error) => {
                            drop(lease);
                            let work = match admission.commit(None, None, capability.clone()) {
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

                let work = match admission.commit(lease, buffer, capability.clone()) {
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
    use core::mem::MaybeUninit;

    use axhal::paging::{MappingFlags, PageSize};
    use axsync::Mutex;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::*;

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
    fn default_batch_stops_after_a_submission_failure() {
        assert!(SubmissionOutcome::FailedDuringSubmission.stops_default_batch());
        assert!(!SubmissionOutcome::Accepted.stops_default_batch());
    }

    #[test]
    fn fixed_submission_keeps_registration_address_space() {
        let mut registered =
            crate::mm::AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K * 2).unwrap();
        registered
            .map(
                VirtAddr::from(0x1000),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                crate::mm::Backend::new_alloc(VirtAddr::from(0x1000), PageSize::Size4K),
            )
            .unwrap();
        let registered = UserMemoryCapability::new(Arc::new(Mutex::new(registered)));
        let caller = UserMemoryCapability::new(Arc::new(Mutex::new(
            crate::mm::AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K * 2).unwrap(),
        )));

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

        selected.write_bytes(0x1000, &[0x5a]).unwrap();
        let mut value = [MaybeUninit::<u8>::uninit()];
        registered.read_bytes(0x1000, &mut value).unwrap();
        // SAFETY: the selected capability initialized the mapped byte.
        assert_eq!(unsafe { value[0].assume_init() }, 0x5a);
        let mut wrong_space = [MaybeUninit::<u8>::uninit()];
        assert!(caller.read_bytes(0x1000, &mut wrong_space).is_err());

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
