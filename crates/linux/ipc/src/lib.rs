//! Pure System V IPC and POSIX mqueue policy. It owns neither managers, queues, nor VM.
#![no_std]
#![forbid(unsafe_code)]

pub const IPC_NOWAIT: u16 = 0o4000;
pub const SEM_UNDO: u16 = 0x1000;
pub const SHM_RDONLY: u32 = 0o10000;
pub const SHM_RND: u32 = 0o20000;
/// Linux `include/uapi/linux/shm.h`: accept the segment without reserving
/// swap for it.  It shares its value with `SHM_RDONLY` because they belong to
/// different syscalls.
pub const SHM_NORESERVE: u32 = 0o10000;
pub const SHM_HUGETLB: u32 = 0o4000;
/// Linux `include/uapi/linux/shm.h`: the `SHM_HUGE_*` page-size hint.
pub const SHM_HUGE_SHIFT: u32 = 26;
pub const SHM_HUGE_MASK: u32 = 0x3f;
pub const SHMLBA: usize = 4096;
/// Linux `MQ_PRIO_MAX` (`include/uapi/linux/mqueue.h`). `mq_timedsend()`
/// rejects `msg_prio >= MQ_PRIO_MAX` with `EINVAL`.
pub const MQ_PRIO_MAX: u32 = 32768;
/// Linux `ipc/sem.c`: `SEMVMX`, the largest semaphore value, and `SEMAEM`, the
/// largest magnitude of one `sem_undo` adjustment.
pub const SEMVMX: i32 = 32767;
pub const SEMAEM: i32 = SEMVMX;


/// `sizeof(struct msg_msg)` (`include/linux/msg.h`) on x86_64: list_head plus
/// `m_type`, `m_ts`, `next` and `security`. `mqueue_get_inode()` charges one
/// per `mq_maxmsg` slot.
pub const MQ_MSG_OVERHEAD: u64 = 48;

/// `sizeof(struct posix_msg_tree_node)` (`ipc/mqueue.c`) on x86_64: `rb_node`,
/// `msg_list` and `priority`. Linux pins `min(mq_maxmsg, MQ_PRIO_MAX)` of them
/// because at most one node exists per distinct priority actually in use.
pub const MQ_TREE_NODE_OVERHEAD: u64 = 48;

/// Namespace defaults from `mq_init_ns()` (`ipc/mqueue.c`) and
/// `include/linux/ipc_namespace.h`. They are the values `/proc/sys/fs/mqueue`
/// publishes until an administrator overrides them.
pub const MQ_QUEUES_MAX_DEFAULT: usize = 256;
pub const MQ_MSG_MAX_DEFAULT: usize = 10;
pub const MQ_MSGSIZE_MAX_DEFAULT: usize = 8192;

/// `HARD_MSGMAX` / `HARD_MSGSIZEMAX` (`include/linux/ipc_namespace.h`): the
/// ceiling `/proc/sys/fs/mqueue` may never exceed and the limit
/// `CAP_SYS_RESOURCE` tasks are held to.
pub const MQ_MSG_MAX_HARD: u64 = 65536;
pub const MQ_MSGSIZE_MAX_HARD: u64 = 16 * 1024 * 1024;

/// Why `mqueue_get_inode()` refused a create request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MqAttributeError {
    /// `mq_maxmsg <= 0`, `mq_msgsize <= 0`, or either value above the
    /// applicable limit: `EINVAL`.
    Invalid,
    /// The `RLIMIT_MSGQUEUE` charge computation cannot be represented:
    /// `EOVERFLOW`.
    Overflow,
}

/// Bytes Linux charges against `RLIMIT_MSGQUEUE` for one queue.
///
/// `mqueue_get_inode()` computes
/// `mq_maxmsg * mq_msgsize + mq_maxmsg * sizeof(struct msg_msg) +
/// min(mq_maxmsg, MQ_PRIO_MAX) * sizeof(struct posix_msg_tree_node)` in
/// unsigned long arithmetic and refuses the queue with `EOVERFLOW` when the
/// sums wrap. `None` models that wrap, so the caller can keep Linux's errno
/// precedence instead of saturating.
pub const fn mqueue_charge_bytes(max_messages: u64, message_size: u64) -> Option<u64> {
    let nodes = if max_messages < MQ_PRIO_MAX as u64 {
        max_messages
    } else {
        MQ_PRIO_MAX as u64
    };
    let tree_size = max_messages
        .wrapping_mul(MQ_MSG_OVERHEAD)
        .wrapping_add(nodes.wrapping_mul(MQ_TREE_NODE_OVERHEAD));
    let payload = max_messages.wrapping_mul(message_size);
    let total = payload.wrapping_add(tree_size);
    if total < payload { None } else { Some(total) }
}

/// Validates one create-time `mq_attr` and returns its `RLIMIT_MSGQUEUE`
/// charge.
///
/// Order and errno follow `mqueue_get_inode()` exactly: a non-positive count or
/// size is `EINVAL` before the limit check, the limit check is `EINVAL` before
/// the overflow checks, and only then is the charge computed. A
/// `CAP_SYS_RESOURCE` caller is measured against `HARD_MSGMAX` /
/// `HARD_MSGSIZEMAX`; everyone else is measured against the namespace values
/// behind `/proc/sys/fs/mqueue`.
pub const fn validate_mq_attributes(
    max_messages: i64,
    message_size: i64,
    limits: MqLimits,
    cap_sys_resource: bool,
) -> Result<u64, MqAttributeError> {
    if max_messages <= 0 || message_size <= 0 {
        return Err(MqAttributeError::Invalid);
    }
    let (max_count, max_size) = if cap_sys_resource {
        (MQ_MSG_MAX_HARD, MQ_MSGSIZE_MAX_HARD)
    } else {
        (limits.max_messages as u64, limits.max_message_size as u64)
    };
    let max_messages = max_messages as u64;
    let message_size = message_size as u64;
    if max_messages > max_count || message_size > max_size {
        return Err(MqAttributeError::Invalid);
    }
    if message_size > u64::MAX / max_messages {
        return Err(MqAttributeError::Overflow);
    }
    match mqueue_charge_bytes(max_messages, message_size) {
        Some(bytes) => Ok(bytes),
        None => Err(MqAttributeError::Overflow),
    }
}

