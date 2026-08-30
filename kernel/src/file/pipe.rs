use alloc::{borrow::Cow, sync::Arc};
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
use axtask::current;
use linux_raw_sys::{
    general::{CAP_SYS_RESOURCE, O_ACCMODE, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, POLL_IN},
    ioctl::FIONREAD,
};
use memory_addr::PAGE_SIZE_4K;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer},
};
use thekernel_linux_signal::{SignalInfo, Signo};

use super::{
    AsyncIoState, FileLike, IoctlContext, Kstat, PseudoInode, fs::location_to_kstat, send_sigio,
    try_owned_path, try_pseudo_inode_path,
};
use crate::{
    file::{IoDst, IoSrc},
    readiness::block_on_poll_io,
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
    // Host unit tests do not initialize the scheduler/current-task slot. The
    // fallback is the same default used when no Linux thread policy applies.
    #[cfg(test)]
    if axtask::current_may_uninit().is_none() {
        return RING_BUFFER_INIT_SIZE;
    }
    match current().try_as_thread() {
        Some(thr) if !thr.current_cred().is_initial_root_euid() => {
            RING_BUFFER_INIT_SIZE.min(pipe_capacity_limit())
        }
        _ => RING_BUFFER_INIT_SIZE,
    }
}

fn pipe_poll_writable(buffer: &HeapRb<u8>) -> bool {
    buffer.vacant_len() >= PIPE_BUF_SIZE
}

const fn pipe_write_is_complete(written: usize, requested: usize, nonblocking: bool) -> bool {
    written == requested || nonblocking
}

#[derive(Clone, Copy, Default)]
struct PipeTransfer {
    len: usize,
    wake_readers: bool,
    became_writable: bool,
}

impl PipeTransfer {
    const fn none() -> Self {
        Self {
            len: 0,
            wake_readers: false,
            became_writable: false,
        }
    }
}

fn notify_pipe_readable(poll_rx: &PollSet, async_io: &Mutex<PipeAsyncIo>, transfer: PipeTransfer) {
    // Linux publishes a pipe read-side poll wake for every successful write,
    // not only for an empty-to-nonempty readiness transition. EPOLLET uses
    // that source notification to report newly appended data even when an
    // earlier byte remains unread.
    if transfer.wake_readers {
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
        wake_readers: count > 0,
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
        wake_readers: false,
        became_writable: !was_writable && pipe_poll_writable(&cons),
    })
}

#[derive(Clone, Copy)]
struct PipeReadReservation {
    available: usize,
    was_writable: bool,
}

fn reserve_pipe_prefix(
    source: &HeapRb<u8>,
    dst: &mut [u8],
    source_closed: bool,
) -> AxResult<Option<PipeReadReservation>> {
    let available = source.occupied_len().min(dst.len());
    if available == 0 {
        return if source_closed {
            Ok(None)
        } else {
            Err(AxError::WouldBlock)
        };
    }

    let (left, right) = source.as_slices();
    let left_len = left.len().min(available);
    dst[..left_len].copy_from_slice(&left[..left_len]);
    let right_len = available - left_len;
    if right_len > 0 {
        dst[left_len..available].copy_from_slice(&right[..right_len]);
    }
    Ok(Some(PipeReadReservation {
        available,
        was_writable: pipe_poll_writable(source),
    }))
}

fn commit_pipe_prefix(
    source: &HeapRb<u8>,
    written: usize,
    reservation: PipeReadReservation,
) -> AxResult<PipeTransfer> {
    if source.occupied_len() < written {
        return Err(AxError::BadState);
    }
    unsafe { source.advance_read_index(written) };
    Ok(PipeTransfer {
        len: written,
        wake_readers: false,
        became_writable: !reservation.was_writable && pipe_poll_writable(source),
    })
}

