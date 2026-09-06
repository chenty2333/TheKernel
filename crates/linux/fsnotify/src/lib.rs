//! Bounded, fallible admission planning for filesystem notifications.
//!
//! This crate plans watches, marks, queues, and permission decisions without
//! owning VFS locations, file descriptors, signals, or poll state.

#![no_std]
#![deny(missing_docs)]

/// fanotify access event.
pub const FAN_ACCESS: u64 = 0x0000_0001;
/// fanotify data-modification event.
pub const FAN_MODIFY: u64 = 0x0000_0002;
/// fanotify metadata-change event.
pub const FAN_ATTRIB: u64 = 0x0000_0004;
/// fanotify writable-close event.
pub const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
/// fanotify non-writable-close event.
pub const FAN_CLOSE_NOWRITE: u64 = 0x0000_0010;
/// fanotify open event.
pub const FAN_OPEN: u64 = 0x0000_0020;
/// fanotify move-from event.
pub const FAN_MOVED_FROM: u64 = 0x0000_0040;
/// fanotify move-to event.
pub const FAN_MOVED_TO: u64 = 0x0000_0080;
/// fanotify child-create event.
pub const FAN_CREATE: u64 = 0x0000_0100;
/// fanotify child-delete event.
pub const FAN_DELETE: u64 = 0x0000_0200;
/// fanotify watched-object delete event.
pub const FAN_DELETE_SELF: u64 = 0x0000_0400;
/// fanotify watched-object move event.
pub const FAN_MOVE_SELF: u64 = 0x0000_0800;
/// fanotify executable-open event.
pub const FAN_OPEN_EXEC: u64 = 0x0000_1000;
/// fanotify queue-overflow event.
pub const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
/// fanotify filesystem-error event.
pub const FAN_FS_ERROR: u64 = 0x0000_8000;
/// fanotify open-permission event.
pub const FAN_OPEN_PERM: u64 = 0x0001_0000;
/// fanotify access-permission event.
pub const FAN_ACCESS_PERM: u64 = 0x0002_0000;
/// fanotify executable-open permission event.
pub const FAN_OPEN_EXEC_PERM: u64 = 0x0004_0000;
/// Deliver an event occurring below a marked directory.
pub const FAN_EVENT_ON_CHILD: u64 = 0x0800_0000;
/// fanotify rename event.
pub const FAN_RENAME: u64 = 0x1000_0000;
/// Mark an event as referring to a directory.
pub const FAN_ONDIR: u64 = 0x4000_0000;
/// Both fanotify close events.
pub const FAN_CLOSE: u64 = FAN_CLOSE_WRITE | FAN_CLOSE_NOWRITE;
/// Both fanotify move events.
pub const FAN_MOVE: u64 = FAN_MOVED_FROM | FAN_MOVED_TO;

/// Set close-on-exec on a fanotify group fd.
pub const FAN_CLOEXEC: u32 = 0x0000_0001;
/// Create a nonblocking fanotify group fd.
pub const FAN_NONBLOCK: u32 = 0x0000_0002;
/// Notification-only fanotify class.
pub const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
/// Content-permission fanotify class.
pub const FAN_CLASS_CONTENT: u32 = 0x0000_0004;
/// Pre-content-permission fanotify class.
pub const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
/// Request an unlimited event queue.
pub const FAN_UNLIMITED_QUEUE: u32 = 0x0000_0010;
/// Request unlimited marks.
pub const FAN_UNLIMITED_MARKS: u32 = 0x0000_0020;
/// Enable audit records for responses.
pub const FAN_ENABLE_AUDIT: u32 = 0x0000_0040;
/// Report event origin through a pidfd.
pub const FAN_REPORT_PIDFD: u32 = 0x0000_0080;
/// Report the triggering thread id rather than process id.
pub const FAN_REPORT_TID: u32 = 0x0000_0100;
/// Report file handles.
pub const FAN_REPORT_FID: u32 = 0x0000_0200;
/// Report directory file handles.
pub const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
/// Report a name alongside a directory file handle.
pub const FAN_REPORT_NAME: u32 = 0x0000_0800;
/// Report target file handles.
pub const FAN_REPORT_TARGET_FID: u32 = 0x0000_1000;
/// Report directory file handle and name.
pub const FAN_REPORT_DFID_NAME: u32 = FAN_REPORT_DIR_FID | FAN_REPORT_NAME;
/// Report directory/name and both source and target file handles.
pub const FAN_REPORT_DFID_NAME_TARGET: u32 =
    FAN_REPORT_DFID_NAME | FAN_REPORT_FID | FAN_REPORT_TARGET_FID;

/// Permit a fanotify permission event.
pub const FAN_ALLOW: u32 = 0x01;
/// Deny a fanotify permission event.
pub const FAN_DENY: u32 = 0x02;
/// Audit a fanotify permission response.
pub const FAN_AUDIT: u32 = 0x10;
/// Reserved fanotify response info flag.
pub const FAN_INFO: u32 = 0x20;
/// Number of bits allotted to `FAN_DENY_ERRNO`'s errno value.
pub const FAN_ERRNO_BITS: u32 = 8;
/// Bit position of `FAN_DENY_ERRNO`'s errno value.
pub const FAN_ERRNO_SHIFT: u32 = 32 - FAN_ERRNO_BITS;
/// Unshifted bit mask for `FAN_DENY_ERRNO`'s errno value.
pub const FAN_ERRNO_MASK: u32 = (1 << FAN_ERRNO_BITS) - 1;

