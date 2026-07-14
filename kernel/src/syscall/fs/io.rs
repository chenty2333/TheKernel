use alloc::{vec, vec::Vec};
use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FileFlags, OpenOptions, WritePlacement};
use axfs_ng_vfs::{Location, MetadataUpdate, NodeFlags};
use axio::{IoBufMut, Seek, SeekFrom, Write};
use axnet::SocketTransferDirection;
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use linux_raw_sys::general::{
    __kernel_off_t, IN_ACCESS, IN_ATTRIB, IN_MODIFY, O_APPEND, O_DSYNC, O_SYNC, W_OK,
};
use spin::Lazy;
use starry_vm::{VmMutPtr, VmPtr};
use syscalls::Sysno;

use crate::{
    file::{
        Directory, File, FileHandle, FileLike, FileLikeKind, IoDst, IoSrc, OfdIoStatus, PidFd,
        Pipe, Socket, allowed_write_len, check_resize_limit, executable, flock, get_file_like,
        get_typed_file, inode_flags,
        inotify::{notify_exact, notify_parent, notify_read, notify_write},
        lease, memfd,
        permission::{DacFsContextExt, check_open_permissions, check_writable_mount},
        pipe::{NamedPipe, PipeEndpoint},
    },
    mm::{
        IoVec, IoVectorBuf, PinnedUserSegments, PinnedUserSegmentsMut, UserConstPtr, VmBytes,
        VmBytesMut, prefault_user_io_from_user, prefault_user_io_to_user,
        record_user_io_async_direct_read, record_user_io_async_direct_write,
        record_user_io_async_resource_unpins, record_user_io_async_signal_after_submit,
        record_user_io_async_submit_fallback, record_user_io_direct_read,
        record_user_io_direct_read_fallback, record_user_io_direct_write,
        record_user_io_direct_write_fallback, try_pin_user_segments_from_user,
        try_pin_user_segments_to_user, try_pin_user_slice_from_user, try_pin_user_slice_to_user,
        try_with_pinned_user_segment_mut_slices, user_io_async_direct_enabled,
        with_pinned_user_segment_slices,
    },
    mounts,
    pseudofs::tmp,
    readiness::block_on_poll_io,
    task::AsThread,
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
// Regular-file O_DIRECT is constrained by logical sector alignment. Valid
// 512-byte offsets and 1 KiB transfers must not inherit a 4 KiB alignment.
const DIRECT_IO_ALIGNMENT: usize = 512;
const USER_SLICE_FAST_MIN: usize = 4096;
const USER_IOV_FAST_MAX_SEGMENTS: usize = 64;
const USER_COPY_PREFAULT_MIN: usize = 16 * 1024;
const USER_DIRECT_ASYNC_ALIGNMENT: usize = 4096;
const TRANSFER_ATTEMPT_LOCK_COUNT: usize = 64;

static TRANSFER_ATTEMPT_LOCKS: Lazy<[Mutex<()>; TRANSFER_ATTEMPT_LOCK_COUNT]> =
    Lazy::new(|| core::array::from_fn(|_| Mutex::new(())));

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
        mtime: Some(now),
        ctime: Some(now),
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
    if !file_uses_direct_io(file) || iov.len() == 0 {
        return Ok(());
    }
    if !(offset as usize).is_multiple_of(DIRECT_IO_ALIGNMENT)
        || !iov.len().is_multiple_of(DIRECT_IO_ALIGNMENT)
        || !iov.is_aligned(DIRECT_IO_ALIGNMENT)?
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
    if let Some(file) = file_like.downcast_ref::<File>() {
        file.read_with_status(status, dst)
    } else if let Some(pipe) = file_like.downcast_ref::<Pipe>() {
        pipe.read_with_nonblocking(dst, status.nonblocking())
    } else if let Some(pipe) = file_like.downcast_ref::<NamedPipe>() {
        pipe.read_with_nonblocking(dst, status.nonblocking())
    } else if let Some(socket) = file_like.downcast_ref::<Socket>() {
        socket.read_with_nonblocking(dst, status.nonblocking())
    } else {
        file_like.read(dst)
    }
}

fn write_file_like_with_status(
    file_like: &FileHandle<dyn FileLike>,
    status: OfdIoStatus,
    src: &mut IoSrc,
) -> AxResult<usize> {
    if let Some(file) = file_like.downcast_ref::<File>() {
        file.write_with_status(status, src)
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

fn regular_file_read_prefault_len(file: &File, len: usize, offset: u64) -> AxResult<usize> {
    let size = file.inner().location().len()?;
    if offset >= size {
        return Ok(0);
    }
    let available = size - offset;
    Ok(len.min(available.min(usize::MAX as u64) as usize))
}

fn prefault_regular_file_read_fallback(
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
        prefault_user_io_to_user(buf, len)?;
    }
    Ok(())
}

fn prefault_regular_file_write_fallback(file: &File, buf: *const u8, len: usize) -> AxResult<()> {
    if len < USER_COPY_PREFAULT_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(());
    }
    prefault_user_io_from_user(buf, len)?;
    Ok(())
}

fn user_direct_async_base_enabled() -> bool {
    user_io_async_direct_enabled() && axdriver::virtio_async_block_enabled()
}

fn user_direct_async_candidate(offset: u64, len: usize) -> bool {
    user_direct_async_base_enabled()
        && len >= USER_SLICE_FAST_MIN
        && len.is_multiple_of(USER_DIRECT_ASYNC_ALIGNMENT)
        && (offset as usize).is_multiple_of(USER_DIRECT_ASYNC_ALIGNMENT)
}

fn user_direct_async_segments_candidate(offset: u64, len: usize, segments: usize) -> bool {
    user_direct_async_candidate(offset, len) && segments != 0
}

fn user_direct_async_reject_if_enabled() {
    if user_direct_async_base_enabled() {
        record_user_io_async_submit_fallback();
    }
}

fn run_user_direct_async_io<T, E>(op: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let was_interrupted = axtask::current_may_uninit().is_some_and(|task| task.is_interrupted());
    let result = op();
    if !was_interrupted && axtask::current_may_uninit().is_some_and(|task| task.is_interrupted()) {
        record_user_io_async_signal_after_submit();
    }
    result
}

fn user_direct_async_segments_ok(segments: impl IntoIterator<Item = (usize, usize)>) -> bool {
    segments.into_iter().all(|(paddr, len)| {
        len != 0
            && len.is_multiple_of(USER_DIRECT_ASYNC_ALIGNMENT)
            && paddr.is_multiple_of(USER_DIRECT_ASYNC_ALIGNMENT)
    })
}

