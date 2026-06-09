use alloc::vec;
use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FileFlags, OpenOptions};
use axio::{Seek, SeekFrom};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::{__kernel_off_t, W_OK};
use starry_vm::{VmMutPtr, VmPtr};
use syscalls::Sysno;

use crate::{
    file::{
        Directory, File, FileHandle, FileLike, FileLikeKind, Pipe, Socket, allowed_write_len,
        check_resize_limit, get_file_like, get_typed_file,
        inotify::{notify_read, notify_write},
        executable, inode_flags, lease, memfd,
        permission::check_open_permissions,
    },
    mm::{IoVec, IoVectorBuf, UserConstPtr, VmBytes, VmBytesMut},
    pseudofs::tmp,
    task::AsThread,
};

const SEEK_DATA: c_int = 3;
const SEEK_HOLE: c_int = 4;
const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
const TMPFS_FALLOC_BLOCK_SIZE: u64 = 4096;
const FALLOC_IO_CHUNK: usize = 64 * 1024;
const MAX_FILE_OFFSET: u64 = i64::MAX as u64;
const SPLICE_F_NONBLOCK: u32 = 0x02;
const SYNC_FILE_RANGE_WAIT_BEFORE: u32 = 0x01;
const SYNC_FILE_RANGE_WRITE: u32 = 0x02;
const SYNC_FILE_RANGE_WAIT_AFTER: u32 = 0x04;

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

pub fn sys_dummy_fd(sysno: Sysno) -> AxResult<isize> {
    warn!("Unimplemented fd syscall: {sysno}");
    Err(AxError::Unsupported)
}

/// Read data from the file indicated by `fd`.
///
/// Return the read size if success.
pub fn sys_read(fd: i32, buf: *mut u8, len: usize) -> AxResult<isize> {
    debug!("sys_read <= fd: {fd}, buf: {buf:p}, len: {len}");
    let read = get_file_like(fd)?.read(&mut VmBytesMut::new(buf, len))? as isize;
    if read > 0 {
        notify_read(fd);
    }
    Ok(read)
}

pub fn sys_readv(fd: i32, iov: *const IoVec, iovcnt: usize) -> AxResult<isize> {
    debug!("sys_readv <= fd: {fd}, iovcnt: {iovcnt}");
    let f = get_file_like(fd)?;
    let read = f.read(&mut IoVectorBuf::new(iov, iovcnt)?.into_io())? as isize;
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
    let written = get_file_like(fd)?.write(&mut VmBytes::new(buf, len))? as isize;
    if written > 0 {
        notify_write(fd);
    }
    Ok(written)
}

pub fn sys_writev(fd: i32, iov: *const IoVec, iovcnt: usize) -> AxResult<isize> {
    debug!("sys_writev <= fd: {fd}, iovcnt: {iovcnt}");
    let iov = IoVectorBuf::new(iov, iovcnt)?;
    let written = if let Ok(file) = get_typed_file::<File>(fd) {
        iov.check_readable()?;
        file.write(&mut iov.into_io())?
    } else {
        let f = get_file_like(fd)?;
        f.write(&mut iov.into_io())?
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
        executable::check_not_active(file.inner().location())?;
        inode_flags::check_write(file.inner().location(), false)?;
    } else {
        file.inner().access(FileFlags::READ)?;
    }
    Ok(file)
}