fn transfer_pipe_prefix(
    dst: &mut [u8],
    mut reserve: impl FnMut(&mut [u8]) -> AxResult<Option<PipeReadReservation>>,
    write: &mut impl FnMut(&[u8]) -> AxResult<usize>,
    mut commit: impl FnMut(usize, PipeReadReservation) -> AxResult<()>,
) -> AxResult<(usize, bool)> {
    let Some(reservation) = reserve(dst)? else {
        return Ok((0, false));
    };

    let written = write(&dst[..reservation.available])?;
    if written > reservation.available {
        return Err(AxError::InvalidInput);
    }
    commit(written, reservation)?;
    Ok((written, written < reservation.available))
}

fn move_pipe_buffer(src: &mut HeapRb<u8>, dst: &mut HeapRb<u8>, max_len: usize) -> PipeTransfer {
    let (left, right) = src.as_slices();
    let written = copy_slices_to_ring(dst, &[left, right], max_len);
    // `written` came from the currently occupied source slices and therefore
    // cannot exceed the initialized prefix owned by the consumer.
    unsafe { src.advance_read_index(written) };
    PipeTransfer {
        len: written,
        wake_readers: written > 0,
        became_writable: false,
    }
}

fn copy_pipe_buffer(src: &HeapRb<u8>, dst: &mut HeapRb<u8>, max_len: usize) -> PipeTransfer {
    let (left, right) = src.as_slices();
    let written = copy_slices_to_ring(dst, &[left, right], max_len);
    PipeTransfer {
        len: written,
        wake_readers: written > 0,
        became_writable: false,
    }
}

fn blocked_pipe_transfer_result(
    progress: usize,
    source_empty: bool,
    source_closed: bool,
    destination_full: bool,
) -> AxResult<usize> {
    if progress > 0 {
        Ok(progress)
    } else if destination_full {
        // Output admission has Linux precedence over an empty closed input.
        Err(AxError::WouldBlock)
    } else if source_empty && source_closed {
        Ok(0)
    } else {
        Err(AxError::WouldBlock)
    }
}

struct Shared {
    inode: PseudoInode,
    /// Serializes consumers while allowing a transfer to release the ring
    /// lock before invoking an arbitrary destination.
    read_transaction: Mutex<()>,
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
    fn from_flags(flags: u32) -> AxResult<Self> {
        match flags & O_ACCMODE {
            O_RDONLY => Ok(Self::Read),
            O_WRONLY => Ok(Self::Write),
            O_RDWR => Ok(Self::ReadWrite),
            // Unlike regular files and O_TMPFILE, Linux rejects the reserved
            // access mode 3 for FIFOs instead of creating a no-data pipe
            // description. Never let the catch-all case grant both ends.
            _ => Err(AxError::InvalidInput),
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
    read_transaction: Mutex<()>,
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
            read_transaction: Mutex::new(()),
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
        IoEvents::READABLE
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::single(&self.state.poll_open, context.waker())
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

/// A borrowed anonymous-pipe or FIFO endpoint.
///
/// The two objects have the same byte-stream mechanics but different lifetime
/// owners. Keeping that distinction behind this view lets pipe-to-pipe splice
/// use one ordered ring transaction for every anonymous/FIFO combination.
#[derive(Clone, Copy)]
pub(crate) enum PipeEndpoint<'a> {
    Anonymous(&'a Pipe),
    Named(&'a NamedPipe),
}

impl<'a> PipeEndpoint<'a> {
    fn is_read(self) -> bool {
        match self {
            Self::Anonymous(pipe) => pipe.is_read(),
            Self::Named(pipe) => pipe.is_read(),
        }
    }

    fn is_write(self) -> bool {
        match self {
            Self::Anonymous(pipe) => pipe.is_write(),
            Self::Named(pipe) => pipe.is_write(),
        }
    }

    fn state_key(self) -> usize {
        match self {
            Self::Anonymous(pipe) => Arc::as_ptr(&pipe.shared).cast::<()>() as usize,
            Self::Named(pipe) => Arc::as_ptr(&pipe.state).cast::<()>() as usize,
        }
    }

    fn read_transaction(self) -> &'a Mutex<()> {
        match self {
            Self::Anonymous(pipe) => &pipe.shared.read_transaction,
            Self::Named(pipe) => &pipe.state.read_transaction,
        }
    }

    fn buffer(self) -> &'a Mutex<HeapRb<u8>> {
        match self {
            Self::Anonymous(pipe) => &pipe.shared.buffer,
            Self::Named(pipe) => &pipe.state.buffer,
        }
    }

    fn poll_rx(self) -> &'a PollSet {
        match self {
            Self::Anonymous(pipe) => &pipe.shared.poll_rx,
            Self::Named(pipe) => &pipe.state.poll_rx,
        }
    }

    fn poll_tx(self) -> &'a PollSet {
        match self {
            Self::Anonymous(pipe) => &pipe.shared.poll_tx,
            Self::Named(pipe) => &pipe.state.poll_tx,
        }
    }

