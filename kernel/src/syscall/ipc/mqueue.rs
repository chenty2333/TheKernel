use alloc::{borrow::Cow, collections::BTreeMap, format, string::String, sync::Arc, vec::Vec};
use core::{
    ffi::c_char,
    sync::atomic::{AtomicUsize, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::{
    __kernel_mode_t, CAP_DAC_OVERRIDE, CAP_FOWNER, O_ACCMODE, O_CLOEXEC, O_CREAT, O_EXCL,
    O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, SI_MESGQ, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD,
    sigevent, timespec,
};
use starry_signal::{SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr, vm_load, vm_write_slice};

use crate::{
    file::{
        FileHandle, FileLike, Kstat, NetlinkSocket, add_file_like_with_flags, get_file_description,
        get_typed_file,
    },
    mm::vm_load_string,
    task::{
        AsThread, ProcStateHint, has_pending_syscall_signal, send_signal_to_process,
        with_proc_state_hint,
    },
    time::{TimeValueLike, wall_time},
};

const MQ_PRIO_MAX: u32 = 32768;
const DEFAULT_MQ_MAXMSG: isize = 10;
const DEFAULT_MQ_MSGSIZE: isize = 8192;
const DEFAULT_QUEUES_MAX: usize = 256;
const DEFAULT_MSG_MAX: usize = 1024;
const DEFAULT_MSGSIZE_MAX: usize = 1 << 20;
const MQ_WAIT_SLICE: Duration = Duration::from_millis(10);
const MQ_NAME_MAX: usize = 255;
const NOTIFY_COOKIE_LEN: usize = 32;
const NOTIFY_WOKENUP: u8 = 1;
const NOTIFY_REMOVED: u8 = 2;

static MQ_MANAGER: Mutex<MqManager> = Mutex::new(MqManager::new());
static MQ_QUEUES_MAX: AtomicUsize = AtomicUsize::new(DEFAULT_QUEUES_MAX);
static MQ_MSG_MAX: AtomicUsize = AtomicUsize::new(DEFAULT_MSG_MAX);
static MQ_MSGSIZE_MAX: AtomicUsize = AtomicUsize::new(DEFAULT_MSGSIZE_MAX);

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct MqAttr {
    mq_flags: isize,
    mq_maxmsg: isize,
    mq_msgsize: isize,
    mq_curmsgs: isize,
    __reserved: [isize; 4],
}

#[derive(Clone)]
struct MqMessage {
    priority: u32,
    sequence: u64,
    data: Vec<u8>,
}

#[derive(Clone)]
struct MqThreadNotifier {
    netlink: FileHandle<NetlinkSocket>,
    cookie: [u8; NOTIFY_COOKIE_LEN],
}

#[derive(Clone)]
struct MqNotifier {
    pid: u32,
    notify: i32,
    signo: i32,
    value_int: i32,
    thread: Option<MqThreadNotifier>,
}

struct PosixMqueue {
    name: String,
    mode: __kernel_mode_t,
    uid: u32,
    gid: u32,
    maxmsg: usize,
    msgsize: usize,
    messages: Vec<MqMessage>,
    next_sequence: u64,
    notifier: Option<MqNotifier>,
    waiters: Arc<axtask::WaitQueue>,
}

struct MqManager {
    queues: BTreeMap<String, Arc<Mutex<PosixMqueue>>>,
}

pub struct MqFd {
    queue: Arc<Mutex<PosixMqueue>>,
    access: MqAccess,
    nonblocking: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MqAccess {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

impl MqManager {
    const fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
        }
    }
}

impl PosixMqueue {
    fn new(name: String, mode: __kernel_mode_t, uid: u32, gid: u32, attr: MqAttr) -> Self {
        Self {
            name,
            mode,
            uid,
            gid,
            maxmsg: attr.mq_maxmsg as usize,
            msgsize: attr.mq_msgsize as usize,
            messages: Vec::new(),
            next_sequence: 0,
            notifier: None,
            waiters: Arc::new(axtask::WaitQueue::new()),
        }
    }

    fn attr(&self, flags: isize) -> MqAttr {
        MqAttr {
            mq_flags: flags,
            mq_maxmsg: self.maxmsg as isize,
            mq_msgsize: self.msgsize as isize,
            mq_curmsgs: self.messages.len() as isize,
            __reserved: [0; 4],
        }
    }

    fn insert_message(&mut self, priority: u32, data: Vec<u8>) -> bool {
        let was_empty = self.messages.is_empty();
        let message = MqMessage {
            priority,
            sequence: self.next_sequence,
            data,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);

        let index = self.messages.partition_point(|existing| {
            existing.priority > message.priority
                || (existing.priority == message.priority && existing.sequence <= message.sequence)
        });
        self.messages.insert(index, message);
        was_empty
    }

    fn pop_message(&mut self) -> Option<MqMessage> {
        if self.messages.is_empty() {
            None
        } else {
            Some(self.messages.remove(0))
        }
    }
}

impl MqAccess {
    fn from_flags(flags: i32) -> AxResult<Self> {
        match (flags as u32) & O_ACCMODE {
            O_RDONLY => Ok(Self::ReadOnly),
            O_WRONLY => Ok(Self::WriteOnly),
            O_RDWR => Ok(Self::ReadWrite),
            _ => Err(AxError::InvalidInput),
        }
    }

    fn can_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    fn can_write(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }
}

impl MqFd {
    fn new(queue: Arc<Mutex<PosixMqueue>>, access: MqAccess, nonblocking: bool) -> Self {
        Self {
            queue,
            access,
            nonblocking: AtomicUsize::new(nonblocking as usize),
        }
    }

    fn is_nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire) != 0
    }
}

