use alloc::{
    borrow::Cow,
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ffi::c_char,
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollSet, Pollable};
#[cfg(not(test))]
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::{
    __kernel_mode_t, CAP_DAC_OVERRIDE, CAP_FOWNER, O_ACCMODE, O_CLOEXEC, O_CREAT, O_EXCL,
    O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, SI_MESGQ, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD,
    timespec,
};
// Host unit tests do not initialize a scheduler/current task. The ownership
// tests below exercise the same queue/registry critical sections with a spin
// mutex; blocking/wakeup behavior remains covered only by kernel/guest tests.
#[cfg(test)]
use spin::Mutex;
use thekernel_linux_signal::{PreparedSignal, SignalInfo, SignalRtPayload, Signo};
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load, vm_load_until_nul, vm_write_slice,
};

use crate::{
    file::{
        FileHandle, FileLike, Kstat, NetlinkSocket, PseudoInode, add_file_like_with_flags,
        get_typed_file,
    },
    mm::map_usercopy_error,
    readiness::block_on_poll_io_until,
    syscall::RawSigevent,
    task::{
        AsThread, Kgid, Kuid, PidNamespace, ProcStateHint, ProcessData, UserNamespace,
        prepare_queued_signal_for_process, send_prepared_signal_to_process_data,
        with_proc_state_hint,
    },
    time::TimeValueLike,
};

const MQ_PRIO_MAX: u32 = 32768;
const DEFAULT_MQ_MAXMSG: isize = 10;
const DEFAULT_MQ_MSGSIZE: isize = 8192;
const DEFAULT_QUEUES_MAX: usize = 256;
const DEFAULT_MSG_MAX: usize = 1024;
const DEFAULT_MSGSIZE_MAX: usize = 1 << 20;
const MQ_NAME_MAX: usize = 255;
const NOTIFY_COOKIE_LEN: usize = 32;
const NOTIFY_WOKENUP: u8 = 1;
const NOTIFY_REMOVED: u8 = 2;
const MQ_INODE_SIZE: u64 = 80;

static MQ_MANAGER: Mutex<MqManager> = Mutex::new(MqManager::new());
static MQ_NOTIFICATION_REGISTRY: Mutex<MqNotificationRegistry> =
    Mutex::new(MqNotificationRegistry::new());
static MQ_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);
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

// The queue attribute is a Linux ABI value made entirely of isize words. Keep
// the unchecked copyout below tied to the actual repr(C) layout instead of
// relying on a generated `Pod` marker.
const _: () = {
    assert!(align_of::<MqAttr>() == align_of::<isize>());
    assert!(size_of::<MqAttr>() == isize::BITS as usize);
    assert!(offset_of!(MqAttr, mq_flags) == 0);
    assert!(offset_of!(MqAttr, mq_maxmsg) == size_of::<isize>());
    assert!(offset_of!(MqAttr, mq_msgsize) == size_of::<isize>() * 2);
    assert!(offset_of!(MqAttr, mq_curmsgs) == size_of::<isize>() * 3);
    assert!(offset_of!(MqAttr, __reserved) == size_of::<isize>() * 4);
};

#[derive(Clone)]
struct MqSender {
    pid: u32,
    real_uid: Kuid,
    pid_ns: Arc<PidNamespace>,
}

#[derive(Clone)]
struct MqMessage {
    priority: u32,
    sequence: u64,
    sender: MqSender,
    data: Vec<u8>,
}

#[derive(Clone)]
struct MqThreadNotifier {
    netlink: FileHandle<NetlinkSocket>,
    cookie: [u8; NOTIFY_COOKIE_LEN],
}

struct MqNotifier {
    pid: u32,
    notify: i32,
    thread: Option<MqThreadNotifier>,
    signal: Option<MqSignalNotifier>,
    registration: Arc<MqNotificationToken>,
}

struct MqNotificationToken {
    id: u64,
    active: AtomicBool,
}

struct MqSignalNotifier {
    target: Weak<ProcessData>,
    target_user_ns: Arc<UserNamespace>,
    target_pid_ns: Arc<PidNamespace>,
    info: SignalInfo,
    prepared: PreparedSignal,
}

struct PosixMqueue {
    inode: PseudoInode,
    name: String,
    mode: __kernel_mode_t,
    uid: u32,
    gid: u32,
    maxmsg: usize,
    msgsize: usize,
    messages: Vec<MqMessage>,
    next_sequence: u64,
    notifier: Option<MqNotifier>,
    readiness: Arc<MqReadiness>,
}

