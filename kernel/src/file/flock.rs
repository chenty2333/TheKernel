use alloc::collections::{BTreeMap, BTreeSet};
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use axtask::future::{block_on, interruptible};
use spin::Mutex;

/// Inode identity: (device, inode number).
pub(crate) type InodeId = (u64, u64);
type FlockOwner = u64;

enum FlockState {
    /// One or more open file descriptions hold shared locks.
    Shared(BTreeSet<FlockOwner>),
    /// Exactly one open file description holds an exclusive lock.
    Exclusive(FlockOwner),
}

struct FlockTableInner {
    locks: BTreeMap<InodeId, FlockState>,
    owners: BTreeMap<FlockOwner, BTreeSet<InodeId>>,
    /// Woken whenever any lock changes, so blocked acquirers can retry.
    waiters: PollSet,
}

static FLOCK_TABLE: Mutex<FlockTableInner> = Mutex::new(FlockTableInner {
    locks: BTreeMap::new(),
    owners: BTreeMap::new(),
    waiters: PollSet::new(),
});

fn remember_owner_lock(table: &mut FlockTableInner, owner: FlockOwner, id: InodeId) {
    table.owners.entry(owner).or_default().insert(id);
}

fn forget_owner_lock(table: &mut FlockTableInner, owner: FlockOwner, id: InodeId) {
    if let Some(locks) = table.owners.get_mut(&owner) {
        locks.remove(&id);
        if locks.is_empty() {
            table.owners.remove(&owner);
        }
    }
}

/// Attempt to acquire a shared lock. Returns `true` on success.
fn try_lock_shared(id: InodeId, owner: FlockOwner) -> bool {
    let mut table = FLOCK_TABLE.lock();
    match table.locks.get_mut(&id) {
        None => {
            let mut holders = BTreeSet::new();
            holders.insert(owner);
            table.locks.insert(id, FlockState::Shared(holders));
            remember_owner_lock(&mut table, owner, id);
            true
        }
        Some(FlockState::Shared(holders)) => {
            holders.insert(owner);
            remember_owner_lock(&mut table, owner, id);
            true
        }
        Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => {
            let mut holders = BTreeSet::new();
            holders.insert(owner);
            table.locks.insert(id, FlockState::Shared(holders));
            remember_owner_lock(&mut table, owner, id);
            table.waiters.wake();
            true
        }
        Some(FlockState::Exclusive(_)) => false,
    }
}

/// Attempt to acquire an exclusive lock. Returns `true` on success.
fn try_lock_exclusive(id: InodeId, owner: FlockOwner) -> bool {
    let mut table = FLOCK_TABLE.lock();
    match table.locks.get_mut(&id) {
        None => {
            table.locks.insert(id, FlockState::Exclusive(owner));
            remember_owner_lock(&mut table, owner, id);
            true
        }
        Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => true,
        Some(FlockState::Shared(holders)) if holders.len() == 1 && holders.contains(&owner) => {
            table.locks.insert(id, FlockState::Exclusive(owner));
            remember_owner_lock(&mut table, owner, id);
            true
        }
        _ => false,
    }
}

/// Release the lock held by `owner` on the given inode.
pub fn flock_unlock(id: InodeId, owner: FlockOwner) {
    let mut table = FLOCK_TABLE.lock();
    let (changed, should_remove) = match table.locks.get_mut(&id) {
        Some(FlockState::Shared(holders)) => {
            let changed = holders.remove(&owner);
            (changed, holders.is_empty())
        }
        Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => (true, true),
        _ => (false, false),
    };
    if changed {
        forget_owner_lock(&mut table, owner, id);
    }
    if should_remove {
        table.locks.remove(&id);
    }
    if changed {
        table.waiters.wake();
    }
}

/// Release every flock lock owned by the given open file description.
pub fn release_owner(owner: FlockOwner) {
    let mut table = FLOCK_TABLE.lock();
    let Some(owned_locks) = table.owners.remove(&owner) else {
        return;
    };

    for id in owned_locks {
        let should_remove = match table.locks.get_mut(&id) {
            Some(FlockState::Shared(holders)) => {
                holders.remove(&owner);
                holders.is_empty()
            }
            Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => true,
            _ => false,
        };
        if should_remove {
            table.locks.remove(&id);
        }
    }
    table.waiters.wake();
}

/// Acquire a shared lock, blocking if necessary.
fn lock_shared_blocking(id: InodeId, owner: FlockOwner) -> AxResult<()> {
    block_on(interruptible(poll_fn(|cx| {
        if try_lock_shared(id, owner) {
            Poll::Ready(Ok(()))
        } else {
            FLOCK_TABLE.lock().waiters.register(cx.waker());
            if try_lock_shared(id, owner) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    })))?
}

/// Acquire an exclusive lock, blocking if necessary.
fn lock_exclusive_blocking(id: InodeId, owner: FlockOwner) -> AxResult<()> {
    block_on(interruptible(poll_fn(|cx| {
        if try_lock_exclusive(id, owner) {
            Poll::Ready(Ok(()))
        } else {
            FLOCK_TABLE.lock().waiters.register(cx.waker());
            if try_lock_exclusive(id, owner) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    })))?
}

/// Perform a flock operation on the given inode identity.
///
/// `operation` uses Linux LOCK_* constants:
/// - `LOCK_SH` (1): Shared lock
/// - `LOCK_EX` (2): Exclusive lock
/// - `LOCK_UN` (8): Unlock
/// - `LOCK_NB` (4): Non-blocking (OR'd with SH or EX)
pub fn do_flock(id: InodeId, owner: FlockOwner, operation: i32) -> AxResult<()> {
    const LOCK_SH: i32 = 1;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;

    let non_blocking = (operation & LOCK_NB) != 0;
    let op = operation & !LOCK_NB;

    match op {
        LOCK_SH => {
            if non_blocking {
                if try_lock_shared(id, owner) {
                    Ok(())
                } else {
                    Err(AxError::WouldBlock)
                }
            } else {
                lock_shared_blocking(id, owner)
            }
        }
        LOCK_EX => {
            if non_blocking {
                if try_lock_exclusive(id, owner) {
                    Ok(())
                } else {
                    Err(AxError::WouldBlock)
                }
            } else {
                lock_exclusive_blocking(id, owner)
            }
        }
        LOCK_UN => {
            flock_unlock(id, owner);
            Ok(())
        }
        _ => Err(AxError::InvalidInput),
    }
}
