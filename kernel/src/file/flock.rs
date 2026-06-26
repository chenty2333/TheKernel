use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::PollSet;
use axtask::future::{block_on, interruptible};
use linux_raw_sys::general::{F_RDLCK, F_UNLCK, F_WRLCK, SEEK_CUR, SEEK_END, SEEK_SET, flock64};
use spin::Mutex;
use starry_process::Pid;

/// Inode identity: (device, inode number).
pub(crate) type InodeId = (u64, u64);
type FlockOwner = u64;

const RECORD_EOF: u64 = u64::MAX;

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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordLockOwner {
    Posix(Pid),
    Ofd(u64),
}

#[derive(Clone, Copy)]
struct RecordRange {
    start: u64,
    end: u64,
}

#[derive(Clone, Copy)]
struct RecordLockRequest {
    ty: i16,
    range: RecordRange,
}

#[derive(Clone)]
struct RecordLock {
    owner: RecordLockOwner,
    ty: i16,
    range: RecordRange,
}

#[derive(Clone, Copy)]
struct RecordLockWait {
    id: InodeId,
    req: RecordLockRequest,
}

struct RecordLockTableInner {
    locks: BTreeMap<InodeId, Vec<RecordLock>>,
    wait_requests: BTreeMap<RecordLockOwner, RecordLockWait>,
    /// Woken whenever any record lock changes, so blocked acquirers can retry.
    waiters: PollSet,
}

static RECORD_LOCK_TABLE: Mutex<RecordLockTableInner> = Mutex::new(RecordLockTableInner {
    locks: BTreeMap::new(),
    wait_requests: BTreeMap::new(),
    waiters: PollSet::new(),
});

fn take_flock_waiters(table: &mut FlockTableInner) -> PollSet {
    core::mem::replace(&mut table.waiters, PollSet::new())
}

fn take_record_lock_waiters(table: &mut RecordLockTableInner) -> PollSet {
    core::mem::replace(&mut table.waiters, PollSet::new())
}

fn wake_waiters(waiters: Option<PollSet>) {
    if let Some(waiters) = waiters {
        waiters.wake();
    }
}

