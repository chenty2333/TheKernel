//! Pure System V IPC and POSIX mqueue policy. It owns neither managers, queues, nor VM.
#![no_std]
#![forbid(unsafe_code)]

pub const IPC_NOWAIT: u16 = 0o4000;
pub const SEM_UNDO: u16 = 0x1000;
pub const SHM_RDONLY: u32 = 0o10000;
pub const SHM_RND: u32 = 0o20000;
pub const SHMLBA: usize = 4096;
/// Linux `MQ_PRIO_MAX` (`include/uapi/linux/mqueue.h`). `mq_timedsend()`
/// rejects `msg_prio >= MQ_PRIO_MAX` with `EINVAL`.
pub const MQ_PRIO_MAX: u32 = 32768;

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
    },
    WaitDecrease {
        index: u16,
        amount: u16,
        nowait: bool,
        undo: bool,
    },
}
pub const fn plan_sem_op(op: SemBuf) -> Result<SemPlan, IpcError> {
    if op.flags & !(IPC_NOWAIT as i16 | SEM_UNDO as i16) != 0 {
        return Err(IpcError::InvalidOperation);
    }
    let nowait = op.flags & IPC_NOWAIT as i16 != 0;
    let undo = op.flags & SEM_UNDO as i16 != 0;
    if op.op > 0 {
        Ok(SemPlan::Adjust {
            index: op.num,
            delta: op.op,
            undo,
        })
    } else if op.op == 0 {
        if undo {
            Err(IpcError::InvalidOperation)
        } else {
            Ok(SemPlan::WaitZero {
                index: op.num,
                nowait,
            })
        }
    } else {
        Ok(SemPlan::WaitDecrease {
            index: op.num,
            amount: op.op.unsigned_abs(),
            nowait,
            undo,
        })
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
    fn sem_validation_order() {
        assert_eq!(
            plan_sem_op(SemBuf {
                num: 0,
                op: 0,
                flags: SEM_UNDO as i16
            }),
            Err(IpcError::InvalidOperation)
        );
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
