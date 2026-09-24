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
/// Mark a single inode: the zero-valued default mark type.
pub const FAN_MARK_INODE: u32 = 0x0000_0000;

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

/// All fanotify permission-event bits (requires
/// `CONFIG_FANOTIFY_ACCESS_PERMISSIONS` in Linux).
pub const FANOTIFY_PERM_EVENTS: u64 = FANOTIFY_CONTENT_PERM_EVENTS | FANOTIFY_PRE_CONTENT_EVENTS;
/// All ordinary fanotify event bits (`FANOTIFY_PATH_EVENTS |
/// FANOTIFY_INODE_EVENTS | FANOTIFY_ERROR_EVENTS | FANOTIFY_MOUNT_EVENTS`).
pub const FANOTIFY_EVENTS: u64 = FANOTIFY_PATH_EVENTS
    | FANOTIFY_INODE_EVENTS
    | FANOTIFY_ERROR_EVENTS
    | FANOTIFY_MOUNT_EVENTS;
/// `FANOTIFY_OUTGOING_EVENTS` — the bits that may be reported to user space.
pub const FANOTIFY_OUTGOING_EVENTS: u64 =
    FANOTIFY_EVENTS | FANOTIFY_PERM_EVENTS | FAN_Q_OVERFLOW | FAN_ONDIR;
/// `ALL_FANOTIFY_EVENT_BITS`.
pub const ALL_FANOTIFY_EVENT_BITS: u64 = FANOTIFY_OUTGOING_EVENTS | FANOTIFY_EVENT_FLAGS;
/// All file-identifier report flags.
pub const FANOTIFY_FID_BITS: u32 = FAN_REPORT_DFID_NAME_TARGET;
/// fanotify-init flags requiring elevated accounting or authority.
pub const FANOTIFY_ADMIN_INIT_FLAGS: u32 = FAN_CLASS_CONTENT
    | FAN_CLASS_PRE_CONTENT
    | FAN_REPORT_TID
    | FAN_REPORT_PIDFD
    | FAN_REPORT_FD_ERROR
    | FAN_UNLIMITED_QUEUE
    | FAN_UNLIMITED_MARKS;
/// fanotify-init flags available without elevated authority.
pub const FANOTIFY_USER_INIT_FLAGS: u32 =
    FAN_CLASS_NOTIF | FANOTIFY_FID_BITS | FAN_REPORT_MNT | FAN_CLOEXEC | FAN_NONBLOCK;
/// All recognized fanotify-init flags except `FAN_ENABLE_AUDIT`, which Linux
/// admits only under `CONFIG_AUDITSYSCALL`.
pub const FANOTIFY_INIT_FLAGS: u32 = FANOTIFY_ADMIN_INIT_FLAGS | FANOTIFY_USER_INIT_FLAGS;
/// All recognized fanotify-mark flags.
pub const FANOTIFY_MARK_FLAGS: u32 = FANOTIFY_MARK_TYPE_BITS
    | FANOTIFY_MARK_CMD_BITS
    | FANOTIFY_MARK_IGNORE_BITS
    | FAN_MARK_DONT_FOLLOW
    | FAN_MARK_ONLYDIR
    | FAN_MARK_IGNORED_SURV_MODIFY
    | FAN_MARK_EVICTABLE;
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

/// Pre-content access event (`FAN_PRE_ACCESS`).
pub const FAN_PRE_ACCESS: u64 = 0x0010_0000;
/// Mount-attach event (`FAN_MNT_ATTACH`).
pub const FAN_MNT_ATTACH: u64 = 0x0100_0000;
/// Mount-detach event (`FAN_MNT_DETACH`).
pub const FAN_MNT_DETACH: u64 = 0x0200_0000;
/// Report mount events (`FAN_REPORT_MNT`).
pub const FAN_REPORT_MNT: u32 = 0x0000_4000;
/// Report the failure errno in `event->fd` (`FAN_REPORT_FD_ERROR`).
pub const FAN_REPORT_FD_ERROR: u32 = 0x0000_2000;
/// Mark a whole mount namespace (`FAN_MARK_MNTNS == FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM`).
pub const FAN_MARK_MNTNS: u32 = 0x0000_0110;

/// `FANOTIFY_PATH_EVENTS` (include/linux/fanotify.h).
pub const FANOTIFY_PATH_EVENTS: u64 = FAN_ACCESS
    | FAN_MODIFY
    | FAN_CLOSE
    | FAN_OPEN
    | FAN_OPEN_EXEC;
/// `FANOTIFY_DIRENT_EVENTS`.
pub const FANOTIFY_DIRENT_EVENTS: u64 = FAN_MOVE | FAN_CREATE | FAN_DELETE | FAN_RENAME;
/// `FANOTIFY_CONTENT_PERM_EVENTS`.
pub const FANOTIFY_CONTENT_PERM_EVENTS: u64 =
    FAN_OPEN_PERM | FAN_OPEN_EXEC_PERM | FAN_ACCESS_PERM;
