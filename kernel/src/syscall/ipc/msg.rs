use alloc::{
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{
    fmt::Write as _,
    sync::atomic::{AtomicI32, AtomicUsize, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::*;
use starry_process::Pid;
use starry_vm::{VmMutPtr, VmPtr, vm_load, vm_write_slice};

use super::{
    IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, IpcPerm, MSG_INFO,
    MSG_STAT, MSG_STAT_ANY, has_ipc_permission, next_ipc_id,
};
use crate::{
    task::{AsThread, ProcStateHint, has_pending_syscall_signal, with_proc_state_hint},
    time::wall_time,
};

const MSG_WAIT_SLICE: Duration = Duration::from_millis(10);

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
        // Check queue size limits
        if self.total_bytes + data_len > self.msqid_ds.msg_qbytes as usize {
            return Err(AxError::from(LinuxError::ENOSPC)); // ENOSPC
        }

        let message = Message { mtype, data };

        self.messages.push_back(message);
        self.total_bytes += data_len;
        self.msqid_ds.msg_cbytes += data_len as __kernel_size_t;
        self.msqid_ds.msg_qnum += 1;

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
}

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
        }
    }

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
    let ids = proc_data.current_cred().ids();
    let current_uid = ids.euid;
    let current_gid = ids.egid;
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
        if !has_ipc_permission(
            &msg_queue.msqid_ds.msg_perm,
            current_uid,
            current_gid,
            false,
        ) {
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

pub fn sys_msgsnd(
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
    let ids = proc_data.current_cred().ids();
    let current_uid = ids.euid;
    let current_gid = ids.egid;
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
    let mtype: i64 = mtype_ptr.vm_read()?;

    if mtype <= 0 {
        return Err(AxError::from(LinuxError::EINVAL)); // EINVAL - invalid message type
    }

    // read data part
    let mtext_ptr = unsafe { core::ptr::addr_of!((*msgp).mtext) };
    let data_vec = vm_load(mtext_ptr.cast::<u8>(), msgsz)?;

    loop {
        let waiters = {
            let mut msg_queue = msg_queue.lock();

            if !has_ipc_permission(
                &msg_queue.msqid_ds.msg_perm,
                current_uid as _,
                current_gid as _,
                true,
            ) {
                return Err(AxError::from(LinuxError::EACCES)); // EACCES
            }

            if msg_queue.mark_removed {
                return Err(AxError::from(LinuxError::EIDRM));
            }

            // Note: According to Linux manpage, both byte count and message count
            // are limited by msg_qbytes field (this appears to be the actual behavior)
            let would_exceed_bytes =
                msg_queue.total_bytes + data_vec.len() > msg_queue.msqid_ds.msg_qbytes as usize;
            let would_exceed_messages =
                (msg_queue.msqid_ds.msg_qnum + 1) as usize > msg_queue.msqid_ds.msg_qbytes as usize;

            if !would_exceed_bytes && !would_exceed_messages {
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
            waiters.wait_timeout(MSG_WAIT_SLICE);
        });
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

pub fn sys_msgrcv(
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
    let ids = proc_data.current_cred().ids();
    let current_uid = ids.euid;
    let current_gid = ids.egid;
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
        let waiters = {
            let mut msg_queue = msg_queue.lock();

            // Permission check
            if !has_ipc_permission(
                &msg_queue.msqid_ds.msg_perm,
                current_uid as _,
                current_gid as _,
                false,
            ) {
                return Err(AxError::from(LinuxError::EACCES)); // EACCES
            }

            if msg_queue.mark_removed {
                return Err(AxError::from(LinuxError::EIDRM)); // EIDRM
            }

            if let Some((index, should_remove)) = find_msgrcv_message(&msg_queue, msgtyp, &flags) {
                // Message size check
                let data_len = msg_queue
                    .get_message_by_index(index)
                    .ok_or(AxError::from(LinuxError::ENOMSG))?
                    .data
                    .len();
                if data_len > msgsz && !flags.contains(MsgRcvFlags::MSG_NOERROR) {
                    return Err(AxError::from(LinuxError::E2BIG)); // E2BIG
                }

                let copy_len = {
                    let message = msg_queue
                        .get_message_by_index(index)
                        .ok_or(AxError::from(LinuxError::ENOMSG))?;
                    let copy_len = message.data.len().min(msgsz);

                    // Write mtype
                    let mtype_ptr = unsafe { core::ptr::addr_of_mut!((*msgp).mtype) };
                    mtype_ptr.vm_write(message.mtype)?;

                    // Write data part
                    let data_ptr = unsafe { core::ptr::addr_of_mut!((*msgp).mtext) };
                    vm_write_slice(data_ptr.cast::<u8>(), &message.data[..copy_len])?;

                    copy_len
                };

                if should_remove {
                    msg_queue.remove_message_by_index(index)?;
                }

                msg_queue.msqid_ds.msg_lrpid = current_pid as _;
                msg_queue.msqid_ds.msg_rtime = ipc_time_secs();

                if should_remove {
                    msg_queue.waiters.notify_all(false);
                }

                return Ok(copy_len as isize);
            }

            if flags.contains(MsgRcvFlags::IPC_NOWAIT) {
                return Err(AxError::from(LinuxError::ENOMSG)); // ENOMSG
            }

            msg_queue.waiters.clone()
        };

        if has_pending_syscall_signal(thread) {
            return Err(AxError::Interrupted);
        }
        with_proc_state_hint(ProcStateHint::Interruptible, || {
            waiters.wait_timeout(MSG_WAIT_SLICE);
        });
    }
}

pub fn sys_msgctl(msqid: i32, cmd: i32, buf: usize) -> AxResult<isize> {
    //  Get current process information
    let current = current();
    let proc_data = &current.as_thread().proc_data;
    let ids = proc_data.current_cred().ids();
    let current_uid = ids.euid;
    let current_gid = ids.egid;
    let is_privileged = current_uid == 0; // root user check

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
        ptr.vm_write(info)?;
        return Ok(msg_manager.max_active_index());
    }

    // MSG_INFO (put before looking up the queue!)
    if cmd == MSG_INFO {
        let msg_manager = MSG_MANAGER.lock();
        let info = MsgInfo::msg_info(&msg_manager);
        let ptr = buf as *mut MsgInfo;
        ptr.vm_write(info)?;
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

                if cmd == MSG_STAT
                    && !has_ipc_permission(
                        &guard.msqid_ds.msg_perm,
                        current_uid,
                        current_gid,
                        false, // read permission check
                    )
                {
                    return Err(AxError::from(LinuxError::EACCES));
                }

                let ptr = buf as *mut msqid_ds;
                ptr.vm_write(guard.msqid_ds)?;
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

    // Lock the internal structure of the queue
    let mut msg_queue = msg_queue.lock();
    // Check if the queue is marked as removed
    if msg_queue.mark_removed {
        return Err(AxError::from(LinuxError::EIDRM)); // EIDRM - Queue has been removed
    }
    if cmd == IPC_STAT {
        // Check read permissions
        if !has_ipc_permission(
            &msg_queue.msqid_ds.msg_perm,
            current_uid,
            current_gid,
            false,
        ) {
            return Err(AxError::from(LinuxError::EACCES)); // EACCES
        }

        // Copy queue status to user space
        let ptr = buf as *mut msqid_ds;
        ptr.vm_write(msg_queue.msqid_ds)?;

        return Ok(0);
    }

    // Check permissions (owner, creator, or privileged user)
    let is_owner = current_uid == msg_queue.msqid_ds.msg_perm.uid;
    let is_creator = current_uid == msg_queue.msqid_ds.msg_perm.cuid;

    if !is_privileged && !is_owner && !is_creator {
        return Err(AxError::from(LinuxError::EPERM)); // EPERM
    }

    if cmd == IPC_SET {
        // Read new settings from user space
        let ptr = buf as *const msqid_ds;
        let user_buf = ptr.vm_read()?;

        // Update permission information (fields allowed by man-page)
        msg_queue.msqid_ds.msg_perm.uid = user_buf.msg_perm.uid;
        msg_queue.msqid_ds.msg_perm.gid = user_buf.msg_perm.gid;
        msg_queue.msqid_ds.msg_perm.mode = user_buf.msg_perm.mode & 0o777; // Only take permission bits

        // Update queue size limit (requires privilege check)
        if user_buf.msg_qbytes != msg_queue.msqid_ds.msg_qbytes {
            if user_buf.msg_qbytes > MSGMNB as _ && !is_privileged {
                return Err(AxError::from(LinuxError::EPERM)); // EPERM - requires privilege to exceed MSGMNB
            }
            msg_queue.msqid_ds.msg_qbytes = user_buf.msg_qbytes;
        }

        // Update modification time
        msg_queue.msqid_ds.msg_ctime = ipc_time_secs();
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
