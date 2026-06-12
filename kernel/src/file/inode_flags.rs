use alloc::collections::BTreeMap;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType};
use axsync::Mutex;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::STATX_ATTR_MOUNT_ROOT;
use starry_vm::{VmMutPtr, VmPtr};

pub const FS_COMPR_FL: u32 = 0x0000_0004;
pub const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
pub const FS_APPEND_FL: u32 = 0x0000_0020;
pub const FS_NODUMP_FL: u32 = 0x0000_0040;
pub const FS_ENCRYPT_FL: u32 = 0x0000_0800;
pub const FS_VERITY_FL: u32 = 0x0010_0000;

const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
const FS_IOC_ENABLE_VERITY: u32 = 0x4080_6685;
const FS_VERITY_HASH_ALG_SHA256: u32 = 1;
const FS_VERITY_HASH_ALG_SHA512: u32 = 2;
const SETTABLE_FLAGS: u32 = FS_COMPR_FL | FS_IMMUTABLE_FL | FS_APPEND_FL | FS_NODUMP_FL;
const STATX_ATTRIBUTE_FLAGS: u32 = SETTABLE_FLAGS | FS_ENCRYPT_FL | FS_VERITY_FL;

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct FsVerityEnableArg {
    version: u32,
    hash_algorithm: u32,
    block_size: u32,
    salt_size: u32,
    salt_ptr: u64,
    sig_size: u32,
    reserved1: u32,
    sig_ptr: u64,
    reserved2: [u64; 11],
}

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
    let mut attributes = 0_u64;
    let mut attributes_mask = STATX_ATTR_MOUNT_ROOT as u64;

    if loc.is_root_of_mount() {
        attributes |= STATX_ATTR_MOUNT_ROOT as u64;
    }

    if flags_supported_on(loc) {
        let flags = flags(loc) & STATX_ATTRIBUTE_FLAGS;
        attributes |= flags as u64;
        attributes_mask |= STATX_ATTRIBUTE_FLAGS as u64;
    }

    (attributes, attributes_mask)
}

pub fn clear(loc: &Location) {
    INODE_FLAGS.lock().remove(&key(loc));
}

pub fn ioctl(loc: &Location, cmd: u32, arg: usize) -> Option<AxResult<usize>> {
    match cmd {
        FS_IOC_GETFLAGS => Some(get_flags(loc, arg)),
        FS_IOC_SETFLAGS => Some(set_flags(loc, arg)),
        FS_IOC_ENABLE_VERITY => Some(enable_verity(loc, arg)),
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
    if new_flags & !SETTABLE_FLAGS != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }

    store_flags(loc, new_flags);
    Ok(0)
}

fn enable_verity(loc: &Location, arg: usize) -> AxResult<usize> {
    match loc.node_type() {
        NodeType::RegularFile => {}
        NodeType::Directory => return Err(LinuxError::EISDIR.into()),
        _ => return Err(LinuxError::EINVAL.into()),
    }

    let enable: FsVerityEnableArg = (arg as *const FsVerityEnableArg).vm_read()?;
    if enable.version != 1
        || enable.block_size == 0
        || !enable.block_size.is_power_of_two()
        || !matches!(
            enable.hash_algorithm,
            FS_VERITY_HASH_ALG_SHA256 | FS_VERITY_HASH_ALG_SHA512
        )
        || enable.reserved1 != 0
        || enable.reserved2.iter().any(|&reserved| reserved != 0)
    {
        return Err(LinuxError::EINVAL.into());
    }

    store_flags(loc, flags(loc) | FS_VERITY_FL);
    Ok(0)
}

fn store_flags(loc: &Location, new_flags: u32) {
    let mut store = INODE_FLAGS.lock();
    let key = key(loc);
    if new_flags == 0 {
        store.remove(&key);
    } else {
        store.insert(key, new_flags);
    }
}

pub fn check_write(loc: &Location, appending: bool) -> AxResult<()> {
    let flags = flags(loc);
    if flags & (FS_IMMUTABLE_FL | FS_VERITY_FL) != 0 {
        return Err(LinuxError::EPERM.into());
    }
    if flags & FS_APPEND_FL != 0 && !appending {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

pub fn check_resize(loc: &Location) -> AxResult<()> {
    let flags = flags(loc);
    if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL | FS_VERITY_FL) != 0 {
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