    fn poll_close(self) -> &'a PollSet {
        match self {
            Self::Anonymous(pipe) => &pipe.shared.poll_close,
            Self::Named(pipe) => &pipe.state.poll_open,
        }
    }

    fn async_io(self) -> &'a Mutex<PipeAsyncIo> {
        match self {
            Self::Anonymous(pipe) => &pipe.shared.async_io,
            Self::Named(pipe) => &pipe.state.async_io,
        }
    }

    fn source_closed(self) -> bool {
        match self {
            Self::Anonymous(pipe) => pipe.shared.writers.load(Ordering::Acquire) == 0,
            Self::Named(pipe) => pipe.state.writer_count() == 0,
        }
    }

    fn destination_closed(self) -> bool {
        match self {
            Self::Anonymous(pipe) => pipe.shared.readers.load(Ordering::Acquire) == 0,
            Self::Named(pipe) => pipe.state.reader_count() == 0,
        }
    }

    fn notify_writable(self, transfer: PipeTransfer) {
        notify_pipe_writable(self.poll_tx(), transfer);
    }

    fn notify_readable(self, transfer: PipeTransfer) {
        notify_pipe_readable(self.poll_rx(), self.async_io(), transfer);
    }

    /// Moves bytes between any anonymous-pipe/FIFO pair in one ordered ring
    /// transaction. Output admission intentionally precedes closed-input EOF.
    pub(crate) fn splice_to(
        self,
        out: PipeEndpoint<'a>,
        len: usize,
        nonblocking: bool,
    ) -> AxResult<usize> {
        if !self.is_read() || !out.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        if len == 0 {
            return Ok(0);
        }
        if self.state_key() == out.state_key() {
            return Err(AxError::InvalidInput);
        }

        struct SplicePoll<'a> {
            src: PipeEndpoint<'a>,
            dst: PipeEndpoint<'a>,
        }

        impl Pollable for SplicePoll<'_> {
            fn poll(&self) -> IoEvents {
                let mut events = IoEvents::empty();
                let source = self.src.buffer().lock();
                events.set(IoEvents::READABLE, source.occupied_len() > 0);
                drop(source);
                let destination = self.dst.buffer().lock();
                events.set(IoEvents::WRITABLE, pipe_poll_writable(&destination));
                events
            }

            fn register<'b>(
                &'b self,
                context: &mut Context<'_>,
                events: IoEvents,
            ) -> Result<axpoll::PollRegistration<'b>, axpoll::PollRegistrationError> {
                let read = events.contains(IoEvents::READABLE);
                let write = events.contains(IoEvents::WRITABLE);
                let mut prepared =
                    axpoll::PreparedPollRegistration::try_new(2 + read as usize + write as usize)?;
                if read {
                    prepared.arm(self.src.poll_rx(), context.waker())?;
                }
                if write {
                    prepared.arm(self.dst.poll_tx(), context.waker())?;
                }
                prepared.arm(self.src.poll_close(), context.waker())?;
                prepared.arm(self.dst.poll_close(), context.waker())?;
                prepared.commit()
            }
        }

        let poller = SplicePoll {
            src: self,
            dst: out,
        };
        let mut total_moved = 0usize;
        block_on_poll_io(
            &poller,
            IoEvents::READABLE | IoEvents::WRITABLE,
            nonblocking,
            || {
                let _transaction = self.read_transaction().lock();
                if out.destination_closed() {
                    if total_moved > 0 {
                        return Ok(total_moved);
                    }
                    raise_pipe();
                    return Err(AxError::BrokenPipe);
                }
                let remaining = len - total_moved;
                if remaining == 0 {
                    return Ok(total_moved);
                }

                let source_first = self.state_key() < out.state_key();
                let (moved, source_wakeup, source_empty, destination_full) = if source_first {
                    let mut source = self.buffer().lock();
                    let was_writable = pipe_poll_writable(&source);
                    let mut destination = out.buffer().lock();
                    let source_empty = source.occupied_len() == 0;
                    let destination_full = destination.vacant_len() == 0;
                    let count = remaining
                        .min(source.occupied_len())
                        .min(destination.vacant_len());
                    let moved = move_pipe_buffer(&mut source, &mut destination, count);
                    let source_wakeup = PipeTransfer {
                        len: moved.len,
                        wake_readers: false,
                        became_writable: !was_writable && pipe_poll_writable(&source),
                    };
                    (moved, source_wakeup, source_empty, destination_full)
                } else {
                    let mut destination = out.buffer().lock();
                    let mut source = self.buffer().lock();
                    let was_writable = pipe_poll_writable(&source);
                    let source_empty = source.occupied_len() == 0;
                    let destination_full = destination.vacant_len() == 0;
                    let count = remaining
                        .min(source.occupied_len())
                        .min(destination.vacant_len());
                    let moved = move_pipe_buffer(&mut source, &mut destination, count);
                    let source_wakeup = PipeTransfer {
                        len: moved.len,
                        wake_readers: false,
                        became_writable: !was_writable && pipe_poll_writable(&source),
                    };
                    (moved, source_wakeup, source_empty, destination_full)
                };

                if moved.len == 0 {
                    return blocked_pipe_transfer_result(
                        total_moved,
                        source_empty,
                        self.source_closed(),
                        destination_full,
                    );
                }
                self.notify_writable(source_wakeup);
                out.notify_readable(moved);
                total_moved += moved.len;
                Ok(total_moved)
            },
        )
    }
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
            read_transaction: Mutex::new(()),
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
            !thr.has_effective_capability(CAP_SYS_RESOURCE) && new_size > pipe_capacity_limit()
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

        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let _transaction = self.shared.read_transaction.lock();
            let read = read_pipe_buffer(&self.shared.buffer, dst)?;
            if read.len > 0 {
                notify_pipe_writable(&self.shared.poll_tx, read);
                Ok(read.len)
            } else if self.closed() {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            }
        })
    }

    pub fn vmsplice_write(&self, src: &mut IoSrc, nonblocking: bool) -> AxResult<usize> {
        if !self.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        if src.remaining() == 0 {
            return Ok(0);
        }

        block_on_poll_io(self, IoEvents::WRITABLE, nonblocking, || {
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
        })
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
                events.set(IoEvents::READABLE, src.occupied_len() > 0);
                drop(src);
                let dst = self.dst.shared.buffer.lock();
                events.set(IoEvents::WRITABLE, pipe_poll_writable(&dst));
                events
            }

            fn register<'a>(
                &'a self,
                context: &mut Context<'_>,
                events: IoEvents,
            ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
                let read = events.contains(IoEvents::READABLE);
                let write = events.contains(IoEvents::WRITABLE);
                let mut prepared =
                    axpoll::PreparedPollRegistration::try_new(2 + read as usize + write as usize)?;
                if read {
                    prepared.arm(&self.src.shared.poll_rx, context.waker())?;
                }
                if write {
                    prepared.arm(&self.dst.shared.poll_tx, context.waker())?;
                }
                prepared.arm(&self.src.shared.poll_close, context.waker())?;
                prepared.arm(&self.dst.shared.poll_close, context.waker())?;
                prepared.commit()
            }
        }

        let poller = TeePoll {
            src: self,
            dst: out,
        };
        let mut total_copied = 0usize;
        block_on_poll_io(
            &poller,
            IoEvents::READABLE | IoEvents::WRITABLE,
            nonblocking,
            || {
                let _transaction = self.shared.read_transaction.lock();
                if out.closed() {
                    if total_copied > 0 {
                        return Ok(total_copied);
                    }
                    raise_pipe();
                    return Err(AxError::BrokenPipe);
                }
                let remaining = len - total_copied;
                if remaining == 0 {
                    return Ok(total_copied);
                }

                // tee and splice share the same address order so concurrent
                // opposite-direction operations cannot form an ABBA cycle.
                let source_first = Arc::as_ptr(&self.shared) < Arc::as_ptr(&out.shared);
                let (written, source_empty, destination_full) = if source_first {
                    let source = self.shared.buffer.lock();
                    let mut destination = out.shared.buffer.lock();
                    let source_empty = source.occupied_len() == 0;
                    let destination_full = destination.vacant_len() == 0;
                    let count = remaining
                        .min(source.occupied_len())
                        .min(destination.vacant_len());
                    (
                        copy_pipe_buffer(&source, &mut destination, count),
                        source_empty,
                        destination_full,
                    )
                } else {
                    let mut destination = out.shared.buffer.lock();
                    let source = self.shared.buffer.lock();
                    let source_empty = source.occupied_len() == 0;
                    let destination_full = destination.vacant_len() == 0;
                    let count = remaining
                        .min(source.occupied_len())
                        .min(destination.vacant_len());
                    (
                        copy_pipe_buffer(&source, &mut destination, count),
                        source_empty,
                        destination_full,
                    )
                };
                if written.len == 0 {
                    return blocked_pipe_transfer_result(
                        total_copied,
                        source_empty,
                        self.closed(),
                        destination_full,
                    );
                }
                notify_pipe_readable(&out.shared.poll_rx, &out.shared.async_io, written);
                total_copied += written.len;
                // Linux returns an available prefix instead of waiting to fill
                // the caller's entire requested length.
                Ok(total_copied)
            },
        )
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
    pub(crate) fn location(&self) -> &Location {
        &self.location
    }

    pub(crate) fn open(location: Location, flags: u32) -> AxResult<Self> {
        let access = PipeAccess::from_flags(flags)?;
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
            block_on_poll_io(&waiter, IoEvents::READABLE, false, || {
                if state.writer_count() > 0 {
                    Ok(())
                } else {
                    Err(AxError::WouldBlock)
                }
            })
        } else if access.waits_for_reader(nonblocking) {
            block_on_poll_io(&waiter, IoEvents::READABLE, false, || {
                if state.reader_count() > 0 {
                    Ok(())
                } else {
                    Err(AxError::WouldBlock)
                }
            })
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

    fn fifo_path(&self) -> AxResult<Cow<'_, str>> {
        let path = self.location.absolute_path()?;
        Ok(Cow::Owned(try_owned_path(path.as_str())?))
    }

    pub(crate) fn set_async_io(&self, enabled: bool, state: AsyncIoState, fd: i32) {
        if self.access.can_read() {
            *self.state.async_io.lock() = PipeAsyncIo { enabled, state, fd };
        }
    }

    pub(crate) const fn is_read(&self) -> bool {
        self.access.can_read()
    }

    pub(crate) const fn is_write(&self) -> bool {
        self.access.can_write()
    }

    pub(crate) fn same_pipe(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub(crate) fn read_with_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
    ) -> AxResult<usize> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let _transaction = self.state.read_transaction.lock();
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
        })
    }

    pub(crate) fn write_with_nonblocking(
        &self,
        src: &mut IoSrc,
        nonblocking: bool,
    ) -> AxResult<usize> {
        if !self.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        let size = src.remaining();
        if size == 0 {
            return Ok(0);
        }

        let atomic_len = (size <= PIPE_BUF_SIZE).then_some(size);
        let mut total_written = 0;
        block_on_poll_io(self, IoEvents::WRITABLE, nonblocking, || {
            if self.state.reader_count() == 0 {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = write_pipe_buffer(&self.state.buffer, src, atomic_len)?;
            if written.len > 0 {
                self.state.poll_rx.wake();
                notify_async_readable(&self.state.async_io);
                total_written += written.len;
                if pipe_write_is_complete(total_written, size, nonblocking) {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        })
    }

    pub(crate) fn splice_read_with(
        &self,
        dst: &mut [u8],
        nonblocking: bool,
        mut write: impl FnMut(&[u8]) -> AxResult<usize>,
    ) -> AxResult<(usize, bool)> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_empty() {
            return Ok((0, false));
        }

        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            self.try_splice_read_with(dst, &mut write)
        })
    }

    fn try_splice_read_with(
        &self,
        dst: &mut [u8],
        write: &mut impl FnMut(&[u8]) -> AxResult<usize>,
    ) -> AxResult<(usize, bool)> {
        let _transaction = self.state.read_transaction.lock();
        transfer_pipe_prefix(
            dst,
            |dst| {
                let source = self.state.buffer.lock();
                reserve_pipe_prefix(&source, dst, self.state.writer_count() == 0)
            },
            write,
            |written, reservation| {
                let source = self.state.buffer.lock();
                let transfer = commit_pipe_prefix(&source, written, reservation)?;
                drop(source);
                if transfer.became_writable {
                    self.state.poll_tx.wake();
                }
                Ok(())
            },
        )
    }
}

