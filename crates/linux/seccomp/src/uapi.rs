//! Linux seccomp and classic-BPF UAPI constants used by the policy core.

use crate::SeccompData;

/// `seccomp()` operation: enter strict mode.
pub const SECCOMP_SET_MODE_STRICT: u32 = 0;
/// `seccomp()` operation: install a filter.
pub const SECCOMP_SET_MODE_FILTER: u32 = 1;
/// `seccomp()` operation: query an action.
pub const SECCOMP_GET_ACTION_AVAIL: u32 = 2;
/// `seccomp()` operation: query notification structure sizes.
pub const SECCOMP_GET_NOTIF_SIZES: u32 = 3;

/// `SECCOMP_IOCTL_NOTIF_RECV` request number on x86_64.
pub const SECCOMP_IOCTL_NOTIF_RECV: u32 = 0xc050_2100;
/// `SECCOMP_IOCTL_NOTIF_SEND` request number on x86_64.
pub const SECCOMP_IOCTL_NOTIF_SEND: u32 = 0xc018_2101;
/// `SECCOMP_IOCTL_NOTIF_ID_VALID` request number on x86_64.
pub const SECCOMP_IOCTL_NOTIF_ID_VALID: u32 = 0x4008_2102;
/// `SECCOMP_IOCTL_NOTIF_ADDFD` request number on x86_64.
pub const SECCOMP_IOCTL_NOTIF_ADDFD: u32 = 0x4018_2103;
/// `SECCOMP_IOCTL_NOTIF_SET_FLAGS` request number on x86_64.
pub const SECCOMP_IOCTL_NOTIF_SET_FLAGS: u32 = 0x4008_2104;

/// Continue the intercepted syscall after a notification response.
pub const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
/// Install an injected descriptor at `newfd`.
pub const SECCOMP_ADDFD_FLAG_SETFD: u32 = 1;
/// Atomically complete the notification after descriptor insertion.
pub const SECCOMP_ADDFD_FLAG_SEND: u32 = 2;
/// Synchronously wake a receiver when a notification is queued.
pub const SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP: u64 = 1;

/// Linux `struct seccomp_notif_sizes`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SeccompNotifSizes {
    /// Size of [`SeccompNotif`].
    pub seccomp_notif: u16,
    /// Size of [`SeccompNotifResp`].
    pub seccomp_notif_resp: u16,
    /// Size of [`SeccompData`].
    pub seccomp_data: u16,
}

/// Linux `struct seccomp_notif`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeccompNotif {
    /// Opaque request identifier.
    pub id: u64,
    /// Stopped task ID in its PID namespace.
    pub pid: u32,
    /// Reserved notification flags.
    pub flags: u32,
    /// Syscall register snapshot.
    pub data: SeccompData,
}

/// Linux `struct seccomp_notif_resp`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SeccompNotifResp {
    /// Opaque request identifier.
    pub id: u64,
    /// Broker supplied return value.
    pub val: i64,
    /// Broker supplied negative Linux errno.
    pub error: i32,
    /// Response flags.
    pub flags: u32,
}

/// Linux `struct seccomp_notif_addfd`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SeccompNotifAddfd {
    /// Opaque request identifier.
    pub id: u64,
    /// Add-fd flags.
    pub flags: u32,
    /// Source descriptor in the supervisor.
    pub srcfd: u32,
    /// Requested target descriptor.
    pub newfd: u32,
    /// Target descriptor flags.
    pub newfd_flags: u32,
}

/// Synchronize an installed filter to eligible sibling threads.
pub const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1 << 0;
/// Request audit logging for non-allow results from this filter.
pub const SECCOMP_FILTER_FLAG_LOG: u32 = 1 << 1;
/// Disable speculative-execution mitigation for this filter installation.
pub const SECCOMP_FILTER_FLAG_SPEC_ALLOW: u32 = 1 << 2;
/// Create a seccomp user-notification listener.
pub const SECCOMP_FILTER_FLAG_NEW_LISTENER: u32 = 1 << 3;
/// Make a failed thread synchronization return `ESRCH` instead of a TID.
pub const SECCOMP_FILTER_FLAG_TSYNC_ESRCH: u32 = 1 << 4;
/// Use a killable listener wait after a user notification is received.
pub const SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV: u32 = 1 << 5;

/// All flags defined by Linux 6.12.
pub const SECCOMP_FILTER_FLAG_MASK: u32 = SECCOMP_FILTER_FLAG_TSYNC
    | SECCOMP_FILTER_FLAG_LOG
    | SECCOMP_FILTER_FLAG_SPEC_ALLOW
    | SECCOMP_FILTER_FLAG_NEW_LISTENER
    | SECCOMP_FILTER_FLAG_TSYNC_ESRCH
    | SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV;

/// Action mask including the full 16-bit action field.
pub const SECCOMP_RET_ACTION_FULL: u32 = 0xffff_0000;
/// Historical action mask without the kill-process bit.
pub const SECCOMP_RET_ACTION: u32 = 0x7fff_0000;
/// Action-specific low-order data mask.
pub const SECCOMP_RET_DATA: u32 = 0x0000_ffff;
/// Terminate the entire thread group with `SIGSYS`.
pub const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
/// Terminate the calling thread with `SIGSYS`.
pub const SECCOMP_RET_KILL_THREAD: u32 = 0x0000_0000;
/// Deliver a synchronous `SIGSYS` trap.
pub const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
/// Skip the syscall and return the low-order errno.
pub const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
/// Delegate the syscall to a user-notification listener.
pub const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
/// Report a ptrace seccomp event.
pub const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
/// Audit-log and execute the syscall.
pub const SECCOMP_RET_LOG: u32 = 0x7ffc_0000;
/// Execute the syscall.
pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

/// Maximum classic-BPF instructions in one program.
pub const BPF_MAXINSNS: usize = 4096;
/// Number of classic-BPF scratch words.
pub const BPF_MEMWORDS: usize = 16;
/// Maximum Linux v6.12 seccomp path cost in converted execution instructions.
pub const MAX_INSNS_PER_PATH: usize = 32_768;
/// Per-ancestor path penalty used by Linux when stacking filters.
pub const FILTER_PATH_PENALTY: usize = 4;

/// Linux audit architecture value for little-endian x86_64.
pub const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

/// Size of Linux `struct seccomp_data` on supported 64-bit ABIs.
pub const SECCOMP_DATA_SIZE: usize = 64;
