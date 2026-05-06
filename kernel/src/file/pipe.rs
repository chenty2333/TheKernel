use alloc::{borrow::Cow, format, string::ToString, sync::Arc, vec};
use core::{
    cell::UnsafeCell,
    hint::spin_loop,
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
    general::{O_ACCMODE, O_NONBLOCK, O_RDONLY, O_WRONLY, S_IFIFO},
    ioctl::FIONREAD,
};
use memory_addr::PAGE_SIZE_4K;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer},
};
use starry_signal::{SignalInfo, Signo};
use starry_vm::VmMutPtr;

use super::{FileLike, Kstat, fs::metadata_to_kstat};
use crate::{
    file::{IoDst, IoSrc},
    task::{AsThread, send_signal_to_process},
};

const RING_BUFFER_INIT_SIZE: usize = 65536; // 64 KiB

static PIPE_MAX_SIZE: AtomicUsize = AtomicUsize::new(RING_BUFFER_INIT_SIZE);

struct AtomicGateGuard<'a>(&'a AtomicBool);

impl Drop for AtomicGateGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn acquire_gate(gate: &AtomicBool) -> AtomicGateGuard<'_> {
    while gate
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spin_loop();
    }
    AtomicGateGuard(gate)
}

struct PipeBuffer {
    buf: UnsafeCell<alloc::boxed::Box<[u8]>>,
    capacity: AtomicUsize,
    head: AtomicUsize,
    tail: AtomicUsize,
    read_gate: AtomicBool,
    write_gate: AtomicBool,
}

unsafe impl Sync for PipeBuffer {}

impl PipeBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: UnsafeCell::new(vec![0; capacity].into_boxed_slice()),
            capacity: AtomicUsize::new(capacity),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            read_gate: AtomicBool::new(false),
            write_gate: AtomicBool::new(false),
        }
    }

    fn capacity(&self) -> usize {
        self.capacity.load(Ordering::Acquire)
    }

    fn occupied_len(&self) -> usize {
        self.tail
            .load(Ordering::Acquire)
            .saturating_sub(self.head.load(Ordering::Acquire))
    }

    fn vacant_len(&self) -> usize {
        self.capacity().saturating_sub(self.occupied_len())
    }

    fn resize(&self, new_size: usize) -> AxResult<()> {
        let _write = acquire_gate(&self.write_gate);
        let _read = acquire_gate(&self.read_gate);

        let old_cap = self.capacity();
        if new_size == old_cap {
            return Ok(());
        }

        let head = self.head.load(Ordering::Acquire);
        let occupied = self.tail.load(Ordering::Acquire).saturating_sub(head);
        if new_size < occupied {
            return Err(AxError::ResourceBusy);
        }

        let old = unsafe { &*self.buf.get() };
        let mut new = vec![0; new_size].into_boxed_slice();
        let first = occupied.min(old_cap - head % old_cap);
        new[..first].copy_from_slice(&old[head % old_cap..head % old_cap + first]);
        if occupied > first {
            new[first..occupied].copy_from_slice(&old[..occupied - first]);
        }

        unsafe { *self.buf.get() = new };
        self.head.store(0, Ordering::Release);
        self.tail.store(occupied, Ordering::Release);
        self.capacity.store(new_size, Ordering::Release);
        Ok(())
    }

    fn read_into(&self, dst: &mut IoDst) -> AxResult<usize> {
        let _read = acquire_gate(&self.read_gate);
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        let available = tail.saturating_sub(head);
        if available == 0 || dst.is_full() {
            return Ok(0);
        }

        let cap = self.capacity();
        let count = available.min(dst.remaining_mut());
        let start = head % cap;
        let first = count.min(cap - start);
        let buf = unsafe { &*self.buf.get() };
        let mut copied = dst.write(&buf[start..start + first])?;
        if copied == first && copied < count {
            copied += dst.write(&buf[..count - first])?;
        }
        if copied > 0 {
            self.head.store(head + copied, Ordering::Release);
        }
        Ok(copied)
    }

    fn write_from(&self, src: &mut IoSrc) -> AxResult<usize> {
        let _write = acquire_gate(&self.write_gate);
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        let vacant = self.capacity().saturating_sub(tail.saturating_sub(head));
        if vacant == 0 || src.is_empty() {
            return Ok(0);
        }

        let cap = self.capacity();
        let count = vacant.min(src.remaining());
        let start = tail % cap;
        let first = count.min(cap - start);
        let buf = unsafe { &mut *self.buf.get() };
        let mut copied = src.read(&mut buf[start..start + first])?;
        if copied == first && copied < count {
            copied += src.read(&mut buf[..count - first])?;
        }
        if copied > 0 {
            self.tail.store(tail + copied, Ordering::Release);
        }
        Ok(copied)
    }

    fn peek_vec(&self, len: usize) -> alloc::vec::Vec<u8> {
        let _read = acquire_gate(&self.read_gate);
        let head = self.head.load(Ordering::Acquire);
        let available = self.tail.load(Ordering::Acquire).saturating_sub(head);
        let count = len.min(available);
        let cap = self.capacity();
        let start = head % cap;
        let first = count.min(cap - start);
        let buf = unsafe { &*self.buf.get() };
        let mut out = vec![0; count];
        out[..first].copy_from_slice(&buf[start..start + first]);
        if count > first {
            out[first..].copy_from_slice(&buf[..count - first]);
        }
        out
    }

    fn write_slice(&self, src: &[u8]) -> usize {
        let _write = acquire_gate(&self.write_gate);
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        let vacant = self.capacity().saturating_sub(tail.saturating_sub(head));
        let count = vacant.min(src.len());
        if count == 0 {
            return 0;
        }

        let cap = self.capacity();
        let start = tail % cap;
        let first = count.min(cap - start);
        let buf = unsafe { &mut *self.buf.get() };
        buf[start..start + first].copy_from_slice(&src[..first]);
        if count > first {
            buf[..count - first].copy_from_slice(&src[first..count]);
        }
        self.tail.store(tail + count, Ordering::Release);
        count
    }
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

