use alloc::vec::Vec;

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::PollSet;
use axsync::Mutex;
use axtask::current_may_uninit;
use hashbrown::{HashMap, HashSet};
use lazy_static::lazy_static;
use linux_raw_sys::general::{F_RDLCK, F_UNLCK, F_WRLCK, SEEK_CUR, SEEK_END, SEEK_SET, flock64};
use thekernel_linux_process_adapter::Pid;

use crate::readiness::block_on_poll_set;

/// Inode identity: (device, inode number).
pub(crate) type InodeId = (u64, u64);
type FlockOwner = u64;

const RECORD_EOF: u64 = u64::MAX;

enum FlockState {
    /// One or more open file descriptions hold shared locks.
    Shared(HashSet<FlockOwner>),
    /// Exactly one open file description holds an exclusive lock.
    Exclusive(FlockOwner),
}

struct FlockTableInner {
    locks: HashMap<InodeId, FlockState>,
    owners: HashMap<FlockOwner, HashSet<InodeId>>,
    memberships: usize,
}

lazy_static! {
    static ref FLOCK_TABLE: Mutex<FlockTableInner> = Mutex::new(FlockTableInner {
        locks: HashMap::new(),
        owners: HashMap::new(),
        memberships: 0,
    });
}
static FLOCK_WAITERS: PollSet = PollSet::new();

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordLockOwner {
    Posix(Pid),
    Ofd(u64),
}

/// Record-lock identities which one fd-backed I/O operation owns at the same
/// time. POSIX locks are process-owned while OFD locks follow the exact open
/// file description, so mandatory access must exempt both in one table scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MandatoryOwners {
    posix: RecordLockOwner,
    ofd: RecordLockOwner,
}

impl MandatoryOwners {
    pub const fn new(pid: Pid, ofd: u64) -> Self {
        Self {
            posix: RecordLockOwner::Posix(pid),
            ofd: RecordLockOwner::Ofd(ofd),
        }
    }

    fn contains(self, owner: RecordLockOwner) -> bool {
        owner == self.posix || owner == self.ofd
    }
}

/// Access type used by Linux mandatory record-lock admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MandatoryAccess {
    Read,
    Write,
}

