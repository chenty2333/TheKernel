use alloc::{
    borrow::Cow,
    collections::VecDeque,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::future::{block_on, poll_io};
use bytemuck::AnyBitPattern;
use linux_raw_sys::{general::{O_CLOEXEC, O_NONBLOCK}, ioctl::{UFFDIO_API, UFFDIO_COPY, UFFDIO_REGISTER}};
use memory_addr::{MemoryAddr, VirtAddr, PAGE_SIZE_4K};
use spin::Mutex;
use starry_process::Pid;
use starry_vm::{VmMutPtr, VmPtr, vm_read_slice};

use crate::{
    file::{FileLike, IoDst},
};

const UFFD_API_VERSION: u64 = 0xAA;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 0;
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1;
const UFFDIO_COPY_MODE_DONTWAKE: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct UffdApi {
    pub api: u64,
    pub features: u64,
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct UffdRange {
    pub start: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct UffdRegister {
    pub range: UffdRange,
    pub mode: u64,
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct UffdCopy {
    pub dst: u64,
    pub src: u64,
    pub len: u64,
    pub mode: u64,
    pub copy: i64,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct UffdMsg {
    event: u8,
    reserved1: u8,
    reserved2: u16,
    reserved3: u32,
    flags: u64,
    address: u64,
    feat: u64,
}

#[derive(Clone)]
struct RegisteredRange {
    start: usize,
    len: usize,
}

struct PendingFault {
    page: usize,
    flags: u64,
    resolved: bool,
    data: Option<Vec<u8>>,
}

struct UserfaultState {
    pid: Pid,
    ranges: Vec<RegisteredRange>,
    pending: Vec<PendingFault>,
    queue: VecDeque<UffdMsg>,
}

pub struct UserfaultFile {
    state: Mutex<UserfaultState>,
    nonblocking: AtomicBool,
    poll_rx: PollSet,
}

static USERFAULT_FILES: Mutex<Vec<Weak<UserfaultFile>>> = Mutex::new(Vec::new());

impl UserfaultFile {
    pub fn new(pid: Pid, nonblocking: bool) -> Arc<Self> {
        let file = Arc::new(Self {
            state: Mutex::new(UserfaultState {
                pid,
                ranges: Vec::new(),
                pending: Vec::new(),
                queue: VecDeque::new(),
            }),
            nonblocking: AtomicBool::new(nonblocking),
            poll_rx: PollSet::new(),
        });
        USERFAULT_FILES.lock().push(Arc::downgrade(&file));
        file
    }

    fn api_bits() -> u64 {
        (1u64 << 0) | (1u64 << 3) | (1u64 << 63)
    }

    pub fn ioctl_api(&self, arg: *mut UffdApi) -> AxResult<usize> {
        let Some(arg) = arg.nullable() else {
            return Err(AxError::BadAddress);
        };
        let mut api = arg.vm_read()?;
        if api.api != UFFD_API_VERSION {
            return Err(AxError::InvalidInput);
        }
        api.features = 0;
        api.ioctls = Self::api_bits();
        arg.vm_write(api)?;
        Ok(0)
    }

    pub fn ioctl_register(&self, arg: *mut UffdRegister) -> AxResult<usize> {
        let Some(arg) = arg.nullable() else {
            return Err(AxError::BadAddress);
        };
        let mut reg = arg.vm_read()?;
        if reg.mode != UFFDIO_REGISTER_MODE_MISSING {
            return Err(AxError::Unsupported);
        }
        let start = reg.range.start as usize;
        let len = reg.range.len as usize;
        if len == 0 || !start.is_aligned_4k() || !len.is_aligned_4k() {
            return Err(AxError::InvalidInput);
        }

        let mut state = self.state.lock();
        state.ranges.push(RegisteredRange { start, len });
        reg.ioctls = (1u64 << 3) | (1u64 << 0);
        arg.vm_write(reg)?;
        Ok(0)
    }

    pub fn ioctl_copy(&self, arg: *mut UffdCopy) -> AxResult<usize> {
        let Some(arg) = arg.nullable() else {
            return Err(AxError::BadAddress);
        };
        let mut copy = arg.vm_read()?;
        if copy.len == 0 || !(copy.dst as usize).is_aligned_4k() || !(copy.len as usize).is_aligned_4k() {
            return Err(AxError::InvalidInput);
        }
        if copy.mode & !UFFDIO_COPY_MODE_DONTWAKE != 0 {
            return Err(AxError::Unsupported);
        }

        let mut data = vec![0u8; copy.len as usize];
        vm_read_slice(copy.src as *const u8, unsafe {
            core::slice::from_raw_parts_mut(
                data.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                data.len(),
            )
        })?;

        let mut state = self.state.lock();
        let pending = state
            .pending
            .iter_mut()
            .find(|pending| pending.page == copy.dst as usize)
            .ok_or(AxError::InvalidInput)?;
        pending.data = Some(data);
        pending.resolved = true;
        copy.copy = copy.len as i64;
        arg.vm_write(copy)?;
        Ok(0)
    }

    fn has_events(&self) -> bool {
        !self.state.lock().queue.is_empty()
    }

    fn begin_fault(&self, pid: Pid, addr: VirtAddr, write: bool) -> bool {
        let page = addr.align_down_4k().as_usize();
        let mut state = self.state.lock();
        if state.pid != pid {
            return false;
        }
        if !state
            .ranges
            .iter()
            .any(|range| page >= range.start && page < range.start + range.len)
        {
            return false;
        }
        if state.pending.iter().any(|pending| pending.page == page) {
            return true;
        }

        let flags = if write { UFFD_PAGEFAULT_FLAG_WRITE } else { 0 };
        state.pending.push(PendingFault {
            page,
            flags,
            resolved: false,
            data: None,
        });
        state.queue.push_back(UffdMsg {
            event: UFFD_EVENT_PAGEFAULT,
            flags,
            address: page as u64,
            ..UffdMsg::default()
        });
        self.poll_rx.wake();
        true
    }

    fn take_resolved(&self, page: usize) -> Option<Vec<u8>> {
        let mut state = self.state.lock();
        let idx = state
            .pending
            .iter()
            .position(|pending| pending.page == page && pending.resolved)?;
        Some(state.pending.remove(idx).data.unwrap_or_default())
    }

    fn still_pending(&self, page: usize) -> bool {
        self.state.lock().pending.iter().any(|pending| pending.page == page)
    }
}

impl FileLike for UserfaultFile {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let mut state = self.state.lock();
            let Some(msg) = state.queue.pop_front() else {
                return Err(AxError::WouldBlock);
            };
            let bytes = unsafe {
                core::slice::from_raw_parts((&msg as *const UffdMsg).cast::<u8>(), size_of::<UffdMsg>())
            };
            if dst.remaining_mut() < bytes.len() {
                return Err(AxError::InvalidInput);
            }
            dst.write(bytes)?;
            Ok(bytes.len())
        }))
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            UFFDIO_API => self.ioctl_api(arg as *mut UffdApi),
            UFFDIO_REGISTER => self.ioctl_register(arg as *mut UffdRegister),
            UFFDIO_COPY => self.ioctl_copy(arg as *mut UffdCopy),
            _ => Err(AxError::Unsupported),
        }
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[userfaultfd]".into()
    }
}

impl Pollable for UserfaultFile {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.has_events());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
    }
}

fn each_userfault_file(mut f: impl FnMut(&Arc<UserfaultFile>) -> bool) {
    let mut files = USERFAULT_FILES.lock();
    files.retain(|weak| {
        if let Some(file) = weak.upgrade() {
            f(&file)
        } else {
            false
        }
    });
}

pub fn wait_missing_page_for_current(pid: Pid, addr: VirtAddr, write: bool) -> Option<Vec<u8>> {
    let mut matched = None;
    each_userfault_file(|file| {
        if file.begin_fault(pid, addr, write) {
            matched = Some(file.clone());
        }
        true
    });
    let file = matched?;
    let page = addr.align_down_4k().as_usize();
    loop {
        if let Some(data) = file.take_resolved(page) {
            return Some(data);
        }
        if !file.still_pending(page) {
            return None;
        }
        axtask::yield_now();
    }
}
