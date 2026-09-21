use alloc::{
    borrow::Cow,
    collections::BTreeMap,
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
use axfs_ng_vfs::{FsNameBuf, FsPathBuf};
use axhal::time::wall_time;
use axpoll::{IoEvents, PollSet, Pollable};
#[cfg(not(test))]
use axsync::Mutex;
use axtask::{WaitError, WaitQueue, current};
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::{
    __kernel_mode_t, CAP_DAC_OVERRIDE, CAP_FOWNER, CAP_SYS_RESOURCE, O_ACCMODE, O_CREAT, O_EXCL,
    O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, SI_MESGQ, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD,
    timespec,
};
// Host unit tests do not initialize a scheduler/current task. The ownership
// tests below exercise the same queue/registry critical sections with a spin
// mutex; blocking/wakeup behavior remains covered only by kernel/guest tests.
#[cfg(test)]
use spin::Mutex;
use tk_linux_ipc::{
    MqAttributeError, MqKey, MqLimits, MqNameError, MqUnlinkError, MqUnlinkRequest,
    authorize_mq_unlink, mq_notify_fires, mqueue_insert_prefix, validate_mq_attributes,
    validate_mq_name, validate_priority,
};
use tk_linux_signal::{PreparedSignal, SignalInfo, SignalRtPayload, Signo};
use tk_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load, vm_load_until_nul_bounded,
    vm_write_slice,
};

use super::{IpcNamespace, MqCharge};
use crate::{
    file::{
        File, FileHandle, FileLike, Kstat, NetlinkSocket, PseudoInode, add_file_like_with_flags,
        get_typed_file,
    },
    mm::map_usercopy_error,
    syscall::RawSigevent,
    task::{
        AsThread, Kgid, Kuid, PidNamespace, ProcStateHint, ProcessData, UserNamespace,
        prepare_queued_signal_for_process, send_prepared_signal_to_process_data,
        with_proc_state_hint,
    },
    time::TimeValueLike,
};

/// `DFLT_MSG` / `DFLT_MSGSIZE`: the attribute a queue created with a NULL
/// `mq_attr` inherits, before being clamped by the namespace maxima
/// (`min(ipc_ns->mq_msg_max, ipc_ns->mq_msg_default)` in `mqueue_get_inode()`).
const DEFAULT_MQ_MAXMSG: isize = tk_linux_ipc::MQ_MSG_MAX_DEFAULT as isize;
const DEFAULT_MQ_MSGSIZE: isize = tk_linux_ipc::MQ_MSGSIZE_MAX_DEFAULT as isize;
/// `NAME_MAX`, the longest queue name `simple_lookup()` accepts.
const MQ_NAME_MAX: usize = tk_linux_ipc::MQ_NAME_MAX;
/// `_NSIG` on x86_64, the upper bound Linux `valid_signal()` compares against.
const LINUX_NSIG: i32 = 64;
const NOTIFY_COOKIE_LEN: usize = 32;
const NOTIFY_WOKENUP: u8 = 1;
const NOTIFY_REMOVED: u8 = 2;
const MQ_INODE_SIZE: u64 = 80;

/// The mqueuefs root is created by `mqueue_fill_super()` as
/// `S_IFDIR | S_ISVTX | S_IRWXUGO`, owned by the task that mounted it. TheKernel
/// mounts the queue filesystem from its initial user namespace, so the
/// directory owner is uid 0 and `inode_permission(dir, MAY_WRITE | MAY_EXEC)`
/// always succeeds for the world-writable mode.
pub(crate) const MQUEUE_DIR_MODE: u16 = 0o1777;
pub(crate) const MQUEUE_DIR_UID: u32 = 0;

static MQ_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);

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

/// A message that has not entered `msg_tree`, i.e. the `struct msg_msg` Linux
/// holds between `load_msg()` and either `msg_insert()` or `pipelined_send()`.
struct MqOutgoing {
    priority: u32,
    sender: MqSender,
    data: Vec<u8>,
}

/// One task sleeping in `mq_timedreceive`, the analogue of Linux
/// `struct ext_wait_queue` on `info->e_wait_q[RECV]`.
struct MqReceiver {
    /// Set while [`PosixMqueue::receivers`] holds this waiter, so repeated
    /// attempt loops register exactly once.
    queued: AtomicBool,
    /// The message `pipelined_send()` handed over. It is written under the
    /// queue lock and never partially published, so the receiver either sees
    /// the whole message or nothing.
    slot: Mutex<Option<MqMessage>>,
    /// Wakes this task only, matching `wake_q_add_safe(wake_q, task)` in
    /// `__pipelined_op()`.
    waker: Arc<WaitQueue>,
}

/// One task sleeping in `mq_timedsend`, whose message `do_mq_timedsend()` has
/// already loaded into `ext_wait_queue::msg`.
struct MqSenderWaiter {
    /// Set while [`PosixMqueue::senders`] holds this waiter.
    queued: AtomicBool,
    /// Set by `pipelined_receive()` once the staged message reached `msg_tree`.
    handed_off: AtomicBool,
    /// The staged message, taken exactly once: either by `pipelined_receive()`
    /// or by the sender's own successful attempt.
    staged: Mutex<Option<MqOutgoing>>,
    waker: Arc<WaitQueue>,
}

/// Wakeups a completed queue operation owes after it releases the queue lock.
///
/// This is Linux's `DEFINE_WAKE_Q(wake_q)` plus the `wake_up(&info->wait_q)`
/// and `__do_notify()` work that `do_mq_timedsend()` performs once
/// `info->lock` is dropped.
#[derive(Default)]
struct MqPublish {
    /// A receiver served by `pipelined_send()`.
    receiver: Option<Arc<MqReceiver>>,
    /// A sender whose staged message `pipelined_receive()` inserted.
    sender: Option<Arc<MqSenderWaiter>>,
    /// A message entered `msg_tree`; `__do_notify()` always ends in
    /// `wake_up(&info->wait_q)`.
    readable: bool,
    /// Capacity was freed and no sender was waiting, so Linux falls back to
    /// `wake_up_interruptible(&info->wait_q)`.
    writable: bool,
    /// The one-shot `mq_notify` registration consumed by this operation, with
    /// the sending task the notification must attribute.
    notify: Option<(MqNotifier, MqSender)>,
}

#[derive(Clone)]
struct MqThreadNotifier {
    netlink: FileHandle<NetlinkSocket>,
    cookie: [u8; NOTIFY_COOKIE_LEN],
}

struct MqNotifier {
    pid: u32,
    // The namespace owns the manager which owns this queue.  This must be a
    // weak edge or an installed mq_notify registration makes the complete IPC
    // namespace self-retaining after its last task has gone away.
    ipc_ns: Option<Weak<IpcNamespace>>,
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

pub(crate) struct PosixMqueue {
    inode: PseudoInode,
    name: FsNameBuf,
    mode: __kernel_mode_t,
    uid: u32,
    gid: u32,
    maxmsg: usize,
    msgsize: usize,
    messages: Vec<MqMessage>,
    next_sequence: u64,
    notifier: Option<MqNotifier>,
    /// `info->e_wait_q[RECV]`: tasks sleeping in `mq_timedreceive`. Linux
    /// registers a waiter only while `msg_tree` is empty, so the queue never
    /// holds messages and waiting receivers at the same time.
    receivers: Vec<Arc<MqReceiver>>,
    /// `info->e_wait_q[SEND]`: tasks sleeping in `mq_timedsend` whose message
    /// is already loaded and parked.
    senders: Vec<Arc<MqSenderWaiter>>,
    readiness: Arc<MqReadiness>,
    /// Weak here avoids manager -> queue -> namespace retention; mqd file
    /// descriptions retain the namespace strongly while they are usable.
    ipc_ns: Weak<IpcNamespace>,
    /// Queue-lifetime RLIMIT_MSGQUEUE reservation. Open descriptors retain the
    /// queue after unlink, so this must not be tied to its directory name.
    charge: Option<MqCharge>,
}

pub(crate) struct MqReadiness {
    pub(crate) readable: PollSet,
    pub(crate) writable: PollSet,
}

pub(crate) struct MqManager {
    queues: BTreeMap<FsNameBuf, Arc<Mutex<PosixMqueue>>>,
}

struct MqNotificationRegistration {
    owner: u32,
    queue: Weak<Mutex<PosixMqueue>>,
    token: Arc<MqNotificationToken>,
}

pub(crate) struct MqNotificationRegistry {
    entries: BTreeMap<u64, MqNotificationRegistration>,
}

pub struct MqFd {
    queue: Arc<Mutex<PosixMqueue>>,
    ipc_ns: Arc<IpcNamespace>,
    readiness: Arc<MqReadiness>,
    access: MqAccess,
    nonblocking: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MqAccess {
    ReadOnly,
    WriteOnly,
    ReadWrite,
    /// `O_ACCMODE == O_RDWR | O_WRONLY`. Linux `prepare_open()` accepts this
    /// only on the create path, where `OPEN_FMODE()` maps it to a descriptor
    /// with neither `FMODE_READ` nor `FMODE_WRITE`.
    Unspecified,
}

impl MqManager {
    pub(crate) const fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
        }
    }

    pub(crate) fn names(&self) -> AxResult<Vec<FsNameBuf>> {
        let mut names = Vec::new();
        names
            .try_reserve(self.queues.len())
            .map_err(|_| AxError::NoMemory)?;
        for name in self.queues.keys() {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(name.len())
                .map_err(|_| AxError::NoMemory)?;
            bytes.extend_from_slice(name.as_bytes());
            names.push(FsNameBuf::from_vec(bytes).map_err(AxError::from)?);
        }
        Ok(names)
    }

    pub(crate) fn queue(&self, name: &FsNameBuf) -> Option<Arc<Mutex<PosixMqueue>>> {
        self.queues.get(name).cloned()
    }
}

/// mqueuefs binds a superblock to the IPC namespace active when it is mounted.
/// These helpers are deliberately namespace-explicit: VFS lookup/open paths
/// must not accidentally resolve queue names through whichever task happens to
/// execute a later operation on the mount.
pub(crate) fn mqueuefs_names(namespace: &IpcNamespace) -> AxResult<Vec<FsNameBuf>> {
    namespace.mqueue_manager().lock().names()
}

pub(crate) fn mqueuefs_lookup(
    namespace: &IpcNamespace,
    name: &FsNameBuf,
) -> AxResult<Arc<Mutex<PosixMqueue>>> {
    namespace
        .mqueue_manager()
        .lock()
        .queue(name)
        .ok_or(AxError::NotFound)
}

