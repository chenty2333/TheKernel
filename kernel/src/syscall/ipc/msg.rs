use alloc::{
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{
    fmt::Write as _,
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicI32, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::*;
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load, vm_write_slice,
};

use super::{
    IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, IpcAccess,
    IpcAccessContext, IpcPerm, IpcPermissionUpdateRequest, MSG_INFO, MSG_STAT, MSG_STAT_ANY,
    PreparedIpcPermissionUpdate, next_ipc_id,
};
use crate::{
    mm::map_usercopy_error,
    task::{AsThread, ProcStateHint, has_pending_syscall_signal, with_proc_state_hint},
    time::wall_time,
};

fn ipc_time_secs() -> __kernel_time_t {
    wall_time().as_secs() as __kernel_time_t
}

/// Data structure describing a message queue.
#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
#[allow(non_camel_case_types)]
pub struct msqid_ds {
    /// operation permission struct
    pub msg_perm: IpcPerm,
    /// time of last msgsnd()
    pub msg_stime: __kernel_time_t,
    /// time of last msgrcv()
    pub msg_rtime: __kernel_time_t,
    /// time of last change by msgctl()
    pub msg_ctime: __kernel_time_t,
    /// current number of bytes on queue
    pub msg_cbytes: __kernel_size_t,
    /// number of messages in queue
    pub msg_qnum: __kernel_size_t,
    /// max number of bytes on queue
    pub msg_qbytes: __kernel_size_t,
    /// pid of last msgsnd()
    pub msg_lspid: __kernel_pid_t,
    /// pid of last msgrcv()
    pub msg_lrpid: __kernel_pid_t,
}

// These IPC records contain explicit Linux ABI padding (and `IpcPerm` has an
// alignment hole before its two native-word fields).  Keep the x86_64 layout
// checked and materialize a zeroed copy before the audited unchecked copyout
// so no Rust padding bytes escape to userspace.
const _: () = {
    assert!(align_of::<IpcPerm>() == 8);
    assert!(size_of::<IpcPerm>() == 48);
    assert!(offset_of!(IpcPerm, key) == 0);
    assert!(offset_of!(IpcPerm, mode) == 20);
    assert!(offset_of!(IpcPerm, unused0) == 32);
    assert!(offset_of!(IpcPerm, unused1) == 40);
    assert!(align_of::<msqid_ds>() == 8);
    assert!(size_of::<msqid_ds>() == 104);
    assert!(offset_of!(msqid_ds, msg_perm) == 0);
    assert!(offset_of!(msqid_ds, msg_stime) == 48);
    assert!(offset_of!(msqid_ds, msg_rtime) == 56);
    assert!(offset_of!(msqid_ds, msg_ctime) == 64);
    assert!(offset_of!(msqid_ds, msg_cbytes) == 72);
    assert!(offset_of!(msqid_ds, msg_qnum) == 80);
    assert!(offset_of!(msqid_ds, msg_qbytes) == 88);
    assert!(offset_of!(msqid_ds, msg_lspid) == 96);
    assert!(offset_of!(msqid_ds, msg_lrpid) == 100);
};

fn initialized_msqid_ds(value: msqid_ds) -> msqid_ds {
    // SAFETY: all fields are integer scalars; zero is a valid representation,
    // and starting from zero also initializes the alignment padding.
    let mut result: msqid_ds = unsafe { core::mem::zeroed() };
    // SAFETY: `IpcPerm` is integer-only; zeroing first prevents its implicit
    // four-byte alignment hole from containing uninitialized data.
    let mut perm: IpcPerm = unsafe { core::mem::zeroed() };
    perm.key = value.msg_perm.key;
    perm.uid = value.msg_perm.uid;
    perm.gid = value.msg_perm.gid;
    perm.cuid = value.msg_perm.cuid;
    perm.cgid = value.msg_perm.cgid;
    perm.mode = value.msg_perm.mode;
    perm.pad1 = value.msg_perm.pad1;
    perm.seq = value.msg_perm.seq;
    perm.pad2 = value.msg_perm.pad2;
    perm.unused0 = value.msg_perm.unused0;
    perm.unused1 = value.msg_perm.unused1;
    result.msg_perm = perm;
    result.msg_stime = value.msg_stime;
    result.msg_rtime = value.msg_rtime;
    result.msg_ctime = value.msg_ctime;
    result.msg_cbytes = value.msg_cbytes;
    result.msg_qnum = value.msg_qnum;
    result.msg_qbytes = value.msg_qbytes;
    result.msg_lspid = value.msg_lspid;
    result.msg_lrpid = value.msg_lrpid;
    result
}

fn write_msqid_ds<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut msqid_ds,
    value: msqid_ds,
) -> AxResult<()> {
    // SAFETY: `initialized_msqid_ds` zeroes every byte, including the ABI
    // alignment hole, and the layout assertions cover the complete record.
    unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, initialized_msqid_ds(value)) }
        .map_err(map_usercopy_error)
}

impl msqid_ds {
    fn new(key: i32, mode: __kernel_mode_t, _pid: __kernel_pid_t, uid: u32, gid: u32) -> Self {
        let now = ipc_time_secs();
        Self {
            msg_perm: IpcPerm {
                key,
                uid,
                gid,
                cuid: uid,
                cgid: gid,
                mode: mode as _,
                pad1: 0,
                seq: 0,
                pad2: 0,
                unused0: 0,
                unused1: 0,
            },
            msg_stime: 0,
            msg_rtime: 0,
            msg_ctime: now,
            msg_cbytes: 0,
            msg_qnum: 0,
            msg_qbytes: MSGMNB as __kernel_size_t,
            msg_lspid: 0,
            msg_lrpid: 0,
        }
    }
}

