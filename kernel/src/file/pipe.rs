use alloc::{borrow::Cow, format, string::ToString, sync::Arc};
use core::{
    cmp::min,
    mem,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::Location;
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::Mutex;
use axtask::{
    current,
    future::{block_on, poll_io},
};
use linux_raw_sys::{
    general::{CAP_SYS_RESOURCE, O_ACCMODE, O_NONBLOCK, O_RDONLY, O_WRONLY, POLL_IN},
    ioctl::FIONREAD,
};
use memory_addr::PAGE_SIZE_4K;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer},
};
use starry_signal::{SignalInfo, Signo};
use starry_vm::VmMutPtr;

use super::{AsyncIoState, FileLike, Kstat, PseudoInode, fs::location_to_kstat, send_sigio};
use crate::{
    file::{IoDst, IoSrc},
    task::{AsThread, send_signal_to_process},
};

const PIPE_BUF_SIZE: usize = PAGE_SIZE_4K;
const RING_BUFFER_INIT_SIZE: usize = 65536; // 64 KiB
const PIPE_MAX_CAPACITY_ARG: usize = 1 << 31;

static PIPE_MAX_SIZE: AtomicUsize = AtomicUsize::new(RING_BUFFER_INIT_SIZE);

fn round_pipe_size(size: usize) -> AxResult<usize> {
    if size > PIPE_MAX_CAPACITY_ARG {
        return Err(AxError::InvalidInput);
    }
    if size < PAGE_SIZE_4K {
        return Ok(PAGE_SIZE_4K);
    }
    size.checked_next_power_of_two()
        .filter(|&size| size <= PIPE_MAX_CAPACITY_ARG)
        .ok_or(AxError::InvalidInput)
}

fn pipe_capacity_limit() -> usize {
    PIPE_MAX_SIZE.load(Ordering::Relaxed).max(PAGE_SIZE_4K)
}

fn default_pipe_capacity() -> usize {
    match current().try_as_thread() {
        Some(thr) if thr.proc_data.euid() != 0 => RING_BUFFER_INIT_SIZE.min(pipe_capacity_limit()),
        _ => RING_BUFFER_INIT_SIZE,
    }
}

fn pipe_poll_writable(buffer: &HeapRb<u8>) -> bool {
    buffer.vacant_len() >= PIPE_BUF_SIZE
}

#[derive(Clone, Copy, Default)]
struct PipeTransfer {
    len: usize,
    became_readable: bool,
    became_writable: bool,
}

impl PipeTransfer {
    const fn none() -> Self {
        Self {
            len: 0,
            became_readable: false,
            became_writable: false,
        }
    }
}

fn notify_pipe_readable(poll_rx: &PollSet, async_io: &Mutex<PipeAsyncIo>, transfer: PipeTransfer) {
    if transfer.became_readable {
        poll_rx.wake();
    }
    if transfer.len > 0 {
        notify_async_readable(async_io);
    }
}

fn notify_pipe_writable(poll_tx: &PollSet, transfer: PipeTransfer) {
    if transfer.became_writable {
        poll_tx.wake();
    }
}

fn copy_slices_to_ring(dst: &mut HeapRb<u8>, src: &[&[u8]], max_len: usize) -> usize {
    let (left, right) = dst.vacant_slices_mut();
    // The ring buffer exposes valid writable byte slices here.
    let mut dst_slices = [
        unsafe { core::slice::from_raw_parts_mut(left.as_mut_ptr().cast::<u8>(), left.len()) },
        unsafe { core::slice::from_raw_parts_mut(right.as_mut_ptr().cast::<u8>(), right.len()) },
    ];
    let mut copied = 0;
    let mut src_index = 0;
    let mut src_offset = 0;
    for dst_slice in dst_slices.iter_mut() {
        let mut dst_offset = 0;
        while dst_offset < dst_slice.len() && copied < max_len {
            while src_index < src.len() && src_offset == src[src_index].len() {
                src_index += 1;
                src_offset = 0;
            }
            if src_index == src.len() {
                unsafe { dst.advance_write_index(copied) };
                return copied;
            }
            let src_slice = src[src_index];
            let count = min(dst_slice.len() - dst_offset, max_len - copied)
                .min(src_slice.len() - src_offset);
            dst_slice[dst_offset..dst_offset + count]
                .copy_from_slice(&src_slice[src_offset..src_offset + count]);
            dst_offset += count;
            src_offset += count;
            copied += count;
        }
    }
    unsafe { dst.advance_write_index(copied) };
    copied
}

