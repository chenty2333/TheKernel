use alloc::vec;
use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FS_CONTEXT, FileFlags, OpenOptions};
use axio::{IoBuf, Seek, SeekFrom};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::__kernel_off_t;
use starry_vm::{VmMutPtr, VmPtr};
use syscalls::Sysno;

use crate::{
    file::{
        Directory, File, FileHandle, FileLike, FileLikeKind, Pipe, get_file_like, get_typed_file,
        inotify::{notify_read, notify_write},
        lease,
        memfd,
    },
    mm::{IoVec, IoVectorBuf, UserConstPtr, VmBytes, VmBytesMut},
    pseudofs::tmp,
};

const SEEK_DATA: c_int = 3;
const SEEK_HOLE: c_int = 4;
const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
const TMPFS_FALLOC_BLOCK_SIZE: u64 = 4096;
const FALLOC_IO_CHUNK: usize = 64 * 1024;

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
    let f = get_file_like(fd)?;
    let written = f.write(&mut IoVectorBuf::new(iov, iovcnt)?.into_io())? as isize;
    if written > 0 {
        notify_write(fd);
    }
    Ok(written)
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
    positioned_file(fd, FileFlags::WRITE)
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
    let mut io = IoVectorBuf::new(iov, iovcnt)?.into_io();
    let written = if offset == -1 {
        file.write(&mut io)?
    } else {
        memfd::check_write(file.inner().location(), offset as u64, io.remaining())?;
        file.inner().write_at(io, offset as u64)?
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
                return result.map(|off| off as isize);
            }
            Err(AxError::Unsupported)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_truncate(path: UserConstPtr<c_char>, length: __kernel_off_t) -> AxResult<isize> {
    let path = path.get_as_str()?;
    debug!("sys_truncate <= {path:?} {length}");
    if length < 0 {
        return Err(AxError::InvalidInput);
    }
    let loc = FS_CONTEXT.lock().resolve(path)?;
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
    if offset < 0 || len < 0 {
        return Err(AxError::InvalidInput);
    }

    let f = File::from_fd(fd)?;
    f.inner().access(FileFlags::WRITE)?;

    let file = f.inner();
    let backend = file.backend()?;
    let loc = backend.location().clone();
    let offset = offset as u64;
    let len = len as u64;
    let end = offset.checked_add(len).ok_or(AxError::InvalidInput)?;
    let size = loc.len()?;
    let seals = memfd::current_seals(&loc).unwrap_or(0);
    let supported_modes = FALLOC_FL_KEEP_SIZE
        | FALLOC_FL_PUNCH_HOLE
        | FALLOC_FL_COLLAPSE_RANGE
        | FALLOC_FL_ZERO_RANGE;

    if mode & !supported_modes != 0 {
        return Err(AxError::OperationNotSupported);
    }

    match mode {
        0 => {
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
            copy_within_file(file, end, offset, size - end)?;
            if let Some(result) = tmp::collapse_fallocate_range(&loc, offset, len) {
                result?;
            } else {
                return Err(AxError::OperationNotSupported);
            }
            backend.set_len(size - len)?;
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

pub fn sys_fadvise64(
    fd: c_int,
    offset: __kernel_off_t,
    len: __kernel_off_t,
    advice: u32,
) -> AxResult<isize> {
    debug!("sys_fadvise64 <= fd: {fd}, offset: {offset}, len: {len}, advice: {advice}");
    if Pipe::from_fd(fd).is_ok() {
        return Err(AxError::BrokenPipe);
    }
    if advice > 5 {
        return Err(AxError::InvalidInput);
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
    if len == 0 {
        return Ok(0);
    }
    if offset < 0 {
        return Err(AxError::InvalidInput);
    }
    let f = positioned_write_file(fd)?;
    memfd::check_write(f.inner().location(), offset as u64, len)?;
    let write = f.inner().write_at(VmBytes::new(buf, len), offset as _)?;
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

pub fn sys_sendfile(out_fd: c_int, in_fd: c_int, offset: *mut u64, len: usize) -> AxResult<isize> {
    debug!(
        "sys_sendfile <= out_fd: {}, in_fd: {}, offset: {}, len: {}",
        out_fd,
        in_fd,
        !offset.is_null(),
        len
    );

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

    // TODO: check flags
    // TODO: check both regular files
    // TODO: check same file and overlap

    let src = if !off_in.is_null() {
        SendFile::Offset(File::from_fd(fd_in)?, off_in)
    } else {
        SendFile::Direct(get_file_like(fd_in)?)
    };

    let dst = if !off_out.is_null() {
        SendFile::Offset(File::from_fd(fd_out)?, off_out)
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

    let src = if !off_in.is_null() {
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
        let f = get_file_like(fd_out)?;
        f.write(&mut b"".as_slice())?;
        SendFile::Direct(f)
    };

    if !has_pipe {
        return Err(AxError::InvalidInput);
    }

    do_send(src, dst, len).map(|n| n as _)
}