/// Single message in the queue
pub struct Message {
    /// message type
    pub mtype: i64,
    /// message data
    pub data: Vec<u8>,
}

struct ReceivedMessage {
    message: Message,
    copy_len: usize,
    removed_waiters: Option<Arc<axtask::WaitQueue>>,
}

fn snapshot_message(message: &Message) -> AxResult<Message> {
    let mut data = Vec::new();
    data.try_reserve_exact(message.data.len())
        .map_err(|_| AxError::NoMemory)?;
    data.extend_from_slice(&message.data);
    Ok(Message {
        mtype: message.mtype,
        data,
    })
}

/// This struct is used to maintain the message queue in kernel.
pub struct MessageQueue {
    /// Message queue data structure
    pub msqid_ds: msqid_ds,
    /// FIFO queue of messages
    pub messages: VecDeque<Message>,
    /// Total bytes in queue
    pub total_bytes: usize,
    /// Marked for removal
    pub mark_removed: bool,
    waiters: Arc<axtask::WaitQueue>,
}

impl MessageQueue {
    /// Creates a new [`MessageQueue`].
    pub fn new(key: i32, mode: __kernel_mode_t, pid: Pid, uid: u32, gid: u32) -> Self {
        MessageQueue {
            msqid_ds: msqid_ds::new(key, mode, pid as __kernel_pid_t, uid, gid),
            messages: VecDeque::new(),
            total_bytes: 0,
            mark_removed: false,
            waiters: Arc::new(axtask::WaitQueue::new()),
        }
    }

    /// Add a message to the queue
    pub fn enqueue_message(&mut self, mtype: i64, data: Vec<u8>) -> AxResult<()> {
        let data_len = data.len();
        if message_queue_would_exceed(self, data_len) {
            return Err(AxError::from(LinuxError::ENOSPC)); // ENOSPC
        }
        self.messages
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        let total_bytes = self
            .total_bytes
            .checked_add(data_len)
            .ok_or(AxError::NoMemory)?;
        let msg_cbytes = self
            .msqid_ds
            .msg_cbytes
            .checked_add(data_len as __kernel_size_t)
            .ok_or(AxError::NoMemory)?;
        let msg_qnum = self
            .msqid_ds
            .msg_qnum
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;

        let message = Message { mtype, data };

        self.messages.push_back(message);
        self.total_bytes = total_bytes;
        self.msqid_ds.msg_cbytes = msg_cbytes;
        self.msqid_ds.msg_qnum = msg_qnum;

        Ok(())
    }

    /// Find the first message (without removing)
    pub fn find_first_message(&self) -> Option<(usize, i64, &[u8])> {
        self.messages
            .front()
            .map(|message| (0, message.mtype, &message.data[..]))
    }

    /// Find message by type (without removing)
    pub fn find_message_by_type(&self, msgtyp: i64) -> Option<(usize, i64, &[u8])> {
        self.messages
            .iter()
            .enumerate()
            .find(|(_, message)| message.mtype == msgtyp)
            .map(|(index, message)| (index, message.mtype, &message.data[..]))
    }

    /// Find the first message with a type not equal to the specified value
    /// (without removing)
    pub fn find_message_not_equal(&self, msgtyp: i64) -> Option<(usize, i64, &[u8])> {
        self.messages
            .iter()
            .enumerate()
            .find(|(_, message)| message.mtype != msgtyp)
            .map(|(index, message)| (index, message.mtype, &message.data[..]))
    }

    /// Find the first message with a type less than or equal to |msgtyp|
    /// (without removing)
    pub fn find_message_less_equal(&self, abs_typ: i64) -> Option<(usize, i64, &[u8])> {
        let mut candidate = None;

        for (index, message) in self.messages.iter().enumerate() {
            if message.mtype <= abs_typ
                && candidate.is_none_or(|(_, candidate_type)| message.mtype < candidate_type)
            {
                candidate = Some((index, message.mtype));
                if message.mtype == 1 {
                    break;
                }
            }
        }

        candidate.map(|(index, mtype)| (index, mtype, &self.messages[index].data[..]))
    }

    /// Get total number of messages in the queue (for MSG_COPY)
    pub fn get_total_message_count(&self) -> usize {
        self.messages.len()
    }

    /// Get message by index (for MSG_COPY)
    pub fn get_message_by_index(&self, index: usize) -> Option<&Message> {
        self.messages.get(index)
    }

    /// Remove the message by FIFO index
    pub fn remove_message_by_index(&mut self, index: usize) -> AxResult<Message> {
        let removed_msg = if index == 0 {
            self.messages.pop_front()
        } else {
            self.messages.remove(index)
        }
        .ok_or(AxError::from(LinuxError::ENOMSG))?;

        self.total_bytes -= removed_msg.data.len();
        self.msqid_ds.msg_cbytes -= removed_msg.data.len() as __kernel_size_t;
        self.msqid_ds.msg_qnum -= 1;

        Ok(removed_msg)
    }
}

