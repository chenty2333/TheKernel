use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FadviseReadahead, FileFlags, OpenOptions, PhysicalIoOperation, PinnedPhysicalSegment};
use axfs_ng_vfs::{Location, MetadataUpdate, NodeFlags, NodeType, PhysicalIoSegment};
use axio::{IoBufMut, Seek, SeekFrom, Write};
use axnet::SocketTransferDirection;
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use linux_raw_sys::{
    general::{__kernel_off_t, IN_ACCESS, IN_ATTRIB, IN_MODIFY, O_APPEND, O_DSYNC, O_SYNC, W_OK},
    net::MSG_DONTWAIT,
};
use spin::Lazy;
use syscalls::Sysno;

use super::admit_resize;

use crate::{
    file::{
        Directory, File, FileDescription, FileHandle, FileLike, FileLikeKind, IoDst,
        IoOperationContext, IoSrc, OfdIoStatus, PacketSocket, PidFd, PinnedSocketDescription, Pipe,
        PreparedSocketMessage, Socket, allowed_write_len, check_resize_limit, executable,
        fanotify::{FanotifyEventActor, permission_check_file_like_with_actor_and_status},
        flock, get_file_like, get_typed_file, inode_flags,
        inotify::{
            notify_exact, notify_parent, notify_read, notify_read_file,
            notify_read_file_with_actor, notify_write, notify_write_file,
            notify_write_file_with_actor,
        },
        io_uring::{
            IoUringBufferLease, IoUringFileLease, PreparedPhysicalIoAdmission,
            PreparedPhysicalIoOperation, PreparedPhysicalIoPlan,
        },
        lease, memfd,
        permission::{
            SecurityFsContextExt, VfsSecurityContext, check_landlock_truncate,
            check_open_permissions_with_security, check_writable_mount,
        },
        pipe::{NamedPipe, PipeEndpoint},
        privilege_metadata::{
            ContentWriteCredentialView, ContentWritePrivilegeGuard,
            begin_content_write_privilege_cleanup,
        },
    },
    mm::{
        IoVec, IoVectorBuf, PinnedPhysicalReader, PinnedPhysicalWriter, PinnedUserSegments,
        PinnedUserSegmentsMut, UserIoPinProvenance, UserIoPinSegment, UserMemoryCapability,
        VmBytes, VmBytesMut, map_usercopy_error, pinned_user_mut_segments_are_disjoint,
        prefault_user_io_from_user_with, prefault_user_io_to_user_with, record_user_io_direct_read,
        record_user_io_direct_read_fallback, record_user_io_direct_write,
        record_user_io_direct_write_fallback, try_pin_user_segments_from_user_with,
        try_pin_user_segments_to_user_with, try_pin_user_slice_from_user_with,
        try_pin_user_slice_to_user_with,
    },
    mounts,
    pseudofs::tmp,
    readiness::block_on_poll_io,
    task::{
        AsThread, current_fs_context,
        security::{SocketSecurityContext, dispatch_socket},
    },
    time::wall_time,
};

const SEEK_DATA: c_int = 3;
const SEEK_HOLE: c_int = 4;
const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
const TMPFS_FALLOC_BLOCK_SIZE: u64 = 4096;
const FALLOC_IO_CHUNK: usize = 0x1000;
const MAX_FILE_OFFSET: u64 = i64::MAX as u64;
const SPLICE_F_MOVE: u32 = 0x01;
const SPLICE_F_NONBLOCK: u32 = 0x02;
const SPLICE_F_MORE: u32 = 0x04;
const SPLICE_F_GIFT: u32 = 0x08;
const SPLICE_F_ALL: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT;
const SYNC_FILE_RANGE_WAIT_BEFORE: u32 = 0x01;
const SYNC_FILE_RANGE_WRITE: u32 = 0x02;
const SYNC_FILE_RANGE_WAIT_AFTER: u32 = 0x04;

fn sync_file_range_end(offset: __kernel_off_t, nbytes: __kernel_off_t) -> AxResult<u64> {
    debug_assert!(offset >= 0 && nbytes >= 0);
    if nbytes == 0 {
        // Linux's zero-length form means through EOF; it is not an addition
        // in loff_t space and therefore remains valid at LLONG_MAX.
        return Ok(0);
    }
    offset
        .checked_add(nbytes)
        .map(|end| end as u64)
        .ok_or(AxError::InvalidInput)
}

fn validate_sync_file_range_args(
    offset: __kernel_off_t,
    nbytes: __kernel_off_t,
    flags: u32,
) -> AxResult<u64> {
    if offset < 0 || nbytes < 0 {
        return Err(AxError::InvalidInput);
    }
    let valid_flags =
        SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER;
    if flags & !valid_flags != 0 {
        return Err(AxError::InvalidInput);
    }
    sync_file_range_end(offset, nbytes)
}
// Regular-file O_DIRECT is constrained by logical sector alignment. Valid
// 512-byte offsets and 1 KiB transfers must not inherit a 4 KiB alignment.
const DIRECT_IO_ALIGNMENT: usize = 512;
const USER_SLICE_FAST_MIN: usize = 4096;
const USER_IOV_FAST_MAX_SEGMENTS: usize = 64;
const USER_COPY_PREFAULT_MIN: usize = 16 * 1024;
const TRANSFER_ATTEMPT_LOCK_COUNT: usize = 64;
const IO_URING_DMA_MAX_SEGMENTS: usize = crate::file::io_uring::IO_URING_PHYSICAL_MAX_SEGMENTS;
const IO_URING_DMA_MAX_BYTES: usize = crate::file::io_uring::IO_URING_PHYSICAL_MAX_BYTES;

type IoUringFixedSegments<'a> = (
    &'a [UserIoPinSegment],
    usize,
    usize,
    bool,
    UserIoPinProvenance,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixedDmaOutcome {
    Completed(usize),
    Fallback,
}

/// Result of the deliberately narrow io_uring worker entry point. The worker
/// receives an owned, submitter-prepared token, so it cannot fall back to a
/// generic path after irreversible write policy cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IoUringWorkerResult {
    Completed(isize),
    Failed(AxError),
}

fn fixed_dma_geometry_eligible(
    addr: usize,
    len: usize,
    file_offset: u64,
    segments_disjoint: bool,
    provenance: UserIoPinProvenance,
) -> bool {
    segments_disjoint
        && len != 0
        && provenance == UserIoPinProvenance::PrivateAnonymous
        && addr.is_multiple_of(DIRECT_IO_ALIGNMENT)
        && len.is_multiple_of(DIRECT_IO_ALIGNMENT)
        && file_offset.is_multiple_of(DIRECT_IO_ALIGNMENT as u64)
}

fn fixed_dma_fallback_reason(
    addr: usize,
    len: usize,
    file_offset: u64,
    segments: &[UserIoPinSegment],
    offset_in_segments: usize,
    segments_disjoint: bool,
    provenance: UserIoPinProvenance,
) -> crate::file::io_uring::IoUringDmaFallbackReason {
    use crate::file::io_uring::IoUringDmaFallbackReason;

    if provenance != UserIoPinProvenance::PrivateAnonymous {
        return IoUringDmaFallbackReason::Provenance;
    }
    if !segments_disjoint
        || len == 0
        || !addr.is_multiple_of(DIRECT_IO_ALIGNMENT)
        || !len.is_multiple_of(DIRECT_IO_ALIGNMENT)
        || !file_offset.is_multiple_of(DIRECT_IO_ALIGNMENT as u64)
    {
        return IoUringDmaFallbackReason::Geometry;
    }
    let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
    clip_io_uring_dma_segments_with_reason(segments, offset_in_segments, len, &mut physical)
        .err()
        .unwrap_or(IoUringDmaFallbackReason::DeviceAdmission)
}

fn classify_fixed_dma_result(result: Option<usize>, len: usize) -> AxResult<FixedDmaOutcome> {
    match result {
        Some(bytes) if bytes == len => Ok(FixedDmaOutcome::Completed(bytes)),
        Some(_) => Err(AxError::Io),
        None => Ok(FixedDmaOutcome::Fallback),
    }
}

/// Clips a borrowed registered-buffer physical range into the fixed-size SG
/// descriptor array consumed by the filesystem DMA hook.  Adjacent physical
/// ranges are merged after clipping, so page-fragmented user mappings do not
/// consume descriptors when the device can consume one contiguous span. This
/// never allocates and deliberately rejects ranges requiring more than four
/// descriptors so the caller can use its existing pinned bounce path.
fn clip_io_uring_dma_segments_with_reason(
    segments: &[UserIoPinSegment],
    offset: usize,
    len: usize,
    output: &mut [PhysicalIoSegment; IO_URING_DMA_MAX_SEGMENTS],
) -> Result<usize, crate::file::io_uring::IoUringDmaFallbackReason> {
    let end = offset
        .checked_add(len)
        .ok_or(crate::file::io_uring::IoUringDmaFallbackReason::Geometry)?;
    let mut logical = 0usize;
    let mut count = 0usize;
    for segment in segments.iter().copied() {
        let segment_end = logical
            .checked_add(segment.len)
            .ok_or(crate::file::io_uring::IoUringDmaFallbackReason::Geometry)?;
        let clip_start = offset.max(logical);
        let clip_end = end.min(segment_end);
        if clip_start < clip_end {
            let paddr = segment
                .paddr
                .checked_add(
                    clip_start
                        .checked_sub(logical)
                        .ok_or(crate::file::io_uring::IoUringDmaFallbackReason::Geometry)?,
                )
                .ok_or(crate::file::io_uring::IoUringDmaFallbackReason::Geometry)?;
            let clipped_len = clip_end - clip_start;
            if let Some(previous) = count.checked_sub(1).and_then(|index| output.get_mut(index))
                && previous.paddr.checked_add(previous.len) == Some(paddr)
            {
                previous.len = previous
                    .len
                    .checked_add(clipped_len)
                    .ok_or(crate::file::io_uring::IoUringDmaFallbackReason::Geometry)?;
            } else {
                if count == output.len() {
                    return Err(crate::file::io_uring::IoUringDmaFallbackReason::SgCap);
                }
                output[count] = PhysicalIoSegment::new(paddr, clipped_len);
                count += 1;
            }
        }
        logical = segment_end;
        if logical >= end {
            break;
        }
    }
    if logical < end || count == 0 {
        return Err(crate::file::io_uring::IoUringDmaFallbackReason::Geometry);
    }
    Ok(count)
}

fn clip_io_uring_dma_segments(
    segments: &[UserIoPinSegment],
    offset: usize,
    len: usize,
    output: &mut [PhysicalIoSegment; IO_URING_DMA_MAX_SEGMENTS],
) -> Option<usize> {
    clip_io_uring_dma_segments_with_reason(segments, offset, len, output).ok()
}

static TRANSFER_ATTEMPT_LOCKS: Lazy<[Mutex<()>; TRANSFER_ATTEMPT_LOCK_COUNT]> =
    Lazy::new(|| core::array::from_fn(|_| Mutex::new(())));

fn current_vfs_security() -> VfsSecurityContext {
    let current = axtask::current();
    VfsSecurityContext::new(current.as_thread().current_cred())
}

fn current_io_operation_context<T: ?Sized>(f: &FileHandle<T>) -> IoOperationContext {
    f.capture_io_operation_context(current_vfs_security(), FanotifyEventActor::current())
}

/// Captures the immutable operation identity at SQE admission.  Callers must
/// retain the returned context together with the exact `FileDescription`; no
/// worker path is allowed to derive a replacement context from `current()`.
pub(crate) fn capture_io_operation_context(
    description: &Arc<FileDescription>,
) -> IoOperationContext {
    let file_handle = description.file_handle();
    current_io_operation_context(&file_handle)
}

const fn generic_socket_message_flags(status: OfdIoStatus) -> u32 {
    if status.nonblocking() {
        MSG_DONTWAIT
    } else {
        0
    }
}

fn dispatch_generic_socket_receive(
    socket: &PinnedSocketDescription,
    status: OfdIoStatus,
    iov_count: usize,
    len: usize,
) -> AxResult<()> {
    dispatch_generic_socket_receive_with_security(
        &current_vfs_security(),
        socket,
        status,
        iov_count,
        len,
    )
}

fn dispatch_generic_socket_receive_with_security(
    security: &VfsSecurityContext,
    socket: &PinnedSocketDescription,
    status: OfdIoStatus,
    iov_count: usize,
    len: usize,
) -> AxResult<()> {
    let flags = generic_socket_message_flags(status);
    let message = PreparedSocketMessage::new(flags, iov_count, 0, 0, 0);
    let socket_ref = socket.security_ref()?;
    dispatch_socket(&SocketSecurityContext::receive_message(
        security.actor(),
        &socket_ref,
        &message,
        len,
        flags as i32,
    ))
}

fn dispatch_generic_socket_send(
    security: &VfsSecurityContext,
    socket: &PinnedSocketDescription,
    status: OfdIoStatus,
    iov_count: usize,
    len: usize,
) -> AxResult<()> {
    let flags = generic_socket_message_flags(status);
    let message = PreparedSocketMessage::new(flags, iov_count, 0, 0, 0);
    let socket_ref = socket.security_ref()?;
    dispatch_socket(&SocketSecurityContext::send_message(
        security.actor(),
        &socket_ref,
        &message,
        len,
    ))
}

/// Applies generic read/readv socket policy without changing non-socket I/O.
///
/// Linux ordinary `read(2)` with a zero total length does not enter the socket
/// receive path and therefore cannot claim an AF_PACKET queue record. A
/// nonzero socket read is authorized before its backend can write payload or
/// consume queue ownership.
fn generic_read_after_socket_policy<T>(
    socket: Option<&PinnedSocketDescription>,
    len: usize,
    authorize: impl FnOnce(&PinnedSocketDescription) -> AxResult<()>,
    read: impl FnOnce() -> AxResult<T>,
) -> AxResult<Option<T>> {
    if let Some(socket) = socket {
        if len == 0 {
            return Ok(None);
        }
        authorize(socket)?;
    }
    read().map(Some)
}

/// Applies generic write/writev socket policy before any payload access.
///
/// Unlike generic reads, a zero-length socket write still reaches Linux's
/// send-message security hook. Keeping authorization outside the backend
/// closure also makes a denial precede packet allocation and submission.
fn generic_write_after_socket_policy<T>(
    socket: Option<&PinnedSocketDescription>,
    authorize: impl FnOnce(&PinnedSocketDescription) -> AxResult<()>,
    write: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    if let Some(socket) = socket {
        authorize(socket)?;
    }
    write()
}

fn check_file_write_admission(file: &File, len: usize) -> AxResult<()> {
    file.inner().access(FileFlags::WRITE)?;
    if len != 0 {
        crate::mm::check_not_active(file.inner().location())?;
        check_writable_mount(file.inner().location())?;
    }
    Ok(())
}

fn zero_offset_stream_file_like(
    file_like: &FileHandle<dyn FileLike>,
    no_positioned: NodeFlags,
) -> bool {
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Fifo | FileLikeKind::Socket => true,
        // Character devices such as tty are represented by the generic File
        // adapter. Their stream nature is expressed by the VFS positioned-I/O
        // prohibition rather than by a distinct FileLikeKind variant.
        // Non-File FileLike implementations (eventfd, timerfd, signalfd,
        // inotify, userfaultfd, fanotify, and similar anon-inodes) expose
        // only their direct read/write methods, so they have no positioned
        // operation to fall back to. A generic File still needs its explicit
        // VFS marker, while regular files and directories are excluded by
        // their own kinds above.
        FileLikeKind::Other => file_like
            .downcast_ref::<File>()
            .is_none_or(|file| file.inner().location().flags().contains(no_positioned)),
        FileLikeKind::Regular | FileLikeKind::Directory => false,
    }
}

fn io_uring_stream_read_with_context(
    capability: &UserMemoryCapability,
    file_handle: &FileHandle<dyn FileLike>,
    context: &IoOperationContext,
    buf: *mut u8,
    len: usize,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
    force_nonblocking: bool,
) -> AxResult<isize> {
    let status = context.status();
    file_handle.check_io_status(status)?;
    let socket = PinnedSocketDescription::from_file_handle(file_handle, status)?;
    if socket.is_some() && len == 0 {
        return Ok(0);
    }
    generic_read_after_socket_policy(
        socket.as_ref(),
        len,
        |socket| {
            dispatch_generic_socket_receive_with_security(
                context.security(),
                socket,
                status,
                1,
                len,
            )
        },
        || {
            let read = if let Some((segments, offset, fixed_len, ..)) = fixed_segments {
                let mut destination =
                    PinnedPhysicalWriter::from_validated_range(segments, offset, fixed_len);
                read_file_like_with_status_and_nonblocking(
                    file_handle,
                    status,
                    &mut destination,
                    force_nonblocking,
                )?
            } else {
                read_file_like_with_status_and_nonblocking(
                    file_handle,
                    status,
                    &mut VmBytesMut::new(capability.clone(), buf, len),
                    force_nonblocking,
                )?
            };
            if read > 0
                && let Some(file) = file_handle.downcast_ref::<File>()
            {
                notify_read_file_with_actor(file, context.fanotify_actor());
            }
            Ok(read)
        },
    )
    .map(|read| read.unwrap_or(0) as isize)
}