fn write_pipe_buffer(
    buffer: &Mutex<HeapRb<u8>>,
    src: &mut IoSrc,
    atomic_len: Option<usize>,
) -> AxResult<PipeTransfer> {
    let mut prod = buffer.lock();
    let was_empty = prod.occupied_len() == 0;
    if atomic_len.is_some_and(|len| prod.vacant_len() < len) {
        return Ok(PipeTransfer::none());
    }

    let (left, right) = prod.vacant_slices_mut();
    // The ring buffer exposes valid writable byte slices here.
    let left =
        unsafe { core::slice::from_raw_parts_mut(left.as_mut_ptr().cast::<u8>(), left.len()) };
    let right =
        unsafe { core::slice::from_raw_parts_mut(right.as_mut_ptr().cast::<u8>(), right.len()) };
    let mut count = src.read(left)?;
    if count >= left.len() {
        count += src.read(right)?;
    }
    unsafe { prod.advance_write_index(count) };
    Ok(PipeTransfer {
        len: count,
        became_readable: was_empty && count > 0,
        became_writable: false,
    })
}

fn read_pipe_buffer(buffer: &Mutex<HeapRb<u8>>, dst: &mut IoDst) -> AxResult<PipeTransfer> {
    let cons = buffer.lock();
    let was_writable = pipe_poll_writable(&cons);
    let (left, right) = cons.as_slices();
    let mut count = dst.write(left)?;
    if count >= left.len() {
        count += dst.write(right)?;
    }
    unsafe { cons.advance_read_index(count) };
    Ok(PipeTransfer {
        len: count,
        became_readable: false,
        became_writable: !was_writable && pipe_poll_writable(&cons),
    })
}

struct Shared {
    inode: PseudoInode,
    buffer: Mutex<HeapRb<u8>>,
    poll_rx: PollSet,
    poll_tx: PollSet,
    poll_close: PollSet,
    async_io: Mutex<PipeAsyncIo>,
    readers: AtomicUsize,
    writers: AtomicUsize,
}

#[derive(Clone)]
struct PipeAsyncIo {
    enabled: bool,
    state: AsyncIoState,
    fd: i32,
}