fn message_queue_would_exceed(queue: &MessageQueue, data_len: usize) -> bool {
    let byte_limit = queue.msqid_ds.msg_qbytes as usize;
    let would_exceed_bytes = queue
        .total_bytes
        .checked_add(data_len)
        .is_none_or(|total| total > byte_limit);
    let would_exceed_messages = usize::try_from(queue.msqid_ds.msg_qnum)
        .ok()
        .and_then(|messages| messages.checked_add(1))
        .is_none_or(|messages| messages > byte_limit);
    would_exceed_bytes || would_exceed_messages
}

/// Message queue manager
pub struct MsgManager {
    /// key -> msqid mapping
    key_msqid: BTreeMap<i32, i32>,
    /// msqid -> message queue structure
    msqid_queues: BTreeMap<i32, Arc<Mutex<MessageQueue>>>,
}

impl MsgManager {
    const fn new() -> Self {
        MsgManager {
            key_msqid: BTreeMap::new(),
            msqid_queues: BTreeMap::new(),
        }
    }

    /// Returns an iterator over all message queues
    pub fn iter_msg_queues(&self) -> impl Iterator<Item = (i32, &Arc<Mutex<MessageQueue>>)> {
        self.msqid_queues.iter().map(|(&k, v)| (k, v))
    }

    /// Returns an iterator over all message queues, filtering out removed ones
    pub fn iter_active_queues(&self) -> impl Iterator<Item = (i32, &Arc<Mutex<MessageQueue>>)> {
        self.iter_msg_queues().filter(|(_, queue)| {
            let guard = queue.lock();
            !guard.mark_removed
        })
    }

    /// Returns the message queue ID associated with the given key.
    pub fn get_msqid_by_key(&self, key: i32) -> Option<i32> {
        self.key_msqid.get(&key).cloned()
    }

    /// Returns the message queue associated with the given ID.
    pub fn get_queue_by_msqid(&self, msqid: i32) -> Option<Arc<Mutex<MessageQueue>>> {
        self.msqid_queues.get(&msqid).cloned()
    }

    /// Inserts a mapping from a key to a message queue ID.
    pub fn insert_key_msqid(&mut self, key: i32, msqid: i32) {
        self.key_msqid.insert(key, msqid);
    }

    /// Inserts a mapping from a message queue ID to its queue.
    pub fn insert_msqid_queues(&mut self, msqid: i32, msg_queue: Arc<Mutex<MessageQueue>>) {
        self.msqid_queues.insert(msqid, msg_queue);
    }

    /// Returns the current number of message queues.
    pub fn queue_count(&self) -> usize {
        self.msqid_queues.len()
    }

    /// Remove a message queue
    pub fn remove_msqid(&mut self, msqid: i32) {
        self.key_msqid.retain(|_, &mut v| v != msqid);
        self.msqid_queues.remove(&msqid);
    }

    /// get total bytes in all queues
    pub fn total_bytes(&self) -> usize {
        self.iter_active_queues()
            .map(|(_, queue)| {
                let guard = queue.lock();
                guard.total_bytes
            })
            .sum()
    }

    /// get total number of messages in all queues
    pub fn total_messages(&self) -> usize {
        self.iter_active_queues()
            .map(|(_, queue)| {
                let guard = queue.lock();
                guard.get_total_message_count()
            })
            .sum()
    }

    /// get the largest active IPC index
    pub fn max_active_index(&self) -> isize {
        self.iter_active_queues()
            .map(|(msqid, _)| msqid as isize)
            .max()
            .unwrap_or(0)
    }
}

/// System limits
/// Maximum number of message queues
pub const MSGMNI: usize = 32000;
/// Maximum bytes in a message queue
pub const MSGMNB: usize = 16384;
/// Maximum size of a single message
pub const MSGMAX: usize = 8192;

#[derive(Clone, Copy)]
struct MsgSetRequest {
    permission: IpcPermissionUpdateRequest,
    qbytes: __kernel_size_t,
}

#[derive(Clone, Copy)]
struct PreparedMsgSet {
    permission: PreparedIpcPermissionUpdate,
    qbytes: __kernel_size_t,
    ctime: __kernel_time_t,
}

