use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{mem::size_of, sync::atomic::{AtomicBool, Ordering}, task::Context};

use axerrno::{AxError, AxResult};
use axhal::paging::PageSize;
use axpoll::{IoEvents, Pollable};
use bytemuck::AnyBitPattern;
use spin::Mutex;
use starry_vm::VmPtr;

use crate::{
    file::{File, FileLike},
    mm::{IoVec, SharedPages, UserConstPtr, VmBytesMut},
    syscall::{sys_sendmsg},
};

const IORING_OFF_SQ_RING: usize = 0;
const IORING_OFF_CQ_RING: usize = 0x0800_0000;
const IORING_OFF_SQES: usize = 0x1000_0000;

const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
const IORING_REGISTER_BUFFERS: u32 = 0;
const IORING_UNREGISTER_BUFFERS: u32 = 1;

const IOSQE_IO_DRAIN: u8 = 1 << 1;
const IOSQE_ASYNC: u8 = 1 << 4;

const IORING_OP_READ_FIXED: u8 = 4;
const IORING_OP_SENDMSG: u8 = 9;

const SQ_RING_ARRAY_OFFSET: usize = 64;
const CQ_RING_CQES_OFFSET: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct IoSqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub resv2: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct IoCqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub resv: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct IoUringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: IoSqRingOffsets,
    pub cq_off: IoCqRingOffsets,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IoUringCqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoUringSqePersonality {
    buf_index: u16,
    personality: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
union IoUringSqeTail {
    person: IoUringSqePersonality,
    pad2: [u64; 3],
}

impl Default for IoUringSqeTail {
    fn default() -> Self {
        Self { pad2: [0; 3] }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoUringSqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    tail: IoUringSqeTail,
}

#[derive(Clone, Copy)]
struct RegisteredBuffer {
    base: usize,
    len: usize,
}

struct IoUringState {
    params: IoUringParams,
    sq_ring: Arc<SharedPages>,
    cq_ring: Arc<SharedPages>,
    sqes: Arc<SharedPages>,
    registered_buffers: Vec<RegisteredBuffer>,
}

pub struct IoUringFile {
    state: Mutex<IoUringState>,
    nonblocking: AtomicBool,
}

impl IoUringState {
    fn new(entries: u32) -> AxResult<Self> {
        if entries == 0 {
            return Err(AxError::InvalidInput);
        }
        let entries = entries.next_power_of_two();
        let params = IoUringParams {
            sq_entries: entries,
            cq_entries: entries,
            sq_off: IoSqRingOffsets {
                head: 0,
                tail: 4,
                ring_mask: 8,
                ring_entries: 12,
                flags: 16,
                dropped: 20,
                array: SQ_RING_ARRAY_OFFSET as u32,
                ..IoSqRingOffsets::default()
            },
            cq_off: IoCqRingOffsets {
                head: 0,
                tail: 4,
                ring_mask: 8,
                ring_entries: 12,
                overflow: 16,
                cqes: CQ_RING_CQES_OFFSET as u32,
                ..IoCqRingOffsets::default()
            },
            ..IoUringParams::default()
        };

        let sq_ring_len = (SQ_RING_ARRAY_OFFSET + entries as usize * size_of::<u32>()).next_multiple_of(PageSize::Size4K as usize);
        let cq_ring_len = (CQ_RING_CQES_OFFSET + entries as usize * size_of::<IoUringCqe>()).next_multiple_of(PageSize::Size4K as usize);
        let sqes_len = (entries as usize * size_of::<IoUringSqe>()).next_multiple_of(PageSize::Size4K as usize);

        let sq_ring = Arc::new(SharedPages::new(sq_ring_len, PageSize::Size4K)?);
        let cq_ring = Arc::new(SharedPages::new(cq_ring_len, PageSize::Size4K)?);
        let sqes = Arc::new(SharedPages::new(sqes_len, PageSize::Size4K)?);

        let state = Self {
            params,
            sq_ring,
            cq_ring,
            sqes,
            registered_buffers: Vec::new(),
        };
        state.write_sq_u32(state.params.sq_off.ring_mask as usize, entries - 1)?;
        state.write_sq_u32(state.params.sq_off.ring_entries as usize, entries)?;
        state.write_cq_u32(state.params.cq_off.ring_mask as usize, entries - 1)?;
        state.write_cq_u32(state.params.cq_off.ring_entries as usize, entries)?;
        Ok(state)
    }

    fn sq_ring_len(&self) -> usize {
        self.sq_ring.total_bytes()
    }

    fn cq_ring_len(&self) -> usize {
        self.cq_ring.total_bytes()
    }

    fn sqes_len(&self) -> usize {
        self.sqes.total_bytes()
    }

    fn read_sq_u32(&self, offset: usize) -> AxResult<u32> {
        let mut buf = [0u8; 4];
        self.sq_ring.read_bytes(offset, &mut buf)?;
        Ok(u32::from_ne_bytes(buf))
    }

    fn write_sq_u32(&self, offset: usize, value: u32) -> AxResult {
        self.sq_ring.write_bytes(offset, &value.to_ne_bytes())
    }

    fn read_cq_u32(&self, offset: usize) -> AxResult<u32> {
        let mut buf = [0u8; 4];
        self.cq_ring.read_bytes(offset, &mut buf)?;
        Ok(u32::from_ne_bytes(buf))
    }

    fn write_cq_u32(&self, offset: usize, value: u32) -> AxResult {
        self.cq_ring.write_bytes(offset, &value.to_ne_bytes())
    }

    fn read_sqe(&self, index: u32) -> AxResult<IoUringSqe> {
        let offset = index as usize * size_of::<IoUringSqe>();
        let mut buf = [0u8; size_of::<IoUringSqe>()];
        self.sqes.read_bytes(offset, &mut buf)?;
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr().cast::<IoUringSqe>()) })
    }

    fn write_cqe(&self, index: u32, cqe: IoUringCqe) -> AxResult {
        let offset = self.params.cq_off.cqes as usize + index as usize * size_of::<IoUringCqe>();
        let bytes = unsafe {
            core::slice::from_raw_parts((&cqe as *const IoUringCqe).cast::<u8>(), size_of::<IoUringCqe>())
        };
        self.cq_ring.write_bytes(offset, bytes)
    }

    fn submit_one(&self, sqe: &IoUringSqe) -> IoUringCqe {
        let res = match sqe.opcode {
            IORING_OP_READ_FIXED => self.do_read_fixed(sqe),
            IORING_OP_SENDMSG => self.do_sendmsg(sqe),
            _ => Err(AxError::Unsupported),
        };
        IoUringCqe {
            user_data: sqe.user_data,
            res: res.unwrap_or_else(|err| -(axerrno::LinuxError::from(err).code() as i32)),
            flags: 0,
        }
    }

    fn do_read_fixed(&self, sqe: &IoUringSqe) -> AxResult<i32> {
        let iov = self
            .registered_buffers
            .get(unsafe { sqe.tail.person.buf_index } as usize)
            .ok_or(AxError::InvalidInput)?;
        let len = (sqe.len as usize).min(iov.len);
        let file = File::from_fd(sqe.fd)?;
        let read = file
            .inner()
            .read_at(VmBytesMut::new(iov.base as *mut u8, len), sqe.off)?;
        Ok(read as i32)
    }

    fn do_sendmsg(&self, sqe: &IoUringSqe) -> AxResult<i32> {
        let _ = sqe.flags & IOSQE_IO_DRAIN;
        let _ = sqe.flags & IOSQE_ASYNC;
        let ret = sys_sendmsg(sqe.fd, UserConstPtr::from(sqe.addr as usize), sqe.op_flags)?;
        Ok(ret as i32)
    }
}