fn io_uring_stream_write_with_context(
    capability: &UserMemoryCapability,
    file_handle: &FileHandle<dyn FileLike>,
    context: &IoOperationContext,
    buf: *const u8,
    len: usize,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> AxResult<isize> {
    let security = context.security();
    let status = context.status();
    file_handle.check_io_status(status)?;
    let socket = PinnedSocketDescription::from_file_handle(file_handle, status)?;
    let written = generic_write_after_socket_policy(
        socket.as_ref(),
        |socket| dispatch_generic_socket_send(security, socket, status, 1, len),
        || {
            if let Some(file) = file_handle.downcast_ref::<File>() {
                check_file_write_admission(file, len)?;
            }
            if let Some((segments, offset, fixed_len, ..)) = fixed_segments {
                let mut source =
                    PinnedPhysicalReader::from_validated_range(segments, offset, fixed_len);
                write_file_like_with_status(file_handle, status, &mut source, security)
            } else {
                write_file_like_with_status(
                    file_handle,
                    status,
                    &mut VmBytes::new(capability.clone(), buf, len),
                    security,
                )
            }
        },
    )?;
    if written > 0 {
        sync_file_like_after_status_write(status, file_handle)?;
        if let Some(file) = file_handle.downcast_ref::<File>() {
            notify_write_file_with_actor(file, context.fanotify_actor());
        }
    }
    Ok(written as isize)
}

/// Performs one retry for the narrow io_uring pending-stream owner.
///
/// The owner already captured the exact OFD context and registered-buffer
/// lease at SQE admission. This task-context entry point therefore does not
/// consult `current()` or install a temporary `O_NONBLOCK` flag; it uses the
/// pinned physical writer and an explicit nonblocking read override so a
/// stale/spurious wake simply re-arms readiness.
pub(crate) fn io_uring_pending_read_fixed(
    _capability: &UserMemoryCapability,
    description: &Arc<FileDescription>,
    buffer_lease: &IoUringBufferLease,
    context: &IoOperationContext,
) -> AxResult<isize> {
    context.validate_for(description)?;
    let file_handle = description.file_handle();
    if !matches!(
        FileLikeKind::from_file_like(file_handle.as_ref()),
        FileLikeKind::Fifo
    ) {
        return Err(AxError::BadState);
    }
    file_handle.check_io_status(context.status())?;
    let (segments, offset, length, _) = buffer_lease.physical_range()?;
    let mut destination = PinnedPhysicalWriter::from_validated_range(segments, offset, length);
    let read = read_file_like_with_status_and_nonblocking(
        &file_handle,
        context.status(),
        &mut destination,
        true,
    )?;
    Ok(read as isize)
}

fn begin_inode_content_write(
    location: &Location,
    security: &VfsSecurityContext,
) -> AxResult<ContentWritePrivilegeGuard> {
    begin_content_write_privilege_cleanup(
        location,
        ContentWriteCredentialView::new(security.actor(), security.filesystem_owner_user_ns()),
    )
}

fn validate_splice_flags(flags: u32) -> AxResult<()> {
    if flags & !SPLICE_F_ALL != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

const fn splice_operation_nonblocking(
    flags: u32,
    source_is_pipe: bool,
    source_nonblocking: bool,
    destination_is_pipe: bool,
    destination_nonblocking: bool,
) -> bool {
    flags & SPLICE_F_NONBLOCK != 0
        || (source_is_pipe && source_nonblocking)
        || (destination_is_pipe && destination_nonblocking)
}

/// Freezes the per-endpoint blocking modes used by splice's buffered path.
///
/// Linux propagates `SPLICE_F_NONBLOCK` and the output pipe's `O_NONBLOCK` to
/// source admission, but a socket destination still follows its own OFD mode.
/// Keeping two values prevents an explicit `Some(false)` socket override from
/// erasing the socket's operation-entry snapshot.
const fn splice_endpoint_nonblocking(
    flags: u32,
    source_nonblocking: bool,
    destination_is_pipe: bool,
    destination_nonblocking: bool,
) -> (bool, bool) {
    let flag_nonblocking = flags & SPLICE_F_NONBLOCK != 0;
    let source =
        source_nonblocking || flag_nonblocking || (destination_is_pipe && destination_nonblocking);
    let destination = destination_nonblocking || (destination_is_pipe && flag_nonblocking);
    (source, destination)
}

fn touch_modified_metadata(loc: &Location) -> AxResult<()> {
    let now = wall_time();
    loc.update_supported_metadata(MetadataUpdate {
        mtime: Some(now.into()),
        ctime: Some(now.into()),
        ..Default::default()
    })?;
    Ok(())
}

fn notify_transfer_success(source: Option<&Location>, destination: Option<&Location>) {
    if let Some(source) = source {
        let _ = notify_exact(source, IN_ACCESS);
        let _ = notify_parent(source, IN_ACCESS);
    }
    if let Some(destination) = destination {
        let _ = notify_exact(destination, IN_MODIFY);
        let _ = notify_parent(destination, IN_MODIFY);
    }
}

fn notify_splice_success(source: Option<&Location>, destination: Option<&Location>) {
    // Linux splice publishes output modification before input access. This
    // ordering affects fsnotify event merging and differs from sendfile/copy.
    if let Some(destination) = destination {
        let _ = notify_exact(destination, IN_MODIFY);
        let _ = notify_parent(destination, IN_MODIFY);
    }
    if let Some(source) = source {
        let _ = notify_exact(source, IN_ACCESS);
        let _ = notify_parent(source, IN_ACCESS);
    }
}

fn write_zero_range(file: &axfs::File, offset: u64, len: u64) -> AxResult<()> {
    if len == 0 {
        return Ok(());
    }

    let backend = file.backend()?;
    let zero = vec![0u8; FALLOC_IO_CHUNK];
    let mut written = 0u64;
    while written < len {
        let chunk = (len - written).min(zero.len() as u64) as usize;
        backend.write_at(&zero[..chunk], offset + written)?;
        written += chunk as u64;
    }
    Ok(())
}

fn copy_within_file(file: &axfs::File, src: u64, dst: u64, len: u64) -> AxResult<()> {
    if len == 0 || src == dst {
        return Ok(());
    }

    let backend = file.backend()?;
    let mut buf = vec![0u8; FALLOC_IO_CHUNK];
    let mut done = 0u64;
    while done < len {
        let chunk = (len - done).min(buf.len() as u64) as usize;
        let read = backend.read_at(&mut buf[..chunk], src + done)?;
        if read == 0 {
            break;
        }
        backend.write_at(&buf[..read], dst + done)?;
        done += read as u64;
    }
    Ok(())
}

fn copy_within_file_reverse(file: &axfs::File, src: u64, dst: u64, len: u64) -> AxResult<()> {
    if len == 0 || src == dst {
        return Ok(());
    }

    let backend = file.backend()?;
    let mut buf = vec![0u8; FALLOC_IO_CHUNK];
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(buf.len() as u64) as usize;
        let pos = remaining - chunk as u64;
        let read = backend.read_at(&mut buf[..chunk], src + pos)?;
        if read != chunk {
            return Err(AxError::InvalidInput);
        }
        let written = backend.write_at(&buf[..read], dst + pos)?;
        if written != read {
            return Err(AxError::InvalidInput);
        }
        remaining = pos;
    }
    Ok(())
}

fn file_uses_direct_io(file: &File) -> bool {
    file.inner().flags().contains(FileFlags::DIRECT)
}

fn validate_direct_io(file: &File, addr: usize, len: usize, offset: u64) -> AxResult<()> {
    if !file_uses_direct_io(file) || len == 0 {
        return Ok(());
    }
    if !addr.is_multiple_of(DIRECT_IO_ALIGNMENT)
        || !len.is_multiple_of(DIRECT_IO_ALIGNMENT)
        || !(offset as usize).is_multiple_of(DIRECT_IO_ALIGNMENT)
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_direct_iov(file: &File, iov: &IoVectorBuf, offset: u64) -> AxResult<()> {
    validate_direct_iov_prefix(file, iov, offset, iov.len())
}

fn validate_direct_iov_prefix(
    file: &File,
    iov: &IoVectorBuf,
    offset: u64,
    len: usize,
) -> AxResult<()> {
    let alignment_limit = direct_iov_alignment_limit(file, iov)?;
    validate_direct_iov_prefix_limit(file, offset, len, alignment_limit)
}

fn direct_iov_alignment_limit(file: &File, iov: &IoVectorBuf) -> AxResult<usize> {
    if file_uses_direct_io(file) {
        iov.aligned_prefix_len(DIRECT_IO_ALIGNMENT)
    } else {
        Ok(usize::MAX)
    }
}

fn validate_direct_iov_prefix_limit(
    file: &File,
    offset: u64,
    len: usize,
    alignment_limit: usize,
) -> AxResult<()> {
    if !file_uses_direct_io(file) || len == 0 {
        return Ok(());
    }
    if !(offset as usize).is_multiple_of(DIRECT_IO_ALIGNMENT)
        || !len.is_multiple_of(DIRECT_IO_ALIGNMENT)
        || len > alignment_limit
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn sync_regular_file_after_status_write(status: OfdIoStatus, file: &File) -> AxResult<()> {
    if status.raw() & O_SYNC != 0 {
        file.inner().sync(false)?;
    } else if status.raw() & O_DSYNC != 0 {
        file.inner().sync(true)?;
    }
    Ok(())
}

fn sync_file_after_status_write(status: OfdIoStatus, file: &FileHandle<File>) -> AxResult<()> {
    sync_regular_file_after_status_write(status, file.as_ref())
}

fn sync_file_like_after_status_write(
    status: OfdIoStatus,
    file_like: &FileHandle<dyn FileLike>,
) -> AxResult<()> {
    if let Some(file) = file_like.downcast_ref::<File>() {
        sync_regular_file_after_status_write(status, file)?;
    }
    Ok(())
}

fn read_file_like_with_status(
    file_like: &FileHandle<dyn FileLike>,
    status: OfdIoStatus,
    dst: &mut IoDst,
) -> AxResult<usize> {
    read_file_like_with_status_and_nonblocking(file_like, status, dst, false)
}

/// Executes one file-like read with an explicit nonblocking override.
///
/// io_uring pending-stream admission uses this only for its narrow
/// zero-offset FIFO slice.  It does not mutate the OFD's `O_NONBLOCK` bit;
/// the override is carried by this one attempt and lets the task-context
/// owner return `WouldBlock` without entering a synchronous readiness wait.
fn read_file_like_with_status_and_nonblocking(
    file_like: &FileHandle<dyn FileLike>,
    status: OfdIoStatus,
    dst: &mut IoDst,
    force_nonblocking: bool,
) -> AxResult<usize> {
    let nonblocking = status.nonblocking() || force_nonblocking;
    if let Some(file) = file_like.downcast_ref::<File>() {
        file.read_with_status(status, dst)
    } else if let Some(pipe) = file_like.downcast_ref::<Pipe>() {
        pipe.read_with_nonblocking(dst, nonblocking)
    } else if let Some(pipe) = file_like.downcast_ref::<NamedPipe>() {
        pipe.read_with_nonblocking(dst, nonblocking)
    } else if let Some(socket) = file_like.downcast_ref::<Socket>() {
        socket.read_with_nonblocking(dst, nonblocking)
    } else if let Some(socket) = file_like.downcast_ref::<PacketSocket>() {
        socket.read_with_nonblocking(dst, nonblocking)
    } else {
        file_like.read(dst)
    }
}

fn write_file_like_with_status(
    file_like: &FileHandle<dyn FileLike>,
    status: OfdIoStatus,
    src: &mut IoSrc,
    security: &VfsSecurityContext,
) -> AxResult<usize> {
    if let Some(file) = file_like.downcast_ref::<File>() {
        file.write_with_status(status, src, security)
    } else if let Some(pipe) = file_like.downcast_ref::<NamedPipe>() {
        pipe.write_with_nonblocking(src, status.nonblocking())
    } else if let Some(socket) = file_like.downcast_ref::<Socket>() {
        socket.write_with_nonblocking(src, status.nonblocking())
    } else {
        file_like.write(src)
    }
}

fn regular_file_supports_user_slice_fast_path(file: &File) -> bool {
    file.inner()
        .location()
        .flags()
        .contains(NodeFlags::BLOCKING)
}

fn regular_ext4_physical_worker_plan(
    file: &File,
    status: OfdIoStatus,
    operation: PreparedPhysicalIoOperation,
    addr: usize,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> bool {
    let Some((segments, offset_in_segments, fixed_len, disjoint, provenance)) = fixed_segments
    else {
        return false;
    };
    let Some(device) =
        axfs::block_device_for_filesystem(file.inner().location().mountpoint().device())
    else {
        return false;
    };
    if !crate::file::io_uring::physical_completion_device_ready_for(device.identity_token()) {
        return false;
    }
    if status.path_only()
        || !file_uses_direct_io(file)
        || file.inner().location().node_type() != NodeType::RegularFile
        || file.inner().location().filesystem().name() != "ext4"
        || len > IO_URING_DMA_MAX_BYTES
        || fixed_len != len
        || !fixed_dma_geometry_eligible(addr, len, offset, disjoint, provenance)
    {
        return false;
    }
    // The lower ext4 plan is overwrite-only for writes.  Keep the operation
    // in the admission shape so a read lease can never be paired with a
    // write effect (or vice versa).
    if !matches!(
        operation,
        PreparedPhysicalIoOperation::Read | PreparedPhysicalIoOperation::Write
    ) {
        return false;
    }
    let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
    clip_io_uring_dma_segments_with_reason(segments, offset_in_segments, len, &mut physical).is_ok()
}

/// This is only a cheap submission precheck. The file-specific plan resolves
/// its filesystem identity to an exact SharedBlockDevice and repeats the
/// readiness check before publication.
#[inline]
pub(crate) fn physical_effect_admission_enabled() -> bool {
    // This is only the feature gate for attempting the physical preparation
    // path.  Readiness is resolved again from the exact filesystem-bound
    // SharedBlockDevice in `regular_ext4_physical_worker_plan`; a ready vda
    // must never authorize a request whose file is on vdb (or vice versa).
    true
}

/// Performs submitter-side admission for the only operation shape permitted
/// to cross a future worker boundary. All checks here are made while the
/// exact file and registered-buffer leases are held. A `None` result means the
/// request must use the ordinary submission-task path; an `Err` result is an
/// already-admitted operation error and must not be retried through that path.
pub(crate) fn prepare_physical_io_plan(
    file_lease: &IoUringFileLease,
    buffer_lease: &IoUringBufferLease,
    context: &IoOperationContext,
    operation: PreparedPhysicalIoOperation,
    offset: u64,
) -> AxResult<Option<PreparedPhysicalIoPlan>> {
    let description = file_lease.description()?;
    context.validate_for(description)?;
    let file_handle = description.file_handle();
    let file = match file_handle.downcast::<File>() {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let (address, length) = buffer_lease.range()?;
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    let requested_len = usize::try_from(length).map_err(|_| AxError::BadAddress)?;
    if requested_len == 0 {
        return Ok(None);
    }
    let (segments, offset_in_segments, fixed_len, disjoint) = buffer_lease.physical_range()?;
    let provenance = buffer_lease.physical_provenance()?;
    // This cheap shape check keeps generic files, streams, pseudo-files, and
    // unsupported extents on the submission task without running their
    // policy hooks twice.
    if !regular_ext4_physical_worker_plan(
        file.as_ref(),
        context.status(),
        operation,
        address,
        requested_len,
        offset,
        Some((
            segments,
            offset_in_segments,
            fixed_len,
            disjoint,
            provenance,
        )),
    ) {
        return Ok(None);
    }

    // These calls own the fallible read-side policy proof which used to exist
    // only in worker comments: exact OFD access, executable exclusion, and
    // positioned-operation capability all run before a token is published.
    // A write is deliberately narrower than the generic pwrite path: it may
    // not append, allocate, or extend the file.  The lower ext4 plan repeats
    // the written-extent proof, while this admission keeps Linux policy and
    // error ordering ahead of effect publication.
    match operation {
        PreparedPhysicalIoOperation::Read => {
            let _ = positioned_read_file_handle(file_handle.clone())?;
        }
        PreparedPhysicalIoOperation::Write => {
            if context.status().raw() & O_APPEND != 0 {
                return Ok(None);
            }
            let _ = positioned_write_file_handle(file_handle.clone())?;
            check_file_write_admission(file.as_ref(), requested_len)?;
            executable::check_not_active(file.inner().location())?;
            let file_len = file.inner().location().len()?;
            let end = offset
                .checked_add(requested_len as u64)
                .ok_or(AxError::InvalidInput)?;
            if end > file_len {
                return Ok(None);
            }
            // A physical write is admitted only when Linux's file-size
            // policy accepts the complete request.  A limited prefix stays
            // on the ordinary submission path, which preserves its normal
            // partial-write result and SIGXFSZ ordering.
            if allowed_write_len(offset, requested_len)? != requested_len {
                return Ok(None);
            }
        }
    }
    let allowed_len = requested_len;
    validate_direct_io(file.as_ref(), address, allowed_len, offset)?;
    permission_check_file_like_with_actor_and_status(
        &file_handle,
        crate::file::fanotify::FAN_ACCESS_PERM,
        context.fanotify_actor(),
        context.status(),
    )?;

    let mut plan = buffer_lease.prepared_physical_plan(
        operation,
        offset,
        address,
        requested_len,
        allowed_len,
    )?;
    let device = axfs::block_device_for_filesystem(file.inner().location().mountpoint().device())
        .ok_or(AxError::OperationNotSupported)?;
    plan.bind_device(device.identity_token(), device.completion_generation())?;
    Ok(Some(plan))
}

/// Acquires the memfd reservation which must span physical effect preparation
/// and completion. A concurrent seal therefore cannot invalidate the exact
/// overwrite range while the lower layer maps it.
pub(crate) fn prepare_physical_io_write_memfd_guard(
    file_lease: &IoUringFileLease,
    context: &IoOperationContext,
    plan: &PreparedPhysicalIoPlan,
) -> AxResult<Option<memfd::MemfdMutationGuard>> {
    if plan.operation() != PreparedPhysicalIoOperation::Write {
        return Ok(None);
    }
    let description = file_lease.description()?;
    context.validate_for(description)?;
    let file = description
        .file_handle()
        .downcast::<File>()
        .map_err(|_| AxError::BadFileDescriptor)?;
    let memfd = reserve_memfd_positioned_write(file.as_ref(), plan.offset(), plan.allowed_len())?;
    Ok(Some(memfd))
}

/// Performs the set-id/capability cleanup after the lower effect has been
/// prepared, then retains the exclusion through physical retirement.  If the
/// lower layer reports `None`, no metadata side effect is performed and the
/// request remains eligible for the ordinary fallback.
pub(crate) fn prepare_physical_io_write_privilege_guard(
    file_lease: &IoUringFileLease,
    context: &IoOperationContext,
    plan: &PreparedPhysicalIoPlan,
) -> AxResult<Option<ContentWritePrivilegeGuard>> {
    if plan.operation() != PreparedPhysicalIoOperation::Write {
        return Ok(None);
    }
    let description = file_lease.description()?;
    context.validate_for(description)?;
    let file = description
        .file_handle()
        .downcast::<File>()
        .map_err(|_| AxError::BadFileDescriptor)?;
    Ok(Some(begin_inode_content_write(
        file.inner().location(),
        context.security(),
    )?))
}

/// Converts the exact, lease-derived SG plan into the vendor-owned effect.
/// This is intentionally separate from policy admission so callers can keep
/// all leases in hand until both effect construction and the request
/// publication reservation have succeeded.
pub(crate) fn prepare_physical_io_effect(
    file_lease: &IoUringFileLease,
    plan: &PreparedPhysicalIoPlan,
) -> AxResult<Option<axfs::PreparedPhysicalIoEffect>> {
    let description = file_lease.description()?;
    let file = description
        .file_handle()
        .downcast::<File>()
        .map_err(|_| AxError::BadFileDescriptor)?;
    let operation = match plan.operation() {
        PreparedPhysicalIoOperation::Read => PhysicalIoOperation::Read,
        PreparedPhysicalIoOperation::Write => PhysicalIoOperation::Write,
    };
    let physical = plan.physical_segments()?;
    file.inner()
        .backend()?
        .prepare_physical_io_effect(operation, physical, plan.offset())
}

fn regular_file_read_prefault_len(file: &File, len: usize, offset: u64) -> AxResult<usize> {
    let size = file.inner().location().len()?;
    if offset >= size {
        return Ok(0);
    }
    let available = size - offset;
    Ok(len.min(available.min(usize::MAX as u64) as usize))
}

fn prefault_regular_file_read_fallback(
    capability: &UserMemoryCapability,
    file: &File,
    buf: *mut u8,
    len: usize,
    offset: u64,
) -> AxResult<()> {
    if len < USER_COPY_PREFAULT_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(());
    }
    let len = regular_file_read_prefault_len(file, len, offset)?;
    if len >= USER_COPY_PREFAULT_MIN {
        prefault_user_io_to_user_with(capability, buf, len)?;
    }
    Ok(())
}

fn prefault_regular_file_write_fallback(
    capability: &UserMemoryCapability,
    file: &File,
    buf: *const u8,
    len: usize,
) -> AxResult<()> {
    if len < USER_COPY_PREFAULT_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(());
    }
    prefault_user_io_from_user_with(capability, buf, len)?;
    Ok(())
}

struct AxfsPinnedSegments {
    entries: [PinnedPhysicalSegment; USER_IOV_FAST_MAX_SEGMENTS],
    len: usize,
}

impl AxfsPinnedSegments {
    const fn new() -> Self {
        Self {
            entries: [PinnedPhysicalSegment::new(0, 0); USER_IOV_FAST_MAX_SEGMENTS],
            len: 0,
        }
    }

    fn push_raw(&mut self, paddr: usize, len: usize) -> AxResult<()> {
        let entry = self
            .entries
            .get_mut(self.len)
            .ok_or(AxError::InvalidInput)?;
        *entry = PinnedPhysicalSegment::new(paddr, len);
        self.len += 1;
        Ok(())
    }

    fn push(&mut self, segment: &UserIoPinSegment) -> AxResult<()> {
        self.push_raw(segment.paddr, segment.len)
    }

    fn as_slice(&self) -> &[PinnedPhysicalSegment] {
        &self.entries[..self.len]
    }
}

fn axfs_pinned_segments<'a>(
    segments: impl IntoIterator<Item = &'a UserIoPinSegment>,
) -> AxResult<AxfsPinnedSegments> {
    let mut physical = AxfsPinnedSegments::new();
    for segment in segments {
        physical.push(segment)?;
    }
    Ok(physical)
}

fn axfs_pinned_segments_subrange(
    segments: &[UserIoPinSegment],
    offset: usize,
    len: usize,
) -> AxResult<AxfsPinnedSegments> {
    let end = offset.checked_add(len).ok_or(AxError::BadAddress)?;
    let mut physical = AxfsPinnedSegments::new();
    if len == 0 {
        return Ok(physical);
    }
    let mut logical = 0usize;
    for segment in segments.iter().copied() {
        let segment_end = logical
            .checked_add(segment.len)
            .ok_or(AxError::BadAddress)?;
        let clip_start = offset.max(logical);
        let clip_end = end.min(segment_end);
        if clip_start < clip_end {
            let paddr = segment
                .paddr
                .checked_add(clip_start - logical)
                .ok_or(AxError::BadAddress)?;
            physical.push_raw(paddr, clip_end - clip_start)?;
        }
        logical = segment_end;
        if logical >= end {
            break;
        }
    }
    if logical < end {
        return Err(AxError::BadAddress);
    }
    Ok(physical)
}

fn read_at_pinned_user_segments(
    file: &File,
    segments: &[PinnedPhysicalSegment],
    offset: u64,
) -> AxResult<usize> {
    unsafe {
        // The MM pin owners outlive this call and axfs validates mutable
        // segment disjointness before materializing any destination slice.
        file.inner()
            .read_at_pinned_segments(segments, offset, false)
    }
}

fn write_at_pinned_user_segments(
    file: &File,
    segments: &[PinnedPhysicalSegment],
    offset: u64,
) -> AxResult<usize> {
    unsafe {
        // Pinned source ownership is held by the caller; cache alias policy
        // remains entirely inside axfs-ng.
        file.inner()
            .write_at_pinned_segments(segments, offset, false)
    }
}

/// Executes fixed-buffer regular-file I/O from the physical SG retained by
/// the registration lease.  Up to axfs-ng's bounded mutable SG limit uses its
/// pinned fast path; more fragmented registrations use the raw physical
/// cursor and never fall back to a virtual-address lookup or short pin.
fn read_at_fixed_user_segments(
    file: &File,
    segments: &[UserIoPinSegment],
    offset_in_segments: usize,
    len: usize,
    offset: u64,
    segments_disjoint: bool,
) -> AxResult<usize> {
    let read = if segments_disjoint
        && let Ok(physical) = axfs_pinned_segments_subrange(segments, offset_in_segments, len)
    {
        read_at_pinned_user_segments(file, physical.as_slice(), offset)?
    } else {
        let mut destination =
            PinnedPhysicalWriter::from_validated_range(segments, offset_in_segments, len);
        file.inner().read_at(&mut destination, offset)?
    };
    record_user_io_direct_read(read, segments.len());
    Ok(read)
}

fn write_at_fixed_user_segments(
    file: &File,
    segments: &[UserIoPinSegment],
    offset_in_segments: usize,
    len: usize,
    offset: u64,
) -> AxResult<usize> {
    let written =
        if let Ok(physical) = axfs_pinned_segments_subrange(segments, offset_in_segments, len) {
            write_at_pinned_user_segments(file, physical.as_slice(), offset)?
        } else {
            let mut source =
                PinnedPhysicalReader::from_validated_range(segments, offset_in_segments, len);
            file.inner().write_at(&mut source, offset)?
        };
    record_user_io_direct_write(written, segments.len());
    Ok(written)
}

fn reserve_memfd_positioned_write(
    file: &File,
    offset: u64,
    len: usize,
) -> AxResult<memfd::MemfdMutationGuard> {
    let location = file.inner().location();
    let mutation = memfd::begin_write(location, len)?;
    mutation.admit_write(location, location.len()?, offset, len)?;
    Ok(mutation)
}

fn try_regular_file_read_user_slice(
    capability: &UserMemoryCapability,
    file: &File,
    buf: *mut u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(pinned) = try_pin_user_slice_to_user_with(capability, buf, len) else {
        record_user_io_direct_read_fallback();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let physical = axfs_pinned_segments(pinned.segments())?;
    let read = read_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_read_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    buf: *mut u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(pinned) = try_pin_user_segments_to_user_with(capability, buf, len) else {
        record_user_io_direct_read_fallback();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    if !pinned_user_mut_segments_are_disjoint(core::slice::from_ref(&pinned)) {
        record_user_io_direct_read_fallback();
        return Ok(None);
    }
    let physical = axfs_pinned_segments(pinned.segments())?;
    let read = read_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_pread_user_slice(
    capability: &UserMemoryCapability,
    file: &File,
    buf: *mut u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(pinned) = try_pin_user_slice_to_user_with(capability, buf, len) else {
        record_user_io_direct_read_fallback();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let physical = axfs_pinned_segments(pinned.segments())?;
    let read = read_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_pread_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    buf: *mut u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(pinned) = try_pin_user_segments_to_user_with(capability, buf, len) else {
        record_user_io_direct_read_fallback();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    if !pinned_user_mut_segments_are_disjoint(core::slice::from_ref(&pinned)) {
        record_user_io_direct_read_fallback();
        return Ok(None);
    }
    let physical = axfs_pinned_segments(pinned.segments())?;
    let read = read_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_write_user_slice(
    capability: &UserMemoryCapability,
    file: &File,
    status: OfdIoStatus,
    security: &VfsSecurityContext,
    buf: *const u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    executable::check_not_active(file.inner().location())?;
    let _swap_mutation = crate::mm::admit_mutation(file.inner().location())?;
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    validate_direct_io(file, buf as usize, allowed, offset)?;
    let _memfd_mutation = reserve_memfd_positioned_write(file, offset, allowed)?;

    let Some(pinned) = try_pin_user_slice_from_user_with(capability, buf, allowed) else {
        record_user_io_direct_write_fallback();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let physical = axfs_pinned_segments(pinned.segments())?;
    let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
    let written = write_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_write_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    status: OfdIoStatus,
    security: &VfsSecurityContext,
    buf: *const u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    executable::check_not_active(file.inner().location())?;
    let _swap_mutation = crate::mm::admit_mutation(file.inner().location())?;
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    validate_direct_io(file, buf as usize, allowed, offset)?;
    let _memfd_mutation = reserve_memfd_positioned_write(file, offset, allowed)?;

    let Some(pinned) = try_pin_user_segments_from_user_with(capability, buf, allowed) else {
        record_user_io_direct_write_fallback();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    let physical = axfs_pinned_segments(pinned.segments())?;
    let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
    let written = write_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_pwrite_user_slice(
    capability: &UserMemoryCapability,
    file: &File,
    status: OfdIoStatus,
    security: &VfsSecurityContext,
    buf: *const u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    executable::check_not_active(file.inner().location())?;
    let _swap_mutation = crate::mm::admit_mutation(file.inner().location())?;
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    validate_direct_io(file, buf as usize, allowed, offset)?;
    let _memfd_mutation = reserve_memfd_positioned_write(file, offset, allowed)?;

    let Some(pinned) = try_pin_user_slice_from_user_with(capability, buf, allowed) else {
        record_user_io_direct_write_fallback();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let physical = axfs_pinned_segments(pinned.segments())?;
    let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
    let written = write_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_pwrite_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    status: OfdIoStatus,
    security: &VfsSecurityContext,
    buf: *const u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    executable::check_not_active(file.inner().location())?;
    let _swap_mutation = crate::mm::admit_mutation(file.inner().location())?;
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    validate_direct_io(file, buf as usize, allowed, offset)?;
    let _memfd_mutation = reserve_memfd_positioned_write(file, offset, allowed)?;

    let Some(pinned) = try_pin_user_segments_from_user_with(capability, buf, allowed) else {
        record_user_io_direct_write_fallback();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    let physical = axfs_pinned_segments(pinned.segments())?;
    let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
    let written = write_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_pin_iov_to_user(
    capability: &UserMemoryCapability,
    iov: &IoVectorBuf,
    len: usize,
) -> AxResult<Option<Vec<PinnedUserSegmentsMut>>> {
    let mut remaining = len.min(iov.len());
    let mut pinned = Vec::new();
    pinned
        .try_reserve_exact(iov.iovcnt().min(USER_IOV_FAST_MAX_SEGMENTS))
        .map_err(|_| AxError::NoMemory)?;
    let mut segments = 0usize;
    for idx in 0..iov.iovcnt() {
        if remaining == 0 {
            break;
        }
        let entry = iov.entry(idx)?;
        let iov_len = entry.iov_len as usize;
        if iov_len == 0 {
            continue;
        }
        let chunk = iov_len.min(remaining);
        let Some(pin) = try_pin_user_segments_to_user_with(capability, entry.iov_base, chunk)
        else {
            return Ok(None);
        };
        segments += pin.segments().len();
        if segments > USER_IOV_FAST_MAX_SEGMENTS {
            return Ok(None);
        }
        if pinned.len() == pinned.capacity() {
            return Ok(None);
        }
        pinned.push(pin);
        remaining -= chunk;
    }
    Ok(Some(pinned))
}

fn try_pin_iov_from_user(
    capability: &UserMemoryCapability,
    iov: &IoVectorBuf,
    len: usize,
) -> AxResult<Option<Vec<PinnedUserSegments>>> {
    let mut remaining = len.min(iov.len());
    let mut pinned = Vec::new();
    pinned
        .try_reserve_exact(iov.iovcnt().min(USER_IOV_FAST_MAX_SEGMENTS))
        .map_err(|_| AxError::NoMemory)?;
    let mut segments = 0usize;
    for idx in 0..iov.iovcnt() {
        if remaining == 0 {
            break;
        }
        let entry = iov.entry(idx)?;
        let iov_len = entry.iov_len as usize;
        if iov_len == 0 {
            continue;
        }
        let chunk = iov_len.min(remaining);
        let Some(pin) =
            try_pin_user_segments_from_user_with(capability, entry.iov_base as *const u8, chunk)
        else {
            return Ok(None);
        };
        segments += pin.segments().len();
        if segments > USER_IOV_FAST_MAX_SEGMENTS {
            return Ok(None);
        }
        if pinned.len() == pinned.capacity() {
            return Ok(None);
        }
        pinned.push(pin);
        remaining -= chunk;
    }
    Ok(Some(pinned))
}

fn try_regular_file_readv_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    iov: &IoVectorBuf,
    offset: u64,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(pinned) = try_pin_iov_to_user(capability, iov, iov.len())? else {
        record_user_io_direct_read_fallback();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    if !pinned_user_mut_segments_are_disjoint(&pinned) {
        record_user_io_direct_read_fallback();
        return Ok(None);
    }
    let physical = axfs_pinned_segments(pinned.iter().flat_map(|pin| pin.segments()))?;
    let read = read_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_preadv_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    iov: &IoVectorBuf,
    offset: u64,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(pinned) = try_pin_iov_to_user(capability, iov, iov.len())? else {
        record_user_io_direct_read_fallback();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    if !pinned_user_mut_segments_are_disjoint(&pinned) {
        record_user_io_direct_read_fallback();
        return Ok(None);
    }
    let physical = axfs_pinned_segments(pinned.iter().flat_map(|pin| pin.segments()))?;
    let read = read_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_writev_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    status: OfdIoStatus,
    security: &VfsSecurityContext,
    iov: &IoVectorBuf,
    offset: u64,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    executable::check_not_active(file.inner().location())?;
    let _swap_mutation = crate::mm::admit_mutation(file.inner().location())?;
    let allowed = allowed_write_len(offset, iov.len())?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    validate_direct_iov_prefix(file, iov, offset, allowed)?;
    let _memfd_mutation = reserve_memfd_positioned_write(file, offset, allowed)?;

    let Some(pinned) = try_pin_iov_from_user(capability, iov, allowed)? else {
        record_user_io_direct_write_fallback();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    let physical = axfs_pinned_segments(pinned.iter().flat_map(|pin| pin.segments()))?;
    let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
    let written = write_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_pwritev_user_segments(
    capability: &UserMemoryCapability,
    file: &File,
    status: OfdIoStatus,
    security: &VfsSecurityContext,
    iov: &IoVectorBuf,
    offset: u64,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    executable::check_not_active(file.inner().location())?;
    let _swap_mutation = crate::mm::admit_mutation(file.inner().location())?;
    let allowed = allowed_write_len(offset, iov.len())?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    validate_direct_iov_prefix(file, iov, offset, allowed)?;
    let _memfd_mutation = reserve_memfd_positioned_write(file, offset, allowed)?;

    let Some(pinned) = try_pin_iov_from_user(capability, iov, allowed)? else {
        record_user_io_direct_write_fallback();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    let physical = axfs_pinned_segments(pinned.iter().flat_map(|pin| pin.segments()))?;
    let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
    let written = write_at_pinned_user_segments(file, physical.as_slice(), offset)?;
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

pub fn sys_unsupported_fd(sysno: Sysno) -> AxResult<isize> {
    warn!("Unimplemented fd syscall: {sysno}");
    Err(AxError::Unsupported)
}

/// Read data from the file indicated by `fd`.
///
/// Return the read size if success.
pub fn sys_read(
    capability: UserMemoryCapability,
    fd: i32,
    buf: *mut u8,
    len: usize,
) -> AxResult<isize> {
    debug!("sys_read <= fd: {fd}, buf: {buf:p}, len: {len}");
    let f = get_file_like(fd)?;
    let status = f.io_status_snapshot();
    f.check_io_status(status)?;
    let socket = PinnedSocketDescription::from_file_handle(&f, status)?;
    if socket.is_some() && len == 0 {
        return Ok(0);
    }
    if len != 0 {
        crate::file::fanotify::permission_check_file_like(
            &f,
            crate::file::fanotify::FAN_ACCESS_PERM,
        )?;
    }
    generic_read_after_socket_policy(
        socket.as_ref(),
        len,
        |socket| dispatch_generic_socket_receive(socket, status, 1, len),
        || {
            f.with_read_credentials(|| {
                let read = if let Some(file) = f.downcast_ref::<File>()
                    && file_has_current_position(file.inner())
                {
                    with_current_position_io(file, len, |offset| {
                        validate_direct_io(file, buf as usize, len, offset)?;
                        let fast_read = match try_regular_file_read_user_slice(
                            &capability,
                            file,
                            buf,
                            len,
                            offset,
                        )? {
                            Some(read) => Some(read),
                            None => try_regular_file_read_user_segments(
                                &capability,
                                file,
                                buf,
                                len,
                                offset,
                            )?,
                        };
                        let read = if let Some(read) = fast_read {
                            read
                        } else {
                            if len >= USER_COPY_PREFAULT_MIN {
                                prefault_regular_file_read_fallback(
                                    &capability,
                                    file,
                                    buf,
                                    len,
                                    offset,
                                )?;
                            }
                            file.read_at_with_status(
                                status,
                                &mut VmBytesMut::new(capability.clone(), buf, len),
                                offset,
                            )?
                        };
                        Ok((read, read))
                    })?
                } else {
                    read_file_like_with_status(
                        &f,
                        status,
                        &mut VmBytesMut::new(capability.clone(), buf, len),
                    )?
                } as isize;
                if read > 0
                    && let Some(file) = f.downcast_ref::<File>()
                {
                    notify_read_file(file);
                }
                Ok(read)
            })
        },
    )
    .map(|read| read.unwrap_or(0))
}

pub fn sys_readv(
    capability: UserMemoryCapability,
    fd: i32,
    iov: *const IoVec,
    iovcnt: usize,
) -> AxResult<isize> {
    debug!("sys_readv <= fd: {fd}, iovcnt: {iovcnt}");
    let f = get_file_like(fd)?;
    let status = f.io_status_snapshot();
    f.check_io_status(status)?;
    let socket = PinnedSocketDescription::from_file_handle(&f, status)?;
    let iov = IoVectorBuf::new(capability.clone(), iov, iovcnt)?;
    let len = iov.len();
    let imported_iov_count = iov.iovcnt();
    if socket.is_some() && len == 0 {
        return Ok(0);
    }
    if iovcnt != 0 {
        crate::file::fanotify::permission_check_file_like(
            &f,
            crate::file::fanotify::FAN_ACCESS_PERM,
        )?;
    }
    generic_read_after_socket_policy(
        socket.as_ref(),
        len,
        |socket| dispatch_generic_socket_receive(socket, status, imported_iov_count, len),
        || {
            f.with_read_credentials(|| {
                let read = if let Some(file) = f.downcast_ref::<File>()
                    && file_has_current_position(file.inner())
                {
                    with_current_position_io(file, iov.len(), |offset| {
                        validate_direct_iov(file, &iov, offset)?;
                        let read = if let Some(read) =
                            try_regular_file_readv_user_segments(&capability, file, &iov, offset)?
                        {
                            read
                        } else {
                            file.read_at_with_status(status, &mut iov.into_io(), offset)?
                        };
                        Ok((read, read))
                    })?
                } else {
                    read_file_like_with_status(&f, status, &mut iov.into_io())?
                } as isize;
                if read > 0
                    && let Some(file) = f.downcast_ref::<File>()
                {
                    notify_read_file(file);
                }
                Ok(read)
            })
        },
    )
    .map(|read| read.unwrap_or(0))
}

/// Write data to the file indicated by `fd`.
///
/// Return the written size if success.
pub fn sys_write(
    capability: UserMemoryCapability,
    fd: i32,
    buf: *mut u8,
    len: usize,
) -> AxResult<isize> {
    debug!("sys_write <= fd: {fd}, buf: {buf:p}, len: {len}");
    let security = current_vfs_security();
    let f = get_file_like(fd)?;
    let status = f.io_status_snapshot();
    let socket = PinnedSocketDescription::from_file_handle(&f, status)?;
    let (written, status) = generic_write_after_socket_policy(
        socket.as_ref(),
        |socket| dispatch_generic_socket_send(&security, socket, status, 1, len),
        || {
            f.with_write_credentials_for_status(status, || {
                let regular_file = if let Some(file) = f.downcast_ref::<File>() {
                    check_file_write_admission(file, len)?;
                    Some(file)
                } else {
                    None
                };
                if let Some(file) = regular_file {
                    if write_uses_current_position(file.inner(), status) {
                        return with_current_position_io(file, len, |offset| {
                            let allowed = allowed_write_len(offset, len)?;
                            if let Some(written) = try_regular_file_write_user_slice(
                                &capability,
                                file,
                                status,
                                &security,
                                buf as *const u8,
                                len,
                                offset,
                            )? {
                                return Ok(((written, status), written));
                            }
                            if let Some(written) = try_regular_file_write_user_segments(
                                &capability,
                                file,
                                status,
                                &security,
                                buf as *const u8,
                                len,
                                offset,
                            )? {
                                return Ok(((written, status), written));
                            }
                            if len >= USER_COPY_PREFAULT_MIN {
                                prefault_regular_file_write_fallback(
                                    &capability,
                                    file,
                                    buf as *const u8,
                                    allowed,
                                )?;
                            }
                            let written = file.write_at_with_status_and_direct_validation(
                                status,
                                &mut VmBytes::new(capability.clone(), buf, len),
                                offset,
                                &security,
                                |write_offset, write_len| {
                                    validate_direct_io(file, buf as usize, write_len, write_offset)
                                },
                            )?;
                            Ok(((written, status), written))
                        });
                    }

                    let written = file.write_with_status_and_direct_validation(
                        status,
                        &mut VmBytes::new(capability.clone(), buf, len),
                        &security,
                        |offset, allowed| validate_direct_io(file, buf as usize, allowed, offset),
                    )?;
                    return Ok((written, status));
                }
                write_file_like_with_status(
                    &f,
                    status,
                    &mut VmBytes::new(capability.clone(), buf, len),
                    &security,
                )
                .map(|written| (written, status))
            })
        },
    )?;
    let written = written as isize;
    if written > 0 {
        sync_file_like_after_status_write(status, &f)?;
        if let Some(file) = f.downcast_ref::<File>() {
            notify_write_file(file);
        }
    }
    Ok(written)
}

pub fn sys_writev(
    capability: UserMemoryCapability,
    fd: i32,
    iov: *const IoVec,
    iovcnt: usize,
) -> AxResult<isize> {
    debug!("sys_writev <= fd: {fd}, iovcnt: {iovcnt}");
    let security = current_vfs_security();
    let iov = IoVectorBuf::new(capability.clone(), iov, iovcnt)?;
    let len = iov.len();
    let imported_iov_count = iov.iovcnt();
    let f = get_file_like(fd)?;
    let status = f.io_status_snapshot();
    let socket = PinnedSocketDescription::from_file_handle(&f, status)?;
    let written = generic_write_after_socket_policy(
        socket.as_ref(),
        |socket| dispatch_generic_socket_send(&security, socket, status, imported_iov_count, len),
        || {
            if f.downcast_ref::<File>().is_some() {
                let file = f.downcast::<File>()?;
                let (written, status) = file.with_write_credentials_for_status(status, || {
                    iov.check_readable()?;
                    check_file_write_admission(file.as_ref(), iov.len())?;
                    let direct_alignment_limit = direct_iov_alignment_limit(file.as_ref(), &iov)?;
                    if write_uses_current_position(file.inner(), status) {
                        return with_current_position_io(file.as_ref(), iov.len(), |offset| {
                            if let Some(written) = try_regular_file_writev_user_segments(
                                &capability,
                                file.as_ref(),
                                status,
                                &security,
                                &iov,
                                offset,
                            )? {
                                return Ok(((written, status), written));
                            }
                            let written = file.write_at_with_status_and_direct_validation(
                                status,
                                &mut iov.into_io(),
                                offset,
                                &security,
                                |write_offset, write_len| {
                                    validate_direct_iov_prefix_limit(
                                        file.as_ref(),
                                        write_offset,
                                        write_len,
                                        direct_alignment_limit,
                                    )
                                },
                            )?;
                            Ok(((written, status), written))
                        });
                    }

                    file.write_with_status_and_direct_validation(
                        status,
                        &mut iov.into_io(),
                        &security,
                        |offset, allowed| {
                            validate_direct_iov_prefix_limit(
                                file.as_ref(),
                                offset,
                                allowed,
                                direct_alignment_limit,
                            )
                        },
                    )
                    .map(|written| (written, status))
                })?;
                if written > 0 {
                    sync_file_after_status_write(status, &file)?;
                }
                Ok(written)
            } else {
                let (written, status) = f.with_write_credentials_for_status(status, || {
                    write_file_like_with_status(&f, status, &mut iov.into_io(), &security)
                        .map(|written| (written, status))
                })?;
                if written > 0 {
                    sync_file_like_after_status_write(status, &f)?;
                }
                Ok(written)
            }
        },
    )?;
    let written = written as isize;
    if written > 0
        && let Some(file) = f.downcast_ref::<File>()
    {
        notify_write_file(file);
    }
    Ok(written)
}

pub fn sys_readahead(fd: c_int, offset: __kernel_off_t, count: usize) -> AxResult<isize> {
    debug!("sys_readahead <= fd: {fd}, offset: {offset}, count: {count}");
    if offset < 0 {
        return Err(AxError::InvalidInput);
    }

    let file_like = get_file_like(fd)?;
    if file_like.downcast_ref::<PidFd>().is_some() {
        return Ok(0);
    }
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Regular => {
            let file = file_like
                .downcast_ref::<File>()
                .ok_or(AxError::InvalidInput)?;
            file.inner().access(FileFlags::READ)?;
            Ok(0)
        }
        FileLikeKind::Fifo => Err(AxError::from(LinuxError::ESPIPE)),
        FileLikeKind::Socket => Err(AxError::InvalidInput),
        FileLikeKind::Directory | FileLikeKind::Other => Err(AxError::InvalidInput),
    }
}

fn positioned_file_handle(
    file_like: FileHandle<dyn FileLike>,
    access: FileFlags,
) -> AxResult<FileHandle<File>> {
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Directory => return Err(AxError::IsADirectory),
        FileLikeKind::Fifo | FileLikeKind::Socket => return Err(AxError::from(LinuxError::ESPIPE)),
        FileLikeKind::Regular | FileLikeKind::Other => {}
    }

    // Retain the exact description which was classified above. A second
    // numeric-fd lookup could observe a close-and-reuse by a CLONE_FILES peer.
    let file = file_like.downcast::<File>()?;
    file.inner().access(access)?;
    Ok(file)
}

fn positioned_file(fd: c_int, access: FileFlags) -> AxResult<FileHandle<File>> {
    positioned_file_handle(get_file_like(fd)?, access)
}

fn write_file(fd: c_int) -> AxResult<FileHandle<File>> {
    let file = positioned_file(fd, FileFlags::WRITE)?;
    check_writable_mount(file.inner().location())?;
    executable::check_not_active(file.inner().location())?;
    Ok(file)
}

fn check_positioned_write_flags(flags: NodeFlags) -> AxResult<()> {
    if flags.contains(NodeFlags::NO_POSITIONED_WRITE) {
        Err(LinuxError::ESPIPE.into())
    } else {
        Ok(())
    }
}

fn check_positioned_read_flags(flags: NodeFlags) -> AxResult<()> {
    if flags.contains(NodeFlags::NO_POSITIONED_READ) {
        Err(LinuxError::ESPIPE.into())
    } else {
        Ok(())
    }
}

fn positioned_read_file(fd: c_int) -> AxResult<FileHandle<File>> {
    let file = positioned_file(fd, FileFlags::READ)?;
    check_positioned_read_flags(file.inner().location().flags())?;
    Ok(file)
}

fn positioned_read_file_handle(file_like: FileHandle<dyn FileLike>) -> AxResult<FileHandle<File>> {
    let file = positioned_file_handle(file_like, FileFlags::READ)?;
    check_positioned_read_flags(file.inner().location().flags())?;
    Ok(file)
}

fn positioned_write_file(fd: c_int) -> AxResult<FileHandle<File>> {
    let file = write_file(fd)?;
    crate::mm::check_not_active(file.inner().location())?;
    if !file.inner().supports_positioned_write() {
        return Err(LinuxError::ESPIPE.into());
    }
    Ok(file)
}

fn positioned_write_file_handle(file_like: FileHandle<dyn FileLike>) -> AxResult<FileHandle<File>> {
    let file = positioned_file_handle(file_like, FileFlags::WRITE)?;
    check_writable_mount(file.inner().location())?;
    crate::mm::check_not_active(file.inner().location())?;
    executable::check_not_active(file.inner().location())?;
    if !file.inner().supports_positioned_write() {
        return Err(LinuxError::ESPIPE.into());
    }
    Ok(file)
}

fn regular_copy_file(
    file_like: FileHandle<dyn FileLike>,
    status: OfdIoStatus,
    write: bool,
) -> AxResult<FileHandle<File>> {
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Regular => {}
        FileLikeKind::Directory => return Err(AxError::IsADirectory),
        FileLikeKind::Fifo | FileLikeKind::Socket | FileLikeKind::Other => {
            return Err(AxError::InvalidInput);
        }
    }

    // Type-check and operate on one stable open-file description.
    let file = file_like.downcast::<File>()?;
    if write {
        if status.append() {
            return Err(AxError::BadFileDescriptor);
        }
        file.inner().access(FileFlags::WRITE)?;
        check_writable_mount(file.inner().location())?;
        crate::mm::check_not_active(file.inner().location())?;
        executable::check_not_active(file.inner().location())?;
    } else {
        file.inner().access(FileFlags::READ)?;
    }
    Ok(file)
}

fn with_current_position_io<T>(
    file: &File,
    max_advance: usize,
    operation: impl FnOnce(u64) -> AxResult<(T, usize)>,
) -> AxResult<T> {
    file.inner()
        .with_current_position_transaction(max_advance, operation)
}

fn file_has_current_position(file: &axfs::File) -> bool {
    file.has_current_position()
}

fn write_uses_inode_append(file: &axfs::File, status: OfdIoStatus) -> bool {
    status.append()
        && file_has_current_position(file)
        && !file
            .location()
            .flags()
            .contains(NodeFlags::POSITIONED_APPEND)
}

fn write_uses_current_position(file: &axfs::File, status: OfdIoStatus) -> bool {
    file_has_current_position(file) && !write_uses_inode_append(file, status)
}

fn has_mandatory_lock_mode(loc: &axfs_ng_vfs::Location) -> AxResult<bool> {
    let metadata = loc.metadata()?;
    let mode = metadata.mode.bits();
    Ok(metadata.node_type == axfs_ng_vfs::NodeType::RegularFile
        && mode & 0o2000 != 0
        && mode & 0o010 == 0)
}

#[derive(Default)]
struct MandatoryTransferState {
    admitted: bool,
    wait: Option<flock::MandatoryLockWait>,
}

fn admit_mandatory_transfer_range(
    state: &mut MandatoryTransferState,
    loc: &axfs_ng_vfs::Location,
    ofd_key: u64,
    access: flock::MandatoryAccess,
    start: u64,
    len: usize,
) -> AxResult<()> {
    if state.admitted {
        return Ok(());
    }
    if len == 0 || !mounts::has_mandatory_locking(loc)? || !has_mandatory_lock_mode(loc)? {
        state.admitted = true;
        return Ok(());
    }

    let len = u64::try_from(len).map_err(|_| AxError::InvalidInput)?;
    let metadata = loc.metadata()?;
    let pid = axtask::current().as_thread().proc_data.proc.pid();
    let owners = flock::MandatoryOwners::new(pid, ofd_key);
    match flock::mandatory_access_conflict(
        (metadata.device, metadata.inode),
        owners,
        access,
        start,
        len,
    )? {
        Some(wait) => {
            state.wait = Some(wait);
            Err(AxError::WouldBlock)
        }
        None => {
            state.admitted = true;
            Ok(())
        }
    }
}

pub(crate) fn check_mandatory_truncate_lock(
    loc: &axfs_ng_vfs::Location,
    new_len: u64,
    owner: flock::RecordLockOwner,
) -> AxResult<()> {
    if !mounts::has_mandatory_locking(loc)? || !has_mandatory_lock_mode(loc)? {
        return Ok(());
    }

    let size = loc.len()?;
    if new_len >= size {
        return Ok(());
    }
    let metadata = loc.metadata()?;
    let id = (metadata.device, metadata.inode);
    if flock::mandatory_write_lock_conflicts(id, owner, new_len, size - new_len) {
        return Err(AxError::WouldBlock);
    }
    Ok(())
}

pub(crate) fn check_mandatory_fd_truncate_lock(
    loc: &axfs_ng_vfs::Location,
    new_len: u64,
    ofd_key: u64,
    nonblocking: bool,
) -> AxResult<()> {
    if !mounts::has_mandatory_locking(loc)? || !has_mandatory_lock_mode(loc)? {
        return Ok(());
    }

    let size = loc.len()?;
    if new_len >= size {
        return Ok(());
    }
    let metadata = loc.metadata()?;
    let pid = axtask::current().as_thread().proc_data.proc.pid();
    let owners = flock::MandatoryOwners::new(pid, ofd_key);
    let wait = flock::mandatory_access_conflict(
        (metadata.device, metadata.inode),
        owners,
        flock::MandatoryAccess::Write,
        new_len,
        size - new_len,
    )?;
    let Some(wait) = wait else {
        return Ok(());
    };
    if nonblocking {
        return Err(AxError::WouldBlock);
    }
    flock::wait_for_mandatory_access(wait)
}

fn checked_user_file_offset(capability: &UserMemoryCapability, ptr: *mut u64) -> AxResult<u64> {
    let value = capability
        .read_value(ptr as *const u64)
        .map_err(map_usercopy_error)?;
    if value > MAX_FILE_OFFSET {
        return Err(AxError::InvalidInput);
    }
    Ok(value)
}

fn ranges_overlap(left_start: u64, right_start: u64, len: u64) -> AxResult<bool> {
    if len == 0 {
        return Ok(false);
    }
    let left_end = left_start
        .checked_add(len)
        .ok_or_else(|| AxError::from(LinuxError::EOVERFLOW))?;
    let right_end = right_start
        .checked_add(len)
        .ok_or_else(|| AxError::from(LinuxError::EFBIG))?;
    Ok(left_start < right_end && right_start < left_end)
}

fn copy_file_range_source_count(
    source_offset: u64,
    requested: usize,
    source_size: u64,
) -> AxResult<usize> {
    let requested = u64::try_from(requested).map_err(|_| AxError::from(LinuxError::EOVERFLOW))?;
    source_offset
        .checked_add(requested)
        .ok_or_else(|| AxError::from(LinuxError::EOVERFLOW))?;
    usize::try_from(requested.min(source_size.saturating_sub(source_offset)))
        .map_err(|_| AxError::from(LinuxError::EOVERFLOW))
}

fn copy_file_range_effective_count(
    source_offset: u64,
    destination_offset: u64,
    source_count: usize,
    destination_allowed: usize,
    same_inode: bool,
) -> AxResult<usize> {
    let mut count = source_count.min(destination_allowed);
    if count == 0 {
        return Ok(0);
    }
    if destination_offset >= MAX_FILE_OFFSET {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    let max_count = usize::try_from(MAX_FILE_OFFSET - destination_offset).unwrap_or(usize::MAX);
    count = count.min(max_count);
    let count64 = u64::try_from(count).map_err(|_| AxError::from(LinuxError::EFBIG))?;
    if same_inode && ranges_overlap(source_offset, destination_offset, count64)? {
        return Err(AxError::InvalidInput);
    }
    Ok(count)
}

fn seekable_fd(fd: c_int) -> AxResult<FileHandle<dyn FileLike>> {
    let file_like = get_file_like(fd)?;
    file_like.check_io_access()?;
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Fifo | FileLikeKind::Socket => Err(AxError::from(LinuxError::ESPIPE)),
        FileLikeKind::Regular | FileLikeKind::Directory | FileLikeKind::Other => {
            if file_like
                .downcast_ref::<File>()
                .is_some_and(|file| !file.inner().supports_seek())
            {
                return Err(LinuxError::ESPIPE.into());
            }
            Ok(file_like)
        }
    }
}

fn seek_file_like(
    file_like: &FileHandle<dyn FileLike>,
    offset: __kernel_off_t,
    whence: c_int,
) -> AxResult<isize> {
    if whence == 0 && offset < 0 {
        return Err(AxError::InvalidInput);
    }

    if let Some(file) = file_like.downcast_ref::<File>() {
        let pos = match whence {
            0 => SeekFrom::Start(offset as _),
            1 => SeekFrom::Current(offset as _),
            2 => SeekFrom::End(offset as _),
            _ => return Err(LinuxError::EINVAL.into()),
        };
        return file.inner().seek(pos).map(|off| off as isize);
    }

    if let Some(dir) = file_like.downcast_ref::<Directory>() {
        let mut current = dir.offset.lock();
        let new_pos = match whence {
            0 => offset as u64,
            1 => current
                .checked_add_signed(offset)
                .ok_or(AxError::InvalidInput)?,
            2 => dir
                .inner()
                .len()?
                .checked_add_signed(offset)
                .ok_or(AxError::InvalidInput)?,
            _ => return Err(LinuxError::EINVAL.into()),
        };
        *current = new_pos;
        return Ok(new_pos as isize);
    }

    Err(AxError::from(LinuxError::ESPIPE))
}

fn do_preadv(
    capability: &UserMemoryCapability,
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset: __kernel_off_t,
    flags: u32,
    allow_current_offset: bool,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::OperationNotSupported);
    }
    if offset < 0 && !(allow_current_offset && offset == -1) {
        return Err(AxError::InvalidInput);
    }

    let file = positioned_file(fd, FileFlags::READ)?;
    if offset != -1 {
        check_positioned_read_flags(file.inner().location().flags())?;
    }
    let status = file.io_status_snapshot();
    file.check_io_status(status)?;
    let iov = IoVectorBuf::new(capability.clone(), iov, iovcnt)?;
    if iov.len() != 0 {
        crate::file::fanotify::permission_check_fd(fd, crate::file::fanotify::FAN_ACCESS_PERM)?;
    }
    file.with_read_credentials(|| {
        let read = if offset == -1 && file_has_current_position(file.inner()) {
            with_current_position_io(file.as_ref(), iov.len(), |offset| {
                validate_direct_iov(file.as_ref(), &iov, offset)?;
                let read = if let Some(read) =
                    try_regular_file_readv_user_segments(capability, file.as_ref(), &iov, offset)?
                {
                    read
                } else {
                    file.read_at_with_status(status, &mut iov.into_io(), offset)?
                };
                Ok((read, read))
            })?
        } else if offset == -1 {
            file.read_with_status(status, &mut iov.into_io())?
        } else {
            validate_direct_iov(file.as_ref(), &iov, offset as u64)?;
            if let Some(read) = try_regular_file_preadv_user_segments(
                capability,
                file.as_ref(),
                &iov,
                offset as u64,
            )? {
                if read > 0 {
                    notify_read(fd);
                }
                return Ok(read as _);
            }
            let io = iov.into_io();
            file.inner().read_at(io, offset as u64)?
        } as isize;
        if read > 0 {
            notify_read(fd);
        }
        Ok(read)
    })
}

fn do_pwritev(
    capability: &UserMemoryCapability,
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset: __kernel_off_t,
    flags: u32,
    allow_current_offset: bool,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::OperationNotSupported);
    }
    if offset < 0 && !(allow_current_offset && offset == -1) {
        return Err(AxError::InvalidInput);
    }
    let security = current_vfs_security();

    // pwritev2(offset = -1) is an ordinary current-position write. Only an
    // explicit offset requires positioned-write support.
    let file = if offset == -1 {
        write_file(fd)?
    } else {
        positioned_write_file(fd)?
    };
    let io = IoVectorBuf::new(capability.clone(), iov, iovcnt)?;
    let (written, status) = file.with_write_credentials(|status| {
        let direct_alignment_limit = direct_iov_alignment_limit(file.as_ref(), &io)?;
        if offset == -1 {
            if write_uses_current_position(file.inner(), status) {
                return with_current_position_io(file.as_ref(), io.len(), |write_offset| {
                    if let Some(written) = try_regular_file_writev_user_segments(
                        capability,
                        file.as_ref(),
                        status,
                        &security,
                        &io,
                        write_offset,
                    )? {
                        return Ok(((written, status), written));
                    }
                    let written = file.write_at_with_status_and_direct_validation(
                        status,
                        &mut io.into_io(),
                        write_offset,
                        &security,
                        |offset, len| {
                            validate_direct_iov_prefix_limit(
                                file.as_ref(),
                                offset,
                                len,
                                direct_alignment_limit,
                            )
                        },
                    )?;
                    Ok(((written, status), written))
                });
            }

            file.write_with_status_and_direct_validation(
                status,
                &mut io.into_io(),
                &security,
                |offset, len| {
                    validate_direct_iov_prefix_limit(
                        file.as_ref(),
                        offset,
                        len,
                        direct_alignment_limit,
                    )
                },
            )
        } else if write_uses_inode_append(file.inner(), status) {
            file.write_at_end_with_status_and_direct_validation(
                status,
                &mut io.into_io(),
                &security,
                |append_offset, len| {
                    validate_direct_iov_prefix_limit(
                        file.as_ref(),
                        append_offset,
                        len,
                        direct_alignment_limit,
                    )
                },
            )
        } else {
            if let Some(written) = try_regular_file_pwritev_user_segments(
                capability,
                file.as_ref(),
                status,
                &security,
                &io,
                offset as u64,
            )? {
                return Ok((written, status));
            }
            file.write_at_with_status_and_direct_validation(
                status,
                &mut io.into_io(),
                offset as u64,
                &security,
                |write_offset, len| {
                    validate_direct_iov_prefix_limit(
                        file.as_ref(),
                        write_offset,
                        len,
                        direct_alignment_limit,
                    )
                },
            )
        }
        .map(|written| (written, status))
    })?;
    let written = written as isize;
    if written > 0 {
        sync_file_after_status_write(status, &file)?;
        notify_write(fd);
    }
    Ok(written)
}

pub fn sys_lseek(fd: c_int, offset: __kernel_off_t, whence: c_int) -> AxResult<isize> {
    debug!("sys_lseek <= {fd} {offset} {whence}");
    match whence {
        0..=2 => seek_file_like(&seekable_fd(fd)?, offset, whence),
        SEEK_DATA | SEEK_HOLE => {
            if offset < 0 {
                return Err(AxError::InvalidInput);
            }
            let file = positioned_file(fd, FileFlags::empty())?;
            if !file.inner().supports_seek() {
                return Err(LinuxError::ESPIPE.into());
            }
            if let Some(result) =
                tmp::seek_data_or_hole(file.inner().location(), offset as u64, whence == SEEK_HOLE)
            {
                let off = result?;
                seek_file_like(&seekable_fd(fd)?, off as __kernel_off_t, 0)?;
                return Ok(off as isize);
            }
            let off = generic_seek_data_or_hole(file.inner(), offset as u64, whence == SEEK_HOLE)?;
            seek_file_like(&seekable_fd(fd)?, off as __kernel_off_t, 0)?;
            Ok(off as isize)
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn generic_seek_data_or_hole(file: &axfs::File, offset: u64, seek_hole: bool) -> AxResult<u64> {
    let metadata = file.location().metadata()?;
    let size = metadata.size;
    if offset >= size {
        return Err(AxError::from(LinuxError::ENXIO));
    }

    let block_size = metadata.block_size.max(1) as usize;
    let mut buf = vec![0u8; block_size];
    let mut block = offset / block_size as u64;
    let last_block = size.div_ceil(block_size as u64);
    let mut result = offset;

    while block < last_block {
        let block_start = block * block_size as u64;
        let block_end = (block_start + block_size as u64).min(size);
        let valid = (block_end - block_start) as usize;
        let read = file.read_at(&mut buf[..valid], block_start)?;
        if read < valid {
            buf[read..valid].fill(0);
        }
        let is_data = buf[..valid].iter().any(|&byte| byte != 0);
        if seek_hole != is_data {
            return Ok(result.min(size));
        }

        block += 1;
        result = block * block_size as u64;
    }

    if seek_hole {
        Ok(size)
    } else {
        Err(AxError::from(LinuxError::ENXIO))
    }
}

pub fn sys_truncate(
    memory: UserMemoryCapability,
    path: *const c_char,
    length: __kernel_off_t,
) -> AxResult<isize> {
    let path = String::from_utf8(
        memory
            .load_until_nul(path.cast::<u8>())
            .map_err(map_usercopy_error)?,
    )
    .map_err(|_| AxError::IllegalBytes)?;
    debug!("sys_truncate <= {path:?} {length}");
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    if length < 0 {
        return Err(AxError::InvalidInput);
    }
    let curr = axtask::current();
    let proc_data = &curr.as_thread().proc_data;
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let loc = current_fs_context()
        .lock()
        .resolve_security(path, &security)?;
    check_open_permissions_with_security(
        &loc,
        W_OK,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )?;
    check_writable_mount(&loc)?;
    crate::mm::check_not_active(&loc)?;
    let _swap_mutation = crate::mm::admit_mutation(&loc)?;
    check_landlock_truncate(&loc)?;
    check_resize_limit(length as u64)?;
    // Unlike fd-backed mutations, path truncate has no persistent open-file
    // description carrying the ETXTBSY reference. Hold a transient write
    // reservation across every check and publication after admission so exec
    // credential sampling cannot start in the old check-then-truncate gap.
    let write_open_key = executable::retain_write_open(&loc)?;
    let truncate: AxResult<()> = (|| {
        let _memfd_mutation = memfd::begin_resize(&loc, length as u64)?;
        let _lease_admission = lease::admit_truncate(&loc)?;
        check_mandatory_truncate_lock(
            &loc,
            length as u64,
            flock::RecordLockOwner::Posix(proc_data.proc.pid()),
        )?;
        let file = OpenOptions::new()
            .write(true)
            .open_loc(loc.clone())?
            .into_file()?;
        let backend = file.access(FileFlags::WRITE)?;
        let _privilege_guard = begin_inode_content_write(&loc, &security)?;
        let quota_charge = admit_resize(&loc, loc.len()?, length as u64)?;
        backend.set_len(length as _)?;
        quota_charge.commit_actual_blocks(&loc)?;
        if let Err(error) = touch_modified_metadata(&loc) {
            warn!("truncate metadata update failed after size mutation: {error}");
        }
        Ok(())
    })();
    executable::release_write_open(write_open_key);
    truncate?;
    let _ = notify_exact(&loc, IN_MODIFY | IN_ATTRIB);
    Ok(0)
}

pub fn sys_ftruncate(fd: c_int, length: __kernel_off_t) -> AxResult<isize> {
    debug!("sys_ftruncate <= {fd} {length}");
    ftruncate_length_errno(length)?;
    let file_like = get_file_like(fd)?;
    let kind = FileLikeKind::from_file_like(file_like.as_ref());
    // Linux v6.12.103 fs/open.c uses fdget() without FMODE_PATH: an O_PATH
    // descriptor therefore fails the fd acquisition stage with EBADF.  The
    // subsequent do_ftruncate() checks S_ISREG and FMODE_WRITE, returning
    // EINVAL for either failure, all before the RLIMIT_FSIZE check below.
    ftruncate_admission_errno(true, kind, file_like.is_path_only(), true)?;
    if let Ok(secret) = file_like.downcast::<crate::file::SecretMemFile>() {
        secret.check_truncate()?;
        if (length as u64) > secret.size() {
            check_resize_limit(length as u64)?;
        }
        secret.truncate(length as u64)?;
        return Ok(0);
    }
    let security = current_vfs_security();
    let f = file_like.downcast::<File>()?;
    let backend = f.inner().access(FileFlags::WRITE).map_err(|error| match error {
        AxError::BadFileDescriptor => {
            ftruncate_admission_errno(true, FileLikeKind::Regular, false, false)
                .expect_err("read-only regular ftruncate must be EINVAL")
        }
        other => other,
    })?;
    check_writable_mount(f.inner().location())?;
    crate::mm::check_not_active(f.inner().location())?;
    let _swap_mutation = crate::mm::admit_mutation(f.inner().location())?;
    if !f.landlock_truncate_allowed() {
        return Err(AxError::PermissionDenied);
    }
    executable::check_not_active(f.inner().location())?;
    if (length as u64) > f.inner().location().len()? {
        check_resize_limit(length as u64)?;
    }
    let _memfd_mutation = memfd::begin_resize(f.inner().location(), length as u64)?;
    let _lease_admission = lease::admit_truncate(f.inner().location())?;
    let status = f.io_status_snapshot();
    check_mandatory_fd_truncate_lock(
        f.inner().location(),
        length as u64,
        f.open_file_description_key(),
        status.nonblocking(),
    )?;
    let _privilege_guard = begin_inode_content_write(f.inner().location(), &security)?;
    let quota_charge = admit_resize(f.inner().location(), f.inner().location().len()?, length as u64)?;
    backend.set_len(length as _)?;
    quota_charge.commit_actual_blocks(f.inner().location())?;
    if let Err(error) = touch_modified_metadata(f.inner().location()) {
        warn!("ftruncate metadata update failed after size mutation: {error}");
    }
    notify_write(fd);
    let _ = notify_exact(f.inner().location(), IN_ATTRIB);
    Ok(0)
}

fn ftruncate_admission_errno(
    fd_found: bool,
    kind: FileLikeKind,
    path_only: bool,
    writable: bool,
) -> AxResult {
    if !fd_found || path_only {
        return Err(AxError::BadFileDescriptor);
    }
    if kind != FileLikeKind::Regular || !writable {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn ftruncate_length_errno(length: __kernel_off_t) -> AxResult {
    if length < 0 {
        Err(AxError::InvalidInput)
    } else {
        Ok(())
    }
}

pub fn sys_fallocate(
    fd: c_int,
    mode: u32,
    offset: __kernel_off_t,
    len: __kernel_off_t,
) -> AxResult<isize> {
    debug!("sys_fallocate <= fd: {fd}, mode: {mode}, offset: {offset}, len: {len}");
    if offset < 0 || len <= 0 {
        return Err(AxError::InvalidInput);
    }
    let security = current_vfs_security();

    let file_like = get_file_like(fd)?;
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Regular => {}
        FileLikeKind::Directory => return Err(AxError::IsADirectory),
        FileLikeKind::Fifo | FileLikeKind::Socket | FileLikeKind::Other => {
            return Err(AxError::InvalidInput);
        }
    }

    let f = file_like.downcast::<File>()?;
    f.inner().access(FileFlags::WRITE)?;
    check_writable_mount(f.inner().location())?;

    let file = f.inner();
    let backend = file.backend()?;
    let loc = backend.location().clone();
    crate::mm::check_not_active(&loc)?;
    let _swap_mutation = crate::mm::admit_mutation(&loc)?;
    executable::check_not_active(&loc)?;
    let offset = offset as u64;
    let len = len as u64;
    let len_usize = usize::try_from(len).map_err(|_| AxError::from(LinuxError::EFBIG))?;
    let end = offset
        .checked_add(len)
        .ok_or_else(|| AxError::from(LinuxError::EFBIG))?;
    if end > MAX_FILE_OFFSET {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    let size = loc.len()?;
    let supported_modes = FALLOC_FL_KEEP_SIZE
        | FALLOC_FL_PUNCH_HOLE
        | FALLOC_FL_COLLAPSE_RANGE
        | FALLOC_FL_ZERO_RANGE
        | FALLOC_FL_INSERT_RANGE;

    if mode & !supported_modes != 0 {
        return Err(AxError::OperationNotSupported);
    }

    match mode {
        0 => {
            check_resize_limit(size.max(end))?;
            let _memfd_mutation = memfd::begin_resize(&loc, size.max(end))?;
            let _privilege_guard = begin_inode_content_write(&loc, &security)?;
            if tmp::supports_fallocate_range(&loc) {
                let quota_charge = admit_resize(&loc, size, size.max(end))?;
                tmp::reserve_fallocate_range(&loc, offset, len, true)
                    .ok_or(AxError::BadState)??;
                quota_charge.commit_actual_blocks(&loc)?;
            } else {
                let quota_charge = admit_resize(&loc, size, size.max(end))?;
                backend.set_len(size.max(end))?;
                quota_charge.commit_actual_blocks(&loc)?;
            }
        }
        FALLOC_FL_KEEP_SIZE => {
            let _memfd_mutation = memfd::begin_resize(&loc, size)?;
            let _privilege_guard = begin_inode_content_write(&loc, &security)?;
            if tmp::supports_fallocate_range(&loc) {
                // KEEP_SIZE may still allocate blocks beyond EOF. Reserve the
                // full possible extent, then settle against Metadata.blocks.
                let quota_charge = admit_resize(&loc, size, size.max(end))?;
                tmp::reserve_fallocate_range(&loc, offset, len, false)
                    .ok_or(AxError::BadState)??;
                quota_charge.commit_actual_blocks(&loc)?;
            }
        }
        mode if mode == (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) => {
            if !tmp::supports_fallocate_range(&loc) {
                return Err(AxError::OperationNotSupported);
            }
            let hole_len = end.min(size).saturating_sub(offset);
            let memfd_mutation = memfd::begin_write(&loc, len_usize)?;
            memfd_mutation.admit_write(
                &loc,
                size,
                offset,
                usize::try_from(hole_len).map_err(|_| AxError::from(LinuxError::EFBIG))?,
            )?;
            let _privilege_guard = begin_inode_content_write(&loc, &security)?;
            let quota_charge = admit_resize(&loc, size, size)?;
            write_zero_range(file, offset, hole_len)?;
            tmp::punch_hole_fallocate_range(&loc, offset, len).ok_or(AxError::BadState)??;
            quota_charge.commit_actual_blocks(&loc)?;
        }
        mode if mode == FALLOC_FL_ZERO_RANGE
            || mode == (FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE) =>
        {
            let zero_end = if mode & FALLOC_FL_KEEP_SIZE != 0 {
                end.min(size)
            } else {
                check_resize_limit(size.max(end))?;
                end
            };
            let zero_len = zero_end.saturating_sub(offset);
            let zero_len_usize =
                usize::try_from(zero_len).map_err(|_| AxError::from(LinuxError::EFBIG))?;
            let _memfd_mutation = if mode & FALLOC_FL_KEEP_SIZE != 0 {
                let mutation = memfd::begin_write(&loc, len_usize)?;
                mutation.admit_write(&loc, size, offset, zero_len_usize)?;
                mutation
            } else {
                memfd::begin_write_resize(&loc, offset, zero_len_usize, size.max(end))?
            };
            let _privilege_guard = begin_inode_content_write(&loc, &security)?;
            if mode & FALLOC_FL_KEEP_SIZE == 0 {
                let quota_charge = admit_resize(&loc, size, size.max(end))?;
                backend.set_len(size.max(end))?;
                quota_charge.commit_actual_blocks(&loc)?;
            }
            write_zero_range(file, offset, zero_len)?;
            if let Some(result) = tmp::reserve_fallocate_range(&loc, offset, zero_len, false) {
                result?;
            }
        }
        FALLOC_FL_COLLAPSE_RANGE => {
            if len == 0
                || !offset.is_multiple_of(TMPFS_FALLOC_BLOCK_SIZE)
                || !len.is_multiple_of(TMPFS_FALLOC_BLOCK_SIZE)
                || end > size
            {
                return Err(AxError::InvalidInput);
            }
            let _memfd_mutation = memfd::begin_write_resize(&loc, offset, len_usize, size - len)?;
            let _privilege_guard = begin_inode_content_write(&loc, &security)?;
            if let Some(result) = tmp::collapse_fallocate_range(&loc, offset, len) {
                result?;
            } else {
                copy_within_file(file, end, offset, size - end)?;
            }
            let quota_charge = admit_resize(&loc, size, size - len)?;
            backend.set_len(size - len)?;
            quota_charge.commit_actual_blocks(&loc)?;
        }
        FALLOC_FL_INSERT_RANGE => {
            if len == 0
                || !offset.is_multiple_of(TMPFS_FALLOC_BLOCK_SIZE)
                || !len.is_multiple_of(TMPFS_FALLOC_BLOCK_SIZE)
                || offset >= size
            {
                return Err(AxError::InvalidInput);
            }
            let new_size = size
                .checked_add(len)
                .filter(|new_size| *new_size <= MAX_FILE_OFFSET)
                .ok_or_else(|| AxError::from(LinuxError::EFBIG))?;
            check_resize_limit(new_size)?;
            let _memfd_mutation = memfd::begin_write_resize(&loc, offset, len_usize, new_size)?;
            let _privilege_guard = begin_inode_content_write(&loc, &security)?;
            let quota_charge = admit_resize(&loc, size, new_size)?;
            backend.set_len(new_size)?;
            quota_charge.commit_actual_blocks(&loc)?;
            if let Some(result) = tmp::insert_fallocate_range(&loc, offset, len) {
                result?;
            } else {
                copy_within_file_reverse(file, offset, offset + len, size - offset)?;
                write_zero_range(file, offset, len)?;
            }
        }
        _ => return Err(AxError::InvalidInput),
    }

    if let Err(error) = touch_modified_metadata(&loc) {
        warn!("fallocate metadata update failed after file mutation: {error}");
    }
    let _ = notify_exact(&loc, IN_MODIFY | IN_ATTRIB);
    Ok(0)
}

pub fn sys_fsync(fd: c_int) -> AxResult<isize> {
    debug!("sys_fsync <= {fd}");
    // Keep the Arc returned by this single lookup through completion: a close
    // followed by fd-number reuse must not redirect fsync to another OFD.
    let description = crate::file::get_file_description(fd)?;
    description.sync(false)?;
    Ok(0)
}

pub fn sys_fdatasync(fd: c_int) -> AxResult<isize> {
    debug!("sys_fdatasync <= {fd}");
    let description = crate::file::get_file_description(fd)?;
    description.sync(true)?;
    Ok(0)
}

pub fn sys_sync_file_range(
    fd: c_int,
    offset: __kernel_off_t,
    nbytes: __kernel_off_t,
    flags: u32,
) -> AxResult<isize> {
    debug!(
        "sys_sync_file_range <= fd: {fd}, offset: {offset}, nbytes: {nbytes}, flags: {flags:#x}"
    );

    // Linux resolves and checks the descriptor before inspecting the range,
    // so a bad descriptor wins over EINVAL from malformed arguments.
    let description = crate::file::get_file_description(fd)?;
    description.check_io_access()?;
    let file_like = &description.inner;
    // Range/flag validation follows the retained-fd lookup but precedes the
    // file-operation type check, matching Linux's valid-pipe EINVAL order.
    let end = validate_sync_file_range_args(offset, nbytes, flags)?;
    let node_type = file_like
        .downcast_ref::<File>()
        .map(|file| file.inner().location().node_type())
        .or_else(|| {
            file_like
                .downcast_ref::<Directory>()
                .map(|dir| dir.inner().node_type())
        });
    if !matches!(
        node_type,
        Some(NodeType::RegularFile | NodeType::Directory | NodeType::BlockDevice)
    ) {
        return Err(AxError::from(LinuxError::ESPIPE));
    }

    if flags != 0 && !matches!(node_type, Some(NodeType::Directory)) {
        let f = get_typed_file::<File>(fd)?;
        let len = if end == 0 { 0 } else { end - offset as u64 };
        let finish_wait = |wait: AxResult<()>| {
            // file_fdatawait_range advances the OFD errseq even when its
            // primary wait failed/interrupted.  Preserve that primary errno
            // while consuming a concurrent mapping error exactly once.
            let errseq = description.check_and_advance_writeback_error();
            wait.and(errseq)
        };
        // WAIT_BEFORE observes only requests accepted before this invocation.
        // WRITE merely publishes an inode-shared request; WAIT_AFTER includes
        // the request just published by this syscall.
        let before = f.inner().range_writeback_snapshot()?;
        if flags & SYNC_FILE_RANGE_WAIT_BEFORE != 0 {
            finish_wait(
                f.inner()
                    .wait_range_writeback_through(&before, offset as u64, len),
            )?;
        }
        if flags & SYNC_FILE_RANGE_WRITE != 0 {
            let write_fence = f.inner().submit_range_writeback(offset as u64, len, true)?;
            if flags & SYNC_FILE_RANGE_WAIT_AFTER != 0 {
                finish_wait(f.inner().wait_range_writeback_through(
                    &write_fence,
                    offset as u64,
                    len,
                ))?;
            }
        }
        if flags & SYNC_FILE_RANGE_WAIT_AFTER != 0 && flags & SYNC_FILE_RANGE_WRITE == 0 {
            let after = f.inner().range_writeback_snapshot()?;
            finish_wait(
                f.inner()
                    .wait_range_writeback_through(&after, offset as u64, len),
            )?;
        }
    }
    Ok(0)
}

pub fn sys_fadvise64(
    fd: c_int,
    offset: __kernel_off_t,
    len: __kernel_off_t,
    advice: u32,
) -> AxResult<isize> {
    debug!("sys_fadvise64 <= fd: {fd}, offset: {offset}, len: {len}, advice: {advice}");
    // fdget comes first in Linux too: a bad descriptor wins over malformed
    // range/advice arguments. Keep this exact retained OFD for every later
    // check so close-and-reuse cannot change the target half way through.
    let file_like = get_file_like(fd)?;
    file_like.check_io_access()?;
    // Pipes have no seekable mapping.  Linux reports this before considering
    // the hint payload; sockets, on the other hand, are accepted as a
    // mapping-less no-op.
    if FileLikeKind::from_file_like(file_like.as_ref()) == FileLikeKind::Fifo {
        return Err(AxError::from(LinuxError::ESPIPE));
    }
    if offset < 0 || len < 0 {
        return Err(AxError::InvalidInput);
    }
    if advice > 5 {
        return Err(AxError::InvalidInput);
    }
    let Some(file) = file_like.downcast_ref::<File>() else {
        return Ok(0);
    };
    file.inner().access(FileFlags::empty())?;

    let offset = offset as u64;
    let len = len as u64;
    let end = if len == 0 {
        // A zero length extends through the EOF sampled for this operation.
        file.inner().location().metadata()?.size
    } else {
        offset.checked_add(len).ok_or(AxError::InvalidInput)?
    };
    let effective_len = end.saturating_sub(offset);
    match advice {
        0 => file.inner().set_fadvise_readahead(FadviseReadahead::Normal),
        1 => file.inner().set_fadvise_readahead(FadviseReadahead::Random),
        2 => file
            .inner()
            .set_fadvise_readahead(FadviseReadahead::Sequential),
        3 => file.inner().fadvise_willneed(offset, effective_len)?,
        // DONTNEED is an eviction hint.  Pins, aliases and a concurrent
        // writeback can make a particular range ineligible; that must retain
        // the data rather than turn a successful hint into a syscall error.
        4 => {
            let _ = file.inner().fadvise_dontneed(offset, effective_len);
        }
        5 => {
            file.inner()
                .set_fadvise_readahead(FadviseReadahead::NoReuse);
            file.inner().fadvise_noreuse(offset, effective_len)?;
        }
        _ => unreachable!("advice validated above"),
    }
    Ok(0)
}

pub fn sys_pread64(
    capability: UserMemoryCapability,
    fd: c_int,
    buf: *mut u8,
    len: usize,
    offset: __kernel_off_t,
) -> AxResult<isize> {
    if offset < 0 {
        return Err(AxError::InvalidInput);
    }
    let f = positioned_read_file(fd)?;
    let context = current_io_operation_context(&f);
    validate_direct_io(&f, buf as usize, len, offset as u64)?;
    if len != 0 {
        permission_check_file_like_with_actor_and_status(
            &f.clone().into_file_like(),
            crate::file::fanotify::FAN_ACCESS_PERM,
            context.fanotify_actor(),
            context.status(),
        )?;
    }
    f.with_read_credentials(|| {
        pread64_file_with_context(&capability, &f, &context, buf, len, offset as u64, None)
    })
}

/// Consumes an owned physical admission. No policy, RLIMIT, or task state is
/// sampled here. A lower-layer `NotSubmitted` is terminal for this token;
/// the submitter must only publish a token after the lower owned plan proves
/// that the generic fallback is no longer part of the operation.
pub(crate) fn io_uring_pread64_worker(
    admission: PreparedPhysicalIoAdmission,
) -> AxResult<IoUringWorkerResult> {
    let result = (|| {
        let (file_lease, buffer_lease, context, plan, _memfd, _privilege, effect) =
            admission.into_parts();
        // NotSubmitted proves that no device descriptor became visible. Drop
        // the unpublished effect before touching the synchronous path so its
        // range lease and staged cache invalidation are rolled back first.
        // Otherwise the fallback could conflict with its own range lease or
        // restore stale cache pages after a write.
        drop(effect);
        if plan.operation() != PreparedPhysicalIoOperation::Read {
            return Err(AxError::BadState);
        }
        let description = file_lease.description()?;
        context.validate_for(description)?;
        let file = description
            .file_handle()
            .downcast::<File>()
            .map_err(|_| AxError::BadFileDescriptor)?;
        let (address, length) = buffer_lease.range()?;
        if usize::try_from(address).map_err(|_| AxError::BadAddress)? != plan.address()
            || usize::try_from(length).map_err(|_| AxError::BadAddress)? != plan.requested_len()
        {
            return Err(AxError::BadAddress);
        }
        let (segments, offset_in_segments, fixed_len, disjoint) = buffer_lease.physical_range()?;
        if fixed_len < plan.allowed_len() {
            return Err(AxError::BadAddress);
        }
        // The worker's fallback is deliberately the pinned bounce path. The
        // legacy PhysicalIoAttempt hook may publish an exact descriptor whose
        // owner cannot retain this io_uring buffer lease after a VFS error;
        // only the broker-owned admission above may publish asynchronously.
        crate::file::io_uring::record_io_uring_dma_direct_read_fallback(
            crate::file::io_uring::IoUringDmaFallbackReason::DeviceAdmission,
        );
        let read = read_at_fixed_user_segments(
            file.as_ref(),
            segments,
            offset_in_segments,
            plan.allowed_len(),
            plan.offset(),
            disjoint,
        )?;
        if read != plan.allowed_len() {
            return Err(AxError::Io);
        }
        if read > 0 {
            notify_read_file_with_actor(file.as_ref(), context.fanotify_actor());
        }
        Ok(read as isize)
    })();
    Ok(match result {
        Ok(result) => IoUringWorkerResult::Completed(result),
        Err(error) => IoUringWorkerResult::Failed(error),
    })
}

/// Executes the synchronous fallback for an admitted physical write when the
/// submitter reserved no device route (`NotSubmitted`, including queue-full
/// backpressure).  The prepared effect is still unpublished and is dropped
/// with the exact file/buffer/policy leases; the actual write uses the
/// ordinary cache-aware pinned source path, so no policy is sampled again.
pub(crate) fn io_uring_pwrite64_worker(
    admission: PreparedPhysicalIoAdmission,
) -> AxResult<IoUringWorkerResult> {
    let result = (|| {
        let (file_lease, buffer_lease, context, plan, memfd, privilege, effect) =
            admission.into_parts();
        // See the read fallback: an unpublished effect is ordinary rollback
        // state, not a live DMA owner. Release its range/cache transaction
        // before the cache-aware synchronous write begins.
        drop(effect);
        if plan.operation() != PreparedPhysicalIoOperation::Write {
            return Err(AxError::BadState);
        }
        let _memfd = memfd.ok_or(AxError::BadState)?;
        let _privilege = privilege.ok_or(AxError::BadState)?;
        let description = file_lease.description()?;
        context.validate_for(description)?;
        let file = description
            .file_handle()
            .downcast::<File>()
            .map_err(|_| AxError::BadFileDescriptor)?;
        let (address, length) = buffer_lease.range()?;
        if usize::try_from(address).map_err(|_| AxError::BadAddress)? != plan.address()
            || usize::try_from(length).map_err(|_| AxError::BadAddress)? != plan.requested_len()
        {
            return Err(AxError::BadAddress);
        }
        let (segments, offset_in_segments, fixed_len, _) = buffer_lease.physical_range()?;
        if fixed_len < plan.allowed_len() {
            return Err(AxError::BadAddress);
        }
        crate::file::io_uring::record_io_uring_dma_direct_write_fallback(
            crate::file::io_uring::IoUringDmaFallbackReason::DeviceAdmission,
        );
        let mut source = PinnedPhysicalReader::from_validated_range(
            segments,
            offset_in_segments,
            plan.allowed_len(),
        );
        let written = file.write_at_with_status_and_direct_validation(
            context.status(),
            &mut source,
            plan.offset(),
            context.security(),
            |write_offset, write_len| {
                validate_direct_io(file.as_ref(), plan.address(), write_len, write_offset)
            },
        )?;
        if written > 0 {
            sync_file_after_status_write(context.status(), &file)?;
            notify_write_file_with_actor(file.as_ref(), context.fanotify_actor());
        }
        Ok(written as isize)
    })();
    Ok(match result {
        Ok(result) => IoUringWorkerResult::Completed(result),
        Err(error) => IoUringWorkerResult::Failed(error),
    })
}

fn pread64_file_with_context(
    capability: &UserMemoryCapability,
    f: &FileHandle<File>,
    context: &IoOperationContext,
    buf: *mut u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> AxResult<isize> {
    let status = context.status();
    validate_direct_io(f.as_ref(), buf as usize, len, offset)?;
    f.check_io_status(status)?;
    let fast_read = if let Some((segments, offset_in_segments, fixed_len, disjoint, provenance)) =
        fixed_segments
    {
        // Fixed-buffer physical publication is owned exclusively by the
        // broker admission in `io_uring.rs`. If admission did not produce a
        // worker token, keep this path as the bounded pinned bounce fallback;
        // the legacy PhysicalIoAttempt hook cannot retain the registered
        // buffer lease across a post-publication VFS error.
        crate::file::io_uring::record_io_uring_dma_direct_read_fallback(fixed_dma_fallback_reason(
            buf as usize,
            fixed_len,
            offset,
            segments,
            offset_in_segments,
            disjoint,
            provenance,
        ));
        Some(read_at_fixed_user_segments(
            f.as_ref(),
            segments,
            offset_in_segments,
            fixed_len,
            offset,
            disjoint,
        )?)
    } else {
        match try_regular_file_pread_user_slice(capability, f.as_ref(), buf, len, offset)? {
            Some(read) => Some(read),
            None => try_regular_file_pread_user_segments(capability, f.as_ref(), buf, len, offset)?,
        }
    };
    if let Some(read) = fast_read {
        if read > 0 {
            notify_read_file_with_actor(f.as_ref(), context.fanotify_actor());
        }
        return Ok(read as _);
    }
    if len >= USER_COPY_PREFAULT_MIN {
        prefault_regular_file_read_fallback(capability, f.as_ref(), buf, len, offset)?;
    }
    let read = f
        .inner()
        .read_at(VmBytesMut::new(capability.clone(), buf, len), offset as _)?;
    if read > 0 {
        notify_read_file_with_actor(f.as_ref(), context.fanotify_actor());
    }
    Ok(read as _)
}

/// Executes an io_uring read on the submitting task using a context captured
/// at SQE admission.  The credential wrapper is intentional: generic VFS,
/// procfs, and cgroup files still require the original task-local Linux
/// `with_read_credentials` semantics and are not worker operations.
pub(crate) fn io_uring_pread64_submission(
    capability: &UserMemoryCapability,
    description: &Arc<FileDescription>,
    context: &IoOperationContext,
    buf: *mut u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> AxResult<isize> {
    io_uring_pread64_submission_with_mode(
        capability,
        description,
        context,
        buf,
        len,
        offset,
        fixed_segments,
        false,
    )
}

/// Executes one io_uring read with a task-local nonblocking attempt for the
/// narrow pending-stream admission. The mode is an operation override only;
/// it never changes the file's OFD status flags.
pub(crate) fn io_uring_pread64_submission_nonblocking_stream(
    capability: &UserMemoryCapability,
    description: &Arc<FileDescription>,
    context: &IoOperationContext,
    buf: *mut u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> AxResult<isize> {
    io_uring_pread64_submission_with_mode(
        capability,
        description,
        context,
        buf,
        len,
        offset,
        fixed_segments,
        true,
    )
}

fn io_uring_pread64_submission_with_mode(
    capability: &UserMemoryCapability,
    description: &Arc<FileDescription>,
    context: &IoOperationContext,
    buf: *mut u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
    force_nonblocking_stream: bool,
) -> AxResult<isize> {
    context.validate_for(description)?;
    let file_handle = description.file_handle();
    if !(offset == 0 && zero_offset_stream_file_like(&file_handle, NodeFlags::NO_POSITIONED_READ)) {
        let file = positioned_read_file_handle(file_handle.clone())?;
        validate_direct_io(&file, buf as usize, len, offset)?;
    } else {
        file_handle.check_io_status(context.status())?;
        let _ = PinnedSocketDescription::from_file_handle(&file_handle, context.status())?;
    }
    if len != 0 {
        permission_check_file_like_with_actor_and_status(
            &file_handle,
            crate::file::fanotify::FAN_ACCESS_PERM,
            context.fanotify_actor(),
            context.status(),
        )?;
    }
    file_handle.with_read_credentials(|| {
        execute_io_uring_pread(
            capability,
            description,
            context,
            buf,
            len,
            offset,
            fixed_segments,
            force_nonblocking_stream,
        )
    })
}

/// Executes the submission-task implementation after its caller has installed
/// the original credential view and validated the exact OFD context.
///
/// The context must have been captured from this exact description before
/// submission.  This implementation is intentionally restricted to the
/// submitting task; the worker-safe physical path is a separate, narrower
/// entry point and never reaches this generic implementation.
fn execute_io_uring_pread(
    capability: &UserMemoryCapability,
    description: &Arc<FileDescription>,
    context: &IoOperationContext,
    buf: *mut u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
    force_nonblocking_stream: bool,
) -> AxResult<isize> {
    let file_handle = description.file_handle();
    // Linux's io_uring rw path treats a zero offset on a non-seekable file
    // (pipe, socket, tty) as "no offset" and performs a plain read instead
    // of failing with ESPIPE (verified on the host kernel: READ_FIXED on an
    // empty nonblocking pipe with off=0 completes rather than failing).
    if offset == 0 && zero_offset_stream_file_like(&file_handle, NodeFlags::NO_POSITIONED_READ) {
        return io_uring_stream_read_with_context(
            capability,
            &file_handle,
            context,
            buf,
            len,
            fixed_segments,
            force_nonblocking_stream,
        );
    }
    let file = positioned_read_file_handle(file_handle)?;
    pread64_file_with_context(capability, &file, context, buf, len, offset, fixed_segments)
}

pub fn sys_pwrite64(
    capability: UserMemoryCapability,
    fd: c_int,
    buf: *const u8,
    len: usize,
    offset: __kernel_off_t,
) -> AxResult<isize> {
    if offset < 0 {
        return Err(AxError::InvalidInput);
    }
    // Validate the descriptor and FMODE_PWRITE-equivalent capability even
    // for a zero-length request. Proc id-map controls return ESPIPE here
    // rather than a silent zero-byte success.
    let f = positioned_write_file(fd)?;
    if len == 0 {
        return Ok(0);
    }
    let context = current_io_operation_context(&f);
    f.with_write_credentials_for_status(context.status(), || {
        pwrite64_file_with_context(&capability, &f, &context, buf, len, offset as u64, None)
    })
}

fn pwrite64_file_with_context(
    capability: &UserMemoryCapability,
    f: &FileHandle<File>,
    context: &IoOperationContext,
    buf: *const u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> AxResult<isize> {
    if len == 0 {
        return Ok(0);
    }
    let security = context.security();
    let status = context.status();
    f.check_io_status(status)?;
    let written = if write_uses_inode_append(f.inner(), status) {
        if let Some((segments, offset_in_segments, fixed_len, ..)) = fixed_segments {
            let mut source =
                PinnedPhysicalReader::from_validated_range(segments, offset_in_segments, fixed_len);
            f.write_at_end_with_status_and_direct_validation(
                status,
                &mut source,
                security,
                |append_offset, allowed| {
                    validate_direct_io(f.as_ref(), buf as usize, allowed, append_offset)
                },
            )?
        } else {
            f.write_at_end_with_status_and_direct_validation(
                status,
                &mut VmBytes::new(capability.clone(), buf, len),
                security,
                |append_offset, allowed| {
                    validate_direct_io(f.as_ref(), buf as usize, allowed, append_offset)
                },
            )?
        }
    } else {
        let allowed = allowed_write_len(offset, len)?;
        let fast_written =
            if let Some((segments, offset_in_segments, fixed_len, disjoint, provenance)) =
                fixed_segments
            {
                crate::file::io_uring::record_io_uring_dma_direct_write_fallback(
                    fixed_dma_fallback_reason(
                        buf as usize,
                        allowed.min(fixed_len),
                        offset,
                        segments,
                        offset_in_segments,
                        disjoint,
                        provenance,
                    ),
                );
                if regular_file_supports_user_slice_fast_path(f.as_ref()) {
                    executable::check_not_active(f.inner().location())?;
                    // The fixed-buffer fallback bypasses File's ordinary
                    // write admission, so retain the swap mutation token
                    // until its direct pinned-segment backend effect returns.
                    let _swap_mutation = crate::mm::admit_mutation(f.inner().location())?;
                    if allowed == 0 {
                        Some(0)
                    } else {
                        validate_direct_io(f.as_ref(), buf as usize, allowed, offset)?;
                        let _memfd_mutation =
                            reserve_memfd_positioned_write(f.as_ref(), offset, allowed)?;
                        let _privilege_guard =
                            f.as_ref().begin_content_write_privilege_cleanup(security)?;
                        Some(write_at_fixed_user_segments(
                            f.as_ref(),
                            segments,
                            offset_in_segments,
                            allowed.min(fixed_len),
                            offset,
                        )?)
                    }
                } else {
                    None
                }
            } else {
                match try_regular_file_pwrite_user_slice(
                    capability,
                    f.as_ref(),
                    status,
                    security,
                    buf,
                    len,
                    offset,
                )? {
                    Some(written) => Some(written),
                    None => try_regular_file_pwrite_user_segments(
                        capability,
                        f.as_ref(),
                        status,
                        security,
                        buf,
                        len,
                        offset,
                    )?,
                }
            };
        if let Some(written) = fast_written {
            written
        } else {
            if allowed >= USER_COPY_PREFAULT_MIN && fixed_segments.is_none() {
                prefault_regular_file_write_fallback(capability, f.as_ref(), buf, allowed)?;
            }
            if let Some((segments, offset_in_segments, fixed_len, ..)) = fixed_segments {
                let mut source = PinnedPhysicalReader::from_validated_range(
                    segments,
                    offset_in_segments,
                    fixed_len,
                );
                f.write_at_with_status_and_direct_validation(
                    status,
                    &mut source,
                    offset,
                    security,
                    |write_offset, write_len| {
                        validate_direct_io(f.as_ref(), buf as usize, write_len, write_offset)
                    },
                )?
            } else {
                f.write_at_with_status_and_direct_validation(
                    status,
                    &mut VmBytes::new(capability.clone(), buf, len),
                    offset,
                    security,
                    |write_offset, write_len| {
                        validate_direct_io(f.as_ref(), buf as usize, write_len, write_offset)
                    },
                )?
            }
        }
    };
    if written > 0 {
        sync_file_after_status_write(status, f)?;
        notify_write_file_with_actor(f.as_ref(), context.fanotify_actor());
    }
    Ok(written as _)
}

/// Executes an io_uring write on the submitting task using a context captured
/// at SQE admission.  Generic stream/procfs/cgroup operations stay on this
/// path so their `with_write_credentials` task-local view remains intact.
pub(crate) fn io_uring_pwrite64_submission(
    capability: &UserMemoryCapability,
    description: &Arc<FileDescription>,
    context: &IoOperationContext,
    buf: *const u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> AxResult<isize> {
    context.validate_for(description)?;
    let file_handle = description.file_handle();
    if offset == 0 && zero_offset_stream_file_like(&file_handle, NodeFlags::NO_POSITIONED_WRITE) {
        file_handle.check_io_status(context.status())?;
        let _ = PinnedSocketDescription::from_file_handle(&file_handle, context.status())?;
    } else {
        let _ = positioned_write_file_handle(file_handle.clone())?;
    }
    file_handle.with_write_credentials_for_status(context.status(), || {
        execute_io_uring_pwrite(
            capability,
            description,
            context,
            buf,
            len,
            offset,
            fixed_segments,
        )
    })
}

/// Executes the submission-task implementation after its caller has installed
/// the original credential view and validated the exact OFD context.  This is
/// deliberately not a worker entry point: generic streams and pseudo-files
/// may consult task-local policy while running below this function.
fn execute_io_uring_pwrite(
    capability: &UserMemoryCapability,
    description: &Arc<FileDescription>,
    context: &IoOperationContext,
    buf: *const u8,
    len: usize,
    offset: u64,
    fixed_segments: Option<IoUringFixedSegments<'_>>,
) -> AxResult<isize> {
    let file_handle = description.file_handle();
    // Mirror the pread64 treatment: a zero offset on a non-seekable file
    // performs a plain write, matching Linux's io_uring rw behavior.
    if offset == 0 && zero_offset_stream_file_like(&file_handle, NodeFlags::NO_POSITIONED_WRITE) {
        return io_uring_stream_write_with_context(
            capability,
            &file_handle,
            context,
            buf,
            len,
            fixed_segments,
        );
    }
    let file = positioned_write_file_handle(file_handle)?;
    pwrite64_file_with_context(capability, &file, context, buf, len, offset, fixed_segments)
}

pub fn sys_preadv(
    capability: UserMemoryCapability,
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset: __kernel_off_t,
) -> AxResult<isize> {
    do_preadv(&capability, fd, iov, iovcnt, offset, 0, false)
}

pub fn sys_pwritev(
    capability: UserMemoryCapability,
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset: __kernel_off_t,
) -> AxResult<isize> {
    do_pwritev(&capability, fd, iov, iovcnt, offset, 0, false)
}

pub fn sys_preadv2(
    capability: UserMemoryCapability,
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset_low: i32,
    offset_high: i32,
    _flags: u32,
) -> AxResult<isize> {
    let offset = ((offset_high as i64) << 32) | (offset_low as u32 as i64);
    debug!(
        "sys_preadv2 <= fd: {fd}, iovcnt: {iovcnt}, offset_low: {offset_low}, offset_high: \
         {offset_high}, flags: {_flags}"
    );
    do_preadv(&capability, fd, iov, iovcnt, offset, _flags, true)
}

pub fn sys_pwritev2(
    capability: UserMemoryCapability,
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset_low: i32,
    offset_high: i32,
    _flags: u32,
) -> AxResult<isize> {
    let offset = ((offset_high as i64) << 32) | (offset_low as u32 as i64);
    debug!(
        "sys_pwritev2 <= fd: {fd}, iovcnt: {iovcnt}, offset_low: {offset_low}, offset_high: \
         {offset_high}, flags: {_flags}"
    );
    do_pwritev(&capability, fd, iov, iovcnt, offset, _flags, true)
}

enum SendFile {
    Direct {
        file: FileHandle<dyn FileLike>,
        status: OfdIoStatus,
        nonblocking: bool,
        security: VfsSecurityContext,
        capability: UserMemoryCapability,
    },
    Offset {
        file: FileHandle<File>,
        offset: u64,
        user_offset: *mut u64,
        status: OfdIoStatus,
        security: VfsSecurityContext,
        capability: UserMemoryCapability,
    },
}

struct TransferDestination<'a> {
    file: &'a mut SendFile,
    mandatory_len: usize,
    mandatory: &'a mut MandatoryTransferState,
    attempted: bool,
}

impl TransferDestination<'_> {
    fn preflight(&mut self) -> AxResult<()> {
        match self
            .file
            .preflight_mandatory_write(self.mandatory_len, self.mandatory)
        {
            Err(AxError::WouldBlock) => {
                self.attempted = true;
                Err(AxError::WouldBlock)
            }
            result => result,
        }
    }

    fn write(&mut self, data: &[u8]) -> AxResult<usize> {
        self.attempted = true;
        self.file
            .write(data, true, self.mandatory_len, self.mandatory)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SendStep {
    written: usize,
    destination_short: bool,
}

struct SendAttempt {
    result: AxResult<SendStep>,
    wait_endpoint: TransferWaitEndpoint,
    mandatory_wait: Option<flock::MandatoryLockWait>,
}

struct TransferWriter<'a, F> {
    write: &'a mut F,
    remaining: usize,
    written: usize,
    destination_short: bool,
}

impl<'a, F> TransferWriter<'a, F> {
    fn new(limit: usize, write: &'a mut F) -> Self {
        Self {
            write,
            remaining: limit,
            written: 0,
            destination_short: false,
        }
    }
}

impl<F: FnMut(&[u8]) -> AxResult<usize>> Write for TransferWriter<'_, F> {
    fn write(&mut self, buf: &[u8]) -> AxResult<usize> {
        if self.remaining == 0 || self.destination_short {
            return Ok(0);
        }
        let offered = buf.len().min(self.remaining);
        let written = match (self.write)(&buf[..offered]) {
            Ok(written) => written,
            // Stream transports may expose a wrapped receive queue through
            // more than one writer call. Once the destination accepted a
            // prefix, a later error must complete that prefix instead of
            // making the source retain it and duplicate it on retry.
            Err(_) if self.written > 0 => {
                self.destination_short = true;
                return Ok(0);
            }
            Err(error) => return Err(error),
        };
        if written > offered {
            return Err(AxError::InvalidInput);
        }
        self.written += written;
        self.remaining -= written;
        self.destination_short = written < offered;
        Ok(written)
    }

    fn flush(&mut self) -> AxResult<()> {
        Ok(())
    }
}

impl<F> IoBufMut for TransferWriter<'_, F> {
    fn remaining_mut(&self) -> usize {
        self.remaining
    }
}

fn checked_offset_advance(offset: u64, len: usize) -> AxResult<u64> {
    let offset = offset
        .checked_add(len as u64)
        .ok_or_else(|| AxError::from(LinuxError::EOVERFLOW))?;
    if offset > MAX_FILE_OFFSET {
        return Err(AxError::from(LinuxError::EOVERFLOW));
    }
    Ok(offset)
}

const fn transfer_attempt_lock_index(ofd_key: u64) -> usize {
    let mixed = ofd_key ^ ofd_key.rotate_left(21) ^ ofd_key.rotate_right(17);
    mixed as usize & (TRANSFER_ATTEMPT_LOCK_COUNT - 1)
}

const fn ordered_transfer_attempt_lock_indices(
    source_key: u64,
    destination_key: u64,
) -> (usize, usize) {
    let source = transfer_attempt_lock_index(source_key);
    let destination = transfer_attempt_lock_index(destination_key);
    if source <= destination {
        (source, destination)
    } else {
        (destination, source)
    }
}

fn with_transfer_attempt_locks<T>(
    source_key: u64,
    destination_key: u64,
    transfer: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    let (first, second) = ordered_transfer_attempt_lock_indices(source_key, destination_key);

    let _first = TRANSFER_ATTEMPT_LOCKS[first].lock();
    if first == second {
        transfer()
    } else {
        let _second = TRANSFER_ATTEMPT_LOCKS[second].lock();
        transfer()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferWaitEndpoint {
    Source,
    Destination,
}

const fn transfer_wait_endpoint(destination_attempted: bool) -> TransferWaitEndpoint {
    if destination_attempted {
        TransferWaitEndpoint::Destination
    } else {
        TransferWaitEndpoint::Source
    }
}

fn wait_for_file_like(
    file: &FileHandle<dyn FileLike>,
    event: IoEvents,
    nonblocking: bool,
) -> AxResult<()> {
    let events = event | IoEvents::ERROR | IoEvents::HANGUP;
    block_on_poll_io(file.as_ref(), events, nonblocking, || {
        if file.poll().intersects(events) {
            Ok(())
        } else {
            Err(AxError::WouldBlock)
        }
    })
}

impl SendFile {
    fn ofd_key(&self) -> u64 {
        match self {
            Self::Direct { file, .. } => file.open_file_description_key(),
            Self::Offset { file, .. } => file.open_file_description_key(),
        }
    }

    fn nonblocking(&self) -> bool {
        match self {
            Self::Direct { nonblocking, .. } => *nonblocking,
            Self::Offset { status, .. } => status.nonblocking(),
        }
    }

    fn mandatory_nonblocking(&self) -> bool {
        match self {
            Self::Direct { status, .. } | Self::Offset { status, .. } => status.nonblocking(),
        }
    }

    fn current_regular_file(&self) -> Option<FileHandle<File>> {
        match self {
            Self::Direct { file, .. } => file.downcast::<File>().ok(),
            Self::Offset { .. } => None,
        }
    }

    fn positioned_current(&self, offset: u64) -> AxResult<Self> {
        match self {
            Self::Direct {
                file,
                status,
                security,
                capability,
                ..
            } => Ok(Self::Offset {
                file: file.downcast::<File>()?,
                offset,
                // This cursor is committed by axfs's outer operation
                // transaction, never through userspace copyout.
                user_offset: core::ptr::null_mut(),
                status: *status,
                security: security.clone(),
                capability: capability.clone(),
            }),
            Self::Offset { .. } => Err(AxError::BadState),
        }
    }

    fn positioned_offset(&self) -> AxResult<u64> {
        match self {
            Self::Offset { offset, .. } => Ok(*offset),
            Self::Direct { .. } => Err(AxError::BadState),
        }
    }

    fn positioned_advance_from(&self, start: u64) -> AxResult<usize> {
        let end = self.positioned_offset()?;
        let advance = end.checked_sub(start).ok_or(AxError::BadState)?;
        usize::try_from(advance).map_err(|_| AxError::BadState)
    }

    fn socket_retry_handle(&self) -> Option<FileHandle<dyn FileLike>> {
        match self {
            Self::Direct { file, .. } if file.downcast_ref::<Socket>().is_some() => {
                Some(file.clone())
            }
            _ => None,
        }
    }

    fn preflight_mandatory_write(
        &self,
        mandatory_len: usize,
        mandatory: &mut MandatoryTransferState,
    ) -> AxResult<()> {
        match self {
            Self::Direct { file, .. } => {
                let Some(regular) = file.downcast_ref::<File>() else {
                    mandatory.admitted = true;
                    return Ok(());
                };
                let ofd_key = file.open_file_description_key();
                regular.inner().with_current_position(|offset| {
                    admit_mandatory_transfer_range(
                        mandatory,
                        regular.inner().location(),
                        ofd_key,
                        flock::MandatoryAccess::Write,
                        offset,
                        mandatory_len,
                    )
                })
            }
            Self::Offset { file, offset, .. } => admit_mandatory_transfer_range(
                mandatory,
                file.inner().location(),
                file.open_file_description_key(),
                flock::MandatoryAccess::Write,
                *offset,
                mandatory_len,
            ),
        }
    }

    fn wait_for(&self, event: IoEvents) -> AxResult<()> {
        match self {
            Self::Direct {
                file, nonblocking, ..
            } => wait_for_file_like(file, event, *nonblocking),
            // Explicit offsets are admitted only for regular files, whose
            // transfer readiness is unconditional. A backend error is not a
            // readiness wait and must be returned by the one-shot attempt.
            Self::Offset { .. } => Ok(()),
        }
    }

    fn has_data(&self) -> bool {
        match self {
            SendFile::Direct { file, .. } => file.poll(),
            SendFile::Offset { file, .. } => file.poll(),
        }
        .contains(IoEvents::READABLE)
    }

    fn transfer_with(
        &mut self,
        buf: &mut [u8],
        force_nonblocking: bool,
        mandatory_len: usize,
        mandatory: &mut MandatoryTransferState,
        destination: &mut TransferDestination<'_>,
    ) -> AxResult<SendStep> {
        match self {
            SendFile::Direct {
                file,
                status: _,
                nonblocking,
                ..
            } => file.with_read_credentials(|| {
                let nonblocking = *nonblocking || force_nonblocking;
                if let Some(pipe) = file.downcast_ref::<Pipe>() {
                    destination.preflight()?;
                    let (written, destination_short) =
                        pipe.splice_read_with(buf, nonblocking, |data| destination.write(data))?;
                    return Ok(SendStep {
                        written,
                        destination_short,
                    });
                }

                if let Some(pipe) = file.downcast_ref::<NamedPipe>() {
                    destination.preflight()?;
                    let (written, destination_short) =
                        pipe.splice_read_with(buf, nonblocking, |data| destination.write(data))?;
                    return Ok(SendStep {
                        written,
                        destination_short,
                    });
                }

                if let Some(regular) = file.downcast_ref::<File>() {
                    let mut offered = 0usize;
                    let ofd_key = file.open_file_description_key();
                    let written = regular.inner().read_slice_at_current_checked_with(
                        buf,
                        destination,
                        |destination, offset| {
                            admit_mandatory_transfer_range(
                                mandatory,
                                regular.inner().location(),
                                ofd_key,
                                flock::MandatoryAccess::Read,
                                offset,
                                mandatory_len,
                            )?;
                            destination.preflight()
                        },
                        |destination, data, _offset| {
                            offered = data.len();
                            destination.write(data)
                        },
                    )?;
                    if written > offered {
                        return Err(AxError::InvalidInput);
                    }
                    Ok(SendStep {
                        written,
                        destination_short: written < offered,
                    })
                } else if let Some(socket) = file.downcast_ref::<Socket>() {
                    destination.preflight()?;
                    // Socket recv implementations call the supplied writer
                    // while holding their receive-queue transaction and only
                    // consume the returned prefix. Route that writer directly
                    // into the destination instead of pre-consuming into the
                    // intermediate buffer.
                    let mut write = |data: &[u8]| destination.write(data);
                    let mut writer = TransferWriter::new(buf.len(), &mut write);
                    let received = socket.read_with_nonblocking(&mut writer, nonblocking)?;
                    if received != writer.written {
                        return Err(AxError::InvalidInput);
                    }
                    Ok(SendStep {
                        written: writer.written,
                        destination_short: writer.destination_short,
                    })
                } else {
                    destination.preflight()?;
                    // Non-seekable non-pipe inputs currently have no generic
                    // reservation primitive. Keep their established behavior;
                    // regular files and pipes, which are the supported
                    // sendfile/copy and splice sources, use commit-after-write
                    // paths above.
                    let mut dst = &mut *buf;
                    let offered = file.read(&mut dst)?;
                    if offered == 0 {
                        return Ok(SendStep {
                            written: 0,
                            destination_short: false,
                        });
                    }
                    let written = destination.write(&buf[..offered])?;
                    if written > offered {
                        return Err(AxError::InvalidInput);
                    }
                    Ok(SendStep {
                        written,
                        destination_short: written < offered,
                    })
                }
            }),
            SendFile::Offset { file, offset, .. } => {
                let off = *offset;
                admit_mandatory_transfer_range(
                    mandatory,
                    file.inner().location(),
                    file.open_file_description_key(),
                    flock::MandatoryAccess::Read,
                    off,
                    mandatory_len,
                )?;
                destination.preflight()?;
                let mut dst = &mut *buf;
                let offered = file.with_read_credentials(|| file.inner().read_at(&mut dst, off))?;
                // Validate the largest possible commit before the destination
                // is mutated. A short destination write commits only its exact
                // prefix to the caller-owned offset.
                checked_offset_advance(off, offered)?;
                if offered == 0 {
                    return Ok(SendStep {
                        written: 0,
                        destination_short: false,
                    });
                }
                let written = destination.write(&buf[..offered])?;
                if written > offered {
                    return Err(AxError::InvalidInput);
                }
                *offset = checked_offset_advance(off, written)?;
                Ok(SendStep {
                    written,
                    destination_short: written < offered,
                })
            }
        }
    }

    fn write(
        &mut self,
        mut buf: &[u8],
        force_nonblocking: bool,
        mandatory_len: usize,
        mandatory: &mut MandatoryTransferState,
    ) -> AxResult<usize> {
        match self {
            SendFile::Direct {
                file,
                status,
                nonblocking,
                security,
                ..
            } => {
                let memfd_mutation = file
                    .downcast_ref::<File>()
                    .map(|regular| memfd::begin_write(regular.inner().location(), buf.len()))
                    .transpose()?;
                file.with_write_credentials_for_status(*status, || {
                    let nonblocking = *nonblocking || force_nonblocking;
                    if let Some(pipe) = file.downcast_ref::<Pipe>() {
                        pipe.write_with_nonblocking(&mut buf, nonblocking)
                    } else if let Some(pipe) = file.downcast_ref::<NamedPipe>() {
                        pipe.write_with_nonblocking(&mut buf, nonblocking)
                    } else if let Some(socket) = file.downcast_ref::<Socket>() {
                        socket.write_with_nonblocking(&mut buf, nonblocking)
                    } else if let Some(regular) = file.downcast_ref::<File>() {
                        let ofd_key = file.open_file_description_key();
                        let memfd_mutation = memfd_mutation.as_ref().ok_or(AxError::BadState)?;
                        regular
                            .inner()
                            .write_slice_at_current_then(buf, |data, off| {
                                admit_mandatory_transfer_range(
                                    mandatory,
                                    regular.inner().location(),
                                    ofd_key,
                                    flock::MandatoryAccess::Write,
                                    off,
                                    mandatory_len,
                                )?;
                                crate::mm::check_not_active(regular.inner().location())?;
                                let _swap_mutation = crate::mm::admit_mutation(regular.inner().location())?;
                                executable::check_not_active(regular.inner().location())?;
                                let allowed = allowed_write_len(off, data.len())?;
                                if allowed == 0 {
                                    return Ok(0);
                                }
                                let location = regular.inner().location();
                                memfd_mutation.admit_write(
                                    location,
                                    location.len()?,
                                    off,
                                    allowed,
                                )?;
                                let _privilege_guard =
                                    regular.begin_content_write_privilege_cleanup(security)?;
                                regular.inner().write_at(&data[..allowed], off)
                            })
                    } else {
                        write_file_like_with_status(file, *status, &mut buf, security)
                    }
                })
            }
            SendFile::Offset {
                file,
                offset,
                user_offset,
                status,
                security,
                ..
            } => {
                let off = *offset;
                check_writable_mount(file.inner().location())?;
                crate::mm::check_not_active(file.inner().location())?;
                let _swap_mutation = crate::mm::admit_mutation(file.inner().location())?;
                executable::check_not_active(file.inner().location())?;
                let memfd_mutation = memfd::begin_write(file.inner().location(), buf.len())?;
                // A null pointer marks an internal positioned view whose OFD
                // cursor is owned by an outer current-position transaction.
                // It is an implementation of an ordinary current write, not
                // a userspace pwrite-style request, so NO_POSITIONED_WRITE
                // must retain its established current-position semantics.
                if !user_offset.is_null() {
                    check_positioned_write_flags(file.inner().location().flags())?;
                }
                let allowed = allowed_write_len(off, buf.len())?;
                if allowed == 0 {
                    return Ok(0);
                }
                admit_mandatory_transfer_range(
                    mandatory,
                    file.inner().location(),
                    file.open_file_description_key(),
                    flock::MandatoryAccess::Write,
                    off,
                    mandatory_len,
                )?;
                let location = file.inner().location();
                memfd_mutation.admit_write(location, location.len()?, off, allowed)?;
                let _privilege_guard = file.begin_content_write_privilege_cleanup(security)?;
                let bytes_written = file.with_write_credentials_for_status(*status, || {
                    file.inner().write_at(&buf[..allowed], off)
                })?;
                *offset = checked_offset_advance(off, bytes_written)?;
                Ok(bytes_written)
            }
        }
    }

    fn commit_user_offset(&self) -> AxResult<()> {
        if let Self::Offset {
            offset,
            user_offset,
            capability,
            ..
        } = self
            && !user_offset.is_null()
        {
            capability
                .write_value(*user_offset, *offset)
                .map_err(map_usercopy_error)?;
        }
        Ok(())
    }
}

fn drive_send_with(
    len: usize,
    mut transfer: impl FnMut(&mut [u8], usize) -> AxResult<Option<SendStep>>,
) -> AxResult<usize> {
    let mut buf = vec![0; 0x1000];
    let mut total_written = 0;
    let mut remaining = len;

    while remaining > 0 {
        let to_read = buf.len().min(remaining);
        let step = match transfer(&mut buf[..to_read], total_written) {
            Ok(Some(step)) => step,
            Ok(None) => break,
            Err(_) if total_written > 0 => break,
            Err(error) => return Err(error),
        };
        if step.written == 0 {
            break;
        }
        total_written += step.written;
        remaining -= step.written;
        if step.destination_short {
            break;
        }
    }

    Ok(total_written)
}

fn try_send_once(
    src: &mut SendFile,
    dst: &mut SendFile,
    buf: &mut [u8],
    mandatory_len: usize,
    source_mandatory: &mut MandatoryTransferState,
    destination_mandatory: &mut MandatoryTransferState,
    source_key: u64,
    destination_key: u64,
) -> SendAttempt {
    source_mandatory.wait = None;
    destination_mandatory.wait = None;
    let mut destination = TransferDestination {
        file: dst,
        mandatory_len,
        mandatory: destination_mandatory,
        attempted: false,
    };
    let result = with_transfer_attempt_locks(source_key, destination_key, || {
        src.transfer_with(buf, true, mandatory_len, source_mandatory, &mut destination)
    });
    SendAttempt {
        result,
        wait_endpoint: transfer_wait_endpoint(destination.attempted),
        mandatory_wait: source_mandatory.wait.or(destination_mandatory.wait),
    }
}

fn retry_socket_transfer(
    socket_file: &FileHandle<dyn FileLike>,
    direction: SocketTransferDirection,
    socket_endpoint: TransferWaitEndpoint,
    src: &mut SendFile,
    dst: &mut SendFile,
    buf: &mut [u8],
    mandatory_len: usize,
    source_mandatory: &mut MandatoryTransferState,
    destination_mandatory: &mut MandatoryTransferState,
    source_key: u64,
    destination_key: u64,
) -> AxResult<SendAttempt> {
    let socket = socket_file
        .downcast_ref::<Socket>()
        .ok_or(AxError::BadState)?;
    socket.retry_transfer(direction, false, || {
        let attempt = try_send_once(
            src,
            dst,
            buf,
            mandatory_len,
            source_mandatory,
            destination_mandatory,
            source_key,
            destination_key,
        );
        if matches!(&attempt.result, Err(AxError::WouldBlock))
            && attempt.wait_endpoint == socket_endpoint
            && attempt.mandatory_wait.is_none()
        {
            Err(AxError::WouldBlock)
        } else {
            // A successful step, terminal error, EOF, or opposite-endpoint
            // EAGAIN ends this socket poller without resetting its deadline.
            Ok(attempt)
        }
    })
}

enum DeferredSendWait {
    Mandatory(flock::MandatoryLockWait),
    Readiness(TransferWaitEndpoint),
}

enum SendOperationResult {
    Complete(usize),
    Deferred(DeferredSendWait),
}

fn do_send_with_deferred_wait_policy(
    src: &mut SendFile,
    dst: &mut SendFile,
    len: usize,
    defer_transaction_waits: bool,
) -> AxResult<SendOperationResult> {
    let source_key = src.ofd_key();
    let destination_key = dst.ofd_key();
    let source_socket = src.socket_retry_handle();
    let destination_socket = dst.socket_retry_handle();
    let mut source_mandatory = MandatoryTransferState::default();
    let mut destination_mandatory = MandatoryTransferState::default();
    let mut deferred_wait = None;

    let result = drive_send_with(len, |buf, total_written| {
        if total_written > 0 && !src.has_data() {
            return Ok(None);
        }

        let mut attempt = try_send_once(
            src,
            dst,
            buf,
            len,
            &mut source_mandatory,
            &mut destination_mandatory,
            source_key,
            destination_key,
        );
        loop {
            match attempt.result {
                Err(AxError::WouldBlock) if total_written == 0 => {
                    if let Some(wait) = attempt.mandatory_wait {
                        let nonblocking = match attempt.wait_endpoint {
                            TransferWaitEndpoint::Source => src.mandatory_nonblocking(),
                            TransferWaitEndpoint::Destination => dst.mandatory_nonblocking(),
                        };
                        if nonblocking {
                            return Err(AxError::WouldBlock);
                        }
                        if defer_transaction_waits {
                            // No backend read, atime update, destination
                            // write, or pipe consume has happened yet: both
                            // mandatory admissions precede source mutation.
                            // Let the operation-level cursor owner release its
                            // transaction(s) before sleeping, then reacquire
                            // and re-admit the newly sampled range.
                            deferred_wait = Some(DeferredSendWait::Mandatory(wait));
                            return Err(AxError::WouldBlock);
                        }
                        flock::wait_for_mandatory_access(wait)?;
                        attempt = try_send_once(
                            src,
                            dst,
                            buf,
                            len,
                            &mut source_mandatory,
                            &mut destination_mandatory,
                            source_key,
                            destination_key,
                        );
                        continue;
                    }

                    // The one-shot source transaction has ended. Nonblocking
                    // endpoints report their first EAGAIN without consulting a
                    // potentially stale readiness bit.
                    match attempt.wait_endpoint {
                        TransferWaitEndpoint::Source if src.nonblocking() => {
                            return Err(AxError::WouldBlock);
                        }
                        TransferWaitEndpoint::Destination if dst.nonblocking() => {
                            return Err(AxError::WouldBlock);
                        }
                        _ => {}
                    }

                    // A reverse transfer may need this operation's regular
                    // cursor in order to make a pipe or socket ready. No
                    // prefix has been accepted at this point, so release all
                    // cursor transactions before any readiness sleep. A
                    // speculative regular-file read may already have updated
                    // atime, but its bytes, cursor, pipe source, and
                    // destination remain uncommitted.
                    if defer_transaction_waits {
                        deferred_wait = Some(DeferredSendWait::Readiness(attempt.wait_endpoint));
                        return Err(AxError::WouldBlock);
                    }

                    // Socket waits must stay inside the socket's own poller so
                    // SO_RCVTIMEO/SO_SNDTIMEO and pending-error consumption are
                    // preserved under one deadline. The poller retries only
                    // while the same socket endpoint remains responsible.
                    attempt = match attempt.wait_endpoint {
                        TransferWaitEndpoint::Source if source_socket.is_some() => {
                            retry_socket_transfer(
                                source_socket.as_ref().unwrap(),
                                SocketTransferDirection::Receive,
                                TransferWaitEndpoint::Source,
                                src,
                                dst,
                                buf,
                                len,
                                &mut source_mandatory,
                                &mut destination_mandatory,
                                source_key,
                                destination_key,
                            )?
                        }
                        TransferWaitEndpoint::Destination if destination_socket.is_some() => {
                            retry_socket_transfer(
                                destination_socket.as_ref().unwrap(),
                                SocketTransferDirection::Send,
                                TransferWaitEndpoint::Destination,
                                src,
                                dst,
                                buf,
                                len,
                                &mut source_mandatory,
                                &mut destination_mandatory,
                                source_key,
                                destination_key,
                            )?
                        }
                        TransferWaitEndpoint::Source => {
                            src.wait_for(IoEvents::READABLE)?;
                            try_send_once(
                                src,
                                dst,
                                buf,
                                len,
                                &mut source_mandatory,
                                &mut destination_mandatory,
                                source_key,
                                destination_key,
                            )
                        }
                        TransferWaitEndpoint::Destination => {
                            dst.wait_for(IoEvents::WRITABLE)?;
                            try_send_once(
                                src,
                                dst,
                                buf,
                                len,
                                &mut source_mandatory,
                                &mut destination_mandatory,
                                source_key,
                                destination_key,
                            )
                        }
                    };
                }
                result => {
                    return result.map(Some);
                }
            }
        }
    });

    if let Some(wait) = deferred_wait {
        return match result {
            Err(AxError::WouldBlock) => Ok(SendOperationResult::Deferred(wait)),
            _ => Err(AxError::BadState),
        };
    }
    result.map(SendOperationResult::Complete)
}

fn do_send(src: &mut SendFile, dst: &mut SendFile, len: usize) -> AxResult<usize> {
    match do_send_with_deferred_wait_policy(src, dst, len, false)? {
        SendOperationResult::Complete(transferred) => Ok(transferred),
        SendOperationResult::Deferred(_) => Err(AxError::BadState),
    }
}

fn do_send_deferring_transaction_waits(
    src: &mut SendFile,
    dst: &mut SendFile,
    len: usize,
) -> AxResult<SendOperationResult> {
    do_send_with_deferred_wait_policy(src, dst, len, true)
}

fn positioned_advance_for_send(
    file: &SendFile,
    start: u64,
    result: &SendOperationResult,
) -> AxResult<usize> {
    let advance = file.positioned_advance_from(start)?;
    let expected = match result {
        SendOperationResult::Complete(transferred) => *transferred,
        SendOperationResult::Deferred(_) => 0,
    };
    if advance != expected {
        return Err(AxError::BadState);
    }
    Ok(advance)
}

fn do_send_with_positioned_source(
    src: &SendFile,
    dst: &mut SendFile,
    source_start: u64,
    preflight: &mut impl FnMut(&SendFile, &SendFile) -> AxResult<usize>,
) -> AxResult<(SendOperationResult, usize)> {
    let mut positioned_src = src.positioned_current(source_start)?;
    let effective_len = preflight(&positioned_src, dst)?;
    let result = do_send_deferring_transaction_waits(&mut positioned_src, dst, effective_len)?;
    let source_advance = positioned_advance_for_send(&positioned_src, source_start, &result)?;
    Ok((result, source_advance))
}

fn do_send_with_positioned_destination(
    src: &mut SendFile,
    dst: &SendFile,
    destination_start: u64,
    preflight: &mut impl FnMut(&SendFile, &SendFile) -> AxResult<usize>,
) -> AxResult<(SendOperationResult, usize)> {
    let mut positioned_dst = dst.positioned_current(destination_start)?;
    let effective_len = preflight(src, &positioned_dst)?;
    let result = do_send_deferring_transaction_waits(src, &mut positioned_dst, effective_len)?;
    let destination_advance =
        positioned_advance_for_send(&positioned_dst, destination_start, &result)?;
    Ok((result, destination_advance))
}

fn do_send_with_positioned_pair(
    src: &SendFile,
    dst: &SendFile,
    source_start: u64,
    destination_start: u64,
    preflight: &mut impl FnMut(&SendFile, &SendFile) -> AxResult<usize>,
) -> AxResult<(SendOperationResult, usize, usize)> {
    let mut positioned_src = src.positioned_current(source_start)?;
    let mut positioned_dst = dst.positioned_current(destination_start)?;
    let effective_len = preflight(&positioned_src, &positioned_dst)?;
    let result = do_send_deferring_transaction_waits(
        &mut positioned_src,
        &mut positioned_dst,
        effective_len,
    )?;
    let source_advance = positioned_advance_for_send(&positioned_src, source_start, &result)?;
    let destination_advance =
        positioned_advance_for_send(&positioned_dst, destination_start, &result)?;
    Ok((result, source_advance, destination_advance))
}

fn try_send_preserving_current_positions_once(
    src: &mut SendFile,
    dst: &mut SendFile,
    len: usize,
    source_current: &Option<FileHandle<File>>,
    destination_current: &Option<FileHandle<File>>,
    source_key: u64,
    destination_key: u64,
    preflight: &mut impl FnMut(&SendFile, &SendFile) -> AxResult<usize>,
) -> AxResult<SendOperationResult> {
    match (source_current, destination_current) {
        (Some(source_file), None) => source_file
            .inner()
            .with_current_position_transaction(len, |source_start| {
                do_send_with_positioned_source(src, dst, source_start, preflight)
            }),
        (None, Some(destination_file)) => destination_file
            .inner()
            .with_current_position_transaction(len, |destination_start| {
                do_send_with_positioned_destination(src, dst, destination_start, preflight)
            }),
        (Some(source_file), Some(_)) if source_key == destination_key => source_file
            .inner()
            .with_current_position_transaction(len, |start| {
                let (result, source_advance, destination_advance) =
                    do_send_with_positioned_pair(src, dst, start, start, preflight)?;
                if source_advance != destination_advance {
                    return Err(AxError::BadState);
                }
                Ok((result, source_advance))
            }),
        (Some(source_file), Some(destination_file)) if source_key < destination_key => source_file
            .inner()
            .with_current_position_transaction(len, |source_start| {
                destination_file.inner().with_current_position_transaction(
                    len,
                    |destination_start| {
                        let (result, source_advance, destination_advance) =
                            do_send_with_positioned_pair(
                                src,
                                dst,
                                source_start,
                                destination_start,
                                preflight,
                            )?;
                        Ok(((result, source_advance), destination_advance))
                    },
                )
            }),
        (Some(source_file), Some(destination_file)) => destination_file
            .inner()
            .with_current_position_transaction(len, |destination_start| {
                source_file
                    .inner()
                    .with_current_position_transaction(len, |source_start| {
                        let (result, source_advance, destination_advance) =
                            do_send_with_positioned_pair(
                                src,
                                dst,
                                source_start,
                                destination_start,
                                preflight,
                            )?;
                        Ok(((result, destination_advance), source_advance))
                    })
            }),
        (None, None) => Err(AxError::BadState),
    }
}

fn retry_current_send_with_socket_poller(
    socket_file: &FileHandle<dyn FileLike>,
    direction: SocketTransferDirection,
    socket_endpoint: TransferWaitEndpoint,
    mut attempt: impl FnMut() -> AxResult<SendOperationResult>,
) -> AxResult<SendOperationResult> {
    enum PollerOutcome {
        Send(SendOperationResult),
        Terminal(AxError),
    }

    let socket = socket_file
        .downcast_ref::<Socket>()
        .ok_or(AxError::BadState)?;
    let outcome = socket.retry_transfer(direction, false, || match attempt() {
        Ok(result)
            if matches!(
                &result,
                SendOperationResult::Deferred(DeferredSendWait::Readiness(endpoint))
                    if *endpoint == socket_endpoint
            ) =>
        {
            Err(AxError::WouldBlock)
        }
        Ok(result) => {
            // Completion, mandatory admission, or the opposite endpoint's
            // readiness ends this socket poller. Only retries attributed to
            // this socket remain under its one timeout deadline.
            Ok(PollerOutcome::Send(result))
        }
        // WouldBlock can also be a terminal nonblocking decision for a
        // mandatory or opposite endpoint. Wrap every unattributed error as a
        // successful poller result so the socket cannot swallow and retry it.
        Err(error) => Ok(PollerOutcome::Terminal(error)),
    })?;
    match outcome {
        PollerOutcome::Send(result) => Ok(result),
        PollerOutcome::Terminal(error) => Err(error),
    }
}

/// Runs one transfer while every current-position regular endpoint owns its
/// OFD cursor for the whole admitted operation. Internally the transfer uses
/// positioned I/O and publishes only the final accepted prefix.
///
/// Different current-position OFDs are acquired by their complete stable key,
/// independent of transfer direction. A same-OFD source/destination pair owns
/// one transaction and advances the shared cursor once. Mandatory and
/// readiness sleeps are deliberately deferred until all cursor transactions
/// have been released; retry then samples fresh positions and repeats
/// `preflight` and admission. Socket-attributed retries stay inside one socket
/// poller so its timeout and pending-error semantics remain operation-scoped.
fn do_send_preserving_current_positions(
    src: &mut SendFile,
    dst: &mut SendFile,
    len: usize,
    mut preflight: impl FnMut(&SendFile, &SendFile) -> AxResult<usize>,
) -> AxResult<usize> {
    let source_current = src.current_regular_file();
    let destination_current = dst.current_regular_file();
    if source_current.is_none() && destination_current.is_none() {
        let effective_len = preflight(src, dst)?;
        return do_send(src, dst, effective_len);
    }

    let source_key = src.ofd_key();
    let destination_key = dst.ofd_key();
    let mut result = try_send_preserving_current_positions_once(
        src,
        dst,
        len,
        &source_current,
        &destination_current,
        source_key,
        destination_key,
        &mut preflight,
    )?;
    loop {
        match result {
            SendOperationResult::Complete(transferred) => return Ok(transferred),
            SendOperationResult::Deferred(DeferredSendWait::Mandatory(wait)) => {
                flock::wait_for_mandatory_access(wait)?;
                result = try_send_preserving_current_positions_once(
                    src,
                    dst,
                    len,
                    &source_current,
                    &destination_current,
                    source_key,
                    destination_key,
                    &mut preflight,
                )?;
            }
            SendOperationResult::Deferred(DeferredSendWait::Readiness(
                TransferWaitEndpoint::Source,
            )) => {
                if let Some(socket_file) = src.socket_retry_handle() {
                    result = retry_current_send_with_socket_poller(
                        &socket_file,
                        SocketTransferDirection::Receive,
                        TransferWaitEndpoint::Source,
                        || {
                            try_send_preserving_current_positions_once(
                                src,
                                dst,
                                len,
                                &source_current,
                                &destination_current,
                                source_key,
                                destination_key,
                                &mut preflight,
                            )
                        },
                    )?;
                } else {
                    src.wait_for(IoEvents::READABLE)?;
                    result = try_send_preserving_current_positions_once(
                        src,
                        dst,
                        len,
                        &source_current,
                        &destination_current,
                        source_key,
                        destination_key,
                        &mut preflight,
                    )?;
                }
            }
            SendOperationResult::Deferred(DeferredSendWait::Readiness(
                TransferWaitEndpoint::Destination,
            )) => {
                if let Some(socket_file) = dst.socket_retry_handle() {
                    result = retry_current_send_with_socket_poller(
                        &socket_file,
                        SocketTransferDirection::Send,
                        TransferWaitEndpoint::Destination,
                        || {
                            try_send_preserving_current_positions_once(
                                src,
                                dst,
                                len,
                                &source_current,
                                &destination_current,
                                source_key,
                                destination_key,
                                &mut preflight,
                            )
                        },
                    )?;
                } else {
                    dst.wait_for(IoEvents::WRITABLE)?;
                    result = try_send_preserving_current_positions_once(
                        src,
                        dst,
                        len,
                        &source_current,
                        &destination_current,
                        source_key,
                        destination_key,
                        &mut preflight,
                    )?;
                }
            }
        }
    }
}

fn validate_sendfile_source(fd: c_int) -> AxResult<(FileHandle<File>, OfdIoStatus)> {
    let file_like = get_file_like(fd)?;
    // sendfile's input contract is narrower than ordinary positioned I/O:
    // directories and other non-regular sources report EINVAL, not pread's
    // EISDIR/ESPIPE split.
    if FileLikeKind::from_file_like(file_like.as_ref()) != FileLikeKind::Regular {
        return Err(AxError::InvalidInput);
    }
    let file = file_like.downcast::<File>()?;
    file.inner().access(FileFlags::READ)?;
    let status = file.io_status_snapshot();
    Ok((file, status))
}

fn check_sendfile_destination_status(status_flags: u32) -> AxResult<()> {
    if status_flags & O_APPEND != 0 {
        Err(AxError::InvalidInput)
    } else {
        Ok(())
    }
}

fn validate_sendfile_destination(
    file_like: &FileHandle<dyn FileLike>,
    status: OfdIoStatus,
) -> AxResult<()> {
    // The operation keeps this immutable OFD snapshot through validation and
    // every transfer chunk; a concurrent F_SETFL only affects later calls.
    if let Some(pipe) = file_like.downcast_ref::<Pipe>() {
        // Linux's output-pipe path validates FMODE_WRITE even for len == 0,
        // but does not apply the regular-file O_APPEND rejection.
        return if pipe.is_write() {
            Ok(())
        } else {
            Err(AxError::BadFileDescriptor)
        };
    }

    if let Some(pipe) = file_like.downcast_ref::<NamedPipe>() {
        return if pipe.is_write() {
            Ok(())
        } else {
            Err(AxError::BadFileDescriptor)
        };
    }

    if let Some(file) = file_like.downcast_ref::<File>() {
        // Immutable write authority has Linux's earlier EBADF precedence over
        // the mutable O_APPEND EINVAL rule (including O_RDONLY|O_APPEND).
        file.inner().access(FileFlags::WRITE)?;
        check_sendfile_destination_status(status.raw())?;
        check_writable_mount(file.inner().location())?;
        executable::check_not_active(file.inner().location())?;
    } else if let Some(socket) = file_like.downcast_ref::<Socket>() {
        // The current transfer driver chunks streams at 4 KiB. Sending those
        // chunks independently would split one Linux sendfile datagram and can
        // turn EMSGSIZE into a false multi-packet success, so refuse datagram
        // destinations until a message-preserving transaction exists.
        if matches!(&socket.inner, axnet::Socket::Udp(_))
            || matches!(&socket.inner, axnet::Socket::Unix(unix) if unix.is_datagram())
        {
            return Err(AxError::OperationNotSupported);
        }
        // Stream socket destinations retain the ordinary O_APPEND rejection.
        // Connection/runtime errors are reported by the actual send operation
        // (len == 0 remains a no-op).
        check_sendfile_destination_status(status.raw())?;
    } else if matches!(
        FileLikeKind::from_file_like(file_like.as_ref()),
        FileLikeKind::Directory
    ) {
        return Err(AxError::IsADirectory);
    } else {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_splice_endpoint(
    file_like: &FileHandle<dyn FileLike>,
    status: OfdIoStatus,
    input: bool,
) -> AxResult<()> {
    if let Some(pipe) = file_like.downcast_ref::<Pipe>() {
        return if input {
            if pipe.is_read() {
                Ok(())
            } else {
                Err(AxError::BadFileDescriptor)
            }
        } else if pipe.is_write() {
            Ok(())
        } else {
            Err(AxError::BadFileDescriptor)
        };
    }

    if let Some(pipe) = file_like.downcast_ref::<NamedPipe>() {
        return if input {
            if pipe.is_read() {
                Ok(())
            } else {
                Err(AxError::BadFileDescriptor)
            }
        } else if pipe.is_write() {
            Ok(())
        } else {
            Err(AxError::BadFileDescriptor)
        };
    }

    if let Some(socket) = file_like.downcast_ref::<Socket>() {
        // Datagram splice needs a transactional receive dequeue and a single
        // message-preserving send. The current generic 4 KiB transfer driver
        // can provide neither, so refuse it explicitly instead of dropping a
        // packet after destination EAGAIN or splitting one message.
        if matches!(&socket.inner, axnet::Socket::Udp(_))
            || matches!(&socket.inner, axnet::Socket::Unix(unix) if unix.is_datagram())
        {
            return Err(AxError::OperationNotSupported);
        }
        if matches!(&socket.inner, axnet::Socket::Unix(unix) if !unix.is_connected()) {
            return Err(AxError::InvalidInput);
        }
        if !input && status.append() {
            return Err(AxError::InvalidInput);
        }
        return Ok(());
    }

    if let Some(file) = file_like.downcast_ref::<File>() {
        if input {
            file.inner().access(FileFlags::READ)?;
        } else {
            file.inner().access(FileFlags::WRITE)?;
            if status.append() {
                return Err(AxError::InvalidInput);
            }
            check_writable_mount(file.inner().location())?;
            executable::check_not_active(file.inner().location())?;
        }
        return Ok(());
    }

    if file_like.downcast_ref::<Directory>().is_some() {
        return Err(AxError::InvalidInput);
    }

    Err(AxError::InvalidInput)
}

fn pipe_from_fd(fd: c_int, non_pipe_error: AxError) -> AxResult<FileHandle<Pipe>> {
    let file = get_file_like(fd).map_err(|_| AxError::BadFileDescriptor)?;
    file.downcast::<Pipe>().map_err(|_| non_pipe_error)
}

pub fn sys_sendfile(
    capability: UserMemoryCapability,
    out_fd: c_int,
    in_fd: c_int,
    offset: *mut u64,
    len: usize,
) -> AxResult<isize> {
    debug!(
        "sys_sendfile <= out_fd: {}, in_fd: {}, offset: {}, len: {}",
        out_fd,
        in_fd,
        !offset.is_null(),
        len
    );
    let security = current_vfs_security();

    // Linux copies an explicit offset before fd admission and keeps one local
    // value for the complete operation. Concurrent userspace stores cannot
    // redirect later chunks.
    let explicit_offset = if offset.is_null() {
        None
    } else {
        Some(
            capability
                .read_value(offset as *const u64)
                .map_err(map_usercopy_error)?,
        )
    };

    let mut committed_offset = explicit_offset;
    let result = (|| {
        if explicit_offset.is_some_and(|value| value > MAX_FILE_OFFSET) {
            return Err(AxError::InvalidInput);
        }

        let (src_file, src_status) = validate_sendfile_source(in_fd)?;
        let dst = get_file_like(out_fd)?;
        let source_location = src_file.inner().location().clone();
        let destination_location = dst
            .downcast_ref::<File>()
            .map(|file| file.inner().location().clone());
        dst.with_write_credentials(|status| {
            validate_sendfile_destination(&dst, status)?;
            let mut src = if let Some(explicit_offset) = explicit_offset {
                SendFile::Offset {
                    status: src_status,
                    file: src_file.clone(),
                    offset: explicit_offset,
                    user_offset: offset,
                    security: security.clone(),
                    capability: capability.clone(),
                }
            } else {
                SendFile::Direct {
                    status: src_status,
                    file: src_file.clone().into_file_like(),
                    nonblocking: src_status.nonblocking(),
                    security: security.clone(),
                    capability: capability.clone(),
                }
            };

            let mut destination = SendFile::Direct {
                file: dst.clone(),
                status,
                nonblocking: status.nonblocking(),
                security: security.clone(),
                capability: capability.clone(),
            };
            let sent = do_send_preserving_current_positions(
                &mut src,
                &mut destination,
                len,
                |_src, _dst| Ok(len),
            );
            if let SendFile::Offset { offset, .. } = &src {
                committed_offset = Some(*offset);
            }
            let sent = sent?;
            if sent > 0 {
                sync_file_like_after_status_write(status, &dst)?;
                notify_transfer_success(Some(&source_location), destination_location.as_ref());
            }
            Ok(sent as _)
        })
    })();

    // Linux attempts this put_user after every post-copyin outcome. A fault
    // therefore overrides a transfer or validation error; a successful store
    // leaves the original result intact.
    if let Some(committed_offset) = committed_offset {
        capability
            .write_value(offset, committed_offset)
            .map_err(map_usercopy_error)?;
    }
    result
}

pub fn sys_copy_file_range(
    capability: UserMemoryCapability,
    fd_in: c_int,
    off_in: *mut u64,
    fd_out: c_int,
    off_out: *mut u64,
    len: usize,
    _flags: u32,
) -> AxResult<isize> {
    debug!(
        "sys_copy_file_range <= fd_in: {}, off_in: {}, fd_out: {}, off_out: {}, len: {}, flags: {}",
        fd_in,
        !off_in.is_null(),
        fd_out,
        !off_out.is_null(),
        len,
        _flags
    );
    let security = current_vfs_security();

    // Pin both numeric descriptors before touching user offsets. Full
    // mode/type/O_APPEND admission follows offset copy, matching Linux while
    // retaining exact handles across close-and-reuse by a shared fd table.
    let src_handle = get_file_like(fd_in)?;
    let src_status = src_handle.io_status_snapshot();
    let dst_handle = get_file_like(fd_out)?;
    let dst_status = dst_handle.io_status_snapshot();
    let src_offset = if off_in.is_null() {
        None
    } else {
        Some(checked_user_file_offset(&capability, off_in)?)
    };
    let dst_offset = if off_out.is_null() {
        None
    } else {
        Some(checked_user_file_offset(&capability, off_out)?)
    };
    if _flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if len as u64 > MAX_FILE_OFFSET {
        return Err(AxError::from(LinuxError::EOVERFLOW));
    }

    let src_file = regular_copy_file(src_handle, src_status, false)?;
    let dst_file = regular_copy_file(dst_handle, dst_status, true)?;
    let src_location = src_file.inner().location().clone();
    let dst_location = dst_file.inner().location().clone();
    let same_inode =
        inode_flags::same_inode(src_file.inner().location(), dst_file.inner().location());

    let mut src = if let Some(src_offset) = src_offset {
        SendFile::Offset {
            file: src_file,
            offset: src_offset,
            user_offset: off_in,
            status: src_status,
            security: security.clone(),
            capability: capability.clone(),
        }
    } else {
        SendFile::Direct {
            file: src_file.clone().into_file_like(),
            status: src_status,
            nonblocking: src_status.nonblocking(),
            security: security.clone(),
            capability: capability.clone(),
        }
    };

    let mut dst = if let Some(dst_offset) = dst_offset {
        SendFile::Offset {
            file: dst_file.clone(),
            offset: dst_offset,
            user_offset: off_out,
            status: dst_status,
            security: security.clone(),
            capability: capability.clone(),
        }
    } else {
        SendFile::Direct {
            file: dst_file.clone().into_file_like(),
            status: dst_status,
            nonblocking: dst_status.nonblocking(),
            security: security.clone(),
            capability: capability.clone(),
        }
    };

    let copied = do_send_preserving_current_positions(
        &mut src,
        &mut dst,
        len,
        |positioned_src, positioned_dst| {
            // Current positions are sampled only after all operation cursor
            // transactions have been acquired. Mandatory-wait retries repeat
            // these checks against the newly frozen pair.
            let src_offset = positioned_src.positioned_offset()?;
            let dst_offset = positioned_dst.positioned_offset()?;
            let source_count = copy_file_range_source_count(src_offset, len, src_location.len()?)?;
            let destination_allowed = allowed_write_len(dst_offset, source_count)?;
            copy_file_range_effective_count(
                src_offset,
                dst_offset,
                source_count,
                destination_allowed,
                same_inode,
            )
        },
    )?;
    if copied > 0 {
        touch_modified_metadata(&dst_location)?;
        sync_file_after_status_write(dst_status, &dst_file)?;
        let _ = notify_exact(&src_location, IN_ACCESS);
        let _ = notify_parent(&src_location, IN_ACCESS);
        let _ = notify_exact(&dst_location, IN_MODIFY);
        let _ = notify_parent(&dst_location, IN_MODIFY);
        // Linux updates offset pointers only for a positive return. Evaluate
        // both stores before reporting either fault so their side effects are
        // independent even when the first pointer is invalid.
        let src_commit = src.commit_user_offset();
        let dst_commit = dst.commit_user_offset();
        match (src_commit, dst_commit) {
            (Err(error), _) | (Ok(()), Err(error)) => return Err(error),
            (Ok(()), Ok(())) => {}
        }
    }
    Ok(copied as _)
}

pub fn sys_splice(
    capability: UserMemoryCapability,
    fd_in: c_int,
    off_in: *mut i64,
    fd_out: c_int,
    off_out: *mut i64,
    len: usize,
    _flags: u32,
) -> AxResult<isize> {
    debug!(
        "sys_splice <= fd_in: {}, off_in: {}, fd_out: {}, off_out: {}, len: {}, flags: {}",
        fd_in,
        !off_in.is_null(),
        fd_out,
        !off_out.is_null(),
        len,
        _flags
    );
    let security = current_vfs_security();

    if len == 0 {
        return Ok(0);
    }
    validate_splice_flags(_flags)?;

    let src_handle = get_file_like(fd_in)?;
    let src_status = src_handle.io_status_snapshot();
    let dst_handle = get_file_like(fd_out)?;
    let dst_status = dst_handle.io_status_snapshot();

    let src_pipe = src_handle.downcast::<Pipe>().ok();
    let dst_pipe = dst_handle.downcast::<Pipe>().ok();
    let src_named_pipe = src_handle.downcast::<NamedPipe>().ok();
    let dst_named_pipe = dst_handle.downcast::<NamedPipe>().ok();
    let source_is_pipe = src_pipe.is_some() || src_named_pipe.is_some();
    let destination_is_pipe = dst_pipe.is_some() || dst_named_pipe.is_some();
    let source_pipe_endpoint = src_pipe
        .as_deref()
        .map(PipeEndpoint::Anonymous)
        .or_else(|| src_named_pipe.as_deref().map(PipeEndpoint::Named));
    let destination_pipe_endpoint = dst_pipe
        .as_deref()
        .map(PipeEndpoint::Anonymous)
        .or_else(|| dst_named_pipe.as_deref().map(PipeEndpoint::Named));

    // Pipe/FIFO offsets are rejected without dereferencing userspace. Other
    // offsets are copied before access/O_APPEND/type admission, with output
    // first, matching Linux __do_splice().
    if source_is_pipe && !off_in.is_null() {
        return Err(AxError::from(LinuxError::ESPIPE));
    }
    if destination_is_pipe && !off_out.is_null() {
        return Err(AxError::from(LinuxError::ESPIPE));
    }
    let output_offset = if off_out.is_null() {
        None
    } else {
        Some(
            capability
                .read_value(off_out as *const i64)
                .map_err(map_usercopy_error)?,
        )
    };
    let input_offset = if off_in.is_null() {
        None
    } else {
        Some(
            capability
                .read_value(off_in as *const i64)
                .map_err(map_usercopy_error)?,
        )
    };

    validate_splice_endpoint(&src_handle, src_status, true)?;
    validate_splice_endpoint(&dst_handle, dst_status, false)?;
    if !source_is_pipe && !destination_is_pipe {
        return Err(AxError::InvalidInput);
    }
    let source_location = src_handle
        .downcast_ref::<File>()
        .map(|file| file.inner().location().clone());
    let destination_location = dst_handle
        .downcast_ref::<File>()
        .map(|file| file.inner().location().clone());
    if let (Some(src), Some(dst)) = (&src_named_pipe, &dst_named_pipe)
        && src.same_pipe(dst)
    {
        return Err(AxError::InvalidInput);
    }
    let operation_nonblocking = splice_operation_nonblocking(
        _flags,
        source_is_pipe,
        src_status.nonblocking(),
        destination_is_pipe,
        dst_status.nonblocking(),
    );
    let (source_nonblocking, destination_nonblocking) = splice_endpoint_nonblocking(
        _flags,
        src_status.nonblocking(),
        destination_is_pipe,
        dst_status.nonblocking(),
    );

    // A direct pipe move can commit both ring indices in one ordered lock
    // domain. The buffered read/write fallback cannot roll a consumed pipe
    // prefix back after a destination short write or error.
    if off_in.is_null()
        && off_out.is_null()
        && let (Some(src), Some(dst)) = (source_pipe_endpoint, destination_pipe_endpoint)
    {
        return src
            .splice_to(dst, len, operation_nonblocking)
            .map(|moved| moved as isize);
    }

    let mut src = if let Some(offset) = input_offset {
        if offset < 0 {
            return Err(AxError::InvalidInput);
        }
        let file = src_handle.downcast::<File>()?;
        SendFile::Offset {
            status: src_status,
            file,
            offset: offset as u64,
            user_offset: off_in.cast(),
            security: security.clone(),
            capability: capability.clone(),
        }
    } else {
        if let Some(file) = src_handle.downcast_ref::<File>()
            && file.inner().is_path()
        {
            return Err(AxError::InvalidInput);
        }
        SendFile::Direct {
            status: src_status,
            file: src_handle,
            nonblocking: source_nonblocking,
            security: security.clone(),
            capability: capability.clone(),
        }
    };

    let mut dst = if let Some(offset) = output_offset {
        if offset < 0 {
            return Err(AxError::InvalidInput);
        }
        let file = dst_handle.downcast::<File>()?;
        SendFile::Offset {
            status: dst_status,
            file,
            offset: offset as u64,
            user_offset: off_out.cast(),
            security: security.clone(),
            capability: capability.clone(),
        }
    } else {
        SendFile::Direct {
            file: dst_handle.clone(),
            status: dst_status,
            nonblocking: destination_nonblocking,
            security,
            capability: capability.clone(),
        }
    };

    let spliced =
        do_send_preserving_current_positions(&mut src, &mut dst, len, |_src, _dst| Ok(len))?;
    if spliced > 0 {
        sync_file_like_after_status_write(dst_status, &dst_handle)?;
        notify_splice_success(source_location.as_ref(), destination_location.as_ref());
    }
    dst.commit_user_offset()?;
    src.commit_user_offset()?;
    Ok(spliced as _)
}

pub fn sys_tee(fd_in: c_int, fd_out: c_int, len: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_tee <= fd_in: {fd_in}, fd_out: {fd_out}, len: {len}, flags: {flags:#x}");

    validate_splice_flags(flags)?;
    if len == 0 {
        return Ok(0);
    }

    let src = pipe_from_fd(fd_in, AxError::InvalidInput)?;
    let dst = pipe_from_fd(fd_out, AxError::InvalidInput)?;
    let nonblocking = flags & SPLICE_F_NONBLOCK != 0
        || src.io_status_snapshot().nonblocking()
        || dst.io_status_snapshot().nonblocking();
    src.tee_to(&dst, len, nonblocking).map(|n| n as _)
}

pub fn sys_vmsplice(
    capability: UserMemoryCapability,
    fd: c_int,
    iov: *const IoVec,
    nr_segs: usize,
    flags: u32,
) -> AxResult<isize> {
    debug!("sys_vmsplice <= fd: {fd}, iov: {iov:p}, nr_segs: {nr_segs}, flags: {flags:#x}");

    validate_splice_flags(flags)?;

    let pipe = pipe_from_fd(fd, AxError::BadFileDescriptor)?;
    let mut io = IoVectorBuf::new(capability, iov, nr_segs)?.into_io();
    let nonblocking = flags & SPLICE_F_NONBLOCK != 0 || pipe.io_status_snapshot().nonblocking();

    let result = if pipe.is_write() {
        pipe.vmsplice_write(&mut io, nonblocking)
    } else {
        pipe.vmsplice_read(&mut io, nonblocking)
    };

    result.map(|n| n as _)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::{Arc, Weak};
    use core::{
        any::Any,
        cell::Cell,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::Context,
        time::Duration,
    };
    use std::{
        sync::{Mutex as StdMutex, mpsc},
        thread,
    };

    use axfs_ng_vfs::{
        DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps, Metadata, MetadataUpdate,
        Mountpoint, NodeOps, NodePermission, NodeType, Reference, StatFs, VfsError, VfsResult,
        XattrProvider, XattrSetMode,
    };
    use axio::{IoBuf, Read};
    use thekernel_linux_packet::{
        PacketSendAddress, PacketSocketType, ProtocolSelector, ReceiveFlags, SetPacketOption,
    };

    use super::*;
    use crate::{
        file::{PacketSocket, packet_socket::packet_test_context},
        task::{NetworkNamespace, UserNamespace},
    };

    struct CountingPacketSource<'a> {
        bytes: &'a [u8],
        offset: usize,
        reads: &'a Cell<usize>,
    }

    impl Read for CountingPacketSource<'_> {
        fn read(&mut self, output: &mut [u8]) -> AxResult<usize> {
            self.reads.set(self.reads.get() + 1);
            let source = &self.bytes[self.offset..];
            let copied = source.len().min(output.len());
            output[..copied].copy_from_slice(&source[..copied]);
            self.offset += copied;
            Ok(copied)
        }
    }

    impl IoBuf for CountingPacketSource<'_> {
        fn remaining(&self) -> usize {
            self.bytes.len() - self.offset
        }
    }

    fn packet_test_namespace() -> Arc<NetworkNamespace> {
        NetworkNamespace::try_new_loopback_only(UserNamespace::try_new_root().unwrap()).unwrap()
    }

    fn raw_ipv4_packet() -> [u8; 34] {
        let mut frame = [0_u8; 34];
        frame[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&20_u16.to_be_bytes());
        frame[22] = 64;
        frame
    }

    fn loopback_packet_destination() -> PacketSendAddress {
        PacketSendAddress::try_from_network_order_fields(0x0800_u16.to_be(), 1, 6, [0; 8]).unwrap()
    }

    fn pinned_packet(socket: Arc<PacketSocket>) -> PinnedSocketDescription {
        let file: Arc<dyn FileLike> = socket;
        let description = FileDescription::new(file).unwrap();
        let handle = FileHandle::<dyn FileLike>::from_description_for_test(description);
        let status = handle.io_status_snapshot();
        let pinned = PinnedSocketDescription::from_file_handle(&handle, status)
            .unwrap()
            .unwrap();
        assert_eq!(
            pinned.security_ref().unwrap().ofd_identity(),
            handle.open_file_description_key()
        );
        pinned
    }

    fn enqueue_outgoing_packet(sender: &PacketSocket, frame: &[u8]) {
        let plan = sender
            .prepare_send(frame.len(), Some(loopback_packet_destination()))
            .unwrap();
        assert_eq!(sender.send_prepared(plan, frame), Ok(frame.len()));
    }

    #[test]
    fn denied_generic_packet_read_preserves_payload_and_queue_ownership() {
        let _context = packet_test_context();
        let namespace = packet_test_namespace();
        let receiver = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::All,
            namespace.clone(),
        )
        .unwrap();
        receiver
            .set_packet_option(SetPacketOption::IgnoreOutgoing(true))
            .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
        let pinned = pinned_packet(receiver.clone());
        let frame = raw_ipv4_packet();
        enqueue_outgoing_packet(&sender, &frame);
        assert!(receiver.poll().contains(IoEvents::READABLE));

        let authorize_calls = Cell::new(0);
        let backend_calls = Cell::new(0);
        let mut output = [0xa5_u8; 34];
        let output_len = output.len();
        let result = generic_read_after_socket_policy(
            Some(&pinned),
            output_len,
            |_| {
                authorize_calls.set(authorize_calls.get() + 1);
                Err(AxError::PermissionDenied)
            },
            || {
                backend_calls.set(backend_calls.get() + 1);
                let mut destination = &mut output[..];
                receiver
                    .recv_with_nonblocking(&mut destination, ReceiveFlags::EMPTY, true)
                    .map(|outcome| outcome.returned_len())
            },
        );

        assert_eq!(result, Err(AxError::PermissionDenied));
        assert_eq!(authorize_calls.get(), 1);
        assert_eq!(backend_calls.get(), 0);
        assert_eq!(output, [0xa5; 34]);
        assert!(receiver.poll().contains(IoEvents::READABLE));

        let mut drained = [0_u8; 34];
        let mut destination = &mut drained[..];
        let outcome = receiver
            .recv_with_nonblocking(&mut destination, ReceiveFlags::EMPTY, true)
            .unwrap();
        assert_eq!(outcome.returned_len(), frame.len());
        assert_eq!(drained, frame);
        assert!(!receiver.poll().contains(IoEvents::READABLE));
    }

    #[test]
    fn generic_packet_read_uses_the_frozen_ofd_nonblocking_state() {
        let _context = packet_test_context();
        let namespace = packet_test_namespace();
        let receiver = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::All,
            namespace.clone(),
        )
        .unwrap();
        receiver
            .set_packet_option(SetPacketOption::IgnoreOutgoing(true))
            .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
        let file: Arc<dyn FileLike> = receiver.clone();
        let description = FileDescription::new(file).unwrap();
        let handle = FileHandle::<dyn FileLike>::from_description_for_test(description);
        let frozen_nonblocking = handle.set_nonblocking_status(true).unwrap();
        assert!(frozen_nonblocking.nonblocking());
        assert!(!handle.set_nonblocking_status(false).unwrap().nonblocking());

        let worker_handle = handle.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut output = [0_u8; 34];
            let result = read_file_like_with_status(
                &worker_handle,
                frozen_nonblocking,
                &mut &mut output[..],
            );
            result_tx.send(result).unwrap();
        });

        match result_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(result) => assert_eq!(result, Err(AxError::WouldBlock)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                enqueue_outgoing_packet(&sender, &raw_ipv4_packet());
                let _ = result_rx.recv_timeout(Duration::from_secs(1));
                worker.join().unwrap();
                panic!("packet read resampled the live blocking flag");
            }
            Err(error) => panic!("packet read worker failed: {error:?}"),
        }
        worker.join().unwrap();
    }

    #[test]
    fn denied_generic_packet_write_reads_no_payload_and_submits_no_frame() {
        let _context = packet_test_context();
        let namespace = packet_test_namespace();
        let observer = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::All,
            namespace.clone(),
        )
        .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
        let pinned = pinned_packet(sender.clone());
        let frame = raw_ipv4_packet();
        let reads = Cell::new(0);
        let backend_calls = Cell::new(0);
        let authorize_calls = Cell::new(0);
        let mut source = CountingPacketSource {
            bytes: &frame,
            offset: 0,
            reads: &reads,
        };

        let result = generic_write_after_socket_policy(
            Some(&pinned),
            |_| {
                authorize_calls.set(authorize_calls.get() + 1);
                Err(AxError::PermissionDenied)
            },
            || {
                backend_calls.set(backend_calls.get() + 1);
                sender.write(&mut source)
            },
        );

        assert_eq!(result, Err(AxError::PermissionDenied));
        assert_eq!(authorize_calls.get(), 1);
        assert_eq!(backend_calls.get(), 0);
        assert_eq!(reads.get(), 0);
        assert!(!observer.poll().contains(IoEvents::READABLE));
    }

    #[test]
    fn zero_length_packet_read_skips_hook_but_write_still_dispatches() {
        let _context = packet_test_context();
        let namespace = packet_test_namespace();
        let receiver = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::All,
            namespace.clone(),
        )
        .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
        let pinned = pinned_packet(receiver.clone());
        enqueue_outgoing_packet(&sender, &raw_ipv4_packet());

        let receive_hooks = Cell::new(0);
        let receive_calls = Cell::new(0);
        let result = generic_read_after_socket_policy(
            Some(&pinned),
            0,
            |_| {
                receive_hooks.set(receive_hooks.get() + 1);
                Ok(())
            },
            || {
                receive_calls.set(receive_calls.get() + 1);
                Ok(0usize)
            },
        );
        assert_eq!(result, Ok(None));
        assert_eq!(receive_hooks.get(), 0);
        assert_eq!(receive_calls.get(), 0);
        assert!(receiver.poll().contains(IoEvents::READABLE));

        let send_hooks = Cell::new(0);
        let send_calls = Cell::new(0);
        let empty_reads = Cell::new(0);
        let mut empty = CountingPacketSource {
            bytes: &[],
            offset: 0,
            reads: &empty_reads,
        };
        let result = generic_write_after_socket_policy(
            Some(&pinned),
            |_| {
                send_hooks.set(send_hooks.get() + 1);
                Err(AxError::PermissionDenied)
            },
            || {
                send_calls.set(send_calls.get() + 1);
                receiver.write(&mut empty)
            },
        );
        assert_eq!(result, Err(AxError::PermissionDenied));
        assert_eq!(send_hooks.get(), 1);
        assert_eq!(send_calls.get(), 0);
        assert_eq!(empty_reads.get(), 0);
        assert!(receiver.poll().contains(IoEvents::READABLE));
    }

    struct IoContractFs {
        this: Weak<Self>,
        flags: NodeFlags,
        node_type: NodeType,
        size: u64,
        fail_open: AtomicBool,
        fail_remove_xattr: AtomicBool,
        open_calls: AtomicUsize,
        remove_xattr_calls: AtomicUsize,
        set_len_calls: AtomicUsize,
        write_offsets: StdMutex<Vec<u64>>,
    }

    impl IoContractFs {
        fn new(flags: NodeFlags, size: u64) -> Arc<Self> {
            Self::new_with_type(flags, size, NodeType::RegularFile)
        }

        fn new_with_type(flags: NodeFlags, size: u64, node_type: NodeType) -> Arc<Self> {
            Arc::new_cyclic(|this| Self {
                this: this.clone(),
                flags,
                node_type,
                size,
                fail_open: AtomicBool::new(false),
                fail_remove_xattr: AtomicBool::new(false),
                open_calls: AtomicUsize::new(0),
                remove_xattr_calls: AtomicUsize::new(0),
                set_len_calls: AtomicUsize::new(0),
                write_offsets: StdMutex::new(Vec::new()),
            })
        }

        fn location(self: &Arc<Self>) -> Location {
            let filesystem = Filesystem::new(self.clone());
            Mountpoint::new_root(&filesystem).root_location()
        }
    }

    impl FilesystemOps for IoContractFs {
        fn name(&self) -> &str {
            "io-contract-test"
        }

        fn root_dir(&self) -> DirEntry {
            let fs = self.this.upgrade().expect("test filesystem is live");
            DirEntry::new_file(
                FileNode::new(Arc::new(IoContractNode { fs })),
                self.node_type,
                Reference::root(),
            )
        }

        fn stat(&self) -> VfsResult<StatFs> {
            Ok(StatFs {
                fs_type: 0,
                block_size: 4096,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                file_count: 1,
                free_file_count: 0,
                name_length: 255,
                fragment_size: 4096,
                mount_flags: 0,
            })
        }
    }

    struct IoContractNode {
        fs: Arc<IoContractFs>,
    }

    impl NodeOps for IoContractNode {
        fn inode(&self) -> u64 {
            1
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: 1,
                nlink: 1,
                mode: NodePermission::from_bits_truncate(0o600),
                node_type: self.fs.node_type,
                uid: 0,
                gid: 0,
                project_id: 0,
                size: self.fs.size,
                block_size: 4096,
                blocks: 0,
                rdev: Default::default(),
                atime: axfs_ng_vfs::Timestamp::ZERO,
                btime: axfs_ng_vfs::Timestamp::ZERO,
                mtime: axfs_ng_vfs::Timestamp::ZERO,
                ctime: axfs_ng_vfs::Timestamp::ZERO,
            })
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            &*self.fs
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn flags(&self) -> NodeFlags {
            self.fs.flags
        }

        fn open(&self, _read: bool, _write: bool) -> VfsResult<()> {
            self.fs.open_calls.fetch_add(1, Ordering::AcqRel);
            if self.fs.fail_open.load(Ordering::Acquire) {
                Err(VfsError::PermissionDenied)
            } else {
                Ok(())
            }
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }

        fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
            Some(self)
        }
    }

    impl XattrProvider for IoContractNode {
        fn get_xattr(&self, _name: &[u8]) -> VfsResult<Vec<u8>> {
            Err(LinuxError::ENODATA.into())
        }

        fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
            Ok(Vec::new())
        }

        fn set_xattr(&self, _name: &[u8], _value: &[u8], _mode: XattrSetMode) -> VfsResult<()> {
            Ok(())
        }

        fn remove_xattr(&self, _name: &[u8]) -> VfsResult<()> {
            self.fs.remove_xattr_calls.fetch_add(1, Ordering::AcqRel);
            if self.fs.fail_remove_xattr.load(Ordering::Acquire) {
                Err(VfsError::Io)
            } else {
                Err(LinuxError::ENODATA.into())
            }
        }
    }

    impl Pollable for IoContractNode {
        fn poll(&self) -> IoEvents {
            IoEvents::READABLE | IoEvents::WRITABLE
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileNodeOps for IoContractNode {
        fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
            Ok(0)
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
            self.fs.write_offsets.lock().unwrap().push(offset);
            Ok(buf.len())
        }

        fn write_at_vectored(&self, bufs: &[&[u8]], offset: u64) -> VfsResult<usize> {
            let _ = offset;
            bufs.iter().try_fold(0usize, |total, buf| {
                total.checked_add(buf.len()).ok_or(VfsError::InvalidInput)
            })
        }

        fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
            Ok((buf.len(), self.fs.size + buf.len() as u64))
        }

        fn set_len(&self, _len: u64) -> VfsResult<()> {
            self.fs.set_len_calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn set_symlink(&self, _target: &str) -> VfsResult<()> {
            Err(VfsError::InvalidInput)
        }
    }

    #[test]
    fn axfs_pinned_segment_adapter_is_fixed_and_bounded() {
        let segments: [UserIoPinSegment; USER_IOV_FAST_MAX_SEGMENTS] =
            core::array::from_fn(|index| UserIoPinSegment {
                paddr: 0x1000 + index * 0x1000,
                len: 0x1000,
            });
        let physical = axfs_pinned_segments(&segments).unwrap();
        assert_eq!(physical.as_slice().len(), USER_IOV_FAST_MAX_SEGMENTS);
        assert_eq!(physical.as_slice()[3].paddr(), 0x4000);

        let overflow: [UserIoPinSegment; USER_IOV_FAST_MAX_SEGMENTS + 1] =
            core::array::from_fn(|index| UserIoPinSegment {
                paddr: 0x1000 + index * 0x1000,
                len: 0x1000,
            });
        assert_eq!(
            axfs_pinned_segments(&overflow).err(),
            Some(AxError::InvalidInput)
        );
    }

    #[test]
    fn io_uring_dma_clip_keeps_exact_subrange_without_allocating() {
        let segments = [
            UserIoPinSegment {
                paddr: 0x10_000,
                len: 0x800,
            },
            UserIoPinSegment {
                paddr: 0x20_000,
                len: 0x800,
            },
            UserIoPinSegment {
                paddr: 0x30_000,
                len: 0x800,
            },
        ];
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
        let count = clip_io_uring_dma_segments(&segments, 0x200, 0x1_000, &mut physical);
        assert_eq!(count, Some(3));
        assert_eq!(
            &physical[..3],
            &[
                PhysicalIoSegment::new(0x10_200, 0x600),
                PhysicalIoSegment::new(0x20_000, 0x800),
                PhysicalIoSegment::new(0x30_000, 0x200),
            ]
        );
    }

    #[test]
    fn io_uring_dma_clip_rejects_more_than_four_physical_ranges() {
        let segments: [UserIoPinSegment; IO_URING_DMA_MAX_SEGMENTS + 1] =
            core::array::from_fn(|index| UserIoPinSegment {
                paddr: 0x10_000 + index * 0x2_000,
                len: 0x1_000,
            });
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
        assert_eq!(
            clip_io_uring_dma_segments(&segments, 0, segments.len() * 0x1_000, &mut physical),
            None
        );
    }

    #[test]
    fn io_uring_dma_clip_accepts_one_through_four_sg_ranges() {
        let segments: [UserIoPinSegment; IO_URING_DMA_MAX_SEGMENTS] =
            core::array::from_fn(|index| UserIoPinSegment {
                paddr: 0x60_000 + index * 0x2_000,
                len: 0x1_000,
            });
        for count in 1..=IO_URING_DMA_MAX_SEGMENTS {
            let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
            assert_eq!(
                clip_io_uring_dma_segments(&segments[..count], 0, count * 0x1_000, &mut physical),
                Some(count)
            );
        }
    }

    #[test]
    fn io_uring_dma_clip_merges_adjacent_physical_pages_for_256k_request() {
        let segments: [UserIoPinSegment; 64] = core::array::from_fn(|index| UserIoPinSegment {
            paddr: 0x40_000 + index * 0x1_000,
            len: 0x1_000,
        });
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
        let count = clip_io_uring_dma_segments(&segments, 0, IO_URING_DMA_MAX_BYTES, &mut physical);
        assert_eq!(count, Some(1));
        assert_eq!(
            physical[0],
            PhysicalIoSegment::new(0x40_000, IO_URING_DMA_MAX_BYTES)
        );
    }

    #[test]
    fn io_uring_dma_clip_reports_sg_cap_only_for_nonadjacent_ranges() {
        let segments: [UserIoPinSegment; IO_URING_DMA_MAX_SEGMENTS + 1] =
            core::array::from_fn(|index| UserIoPinSegment {
                paddr: 0x80_000 + index * 0x2_000,
                len: 0x1_000,
            });
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
        assert_eq!(
            clip_io_uring_dma_segments_with_reason(
                &segments,
                0,
                segments.len() * 0x1_000,
                &mut physical,
            ),
            Err(crate::file::io_uring::IoUringDmaFallbackReason::SgCap)
        );
    }

    #[test]
    fn io_uring_dma_geometry_requires_private_aligned_nonzero_range() {
        assert!(fixed_dma_geometry_eligible(
            0x2000,
            0x1000,
            0x4000,
            true,
            UserIoPinProvenance::PrivateAnonymous,
        ));
        assert!(!fixed_dma_geometry_eligible(
            0x2000,
            0x1000,
            0x4000,
            true,
            UserIoPinProvenance::Ineligible,
        ));
        assert!(!fixed_dma_geometry_eligible(
            0x2201,
            0x1000,
            0x4000,
            true,
            UserIoPinProvenance::PrivateAnonymous,
        ));
        assert!(!fixed_dma_geometry_eligible(
            0x2000,
            0,
            0x4000,
            true,
            UserIoPinProvenance::PrivateAnonymous,
        ));
        assert!(!fixed_dma_geometry_eligible(
            0x2000,
            0x1000,
            0x4000,
            false,
            UserIoPinProvenance::PrivateAnonymous,
        ));
    }

    #[test]
    fn io_uring_dma_result_requires_full_completion_and_never_bounces_errors() {
        assert_eq!(
            classify_fixed_dma_result(Some(0x1000), 0x1000),
            Ok(FixedDmaOutcome::Completed(0x1000))
        );
        assert_eq!(
            classify_fixed_dma_result(None, 0x1000),
            Ok(FixedDmaOutcome::Fallback)
        );
        assert_eq!(
            classify_fixed_dma_result(Some(0x200), 0x1000),
            Err(AxError::Io)
        );
    }

    #[test]
    fn explicit_write_marker_maps_to_espipe_only() {
        assert!(check_positioned_write_flags(NodeFlags::empty()).is_ok());
        assert_eq!(
            check_positioned_write_flags(NodeFlags::NO_POSITIONED_WRITE),
            Err(AxError::from(LinuxError::ESPIPE))
        );
    }

    #[test]
    fn stream_cursor_and_explicit_io_capabilities_are_independent() {
        let stream = NodeFlags::STREAM;
        assert!(check_positioned_read_flags(stream).is_ok());
        assert!(check_positioned_write_flags(stream).is_ok());

        assert_eq!(
            check_positioned_read_flags(stream | NodeFlags::NO_POSITIONED_READ),
            Err(AxError::from(LinuxError::ESPIPE))
        );
        assert_eq!(
            check_positioned_write_flags(stream | NodeFlags::NO_POSITIONED_WRITE),
            Err(AxError::from(LinuxError::ESPIPE))
        );
    }

    #[test]
    fn inode_append_classification_excludes_stream_and_positioned_nodes() {
        let open = |flags| {
            let fs = IoContractFs::new(flags, 4096);
            let mut options = OpenOptions::new();
            options.write(true);
            File::new(
                options
                    .open_loc(fs.location())
                    .unwrap()
                    .into_file()
                    .unwrap(),
            )
        };
        let append = OfdIoStatus::new(O_APPEND);

        let regular = open(NodeFlags::NON_CACHEABLE);
        assert!(write_uses_inode_append(regular.inner(), append));
        assert!(!write_uses_current_position(regular.inner(), append));

        let stream = open(NodeFlags::NON_CACHEABLE | NodeFlags::STREAM);
        assert!(!write_uses_inode_append(stream.inner(), append));
        assert!(!write_uses_current_position(stream.inner(), append));

        let positioned = open(NodeFlags::NON_CACHEABLE | NodeFlags::POSITIONED_APPEND);
        assert!(!write_uses_inode_append(positioned.inner(), append));
        assert!(write_uses_current_position(positioned.inner(), append));
    }

    #[test]
    fn generic_regular_file_is_not_worker_safe_even_with_fixed_direct_plan() {
        let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 4096);
        let mut options = OpenOptions::new();
        options.read(true).direct(true);
        let file = Arc::new(File::new(
            options
                .open_loc(fs.location())
                .unwrap()
                .into_file()
                .unwrap(),
        ));
        let segments = [UserIoPinSegment {
            paddr: 0x2000,
            len: 0x1000,
        }];
        let fixed = Some((
            segments.as_slice(),
            0,
            0x1000,
            true,
            UserIoPinProvenance::PrivateAnonymous,
        ));

        assert!(file_uses_direct_io(file.as_ref()));
        assert!(!regular_ext4_physical_worker_plan(
            file.as_ref(),
            OfdIoStatus::new(0),
            PreparedPhysicalIoOperation::Read,
            0x2000,
            0x1000,
            0,
            fixed,
        ));
    }

    #[test]
    fn zero_offset_io_uses_stream_dispatch_for_tty_like_files_only() {
        let _context = crate::test_support::scheduler_test_context();
        let stream_fs = IoContractFs::new_with_type(
            NodeFlags::NON_CACHEABLE
                | NodeFlags::STREAM
                | NodeFlags::NO_POSITIONED_READ
                | NodeFlags::NO_POSITIONED_WRITE,
            0,
            NodeType::CharacterDevice,
        );
        let mut stream_options = OpenOptions::new();
        stream_options.read(true).write(true);
        let stream_file = File::new(
            stream_options
                .open_loc(stream_fs.location())
                .unwrap()
                .into_file()
                .unwrap(),
        );
        let stream: Arc<dyn FileLike> = Arc::new(stream_file);
        let stream_description = FileDescription::new(stream).unwrap();
        let stream_handle =
            FileHandle::<dyn FileLike>::from_description_for_test(stream_description);
        assert!(zero_offset_stream_file_like(
            &stream_handle,
            NodeFlags::NO_POSITIONED_READ
        ));
        assert!(zero_offset_stream_file_like(
            &stream_handle,
            NodeFlags::NO_POSITIONED_WRITE
        ));

        // An anon-inode FileLike has no positioned operation at all. It must
        // still use its direct read/write methods at offset zero.
        let event: Arc<dyn FileLike> = crate::file::event::EventFd::new(0, false);
        let event_description = FileDescription::new(event).unwrap();
        let event_handle = FileHandle::<dyn FileLike>::from_description_for_test(event_description);
        assert!(zero_offset_stream_file_like(
            &event_handle,
            NodeFlags::NO_POSITIONED_READ
        ));
        assert!(zero_offset_stream_file_like(
            &event_handle,
            NodeFlags::NO_POSITIONED_WRITE
        ));

        // A regular file remains on the positioned path even if a malformed
        // test backend happens to advertise the stream prohibition flags.
        let regular_fs = IoContractFs::new(
            NodeFlags::NON_CACHEABLE
                | NodeFlags::STREAM
                | NodeFlags::NO_POSITIONED_READ
                | NodeFlags::NO_POSITIONED_WRITE,
            0,
        );
        let mut regular_options = OpenOptions::new();
        regular_options.read(true).write(true);
        let regular_file = File::new(
            regular_options
                .open_loc(regular_fs.location())
                .unwrap()
                .into_file()
                .unwrap(),
        );
        let regular: Arc<dyn FileLike> = Arc::new(regular_file);
        let regular_description = FileDescription::new(regular).unwrap();
        let regular_handle =
            FileHandle::<dyn FileLike>::from_description_for_test(regular_description);
        assert!(!zero_offset_stream_file_like(
            &regular_handle,
            NodeFlags::NO_POSITIONED_READ
        ));
        assert!(!zero_offset_stream_file_like(
            &regular_handle,
            NodeFlags::NO_POSITIONED_WRITE
        ));
    }

    #[test]
    fn stream_write_admission_rejects_read_only_mount_for_nonzero_io() {
        let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 0);
        let filesystem = Filesystem::new(fs.clone());
        let mountpoint = Mountpoint::new_root(&filesystem);
        mounts::initialize_test_mount(&mountpoint, 1).unwrap();
        let mut options = OpenOptions::new();
        options.write(true);
        let file = File::new(
            options
                .open_loc(mountpoint.root_location())
                .unwrap()
                .into_file()
                .unwrap(),
        );

        assert_eq!(
            check_file_write_admission(&file, 1),
            Err(AxError::ReadOnlyFilesystem)
        );
        // Ordinary sys_write permits a zero-length request after access
        // admission, so the io_uring stream path must preserve that rule.
        assert!(check_file_write_admission(&file, 0).is_ok());
    }

    #[repr(align(512))]
    struct AlignedDirectBuffer([u8; DIRECT_IO_ALIGNMENT]);

    #[test]
    fn current_position_admission_and_write_exclude_shared_ofd_seek() {
        let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 4096);
        let mut options = OpenOptions::new();
        options.write(true).direct(true);
        let file = Arc::new(File::new(
            options
                .open_loc(fs.location())
                .unwrap()
                .into_file()
                .unwrap(),
        ));
        let buffer = Arc::new(AlignedDirectBuffer([0x5a; DIRECT_IO_ALIGNMENT]));
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let writer_file = file.clone();
        let writer_buffer = buffer.clone();
        let writer_fs = fs.clone();
        let writer = thread::spawn(move || {
            with_current_position_io(writer_file.as_ref(), DIRECT_IO_ALIGNMENT, |offset| {
                validate_direct_io(
                    writer_file.as_ref(),
                    writer_buffer.0.as_ptr() as usize,
                    writer_buffer.0.len(),
                    offset,
                )?;
                admitted_tx.send(offset).unwrap();
                release_rx.recv().unwrap();
                // Model the positioned backend callback used by both fast and
                // fallback paths without requiring a host kernel task.
                writer_fs.write_offsets.lock().unwrap().push(offset);
                let written = writer_buffer.0.len();
                Ok((written, written))
            })
        });
        assert_eq!(admitted_rx.recv().unwrap(), 0);

        let (seek_started_tx, seek_started_rx) = mpsc::channel();
        let (seek_done_tx, seek_done_rx) = mpsc::channel();
        let seeker_file = file.clone();
        let seeker = thread::spawn(move || {
            seek_started_tx.send(()).unwrap();
            let mut inner = seeker_file.inner();
            let position = inner
                .seek(SeekFrom::Current(DIRECT_IO_ALIGNMENT as i64))
                .unwrap();
            seek_done_tx.send(position).unwrap();
        });
        seek_started_rx.recv().unwrap();
        assert_eq!(
            seek_done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        release_tx.send(()).unwrap();
        assert_eq!(writer.join().unwrap(), Ok(DIRECT_IO_ALIGNMENT));
        assert_eq!(
            seek_done_rx.recv_timeout(Duration::from_secs(1)),
            Ok((DIRECT_IO_ALIGNMENT * 2) as u64)
        );
        seeker.join().unwrap();
        assert_eq!(&*fs.write_offsets.lock().unwrap(), &[0]);
    }

    #[test]
    fn open_hook_rejects_before_truncate_side_effects() {
        let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 17);
        fs.fail_open.store(true, Ordering::Release);
        let mut options = OpenOptions::new();
        options.write(true).truncate(true);
        assert!(matches!(
            options.open_loc(fs.location()),
            Err(VfsError::PermissionDenied)
        ));
        assert_eq!(fs.open_calls.load(Ordering::Acquire), 1);
        assert_eq!(fs.set_len_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn privilege_cleanup_failure_rejects_before_truncate_side_effects() {
        executable::init().unwrap();
        let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 17);
        fs.fail_remove_xattr.store(true, Ordering::Release);
        let mut options = OpenOptions::new();
        options.write(true);
        let file = options
            .open_loc(fs.location())
            .unwrap()
            .into_file()
            .unwrap();
        let namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let security = VfsSecurityContext::new(crate::task::Cred::try_root(namespace).unwrap());

        let result = (|| {
            let location = file.backend()?.location();
            let _privilege_guard = begin_inode_content_write(location, &security)?;
            file.backend()?.set_len(0)
        })();
        assert_eq!(result, Err(AxError::Io));
        assert_eq!(fs.remove_xattr_calls.load(Ordering::Acquire), 1);
        assert_eq!(fs.set_len_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn sendfile_destination_rejects_authoritative_ofd_append_status() {
        assert!(check_sendfile_destination_status(0).is_ok());
        assert_eq!(
            check_sendfile_destination_status(O_APPEND),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn copy_file_range_effective_count_uses_eof_short_source_and_write_limit() {
        let eof = copy_file_range_source_count(MAX_FILE_OFFSET - 1, 4, 0).unwrap();
        assert_eq!(eof, 0);
        assert_eq!(
            copy_file_range_effective_count(
                MAX_FILE_OFFSET - 1,
                MAX_FILE_OFFSET - 1,
                eof,
                eof,
                true,
            ),
            Ok(0)
        );

        assert_eq!(
            copy_file_range_effective_count(0, MAX_FILE_OFFSET - 1, 4, 4, false),
            Ok(1)
        );
        assert_eq!(
            copy_file_range_effective_count(0, MAX_FILE_OFFSET, 1, 1, false),
            Err(AxError::from(LinuxError::EFBIG))
        );

        // The requested ranges overlap (0..8 and 5..13), but only four
        // source bytes exist, so the effective ranges do not.
        let short = copy_file_range_source_count(0, 8, 4).unwrap();
        assert_eq!(short, 4);
        assert_eq!(
            copy_file_range_effective_count(0, 5, short, short, true),
            Ok(4)
        );

        // A destination limit can make otherwise overlapping requested
        // ranges disjoint before the overlap and destination-end checks.
        assert_eq!(copy_file_range_effective_count(0, 8, 10, 3, true), Ok(3));
        assert_eq!(
            copy_file_range_effective_count(0, 2, 4, 4, true),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn copy_file_range_source_wrap_precedes_eof_clamping() {
        let error = copy_file_range_source_count(u64::MAX - 1, 4, 0).unwrap_err();
        assert_eq!(LinuxError::from(error), LinuxError::EOVERFLOW);
    }

    #[test]
    fn splice_nonblocking_is_derived_only_from_explicit_or_pipe_status() {
        assert!(splice_operation_nonblocking(
            SPLICE_F_NONBLOCK,
            false,
            false,
            true,
            false,
        ));
        assert!(splice_operation_nonblocking(0, true, true, false, true));
        assert!(splice_operation_nonblocking(0, true, false, true, true));
        assert!(!splice_operation_nonblocking(0, false, true, true, false,));
        assert!(!splice_operation_nonblocking(0, true, false, false, true,));

        // The buffered path freezes each endpoint separately. A source socket
        // keeps its own O_NONBLOCK, an output pipe can make source admission
        // nonblocking, and SPLICE_F_NONBLOCK does not override a socket
        // destination's own blocking mode.
        assert_eq!(
            splice_endpoint_nonblocking(0, true, true, false),
            (true, false)
        );
        assert_eq!(
            splice_endpoint_nonblocking(0, false, true, true),
            (true, true)
        );
        assert_eq!(
            splice_endpoint_nonblocking(SPLICE_F_NONBLOCK, false, false, false),
            (true, false)
        );
        assert_eq!(
            splice_endpoint_nonblocking(0, false, false, true),
            (false, true)
        );
    }

    #[test]
    fn reciprocal_transfers_use_the_same_bounded_attempt_lock_order() {
        let forward = ordered_transfer_attempt_lock_indices(0x1234, 0xfeed_beef);
        let reverse = ordered_transfer_attempt_lock_indices(0xfeed_beef, 0x1234);
        assert_eq!(forward, reverse);
        assert!(forward.0 <= forward.1);
        assert!(forward.1 < TRANSFER_ATTEMPT_LOCK_COUNT);

        let same = ordered_transfer_attempt_lock_indices(0x1234, 0x1234);
        assert_eq!(same.0, same.1);
    }

    #[test]
    fn transfer_eagain_waits_on_the_endpoint_that_was_attempted() {
        assert_eq!(transfer_wait_endpoint(false), TransferWaitEndpoint::Source);
        assert_eq!(
            transfer_wait_endpoint(true),
            TransferWaitEndpoint::Destination
        );
    }

    #[test]
    fn transfer_driver_counts_short_destination_prefix_before_stopping() {
        let mut calls = 0;
        let transferred = drive_send_with(8, |_buf, _total| {
            calls += 1;
            Ok(Some(SendStep {
                written: 3,
                destination_short: true,
            }))
        })
        .unwrap();
        assert_eq!(transferred, 3);
        assert_eq!(calls, 1);
    }

    #[test]
    fn transfer_driver_returns_progress_instead_of_later_error() {
        let mut calls = 0;
        let transferred = drive_send_with(8, |_buf, total| {
            calls += 1;
            if total == 0 {
                Ok(Some(SendStep {
                    written: 4,
                    destination_short: false,
                }))
            } else {
                Err(AxError::InvalidInput)
            }
        })
        .unwrap();
        assert_eq!(transferred, 4);
        assert_eq!(calls, 2);

        assert_eq!(
            drive_send_with(8, |_buf, _total| Err(AxError::InvalidInput)),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn transfer_writer_commits_a_stream_prefix_before_a_later_error() {
        let mut calls = 0;
        let mut destination = |buf: &[u8]| {
            calls += 1;
            if calls == 1 {
                Ok(buf.len())
            } else {
                Err(AxError::WouldBlock)
            }
        };
        let mut writer = TransferWriter::new(4, &mut destination);

        assert_eq!(writer.write(b"ab"), Ok(2));
        assert_eq!(writer.write(b"cd"), Ok(0));
        assert_eq!(writer.written, 2);
        assert!(writer.destination_short);
    }

    #[test]
    fn sync_file_range_checks_signed_loff_t_overflow_but_keeps_zero_to_eof() {
        assert_eq!(sync_file_range_end(i64::MAX, 1), Err(AxError::InvalidInput));
        assert_eq!(sync_file_range_end(i64::MAX, 0), Ok(0));
        assert_eq!(sync_file_range_end(i64::MAX - 1, 1), Ok(i64::MAX as u64));
    }

    #[test]
    fn sync_file_range_validates_a_pipe_range_before_the_type_error() {
        // `sys_sync_file_range` calls this helper after successful fd lookup
        // and before its NodeType::Pipe ESPIPE branch.
        assert_eq!(
            validate_sync_file_range_args(i64::MAX, 1, 0),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn ftruncate_admission_matches_linux_fdget_and_do_ftruncate() {
        let cases = [
            (false, FileLikeKind::Regular, false, true, AxError::BadFileDescriptor),
            (true, FileLikeKind::Regular, true, true, AxError::BadFileDescriptor),
            (true, FileLikeKind::Directory, false, true, AxError::InvalidInput),
            (true, FileLikeKind::Fifo, false, true, AxError::InvalidInput),
            (true, FileLikeKind::Socket, false, true, AxError::InvalidInput),
            (true, FileLikeKind::Other, false, true, AxError::InvalidInput),
            (true, FileLikeKind::Regular, false, false, AxError::InvalidInput),
        ];
        for (fd_found, kind, path_only, writable, expected) in cases {
            assert_eq!(
                ftruncate_admission_errno(fd_found, kind, path_only, writable),
                Err(expected)
            );
        }
    }

    #[test]
    fn ftruncate_negative_length_precedes_invalid_and_opath_fd_admission() {
        for (fd_found, path_only) in [(false, false), (true, true)] {
            let result = ftruncate_length_errno(-1)
                .and_then(|()| ftruncate_admission_errno(fd_found, FileLikeKind::Regular, path_only, true));
            assert_eq!(result, Err(AxError::InvalidInput));
        }
    }
}