pub(crate) fn mqueuefs_unlink(namespace: &IpcNamespace, name: &FsNameBuf) -> AxResult<()> {
    // Keep the name table locked from lookup through removal.  Releasing it
    // between those two steps lets an unlink of an old queue remove a newly
    // created queue with the same name: mq_unlink removes the name while old
    // descriptors retain the queue object, so immediate reuse is legal.
    // The manager -> queue lock order is also the one used by mq_open.
    let mut manager = namespace.mqueue_manager().lock();
    let queue = manager.queue(name).ok_or(AxError::NotFound)?;
    let queue_guard = queue.lock();
    let curr = current();
    let cred = curr.as_thread().current_cred();
    check_unlink_authority(queue_guard.uid, &cred)?;
    drop(queue_guard);
    // `manager` has remained locked, so this removes exactly the object whose
    // ownership was checked above rather than a same-name successor.
    manager.queues.remove(name);
    Ok(())
}

/// Linux `vfs_unlink()` authority for one mqueuefs name.
///
/// `may_delete_dentry()` (`fs/namei.c`) runs `inode_permission(dir,
/// MAY_WRITE | MAY_EXEC)` first and `check_sticky()` second, and the mqueuefs
/// root is always the sticky, world-writable `01777` directory
/// `mqueue_fill_super()` creates. A caller that is neither the queue owner, nor
/// the owner of that directory, nor `CAP_FOWNER` is therefore rejected with
/// `-EPERM` rather than `-EACCES`.
fn check_unlink_authority(queue_uid: u32, cred: &crate::task::Cred) -> AxResult<()> {
    let request = MqUnlinkRequest {
        queue_uid,
        directory_uid: MQUEUE_DIR_UID,
        fsuid: cred.ids().fsuid.into_raw(),
        directory_sticky: MQUEUE_DIR_MODE & 0o1000 != 0,
        directory_write_exec: true,
        cap_fowner: cred.has_effective_capability(CAP_FOWNER),
    };
    match authorize_mq_unlink(request) {
        Ok(()) => Ok(()),
        Err(MqUnlinkError::StickyDirectory) => Err(AxError::from(LinuxError::EPERM)),
        Err(MqUnlinkError::DirectoryInaccessible) => Err(AxError::PermissionDenied),
    }
}

pub(crate) fn mqueuefs_metadata(queue: &Arc<Mutex<PosixMqueue>>) -> (u32, u32, u32, u64) {
    let queue = queue.lock();
    (queue.mode, queue.uid, queue.gid, MQ_INODE_SIZE)
}

pub(crate) fn mqueuefs_read(
    queue: &Arc<Mutex<PosixMqueue>>,
    destination: &mut [u8],
) -> AxResult<usize> {
    let (message, publish, readiness) = {
        let mut queue = queue.lock();
        if !has_queue_permission(&queue, MqAccess::ReadOnly) {
            return Err(AxError::PermissionDenied);
        }
        if destination.len() < queue.msgsize {
            return Err(LinuxError::EMSGSIZE.into());
        }
        let (message, publish) = queue.take_message().ok_or(AxError::WouldBlock)?;
        let readiness = Arc::clone(&queue.readiness);
        (message, publish, readiness)
    };
    publish.publish(&readiness);
    destination[..message.data.len()].copy_from_slice(&message.data);
    Ok(message.data.len())
}

pub(crate) fn mqueuefs_write(queue: &Arc<Mutex<PosixMqueue>>, source: &[u8]) -> AxResult<usize> {
    let mut data = Vec::new();
    data.try_reserve_exact(source.len())
        .map_err(|_| AxError::NoMemory)?;
    data.extend_from_slice(source);
    let outgoing = MqOutgoing {
        priority: 0,
        sender: current_mq_sender(),
        data,
    };
    let (publish, readiness) = {
        let mut queue = queue.lock();
        if !has_queue_permission(&queue, MqAccess::WriteOnly) {
            return Err(AxError::PermissionDenied);
        }
        if source.len() > queue.msgsize {
            return Err(LinuxError::EMSGSIZE.into());
        }
        if queue.messages.len() >= queue.maxmsg {
            return Err(AxError::WouldBlock);
        }
        let readiness = Arc::clone(&queue.readiness);
        (queue.dispatch_message(outgoing), readiness)
    };
    publish.publish(&readiness);
    Ok(source.len())
}

pub(crate) fn mqueuefs_readiness(queue: &Arc<Mutex<PosixMqueue>>) -> Arc<MqReadiness> {
    Arc::clone(&queue.lock().readiness)
}

pub(crate) fn mqueuefs_poll(queue: &Arc<Mutex<PosixMqueue>>) -> IoEvents {
    let queue = queue.lock();
    let mut events = IoEvents::empty();
    if !queue.messages.is_empty() {
        events |= IoEvents::READABLE;
    }
    if queue.messages.len() < queue.maxmsg {
        events |= IoEvents::WRITABLE;
    }
    events
}

impl MqNotificationRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl PosixMqueue {
    fn new(
        name: FsNameBuf,
        mode: __kernel_mode_t,
        uid: u32,
        gid: u32,
        attr: MqAttr,
        ipc_ns: &Arc<IpcNamespace>,
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
            receivers: Vec::new(),
            senders: Vec::new(),
            readiness,
            ipc_ns: Arc::downgrade(ipc_ns),
            charge: None,
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

    /// `msg_insert()` (`ipc/mqueue.c`).
    ///
    /// The stored order is the dequeue order decided by
    /// [`tk_linux_ipc::mqueue_dequeue_precedes`], so insertion into priority
    /// order also preserves FIFO among equal priorities. Returns whether the
    /// queue was empty, which is the empty-to-nonempty edge `__do_notify()`
    /// keys off (`info->attr.mq_curmsgs == 1` after the insert).
    fn insert_message(&mut self, outgoing: MqOutgoing) -> bool {
        let was_empty = self.messages.is_empty();
        let key = MqKey {
            priority: outgoing.priority,
            sequence: self.next_sequence,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);

        let index = self.messages.partition_point(|stored| {
            mqueue_insert_prefix(
                MqKey {
                    priority: stored.priority,
                    sequence: stored.sequence,
                },
                key,
            )
        });
        self.messages.insert(
            index,
            MqMessage {
                priority: outgoing.priority,
                sequence: key.sequence,
                sender: outgoing.sender,
                data: outgoing.data,
            },
        );
        was_empty
    }

    /// `msg_get()` (`ipc/mqueue.c`): the rightmost `msg_tree` leaf first, i.e.
    /// the highest priority with the oldest message at that priority.
    fn pop_message(&mut self) -> Option<MqMessage> {
        if self.messages.is_empty() {
            None
        } else {
            Some(self.messages.remove(0))
        }
    }

    /// `pipelined_send()` (`ipc/mqueue.c`): hand `outgoing` straight to the
    /// task waiting in `mq_timedreceive()` without inserting it into
    /// `msg_tree`. The caller publishes the returned waiter's wakeup after
    /// dropping the queue lock.
    fn pipeline_to_receiver(&mut self, outgoing: MqOutgoing) -> Option<Arc<MqReceiver>> {
        let receiver = self.receivers.first().cloned()?;
        self.receivers.remove(0);
        receiver.queued.store(false, Ordering::Release);
        let key = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        *receiver.slot.lock() = Some(MqMessage {
            priority: outgoing.priority,
            sequence: key,
            sender: outgoing.sender,
            data: outgoing.data,
        });
        Some(receiver)
    }

    /// `pipelined_receive()` (`ipc/mqueue.c`): the slot freed by `msg_get()` is
    /// transferred to the first sleeping sender, whose message is inserted in
    /// its place instead of waking that sender to contend for the capacity.
    ///
    /// Returns `None` when no sender is waiting, which is Linux's
    /// `wake_up_interruptible(&info->wait_q)` path.
    fn pipeline_receive(&mut self) -> Option<Arc<MqSenderWaiter>> {
        let sender = self.senders.first().cloned()?;
        self.senders.remove(0);
        sender.queued.store(false, Ordering::Release);
        match sender.take_staged() {
            Some(outgoing) => {
                self.insert_message(outgoing);
                sender.handed_off.store(true, Ordering::Release);
                Some(sender)
            }
            // A queued sender always has its staged message; Linux refuses to
            // insert a message-less waiter too and leaves the slot to the
            // regular readiness wakeup.
            None => None,
        }
    }

    /// `wq_sleep()`'s parking half plus the `wq_add()` position rule.
    ///
    /// `wq_add()` (`ipc/mqueue.c:689`) inserts before the first waiter whose
    /// `task->prio` is at least as strong as `current->prio`, and
    /// `wq_get_first_waiter()` returns the list tail, so Linux serves the
    /// strongest waiter and the oldest of the equally strong ones. TheKernel
    /// parks in arrival order and serves `first()`, which reproduces the FIFO
    /// half of that rule but **not** the scheduler-priority half: two waiters
    /// with different nice values are served oldest-first here and
    /// strongest-first by Linux. The primitive Linux orders by exists as
    /// `tk-axtask::pi_kernel_priority()` (`kernel/src/task/futex.rs`) over
    /// `task_scheduling_snapshot()`; ordering these vectors by it, with the
    /// strongest waiter last, is what closing that divergence needs.
    fn park_receiver(&mut self, receiver: &Arc<MqReceiver>) {
        if !receiver.queued.swap(true, Ordering::AcqRel) {
            self.receivers.push(receiver.clone());
        }
    }

    /// Removes a receiver from `e_wait_q[RECV]`; idempotent, because the
    /// handoff path may already have taken it.
    fn drop_receiver(&mut self, receiver: &Arc<MqReceiver>) {
        if receiver.queued.swap(false, Ordering::AcqRel) {
            self.receivers
                .retain(|waiter| !Arc::ptr_eq(waiter, receiver));
        }
    }

    /// Sender-side counterpart of [`Self::park_receiver`], with the same
    /// `wq_add()` priority divergence.
    fn park_sender(&mut self, sender: &Arc<MqSenderWaiter>) {
        if !sender.queued.swap(true, Ordering::AcqRel) {
            self.senders.push(sender.clone());
        }
    }

    fn drop_sender(&mut self, sender: &Arc<MqSenderWaiter>) {
        if sender.queued.swap(false, Ordering::AcqRel) {
            self.senders.retain(|waiter| !Arc::ptr_eq(waiter, sender));
        }
    }

    /// The `msg_insert()` / `pipelined_send()` decision of `do_mq_timedsend()`
    /// once the caller has established that the queue has room.
    ///
    /// Linux consults `wq_get_first_waiter(info, RECV)` before touching
    /// `msg_tree`, so a blocked receiver is served by a direct handoff: the
    /// message never becomes visible through `poll()` and never fires the
    /// one-shot notification.
    fn dispatch_message(&mut self, outgoing: MqOutgoing) -> MqPublish {
        // A waiting receiver is served first; the queue never holds messages
        // and blocked receivers at the same time, so this branch and the
        // insertion branch are mutually exclusive.
        if self.receivers.is_empty() {
            let sender = outgoing.sender.clone();
            self.insert_message(outgoing);
            let mut publish = MqPublish {
                readable: true,
                ..MqPublish::default()
            };
            if mq_notify_fires(self.notifier.is_some(), false, self.messages.len()) {
                publish.notify = self.notifier.take().map(|notifier| (notifier, sender));
            }
            return publish;
        }
        MqPublish {
            receiver: self.pipeline_to_receiver(outgoing),
            ..MqPublish::default()
        }
    }

    /// One `do_mq_timedsend()` attempt with the queue lock held.
    ///
    /// Like Linux, a full queue sleeps the sender before the receiver check
    /// runs, and `park` mirrors `wq_sleep()` being the only path that publishes
    /// the waiter: the non-blocking `-EAGAIN` path never registers.
    fn attempt_send(
        &mut self,
        waiter: &Arc<MqSenderWaiter>,
        park: bool,
    ) -> AxResult<MqSendAttempt> {
        if waiter.handed_off.load(Ordering::Acquire) {
            return Ok(MqSendAttempt::Sent(MqPublish::default()));
        }
        if self.messages.len() >= self.maxmsg {
            if park {
                self.park_sender(waiter);
            }
            return Ok(MqSendAttempt::Sleep);
        }
        let Some(outgoing) = waiter.take_staged() else {
            return Err(AxError::BadState);
        };
        self.drop_sender(waiter);
        Ok(MqSendAttempt::Sent(self.dispatch_message(outgoing)))
    }

    /// The `msg_get()` / `pipelined_receive()` pair of `do_mq_timedreceive()`.
    fn take_message(&mut self) -> Option<(MqMessage, MqPublish)> {
        let message = self.pop_message()?;
        let mut publish = MqPublish::default();
        match self.pipeline_receive() {
            Some(sender) => publish.sender = Some(sender),
            None => publish.writable = true,
        }
        Some((message, publish))
    }

    /// One `do_mq_timedreceive()` attempt with the queue lock held.
    fn attempt_receive(
        &mut self,
        waiter: &Arc<MqReceiver>,
        park: bool,
    ) -> Option<(MqMessage, MqPublish)> {
        if let Some(message) = waiter.take_slot() {
            return Some((message, MqPublish::default()));
        }
        if let Some(taken) = self.take_message() {
            return Some(taken);
        }
        if park {
            self.park_receiver(waiter);
        }
        None
    }
}

impl MqReceiver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queued: AtomicBool::new(false),
            slot: Mutex::new(None),
            waker: Arc::new(WaitQueue::new()),
        })
    }

    fn take_slot(&self) -> Option<MqMessage> {
        self.slot.lock().take()
    }
}