impl PreparedMsgSet {
    fn prepare(
        context: &IpcAccessContext,
        current: &msqid_ds,
        request: MsgSetRequest,
        ctime: __kernel_time_t,
    ) -> AxResult<Self> {
        let permission =
            context.prepare_permission_update(&current.msg_perm, request.permission)?;
        if request.qbytes > MSGMNB as __kernel_size_t && !context.may_raise_resource_limit() {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(Self {
            permission,
            qbytes: request.qbytes,
            ctime,
        })
    }

    fn commit(self, queue: &mut MessageQueue) {
        self.permission.commit(&mut queue.msqid_ds.msg_perm);
        queue.msqid_ds.msg_qbytes = self.qbytes;
        queue.msqid_ds.msg_ctime = self.ctime;
    }
}

/// Global message queue manager
pub static MSG_MANAGER: Mutex<MsgManager> = Mutex::new(MsgManager::new());
static MSGMNI_LIMIT: AtomicUsize = AtomicUsize::new(MSGMNI);
static MSG_NEXT_ID: AtomicI32 = AtomicI32::new(-1);

fn allocate_msg_id(msg_manager: &MsgManager) -> i32 {
    let desired = MSG_NEXT_ID.swap(-1, Ordering::Relaxed);
    if desired >= 0 && !msg_manager.msqid_queues.contains_key(&desired) {
        desired
    } else {
        loop {
            let candidate = next_ipc_id();
            if !msg_manager.msqid_queues.contains_key(&candidate) {
                return candidate;
            }
        }
    }
}

pub(crate) fn msgmni_limit() -> usize {
    MSGMNI_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn set_msgmni_limit(value: usize) {
    MSGMNI_LIMIT.store(value.max(1), Ordering::Relaxed);
}

pub(crate) fn msg_next_id() -> i32 {
    MSG_NEXT_ID.load(Ordering::Relaxed)
}

pub(crate) fn set_msg_next_id(value: i32) -> AxResult<()> {
    if value < -1 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    MSG_NEXT_ID.store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn sysvipc_msg_snapshot() -> String {
    let mut out =
        String::from(
            "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  \
             cgid      stime      rtime      ctime\n",
        );
    let msg_manager = MSG_MANAGER.lock();
    for (msqid, queue) in msg_manager.iter_active_queues() {
        let queue = queue.lock();
        let ds = queue.msqid_ds;
        let _ = writeln!(
            out,
            "{:10} {:10} {:5o} {:11} {:10} {:5} {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10}",
            ds.msg_perm.key,
            msqid,
            ds.msg_perm.mode & 0o777,
            ds.msg_cbytes,
            ds.msg_qnum,
            ds.msg_lspid,
            ds.msg_lrpid,
            ds.msg_perm.uid,
            ds.msg_perm.gid,
            ds.msg_perm.cuid,
            ds.msg_perm.cgid,
            ds.msg_stime,
            ds.msg_rtime,
            ds.msg_ctime,
        );
    }
    out
}

#[repr(C)]
struct MsgInfo {
    msgpool: i32,
    msgmap: i32,
    msgmax: i32,
    msgmnb: i32,
    msgmni: i32,
    msgssz: i32,
    msgtql: i32,
    msgseg: u16,
    __padding: u16,
}

const _: () = {
    assert!(align_of::<MsgInfo>() == 4);
    assert!(size_of::<MsgInfo>() == 32);
    assert!(offset_of!(MsgInfo, msgpool) == 0);
    assert!(offset_of!(MsgInfo, msgmap) == 4);
    assert!(offset_of!(MsgInfo, msgmax) == 8);
    assert!(offset_of!(MsgInfo, msgmnb) == 12);
    assert!(offset_of!(MsgInfo, msgmni) == 16);
    assert!(offset_of!(MsgInfo, msgssz) == 20);
    assert!(offset_of!(MsgInfo, msgtql) == 24);
    assert!(offset_of!(MsgInfo, msgseg) == 28);
    assert!(offset_of!(MsgInfo, __padding) == 30);
};

impl MsgInfo {
    fn ipc_info() -> Self {
        Self {
            msgpool: MSGMNI.saturating_mul(MSGMNB / 1024) as i32,
            msgmap: MSGMNB as i32,
            msgmax: MSGMAX as i32,
            msgmnb: MSGMNB as i32,
            msgmni: msgmni_limit() as i32,
            msgssz: 16,
            msgtql: MSGMNB as i32,
            msgseg: u16::MAX,
            __padding: 0,
        }
    }

    // Mirrors the `MSG_INFO` msgctl command rather than a Rust constructor
    // convention; the name is the Linux operation it answers.
    #[allow(clippy::self_named_constructors)]
    fn msg_info(msg_manager: &MsgManager) -> Self {
        let mut info = Self::ipc_info();
        info.msgpool = msg_manager.queue_count().min(i32::MAX as usize) as i32;
        info.msgmap = msg_manager.total_messages().min(i32::MAX as usize) as i32;
        info.msgtql = msg_manager.total_bytes().min(i32::MAX as usize) as i32;
        info
    }
}

bitflags::bitflags! {
    /// Flags for msgrcv
    #[derive(Debug)]
    pub struct MsgRcvFlags: i32 {
        /// Non-blocking receive (return immediately if no message)
        const IPC_NOWAIT = 0o4000;
        /// Truncate message if too long (instead of failing)
        const MSG_NOERROR = 0o10000;
        /// For internal use - mark as COPIED
        const MSG_COPY = 0o40000;
        /// Receive any message except of specified type (Linux extension)
        const MSG_EXCEPT = 0o20000;
    }
}

bitflags::bitflags! {
    /// Flags for msgsnd
    #[derive(Debug)]
    pub struct MsgSndFlags: i32 {
        /// Non-blocking send (return immediately if queue full)
        const IPC_NOWAIT = 0o4000;
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct UserMsgbuf {
    pub mtype: i64,     // type of message
    pub mtext: [u8; 0], // actual data, use zero-sized array to simulate flexible array
}

pub fn sys_msgget(key: i32, msgflg: i32) -> AxResult<isize> {
    let current = current();
    let thread = current.as_thread();
    let proc_data = &thread.proc_data;
    let context = IpcAccessContext::for_initial_user_namespace(thread.current_cred());
    let current_uid = context.effective_uid_raw();
    let current_gid = context.effective_gid_raw();
    let current_pid = proc_data.proc.pid();

    let mut msg_manager = MSG_MANAGER.lock();

    // Handle IPC_PRIVATE (always create new queue)
    if key == IPC_PRIVATE {
        if msg_manager.queue_count() >= msgmni_limit() {
            return Err(AxError::from(LinuxError::ENOSPC)); // ENOSPC
        }
        let msqid = allocate_msg_id(&msg_manager);
        let msg_queue = Arc::new(Mutex::new(MessageQueue::new(
            key,
            (msgflg & 0o777) as _,
            current_pid,
            current_uid,
            current_gid,
        )));

        msg_manager.insert_msqid_queues(msqid, msg_queue);
        return Ok(msqid as isize);
    }

    // Look for existing message queue
    if let Some(msqid) = msg_manager.get_msqid_by_key(key) {
        let msg_queue = msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(AxError::from(LinuxError::ENOENT))?; // ENOENT

        let msg_queue = msg_queue.lock();

        // Check permissions
        if !context.allows(&msg_queue.msqid_ds.msg_perm, IpcAccess::Read) {
            return Err(AxError::from(LinuxError::EACCES)); // EACCES
        }

        // Check if marked for removal
        if msg_queue.mark_removed {
            return Err(AxError::from(LinuxError::EIDRM)); // EIDRM
        }

        // Check IPC_EXCL flag
        if (msgflg & IPC_EXCL) != 0 && (msgflg & IPC_CREAT) != 0 {
            return Err(AxError::from(LinuxError::EEXIST)); // EEXIST
        }

        return Ok(msqid as isize);
    }

    // Create new message queue
    if (msgflg & IPC_CREAT) == 0 {
        return Err(AxError::from(LinuxError::ENOENT)); // ENOENT
    }
    if msg_manager.queue_count() >= msgmni_limit() {
        return Err(AxError::from(LinuxError::ENOSPC)); // ENOSPC
    }

    let msqid = allocate_msg_id(&msg_manager);
    let msg_queue = Arc::new(Mutex::new(MessageQueue::new(
        key,
        (msgflg & 0o777) as _,
        current_pid,
        current_uid,
        current_gid,
    )));

    msg_manager.insert_key_msqid(key, msqid);
    msg_manager.insert_msqid_queues(msqid, msg_queue);

    Ok(msqid as isize)
}

pub fn sys_msgsnd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    msqid: i32,
    msgp: *const UserMsgbuf,
    msgsz: usize,
    msgflg: i32,
) -> AxResult<isize> {
    // MSGMAX = 8192
    if msgsz > MSGMAX {
        return Err(AxError::from(LinuxError::EINVAL)); // EINVAL
    }
    let current = current();
    let thread = current.as_thread();
    let proc_data = &thread.proc_data;
    let context = IpcAccessContext::for_initial_user_namespace(thread.current_cred());
    let current_pid = proc_data.proc.pid();
    let flags = MsgSndFlags::from_bits_truncate(msgflg);

    let msg_queue = {
        let msg_manager = MSG_MANAGER.lock();
        msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(AxError::from(LinuxError::EINVAL))? // EINVAL - queue does not exist
    };

    // read message from user space
    let mtype_ptr = unsafe { core::ptr::addr_of!((*msgp).mtype) };
    let mtype: i64 = VmPtr::vm_read(mtype_ptr, memory).map_err(map_usercopy_error)?;

    if mtype <= 0 {
        return Err(AxError::from(LinuxError::EINVAL)); // EINVAL - invalid message type
    }

    // read data part
    let mtext_ptr = unsafe { core::ptr::addr_of!((*msgp).mtext) };
    let data_vec = vm_load(memory, mtext_ptr.cast::<u8>(), msgsz).map_err(map_usercopy_error)?;

    loop {
        let waiters = {
            let mut msg_queue = msg_queue.lock();

            if !context.allows(&msg_queue.msqid_ds.msg_perm, IpcAccess::Write) {
                return Err(AxError::from(LinuxError::EACCES)); // EACCES
            }

            if msg_queue.mark_removed {
                return Err(AxError::from(LinuxError::EIDRM));
            }

            // Note: According to Linux manpage, both byte count and message count
            // are limited by msg_qbytes field (this appears to be the actual behavior)
            if !message_queue_would_exceed(&msg_queue, data_vec.len()) {
                msg_queue.enqueue_message(mtype, data_vec)?;
                msg_queue.msqid_ds.msg_lspid = current_pid as _;
                msg_queue.msqid_ds.msg_stime = ipc_time_secs();
                msg_queue.waiters.notify_all(false);
                return Ok(0);
            }

            if flags.contains(MsgSndFlags::IPC_NOWAIT) {
                return Err(AxError::from(LinuxError::EAGAIN)); // EAGAIN
            }

            msg_queue.waiters.clone()
        };

        if has_pending_syscall_signal(thread) {
            return Err(AxError::Interrupted);
        }
        with_proc_state_hint(ProcStateHint::Interruptible, || {
            waiters.wait_until_interruptible(|| {
                let queue = msg_queue.lock();
                queue.mark_removed
                    || !message_queue_would_exceed(&queue, data_vec.len())
                    || has_pending_syscall_signal(thread)
            })
        })
        .map_err(AxError::from)?;
    }
}

fn find_msgrcv_message(
    msg_queue: &MessageQueue,
    msgtyp: i64,
    flags: &MsgRcvFlags,
) -> Option<(usize, bool)> {
    if flags.contains(MsgRcvFlags::MSG_COPY) {
        let index = msgtyp as usize;
        msg_queue.get_message_by_index(index)?;
        return Some((index, false));
    }

    let matched_message = match msgtyp {
        0 => msg_queue.find_first_message(), // First message
        typ if typ > 0 => {
            if flags.contains(MsgRcvFlags::MSG_EXCEPT) {
                msg_queue.find_message_not_equal(typ) // Type not equal to msgtyp
            } else {
                msg_queue.find_message_by_type(typ) // Type equal to msgtyp
            }
        }
        typ if typ < 0 => {
            let abs_typ = typ.abs();
            msg_queue.find_message_less_equal(abs_typ) // Type ≤ |msgtyp|
        }
        _ => None,
    };

    matched_message.map(|(index, ..)| (index, true))
}

fn prepare_received_message(
    msg_queue: &mut MessageQueue,
    msgtyp: i64,
    msgsz: usize,
    flags: &MsgRcvFlags,
    current_pid: __kernel_pid_t,
) -> AxResult<Option<ReceivedMessage>> {
    let Some((index, should_remove)) = find_msgrcv_message(msg_queue, msgtyp, flags) else {
        return Ok(None);
    };

    let data_len = msg_queue
        .get_message_by_index(index)
        .ok_or(AxError::from(LinuxError::ENOMSG))?
        .data
        .len();
    if data_len > msgsz && !flags.contains(MsgRcvFlags::MSG_NOERROR) {
        return Err(AxError::from(LinuxError::E2BIG));
    }

    // Snapshot the selected message while the queue is locked, then perform
    // all potentially faulting usercopy after the lock is released. A normal
    // receive unlinks the message before copyout, so EFAULT still consumes it.
    // MSG_COPY is nondestructive: retain an owned snapshot but leave queue
    // contents and receive metadata unchanged even on EFAULT.
    let message = if should_remove {
        msg_queue.remove_message_by_index(index)?
    } else {
        snapshot_message(
            msg_queue
                .get_message_by_index(index)
                .ok_or(AxError::from(LinuxError::ENOMSG))?,
        )?
    };

    if should_remove {
        msg_queue.msqid_ds.msg_lrpid = current_pid as _;
        msg_queue.msqid_ds.msg_rtime = ipc_time_secs();
    }

    Ok(Some(ReceivedMessage {
        copy_len: message.data.len().min(msgsz),
        message,
        removed_waiters: should_remove.then(|| msg_queue.waiters.clone()),
    }))
}

fn copy_received_message<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    msgp: *mut UserMsgbuf,
    received: ReceivedMessage,
) -> AxResult<isize> {
    // Keep mtype-before-payload order so a payload fault has the same
    // partial-copy behavior as the old path.
    let mtype_ptr = unsafe { core::ptr::addr_of_mut!((*msgp).mtype) };
    VmMutPtr::vm_write(mtype_ptr, memory, received.message.mtype).map_err(map_usercopy_error)?;

    let data_ptr = unsafe { core::ptr::addr_of_mut!((*msgp).mtext) };
    vm_write_slice(
        memory,
        data_ptr.cast::<u8>(),
        &received.message.data[..received.copy_len],
    )
    .map_err(map_usercopy_error)?;

    Ok(received.copy_len as isize)
}

pub fn sys_msgrcv<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    msqid: i32,
    msgp: *mut UserMsgbuf,
    msgsz: usize,
    msgtyp: i64,
    msgflg: i32,
) -> AxResult<isize> {
    // Parse flags and get current process information

    let flags = MsgRcvFlags::from_bits_truncate(msgflg);
    let current = current();
    let thread = current.as_thread();
    let proc_data = &thread.proc_data;
    let context = IpcAccessContext::for_initial_user_namespace(thread.current_cred());
    let current_pid = proc_data.proc.pid();

    // Check validity of flag combinations
    if flags.contains(MsgRcvFlags::MSG_COPY) {
        if !flags.contains(MsgRcvFlags::IPC_NOWAIT) {
            return Err(AxError::from(LinuxError::EINVAL)); // EINVAL - MSG_COPY must be used with IPC_NOWAIT
        }
        if flags.contains(MsgRcvFlags::MSG_EXCEPT) {
            return Err(AxError::from(LinuxError::EINVAL)); // EINVAL - MSG_COPY and MSG_EXCEPT are mutually exclusive
        }
    }

    // Get the message queue
    let msg_queue = {
        let msg_manager = MSG_MANAGER.lock();
        msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(AxError::from(LinuxError::EINVAL))? // EINVAL
    };

    loop {
        let (received, waiters) = {
            let mut msg_queue = msg_queue.lock();

            // Permission check
            if !context.allows(&msg_queue.msqid_ds.msg_perm, IpcAccess::Read) {
                return Err(AxError::from(LinuxError::EACCES)); // EACCES
            }

            if msg_queue.mark_removed {
                return Err(AxError::from(LinuxError::EIDRM)); // EIDRM
            }

            if let Some(received) = prepare_received_message(
                &mut msg_queue,
                msgtyp,
                msgsz,
                &flags,
                current_pid as __kernel_pid_t,
            )? {
                (Some(received), None)
            } else if flags.contains(MsgRcvFlags::IPC_NOWAIT) {
                return Err(AxError::from(LinuxError::ENOMSG)); // ENOMSG
            } else {
                (None, Some(msg_queue.waiters.clone()))
            }
        };

        if let Some(received) = received {
            if let Some(waiters) = received.removed_waiters.as_ref() {
                waiters.notify_all(false);
            }
            return copy_received_message(memory, msgp, received);
        }

        if has_pending_syscall_signal(thread) {
            return Err(AxError::Interrupted);
        }
        let waiters = waiters.expect("blocking receive has a wait queue");
        with_proc_state_hint(ProcStateHint::Interruptible, || {
            waiters.wait_until_interruptible(|| {
                let queue = msg_queue.lock();
                queue.mark_removed
                    || find_msgrcv_message(&queue, msgtyp, &flags).is_some()
                    || has_pending_syscall_signal(thread)
            })
        })
        .map_err(AxError::from)?;
    }
}

pub fn sys_msgctl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    msqid: i32,
    cmd: i32,
    buf: usize,
) -> AxResult<isize> {
    let current = current();
    let context = IpcAccessContext::for_initial_user_namespace(current.as_thread().current_cred());

    // Validate command code
    if cmd != IPC_STAT
        && cmd != IPC_SET
        && cmd != IPC_RMID
        && cmd != IPC_INFO
        && cmd != MSG_INFO
        && cmd != MSG_STAT
        && cmd != MSG_STAT_ANY
    {
        // Simplified: do not support some Linux extensions
        return Err(AxError::from(LinuxError::EINVAL)); // EINVAL
    }

    // IPC_INFO (put before looking up the queue!)
    if cmd == IPC_INFO {
        let msg_manager = MSG_MANAGER.lock();
        let info = MsgInfo::ipc_info();
        let ptr = buf as *mut MsgInfo;
        // SAFETY: `MsgInfo` has an explicit initialized tail field for the
        // Linux ABI's two-byte trailing padding; its integer layout is fixed
        // by repr(C) and asserted below.
        unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, info) }.map_err(map_usercopy_error)?;
        return Ok(msg_manager.max_active_index());
    }

    // MSG_INFO (put before looking up the queue!)
    if cmd == MSG_INFO {
        let msg_manager = MSG_MANAGER.lock();
        let info = MsgInfo::msg_info(&msg_manager);
        let ptr = buf as *mut MsgInfo;
        // SAFETY: see the IPC_INFO copyout above.
        unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, info) }.map_err(map_usercopy_error)?;
        return Ok(msg_manager.max_active_index());
    }
    // MSG_STAT and MSG_STAT_ANY use an IPC index and return the real queue ID.
    if cmd == MSG_STAT || cmd == MSG_STAT_ANY {
        let msg_manager = MSG_MANAGER.lock();

        let result = msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(AxError::from(LinuxError::EINVAL))
            .map(|queue| (msqid, queue))
            .and_then(|(actual_msqid, queue)| {
                let guard = queue.lock();

                if cmd == MSG_STAT && !context.allows(&guard.msqid_ds.msg_perm, IpcAccess::Read) {
                    return Err(AxError::from(LinuxError::EACCES));
                }

                let ptr = buf as *mut msqid_ds;
                write_msqid_ds(memory, ptr, guard.msqid_ds)?;
                Ok(actual_msqid as isize)
            });

        return result;
    }

    // Find message queue by msqid
    let msg_queue = {
        let msg_manager = MSG_MANAGER.lock();
        msg_manager
            .get_queue_by_msqid(msqid)
            .ok_or(AxError::from(LinuxError::EINVAL))? // EINVAL - Queue does not exist
    };

    // IPC_SET is a prepare/authorize/commit transaction. Potentially faulting
    // usercopy and namespace ID mapping happen before the live queue lock is
    // acquired. No queue field is changed until every check has succeeded.
    let set_request = if cmd == IPC_SET {
        let user_buf =
            VmPtr::vm_read(buf as *const msqid_ds, memory).map_err(map_usercopy_error)?;
        Some(MsgSetRequest {
            permission: context.map_permission_update(
                user_buf.msg_perm.uid,
                user_buf.msg_perm.gid,
                user_buf.msg_perm.mode,
            )?,
            qbytes: user_buf.msg_qbytes,
        })
    } else {
        None
    };

    // Lock the internal structure of the queue
    let mut msg_queue = msg_queue.lock();
    // Check if the queue is marked as removed
    if msg_queue.mark_removed {
        return Err(AxError::from(LinuxError::EIDRM)); // EIDRM - Queue has been removed
    }
    if cmd == IPC_STAT {
        // Check read permissions
        if !context.allows(&msg_queue.msqid_ds.msg_perm, IpcAccess::Read) {
            return Err(AxError::from(LinuxError::EACCES)); // EACCES
        }

        // Copy queue status to user space
        let ptr = buf as *mut msqid_ds;
        write_msqid_ds(memory, ptr, msg_queue.msqid_ds)?;

        return Ok(0);
    }

    if !context.may_control(&msg_queue.msqid_ds.msg_perm) {
        return Err(AxError::from(LinuxError::EPERM)); // EPERM
    }

    if cmd == IPC_SET {
        let prepared = PreparedMsgSet::prepare(
            &context,
            &msg_queue.msqid_ds,
            set_request.expect("IPC_SET request was prepared before locking"),
            ipc_time_secs(),
        )?;
        prepared.commit(&mut msg_queue);
        msg_queue.waiters.notify_all(false);

        return Ok(0);
    }
    if cmd == IPC_RMID {
        // Mark the queue as removed
        msg_queue.mark_removed = true;
        msg_queue.msqid_ds.msg_ctime = ipc_time_secs();
        msg_queue.waiters.notify_all(false);

        drop(msg_queue); // Release the lock to avoid deadlock

        MSG_MANAGER.lock().remove_msqid(msqid);

        return Ok(0);
    }
    // Currently unsupported operations
    // some Linux-specific extensions
    // These Linux-specific extensions are not implemented for now because the basic
    // operations are sufficient and these are not POSIX standard They can be
    // implemented later to support tools like ipcs
    Err(AxError::from(LinuxError::EINVAL)) // EINVAL
}

