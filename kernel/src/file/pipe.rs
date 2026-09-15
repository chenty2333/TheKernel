use alloc::{borrow::Cow, sync::Arc};
use core::{
    cmp::min,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::Location;
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::{
    general::{
        CAP_SYS_RESOURCE, O_ACCMODE, O_DIRECT, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, POLL_IN,
    },
    ioctl::FIONREAD,
};
use memory_addr::PAGE_SIZE_4K;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer},
};
use tk_linux_signal::{SignalInfo, Signo};

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

static PIPE_MAX_SIZE: AtomicUsize = AtomicUsize::new(1024 * 1024);

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

/// One Linux `struct pipe_buffer`, as far as a byte ring can describe it.
///
/// Linux stores pipe payload in a ring of page-backed `struct pipe_buffer`
/// objects, each carrying its own `flags` word (`include/linux/pipe_fs_i.h:26-32`).
/// TheKernel keeps the payload in a byte ring, so a buffer is described by its
/// byte extent in the pipe's absolute stream position plus that flags word.
/// Only packetized buffers need an entry: a byte no entry covers belongs to a
/// `PIPE_BUF_FLAG_CAN_MERGE` buffer, and `read(2)` treats those as one
/// undelimited byte stream, which is exactly what the byte ring already is.
#[derive(Clone, Copy)]
struct PipeBufferMark {
    /// Absolute stream position of the buffer's first byte (`buf->offset`).
    start: u64,
    /// `buf->len`.
    len: usize,
    /// `buf->flags`.
    flags: u8,
}

/// `PIPE_BUF_FLAG_PACKET`: `read(2)` returns this buffer whole
/// (`include/linux/pipe_fs_i.h:10`).  `anon_pipe_write()` stamps it on every
/// buffer a descriptor with `O_DIRECT` creates (`fs/pipe.c:631-634`).
const PIPE_BUF_FLAG_PACKET: u8 = 0x08;
/// `PIPE_BUF_FLAG_WHOLE`: `read(2)` must return the whole buffer or `-ENOBUFS`
/// (`include/linux/pipe_fs_i.h:12`).  Its only producer is `watch_queue`
/// (`kernel/watch_queue.c:132`), and `pipe2(O_NOTIFICATION_PIPE)` reports
/// `-ENOPKG` on this build, so no buffer here can carry it.  The read branch
/// models the rule regardless.
const PIPE_BUF_FLAG_WHOLE: u8 = 0x20;

/// The pipe payload plus the per-buffer flags Linux keeps beside it.
///
/// `bytes.capacity()` is Linux's `pipe->max_usage * PAGE_SIZE`: `F_GETPIPE_SZ`
/// reports it and `pipe_set_size()` sizes it (`fs/pipe.c:1466-1501`).  `marks`
/// therefore holds at most `max_usage` entries, which is the count
/// `pipe_full(head, tail, pipe->max_usage)` tests (`include/linux/pipe_fs_i.h`).
/// A stream write fills both rings without an entry, because Linux writes such
/// data into mergeable buffers whose only externally visible property is that
/// `read(2)` may cross them.
struct PipeRing {
    /// Payload bytes, in pipe order.
    bytes: HeapRb<u8>,
    /// One entry per packetized buffer, in pipe order.
    marks: HeapRb<PipeBufferMark>,
    /// Absolute stream position of the byte ring's write index.
    write_pos: u64,
    /// Absolute stream position of the byte ring's read index.  A packet mark
    /// is expressed against the same index, so the offsets survive an
    /// `F_SETPIPE_SZ` capacity change, which moves no bytes.
    read_pos: u64,
}

impl PipeRing {
    /// Linux `pipe->max_usage`: the buffer array is sized in `PAGE_SIZE` slots
    /// (`fs/pipe.c:1440-1446`), and `pipe->max_usage * PAGE_SIZE` is the
    /// capacity `F_GETPIPE_SZ` reports.
    fn max_usage(capacity: usize) -> usize {
        // `round_pipe_size()` never returns less than one page, and
        // `alloc_pipe_info()` starts at `PIPE_DEF_BUFFERS`, so this floor only
        // keeps degenerate unit-test capacities usable.
        (capacity / PIPE_BUF_SIZE).max(1)
    }

    fn new(capacity: usize) -> Self {
        Self {
            bytes: HeapRb::new(capacity),
            marks: HeapRb::new(Self::max_usage(capacity)),
            write_pos: 0,
            read_pos: 0,
        }
    }

    fn try_new(capacity: usize) -> Result<Self, alloc::collections::TryReserveError> {
        Ok(Self {
            bytes: HeapRb::try_new(capacity)?,
            marks: HeapRb::try_new(Self::max_usage(capacity))?,
            write_pos: 0,
            read_pos: 0,
        })
    }

    fn capacity(&self) -> usize {
        self.bytes.capacity().get()
    }

    fn occupied_len(&self) -> usize {
        self.bytes.occupied_len()
    }

