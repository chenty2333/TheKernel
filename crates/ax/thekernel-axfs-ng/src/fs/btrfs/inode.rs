use alloc::vec::Vec;

use axerrno::AxError;
use axfs_ng_vfs::{
    FileAttr, FileAttrProvider, FileLock, LockOps, NodeUserData, VfsError, VfsResult,
};
use axsync::Mutex;

/// A POSIX byte-range lock mode stored by the Btrfs inode provider.  The VFS
/// maps fcntl/OFD ownership onto `owner`; this layer only enforces range
/// conflicts, so it can be reused by NFS delegation and local locks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeLockMode {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRangeLock {
    pub owner: u64,
    pub start: u64,
    pub end: Option<u64>,
    pub mode: RangeLockMode,
}

/// Native inode-side persistent state.  Attributes are not stored as a
/// private xattr: project IDs and COW extent hints live beside the inode item
/// and the eventual tree writer serialises this state into that native item.
const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
const FS_XFLAG_APPEND: u64 = 0x0000_0010;
const FS_XFLAG_SYNC: u64 = 0x0000_0020;
const FS_XFLAG_NOATIME: u64 = 0x0000_0040;
const FS_XFLAG_NODUMP: u64 = 0x0000_0080;
const FS_XFLAG_PROJINHERIT: u64 = 0x0000_0200;
const BTRFS_XFLAGS: u64 = FS_XFLAG_IMMUTABLE
    | FS_XFLAG_APPEND
    | FS_XFLAG_SYNC
    | FS_XFLAG_NOATIME
    | FS_XFLAG_NODUMP
    | FS_XFLAG_PROJINHERIT;

/// Native inode-side state shared by the VFS adapter and the transaction
/// writer.  `take_dirty_file_attr` is deliberately explicit: an adapter may
/// not report a successful attribute operation until it has serialised the
/// returned value into the inode tree in its surrounding COW transaction.
pub struct BtrfsInodeState {
    attrs: Mutex<FileAttr>,
    dirty_attrs: Mutex<bool>,
    locks: Mutex<Vec<ByteRangeLock>>,
    runtime: NodeUserData,
}

impl BtrfsInodeState {
    pub fn new(attrs: FileAttr) -> Self {
        Self {
            attrs: Mutex::new(attrs),
            dirty_attrs: Mutex::new(false),
            locks: Mutex::new(Vec::new()),
            runtime: NodeUserData::new(),
        }
    }

    pub fn persistent_user_data(&self) -> &NodeUserData {
        &self.runtime
    }

    pub fn validate_file_attr(attr: FileAttr) -> VfsResult<()> {
        if attr.nextents != 0 || attr.xflags & !BTRFS_XFLAGS != 0 {
            return Err(AxError::OperationNotSupported);
        }
        Ok(())
    }

    /// Returns a changed attribute exactly once for inclusion in an inode
    /// item.  This avoids a second, private xattr representation of project
    /// IDs or COW extent hints.
    // Writer-side attribute handoff kept for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn take_dirty_file_attr(&self) -> Option<FileAttr> {
        let mut dirty = self.dirty_attrs.lock();
        if !*dirty {
            return None;
        }
        *dirty = false;
        Some(*self.attrs.lock())
    }

    /// Marks a value already serialized by the surrounding inode-tree COW
    /// transaction.  This is distinct from `take_dirty_file_attr`: VFS file
    /// attribute operations persist synchronously and must not leave a stale
    /// dirty bit that a later unrelated fsync could publish out of order.
    pub fn mark_file_attr_persisted(&self) {
        *self.dirty_attrs.lock() = false;
    }

    pub fn lock(&self, request: ByteRangeLock, wait: bool) -> VfsResult<()> {
        if request.end.map_or(false, |end| end < request.start) {
            return Err(AxError::InvalidInput);
        }
        let mut locks = self.locks.lock();
        if locks.iter().any(|held| conflicts(*held, request)) {
            return Err(if wait {
                AxError::ResourceBusy
            } else {
                AxError::WouldBlock
            });
        }
        locks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        locks.push(request);
        Ok(())
    }

    /// Removes exactly the caller-owned lock coverage.  A different lock
    /// owner cannot unlock someone else's range, including a same-PID OFD.
    pub fn unlock(&self, owner: u64, start: u64, end: Option<u64>) -> VfsResult<()> {
        if end.map_or(false, |value| value < start) {
            return Err(AxError::InvalidInput);
        }
        let mut locks = self.locks.lock();
        let mut retained = Vec::new();
        retained
            .try_reserve(locks.len().checked_mul(2).ok_or(AxError::NoMemory)?)
            .map_err(|_| AxError::NoMemory)?;
        for lock in locks.iter().copied() {
            if lock.owner != owner || !overlaps(lock.start, lock.end, start, end) {
                retained.push(lock);
                continue;
            }
            // POSIX unlock is a range subtraction.  Keep the two remaining
            // fragments rather than releasing an unrelated byte of the OFD's
            // lock.
            if lock.start < start {
                retained.push(ByteRangeLock {
                    end: Some(start - 1),
                    ..lock
                });
            }
            let unlock_end = end.unwrap_or(u64::MAX);
            let held_end = lock.end.unwrap_or(u64::MAX);
            if unlock_end < held_end {
                retained.push(ByteRangeLock {
                    start: unlock_end + 1,
                    ..lock
                });
            }
        }
        *locks = retained;
        Ok(())
    }

    /// Releases all locks belonging to an OFD during close/exec teardown.
    // Wired up when the Btrfs VFS adapter gains POSIX lock teardown.
    #[allow(dead_code)]
    pub fn release_owner(&self, owner: u64) {
        self.locks.lock().retain(|lock| lock.owner != owner);
    }
}

