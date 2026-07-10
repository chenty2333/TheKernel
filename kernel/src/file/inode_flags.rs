use axerrno::{AxResult, LinuxError};
use axfs_ng_vfs::Location;
use linux_raw_sys::general::STATX_ATTR_MOUNT_ROOT;

const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
const FS_IOC_ENABLE_VERITY: u32 = 0x4080_6685;

pub fn same_inode(left: &Location, right: &Location) -> bool {
    left.mountpoint().device() == right.mountpoint().device() && left.inode() == right.inode()
}

pub fn statx_attributes(loc: &Location) -> (u64, u64) {
    let attribute = if loc.is_root_of_mount() {
        STATX_ATTR_MOUNT_ROOT as u64
    } else {
        0
    };
    (attribute, STATX_ATTR_MOUNT_ROOT as u64)
}

pub fn ioctl(_loc: &Location, cmd: u32, _arg: usize) -> Option<AxResult<usize>> {
    matches!(
        cmd,
        FS_IOC_GETFLAGS | FS_IOC_SETFLAGS | FS_IOC_ENABLE_VERITY
    )
    .then(|| Err(LinuxError::EOPNOTSUPP.into()))
}
