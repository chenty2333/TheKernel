use alloc::{vec, vec::Vec};
use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FileFlags, OpenOptions};
use axfs_ng_vfs::{Location, MetadataUpdate, NodeFlags};
use axio::{Seek, SeekFrom};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::{__kernel_off_t, IN_ATTRIB, IN_MODIFY, O_DSYNC, O_SYNC, W_OK};
use starry_vm::{VmMutPtr, VmPtr};
use syscalls::Sysno;

use crate::{
    file::{
        Directory, File, FileHandle, FileLike, FileLikeKind, PidFd, Pipe, Socket,
        allowed_write_len, check_resize_limit, executable, flock, get_file_like, get_typed_file,
        inode_flags,
        inotify::{notify_exact, notify_read, notify_write},
        lease, memfd,
        permission::{DacFsContextExt, check_open_permissions, check_writable_mount},
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
// Regular-file O_DIRECT is constrained by logical sector alignment. LTP
// exercises 512-byte offset and 1 KiB transfer cases.
const DIRECT_IO_ALIGNMENT: usize = 512;
const USER_SLICE_FAST_MIN: usize = 4096;
const USER_IOV_FAST_MAX_SEGMENTS: usize = 64;
const USER_COPY_PREFAULT_MIN: usize = 16 * 1024;
const USER_DIRECT_ASYNC_ALIGNMENT: usize = 4096;

fn validate_splice_flags(flags: u32) -> AxResult<()> {
    if flags & !SPLICE_F_ALL != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
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

fn sync_regular_file_after_status_write(status_flags: u32, file: &File) -> AxResult<()> {
    if status_flags & O_SYNC != 0 {
        file.inner().sync(false)?;
    } else if status_flags & O_DSYNC != 0 {
        file.inner().sync(true)?;
    }
    Ok(())
}

fn sync_file_after_status_write(file: &FileHandle<File>) -> AxResult<()> {
    sync_regular_file_after_status_write(file.status_flags(), file.as_ref())
}

fn sync_file_like_after_status_write(file_like: &FileHandle<dyn FileLike>) -> AxResult<()> {
    if let Some(file) = file_like.downcast_ref::<File>() {
        sync_regular_file_after_status_write(file_like.status_flags(), file)?;
    }
    Ok(())
}

fn sync_fd_after_status_write(fd: c_int) -> AxResult<()> {
    sync_file_like_after_status_write(&get_file_like(fd)?)
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

fn write_vectored_slice_sync(file: &File, bufs: &[&[u8]]) -> AxResult<usize> {
    let mut total = 0usize;
    for buf in bufs.iter().copied() {
        if buf.is_empty() {
            continue;
        }
        let requested = buf.len();
        let written = file.inner().write_slice(buf)?;
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

fn write_vectored_slice_non_async(file: &File, bufs: &[&[u8]]) -> AxResult<usize> {
    if axdriver::virtio_async_block_enabled() {
        write_vectored_slice_sync(file, bufs)
    } else {
        Ok(file.inner().write_vectored_slice(bufs)?)
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
    buf: *const u8,
    len: usize,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if file.inner().flags().contains(FileFlags::APPEND) {
        return Ok(None);
    }

    let offset = current_file_offset(file.inner())?;
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
        let written = run_user_direct_async_io(|| file.inner().write_vectored_slice(&bufs))?;
        record_user_io_async_direct_write(written, segments);
        record_user_io_async_resource_unpins(1);
        written
    } else {
        user_direct_async_reject_if_enabled();
        file.inner().write_slice(pinned.as_slice())?
    };
    record_user_io_direct_write(written, segments);
    Ok(Some(written))
}

fn try_regular_file_write_user_segments(
    file: &File,
    buf: *const u8,
    len: usize,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if file.inner().flags().contains(FileFlags::APPEND) {
        return Ok(None);
    }

    let offset = current_file_offset(file.inner())?;
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
            run_user_direct_async_io(|| file.inner().write_vectored_slice(segments))
        } else {
            write_vectored_slice_non_async(file, segments)
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
    buf: *const u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if file.inner().flags().contains(FileFlags::APPEND) {
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
    buf: *const u8,
    len: usize,
    offset: u64,
) -> AxResult<Option<usize>> {
    if len < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if file.inner().flags().contains(FileFlags::APPEND) {
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
    iov: &IoVectorBuf,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if file.inner().flags().contains(FileFlags::APPEND) {
        return Ok(None);
    }

    let offset = current_file_offset(file.inner())?;
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
            run_user_direct_async_io(|| file.inner().write_vectored_slice(segments))
        } else {
            write_vectored_slice_non_async(file, segments)
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
    iov: &IoVectorBuf,
    offset: u64,
) -> AxResult<Option<usize>> {
    if iov.len() < USER_SLICE_FAST_MIN || !regular_file_supports_user_slice_fast_path(file) {
        return Ok(None);
    }
    if file.inner().flags().contains(FileFlags::APPEND) {
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
    if len != 0 {
        crate::file::fanotify::permission_check_fd(fd, crate::file::fanotify::FAN_ACCESS_PERM)?;
    }
    let f = get_file_like(fd)?;
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
    let read = f.read(&mut VmBytesMut::new(buf, len))? as isize;
    if read > 0 {
        notify_read(fd);
    }
    Ok(read)
}

pub fn sys_readv(fd: i32, iov: *const IoVec, iovcnt: usize) -> AxResult<isize> {
    debug!("sys_readv <= fd: {fd}, iovcnt: {iovcnt}");
    if iovcnt != 0 {
        crate::file::fanotify::permission_check_fd(fd, crate::file::fanotify::FAN_ACCESS_PERM)?;
    }
    let f = get_file_like(fd)?;
    let iov = IoVectorBuf::new(iov, iovcnt)?;
    let regular_file = if let Some(file) = f.downcast_ref::<File>() {
        validate_direct_iov(file, &iov, current_file_offset(file.inner())?)?;
        Some(file)
    } else {
        None
    };
    if let Some(file) = regular_file {
        if let Some(read) = try_regular_file_readv_user_segments(file, &iov)? {
            let read = read as isize;
            if read > 0 {
                notify_read(fd);
            }
            return Ok(read);
        }
    }
    let read = f.read(&mut iov.into_io())? as isize;
    if read > 0 {
        notify_read(fd);
    }
    Ok(read)
}

/// Write data to the file indicated by `fd`.
///
/// Return the written size if success.
pub fn sys_write(fd: i32, buf: *mut u8, len: usize) -> AxResult<isize> {
    debug!("sys_write <= fd: {fd}, buf: {buf:p}, len: {len}");
    let f = get_file_like(fd)?;
    let regular_file = if let Some(file) = f.downcast_ref::<File>() {
        file.inner().access(FileFlags::WRITE)?;
        if len != 0 {
            check_writable_mount(file.inner().location())?;
        }
        let offset = if file.inner().flags().contains(FileFlags::APPEND) {
            file.inner().location().len()?
        } else {
            current_file_offset(file.inner())?
        };
        validate_direct_io(file, buf as usize, len, offset)?;
        Some(file)
    } else {
        None
    };
    if let Some(file) = regular_file {
        if let Some(written) = f.with_write_credentials(|| {
            if let Some(written) = try_regular_file_write_user_slice(file, buf as *const u8, len)? {
                return Ok(Some(written));
            }
            try_regular_file_write_user_segments(file, buf as *const u8, len)
        })? {
            let written = written as isize;
            if written > 0 {
                sync_file_like_after_status_write(&f)?;
                notify_write(fd);
            }
            return Ok(written);
        }
    }
    let written = f.with_write_credentials(|| {
        if let Some(file) = regular_file.filter(|_| len >= USER_COPY_PREFAULT_MIN) {
            let offset = if file.inner().flags().contains(FileFlags::APPEND) {
                file.inner().location().len()?
            } else {
                current_file_offset(file.inner())?
            };
            let allowed = allowed_write_len(offset, len)?;
            prefault_regular_file_write_fallback(file, buf as *const u8, allowed)?;
        }
        f.write(&mut VmBytes::new(buf, len))
    })? as isize;
    if written > 0 {
        sync_file_like_after_status_write(&f)?;
        notify_write(fd);
    }
    Ok(written)
}

pub fn sys_writev(fd: i32, iov: *const IoVec, iovcnt: usize) -> AxResult<isize> {
    debug!("sys_writev <= fd: {fd}, iovcnt: {iovcnt}");
    let iov = IoVectorBuf::new(iov, iovcnt)?;
    let written = if let Ok(file) = get_typed_file::<File>(fd) {
        iov.check_readable()?;
        file.inner().access(FileFlags::WRITE)?;
        if iov.len() != 0 {
            check_writable_mount(file.inner().location())?;
        }
        let offset = if file.inner().flags().contains(FileFlags::APPEND) {
            file.inner().location().len()?
        } else {
            current_file_offset(file.inner())?
        };
        validate_direct_iov(file.as_ref(), &iov, offset)?;
        let written = file.with_write_credentials(|| {
            if let Some(written) = try_regular_file_writev_user_segments(file.as_ref(), &iov)? {
                return Ok(written);
            }
            file.write(&mut iov.into_io())
        })?;
        if written > 0 {
            sync_file_after_status_write(&file)?;
        }
        written
    } else {
        let f = get_file_like(fd)?;
        let written = f.with_write_credentials(|| f.write(&mut iov.into_io()))?;
        if written > 0 {
            sync_file_like_after_status_write(&f)?;
        }
        written
    } as isize;
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

    let file = get_typed_file::<File>(fd)?;
    file.inner().access(access)?;
    Ok(file)
}

fn positioned_write_file(fd: c_int) -> AxResult<FileHandle<File>> {
    let file = positioned_file(fd, FileFlags::WRITE)?;
    check_writable_mount(file.inner().location())?;
    executable::check_not_active(file.inner().location())?;
    Ok(file)
}

fn regular_copy_file(fd: c_int, write: bool) -> AxResult<FileHandle<File>> {
    let file_like = get_file_like(fd)?;
    match FileLikeKind::from_file_like(file_like.as_ref()) {
        FileLikeKind::Regular => {}
        FileLikeKind::Directory => return Err(AxError::IsADirectory),
        FileLikeKind::Fifo | FileLikeKind::Socket | FileLikeKind::Other => {
            return Err(AxError::InvalidInput);
        }
    }

    let file = get_typed_file::<File>(fd)?;
    if write {
        if file.inner().flags().contains(FileFlags::APPEND) {
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

fn has_mandatory_lock_mode(loc: &axfs_ng_vfs::Location) -> AxResult<bool> {
    let metadata = loc.metadata()?;
    let mode = metadata.mode.bits();
    Ok(metadata.node_type == axfs_ng_vfs::NodeType::RegularFile
        && mode & 0o2000 != 0
        && mode & 0o010 == 0)
}

fn check_mandatory_truncate_lock(
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

fn checked_user_file_offset(ptr: *mut u64) -> AxResult<u64> {
    let value = ptr.vm_read()?;
    if value > MAX_FILE_OFFSET {
        return Err(AxError::InvalidInput);
    }
    Ok(value)
}

fn copy_file_range_offset(file: &File, ptr: *mut u64) -> AxResult<u64> {
    if ptr.is_null() {
        current_file_offset(file.inner())
    } else {
        checked_user_file_offset(ptr)
    }
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

fn seekable_fd(fd: c_int) -> AxResult<FileHandle<dyn FileLike>> {
    let file_like = get_file_like(fd)?;
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

    let file = positioned_write_file(fd)?;
    let io = IoVectorBuf::new(iov, iovcnt)?;
    let written = file.with_write_credentials(|| {
        if offset == -1 {
            let appending = file.inner().flags().contains(FileFlags::APPEND);
            let write_offset = if appending {
                file.inner().location().len()?
            } else {
                current_file_offset(file.inner())?
            };
            validate_direct_iov(file.as_ref(), &io, write_offset)?;
            let allowed = allowed_write_len(write_offset, io.len())?;
            memfd::check_write(file.inner().location(), write_offset, allowed)?;
            if !appending {
                if let Some(written) = try_regular_file_writev_user_segments(file.as_ref(), &io)? {
                    return Ok(written);
                }
            }
            let mut io = io.into_io();
            io.limit_remaining(allowed);
            file.write(&mut io)
        } else {
            if file.inner().flags().contains(FileFlags::APPEND) {
                let append_offset = file.inner().location().len()?;
                validate_direct_iov(file.as_ref(), &io, append_offset)?;
                let allowed = allowed_write_len(append_offset, io.len())?;
                memfd::check_write(file.inner().location(), append_offset, allowed)?;
                let mut io = io.into_io();
                io.limit_remaining(allowed);
                file.inner()
                    .access(FileFlags::APPEND)?
                    .append(io)
                    .map(|it| it.0)
            } else {
                validate_direct_iov(file.as_ref(), &io, offset as u64)?;
                let allowed = allowed_write_len(offset as u64, io.len())?;
                memfd::check_write(file.inner().location(), offset as u64, allowed)?;
                if let Some(written) =
                    try_regular_file_pwritev_user_segments(file.as_ref(), &io, offset as u64)?
                {
                    return Ok(written);
                }
                let mut io = io.into_io();
                io.limit_remaining(allowed);
                file.inner().write_at(io, offset as u64)
            }
        }
    })? as isize;
    if written > 0 {
        sync_file_after_status_write(&file)?;
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
    let credentials = proc_data.fs_dac_credentials();
    let loc = FS_CONTEXT.lock().resolve_dac(path, &credentials)?;
    check_open_permissions(&loc, W_OK as u32, &credentials)?;
    check_writable_mount(&loc)?;
    check_resize_limit(length as u64)?;
    executable::check_not_active(&loc)?;
    lease::wait_for_truncate(&loc)?;
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
    file.access(FileFlags::WRITE)?.set_len(length as _)?;
    touch_modified_metadata(&loc)?;
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
    let f = get_typed_file::<File>(fd)?;
    let backend = f
        .inner()
        .access(FileFlags::WRITE)
        .map_err(|err| match err {
            AxError::BadFileDescriptor => AxError::InvalidInput,
            other => other,
        })?;
    check_writable_mount(f.inner().location())?;
    executable::check_not_active(f.inner().location())?;
    lease::wait_for_truncate(f.inner().location())?;
    memfd::check_resize(f.inner().location(), length as u64)?;
    check_mandatory_truncate_lock(
        f.inner().location(),
        length as u64,
        flock::RecordLockOwner::Posix(axtask::current().as_thread().proc_data.proc.pid()),
    )?;
    backend.set_len(length as _)?;
    touch_modified_metadata(f.inner().location())?;
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

    let f = get_typed_file::<File>(fd)?;
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
            if let Some(result) = tmp::reserve_fallocate_range(&loc, offset, len, true) {
                result?;
            } else {
                backend.set_len(size.max(end))?;
            }
        }
        FALLOC_FL_KEEP_SIZE => {
            if let Some(result) = tmp::reserve_fallocate_range(&loc, offset, len, false) {
                result?;
            }
        }
        mode if mode == (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) => {
            if seals & linux_raw_sys::general::F_SEAL_WRITE != 0 {
                return Err(AxError::OperationNotPermitted);
            }
            let hole_len = end.min(size).saturating_sub(offset);
            write_zero_range(file, offset, hole_len)?;
            if let Some(result) = tmp::punch_hole_fallocate_range(&loc, offset, len) {
                result?;
            } else {
                return Err(AxError::OperationNotSupported);
            }
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
                backend.set_len(size.max(end))?;
                end
            };
            let zero_len = zero_end.saturating_sub(offset);
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

    touch_modified_metadata(&loc)?;
    let _ = notify_exact(&loc, IN_MODIFY | IN_ATTRIB);
    Ok(0)
}

pub fn sys_fsync(fd: c_int) -> AxResult<isize> {
    debug!("sys_fsync <= {fd}");
    let f = get_file_like(fd)?;
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
    let fast_read = match try_regular_file_pread_user_slice(f.as_ref(), buf, len, offset as u64)? {
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
    if len == 0 {
        return Ok(0);
    }
    let f = positioned_write_file(fd)?;
    let write = f.with_write_credentials(|| {
        if f.inner().flags().contains(FileFlags::APPEND) {
            let append_offset = f.inner().location().len()?;
            validate_direct_io(f.as_ref(), buf as usize, len, append_offset)?;
            let allowed = allowed_write_len(append_offset, len)?;
            memfd::check_write(f.inner().location(), append_offset, allowed)?;
            if allowed >= USER_COPY_PREFAULT_MIN {
                prefault_regular_file_write_fallback(f.as_ref(), buf, allowed)?;
            }
            f.inner()
                .access(FileFlags::APPEND)?
                .append(VmBytes::new(buf, allowed))
                .map(|it| it.0)
        } else {
            validate_direct_io(f.as_ref(), buf as usize, len, offset as u64)?;
            let allowed = allowed_write_len(offset as u64, len)?;
            memfd::check_write(f.inner().location(), offset as u64, allowed)?;
            let fast_written =
                match try_regular_file_pwrite_user_slice(f.as_ref(), buf, len, offset as u64)? {
                    Some(written) => Some(written),
                    None => {
                        try_regular_file_pwrite_user_segments(f.as_ref(), buf, len, offset as u64)?
                    }
                };
            if let Some(written) = fast_written {
                return Ok(written);
            }
            if allowed >= USER_COPY_PREFAULT_MIN {
                prefault_regular_file_write_fallback(f.as_ref(), buf, allowed)?;
            }
            f.inner().write_at(VmBytes::new(buf, allowed), offset as _)
        }
    })?;
    if write > 0 {
        sync_file_after_status_write(&f)?;
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
    Direct(FileHandle<dyn FileLike>),
    Offset(FileHandle<File>, *mut u64),
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

impl SendFile {
    fn has_data(&self) -> bool {
        match self {
            SendFile::Direct(file) => file.poll(),
            SendFile::Offset(file, ..) => file.poll(),
        }
        .contains(IoEvents::IN)
    }

    fn read(&mut self, mut buf: &mut [u8]) -> AxResult<usize> {
        match self {
            SendFile::Direct(file) => file.read(&mut buf),
            SendFile::Offset(file, offset) => {
                let off = offset.vm_read()?;
                let bytes_read = file.inner().read_at(&mut buf, off)?;
                offset.vm_write(checked_offset_advance(off, bytes_read)?)?;
                Ok(bytes_read)
            }
        }
    }

    fn write(&mut self, mut buf: &[u8]) -> AxResult<usize> {
        match self {
            SendFile::Direct(file) => file.with_write_credentials(|| file.write(&mut buf)),
            SendFile::Offset(file, offset) => {
                let off = offset.vm_read()?;
                check_writable_mount(file.inner().location())?;
                executable::check_not_active(file.inner().location())?;
                let allowed = allowed_write_len(off, buf.len())?;
                if allowed == 0 {
                    return Ok(0);
                }
                memfd::check_write(file.inner().location(), off, allowed)?;
                let bytes_written =
                    file.with_write_credentials(|| file.inner().write_at(&buf[..allowed], off))?;
                offset.vm_write(checked_offset_advance(off, bytes_written)?)?;
                Ok(bytes_written)
            }
        }
    }
}

fn do_send(mut src: SendFile, mut dst: SendFile, len: usize) -> AxResult<usize> {
    let mut buf = vec![0; 0x1000];
    let mut total_written = 0;
    let mut remaining = len;

    while remaining > 0 {
        if total_written > 0 && !src.has_data() {
            break;
        }
        let to_read = buf.len().min(remaining);
        let bytes_read = match src.read(&mut buf[..to_read]) {
            Ok(n) => n,
            Err(AxError::WouldBlock) if total_written > 0 => break,
            Err(e) => return Err(e),
        };
        if bytes_read == 0 {
            break;
        }

        let bytes_written = dst.write(&buf[..bytes_read])?;
        if bytes_written < bytes_read {
            break;
        }

        total_written += bytes_written;
        remaining -= bytes_written;
    }

    Ok(total_written)
}

fn validate_sendfile_source(fd: c_int) -> AxResult<()> {
    let file = positioned_file(fd, FileFlags::READ)?;
    file.inner().access(FileFlags::READ)?;
    Ok(())
}

fn validate_sendfile_destination(fd: c_int) -> AxResult<()> {
    let file_like = get_file_like(fd)?;
    if let Some(file) = file_like.downcast_ref::<File>() {
        file.inner().access(FileFlags::WRITE)?;
        check_writable_mount(file.inner().location())?;
        executable::check_not_active(file.inner().location())?;
    } else if matches!(
        FileLikeKind::from_file_like(file_like.as_ref()),
        FileLikeKind::Directory
    ) {
        return Err(AxError::IsADirectory);
    }
    Ok(())
}

fn validate_splice_endpoint(fd: c_int, input: bool) -> AxResult<()> {
    let file_like = get_file_like(fd)?;

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

    if let Some(socket) = file_like.downcast_ref::<Socket>() {
        if matches!(&socket.inner, axnet::Socket::Unix(unix) if !unix.is_connected()) {
            return Err(AxError::InvalidInput);
        }
        return Ok(());
    }

    if let Some(file) = file_like.downcast_ref::<File>() {
        if input {
            file.inner().access(FileFlags::READ)?;
        } else {
            if file.inner().access(FileFlags::APPEND).is_ok() {
                return Err(AxError::InvalidInput);
            }
            file.inner().access(FileFlags::WRITE)?;
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
    if file.downcast_ref::<Pipe>().is_none() {
        return Err(non_pipe_error);
    }
    get_typed_file(fd)
}

pub fn sys_sendfile(out_fd: c_int, in_fd: c_int, offset: *mut u64, len: usize) -> AxResult<isize> {
    debug!(
        "sys_sendfile <= out_fd: {}, in_fd: {}, offset: {}, len: {}",
        out_fd,
        in_fd,
        !offset.is_null(),
        len
    );

    validate_sendfile_source(in_fd)?;
    validate_sendfile_destination(out_fd)?;

    let src = if !offset.is_null() {
        if offset.vm_read()? > MAX_FILE_OFFSET {
            return Err(AxError::InvalidInput);
        }
        SendFile::Offset(File::from_fd(in_fd)?, offset)
    } else {
        SendFile::Direct(get_file_like(in_fd)?)
    };

    let dst = SendFile::Direct(get_file_like(out_fd)?);

    let sent = do_send(src, dst, len)?;
    if sent > 0 {
        sync_fd_after_status_write(out_fd)?;
    }
    Ok(sent as _)
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

    if _flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if len as u64 > MAX_FILE_OFFSET {
        return Err(AxError::from(LinuxError::EOVERFLOW));
    }

    let src_file = regular_copy_file(fd_in, false)?;
    let dst_file = regular_copy_file(fd_out, true)?;
    let len64 = len as u64;
    let src_offset = copy_file_range_offset(&src_file, off_in)?;
    let dst_offset = copy_file_range_offset(&dst_file, off_out)?;

    if src_offset
        .checked_add(len64)
        .is_none_or(|end| end > MAX_FILE_OFFSET)
    {
        return Err(AxError::from(LinuxError::EOVERFLOW));
    }
    if dst_offset
        .checked_add(len64)
        .is_none_or(|end| end > MAX_FILE_OFFSET)
    {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    if inode_flags::same_inode(src_file.inner().location(), dst_file.inner().location())
        && ranges_overlap(src_offset, dst_offset, len64)?
    {
        return Err(AxError::InvalidInput);
    }

    let src = if !off_in.is_null() {
        SendFile::Offset(src_file, off_in)
    } else {
        SendFile::Direct(get_file_like(fd_in)?)
    };

    let dst = if !off_out.is_null() {
        SendFile::Offset(dst_file.clone(), off_out)
    } else {
        SendFile::Direct(get_file_like(fd_out)?)
    };

    let copied = do_send(src, dst, len)?;
    if copied > 0 {
        touch_modified_metadata(dst_file.inner().location())?;
        sync_file_after_status_write(&dst_file)?;
        notify_read(fd_in);
        notify_write(fd_out);
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

    let mut has_pipe = false;

    validate_splice_endpoint(fd_in, true)?;
    validate_splice_endpoint(fd_out, false)?;

    let src = if !off_in.is_null() {
        if let Ok(pipe) = Pipe::from_fd(fd_in) {
            if !pipe.is_read() {
                return Err(AxError::BadFileDescriptor);
            }
            return Err(AxError::from(LinuxError::ESPIPE));
        }
        if off_in.vm_read()? < 0 {
            return Err(AxError::InvalidInput);
        }
        SendFile::Offset(File::from_fd(fd_in)?, off_in.cast())
    } else {
        if let Ok(src) = Pipe::from_fd(fd_in) {
            if !src.is_read() {
                return Err(AxError::BadFileDescriptor);
            }
            has_pipe = true;
        }
        if let Ok(file) = File::from_fd(fd_in)
            && file.inner().is_path()
        {
            return Err(AxError::InvalidInput);
        }
        SendFile::Direct(get_file_like(fd_in)?)
    };

    let dst = if !off_out.is_null() {
        if let Ok(pipe) = Pipe::from_fd(fd_out) {
            if !pipe.is_write() {
                return Err(AxError::BadFileDescriptor);
            }
            return Err(AxError::from(LinuxError::ESPIPE));
        }
        if off_out.vm_read()? < 0 {
            return Err(AxError::InvalidInput);
        }
        SendFile::Offset(File::from_fd(fd_out)?, off_out.cast())
    } else {
        if let Ok(dst) = Pipe::from_fd(fd_out) {
            if !dst.is_write() {
                return Err(AxError::BadFileDescriptor);
            }
            has_pipe = true;
        }
        if let Ok(file) = File::from_fd(fd_out)
            && file.inner().access(FileFlags::APPEND).is_ok()
        {
            return Err(AxError::InvalidInput);
        }
        SendFile::Direct(get_file_like(fd_out)?)
    };

    if !has_pipe {
        return Err(AxError::InvalidInput);
    }

    let spliced = do_send(src, dst, len)?;
    if spliced > 0 {
        sync_fd_after_status_write(fd_out)?;
    }
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
    src.tee_to(&dst, len, flags & SPLICE_F_NONBLOCK != 0)
        .map(|n| n as _)
}

pub fn sys_vmsplice(fd: c_int, iov: *const IoVec, nr_segs: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_vmsplice <= fd: {fd}, iov: {iov:p}, nr_segs: {nr_segs}, flags: {flags:#x}");

    validate_splice_flags(flags)?;

    let pipe = pipe_from_fd(fd, AxError::BadFileDescriptor)?;
    let mut io = IoVectorBuf::new(iov, nr_segs)?.into_io();
    let nonblocking = flags & SPLICE_F_NONBLOCK != 0 || pipe.nonblocking();

    let result = if pipe.is_write() {
        pipe.vmsplice_write(&mut io, nonblocking)
    } else {
        pipe.vmsplice_read(&mut io, nonblocking)
    };

    result.map(|n| n as _)
}