impl Pipe {
    pub(crate) fn read_with_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
    ) -> AxResult<usize> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let _transaction = self.shared.read_transaction.lock();
            let read = read_pipe_buffer(&self.shared.buffer, dst)?;
            if read.len > 0 {
                notify_pipe_writable(&self.shared.poll_tx, read);
                Ok(read.len)
            } else if self.closed() {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            }
        })
    }

    pub(crate) fn write_with_nonblocking(
        &self,
        src: &mut IoSrc,
        nonblocking: bool,
    ) -> AxResult<usize> {
        if !self.is_write() {
            return Err(AxError::BadFileDescriptor);
        }
        let size = src.remaining();
        if size == 0 {
            return Ok(0);
        }

        let atomic_len = (size <= PIPE_BUF_SIZE).then_some(size);
        let mut total_written = 0;
        block_on_poll_io(self, IoEvents::WRITABLE, nonblocking, || {
            if self.closed() {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = write_pipe_buffer(&self.shared.buffer, src, atomic_len)?;
            if written.len > 0 {
                notify_pipe_readable(&self.shared.poll_rx, &self.shared.async_io, written);
                total_written += written.len;
                if pipe_write_is_complete(total_written, size, nonblocking) {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        })
    }

    /// Offers one pipe prefix to a destination and consumes exactly the bytes
    /// the destination accepted.
    ///
    /// A source-consumer transaction pins the offered prefix, but the ring
    /// lock itself is released before `write`. This preserves commit-after-
    /// destination semantics without forming a lock-order cycle with an
    /// arbitrary destination. Pipe to pipe moves use [`Self::splice_to`]
    /// instead to give both rings one direct transaction.
    pub(crate) fn splice_read_with(
        &self,
        dst: &mut [u8],
        nonblocking: bool,
        mut write: impl FnMut(&[u8]) -> AxResult<usize>,
    ) -> AxResult<(usize, bool)> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_empty() {
            return Ok((0, false));
        }

        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            self.try_splice_read_with(dst, &mut write)
        })
    }

    fn try_splice_read_with(
        &self,
        dst: &mut [u8],
        write: &mut impl FnMut(&[u8]) -> AxResult<usize>,
    ) -> AxResult<(usize, bool)> {
        let _transaction = self.shared.read_transaction.lock();
        transfer_pipe_prefix(
            dst,
            |dst| {
                let source = self.shared.buffer.lock();
                reserve_pipe_prefix(&source, dst, self.closed())
            },
            write,
            |written, reservation| {
                let source = self.shared.buffer.lock();
                let transfer = commit_pipe_prefix(&source, written, reservation)?;
                drop(source);
                notify_pipe_writable(&self.shared.poll_tx, transfer);
                Ok(())
            },
        )
    }
}