impl FileLike for MqFd {
    fn stat(&self) -> AxResult<Kstat> {
        let queue = self.queue.lock();
        Ok(Kstat {
            mode: linux_raw_sys::general::S_IFREG | (queue.mode & 0o777),
            uid: queue.uid,
            gid: queue.gid,
            size: queue.messages.len() as u64,
            ..Kstat::default()
        })
    }

    fn path(&self) -> Cow<'_, str> {
        Cow::Owned(format!("mqueue:{}", self.queue.lock().name))
    }

    fn nonblocking(&self) -> bool {
        self.is_nonblocking()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.nonblocking
            .store(nonblocking as usize, Ordering::Release);
        Ok(())
    }
}

impl Pollable for MqFd {
    fn poll(&self) -> IoEvents {
        let queue = self.queue.lock();
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, queue.messages.len() > 0);
        events.set(IoEvents::OUT, queue.messages.len() < queue.maxmsg);
        events
    }

    fn register(&self, context: &mut Context<'_>, _events: IoEvents) {
        let _ = context;
    }
}

pub(crate) fn mq_queues_max() -> usize {
    MQ_QUEUES_MAX.load(Ordering::Acquire)
}

pub(crate) fn set_mq_queues_max(value: usize) {
    MQ_QUEUES_MAX.store(value.max(1), Ordering::Release);
}

pub(crate) fn mq_msg_max() -> usize {
    MQ_MSG_MAX.load(Ordering::Acquire)
}

pub(crate) fn set_mq_msg_max(value: usize) {
    MQ_MSG_MAX.store(value.max(1), Ordering::Release);
}

pub(crate) fn mq_msgsize_max() -> usize {
    MQ_MSGSIZE_MAX.load(Ordering::Acquire)
}

pub(crate) fn set_mq_msgsize_max(value: usize) {
    MQ_MSGSIZE_MAX.store(value.max(1), Ordering::Release);
}

fn current_ids() -> (u32, u32) {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    (proc_data.fsuid(), proc_data.fsgid())
}

fn normalize_name(name: *const c_char) -> AxResult<String> {
    let raw = vm_load_string(name)?;
    if raw.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let inner = raw.strip_prefix('/').unwrap_or(&raw);
    if inner.is_empty() || inner.as_bytes().contains(&b'/') {
        return Err(AxError::InvalidInput);
    }
    if inner.len() > MQ_NAME_MAX {
        return Err(AxError::NameTooLong);
    }
    Ok(inner.into())
}

fn default_attr() -> MqAttr {
    MqAttr {
        mq_flags: 0,
        mq_maxmsg: DEFAULT_MQ_MAXMSG,
        mq_msgsize: DEFAULT_MQ_MSGSIZE,
        mq_curmsgs: 0,
        __reserved: [0; 4],
    }
}

fn read_create_attr(attr: *const MqAttr) -> AxResult<MqAttr> {
    let attr = if attr.is_null() {
        default_attr()
    } else {
        attr.vm_read()?
    };

    if attr.mq_maxmsg <= 0 || attr.mq_msgsize <= 0 {
        return Err(AxError::InvalidInput);
    }
    if attr.mq_maxmsg as usize > mq_msg_max() || attr.mq_msgsize as usize > mq_msgsize_max() {
        return Err(AxError::InvalidInput);
    }
    Ok(MqAttr {
        mq_flags: 0,
        mq_curmsgs: 0,
        __reserved: [0; 4],
        ..attr
    })
}