impl Default for PipeAsyncIo {
    fn default() -> Self {
        Self {
            enabled: false,
            state: AsyncIoState::default(),
            fd: -1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PipeAccess {
    Read,
    Write,
    ReadWrite,
}

impl PipeAccess {
    fn from_flags(flags: u32) -> Self {
        match flags & O_ACCMODE {
            O_WRONLY => Self::Write,
            O_RDONLY => Self::Read,
            _ => Self::ReadWrite,
        }
    }

    const fn can_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    const fn can_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    const fn waits_for_writer(self, nonblocking: bool) -> bool {
        !nonblocking && matches!(self, Self::Read)
    }

    const fn waits_for_reader(self, nonblocking: bool) -> bool {
        !nonblocking && matches!(self, Self::Write)
    }
}

struct NamedPipeState {
    buffer: Mutex<HeapRb<u8>>,
    poll_rx: PollSet,
    poll_tx: PollSet,
    poll_open: PollSet,
    async_io: Mutex<PipeAsyncIo>,
    readers: AtomicUsize,
    writers: AtomicUsize,
}

impl NamedPipeState {
    fn new() -> Self {
        Self {
            buffer: Mutex::new(HeapRb::new(default_pipe_capacity())),
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
            poll_open: PollSet::new(),
            async_io: Mutex::new(PipeAsyncIo::default()),
            readers: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
        }
    }

    fn reader_count(&self) -> usize {
        self.readers.load(Ordering::Acquire)
    }

    fn writer_count(&self) -> usize {
        self.writers.load(Ordering::Acquire)
    }

    fn add_access(&self, access: PipeAccess) {
        if access.can_read() {
            self.readers.fetch_add(1, Ordering::AcqRel);
        }
        if access.can_write() {
            self.writers.fetch_add(1, Ordering::AcqRel);
        }
        self.poll_open.wake();
        self.poll_rx.wake();
        self.poll_tx.wake();
    }

    fn remove_access(&self, access: PipeAccess) {
        if access.can_read() {
            self.readers.fetch_sub(1, Ordering::AcqRel);
        }
        if access.can_write() {
            self.writers.fetch_sub(1, Ordering::AcqRel);
        }
        self.poll_open.wake();
        self.poll_rx.wake();
        self.poll_tx.wake();
    }
}

struct NamedPipeOpenWaiter<'a> {
    state: &'a NamedPipeState,
}

impl Pollable for NamedPipeOpenWaiter<'_> {
    fn poll(&self) -> IoEvents {
        IoEvents::IN
    }

    fn register(&self, context: &mut Context<'_>, _events: IoEvents) {
        self.state.poll_open.register(context.waker());
    }
}

pub(crate) struct NamedPipe {
    access: PipeAccess,
    location: Location,
    state: Arc<NamedPipeState>,
    non_blocking: AtomicBool,
}

impl Drop for NamedPipe {
    fn drop(&mut self) {
        self.state.remove_access(self.access);
    }
}

pub struct Pipe {
    read_side: bool,
    shared: Arc<Shared>,
    non_blocking: AtomicBool,
}
impl Drop for Pipe {
    fn drop(&mut self) {
        if self.read_side {
            self.shared.readers.fetch_sub(1, Ordering::AcqRel);
        } else {
            self.shared.writers.fetch_sub(1, Ordering::AcqRel);
        }
        self.shared.poll_rx.wake();
        self.shared.poll_tx.wake();
        self.shared.poll_close.wake();
    }
}

impl Pipe {
    pub fn new() -> (Pipe, Pipe) {
        let shared = Arc::new(Shared {
            inode: PseudoInode::pipe(),
            buffer: Mutex::new(HeapRb::new(default_pipe_capacity())),
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
            poll_close: PollSet::new(),
            async_io: Mutex::new(PipeAsyncIo::default()),
            readers: AtomicUsize::new(1),
            writers: AtomicUsize::new(1),
        });
        let read_end = Pipe {
            read_side: true,
            shared: shared.clone(),
            non_blocking: AtomicBool::new(false),
        };
        let write_end = Pipe {
            read_side: false,
            shared,
            non_blocking: AtomicBool::new(false),
        };
        (read_end, write_end)
    }

    pub const fn is_read(&self) -> bool {
        self.read_side
    }

    pub const fn is_write(&self) -> bool {
        !self.read_side
    }

    pub fn closed(&self) -> bool {
        if self.read_side {
            self.shared.writers.load(Ordering::Acquire) == 0
        } else {
            self.shared.readers.load(Ordering::Acquire) == 0
        }
    }

    pub fn capacity(&self) -> usize {
        self.shared.buffer.lock().capacity().get()
    }