impl FileLike for Pipe {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let nonblocking = self.nonblocking();
        self.read_with_nonblocking(dst, nonblocking)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let nonblocking = self.nonblocking();
        self.write_with_nonblocking(src, nonblocking)
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.shared.inode.stat())
    }

    fn update_timestamps(
        &self,
        atime: Option<core::time::Duration>,
        mtime: Option<core::time::Duration>,
        ctime: core::time::Duration,
    ) -> AxResult<()> {
        self.shared.inode.update_timestamps(atime, mtime, ctime);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        try_pseudo_inode_path("pipe", self.shared.inode.inode())
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn ioctl(&self, _context: &IoctlContext, cmd: u32, _arg: usize) -> AxResult<usize> {
        match cmd {
            FIONREAD => Ok(self.shared.buffer.lock().occupied_len()),
            _ => Err(AxError::NotATty),
        }
    }
}

impl Pollable for Pipe {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let buf = self.shared.buffer.lock();
        if self.read_side {
            events.set(IoEvents::READABLE, buf.occupied_len() > 0);
            events.set(IoEvents::HANGUP, self.closed());
        } else {
            events.set(IoEvents::WRITABLE, pipe_poll_writable(&buf));
            events.set(IoEvents::ERROR, self.closed());
        }
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        let read = events.contains(IoEvents::READABLE);
        let write = events.contains(IoEvents::WRITABLE);
        let mut prepared =
            axpoll::PreparedPollRegistration::try_new(1 + read as usize + write as usize)?;
        if read {
            prepared.arm(&self.shared.poll_rx, context.waker())?;
        }
        if write {
            prepared.arm(&self.shared.poll_tx, context.waker())?;
        }
        prepared.arm(&self.shared.poll_close, context.waker())?;
        prepared.commit()
    }
}