/// Encodes a Linux `FAN_DENY_ERRNO(err)` permission response.
#[must_use]
pub const fn fan_deny_errno(errno: u32) -> u32 {
    FAN_DENY | (errno & FAN_ERRNO_MASK) << FAN_ERRNO_SHIFT
}

/// Add a fanotify mark.
pub const FAN_MARK_ADD: u32 = 0x0000_0001;
/// Remove a fanotify mark.
pub const FAN_MARK_REMOVE: u32 = 0x0000_0002;
/// Do not follow a final symlink while resolving a mark target.
pub const FAN_MARK_DONT_FOLLOW: u32 = 0x0000_0004;
/// Require a directory mark target.
pub const FAN_MARK_ONLYDIR: u32 = 0x0000_0008;
/// Mark a mount.
pub const FAN_MARK_MOUNT: u32 = 0x0000_0010;
/// Update an ignored mask.
pub const FAN_MARK_IGNORED_MASK: u32 = 0x0000_0020;
/// Keep an ignored mask after modification.
pub const FAN_MARK_IGNORED_SURV_MODIFY: u32 = 0x0000_0040;
/// Flush selected fanotify marks.
pub const FAN_MARK_FLUSH: u32 = 0x0000_0080;
/// Mark an entire filesystem.
pub const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;
/// Mark a mark evictable.
pub const FAN_MARK_EVICTABLE: u32 = 0x0000_0200;
/// Add an ignore mark.
pub const FAN_MARK_IGNORE: u32 = 0x0000_0400;

/// Linux fanotify metadata wire-format version.
pub const FANOTIFY_METADATA_VERSION: u8 = 3;
/// Sentinel indicating no file descriptor accompanies an event.
pub const FAN_NOFD: i32 = -1;
/// Sentinel indicating no pidfd accompanies an event.
pub const FAN_NOPIDFD: i32 = FAN_NOFD;
/// Sentinel indicating pidfd creation failed.
pub const FAN_EPIDFD: i32 = -2;
/// fanotify event-info record type for a pidfd.
pub const FAN_EVENT_INFO_TYPE_PIDFD: u8 = 4;

/// All fanotify permission-event bits.
pub const FANOTIFY_PERM_EVENTS: u64 = FAN_OPEN_PERM | FAN_ACCESS_PERM | FAN_OPEN_EXEC_PERM;
/// All ordinary fanotify event bits.
pub const FANOTIFY_EVENTS: u64 = FAN_ACCESS
    | FAN_MODIFY
    | FAN_ATTRIB
    | FAN_CLOSE
    | FAN_OPEN
    | FAN_OPEN_EXEC
    | FAN_MOVE
    | FAN_CREATE
    | FAN_DELETE
    | FAN_RENAME
    | FAN_DELETE_SELF
    | FAN_MOVE_SELF
    | FAN_FS_ERROR;
/// All event-mask bits accepted by fanotify mark commands.
pub const ALL_FANOTIFY_EVENT_BITS: u64 =
    FANOTIFY_EVENTS | FANOTIFY_PERM_EVENTS | FAN_Q_OVERFLOW | FAN_ONDIR | FAN_EVENT_ON_CHILD;
/// All file-identifier report flags.
pub const FANOTIFY_FID_BITS: u32 = FAN_REPORT_DFID_NAME_TARGET;
/// fanotify-init flags requiring elevated accounting or authority.
pub const FANOTIFY_ADMIN_INIT_FLAGS: u32 = FAN_CLASS_CONTENT
    | FAN_CLASS_PRE_CONTENT
    | FAN_REPORT_TID
    | FAN_REPORT_PIDFD
    | FAN_UNLIMITED_QUEUE
    | FAN_UNLIMITED_MARKS
    | FAN_ENABLE_AUDIT;
/// fanotify-init flags available without elevated authority.
pub const FANOTIFY_USER_INIT_FLAGS: u32 =
    FAN_CLASS_NOTIF | FANOTIFY_FID_BITS | FAN_CLOEXEC | FAN_NONBLOCK;
/// All recognized fanotify-init flags.
pub const FANOTIFY_INIT_FLAGS: u32 = FANOTIFY_ADMIN_INIT_FLAGS | FANOTIFY_USER_INIT_FLAGS;
/// All recognized fanotify-mark flags.
pub const FANOTIFY_MARK_FLAGS: u32 = FAN_MARK_ADD
    | FAN_MARK_REMOVE
    | FAN_MARK_FLUSH
    | FAN_MARK_DONT_FOLLOW
    | FAN_MARK_ONLYDIR
    | FAN_MARK_MOUNT
    | FAN_MARK_FILESYSTEM
    | FAN_MARK_IGNORED_MASK
    | FAN_MARK_IGNORED_SURV_MODIFY
    | FAN_MARK_EVICTABLE
    | FAN_MARK_IGNORE;