    pub fn resize(&self, requested_size: usize) -> AxResult<usize> {
        let new_size = round_pipe_size(requested_size)?;

        if current().try_as_thread().is_some_and(|thr| {
            !thr.proc_data.has_effective_capability(CAP_SYS_RESOURCE)
                && new_size > pipe_capacity_limit()
        }) {
            return Err(AxError::OperationNotPermitted);
        }

        let mut buffer = self.shared.buffer.lock();
        if new_size == buffer.capacity().get() {
            return Ok(new_size);
        }
        if new_size < buffer.occupied_len() {
            return Err(AxError::ResourceBusy);
        }
        let old_buffer = mem::replace(&mut *buffer, HeapRb::new(new_size));
        let (left, right) = old_buffer.as_slices();
        buffer.push_slice(left);
        buffer.push_slice(right);
        Ok(new_size)
    }

    pub(crate) fn set_async_io(&self, enabled: bool, state: AsyncIoState, fd: i32) {
        if self.is_read() {
            *self.shared.async_io.lock() = PipeAsyncIo { enabled, state, fd };
        }
    }

    pub fn vmsplice_read(&self, dst: &mut IoDst, nonblocking: bool) -> AxResult<usize> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        block_on(poll_io(self, IoEvents::IN, nonblocking, || {
            let read = read_pipe_buffer(&self.shared.buffer, dst)?;
            if read.len > 0 {
                notify_pipe_writable(&self.shared.poll_tx, read);
                Ok(read.len)
            } else if self.closed() {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            }
        }))
    }

    pub fn vmsplice_write(&self, src: &mut IoSrc, nonblocking: bool) -> AxResult<usize> {
        if !self.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        if src.remaining() == 0 {
            return Ok(0);
        }

        block_on(poll_io(self, IoEvents::OUT, nonblocking, || {
            if self.closed() {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = write_pipe_buffer(&self.shared.buffer, src, None)?;
            if written.len > 0 {
                notify_pipe_readable(&self.shared.poll_rx, &self.shared.async_io, written);
                Ok(written.len)
            } else {
                Err(AxError::WouldBlock)
            }
        }))
    }

    pub fn tee_to(&self, out: &Self, len: usize, nonblocking: bool) -> AxResult<usize> {
        if !self.is_read() || !out.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        if len == 0 {
            return Ok(0);
        }
        if Arc::ptr_eq(&self.shared, &out.shared) {
            return Err(AxError::InvalidInput);
        }

        struct TeePoll<'a> {
            src: &'a Pipe,
            dst: &'a Pipe,
        }

        impl Pollable for TeePoll<'_> {
            fn poll(&self) -> IoEvents {
                let mut events = IoEvents::empty();
                let src = self.src.shared.buffer.lock();
                events.set(IoEvents::IN, src.occupied_len() > 0);
                drop(src);
                let dst = self.dst.shared.buffer.lock();
                events.set(IoEvents::OUT, pipe_poll_writable(&dst));
                events
            }

            fn register(&self, context: &mut Context<'_>, events: IoEvents) {
                if events.contains(IoEvents::IN) {
                    self.src.shared.poll_rx.register(context.waker());
                }
                if events.contains(IoEvents::OUT) {
                    self.dst.shared.poll_tx.register(context.waker());
                }
                self.src.shared.poll_close.register(context.waker());
                self.dst.shared.poll_close.register(context.waker());
            }
        }

        let poller = TeePoll {
            src: self,
            dst: out,
        };
        let mut total_copied = 0usize;
        block_on(poll_io(
            &poller,
            IoEvents::IN | IoEvents::OUT,
            nonblocking,
            || {
                if out.closed() {
                    raise_pipe();
                    return Err(AxError::BrokenPipe);
                }
                let remaining = len - total_copied;
                if remaining == 0 {
                    return Ok(total_copied);
                }

                let src_available = self.shared.buffer.lock().occupied_len();
                if src_available == 0 {
                    return if self.closed() {
                        Ok(total_copied)
                    } else {
                        Err(AxError::WouldBlock)
                    };
                }

                let dst_space = out.shared.buffer.lock().vacant_len();
                if dst_space == 0 {
                    return Err(AxError::WouldBlock);
                }

                let to_copy = remaining.min(src_available).min(dst_space);
                let written = {
                    let src = self.shared.buffer.lock();
                    let (src_left, src_right) = src.as_slices();
                    let mut dst = out.shared.buffer.lock();
                    let was_empty = dst.occupied_len() == 0;
                    let written = copy_slices_to_ring(&mut dst, &[src_left, src_right], to_copy);
                    PipeTransfer {
                        len: written,
                        became_readable: was_empty && written > 0,
                        became_writable: false,
                    }
                };
                if written.len == 0 {
                    return Err(AxError::WouldBlock);
                }
                notify_pipe_readable(&out.shared.poll_rx, &out.shared.async_io, written);
                total_copied += written.len;
                if total_copied == len || nonblocking {
                    Ok(total_copied)
                } else {
                    Err(AxError::WouldBlock)
                }
            },
        ))
    }
}