impl FileAttrProvider for BtrfsInodeState {
    fn get_file_attr(&self) -> VfsResult<FileAttr> {
        Ok(*self.attrs.lock())
    }
    fn try_get_file_attr(&self) -> VfsResult<FileAttr> {
        self.attrs
            .try_lock()
            .map(|attr| *attr)
            .ok_or(VfsError::WouldBlock)
    }
    fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
        // Btrfs does not have ext4's allocated-extent setter; it is always a
        // calculated output.  Reject attempts to manufacture it.
        Self::validate_file_attr(attr)?;
        *self.attrs.lock() = attr;
        *self.dirty_attrs.lock() = true;
        Ok(())
    }
}

impl LockOps for BtrfsInodeState {
    fn get_lock(&self, owner: u64, lock: FileLock) -> VfsResult<FileLock> {
        let end = (lock.end != u64::MAX).then_some(lock.end);
        let wanted = match lock.kind {
            0 => RangeLockMode::Read,
            1 => RangeLockMode::Write,
            2 => return Ok(lock),
            _ => return Err(AxError::InvalidInput),
        };
        let request = ByteRangeLock {
            owner,
            start: lock.start,
            end,
            mode: wanted,
        };
        if let Some(held) = self
            .locks
            .lock()
            .iter()
            .copied()
            .find(|held| conflicts(*held, request))
        {
            return Ok(FileLock {
                start: held.start,
                end: held.end.unwrap_or(u64::MAX),
                kind: match held.mode {
                    RangeLockMode::Read => 0,
                    RangeLockMode::Write => 1,
                },
                pid: 0,
            });
        }
        Ok(FileLock { kind: 2, ..lock })
    }

    fn set_lock(&self, owner: u64, lock: FileLock, wait: bool) -> VfsResult<()> {
        let end = (lock.end != u64::MAX).then_some(lock.end);
        match lock.kind {
            0 => self.lock(
                ByteRangeLock {
                    owner,
                    start: lock.start,
                    end,
                    mode: RangeLockMode::Read,
                },
                wait,
            ),
            1 => self.lock(
                ByteRangeLock {
                    owner,
                    start: lock.start,
                    end,
                    mode: RangeLockMode::Write,
                },
                wait,
            ),
            2 => self.unlock(owner, lock.start, end),
            _ => Err(AxError::InvalidInput),
        }
    }
}

fn conflicts(held: ByteRangeLock, requested: ByteRangeLock) -> bool {
    held.owner != requested.owner
        && overlaps(held.start, held.end, requested.start, requested.end)
        && matches!(
            (held.mode, requested.mode),
            (RangeLockMode::Write, _) | (_, RangeLockMode::Write)
        )
}
fn overlaps(
    left_start: u64,
    left_end: Option<u64>,
    right_start: u64,
    right_end: Option<u64>,
) -> bool {
    let left_end = left_end.unwrap_or(u64::MAX);
    let right_end = right_end.unwrap_or(u64::MAX);
    left_start <= right_end && right_start <= left_end
}