#[cfg(test)]
mod credential_caller_tests {
    use super::*;
    use crate::task::{Cred, UserNamespace};

    #[test]
    fn credential_caller_msg_set_resource_failure_rolls_back_every_field() {
        let root_ns = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(root_ns.clone()).unwrap();
        let mut context = IpcAccessContext::new(actor, root_ns);
        context.authority.resource_override = false;

        let mut queue = MessageQueue::new(1, 0o600, 1, 0, 0);
        queue.msqid_ds.msg_ctime = 41;
        let before = (
            queue.msqid_ds.msg_perm.uid,
            queue.msqid_ds.msg_perm.gid,
            queue.msqid_ds.msg_perm.mode,
            queue.msqid_ds.msg_qbytes,
            queue.msqid_ds.msg_ctime,
        );
        let request = MsgSetRequest {
            permission: context.map_permission_update(0, 0, 0o666).unwrap(),
            qbytes: MSGMNB as __kernel_size_t + 1,
        };

        let result = PreparedMsgSet::prepare(&context, &queue.msqid_ds, request, 99)
            .map(|prepared| prepared.commit(&mut queue));
        assert_eq!(result, Err(AxError::OperationNotPermitted));
        assert_eq!(
            (
                queue.msqid_ds.msg_perm.uid,
                queue.msqid_ds.msg_perm.gid,
                queue.msqid_ds.msg_perm.mode,
                queue.msqid_ds.msg_qbytes,
                queue.msqid_ds.msg_ctime,
            ),
            before
        );
    }