pub(crate) fn pipe_max_size() -> usize {
    pipe_capacity_limit()
}

pub(crate) fn set_pipe_max_size(size: usize) -> AxResult<()> {
    PIPE_MAX_SIZE.store(round_pipe_size(size)?, Ordering::Relaxed);
    Ok(())
}

fn raise_pipe() {
    let curr = current();
    let _ = send_signal_to_process(
        curr.as_thread().proc_data.proc.pid(),
        Some(SignalInfo::new_kernel(Signo::SIGPIPE)),
    );
}

fn notify_async_readable(async_io: &Mutex<PipeAsyncIo>) {
    let async_io = async_io.lock().clone();
    if !async_io.enabled {
        return;
    }
    send_sigio(&async_io.state, async_io.fd, POLL_IN);
}

impl NamedPipe {
    pub(crate) fn open(location: Location, flags: u32) -> AxResult<Self> {
        let access = PipeAccess::from_flags(flags);
        let nonblocking = flags & O_NONBLOCK != 0;
        let state = {
            let mut guard = location.user_data();
            guard.try_get_or_insert_with(NamedPipeState::new)?
        };

        if access == PipeAccess::Write && nonblocking && state.reader_count() == 0 {
            return Err(AxError::from(LinuxError::ENXIO));
        }

        state.add_access(access);

        let waiter = NamedPipeOpenWaiter {
            state: state.as_ref(),
        };
        let wait_result = if access.waits_for_writer(nonblocking) {
            block_on(poll_io(&waiter, IoEvents::IN, false, || {
                if state.writer_count() > 0 {
                    Ok(())
                } else {
                    Err(AxError::WouldBlock)
                }
            }))
        } else if access.waits_for_reader(nonblocking) {
            block_on(poll_io(&waiter, IoEvents::IN, false, || {
                if state.reader_count() > 0 {
                    Ok(())
                } else {
                    Err(AxError::WouldBlock)
                }
            }))
        } else {
            Ok(())
        };

        if let Err(err) = wait_result {
            state.remove_access(access);
            return Err(err);
        }

        Ok(Self {
            access,
            location,
            state,
            non_blocking: AtomicBool::new(nonblocking),
        })
    }

    fn fifo_path(&self) -> Cow<'_, str> {
        self.location
            .absolute_path()
            .map_or_else(|_| "<error>".into(), |path| Cow::Owned(path.to_string()))
    }

    pub(crate) fn set_async_io(&self, enabled: bool, state: AsyncIoState, fd: i32) {
        if self.access.can_read() {
            *self.state.async_io.lock() = PipeAsyncIo { enabled, state, fd };
        }
    }
}

impl FileLike for Pipe {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        let nonblocking = self.nonblocking();
        block_on(poll_io(self, IoEvents::IN, nonblocking, || {
            let read = read_pipe_buffer(&self.shared.buffer, dst)?;
            if read.len > 0 {
                notify_pipe_writable(&self.shared.poll_tx, read);
                Ok(read.len)
            } else if self.closed() {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            }
        }))
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        if !self.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        let size = src.remaining();
        if size == 0 {
            return Ok(0);
        }