/// `NAME_MAX` (`include/uapi/linux/limits.h`). `simple_lookup()`
/// (`fs/libfs.c`) refuses a longer dentry name with `ENAMETOOLONG`, and the
/// mqueuefs root uses it:
///
/// ```c
/// struct dentry *simple_lookup(struct inode *dir, struct dentry *dentry, unsigned int flags)
/// {
///     if (dentry->d_name.len > NAME_MAX)
///         return ERR_PTR(-ENAMETOOLONG);
/// ```
pub const MQ_NAME_MAX: usize = 255;

/// Why Linux refused an `mq_open()` / `mq_unlink()` queue name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MqNameError {
    /// The empty name. `do_getname()` (`fs/namei.c`) runs with clear
    /// `LOOKUP_EMPTY`, so `getname_flags()` turns the empty path into
    /// `-ENOENT` before any mqueuefs lookup:
    ///
    /// ```c
    ///     len = strncpy_from_user(kname, filename, EMBEDDED_NAME_MAX);
    ///     /*
    ///      * Handle both empty path and copy failure in one go.
    ///      */
    ///     if (unlikely(len <= 0)) {
    ///         /* The empty path is special. */
    ///         if (!len && !(flags & LOOKUP_EMPTY))
    ///             len = -ENOENT;
    ///     }
    /// ```
    Empty,
    /// `"."`, `".."`, or any name holding a `'/'`. `lookup_noperm_common()`
    /// (`fs/namei.c`), which `start_creating_noperm()` and
    /// `start_removing_noperm()` call for the mqueuefs root, returns
    /// `-EACCES` for all of them:
    ///
    /// ```c
    ///     if (name_is_dot_dotdot(name, len))
    ///         return -EACCES;
    ///
    ///     while (len--) {
    ///         unsigned int c = *(const unsigned char *)name++;
    ///         if (c == '/' || c == '\0')
    ///             return -EACCES;
    ///     }
    /// ```
    ///
    /// This is where a leading slash dies: `SYSCALL_DEFINE4(mq_open)` passes
    /// the raw user name to `prepare_open()` through
    /// `start_creating_noperm(mnt->mnt_root, &QSTR(name->name))`, so the
    /// kernel's queue names never carry a slash. POSIX's leading slash belongs
    /// to libc, which passes `name + 1` to the syscall; `/dev/mqueue` then
    /// lists the slash-free dentry.
    Invalid,
    /// More than [`MQ_NAME_MAX`] bytes.
    TooLong,
}

/// Validates one raw queue name exactly as Linux 7.2.3 does for `mq_open()`
/// and `mq_unlink()`.
///
/// Order and errno follow the syscall path: `do_getname()` first (empty name
/// is `ENOENT`), then `lookup_noperm_common()` (`EACCES` for `"."`, `".."` and
/// every `'/'`), and only then `simple_lookup()`'s `NAME_MAX` check
/// (`ENAMETOOLONG`).
pub const fn validate_mq_name(raw: &[u8]) -> Result<(), MqNameError> {
    if raw.is_empty() {
        return Err(MqNameError::Empty);
    }
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'/' {
            return Err(MqNameError::Invalid);
        }
        index += 1;
    }
    if raw[0] == b'.' && (raw.len() == 1 || (raw.len() == 2 && raw[1] == b'.')) {
        return Err(MqNameError::Invalid);
    }
    if raw.len() > MQ_NAME_MAX {
        return Err(MqNameError::TooLong);
    }
    Ok(())
}

/// The `(priority, sequence)` identity of one queued message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqKey {
    pub priority: u32,
    pub sequence: u64,
}

/// `msg_insert()` places a message before every stored message that must be
/// dequeued first; `msg_get()` always removes the front of that order.
///
/// The order is descending `mq_prio` (`msg_tree` keeps low priorities left and
/// high priorities right) with FIFO inside one priority, because `msg_insert()`
/// appends with `list_add_tail()` to the leaf's `msg_list` and `msg_get()`
/// takes `list_first_entry()`.
pub const fn mqueue_dequeue_precedes(a: MqKey, b: MqKey) -> bool {
    a.priority > b.priority || (a.priority == b.priority && a.sequence < b.sequence)
}

/// Prefix form of [`mqueue_dequeue_precedes`] for a binary-search insertion
/// point: true while `stored` must stay ahead of `candidate`.
pub const fn mqueue_insert_prefix(stored: MqKey, candidate: MqKey) -> bool {
    !mqueue_dequeue_precedes(candidate, stored)
}

/// One-shot `mq_notify` publication rule.
///
/// `__do_notify()` (`ipc/mqueue.c`) consumes the registration only for the
/// message `msg_insert()` added to an *empty* `msg_tree`, and only when no
/// receiver was served synchronously: `info->notify_owner != NULL &&
/// info->attr.mq_curmsgs == 1`. A message handed straight to a blocked receiver
/// never touches the tree, so it leaves the registration armed.
pub const fn mq_notify_fires(
    registration: bool,
    pipelined_receiver: bool,
    messages_after_send: usize,
) -> bool {
    registration && !pipelined_receiver && messages_after_send == 1
}

/// Why Linux `may_delete_dentry()` would refuse an `mq_unlink`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MqUnlinkError {
    /// `inode_permission(dir, MAY_WRITE | MAY_EXEC)` failed: `EACCES`.
    DirectoryInaccessible,
    /// `check_sticky()` failed on a sticky directory: `EPERM`.
    StickyDirectory,
}

/// Everything `may_delete_dentry()` inspects for an mqueuefs unlink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqUnlinkRequest {
    /// Owner of the queue inode (`inode->i_uid` as `current_fsuid()` saw it
    /// when the queue was created).
    pub queue_uid: u32,
    /// Owner of the mqueuefs root, which `mqueue_fill_super()` always creates
    /// as `S_IFDIR | S_ISVTX | S_IRWXUGO`.
    pub directory_uid: u32,
    /// `current_fsuid()`.
    pub fsuid: u32,
    /// `dir->i_mode & S_ISVTX`.
    pub directory_sticky: bool,
    /// `inode_permission(dir, MAY_WRITE | MAY_EXEC)`.
    pub directory_write_exec: bool,
    /// `capable_wrt_inode_uidgid(inode, CAP_FOWNER)`.
    pub cap_fowner: bool,
}