/// `FANOTIFY_PRE_CONTENT_EVENTS`.
pub const FANOTIFY_PRE_CONTENT_EVENTS: u64 = FAN_PRE_ACCESS;
/// `FANOTIFY_PERM_EVENTS` (requires `CONFIG_FANOTIFY_ACCESS_PERMISSIONS`).
/// `FANOTIFY_FD_EVENTS`.
pub const FANOTIFY_FD_EVENTS: u64 = FANOTIFY_PATH_EVENTS | FANOTIFY_PERM_EVENTS;
/// `FANOTIFY_INODE_EVENTS`.
pub const FANOTIFY_INODE_EVENTS: u64 =
    FANOTIFY_DIRENT_EVENTS | FAN_ATTRIB | FAN_MOVE_SELF | FAN_DELETE_SELF;
/// `FANOTIFY_ERROR_EVENTS`.
pub const FANOTIFY_ERROR_EVENTS: u64 = FAN_FS_ERROR;
/// `FANOTIFY_MOUNT_EVENTS`.
pub const FANOTIFY_MOUNT_EVENTS: u64 = FAN_MNT_ATTACH | FAN_MNT_DETACH;
/// `FANOTIFY_EVENT_FLAGS`.
pub const FANOTIFY_EVENT_FLAGS: u64 = FAN_EVENT_ON_CHILD | FAN_ONDIR;
/// `valid_mask` in `do_fanotify_mark()`: `FANOTIFY_EVENTS |
/// FANOTIFY_EVENT_FLAGS` plus `FANOTIFY_PERM_EVENTS` under
/// `CONFIG_FANOTIFY_ACCESS_PERMISSIONS`.  `FAN_Q_OVERFLOW` is deliberately
/// absent, so requesting it is `EINVAL` rather than an accepted mark.
pub const FANOTIFY_MARK_VALID_MASK: u64 =
    FANOTIFY_EVENTS | FANOTIFY_EVENT_FLAGS | FANOTIFY_PERM_EVENTS;
/// `FANOTIFY_DIRONLY_EVENT_BITS` — the bits rejected with `ENOTDIR` on a
/// non-directory inode mark for the strict (v5.17+) APIs.
pub const FANOTIFY_DIRONLY_EVENT_BITS: u64 =
    FANOTIFY_DIRENT_EVENTS | FAN_EVENT_ON_CHILD | FAN_ONDIR;
/// `FANOTIFY_INIT_ALL_EVENT_F_BITS` — every open flag `fanotify_init`'s
/// `event_f_flags` may carry.
pub const FANOTIFY_INIT_EVENT_F_FLAGS: u32 = 0x001C_9C03;
/// `FANOTIFY_CLASS_BITS`.
pub const FANOTIFY_CLASS_BITS: u32 = FAN_CLASS_NOTIF | FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT;
/// `FANOTIFY_MARK_TYPE_BITS`.
pub const FANOTIFY_MARK_TYPE_BITS: u32 =
    FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM | FAN_MARK_MNTNS;
/// `FANOTIFY_MARK_CMD_BITS`.
pub const FANOTIFY_MARK_CMD_BITS: u32 = FAN_MARK_ADD | FAN_MARK_REMOVE | FAN_MARK_FLUSH;
/// `FANOTIFY_MARK_IGNORE_BITS`.
pub const FANOTIFY_MARK_IGNORE_BITS: u32 = FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE;

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