fn current_file_offset(file: &axfs::File) -> AxResult<u64> {
    let mut file = file;
    file.seek(SeekFrom::Current(0))
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
            _ => unreachable!(),
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
            _ => unreachable!(),
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
    let mut io = IoVectorBuf::new(iov, iovcnt)?.into_io();
    let read = if offset == -1 {
        file.read(&mut io)?
    } else {
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
    let written = if offset == -1 {
        let appending = file.inner().flags().contains(FileFlags::APPEND);
        let write_offset = if appending {
            file.inner().location().len()?
        } else {
            current_file_offset(file.inner())?
        };
        let allowed = allowed_write_len(write_offset, io.len())?;
        inode_flags::check_write(file.inner().location(), appending)?;
        memfd::check_write(file.inner().location(), write_offset, allowed)?;
        let mut io = io.into_io();
        io.limit_remaining(allowed);
        file.write(&mut io)?
    } else {
        if file.inner().flags().contains(FileFlags::APPEND) {
            let append_offset = file.inner().location().len()?;
            let allowed = allowed_write_len(append_offset, io.len())?;
            inode_flags::check_write(file.inner().location(), true)?;
            memfd::check_write(file.inner().location(), append_offset, allowed)?;
            let mut io = io.into_io();
            io.limit_remaining(allowed);
            file.inner()
                .access(FileFlags::APPEND)?
                .append(io)?
                .0
        } else {
            let allowed = allowed_write_len(offset as u64, io.len())?;
            inode_flags::check_write(file.inner().location(), false)?;
            memfd::check_write(file.inner().location(), offset as u64, allowed)?;
            let mut io = io.into_io();
            io.limit_remaining(allowed);
            file.inner()
                .write_at(io, offset as u64)?
        }
    } as isize;
    if written > 0 {
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
    if offset > size {
        return Err(AxError::InvalidInput);
    }
    if offset == size {
        return if seek_hole {
            Ok(size)
        } else {
            Err(AxError::NotFound)
        };
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
        Err(AxError::NotFound)
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
    let supplementary_groups = proc_data.supplementary_groups();
    let loc = FS_CONTEXT.lock().resolve(path)?;
    check_open_permissions(
        &loc,
        W_OK as u32,
        proc_data.fsuid(),
        proc_data.fsgid(),
        &supplementary_groups,
    )?;
    check_resize_limit(length as u64)?;
    executable::check_not_active(&loc)?;
    inode_flags::check_resize(&loc)?;
    lease::wait_for_truncate(&loc)?;
    memfd::check_resize(&loc, length as u64)?;
    let file = OpenOptions::new()
        .write(true)
        .open(&FS_CONTEXT.lock(), path)?
        .into_file()?;
    file.access(FileFlags::WRITE)?.set_len(length as _)?;
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
    executable::check_not_active(f.inner().location())?;
    inode_flags::check_resize(f.inner().location())?;
    lease::wait_for_truncate(f.inner().location())?;
    memfd::check_resize(f.inner().location(), length as u64)?;
    backend.set_len(length as _)?;
    notify_write(fd);
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

    let file = f.inner();
    let backend = file.backend()?;
    let loc = backend.location().clone();
    executable::check_not_active(&loc)?;
    inode_flags::check_resize(&loc)?;
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
            backend.set_len(size.max(end))?;
            let _ = tmp::reserve_fallocate_range(&loc, offset, len, false);
        }
        FALLOC_FL_KEEP_SIZE => {
            let _ = tmp::reserve_fallocate_range(&loc, offset, len, false);
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
            let _ = tmp::reserve_fallocate_range(&loc, offset, zero_len, false);
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
    let write = if f.inner().flags().contains(FileFlags::APPEND) {
        let append_offset = f.inner().location().len()?;
        let allowed = allowed_write_len(append_offset, len)?;
        inode_flags::check_write(f.inner().location(), true)?;
        memfd::check_write(f.inner().location(), append_offset, allowed)?;
        f.inner()
            .access(FileFlags::APPEND)?
            .append(VmBytes::new(buf, allowed))?
            .0
    } else {
        let allowed = allowed_write_len(offset as u64, len)?;
        inode_flags::check_write(f.inner().location(), false)?;
        memfd::check_write(f.inner().location(), offset as u64, allowed)?;
        f.inner()
            .write_at(VmBytes::new(buf, allowed), offset as _)?
    };
    if write > 0 {
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
                offset.vm_write(off + bytes_read as u64)?;
                Ok(bytes_read)
            }
        }
    }

    fn write(&mut self, mut buf: &[u8]) -> AxResult<usize> {
        match self {
            SendFile::Direct(file) => file.write(&mut buf),
            SendFile::Offset(file, offset) => {
                let off = offset.vm_read()?;
                executable::check_not_active(file.inner().location())?;
                inode_flags::check_write(file.inner().location(), false)?;
                let bytes_written = file.inner().write_at(buf, off)?;
                offset.vm_write(off + bytes_written as u64)?;
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
        if matches!(&socket.0, axnet::Socket::Unix(unix) if !unix.is_connected()) {
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
        if offset.vm_read()? > u32::MAX as u64 {
            return Err(AxError::InvalidInput);
        }
        SendFile::Offset(File::from_fd(in_fd)?, offset)
    } else {
        SendFile::Direct(get_file_like(in_fd)?)
    };

    let dst = SendFile::Direct(get_file_like(out_fd)?);

    do_send(src, dst, len).map(|n| n as _)
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
        SendFile::Offset(dst_file, off_out)
    } else {
        SendFile::Direct(get_file_like(fd_out)?)
    };

    do_send(src, dst, len).map(|n| n as _)
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

    do_send(src, dst, len).map(|n| n as _)
}

pub fn sys_tee(fd_in: c_int, fd_out: c_int, len: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_tee <= fd_in: {fd_in}, fd_out: {fd_out}, len: {len}, flags: {flags:#x}");

    let src = pipe_from_fd(fd_in, AxError::InvalidInput)?;
    let dst = pipe_from_fd(fd_out, AxError::InvalidInput)?;
    src.tee_to(&dst, len, flags & SPLICE_F_NONBLOCK != 0)
        .map(|n| n as _)
}

pub fn sys_vmsplice(fd: c_int, iov: *const IoVec, nr_segs: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_vmsplice <= fd: {fd}, iov: {iov:p}, nr_segs: {nr_segs}, flags: {flags:#x}");

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
