use alloc::collections::BTreeMap;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType};
use axsync::Mutex;
use starry_vm::{VmMutPtr, VmPtr};

pub const FS_COMPR_FL: u32 = 0x0000_0004;
pub const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
pub const FS_APPEND_FL: u32 = 0x0000_0020;
pub const FS_NODUMP_FL: u32 = 0x0000_0040;

const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
const SUPPORTED_FLAGS: u32 = FS_COMPR_FL | FS_IMMUTABLE_FL | FS_APPEND_FL | FS_NODUMP_FL;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct InodeFlagKey {
    device: u64,
    inode: u64,
}

static INODE_FLAGS: Mutex<BTreeMap<InodeFlagKey, u32>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum TimeUpdate {
    Omit,
    Now,
    Explicit,
}

fn key(loc: &Location) -> InodeFlagKey {
    InodeFlagKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    }
}

fn flags_supported_on(loc: &Location) -> bool {
    matches!(loc.node_type(), NodeType::RegularFile | NodeType::Directory)
}

pub fn same_inode(left: &Location, right: &Location) -> bool {
    key(left) == key(right)
}

pub fn flags(loc: &Location) -> u32 {
    INODE_FLAGS.lock().get(&key(loc)).copied().unwrap_or(0)
}

pub fn statx_attributes(loc: &Location) -> (u64, u64) {
    if !flags_supported_on(loc) {
        return (0, 0);
    }
    let flags = flags(loc) & SUPPORTED_FLAGS;
    (flags as u64, SUPPORTED_FLAGS as u64)
}

pub fn clear(loc: &Location) {
    INODE_FLAGS.lock().remove(&key(loc));
}

pub fn ioctl(loc: &Location, cmd: u32, arg: usize) -> Option<AxResult<usize>> {
    match cmd {
        FS_IOC_GETFLAGS => Some(get_flags(loc, arg)),
        FS_IOC_SETFLAGS => Some(set_flags(loc, arg)),
        _ => None,
    }
}

fn get_flags(loc: &Location, arg: usize) -> AxResult<usize> {
    if !flags_supported_on(loc) {
        return Err(AxError::NotATty);
    }
    (arg as *mut u32).vm_write(flags(loc))?;
    Ok(0)
}

fn set_flags(loc: &Location, arg: usize) -> AxResult<usize> {
    if !flags_supported_on(loc) {
        return Err(AxError::NotATty);
    }
    let new_flags = (arg as *const u32).vm_read()?;
    if new_flags & !SUPPORTED_FLAGS != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }

    let mut store = INODE_FLAGS.lock();
    let key = key(loc);
    if new_flags == 0 {
        store.remove(&key);
    } else {
        store.insert(key, new_flags);
    }
    Ok(0)
}

pub fn check_write(loc: &Location, appending: bool) -> AxResult<()> {
    let flags = flags(loc);
    if flags & FS_IMMUTABLE_FL != 0 {
        return Err(LinuxError::EPERM.into());
    }
    if flags & FS_APPEND_FL != 0 && !appending {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

pub fn check_resize(loc: &Location) -> AxResult<()> {
    let flags = flags(loc);
    if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

pub fn check_remove(loc: &Location) -> AxResult<()> {
    let flags = flags(loc);
    if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

pub fn check_metadata_update(loc: &Location) -> AxResult<()> {
    let flags = flags(loc);
    if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

pub fn check_xattr_update(loc: &Location) -> AxResult<()> {
    check_metadata_update(loc)
}

pub fn check_time_update(loc: &Location, atime: TimeUpdate, mtime: TimeUpdate) -> AxResult<()> {
    let flags = flags(loc);
    if flags & FS_IMMUTABLE_FL != 0 && (atime != TimeUpdate::Omit || mtime != TimeUpdate::Omit) {
        return Err(LinuxError::EPERM.into());
    }
    if flags & FS_APPEND_FL != 0
        && !matches!(
            (atime, mtime),
            (TimeUpdate::Omit, TimeUpdate::Omit) | (TimeUpdate::Now, TimeUpdate::Now)
        )
    {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}