impl FileLike for NamedPipe {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let nonblocking = self.nonblocking();
        self.read_with_nonblocking(dst, nonblocking)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let nonblocking = self.nonblocking();
        self.write_with_nonblocking(src, nonblocking)
    }

    fn stat(&self) -> AxResult<Kstat> {
        location_to_kstat(&self.location)
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        self.fifo_path()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn ioctl(&self, _context: &IoctlContext, cmd: u32, _arg: usize) -> AxResult<usize> {
        match cmd {
            FIONREAD => Ok(self.state.buffer.lock().occupied_len()),
            _ => Err(AxError::NotATty),
        }
    }
}

impl Pollable for NamedPipe {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let buf = self.state.buffer.lock();
        if self.access.can_read() {
            events.set(IoEvents::READABLE, buf.occupied_len() > 0);
            events.set(IoEvents::HANGUP, self.state.writer_count() == 0);
        }
        if self.access.can_write() {
            events.set(
                IoEvents::WRITABLE,
                self.state.reader_count() > 0 && pipe_poll_writable(&buf),
            );
            events.set(IoEvents::ERROR, self.state.reader_count() == 0);
        }
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        let read = self.access.can_read() && events.contains(IoEvents::READABLE);
        let write = self.access.can_write() && events.contains(IoEvents::WRITABLE);
        let open = (self.access.can_read() && events.contains(IoEvents::HANGUP))
            || self.access.can_write();
        let mut prepared = axpoll::PreparedPollRegistration::try_new(
            read as usize + write as usize + open as usize,
        )?;
        if read {
            prepared.arm(&self.state.poll_rx, context.waker())?;
        }
        if write {
            prepared.arm(&self.state.poll_tx, context.waker())?;
        }
        if open {
            prepared.arm(&self.state.poll_open, context.waker())?;
        }
        prepared.commit()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake, vec::Vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    use axio::{IoBuf, Read};