/// Linux unlink authority for a queue name.
///
/// `may_delete_dentry()` checks the directory permission first and only then
/// `check_sticky()`, which grants a sticky directory's owner (and the victim's
/// owner, and `CAP_FOWNER`) the right to unlink inside it. The mqueuefs root is
/// sticky and world-writable, so the directory permission never fails in
/// practice and a non-owner is rejected with `EPERM`, not `EACCES`.
pub const fn authorize_mq_unlink(request: MqUnlinkRequest) -> Result<(), MqUnlinkError> {
    if !request.directory_write_exec {
        return Err(MqUnlinkError::DirectoryInaccessible);
    }
    if !request.directory_sticky {
        return Ok(());
    }
    if request.queue_uid == request.fsuid
        || request.directory_uid == request.fsuid
        || request.cap_fowner
    {
        return Ok(());
    }
    Err(MqUnlinkError::StickyDirectory)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcError {
    PermissionDenied,
    InvalidMode,
    InvalidSelection,
    InvalidOperation,
    WouldBlock,
    InvalidAddress,
    InvalidQueueName,
    InvalidAttributes,
    InvalidPriority,
    /// Linux `-ENOSPC` from `ipc_addid()`/`idr_alloc()`.
    NoSpace,
}

/// Linux `ipc/util.h`: a published SysV identifier packs an index into the low
/// `IPCMNI_SHIFT` bits and a per-table sequence number into the bits above it:
/// bits 0-14 are the index (32k, 15 bits) and bits 15-30 the sequence number
/// (64k, 16 bits).
///
/// `IPCMNI_EXTEND_SHIFT` is Linux's `ipcmni_extend` boot-parameter layout
/// (24/7 bits).  This kernel does not implement the extension, so the default
/// split is the only composition it publishes or accepts.
pub const IPCMNI_SHIFT: u32 = 15;
pub const IPCMNI_EXTEND_SHIFT: u32 = 24;
pub const IPCMNI: i32 = 1 << IPCMNI_SHIFT;
pub const IPCMNI_IDX_MASK: i32 = IPCMNI - 1;
/// Linux `ipc/ipc_sysctl.c`: `ipc_min_cycle = RADIX_TREE_MAP_SIZE`, the floor
/// of the cyclic window `idr_alloc_cyclic()` searches.
pub const IPC_MIN_CYCLE: i32 = 64;
/// Linux `ipc/util.h:ipcid_seq_max()`, the first sequence value that wraps.
pub const IPCID_SEQ_MAX: i32 = i32::MAX >> IPCMNI_SHIFT;

/// Linux `ipc/util.h:ipcid_to_idx()`.
pub const fn ipcid_to_idx(id: i32) -> i32 {
    id & IPCMNI_IDX_MASK
}

/// Linux `ipc/util.h:ipcid_to_seqx()`.
pub const fn ipcid_to_seqx(id: i32) -> i32 {
    id >> IPCMNI_SHIFT
}

/// Linux `ipc/util.c:ipc_idr_alloc()`: `new->id = (new->seq <<
/// ipcmni_seq_shift()) + idx`.
pub const fn ipcid_compose(index: i32, sequence: i32) -> i32 {
    (sequence << IPCMNI_SHIFT) + index
}

/// Linux `ipc/util.h:ipc_checkid()`.
pub const fn ipcid_is_stale(id: i32, sequence: i32) -> bool {
    ipcid_to_seqx(id) != sequence
}

/// The flags Linux `ipc/shm.c:newseg()` derives from a `shmget()` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShmCreationPlan {
    /// `shmflg & SHM_HUGETLB`.
    pub hugetlb: bool,
    /// The `SHM_HUGE_*` size hint, which Linux only consults for a huge-page
    /// segment.
    pub huge_hint: u32,
    /// `shmflg & SHM_NORESERVE`, which Linux drops under `OVERCOMMIT_NEVER`:
    /// "Do not allow no accounting for OVERCOMMIT_NEVER, even if it's asked
    /// for."
    pub no_reserve: bool,
}

/// Decodes the creation flags of one `shmget()` call.
pub const fn shm_creation_plan(flags: u32, overcommit_never: bool) -> ShmCreationPlan {
    let hugetlb = flags & SHM_HUGETLB != 0;
    ShmCreationPlan {
        hugetlb,
        huge_hint: if hugetlb {
            (flags >> SHM_HUGE_SHIFT) & SHM_HUGE_MASK
        } else {
            0
        },
        no_reserve: flags & SHM_NORESERVE != 0 && !overcommit_never,
    }
}

/// Whether a `SHM_HUGETLB` segment of the requested size class can be backed.
///
/// Linux resolves the hint with `hstate_sizelog()` and rejects a segment whose
/// hint names no configured huge-page size with EINVAL.  TheKernel has no
/// huge-page pool, so no hint resolves and every huge-page creation is
/// rejected - the answer Linux gives for an unconfigured size class.
pub const fn shm_supports_huge_page_hint(_hint: u32) -> bool {
    false
}

/// One allocated SysV identifier, before it is published to userspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcId {
    index: i32,
    sequence: i32,
}

impl IpcId {
    pub const fn from_parts(index: i32, sequence: i32) -> Self {
        Self { index, sequence }
    }

    /// The IDR slot this identifier resolves through.
    pub const fn index(self) -> i32 {
        self.index
    }

    /// The value the object's `ipc_perm.seq` field must hold.
    pub const fn sequence(self) -> i32 {
        self.sequence
    }

    /// The value userspace receives.
    pub const fn raw(self) -> i32 {
        ipcid_compose(self.index, self.sequence)
    }
}

/// The identifier table for one SysV object type in one IPC namespace: Linux's
/// `struct ipc_ids`.
///
/// It reproduces the observable part of `ipc/util.c:ipc_idr_alloc()` and
/// `ipc/util.c:ipc_rmid()`.  Indexes are handed out cyclically inside
/// `[0, max(in_use * 3 / 2, ipc_min_cycle))` capped at `ipc_mni`, and the
/// sequence number advances only when the new index does not move past the
/// previous one.  That rule is what makes a retired identifier keep failing
/// `ipc_checkid()` until the 16-bit sequence finally wraps, so a stale handle
/// cannot resolve to a successor object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcIdTable {
    in_use: i32,
    sequence: i32,
    max_index: i32,
    last_index: i32,
    next: i32,
}