/// The mutually-exclusive action bits in a permission response.
pub const FANOTIFY_RESPONSE_ACCESS: u32 = FAN_ALLOW | FAN_DENY;
/// Optional permission-response flags.
pub const FANOTIFY_RESPONSE_FLAGS: u32 = FAN_AUDIT | FAN_INFO;
/// Shifted errno bits admitted in a pre-content deny response.
pub const FANOTIFY_RESPONSE_ERRNO_MASK: u32 = FAN_ERRNO_MASK << FAN_ERRNO_SHIFT;
/// All recognized permission-response bits.
pub const FANOTIFY_RESPONSE_VALID_MASK: u32 =
    FANOTIFY_RESPONSE_ACCESS | FANOTIFY_RESPONSE_FLAGS | FANOTIFY_RESPONSE_ERRNO_MASK;
/// Directory-entry event bits.
pub const FANOTIFY_DIR_ENTRY_EVENTS: u64 = FAN_CREATE | FAN_DELETE | FAN_MOVE | FAN_RENAME;
/// Permission-group dispatch order, from highest to lowest Linux priority.
pub const FANOTIFY_PERMISSION_CLASSES: [u32; 2] = [FAN_CLASS_PRE_CONTENT, FAN_CLASS_CONTENT];

/// The fixed header that begins every fanotify event record.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanotifyEventMetadata {
    /// Total event-record length, including appended info records.
    pub event_len: u32,
    /// [`FANOTIFY_METADATA_VERSION`].
    pub vers: u8,
    /// Reserved, and zero in emitted records.
    pub reserved: u8,
    /// Size of this metadata header.
    pub metadata_len: u16,
    /// Event-mask bits.
    pub mask: u64,
    /// Event file descriptor or [`FAN_NOFD`].
    pub fd: i32,
    /// Triggering process or thread id.
    pub pid: i32,
}

/// A fanotify pidfd event-info record.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanotifyEventInfoPidfd {
    /// [`FAN_EVENT_INFO_TYPE_PIDFD`].
    pub info_type: u8,
    /// Padding byte, and zero in emitted records.
    pub pad: u8,
    /// Size of this record.
    pub len: u16,
    /// Event pidfd or [`FAN_NOPIDFD`].
    pub pidfd: i32,
}

/// A fanotify userspace permission response record.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanotifyResponse {
    /// File descriptor supplied with the permission event.
    pub fd: i32,
    /// Permission action and optional response flags.
    pub response: u32,
}

/// The result of attempting to append one event to a bounded queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueAdmission {
    /// The event may be appended normally.
    Enqueue,
    /// Drop the incoming event because an overflow marker is already pending.
    Drop,
    /// Replace ordinary data with one overflow marker.
    Overflow,
}

/// fanotify init grammar rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanotifyInitReject {
    /// The supplied bit combination is not valid Linux fanotify grammar.
    Invalid,
    /// The request needs an accounting mode unavailable to this kernel.
    Unsupported,
}

/// fanotify permission-response grammar rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanotifyResponseReject {
    /// The action or flags do not form a valid response.
    Invalid,
    /// The response requests auditing for a group without audit enabled.
    AuditNotEnabled,
    /// The response requests unsupported information.
    InfoUnsupported,
}

/// An admitted fanotify permission response, with its selected denial errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanotifyResponsePlan {
    /// Permit the blocked operation.
    Allow,
    /// Deny the operation, optionally with a Linux errno selected by a
    /// pre-content group.
    Deny {
        /// The selected positive Linux errno, if one was encoded.
        errno: Option<u8>,
    },
}

/// inotify add-watch grammar result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InotifyWatchPlan {
    /// Allocate a new watch.
    New,
    /// Replace an existing watch mask.
    Replace,
    /// Add bits to an existing watch mask.
    Add,
}
/// Invalid inotify add-watch flag combination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InotifyWatchReject {
    /// IN_MASK_ADD and IN_MASK_CREATE were both specified.
    ConflictingUpdateFlags,
    /// IN_MASK_CREATE named an already watched object.
    ExistingWatch,
}
/// Plans inotify's existing-watch update grammar.
pub const fn plan_inotify_watch(
    mask: u32,
    exists: bool,
) -> Result<InotifyWatchPlan, InotifyWatchReject> {
    const MASK_ADD: u32 = 0x2000_0000;
    const MASK_CREATE: u32 = 0x1000_0000;
    if mask & MASK_ADD != 0 && mask & MASK_CREATE != 0 {
        return Err(InotifyWatchReject::ConflictingUpdateFlags);
    }
    if exists && mask & MASK_CREATE != 0 {
        return Err(InotifyWatchReject::ExistingWatch);
    }
    Ok(if !exists {
        InotifyWatchPlan::New
    } else if mask & MASK_ADD != 0 {
        InotifyWatchPlan::Add
    } else {
        InotifyWatchPlan::Replace
    })
}

