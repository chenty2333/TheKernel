use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use axfs_ng_vfs::DeviceId;
use axsync::Mutex;
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

#[derive(Debug)]
pub(crate) struct PseudoInode {
    device: DeviceId,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    times: Mutex<PseudoTimes>,
}

#[derive(Debug)]
struct PseudoTimes {
    atime: Duration,
    mtime: Duration,
    ctime: Duration,
}

impl PseudoInode {
    fn new(device: DeviceId, mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            device,
            inode: NEXT_PSEUDO_INODE.fetch_add(1, Ordering::Relaxed),
            mode,
            uid,
            gid,
            times: Mutex::new(PseudoTimes {
                atime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
            }),
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

    pub(crate) const fn inode(&self) -> u64 {
        self.inode
    }

    pub(crate) fn stat(&self) -> Kstat {
        let times = self.times.lock();
        Kstat {
            dev: self.device.0,
            ino: self.inode,
            nlink: 1,
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
            blksize: 4096,
            atime: times.atime,
            mtime: times.mtime,
            ctime: times.ctime,
            ..Kstat::default()
        }
    }

    pub(crate) fn update_timestamps(
        &self,
        atime: Option<Duration>,
        mtime: Option<Duration>,
        ctime: Duration,
    ) {
        let mut times = self.times.lock();
        if let Some(atime) = atime {
            times.atime = atime;
        }
        if let Some(mtime) = mtime {
            times.mtime = mtime;
        }
        times.ctime = ctime;
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
    (thread.fsuid().into_raw(), thread.fsgid().into_raw())
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::PseudoInode;

    #[test]
    fn pseudo_inode_timestamp_publication_is_single_snapshot() {
        let inode = PseudoInode::pipe();
        inode.update_timestamps(
            Some(Duration::from_secs(11)),
            Some(Duration::from_secs(12)),
            Duration::from_secs(13),
        );
        let stat = inode.stat();
        assert_eq!(stat.atime, Duration::from_secs(11));
        assert_eq!(stat.mtime, Duration::from_secs(12));
        assert_eq!(stat.ctime, Duration::from_secs(13));
    }
}