impl MqSenderWaiter {
    fn new(outgoing: MqOutgoing) -> Arc<Self> {
        Arc::new(Self {
            queued: AtomicBool::new(false),
            handed_off: AtomicBool::new(false),
            staged: Mutex::new(Some(outgoing)),
            waker: Arc::new(WaitQueue::new()),
        })
    }

    fn take_staged(&self) -> Option<MqOutgoing> {
        self.staged.lock().take()
    }
}

/// Outcome of one `do_mq_timedsend()` attempt.
// `MqPublish` carries its deferred wakeups inline; boxing it would add an
// allocation to every send, so the variant size difference is accepted.
#[allow(clippy::large_enum_variant)]
enum MqSendAttempt {
    /// The message reached the queue or a receiver.
    Sent(MqPublish),
    /// Linux `wq_sleep(info, SEND, timeout, &wait)`.
    Sleep,
}

impl MqPublish {
    /// Publishes every deferred wakeup. Must run after the queue lock is
    /// dropped, because a woken waiter immediately re-enters the queue.
    fn publish(self, readiness: &MqReadiness) {
        if self.readable {
            readiness.readable.wake();
        }
        if self.writable {
            readiness.writable.wake();
        }
        if let Some(receiver) = self.receiver {
            receiver.waker.notify_one(false);
        }
        if let Some(sender) = self.sender {
            sender.waker.notify_one(false);
        }
        if let Some((notifier, sender)) = self.notify {
            maybe_notify(Some(notifier), sender);
        }
    }
}

impl Drop for PosixMqueue {
    fn drop(&mut self) {
        if let Some(notifier) = self.notifier.as_ref() {
            notifier.registration.active.store(false, Ordering::Release);
            // A named queue can outlive its directory entry through open file
            // descriptions.  Its final drop may consequently run in an
            // unrelated task and IPC namespace, so never consult `current()`
            // here: the registration belongs to the namespace captured when
            // mq_notify installed it.
            if let Some(ipc_ns) = notifier.ipc_ns.as_ref().and_then(Weak::upgrade) {
                unregister_notification(&ipc_ns, notifier.registration.id);
            }
        }
    }
}

