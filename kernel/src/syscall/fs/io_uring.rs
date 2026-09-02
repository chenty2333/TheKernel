use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{
    cell::RefCell,
    ffi::c_int,
    mem::{MaybeUninit, size_of},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{
    FileIoCompletion, FileIoPrepareError, ImmediateFileIoResult, OwnedFileIoCompletion,
};
use axhal::{
    time::{TimeValue, wall_time},
    uspace::UserContext,
};
use axpoll::IoEvents;
use axtask::future;
use linux_raw_sys::general::{AT_FDCWD, RESOLVE_IN_ROOT};
use memory_addr::PAGE_SIZE_4K;
use spin::Mutex;
use thekernel_linux_io_uring::{
    BufferSlot, EnterFlags, EnterRequest, FeatureFlags, FileTarget, IO_URING_PARAMS_BYTES,
    IoUringError, IoUringGeteventsArg, IoUringParams, LegacySignalMask, ParsedSubmission,
    PreparedRequest, ReadWriteRequest, RegistrationOperation, RegistrationRequest, SetupRequest,
    SubmissionOperation, TerminalCause, encode_probe, probe_output_bytes,
};
use thekernel_linux_signal::SignalSet;

use super::{
    ClassicAioOperation, IoUringWorkerResult, capture_io_operation_context_for_actor,
    execute_classic_aio_operation, io_uring_pread64_submission,
    io_uring_pread64_submission_nonblocking_stream, io_uring_pread64_worker,
    io_uring_pwrite64_submission, io_uring_pwrite64_worker, physical_effect_admission_enabled,
    prepare_classic_aio_owned_operation, prepare_io_uring_vectored_operation,
    prepare_physical_io_effect, prepare_physical_io_plan, prepare_physical_io_write_memfd_guard,
    prepare_physical_io_write_privilege_guard, rwf_status,
};

/// Completion bridge for generic owned file I/O.  The exact request identity
/// includes the reusable slot generation, while the weak ring link prevents a
/// provider-held long-term pin from extending a closed ring's lifetime.
struct OwnedFileIoCompletionBridge {
    ring: alloc::sync::Weak<IoUring>,
    id: thekernel_linux_io_uring::RequestId,
}

impl OwnedFileIoCompletion for OwnedFileIoCompletionBridge {
    fn complete(self: Box<Self>, completion: FileIoCompletion) {
        let Some(ring) = self.ring.upgrade() else {
            return;
        };
        let result = match completion.result {
            ImmediateFileIoResult::Completed(bytes) => {
                i32::try_from(bytes).unwrap_or(-LinuxError::EOVERFLOW.code())
            }
            ImmediateFileIoResult::Cancelled => -LinuxError::ECANCELED.code(),
            ImmediateFileIoResult::Failed(error) => -LinuxError::from(error).code(),
        };
        // The request registry owns the single terminal credit; a stale
        // provider completion after cancel/close is intentionally ignored.
        ring.complete_owned_file_io(self.id, result);
    }

    fn into_retry_completion(self: Box<Self>) -> Box<dyn OwnedFileIoCompletion> {
        self
    }
}
use crate::{
    file::{
        File, FileDescription, FileHandle, FileLike, FileLikeKind, IoOperationContext, UringCmd,
        current_fd_table,
        event::EventFd,
        fanotify::FanotifyEventActor,
        get_file_description, get_file_like, get_typed_file,
        io_uring::{
            DependencyDispatch, IoUring, IoUringBufferLease, IoUringFileLease, IoUringOpenAt2Work,
            IoUringSubmissionActor, PendingStreamAdmissionError, PreparedPhysicalIoAdmission,
            PreparedPhysicalIoOperation, SubmissionStep, SubmissionWork,
        },
        permission::VfsSecurityContext,
        prepare_file_description_with_resource, reserve_fd,
    },
    mm::{IoVec, UserMemoryCapability, check_user_writable_with, map_usercopy_error},
    syscall::{io_mpx::wait_io_result, signal::check_sigset_size},
    task::{AsThread, has_pending_syscall_signal},
};

const THEKERNEL_IO_URING_FEATURES: FeatureFlags = FeatureFlags::SINGLE_MMAP
    .union(FeatureFlags::NODROP)
    .union(FeatureFlags::SUBMIT_STABLE)
    .union(FeatureFlags::POLL_32BITS);

// Linux's v6.18 io_validate_user_buf_range rejects fixed registrations above
// SZ_1G before checking address arithmetic. Preserve that ordering so
// SIZE_MAX-sized descriptors remain EFAULT, while under-limit page-cover
// overflow is reported as EOVERFLOW.
const REGISTERED_BUFFER_MAX_LEN: usize = 1 << 30;
// `io_uring_mem_region_reg` is bounded independently from ordinary fixed
// buffers: its ABI limit is INT_MAX 4K pages, not the registered-buffer 1GiB
// policy cap.
const MEM_REGION_MAX_LEN: usize = (i32::MAX as usize) * PAGE_SIZE_4K;
const IO_RINGFD_REG_MAX: usize = 16;

/// v6.18 registered rings are task-local resources. The table stores typed
/// owners rather than descriptor numbers, so fd reuse/final descriptor close
/// cannot retarget an existing index.
struct RegisteredRingTable {
    rings: Vec<Option<Arc<IoUring>>>,
}

static REGISTERED_RINGS: Mutex<BTreeMap<u64, RegisteredRingTable>> = Mutex::new(BTreeMap::new());

/// Linux's `current->io_uring` lifetime is distinct from whether that task
/// currently owns any registered-ring indexes.  Keep the task context as its
/// own TID-keyed object so an empty index table cannot change unregister's
/// usercopy ordering.
static IO_URING_TASK_CONTEXTS: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());

fn current_task_context_id() -> u64 {
    axtask::current().as_thread().kernel_tid() as u64
}

fn ensure_current_task_io_uring_context() {
    IO_URING_TASK_CONTEXTS
        .lock()
        .insert(current_task_context_id());
}

fn validate_registered_buffer_range(address: usize, length: usize) -> AxResult<()> {
    if length == 0 {
        // Linux's io_validate_user_buf_range reports an empty non-NULL
        // registration as EFAULT before it attempts to pin the range.
        return Err(AxError::BadAddress);
    }
    if length > REGISTERED_BUFFER_MAX_LEN {
        return Err(AxError::BadAddress);
    }
    let end = address
        .checked_add(length)
        .ok_or_else(|| AxError::from(LinuxError::EOVERFLOW))?;
    end.checked_add(PAGE_SIZE_4K - 1)
        .ok_or_else(|| AxError::from(LinuxError::EOVERFLOW))?;
    Ok(())
}