impl RecordRange {
    fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

impl RecordLockRequest {
    fn from_flock(lock: &flock64, file_size: u64, current_offset: u64) -> AxResult<Self> {
        let ty = lock.l_type;
        if ty != F_RDLCK as i16 && ty != F_WRLCK as i16 && ty != F_UNLCK as i16 {
            return Err(AxError::InvalidInput);
        }

        let base = match lock.l_whence as u32 {
            SEEK_SET => 0,
            SEEK_CUR => current_offset as i128,
            SEEK_END => file_size as i128,
            _ => return Err(AxError::InvalidInput),
        };
        let start = base + lock.l_start as i128;
        let len = lock.l_len as i128;

        let (start, end) = if len == 0 {
            (start, RECORD_EOF)
        } else if len > 0 {
            let end = start.checked_add(len).ok_or(AxError::InvalidInput)?;
            (start, end.try_into().map_err(|_| AxError::InvalidInput)?)
        } else {
            let new_start = start.checked_add(len).ok_or(AxError::InvalidInput)?;
            (
                new_start,
                start.try_into().map_err(|_| AxError::InvalidInput)?,
            )
        };

        if start < 0 {
            return Err(AxError::InvalidInput);
        }
        let start = start.try_into().map_err(|_| AxError::InvalidInput)?;
        Ok(Self {
            ty,
            range: RecordRange { start, end },
        })
    }
}

fn record_lock_conflicts(
    lock: &RecordLock,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> bool {
    if lock.owner == owner || !lock.range.overlaps(req.range) {
        return false;
    }
    lock.ty == F_WRLCK as i16 || req.ty == F_WRLCK as i16
}

fn record_lock_conflict_owners(
    table: &RecordLockTableInner,
    id: InodeId,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> BTreeSet<RecordLockOwner> {
    table
        .locks
        .get(&id)
        .into_iter()
        .flat_map(|locks| locks.iter())
        .filter(|lock| record_lock_conflicts(lock, owner, req))
        .map(|lock| lock.owner)
        .collect()
}

fn record_lock_would_deadlock(
    table: &RecordLockTableInner,
    owner: RecordLockOwner,
    blockers: &BTreeSet<RecordLockOwner>,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<RecordLockOwner> = blockers.iter().copied().collect();

    while let Some(blocker) = stack.pop() {
        if blocker == owner {
            return true;
        }
        if !matches!(blocker, RecordLockOwner::Posix(_)) {
            continue;
        }
        if !seen.insert(blocker) {
            continue;
        }
        let Some(wait) = table.wait_requests.get(&blocker) else {
            continue;
        };
        stack.extend(record_lock_conflict_owners(
            table, wait.id, blocker, wait.req,
        ));
    }

    false
}

fn split_out_range(lock: &RecordLock, range: RecordRange, out: &mut Vec<RecordLock>) {
    if !lock.range.overlaps(range) {
        out.push(lock.clone());
        return;
    }
    if lock.range.start < range.start {
        let mut left = lock.clone();
        left.range.end = range.start;
        out.push(left);
    }
    if range.end < lock.range.end {
        let mut right = lock.clone();
        right.range.start = range.end;
        out.push(right);
    }
}

fn insert_record_lock(locks: &mut Vec<RecordLock>, new_lock: RecordLock) {
    let mut updated = Vec::new();
    for lock in locks.iter() {
        if lock.owner == new_lock.owner {
            split_out_range(lock, new_lock.range, &mut updated);
        } else {
            updated.push(lock.clone());
        }
    }
    updated.push(new_lock);
    updated.sort_by_key(|lock| (lock.range.start, lock.range.end, lock.owner, lock.ty));

    let mut merged: Vec<RecordLock> = Vec::new();
    for lock in updated {
        if let Some(last) = merged.last_mut()
            && last.owner == lock.owner
            && last.ty == lock.ty
            && last.range.end >= lock.range.start
        {
            last.range.end = last.range.end.max(lock.range.end);
            continue;
        }
        merged.push(lock);
    }
    *locks = merged;
}

fn unlock_record_range(locks: &mut Vec<RecordLock>, owner: RecordLockOwner, range: RecordRange) {
    let mut updated = Vec::new();
    for lock in locks.iter() {
        if lock.owner == owner {
            split_out_range(lock, range, &mut updated);
        } else {
            updated.push(lock.clone());
        }
    }
    *locks = updated;
}

fn try_set_record_lock_inner(
    table: &mut RecordLockTableInner,
    id: InodeId,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> bool {
    if req.ty != F_UNLCK as i16 && !record_lock_conflict_owners(table, id, owner, req).is_empty() {
        return false;
    }

    table.wait_requests.remove(&owner);
    let locks = table.locks.entry(id).or_default();
    if req.ty == F_UNLCK as i16 {
        unlock_record_range(locks, owner, req.range);
    } else {
        insert_record_lock(
            locks,
            RecordLock {
                owner,
                ty: req.ty,
                range: req.range,
            },
        );
    }

    if locks.is_empty() {
        table.locks.remove(&id);
    }
    true
}

fn try_set_record_lock(id: InodeId, owner: RecordLockOwner, req: RecordLockRequest) -> bool {
    let (ok, waiters) = {
        let mut table = RECORD_LOCK_TABLE.lock();
        let ok = try_set_record_lock_inner(&mut table, id, owner, req);
        let waiters = ok.then(|| take_record_lock_waiters(&mut table));
        (ok, waiters)
    };
    wake_waiters(waiters);
    ok
}

fn record_lock_blocking(
    id: InodeId,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> AxResult<()> {
    match block_on(interruptible(poll_fn(|cx| {
        let mut table = RECORD_LOCK_TABLE.lock();
        if try_set_record_lock_inner(&mut table, id, owner, req) {
            let waiters = take_record_lock_waiters(&mut table);
            drop(table);
            wake_waiters(Some(waiters));
            Poll::Ready(Ok(()))
        } else {
            let blockers = record_lock_conflict_owners(&table, id, owner, req);
            if matches!(owner, RecordLockOwner::Posix(_))
                && record_lock_would_deadlock(&table, owner, &blockers)
            {
                table.wait_requests.remove(&owner);
                return Poll::Ready(Err(LinuxError::EDEADLK.into()));
            }

            table
                .wait_requests
                .insert(owner, RecordLockWait { id, req });
            table.waiters.register(cx.waker());
            if try_set_record_lock_inner(&mut table, id, owner, req) {
                let waiters = take_record_lock_waiters(&mut table);
                drop(table);
                wake_waiters(Some(waiters));
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }))) {
        Ok(res) => res,
        Err(err) => {
            RECORD_LOCK_TABLE.lock().wait_requests.remove(&owner);
            Err(err.into())
        }
    }
}

pub fn set_record_lock(
    id: InodeId,
    owner: RecordLockOwner,
    file_size: u64,
    current_offset: u64,
    lock: &flock64,
    blocking: bool,
) -> AxResult<()> {
    let req = RecordLockRequest::from_flock(lock, file_size, current_offset)?;
    if blocking {
        record_lock_blocking(id, owner, req)
    } else if try_set_record_lock(id, owner, req) {
        Ok(())
    } else {
        Err(AxError::WouldBlock)
    }
}

pub fn get_record_lock(
    id: InodeId,
    owner: RecordLockOwner,
    file_size: u64,
    current_offset: u64,
    lock: &mut flock64,
) -> AxResult<()> {
    let req = RecordLockRequest::from_flock(lock, file_size, current_offset)?;
    let table = RECORD_LOCK_TABLE.lock();
    let conflict = table.locks.get(&id).and_then(|locks| {
        locks
            .iter()
            .filter(|record| record_lock_conflicts(record, owner, req))
            .min_by_key(|record| (record.range.start, record.range.end))
    });

    if let Some(conflict) = conflict {
        lock.l_type = conflict.ty;
        lock.l_whence = SEEK_SET as _;
        lock.l_start = conflict.range.start as _;
        lock.l_len = if conflict.range.end == RECORD_EOF {
            0
        } else {
            (conflict.range.end - conflict.range.start) as _
        };
        lock.l_pid = match conflict.owner {
            RecordLockOwner::Posix(pid) => pid as _,
            RecordLockOwner::Ofd(_) => -1,
        };
    } else {
        lock.l_type = F_UNLCK as _;
    }
    Ok(())
}

pub fn mandatory_write_lock_conflicts(
    id: InodeId,
    requester: RecordLockOwner,
    start: u64,
    len: u64,
) -> bool {
    let range = if len == 0 {
        RecordRange {
            start,
            end: RECORD_EOF,
        }
    } else {
        let Some(end) = start.checked_add(len) else {
            return true;
        };
        RecordRange { start, end }
    };
    let req = RecordLockRequest {
        ty: F_WRLCK as i16,
        range,
    };
    let table = RECORD_LOCK_TABLE.lock();
    !record_lock_conflict_owners(&table, id, requester, req).is_empty()
}

pub fn release_posix_owner(pid: Pid) {
    release_record_owner(RecordLockOwner::Posix(pid), None);
}

pub fn release_posix_owner_on_inode(pid: Pid, id: InodeId) {
    release_record_owner(RecordLockOwner::Posix(pid), Some(id));
}

pub fn release_ofd_owner(owner: u64) {
    release_record_owner(RecordLockOwner::Ofd(owner), None);
}

fn release_record_owner(owner: RecordLockOwner, only_id: Option<InodeId>) {
    let mut table = RECORD_LOCK_TABLE.lock();
    table.wait_requests.remove(&owner);
    let ids: Vec<InodeId> = if let Some(id) = only_id {
        Vec::from([id])
    } else {
        table.locks.keys().copied().collect()
    };

    let mut changed = false;
    for id in ids {
        let Some(locks) = table.locks.get_mut(&id) else {
            continue;
        };
        let before = locks.len();
        locks.retain(|lock| lock.owner != owner);
        changed |= locks.len() != before;
        if locks.is_empty() {
            table.locks.remove(&id);
        }
    }
    let waiters = changed.then(|| take_record_lock_waiters(&mut table));
    drop(table);
    wake_waiters(waiters);
}

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
    let (ok, waiters) = {
        let mut table = FLOCK_TABLE.lock();
        match table.locks.get_mut(&id) {
            None => {
                let mut holders = BTreeSet::new();
                holders.insert(owner);
                table.locks.insert(id, FlockState::Shared(holders));
                remember_owner_lock(&mut table, owner, id);
                (true, None)
            }
            Some(FlockState::Shared(holders)) => {
                holders.insert(owner);
                remember_owner_lock(&mut table, owner, id);
                (true, None)
            }
            Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => {
                let mut holders = BTreeSet::new();
                holders.insert(owner);
                table.locks.insert(id, FlockState::Shared(holders));
                remember_owner_lock(&mut table, owner, id);
                let waiters = take_flock_waiters(&mut table);
                (true, Some(waiters))
            }
            Some(FlockState::Exclusive(_)) => (false, None),
        }
    };
    wake_waiters(waiters);
    ok
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
    let waiters = {
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
        changed.then(|| take_flock_waiters(&mut table))
    };
    wake_waiters(waiters);
}

/// Release every flock lock owned by the given open file description.
pub fn release_owner(owner: FlockOwner) {
    let waiters = {
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
        take_flock_waiters(&mut table)
    };
    wake_waiters(Some(waiters));
}

/// Acquire a shared lock, blocking if necessary.
fn lock_shared_blocking(id: InodeId, owner: FlockOwner) -> AxResult<()> {
    match block_on(interruptible(poll_fn(|cx| {
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
    }))) {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
}

/// Acquire an exclusive lock, blocking if necessary.
fn lock_exclusive_blocking(id: InodeId, owner: FlockOwner) -> AxResult<()> {
    match block_on(interruptible(poll_fn(|cx| {
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
    }))) {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
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