impl Default for IpcIdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl IpcIdTable {
    pub const fn new() -> Self {
        Self {
            in_use: 0,
            sequence: 0,
            max_index: -1,
            last_index: -1,
            next: 0,
        }
    }

    pub const fn in_use(self) -> i32 {
        self.in_use
    }

    /// Linux `ipc/util.h:ipc_get_maxidx()`.
    pub const fn max_index(self) -> i32 {
        if self.in_use == 0 {
            -1
        } else if self.in_use >= IPCMNI {
            IPCMNI - 1
        } else {
            self.max_index
        }
    }

    /// Allocates one identifier.  `requested` is the raw `*_next_id` sysctl
    /// value, if a writer armed one; `is_used` reports whether an index is
    /// still occupied.
    ///
    /// A requested identifier keeps its sequence number and is placed at the
    /// first free index at or after `ipcid_to_idx(requested)`, exactly as
    /// Linux' `idr_alloc(..., ipcid_to_idx(next_id), ipc_mni, ...)` does.  It
    /// does not disturb the cyclic cursor, so the two paths compose the way
    /// Linux' two branches do.
    pub fn allocate(
        &mut self,
        requested: Option<i32>,
        is_used: impl Fn(i32) -> bool,
    ) -> Result<IpcId, IpcError> {
        if let Some(requested) = requested {
            let index = first_free(ipcid_to_idx(requested), IPCMNI, &is_used)
                .ok_or(IpcError::NoSpace)?;
            self.register(index);
            return Ok(IpcId {
                index,
                sequence: ipcid_to_seqx(requested),
            });
        }

        // `max(ids->in_use * 3 / 2, ipc_min_cycle)`, then `min(.., ipc_mni)`.
        let window = (self.in_use * 3 / 2).clamp(IPC_MIN_CYCLE, IPCMNI);
        let index = first_free(self.next, window, &is_used)
            .or_else(|| first_free(0, window, &is_used))
            .ok_or(IpcError::NoSpace)?;
        if index <= self.last_index {
            self.sequence += 1;
            if self.sequence >= IPCID_SEQ_MAX {
                self.sequence = 0;
            }
        }
        self.last_index = index;
        self.next = index + 1;
        self.register(index);
        Ok(IpcId {
            index,
            sequence: self.sequence,
        })
    }

    /// Linux `ipc/util.c:ipc_rmid()`: drop `index` and, when it was the
    /// highest one, cache the highest remaining index instead.  `is_used` is
    /// consulted only for that recomputation and must reflect the table after
    /// the caller removed the object.
    pub fn release(&mut self, index: i32, is_used: impl Fn(i32) -> bool) {
        self.in_use = self.in_use.saturating_sub(1);
        if index == self.max_index {
            let mut candidate = index - 1;
            while candidate >= 0 && !is_used(candidate) {
                candidate -= 1;
            }
            self.max_index = candidate;
        }
    }

    fn register(&mut self, index: i32) {
        self.in_use += 1;
        if index > self.max_index {
            self.max_index = index;
        }
    }
}