impl FanotifyEventMetadata {
    /// The record's native-endian wire image, as `read(2)` delivers it.
    pub fn to_ne_bytes(&self) -> [u8; core::mem::size_of::<Self>()] {
        let mut out = [0; core::mem::size_of::<Self>()];
        out[0..4].copy_from_slice(&self.event_len.to_ne_bytes());
        out[4] = self.vers;
        out[5] = self.reserved;
        out[6..8].copy_from_slice(&self.metadata_len.to_ne_bytes());
        out[8..16].copy_from_slice(&self.mask.to_ne_bytes());
        out[16..20].copy_from_slice(&self.fd.to_ne_bytes());
        out[20..24].copy_from_slice(&self.pid.to_ne_bytes());
        out
    }
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

impl FanotifyEventInfoPidfd {
    /// The record's native-endian wire image, as `read(2)` delivers it.
    pub fn to_ne_bytes(&self) -> [u8; core::mem::size_of::<Self>()] {
        let mut out = [0; core::mem::size_of::<Self>()];
        out[0] = self.info_type;
        out[1] = self.pad;
        out[2..4].copy_from_slice(&self.len.to_ne_bytes());
        out[4..8].copy_from_slice(&self.pidfd.to_ne_bytes());
        out
    }
}

const _: () = {
    assert!(core::mem::size_of::<FanotifyEventMetadata>() == 24);
    assert!(core::mem::offset_of!(FanotifyEventMetadata, mask) == 8);
    assert!(core::mem::offset_of!(FanotifyEventMetadata, pid) == 20);
    assert!(core::mem::size_of::<FanotifyEventInfoPidfd>() == 8);
};

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
    /// The caller is not capable in the group's user namespace.
    Permission,
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
/// `do_fanotify_mark()` steps 1-6 (fs/notify/fanotify/fanotify_user.c): every
/// rejection that depends only on the two copied scalars, and therefore every
/// rejection that Linux reports *before* it looks at the fanotify descriptor:
///   if (upper_32_bits(mask)) return -EINVAL;
///   if (flags & ~FANOTIFY_MARK_FLAGS) return -EINVAL;
///   switch (mark_type) { ... default: return -EINVAL; }
///   switch (mark_cmd) { case ADD: case REMOVE: if (!mask) return -EINVAL; ...
///                       case FLUSH: if (flags & ~(TYPE_BITS | FLUSH)) return -EINVAL;
///                       default: return -EINVAL; }
///   if (mask & ~valid_mask) return -EINVAL;
///   if (ignore == (FAN_MARK_IGNORE | FAN_MARK_IGNORED_MASK)) return -EINVAL;
pub const fn fanotify_mark_scalars(flags: u32, mask: u64) -> Result<(), FanotifyMarkReject> {
    if mask >> 32 != 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    if flags & !FANOTIFY_MARK_FLAGS != 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    // The mark-type switch accepts every value of the two type bits: inode
    // (zero), mount, filesystem and `FAN_MARK_MNTNS` (both bits).  Whether the
    // mount-namespace scope is usable depends on the group and is decided
    // after the descriptor lookup.
    let mark_cmd = flags & FANOTIFY_MARK_CMD_BITS;
    match mark_cmd {
        FAN_MARK_ADD | FAN_MARK_REMOVE => {
            if mask & 0xffff_ffff == 0 {
                return Err(FanotifyMarkReject::Invalid);
            }
        }
        FAN_MARK_FLUSH => {
            if flags & !(FANOTIFY_MARK_TYPE_BITS | FAN_MARK_FLUSH) != 0 {
                return Err(FanotifyMarkReject::Invalid);
            }
        }
        _ => return Err(FanotifyMarkReject::Invalid),
    }
    // `valid_mask` excludes FAN_Q_OVERFLOW (0x4000) and the reserved
    // FAN_DIR_MODIFY bit; an unknown bit is EINVAL rather than EOPNOTSUPP.
    if mask & !FANOTIFY_MARK_VALID_MASK != 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    if flags & FANOTIFY_MARK_IGNORE_BITS == FANOTIFY_MARK_IGNORE_BITS {
        return Err(FanotifyMarkReject::Invalid);
    }
    Ok(())
}

/// `do_fanotify_mark()` from the group report-mode checks onward.  The
/// `target_is_dir` argument is `None` when the caller has no resolved object
/// (which is only reachable for `FAN_MARK_FLUSH`, short-circuited here) and
/// `Some` afterwards:
///   if (FAN_GROUP_FLAG(group, FAN_REPORT_MNT)) { ... } else {
///           if (mask & FANOTIFY_MOUNT_EVENTS) return -EINVAL;
///           if (mark_type == FAN_MARK_MNTNS) return -EINVAL; }
///   if (mask & FANOTIFY_PERM_EVENTS && group->priority == FSNOTIFY_PRIO_NORMAL) return -EINVAL;
///   else if (mask & FANOTIFY_PRE_CONTENT_EVENTS && group->priority == FSNOTIFY_PRIO_CONTENT)
///           return -EINVAL;
///   if (mask & FAN_FS_ERROR && mark_type != FAN_MARK_FILESYSTEM) return -EINVAL;
///   if (flags & FAN_MARK_EVICTABLE && mark_type != FAN_MARK_INODE) return -EINVAL;
///   if (mask & ~(FANOTIFY_FD_EVENTS|FANOTIFY_MOUNT_EVENTS|FANOTIFY_EVENT_FLAGS) &&
///       (!fid_mode || mark_type == FAN_MARK_MOUNT)) return -EINVAL;
///   if (mask & FAN_RENAME && !(fid_mode & FAN_REPORT_NAME)) return -EINVAL;
///   if (mask & FANOTIFY_PRE_CONTENT_EVENTS && mask & FAN_ONDIR) return -EINVAL;
pub const fn plan_fanotify_mark(
    flags: u32,
    mask: u64,
    group_flags: u32,
    group_admin: bool,
    target_is_dir: Option<bool>,
) -> Result<FanotifyMarkPlan, FanotifyMarkReject> {
    if let Err(reject) = fanotify_mark_scalars(flags, mask) {
        return Err(reject);
    }
    let mask = mask & 0xffff_ffff;
    let mark_type = flags & FANOTIFY_MARK_TYPE_BITS;
    // Only a group created with FAN_REPORT_MNT may carry mount events, and
    // only FAN_MARK_MNTNS may take them.  TheKernel cannot create such a
    // group, so the first branch is unreachable and the second is the rule.
    if group_flags & FAN_REPORT_MNT != 0 {
        if mask & !FANOTIFY_MOUNT_EVENTS != 0 || mark_type != FAN_MARK_MNTNS {
            return Err(FanotifyMarkReject::Invalid);
        }
    } else if mask & FANOTIFY_MOUNT_EVENTS != 0 || mark_type == FAN_MARK_MNTNS {
        return Err(FanotifyMarkReject::Invalid);
    }
    // A user is allowed to setup sb/mount/mntns marks only if it is capable in
    // the user ns where the group was created
    // (fs/notify/fanotify/fanotify_user.c:1958-1963):
    //   if (mark_type != FAN_MARK_INODE &&
    //       !ns_capable(group->user_ns, CAP_SYS_ADMIN))
    //           return -EPERM;
    // The domain is the *group's* user namespace, not the caller's: a group
    // created in the initial user namespace by a caller without
    // `CAP_SYS_ADMIN` there can never take one of these mark scopes.
    if mark_type != FAN_MARK_INODE && !group_admin {
        return Err(FanotifyMarkReject::Permission);
    }
    let class = group_flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT);
    if mask & FANOTIFY_PERM_EVENTS != 0 && class == FAN_CLASS_NOTIF {
        return Err(FanotifyMarkReject::Invalid);
    }
    if mask & FANOTIFY_PRE_CONTENT_EVENTS != 0 && class == FAN_CLASS_CONTENT {
        return Err(FanotifyMarkReject::Invalid);
    }
    if mask & FAN_FS_ERROR != 0 && mark_type != FAN_MARK_FILESYSTEM {
        return Err(FanotifyMarkReject::Invalid);
    }
    // Eviction only applies to inode marks.
    if flags & FAN_MARK_EVICTABLE != 0 && mark_type != FAN_MARK_INODE {
        return Err(FanotifyMarkReject::Invalid);
    }
    // Events that cannot carry an event fd need a group that reports file
    // identifiers, and are never valid on a mount mark.
    let fid_mode = group_flags & FANOTIFY_FID_BITS;
    if mask & !(FANOTIFY_FD_EVENTS | FANOTIFY_MOUNT_EVENTS | FANOTIFY_EVENT_FLAGS) != 0
        && (fid_mode == 0 || mark_type == FAN_MARK_MOUNT)
    {
        return Err(FanotifyMarkReject::Invalid);
    }
    if mask & FAN_RENAME != 0 && fid_mode & FAN_REPORT_NAME == 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    if mask & FANOTIFY_PRE_CONTENT_EVENTS != 0 && mask & FAN_ONDIR != 0 {
        return Err(FanotifyMarkReject::Invalid);
    }
    if flags & FAN_MARK_FLUSH != 0 {
        // `fsnotify_clear_marks_by_group(group, obj_type); return 0;`
        return Ok(FanotifyMarkPlan::Flush);
    }
    if flags & FAN_MARK_ONLYDIR != 0 && matches!(target_is_dir, Some(false)) {
        return Err(FanotifyMarkReject::NotDirectory);
    }
    if flags & FAN_MARK_ADD != 0 {
        // `fanotify_events_supported()` is called only for FAN_MARK_ADD.  Its
        // strict dir-only rejection does not apply to directory marks, to the
        // legacy APIs, or to scope marks:
        //   bool strict_dir_events = FAN_GROUP_FLAG(group, FAN_REPORT_TARGET_FID) ||
        //                            (mask & FAN_RENAME) || (flags & FAN_MARK_IGNORE);
        //   if (strict_dir_events && mark_type == FAN_MARK_INODE && !is_dir &&
        //       (mask & FANOTIFY_DIRONLY_EVENT_BITS)) return -ENOTDIR;
        let strict_dir_events = group_flags & FAN_REPORT_TARGET_FID != 0
            || mask & FAN_RENAME != 0
            || flags & FAN_MARK_IGNORE != 0;
        if strict_dir_events
            && mark_type == FAN_MARK_INODE
            && matches!(target_is_dir, Some(false))
            && mask & FANOTIFY_DIRONLY_EVENT_BITS != 0
        {
            return Err(FanotifyMarkReject::NotDirectory);
        }
    }
    // `do_fanotify_mark()` applies this rule only after the target inode is
    // known, so the descriptor precheck (target still unknown) must not answer
    // for it: `fanotify_find_path()` reports EFAULT/ENOENT/EACCES first.
    if flags & FAN_MARK_ADD != 0
        && flags & FANOTIFY_MARK_IGNORE_BITS != 0
        && flags & FAN_MARK_IGNORED_SURV_MODIFY == 0
        && target_is_dir.is_some()
    {
        // Legacy FAN_MARK_IGNORED_MASK forbids the non-inode scopes outright;
        // FAN_MARK_IGNORE additionally requires SURV_MODIFY for a directory.
        // A writable-open directory is the caller's EISDIR check.
        if mark_type != FAN_MARK_INODE {
            return Err(FanotifyMarkReject::Invalid);
        }
        if matches!(target_is_dir, Some(true)) && flags & FAN_MARK_IGNORE != 0 {
            return Err(FanotifyMarkReject::IsDirectory);
        }
    }
    Ok(if flags & FANOTIFY_MARK_IGNORE_BITS != 0 {
        FanotifyMarkPlan::Ignored
    } else if flags & FAN_MARK_ADD != 0 {
        FanotifyMarkPlan::Add
    } else {
        FanotifyMarkPlan::Remove
    })
}

