use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

mod msg;
mod shm;
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    ctypes::{c_ulong, c_ushort},
    general::*,
};

pub use self::{msg::*, shm::*};

static IPC_ID: AtomicI32 = AtomicI32::new(0);

fn next_ipc_id() -> i32 {
    IPC_ID.fetch_add(1, Ordering::Relaxed)
}

// IPC command constants
const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const MSG_STAT: i32 = 11;
const MSG_INFO: i32 = 12;
pub(crate) const SHM_LOCK: i32 = 11;
pub(crate) const SHM_UNLOCK: i32 = 12;
pub(crate) const SHM_STAT: i32 = 13;
pub(crate) const SHM_INFO: i32 = 14;
pub(crate) const SHM_STAT_ANY: i32 = 15;

// Permission bits
const USER_READ: c_ushort = 0o400;
const USER_WRITE: c_ushort = 0o200;
const GROUP_READ: c_ushort = 0o040;
const GROUP_WRITE: c_ushort = 0o020;
const OTHER_READ: c_ushort = 0o004;
const OTHER_WRITE: c_ushort = 0o002;
pub(crate) const SHM_DEST: u32 = 0o1000;
pub(crate) const SHM_LOCKED: u32 = 0o2000;
pub(crate) const SHMMIN: usize = 1;
const DEFAULT_SHMMAX: usize = 0xFFFF_FFFF;
const DEFAULT_SHMMNI: usize = 4096;
const DEFAULT_SHMSEG: usize = 1024;
const DEFAULT_SHMALL: usize = 0xFFFF_FFFF;

static SHM_MAX_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SHMMAX);
static SHM_MNI_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SHMMNI);
static SHM_SEG_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SHMSEG);
static SHM_ALL_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SHMALL);

pub(crate) fn shmmax_limit() -> usize {
    SHM_MAX_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn set_shmmax_limit(value: usize) {
    SHM_MAX_LIMIT.store(value.max(SHMMIN), Ordering::Relaxed);
}

pub(crate) fn shmmni_limit() -> usize {
    SHM_MNI_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn shmseg_limit() -> usize {
    SHM_SEG_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn shmall_limit() -> usize {
    SHM_ALL_LIMIT.load(Ordering::Relaxed)
}

/// Data structure used to pass permission information to IPC operations.
#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct IpcPerm {
    /// Key supplied to msgget(2)
    pub key: __kernel_key_t,
    /// Effective UID of owner
    pub uid: __kernel_uid_t,
    /// Effective GID of owner
    pub gid: __kernel_gid_t,
    /// Effective UID of creator
    pub cuid: __kernel_uid_t,
    /// Effective GID of creator
    pub cgid: __kernel_gid_t,
    /// Permissions (least significant 9 bits define access permissions)
    pub mode: c_ushort,
    /// Padding
    pub pad1: c_ushort,
    /// Sequence number
    pub seq: c_ushort,
    /// Padding
    pub pad2: c_ushort,
    /// Unused field
    pub unused0: c_ulong,
    /// Unused field
    pub unused1: c_ulong,
}

// add a helper function to check IPC permissions
fn has_ipc_permission(perm: &IpcPerm, current_uid: u32, current_gid: u32, is_write: bool) -> bool {
    // root user has all permissions
    if current_uid == 0 {
        return true;
    }

    if perm.uid == current_uid || perm.cuid == current_uid {
        (perm.mode & if is_write { USER_WRITE } else { USER_READ }) != 0
    } else if perm.gid == current_gid || perm.cgid == current_gid {
        (perm.mode & if is_write { GROUP_WRITE } else { GROUP_READ }) != 0
    } else {
        (perm.mode & if is_write { OTHER_WRITE } else { OTHER_READ }) != 0
    }
}