    #[test]
    fn credential_caller_cap_sys_resource_only_lifts_qbytes_ceiling() {
        let root_ns = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(root_ns.clone()).unwrap();
        let mut context = IpcAccessContext::new(actor, root_ns);
        context.authority = super::super::IpcAuthority {
            resource_override: true,
            ..super::super::IpcAuthority::NONE
        };
        let request = MsgSetRequest {
            permission: context.map_permission_update(0, 0, 0o600).unwrap(),
            qbytes: MSGMNB as __kernel_size_t + 1,
        };

        let owned = MessageQueue::new(1, 0o600, 1, 0, 0);
        assert!(PreparedMsgSet::prepare(&context, &owned.msqid_ds, request, 99).is_ok());

        let foreign = MessageQueue::new(1, 0o600, 1, 2000, 200);
        assert!(matches!(
            PreparedMsgSet::prepare(&context, &foreign.msqid_ds, request, 99),
            Err(AxError::OperationNotPermitted)
        ));
    }
}

#[cfg(test)]
mod receive_copyout_tests {
    use core::mem::MaybeUninit;

    use thekernel_linux_usercopy::{UserCopyError, VmResult};

    use super::*;

    struct FaultMemory;

    // SAFETY: this provider deliberately faults every user access.
    unsafe impl UserMemory for FaultMemory {
        fn read(&mut self, _start: usize, _dst: &mut [MaybeUninit<u8>]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }

        fn write(&mut self, _start: usize, _src: &[u8]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }
    }

    fn queue_with_one_message() -> MessageQueue {
        let mut queue = MessageQueue::new(1, 0o600, 1, 0, 0);
        queue.enqueue_message(7, b"payload".to_vec()).unwrap();
        queue
    }

    #[test]
    fn receive_fault_consumes_normal_message_before_copyout() {
        let mut queue = queue_with_one_message();
        let flags = MsgRcvFlags::empty();
        let received = prepare_received_message(&mut queue, 0, 64, &flags, 77)
            .unwrap()
            .unwrap();
        assert_eq!(queue.messages.len(), 0);

        let mut provider = FaultMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            copy_received_message(
                &mut memory,
                core::ptr::dangling_mut::<UserMsgbuf>(),
                received
            ),
            Err(AxError::BadAddress)
        );
        assert_eq!(queue.messages.len(), 0);
    }

    #[test]
    fn msg_copy_fault_leaves_message_queued() {
        let mut queue = queue_with_one_message();
        let flags = MsgRcvFlags::MSG_COPY | MsgRcvFlags::IPC_NOWAIT;
        let received = prepare_received_message(&mut queue, 0, 64, &flags, 77)
            .unwrap()
            .unwrap();
        assert_eq!(queue.messages.len(), 1);
        assert_eq!(queue.total_bytes, b"payload".len());

        let mut provider = FaultMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            copy_received_message(
                &mut memory,
                core::ptr::dangling_mut::<UserMsgbuf>(),
                received
            ),
            Err(AxError::BadAddress)
        );
        assert_eq!(queue.messages.len(), 1);
        assert_eq!(queue.total_bytes, b"payload".len());
    }
}