/// fanotify mark grammar rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanotifyMarkReject {
    /// Mark flags or masks are invalid.
    Invalid,
    /// The requested operation requires a directory.
    NotDirectory,
    /// The requested ignore form requires a non-directory.
    IsDirectory,
}
/// fanotify mark action after flag grammar admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanotifyMarkPlan {
    /// Flush selected mark scopes.
    Flush,
    /// Update an ignored mask.
    Ignored,
    /// Add a mark.
    Add,
    /// Remove a mark.
    Remove,
}
/// Validates mark command and target grammar from copied scalar inputs.
pub const fn plan_fanotify_mark(
    flags: u32,
    mask: u64,
    group_flags: u32,
    target_is_dir: Option<bool>,
) -> Result<FanotifyMarkPlan, FanotifyMarkReject> {
    if flags & !FANOTIFY_MARK_FLAGS != 0 || mask & !ALL_FANOTIFY_EVENT_BITS != 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    let commands = (flags & FAN_MARK_ADD != 0) as u8
        + (flags & FAN_MARK_REMOVE != 0) as u8
        + (flags & FAN_MARK_FLUSH != 0) as u8;
    if commands != 1
        || flags & FAN_MARK_IGNORED_MASK != 0 && flags & FAN_MARK_IGNORE != 0
        || flags & FAN_MARK_MOUNT != 0 && flags & FAN_MARK_FILESYSTEM != 0
    {
        return Err(FanotifyMarkReject::Invalid);
    }
    if flags & FAN_MARK_FLUSH != 0 {
        if flags & !(FAN_MARK_FLUSH | FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
            return Err(FanotifyMarkReject::Invalid);
        }
        return Ok(FanotifyMarkPlan::Flush);
    }
    if mask == 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    if target_is_dir.is_none() {
        return Ok(if flags & (FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE) != 0 {
            FanotifyMarkPlan::Ignored
        } else if flags & FAN_MARK_ADD != 0 {
            FanotifyMarkPlan::Add
        } else {
            FanotifyMarkPlan::Remove
        });
    }
    let target_is_dir = match target_is_dir {
        Some(value) => value,
        None => return Err(FanotifyMarkReject::Invalid),
    };
    if flags & FAN_MARK_ONLYDIR != 0 && !target_is_dir {
        return Err(FanotifyMarkReject::NotDirectory);
    }
    if flags & FAN_MARK_EVICTABLE != 0 && flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    if mask & FANOTIFY_PERM_EVENTS != 0
        && group_flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT) == 0
    {
        return Err(FanotifyMarkReject::Invalid);
    }
    if group_flags & FANOTIFY_FID_BITS == 0
        && mask & (FAN_ATTRIB | FANOTIFY_DIR_ENTRY_EVENTS | FAN_DELETE_SELF | FAN_MOVE_SELF) != 0
        || group_flags & FAN_REPORT_NAME == 0 && mask & FAN_RENAME != 0
        || flags & FAN_MARK_MOUNT != 0 && mask & FANOTIFY_DIR_ENTRY_EVENTS != 0
    {
        return Err(FanotifyMarkReject::Invalid);
    }
    let inode_mark = flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) == 0;
    let strict_dir_events = group_flags & FAN_REPORT_TARGET_FID != 0
        || mask & FAN_RENAME != 0
        || flags & FAN_MARK_IGNORE != 0;
    if inode_mark
        && strict_dir_events
        && !target_is_dir
        && mask & (FANOTIFY_DIR_ENTRY_EVENTS | FAN_ONDIR | FAN_EVENT_ON_CHILD) != 0
    {
        return Err(FanotifyMarkReject::NotDirectory);
    }
    if flags & FAN_MARK_ADD != 0
        && flags & FAN_MARK_IGNORE != 0
        && flags & FAN_MARK_IGNORED_SURV_MODIFY == 0
    {
        if !inode_mark {
            return Err(FanotifyMarkReject::Invalid);
        }
        if target_is_dir {
            return Err(FanotifyMarkReject::IsDirectory);
        }
    }
    Ok(if flags & (FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE) != 0 {
        FanotifyMarkPlan::Ignored
    } else if flags & FAN_MARK_ADD != 0 {
        FanotifyMarkPlan::Add
    } else {
        FanotifyMarkPlan::Remove
    })
}

/// Validates Linux fanotify_init flag grammar independent of FD allocation.
pub const fn fanotify_init_admission(
    flags: u32,
    event_flags: u32,
) -> Result<(), FanotifyInitReject> {
    if flags & !FANOTIFY_INIT_FLAGS != 0
        || flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT)
            == FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT
        || flags & FAN_REPORT_PIDFD != 0 && flags & FAN_REPORT_TID != 0
        || flags & FAN_REPORT_FID != 0 && flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT) != 0
        || flags & FAN_REPORT_NAME != 0 && flags & FAN_REPORT_DIR_FID == 0
        || flags & FAN_REPORT_TARGET_FID != 0
            && flags & (FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME)
                != FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME
        || event_flags & !(3 | 0x80000) != 0
    {
        return Err(FanotifyInitReject::Invalid);
    }
    if flags & (FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS) != 0 {
        return Err(FanotifyInitReject::Unsupported);
    }
    Ok(())
}