impl MqAccess {
    /// Linux `prepare_open()` decodes `oflag & O_ACCMODE` through
    /// `oflag2acc[]`, whose third entry is `O_RDWR | O_WRONLY`. That value is
    /// only rejected for an *existing* queue; a create request keeps it and
    /// `OPEN_FMODE()` then yields a descriptor that can neither read nor write.
    fn from_flags(flags: i32) -> Self {
        match (flags as u32) & O_ACCMODE {
            O_RDONLY => Self::ReadOnly,
            O_WRONLY => Self::WriteOnly,
            O_RDWR => Self::ReadWrite,
            _ => Self::Unspecified,
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
    fn new(
        queue: Arc<Mutex<PosixMqueue>>,
        ipc_ns: Arc<IpcNamespace>,
        access: MqAccess,
        nonblocking: bool,
    ) -> Self {
        let readiness = Arc::clone(&queue.lock().readiness);
        Self {
            queue,
            ipc_ns,
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

    fn update_timestamps(
        &self,
        atime: Option<axfs_ng_vfs::Timestamp>,
        mtime: Option<axfs_ng_vfs::Timestamp>,
        ctime: axfs_ng_vfs::Timestamp,
    ) -> AxResult<()> {
        self.queue
            .lock()
            .inode
            .update_timestamps(atime, mtime, ctime);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        // Reserve the protocol maximum before taking the queue lock so path
        // rendering cannot allocate while the queue's spin mutex is held.
        let mut path = Vec::new();
        path.try_reserve_exact(MQ_NAME_MAX + 1)
            .map_err(|_| AxError::NoMemory)?;
        path.push(b'/');
        let queue = self.queue.lock();
        debug_assert!(queue.name.len() <= MQ_NAME_MAX);
        path.extend_from_slice(queue.name.as_bytes());
        Ok(Cow::Owned(FsPathBuf::from_vec(path)))
    }

    fn nonblocking(&self) -> bool {
        self.is_nonblocking()
    }

    /// Linux `mqueue_flush_file()` (`ipc/mqueue.c:658-668`):
    ///
    /// ```c
    /// 	spin_lock(&info->lock);
    /// 	if (task_tgid(current) == info->notify_owner)
    /// 		remove_notification(info);
    ///
    /// 	spin_unlock(&info->lock);
    /// ```
    ///
    /// `.flush` runs for *every* descriptor of the queue the owning process
    /// closes, not only for the last one and not only for the descriptor the
    /// registration named, so the one-shot `mq_notify` registration is gone
    /// after `mq_open()` + `close()` of a sibling descriptor. `current()`
    /// resolves the same `task_tgid` the registration recorded in
    /// [`MqNotifier::pid`]; a close performed by any other process leaves the
    /// registration armed.
    fn flush_on_close(&self) {
        // Every Linux `filp_close()` site runs in task context. A close that
        // somehow cannot name a current task has no `task_tgid(current)` to
        // compare against, so it leaves the registration alone rather than
        // guessing.
        if !axtask::can_block_current() {
            return;
        }
        let curr = current();
        let pid = curr.as_thread().proc_data.proc.pid();
        let removed = {
            let mut queue = self.queue.lock();
            if queue
                .notifier
                .as_ref()
                .is_some_and(|notifier| notifier.pid == pid)
            {
                queue.notifier.take()
            } else {
                None
            }
        };
        remove_notification(removed);
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

/// The three `/proc/sys/fs/mqueue` ceilings of the *calling task's* IPC
/// namespace, exactly as Linux's `set_lookup()` resolves
/// `current->nsproxy->ipc_ns->mq_set` (`ipc/mq_sysctl.c:66-70`).  A write in
/// one namespace must never change what another namespace reads or enforces.
pub(crate) fn mq_queues_max() -> usize {
    current().as_thread().ipc_ns().mq_queues_max()
}

pub(crate) fn set_mq_queues_max(value: usize) {
    current().as_thread().ipc_ns().set_mq_queues_max(value);
}

pub(crate) fn mq_msg_max() -> usize {
    current().as_thread().ipc_ns().mq_msg_max()
}

pub(crate) fn set_mq_msg_max(value: usize) -> AxResult<()> {
    current().as_thread().ipc_ns().set_mq_msg_max(value)
}

pub(crate) fn mq_msgsize_max() -> usize {
    current().as_thread().ipc_ns().mq_msgsize_max()
}

pub(crate) fn set_mq_msgsize_max(value: usize) -> AxResult<()> {
    current().as_thread().ipc_ns().set_mq_msgsize_max(value)
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
        pid_ns: thread.pid_ns(),
    }
}

fn normalize_name<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const c_char,
) -> AxResult<FsNameBuf> {
    // `do_mq_open()` reads the name with `CLASS(filename, name)(u_name)` and
    // hands it to `start_creating_noperm(mnt->mnt_root, &QSTR(name->name))`
    // unchanged: the kernel's queue name is the raw syscall name, so a leading
    // slash is rejected with `EACCES` like any other slash (libc strips it by
    // passing `name + 1`). The bound scans one byte past `NAME_MAX` so that an
    // over-long name reports Linux's `ENAMETOOLONG` instead of running on.
    let raw = vm_load_until_nul_bounded(memory, name.cast::<u8>(), MQ_NAME_MAX + 2)
        .map_err(map_usercopy_error)?;
    match validate_mq_name(&raw) {
        Ok(()) => {}
        Err(MqNameError::Empty) => return Err(AxError::NotFound),
        Err(MqNameError::Invalid) => return Err(AxError::PermissionDenied),
        Err(MqNameError::TooLong) => return Err(AxError::NameTooLong),
    }
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(raw.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(&raw);
    FsNameBuf::from_vec(owned).map_err(Into::into)
}

/// The attribute `mqueue_get_inode()` gives a queue created without an
/// `mq_attr`: `min(ipc_ns->mq_msg_max, ipc_ns->mq_msg_default)` and
/// `min(ipc_ns->mq_msgsize_max, ipc_ns->mq_msgsize_default)`.
fn default_attr(namespace: &IpcNamespace) -> MqAttr {
    MqAttr {
        mq_flags: 0,
        mq_maxmsg: namespace
            .mq_msg_max()
            .min(DEFAULT_MQ_MAXMSG as usize) as isize,
        mq_msgsize: namespace
            .mq_msgsize_max()
            .min(DEFAULT_MQ_MSGSIZE as usize) as isize,
        mq_curmsgs: 0,
        __reserved: [0; 4],
    }
}

/// Reads the create-time `mq_attr`, if any.
///
/// Linux `SYSCALL_DEFINE4(mq_open)` copies the whole structure up front, so a
/// bad pointer is `-EFAULT` even when the queue already exists and the
/// attributes are ignored. The limits are checked later, only on the create
/// path, by [`read_created_attributes`].
fn read_create_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: *const MqAttr,
) -> AxResult<Option<MqAttr>> {
    if attr.is_null() {
        return Ok(None);
    }
    let attr = VmPtr::vm_read(attr, memory).map_err(map_usercopy_error)?;
    Ok(Some(MqAttr {
        mq_flags: 0,
        mq_curmsgs: 0,
        __reserved: [0; 4],
        ..attr
    }))
}

/// The namespace maxima and the caller's `CAP_SYS_RESOURCE` state.
fn create_limits(namespace: &IpcNamespace) -> (MqLimits, bool) {
    let curr = current();
    let thread = curr.as_thread();
    (
        MqLimits {
            queues: namespace.mq_queues_max(),
            max_messages: namespace.mq_msg_max(),
            max_message_size: namespace.mq_msgsize_max(),
        },
        thread
            .current_cred()
            .has_effective_capability(CAP_SYS_RESOURCE),
    )
}

/// Applies the `mqueue_get_inode()` admission rules to a requested attribute.
///
/// `CAP_SYS_RESOURCE` replaces the namespace maxima with `HARD_MSGMAX` /
/// `HARD_MSGSIZEMAX`; `EINVAL` from the size checks always precedes the
/// `EOVERFLOW` of the charge computation.
fn read_created_attributes(
    requested: Option<MqAttr>,
    namespace: &IpcNamespace,
    limits: MqLimits,
    cap_sys_resource: bool,
) -> AxResult<MqAttr> {
    let attr = requested.unwrap_or_else(|| default_attr(namespace));
    match validate_mq_attributes(
        attr.mq_maxmsg as i64,
        attr.mq_msgsize as i64,
        limits,
        cap_sys_resource,
    ) {
        Ok(_) => Ok(attr),
        Err(MqAttributeError::Invalid) => Err(AxError::InvalidInput),
        Err(MqAttributeError::Overflow) => Err(AxError::from(LinuxError::EOVERFLOW)),
    }
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
        // `prepare_open()` rejects this access mode for an existing queue
        // before it ever asks for permission.
        MqAccess::Unspecified => false,
    }
}

fn get_mq_fd(fd: i32) -> AxResult<crate::file::FileHandle<MqFd>> {
    get_typed_file::<MqFd>(fd).map_err(|_| AxError::BadFileDescriptor)
}

/// mq_open creates `MqFd`, while opening the same queue through a mounted
/// mqueuefs creates the normal VFS `File` wrapper.  Both must resolve to this
/// one queue object for mq_notify and attribute operations.
fn get_mq_queue(fd: i32) -> AxResult<(Arc<Mutex<PosixMqueue>>, Arc<IpcNamespace>)> {
    if let Ok(file) = get_mq_fd(fd) {
        return Ok((file.queue.clone(), file.ipc_ns.clone()));
    }
    let file = get_typed_file::<File>(fd).map_err(|_| AxError::BadFileDescriptor)?;
    let queue = crate::pseudofs::mqueue::queue_for_location(file.inner().location())
        .ok_or(AxError::BadFileDescriptor)?;
    let ipc_ns = queue.lock().ipc_ns.upgrade().ok_or(AxError::BadState)?;
    Ok((queue, ipc_ns))
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
            // `do_mq_notify()` guards the signal mode with Linux `valid_signal()`,
            // which only checks the upper bound:
            //
            // 	static inline int valid_signal(unsigned long sig)
            // 	{
            // 		return sig <= _NSIG ? 1 : 0;
            // 	}
            //
            // so `sigev_signo == 0` is a *successful* registration even though
            // `__do_notify()` then skips it ("do_mq_notify() accepts
            // sigev_signo == 0, why??") and still consumes the one-shot
            // registration. A negative signo is converted to a huge unsigned
            // value by that prototype and stays `-EINVAL`.
            if (0..=LINUX_NSIG).contains(&event.signo()) {
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
    let thread_owner = curr.as_thread();
    let proc_data = &thread_owner.proc_data;
    let mut notifier = MqNotifier {
        pid: proc_data.proc.pid(),
        ipc_ns: Some(Arc::downgrade(&thread_owner.ipc_ns())),
        notify: event.notify(),
        thread: None,
        signal: None,
        registration: new_notification_token()?,
    };

    if event.notify() == SIGEV_THREAD as i32 {
        // `do_mq_notify()` copies the notification cookie before it resolves
        // the netlink descriptor, so a bad cookie pointer is `-EFAULT` and a
        // bad descriptor is `-EBADF`, never the other way round.
        let cookie_ptr = event.value_ptr_address() as *const u8;
        let cookie_data =
            vm_load(memory, cookie_ptr, NOTIFY_COOKIE_LEN).map_err(map_usercopy_error)?;
        let mut cookie = [0u8; NOTIFY_COOKIE_LEN];
        cookie.copy_from_slice(&cookie_data);
        let netlink =
            NetlinkSocket::from_fd(event.signo()).map_err(|_| AxError::BadFileDescriptor)?;
        notifier.thread = Some(MqThreadNotifier { netlink, cookie });
    } else if event.notify() == SIGEV_SIGNAL as i32
        && let Some(signo) = u8::try_from(event.signo()).ok().and_then(Signo::from_repr)
    {
        // `sigev_signo == 0` passes `valid_signal()` but has no `Signo`, so the
        // registration stays record-free: `maybe_notify()` consumes it at the
        // empty edge and sends nothing, exactly like `__do_notify()`.
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

/// One `wq_sleep()` of `ipc/mqueue.c` for a single mqueue waiter.
///
/// `attempt` is the operation's own "is the message there / is there room"
/// test and runs outside the synchronous block session. `abandon` re-tests the
/// handoff after the wait gives up, because Linux re-checks
/// `ext_wait_queue::state` under `info->lock` before reporting `-ETIMEDOUT` or
/// `-ERESTARTSYS`: a message that was pipelined while the waiter was leaving
/// still completes the syscall, and a message is never orphaned in a slot whose
/// owner walked away.
fn mq_sleep<T>(
    waker: &WaitQueue,
    deadline: Option<Duration>,
    state: &mut T,
    mut attempt: impl FnMut(&mut T) -> AxResult<bool>,
    mut abandon: impl FnMut(&mut T) -> AxResult<bool>,
) -> AxResult<()> {
    let mut failure = None;
    let mut ready = || match attempt(state) {
        Ok(ready) => ready,
        Err(error) => {
            failure = Some(error);
            true
        }
    };

    // The absolute CLOCK_REALTIME deadline is converted to a remaining
    // duration on every entry, so a spurious wakeup cannot extend the wait and
    // an already-expired deadline times out immediately.
    let waited = match deadline {
        Some(end) => {
            waker.wait_timeout_until_interruptible(end.saturating_sub(wall_time()), &mut ready)
        }
        None => waker.wait_until_interruptible(&mut ready).map(|()| false),
    };

    if let Some(error) = failure {
        return Err(error);
    }
    match waited {
        Ok(false) => Ok(()),
        Ok(true) => {
            if abandon(state)? {
                Ok(())
            } else {
                Err(AxError::TimedOut)
            }
        }
        Err(WaitError::Interrupted) => {
            if abandon(state)? {
                Ok(())
            } else {
                Err(AxError::Interrupted)
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Blocks one `mq_timedsend` until its staged message is accepted.
fn send_blocking(
    file: &MqFd,
    waiter: &Arc<MqSenderWaiter>,
    deadline: Option<Duration>,
) -> AxResult<()> {
    let queue_handle = Arc::clone(&file.queue);
    let readiness = Arc::clone(&file.readiness);
    let mut completed = false;
    with_proc_state_hint(ProcStateHint::Interruptible, || {
        mq_sleep(
            &waiter.waker,
            deadline,
            &mut completed,
            |completed| {
                if *completed {
                    return Ok(true);
                }
                let publish = {
                    let mut queue = queue_handle.lock();
                    match queue.attempt_send(waiter, true)? {
                        MqSendAttempt::Sent(publish) => {
                            *completed = true;
                            publish
                        }
                        MqSendAttempt::Sleep => MqPublish::default(),
                    }
                };
                publish.publish(&readiness);
                Ok(*completed)
            },
            |completed| {
                let mut queue = queue_handle.lock();
                queue.drop_sender(waiter);
                if waiter.handed_off.load(Ordering::Acquire) {
                    *completed = true;
                }
                Ok(*completed)
            },
        )
    })
}

/// Blocks one `mq_timedreceive` until a message is owned by the caller.
fn receive_blocking(
    file: &MqFd,
    deadline: Option<Duration>,
    owned: &mut Option<MqMessage>,
) -> AxResult<()> {
    let waiter = MqReceiver::new();
    let queue_handle = Arc::clone(&file.queue);
    let readiness = Arc::clone(&file.readiness);
    let wait = with_proc_state_hint(ProcStateHint::Interruptible, || {
        mq_sleep(
            &waiter.waker,
            deadline,
            owned,
            |owned| {
                if owned.is_some() {
                    return Ok(true);
                }
                let (ready, publish) = {
                    let mut queue = queue_handle.lock();
                    match queue.attempt_receive(&waiter, true) {
                        Some((message, publish)) => {
                            *owned = Some(message);
                            (true, publish)
                        }
                        None => (false, MqPublish::default()),
                    }
                };
                publish.publish(&readiness);
                Ok(ready)
            },
            |owned| {
                let mut queue = queue_handle.lock();
                queue.drop_receiver(&waiter);
                if owned.is_none() {
                    *owned = waiter.take_slot();
                }
                Ok(owned.is_some())
            },
        )
    });
    // Every exit path, including a failed block session, must leave
    // `e_wait_q[RECV]`; a slot filled while giving up still wins over the
    // reported timeout or interruption.
    if owned.is_none() {
        let mut queue = file.queue.lock();
        queue.drop_receiver(&waiter);
        *owned = waiter.take_slot();
    }
    if owned.is_some() { Ok(()) } else { wait }
}

fn send_thread_notification(thread: &MqThreadNotifier, state: u8) {
    let mut cookie = thread.cookie;
    cookie[NOTIFY_COOKIE_LEN - 1] = state;
    thread.netlink.enqueue_kernel(Vec::from(cookie));
}

fn unregister_notification(ipc_ns: &IpcNamespace, id: u64) {
    let removed = ipc_ns.mqueue_notifications().lock().entries.remove(&id);
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
    if let Some(ipc_ns) = notifier.ipc_ns.as_ref().and_then(Weak::upgrade) {
        unregister_notification(&ipc_ns, notifier.registration.id);
    }
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
            // The number is a stored identity, so render it the way
            // `pid_nr_ns()` does: a missing binding is zero, never the
            // sender's init-namespace number.
            return namespace
                .visible_pid_checked(sender.pid)
                .unwrap_or(0) as i32;
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
pub(crate) fn cleanup_process_mqueue_notifications_in(namespace: &IpcNamespace, pid: u32) {
    loop {
        let registration = {
            let mut registry = namespace.mqueue_notifications().lock();
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

pub(crate) fn cleanup_process_mqueue_notifications(pid: u32) {
    let curr = current();
    let namespace = curr.as_thread().ipc_ns();
    cleanup_process_mqueue_notifications_in(&namespace, pid);
}

pub fn sys_mq_open<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const c_char,
    oflag: i32,
    mode: __kernel_mode_t,
    attr: *const MqAttr,
) -> AxResult<isize> {
    // Linux `SYSCALL_DEFINE4(mq_open)` copies the requested attributes before
    // the name is resolved, so a bad attribute pointer is `-EFAULT` even for an
    // open that would ignore it.
    let requested_attr = read_create_attr(memory, attr)?;
    let name = normalize_name(memory, name)?;
    let access = MqAccess::from_flags(oflag);
    let create = (oflag as u32) & O_CREAT != 0;
    let excl = (oflag as u32) & O_EXCL != 0;
    let nonblocking = (oflag as u32) & O_NONBLOCK != 0;
    let curr = current();
    let ipc_ns = curr.as_thread().ipc_ns();

    let (queue, created) = {
        let mut manager = ipc_ns.mqueue_manager().lock();
        if let Some(queue) = manager.queues.get(&name).cloned() {
            // `prepare_open()` decides in this order: `O_CREAT | O_EXCL` on an
            // existing name is `-EEXIST`, then the reserved `O_RDWR|O_WRONLY`
            // access mode is `-EINVAL`, and only then is access checked against
            // the inode mode.
            if create && excl {
                return Err(AxError::AlreadyExists);
            }
            if access == MqAccess::Unspecified {
                return Err(AxError::InvalidInput);
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
            // `mqueue_create_attr()`: `queues_max` is the first admission
            // check, and `CAP_SYS_RESOURCE` skips it entirely.
            let (limits, cap_sys_resource) = create_limits(&ipc_ns);
            if manager.queues.len() >= limits.queues && !cap_sys_resource {
                return Err(LinuxError::ENOSPC.into());
            }
            let attr = read_created_attributes(requested_attr, &ipc_ns, limits, cap_sys_resource)?;
            // `mqueue_get_inode()` charges the full Linux footprint against the
            // creating task's `RLIMIT_MSGQUEUE`: message area plus one
            // `struct msg_msg` and one priority-tree node per slot.
            let charge_bytes =
                tk_linux_ipc::mqueue_charge_bytes(attr.mq_maxmsg as u64, attr.mq_msgsize as u64)
                    .ok_or(AxError::from(LinuxError::EOVERFLOW))?;
            let (uid, gid) = current_ids();
            let curr = current();
            let thread = curr.as_thread();
            let charge = ipc_ns.try_charge_mqueue(
                thread.current_cred().ids().ruid,
                usize::try_from(charge_bytes).map_err(|_| AxError::NoMemory)?,
                thread.proc_data.rlim.read()[linux_raw_sys::general::RLIMIT_MSGQUEUE].current,
            )?;
            let create_mode = (mode & !crate::task::current_fs_context().lock().umask()) & 0o777;
            let queue = Arc::try_new(Mutex::new(PosixMqueue::new(
                name.clone(),
                create_mode,
                uid,
                gid,
                attr,
                &ipc_ns,
            )?))
            .map_err(|_| AxError::NoMemory)?;
            queue.lock().charge = Some(charge);
            manager.queues.insert(name.clone(), queue.clone());
            (queue, true)
        }
    };

    let mqfd = Arc::new(MqFd::new(
        queue.clone(),
        ipc_ns.clone(),
        access,
        nonblocking,
    ));
    let status_flags = ((oflag as u32) & O_NONBLOCK) | ((oflag as u32) & O_ACCMODE);
    // `do_mq_open()` installs the descriptor with
    //
    // 	fd = FD_ADD(O_CLOEXEC, mqueue_file_open(name, mnt, oflag, ro, mode, attr));
    //
    // (`ipc/mqueue.c:924`), and `FD_ADD` (`include/linux/file.h:252`) hands that
    // first argument straight to `get_unused_fd_flags()`. The queue descriptor
    // is therefore close-on-exec whether or not the caller passed `O_CLOEXEC`;
    // `oflag` only reaches `dentry_open()`'s `f_flags`. A 7.2.3 oracle guest
    // confirms it: `F_GETFD` reports `FD_CLOEXEC` for a plain
    // `O_CREAT|O_RDWR` `mq_open()`.
    match add_file_like_with_flags(mqfd, true, status_flags) {
        Ok(fd) => Ok(fd as isize),
        Err(err) => {
            if created {
                let mut manager = ipc_ns.mqueue_manager().lock();
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
    let curr = current();
    let ipc_ns = curr.as_thread().ipc_ns();
    // An old queue remains alive through open descriptors after unlink and a
    // same name may be recreated immediately.  Make the permission decision
    // and removal one namespace-manager transaction so an old lookup can
    // never unlink that successor.
    let mut manager = ipc_ns.mqueue_manager().lock();
    let queue = manager
        .queues
        .get(&name)
        .cloned()
        .ok_or(AxError::NotFound)?;
    let queue = queue.lock();
    let curr = current();
    let cred = curr.as_thread().current_cred();
    check_unlink_authority(queue.uid, &cred)?;
    drop(queue);
    let removed = manager.queues.remove(&name);
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
    // `SYSCALL_DEFINE5(mq_timedsend)` prepares the absolute timeout before it
    // enters `do_mq_timedsend()`, so a bad timespec outranks everything below.
    // Inside `do_mq_timedsend()` the order is priority (EINVAL), fd access
    // (EBADF), the `mq_msgsize` bound (EMSGSIZE), and only then `load_msg()`
    // (EFAULT).
    let deadline = validate_timespec(memory, abs_timeout)?;
    validate_priority(msg_prio).map_err(|_| AxError::InvalidInput)?;
    let file = get_mq_fd(fd)?;
    if !file.access.can_write() {
        return Err(AxError::BadFileDescriptor);
    }
    {
        let queue = file.queue.lock();
        if msg_len > queue.msgsize {
            return Err(AxError::from(LinuxError::EMSGSIZE));
        }
    }
    let data = vm_load(memory, msg_ptr, msg_len).map_err(map_usercopy_error)?;

    let waiter = MqSenderWaiter::new(MqOutgoing {
        priority: msg_prio,
        sender: current_mq_sender(),
        data,
    });
    if file.is_nonblocking() {
        // Linux answers a full queue with `-EAGAIN` from `do_mq_timedsend()`
        // and never publishes the loaded message on `e_wait_q[SEND]`.
        let publish = {
            let mut queue = file.queue.lock();
            match queue.attempt_send(&waiter, false)? {
                MqSendAttempt::Sent(publish) => publish,
                MqSendAttempt::Sleep => return Err(AxError::WouldBlock),
            }
        };
        publish.publish(&file.readiness);
        return Ok(0);
    }

    send_blocking(&file, &waiter, deadline)?;
    Ok(0)
}

fn try_mq_receive<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    msg_ptr: *mut u8,
    msg_prio: *mut u32,
    message: MqMessage,
) -> AxResult<isize> {
    // Dequeue commits free capacity even if either copyout faults.
    // Linux wakes a waiting sender before copying priority/data to user, and
    // the priority store short-circuits the message store exactly as
    // `put_user(...) || store_msg(...)` does.
    if !msg_prio.is_null() {
        VmMutPtr::vm_write(msg_prio, memory, message.priority).map_err(map_usercopy_error)?;
    }
    if !msg_ptr.is_null() {
        vm_write_slice(memory, msg_ptr, &message.data).map_err(map_usercopy_error)?;
    }
    Ok(message.data.len() as isize)
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
    {
        // Linux checks `msg_len < info->attr.mq_msgsize` before it takes
        // `info->lock`, so a buffer that cannot hold the queue's maximum
        // message is `-EMSGSIZE` even on an empty queue where `O_NONBLOCK`
        // would otherwise report `-EAGAIN`. `do_mq_timedreceive()` waives the
        // check when both the buffer pointer and the length are absent
        // (`msg_ptr || msg_len`), letting a receive discard the payload.
        let queue = file.queue.lock();
        if msg_len < queue.msgsize && (!msg_ptr.is_null() || msg_len != 0) {
            return Err(AxError::from(LinuxError::EMSGSIZE));
        }
    }

    let mut owned = None;
    if file.is_nonblocking() {
        let (message, publish) = {
            let mut queue = file.queue.lock();
            queue.take_message().ok_or(AxError::WouldBlock)?
        };
        publish.publish(&file.readiness);
        owned = Some(message);
    } else {
        receive_blocking(&file, deadline, &mut owned)?;
    }
    let Some(message) = owned else {
        return Err(AxError::WouldBlock);
    };
    try_mq_receive(memory, msg_ptr, msg_prio, message)
}

pub fn sys_mq_notify<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    notification: *const RawSigevent,
) -> AxResult<isize> {
    let mut notifier = if notification.is_null() {
        None
    } else {
        let event =
            RawSigevent::read_from_user(memory, notification).map_err(map_usercopy_error)?;
        Some(build_notifier(memory, &event)?)
    };
    let (queue_file, ipc_ns) = get_mq_queue(fd)?;
    let curr = current();
    let current_thread = curr.as_thread();
    if notifier.is_some() {
        // mq_notify belongs to the process, not the task which happened to
        // install it. Remember this IPC manager before registry publication.
        current_thread
            .proc_data
            .register_touched_ipc_namespace(ipc_ns.clone())?;
        notifier.as_mut().expect("checked above").ipc_ns = Some(Arc::downgrade(&ipc_ns));
    }

    let mut rejected = None;
    let mut detached_registration = None;
    let mut busy = false;
    let removed = {
        let mut registry = ipc_ns.mqueue_notifications().lock();
        let mut queue = queue_file.lock();
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
                        queue: Arc::downgrade(&queue_file),
                        token: notifier.registration.clone(),
                    },
                );
                queue.notifier = Some(notifier);
                None
            }
        } else {
            let curr_pid = current_thread.proc_data.proc.pid();
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

fn write_queue_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    queue: &Mutex<PosixMqueue>,
    flags: isize,
    destination: *mut MqAttr,
) -> AxResult<()> {
    let snapshot = queue.lock().attr(flags);
    // SAFETY: attr initializes every repr(C) field, including reserved words.
    unsafe { VmMutPtr::vm_write_unchecked(destination, memory, snapshot) }
        .map_err(map_usercopy_error)
}

pub fn sys_mq_getsetattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    new_attr: *const MqAttr,
    old_attr: *mut MqAttr,
) -> AxResult<isize> {
    let new = if new_attr.is_null() {
        None
    } else {
        let attr = VmPtr::vm_read(new_attr, memory).map_err(map_usercopy_error)?;
        if attr.mq_flags & !(O_NONBLOCK as isize) != 0 {
            return Err(AxError::InvalidInput);
        }
        Some(attr)
    };

    if let Ok(file) = get_mq_fd(fd) {
        if !old_attr.is_null() {
            let flags = nonblock_flags(&file);
            write_queue_attr(memory, &file.queue, flags, old_attr)?;
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
        return Ok(0);
    }

    let file = get_typed_file::<File>(fd).map_err(|_| AxError::BadFileDescriptor)?;
    let queue = crate::pseudofs::mqueue::queue_for_location(file.inner().location())
        .ok_or(AxError::BadFileDescriptor)?;

    if !old_attr.is_null() {
        let flags = if file.nonblocking() {
            O_NONBLOCK as isize
        } else {
            0
        };
        write_queue_attr(memory, &queue, flags, old_attr)?;
    }
    if let Some(attr) = new {
        file.set_nonblocking(attr.mq_flags & O_NONBLOCK as isize != 0)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::{mem::MaybeUninit, ops::Range};

    use tk_linux_ipc::{MQ_MSG_MAX_DEFAULT, MQ_MSGSIZE_MAX_DEFAULT, MQ_QUEUES_MAX_DEFAULT};
    use tk_linux_usercopy::{UserCopyError, VmResult};

    use super::*;
    use crate::task::{Cred, CredentialSlot};

    #[test]
    fn attribute_copyout_does_not_hold_queue_or_namespace_locks() {
        struct Probe<'a> {
            queue: &'a Mutex<PosixMqueue>,
            namespace: &'a IpcNamespace,
            writes: usize,
        }
        // SAFETY: the probe never reads user memory and fails writes without
        // claiming that user bytes have been initialized or copied.
        unsafe impl UserMemory for Probe<'_> {
            fn read(&mut self, _: usize, _: &mut [MaybeUninit<u8>]) -> VmResult {
                Err(UserCopyError::BadAddress)
            }
            fn write(&mut self, _: usize, _: &[u8]) -> VmResult {
                assert!(self.queue.try_lock().is_some());
                assert!(self.namespace.mqueue_manager().try_lock().is_some());
                self.writes += 1;
                Err(UserCopyError::BadAddress)
            }
        }
        let namespace = IpcNamespace::try_new(UserNamespace::try_new_root().unwrap()).unwrap();
        let queue = Mutex::new(
            PosixMqueue::new(
                FsNameBuf::from_vec(b"attr-copyout".to_vec()).unwrap(),
                0o600,
                0,
                0,
                default_attr(&namespace),
                &namespace,
            )
            .unwrap(),
        );
        let mut probe = Probe {
            queue: &queue,
            namespace: &namespace,
            writes: 0,
        };
        let mut memory = UserMemoryContext::new(&mut probe);
        assert_eq!(
            write_queue_attr(&mut memory, &queue, 0, 8_usize as *mut MqAttr),
            Err(AxError::BadAddress)
        );
        drop(memory);
        assert_eq!(probe.writes, 1);
    }

    #[test]
    fn receive_copyout_fault_wakes_sender_after_freeing_capacity() {
        use alloc::task::Wake;
        use core::{
            sync::atomic::{AtomicUsize, Ordering},
            task::Waker,
        };

        struct SenderWake {
            count: AtomicUsize,
            queue: Arc<Mutex<PosixMqueue>>,
        }
        impl Wake for SenderWake {
            fn wake(self: Arc<Self>) {
                // The sender may immediately lock the queue and retry.
                let queue = self.queue.try_lock().expect("wake after queue unlock");
                assert!(queue.messages.len() < queue.maxmsg);
                self.count.fetch_add(1, Ordering::Relaxed);
            }
        }
        let user_ns = UserNamespace::try_new_root().unwrap();
        let pid_ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        let ipc_ns = IpcNamespace::try_new(user_ns).unwrap();
        for priority_fault in [false, true] {
            let mut attr = default_attr(&ipc_ns);
            attr.mq_maxmsg = 1;
            attr.mq_msgsize = 4;
            let queue = Arc::new(Mutex::new(
                PosixMqueue::new(
                    FsNameBuf::from_vec(b"fault-wakeup".to_vec()).unwrap(),
                    0o600,
                    0,
                    0,
                    attr,
                    &ipc_ns,
                )
                .unwrap(),
            ));
            queue.lock().insert_message(MqOutgoing {
                priority: 3,
                sender: MqSender {
                    pid: 1,
                    real_uid: Kuid::INITIAL_ROOT,
                    pid_ns: pid_ns.clone(),
                },
                data: vec![1, 2, 3, 4],
            });
            let readiness = queue.lock().readiness.clone();
            let sender = Arc::new(SenderWake {
                count: AtomicUsize::new(0),
                queue: queue.clone(),
            });
            let token = readiness
                .writable
                .register(&Waker::from(sender.clone()))
                .unwrap();
            let mut provider = TestMemory { bytes: vec![0; 4] };
            let mut memory = UserMemoryContext::new(&mut provider);
            let (data, priority) = if priority_fault {
                (0_usize as *mut u8, 8_usize as *mut u32)
            } else {
                (8_usize as *mut u8, core::ptr::null_mut())
            };
            let (message, publish) = queue
                .lock()
                .take_message()
                .expect("the test inserted one message");
            // Linux frees the capacity and wakes waiters before touching user
            // memory, so the wake must not be deferred behind the copyout.
            publish.publish(&readiness);
            assert_eq!(
                try_mq_receive(&mut memory, data, priority, message),
                Err(AxError::BadAddress)
            );
            drop(memory);
            if priority_fault {
                assert_eq!(provider.bytes, vec![0; 4]);
            }
            assert_eq!(sender.count.load(Ordering::Relaxed), 1);
            assert!(queue.lock().messages.is_empty());
            readiness.writable.cancel(token);
        }
    }

    /// A queue plus the namespaces a handoff test needs. The namespace is
    /// returned because a queue only holds a weak edge to it.
    fn handoff_queue(
        maxmsg: isize,
        msgsize: isize,
    ) -> (
        Arc<Mutex<PosixMqueue>>,
        Arc<PidNamespace>,
        Arc<IpcNamespace>,
    ) {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let pid_ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        let ipc_ns = IpcNamespace::try_new(user_ns).unwrap();
        let mut attr = default_attr(&ipc_ns);
        attr.mq_maxmsg = maxmsg;
        attr.mq_msgsize = msgsize;
        let queue = Arc::new(Mutex::new(
            PosixMqueue::new(
                FsNameBuf::from_vec(b"handoff".to_vec()).unwrap(),
                0o600,
                0,
                0,
                attr,
                &ipc_ns,
            )
            .unwrap(),
        ));
        (queue, pid_ns, ipc_ns)
    }

    fn handoff_sender(
        pid_ns: &Arc<PidNamespace>,
        priority: u32,
        data: &[u8],
    ) -> Arc<MqSenderWaiter> {
        MqSenderWaiter::new(MqOutgoing {
            priority,
            sender: MqSender {
                pid: 1,
                real_uid: Kuid::INITIAL_ROOT,
                pid_ns: pid_ns.clone(),
            },
            data: data.to_vec(),
        })
    }

    /// A registration the queue can hold without a live socket or signal.
    fn handoff_notifier(ipc_ns: &Arc<IpcNamespace>) -> MqNotifier {
        MqNotifier {
            pid: 1,
            ipc_ns: Some(Arc::downgrade(ipc_ns)),
            notify: SIGEV_NONE as i32,
            thread: None,
            signal: None,
            registration: new_notification_token().unwrap(),
        }
    }

    #[test]
    fn pipelined_send_bypasses_the_queue_and_the_notification() {
        let (queue, pid_ns, ipc_ns) = handoff_queue(1, 8);
        let receiver = MqReceiver::new();
        {
            let mut queue = queue.lock();
            queue.notifier = Some(handoff_notifier(&ipc_ns));
            queue.park_receiver(&receiver);
        }

        let waiter = handoff_sender(&pid_ns, 9, b"direct");
        let publish = {
            let mut queue = queue.lock();
            match queue.attempt_send(&waiter, true).unwrap() {
                MqSendAttempt::Sent(publish) => publish,
                MqSendAttempt::Sleep => panic!("a parked receiver accepts without queueing"),
            }
        };

        // Linux `pipelined_send()` stores into the waiter's message and never
        // touches `msg_tree`, so the queue stays empty and `__do_notify()` is
        // not reached: the registration survives for the next empty edge.
        assert!(queue.lock().messages.is_empty());
        assert!(queue.lock().notifier.is_some());
        assert!(publish.receiver.is_some());
        assert!(!publish.readable);
        assert!(publish.notify.is_none());
        assert!(queue.lock().receivers.is_empty());
        assert!(!receiver.queued.load(Ordering::Acquire));

        let message = receiver.take_slot().expect("the receiver owns the message");
        assert_eq!(message.priority, 9);
        assert_eq!(message.data, b"direct".to_vec());
        assert!(receiver.take_slot().is_none());
    }

    #[test]
    fn tree_insert_consumes_the_notification_only_on_the_empty_edge() {
        let (queue, pid_ns, ipc_ns) = handoff_queue(4, 8);
        queue.lock().notifier = Some(handoff_notifier(&ipc_ns));

        let first = handoff_sender(&pid_ns, 1, b"one");
        let publish = {
            let mut queue = queue.lock();
            match queue.attempt_send(&first, true).unwrap() {
                MqSendAttempt::Sent(publish) => publish,
                MqSendAttempt::Sleep => panic!("an empty queue accepts"),
            }
        };
        assert!(publish.readable);
        assert!(publish.notify.is_some());
        assert!(queue.lock().notifier.is_none());

        let second = handoff_sender(&pid_ns, 2, b"two");
        let publish = {
            let mut queue = queue.lock();
            match queue.attempt_send(&second, true).unwrap() {
                MqSendAttempt::Sent(publish) => publish,
                MqSendAttempt::Sleep => panic!("a queue below maxmsg accepts"),
            }
        };
        assert!(publish.readable);
        assert!(publish.notify.is_none());
        assert_eq!(queue.lock().messages.len(), 2);
    }

    #[test]
    fn pipelined_receive_hands_the_freed_slot_to_the_oldest_sender() {
        let (queue, pid_ns, _ipc_ns) = handoff_queue(1, 8);
        let first = handoff_sender(&pid_ns, 1, b"first");
        let second = handoff_sender(&pid_ns, 2, b"second");
        {
            let mut queue = queue.lock();
            queue.insert_message(MqOutgoing {
                priority: 5,
                sender: MqSender {
                    pid: 1,
                    real_uid: Kuid::INITIAL_ROOT,
                    pid_ns: pid_ns.clone(),
                },
                data: b"stored".to_vec(),
            });
            queue.park_sender(&first);
            queue.park_sender(&second);
        }

        let (message, publish) = queue.lock().take_message().unwrap();
        assert_eq!(message.data, b"stored".to_vec());
        // Linux `pipelined_receive()` inserts the strongest sleeping sender's
        // message into the freed slot instead of waking it to contend, and
        // `wq_get_first_waiter()` picks the `e_wait_q[SEND]` tail, so the
        // oldest of the equally strong waiters wins. TheKernel parks senders in
        // arrival order, which matches that rule for equal nice values only
        // (see `park_receiver` for the missing priority ordering).
        assert!(first.handed_off.load(Ordering::Acquire));
        assert!(!second.handed_off.load(Ordering::Acquire));
        assert!(publish.sender.is_some());
        assert!(!publish.writable);
        let queue_guard = queue.lock();
        assert_eq!(queue_guard.messages.len(), 1);
        assert_eq!(queue_guard.messages[0].data, b"first".to_vec());
        assert_eq!(queue_guard.senders.len(), 1);
        assert!(queue_guard.receivers.is_empty());
        drop(queue_guard);
        assert!(Arc::ptr_eq(queue.lock().senders.first().unwrap(), &second));

        let publish = {
            let mut queue = queue.lock();
            match queue.attempt_send(&first, true).unwrap() {
                MqSendAttempt::Sent(publish) => publish,
                MqSendAttempt::Sleep => panic!("the handoff already committed"),
            }
        };
        // The sender's second attempt observes the completed handoff and must
        // not insert the staged message a second time.
        assert!(publish.notify.is_none());
        assert!(!publish.readable);
        assert_eq!(queue.lock().messages.len(), 1);
    }

    #[test]
    fn abandoning_a_receiver_still_claims_a_pipelined_message() {
        let (queue, pid_ns, _ipc_ns) = handoff_queue(1, 8);
        let receiver = MqReceiver::new();
        queue.lock().park_receiver(&receiver);

        let waiter = handoff_sender(&pid_ns, 3, b"kept");
        let publish = {
            let mut queue = queue.lock();
            match queue.attempt_send(&waiter, true).unwrap() {
                MqSendAttempt::Sent(publish) => publish,
                MqSendAttempt::Sleep => panic!("a parked receiver accepts"),
            }
        };
        assert!(publish.receiver.is_some());

        // `mq_sleep()`'s abandon path: leave `e_wait_q[RECV]`, then re-check
        // the handoff. The message must still reach the caller that is timing
        // out, never the empty queue or a dropped slot.
        let claimed = {
            let mut queue = queue.lock();
            queue.drop_receiver(&receiver);
            receiver.take_slot()
        };
        assert_eq!(
            claimed.expect("the pipelined message survives").data,
            b"kept".to_vec()
        );
        assert!(queue.lock().messages.is_empty());
    }

    #[test]
    fn create_attribute_admission_uses_namespace_limits_and_capability() {
        let namespace = IpcNamespace::try_new(UserNamespace::try_new_root().unwrap()).unwrap();
        let limits = MqLimits {
            queues: namespace.mq_queues_max(),
            max_messages: namespace.mq_msg_max(),
            max_message_size: namespace.mq_msgsize_max(),
        };
        let attr = |maxmsg: isize, msgsize: isize| MqAttr {
            mq_flags: 0,
            mq_maxmsg: maxmsg,
            mq_msgsize: msgsize,
            mq_curmsgs: 0,
            __reserved: [0; 4],
        };

        // `EINVAL` from the size checks precedes any arithmetic outcome, and
        // `CAP_SYS_RESOURCE` replaces the namespace maxima with HARD_MSGMAX /
        // HARD_MSGSIZEMAX.
        assert!(matches!(
            read_created_attributes(Some(attr(0, 64)), &namespace, limits, true),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            read_created_attributes(Some(attr(100, 64)), &namespace, limits, false),
            Err(AxError::InvalidInput)
        ));
        assert!(
            read_created_attributes(Some(attr(100, 64)), &namespace, limits, true).is_ok(),
            "unprivileged ceilings do not bind a CAP_SYS_RESOURCE caller"
        );
        assert!(read_created_attributes(Some(attr(65_537, 1)), &namespace, limits, true).is_err());
        assert!(matches!(
            read_created_attributes(
                Some(attr(1, 16 * 1024 * 1024 + 1)),
                &namespace,
                limits,
                true
            ),
            Err(AxError::InvalidInput)
        ));
        // `ipc/mqueue.c` defaults to `min(mq_msg_max, DFLT_MSGMAX)` rather than
        // to the raw namespace maximum.
        let default = read_created_attributes(None, &namespace, limits, false).unwrap();
        assert_eq!(default.mq_maxmsg, MQ_MSG_MAX_DEFAULT as isize);
        assert_eq!(default.mq_msgsize, MQ_MSGSIZE_MAX_DEFAULT as isize);

        // Both ceilings are attributes of *this* namespace, so lowering them
        // lowers what an attribute-less create inherits here and nowhere else.
        namespace.set_mq_msg_max(4).unwrap();
        namespace.set_mq_msgsize_max(512).unwrap();
        let shrunk = read_created_attributes(None, &namespace, limits, false).unwrap();
        assert_eq!(shrunk.mq_maxmsg, 4);
        assert_eq!(shrunk.mq_msgsize, 512);
        let sibling = IpcNamespace::try_new(UserNamespace::try_new_root().unwrap()).unwrap();
        assert_eq!(sibling.mq_msg_max(), MQ_MSG_MAX_DEFAULT);
        assert_eq!(sibling.mq_msgsize_max(), MQ_MSGSIZE_MAX_DEFAULT);

        // The sysctl window is `[MIN_MSGMAX, HARD_MSGMAX]` /
        // `[MIN_MSGSIZEMAX, HARD_MSGSIZEMAX]` with `EINVAL` outside, so a write
        // below the floor cannot silently clamp every new queue.
        assert!(matches!(
            namespace.set_mq_msg_max(0),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            namespace.set_mq_msgsize_max(crate::syscall::ipc::MQ_MSGSIZE_MAX_MIN - 1),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            namespace.set_mq_msg_max(tk_linux_ipc::MQ_MSG_MAX_HARD as usize + 1),
            Err(AxError::InvalidInput)
        ));
        assert_eq!(namespace.mq_msg_max(), 4);
        assert_eq!(namespace.mq_msgsize_max(), 512);
        // `queues_max` has no bound at all, and a write of 0 is accepted.
        assert_eq!(sibling.mq_queues_max(), MQ_QUEUES_MAX_DEFAULT);
        sibling.set_mq_queues_max(0);
        assert_eq!(sibling.mq_queues_max(), 0);
    }

    #[test]
    fn unlink_authority_maps_linux_sticky_directory_outcomes() {
        let _context = crate::test_support::scheduler_test_context();
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(namespace).unwrap());
        let owner_fsuid = Kuid::from_raw(1000).unwrap();
        let owner_fsgid = Kgid::from_raw(1000).unwrap();
        // `check_sticky()` exemption needs `CAP_FOWNER` to be absent from the
        // effective set, which only a capability-free credential proves.
        slot.replace_capabilities_for_test(&[], &[]).unwrap();

        // The queue owner passes both `may_delete_dentry()` and
        // `check_sticky()`.
        let queue_owner = slot
            .replace_fs_ids_for_test(owner_fsuid, owner_fsgid)
            .unwrap();
        assert_eq!(check_unlink_authority(1000, &queue_owner), Ok(()));
        // A foreign queue in the root-owned sticky 01777 directory is `-EPERM`
        // from `check_sticky()`, not the `-EACCES` of a directory permission
        // failure.
        assert_eq!(
            check_unlink_authority(0, &queue_owner),
            Err(AxError::from(LinuxError::EPERM))
        );
        // `check_sticky()` also accepts the owner of the directory itself.
        let directory_owner = slot
            .replace_fs_ids_for_test(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT)
            .unwrap();
        assert_eq!(check_unlink_authority(1000, &directory_owner), Ok(()));
        // ... and `CAP_FOWNER` bypasses it entirely.
        let capable = slot
            .replace_fs_ids_for_test(owner_fsuid, owner_fsgid)
            .unwrap();
        assert_eq!(
            check_unlink_authority(0, &capable),
            Err(AxError::from(LinuxError::EPERM))
        );
        let capable = slot
            .replace_capabilities_for_test(&[CAP_FOWNER], &[CAP_FOWNER])
            .unwrap();
        assert_eq!(check_unlink_authority(0, &capable), Ok(()));
    }

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
    fn signal_notification_accepts_the_valid_signal_boundary() {
        let mut provider = TestMemory {
            bytes: vec![0; size_of::<RawSigevent>()],
        };
        let notify_offset = core::mem::offset_of!(linux_raw_sys::general::sigevent, sigev_notify);
        let signo_offset = core::mem::offset_of!(linux_raw_sys::general::sigevent, sigev_signo);
        // Linux `valid_signal()` is `sig <= _NSIG`: 0 and 64 register, a
        // negative signo or anything above _NSIG is `-EINVAL`.
        for (notify, signo, expected) in [
            (SIGEV_SIGNAL, 0, Ok(())),
            (SIGEV_SIGNAL, 1, Ok(())),
            (SIGEV_SIGNAL, 64, Ok(())),
            (SIGEV_SIGNAL, 65, Err(AxError::InvalidInput)),
            (SIGEV_SIGNAL, -1, Err(AxError::InvalidInput)),
            (SIGEV_SIGNAL, i32::MIN, Err(AxError::InvalidInput)),
            (SIGEV_NONE, 0, Ok(())),
            (SIGEV_THREAD, -1, Ok(())),
        ] {
            provider.bytes[notify_offset..notify_offset + 4]
                .copy_from_slice(&(notify as i32).to_ne_bytes());
            provider.bytes[signo_offset..signo_offset + 4].copy_from_slice(&signo.to_ne_bytes());
            let mut memory = UserMemoryContext::new(&mut provider);
            let event = RawSigevent::read_from_user(&mut memory, core::ptr::null()).unwrap();
            assert_eq!(validate_notify_event(&event), expected, "{notify}/{signo}");
        }
        // The accepted zero signo has no `Signo`, so `build_notifier()` leaves
        // `signal` empty and `maybe_notify()` consumes the one-shot without
        // delivering anything, like `__do_notify()` does.
        assert!(Signo::from_repr(0).is_none());
        assert!(Signo::from_repr(64).is_some());
    }

    #[test]
    fn mqueue_usercopy_helpers_snapshot_unaligned_inputs() {
        let mut provider = TestMemory {
            bytes: vec![0; 128],
        };
        let name_addr = 3;
        // Kernel queue names carry no leading slash: libc strips it before the
        // syscall, and `lookup_noperm_common()` rejects every '/'.
        provider.bytes[name_addr..name_addr + 8].copy_from_slice(b"queue\0\0\0");
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

        assert_eq!(name.as_bytes(), b"queue");
        let copied_attr = copied_attr.expect("a non-null attribute pointer is copied");
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

    /// Binds `global_pid` in `namespace` the way process creation does and
    /// returns the namespace-local number it was given.
    ///
    /// The tests below used to pass synthetic PIDs that were never admitted
    /// through the lifecycle.  `visible_pid` renders such an identity as
    /// itself (see its comment on the unit-test fallback), so the assertions
    /// held without the namespace mapping ever running.  `si_pid` rendering
    /// is `pid_nr_ns`-strict now, which is what exposed that: a real sender
    /// always holds a binding, so the tests have to establish one.
    fn bind_sender_pid(
        namespace: &Arc<PidNamespace>,
        global_pid: tk_linux_process::Pid,
    ) -> tk_linux_process::Pid {
        namespace
            .reserve_process(global_pid)
            .expect("sender PID reservation")
            .commit();
        let local = namespace
            .visible_pid_checked(global_pid)
            .expect("reserved sender PID must be bound");
        assert_ne!(
            local, global_pid,
            "the binding must exercise the namespace mapping, not the identity"
        );
        local
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
        let local = bind_sender_pid(&target_pid_ns, 4242);
        let info = mq_signal_info_for_sender(&registration_info, &target, &target_pid_ns, sender);

        assert_eq!(
            info.rt_payload(),
            SignalRtPayload::new(local as i32, 0, 0xfeed)
        );
    }

    #[test]
    fn mq_signal_info_maps_sender_pid_to_target_pid_namespace() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let root_pid_ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        let child_pid_ns = root_pid_ns.try_fork(100, user_ns.clone()).unwrap();
        let root_local = bind_sender_pid(&root_pid_ns, 4242);
        let child_local = bind_sender_pid(&child_pid_ns, 4243);
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
        assert_eq!(same_namespace.rt_payload().pid, root_local as i32);

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
                pid: 4243,
                real_uid: Kuid::INITIAL_ROOT,
                pid_ns: child_pid_ns.clone(),
            },
        );
        assert_eq!(visible_nested_sender.rt_payload().pid, child_local as i32);

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
        Arc<IpcNamespace>,
        Arc<Mutex<PosixMqueue>>,
        Arc<MqNotificationToken>,
        Arc<tk_linux_signal::SignalQueueAccount>,
        Arc<tk_linux_signal::SignalQueueAccount>,
    ) {
        let ipc_ns = IpcNamespace::try_new(UserNamespace::try_new_root().unwrap()).unwrap();
        let queue = Arc::new(Mutex::new(
            PosixMqueue::new(
                FsNameBuf::from_vec(Vec::from(&b"registry-account-test"[..])).unwrap(),
                0o600,
                pid,
                0,
                default_attr(&ipc_ns),
                &ipc_ns,
            )
            .unwrap(),
        ));
        let token = new_notification_token().unwrap();
        let per_user = tk_linux_signal::SignalQueueAccount::try_new(4).unwrap();
        let global = tk_linux_signal::SignalQueueAccount::try_new(4).unwrap();
        let info = SignalInfo::new_rt(Signo::SIGRTMIN, SI_MESGQ, SignalRtPayload::new(0, 0, 0));
        let prepared = PreparedSignal::try_accounted(info.clone(), &per_user, 4, &global).unwrap();
        let target_user_ns = UserNamespace::try_new_root().unwrap();
        let target_pid_ns = PidNamespace::try_new_root(target_user_ns.clone()).unwrap();
        queue.lock().notifier = Some(MqNotifier {
            pid,
            ipc_ns: Some(Arc::downgrade(&ipc_ns)),
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
        ipc_ns.mqueue_notifications().lock().entries.insert(
            token.id,
            MqNotificationRegistration {
                owner: pid,
                queue: Arc::downgrade(&queue),
                token: token.clone(),
            },
        );
        (ipc_ns, queue, token, per_user, global)
    }

    fn install_inert_notification(
        pid: u32,
    ) -> (
        Arc<IpcNamespace>,
        Arc<Mutex<PosixMqueue>>,
        Arc<MqNotificationToken>,
    ) {
        let ipc_ns = IpcNamespace::try_new(UserNamespace::try_new_root().unwrap()).unwrap();
        let queue = Arc::new(Mutex::new(
            PosixMqueue::new(
                FsNameBuf::from_vec(Vec::from(&b"registry-test"[..])).unwrap(),
                0o600,
                pid,
                0,
                default_attr(&ipc_ns),
                &ipc_ns,
            )
            .unwrap(),
        ));
        let token = new_notification_token().unwrap();
        queue.lock().notifier = Some(MqNotifier {
            pid,
            ipc_ns: Some(Arc::downgrade(&ipc_ns)),
            notify: SIGEV_NONE as i32,
            thread: None,
            signal: None,
            registration: token.clone(),
        });
        ipc_ns.mqueue_notifications().lock().entries.insert(
            token.id,
            MqNotificationRegistration {
                owner: pid,
                queue: Arc::downgrade(&queue),
                token: token.clone(),
            },
        );
        (ipc_ns, queue, token)
    }

    #[test]
    fn owner_exit_cancels_unlinked_but_open_notification() {
        let _context = crate::test_support::scheduler_test_context();
        let pid = 0xf001;
        let (ipc_ns, queue, token) = install_inert_notification(pid);
        cleanup_process_mqueue_notifications_in(&ipc_ns, pid);

        assert!(queue.lock().notifier.is_none());
        assert!(!token.active.load(Ordering::Acquire));
        assert!(
            !ipc_ns
                .mqueue_notifications()
                .lock()
                .entries
                .contains_key(&token.id)
        );
    }

    #[test]
    fn owner_exit_wins_against_a_notification_already_taken_for_delivery() {
        let _context = crate::test_support::scheduler_test_context();
        let pid = 0xf002;
        let (ipc_ns, queue, token) = install_inert_notification(pid);
        let moved = queue.lock().notifier.take().unwrap();

        cleanup_process_mqueue_notifications_in(&ipc_ns, pid);
        assert!(!claim_notification(&moved));
        assert!(!token.active.load(Ordering::Acquire));
    }

    #[test]
    fn owner_exit_refunds_an_unlinked_queue_signal_reservation() {
        let _context = crate::test_support::scheduler_test_context();
        let pid = 0xf003;
        let (ipc_ns, queue, token, per_user, global) = install_accounted_notification(pid);
        assert_eq!(per_user.queued(), 1);
        assert_eq!(global.queued(), 1);

        cleanup_process_mqueue_notifications_in(&ipc_ns, pid);
        assert!(queue.lock().notifier.is_none());
        assert!(!token.active.load(Ordering::Acquire));
        assert_eq!(per_user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }

    #[test]
    fn exit_cancellation_of_taken_notification_refunds_exactly_on_drop() {
        let _context = crate::test_support::scheduler_test_context();
        let pid = 0xf004;
        let (ipc_ns, queue, token, per_user, global) = install_accounted_notification(pid);
        let moved = queue.lock().notifier.take().unwrap();

        cleanup_process_mqueue_notifications_in(&ipc_ns, pid);
        assert!(!claim_notification(&moved));
        assert!(!token.active.load(Ordering::Acquire));
        assert_eq!(per_user.queued(), 1);
        assert_eq!(global.queued(), 1);

        drop(moved);
        assert_eq!(per_user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }
}