fn has_queue_permission(queue: &PosixMqueue, access: MqAccess) -> bool {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if proc_data.has_effective_capability(CAP_DAC_OVERRIDE) {
        return true;
    }

    let owner_bits = (queue.mode >> 6) & 0o7;
    let group_bits = (queue.mode >> 3) & 0o7;
    let other_bits = queue.mode & 0o7;
    let bits = if proc_data.fsuid() == queue.uid {
        owner_bits
    } else if proc_data.fsgid() == queue.gid || proc_data.is_in_fs_group(queue.gid) {
        group_bits
    } else {
        other_bits
    };

    let read_ok = bits & 0o4 != 0;
    let write_ok = bits & 0o2 != 0;
    match access {
        MqAccess::ReadOnly => read_ok,
        MqAccess::WriteOnly => write_ok,
        MqAccess::ReadWrite => read_ok && write_ok,
    }
}

fn get_mq_fd(fd: i32) -> AxResult<crate::file::FileHandle<MqFd>> {
    get_typed_file::<MqFd>(fd).map_err(|_| AxError::BadFileDescriptor)
}

fn nonblock_flags(file: &crate::file::FileHandle<MqFd>) -> isize {
    if file.is_nonblocking() {
        O_NONBLOCK as isize
    } else {
        0
    }
}

fn validate_timespec(timeout: *const timespec) -> AxResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let timeout = unsafe { timeout.vm_read_uninit()?.assume_init() };
    let tv = timeout.try_into_time_value()?;
    Ok(Some(Duration::from_nanos(
        tv.as_nanos().min(u64::MAX as u128) as u64,
    )))
}