    use super::*;

    struct SliceSource {
        bytes: &'static [u8],
        position: usize,
    }

    impl Read for SliceSource {
        fn read(&mut self, destination: &mut [u8]) -> axio::Result<usize> {
            let source = &self.bytes[self.position..];
            let copied = source.len().min(destination.len());
            destination[..copied].copy_from_slice(&source[..copied]);
            self.position += copied;
            Ok(copied)
        }
    }

    impl IoBuf for SliceSource {
        fn remaining(&self) -> usize {
            self.bytes.len() - self.position
        }
    }

    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn fifo_access_mode_three_is_rejected_instead_of_granting_both_ends() {
        assert!(matches!(
            PipeAccess::from_flags(O_ACCMODE | O_NONBLOCK),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            PipeAccess::from_flags(O_RDONLY),
            Ok(PipeAccess::Read)
        ));
        assert!(matches!(
            PipeAccess::from_flags(O_WRONLY),
            Ok(PipeAccess::Write)
        ));
        assert!(matches!(
            PipeAccess::from_flags(O_RDWR),
            Ok(PipeAccess::ReadWrite)
        ));
    }

    #[test]
    fn one_write_uses_one_nonblocking_snapshot() {
        assert!(!pipe_write_is_complete(1, 2, false));
        assert!(pipe_write_is_complete(1, 2, true));
        assert!(pipe_write_is_complete(2, 2, false));
    }