        let atomic_len = (size <= PIPE_BUF_SIZE).then_some(size);
        let mut total_written = 0;
        let nonblocking = self.nonblocking();

        block_on(poll_io(self, IoEvents::OUT, nonblocking, || {
            if self.closed() {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = write_pipe_buffer(&self.shared.buffer, src, atomic_len)?;
            if written.len > 0 {
                notify_pipe_readable(&self.shared.poll_rx, &self.shared.async_io, written);
                total_written += written.len;
                if total_written == size || self.nonblocking() {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        }))
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.shared.inode.stat())
    }

    fn path(&self) -> Cow<'_, str> {
        format!("pipe:[{}]", self.shared.inode.inode()).into()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            FIONREAD => {
                (arg as *mut u32).vm_write(self.shared.buffer.lock().occupied_len() as u32)?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }
}

impl Pollable for Pipe {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let buf = self.shared.buffer.lock();
        if self.read_side {
            events.set(IoEvents::IN, buf.occupied_len() > 0);
            events.set(IoEvents::HUP, self.closed());
        } else {
            events.set(IoEvents::OUT, pipe_poll_writable(&buf));
            events.set(IoEvents::ERR, self.closed());
        }
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.shared.poll_rx.register(context.waker());
        }
        if events.contains(IoEvents::OUT) {
            self.shared.poll_tx.register(context.waker());
        }
        self.shared.poll_close.register(context.waker());
    }
}

impl FileLike for NamedPipe {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if !self.access.can_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let read = {
                let cons = self.state.buffer.lock();
                let (left, right) = cons.as_slices();
                let mut count = dst.write(left)?;
                if count >= left.len() {
                    count += dst.write(right)?;
                }
                unsafe { cons.advance_read_index(count) };
                count
            };
            if read > 0 {
                self.state.poll_tx.wake();
                Ok(read)
            } else if self.state.writer_count() == 0 {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            }
        }))
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        if !self.access.can_write() {
            return Err(AxError::BadFileDescriptor);
        }
        let size = src.remaining();
        if size == 0 {
            return Ok(0);
        }

        let atomic_len = (size <= PIPE_BUF_SIZE).then_some(size);
        let mut total_written = 0;
        block_on(poll_io(self, IoEvents::OUT, self.nonblocking(), || {
            if self.state.reader_count() == 0 {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = write_pipe_buffer(&self.state.buffer, src, atomic_len)?;
            if written.len > 0 {
                self.state.poll_rx.wake();
                notify_async_readable(&self.state.async_io);
                total_written += written.len;
                if total_written == size || self.nonblocking() {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        }))
    }

    fn stat(&self) -> AxResult<Kstat> {
        location_to_kstat(&self.location)
    }

    fn path(&self) -> Cow<'_, str> {
        self.fifo_path()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            FIONREAD => {
                (arg as *mut u32).vm_write(self.state.buffer.lock().occupied_len() as u32)?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }
}

impl Pollable for NamedPipe {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let buf = self.state.buffer.lock();
        if self.access.can_read() {
            events.set(IoEvents::IN, buf.occupied_len() > 0);
            events.set(IoEvents::HUP, self.state.writer_count() == 0);
        }
        if self.access.can_write() {
            events.set(
                IoEvents::OUT,
                self.state.reader_count() > 0 && pipe_poll_writable(&buf),
            );
            events.set(IoEvents::ERR, self.state.reader_count() == 0);
        }
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if self.access.can_read() {
            if events.contains(IoEvents::IN) {
                self.state.poll_rx.register(context.waker());
            }
            if events.contains(IoEvents::HUP) {
                self.state.poll_open.register(context.waker());
            }
        }
        if self.access.can_write() {
            if events.contains(IoEvents::OUT) {
                self.state.poll_tx.register(context.waker());
            }
            self.state.poll_open.register(context.waker());
        }
    }
}