/// The first free index in `from..end`, mirroring `idr_get_free()`'s forward
/// scan from the start of the range.
fn first_free(from: i32, end: i32, is_used: &impl Fn(i32) -> bool) -> Option<i32> {
    let mut index = from.max(0);
    while index < end {
        if !is_used(index) {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Linux `ipc/sem.c:perform_atomic_semop()`: the deferred adjustment
/// `semadj - sem_op` must stay inside `[-SEMAEM - 1, SEMAEM]`.  Exceeding the
/// range fails the whole operation with `-ERANGE` before any semaphore value
/// changes; the current code clamped instead.
pub const fn sem_undo_delta_in_range(prior: i32, sem_op: i16) -> bool {
    let undo = prior - sem_op as i32;
    undo >= -SEMAEM - 1 && undo <= SEMAEM
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Credentials {
    pub euid: u32,
    pub egid: u32,
    pub privileged: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcPermission {
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Read,
    Write,
    Alter,
}
pub const fn authorize(c: Credentials, p: IpcPermission, a: Access) -> Result<(), IpcError> {
    if c.privileged {
        return Ok(());
    }
    let shift = if c.euid == p.uid {
        6
    } else if c.egid == p.gid {
        3
    } else {
        0
    };
    let need = match a {
        Access::Read => 4,
        Access::Write => 2,
        Access::Alter => 2,
    };
    if ((p.mode >> shift) & need) != 0 {
        Ok(())
    } else {
        Err(IpcError::PermissionDenied)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageSelection {
    First,
    Exact(i64),
    LowestAtLeast(i64),
    LowestType,
}
pub const fn select_message(request: i64) -> Result<MessageSelection, IpcError> {
    if request == 0 {
        Ok(MessageSelection::First)
    } else if request > 0 {
        Ok(MessageSelection::Exact(request))
    } else if request == i64::MIN {
        // Linux treats the unrepresentable absolute value as the largest
        // admissible type bound, so this still selects the lowest message
        // type rather than rejecting the receive request.
        Ok(MessageSelection::LowestType)
    } else if request == -0x7fff_ffff_ffff_ffff {
        Ok(MessageSelection::LowestType)
    } else {
        Ok(MessageSelection::LowestAtLeast(-request))
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemBuf {
    pub num: u16,
    pub op: i16,
    pub flags: i16,
}
const _: () = {
    assert!(core::mem::size_of::<SemBuf>() == 6);
    assert!(core::mem::align_of::<SemBuf>() == 2);
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemPlan {
    Adjust {
        index: u16,
        delta: i16,
        undo: bool,
    },
    WaitZero {
        index: u16,
        nowait: bool,
        undo: bool,
    },
    WaitDecrease {
        index: u16,
        amount: u16,
        nowait: bool,
        undo: bool,
    },
}

impl SemPlan {
    /// The raw `SEM_UNDO` bit.  Linux validates the deferred-adjustment range
    /// for every `SEM_UNDO` operation, including a wait-for-zero.
    pub const fn undo(self) -> bool {
        match self {
            Self::Adjust { undo, .. }
            | Self::WaitZero { undo, .. }
            | Self::WaitDecrease { undo, .. } => undo,
        }
    }

    /// Whether the operation registers a deferred adjustment.
    ///
    /// Linux `ipc/sem.c:perform_atomic_semop()` stores `semadj - sem_op` for
    /// every `SEM_UNDO` operation, which is the identity for a wait-for-zero,
    /// so only a non-zero `sem_op` can change a process's undo list.
    pub const fn records_undo(self) -> bool {
        match self {
            Self::Adjust { undo, .. } | Self::WaitDecrease { undo, .. } => undo,
            Self::WaitZero { .. } => false,
        }
    }

    pub const fn index(self) -> u16 {
        match self {
            Self::Adjust { index, .. }
            | Self::WaitZero { index, .. }
            | Self::WaitDecrease { index, .. } => index,
        }
    }
}

/// Classifies one `sembuf`.
///
/// Linux never validates `sem_flg`: `ipc/sem.c` only ever tests the
/// `SEM_UNDO` and `IPC_NOWAIT` bits, so every other bit is accepted and
/// ignored.  A wait-for-zero combined with `SEM_UNDO` is accepted too; its
/// deferred adjustment is the identity, so it records nothing.
pub const fn plan_sem_op(op: SemBuf) -> SemPlan {
    let nowait = op.flags & IPC_NOWAIT as i16 != 0;
    let undo = op.flags & SEM_UNDO as i16 != 0;
    if op.op > 0 {
        SemPlan::Adjust {
            index: op.num,
            delta: op.op,
            undo,
        }
    } else if op.op == 0 {
        SemPlan::WaitZero {
            index: op.num,
            nowait,
            undo,
        }
    } else {
        SemPlan::WaitDecrease {
            index: op.num,
            amount: op.op.unsigned_abs(),
            nowait,
            undo,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShmSnapshot {
    pub length: usize,
    pub min_address: usize,
    pub max_address: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShmAttachPlan {
    pub address: usize,
    pub length: usize,
    pub readonly: bool,
}
pub const fn plan_shmat(
    snapshot: ShmSnapshot,
    requested: usize,
    flags: u32,
) -> Result<ShmAttachPlan, IpcError> {
    if flags & !(SHM_RDONLY | SHM_RND) != 0 {
        return Err(IpcError::InvalidOperation);
    }
    let address = if requested == 0 {
        snapshot.min_address
    } else if flags & SHM_RND != 0 {
        requested & !(SHMLBA - 1)
    } else if requested & (SHMLBA - 1) == 0 {
        requested
    } else {
        return Err(IpcError::InvalidAddress);
    };
    let end = match address.checked_add(snapshot.length) {
        Some(v) => v,
        None => return Err(IpcError::InvalidAddress),
    };
    if address < snapshot.min_address || end > snapshot.max_address {
        return Err(IpcError::InvalidAddress);
    }
    Ok(ShmAttachPlan {
        address,
        length: snapshot.length,
        readonly: flags & SHM_RDONLY != 0,
    })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqAttributes {
    pub max_messages: usize,
    pub message_size: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqLimits {
    pub queues: usize,
    pub max_messages: usize,
    pub max_message_size: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MqPlan {
    Create { attributes: MqAttributes },
    Open,
}
pub fn plan_mq_open(
    name: &[u8],
    create: bool,
    attr: Option<MqAttributes>,
    limits: MqLimits,
) -> Result<MqPlan, IpcError> {
    if name.len() < 2 || name[0] != b'/' || name[1..].contains(&0) || name[1..].contains(&b'/') {
        return Err(IpcError::InvalidQueueName);
    }
    if !create {
        return Ok(MqPlan::Open);
    }
    let a = attr.ok_or(IpcError::InvalidAttributes)?;
    if a.max_messages == 0
        || a.max_messages > limits.max_messages
        || a.message_size == 0
        || a.message_size > limits.max_message_size
    {
        return Err(IpcError::InvalidAttributes);
    }
    Ok(MqPlan::Create { attributes: a })
}
pub const fn validate_priority(priority: u32) -> Result<(), IpcError> {
    if priority < MQ_PRIO_MAX {
        Ok(())
    } else {
        Err(IpcError::InvalidPriority)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Index occupancy probe with a bounded table for tests.
    fn used(live: &[bool], index: i32) -> bool {
        live.get(index as usize).copied().unwrap_or(false)
    }

    #[test]
    fn permissions_and_selection() {
        assert_eq!(
            authorize(
                Credentials {
                    euid: 2,
                    egid: 2,
                    privileged: false
                },
                IpcPermission {
                    uid: 1,
                    gid: 2,
                    mode: 0o060
                },
                Access::Write
            ),
            Ok(())
        );
        assert_eq!(select_message(i64::MIN), Ok(MessageSelection::LowestType));
    }

    #[test]
    fn identifier_layout_round_trips() {
        assert_eq!(IPCMNI, 32768);
        assert_eq!(IPCMNI_IDX_MASK, 0x7fff);
        assert_eq!(IPC_MIN_CYCLE, 64);
        assert_eq!(IPCID_SEQ_MAX, 65535);
        // Linux `ipc/util.h`: index in the low bits, sequence above it.
        assert_eq!(ipcid_to_idx(0), 0);
        assert_eq!(ipcid_to_seqx(0), 0);
        assert_eq!(ipcid_compose(0, 1), 32768);
        assert_eq!(ipcid_to_idx(32768), 0);
        assert_eq!(ipcid_to_seqx(32768), 1);
        assert_eq!(ipcid_compose(7, 1), 32775);
        assert_eq!(ipcid_to_idx(32775), 7);
        assert_eq!(ipcid_to_seqx(32775), 1);
        for index in [0, 1, 0x7fff] {
            for sequence in [0, 1, 2, 65534] {
                let id = ipcid_compose(index, sequence);
                assert!(id >= 0, "{id} must not go negative");
                assert_eq!(ipcid_to_idx(id), index);
                assert_eq!(ipcid_to_seqx(id), sequence);
                assert!(!ipcid_is_stale(id, sequence));
                assert!(ipcid_is_stale(id, sequence.wrapping_add(1)));
            }
        }
        // Linux never publishes a negative identifier.
        assert!(ipcid_compose(IPCMNI - 1, IPCID_SEQ_MAX - 1) > 0);
    }

    #[test]
    fn first_identifiers_are_sequential_and_never_reuse_a_live_index() {
        let mut table = IpcIdTable::new();
        let mut live = [false; 8];
        let mut indexes = [0i32; 8];
        let mut sequences = [0i32; 8];
        for slot in 0..8 {
            let id = table
                .allocate(None, |index| live[index as usize])
                .expect("table has room");
            assert!(!live[id.index() as usize]);
            live[id.index() as usize] = true;
            indexes[slot] = id.index();
            sequences[slot] = id.sequence();
            assert_eq!(id.raw(), id.index());
        }
        // Linux `ipc_idr_alloc()`: the sequence only moves when the cyclic
        // allocation wraps, so a fresh table publishes 0..7 with sequence 0.
        assert_eq!(indexes, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(sequences, [0; 8]);
        assert_eq!(table.in_use(), 8);
        assert_eq!(table.max_index(), 7);
    }

    #[test]
    fn releasing_the_lowest_index_does_not_reissue_it() {
        let mut table = IpcIdTable::new();
        let live = [false; 8];
        let first = table
            .allocate(None, |index| used(&live, index))
            .unwrap();
        let second = table
            .allocate(None, |index| used(&live, index))
            .unwrap();
        assert_eq!((first.index(), second.index()), (0, 1));
        // `ipc_rmid()` the lowest live index; `idr_alloc_cyclic()` continues
        // from its cursor, so 0 must stay retired instead of being handed to
        // the next caller.  The retired identifier names index 0, which the
        // successor does not.
        table.release(first.index(), |_| false);
        assert_eq!(table.in_use(), 1);
        assert_eq!(table.max_index(), 1);
        let third = table
            .allocate(None, |index| index == second.index())
            .unwrap();
        assert_eq!(third.index(), 2, "0 must not come back immediately");
        assert_ne!(third.raw(), first.raw());
        assert_eq!(ipcid_to_idx(first.raw()), 0);
    }

    #[test]
    fn sequence_advances_when_the_cyclic_window_wraps() {
        let mut table = IpcIdTable::new();
        // Walk the cursor to the end of the `ipc_min_cycle` window while
        // releasing every index again, so the window never grows and the next
        // allocation has to wrap.  This is the create/remove loop an
        // unprivileged caller can run.
        for index in 0..IPC_MIN_CYCLE {
            let id = table.allocate(None, |_| false).unwrap();
            assert_eq!(id.index(), index);
            table.release(id.index(), |_| false);
        }
        let wrapped = table.allocate(None, |_| false).unwrap();
        assert_eq!(wrapped.index(), 0);
        assert_eq!(
            wrapped.sequence(),
            1,
            "`idx <= ids->last_idx` must bump the sequence"
        );
        assert_eq!(wrapped.raw(), IPCMNI);
    }

    #[test]
    fn a_stale_identifier_stays_stale_across_a_successor_lifetime() {
        let mut table = IpcIdTable::new();
        let first = table.allocate(None, |_| false).unwrap();
        assert_eq!(first.raw(), 0);
        table.release(first.index(), |_| false);
        // The successor that eventually receives index 0 keeps the retired
        // numeric identifier invalid because its sequence differs.
        for _ in 0..(IPC_MIN_CYCLE - 1) {
            let id = table.allocate(None, |_| false).unwrap();
            table.release(id.index(), |_| false);
        }
        let successor = table.allocate(None, |_| false).unwrap();
        assert_eq!(successor.index(), 0);
        assert!(ipcid_is_stale(first.raw(), successor.sequence()));
        assert_ne!(first.raw(), successor.raw());
        assert_eq!(successor.raw(), IPCMNI);
    }

    #[test]
    fn requested_identifier_keeps_its_sequence() {
        let mut table = IpcIdTable::new();
        // `echo $((5 | (2 << 15))) > /proc/sys/kernel/msg_next_id` publishes
        // index 5 with sequence 2 on the next create.
        let requested = ipcid_compose(5, 2);
        let id = table.allocate(Some(requested), |_| false).unwrap();
        assert_eq!(id.index(), 5);
        assert_eq!(id.sequence(), 2);
        assert_eq!(id.raw(), requested);
        // The requested path must not disturb the cyclic cursor: the next
        // ordinary allocation continues from index 0.
        let next = table.allocate(None, |_| false).unwrap();
        assert_eq!(next.index(), 0);
        assert_eq!(next.sequence(), 0);
    }

    #[test]
    fn requested_identifier_skips_occupied_indexes_and_reports_exhaustion() {
        let live = [true, true, false, false];
        let mut table = IpcIdTable::new();
        // Linux `idr_alloc(start = ipcid_to_idx(next_id), end = ipc_mni)`
        // takes the first free index at or after the request.
        let id = table
            .allocate(Some(ipcid_compose(0, 1)), |index| used(&live, index))
            .unwrap();
        assert_eq!(id.index(), 2);
        assert_eq!(id.sequence(), 1);

        // The requested path reports exhaustion of the whole index space
        // instead of silently publishing an unrelated identifier.
        let mut full = IpcIdTable::new();
        assert_eq!(
            full.allocate(Some(ipcid_compose(0, 3)), |_| true),
            Err(IpcError::NoSpace)
        );
        assert_eq!(full.in_use(), 0);
    }

    #[test]
    fn allocation_reports_exhaustion_instead_of_reusing_live_indexes() {
        let mut table = IpcIdTable::new();
        let live = [true; IPC_MIN_CYCLE as usize];
        assert_eq!(
            table.allocate(None, |index| live[index as usize]),
            Err(IpcError::NoSpace)
        );
        // A failed allocation must not consume the table's bookkeeping.
        assert_eq!(table.in_use(), 0);
        assert_eq!(table.max_index(), -1);
    }

    #[test]
    fn max_index_tracks_the_highest_live_index() {
        let mut table = IpcIdTable::new();
        assert_eq!(table.max_index(), -1);
        let mut live = [false; 8];
        let mut indexes = [0i32; 4];
        for slot in 0..4 {
            let id = table.allocate(None, |index| live[index as usize]).unwrap();
            live[id.index() as usize] = true;
            indexes[slot] = id.index();
        }
        assert_eq!(table.max_index(), 3);
        // Removing a lower index leaves the cached maximum alone.
        live[0] = false;
        table.release(0, |index| live[index as usize]);
        assert_eq!(table.max_index(), 3);
        // Removing the maximum recomputes it from the surviving indexes.
        live[3] = false;
        table.release(3, |index| live[index as usize]);
        assert_eq!(table.max_index(), 2);
        for index in [indexes[1], indexes[2]] {
            live[index as usize] = false;
            table.release(index, |live_index| live[live_index as usize]);
        }
        assert_eq!(table.max_index(), -1);
    }

    #[test]
    fn shm_creation_flags_split_hugetlb_hint_and_no_reserve() {
        // Plain segment: neither huge pages nor a no-reserve request.
        assert_eq!(
            shm_creation_plan(0o600, false),
            ShmCreationPlan {
                hugetlb: false,
                huge_hint: 0,
                no_reserve: false
            }
        );
        // `SHM_HUGE_*` occupies bits 26-31 and is only consulted together with
        // `SHM_HUGETLB`; `SHM_NORESERVE` shares SHM_RDONLY's value but belongs
        // to `shmget`.
        assert_eq!(
            shm_creation_plan(SHM_HUGETLB | 0o600, false),
            ShmCreationPlan {
                hugetlb: true,
                huge_hint: 0,
                no_reserve: false
            }
        );
        assert_eq!(
            shm_creation_plan(SHM_HUGETLB | (21 << SHM_HUGE_SHIFT) | 0o600, false),
            ShmCreationPlan {
                hugetlb: true,
                huge_hint: 21,
                no_reserve: false
            }
        );
        // The hint is ignored without SHM_HUGETLB.
        assert_eq!(
            shm_creation_plan((21 << SHM_HUGE_SHIFT) | 0o600, false).huge_hint,
            0
        );
        assert_eq!(
            shm_creation_plan((21 << SHM_HUGE_SHIFT) | 0o600, false).hugetlb,
            false
        );
        assert_eq!(
            shm_creation_plan(SHM_NORESERVE | 0o600, false),
            ShmCreationPlan {
                hugetlb: false,
                huge_hint: 0,
                no_reserve: true
            }
        );
        // "Do not allow no accounting for OVERCOMMIT_NEVER, even if it's asked
        // for."
        assert!(!shm_creation_plan(SHM_NORESERVE | 0o600, true).no_reserve);
        // The hint value is masked to six bits.
        assert_eq!(
            shm_creation_plan(SHM_HUGETLB | (0x7f << SHM_HUGE_SHIFT), false).huge_hint,
            0x3f
        );
        // No huge-page size class is backed, so every hint is unsupported.
        assert!(!shm_supports_huge_page_hint(0));
        assert!(!shm_supports_huge_page_hint(21));
    }

    #[test]
    fn sem_flags_outside_nowait_and_undo_are_ignored() {
        // Linux never validates `sem_flg`; unknown bits are ignored.
        assert_eq!(
            plan_sem_op(SemBuf {
                num: 3,
                op: 5,
                flags: 0x2000,
            }),
            SemPlan::Adjust {
                index: 3,
                delta: 5,
                undo: false
            }
        );
        assert_eq!(
            plan_sem_op(SemBuf {
                num: 0,
                op: -2,
                flags: (SEM_UNDO as i16) | 0x4000,
            }),
            SemPlan::WaitDecrease {
                index: 0,
                amount: 2,
                nowait: false,
                undo: true
            }
        );
        // A wait-for-zero with SEM_UNDO is accepted and records nothing.
        let wait = plan_sem_op(SemBuf {
            num: 1,
            op: 0,
            flags: (SEM_UNDO as i16) | (IPC_NOWAIT as i16),
        });
        assert_eq!(
            wait,
            SemPlan::WaitZero {
                index: 1,
                nowait: true,
                undo: true
            }
        );
        assert!(!wait.records_undo());
        assert!(!plan_sem_op(SemBuf {
            num: 0,
            op: 4,
            flags: 0
        })
        .records_undo());
        assert!(plan_sem_op(SemBuf {
            num: 0,
            op: 4,
            flags: SEM_UNDO as i16
        })
        .records_undo());
        assert_eq!(wait.index(), 1);
    }

    #[test]
    fn sem_undo_range_is_inclusive_at_the_linux_bounds() {
        // Linux `perform_atomic_semop()`: `undo = semadj - sem_op` must satisfy
        // `-SEMAEM - 1 <= undo <= SEMAEM`.
        assert!(sem_undo_delta_in_range(0, -SEMAEM as i16));
        assert!(sem_undo_delta_in_range(0, SEMAEM as i16));
        assert!(sem_undo_delta_in_range(-SEMAEM - 1, 0));
        assert!(!sem_undo_delta_in_range(SEMAEM, -1));
        assert!(!sem_undo_delta_in_range(-SEMAEM - 1, 1));
        assert!(sem_undo_delta_in_range(SEMAEM - 1, -1));
        assert!(sem_undo_delta_in_range(-SEMAEM, 1));
    }

    fn namespace_limits() -> MqLimits {
        MqLimits {
            queues: MQ_QUEUES_MAX_DEFAULT,
            max_messages: MQ_MSG_MAX_DEFAULT,
            max_message_size: MQ_MSGSIZE_MAX_DEFAULT,
        }
    }

    #[test]
    fn mqueue_charge_matches_linux_accounting() {
        // `mq_maxmsg * mq_msgsize` plus one `struct msg_msg` and one
        // `struct posix_msg_tree_node` per slot, as measured against a Linux
        // 7.2 guest: 10 * 8192 + 10 * 48 + 10 * 48 == 82880.
        assert_eq!(mqueue_charge_bytes(10, 8192), Some(82_880));
        assert_eq!(mqueue_charge_bytes(1, 1), Some(97));
        // The tree charge stops at MQ_PRIO_MAX priorities.
        assert_eq!(
            mqueue_charge_bytes(65536, 1),
            Some(65536 + 65536 * 48 + 32768 * 48)
        );
        // Linux performs the addition in unsigned long and refuses the wrap
        // with EOVERFLOW; it never saturates.
        assert_eq!(mqueue_charge_bytes(u64::MAX, 1), None);
        assert_eq!(mqueue_charge_bytes(1, u64::MAX), None);
    }

    #[test]
    fn mq_attribute_admission_order_and_capability() {
        assert_eq!(
            validate_mq_attributes(0, 64, namespace_limits(), true),
            Err(MqAttributeError::Invalid)
        );
        assert_eq!(
            validate_mq_attributes(10, 0, namespace_limits(), true),
            Err(MqAttributeError::Invalid)
        );
        // A CAP_SYS_RESOURCE caller is bounded by HARD_MSGMAX/HARD_MSGSIZEMAX.
        assert_eq!(
            validate_mq_attributes(100, 64, namespace_limits(), false),
            Err(MqAttributeError::Invalid)
        );
        assert_eq!(validate_mq_attributes(100, 64, namespace_limits(), true), Ok(100 * 64 + 100 * 96));
        assert_eq!(
            validate_mq_attributes(MQ_MSG_MAX_HARD as i64 + 1, 64, namespace_limits(), true),
            Err(MqAttributeError::Invalid)
        );
        assert_eq!(
            validate_mq_attributes(2, MQ_MSGSIZE_MAX_HARD as i64 + 1, namespace_limits(), true),
            Err(MqAttributeError::Invalid)
        );
        // Exactly at the namespace limit is admitted without the capability.
        assert_eq!(
            validate_mq_attributes(10, 8192, namespace_limits(), false),
            Ok(82_880)
        );
    }

    #[test]
    fn queue_names_follow_the_raw_syscall_contract() {
        // Plain names are the kernel's own form; a leading slash is libc's.
        assert_eq!(validate_mq_name(b"tk-mq-1"), Ok(()));
        assert_eq!(validate_mq_name(b"a"), Ok(()));
        assert_eq!(validate_mq_name(b"..."), Ok(()));
        assert_eq!(validate_mq_name(b".a"), Ok(()));
        // libc strips the leading slash, so the kernel rejects it.
        assert_eq!(validate_mq_name(b"/tk-mq-1"), Err(MqNameError::Invalid));
        assert_eq!(validate_mq_name(b"a/b"), Err(MqNameError::Invalid));
        assert_eq!(validate_mq_name(b"/"), Err(MqNameError::Invalid));
        // `name_is_dot_dotdot()` covers exactly "." and "..".
        assert_eq!(validate_mq_name(b"."), Err(MqNameError::Invalid));
        assert_eq!(validate_mq_name(b".."), Err(MqNameError::Invalid));
        // The empty path is the one name that reports ENOENT.
        assert_eq!(validate_mq_name(b""), Err(MqNameError::Empty));
        // NAME_MAX is inclusive; the check runs after the syntax checks.
        let at_limit = [b'x'; MQ_NAME_MAX];
        let over_limit = [b'x'; MQ_NAME_MAX + 1];
        assert_eq!(validate_mq_name(&at_limit), Ok(()));
        assert_eq!(validate_mq_name(&over_limit), Err(MqNameError::TooLong));
        let long_dots = [b'.'; MQ_NAME_MAX + 1];
        assert_eq!(validate_mq_name(&long_dots), Err(MqNameError::TooLong));
        let long_with_slash = [b'/'; MQ_NAME_MAX + 1];
        assert_eq!(validate_mq_name(&long_with_slash), Err(MqNameError::Invalid));
    }

    #[test]
    fn mqueue_dequeue_order_is_priority_then_fifo() {
        let low_first = MqKey {
            priority: 1,
            sequence: 0,
        };
        let low_second = MqKey {
            priority: 1,
            sequence: 1,
        };
        let high = MqKey {
            priority: 2,
            sequence: 2,
        };
        assert!(mqueue_dequeue_precedes(high, low_first));
        assert!(mqueue_dequeue_precedes(low_first, low_second));
        assert!(!mqueue_dequeue_precedes(low_second, low_first));
        assert!(!mqueue_dequeue_precedes(high, high));
        // Insertion keeps the stored prefix ahead of the candidate, so equal
        // priorities stay FIFO and a higher priority jumps the queue.
        assert!(mqueue_insert_prefix(high, low_first));
        assert!(!mqueue_insert_prefix(low_first, high));
        assert!(mqueue_insert_prefix(low_first, low_second));
    }

    #[test]
    fn mq_notify_only_fires_on_the_unsupervised_empty_edge() {
        // Empty -> one message inserted by msg_insert(): notification fires.
        assert!(mq_notify_fires(true, false, 1));
        // Pipelined to a blocked receiver: the tree is untouched.
        assert!(!mq_notify_fires(true, true, 0));
        assert!(!mq_notify_fires(true, true, 1));
        // No registration, or a message that did not cross the empty edge.
        assert!(!mq_notify_fires(false, false, 1));
        assert!(!mq_notify_fires(true, false, 2));
    }

    #[test]
    fn mq_unlink_sticky_directory_authority() {
        let base = MqUnlinkRequest {
            queue_uid: 1000,
            directory_uid: 0,
            fsuid: 1001,
            directory_sticky: true,
            directory_write_exec: true,
            cap_fowner: false,
        };
        // A different user in the sticky root is refused with EPERM.
        assert_eq!(
            authorize_mq_unlink(base),
            Err(MqUnlinkError::StickyDirectory)
        );
        // The queue owner may unlink its own queue.
        assert_eq!(
            authorize_mq_unlink(MqUnlinkRequest {
                fsuid: 1000,
                ..base
            }),
            Ok(())
        );
        // So may the owner of the directory, even for someone else's queue.
        assert_eq!(
            authorize_mq_unlink(MqUnlinkRequest {
                directory_uid: 1001,
                ..base
            }),
            Ok(())
        );
        assert_eq!(
            authorize_mq_unlink(MqUnlinkRequest {
                cap_fowner: true,
                ..base
            }),
            Ok(())
        );
        // The directory permission is checked before the sticky bit.
        assert_eq!(
            authorize_mq_unlink(MqUnlinkRequest {
                directory_write_exec: false,
                ..base
            }),
            Err(MqUnlinkError::DirectoryInaccessible)
        );
        // A non-sticky directory never rejects on ownership.
        assert_eq!(
            authorize_mq_unlink(MqUnlinkRequest {
                directory_sticky: false,
                ..base
            }),
            Ok(())
        );
    }
}