    #[test]
    fn appending_to_a_readable_pipe_wakes_a_rearmed_edge_waiter() {
        let buffer = Mutex::new(HeapRb::new(8));
        assert_eq!(buffer.lock().push_slice(b"old"), 3);

        let poll_rx = PollSet::new();
        let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let _registration = poll_rx.register(&waker).unwrap();

        let mut source = SliceSource {
            bytes: b"new",
            position: 0,
        };
        let transfer = write_pipe_buffer(&buffer, &mut source, Some(3)).unwrap();
        assert_eq!(transfer.len, 3);
        assert!(transfer.wake_readers);
        assert_eq!(buffer.lock().occupied_len(), 6);

        notify_pipe_readable(&poll_rx, &Mutex::new(PipeAsyncIo::default()), transfer);
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn pipe_move_consumes_only_the_destination_prefix() {
        let mut source = HeapRb::new(8);
        assert_eq!(source.push_slice(b"abcdef"), 6);
        let mut destination = HeapRb::new(2);

        let moved = move_pipe_buffer(&mut source, &mut destination, 6);
        assert_eq!(moved.len, 2);
        assert!(moved.wake_readers);

        let (left, right) = source.as_slices();
        let remaining = left.iter().chain(right).copied().collect::<Vec<_>>();
        assert_eq!(remaining, b"cdef");
        let (left, right) = destination.as_slices();
        let accepted = left.iter().chain(right).copied().collect::<Vec<_>>();
        assert_eq!(accepted, b"ab");
    }

    #[test]
    fn pipe_transfer_releases_the_ring_before_destination_admission() {
        let source = spin::Mutex::new(HeapRb::new(8));
        source.lock().push_slice(b"abcd");
        let mut scratch = [0u8; 4];

        let (written, short) = transfer_pipe_prefix(
            &mut scratch,
            |dst| {
                let source = source.lock();
                reserve_pipe_prefix(&source, dst, false)
            },
            &mut |data| {
                assert_eq!(data, b"abcd");
                assert!(source.try_lock().is_some());
                Ok(2)
            },
            |written, reservation| {
                let source = source.lock();
                commit_pipe_prefix(&source, written, reservation).map(drop)
            },
        )
        .unwrap();

        assert_eq!(written, 2);
        assert!(short);
        assert_eq!(source.lock().occupied_len(), 2);
    }

    #[test]
    fn pipe_transfer_does_not_commit_a_destination_would_block() {
        let source = spin::Mutex::new(HeapRb::new(8));
        source.lock().push_slice(b"abcd");
        let mut scratch = [0u8; 4];
        let mut commit_called = false;

        let result = transfer_pipe_prefix(
            &mut scratch,
            |dst| {
                let source = source.lock();
                reserve_pipe_prefix(&source, dst, false)
            },
            &mut |_data| Err(AxError::WouldBlock),
            |_written, _reservation| {
                commit_called = true;
                Ok(())
            },
        );

        assert_eq!(result, Err(AxError::WouldBlock));
        assert!(!commit_called);
        assert_eq!(source.lock().occupied_len(), 4);
    }

    #[test]
    fn pipe_transfer_returns_progress_and_checks_output_before_eof() {
        assert_eq!(blocked_pipe_transfer_result(3, true, true, true), Ok(3));
        assert_eq!(
            blocked_pipe_transfer_result(0, true, true, true),
            Err(AxError::WouldBlock)
        );
        assert_eq!(blocked_pipe_transfer_result(0, true, true, false), Ok(0));
    }
}