struct Shared {
    buffer: PipeBuffer,
    poll_rx: PollSet,
    poll_tx: PollSet,
    poll_close: PollSet,
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
        self.shared.poll_close.wake();
    }
}

impl Pipe {
    pub fn new() -> (Pipe, Pipe) {
        let shared = Arc::new(Shared {
            buffer: PipeBuffer::new(default_pipe_capacity()),
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
            poll_close: PollSet::new(),
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
        Arc::strong_count(&self.shared) == 1
    }

    pub fn capacity(&self) -> usize {
        self.shared.buffer.capacity()
    }

    pub fn resize(&self, new_size: usize) -> AxResult<()> {
        let new_size = new_size.div_ceil(PAGE_SIZE_4K).max(1) * PAGE_SIZE_4K;
        if current()
            .try_as_thread()
            .is_some_and(|thr| thr.proc_data.euid() != 0 && new_size > pipe_capacity_limit())
        {
            return Err(AxError::OperationNotPermitted);
        }

        self.shared.buffer.resize(new_size)
    }

    pub fn vmsplice_read(&self, dst: &mut IoDst, nonblocking: bool) -> AxResult<usize> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        block_on(poll_io(self, IoEvents::IN, nonblocking, || {
            let read = self.shared.buffer.read_into(dst)?;
            if read > 0 {
                self.shared.poll_tx.wake();
                Ok(read)
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

            let written = self.shared.buffer.write_from(src)?;
            if written > 0 {
                self.shared.poll_rx.wake();
                Ok(written)
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
                events.set(IoEvents::IN, self.src.shared.buffer.occupied_len() > 0);
                events.set(IoEvents::OUT, self.dst.shared.buffer.vacant_len() > 0);
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

                let src_available = self.shared.buffer.occupied_len();
                if src_available == 0 {
                    return if self.closed() {
                        Ok(total_copied)
                    } else {
                        Err(AxError::WouldBlock)
                    };
                }

                let dst_space = out.shared.buffer.vacant_len();
                if dst_space == 0 {
                    return Err(AxError::WouldBlock);
                }

                let to_copy = remaining.min(src_available).min(dst_space);
                let tmp = self.shared.buffer.peek_vec(to_copy);
                let copied = out.shared.buffer.write_slice(&tmp);
                if copied == 0 {
                    return Err(AxError::WouldBlock);
                }
                out.shared.poll_rx.wake();
                total_copied += copied;
                if total_copied == len || nonblocking {
                    Ok(total_copied)
                } else {
                    Err(AxError::WouldBlock)
                }
            },
        ))
    }

    /// Fast-path read that bypasses `FileDescription` and `FileLike` vtable
    /// dispatch. Called directly from `sys_read` when the fd table indicates
    /// a pipe.
    pub fn read_fast(&self, dst: &mut super::types::IoDst) -> AxResult<usize> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        // Fast synchronous path: copy immediately without entering the async
        // poll/park machinery.
        let count = self.shared.buffer.read_into(dst)?;
        if count > 0 {
            self.shared.poll_tx.wake();
            return Ok(count);
        }
        if self.closed() {
            return Ok(0);
        }
        if self.nonblocking() {
            return Err(AxError::WouldBlock);
        }

        // Slow path: nothing available right now, wait via async poll.
        block_on(poll_io(self, IoEvents::IN, false, || {
            let read = self.shared.buffer.read_into(dst)?;
            if read > 0 {
                self.shared.poll_tx.wake();
                Ok(read)
            } else if self.closed() {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            }
        }))
    }

    /// Fast-path write that bypasses `FileDescription` and `FileLike` vtable
    /// dispatch. Called directly from `sys_write` when the fd table indicates
    /// a pipe.
    pub fn write_fast(&self, src: &mut super::types::IoSrc) -> AxResult<usize> {
        if !self.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        let size = src.remaining();
        if size == 0 {
            return Ok(0);
        }

        // Fast synchronous path: try to write immediately into the ring buffer
        // without entering the async poll/park machinery.  Only attempt when
        // a reader is still attached — writing into a closed pipe must fail
        // with EPIPE (and SIGPIPE), not buffer unreadable bytes.
        let mut total_written = 0;
        if !self.closed() {
            total_written = self.shared.buffer.write_from(src)?;
        }
        if total_written > 0 {
            self.shared.poll_rx.wake();
            if total_written == size || self.nonblocking() {
                return Ok(total_written);
            }
        }
        if self.closed() {
            raise_pipe();
            return Err(AxError::BrokenPipe);
        }
        if self.nonblocking() {
            return Err(AxError::WouldBlock);
        }

        // Slow path: ring buffer full or partial write, wait for space.
        block_on(poll_io(self, IoEvents::OUT, false, || {
            if self.closed() {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = self.shared.buffer.write_from(src)?;
            if written > 0 {
                self.shared.poll_rx.wake();
                total_written += written;
                if total_written == size || self.nonblocking() {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        }))
    }
}

pub(crate) fn pipe_max_size() -> usize {
    pipe_capacity_limit()
}

pub(crate) fn set_pipe_max_size(size: usize) {
    PIPE_MAX_SIZE.store(size.max(PAGE_SIZE_4K), Ordering::Relaxed);
}

fn raise_pipe() {
    let curr = current();
    send_signal_to_process(
        curr.as_thread().proc_data.proc.pid(),
        Some(SignalInfo::new_kernel(Signo::SIGPIPE)),
    )
    .expect("Failed to send SIGPIPE");
}

impl NamedPipe {
    pub(crate) fn open(location: Location, flags: u32) -> AxResult<Self> {
        let access = PipeAccess::from_flags(flags);
        let nonblocking = flags & O_NONBLOCK != 0;
        let state = {
            let mut guard = location.user_data();
            guard.get_or_insert_with(NamedPipeState::new)
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
}

impl FileLike for Pipe {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_fast(dst)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.write_fast(src)
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(Kstat {
            mode: S_IFIFO | if self.is_read() { 0o444 } else { 0o222 },
            ..Default::default()
        })
    }

    fn path(&self) -> Cow<'_, str> {
        format!("pipe:[{}]", self as *const _ as usize).into()
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
                (arg as *mut u32).vm_write(self.shared.buffer.occupied_len() as u32)?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }
}

impl Pollable for Pipe {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if self.read_side {
            events.set(IoEvents::IN, self.shared.buffer.occupied_len() > 0);
            events.set(IoEvents::HUP, self.closed());
        } else {
            events.set(IoEvents::OUT, self.shared.buffer.vacant_len() > 0);
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

        let mut total_written = 0;
        block_on(poll_io(self, IoEvents::OUT, self.nonblocking(), || {
            if self.state.reader_count() == 0 {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = {
                let mut prod = self.state.buffer.lock();
                let (left, right) = prod.vacant_slices_mut();
                let left = unsafe {
                    core::slice::from_raw_parts_mut(left.as_mut_ptr().cast::<u8>(), left.len())
                };
                let right = unsafe {
                    core::slice::from_raw_parts_mut(right.as_mut_ptr().cast::<u8>(), right.len())
                };
                let mut count = src.read(left)?;
                if count >= left.len() {
                    count += src.read(right)?;
                }
                unsafe { prod.advance_write_index(count) };
                count
            };
            if written > 0 {
                self.state.poll_rx.wake();
                total_written += written;
                if total_written == size || self.nonblocking() {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        }))
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(metadata_to_kstat(&self.location.metadata()?))
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
                self.state.reader_count() > 0 && buf.vacant_len() > 0,
            );
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