fn read_vectored_slice_sync(file: &File, bufs: &mut [&mut [u8]]) -> AxResult<usize> {
    let mut total = 0usize;
    for buf in bufs.iter_mut() {
        if buf.is_empty() {
            continue;
        }
        let requested = buf.len();
        let read = file.inner().read_slice(buf)?;
        total += read;
        if read < requested || read == 0 {
            break;
        }
    }
    Ok(total)
}

fn read_at_vectored_slice_sync(
    file: &File,
    bufs: &mut [&mut [u8]],
    mut offset: u64,
) -> AxResult<usize> {
    let mut total = 0usize;
    for buf in bufs.iter_mut() {
        if buf.is_empty() {
            continue;
        }
        let requested = buf.len();
        let read = file.inner().read_at_slice(buf, offset)?;
        total += read;
        offset = offset
            .checked_add(read as u64)
            .ok_or(AxError::InvalidInput)?;
        if read < requested || read == 0 {
            break;
        }
    }
    Ok(total)
}

fn read_vectored_slice_non_async(file: &File, bufs: &mut [&mut [u8]]) -> AxResult<usize> {
    if axdriver::virtio_async_block_enabled() {
        read_vectored_slice_sync(file, bufs)
    } else {
        Ok(file.inner().read_vectored_slice(bufs)?)
    }
}

fn read_at_vectored_slice_non_async(
    file: &File,
    bufs: &mut [&mut [u8]],
    offset: u64,
) -> AxResult<usize> {
    if axdriver::virtio_async_block_enabled() {
        read_at_vectored_slice_sync(file, bufs, offset)
    } else {
        Ok(file.inner().read_at_vectored_slice(bufs, offset)?)
    }
}

fn write_vectored_slice_sync(file: &File, status: OfdIoStatus, bufs: &[&[u8]]) -> AxResult<usize> {
    let mut total = 0usize;
    let placement = unpositioned_write_placement(file.inner(), status);
    for buf in bufs.iter().copied() {
        if buf.is_empty() {
            continue;
        }
        let requested = buf.len();
        let written = file.inner().write_slice_with_placement(buf, placement)?;
        total += written;
        if written < requested || written == 0 {
            break;
        }
    }
    Ok(total)
}

fn write_at_vectored_slice_sync(file: &File, bufs: &[&[u8]], mut offset: u64) -> AxResult<usize> {
    let mut total = 0usize;
    for buf in bufs.iter().copied() {
        if buf.is_empty() {
            continue;
        }
        let requested = buf.len();
        let written = file.inner().write_at_slice(buf, offset)?;
        total += written;
        offset = offset
            .checked_add(written as u64)
            .ok_or(AxError::InvalidInput)?;
        if written < requested || written == 0 {
            break;
        }
    }
    Ok(total)
}

fn write_vectored_slice_non_async(
    file: &File,
    status: OfdIoStatus,
    bufs: &[&[u8]],
) -> AxResult<usize> {
    if axdriver::virtio_async_block_enabled() {
        write_vectored_slice_sync(file, status, bufs)
    } else {
        Ok(file.inner().write_vectored_slice_with_placement(
            bufs,
            unpositioned_write_placement(file.inner(), status),
        )?)
    }
}

fn write_at_vectored_slice_non_async(file: &File, bufs: &[&[u8]], offset: u64) -> AxResult<usize> {
    if axdriver::virtio_async_block_enabled() {
        write_at_vectored_slice_sync(file, bufs, offset)
    } else {
        Ok(file.inner().write_at_vectored_slice(bufs, offset)?)
    }
}