/// Validates Linux `fanotify_init` flag grammar independent of FD allocation,
/// in Linux's own order (fs/notify/fanotify/fanotify_user.c):
///   if (flags & ~(FANOTIFY_INIT_FLAGS | FAN_ENABLE_AUDIT)) return -EINVAL;
///   if (flags & FAN_REPORT_MNT) { if (class != FAN_CLASS_NOTIF) return -EINVAL;
///           if (flags & (FANOTIFY_FID_BITS | FAN_REPORT_FD_ERROR)) return -EINVAL; }
///   if (event_f_flags & ~FANOTIFY_INIT_ALL_EVENT_F_BITS) return -EINVAL;
///   switch (event_f_flags & O_ACCMODE) { ... default: return -EINVAL; }
///   if (fid_mode && class != FAN_CLASS_NOTIF) return -EINVAL;
///   if ((fid_mode & FAN_REPORT_NAME) && !(fid_mode & FAN_REPORT_DIR_FID)) return -EINVAL;
///   if ((fid_mode & FAN_REPORT_TARGET_FID) &&
///       (!(fid_mode & FAN_REPORT_NAME) || !(fid_mode & FAN_REPORT_FID))) return -EINVAL;
///   switch (class) { ... default: return -EINVAL; }
pub const fn fanotify_init_grammar(
    flags: u32,
    event_flags: u32,
) -> Result<(), FanotifyInitReject> {
    // FAN_ENABLE_AUDIT is admitted here because this kernel audits responses
    // through the same capability Linux checks with CONFIG_AUDITSYSCALL.
    if flags & !(FANOTIFY_INIT_FLAGS | FAN_ENABLE_AUDIT) != 0 {
        return Err(FanotifyInitReject::Invalid);
    }
    let class = flags & FANOTIFY_CLASS_BITS;
    if flags & FAN_REPORT_MNT != 0 {
        if class != FAN_CLASS_NOTIF {
            return Err(FanotifyInitReject::Invalid);
        }
        if flags & (FANOTIFY_FID_BITS | FAN_REPORT_FD_ERROR) != 0 {
            return Err(FanotifyInitReject::Invalid);
        }
    }
    if event_flags & !FANOTIFY_INIT_EVENT_F_FLAGS != 0 {
        return Err(FanotifyInitReject::Invalid);
    }
    // O_ACCMODE == 3 is the only rejected access mode.
    if event_flags & 0x3 == 0x3 {
        return Err(FanotifyInitReject::Invalid);
    }
    let fid_mode = flags & FANOTIFY_FID_BITS;
    if fid_mode != 0 && class != FAN_CLASS_NOTIF {
        return Err(FanotifyInitReject::Invalid);
    }
    if fid_mode & FAN_REPORT_NAME != 0 && fid_mode & FAN_REPORT_DIR_FID == 0 {
        return Err(FanotifyInitReject::Invalid);
    }
    if fid_mode & FAN_REPORT_TARGET_FID != 0
        && fid_mode & (FAN_REPORT_NAME | FAN_REPORT_FID)
            != FAN_REPORT_NAME | FAN_REPORT_FID
    {
        return Err(FanotifyInitReject::Invalid);
    }
    // The class switch runs after group allocation; both class bits at once
    // select FAN_CLASS_CONTENT|FAN_CLASS_PRE_CONTENT and fall through to
    // `default: return -EINVAL;`.
    if class == FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT {
        return Err(FanotifyInitReject::Invalid);
    }
    // Every remaining grammar rule is satisfied.  Linux now finishes with the
    // FAN_ENABLE_AUDIT capability check and the unprivileged-group decisions.
    Ok(())
}