impl MandatoryAccess {
    const fn lock_type(self) -> i16 {
        match self {
            Self::Read => F_RDLCK as i16,
            Self::Write => F_WRLCK as i16,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecordRange {
    start: u64,
    end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecordLockRequest {
    ty: i16,
    range: RecordRange,
}

/// A stable conflict description which can be rechecked after callers have
/// released their position, pipe, socket, and ordered transfer-attempt locks.
/// Its fields remain private so a waiter can only originate from a real table
/// query performed by [`mandatory_access_conflict`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MandatoryLockWait {
    id: InodeId,
    owners: MandatoryOwners,
    req: RecordLockRequest,
}

#[derive(Clone, Copy)]
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
    locks: HashMap<InodeId, Vec<RecordLock>>,
    owners: HashMap<RecordLockOwner, HashSet<InodeId>>,
    wait_requests: HashMap<RecordLockOwner, RecordLockWait>,
    record_count: usize,
}

lazy_static! {
    static ref RECORD_LOCK_TABLE: Mutex<RecordLockTableInner> = Mutex::new(RecordLockTableInner {
        locks: HashMap::new(),
        owners: HashMap::new(),
        wait_requests: HashMap::new(),
        record_count: 0,
    });
}
static RECORD_LOCK_WAITERS: PollSet = PollSet::new();

const MAX_FLOCK_MEMBERSHIPS: usize = 65_536;
const MAX_RECORD_LOCKS: usize = 65_536;
const MAX_RECORD_LOCKS_PER_INODE: usize = 256;
const MAX_RECORD_WAIT_REQUESTS: usize = 65_536;

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

fn record_lock_has_conflict(
    table: &RecordLockTableInner,
    id: InodeId,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> bool {
    table
        .locks
        .get(&id)
        .into_iter()
        .flat_map(|locks| locks.iter())
        .any(|lock| record_lock_conflicts(lock, owner, req))
}

fn mandatory_record_lock_conflicts(
    lock: &RecordLock,
    owners: MandatoryOwners,
    req: RecordLockRequest,
) -> bool {
    if owners.contains(lock.owner) || !lock.range.overlaps(req.range) {
        return false;
    }
    lock.ty == F_WRLCK as i16 || req.ty == F_WRLCK as i16
}

fn mandatory_lock_has_conflict(
    table: &RecordLockTableInner,
    id: InodeId,
    owners: MandatoryOwners,
    req: RecordLockRequest,
) -> bool {
    table
        .locks
        .get(&id)
        .into_iter()
        .flat_map(|locks| locks.iter())
        .any(|lock| mandatory_record_lock_conflicts(lock, owners, req))
}

fn record_lock_would_deadlock(
    table: &RecordLockTableInner,
    owner: RecordLockOwner,
    id: InodeId,
    req: RecordLockRequest,
) -> AxResult<bool> {
    let mut seen = HashSet::new();
    seen.try_reserve(table.wait_requests.len().min(MAX_RECORD_WAIT_REQUESTS))
        .map_err(|_| AxError::NoMemory)?;
    let mut stack = Vec::new();
    stack
        .try_reserve(table.locks.get(&id).map_or(0, |locks| locks.len()))
        .map_err(|_| AxError::NoMemory)?;
    if let Some(locks) = table.locks.get(&id) {
        for lock in locks {
            if record_lock_conflicts(lock, owner, req) {
                stack.push(lock.owner);
            }
        }
    }

    while let Some(blocker) = stack.pop() {
        if blocker == owner {
            return Ok(true);
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
        if let Some(locks) = table.locks.get(&wait.id) {
            for lock in locks {
                if !record_lock_conflicts(lock, blocker, wait.req) {
                    continue;
                }
                if stack.len() >= MAX_RECORD_WAIT_REQUESTS {
                    // Linux's deadlock detector is intentionally bounded too.
                    // Conservatively reject an over-deep dependency graph.
                    return Ok(true);
                }
                stack.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                stack.push(lock.owner);
            }
        }
    }

    Ok(false)
}

fn split_out_range(lock: &RecordLock, range: RecordRange, out: &mut Vec<RecordLock>) {
    if !lock.range.overlaps(range) {
        out.push(*lock);
        return;
    }
    if lock.range.start < range.start {
        let mut left = *lock;
        left.range.end = range.start;
        out.push(left);
    }
    if range.end < lock.range.end {
        let mut right = *lock;
        right.range.start = range.end;
        out.push(right);
    }
}

fn build_record_locks(
    locks: &[RecordLock],
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> AxResult<Vec<RecordLock>> {
    let mut updated = Vec::new();
    let capacity = locks
        .len()
        .checked_mul(2)
        .and_then(|capacity| capacity.checked_add(1))
        .ok_or(LinuxError::ENOLCK)?;
    updated
        .try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    for lock in locks {
        if lock.owner == owner {
            split_out_range(lock, req.range, &mut updated);
        } else {
            updated.push(*lock);
        }
    }
    if req.ty != F_UNLCK as i16 {
        updated.push(RecordLock {
            owner,
            ty: req.ty,
            range: req.range,
        });
        updated.sort_by_key(|lock| (lock.owner, lock.ty, lock.range.start, lock.range.end));

        // Merge in place so the admitted temporary vector is the only
        // allocation needed for this transaction.
        let mut written = 0;
        for read in 0..updated.len() {
            let lock = updated[read];
            if written != 0 {
                let last = &mut updated[written - 1];
                if last.owner == lock.owner
                    && last.ty == lock.ty
                    && last.range.end >= lock.range.start
                {
                    last.range.end = last.range.end.max(lock.range.end);
                    continue;
                }
            }
            updated[written] = lock;
            written += 1;
        }
        updated.truncate(written);
        updated.sort_by_key(|lock| (lock.range.start, lock.range.end, lock.owner, lock.ty));
    }
    Ok(updated)
}

#[derive(Default)]
struct RetiredRecordStorage {
    locks: Option<Vec<RecordLock>>,
    empty_locks: Option<Vec<RecordLock>>,
    owner_ids: Option<HashSet<InodeId>>,
}

fn try_set_record_lock_inner(
    table: &mut RecordLockTableInner,
    id: InodeId,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> AxResult<(bool, RetiredRecordStorage)> {
    if req.ty != F_UNLCK as i16 && record_lock_has_conflict(table, id, owner, req) {
        return Ok((false, RetiredRecordStorage::default()));
    }

    let old_locks = table.locks.get(&id).map(Vec::as_slice).unwrap_or(&[]);
    let had_owner = old_locks.iter().any(|lock| lock.owner == owner);
    let updated = build_record_locks(old_locks, owner, req)?;
    if updated.len() > MAX_RECORD_LOCKS_PER_INODE {
        return Err(LinuxError::ENOLCK.into());
    }
    let new_count = table
        .record_count
        .checked_sub(old_locks.len())
        .and_then(|count| count.checked_add(updated.len()))
        .ok_or(LinuxError::ENOLCK)?;
    if new_count > MAX_RECORD_LOCKS {
        return Err(LinuxError::ENOLCK.into());
    }

    let has_owner = updated.iter().any(|lock| lock.owner == owner);
    let mut new_owner_ids = None;
    if !had_owner && has_owner {
        if let Some(ids) = table.owners.get_mut(&owner) {
            ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        } else {
            table.owners.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            let mut ids = HashSet::new();
            ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            new_owner_ids = Some(ids);
        }
    }
    if !updated.is_empty() && !table.locks.contains_key(&id) {
        table.locks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    }

    if !had_owner && has_owner {
        if let Some(ids) = new_owner_ids {
            table.owners.insert(owner, ids);
        }
        let Some(ids) = table.owners.get_mut(&owner) else {
            return Err(AxError::BadState);
        };
        ids.insert(id);
    }

    let mut retired = RetiredRecordStorage::default();
    if updated.is_empty() {
        retired.empty_locks = table.locks.remove(&id);
    } else {
        retired.locks = table.locks.insert(id, updated);
    }
    table.record_count = new_count;
    table.wait_requests.remove(&owner);

    if had_owner && !has_owner {
        let empty = if let Some(ids) = table.owners.get_mut(&owner) {
            ids.remove(&id);
            ids.is_empty()
        } else {
            false
        };
        if empty {
            retired.owner_ids = table.owners.remove(&owner);
        }
    }
    Ok((true, retired))
}

fn try_set_record_lock(
    id: InodeId,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> AxResult<bool> {
    let (ok, retired) = {
        let mut table = RECORD_LOCK_TABLE.lock();
        try_set_record_lock_inner(&mut table, id, owner, req)?
    };
    drop(retired);
    if ok {
        RECORD_LOCK_WAITERS.wake();
    }
    Ok(ok)
}

fn record_lock_blocking(
    id: InodeId,
    owner: RecordLockOwner,
    req: RecordLockRequest,
) -> AxResult<()> {
    let result = block_on_poll_set(&RECORD_LOCK_WAITERS, || {
        match try_set_record_lock(id, owner, req) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(error),
        }

        let admission: AxResult<()> = (|| {
            let mut table = RECORD_LOCK_TABLE.lock();
            if matches!(owner, RecordLockOwner::Posix(_)) {
                match record_lock_would_deadlock(&table, owner, id, req) {
                    Ok(true) => {
                        table.wait_requests.remove(&owner);
                        return Err(LinuxError::EDEADLK.into());
                    }
                    Ok(false) => {}
                    Err(error) => return Err(error),
                }
            }
            if !table.wait_requests.contains_key(&owner) {
                if table.wait_requests.len() >= MAX_RECORD_WAIT_REQUESTS {
                    return Err(LinuxError::ENOLCK.into());
                }
                table
                    .wait_requests
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
            }
            table
                .wait_requests
                .insert(owner, RecordLockWait { id, req });
            Ok(())
        })();
        admission?;
        Err(AxError::WouldBlock)
    });
    if result.is_err() {
        let _ = RECORD_LOCK_TABLE.lock().wait_requests.remove(&owner);
    }
    result
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
    } else {
        match try_set_record_lock(id, owner, req)? {
            true => Ok(()),
            false => Err(AxError::WouldBlock),
        }
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

/// Checks one non-empty fd-backed I/O range for mandatory lock conflicts.
///
/// The range is half-open (`start..start + len`), unlike `struct flock` where
/// a zero length means through EOF. Consequently zero-length I/O is admitted
/// without consulting the lock table. A conflict returns a typed token which
/// can be passed to [`wait_for_mandatory_access`] only after higher-level I/O
/// transaction locks have been released.
pub fn mandatory_access_conflict(
    id: InodeId,
    owners: MandatoryOwners,
    access: MandatoryAccess,
    start: u64,
    len: u64,
) -> AxResult<Option<MandatoryLockWait>> {
    if len == 0 {
        return Ok(None);
    }
    start.checked_add(len).ok_or(AxError::InvalidInput)?;
    let table = RECORD_LOCK_TABLE.lock();
    mandatory_access_conflict_in(&table, id, owners, access, start, len)
}

fn mandatory_access_conflict_in(
    table: &RecordLockTableInner,
    id: InodeId,
    owners: MandatoryOwners,
    access: MandatoryAccess,
    start: u64,
    len: u64,
) -> AxResult<Option<MandatoryLockWait>> {
    if len == 0 {
        return Ok(None);
    }
    let end = start.checked_add(len).ok_or(AxError::InvalidInput)?;
    let req = RecordLockRequest {
        ty: access.lock_type(),
        range: RecordRange { start, end },
    };
    Ok(
        mandatory_lock_has_conflict(table, id, owners, req).then_some(MandatoryLockWait {
            id,
            owners,
            req,
        }),
    )
}

fn recheck_mandatory_access(wait: MandatoryLockWait) -> AxResult<()> {
    let table = RECORD_LOCK_TABLE.lock();
    recheck_mandatory_access_in(&table, wait)
}

fn recheck_mandatory_access_in(
    table: &RecordLockTableInner,
    wait: MandatoryLockWait,
) -> AxResult<()> {
    if mandatory_lock_has_conflict(table, wait.id, wait.owners, wait.req) {
        Err(AxError::WouldBlock)
    } else {
        Ok(())
    }
}

/// Interruptibly waits until the conflict represented by `wait` is gone.
///
/// `block_on_poll_set` supplies the check-arm-check protocol, so an unlock
/// between the original query and waiter registration cannot be lost. This
/// access waiter deliberately does not enter `wait_requests`; that graph is
/// reserved for record-lock acquisition and POSIX deadlock detection.
pub fn wait_for_mandatory_access(wait: MandatoryLockWait) -> AxResult<()> {
    block_on_poll_set(&RECORD_LOCK_WAITERS, || recheck_mandatory_access(wait))
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
    record_lock_has_conflict(&table, id, requester, req)
}

pub fn release_posix_owner(pid: Pid) {
    while !release_record_owner_batch(RecordLockOwner::Posix(pid), 16) {
        if current_may_uninit().is_some() {
            axtask::yield_now();
        }
    }
}

pub fn release_posix_owner_on_inode(pid: Pid, id: InodeId) {
    release_record_owner_on_inode(RecordLockOwner::Posix(pid), id);
}

struct RetiredRecordRelease {
    locks: Option<Vec<RecordLock>>,
    owner_ids: Option<HashSet<InodeId>>,
    wait: Option<RecordLockWait>,
}

fn release_record_owner_on_inode(owner: RecordLockOwner, id: InodeId) {
    let (changed, retired) = {
        let mut table = RECORD_LOCK_TABLE.lock();
        let wait = table.wait_requests.remove(&owner);
        let Some(locks) = table.locks.get_mut(&id) else {
            return;
        };
        let (removed, empty_locks) = {
            let before = locks.len();
            locks.retain(|lock| lock.owner != owner);
            (before - locks.len(), locks.is_empty())
        };
        table.record_count -= removed;
        let locks = empty_locks.then(|| table.locks.remove(&id)).flatten();

        let empty_owner = if let Some(ids) = table.owners.get_mut(&owner) {
            ids.remove(&id);
            ids.is_empty()
        } else {
            false
        };
        let owner_ids = empty_owner.then(|| table.owners.remove(&owner)).flatten();
        (
            removed != 0,
            RetiredRecordRelease {
                locks,
                owner_ids,
                wait,
            },
        )
    };
    let RetiredRecordRelease {
        locks,
        owner_ids,
        wait,
    } = retired;
    drop((locks, owner_ids));
    let _ = wait;
    if changed {
        RECORD_LOCK_WAITERS.wake();
    }
}

fn release_record_owner_batch(owner: RecordLockOwner, budget: usize) -> bool {
    let mut changed = false;
    for _ in 0..budget.max(1) {
        let (done, removed, retired) = {
            let mut table = RECORD_LOCK_TABLE.lock();
            let wait = table.wait_requests.remove(&owner);
            let id = table
                .owners
                .get(&owner)
                .and_then(|ids| ids.iter().next().copied());
            if let Some(id) = id {
                let mut removed = 0;
                let empty_locks = if let Some(locks) = table.locks.get_mut(&id) {
                    let before = locks.len();
                    locks.retain(|lock| lock.owner != owner);
                    removed = before - locks.len();
                    locks.is_empty()
                } else {
                    false
                };
                table.record_count -= removed;
                let locks = empty_locks.then(|| table.locks.remove(&id)).flatten();

                let empty_owner = if let Some(ids) = table.owners.get_mut(&owner) {
                    ids.remove(&id);
                    ids.is_empty()
                } else {
                    true
                };
                let owner_ids = empty_owner.then(|| table.owners.remove(&owner)).flatten();
                (
                    empty_owner,
                    removed != 0,
                    RetiredRecordRelease {
                        locks,
                        owner_ids,
                        wait,
                    },
                )
            } else {
                let owner_ids = table.owners.remove(&owner);
                (
                    true,
                    false,
                    RetiredRecordRelease {
                        locks: None,
                        owner_ids,
                        wait,
                    },
                )
            }
        };
        changed |= removed;
        let RetiredRecordRelease {
            locks,
            owner_ids,
            wait,
        } = retired;
        drop((locks, owner_ids));
        let _ = wait;
        if done {
            if changed {
                RECORD_LOCK_WAITERS.wake();
            }
            return true;
        }
    }
    if changed {
        RECORD_LOCK_WAITERS.wake();
    }
    false
}

/// Removes a fixed number of inode memberships for a final OFD.  It performs
/// no allocation and drops detached Vec/HashSet storage after releasing the
/// table mutex.
pub(crate) fn release_ofd_owner_batch(owner: u64, budget: usize) -> bool {
    release_record_owner_batch(RecordLockOwner::Ofd(owner), budget)
}

fn prepare_owner_membership(
    table: &mut FlockTableInner,
    owner: FlockOwner,
) -> AxResult<Option<HashSet<InodeId>>> {
    if let Some(ids) = table.owners.get_mut(&owner) {
        ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        Ok(None)
    } else {
        table.owners.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        let mut ids = HashSet::new();
        ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        Ok(Some(ids))
    }
}

fn commit_owner_membership(
    table: &mut FlockTableInner,
    owner: FlockOwner,
    id: InodeId,
    new_ids: Option<HashSet<InodeId>>,
) -> AxResult<()> {
    if let Some(ids) = new_ids {
        table.owners.insert(owner, ids);
    }
    table
        .owners
        .get_mut(&owner)
        .ok_or(AxError::BadState)?
        .insert(id);
    table.memberships += 1;
    Ok(())
}

/// Attempt to acquire a shared lock. Returns `true` on success.
fn try_lock_shared(id: InodeId, owner: FlockOwner) -> AxResult<bool> {
    let (ok, changed, retired) = {
        let mut table = FLOCK_TABLE.lock();
        match table.locks.get(&id) {
            None => {
                if table.memberships >= MAX_FLOCK_MEMBERSHIPS {
                    return Err(LinuxError::ENOLCK.into());
                }
                table.locks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                let new_ids = prepare_owner_membership(&mut table, owner)?;
                let mut holders = HashSet::new();
                holders.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                holders.insert(owner);
                commit_owner_membership(&mut table, owner, id, new_ids)?;
                table.locks.insert(id, FlockState::Shared(holders));
                (true, true, None)
            }
            Some(FlockState::Shared(holders)) if holders.contains(&owner) => (true, false, None),
            Some(FlockState::Shared(_)) => {
                if table.memberships >= MAX_FLOCK_MEMBERSHIPS {
                    return Err(LinuxError::ENOLCK.into());
                }
                table
                    .locks
                    .get_mut(&id)
                    .and_then(|state| match state {
                        FlockState::Shared(holders) => Some(holders),
                        FlockState::Exclusive(_) => None,
                    })
                    .ok_or(AxError::BadState)?
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
                let new_ids = prepare_owner_membership(&mut table, owner)?;
                commit_owner_membership(&mut table, owner, id, new_ids)?;
                let Some(FlockState::Shared(holders)) = table.locks.get_mut(&id) else {
                    return Err(AxError::BadState);
                };
                holders.insert(owner);
                (true, true, None)
            }
            Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => {
                let mut holders = HashSet::new();
                holders.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                holders.insert(owner);
                let retired = table.locks.insert(id, FlockState::Shared(holders));
                (true, true, retired)
            }
            Some(FlockState::Exclusive(_)) => (false, false, None),
        }
    };
    drop(retired);
    if changed {
        FLOCK_WAITERS.wake();
    }
    Ok(ok)
}

/// Attempt to acquire an exclusive lock. Returns `true` on success.
fn try_lock_exclusive(id: InodeId, owner: FlockOwner) -> AxResult<bool> {
    let (ok, changed, retired) = {
        let mut table = FLOCK_TABLE.lock();
        match table.locks.get(&id) {
            None => {
                if table.memberships >= MAX_FLOCK_MEMBERSHIPS {
                    return Err(LinuxError::ENOLCK.into());
                }
                table.locks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                let new_ids = prepare_owner_membership(&mut table, owner)?;
                commit_owner_membership(&mut table, owner, id, new_ids)?;
                table.locks.insert(id, FlockState::Exclusive(owner));
                (true, true, None)
            }
            Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => {
                (true, false, None)
            }
            Some(FlockState::Shared(holders)) if holders.len() == 1 && holders.contains(&owner) => {
                let retired = table.locks.insert(id, FlockState::Exclusive(owner));
                (true, true, retired)
            }
            _ => (false, false, None),
        }
    };
    drop(retired);
    if changed {
        FLOCK_WAITERS.wake();
    }
    Ok(ok)
}

/// Release the lock held by `owner` on the given inode.
pub fn flock_unlock(id: InodeId, owner: FlockOwner) {
    let (changed, retired_state, retired_ids) = {
        let mut table = FLOCK_TABLE.lock();
        let (changed, should_remove) = match table.locks.get_mut(&id) {
            Some(FlockState::Shared(holders)) => {
                let changed = holders.remove(&owner);
                (changed, holders.is_empty())
            }
            Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => (true, true),
            _ => (false, false),
        };
        let empty_owner = if changed {
            table.memberships -= 1;
            if let Some(ids) = table.owners.get_mut(&owner) {
                ids.remove(&id);
                ids.is_empty()
            } else {
                false
            }
        } else {
            false
        };
        let retired_ids = empty_owner.then(|| table.owners.remove(&owner)).flatten();
        let retired_state = should_remove.then(|| table.locks.remove(&id)).flatten();
        (changed, retired_state, retired_ids)
    };
    drop((retired_state, retired_ids));
    if changed {
        FLOCK_WAITERS.wake();
    }
}

/// Releases at most `budget` flock memberships for one final OFD.  Detached
/// HashSet/FlockState allocations are destroyed only after the table lock is
/// released.
pub(crate) fn release_owner_batch(owner: FlockOwner, budget: usize) -> bool {
    let mut changed = false;
    for _ in 0..budget.max(1) {
        let (done, removed, retired_state, retired_ids) = {
            let mut table = FLOCK_TABLE.lock();
            let id = table
                .owners
                .get(&owner)
                .and_then(|ids| ids.iter().next().copied());
            if let Some(id) = id {
                let should_remove = match table.locks.get_mut(&id) {
                    Some(FlockState::Shared(holders)) => {
                        holders.remove(&owner);
                        holders.is_empty()
                    }
                    Some(FlockState::Exclusive(current_owner)) if *current_owner == owner => true,
                    _ => false,
                };
                let removed = if let Some(ids) = table.owners.get_mut(&owner) {
                    ids.remove(&id)
                } else {
                    false
                };
                if removed {
                    table.memberships -= 1;
                }
                let empty_owner = table.owners.get(&owner).is_none_or(HashSet::is_empty);
                let retired_ids = empty_owner.then(|| table.owners.remove(&owner)).flatten();
                let retired_state = should_remove.then(|| table.locks.remove(&id)).flatten();
                (empty_owner, removed, retired_state, retired_ids)
            } else {
                let retired_ids = table.owners.remove(&owner);
                (true, false, None, retired_ids)
            }
        };
        changed |= removed;
        drop((retired_state, retired_ids));
        if done {
            if changed {
                FLOCK_WAITERS.wake();
            }
            return true;
        }
    }
    if changed {
        FLOCK_WAITERS.wake();
    }
    false
}

/// Acquire a shared lock, blocking if necessary.
fn lock_shared_blocking(id: InodeId, owner: FlockOwner) -> AxResult<()> {
    block_on_poll_set(&FLOCK_WAITERS, || {
        match try_lock_shared(id, owner) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
        Err(AxError::WouldBlock)
    })
}

/// Acquire an exclusive lock, blocking if necessary.
fn lock_exclusive_blocking(id: InodeId, owner: FlockOwner) -> AxResult<()> {
    block_on_poll_set(&FLOCK_WAITERS, || {
        match try_lock_exclusive(id, owner) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
        Err(AxError::WouldBlock)
    })
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
                if try_lock_shared(id, owner)? {
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
                if try_lock_exclusive(id, owner)? {
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

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    fn test_record_lock(owner: RecordLockOwner, ty: u32, start: u64, end: u64) -> RecordLock {
        RecordLock {
            owner,
            ty: ty as _,
            range: RecordRange { start, end },
        }
    }

    fn test_record_table(id: InodeId, locks: Vec<RecordLock>) -> RecordLockTableInner {
        let record_count = locks.len();
        let mut table = RecordLockTableInner {
            locks: HashMap::new(),
            owners: HashMap::new(),
            wait_requests: HashMap::new(),
            record_count,
        };
        assert!(table.locks.insert(id, locks).is_none());
        table
    }

    fn range_lock(ty: u32, start: i64, len: i64) -> flock64 {
        flock64 {
            l_type: ty as _,
            l_whence: SEEK_SET as _,
            l_start: start,
            l_len: len,
            l_pid: 0,
        }
    }

    fn whole_file_write_lock() -> flock64 {
        range_lock(F_WRLCK, 0, 0)
    }

    #[test]
    fn mandatory_access_exempts_posix_and_endpoint_ofd_together() {
        const PID: Pid = u32::MAX - 100;
        const OFD: u64 = u64::MAX - 300;
        const OTHER_OFD: u64 = u64::MAX - 301;
        const DEVICE: u64 = u64::MAX - 302;
        const ID: InodeId = (DEVICE, 1);

        let table = test_record_table(
            ID,
            Vec::from([
                test_record_lock(RecordLockOwner::Posix(PID), F_WRLCK, 0, 4),
                test_record_lock(RecordLockOwner::Ofd(OFD), F_WRLCK, 4, 8),
            ]),
        );

        assert_eq!(
            mandatory_access_conflict_in(
                &table,
                ID,
                MandatoryOwners::new(PID, OFD),
                MandatoryAccess::Read,
                0,
                8,
            ),
            Ok(None)
        );
        assert!(
            mandatory_access_conflict_in(
                &table,
                ID,
                MandatoryOwners::new(PID, OTHER_OFD),
                MandatoryAccess::Read,
                0,
                8,
            )
            .unwrap()
            .is_some()
        );
        assert!(
            mandatory_access_conflict_in(
                &table,
                ID,
                MandatoryOwners::new(PID - 1, OFD),
                MandatoryAccess::Read,
                0,
                8,
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn mandatory_access_uses_read_and_write_conflict_rules() {
        const PID: Pid = u32::MAX - 110;
        const OFD: u64 = u64::MAX - 310;
        const FOREIGN: u64 = u64::MAX - 311;
        const DEVICE: u64 = u64::MAX - 312;
        const ID: InodeId = (DEVICE, 1);
        let owners = MandatoryOwners::new(PID, OFD);

        let table = test_record_table(
            ID,
            Vec::from([
                test_record_lock(RecordLockOwner::Ofd(FOREIGN), F_RDLCK, 4, 8),
                test_record_lock(RecordLockOwner::Ofd(FOREIGN), F_WRLCK, 8, 12),
            ]),
        );
        assert_eq!(
            mandatory_access_conflict_in(&table, ID, owners, MandatoryAccess::Read, 0, 8),
            Ok(None)
        );
        assert!(
            mandatory_access_conflict_in(&table, ID, owners, MandatoryAccess::Write, 0, 8)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            mandatory_access_conflict_in(&table, ID, owners, MandatoryAccess::Write, 0, 4),
            Ok(None)
        );

        assert!(
            mandatory_access_conflict_in(&table, ID, owners, MandatoryAccess::Read, 8, 4)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            mandatory_access_conflict_in(&table, ID, owners, MandatoryAccess::Read, 12, 4),
            Ok(None)
        );
    }

    #[test]
    fn mandatory_access_range_is_empty_at_zero_and_rejects_overflow() {
        const PID: Pid = u32::MAX - 120;
        const OFD: u64 = u64::MAX - 320;
        const DEVICE: u64 = u64::MAX - 321;
        let owners = MandatoryOwners::new(PID, OFD);
        let table = test_record_table((DEVICE, 1), Vec::new());

        assert_eq!(
            mandatory_access_conflict_in(
                &table,
                (DEVICE, 1),
                owners,
                MandatoryAccess::Write,
                u64::MAX,
                0,
            ),
            Ok(None)
        );
        assert_eq!(
            mandatory_access_conflict_in(
                &table,
                (DEVICE, 1),
                owners,
                MandatoryAccess::Write,
                u64::MAX,
                1,
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn mandatory_wait_token_rechecks_the_same_conflict() {
        const PID: Pid = u32::MAX - 130;
        const OFD: u64 = u64::MAX - 330;
        const FOREIGN: u64 = u64::MAX - 331;
        const DEVICE: u64 = u64::MAX - 332;
        const ID: InodeId = (DEVICE, 1);
        let owners = MandatoryOwners::new(PID, OFD);

        let mut table = test_record_table(
            ID,
            Vec::from([test_record_lock(
                RecordLockOwner::Ofd(FOREIGN),
                F_RDLCK,
                0,
                8,
            )]),
        );
        let wait = mandatory_access_conflict_in(&table, ID, owners, MandatoryAccess::Write, 0, 8)
            .unwrap()
            .unwrap();
        assert_eq!(
            recheck_mandatory_access_in(&table, wait),
            Err(AxError::WouldBlock)
        );
        table.locks.remove(&ID);
        assert_eq!(recheck_mandatory_access_in(&table, wait), Ok(()));
    }

    #[test]
    fn ofd_record_release_is_batched_and_removes_only_its_owner() {
        // Reaches the global record-lock table through a sleepable mutex,
        // which needs a live current task.
        let _context = crate::test_support::scheduler_test_context();
        const OWNER: u64 = u64::MAX - 100;
        const OBSERVER: u64 = u64::MAX - 101;
        const DEVICE: u64 = u64::MAX - 102;
        const LOCKS: usize = 19;

        let request = whole_file_write_lock();
        for inode in 0..LOCKS as u64 {
            set_record_lock(
                (DEVICE, inode),
                RecordLockOwner::Ofd(OWNER),
                0,
                0,
                &request,
                false,
            )
            .unwrap();
        }

        assert!(!release_ofd_owner_batch(OWNER, 4));
        assert_eq!(
            RECORD_LOCK_TABLE
                .lock()
                .owners
                .get(&RecordLockOwner::Ofd(OWNER))
                .map(HashSet::len),
            Some(LOCKS - 4)
        );
        while !release_ofd_owner_batch(OWNER, 4) {}

        assert!(
            !RECORD_LOCK_TABLE
                .lock()
                .owners
                .contains_key(&RecordLockOwner::Ofd(OWNER))
        );
        for inode in 0..LOCKS as u64 {
            assert!(!mandatory_write_lock_conflicts(
                (DEVICE, inode),
                RecordLockOwner::Ofd(OBSERVER),
                0,
                0,
            ));
        }
    }

    #[test]
    fn flock_release_is_batched_and_allows_immediate_reacquire() {
        let _context = crate::test_support::scheduler_test_context();
        const OWNER: u64 = u64::MAX - 200;
        const OBSERVER: u64 = u64::MAX - 201;
        const DEVICE: u64 = u64::MAX - 202;
        const LOCKS: usize = 21;
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        const LOCK_UN: i32 = 8;

        for inode in 0..LOCKS as u64 {
            do_flock((DEVICE, inode), OWNER, LOCK_EX | LOCK_NB).unwrap();
        }
        assert!(!release_owner_batch(OWNER, 5));
        assert_eq!(
            FLOCK_TABLE.lock().owners.get(&OWNER).map(HashSet::len),
            Some(LOCKS - 5)
        );
        while !release_owner_batch(OWNER, 5) {}

        for inode in 0..LOCKS as u64 {
            do_flock((DEVICE, inode), OBSERVER, LOCK_EX | LOCK_NB).unwrap();
            do_flock((DEVICE, inode), OBSERVER, LOCK_UN).unwrap();
        }
    }
}