fn validate_mem_region_user_range(address: usize, length: usize) -> AxResult<()> {
    if length > MEM_REGION_MAX_LEN {
        return Err(AxError::from(LinuxError::E2BIG));
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

/// `io_uring_enter()` exposes a successfully accepted SQ batch even when the
/// following GETEVENTS decoding, usercopy, signal-mask install, or wait fails.
/// Keep that rule at one boundary so new post-submit validation cannot
/// accidentally turn an accepted batch into a syscall errno.
fn post_submit_result(submitted: u32, result: AxResult<isize>) -> AxResult<isize> {
    if submitted != 0 {
        result.or(Ok(submitted as isize))
    } else {
        result
    }
}

fn ring_from_fd(fd: c_int) -> AxResult<FileHandle<IoUring>> {
    get_file_like(fd)?
        .downcast::<IoUring>()
        .map_err(|_| AxError::OperationNotSupported)
}

fn current_submission_actor(capability: UserMemoryCapability) -> IoUringSubmissionActor {
    let current = axtask::current();
    let thread = current.as_thread();
    IoUringSubmissionActor::new(
        current_fd_table(),
        capability,
        VfsSecurityContext::new(thread.current_cred()),
        FanotifyEventActor::current(),
        thread.proc_data.proc.pid(),
        thread.namespace_credential_fs_snapshot(),
        thread.proc_data.rlim.read()[linux_raw_sys::general::RLIMIT_NOFILE].current as usize,
    )
}

pub fn sys_io_uring_setup(
    capability: UserMemoryCapability,
    entries: u32,
    params: *mut u8,
) -> AxResult<isize> {
    let input_bytes = capability
        .read_value_uninit(params as *const [u8; IO_URING_PARAMS_BYTES])
        .map_err(map_usercopy_error)?;
    // SAFETY: `read_value_uninit` returned success only after initializing the
    // complete fixed-size byte array. Every bit pattern is valid for bytes.
    let input = IoUringParams::decode(unsafe { input_bytes.assume_init() });
    let layout = SetupRequest::from_raw(
        entries,
        input.cq_entries(),
        input.flags(),
        input.sq_thread_cpu(),
        input.sq_thread_idle(),
        input.wq_fd(),
        input.reserved(),
    )
    .and_then(|request| request.resolve(THEKERNEL_IO_URING_FEATURES))
    .map_err(map_policy_error)?;

    let reservation = reserve_fd(true)?;
    let ring = IoUring::try_new(layout)?;
    // Construct the final-close owner before any worker can retain an Arc.
    // Every subsequent setup failure then drops this resource and requests
    // worker termination rather than leaking the actor/mm/files snapshot.
    let finalizer = ring.try_finalizer_resource()?;
    if ring.sqpoll_enabled() {
        ring.install_sqpoll_actor(current_submission_actor(capability.clone()))?;
        if !ring.disabled() {
            start_sqpoll_worker(ring.clone())?;
        }
    }
    let ring_file: Arc<dyn FileLike> = ring.clone();
    let description = prepare_file_description_with_resource(ring_file, 0, None, Some(finalizer))?;
    let publication = reservation.prepare_publication(description)?;

    // The descriptor is still absent from lookup while userspace receives the
    // exact geometry. A copyout fault therefore rolls every prepared owner
    // back without exposing a partially initialized ring.
    capability
        .write_bytes(
            params as usize,
            &IoUringParams::from_layout(ring.layout()).encode(),
        )
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
    let bytes = probe_output_bytes(operations);
    let address = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(bytes)
        .map_err(|_| AxError::NoMemory)?;
    output.resize(bytes, 0);
    if !encode_probe(&mut output, operations) {
        return Err(AxError::InvalidInput);
    }
    capability
        .write_bytes(address, &output)
        .map_err(map_usercopy_error)
}

fn copied_ring_update(capability: &UserMemoryCapability, address: usize) -> AxResult<(u32, u64)> {
    let raw = capability
        .read_value_uninit(address as *const [u8; 16])
        .map_err(map_usercopy_error)?;
    // SAFETY: the full fixed update record was copied.
    let raw = unsafe { raw.assume_init() };
    if raw[4..8].iter().any(|byte| *byte != 0) {
        return Err(AxError::InvalidInput);
    }
    Ok((
        u32::from_ne_bytes(raw[0..4].try_into().unwrap()),
        u64::from_ne_bytes(raw[8..16].try_into().unwrap()),
    ))
}

fn copy_ring_update_offset(
    capability: &UserMemoryCapability,
    address: usize,
    offset: u32,
) -> AxResult<()> {
    capability
        .write_bytes(address, &offset.to_ne_bytes())
        .map_err(map_usercopy_error)
}

fn register_ring_fds(
    capability: &UserMemoryCapability,
    argument: u64,
    count: u32,
) -> AxResult<usize> {
    if count == 0 || count as usize > IO_RINGFD_REG_MAX {
        return Err(AxError::InvalidInput);
    }
    // io_uring_add_tctx_node follows the bounded nr_args validation and
    // precedes the caller-controlled update-array copy.
    ensure_current_task_io_uring_context();
    let base = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
    let task = axtask::current().as_thread().kernel_tid() as u64;
    let mut completed = 0usize;
    for item in 0..usize::try_from(count).map_err(|_| AxError::InvalidInput)? {
        let address = base
            .checked_add(item.checked_mul(16).ok_or(AxError::BadAddress)?)
            .ok_or(AxError::BadAddress)?;
        let (requested, data) = match copied_ring_update(capability, address) {
            Ok(value) => value,
            Err(_) if completed != 0 => return Ok(completed),
            Err(error) => return Err(error),
        };
        // IORING_FILE_INDEX_ALLOC is the sole automatic-index marker.  An
        // explicit index is validated before fget(), so an invalid offset
        // cannot be obscured by an unrelated invalid `data` descriptor.
        let automatic = requested == u32::MAX;
        if !automatic && requested as usize >= IO_RINGFD_REG_MAX {
            return if completed != 0 {
                Ok(completed)
            } else {
                Err(AxError::InvalidInput)
            };
        }
        // `io_uring_rsrc_update2.data` carries an `int fd`; Linux consumes
        // its low 32 bits and ignores upper-bit noise rather than validating
        // the raw u64 as a sign-extended integer.
        let fd = (data as u32) as i32;
        let target = match ring_from_fd(fd) {
            Ok(ring) => ring.clone_object(),
            Err(_) if completed != 0 => return Ok(completed),
            Err(error) => return Err(error),
        };
        let assigned = {
            let mut tables = REGISTERED_RINGS.lock();
            let table = tables
                .entry(task)
                .or_insert_with(|| RegisteredRingTable { rings: Vec::new() });
            let assigned = if automatic {
                table
                    .rings
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or(table.rings.len())
            } else {
                requested as usize
            };
            // A valid occupied index, or automatic placement after all 16
            // slots are full, is the EBUSY resource-exhaustion case.
            if assigned >= IO_RINGFD_REG_MAX
                || table.rings.get(assigned).is_some_and(Option::is_some)
            {
                return if completed != 0 {
                    Ok(completed)
                } else {
                    Err(AxError::ResourceBusy)
                };
            }
            if assigned == table.rings.len() {
                table
                    .rings
                    .try_reserve_exact(1)
                    .map_err(|_| AxError::NoMemory)?;
                table.rings.push(None);
            }
            table.rings[assigned] = Some(target);
            u32::try_from(assigned).map_err(|_| AxError::InvalidInput)?
        };
        if let Err(error) = copy_ring_update_offset(capability, address, assigned) {
            // The copyout is part of publication: callers cannot discover a
            // newly allocated index after EFAULT, so retract this exact slot
            // before returning. Earlier successfully copied records remain.
            if let Some(table) = REGISTERED_RINGS.lock().get_mut(&task)
                && let Some(slot) = table.rings.get_mut(assigned as usize)
            {
                *slot = None;
            }
            return if completed != 0 {
                Ok(completed)
            } else {
                Err(error)
            };
        }
        completed += 1;
    }
    Ok(completed)
}

fn unregister_ring_fds(
    capability: &UserMemoryCapability,
    argument: u64,
    count: u32,
) -> AxResult<usize> {
    if count == 0 || count as usize > IO_RINGFD_REG_MAX {
        return Err(AxError::InvalidInput);
    }
    let task = current_task_context_id();
    // Linux has no per-task io_uring context to update, so unregister is a
    // zero-result no-op before it even touches the caller's update array.
    if !IO_URING_TASK_CONTEXTS.lock().contains(&task) {
        return Ok(0);
    }
    let base = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
    let mut completed = 0usize;
    for item in 0..usize::try_from(count).map_err(|_| AxError::InvalidInput)? {
        let address = base
            .checked_add(item.checked_mul(16).ok_or(AxError::BadAddress)?)
            .ok_or(AxError::BadAddress)?;
        let (offset, data) = match copied_ring_update(capability, address) {
            Ok(value) => value,
            Err(_) if completed != 0 => return Ok(completed),
            Err(error) => return Err(error),
        };
        if data != 0 || offset as usize >= IO_RINGFD_REG_MAX {
            return if completed != 0 {
                Ok(completed)
            } else {
                Err(AxError::InvalidInput)
            };
        }
        let mut tables = REGISTERED_RINGS.lock();
        // Linux treats an absent per-task table and an already-empty entry as
        // a successful no-op; neither condition revives a descriptor lookup.
        if let Some(table) = tables.get_mut(&task)
            && let Some(slot) = table.rings.get_mut(offset as usize)
        {
            slot.take();
        }
        completed += 1;
    }
    Ok(completed)
}

/// Called by exec and exact task teardown. The task identity is released
/// before its numeric TID can be reused, and no numeric fd lookup occurs
/// during teardown.
pub(crate) fn release_registered_ring_fds(task: u64) {
    IO_URING_TASK_CONTEXTS.lock().remove(&task);
    REGISTERED_RINGS.lock().remove(&task);
}

fn register_wait_region(
    capability: &UserMemoryCapability,
    ring: &IoUring,
    argument: u64,
) -> AxResult<()> {
    // The in/out descriptor becomes available only after its complete copy
    // succeeds in `prepare`; RefCell lets the later copyout closure consume
    // that exact owned image while the ring retains registration_serial.
    let copied_descriptor = RefCell::new(None);
    ring.register_wait_region_transaction(
        || {
            let address = usize::try_from(argument).map_err(|_| AxError::BadAddress)?;
            let header = capability
                .read_value_uninit(address as *const [u8; 32])
                .map_err(map_usercopy_error)?;
            // SAFETY: the complete fixed header was copied above.
            let header = unsafe { header.assume_init() };
            let desc_ptr = u64::from_ne_bytes(header[0..8].try_into().unwrap());
            let flags = u64::from_ne_bytes(header[8..16].try_into().unwrap());
            // Linux copies the descriptor after the outer registration record
            // but before validating either record's reserved fields/flags.
            let desc_address = usize::try_from(desc_ptr).map_err(|_| AxError::BadAddress)?;
            let desc = capability
                .read_value_uninit(desc_address as *const [u8; 64])
                .map_err(map_usercopy_error)?;
            // SAFETY: the explicit complete descriptor copy succeeded.
            let desc = unsafe { desc.assume_init() };
            if desc_ptr == 0 || flags & !1 != 0 || header[16..32].iter().any(|byte| *byte != 0) {
                return Err(AxError::InvalidInput);
            }
            let user_addr = u64::from_ne_bytes(desc[0..8].try_into().unwrap());
            let length = usize::try_from(u64::from_ne_bytes(desc[8..16].try_into().unwrap()))
                .map_err(|_| AxError::InvalidInput)?;
            let region_flags = u32::from_ne_bytes(desc[16..20].try_into().unwrap());
            let id = u32::from_ne_bytes(desc[20..24].try_into().unwrap());
            let mmap_offset = u64::from_ne_bytes(desc[24..32].try_into().unwrap());
            if region_flags & !1 != 0
                || id != 0
                || mmap_offset != 0
                || desc[32..64].iter().any(|byte| *byte != 0)
            {
                return Err(AxError::InvalidInput);
            }
            if length == 0 || length % PAGE_SIZE_4K != 0 {
                return Err(AxError::InvalidInput);
            }
            if length / PAGE_SIZE_4K > i32::MAX as usize {
                return Err(AxError::from(LinuxError::E2BIG));
            }
            if flags & 1 != 0 && length < 64 {
                return Err(AxError::InvalidInput);
            }
            let user_backing = if region_flags & 1 != 0 {
                let address = usize::try_from(user_addr).map_err(|_| AxError::BadAddress)?;
                // io_create_region treats a missing TYPE_USER address as the
                // user-memory pairing fault; only a nonzero misaligned
                // address is structural UAPI EINVAL.
                if address == 0 {
                    return Err(AxError::BadAddress);
                }
                if address % PAGE_SIZE_4K != 0 {
                    return Err(AxError::InvalidInput);
                }
                validate_mem_region_user_range(address, length)?;
                Some((
                    capability.clone(),
                    address,
                    crate::mm::pin_user_segments_from_user_longterm_with(
                        capability,
                        address as *const u8,
                        length,
                    )?,
                ))
            } else {
                if user_addr != 0 {
                    return Err(AxError::BadAddress);
                }
                None
            };
            copied_descriptor.replace(Some((desc_address, desc)));
            Ok((length, flags & 1 != 0, user_backing))
        },
        |id, mmap_offset| {
            let (desc_address, mut output) = copied_descriptor
                .borrow_mut()
                .take()
                .ok_or(AxError::BadState)?;
            output[20..24].copy_from_slice(&id.to_ne_bytes());
            output[24..32].copy_from_slice(&mmap_offset.to_ne_bytes());
            capability
                .write_bytes(desc_address, &output)
                .map_err(map_usercopy_error)
        },
    )
}

fn registered_extended_enter_argument(
    ring: &IoUring,
    offset: usize,
) -> AxResult<(IoUringGeteventsArg, ExtendedEnterWait)> {
    let raw = ring.copy_registered_wait(offset)?;
    // `io_uring_reg_wait` begins with a native timespec followed by the
    // fields represented by io_uring_getevents_arg. `IORING_REG_WAIT_TS`
    // controls whether the embedded timespec is active; reserved words are
    // checked before any wait can block.
    let flags = u32::from_ne_bytes(raw[20..24].try_into().unwrap());
    if flags & !1 != 0 {
        return Err(AxError::InvalidInput);
    }
    let mut argument = [0u8; IoUringGeteventsArg::BYTES];
    let sigmask = u64::from_ne_bytes(raw[24..32].try_into().unwrap());
    let sigmask_size = u32::from_ne_bytes(raw[32..36].try_into().unwrap());
    argument[0..8].copy_from_slice(&raw[24..32]);
    argument[8..12].copy_from_slice(&raw[32..36]);
    argument[12..16].copy_from_slice(&raw[16..20]);
    // The registered form embeds timespec by value. Keep an all-zero outer
    // timespec pointer: conversion to a Duration is handled separately by
    // the registered path before it blocks.
    let seconds = i64::from_ne_bytes(raw[0..8].try_into().unwrap());
    let nanos = i64::from_ne_bytes(raw[8..16].try_into().unwrap());
    if flags & 1 != 0 && (seconds < 0 || !(0..1_000_000_000).contains(&nanos)) {
        return Err(AxError::InvalidInput);
    }
    let argument = IoUringGeteventsArg::from_ne_bytes(argument);
    Ok((
        argument,
        ExtendedEnterWait {
            signal_mask: None,
            deferred_signal_mask: (sigmask != 0).then_some((sigmask, sigmask_size)),
            timeout: (flags & 1 != 0).then(|| Duration::new(seconds as u64, nanos as u32)),
            deferred_timeout: None,
            min_wait: Duration::from_micros(argument.min_wait_usec() as u64),
        },
    ))
}

fn registered_ring(index: i32) -> AxResult<Arc<IoUring>> {
    if index < 0 {
        return Err(AxError::InvalidInput);
    }
    let task = axtask::current().as_thread().kernel_tid() as u64;
    let tables = REGISTERED_RINGS.lock();
    let Some(table) = tables.get(&task) else {
        return Err(AxError::InvalidInput);
    };
    let Some(slot) = table.rings.get(index as usize) else {
        return Err(AxError::InvalidInput);
    };
    slot.clone().ok_or(AxError::BadFileDescriptor)
}

pub fn sys_io_uring_register(
    capability: UserMemoryCapability,
    fd: i32,
    opcode: u32,
    arg: usize,
    nr_args: u32,
) -> AxResult<isize> {
    let request = RegistrationRequest::new(opcode, arg as u64, nr_args);
    // Linux rejects opcodes beyond the fixed v6.18 enum before fd lookup.
    // For every recognized opcode, resolve the requested normal/registered
    // ring first so EBADF/EOPNOTSUPP outrank operation-body EINVAL/EFAULT.
    request.validate_envelope().map_err(map_policy_error)?;
    let ring = if request.use_registered_ring() {
        registered_ring(fd)?
    } else {
        ring_from_fd(fd)?.clone_object()
    };
    let operation = request.decode().map_err(map_policy_error)?;
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
        RegistrationOperation::RegisterEventFd { fd } => {
            ring.register_completion_eventfd(get_typed_file::<EventFd>(fd)?)?
        }
        RegistrationOperation::UnregisterEventFd => ring.unregister_completion_eventfd()?,
        RegistrationOperation::Probe {
            argument,
            operations,
        } => register_probe(&capability, argument, operations)?,
        RegistrationOperation::RegisterRingFds { argument, count } => {
            return Ok(register_ring_fds(&capability, argument, count)? as isize);
        }
        RegistrationOperation::UnregisterRingFds { argument, count } => {
            return Ok(unregister_ring_fds(&capability, argument, count)? as isize);
        }
        RegistrationOperation::RegisterMemRegion { argument } => {
            register_wait_region(&capability, ring.as_ref(), argument)?
        }
        RegistrationOperation::EnableRings => {
            ring.enable()?;
            if ring.sqpoll_enabled() {
                start_sqpoll_worker(ring.clone())?;
                ring.wake_sqpoll();
            }
        }
    }
    Ok(0)
}

fn submission_file(parsed: ParsedSubmission) -> Option<FileTarget> {
    match parsed.operation() {
        SubmissionOperation::Readv(request) | SubmissionOperation::Writev(request) => {
            Some(request.file())
        }
        SubmissionOperation::Read(request) | SubmissionOperation::Write(request) => {
            Some(request.file())
        }
        SubmissionOperation::Fsync(request) => Some(request.file()),
        SubmissionOperation::Fadvise(request) => Some(request.file()),
        SubmissionOperation::SyncFileRange(request) => Some(request.file()),
        SubmissionOperation::Fallocate(request) => Some(request.file()),
        SubmissionOperation::Shutdown(request) => Some(request.file()),
        SubmissionOperation::PollAdd(request) => Some(request.file()),
        SubmissionOperation::Accept(request) => Some(request.file()),
        SubmissionOperation::UringCmd(request) => Some(request.file()),
        SubmissionOperation::Nop
        | SubmissionOperation::OpenAt2(_)
        | SubmissionOperation::Close(_)
        | SubmissionOperation::Timeout(_)
        | SubmissionOperation::TimeoutRemove { .. }
        | SubmissionOperation::PollRemove { .. }
        | SubmissionOperation::AsyncCancel { .. }
        | SubmissionOperation::ProvideBuffers(_)
        | SubmissionOperation::RemoveBuffers(_) => None,
    }
}

fn submission_buffer(parsed: ParsedSubmission) -> Option<(BufferSlot, u64, u32)> {
    match parsed.operation() {
        SubmissionOperation::Read(request) | SubmissionOperation::Write(request) => request
            .fixed_buffer()
            .map(|slot| (slot, request.buffer().address(), request.buffer().length())),
        SubmissionOperation::Nop
        | SubmissionOperation::OpenAt2(_)
        | SubmissionOperation::Readv(_)
        | SubmissionOperation::Writev(_)
        | SubmissionOperation::Fsync(_)
        | SubmissionOperation::Close(_)
        | SubmissionOperation::Fadvise(_)
        | SubmissionOperation::SyncFileRange(_)
        | SubmissionOperation::Fallocate(_)
        | SubmissionOperation::Shutdown(_)
        | SubmissionOperation::PollAdd(_)
        | SubmissionOperation::Timeout(_)
        | SubmissionOperation::TimeoutRemove { .. }
        | SubmissionOperation::PollRemove { .. }
        | SubmissionOperation::AsyncCancel { .. }
        | SubmissionOperation::ProvideBuffers(_)
        | SubmissionOperation::RemoveBuffers(_)
        | SubmissionOperation::Accept(_)
        | SubmissionOperation::UringCmd(_) => None,
    }
}

fn submission_provided_buffer(parsed: ParsedSubmission) -> Option<u16> {
    match parsed.operation() {
        // A multishot receive acquires a fresh supplied buffer only when a
        // readiness shot actually runs.  Leasing one at SQ admission would
        // pin it indefinitely and make the group spuriously empty.
        SubmissionOperation::Read(request) if !request.multishot() => {
            request.provided_buffer_group()
        }
        _ => None,
    }
}

/// Copy the whole OPENAT2 ABI input while the submitter's address space is
/// current. The future executor owns only this value and its actor snapshot.
fn copy_openat2_submission(
    actor: &IoUringSubmissionActor,
    parsed: ParsedSubmission,
) -> AxResult<Option<IoUringOpenAt2Work>> {
    let SubmissionOperation::OpenAt2(request) = parsed.operation() else {
        return Ok(None);
    };
    let path = usize::try_from(request.path_address()).map_err(|_| AxError::BadAddress)?;
    let how = usize::try_from(request.how_address()).map_err(|_| AxError::BadAddress)?;
    let (path, how, flags) = super::fd_ops::copy_openat2_input(
        actor.memory(),
        path as *const core::ffi::c_char,
        how as *const u8,
        request.how_size() as usize,
    )?;
    super::fd_ops::validate_openat2_copied_path(&path, &how)?;
    // An absolute path ignores dirfd unless IN_ROOT makes that fd the new
    // root. Do not turn an otherwise ignored invalid descriptor into an
    // early EBADF while accepting the SQE.
    let dirfd_is_used = !path.is_absolute() || how.resolve & RESOLVE_IN_ROOT as u64 != 0;
    let dirfd = if request.dirfd() == AT_FDCWD || !dirfd_is_used {
        None
    } else {
        Some(actor.files().get_description(request.dirfd())?)
    };
    Ok(Some(IoUringOpenAt2Work {
        path,
        how,
        flags,
        dirfd,
        actor: actor.clone(),
    }))
}

/// Copy the `PROVIDE_BUFFERS` geometry into owned ring metadata.  The group
/// keeps the submitting address-space capability, but each range is checked
/// before the group changes so later BUFFER_SELECT never converts a malformed
/// provision operation into a deferred usercopy fault.
fn copy_provided_buffers(
    capability: &UserMemoryCapability,
    request: thekernel_linux_io_uring::ProvideBuffersRequest,
) -> AxResult<Vec<(usize, usize, UserMemoryCapability)>> {
    let base = usize::try_from(request.address()).map_err(|_| AxError::BadAddress)?;
    let length = usize::try_from(request.length()).map_err(|_| AxError::InvalidInput)?;
    let count = usize::from(request.count());
    let bytes = length.checked_mul(count).ok_or(AxError::BadAddress)?;
    base.checked_add(bytes).ok_or(AxError::BadAddress)?;
    let mut buffers = Vec::new();
    buffers
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..count {
        let offset = index.checked_mul(length).ok_or(AxError::BadAddress)?;
        let address = base.checked_add(offset).ok_or(AxError::BadAddress)?;
        check_user_writable_with(capability, address, length)?;
        buffers.push((address, length, capability.clone()));
    }
    Ok(buffers)
}

fn retain_submission_file(
    ring: &IoUring,
    actor: &IoUringSubmissionActor,
    target: FileTarget,
) -> AxResult<IoUringFileLease> {
    match target {
        FileTarget::Descriptor(fd) => {
            let fd = i32::try_from(fd).map_err(|_| AxError::BadFileDescriptor)?;
            ring.retain_descriptor(actor.files().get_description(fd)?)
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
    let (address, length) = if request.uses_ring_buffer() {
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

/// Performs the generic owned-file preparation while SQ admission still owns
/// the exact file lease, frozen actor/status, and submitting address space.
/// A provider's explicit pre-publication refusal is the only fallback signal.
fn prepare_owned_submission(
    ring: &IoUring,
    id: thekernel_linux_io_uring::RequestId,
    capability: UserMemoryCapability,
    description: &Arc<FileDescription>,
    context: IoOperationContext,
    operation: SubmissionOperation,
    buffer: Option<&IoUringBufferLease>,
) -> Result<Option<axfs::PreparedOwnedFileIo>, AxError> {
    let operation = match operation {
        SubmissionOperation::Read(request) => {
            let write = false;
            let (address, length) = submission_io_range(request, buffer)?;
            if length == 0 {
                return Ok(None);
            }
            let file = description.file_handle().downcast::<File>()?;
            let context = context
                .with_status(rwf_status(context.status(), request.rw_flags(), write)?)
                .with_rwf_flags(request.rw_flags());
            let operation = if write {
                ClassicAioOperation::Write {
                    capability,
                    file,
                    context,
                    buf: address,
                    len: length,
                    offset: request.offset(),
                    ioprio: 0,
                }
            } else {
                ClassicAioOperation::Read {
                    capability,
                    file,
                    context,
                    buf: address,
                    len: length,
                    offset: request.offset(),
                    ioprio: 0,
                }
            };
            operation
        }
        SubmissionOperation::Write(request) => {
            let write = true;
            let (address, length) = submission_io_range(request, buffer)?;
            if length == 0 {
                return Ok(None);
            }
            let file = description.file_handle().downcast::<File>()?;
            let context = context
                .with_status(rwf_status(context.status(), request.rw_flags(), write)?)
                .with_rwf_flags(request.rw_flags());
            ClassicAioOperation::Write {
                capability,
                file,
                context,
                buf: address,
                len: length,
                offset: request.offset(),
                ioprio: 0,
            }
        }
        SubmissionOperation::Readv(request) => {
            let write = false;
            let context = context
                .with_status(rwf_status(context.status(), request.rw_flags(), write)?)
                .with_rwf_flags(request.rw_flags());
            let operation = prepare_io_uring_vectored_operation(
                capability,
                description,
                context,
                write,
                usize::try_from(request.iov_address()).map_err(|_| AxError::BadAddress)?,
                usize::try_from(request.iov_count()).map_err(|_| AxError::InvalidInput)?,
                request.offset(),
            )?;
            operation
        }
        SubmissionOperation::Writev(request) => {
            let write = true;
            let context = context
                .with_status(rwf_status(context.status(), request.rw_flags(), write)?)
                .with_rwf_flags(request.rw_flags());
            prepare_io_uring_vectored_operation(
                capability,
                description,
                context,
                write,
                usize::try_from(request.iov_address()).map_err(|_| AxError::BadAddress)?,
                usize::try_from(request.iov_count()).map_err(|_| AxError::InvalidInput)?,
                request.offset(),
            )?
        }
        _ => return Ok(None),
    };
    let completion: Box<dyn OwnedFileIoCompletion> = Box::new(OwnedFileIoCompletionBridge {
        ring: ring.weak_owner(),
        id,
    });
    match prepare_classic_aio_owned_operation(&operation, completion) {
        Ok(super::io::ClassicAioOwnedPreparation::Prepared(prepared)) => Ok(Some(prepared)),
        Ok(super::io::ClassicAioOwnedPreparation::Zero) => Ok(None),
        Ok(super::io::ClassicAioOwnedPreparation::Unsupported) => {
            Err(AxError::OperationNotSupported)
        }
        Err((error, _)) => Err(error),
    }
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
    cancellation_mode: Option<thekernel_linux_io_uring::CancellationMode>,
) -> AxResult<Option<thekernel_linux_io_uring::IssuedRequest>> {
    let id = prepared.id();
    let issued = match cancellation_mode {
        Some(mode) => ring.issue_request_with_cancellation_mode(prepared, mode),
        None => ring.issue_request(prepared),
    };
    match issued {
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

/// Native `__kernel_timespec` used only for the one stable copy performed at
/// timeout SQE admission.  Keeping it byte-decoded avoids retaining a
/// userspace pointer in a request which may outlive the submitting task.
fn copy_timeout_duration(capability: &UserMemoryCapability, address: u64) -> AxResult<Duration> {
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    let bytes = capability
        .read_value_uninit(address as *const [u8; 16])
        .map_err(map_usercopy_error)?;
    // SAFETY: the capability initialized all sixteen bytes before success.
    let bytes = unsafe { bytes.assume_init() };
    let seconds = i64::from_ne_bytes(bytes[..8].try_into().unwrap());
    let nanos = i64::from_ne_bytes(bytes[8..].try_into().unwrap());
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Err(AxError::InvalidInput);
    }
    Ok(Duration::new(seconds as u64, nanos as u32))
}

fn spawn_timeout(
    ring: &IoUring,
    issued: thekernel_linux_io_uring::IssuedRequest,
    duration: Duration,
) -> AxResult<()> {
    let owner = ring.arc_owner()?;
    let mut name = alloc::string::String::new();
    name.try_reserve_exact(16).map_err(|_| AxError::NoMemory)?;
    name.push_str("io-uring-timeout");
    axtask::try_spawn_with_name(
        move || {
            // A cancellation claims the request-table terminal transition
            // while this task sleeps.  `complete_issued` then observes the
            // stale issued proof and deliberately cannot publish a second CQE.
            let _ = future::block_on(future::sleep(duration));
            let _ = owner.complete_issued(
                issued,
                TerminalCause::Completed,
                -LinuxError::ETIME.code(),
                0,
            );
        },
        name,
    )
    .map(|_| ())
}

/// Starts the ring-local SQ poll owner only after its explicit setup actor
/// has been installed.  The worker never consults its own task state: files,
/// userspace memory, credentials, and fanotify identity all flow through the
/// retained actor to `submit_entries`.
fn start_sqpoll_worker(ring: Arc<IoUring>) -> AxResult<()> {
    let sq_aff = ring
        .layout()
        .setup_flags()
        .contains(thekernel_linux_io_uring::SetupFlags::SQ_AFF);
    if sq_aff
        && usize::try_from(ring.layout().sq_thread_cpu())
            .ok()
            .is_none_or(|cpu| cpu >= axhal::cpu_num())
    {
        return Err(AxError::InvalidInput);
    }
    let idle_interval = Duration::from_millis(u64::from(if ring.layout().sq_thread_idle() == 0 {
        2_000
    } else {
        ring.layout().sq_thread_idle()
    }));
    let mut name = String::new();
    name.try_reserve_exact("io-uring-sqpoll".len())
        .map_err(|_| AxError::NoMemory)?;
    name.push_str("io-uring-sqpoll");
    axtask::try_spawn_with_name(
        move || {
            let cpu = usize::try_from(ring.layout().sq_thread_cpu()).unwrap_or(0);
            if sq_aff {
                let mut affinity = axtask::AxCpuMask::new();
                affinity.set(cpu, true);
                if let Err(error) = axtask::set_current_affinity(affinity) {
                    error!("io_uring SQPOLL worker CPU {cpu} affinity bind failed: {error:?}");
                    ring.fail_sqpoll();
                    ring.mark_sqpoll_stopped();
                    return;
                }
                debug_assert_eq!(axhal::percpu::this_cpu_id(), cpu);
            }
            if ring.mark_sqpoll_started().is_err() {
                ring.fail_sqpoll();
                return;
            }
            while !ring.sqpoll_should_stop() {
                // CPU-offline migration is terminal for a pinned SQPOLL
                // owner.  Falling back to another CPU would silently violate
                // the requested polling/latency contract.
                if sq_aff && axhal::percpu::this_cpu_id() != cpu {
                    ring.fail_sqpoll();
                    break;
                }
                let actor = match ring.sqpoll_actor() {
                    Ok(actor) => actor,
                    Err(_) => break,
                };
                // One bounded ring pass provides fairness among SQPOLL
                // rings.  Every accepted entry retains descriptor and MM
                // ownership before the worker can sleep again.
                let progress =
                    match submit_entries(ring.as_ref(), ring.layout().sq_entries(), &actor) {
                        Ok((submitted, _)) => submitted,
                        Err(error) => {
                            error!("io_uring SQPOLL worker stopped submission pass: {error:?}");
                            0
                        }
                    };
                if progress == 0 {
                    // SQPOLL actively samples direct shared-tail stores for
                    // its requested idle interval.  Only after that window
                    // does it expose NEED_WAKEUP and block for SQ_WAKEUP.
                    let deadline = crate::time::wall_time().saturating_add(idle_interval);
                    while !ring.sqpoll_should_stop()
                        && crate::time::wall_time() < deadline
                        && !ring.sqpoll_has_submissions().unwrap_or(true)
                    {
                        axtask::yield_now();
                    }
                    if !ring.sqpoll_should_stop() && !ring.sqpoll_has_submissions().unwrap_or(true)
                    {
                        // After the configured active polling interval, an
                        // SQ_WAKEUP/close edge is the blocking wake source.
                        // The predicate also catches a tail advanced in the
                        // narrow interval before the waiter registered.
                        let _ = ring.wait_sqpoll_wakeup();
                    }
                }
            }
            ring.mark_sqpoll_stopped();
        },
        name,
    )
    .map(|_| ())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubmissionOutcome {
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
    let (
        prepared,
        parsed,
        file,
        mut buffer,
        mut context,
        physical,
        admission_error,
        openat2,
        capability,
        owned,
    ) = work.into_parts();
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            complete_preparation_failure(ring, prepared, negative_policy_errno(error))?;
            return Ok(SubmissionOutcome::FailedDuringSubmission);
        }
    };
    let id = prepared.id();
    let command_cancellation = if let SubmissionOperation::UringCmd(request) = parsed.operation() {
        file.as_ref()
            .and_then(|lease| lease.description().ok())
            .map(|description| description.file_handle())
            .and_then(|object| {
                object
                    .uring_cmd_manifest()
                    .iter()
                    .copied()
                    .find(|entry| entry.command() == request.command())
            })
            .map(|manifest| {
                if manifest.cancellable() {
                    thekernel_linux_io_uring::CancellationMode::Cancellable
                } else {
                    thekernel_linux_io_uring::CancellationMode::Uncancellable
                }
            })
    } else {
        None
    };
    // A generic owned provider request retains an explicit consuming cancel
    // control.  Reads and writes use the same cancellable registry mode;
    // without this override WRITE/WRITEV would be invisible to ASYNC_CANCEL
    // and final close despite having a provider-owned long-term pin.
    let cancellation_mode = if owned.is_some() {
        Some(thekernel_linux_io_uring::CancellationMode::Cancellable)
    } else {
        command_cancellation
    };
    let Some(issued) = issue_prepared(ring, prepared, cancellation_mode)? else {
        return Ok(SubmissionOutcome::Accepted);
    };

    if let Some(error) = admission_error {
        drop((issued, file, buffer, context, physical, owned));
        ring.complete_request(id, TerminalCause::Completed, negative_errno(error), 0)?;
        return Ok(SubmissionOutcome::Accepted);
    }

    if let Some(owned) = owned {
        drop((file, context, physical));
        if owned.is_nowait() {
            // NOWAIT is an immediate-only provider contract: it must never
            // publish a queue-owned request which could later block.
            return match owned.try_complete_immediate() {
                Ok(result) => {
                    let result = match result {
                        ImmediateFileIoResult::Completed(bytes) => {
                            i32::try_from(bytes).unwrap_or(-LinuxError::EOVERFLOW.code())
                        }
                        ImmediateFileIoResult::Cancelled => -LinuxError::ECANCELED.code(),
                        ImmediateFileIoResult::Failed(error) => -LinuxError::from(error).code(),
                    };
                    ring.complete_owned_immediate(issued, result, buffer)
                        .map(|_| SubmissionOutcome::Accepted)
                }
                Err(error) => {
                    let FileIoPrepareError {
                        error,
                        request,
                        completion,
                    } = error;
                    drop((request, completion));
                    ring.complete_owned_immediate(issued, negative_errno(error), buffer)
                        .map(|_| SubmissionOutcome::Accepted)
                }
            };
        }
        return ring
            .publish_owned_file_io(issued, owned, buffer)
            .map(|_| SubmissionOutcome::Accepted)
            .map_err(|(error, prepared, buffer)| {
                drop((prepared, buffer));
                error
            });
    }

    match parsed.operation() {
        SubmissionOperation::Nop => {
            drop(issued);
            drop((file, buffer));
            ring.complete_request(id, TerminalCause::Completed, 0, 0)
        }
        SubmissionOperation::OpenAt2(request) => {
            let result = match openat2 {
                Some(work) => {
                    let files = super::fd_ops::OpenFdSnapshot::new(
                        work.actor.files().clone(),
                        work.actor.nofile_limit(),
                    );
                    super::fd_ops::openat2_copied_with_snapshot(
                        &files,
                        work.actor.path_snapshot(),
                        request.dirfd(),
                        work.dirfd.as_ref(),
                        work.actor.fanotify_actor(),
                        work.path,
                        work.how,
                        work.flags,
                    )
                }
                None => Err(AxError::BadState),
            };
            drop((file, buffer, context, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::ProvideBuffers(request) => {
            let result = copy_provided_buffers(&capability, request).and_then(|buffers| {
                ring.provide_buffers(request.group(), request.first_id(), buffers)
            });
            drop((issued, file, buffer, context, physical));
            ring.complete_request(
                id,
                TerminalCause::Completed,
                io_result(result.map(|_| 0_isize)),
                0,
            )
        }
        SubmissionOperation::RemoveBuffers(request) => {
            let result = ring
                .remove_buffers(request.group(), usize::from(request.count()))
                .and_then(|removed| isize::try_from(removed).map_err(|_| AxError::OutOfRange));
            drop((issued, file, buffer, context, physical));
            ring.complete_request(id, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::UringCmd(request) => {
            let Some(file) = file else {
                drop((buffer, context, physical));
                ring.complete_issued(
                    issued,
                    TerminalCause::PreparationFailed,
                    -LinuxError::EBADF.code(),
                    0,
                )?;
                return Ok(SubmissionOutcome::Accepted);
            };
            let object = match file.description() {
                Ok(description) => description.file_handle(),
                Err(error) => {
                    drop((file, buffer, context, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            negative_errno(error),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                }
            };
            let Some(manifest) = object
                .uring_cmd_manifest()
                .iter()
                .copied()
                .find(|entry| entry.command() == request.command())
            else {
                drop((file, buffer, context, physical));
                return ring
                    .complete_issued(
                        issued,
                        TerminalCause::PreparationFailed,
                        -LinuxError::EOPNOTSUPP.code(),
                        0,
                    )
                    .map(|_| SubmissionOutcome::Accepted);
            };
            if !manifest.accepts_flags(request.flags())
                || (ring.iopoll_enabled() && !manifest.iopoll())
            {
                let error = if manifest.accepts_flags(request.flags()) {
                    -LinuxError::EOPNOTSUPP.code()
                } else {
                    -LinuxError::EINVAL.code()
                };
                drop((file, buffer, context, physical));
                return ring
                    .complete_issued(issued, TerminalCause::PreparationFailed, error, 0)
                    .map(|_| SubmissionOutcome::Accepted);
            }
            let iopoll = ring.iopoll_enabled();
            if let Err(error) = ring.register_uring_cmd(&issued, object.clone(), iopoll) {
                drop((file, buffer, context, physical));
                return ring
                    .complete_issued(
                        issued,
                        TerminalCause::PreparationFailed,
                        negative_errno(error),
                        0,
                    )
                    .map(|_| SubmissionOutcome::Accepted);
            }
            if !ring.begin_uring_cmd_handoff(issued.id())? {
                // Cancellation/close already owns the terminal credit.  The
                // generation-bound registry entry is only admission custody;
                // remove it without manufacturing a second completion.
                ring.unregister_iopoll_uring_cmd(issued.id());
                drop((file, buffer, context, physical));
                drop(issued);
                return Ok(SubmissionOutcome::Accepted);
            }
            let completion = ring.uring_cmd_completion(issued, Some(file), iopoll);
            drop((buffer, context, physical));
            match object.submit_uring_cmd(
                UringCmd::new(request.command(), request.flags(), request.payload()),
                completion,
            ) {
                Ok(()) => {
                    if let Some(provider) = ring.finish_uring_cmd_handoff(id)? {
                        provider.cancel_uring_cmd(id);
                    }
                    Ok(())
                }
                Err((error, completion)) => completion.fail(error),
            }
        }
        SubmissionOperation::Timeout(request) => {
            let duration = copy_timeout_duration(&capability, request.timespec_address());
            // A spawn failure leaves no executor behind; the request-table
            // identity is therefore completed synchronously below.  A
            // successful spawn owns the issued proof until timeout/cancel.
            match duration {
                Ok(duration) => match spawn_timeout(ring, issued, duration) {
                    Ok(()) => Ok(()),
                    Err(error) => ring.complete_request(
                        id,
                        TerminalCause::PreparationFailed,
                        negative_errno(error),
                        0,
                    ),
                },
                Err(error) => ring.complete_request(
                    id,
                    TerminalCause::PreparationFailed,
                    negative_errno(error),
                    0,
                ),
            }
        }
        SubmissionOperation::Readv(request) | SubmissionOperation::Writev(request) => {
            let write = matches!(parsed.operation(), SubmissionOperation::Writev(_));
            let result = file
                .as_ref()
                .ok_or(AxError::BadFileDescriptor)
                .and_then(|lease| lease.description())
                .and_then(|description| {
                    let context = context.take().ok_or(AxError::BadState)?;
                    let address =
                        usize::try_from(request.iov_address()).map_err(|_| AxError::BadAddress)?;
                    let count =
                        usize::try_from(request.iov_count()).map_err(|_| AxError::InvalidInput)?;
                    prepare_io_uring_vectored_operation(
                        capability.clone(),
                        &description,
                        context,
                        write,
                        address,
                        count,
                        request.offset(),
                    )
                    .and_then(execute_classic_aio_operation)
                });
            drop((file, buffer, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::Fsync(request) => {
            // The lease was acquired before SQ-head publication, so this
            // operates on the submitted open-file description even if its fd
            // number is closed and reused before completion.
            let result = file
                .as_ref()
                .ok_or(AxError::BadFileDescriptor)
                .and_then(|lease| lease.description())
                .and_then(|description| description.sync(request.datasync()));
            drop((file, buffer, context, physical));
            ring.complete_issued(
                issued,
                TerminalCause::Completed,
                io_result(result.map(|_| 0)),
                0,
            )
        }
        SubmissionOperation::Close(request) => {
            // Close consumes exactly the descriptor copied in the SQE.  It
            // intentionally has no retained lease: keeping a lease here
            // would turn the close into a delayed final-close and alter the
            // fd-table visibility observed by concurrent threads.
            let result = if ring.sqpoll_enabled() {
                // SQPOLL executes in a kernel worker, so `current()` names
                // neither the submitting files_struct nor its POSIX-lock
                // owner. The retained actor linearizes both against setup's
                // exact process instead.
                ring.sqpoll_actor()
                    .and_then(|actor| {
                        actor
                            .files()
                            .close_for_process(request.fd(), actor.process_id())
                    })
                    .map(|_| 0)
            } else {
                crate::file::close_file_like(request.fd()).and_then(|_| {
                    crate::file::inotify::wait_current_close_notifications();
                    Ok(0)
                })
            };
            drop((file, buffer, context, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::Fadvise(request) => {
            let result = file
                .as_ref()
                .ok_or(AxError::BadFileDescriptor)
                .and_then(|lease| lease.description())
                .and_then(|description| {
                    super::io::fadvise_file_like(
                        &description.file_handle(),
                        request.offset(),
                        request.length(),
                        request.advice(),
                    )
                });
            drop((file, buffer, context, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::SyncFileRange(request) => {
            let result = file
                .as_ref()
                .ok_or(AxError::BadFileDescriptor)
                .and_then(|lease| lease.description())
                .and_then(|description| {
                    super::io::sync_file_range_description(
                        &description,
                        request.offset(),
                        request.length(),
                        request.flags(),
                    )
                });
            drop((file, buffer, context, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::Fallocate(request) => {
            let result = file
                .as_ref()
                .ok_or(AxError::BadFileDescriptor)
                .and_then(|lease| lease.description())
                .and_then(|description| {
                    let context = context.as_ref().ok_or(AxError::BadState)?;
                    super::io::fallocate_file_like(
                        &description.file_handle(),
                        context.security(),
                        request.mode(),
                        request.offset(),
                        request.length(),
                    )
                });
            drop((file, buffer, context, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::Shutdown(request) => {
            let result = file
                .as_ref()
                .ok_or(AxError::BadFileDescriptor)
                .and_then(|lease| lease.description())
                .and_then(|description| {
                    let context = context.as_ref().ok_or(AxError::BadState)?;
                    let socket = crate::file::PinnedSocketDescription::from_description(
                        description.clone(),
                    )?;
                    crate::syscall::shutdown_pinned_socket(
                        &socket,
                        context.security().actor_arc(),
                        request.how(),
                    )
                });
            drop((file, buffer, context, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::Accept(request) => {
            if request.multishot() {
                let Some(file) = file else {
                    drop((buffer, context, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::EBADF.code(),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                };
                let Some(context) = context else {
                    drop((file, buffer, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::EIO.code(),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                };
                // Validate the retained target before ownership crosses into
                // the readiness path; no later numeric descriptor lookup is
                // permitted for an ACCEPT multishot.
                if file
                    .description()
                    .and_then(|description| {
                        crate::file::PinnedSocketDescription::from_description(description.clone())
                    })
                    .is_err()
                {
                    drop((file, buffer, context, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::ENOTSOCK.code(),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                }
                drop((buffer, physical));
                return ring
                    .admit_socket_multishot(
                        issued,
                        file,
                        context,
                        true,
                        request.flags(),
                        None,
                        None,
                        capability,
                    )
                    .map(|_| SubmissionOutcome::Accepted);
            }
            let result = file
                .as_ref()
                .ok_or(AxError::BadFileDescriptor)
                .and_then(|lease| lease.description())
                .and_then(|description| {
                    let context = context.as_ref().ok_or(AxError::BadState)?;
                    let socket = crate::file::PinnedSocketDescription::from_description(
                        description.clone(),
                    )?;
                    crate::syscall::accept_pinned(
                        &socket,
                        context.security().actor_arc(),
                        request.flags(),
                    )
                });
            drop((file, buffer, context, physical));
            ring.complete_issued(issued, TerminalCause::Completed, io_result(result), 0)
        }
        SubmissionOperation::Read(request) => {
            if request.multishot() {
                let Some(file) = file else {
                    drop((buffer, context, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::EBADF.code(),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                };
                let Some(context) = context else {
                    drop((file, buffer, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::EIO.code(),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                };
                let Some(group) = request.provided_buffer_group() else {
                    drop((file, buffer, context, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::EINVAL.code(),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                };
                if file
                    .description()
                    .and_then(|description| {
                        crate::file::PinnedSocketDescription::from_description(description.clone())
                    })
                    .is_err()
                {
                    drop((file, buffer, context, physical));
                    return ring
                        .complete_issued(
                            issued,
                            TerminalCause::PreparationFailed,
                            -LinuxError::ENOTSOCK.code(),
                            0,
                        )
                        .map(|_| SubmissionOutcome::Accepted);
                }
                drop((buffer, physical));
                return ring
                    .admit_socket_multishot(
                        issued,
                        file,
                        context,
                        false,
                        0,
                        Some(request),
                        Some(group),
                        capability,
                    )
                    .map(|_| SubmissionOutcome::Accepted);
            }
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
            if request.uses_ring_buffer() && buffer.is_none() {
                drop(file);
                ring.complete_issued(
                    issued,
                    TerminalCause::PreparationFailed,
                    -LinuxError::EFAULT.code(),
                    0,
                )?;
                return Ok(SubmissionOutcome::Accepted);
            }
            let io_capability =
                match submission_io_capability(&capability, request.uses_ring_buffer(), || {
                    buffer.as_ref().ok_or(AxError::BadAddress)?.capability()
                }) {
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
            // A provided-buffer completion identifies the exact leased slot;
            // dropping the lease after publication returns that slot to its
            // group, while the CQE is the user-visible hand-off point.
            const IORING_CQE_F_BUFFER: u32 = 1;
            let flags = buffer
                .as_ref()
                .and_then(IoUringBufferLease::provided_id)
                .map(|id| IORING_CQE_F_BUFFER | (u32::from(id) << 16))
                .unwrap_or(0);
            let completed = ring.complete_issued_with_claim_hook(
                issued,
                TerminalCause::Completed,
                result,
                flags,
                || {
                    if flags & IORING_CQE_F_BUFFER != 0 {
                        if let Some(buffer) = buffer.as_mut() {
                            buffer.consume_provided();
                        }
                    }
                },
            );
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
        SubmissionOperation::TimeoutRemove { target_user_data } => {
            drop((file, buffer));
            ring.cancel_timeout_request(issued, target_user_data)
        }
        SubmissionOperation::PollRemove { target_user_data } => {
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

/// Runs a work item released by a predecessor terminal transition.  This is
/// intentionally callable from the ring core only after it has dropped the
/// state lock; execution can synchronously complete and release another edge.
pub(crate) fn dispatch_dependency_submission(
    ring: &IoUring,
    dispatch: DependencyDispatch,
) -> AxResult<SubmissionOutcome> {
    match dispatch {
        DependencyDispatch::Execute(work) => execute_submission(ring, work),
        DependencyDispatch::Cancelled(work) => {
            complete_preparation_failure(ring, work.into_parts().0, -LinuxError::ECANCELED.code())?;
            Ok(SubmissionOutcome::Accepted)
        }
        DependencyDispatch::Parked => Ok(SubmissionOutcome::Accepted),
    }
}

fn submit_entries(
    ring: &IoUring,
    requested: u32,
    actor: &IoUringSubmissionActor,
) -> AxResult<(u32, bool)> {
    if ring.disabled() {
        return Err(AxError::from(LinuxError::EBADFD));
    }
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
                let openat2 = match parsed {
                    Ok(parsed) => match copy_openat2_submission(actor, parsed) {
                        Ok(work) => work,
                        Err(error) => {
                            let work = match admission.commit(
                                None,
                                None,
                                None,
                                None,
                                None,
                                actor.memory().clone(),
                            ) {
                                Ok(work) => work,
                                Err(_) if submitted != 0 => {
                                    return Ok((submitted, false));
                                }
                                Err(commit_error) => return Err(commit_error),
                            };
                            submitted += 1;
                            let (prepared, ..) = work.into_parts();
                            complete_preparation_failure(ring, prepared, negative_errno(error))?;
                            continue;
                        }
                    },
                    Err(_) => None,
                };
                let mut lease = match parsed.ok().and_then(submission_file) {
                    Some(target) => match retain_submission_file(ring, actor, target) {
                        Ok(lease) => Some(lease),
                        Err(error) => {
                            let work = match admission.commit(
                                None,
                                None,
                                None,
                                None,
                                None,
                                actor.memory().clone(),
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
                                actor.memory().clone(),
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

                if buffer.is_none()
                    && let Some(group) = parsed.ok().and_then(submission_provided_buffer)
                {
                    match ring.acquire_provided_buffer(group) {
                        Ok(lease) => buffer = Some(lease),
                        Err(error) => {
                            drop(lease);
                            let work = match admission.commit(
                                None,
                                None,
                                None,
                                None,
                                None,
                                actor.memory().clone(),
                            ) {
                                Ok(work) => work,
                                Err(commit_error) if submitted != 0 => {
                                    error!(
                                        "io_uring stopped before committing provided-buffer \
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
                                    "io_uring failed to publish provided-buffer error after \
                                     acceptance: {completion_error:?}"
                                );
                                return Ok((submitted, false));
                            }
                            continue;
                        }
                    }
                }

                // Capture the immutable actor/OFD snapshot while the SQE is
                // still being admitted.  The retained lease supplies the
                // exact description; execution must not recreate this from
                // whichever task happens to drain a future work item.
                let mut context = match parsed.ok().map(ParsedSubmission::operation) {
                    Some(SubmissionOperation::Read(_))
                    | Some(SubmissionOperation::Write(_))
                    | Some(SubmissionOperation::Readv(_))
                    | Some(SubmissionOperation::Writev(_))
                    | Some(SubmissionOperation::Fallocate(_))
                    | Some(SubmissionOperation::Shutdown(_))
                    | Some(SubmissionOperation::Accept(_)) => lease
                        .as_ref()
                        .map(|lease| {
                            lease.description().map(|description| {
                                capture_io_operation_context_for_actor(
                                    description,
                                    actor.security().clone(),
                                    actor.fanotify_actor(),
                                )
                            })
                        })
                        .transpose()?,
                    _ => None,
                };

                // Only a fixed-buffer regular ext4 O_DIRECT request can
                // receive an owned worker token. Policy failures are carried
                // into the accepted CQE so execution does not repeat a
                // fanotify wait or RLIMIT_FSIZE signal.
                let mut physical = None;
                let mut owned = None;
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
                                                        let mutation = if plan.operation()
                                                            == PreparedPhysicalIoOperation::Write
                                                        {
                                                            file_lease
                                                                .description()?
                                                                .inner
                                                                .downcast_ref::<File>()
                                                                .map(|file| {
                                                                    crate::mm::admit_mutation(
                                                                        file.inner().location(),
                                                                    )
                                                                })
                                                                .transpose()?
                                                        } else {
                                                            None
                                                        };
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
                                                                mutation,
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

                // Generic owned I/O pins before SQE publication.  IOPOLL has
                // no generic provider-harvest contract, so reject before
                // pinning instead of accepting a request we cannot retire.
                if physical.is_none()
                    && admission_error.is_none()
                    && let Ok(parsed) = parsed
                    && match parsed.operation() {
                        SubmissionOperation::Read(request) => !request.multishot(),
                        SubmissionOperation::Write(_)
                        | SubmissionOperation::Readv(_)
                        | SubmissionOperation::Writev(_) => true,
                        _ => false,
                    }
                {
                    if ring.iopoll_enabled() {
                        admission_error = Some(AxError::OperationNotSupported);
                    } else if let (Some(file_lease), Some(context_ref)) =
                        (lease.as_ref(), context.as_ref())
                    {
                        match file_lease.description().and_then(|description| {
                            prepare_owned_submission(
                                ring,
                                admission.id()?,
                                actor.memory().clone(),
                                &description,
                                context_ref.clone(),
                                parsed.operation(),
                                buffer.as_ref(),
                            )
                        }) {
                            Ok(prepared) => owned = prepared,
                            // Only an explicit pre-publication provider
                            // refusal selects the legacy borrowed route.
                            Err(AxError::OperationNotSupported) => {}
                            Err(error) => admission_error = Some(error),
                        }
                    }
                }

                // IOPOLL is a provider completion contract, not a request
                // scheduling hint.  Never execute an ordinary buffered,
                // socket, or metadata operation synchronously on an IOPOLL
                // ring: it must have reached the bounded physical provider
                // path above, otherwise its accepted SQE receives the normal
                // per-operation EOPNOTSUPP CQE.
                let iopoll_uring_cmd = parsed.ok().is_some_and(|parsed| {
                    let SubmissionOperation::UringCmd(command) = parsed.operation() else {
                        return false;
                    };
                    lease
                        .as_ref()
                        .and_then(|lease| lease.description().ok())
                        .is_some_and(|description| {
                            description
                                .file_handle()
                                .uring_cmd_manifest()
                                .iter()
                                .any(|entry| entry.command() == command.command() && entry.iopoll())
                        })
                });
                if ring.iopoll_enabled()
                    && physical.is_none()
                    && !iopoll_uring_cmd
                    && admission_error.is_none()
                {
                    admission_error = Some(AxError::OperationNotSupported);
                }

                if !ring.iopoll_enabled()
                    && let Ok(parsed) = parsed
                    && let SubmissionOperation::PollAdd(request) = parsed.operation()
                {
                    let lease = lease.ok_or(AxError::BadState)?;
                    if let Err(error) = admission.commit_poll(
                        lease,
                        request.events(),
                        request.multishot(),
                        actor.memory().clone(),
                    ) {
                        if submitted != 0 {
                            error!("io_uring stopped before POLL admission: {error:?}");
                            return Ok((submitted, false));
                        }
                        return Err(error);
                    }
                    submitted += 1;
                    continue;
                }

                let mut work = match admission.commit_with_openat2(
                    lease,
                    buffer,
                    context,
                    physical,
                    admission_error,
                    openat2,
                    actor.memory().clone(),
                ) {
                    Ok(work) => work,
                    Err(error) if submitted != 0 => {
                        error!("io_uring stopped before SQ admission commit: {error:?}");
                        return Ok((submitted, false));
                    }
                    Err(error) => return Err(error),
                };
                if let Some(owned) = owned {
                    if let Err(error) = work.set_owned(owned) {
                        let (prepared, ..) = work.into_parts();
                        complete_preparation_failure(ring, prepared, negative_errno(error))?;
                        return Ok((submitted, false));
                    }
                }
                submitted += 1;
                let dispatch = match ring.submit_with_dependencies(work) {
                    Ok(dispatch) => dispatch,
                    Err(error) => {
                        error!("io_uring dependency admission failed after acceptance: {error:?}");
                        return Ok((submitted, false));
                    }
                };
                match dispatch_dependency_submission(ring, dispatch) {
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

/// Deferred EXT_ARG wait state.  The outer record is copied before submission;
/// inner sigset/timespec pointers are intentionally not touched until the
/// CQ-ready fast path proves that this enter will actually wait.
struct ExtendedEnterWait {
    signal_mask: Option<SignalSet>,
    deferred_signal_mask: Option<(u64, u32)>,
    timeout: Option<Duration>,
    deferred_timeout: Option<u64>,
    min_wait: Duration,
}

fn copied_extended_enter_argument(
    capability: &UserMemoryCapability,
    address: usize,
    argsz: usize,
) -> AxResult<IoUringGeteventsArg> {
    if argsz != IoUringGeteventsArg::BYTES {
        return Err(AxError::InvalidInput);
    }
    let bytes = capability
        .read_value_uninit(address as *const [u8; IoUringGeteventsArg::BYTES])
        .map_err(map_usercopy_error)?;
    // SAFETY: the explicit capability read initialized the complete UAPI
    // record before returning success.
    Ok(IoUringGeteventsArg::from_ne_bytes(unsafe {
        bytes.assume_init()
    }))
}

fn deferred_extended_enter_wait(
    capability: &UserMemoryCapability,
    argument: IoUringGeteventsArg,
) -> AxResult<ExtendedEnterWait> {
    // io_get_ext_arg imports the timespec before the CQ fast-path; only the
    // signal mask belongs to set_user_sigmask and remains lazy.
    let timeout = if argument.timespec_address() == 0 {
        None
    } else {
        Some(copy_timeout_duration(
            capability,
            argument.timespec_address(),
        )?)
    };
    Ok(ExtendedEnterWait {
        signal_mask: None,
        deferred_signal_mask: (argument.sigmask_address() != 0)
            .then_some((argument.sigmask_address(), argument.sigmask_size())),
        timeout,
        deferred_timeout: None,
        min_wait: Duration::from_micros(argument.min_wait_usec() as u64),
    })
}

fn prepare_extended_enter_wait(
    capability: &UserMemoryCapability,
    extended: &mut ExtendedEnterWait,
) -> AxResult<()> {
    if let Some(address) = extended.deferred_timeout.take() {
        extended.timeout = Some(copy_timeout_duration(capability, address)?);
    }
    if let Some((address, size)) = extended.deferred_signal_mask.take() {
        check_sigset_size(size as usize)?;
        let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
        let value = capability
            .read_value_uninit::<SignalSet>(address as *const SignalSet)
            .map_err(map_usercopy_error)?;
        extended.signal_mask = Some(unsafe { value.assume_init() });
    }
    Ok(())
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
    // Linux rejects unknown enter bits before fd lookup or submission.  The
    // EXT_ARG payload itself is intentionally copied only on GETEVENTS, after
    // submission, matching the kernel's io_get_ext_arg ordering.
    let enter_flags = EnterFlags::from_bits(flags).map_err(|_| AxError::InvalidInput)?;
    let ring = if enter_flags.contains(EnterFlags::REGISTERED_RING) {
        registered_ring(fd)?
    } else {
        ring_from_fd(fd)?.clone_object()
    };
    if ring.disabled() {
        return Err(AxError::from(LinuxError::EBADFD));
    }
    if ring.sqpoll_enabled() && ring.sqpoll_failed() {
        return Err(AxError::BadState);
    }
    if ring.iopoll_enabled() {
        // An enter edge actively harvests provider completions.  The shared
        // device router remains the sole lower-queue owner, preserving its
        // bounded multi-ring ordering.
        crate::file::io_uring::drain_physical_completion_work();
        ring.harvest_iopoll_uring_cmd()?;
    }
    // DEFER_TASKRUN exposes completed work only at an explicit enter edge;
    // COOP_TASKRUN additionally gives every issuer a bounded cooperative
    // drain before it consumes/submits more SQEs.
    ring.run_task_work()?;
    ring.observe_completion_head()?;

    let actor = current_submission_actor(capability.clone());
    if !ring.sqpoll_enabled() && to_submit != 0 {
        // Match io_uring_add_tctx_node(): normal enter attaches current before
        // it attempts the SQ batch, so an empty/rejected batch still retains
        // the task context. Earlier outer validation/disabled failures above
        // intentionally leave no context; SQPOLL has its reviewed worker
        // ownership semantics and does not attach here.
        ensure_current_task_io_uring_context();
    }
    let (mut submitted, complete_batch) = if ring.sqpoll_enabled() {
        // SQPOLL owns SQ consumption.  An enter edge is only a low-latency
        // wake; the worker remains responsible for observing direct
        // userspace tail updates after its configured idle interval.
        if enter_flags.contains(EnterFlags::SQ_WAKEUP) || to_submit != 0 {
            ring.wake_sqpoll();
        }
        (0, to_submit == 0)
    } else {
        submit_entries(ring.as_ref(), to_submit, &actor)?
    };
    macro_rules! post_submit_try {
        ($result:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => return post_submit_result(submitted, Err(error)),
            }
        };
    }
    if enter_flags.contains(EnterFlags::SQ_WAIT) {
        if ring.sqpoll_enabled() {
            // SQ_WAIT acknowledges actual worker consumption, not merely the
            // wake edge.  The worker advances the shared SQ head; yielding
            // leaves it runnable and the predicate rechecks that exact state.
            while post_submit_try!(ring.sqpoll_has_submissions()) {
                if has_pending_syscall_signal(axtask::current().as_thread()) {
                    return if submitted == 0 {
                        Err(AxError::Interrupted)
                    } else {
                        Ok(submitted as isize)
                    };
                }
                axtask::yield_now();
            }
            // SQPOLL owns admission, but once its observed tail is consumed
            // Linux reports the caller's requested submit count.
            submitted = to_submit;
        }
    }
    if ring.iopoll_enabled() {
        crate::file::io_uring::drain_physical_completion_work();
    }
    post_submit_try!(ring.run_task_work());
    if !enter_flags.contains(EnterFlags::GETEVENTS) || !complete_batch || submitted != to_submit {
        return Ok(submitted as isize);
    }

    let (request, mut extended_wait) = if enter_flags.contains(EnterFlags::EXT_ARG_REG)
        && enter_flags.contains(EnterFlags::EXT_ARG)
    {
        // io_validate_ext_arg rejects registered records on the IOPOLL path
        // before it attempts to resolve their region or inner pointers.
        if ring.iopoll_enabled() || argsz != 64 {
            return post_submit_result(submitted, Err(AxError::InvalidInput));
        }
        let (argument, copied) =
            post_submit_try!(registered_extended_enter_argument(ring.as_ref(), sig));
        (
            post_submit_try!(
                EnterRequest::from_extended(to_submit, min_complete, flags, argument,)
                    .map_err(map_policy_error)
            ),
            Some(copied),
        )
    } else if enter_flags.contains(EnterFlags::EXT_ARG) {
        let argument = post_submit_try!(copied_extended_enter_argument(&capability, sig, argsz));
        // v6.18's IOPOLL path calls io_validate_ext_arg then io_iopoll_check:
        // it copies only this 24-byte outer record and does not dereference
        // its sigmask/timespec fields.
        let copied = if ring.iopoll_enabled() {
            None
        } else {
            Some(post_submit_try!(deferred_extended_enter_wait(
                &capability,
                argument
            )))
        };
        (
            post_submit_try!(
                EnterRequest::from_extended(to_submit, min_complete, flags, argument)
                    .map_err(map_policy_error)
            ),
            copied,
        )
    } else {
        // io_iopoll_check never interprets the legacy argp/argsz pair.  Keep
        // it out of EnterRequest too, so neither a malformed sigset size nor
        // an inaccessible legacy pointer changes the active IOPOLL result.
        let (legacy_argp, legacy_argsz) = if ring.iopoll_enabled() {
            (0, 0)
        } else {
            (sig as u64, argsz as u64)
        };
        (
            post_submit_try!(
                EnterRequest::from_raw(
                    to_submit,
                    min_complete,
                    flags,
                    legacy_argp,
                    legacy_argsz,
                    size_of::<SignalSet>() as u64,
                )
                .map_err(map_policy_error)
            ),
            None,
        )
    };

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

    // Only this path is an actual GETEVENTS wait.  All inner EXT_ARG and
    // legacy sigset/timespec pointers remain untouched until this point.
    if !ring.iopoll_enabled() {
        if let Some(extended) = extended_wait.as_mut() {
            post_submit_try!(prepare_extended_enter_wait(&capability, extended));
        }
    }
    let sigmask = if ring.iopoll_enabled() {
        None
    } else {
        match extended_wait.as_ref() {
            Some(extended) => extended.signal_mask,
            None => match request.signal_mask() {
                LegacySignalMask::None => None,
                LegacySignalMask::Address(_) => {
                    post_submit_try!(check_sigset_size(argsz));
                    post_submit_try!(copied_signal_mask(&capability, request))
                }
            },
        }
    };
    let wait_started = wall_time();
    let timeout_deadline = extended_wait
        .as_ref()
        .and_then(|extended| extended.timeout)
        .map(|timeout| {
            if enter_flags.contains(EnterFlags::ABS_TIMER) {
                TimeValue::from_nanos(timeout.as_nanos().min(u128::from(u64::MAX)) as u64)
            } else {
                wait_started.saturating_add(timeout)
            }
        });
    let mut minimum_wait_deadline: Option<TimeValue> = extended_wait
        .as_ref()
        .filter(|extended| !extended.min_wait.is_zero())
        .map(|extended| wait_started.saturating_add(extended.min_wait));
    let mut wait_minimum = minimum;

    let mut wait_once = || {
        if ring.iopoll_enabled() {
            loop {
                // Match io_iopoll_check: enter actively reaps the provider,
                // yields only as a polling fairness point, and never turns
                // IOPOLL into a timer-based readiness wait.
                crate::file::io_uring::drain_physical_completion_work();
                if let Err(error) = ring.harvest_iopoll_uring_cmd() {
                    return Ok(Err(error));
                }
                if let Err(error) = ring.run_task_work() {
                    return Ok(Err(error));
                }
                if let Err(error) = ring.observe_completion_head() {
                    return Ok(Err(error));
                }
                match ring.available_completions() {
                    Ok(available) if available >= minimum => {
                        return Ok(Ok(submitted as isize));
                    }
                    Ok(_) => {}
                    Err(error) => return Ok(Err(error)),
                }
                if has_pending_syscall_signal(axtask::current().as_thread()) {
                    return Ok(Err(AxError::Interrupted));
                }
                axtask::yield_now();
            }
        }
        loop {
            if minimum_wait_deadline.is_some_and(|deadline| wall_time() >= deadline) {
                minimum_wait_deadline = None;
                // Linux's min timer switches the post-minimum wait target to
                // one CQE, rather than continuing to demand min_complete.
                wait_minimum = 1;
            }
            let deadline = minimum_wait_deadline.or(timeout_deadline);
            let result = crate::readiness::block_on_poll_io_until(
                ring.as_ref(),
                IoEvents::READABLE,
                false,
                enter_flags.contains(EnterFlags::NO_IOWAIT),
                true,
                deadline,
                || {
                    ring.observe_completion_head()?;
                    if ring.available_completions()? >= wait_minimum
                        && minimum_wait_deadline.is_none()
                    {
                        Ok(submitted as isize)
                    } else {
                        Err(AxError::WouldBlock)
                    }
                },
            );
            match result {
                Err(_) if minimum_wait_deadline.is_some() => {
                    minimum_wait_deadline = None;
                    // The lower-bound timer is not a timeout result.  Once it
                    // expires, re-evaluate CQ state and continue toward the
                    // EXT_ARG relative deadline if the requested count is
                    // still unavailable.
                    wait_minimum = 1;
                    if let Err(error) = ring.observe_completion_head() {
                        return Ok(Err(error));
                    }
                    let available = match ring.available_completions() {
                        Ok(available) => available,
                        Err(error) => return Ok(Err(error)),
                    };
                    if available >= wait_minimum {
                        return Ok(Ok(submitted as isize));
                    }
                    if timeout_deadline.is_some_and(|deadline| wall_time() >= deadline) {
                        return Ok(Ok(-(LinuxError::ETIME.code() as isize)));
                    }
                }
                Err(_) => return Ok(Ok(-(LinuxError::ETIME.code() as isize))),
                Ok(result) => return Ok(result),
            }
        }
    };
    if submitted == 0 {
        wait_io_result(Some(uctx), sigmask, &mut wait_once)
    } else {
        // Linux returns the accepted submission count when the later wait is
        // interrupted or otherwise fails. Leave signal dispatch to the normal
        // return-to-userspace path so its frame observes that successful count,
        // while wait_io_result still restores the temporary mask on every exit.
        let waited = wait_io_result(None, sigmask, &mut wait_once);
        match waited {
            Ok(result) if result < 0 => Ok(submitted as isize),
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

        // Linux rejects an over-SZ_1G descriptor as EFAULT before considering
        // a potentially overflowing address expression.
        assert_eq!(
            overflow(0x1000, REGISTERED_BUFFER_MAX_LEN + 1),
            LinuxError::EFAULT
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
