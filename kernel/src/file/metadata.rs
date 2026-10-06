use core::sync::atomic::{AtomicU64, Ordering};

use axfs_ng_vfs::{DeviceId, Timestamp};
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
    times: Mutex<PseudoMetadata>,
}

#[derive(Debug)]
struct PseudoMetadata {
    mode: u32,
    uid: u32,
    gid: u32,
    atime: Timestamp,
    mtime: Timestamp,
    ctime: Timestamp,
}

impl PseudoInode {
    fn new(device: DeviceId, mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            device,
            inode: NEXT_PSEUDO_INODE.fetch_add(1, Ordering::Relaxed),
            times: Mutex::new(PseudoMetadata {
                mode,
                uid,
                gid,
                atime: Timestamp::ZERO,
                mtime: Timestamp::ZERO,
                ctime: Timestamp::ZERO,
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

    pub(crate) fn owner_uid(&self) -> u32 {
        self.times.lock().uid
    }

    pub(crate) fn stat(&self) -> Kstat {
        let times = self.times.lock();
        Kstat {
            dev: self.device.0,
            ino: self.inode,
            nlink: 1,
            mode: times.mode,
            uid: times.uid,
            gid: times.gid,
            blksize: 4096,
            atime: times.atime,
            mtime: times.mtime,
            ctime: times.ctime,
            ..Kstat::default()
        }
    }

    pub(crate) fn chmod(&self, mode: u16, ctime: Timestamp) {
        let mut metadata = self.times.lock();
        metadata.mode = (metadata.mode & !0o7777) | u32::from(mode & 0o7777);
        metadata.ctime = ctime;
    }

    pub(crate) fn chown(&self, uid: u32, gid: u32, mode: u16, ctime: Timestamp) {
        let mut metadata = self.times.lock();
        metadata.uid = uid;
        metadata.gid = gid;
        metadata.mode = (metadata.mode & !0o7777) | u32::from(mode & 0o7777);
        metadata.ctime = ctime;
    }

    pub(crate) fn update_timestamps(
        &self,
        atime: Option<Timestamp>,
        mtime: Option<Timestamp>,
        ctime: Timestamp,
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
    use axfs_ng_vfs::Timestamp;

    use super::PseudoInode;

    #[test]
    fn socket_mode_publication_preserves_identity_and_type() {
        let inode = PseudoInode::socket();
        let before = inode.stat();
        inode.chmod(0o700, Timestamp::new(19, 0));
        let after = inode.stat();
        assert_eq!(after.mode, linux_raw_sys::general::S_IFSOCK | 0o700);
        assert_eq!((after.dev, after.ino, after.uid, after.gid),
                   (before.dev, before.ino, before.uid, before.gid));
        assert_eq!(after.ctime, Timestamp::new(19, 0));
    }

    #[test]
    fn pipe_owner_and_mode_publish_in_one_inode_snapshot() {
        let inode = PseudoInode::pipe();
        let identity = inode.inode();
        inode.chown(1000, 1001, 0o640, Timestamp::new(20, 0));
        let stat = inode.stat();
        assert_eq!((stat.uid, stat.gid, stat.mode),
                   (1000, 1001, linux_raw_sys::general::S_IFIFO | 0o640));
        assert_eq!(stat.ino, identity);
        assert_eq!(stat.ctime, Timestamp::new(20, 0));
    }

    #[test]
    fn pseudo_inode_timestamp_publication_is_single_snapshot() {
        let inode = PseudoInode::pipe();
        inode.update_timestamps(
            Some(Timestamp::new(11, 0)),
            Some(Timestamp::new(12, 0)),
            Timestamp::new(13, 0),
        );
        let stat = inode.stat();
        assert_eq!(stat.atime, Timestamp::new(11, 0));
        assert_eq!(stat.mtime, Timestamp::new(12, 0));
        assert_eq!(stat.ctime, Timestamp::new(13, 0));
    }
}