fn try_regular_file_read_user_slice(
    file: &File,
    buf: *mut u8,
    len: usize,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(mut pinned) = try_pin_user_slice_to_user(buf, len) else {
        record_user_io_direct_read_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let offset = current_file_offset(file.inner())?;
    let read = if user_direct_async_segments_candidate(offset, len, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        ) {
        let mut bufs = [pinned.as_mut_slice()];
        let read = run_user_direct_async_io(|| file.inner().read_vectored_slice(&mut bufs))?;
        record_user_io_async_direct_read(read, segments);
        record_user_io_async_resource_unpins(1);
        read
    } else {
        user_direct_async_reject_if_enabled();
        file.inner().read_slice(pinned.as_mut_slice())?
    };
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_read_user_segments(
    file: &File,
    buf: *mut u8,
    len: usize,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(mut pinned) = try_pin_user_segments_to_user(buf, len) else {
        record_user_io_direct_read_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    let offset = current_file_offset(file.inner())?;
    let async_direct = user_direct_async_segments_candidate(offset, len, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        );
    let read = pinned.with_segment_mut_slices(|segments| {
        if async_direct {
            run_user_direct_async_io(|| file.inner().read_vectored_slice(segments))
        } else {
            read_vectored_slice_non_async(file, segments)
        }
    })?;
    if async_direct {
        record_user_io_async_direct_read(read, segments);
        record_user_io_async_resource_unpins(1);
    } else {
        user_direct_async_reject_if_enabled();
    }
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_pread_user_slice(
    file: &File,
    buf: *mut u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(mut pinned) = try_pin_user_slice_to_user(buf, len) else {
        record_user_io_direct_read_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let read = if user_direct_async_segments_candidate(offset, len, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        ) {
        let mut bufs = [pinned.as_mut_slice()];
        let read =
            run_user_direct_async_io(|| file.inner().read_at_vectored_slice(&mut bufs, offset))?;
        record_user_io_async_direct_read(read, segments);
        record_user_io_async_resource_unpins(1);
        read
    } else {
        user_direct_async_reject_if_enabled();
        file.inner().read_at_slice(pinned.as_mut_slice(), offset)?
    };
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_pread_user_segments(
    file: &File,
    buf: *mut u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(mut pinned) = try_pin_user_segments_to_user(buf, len) else {
        record_user_io_direct_read_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    let async_direct = user_direct_async_segments_candidate(offset, len, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        );
    let read = pinned.with_segment_mut_slices(|segments| {
        if async_direct {
            run_user_direct_async_io(|| file.inner().read_at_vectored_slice(segments, offset))
        } else {
            read_at_vectored_slice_non_async(file, segments, offset)
        }
    })?;
    if async_direct {
        record_user_io_async_direct_read(read, segments);
        record_user_io_async_resource_unpins(1);
    } else {
        user_direct_async_reject_if_enabled();
    }
    record_user_io_direct_read(read, segments);
    Ok(Some(read))
}

fn try_regular_file_write_user_slice(
    file: &File,
    status: OfdIoStatus,
    buf: *const u8,
    len: usize,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    let offset = current_write_offset(file.inner(), status)?;
    executable::check_not_active(file.inner().location())?;
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    memfd::check_write(file.inner().location(), offset, allowed)?;

    let Some(pinned) = try_pin_user_slice_from_user(buf, allowed) else {
        record_user_io_direct_write_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let written = if user_direct_async_segments_candidate(offset, allowed, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        ) {
        let bufs = [pinned.as_slice()];
        let written = run_user_direct_async_io(|| {
            file.inner()
                .write_vectored_slice_with_placement(&bufs, WritePlacement::Current)
        })?;
        record_user_io_async_direct_write(written, segments);
        record_user_io_async_resource_unpins(1);
        written
    } else {
        user_direct_async_reject_if_enabled();
        file.inner()
            .write_slice_with_placement(pinned.as_slice(), WritePlacement::Current)?
    };
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_write_user_segments(
    file: &File,
    status: OfdIoStatus,
    buf: *const u8,
    len: usize,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    let offset = current_write_offset(file.inner(), status)?;
    executable::check_not_active(file.inner().location())?;
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    memfd::check_write(file.inner().location(), offset, allowed)?;

    let Some(pinned) = try_pin_user_segments_from_user(buf, allowed) else {
        record_user_io_direct_write_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    let async_direct = user_direct_async_segments_candidate(offset, allowed, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        );
    let written = pinned.with_segment_slices(|segments| {
        if async_direct {
            run_user_direct_async_io(|| {
                file.inner()
                    .write_vectored_slice_with_placement(segments, WritePlacement::Current)
            })
        } else {
            write_vectored_slice_non_async(file, status, segments)
        }
    })?;
    if async_direct {
        record_user_io_async_direct_write(written, segments);
        record_user_io_async_resource_unpins(1);
    } else {
        user_direct_async_reject_if_enabled();
    }
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_pwrite_user_slice(
    file: &File,
    status: OfdIoStatus,
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
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    memfd::check_write(file.inner().location(), offset, allowed)?;

    let Some(pinned) = try_pin_user_slice_from_user(buf, allowed) else {
        record_user_io_direct_write_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    debug_assert_eq!(pinned.segments().len(), 1);
    let segments = pinned.segments().len();
    let written = if user_direct_async_segments_candidate(offset, allowed, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        ) {
        let bufs = [pinned.as_slice()];
        let written =
            run_user_direct_async_io(|| file.inner().write_at_vectored_slice(&bufs, offset))?;
        record_user_io_async_direct_write(written, segments);
        record_user_io_async_resource_unpins(1);
        written
    } else {
        user_direct_async_reject_if_enabled();
        file.inner().write_at_slice(pinned.as_slice(), offset)?
    };
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_pwrite_user_segments(
    file: &File,
    status: OfdIoStatus,
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
    let allowed = allowed_write_len(offset, len)?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    memfd::check_write(file.inner().location(), offset, allowed)?;

    let Some(pinned) = try_pin_user_segments_from_user(buf, allowed) else {
        record_user_io_direct_write_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };

    let segments = pinned.segments().len();
    let async_direct = user_direct_async_segments_candidate(offset, allowed, segments)
        && user_direct_async_segments_ok(
            pinned
                .segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len)),
        );
    let written = pinned.with_segment_slices(|segments| {
        if async_direct {
            run_user_direct_async_io(|| file.inner().write_at_vectored_slice(segments, offset))
        } else {
            write_at_vectored_slice_non_async(file, segments, offset)
        }
    })?;
    if async_direct {
        record_user_io_async_direct_write(written, segments);
        record_user_io_async_resource_unpins(1);
    } else {
        user_direct_async_reject_if_enabled();
    }
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_pin_iov_to_user(
    iov: &IoVectorBuf,
    len: usize,
) -> AxResult<Option<Vec<PinnedUserSegmentsMut>>> {
    let mut remaining = len.min(iov.len());
    let mut pinned = Vec::new();
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
        let Some(pin) = try_pin_user_segments_to_user(entry.iov_base, chunk) else {
            return Ok(None);
        };
        segments += pin.segments().len();
        if segments > USER_IOV_FAST_MAX_SEGMENTS {
            return Ok(None);
        }
        pinned.push(pin);
        remaining -= chunk;
    }
    Ok(Some(pinned))
}

fn try_pin_iov_from_user(
    iov: &IoVectorBuf,
    len: usize,
) -> AxResult<Option<Vec<PinnedUserSegments>>> {
    let mut remaining = len.min(iov.len());
    let mut pinned = Vec::new();
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
        let Some(pin) = try_pin_user_segments_from_user(entry.iov_base as *const u8, chunk) else {
            return Ok(None);
        };
        segments += pin.segments().len();
        if segments > USER_IOV_FAST_MAX_SEGMENTS {
            return Ok(None);
        }
        pinned.push(pin);
        remaining -= chunk;
    }
    Ok(Some(pinned))
}

fn try_regular_file_readv_user_segments(file: &File, iov: &IoVectorBuf) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(mut pinned) = try_pin_iov_to_user(iov, iov.len())? else {
        record_user_io_direct_read_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    let offset = current_file_offset(file.inner())?;
    let async_direct = user_direct_async_segments_candidate(offset, iov.len(), segments)
        && user_direct_async_segments_ok(pinned.iter().flat_map(|pin| {
            pin.segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len))
        }));
    match try_with_pinned_user_segment_mut_slices(&mut pinned, |segments| {
        if async_direct {
            run_user_direct_async_io(|| file.inner().read_vectored_slice(segments))
        } else {
            read_vectored_slice_non_async(file, segments)
        }
    }) {
        Some(result) => {
            let read = result?;
            if async_direct {
                record_user_io_async_direct_read(read, segments);
                record_user_io_async_resource_unpins(pinned.len());
            } else {
                user_direct_async_reject_if_enabled();
            }
            record_user_io_direct_read(read, segments);
            Ok(Some(read))
        }
        None => {
            record_user_io_direct_read_fallback();
            user_direct_async_reject_if_enabled();
            Ok(None)
        }
    }
}

fn try_regular_file_preadv_user_segments(
    file: &File,
    iov: &IoVectorBuf,
    offset: u64,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    let Some(mut pinned) = try_pin_iov_to_user(iov, iov.len())? else {
        record_user_io_direct_read_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    let async_direct = user_direct_async_segments_candidate(offset, iov.len(), segments)
        && user_direct_async_segments_ok(pinned.iter().flat_map(|pin| {
            pin.segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len))
        }));
    match try_with_pinned_user_segment_mut_slices(&mut pinned, |segments| {
        if async_direct {
            run_user_direct_async_io(|| file.inner().read_at_vectored_slice(segments, offset))
        } else {
            read_at_vectored_slice_non_async(file, segments, offset)
        }
    }) {
        Some(result) => {
            let read = result?;
            if async_direct {
                record_user_io_async_direct_read(read, segments);
                record_user_io_async_resource_unpins(pinned.len());
            } else {
                user_direct_async_reject_if_enabled();
            }
            record_user_io_direct_read(read, segments);
            Ok(Some(read))
        }
        None => {
            record_user_io_direct_read_fallback();
            user_direct_async_reject_if_enabled();
            Ok(None)
        }
    }
}

fn try_regular_file_writev_user_segments(
    file: &File,
    status: OfdIoStatus,
    iov: &IoVectorBuf,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if status.append() {
        return Ok(None);
    }

    let offset = current_write_offset(file.inner(), status)?;
    executable::check_not_active(file.inner().location())?;
    let allowed = allowed_write_len(offset, iov.len())?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    memfd::check_write(file.inner().location(), offset, allowed)?;

    let Some(pinned) = try_pin_iov_from_user(iov, allowed)? else {
        record_user_io_direct_write_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    let async_direct = user_direct_async_segments_candidate(offset, allowed, segments)
        && user_direct_async_segments_ok(pinned.iter().flat_map(|pin| {
            pin.segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len))
        }));
    let written = with_pinned_user_segment_slices(&pinned, |segments| {
        if async_direct {
            run_user_direct_async_io(|| {
                file.inner()
                    .write_vectored_slice_with_placement(segments, WritePlacement::Current)
            })
        } else {
            write_vectored_slice_non_async(file, status, segments)
        }
    })?;
    if async_direct {
        record_user_io_async_direct_write(written, segments);
        record_user_io_async_resource_unpins(pinned.len());
    } else {
        user_direct_async_reject_if_enabled();
    }
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_pwritev_user_segments(
    file: &File,
    status: OfdIoStatus,
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
    let allowed = allowed_write_len(offset, iov.len())?;
    if allowed == 0 {
        return Ok(Some(0));
    }
    if allowed < USER_SLICE_FAST_MIN {
        return Ok(None);
    }
    memfd::check_write(file.inner().location(), offset, allowed)?;

    let Some(pinned) = try_pin_iov_from_user(iov, allowed)? else {
        record_user_io_direct_write_fallback();
        user_direct_async_reject_if_enabled();
        return Ok(None);
    };
    if pinned.is_empty() {
        return Ok(Some(0));
    }
    let segments = pinned.iter().map(|pin| pin.segments().len()).sum();
    let async_direct = user_direct_async_segments_candidate(offset, allowed, segments)
        && user_direct_async_segments_ok(pinned.iter().flat_map(|pin| {
            pin.segments()
                .iter()
                .map(|segment| (segment.paddr, segment.len))
        }));
    let written = with_pinned_user_segment_slices(&pinned, |segments| {
        if async_direct {
            run_user_direct_async_io(|| file.inner().write_at_vectored_slice(segments, offset))
        } else {
            write_at_vectored_slice_non_async(file, segments, offset)
        }
    })?;
    if async_direct {
        record_user_io_async_direct_write(written, segments);
        record_user_io_async_resource_unpins(pinned.len());
    } else {
        user_direct_async_reject_if_enabled();
    }
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
pub fn sys_read(fd: i32, buf: *mut u8, len: usize) -> AxResult<isize> {
    debug!("sys_read <= fd: {fd}, buf: {buf:p}, len: {len}");
    let f = get_file_like(fd)?;
    let status = f.io_status_snapshot();
    f.check_io_status(status)?;
    if len != 0 {
        crate::file::fanotify::permission_check_fd(fd, crate::file::fanotify::FAN_ACCESS_PERM)?;
    }
    f.with_read_credentials(|| {
        let regular_file = if let Some(file) = f.downcast_ref::<File>() {
            validate_direct_io(file, buf as usize, len, current_file_offset(file.inner())?)?;
            Some(file)
        } else {
            None
        };
        if let Some(file) = regular_file {
            let fast_read = match try_regular_file_read_user_slice(file, buf, len)? {
                Some(read) => Some(read),
                None => try_regular_file_read_user_segments(file, buf, len)?,
            };
            if let Some(read) = fast_read {
                let read = read as isize;
                if read > 0 {
                    notify_read(fd);
                }
                return Ok(read);
            }
            if len >= USER_COPY_PREFAULT_MIN {
                let offset = current_file_offset(file.inner())?;
                prefault_regular_file_read_fallback(file, buf, len, offset)?;
            }
        }
        let read = read_file_like_with_status(&f, status, &mut VmBytesMut::new(buf, len))? as isize;
        if read > 0 {
            notify_read(fd);
        }
        Ok(read)
    })
}

pub fn sys_readv(fd: i32, iov: *const IoVec, iovcnt: usize) -> AxResult<isize> {
    debug!("sys_readv <= fd: {fd}, iovcnt: {iovcnt}");
    let f = get_file_like(fd)?;
    let status = f.io_status_snapshot();
    f.check_io_status(status)?;
    if iovcnt != 0 {
        crate::file::fanotify::permission_check_fd(fd, crate::file::fanotify::FAN_ACCESS_PERM)?;
    }
    let iov = IoVectorBuf::new(iov, iovcnt)?;
    f.with_read_credentials(|| {
        let regular_file = if let Some(file) = f.downcast_ref::<File>() {
            validate_direct_iov(file, &iov, current_file_offset(file.inner())?)?;
            Some(file)
        } else {
            None
        };
        if let Some(file) = regular_file
            && let Some(read) = try_regular_file_readv_user_segments(file, &iov)?
        {
            let read = read as isize;
            if read > 0 {
                notify_read(fd);
            }
            return Ok(read);
        }
        let read = read_file_like_with_status(&f, status, &mut iov.into_io())? as isize;
        if read > 0 {
            notify_read(fd);
        }
        Ok(read)
    })
}

/// Write data to the file indicated by `fd`.
///
/// Return the written size if success.
pub fn sys_write(fd: i32, buf: *mut u8, len: usize) -> AxResult<isize> {
    debug!("sys_write <= fd: {fd}, buf: {buf:p}, len: {len}");
    let f = get_file_like(fd)?;
    let (written, status) = f.with_write_credentials(|status| {
        let regular_file = if let Some(file) = f.downcast_ref::<File>() {
            file.inner().access(FileFlags::WRITE)?;
            if len != 0 {
                check_writable_mount(file.inner().location())?;
            }
            let offset = current_write_offset(file.inner(), status)?;
            validate_direct_io(file, buf as usize, len, offset)?;
            if len != 0 {
                file.killpriv_for_content_mutation()?;
            }
            Some(file)
        } else {
            None
        };
        if let Some(file) = regular_file {
            if let Some(written) =
                try_regular_file_write_user_slice(file, status, buf as *const u8, len)?
            {
                return Ok((written, status));
            }
            if let Some(written) =
                try_regular_file_write_user_segments(file, status, buf as *const u8, len)?
            {
                return Ok((written, status));
            }
            if len >= USER_COPY_PREFAULT_MIN {
                let offset = current_write_offset(file.inner(), status)?;
                let allowed = allowed_write_len(offset, len)?;
                prefault_regular_file_write_fallback(file, buf as *const u8, allowed)?;
            }
        }
        write_file_like_with_status(&f, status, &mut VmBytes::new(buf, len))
            .map(|written| (written, status))
    })?;
    let written = written as isize;
    if written > 0 {
        sync_file_like_after_status_write(status, &f)?;
        notify_write(fd);
    }
    Ok(written)
}

pub fn sys_writev(fd: i32, iov: *const IoVec, iovcnt: usize) -> AxResult<isize> {
    debug!("sys_writev <= fd: {fd}, iovcnt: {iovcnt}");
    let iov = IoVectorBuf::new(iov, iovcnt)?;
    let written = if let Ok(file) = get_typed_file::<File>(fd) {
        let (written, status) = file.with_write_credentials(|status| {
            iov.check_readable()?;
            file.inner().access(FileFlags::WRITE)?;
            if iov.len() != 0 {
                check_writable_mount(file.inner().location())?;
            }
            let offset = current_write_offset(file.inner(), status)?;
            validate_direct_iov(file.as_ref(), &iov, offset)?;
            if iov.len() != 0 {
                file.killpriv_for_content_mutation()?;
            }
            if let Some(written) =
                try_regular_file_writev_user_segments(file.as_ref(), status, &iov)?
            {
                return Ok((written, status));
            }
            file.write_with_status(status, &mut iov.into_io())
                .map(|written| (written, status))
        })?;
        if written > 0 {
            sync_file_after_status_write(status, &file)?;
        }
        written
    } else {
        let f = get_file_like(fd)?;
        let (written, status) = f.with_write_credentials(|status| {
            write_file_like_with_status(&f, status, &mut iov.into_io())
                .map(|written| (written, status))
        })?;
        if written > 0 {
            sync_file_like_after_status_write(status, &f)?;
        }
        written
    };
    let written = written as isize;
    if written > 0 {
        notify_write(fd);
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

fn positioned_file(fd: c_int, access: FileFlags) -> AxResult<FileHandle<File>> {
    let file_like = get_file_like(fd)?;
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

fn positioned_write_file(fd: c_int) -> AxResult<FileHandle<File>> {
    let file = write_file(fd)?;
    check_positioned_write_flags(file.inner().location().flags())?;
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
        executable::check_not_active(file.inner().location())?;
    } else {
        file.inner().access(FileFlags::READ)?;
    }
    Ok(file)
}

fn current_file_offset(file: &axfs::File) -> AxResult<u64> {
    let mut file = file;
    file.seek(SeekFrom::Current(0))
}

fn current_write_offset(file: &axfs::File, status: OfdIoStatus) -> AxResult<u64> {
    let inode_append = status.append()
        && !file
            .location()
            .flags()
            .contains(NodeFlags::POSITIONED_APPEND);
    if inode_append {
        file.location().len()
    } else {
        current_file_offset(file)
    }
}

fn unpositioned_write_placement(file: &axfs::File, status: OfdIoStatus) -> WritePlacement {
    if status.append()
        && !file
            .location()
            .flags()
            .contains(NodeFlags::POSITIONED_APPEND)
    {
        WritePlacement::End
    } else {
        WritePlacement::Current
    }
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

fn checked_user_file_offset(ptr: *mut u64) -> AxResult<u64> {
    let value = ptr.vm_read()?;
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
        FileLikeKind::Regular | FileLikeKind::Directory | FileLikeKind::Other => Ok(file_like),
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
    let iov = IoVectorBuf::new(iov, iovcnt)?;
    if iov.len() != 0 {
        crate::file::fanotify::permission_check_fd(fd, crate::file::fanotify::FAN_ACCESS_PERM)?;
    }
    file.with_read_credentials(|| {
        let read = if offset == -1 {
            validate_direct_iov(file.as_ref(), &iov, current_file_offset(file.inner())?)?;
            if let Some(read) = try_regular_file_readv_user_segments(file.as_ref(), &iov)? {
                if read > 0 {
                    notify_read(fd);
                }
                return Ok(read as _);
            }
            let mut io = iov.into_io();
            file.read(&mut io)?
        } else {
            validate_direct_iov(file.as_ref(), &iov, offset as u64)?;
            if let Some(read) =
                try_regular_file_preadv_user_segments(file.as_ref(), &iov, offset as u64)?
            {
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

    // pwritev2(offset = -1) is an ordinary current-position write. Only an
    // explicit offset requires positioned-write support.
    let file = if offset == -1 {
        write_file(fd)?
    } else {
        positioned_write_file(fd)?
    };
    let io = IoVectorBuf::new(iov, iovcnt)?;
    if io.len() != 0 {
        file.killpriv_for_content_mutation()?;
    }
    let (written, status) = file.with_write_credentials(|status| {
        if offset == -1 {
            let write_offset = current_write_offset(file.inner(), status)?;
            validate_direct_iov(file.as_ref(), &io, write_offset)?;
            let allowed = allowed_write_len(write_offset, io.len())?;
            memfd::check_write(file.inner().location(), write_offset, allowed)?;
            if let Some(written) =
                try_regular_file_writev_user_segments(file.as_ref(), status, &io)?
            {
                return Ok((written, status));
            }
            let mut io = io.into_io();
            io.limit_remaining(allowed);
            file.write_with_status(status, &mut io)
        } else {
            if status.append() {
                let append_offset = file.inner().location().len()?;
                validate_direct_iov(file.as_ref(), &io, append_offset)?;
                let allowed = allowed_write_len(append_offset, io.len())?;
                memfd::check_write(file.inner().location(), append_offset, allowed)?;
                let mut io = io.into_io();
                io.limit_remaining(allowed);
                file.inner().write_at_end(io)
            } else {
                validate_direct_iov(file.as_ref(), &io, offset as u64)?;
                let allowed = allowed_write_len(offset as u64, io.len())?;
                memfd::check_write(file.inner().location(), offset as u64, allowed)?;
                if let Some(written) = try_regular_file_pwritev_user_segments(
                    file.as_ref(),
                    status,
                    &io,
                    offset as u64,
                )? {
                    return Ok((written, status));
                }
                let mut io = io.into_io();
                io.limit_remaining(allowed);
                file.inner().write_at(io, offset as u64)
            }
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

pub fn sys_truncate(path: UserConstPtr<c_char>, length: __kernel_off_t) -> AxResult<isize> {
    let path = path.get_as_str()?;
    debug!("sys_truncate <= {path:?} {length}");
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    if length < 0 {
        return Err(AxError::InvalidInput);
    }
    let curr = axtask::current();
    let proc_data = &curr.as_thread().proc_data;
    let credentials = curr.as_thread().fs_dac_credentials();
    let loc = FS_CONTEXT.lock().resolve_dac(path, &credentials)?;
    check_open_permissions(&loc, W_OK as u32, &credentials)?;
    check_writable_mount(&loc)?;
    check_resize_limit(length as u64)?;
    // Unlike fd-backed mutations, path truncate has no persistent open-file
    // description carrying the ETXTBSY reference. Hold a transient write
    // reservation across every check and publication after admission so exec
    // credential sampling cannot start in the old check-then-truncate gap.
    let write_open_key = executable::retain_write_open(&loc)?;
    let truncate: AxResult<()> = (|| {
        let _lease_admission = lease::admit_truncate(&loc)?;
        memfd::check_resize(&loc, length as u64)?;
        check_mandatory_truncate_lock(
            &loc,
            length as u64,
            flock::RecordLockOwner::Posix(proc_data.proc.pid()),
        )?;
        let file = OpenOptions::new()
            .write(true)
            .open_loc(loc.clone())?
            .into_file()?;
        File::set_len_with_killpriv(file.access(FileFlags::WRITE)?, length as _)?;
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
    if length < 0 {
        return Err(AxError::InvalidInput);
    }
    check_resize_limit(length as u64)?;
    let file_like = get_file_like(fd)?;
    let kind = FileLikeKind::from_file_like(file_like.as_ref());
    match kind {
        FileLikeKind::Fifo => return Err(AxError::from(LinuxError::ESPIPE)),
        FileLikeKind::Socket => return Err(AxError::InvalidInput),
        FileLikeKind::Directory => return Err(AxError::IsADirectory),
        FileLikeKind::Regular | FileLikeKind::Other => {}
    }
    let f = file_like.downcast::<File>()?;
    let backend = f
        .inner()
        .access(FileFlags::WRITE)
        .map_err(|err| match err {
            AxError::BadFileDescriptor => AxError::InvalidInput,
            other => other,
        })?;
    check_writable_mount(f.inner().location())?;
    executable::check_not_active(f.inner().location())?;
    let _lease_admission = lease::admit_truncate(f.inner().location())?;
    memfd::check_resize(f.inner().location(), length as u64)?;
    let status = f.io_status_snapshot();
    check_mandatory_fd_truncate_lock(
        f.inner().location(),
        length as u64,
        f.open_file_description_key(),
        status.nonblocking(),
    )?;
    File::set_len_with_killpriv(backend, length as _)?;
    if let Err(error) = touch_modified_metadata(f.inner().location()) {
        warn!("ftruncate metadata update failed after size mutation: {error}");
    }
    notify_write(fd);
    let _ = notify_exact(f.inner().location(), IN_ATTRIB);
    Ok(0)
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
    executable::check_not_active(&loc)?;
    let offset = offset as u64;
    let len = len as u64;
    let end = offset
        .checked_add(len)
        .ok_or_else(|| AxError::from(LinuxError::EFBIG))?;
    if end > MAX_FILE_OFFSET {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    let size = loc.len()?;
    let seals = memfd::current_seals(&loc).unwrap_or(0);
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
            memfd::check_resize(&loc, size.max(end))?;
            File::killpriv_before_file_mutation(&loc)?;
            if let Some(result) = tmp::reserve_fallocate_range(&loc, offset, len, true) {
                result?;
            } else {
                backend.set_len(size.max(end))?;
            }
        }
        FALLOC_FL_KEEP_SIZE => {
            File::killpriv_before_file_mutation(&loc)?;
            if let Some(result) = tmp::reserve_fallocate_range(&loc, offset, len, false) {
                result?;
            }
        }
        mode if mode == (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) => {
            if seals & linux_raw_sys::general::F_SEAL_WRITE != 0 {
                return Err(AxError::OperationNotPermitted);
            }
            if !tmp::supports_fallocate_range(&loc) {
                return Err(AxError::OperationNotSupported);
            }
            let hole_len = end.min(size).saturating_sub(offset);
            File::killpriv_before_file_mutation(&loc)?;
            write_zero_range(file, offset, hole_len)?;
            tmp::punch_hole_fallocate_range(&loc, offset, len).ok_or(AxError::BadState)??;
        }
        mode if mode == FALLOC_FL_ZERO_RANGE
            || mode == (FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE) =>
        {
            if seals & linux_raw_sys::general::F_SEAL_WRITE != 0 {
                return Err(AxError::OperationNotPermitted);
            }
            let zero_end = if mode & FALLOC_FL_KEEP_SIZE != 0 {
                end.min(size)
            } else {
                check_resize_limit(size.max(end))?;
                memfd::check_resize(&loc, size.max(end))?;
                end
            };
            let zero_len = zero_end.saturating_sub(offset);
            File::killpriv_before_file_mutation(&loc)?;
            if mode & FALLOC_FL_KEEP_SIZE == 0 {
                backend.set_len(size.max(end))?;
            }
            write_zero_range(file, offset, zero_len)?;
            if let Some(result) = tmp::reserve_fallocate_range(&loc, offset, zero_len, false) {
                result?;
            }
        }
        FALLOC_FL_COLLAPSE_RANGE => {
            if len == 0
                || offset % TMPFS_FALLOC_BLOCK_SIZE != 0
                || len % TMPFS_FALLOC_BLOCK_SIZE != 0
                || end > size
            {
                return Err(AxError::InvalidInput);
            }
            if seals & linux_raw_sys::general::F_SEAL_WRITE != 0 {
                return Err(AxError::OperationNotPermitted);
            }
            memfd::check_resize(&loc, size - len)?;
            File::killpriv_before_file_mutation(&loc)?;
            if let Some(result) = tmp::collapse_fallocate_range(&loc, offset, len) {
                result?;
            } else {
                copy_within_file(file, end, offset, size - end)?;
            }
            backend.set_len(size - len)?;
        }
        FALLOC_FL_INSERT_RANGE => {
            if len == 0
                || offset % TMPFS_FALLOC_BLOCK_SIZE != 0
                || len % TMPFS_FALLOC_BLOCK_SIZE != 0
                || offset >= size
            {
                return Err(AxError::InvalidInput);
            }
            if seals & linux_raw_sys::general::F_SEAL_WRITE != 0 {
                return Err(AxError::OperationNotPermitted);
            }
            let new_size = size
                .checked_add(len)
                .filter(|new_size| *new_size <= MAX_FILE_OFFSET)
                .ok_or_else(|| AxError::from(LinuxError::EFBIG))?;
            check_resize_limit(new_size)?;
            memfd::check_resize(&loc, new_size)?;
            File::killpriv_before_file_mutation(&loc)?;
            backend.set_len(new_size)?;
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
    let f = get_file_like(fd)?;
    f.check_io_access()?;
    match FileLikeKind::from_file_like(f.as_ref()) {
        FileLikeKind::Fifo | FileLikeKind::Socket => return Err(AxError::InvalidInput),
        FileLikeKind::Regular | FileLikeKind::Directory | FileLikeKind::Other => {}
    }
    let f = get_typed_file::<File>(fd)?;
    f.inner().sync(false)?;
    Ok(0)
}

pub fn sys_fdatasync(fd: c_int) -> AxResult<isize> {
    debug!("sys_fdatasync <= {fd}");
    let f = get_file_like(fd)?;
    f.check_io_access()?;
    match FileLikeKind::from_file_like(f.as_ref()) {
        FileLikeKind::Fifo | FileLikeKind::Socket => return Err(AxError::InvalidInput),
        FileLikeKind::Regular | FileLikeKind::Directory | FileLikeKind::Other => {}
    }
    let f = get_typed_file::<File>(fd)?;
    f.inner().sync(true)?;
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

    if offset < 0 || nbytes < 0 {
        return Err(AxError::InvalidInput);
    }
    let valid_flags =
        SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER;
    if flags & !valid_flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let file_like = get_file_like(fd)?;
    file_like.check_io_access()?;
    if !matches!(
        FileLikeKind::from_file_like(file_like.as_ref()),
        FileLikeKind::Regular
    ) {
        return Err(AxError::from(LinuxError::ESPIPE));
    }

    let f = get_typed_file::<File>(fd)?;
    f.inner().sync(true)?;
    Ok(0)
}

pub fn sys_fadvise64(
    fd: c_int,
    offset: __kernel_off_t,
    len: __kernel_off_t,
    advice: u32,
) -> AxResult<isize> {
    debug!("sys_fadvise64 <= fd: {fd}, offset: {offset}, len: {len}, advice: {advice}");
    if offset < 0 || len < 0 {
        return Err(AxError::InvalidInput);
    }
    if advice > 5 {
        return Err(AxError::InvalidInput);
    }
    let file_like = get_file_like(fd)?;
    file_like.check_io_access()?;
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Fifo | FileLikeKind::Socket => {
            return Err(AxError::from(LinuxError::ESPIPE));
        }
        FileLikeKind::Regular | FileLikeKind::Directory | FileLikeKind::Other => {}
    }
    if let Some(file) = file_like.downcast_ref::<File>() {
        file.inner().access(FileFlags::empty())?;
    }
    Ok(0)
}

pub fn sys_pread64(fd: c_int, buf: *mut u8, len: usize, offset: __kernel_off_t) -> AxResult<isize> {
    if offset < 0 {
        return Err(AxError::InvalidInput);
    }
    let f = positioned_file(fd, FileFlags::READ)?;
    validate_direct_io(f.as_ref(), buf as usize, len, offset as u64)?;
    if len != 0 {
        crate::file::fanotify::permission_check_fd(fd, crate::file::fanotify::FAN_ACCESS_PERM)?;
    }
    f.with_read_credentials(|| {
        let fast_read =
            match try_regular_file_pread_user_slice(f.as_ref(), buf, len, offset as u64)? {
                Some(read) => Some(read),
                None => try_regular_file_pread_user_segments(f.as_ref(), buf, len, offset as u64)?,
            };
        if let Some(read) = fast_read {
            if read > 0 {
                notify_read(fd);
            }
            return Ok(read as _);
        }
        if len >= USER_COPY_PREFAULT_MIN {
            prefault_regular_file_read_fallback(f.as_ref(), buf, len, offset as u64)?;
        }
        let read = f.inner().read_at(VmBytesMut::new(buf, len), offset as _)?;
        if read > 0 {
            notify_read(fd);
        }
        Ok(read as _)
    })
}

pub fn sys_pwrite64(
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
    f.killpriv_for_content_mutation()?;
    let (write, status) = f.with_write_credentials(|status| {
        let written = if status.append() {
            let append_offset = f.inner().location().len()?;
            validate_direct_io(f.as_ref(), buf as usize, len, append_offset)?;
            let allowed = allowed_write_len(append_offset, len)?;
            memfd::check_write(f.inner().location(), append_offset, allowed)?;
            if allowed >= USER_COPY_PREFAULT_MIN {
                prefault_regular_file_write_fallback(f.as_ref(), buf, allowed)?;
            }
            f.inner().write_at_end(VmBytes::new(buf, allowed))
        } else {
            validate_direct_io(f.as_ref(), buf as usize, len, offset as u64)?;
            let allowed = allowed_write_len(offset as u64, len)?;
            memfd::check_write(f.inner().location(), offset as u64, allowed)?;
            let fast_written = match try_regular_file_pwrite_user_slice(
                f.as_ref(),
                status,
                buf,
                len,
                offset as u64,
            )? {
                Some(written) => Some(written),
                None => try_regular_file_pwrite_user_segments(
                    f.as_ref(),
                    status,
                    buf,
                    len,
                    offset as u64,
                )?,
            };
            if let Some(written) = fast_written {
                return Ok((written, status));
            }
            if allowed >= USER_COPY_PREFAULT_MIN {
                prefault_regular_file_write_fallback(f.as_ref(), buf, allowed)?;
            }
            f.inner().write_at(VmBytes::new(buf, allowed), offset as _)
        }?;
        Ok((written, status))
    })?;
    if write > 0 {
        sync_file_after_status_write(status, &f)?;
        notify_write(fd);
    }
    Ok(write as _)
}

pub fn sys_preadv(
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset: __kernel_off_t,
) -> AxResult<isize> {
    do_preadv(fd, iov, iovcnt, offset, 0, false)
}

pub fn sys_pwritev(
    fd: c_int,
    iov: *const IoVec,
    iovcnt: usize,
    offset: __kernel_off_t,
) -> AxResult<isize> {
    do_pwritev(fd, iov, iovcnt, offset, 0, false)
}

pub fn sys_preadv2(
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
    do_preadv(fd, iov, iovcnt, offset, _flags, true)
}

pub fn sys_pwritev2(
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
    do_pwritev(fd, iov, iovcnt, offset, _flags, true)
}

enum SendFile {
    Direct {
        file: FileHandle<dyn FileLike>,
        status: OfdIoStatus,
        nonblocking: bool,
    },
    Offset {
        file: FileHandle<File>,
        offset: u64,
        user_offset: *mut u64,
        status: OfdIoStatus,
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
            Self::Direct { file, status, .. } => Ok(Self::Offset {
                file: file.downcast::<File>()?,
                offset,
                // This cursor is committed by axfs's outer operation
                // transaction, never through userspace copyout.
                user_offset: core::ptr::null_mut(),
                status: *status,
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
            } => file.with_write_credentials_for_status(*status, || {
                let nonblocking = *nonblocking || force_nonblocking;
                if let Some(pipe) = file.downcast_ref::<Pipe>() {
                    pipe.write_with_nonblocking(&mut buf, nonblocking)
                } else if let Some(pipe) = file.downcast_ref::<NamedPipe>() {
                    pipe.write_with_nonblocking(&mut buf, nonblocking)
                } else if let Some(socket) = file.downcast_ref::<Socket>() {
                    socket.write_with_nonblocking(&mut buf, nonblocking)
                } else if let Some(regular) = file.downcast_ref::<File>() {
                    let ofd_key = file.open_file_description_key();
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
                            executable::check_not_active(regular.inner().location())?;
                            let allowed = allowed_write_len(off, data.len())?;
                            if allowed == 0 {
                                return Ok(0);
                            }
                            memfd::check_write(regular.inner().location(), off, allowed)?;
                            regular.killpriv_for_content_mutation()?;
                            regular.inner().write_at(&data[..allowed], off)
                        })
                } else {
                    write_file_like_with_status(file, *status, &mut buf)
                }
            }),
            SendFile::Offset {
                file,
                offset,
                user_offset,
                status,
            } => {
                let off = *offset;
                check_writable_mount(file.inner().location())?;
                executable::check_not_active(file.inner().location())?;
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
                memfd::check_write(file.inner().location(), off, allowed)?;
                file.killpriv_for_content_mutation()?;
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
            ..
        } = self
            && !user_offset.is_null()
        {
            user_offset.vm_write(*offset)?;
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

pub fn sys_sendfile(out_fd: c_int, in_fd: c_int, offset: *mut u64, len: usize) -> AxResult<isize> {
    debug!(
        "sys_sendfile <= out_fd: {}, in_fd: {}, offset: {}, len: {}",
        out_fd,
        in_fd,
        !offset.is_null(),
        len
    );

    // Linux copies an explicit offset before fd admission and keeps one local
    // value for the complete operation. Concurrent userspace stores cannot
    // redirect later chunks.
    let explicit_offset = if offset.is_null() {
        None
    } else {
        Some(offset.vm_read()?)
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
                }
            } else {
                SendFile::Direct {
                    status: src_status,
                    file: src_file.clone().into_file_like(),
                    nonblocking: src_status.nonblocking(),
                }
            };

            let mut destination = SendFile::Direct {
                file: dst.clone(),
                status,
                nonblocking: status.nonblocking(),
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
        offset.vm_write(committed_offset)?;
    }
    result
}

pub fn sys_copy_file_range(
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
        Some(checked_user_file_offset(off_in)?)
    };
    let dst_offset = if off_out.is_null() {
        None
    } else {
        Some(checked_user_file_offset(off_out)?)
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
        }
    } else {
        SendFile::Direct {
            file: src_file.clone().into_file_like(),
            status: src_status,
            nonblocking: src_status.nonblocking(),
        }
    };

    let mut dst = if let Some(dst_offset) = dst_offset {
        SendFile::Offset {
            file: dst_file.clone(),
            offset: dst_offset,
            user_offset: off_out,
            status: dst_status,
        }
    } else {
        SendFile::Direct {
            file: dst_file.clone().into_file_like(),
            status: dst_status,
            nonblocking: dst_status.nonblocking(),
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
        src_commit?;
        dst_commit?;
    }
    Ok(copied as _)
}

pub fn sys_splice(
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
        Some(off_out.vm_read()?)
    };
    let input_offset = if off_in.is_null() {
        None
    } else {
        Some(off_in.vm_read()?)
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
        }
    } else {
        SendFile::Direct {
            file: dst_handle.clone(),
            status: dst_status,
            nonblocking: destination_nonblocking,
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

pub fn sys_vmsplice(fd: c_int, iov: *const IoVec, nr_segs: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_vmsplice <= fd: {fd}, iov: {iov:p}, nr_segs: {nr_segs}, flags: {flags:#x}");

    validate_splice_flags(flags)?;

    let pipe = pipe_from_fd(fd, AxError::BadFileDescriptor)?;
    let mut io = IoVectorBuf::new(iov, nr_segs)?.into_io();
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
    use alloc::sync::{Arc, Weak};
    use core::{
        any::Any,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::Context,
        time::Duration,
    };

    use axfs_ng_vfs::{
        DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps, Metadata, MetadataUpdate,
        Mountpoint, NodeOps, NodePermission, NodeType, Reference, StatFs, VfsError, VfsResult,
        XattrProvider, XattrSetMode,
    };

    use super::*;

    struct IoContractFs {
        this: Weak<Self>,
        flags: NodeFlags,
        size: u64,
        fail_open: AtomicBool,
        fail_remove_xattr: AtomicBool,
        open_calls: AtomicUsize,
        remove_xattr_calls: AtomicUsize,
        set_len_calls: AtomicUsize,
    }

    impl IoContractFs {
        fn new(flags: NodeFlags, size: u64) -> Arc<Self> {
            Arc::new_cyclic(|this| Self {
                this: this.clone(),
                flags,
                size,
                fail_open: AtomicBool::new(false),
                fail_remove_xattr: AtomicBool::new(false),
                open_calls: AtomicUsize::new(0),
                remove_xattr_calls: AtomicUsize::new(0),
                set_len_calls: AtomicUsize::new(0),
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
                NodeType::RegularFile,
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
                node_type: NodeType::RegularFile,
                uid: 0,
                gid: 0,
                size: self.fs.size,
                block_size: 4096,
                blocks: 0,
                rdev: Default::default(),
                atime: Duration::ZERO,
                btime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
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
            let _ = offset;
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
    fn explicit_write_marker_maps_to_espipe_only() {
        assert!(check_positioned_write_flags(NodeFlags::empty()).is_ok());
        assert_eq!(
            check_positioned_write_flags(NodeFlags::NO_POSITIONED_WRITE),
            Err(AxError::from(LinuxError::ESPIPE))
        );
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
    fn killpriv_failure_rejects_before_truncate_side_effects() {
        let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 17);
        fs.fail_remove_xattr.store(true, Ordering::Release);
        let mut options = OpenOptions::new();
        options.write(true);
        let file = options
            .open_loc(fs.location())
            .unwrap()
            .into_file()
            .unwrap();

        assert_eq!(
            File::set_len_with_killpriv(file.backend().unwrap(), 0),
            Err(AxError::Io)
        );
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
}