/// Facilities TheKernel has no provider for.  `FAN_REPORT_MNT` is a
/// well-formed Linux request that needs mount-event delivery, which this
/// kernel does not implement, so [`fanotify_init_admission`] reports it as
/// unsupported rather than invalid.
///
/// `FAN_REPORT_FD_ERROR`, `FAN_UNLIMITED_QUEUE` and `FAN_UNLIMITED_MARKS` are
/// deliberately *not* in this set: Linux implements all three, so answering
/// them with `EOPNOTSUPP` would be wrong.  The first two are also
/// `FANOTIFY_ADMIN_INIT_FLAGS`, so they sit behind the `CAP_SYS_ADMIN` gate
/// (fs/notify/fanotify/fanotify_user.c:1599-1611), and the unlimited budgets
/// are configured by `fanotify_init(2)` itself (":1709-1714" for the event
/// queue limit and ":1406-1443" for the mark limit).
pub const fn fanotify_init_unsupported(flags: u32) -> bool {
    flags & FAN_REPORT_MNT != 0
}

/// Validates `fanotify_init` grammar and provider availability together.  The
/// kernel entry point must interleave the `CAP_SYS_ADMIN`/`CAP_AUDIT_WRITE`
/// decisions, so it calls [`fanotify_init_grammar`] and
/// [`fanotify_init_unsupported`] separately.
pub const fn fanotify_init_admission(
    flags: u32,
    event_flags: u32,
) -> Result<(), FanotifyInitReject> {
    if let Err(reject) = fanotify_init_grammar(flags, event_flags) {
        return Err(reject);
    }
    if fanotify_init_unsupported(flags) {
        return Err(FanotifyInitReject::Unsupported);
    }
    Ok(())
}