/// Admits the non-FD portion of a Linux fanotify permission response.
///
/// `pre_content` selects the only group class that may encode an errno in a
/// `FAN_DENY_ERRNO` response.
pub const fn fanotify_response_admission(
    response: u32,
    audit_enabled: bool,
    pre_content: bool,
) -> Result<FanotifyResponsePlan, FanotifyResponseReject> {
    if response & !FANOTIFY_RESPONSE_VALID_MASK != 0
        || !matches!(response & FANOTIFY_RESPONSE_ACCESS, FAN_ALLOW | FAN_DENY)
    {
        return Err(FanotifyResponseReject::Invalid);
    }
    if response & FAN_AUDIT != 0 && !audit_enabled {
        return Err(FanotifyResponseReject::AuditNotEnabled);
    }
    if response & FAN_INFO != 0 {
        return Err(FanotifyResponseReject::InfoUnsupported);
    }
    let errno = ((response & FANOTIFY_RESPONSE_ERRNO_MASK) >> FAN_ERRNO_SHIFT) as u8;
    if errno != 0
        && (response & FANOTIFY_RESPONSE_ACCESS != FAN_DENY
            || !pre_content
            || !matches!(errno, 1 | 5 | 11 | 16 | 26 | 28 | 122))
    {
        return Err(FanotifyResponseReject::Invalid);
    }
    Ok(if response & FANOTIFY_RESPONSE_ACCESS == FAN_ALLOW {
        FanotifyResponsePlan::Allow
    } else {
        FanotifyResponsePlan::Deny {
            errno: if errno == 0 { None } else { Some(errno) },
        }
    })
}

/// Plans Linux's one-shot overflow-marker policy for a bounded queue.
#[must_use]
pub const fn plan_queue_admission(
    queued: usize,
    capacity: usize,
    overflow_pending: bool,
    event_is_overflow: bool,
) -> QueueAdmission {
    if overflow_pending {
        QueueAdmission::Drop
    } else if event_is_overflow || queued >= capacity {
        QueueAdmission::Overflow
    } else {
        QueueAdmission::Enqueue
    }
}

/// Computes the Linux inotify wire payload bytes for a child name, including
/// its NUL and `inotify_event` alignment padding.
#[must_use]
pub const fn inotify_name_wire_len(name_len: usize, header_len: usize) -> usize {
    if name_len == 0 {
        0
    } else {
        let bytes = name_len.saturating_add(1);
        bytes.saturating_add(header_len.saturating_sub(1)) / header_len * header_len
    }
}

/// Adds `IN_ISDIR` except for self events, matching Linux event grammar.
#[must_use]
pub const fn inotify_exact_mask(
    mask: u32,
    is_dir: bool,
    move_self: u32,
    delete_self: u32,
    isdir: u32,
) -> u32 {
    if is_dir && mask != move_self && mask != delete_self {
        mask | isdir
    } else {
        mask
    }
}

/// Converts an inotify event mask into its corresponding fanotify event mask.
#[must_use]
pub const fn inotify_to_fanotify(mask: u32) -> u64 {
    let mut out = 0;
    if mask & 0x0000_0001 != 0 {
        out |= 0x0000_0001;
    }
    if mask & 0x0000_0002 != 0 {
        out |= 0x0000_0002;
    }
    if mask & 0x0000_0004 != 0 {
        out |= 0x0000_0004;
    }
    if mask & 0x0000_0008 != 0 {
        out |= 0x0000_0008;
    }
    if mask & 0x0000_0010 != 0 {
        out |= 0x0000_0010;
    }
    if mask & 0x0000_0020 != 0 {
        out |= 0x0000_0020;
    }
    if mask & 0x0000_0040 != 0 {
        out |= 0x0000_0040;
    }
    if mask & 0x0000_0080 != 0 {
        out |= 0x0000_0080;
    }
    if mask & 0x0000_0100 != 0 {
        out |= 0x0000_0100;
    }
    if mask & 0x0000_0200 != 0 {
        out |= 0x0000_0200;
    }
    if mask & 0x0000_0400 != 0 {
        out |= 0x0000_0400;
    }
    if mask & 0x0000_0800 != 0 {
        out |= 0x0000_0800;
    }
    if mask & 0x4000_0000 != 0 {
        out |= 0x4000_0000;
    }
    out
}

/// Converts Linux `F_NOTIFY`'s unsigned-long argument to its unsigned-int
/// mask and discards unsupported bits.
#[must_use]
pub const fn dnotify_mask(arg: usize, allowed: u32) -> u32 {
    (arg as u32) & allowed
}

/// Returns whether a dnotify request withdraws an existing mark.
#[must_use]
pub const fn dnotify_is_remove(mask: u32, multishot: u32) -> bool {
    mask & !multishot == 0
}

/// An opaque filesystem object identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u64);

/// The notification frontend requested by a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    /// An inotify watch.
    Inotify,
    /// A fanotify mark.
    Fanotify,
    /// A legacy dnotify directory watch.
    Dnotify,
}