struct MqReadiness {
    readable: PollSet,
    writable: PollSet,
}

struct MqManager {
    queues: BTreeMap<String, Arc<Mutex<PosixMqueue>>>,
}

struct MqNotificationRegistration {
    owner: u32,
    queue: Weak<Mutex<PosixMqueue>>,
    token: Arc<MqNotificationToken>,
}

struct MqNotificationRegistry {
    entries: BTreeMap<u64, MqNotificationRegistration>,
}

pub struct MqFd {
    queue: Arc<Mutex<PosixMqueue>>,
    readiness: Arc<MqReadiness>,
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

impl MqNotificationRegistry {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl PosixMqueue {
    fn new(
        name: String,
        mode: __kernel_mode_t,
        uid: u32,
        gid: u32,
        attr: MqAttr,
    ) -> AxResult<Self> {
        let maxmsg = attr.mq_maxmsg as usize;
        let mut messages = Vec::new();
        messages
            .try_reserve_exact(maxmsg)
            .map_err(|_| AxError::NoMemory)?;
        let readiness = Arc::try_new(MqReadiness {
            readable: PollSet::new(),
            writable: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            inode: PseudoInode::mqueue(mode, uid, gid),
            name,
            mode,
            uid,
            gid,
            maxmsg,
            msgsize: attr.mq_msgsize as usize,
            messages,
            next_sequence: 0,
            notifier: None,
            readiness,
        })
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

    fn insert_message(&mut self, priority: u32, sender: MqSender, data: Vec<u8>) -> bool {
        let was_empty = self.messages.is_empty();
        let message = MqMessage {
            priority,
            sequence: self.next_sequence,
            sender,
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

impl Drop for PosixMqueue {
    fn drop(&mut self) {
        if let Some(notifier) = self.notifier.as_ref() {
            notifier.registration.active.store(false, Ordering::Release);
            unregister_notification(notifier.registration.id);
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
        let readiness = Arc::clone(&queue.lock().readiness);
        Self {
            queue,
            readiness,
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
        let mut stat = queue.inode.stat();
        stat.mode = linux_raw_sys::general::S_IFREG | (queue.mode & 0o777);
        stat.uid = queue.uid;
        stat.gid = queue.gid;
        stat.size = MQ_INODE_SIZE;
        Ok(stat)
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        // Reserve the protocol maximum before taking the queue lock so path
        // rendering cannot allocate while the queue's spin mutex is held.
        let mut path = String::new();
        path.try_reserve_exact(MQ_NAME_MAX + 1)
            .map_err(|_| AxError::NoMemory)?;
        path.push('/');
        let queue = self.queue.lock();
        debug_assert!(queue.name.len() <= MQ_NAME_MAX);
        path.push_str(&queue.name);
        Ok(Cow::Owned(path))
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
        events.set(IoEvents::READABLE, !queue.messages.is_empty());
        events.set(IoEvents::WRITABLE, queue.messages.len() < queue.maxmsg);
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        let read = self.access.can_read() && events.contains(IoEvents::READABLE);
        let write = self.access.can_write() && events.contains(IoEvents::WRITABLE);
        let mut prepared =
            axpoll::PreparedPollRegistration::try_new(read as usize + write as usize)?;
        if read {
            prepared.arm(&self.readiness.readable, context.waker())?;
        }
        if write {
            prepared.arm(&self.readiness.writable, context.waker())?;
        }
        prepared.commit()
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
    let thread = curr.as_thread();
    (thread.fsuid().into_raw(), thread.fsgid().into_raw())
}

fn current_mq_sender() -> MqSender {
    let curr = current();
    let thread = curr.as_thread();
    MqSender {
        pid: thread.proc_data.proc.pid(),
        real_uid: thread.real_uid(),
        pid_ns: thread.proc_data.pid_ns(),
    }
}

fn normalize_name<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const c_char,
) -> AxResult<String> {
    let raw = vm_load_until_nul(memory, name.cast::<u8>()).map_err(map_usercopy_error)?;
    let raw = String::from_utf8(raw).map_err(|_| AxError::IllegalBytes)?;
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

fn read_create_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: *const MqAttr,
) -> AxResult<MqAttr> {
    let attr = if attr.is_null() {
        default_attr()
    } else {
        VmPtr::vm_read(attr, memory).map_err(map_usercopy_error)?
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
    let cred = curr.as_thread().current_cred();
    let ids = cred.ids();
    if cred.has_effective_capability(CAP_DAC_OVERRIDE) {
        return true;
    }

    let owner_bits = (queue.mode >> 6) & 0o7;
    let group_bits = (queue.mode >> 3) & 0o7;
    let other_bits = queue.mode & 0o7;
    let bits = if Kuid::from_raw(queue.uid) == Some(ids.fsuid) {
        owner_bits
    } else if Kgid::from_raw(queue.gid) == Some(ids.fsgid)
        || Kgid::from_raw(queue.gid).is_some_and(|gid| cred.groups().contains(gid))
    {
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
    if file.io_status_snapshot().nonblocking() {
        O_NONBLOCK as isize
    } else {
        0
    }
}

fn validate_timespec<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timeout: *const timespec,
) -> AxResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }
    // SAFETY: `VmPtr::vm_read_uninit` initializes the complete repr(C)
    // `timespec` object before it is converted into a kernel value.
    let timeout = unsafe {
        VmPtr::vm_read_uninit(timeout, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let tv = timeout.try_into_time_value()?;
    Ok(Some(Duration::from_nanos(
        tv.as_nanos().min(u64::MAX as u128) as u64,
    )))
}

fn validate_notify_event(event: &RawSigevent) -> AxResult {
    match event.notify() as u32 {
        SIGEV_NONE | SIGEV_THREAD => Ok(()),
        SIGEV_SIGNAL => {
            // Signo 0 selects "notify without delivering a signal"; any other
            // value must name a signal this kernel can actually deliver.
            let accepted = event.signo() == 0
                || ((1..=64).contains(&event.signo())
                    && Signo::from_repr(event.signo() as u8).is_some());
            if accepted {
                Ok(())
            } else {
                Err(AxError::InvalidInput)
            }
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn new_notification_token() -> AxResult<Arc<MqNotificationToken>> {
    let id = MQ_NOTIFICATION_ID.fetch_add(1, Ordering::Relaxed).max(1);
    Arc::try_new(MqNotificationToken {
        id,
        active: AtomicBool::new(true),
    })
    .map_err(|_| AxError::NoMemory)
}

fn build_notifier<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    event: &RawSigevent,
) -> AxResult<MqNotifier> {
    validate_notify_event(event)?;
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let mut notifier = MqNotifier {
        pid: proc_data.proc.pid(),
        notify: event.notify(),
        thread: None,
        signal: None,
        registration: new_notification_token()?,
    };

    if event.notify() == SIGEV_THREAD as i32 {
        let netlink =
            NetlinkSocket::from_fd(event.signo()).map_err(|_| AxError::BadFileDescriptor)?;
        let cookie_ptr = event.value_ptr_address() as *const u8;
        let cookie_data =
            vm_load(memory, cookie_ptr, NOTIFY_COOKIE_LEN).map_err(map_usercopy_error)?;
        let mut cookie = [0u8; NOTIFY_COOKIE_LEN];
        cookie.copy_from_slice(&cookie_data);
        notifier.thread = Some(MqThreadNotifier { netlink, cookie });
    } else if event.notify() == SIGEV_SIGNAL as i32
        && let Some(signo) = u8::try_from(event.signo()).ok().and_then(Signo::from_repr)
    {
        // Linux reserves a sigqueue record when mq_notify registers, not when
        // the empty->nonempty edge consumes the one-shot registration. This
        // makes RT siginfo delivery allocation-free. The sender fields are
        // filled from the message snapshot at the edge; only the target
        // namespace and sigev_value belong to this registration.
        let target_user_ns = curr.as_thread().current_user_namespace();
        let target_pid_ns = proc_data.pid_ns();
        let info = SignalInfo::new_rt(
            signo,
            SI_MESGQ,
            SignalRtPayload::new(0, 0, event.value_ptr_address()),
        );
        let prepared = prepare_queued_signal_for_process(proc_data, info.clone())?;
        notifier.signal = Some(MqSignalNotifier {
            target: Arc::downgrade(proc_data),
            target_user_ns,
            target_pid_ns,
            info,
            prepared,
        });
    }

    Ok(notifier)
}

fn wait_mq_operation<T>(
    file: &MqFd,
    events: IoEvents,
    deadline: Option<Duration>,
    operation: impl FnMut() -> AxResult<T>,
) -> AxResult<T> {
    with_proc_state_hint(ProcStateHint::Interruptible, || {
        block_on_poll_io_until(file, events, file.is_nonblocking(), deadline, operation)
            .map_err(|_| AxError::TimedOut)?
    })
}

fn send_thread_notification(thread: &MqThreadNotifier, state: u8) {
    let mut cookie = thread.cookie;
    cookie[NOTIFY_COOKIE_LEN - 1] = state;
    thread.netlink.enqueue_kernel(Vec::from(cookie));
}

fn unregister_notification(id: u64) {
    let removed = MQ_NOTIFICATION_REGISTRY.lock().entries.remove(&id);
    // Weak and token Arcs are released outside the registry mutex.
    drop(removed);
}

fn claim_notification(notifier: &MqNotifier) -> bool {
    if notifier
        .registration
        .active
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    unregister_notification(notifier.registration.id);
    true
}

fn remove_notification(notifier: Option<MqNotifier>) {
    if let Some(notifier) = notifier {
        let claimed = claim_notification(&notifier);
        if claimed
            && notifier.notify == SIGEV_THREAD as i32
            && let Some(thread) = notifier.thread.as_ref()
        {
            send_thread_notification(thread, NOTIFY_REMOVED);
        }
    }
}

fn mq_signal_info_for_sender(
    registration_info: &SignalInfo,
    target_user_ns: &UserNamespace,
    target_pid_ns: &Arc<PidNamespace>,
    sender: MqSender,
) -> SignalInfo {
    let mut info = registration_info.clone();
    let mut payload = info.rt_payload();
    payload.pid = mq_sender_pid_in_namespace(&sender, target_pid_ns);
    payload.uid = target_user_ns.from_kuid_munged(sender.real_uid);
    info.set_rt_payload(payload);
    info
}

fn mq_sender_pid_in_namespace(sender: &MqSender, target_pid_ns: &Arc<PidNamespace>) -> i32 {
    // A task is visible in its own PID namespace and every ancestor, but not
    // in a parent’s other descendants (or in a sibling namespace). Walk the
    // sender's stable namespace ancestry rather than consulting `current()`
    // when the one-shot notification is delivered.
    let mut sender_pid_ns = Some(sender.pid_ns.clone());
    while let Some(namespace) = sender_pid_ns {
        if Arc::ptr_eq(&namespace, target_pid_ns) {
            return target_pid_ns.visible_pid(sender.pid) as i32;
        }
        sender_pid_ns = namespace.parent();
    }
    0
}

fn maybe_notify(notifier: Option<MqNotifier>, sender: MqSender) {
    let Some(mut notifier) = notifier else {
        return;
    };
    if !claim_notification(&notifier) {
        return;
    }
    if notifier.notify == SIGEV_THREAD as i32 {
        if let Some(thread) = notifier.thread.as_ref() {
            send_thread_notification(thread, NOTIFY_WOKENUP);
        }
        return;
    }
    let Some(signal) = notifier.signal.take() else {
        return;
    };
    let Some(target) = signal.target.upgrade() else {
        return;
    };
    let info = mq_signal_info_for_sender(
        &signal.info,
        &signal.target_user_ns,
        &signal.target_pid_ns,
        sender,
    );

    let mut prepared = signal.prepared;
    // The queue record was reserved at registration, but SI_MESGQ's sender
    // fields are defined by the message which crossed the empty edge. Update
    // the already-accounted record so delivery remains allocation-free.
    prepared
        .replace_info(info.clone())
        .expect("mq_notify sender attribution preserves the reserved signal number");
    let _ = send_prepared_signal_to_process_data(&target, info, prepared);
}

/// Releases queue-notification reservations owned by an exiting process.
///
/// The registry stores a weak queue reference independently of the namespace
/// name, so this also finds unlinked-but-open queues.
pub(crate) fn cleanup_process_mqueue_notifications(pid: u32) {
    loop {
        let registration = {
            let mut registry = MQ_NOTIFICATION_REGISTRY.lock();
            let id = registry
                .entries
                .iter()
                .find_map(|(id, registration)| (registration.owner == pid).then_some(*id));
            id.and_then(|id| registry.entries.remove(&id))
        };
        let Some(registration) = registration else {
            break;
        };
        registration.token.active.store(false, Ordering::Release);
        let removed = registration.queue.upgrade().and_then(|queue| {
            let mut queue = queue.lock();
            queue
                .notifier
                .as_ref()
                .is_some_and(|notifier| notifier.registration.id == registration.token.id)
                .then(|| queue.notifier.take())
                .flatten()
        });
        // Drop the preallocated node, account charge, weak queue, and token
        // only after releasing registry and queue mutexes.
        drop(removed);
        drop(registration);
    }
}

pub fn sys_mq_open<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const c_char,
    oflag: i32,
    mode: __kernel_mode_t,
    attr: *const MqAttr,
) -> AxResult<isize> {
    let name = normalize_name(memory, name)?;
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
            let attr = read_create_attr(memory, attr)?;
            let (uid, gid) = current_ids();
            let create_mode = (mode & !current().as_thread().proc_data.umask()) & 0o777;
            let queue = Arc::try_new(Mutex::new(PosixMqueue::new(
                name.clone(),
                create_mode,
                uid,
                gid,
                attr,
            )?))
            .map_err(|_| AxError::NoMemory)?;
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

pub fn sys_mq_unlink<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const c_char,
) -> AxResult<isize> {
    let name = normalize_name(memory, name)?;
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
        let cred = curr.as_thread().current_cred();
        if Kuid::from_raw(queue.uid) != Some(cred.ids().fsuid)
            && !cred.has_effective_capability(CAP_FOWNER)
        {
            return Err(AxError::PermissionDenied);
        }
    }
    let removed = MQ_MANAGER.lock().queues.remove(&name);
    drop(removed);
    Ok(0)
}

pub fn sys_mq_timedsend<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    msg_ptr: *const u8,
    msg_len: usize,
    msg_prio: u32,
    abs_timeout: *const timespec,
) -> AxResult<isize> {
    if msg_prio >= MQ_PRIO_MAX {
        return Err(AxError::InvalidInput);
    }
    let deadline = validate_timespec(memory, abs_timeout)?;
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
        vm_load(memory, msg_ptr, msg_len).map_err(map_usercopy_error)?
    };

    let mut data = Some(data);
    wait_mq_operation(&file, IoEvents::WRITABLE, deadline, || {
        // Snapshot the sender for this successful insertion attempt. Do not
        // retain `current()` in the queue or notification registration.
        let sender = current_mq_sender();
        let (notifier, readiness, sender) = {
            let mut queue = file.queue.lock();
            if queue.messages.len() >= queue.maxmsg {
                return Err(AxError::WouldBlock);
            }
            let payload = data.take().ok_or(AxError::BadState)?;
            let notification_sender = sender.clone();
            let was_empty = queue.insert_message(msg_prio, sender, payload);
            let notification_sender = if was_empty {
                // Read the snapshot back from the message which crossed the
                // empty edge, keeping notification attribution tied to queue
                // state rather than to a later implicit `current()` lookup.
                queue
                    .messages
                    .first()
                    .expect("successful insertion makes the queue non-empty")
                    .sender
                    .clone()
            } else {
                notification_sender
            };
            let notifier = if was_empty {
                queue.notifier.take()
            } else {
                None
            };
            (notifier, Arc::clone(&queue.readiness), notification_sender)
        };
        readiness.readable.wake();
        maybe_notify(notifier, sender);
        Ok(0)
    })
}

pub fn sys_mq_timedreceive<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    msg_ptr: *mut u8,
    msg_len: usize,
    msg_prio: *mut u32,
    abs_timeout: *const timespec,
) -> AxResult<isize> {
    let deadline = validate_timespec(memory, abs_timeout)?;
    let file = get_mq_fd(fd)?;
    if !file.access.can_read() {
        return Err(AxError::BadFileDescriptor);
    }

    wait_mq_operation(&file, IoEvents::READABLE, deadline, || {
        let (message, readiness) = {
            let mut queue = file.queue.lock();
            if queue.messages.is_empty() {
                return Err(AxError::WouldBlock);
            }
            if msg_len < queue.msgsize {
                return Err(AxError::from(LinuxError::EMSGSIZE));
            }
            let message = queue.pop_message().ok_or(AxError::BadState)?;
            (message, Arc::clone(&queue.readiness))
        };
        vm_write_slice(memory, msg_ptr, &message.data).map_err(map_usercopy_error)?;
        if !msg_prio.is_null() {
            VmMutPtr::vm_write(msg_prio, memory, message.priority).map_err(map_usercopy_error)?;
        }
        readiness.writable.wake();
        Ok(message.data.len() as isize)
    })
}

pub fn sys_mq_notify<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    notification: *const RawSigevent,
) -> AxResult<isize> {
    let notifier = if notification.is_null() {
        None
    } else {
        let event =
            RawSigevent::read_from_user(memory, notification).map_err(map_usercopy_error)?;
        Some(build_notifier(memory, &event)?)
    };
    let file = get_mq_fd(fd)?;

    let mut rejected = None;
    let mut detached_registration = None;
    let mut busy = false;
    let removed = {
        let mut registry = MQ_NOTIFICATION_REGISTRY.lock();
        let mut queue = file.queue.lock();
        if let Some(notifier) = notifier {
            if queue.notifier.is_some() || registry.entries.contains_key(&notifier.registration.id)
            {
                rejected = Some(notifier);
                busy = true;
                None
            } else {
                let id = notifier.registration.id;
                registry.entries.insert(
                    id,
                    MqNotificationRegistration {
                        owner: notifier.pid,
                        queue: Arc::downgrade(&file.queue),
                        token: notifier.registration.clone(),
                    },
                );
                queue.notifier = Some(notifier);
                None
            }
        } else {
            let curr_pid = current().as_thread().proc_data.proc.pid();
            if queue
                .notifier
                .as_ref()
                .is_some_and(|notifier| notifier.pid == curr_pid)
            {
                let removed = queue.notifier.take();
                if let Some(notifier) = removed.as_ref() {
                    detached_registration = registry.entries.remove(&notifier.registration.id);
                }
                removed
            } else {
                None
            }
        }
    };
    // A rejected preallocated RT record may deallocate and release account
    // Arcs; do that only after the queue mutex is gone.
    drop(rejected);
    drop(detached_registration);
    if busy {
        return Err(AxError::ResourceBusy);
    }
    remove_notification(removed);
    Ok(0)
}

pub fn sys_mq_getsetattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    new_attr: *const MqAttr,
    old_attr: *mut MqAttr,
) -> AxResult<isize> {
    let file = get_mq_fd(fd)?;
    let new = if new_attr.is_null() {
        None
    } else {
        let attr = VmPtr::vm_read(new_attr, memory).map_err(map_usercopy_error)?;
        if attr.mq_flags & !(O_NONBLOCK as isize) != 0 {
            return Err(AxError::InvalidInput);
        }
        Some(attr)
    };

    if !old_attr.is_null() {
        let flags = nonblock_flags(&file);
        let queue = file.queue.lock();
        // SAFETY: `MqAttr` is a repr(C) array of initialized isize fields;
        // its reserved words are explicitly zeroed by `attr`.
        unsafe { VmMutPtr::vm_write_unchecked(old_attr, memory, queue.attr(flags)) }
            .map_err(map_usercopy_error)?;
    }
    if let Some(attr) = new {
        let nonblocking = attr.mq_flags & O_NONBLOCK as isize != 0;
        file.transition_status_flags(
            |old| (old.raw() & !O_NONBLOCK) | if nonblocking { O_NONBLOCK } else { 0 },
            |old, new| {
                if old.nonblocking() != new.nonblocking() {
                    file.set_nonblocking(new.nonblocking())?;
                }
                Ok(())
            },
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::{mem::MaybeUninit, ops::Range};

    use thekernel_linux_usercopy::{UserCopyError, VmResult};

    use super::*;

    struct TestMemory {
        bytes: Vec<u8>,
    }

    impl TestMemory {
        fn range(&self, start: usize, len: usize) -> Result<Range<usize>, UserCopyError> {
            let end = start.checked_add(len).ok_or(UserCopyError::BadAddress)?;
            (end <= self.bytes.len())
                .then_some(start..end)
                .ok_or(UserCopyError::BadAddress)
        }
    }

    // SAFETY: TestMemory bounds-checks the opaque user address and initializes
    // every destination byte before returning a successful read.
    unsafe impl UserMemory for TestMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            let range = self.range(start, dst.len())?;
            for (output, input) in dst.iter_mut().zip(&self.bytes[range]) {
                output.write(*input);
            }
            Ok(())
        }

        fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
            let range = self.range(start, src.len())?;
            self.bytes[range].copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn mqueue_usercopy_helpers_snapshot_unaligned_inputs() {
        let mut provider = TestMemory {
            bytes: vec![0; 128],
        };
        let name_addr = 3;
        provider.bytes[name_addr..name_addr + 8].copy_from_slice(b"/queue\0\0");
        let attr_addr = 19;
        let attr = MqAttr {
            mq_flags: O_NONBLOCK as isize,
            mq_maxmsg: 4,
            mq_msgsize: 64,
            mq_curmsgs: 99,
            __reserved: [7; 4],
        };
        let attr_bytes = unsafe {
            core::slice::from_raw_parts((&attr as *const MqAttr).cast::<u8>(), size_of::<MqAttr>())
        };
        provider.bytes[attr_addr..attr_addr + attr_bytes.len()].copy_from_slice(attr_bytes);

        let (name, copied_attr) = {
            let mut memory = UserMemoryContext::new(&mut provider);
            let name = normalize_name(&mut memory, name_addr as *const c_char).unwrap();
            let copied_attr = read_create_attr(&mut memory, attr_addr as *const MqAttr).unwrap();
            (name, copied_attr)
        };

        assert_eq!(name, "queue");
        assert_eq!(copied_attr.mq_flags, 0);
        assert_eq!(copied_attr.mq_maxmsg, 4);
        assert_eq!(copied_attr.mq_msgsize, 64);
        assert_eq!(copied_attr.mq_curmsgs, 0);
        assert_eq!(copied_attr.__reserved, [0; 4]);
    }

    #[test]
    fn mqueue_usercopy_helper_preserves_error_mapping() {
        let mut provider = TestMemory { bytes: vec![0; 8] };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            normalize_name(&mut memory, usize::MAX as *const c_char),
            Err(AxError::BadAddress)
        );
        assert_eq!(
            map_usercopy_error(UserCopyError::NoMemory),
            AxError::NoMemory
        );
        assert_eq!(
            map_usercopy_error(UserCopyError::TooLong),
            AxError::NameTooLong
        );
    }

    #[test]
    fn mq_signal_info_uses_sender_snapshot_and_target_uid_mapping() {
        let root = UserNamespace::try_new_root().unwrap();
        let target = root
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let target_pid_ns = PidNamespace::try_new_root(root.clone()).unwrap();
        let uid_map = target
            .try_build_uid_map(vec![crate::task::IdMapInputExtent::new(0, 1000, 1)])
            .unwrap();
        target.publish_uid_map(uid_map).unwrap();

        let registration_info = SignalInfo::new_rt(
            Signo::SIGRTMIN,
            SI_MESGQ,
            SignalRtPayload::new(0, 0, 0xfeed),
        );
        let sender = MqSender {
            pid: 4242,
            real_uid: Kuid::from_raw(1000).unwrap(),
            pid_ns: target_pid_ns.clone(),
        };
        let info = mq_signal_info_for_sender(&registration_info, &target, &target_pid_ns, sender);

        assert_eq!(info.rt_payload(), SignalRtPayload::new(4242, 0, 0xfeed));
    }

    #[test]
    fn mq_signal_info_maps_sender_pid_to_target_pid_namespace() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let root_pid_ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        let child_pid_ns = root_pid_ns.try_fork(100, user_ns.clone()).unwrap();
        let registration_info = SignalInfo::new_rt(
            Signo::SIGRTMIN,
            SI_MESGQ,
            SignalRtPayload::new(0, 0, 0xbeef),
        );

        let same_namespace = mq_signal_info_for_sender(
            &registration_info,
            &user_ns,
            &root_pid_ns,
            MqSender {
                pid: 4242,
                real_uid: Kuid::INITIAL_ROOT,
                pid_ns: root_pid_ns.clone(),
            },
        );
        assert_eq!(same_namespace.rt_payload().pid, 4242);

        let invisible_external_sender = mq_signal_info_for_sender(
            &registration_info,
            &user_ns,
            &child_pid_ns,
            MqSender {
                pid: 4242,
                real_uid: Kuid::INITIAL_ROOT,
                pid_ns: root_pid_ns.clone(),
            },
        );
        assert_eq!(invisible_external_sender.rt_payload().pid, 0);

        let visible_nested_sender = mq_signal_info_for_sender(
            &registration_info,
            &user_ns,
            &child_pid_ns,
            MqSender {
                pid: 4242,
                real_uid: Kuid::INITIAL_ROOT,
                pid_ns: child_pid_ns.clone(),
            },
        );
        assert_eq!(visible_nested_sender.rt_payload().pid, 4242);

        let visible_nested_init = mq_signal_info_for_sender(
            &registration_info,
            &user_ns,
            &child_pid_ns,
            MqSender {
                pid: 100,
                real_uid: Kuid::INITIAL_ROOT,
                pid_ns: child_pid_ns.clone(),
            },
        );
        assert_eq!(visible_nested_init.rt_payload().pid, 1);
    }

    fn install_accounted_notification(
        pid: u32,
    ) -> (
        Arc<Mutex<PosixMqueue>>,
        Arc<MqNotificationToken>,
        Arc<thekernel_linux_signal::SignalQueueAccount>,
        Arc<thekernel_linux_signal::SignalQueueAccount>,
    ) {
        let queue = Arc::new(Mutex::new(
            PosixMqueue::new(
                String::from("registry-account-test"),
                0o600,
                pid,
                0,
                default_attr(),
            )
            .unwrap(),
        ));
        let token = new_notification_token().unwrap();
        let per_user = thekernel_linux_signal::SignalQueueAccount::try_new(4).unwrap();
        let global = thekernel_linux_signal::SignalQueueAccount::try_new(4).unwrap();
        let info = SignalInfo::new_rt(Signo::SIGRTMIN, SI_MESGQ, SignalRtPayload::new(0, 0, 0));
        let prepared = PreparedSignal::try_accounted(info.clone(), &per_user, 4, &global).unwrap();
        let target_user_ns = UserNamespace::try_new_root().unwrap();
        let target_pid_ns = PidNamespace::try_new_root(target_user_ns.clone()).unwrap();
        queue.lock().notifier = Some(MqNotifier {
            pid,
            notify: SIGEV_SIGNAL as i32,
            thread: None,
            signal: Some(MqSignalNotifier {
                target: Weak::new(),
                target_user_ns,
                target_pid_ns,
                info,
                prepared,
            }),
            registration: token.clone(),
        });
        MQ_NOTIFICATION_REGISTRY.lock().entries.insert(
            token.id,
            MqNotificationRegistration {
                owner: pid,
                queue: Arc::downgrade(&queue),
                token: token.clone(),
            },
        );
        (queue, token, per_user, global)
    }

    fn install_inert_notification(pid: u32) -> (Arc<Mutex<PosixMqueue>>, Arc<MqNotificationToken>) {
        let queue = Arc::new(Mutex::new(
            PosixMqueue::new(String::from("registry-test"), 0o600, pid, 0, default_attr()).unwrap(),
        ));
        let token = new_notification_token().unwrap();
        queue.lock().notifier = Some(MqNotifier {
            pid,
            notify: SIGEV_NONE as i32,
            thread: None,
            signal: None,
            registration: token.clone(),
        });
        MQ_NOTIFICATION_REGISTRY.lock().entries.insert(
            token.id,
            MqNotificationRegistration {
                owner: pid,
                queue: Arc::downgrade(&queue),
                token: token.clone(),
            },
        );
        (queue, token)
    }

    #[test]
    fn owner_exit_cancels_unlinked_but_open_notification() {
        let pid = 0xf001;
        let (queue, token) = install_inert_notification(pid);
        cleanup_process_mqueue_notifications(pid);

        assert!(queue.lock().notifier.is_none());
        assert!(!token.active.load(Ordering::Acquire));
        assert!(
            !MQ_NOTIFICATION_REGISTRY
                .lock()
                .entries
                .contains_key(&token.id)
        );
    }

    #[test]
    fn owner_exit_wins_against_a_notification_already_taken_for_delivery() {
        let pid = 0xf002;
        let (queue, token) = install_inert_notification(pid);
        let moved = queue.lock().notifier.take().unwrap();

        cleanup_process_mqueue_notifications(pid);
        assert!(!claim_notification(&moved));
        assert!(!token.active.load(Ordering::Acquire));
    }

    #[test]
    fn owner_exit_refunds_an_unlinked_queue_signal_reservation() {
        let pid = 0xf003;
        let (queue, token, per_user, global) = install_accounted_notification(pid);
        assert_eq!(per_user.queued(), 1);
        assert_eq!(global.queued(), 1);

        cleanup_process_mqueue_notifications(pid);
        assert!(queue.lock().notifier.is_none());
        assert!(!token.active.load(Ordering::Acquire));
        assert_eq!(per_user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }

    #[test]
    fn exit_cancellation_of_taken_notification_refunds_exactly_on_drop() {
        let pid = 0xf004;
        let (queue, token, per_user, global) = install_accounted_notification(pid);
        let moved = queue.lock().notifier.take().unwrap();

        cleanup_process_mqueue_notifications(pid);
        assert!(!claim_notification(&moved));
        assert!(!token.active.load(Ordering::Acquire));
        assert_eq!(per_user.queued(), 1);
        assert_eq!(global.queued(), 1);

        drop(moved);
        assert_eq!(per_user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }
}