impl IoUringFile {
    pub fn new(entries: u32) -> AxResult<Arc<Self>> {
        Ok(Arc::new(Self {
            state: Mutex::new(IoUringState::new(entries)?),
            nonblocking: AtomicBool::new(false),
        }))
    }

    pub fn params(&self) -> IoUringParams {
        self.state.lock().params
    }

    pub fn register_buffers(&self, arg: *const IoVec, nr_args: u32) -> AxResult<isize> {
        let mut buffers = Vec::with_capacity(nr_args as usize);
        for i in 0..nr_args as usize {
            let iov = arg.wrapping_add(i).vm_read()?;
            if iov.iov_len < 0 {
                return Err(AxError::InvalidInput);
            }
            buffers.push(RegisteredBuffer {
                base: iov.iov_base as usize,
                len: iov.iov_len as usize,
            });
        }
        self.state.lock().registered_buffers = buffers;
        Ok(0)
    }

    pub fn unregister_buffers(&self) -> AxResult<isize> {
        self.state.lock().registered_buffers.clear();
        Ok(0)
    }

    pub fn enter(&self, to_submit: u32, _min_complete: u32, flags: u32) -> AxResult<isize> {
        if flags & !IORING_ENTER_GETEVENTS != 0 {
            return Err(AxError::InvalidInput);
        }

        let state = self.state.lock();
        let sq_head = state.read_sq_u32(state.params.sq_off.head as usize)?;
        let sq_tail = state.read_sq_u32(state.params.sq_off.tail as usize)?;
        let sq_mask = state.read_sq_u32(state.params.sq_off.ring_mask as usize)?;
        let pending = sq_tail.saturating_sub(sq_head);
        let submit = pending.min(to_submit);

        let mut cq_tail = state.read_cq_u32(state.params.cq_off.tail as usize)?;
        let cq_mask = state.read_cq_u32(state.params.cq_off.ring_mask as usize)?;

        for idx in 0..submit {
            let array_slot = ((sq_head + idx) & sq_mask) as usize;
            let sqe_index = state.read_sq_u32(state.params.sq_off.array as usize + array_slot * size_of::<u32>())?;
            let sqe = state.read_sqe(sqe_index)?;
            let cqe = state.submit_one(&sqe);
            state.write_cqe(cq_tail & cq_mask, cqe)?;
            cq_tail = cq_tail.wrapping_add(1);
        }

        state.write_sq_u32(state.params.sq_off.head as usize, sq_head.wrapping_add(submit))?;
        state.write_cq_u32(state.params.cq_off.tail as usize, cq_tail)?;

        Ok(submit as isize)
    }

    pub fn map_region(&self, offset: usize) -> Option<(Arc<SharedPages>, usize)> {
        let state = self.state.lock();
        match offset {
            IORING_OFF_SQ_RING => Some((state.sq_ring.clone(), state.sq_ring_len())),
            IORING_OFF_CQ_RING => Some((state.cq_ring.clone(), state.cq_ring_len())),
            IORING_OFF_SQES => Some((state.sqes.clone(), state.sqes_len())),
            _ => None,
        }
    }
}

impl FileLike for IoUringFile {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[io_uring]".into()
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }
}

impl Pollable for IoUringFile {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}
