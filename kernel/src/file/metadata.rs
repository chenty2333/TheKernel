use core::sync::atomic::{AtomicU64, Ordering};

use axfs_ng_vfs::DeviceId;
use axtask::current_may_uninit;
use linux_raw_sys::general::{S_IFIFO, S_IFREG, S_IFSOCK};

use super::Kstat;
use crate::task::AsThread;

// Keep descriptor-only pseudo filesystems separate from VFS-assigned device
// minors. Their exact numbers are not ABI, but equality and separation are.
const ANON_INODE_DEVICE: DeviceId = DeviceId::new(0, 0x00ff_f001);
const PIPE_DEVICE: DeviceId = DeviceId::new(0, 0x00ff_f002);
const SOCKET_DEVICE: DeviceId = DeviceId::new(0, 0x00ff_f003);
const PIDFD_DEVICE: DeviceId = DeviceId::new(0, 0x00ff_f004);
const MQUEUE_DEVICE: DeviceId = DeviceId::new(0, 0x00ff_f005);

static NEXT_PSEUDO_INODE: AtomicU64 = AtomicU64::new(2);

#[derive(Clone, Copy, Debug)]
pub(crate) struct PseudoInode {
    device: DeviceId,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
}

impl PseudoInode {
    fn new(device: DeviceId, mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            device,
            inode: NEXT_PSEUDO_INODE.fetch_add(1, Ordering::Relaxed),
            mode,
            uid,
            gid,
        }
    }

    fn new_owned(device: DeviceId, mode: u32) -> Self {
        let (uid, gid) = current_fs_owner();
        Self::new(device, mode, uid, gid)
    }

    pub(crate) fn pipe() -> Self {
        Self::new_owned(PIPE_DEVICE, S_IFIFO | 0o600)
    }

    pub(crate) fn socket() -> Self {
        Self::new_owned(SOCKET_DEVICE, S_IFSOCK | 0o777)
    }

    pub(crate) fn pidfd() -> Self {
        Self::new(PIDFD_DEVICE, 0o700, 0, 0)
    }

    pub(crate) fn mqueue(mode: u32, uid: u32, gid: u32) -> Self {
        Self::new(MQUEUE_DEVICE, S_IFREG | (mode & 0o777), uid, gid)
    }

    pub(crate) const fn inode(self) -> u64 {
        self.inode
    }

    pub(crate) fn stat(self) -> Kstat {
        Kstat {
            dev: self.device.0,
            ino: self.inode,
            nlink: 1,
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
            blksize: 4096,
            ..Kstat::default()
        }
    }
}

pub(crate) fn anon_inode_stat() -> Kstat {
    Kstat {
        dev: ANON_INODE_DEVICE.0,
        ino: 1,
        nlink: 1,
        mode: 0o600,
        uid: 0,
        gid: 0,
        blksize: 4096,
        ..Kstat::default()
    }
}

fn current_fs_owner() -> (u32, u32) {
    let Some(task) = current_may_uninit() else {
        return (0, 0);
    };
    let Some(thread) = task.try_as_thread() else {
        return (0, 0);
    };
    let ids = thread.proc_data.current_cred().ids();
    (ids.fsuid, ids.fsgid)
}