/// A filesystem event bit mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventMask(pub u32);
impl EventMask {
    /// The empty event set.
    pub const EMPTY: Self = Self(0);
    /// Creation events.
    pub const CREATE: Self = Self(1 << 0);
    /// Deletion events.
    pub const DELETE: Self = Self(1 << 1);
    /// Modification events.
    pub const MODIFY: Self = Self(1 << 2);
    /// Metadata change events.
    pub const ATTRIB: Self = Self(1 << 3);
    /// Rename or movement events.
    pub const MOVE: Self = Self(1 << 4);
    /// Access events.
    pub const ACCESS: Self = Self(1 << 5);
    /// Permission decision events.
    pub const PERMISSION: Self = Self(1 << 6);
    /// Returns whether no event is selected.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    /// Returns whether this mask contains every event in `other`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// A request to create a watch or mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchRequest {
    /// Requested notification frontend.
    pub interface: Interface,
    /// Target object.
    pub object: ObjectId,
    /// Events to receive.
    pub mask: EventMask,
    /// Requested bounded queue capacity.
    pub queue_capacity: u16,
    /// Whether permission events must block for a decision.
    pub permission_events: bool,
}

/// Immutable object and authority state captured for planning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Events which this caller may observe.
    pub observable: EventMask,
    /// Whether this caller can make permission decisions.
    pub may_decide_permission: bool,
    /// Whether the target is a directory.
    pub is_directory: bool,
    /// Whether the target was live at snapshot time.
    pub alive: bool,
    /// Queue credits available in the notification domain.
    pub available_queue_credits: u32,
}

/// Bounds maintained by the notification domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest capacity reservable by one subscription.
    pub max_queue_capacity: u16,
    /// Maximum live marks or watches on one object.
    pub max_marks_per_object: u16,
    /// Existing live marks or watches on the target.
    pub existing_marks: u16,
}

/// The execution phase of a plan step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Reserve finite resources.
    Reserve,
    /// Install a live watch or mark.
    Install,
    /// Publish a complete subscription.
    Publish,
}

/// A transactional forward operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Reserve queue storage.
    ReserveQueue {
        /// Number of credits to reserve.
        capacity: u16,
    },
    /// Install a watch or mark.
    Install {
        /// Interface to install.
        interface: Interface,
        /// Object receiving the watch or mark.
        object: ObjectId,
        /// Events enabled by the operation.
        mask: EventMask,
    },
    /// Publish the subscription to its owner.
    Publish,
}
impl Step {
    /// Returns this step's execution phase.
    pub const fn phase(self) -> Phase {
        match self {
            Self::ReserveQueue { .. } => Phase::Reserve,
            Self::Install { .. } => Phase::Install,
            Self::Publish => Phase::Publish,
        }
    }
}

/// A compensation for a completed plan step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rollback {
    /// Return queue storage.
    ReleaseQueue {
        /// Number of credits to release.
        capacity: u16,
    },
    /// Remove an installed watch or mark.
    Remove {
        /// Interface to remove.
        interface: Interface,
        /// Object from which to remove it.
        object: ObjectId,
    },
    /// Withdraw the published subscription.
    Unpublish,
}

/// A fixed-size install plan; execute rollback entries in reverse order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Forward steps.
    pub steps: [Step; 3],
    /// Corresponding compensations in forward order.
    pub rollback: [Rollback; 3],
}
impl Plan {
    /// Returns whether resource reservation precedes installation and publication.
    pub fn is_monotonic(self) -> bool {
        self.steps[0].phase() <= self.steps[1].phase()
            && self.steps[1].phase() <= self.steps[2].phase()
    }
}

/// A typed admission failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Target was already dead.
    DeadObject,
    /// No events were requested.
    EmptyMask,
    /// The caller lacks observation authority.
    ObserveDenied,
    /// Permission events require fanotify.
    PermissionInterface,
    /// The caller lacks permission-decision authority.
    PermissionDenied,
    /// dnotify requires a directory target.
    DnotifyRequiresDirectory,
    /// Queue capacity was zero.
    ZeroQueue,
    /// Queue capacity exceeded the per-request bound.
    QueueLimit {
        /// Configured maximum capacity.
        maximum: u16,
    },
    /// Insufficient shared queue credits remained.
    QueueExhausted {
        /// Credits observed in the snapshot.
        available: u32,
    },
    /// The target mark/watch limit was reached.
    MarkLimit {
        /// Configured maximum number of marks.
        maximum: u16,
    },
}