fn validate_notify_event(event: &sigevent) -> AxResult {
    match event.sigev_notify as u32 {
        SIGEV_NONE | SIGEV_THREAD => Ok(()),
        SIGEV_SIGNAL => {
            if event.sigev_signo == 0 {
                Ok(())
            } else if (1..=64).contains(&event.sigev_signo)
                && Signo::from_repr(event.sigev_signo as u8).is_some()
            {
                Ok(())
            } else {
                Err(AxError::InvalidInput)
            }
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn build_notifier(event: &sigevent) -> AxResult<MqNotifier> {
    validate_notify_event(event)?;
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let mut notifier = MqNotifier {
        pid: proc_data.proc.pid(),
        notify: event.sigev_notify,
        signo: event.sigev_signo,
        value_int: unsafe { event.sigev_value.sival_int },
        thread: None,
    };

    if event.sigev_notify == SIGEV_THREAD as i32 {
        let netlink =
            NetlinkSocket::from_fd(event.sigev_signo).map_err(|_| AxError::BadFileDescriptor)?;
        let cookie_ptr = unsafe { event.sigev_value.sival_ptr };
        let cookie_data = vm_load(cookie_ptr.cast::<u8>(), NOTIFY_COOKIE_LEN)?;
        let mut cookie = [0u8; NOTIFY_COOKIE_LEN];
        cookie.copy_from_slice(&cookie_data);
        notifier.thread = Some(MqThreadNotifier { netlink, cookie });
    }

    Ok(notifier)
}

fn wall_time_duration() -> Duration {
    let now = wall_time();
    Duration::new(now.as_secs(), now.subsec_nanos())
}

fn wait_for_queue(waiters: Arc<axtask::WaitQueue>, deadline: Option<Duration>) -> AxResult {
    let current = current();
    let thread = current.as_thread();
    if has_pending_syscall_signal(thread) {
        return Err(AxError::Interrupted);
    }
    if let Some(deadline) = deadline
        && wall_time_duration() >= deadline
    {
        return Err(AxError::TimedOut);
    }

    let sleep_for = deadline
        .map(|deadline| {
            deadline
                .saturating_sub(wall_time_duration())
                .min(MQ_WAIT_SLICE)
        })
        .unwrap_or(MQ_WAIT_SLICE);
    with_proc_state_hint(ProcStateHint::Interruptible, || {
        waiters.wait_timeout(sleep_for);
    });
    Ok(())
}

fn send_thread_notification(thread: &MqThreadNotifier, state: u8) {
    let mut cookie = thread.cookie;
    cookie[NOTIFY_COOKIE_LEN - 1] = state;
    thread.netlink.enqueue_kernel(Vec::from(cookie));
}

fn remove_notification(notifier: Option<MqNotifier>) {
    if let Some(notifier) = notifier
        && notifier.notify == SIGEV_THREAD as i32
        && let Some(thread) = notifier.thread.as_ref()
    {
        send_thread_notification(thread, NOTIFY_REMOVED);
    }
}

fn maybe_notify(notifier: Option<MqNotifier>) {
    let Some(notifier) = notifier else {
        return;
    };
    if notifier.notify == SIGEV_THREAD as i32 {
        if let Some(thread) = notifier.thread.as_ref() {
            send_thread_notification(thread, NOTIFY_WOKENUP);
        }
        return;
    }
    if notifier.notify != SIGEV_SIGNAL as i32 {
        return;
    }
    let Some(signo) = Signo::from_repr(notifier.signo as u8) else {
        return;
    };

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let mut info = SignalInfo::new_kernel(signo);
    info.set_code(SI_MESGQ);
    unsafe {
        let rt = &mut info.0.__bindgen_anon_1.__bindgen_anon_1._sifields._rt;
        rt._pid = proc_data.proc.pid() as _;
        rt._uid = proc_data.uid() as _;
        rt._sigval = linux_raw_sys::general::sigval_t {
            sival_int: notifier.value_int,
        };
    }
    let _ = send_signal_to_process(notifier.pid, Some(info));
}

pub fn sys_mq_open(
    name: *const c_char,
    oflag: i32,
    mode: __kernel_mode_t,
    attr: *const MqAttr,
) -> AxResult<isize> {
    let name = normalize_name(name)?;
    let access = MqAccess::from_flags(oflag)?;
    let create = (oflag as u32) & O_CREAT != 0;
    let excl = (oflag as u32) & O_EXCL != 0;
    let nonblocking = (oflag as u32) & O_NONBLOCK != 0;
    let cloexec = (oflag as u32) & O_CLOEXEC != 0;

    let (queue, created) = {
        let mut manager = MQ_MANAGER.lock();
        if let Some(queue) = manager.queues.get(&name).cloned() {
            if create && excl {
                return Err(AxError::AlreadyExists);
            }
            {
                let guard = queue.lock();
                if !has_queue_permission(&guard, access) {
                    return Err(AxError::PermissionDenied);
                }
            }
            (queue, false)
        } else {
            if !create {
                return Err(AxError::NotFound);
            }
            if manager.queues.len() >= mq_queues_max() {
                return Err(LinuxError::ENOSPC.into());
            }
            let attr = read_create_attr(attr)?;
            let (uid, gid) = current_ids();
            let create_mode = (mode & !current().as_thread().proc_data.umask()) & 0o777;
            let queue = Arc::new(Mutex::new(PosixMqueue::new(
                name.clone(),
                create_mode,
                uid,
                gid,
                attr,
            )));
            manager.queues.insert(name.clone(), queue.clone());
            (queue, true)
        }
    };

    let mqfd = Arc::new(MqFd::new(queue.clone(), access, nonblocking));
    let status_flags = ((oflag as u32) & O_NONBLOCK) | ((oflag as u32) & O_ACCMODE);
    match add_file_like_with_flags(mqfd, cloexec, status_flags) {
        Ok(fd) => Ok(fd as isize),
        Err(err) => {
            if created {
                let mut manager = MQ_MANAGER.lock();
                if manager
                    .queues
                    .get(&name)
                    .is_some_and(|existing| Arc::ptr_eq(existing, &queue))
                {
                    manager.queues.remove(&name);
                }
            }
            Err(err)
        }
    }
}

pub fn sys_mq_unlink(name: *const c_char) -> AxResult<isize> {
    let name = normalize_name(name)?;
    let queue = {
        let manager = MQ_MANAGER.lock();
        manager
            .queues
            .get(&name)
            .cloned()
            .ok_or(AxError::NotFound)?
    };
    {
        let queue = queue.lock();
        let curr = current();
        let proc_data = &curr.as_thread().proc_data;
        if proc_data.fsuid() != queue.uid && !proc_data.has_effective_capability(CAP_FOWNER) {
            return Err(AxError::PermissionDenied);
        }
    }
    let removed = MQ_MANAGER.lock().queues.remove(&name);
    if let Some(queue) = removed {
        let guard = queue.lock();
        guard.waiters.notify_all(false);
    }
    Ok(0)
}

pub fn sys_mq_timedsend(
    fd: i32,
    msg_ptr: *const u8,
    msg_len: usize,
    msg_prio: u32,
    abs_timeout: *const timespec,
) -> AxResult<isize> {
    if msg_prio >= MQ_PRIO_MAX {
        return Err(AxError::InvalidInput);
    }
    let deadline = validate_timespec(abs_timeout)?;
    let file = get_mq_fd(fd)?;
    if !file.access.can_write() {
        return Err(AxError::BadFileDescriptor);
    }

    let data = {
        let queue = file.queue.lock();
        if msg_len > queue.msgsize {
            return Err(AxError::from(LinuxError::EMSGSIZE));
        }
        drop(queue);
        vm_load(msg_ptr, msg_len)?
    };

    loop {
        let waiters = {
            let mut queue = file.queue.lock();
            if queue.messages.len() < queue.maxmsg {
                let was_empty = queue.insert_message(msg_prio, data);
                let notifier = if was_empty {
                    queue.notifier.take()
                } else {
                    None
                };
                let waiters = queue.waiters.clone();
                waiters.notify_all(false);
                drop(queue);
                maybe_notify(notifier);
                return Ok(0);
            }
            if file.is_nonblocking() {
                return Err(AxError::WouldBlock);
            }
            queue.waiters.clone()
        };
        wait_for_queue(waiters, deadline)?;
    }
}

pub fn sys_mq_timedreceive(
    fd: i32,
    msg_ptr: *mut u8,
    msg_len: usize,
    msg_prio: *mut u32,
    abs_timeout: *const timespec,
) -> AxResult<isize> {
    let deadline = validate_timespec(abs_timeout)?;
    let file = get_mq_fd(fd)?;
    if !file.access.can_read() {
        return Err(AxError::BadFileDescriptor);
    }

    loop {
        let waiters = {
            let mut queue = file.queue.lock();
            if !queue.messages.is_empty() {
                if msg_len < queue.msgsize {
                    return Err(AxError::from(LinuxError::EMSGSIZE));
                }
                let message = queue.pop_message().ok_or(AxError::InvalidInput)?;
                let waiters = queue.waiters.clone();
                waiters.notify_all(false);
                drop(queue);
                vm_write_slice(msg_ptr, &message.data)?;
                if !msg_prio.is_null() {
                    msg_prio.vm_write(message.priority)?;
                }
                return Ok(message.data.len() as isize);
            }
            if file.is_nonblocking() {
                return Err(AxError::WouldBlock);
            }
            queue.waiters.clone()
        };
        wait_for_queue(waiters, deadline)?;
    }
}

pub fn sys_mq_notify(fd: i32, notification: *const sigevent) -> AxResult<isize> {
    let notifier = if notification.is_null() {
        None
    } else {
        let event = unsafe { notification.vm_read_uninit()?.assume_init() };
        Some(build_notifier(&event)?)
    };
    let file = get_mq_fd(fd)?;

    let removed = {
        let mut queue = file.queue.lock();
        if let Some(notifier) = notifier {
            if queue.notifier.is_some() {
                return Err(AxError::ResourceBusy);
            }
            queue.notifier = Some(notifier);
            None
        } else {
            let curr_pid = current().as_thread().proc_data.proc.pid();
            if queue
                .notifier
                .as_ref()
                .is_some_and(|notifier| notifier.pid == curr_pid)
            {
                queue.notifier.take()
            } else {
                None
            }
        }
    };
    remove_notification(removed);
    Ok(0)
}

pub fn sys_mq_getsetattr(
    fd: i32,
    new_attr: *const MqAttr,
    old_attr: *mut MqAttr,
) -> AxResult<isize> {
    let file = get_mq_fd(fd)?;
    let description = get_file_description(fd)?;
    let new = if new_attr.is_null() {
        None
    } else {
        let attr = new_attr.vm_read()?;
        if attr.mq_flags & !(O_NONBLOCK as isize) != 0 {
            return Err(AxError::InvalidInput);
        }
        Some(attr)
    };

    if !old_attr.is_null() {
        let queue = file.queue.lock();
        old_attr.vm_write(queue.attr(nonblock_flags(&file)))?;
    }
    if let Some(attr) = new {
        let nonblocking = attr.mq_flags & O_NONBLOCK as isize != 0;
        file.set_nonblocking(nonblocking)?;
        let mut flags = description.status_flags() & !O_NONBLOCK;
        if nonblocking {
            flags |= O_NONBLOCK;
        }
        description.set_status_flags(flags);
    }
    Ok(0)
}