/// The event-flag normalizations Linux applies to a mark mask before it is
/// stored:
///   * step 7: `FAN_MARK_IGNORED_MASK` cannot carry `FAN_ONDIR` or
///     `FAN_EVENT_ON_CHILD`, which are kept in a separate `umask`;
///   * step 22: `FAN_EVENT_ON_CHILD` is meaningless outside a directory mark.
pub const fn fanotify_mark_stored_mask(mask: u64, flags: u32, is_dir: bool) -> u64 {
    let mask = if flags & FANOTIFY_MARK_IGNORE_BITS == FAN_MARK_IGNORED_MASK {
        mask & !FANOTIFY_EVENT_FLAGS
    } else {
        mask
    };
    if is_dir { mask } else { mask & !FAN_EVENT_ON_CHILD }
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

    /// `do_fanotify_mark()` steps 1-6: every rejection that precedes the
    /// fanotify descriptor lookup.
    #[test]
    fn fanotify_mark_scalars_reject_before_the_descriptor() {
        assert_eq!(fanotify_mark_scalars(FAN_MARK_ADD, 1 << 32), Err(FanotifyMarkReject::Invalid));
        assert_eq!(
            fanotify_mark_scalars(FAN_MARK_ADD | 0x8000, FAN_ACCESS),
            Err(FanotifyMarkReject::Invalid)
        );
        // No command, two commands, and every FLUSH companion flag.
        assert_eq!(fanotify_mark_scalars(0, FAN_ACCESS), Err(FanotifyMarkReject::Invalid));
        assert_eq!(
            fanotify_mark_scalars(FAN_MARK_ADD | FAN_MARK_REMOVE, FAN_ACCESS),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            fanotify_mark_scalars(FAN_MARK_FLUSH | FAN_MARK_DONT_FOLLOW, 0),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            fanotify_mark_scalars(FAN_MARK_FLUSH | FAN_MARK_ONLYDIR, 0),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(fanotify_mark_scalars(FAN_MARK_ADD, 0), Err(FanotifyMarkReject::Invalid));
        assert_eq!(fanotify_mark_scalars(FAN_MARK_REMOVE, 0), Err(FanotifyMarkReject::Invalid));
        assert_eq!(fanotify_mark_scalars(FAN_MARK_FLUSH, 0), Ok(()));
        // FAN_MARK_FLUSH admits any mark type, including FAN_MARK_MNTNS; the
        // scope is then checked against the group after the fd lookup.
        assert_eq!(fanotify_mark_scalars(FAN_MARK_FLUSH | FAN_MARK_MNTNS, 0), Ok(()));
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_FLUSH | FAN_MARK_MNTNS, 0, 0, true, None),
            Err(FanotifyMarkReject::Invalid)
        );
        // FAN_Q_OVERFLOW is outside `valid_mask`.
        assert_eq!(
            fanotify_mark_scalars(FAN_MARK_ADD, FAN_Q_OVERFLOW),
            Err(FanotifyMarkReject::Invalid)
        );
        // The two ignore forms are mutually exclusive.
        assert_eq!(
            fanotify_mark_scalars(FAN_MARK_ADD | FAN_MARK_IGNORE | FAN_MARK_IGNORED_MASK, 1),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(fanotify_mark_scalars(FAN_MARK_ADD, FAN_PRE_ACCESS), Ok(()));
        assert_eq!(
            fanotify_mark_scalars(FAN_MARK_ADD | FAN_MARK_MNTNS, FAN_MNT_ATTACH),
            Ok(())
        );
    }

    /// The group-dependent half of `do_fanotify_mark()`.
    #[test]
    fn fanotify_mark_group_rules_follow_linux_priority_order() {
        let fid_group = FAN_REPORT_DFID_NAME_TARGET;
        // Mount events and mntns marks need a FAN_REPORT_MNT group.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_MNT_ATTACH, fid_group, true, Some(true)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_MNTNS, FAN_ACCESS, 0, true, Some(true)),
            Err(FanotifyMarkReject::Invalid)
        );
        // Permission events need a permission class: EINVAL, never EPERM.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_OPEN_PERM, 0, true, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_OPEN_PERM, FAN_CLASS_CONTENT, true, Some(false)),
            Ok(FanotifyMarkPlan::Add)
        );
        // Pre-content events are rejected for FAN_CLASS_CONTENT.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_PRE_ACCESS, FAN_CLASS_CONTENT, true, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_PRE_ACCESS, FAN_CLASS_PRE_CONTENT, true, Some(false)),
            Ok(FanotifyMarkPlan::Add)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD,
                FAN_PRE_ACCESS | FAN_ONDIR,
                FAN_CLASS_PRE_CONTENT, true,
                Some(true)
            ),
            Err(FanotifyMarkReject::Invalid)
        );
        // FAN_FS_ERROR is filesystem-scope only.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_FS_ERROR, fid_group, true, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
                FAN_FS_ERROR,
                fid_group, true,
                Some(true)
            ),
            Ok(FanotifyMarkPlan::Add)
        );
        // Events without an event fd need a file-identifier group, and are
        // never valid on a mount mark.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_ATTRIB, 0, true, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_ATTRIB, FAN_REPORT_FID, true, Some(false)),
            Ok(FanotifyMarkPlan::Add)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_MOUNT,
                FAN_CREATE,
                FAN_REPORT_DFID_NAME, true,
                Some(true)
            ),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_RENAME, FAN_REPORT_DFID_NAME, true, Some(true)),
            Ok(FanotifyMarkPlan::Add)
        );
        // The strict dir-only rule is ADD-only and needs a non-directory.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_RENAME, FAN_REPORT_DFID_NAME, true, Some(false)),
            Err(FanotifyMarkReject::NotDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_REMOVE, FAN_RENAME, FAN_REPORT_DFID_NAME, true, Some(false)),
            Ok(FanotifyMarkPlan::Remove)
        );
        // Evictable marks are inode marks.
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_EVICTABLE | FAN_MARK_MOUNT,
                FAN_ACCESS,
                0, true,
                Some(true)
            ),
            Err(FanotifyMarkReject::Invalid)
        );
        // Ignore masks without FAN_MARK_IGNORED_SURV_MODIFY.
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_IGNORE | FAN_MARK_MOUNT,
                FAN_ACCESS,
                0, true,
                Some(true)
            ),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_IGNORE, FAN_ACCESS, 0, true, Some(true)),
            Err(FanotifyMarkReject::IsDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_IGNORE, FAN_ACCESS, 0, true, Some(false)),
            Ok(FanotifyMarkPlan::Ignored)
        );
        // The legacy ignore API accepts a directory without SURV_MODIFY.
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_IGNORED_MASK,
                FAN_ACCESS,
                0, true,
                Some(true)
            ),
            Ok(FanotifyMarkPlan::Ignored)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_FLUSH | FAN_MARK_MOUNT, FAN_ACCESS, 0, true, None),
            Ok(FanotifyMarkPlan::Flush)
        );
    }

    /// Only `FAN_REPORT_MNT` lacks a provider in this kernel; every other
    /// recognised init flag is either implemented here or is a plain flag
    /// Linux's own `fanotify_init(2)` acts on.
    #[test]
    fn fanotify_unsupported_init_flags_are_only_mount_events() {
        assert!(fanotify_init_unsupported(FAN_REPORT_MNT));
        assert!(fanotify_init_unsupported(FAN_REPORT_MNT | FAN_CLASS_NOTIF));
        for supported in [
            FAN_REPORT_FD_ERROR,
            FAN_UNLIMITED_QUEUE,
            FAN_UNLIMITED_MARKS,
            FAN_REPORT_PIDFD,
            FAN_REPORT_TID,
            FAN_CLASS_CONTENT,
            FAN_REPORT_FID,
            0,
        ] {
            assert!(
                !fanotify_init_unsupported(supported),
                "0x{supported:x} has a provider"
            );
        }
    }

    /// `do_fanotify_mark()` gates every non-inode mark scope on
    /// `ns_capable(group->user_ns, CAP_SYS_ADMIN)` and reports `-EPERM`, while
    /// an inode mark needs no capability at all.
    #[test]
    fn fanotify_mount_mark_scopes_require_group_capability() {
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_MOUNT, FAN_ACCESS, 0, false, Some(true)),
            Err(FanotifyMarkReject::Permission)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
                FAN_ACCESS,
                0,
                false,
                Some(true)
            ),
            Err(FanotifyMarkReject::Permission)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_MOUNT, FAN_ACCESS, 0, true, Some(true)),
            Ok(FanotifyMarkPlan::Add)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_ACCESS, 0, false, Some(true)),
            Ok(FanotifyMarkPlan::Add)
        );
        // The report-mode rules still run first, and a mount mark with a bad
        // mask reports EINVAL rather than EPERM.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_MNTNS, FAN_ACCESS, 0, false, Some(true)),
            Err(FanotifyMarkReject::Invalid)
        );
    }

    /// `fanotify_init`'s ordered admission rules.
    #[test]
    fn fanotify_init_admission_matches_v7_2_3_flag_matrix() {
        assert_eq!(fanotify_init_admission(1 << 31, 0), Err(FanotifyInitReject::Invalid));
        // Both class bits: the class switch default.
        assert_eq!(
            fanotify_init_admission(FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT, 0),
            Err(FanotifyInitReject::Invalid)
        );
        // FAN_REPORT_MNT demands the notification class and no inode reports.
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_MNT | FAN_CLASS_CONTENT, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_MNT | FAN_REPORT_FID, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_MNT | FAN_REPORT_FD_ERROR, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_MNT, 0),
            Err(FanotifyInitReject::Unsupported)
        );
        // FAN_REPORT_FD_ERROR is a Linux feature behind the admin gate, and
        // FAN_UNLIMITED_QUEUE is implemented by fanotify_init(2) itself, so
        // neither is an unsupported facility: the flag table and grammar admit
        // them.
        assert_eq!(fanotify_init_admission(FAN_REPORT_FD_ERROR, 0), Ok(()));
        assert_eq!(fanotify_init_admission(FAN_UNLIMITED_QUEUE, 0), Ok(()));
        // event_f_flags outside FANOTIFY_INIT_ALL_EVENT_F_BITS, and O_ACCMODE 3.
        assert_eq!(
            fanotify_init_admission(0, 0x0020_0000),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(fanotify_init_admission(0, 0x3), Err(FanotifyInitReject::Invalid));
        assert_eq!(fanotify_init_admission(0, 0x001C_9C00), Ok(()));
        // FID modes require the notification class, NAME requires DIR_FID and
        // TARGET_FID requires both NAME and FID.
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_FID | FAN_CLASS_CONTENT, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_NAME, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_DFID_NAME | FAN_REPORT_TARGET_FID, 0),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_DFID_NAME_TARGET, 0),
            Ok(())
        );
        assert_eq!(
            fanotify_init_admission(FAN_CLASS_PRE_CONTENT | FAN_CLOEXEC, 0),
            Ok(())
        );
        assert_eq!(fanotify_init_admission(FAN_UNLIMITED_MARKS, 0), Ok(()));
        // FAN_ENABLE_AUDIT is anchored on CAP_AUDIT_WRITE in Linux, not on the
        // flag table.
        assert_eq!(fanotify_init_admission(FAN_ENABLE_AUDIT, 0), Ok(()));
        assert_eq!(
            fanotify_init_admission(FAN_PRE_ACCESS as u32, 0),
            Err(FanotifyInitReject::Invalid)
        );
    }

    #[test]
    fn fanotify_init_grammar_rejects_incompatible_reports() {
        // FAN_REPORT_PIDFD and FAN_REPORT_TID combine in v7.2.3; the second
        // selects per-thread pidfds through PIDFD_THREAD.
        assert_eq!(fanotify_init_admission(0x80 | 0x100, 0), Ok(()));
        // FAN_Q_OVERFLOW is not in `valid_mask`.
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, FAN_Q_OVERFLOW, 0, true, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(0, 0x3),
            Err(FanotifyInitReject::Invalid)
        );
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_FID | FAN_CLASS_CONTENT, 0),
            Err(FanotifyInitReject::Invalid)
        );
        // 0x10 is FAN_UNLIMITED_QUEUE, which fanotify_init(2) itself
        // implements by raising the queue budget, so it is admitted.
        assert_eq!(fanotify_init_admission(FAN_UNLIMITED_QUEUE, 0), Ok(()));
        assert_eq!(
            fanotify_init_admission(FAN_REPORT_MNT, 0),
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
            plan_fanotify_mark(0x80, 0, 0, true, None),
            Ok(FanotifyMarkPlan::Flush)
        );
        assert_eq!(
            plan_fanotify_mark(1 | 8, 1, 0, true, Some(false)),
            Err(FanotifyMarkReject::NotDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_EVICTABLE,
                FAN_ACCESS,
                0, true,
                Some(false)
            ),
            Ok(FanotifyMarkPlan::Add)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, 0x2000, 0, true, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD, 0, 0, true, Some(false)),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_FLUSH | FAN_MARK_DONT_FOLLOW, FAN_ACCESS, 0, true, None),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_FLUSH | FAN_MARK_MOUNT, FAN_ACCESS, 0, true, None),
            Ok(FanotifyMarkPlan::Flush)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD | FAN_MARK_MOUNT | FAN_MARK_IGNORE,
                FAN_ACCESS,
                0, true,
                Some(true)
            ),
            Err(FanotifyMarkReject::Invalid)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_ADD | FAN_MARK_IGNORE, FAN_ACCESS, 0, true, Some(true)),
            Err(FanotifyMarkReject::IsDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD,
                FAN_RENAME,
                FAN_REPORT_DFID_NAME | FAN_REPORT_FID, true,
                Some(false)
            ),
            Err(FanotifyMarkReject::NotDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(
                FAN_MARK_ADD,
                FAN_CREATE | FAN_MOVED_FROM | FAN_MOVED_TO,
                FAN_REPORT_DFID_NAME_TARGET, true,
                Some(false)
            ),
            Err(FanotifyMarkReject::NotDirectory)
        );
        assert_eq!(
            plan_fanotify_mark(FAN_MARK_REMOVE | FAN_MARK_IGNORE, FAN_ACCESS, 0, true, Some(true)),
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
        assert_eq!(ALL_FANOTIFY_EVENT_BITS, 0x0000_0000_5b17_dfff);
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