/// Creates a bounded plan from a copied request and snapshot.
///
/// The executor must revalidate lifecycle facts while executing the plan and
/// execute [`Plan::rollback`] in reverse order after a later failure.
pub fn plan(request: WatchRequest, snapshot: Snapshot, limits: Limits) -> Result<Plan, Reject> {
    if !snapshot.alive {
        return Err(Reject::DeadObject);
    }
    if request.mask.is_empty() {
        return Err(Reject::EmptyMask);
    }
    if !snapshot.observable.contains(request.mask) {
        return Err(Reject::ObserveDenied);
    }
    if request.permission_events || request.mask.contains(EventMask::PERMISSION) {
        if request.interface != Interface::Fanotify {
            return Err(Reject::PermissionInterface);
        }
        if !snapshot.may_decide_permission {
            return Err(Reject::PermissionDenied);
        }
    }
    if request.interface == Interface::Dnotify && !snapshot.is_directory {
        return Err(Reject::DnotifyRequiresDirectory);
    }
    if request.queue_capacity == 0 {
        return Err(Reject::ZeroQueue);
    }
    if request.queue_capacity > limits.max_queue_capacity {
        return Err(Reject::QueueLimit {
            maximum: limits.max_queue_capacity,
        });
    }
    if request.queue_capacity as u32 > snapshot.available_queue_credits {
        return Err(Reject::QueueExhausted {
            available: snapshot.available_queue_credits,
        });
    }
    if limits.existing_marks >= limits.max_marks_per_object {
        return Err(Reject::MarkLimit {
            maximum: limits.max_marks_per_object,
        });
    }
    Ok(Plan {
        steps: [
            Step::ReserveQueue {
                capacity: request.queue_capacity,
            },
            Step::Install {
                interface: request.interface,
                object: request.object,
                mask: request.mask,
            },
            Step::Publish,
        ],
        rollback: [
            Rollback::ReleaseQueue {
                capacity: request.queue_capacity,
            },
            Rollback::Remove {
                interface: request.interface,
                object: request.object,
            },
            Rollback::Unpublish,
        ],
    })
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::*;
    const REQUEST: WatchRequest = WatchRequest {
        interface: Interface::Inotify,
        object: ObjectId(9),
        mask: EventMask(EventMask::CREATE.0 | EventMask::DELETE.0),
        queue_capacity: 4,
        permission_events: false,
    };
    const SNAPSHOT: Snapshot = Snapshot {
        observable: EventMask(u32::MAX),
        may_decide_permission: false,
        is_directory: true,
        alive: true,
        available_queue_credits: 8,
    };
    const LIMITS: Limits = Limits {
        max_queue_capacity: 8,
        max_marks_per_object: 2,
        existing_marks: 0,
    };
    #[test]
    fn plan_is_monotonic_and_has_reverse_compensation() {
        let result = plan(REQUEST, SNAPSHOT, LIMITS).unwrap();
        assert!(result.is_monotonic());
        assert_eq!(
            result.rollback[1],
            Rollback::Remove {
                interface: Interface::Inotify,
                object: ObjectId(9)
            }
        );
    }
    #[test]
    fn permissions_are_fanotify_only_and_authorized() {
        let request = WatchRequest {
            mask: EventMask::PERMISSION,
            permission_events: true,
            ..REQUEST
        };
        assert_eq!(
            plan(request, SNAPSHOT, LIMITS),
            Err(Reject::PermissionInterface)
        );
        let request = WatchRequest {
            interface: Interface::Fanotify,
            ..request
        };
        assert_eq!(
            plan(request, SNAPSHOT, LIMITS),
            Err(Reject::PermissionDenied)
        );
        assert!(
            plan(
                request,
                Snapshot {
                    may_decide_permission: true,
                    ..SNAPSHOT
                },
                LIMITS
            )
            .is_ok()
        );
    }
    #[test]
    fn lifecycle_and_finite_resources_reject_before_installation() {
        assert_eq!(
            plan(
                REQUEST,
                Snapshot {
                    alive: false,
                    ..SNAPSHOT
                },
                LIMITS
            ),
            Err(Reject::DeadObject)
        );
        assert_eq!(
            plan(
                REQUEST,
                Snapshot {
                    available_queue_credits: 3,
                    ..SNAPSHOT
                },
                LIMITS
            ),
            Err(Reject::QueueExhausted { available: 3 })
        );
        assert_eq!(
            plan(
                REQUEST,
                SNAPSHOT,
                Limits {
                    existing_marks: 2,
                    ..LIMITS
                }
            ),
            Err(Reject::MarkLimit { maximum: 2 })
        );
    }
    #[test]
    fn dnotify_requires_a_directory() {
        assert_eq!(
            plan(
                WatchRequest {
                    interface: Interface::Dnotify,
                    ..REQUEST
                },
                Snapshot {
                    is_directory: false,
                    ..SNAPSHOT
                },
                LIMITS
            ),
            Err(Reject::DnotifyRequiresDirectory)
        );
    }

    #[test]
    fn linux_wire_grammar_preserves_padding_masks_and_overflow() {
        assert_eq!(inotify_name_wire_len(0, 16), 0);
        assert_eq!(inotify_name_wire_len(3, 16), 16);
        assert_eq!(inotify_name_wire_len(16, 16), 32);
        assert_eq!(
            inotify_exact_mask(0x800, true, 0x800, 0x400, 0x4000_0000),
            0x800
        );
        assert_eq!(
            inotify_exact_mask(1, true, 0x800, 0x400, 0x4000_0000),
            0x4000_0001
        );
        assert_eq!(
            plan_queue_admission(8, 8, false, false),
            QueueAdmission::Overflow
        );
        assert_eq!(
            plan_queue_admission(8, 8, true, false),
            QueueAdmission::Drop
        );
        assert_eq!(dnotify_mask((1usize << 32) | 3, 7), 3);
    }

    #[test]
    fn fanotify_init_grammar_rejects_incompatible_reports() {
        assert_eq!(
            fanotify_init_admission(0x80 | 0x100, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(0x10, 0),
            Err(FanotifyInitReject::Unsupported)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_TARGET_FID | FAN_REPORT_DFID_NAME, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert!(fanotify_init_admission(1 | 2, 0).is_ok());
    }

    #[test]
    fn watch_and_mark_plans_preserve_update_and_target_order() {
        assert_eq!(
            plan_inotify_watch(0x3000_0000, false),
            Err(InotifyWatchReject::ConflictingUpdateFlags)
        );
        assert_eq!(
            plan_inotify_watch(0x2000_0000, true),
            Ok(InotifyWatchPlan::Add)
        );
        assert_eq!(
            plan_fanotify_mark(0x80, 0, 0, None),
            Ok(FanotifyMarkPlan::Flush)
        );
        assert_eq!(
            plan_fanotify_mark(1 | 8, 1, 0, Some(false)),
            Err(FanotifyMarkReject::NotDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_EVICTABLE,
                FAN_ACCESS,
                0,
                Some(false)
            ),
            Ok(FanotifyMarkPlan::Add)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, 0x2000, 0, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, 0, 0, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_FLUSH | FAN_MARK_DONT_FOLLOW, FAN_ACCESS, 0, None),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_FLUSH | FAN_MARK_MOUNT, FAN_ACCESS, 0, None),
            Ok(FanotifyMarkPlan::Flush)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_MOUNT | FAN_MARK_IGNORE,
                FAN_ACCESS,
                0,
                Some(true)
            ),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_IGNORE, FAN_ACCESS, 0, Some(true)),
            Err(FanotifyMarkReject::IsDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD,
                FAN_RENAME,
                FAN_REPORT_DFID_NAME | FAN_REPORT_FID,
                Some(false)
            ),
            Err(FanotifyMarkReject::NotDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD,
                FAN_CREATE | FAN_MOVED_FROM | FAN_MOVED_TO,
                FAN_REPORT_DFID_NAME_TARGET,
                Some(false)
            ),
            Err(FanotifyMarkReject::NotDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_REMOVE | FAN_MARK_IGNORE, FAN_ACCESS, 0, Some(true)),
            Ok(FanotifyMarkPlan::Ignored)
        );
    }

    #[test]
    fn fanotify_public_abi_preserves_uapi_values_and_wire_layout() {
        assert_eq!(FAN_OPEN_EXEC_PERM, 0x0004_0000);
        assert_eq!(FAN_MARK_EVICTABLE, 0x0000_0200);
        assert_eq!(
            FANOTIFY_RESPONSE_VALID_MASK,
            FAN_ALLOW | FAN_DENY | FAN_AUDIT | FAN_INFO | FANOTIFY_RESPONSE_ERRNO_MASK
        );
        assert_eq!(ALL_FANOTIFY_EVENT_BITS, 0x0000_0000_5807_dfff);
        assert_eq!(
            FANOTIFY_PERMISSION_CLASSES,
            [FAN_CLASS_PRE_CONTENT, FAN_CLASS_CONTENT]
        );
        assert_eq!(size_of::<FanotifyEventMetadata>(), 24);
        assert_eq!(size_of::<FanotifyEventInfoPidfd>(), 8);
        assert_eq!(size_of::<FanotifyResponse>(), 8);
    }

    #[test]
    fn fanotify_response_grammar_is_owned_by_the_abi_boundary() {
        assert_eq!(
            fanotify_response_admission(FAN_ALLOW, false, false),
            Ok(FanotifyResponsePlan::Allow)
        );
        assert_eq!(
            fanotify_response_admission(FAN_ALLOW | FAN_DENY, false, false),
            Err(FanotifyResponseReject::Invalid)
        );
        assert_eq!(
            fanotify_response_admission(FAN_DENY | FAN_AUDIT, false, false),
            Err(FanotifyResponseReject::AuditNotEnabled)
        );
        assert_eq!(
            fanotify_response_admission(FAN_DENY | FAN_INFO, true, false),
            Err(FanotifyResponseReject::InfoUnsupported)
        );
        assert_eq!(
            fanotify_response_admission(fan_deny_errno(5), false, true),
            Ok(FanotifyResponsePlan::Deny { errno: Some(5) })
        );
        assert_eq!(
            fanotify_response_admission(fan_deny_errno(5), false, false),
            Err(FanotifyResponseReject::Invalid)
        );
        assert_eq!(
            fanotify_response_admission(fan_deny_errno(2), false, true),
            Err(FanotifyResponseReject::Invalid)
        );
        assert_eq!(
            fanotify_response_admission(fan_deny_errno(5) | FAN_ALLOW, false, true),
            Err(FanotifyResponseReject::Invalid)
        );
    }
}