    fn vacant_len(&self) -> usize {
        self.bytes.vacant_len()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Free `pipe_buffer` slots; `pipe_full()` is this reaching zero.
    /// Buffer slots the pipe has allocated, Linux `pipe_occupancy(head, tail)`:
    /// one per packetized buffer plus one per page a trailing stream region
    /// spans.
    fn occupied_buffers(&self) -> usize {
        self.marks.occupied_len() + self.tail_stream_len().div_ceil(PIPE_BUF_SIZE)
    }

    /// Buffer slots still vacant, Linux `pipe->max_usage - pipe_occupancy()`.
    fn vacant_buffers(&self) -> usize {
        self.marks
            .capacity()
            .get()
            .saturating_sub(self.occupied_buffers())
    }

    /// Bytes at the read index that no packetized buffer covers.
    ///
    /// Linux walks those as `PIPE_BUF_FLAG_CAN_MERGE` buffers and continues
    /// into the next buffer until the caller's count is exhausted
    /// (`fs/pipe.c:413-459`), so the only boundary a stream prefix exposes is
    /// the packet that follows it.
    fn head_stream_len(&self) -> usize {
        match self.marks.try_peek() {
            Some(mark) => (mark.start - self.read_pos).min(self.occupied_len() as u64) as usize,
            None => self.occupied_len(),
        }
    }

    /// The packetized buffer starting exactly at the read index, as
    /// `(buf->len, buf->flags)`.
    fn head_packet(&self) -> Option<(usize, u8)> {
        self.marks
            .try_peek()
            .filter(|mark| mark.start == self.read_pos)
            .map(|mark| (mark.len, mark.flags))
    }

    /// Bytes after the last packetized buffer: the mergeable tail.
    fn tail_stream_len(&self) -> usize {
        match self.marks.last() {
            Some(mark) => (self.write_pos - mark.start - mark.len as u64) as usize,
            None => self.occupied_len(),
        }
    }

    /// Consumes `count` bytes from the read index.
    ///
    /// Linux keeps a partly read stream buffer in place (`buf->offset += chars;
    /// buf->len -= chars`) and releases a buffer whose `len` reaches zero
    /// (`fs/pipe.c:442-453`).  A splice from a pipe copies the same way, so a
    /// packet buffer can be trimmed; `read(2)` never leaves one behind because
    /// it zeroes `buf->len` unconditionally.
    fn advance_read(&mut self, count: usize) {
        unsafe { self.bytes.advance_read_index(count) };
        self.read_pos += count as u64;
        while self
            .marks
            .try_peek()
            .is_some_and(|mark| mark.start + mark.len as u64 <= self.read_pos)
        {
            let _ = self.marks.try_pop();
        }
        if let Some(mark) = self.marks.first_mut()
            && mark.start < self.read_pos
        {
            mark.len -= (self.read_pos - mark.start) as usize;
            mark.start = self.read_pos;
        }
    }

    /// Copies up to `max_len` bytes from `src` into the vacant region and
    /// advances the byte write index.
    fn push_source(&mut self, src: &mut IoSrc, max_len: usize) -> AxResult<usize> {
        let (left, right) = self.bytes.vacant_slices_mut();
        // The ring buffer exposes valid writable byte slices here.
        let left = unsafe {
            core::slice::from_raw_parts_mut(left.as_mut_ptr().cast::<u8>(), left.len())
        };
        let right = unsafe {
            core::slice::from_raw_parts_mut(right.as_mut_ptr().cast::<u8>(), right.len())
        };
        let left_len = left.len().min(max_len);
        let mut count = src.read(&mut left[..left_len])?;
        if count == left_len && count < max_len {
            let right_len = right.len().min(max_len - count);
            count += src.read(&mut right[..right_len])?;
        }
        unsafe { self.bytes.advance_write_index(count) };
        Ok(count)
    }

    /// Accounts for `count` bytes just copied in as mergeable stream data.
    fn commit_stream(&mut self, count: usize) {
        self.write_pos += count as u64;
    }

    /// Accounts for `count` bytes just copied in as one packetized buffer.
    fn commit_packet(&mut self, count: usize, flags: u8) {
        // Refused only when every slot is taken, which `pipe_full()` rejects
        // before the bytes are copied in.
        let _ = self.marks.try_push(PipeBufferMark {
            start: self.write_pos,
            len: count,
            flags,
        });
        self.write_pos += count as u64;
    }
}

/// Linux `pipe_full(head, tail, pipe->max_usage)`: every buffer slot is taken.
fn pipe_ring_full(ring: &PipeRing) -> bool {
    ring.vacant_buffers() == 0
}

fn pipe_poll_writable(ring: &PipeRing) -> bool {
    ring.vacant_len() >= PIPE_BUF_SIZE && !pipe_ring_full(ring)
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

/// Bytes a stream write can add before Linux runs out of pipe buffers
/// (`fs/pipe.c:572-591`, `:598-641`).
///
/// The merge step accepts `chars = total_len & (PAGE_SIZE-1)` bytes into the
/// tail buffer's page without allocating anything, and every loop iteration
/// after it allocates exactly one buffer and copies at most `PAGE_SIZE` into
/// it.  A write that finds no free buffer and merges nothing reports a partial
/// transfer (the merged bytes) or `-EAGAIN` when it has none, so byte capacity
/// alone over-admits as soon as the marks ring holds buffers: one packet, one
/// slot, one page.
fn stream_write_room(ring: &PipeRing, total_len: usize) -> usize {
    let mut merge_room = 0;
    let tail = ring.tail_stream_len();
    if tail > 0 {
        let tail_used = match tail % PIPE_BUF_SIZE {
            0 => PIPE_BUF_SIZE,
            remainder => remainder,
        };
        let chars = total_len & (PIPE_BUF_SIZE - 1);
        if tail_used + chars <= PIPE_BUF_SIZE {
            merge_room = chars;
        }
    }
    merge_room + ring.vacant_buffers() * PIPE_BUF_SIZE
}

/// `anon_pipe_write()` without `is_packetized(filp)` (`fs/pipe.c:522-699`).
///
/// Linux admits one buffer per iteration and lets a later write append to the
/// last mergeable buffer inside its page, so a byte ring reproduces the
/// externally visible result: an undelimited stream whose only admission limit
/// is `pipe->max_usage * PAGE_SIZE`.
fn write_pipe_buffer(
    buffer: &Mutex<PipeRing>,
    src: &mut IoSrc,
    atomic_len: Option<usize>,
) -> AxResult<PipeTransfer> {
    let mut ring = buffer.lock();
    let room = ring.vacant_len().min(stream_write_room(&ring, src.remaining()));
    if atomic_len.is_some_and(|len| room < len) {
        return Ok(PipeTransfer::none());
    }
    let max_len = atomic_len.unwrap_or(room).min(room);
    let count = ring.push_source(src, max_len)?;
    ring.commit_stream(count);
    Ok(PipeTransfer {
        len: count,
        wake_readers: count > 0,
        became_writable: false,
    })
}

/// `anon_pipe_write()` with `is_packetized(filp)` true (`fs/pipe.c:507-510`,
/// `:598-640`).
///
/// The merge step runs first and only accepts the previous buffer when it
/// still carries `PIPE_BUF_FLAG_CAN_MERGE`:
///     chars = total_len & (PAGE_SIZE-1);
///     if (chars && !was_empty) {
///             ... if ((buf->flags & PIPE_BUF_FLAG_CAN_MERGE) &&
///                     offset + chars <= PAGE_SIZE) { ... }
///     }
/// A packetized buffer never carries that flag, so a packetized write only
/// merges its first `total_len & (PAGE_SIZE-1)` bytes into the tail of an
/// earlier *stream* write.  Every loop iteration then allocates one page,
/// copies at most `PAGE_SIZE`, and stamps the buffer:
///     if (is_packetized(filp))
///             buf->flags = PIPE_BUF_FLAG_PACKET;
/// A write therefore becomes `ceil(len / PAGE_SIZE)` whole packets, and the
/// pipe lock is held for the whole loop, so a packet is never split across
/// writers.
fn write_pipe_packets(buffer: &Mutex<PipeRing>, src: &mut IoSrc) -> AxResult<PipeTransfer> {
    let mut ring = buffer.lock();
    // `was_empty = pipe_empty(head, tail)` is sampled under the pipe lock, so
    // the merge decision cannot observe another writer's buffer.
    let was_empty = ring.is_empty();
    let mut written = 0usize;

    // `chars = total_len & (PAGE_SIZE-1)` merge step.
    let merge_chars = src.remaining() & (PIPE_BUF_SIZE - 1);
    if merge_chars > 0 && !was_empty {
        let tail = ring.tail_stream_len();
        if tail > 0 {
            // `offset + buf->len` of the tail buffer.  A stream tail is a whole
            // number of full pages plus its last partial page, so the used part
            // of that page is the remainder, or a whole page when the region
            // ends exactly on a page boundary.
            let tail_used = match tail % PIPE_BUF_SIZE {
                0 => PIPE_BUF_SIZE,
                remainder => remainder,
            };
            let merge_len = merge_chars.min(ring.vacant_len());
            if tail_used + merge_len <= PIPE_BUF_SIZE {
                let count = ring.push_source(src, merge_len)?;
                ring.commit_stream(count);
                written += count;
            }
        }
    }

    while src.remaining() > 0 {
        // `while (!pipe_full(head, tail, pipe->max_usage))`
        if pipe_ring_full(&ring) {
            break;
        }
        let chunk = src.remaining().min(PIPE_BUF_SIZE);
        if ring.vacant_len() < chunk {
            break;
        }
        let count = ring.push_source(src, chunk)?;
        if count == 0 {
            break;
        }
        ring.commit_packet(count, PIPE_BUF_FLAG_PACKET);
        written += count;
        if count < chunk {
            // `copy_page_from_iter()` returned a partial page with data left in
            // the source; Linux drops the page and reports -EFAULT.  A byte
            // ring cannot retract the bytes, so the partial packet is the
            // whole source and the write ends here.
            break;
        }
    }

    Ok(PipeTransfer {
        len: written,
        wake_readers: written > 0,
        became_writable: false,
    })
}

/// Copies exactly the leading `len` bytes of the ring into `dst`.
fn copy_ring_prefix(ring: &PipeRing, len: usize, dst: &mut IoDst) -> AxResult<usize> {
    let (left, right) = ring.bytes.as_slices();
    let left_len = left.len().min(len);
    let mut count = dst.write(&left[..left_len])?;
    if count == left_len && len > left_len {
        count += dst.write(&right[..len - left_len])?;
    }
    Ok(count)
}

/// `anon_pipe_read()` (`fs/pipe.c:360-497`).
///
/// The walk is buffer by buffer: mergeable buffers contribute `buf->len` bytes
/// and the loop continues into the next buffer, while a packetized buffer ends
/// the read by construction:
///     if (buf->flags & PIPE_BUF_FLAG_PACKET) {
///             total_len = chars;
///             buf->len = 0;
///     }
/// so a user count smaller than the packet truncates it and discards the
/// remainder, and a full packet still stops the read after one buffer.
fn read_pipe_buffer(buffer: &Mutex<PipeRing>, dst: &mut IoDst) -> AxResult<PipeTransfer> {
    let mut ring = buffer.lock();
    let was_writable = pipe_poll_writable(&ring);
    let mut total_len = dst.remaining_mut();
    let mut read = 0usize;

    // Stream prefix: `chars = buf->len`, then `chars = total_len` when the
    // caller asked for less, and the loop continues to the next buffer.
    let stream_len = ring.head_stream_len();
    if stream_len > 0 && total_len > 0 {
        let chars = stream_len.min(total_len);
        let written = copy_ring_prefix(&ring, chars, dst)?;
        ring.advance_read(written);
        read += written;
        total_len -= written;
    }

    if total_len > 0
        && let Some((len, flags)) = ring.head_packet()
    {
        if len > total_len && flags & PIPE_BUF_FLAG_WHOLE != 0 {
            // `if (buf->flags & PIPE_BUF_FLAG_WHOLE) { if (ret == 0) ret =
            // -ENOBUFS; break; }` leaves the buffer untouched, and the break
            // returns whatever the earlier stream buffers contributed.
            if read == 0 {
                return Err(LinuxError::ENOBUFS.into());
            }
        } else {
            let chars = len.min(total_len);
            let written = copy_ring_prefix(&ring, chars, dst)?;
            read += written;
            if written < chars {
                // `if (written < chars) { if (!ret) ret = -EFAULT; break; }`
                // happens before `buf->offset` moves, so only what the
                // destination accepted leaves the pipe.
                ring.advance_read(written);
            } else {
                // `buf->len = 0` releases the whole packet, including the bytes
                // the caller's count could not take.
                ring.advance_read(len);
            }
        }
    }

    Ok(PipeTransfer {
        len: read,
        wake_readers: false,
        became_writable: !was_writable && pipe_poll_writable(&ring),
    })
}

#[derive(Clone, Copy)]
struct PipeReadReservation {
    available: usize,
    was_writable: bool,
}

/// Moves the destination prefix of `src` into `dst`.
fn move_pipe_buffer(src: &mut PipeRing, dst: &mut PipeRing, max_len: usize) -> PipeTransfer {
    let (left, right) = src.bytes.as_slices();
    let written = copy_slices_to_ring(&mut dst.bytes, &[left, right], max_len);
    dst.commit_stream(written);
    // `written` came from the currently occupied source slices and therefore
    // cannot exceed the initialized prefix owned by the consumer.
    src.advance_read(written);
    PipeTransfer {
        len: written,
        wake_readers: written > 0,
        became_writable: false,
    }
}

/// Copies `max_len` bytes of the current source prefix into `dst`.
fn copy_pipe_buffer(src: &PipeRing, dst: &mut PipeRing, max_len: usize) -> PipeTransfer {
    let (left, right) = src.bytes.as_slices();
    let written = copy_slices_to_ring(&mut dst.bytes, &[left, right], max_len);
    dst.commit_stream(written);
    PipeTransfer {
        len: written,
        wake_readers: written > 0,
        became_writable: false,
    }
}

/// Reads the pipe as an undelimited byte stream, ignoring buffer flags.
///
/// `splice_from_pipe_feed()` is the read side of `splice(2)` from a pipe and of
/// `vmsplice(2)`: it copies `min(buf->len, sd->total_len)` from each buffer in
/// turn and stops only when the caller's count is exhausted
/// (`fs/splice.c:442-490`), so `PIPE_BUF_FLAG_PACKET` has no effect there.
fn splice_read_pipe_buffer(buffer: &Mutex<PipeRing>, dst: &mut IoDst) -> AxResult<PipeTransfer> {
    let mut ring = buffer.lock();
    let was_writable = pipe_poll_writable(&ring);
    let count = copy_ring_prefix(&ring, ring.occupied_len().min(dst.remaining_mut()), dst)?;
    ring.advance_read(count);
    Ok(PipeTransfer {
        len: count,
        wake_readers: false,
        became_writable: !was_writable && pipe_poll_writable(&ring),
    })
}

fn reserve_pipe_prefix(
    source: &PipeRing,
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

    let (left, right) = source.bytes.as_slices();
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
    source: &mut PipeRing,
    written: usize,
    reservation: PipeReadReservation,
) -> AxResult<PipeTransfer> {
    if source.occupied_len() < written {
        return Err(AxError::BadState);
    }
    source.advance_read(written);
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
    buffer: Mutex<PipeRing>,
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
    buffer: Mutex<PipeRing>,
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
            buffer: Mutex::new(PipeRing::new(default_pipe_capacity())),
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

impl Pollable for PipeEndpoint<'_> {
    fn poll(&self) -> IoEvents {
        match self {
            Self::Anonymous(pipe) => pipe.poll(),
            Self::Named(pipe) => pipe.poll(),
        }
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        match self {
            Self::Anonymous(pipe) => pipe.register(context, events),
            Self::Named(pipe) => pipe.register(context, events),
        }
    }
}

impl<'a> PipeEndpoint<'a> {
    pub(crate) fn from_file(file: &'a dyn FileLike) -> Option<Self> {
        file.downcast_ref::<Pipe>()
            .map(Self::Anonymous)
            .or_else(|| file.downcast_ref::<NamedPipe>().map(Self::Named))
    }

    pub fn capacity(&self) -> usize {
        self.buffer().lock().capacity()
    }

    pub fn resize(&self, requested_size: usize) -> AxResult<usize> {
        let new_size = round_pipe_size(requested_size)?;

        if current().try_as_thread().is_some_and(|thr| {
            !thr.has_effective_capability(CAP_SYS_RESOURCE) && new_size > pipe_capacity_limit()
        }) {
            return Err(AxError::OperationNotPermitted);
        }

        let mut buffer = self.buffer().lock();
        if new_size == buffer.capacity() {
            return Ok(new_size);
        }
        // `pipe_resize_ring()` refuses a buffer array smaller than the number of
        // allocated buffers, not smaller than the bytes they hold:
        //     if (nr_slots < pipe_occupancy(pipe->head, pipe->tail))
        //             return -EBUSY;
        // (`fs/pipe.c:1389-1412`).  A stream region occupies one slot per page
        // it spans and each packetized buffer occupies one.
        let occupied_buffers = buffer.occupied_buffers();
        if PipeRing::max_usage(new_size) < occupied_buffers {
            return Err(AxError::ResourceBusy);
        }
        if new_size < buffer.occupied_len() {
            return Err(AxError::ResourceBusy);
        }
        // `pipe_resize_ring()` copies the buffers into the new array and only
        // rebases the ring indices, so the payload keeps its order and its
        // flags while the absolute stream positions stay meaningful.
        let mut replacement = PipeRing::try_new(new_size).map_err(|_| AxError::NoMemory)?;
        replacement.read_pos = buffer.read_pos;
        replacement.write_pos = buffer.write_pos;
        let (left, right) = buffer.bytes.as_slices();
        replacement.bytes.push_slice(left);
        replacement.bytes.push_slice(right);
        let (marks, wrapped) = buffer.marks.as_slices();
        for mark in marks.iter().chain(wrapped) {
            // The replacement ring is checked to have at least as many slots as
            // there are occupied buffers, so no push can be refused.
            let _ = replacement.marks.try_push(*mark);
        }
        *buffer = replacement;
        drop(buffer);
        self.poll_tx().wake();
        Ok(new_size)
    }

    pub fn vmsplice_read(&self, dst: &mut IoDst, nonblocking: bool) -> AxResult<usize> {
        if !self.is_read() {
            return Err(AxError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let _transaction = self.read_transaction().lock();
            let read = splice_read_pipe_buffer(&self.buffer(), dst)?;
            if read.len > 0 {
                notify_pipe_writable(&self.poll_tx(), read);
                Ok(read.len)
            } else if self.source_closed() {
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
            if self.destination_closed() {
                raise_pipe();
                return Err(AxError::BrokenPipe);
            }

            let written = write_pipe_buffer(&self.buffer(), src, None)?;
            if written.len > 0 {
                notify_pipe_readable(&self.poll_rx(), &self.async_io(), written);
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
        if self.state_key() == out.state_key() {
            return Err(AxError::InvalidInput);
        }

        struct TeePoll<'a> {
            src: PipeEndpoint<'a>,
            dst: PipeEndpoint<'a>,
        }

        impl Pollable for TeePoll<'_> {
            fn poll(&self) -> IoEvents {
                let mut events = IoEvents::empty();
                let src = self.src.buffer().lock();
                events.set(IoEvents::READABLE, src.occupied_len() > 0);
                drop(src);
                let dst = self.dst.buffer().lock();
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
                    prepared.arm(&self.src.poll_rx(), context.waker())?;
                }
                if write {
                    prepared.arm(&self.dst.poll_tx(), context.waker())?;
                }
                prepared.arm(&self.src.poll_close(), context.waker())?;
                prepared.arm(&self.dst.poll_close(), context.waker())?;
                prepared.commit()
            }
        }

        let poller = TeePoll {
            src: *self,
            dst: *out,
        };
        let mut total_copied = 0usize;
        block_on_poll_io(
            &poller,
            IoEvents::READABLE | IoEvents::WRITABLE,
            nonblocking,
            || {
                let _transaction = self.read_transaction().lock();
                if out.destination_closed() {
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
                let source_first = self.state_key() < out.state_key();
                let (written, source_empty, destination_full) = if source_first {
                    let source = self.buffer().lock();
                    let mut destination = out.buffer().lock();
                    let source_empty = source.occupied_len() == 0;
                    let destination_full =
                        destination.vacant_len() == 0 || pipe_ring_full(&destination);
                    let count = remaining
                        .min(source.occupied_len())
                        .min(destination.vacant_len());
                    (
                        copy_pipe_buffer(&source, &mut destination, count),
                        source_empty,
                        destination_full,
                    )
                } else {
                    let mut destination = out.buffer().lock();
                    let source = self.buffer().lock();
                    let source_empty = source.occupied_len() == 0;
                    let destination_full =
                        destination.vacant_len() == 0 || pipe_ring_full(&destination);
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
                        self.source_closed(),
                        destination_full,
                    );
                }
                notify_pipe_readable(&out.poll_rx(), &out.async_io(), written);
                total_copied += written.len;
                // Linux returns an available prefix instead of waiting to fill
                // the caller's entire requested length.
                Ok(total_copied)
            },
        )
    }

    pub(crate) fn is_read(self) -> bool {
        match self {
            Self::Anonymous(pipe) => pipe.is_read(),
            Self::Named(pipe) => pipe.is_read(),
        }
    }

    pub(crate) fn is_write(self) -> bool {
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

    fn buffer(self) -> &'a Mutex<PipeRing> {
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
                    let destination_full =
                        destination.vacant_len() == 0 || pipe_ring_full(&destination);
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
                    let destination_full =
                        destination.vacant_len() == 0 || pipe_ring_full(&destination);
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
            buffer: Mutex::new(PipeRing::new(default_pipe_capacity())),
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
        PipeEndpoint::Anonymous(self).capacity()
    }
    pub fn resize(&self, size: usize) -> AxResult<usize> {
        PipeEndpoint::Anonymous(self).resize(size)
    }

    pub(crate) fn set_async_io(&self, enabled: bool, state: AsyncIoState, fd: i32) {
        if self.is_read() {
            *self.shared.async_io.lock() = PipeAsyncIo { enabled, state, fd };
        }
    }

    pub fn vmsplice_read(&self, dst: &mut IoDst, nonblocking: bool) -> AxResult<usize> {
        PipeEndpoint::Anonymous(self).vmsplice_read(dst, nonblocking)
    }
    pub fn vmsplice_write(&self, src: &mut IoSrc, nonblocking: bool) -> AxResult<usize> {
        PipeEndpoint::Anonymous(self).vmsplice_write(src, nonblocking)
    }
    pub fn tee_to(&self, out: &Self, len: usize, nonblocking: bool) -> AxResult<usize> {
        PipeEndpoint::Anonymous(self).tee_to(&PipeEndpoint::Anonymous(out), len, nonblocking)
    }
}

pub(crate) fn pipe_max_size() -> usize {
    pipe_capacity_limit()
}

pub(crate) fn set_pipe_max_size(size: usize) -> AxResult<()> {
    PIPE_MAX_SIZE.store(round_pipe_size(size)?, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn raise_sigpipe_for_current() {
    let curr = current();
    let _ = send_signal_to_process(
        curr.as_thread().proc_data.proc.pid(),
        Some(SignalInfo::new_kernel(Signo::SIGPIPE)),
    );
}

fn raise_pipe() {
    raise_sigpipe_for_current();
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

        // `open(2)` of a FIFO with O_DIRECT is the one O_DIRECT-on-a-pipe case
        // Linux refuses: `do_dentry_open()` requires FMODE_CAN_ODIRECT
        // (fs/open.c:966-968), which comes from the file's own `->open` (e.g.
        // `ext4_file_open()`, fs/ext4/file.c:937) or from
        // `f_mapping->a_ops->direct_IO` (fs/open.c:961-962), and `fifo_open()`
        // (fs/pipe.c) grants neither.  Packetized mode on a FIFO is reachable
        // through `F_SETFL`, whose O_DIRECT admission exempts
        // `S_ISFIFO(inode->i_mode)` explicitly (fs/fcntl.c:61-65), and through
        // `pipe2(O_DIRECT)` for anonymous pipes (fs/pipe.c:1042-1056).
        //
        // The rejection comes after the open, not before it: `do_dentry_open()`
        // runs `f_op->open` first (fs/open.c:946-951) and computes
        // `FMODE_CAN_ODIRECT` only afterwards (fs/open.c:961), so a blocking
        // FIFO open waits for its peer and only then fails with -EINVAL.
        if flags & O_DIRECT != 0 {
            state.remove_access(access);
            return Err(AxError::InvalidInput);
        }

        Ok(Self {
            access,
            location,
            state,
            non_blocking: AtomicBool::new(nonblocking),
        })
    }

    fn fifo_path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        let path = self.location.absolute_path()?;
        Ok(Cow::Owned(try_owned_path(&path)?))
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
            let read = read_pipe_buffer(&self.state.buffer, dst)?;
            if read.len > 0 {
                self.state.poll_tx.wake();
                Ok(read.len)
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
        suppress_sigpipe: bool,
        packetized: bool,
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
        let result = block_on_poll_io(self, IoEvents::WRITABLE, nonblocking, || {
            if self.state.reader_count() == 0 {
                if !suppress_sigpipe {
                    raise_pipe();
                }
                return Err(AxError::BrokenPipe);
            }

            let written = if packetized {
                write_pipe_packets(&self.state.buffer, src)?
            } else {
                write_pipe_buffer(&self.state.buffer, src, atomic_len)?
            };
            if written.len > 0 {
                self.state.poll_rx.wake();
                notify_async_readable(&self.state.async_io);
                total_written += written.len;
                if pipe_write_is_complete(total_written, size, nonblocking) {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        });
        result.or_else(|error| {
            if total_written > 0 {
                Ok(total_written)
            } else {
                Err(error)
            }
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
                let mut source = self.state.buffer.lock();
                let transfer = commit_pipe_prefix(&mut source, written, reservation)?;
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

    /// `suppress_sigpipe` is set by callers that report `-EPIPE` instead of
    /// raising `SIGPIPE`, the way `splice_direct_to_actor()` does.
    ///
    /// `packetized` is `is_packetized(filp)` of the *open file description*
    /// being written (`fs/pipe.c:507-510`), sampled by the caller because the
    /// flag lives in `f_flags` and an `F_SETFL` may have changed it since the
    /// pipe was opened.  Every write(2) path passes `status.direct()`; the
    /// transfer paths pass `false` because `splice_to_pipe()` never stamps
    /// `PIPE_BUF_FLAG_PACKET` (`fs/splice.c:223`).
    pub(crate) fn write_with_nonblocking(
        &self,
        src: &mut IoSrc,
        nonblocking: bool,
        suppress_sigpipe: bool,
        packetized: bool,
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
        let result = block_on_poll_io(self, IoEvents::WRITABLE, nonblocking, || {
            if self.closed() {
                if !suppress_sigpipe {
                    raise_pipe();
                }
                return Err(AxError::BrokenPipe);
            }

            let written = if packetized {
                write_pipe_packets(&self.shared.buffer, src)?
            } else {
                write_pipe_buffer(&self.shared.buffer, src, atomic_len)?
            };
            if written.len > 0 {
                notify_pipe_readable(&self.shared.poll_rx, &self.shared.async_io, written);
                total_written += written.len;
                if pipe_write_is_complete(total_written, size, nonblocking) {
                    return Ok(total_written);
                }
            }
            Err(AxError::WouldBlock)
        });
        result.or_else(|error| {
            if total_written > 0 {
                Ok(total_written)
            } else {
                Err(error)
            }
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
                let mut source = self.shared.buffer.lock();
                let transfer = commit_pipe_prefix(&mut source, written, reservation)?;
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
        // This hook is not a `->write_iter` caller, so it never packetizes:
        // `is_packetized(filp)` is sampled from the open file description by
        // `write_file_like_with_status()`, which every write(2) path uses.
        self.write_with_nonblocking(src, nonblocking, false, false)
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.shared.inode.stat())
    }

    fn update_timestamps(
        &self,
        atime: Option<axfs_ng_vfs::Timestamp>,
        mtime: Option<axfs_ng_vfs::Timestamp>,
        ctime: axfs_ng_vfs::Timestamp,
    ) -> AxResult<()> {
        self.shared.inode.update_timestamps(atime, mtime, ctime);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
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
        // This hook is not a `->write_iter` caller, so it never packetizes:
        // `is_packetized(filp)` is sampled from the open file description by
        // `write_file_like_with_status()`, which every write(2) path uses.
        self.write_with_nonblocking(src, nonblocking, false, false)
    }

    fn stat(&self) -> AxResult<Kstat> {
        location_to_kstat(&self.location)
    }

    fn vfs_location(&self) -> Option<&axfs_ng_vfs::Location> {
        Some(&self.location)
    }

    // A FIFO has no file page-cache data of its own, but Linux cachestat(2)
    // still authorizes the query against the backing inode.  Returning this
    // location preserves its exact mount-idmap and LSM/DAC provenance while
    // the default FileLike cachestat snapshot remains empty.
    fn cachestat_location(&self) -> Option<&axfs_ng_vfs::Location> {
        Some(&self.location)
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
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
    fn fifo_endpoints_share_capacity_vmsplice_and_tee_with_anonymous_pipes() {
        let _context = crate::test_support::scheduler_test_context();
        let fs = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let root = axfs_ng_vfs::Mountpoint::new_root(&fs).root_location();
        let location = root
            .create(
                axfs_ng_vfs::FsName::new(b"fifo"),
                axfs_ng_vfs::NodeType::Fifo,
                axfs_ng_vfs::NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        // fs/open.c:966-968: O_DIRECT needs FMODE_CAN_ODIRECT, which
        // `fifo_open()` never grants.
        assert!(matches!(
            NamedPipe::open(location.clone(), O_RDWR | O_DIRECT),
            Err(AxError::InvalidInput)
        ));
        let fifo = NamedPipe::open(location, O_RDWR | O_NONBLOCK).unwrap();
        let endpoint = PipeEndpoint::from_file(&fifo).unwrap();
        assert_eq!(endpoint.resize(4096), Ok(4096));
        assert_eq!(endpoint.capacity(), 4096);
        let mut source = SliceSource {
            bytes: b"fifo",
            position: 0,
        };
        assert_eq!(endpoint.vmsplice_write(&mut source, true), Ok(4));
        let (reader, writer) = Pipe::new();
        assert_eq!(
            endpoint.tee_to(&PipeEndpoint::Anonymous(&writer), 4, true),
            Ok(4)
        );
        assert_eq!(fifo.state.buffer.lock().occupied_len(), 4);
        assert_eq!(reader.shared.buffer.lock().occupied_len(), 4);
    }

    #[test]
    fn anonymous_hangup_is_visible_before_drain_without_read_hangup() {
        let (reader, writer) = Pipe::new();
        writer.shared.buffer.lock().bytes.push_slice(b"pending");
        drop(writer);
        assert!(
            reader
                .poll()
                .contains(IoEvents::READABLE | IoEvents::HANGUP)
        );
        assert!(!reader.poll().contains(IoEvents::READ_HANGUP));
    }

    #[test]
    fn pipe_capacity_rounding_matches_linux_power_of_two_pages() {
        assert_eq!(round_pipe_size(1), Ok(PAGE_SIZE_4K));
        assert_eq!(round_pipe_size(PAGE_SIZE_4K * 3), Ok(PAGE_SIZE_4K * 4));
    }

    #[test]
    fn blocked_large_write_returns_progress_when_the_reader_closes() {
        let _context = crate::test_support::scheduler_test_context();
        struct ClosingSource {
            reader: Option<Pipe>,
            remaining: usize,
        }
        impl IoBuf for ClosingSource {
            fn remaining(&self) -> usize {
                self.remaining
            }
        }
        impl Read for ClosingSource {
            fn read(&mut self, dst: &mut [u8]) -> axio::Result<usize> {
                let count = self.remaining.min(dst.len());
                dst[..count].fill(7);
                self.remaining -= count;
                if count > 0 {
                    drop(self.reader.take());
                }
                Ok(count)
            }
        }
        let (reader, writer) = Pipe::new();
        let capacity = writer.capacity();
        let mut source = ClosingSource {
            reader: Some(reader),
            remaining: capacity * 2,
        };
        assert_eq!(
            writer.write_with_nonblocking(&mut source, false, true, false),
            Ok(capacity)
        );
        assert_eq!(writer.shared.buffer.lock().occupied_len(), capacity);
    }

    #[test]
    fn blocking_vmsplice_returns_an_available_prefix() {
        let (reader, writer) = Pipe::new();
        let capacity = writer.capacity();
        let mut source = SliceSource {
            bytes: &[1; RING_BUFFER_INIT_SIZE * 2],
            position: 0,
        };
        assert_eq!(writer.vmsplice_write(&mut source, false), Ok(capacity));
        assert_eq!(reader.shared.buffer.lock().occupied_len(), capacity);
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
        let buffer = Mutex::new(PipeRing::new(8));
        assert_eq!(buffer.lock().bytes.push_slice(b"old"), 3);

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
    fn a_stream_write_stops_at_the_last_free_pipe_buffer() {
        // One byte per packet: sixteen packets fill a sixteen-slot pipe long
        // before its byte capacity is used, and Linux's `anon_pipe_write()`
        // loop allocates at most one buffer per page of a stream write
        // (fs/pipe.c:598-641).  A stream write into that state must therefore
        // stop after the one buffer that is still free, not after the 65521
        // bytes of ring capacity.
        let mut ring = PipeRing::new(PIPE_BUF_SIZE * 16);
        for _ in 0..16 {
            assert_eq!(ring.bytes.push_slice(b"x"), 1);
            ring.commit_packet(1, PIPE_BUF_FLAG_PACKET);
        }
        assert_eq!(ring.tail_stream_len(), 0);
        assert_eq!(stream_write_room(&ring, PIPE_BUF_SIZE), 0);
        assert_eq!(stream_write_room(&ring, PIPE_BUF_SIZE * 2), 0);

        // Draining one packet frees one slot: a stream write may now add a
        // single page, and a page-aligned request cannot merge into a packet.
        ring.advance_read(1);
        assert_eq!(stream_write_room(&ring, PIPE_BUF_SIZE * 2), PIPE_BUF_SIZE);

        // A trailing stream region merges `total_len & (PAGE_SIZE-1)` bytes
        // into its partial page and allocates one page per remaining slot.
        let mut ring = PipeRing::new(PIPE_BUF_SIZE * 4);
        assert_eq!(ring.bytes.push_slice(&[b's'; 100]), 100);
        ring.commit_stream(100);
        assert_eq!(stream_write_room(&ring, PIPE_BUF_SIZE + 7), 7 + 3 * PIPE_BUF_SIZE);
        assert_eq!(stream_write_room(&ring, PIPE_BUF_SIZE * 8), 3 * PIPE_BUF_SIZE);
    }

    #[test]
    fn pipe_move_consumes_only_the_destination_prefix() {
        let mut source = PipeRing::new(8);
        assert_eq!(source.bytes.push_slice(b"abcdef"), 6);
        source.commit_stream(6);
        let mut destination = PipeRing::new(2);

        let moved = move_pipe_buffer(&mut source, &mut destination, 6);
        assert_eq!(moved.len, 2);
        assert!(moved.wake_readers);

        let (left, right) = source.bytes.as_slices();
        let remaining = left.iter().chain(right).copied().collect::<Vec<_>>();
        assert_eq!(remaining, b"cdef");
        let (left, right) = destination.bytes.as_slices();
        let accepted = left.iter().chain(right).copied().collect::<Vec<_>>();
        assert_eq!(accepted, b"ab");
    }

    #[test]
    fn pipe_transfer_releases_the_ring_before_destination_admission() {
        let source = spin::Mutex::new(PipeRing::new(8));
        source.lock().bytes.push_slice(b"abcd");
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
                let mut source = source.lock();
                commit_pipe_prefix(&mut source, written, reservation).map(drop)
            },
        )
        .unwrap();

        assert_eq!(written, 2);
        assert!(short);
        assert_eq!(source.lock().occupied_len(), 2);
    }

    #[test]
    fn pipe_transfer_does_not_commit_a_destination_would_block() {
        let source = spin::Mutex::new(PipeRing::new(8));
        source.lock().bytes.push_slice(b"abcd");
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
