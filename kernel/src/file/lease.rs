use alloc::collections::BTreeMap;
use core::{future::poll_fn, task::Poll, time::Duration};

use axerrno::{AxError, AxResult};
use axfs::FileFlags;
use axfs_ng_vfs::{Location, NodeType};
use axhal::time::wall_time;
use axpoll::PollSet;
use axtask::{
    current,
    future::{block_on, interruptible, timeout_at},
};
use linux_raw_sys::general::{
    CAP_LEASE, F_RDLCK, F_UNLCK, F_WRLCK, O_ACCMODE, O_PATH, O_RDONLY, O_TRUNC, O_WRONLY,
};
use spin::Mutex;
use starry_signal::{SignalInfo, Signo};

use super::{FD_TABLE, File};
use crate::task::{AsThread, send_signal_to_process};

type InodeId = (u64, u64);
type LeaseOwner = u64;

const DEFAULT_LEASE_BREAK_TIME_SECS: u32 = 45;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaseType {
    Read,
    Write,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ConflictType {
    ReadOpen,
    WriteAccess,
}

struct LeaseState {
    owner: LeaseOwner,
    holder_pid: u32,
    lease_type: LeaseType,
    breaking: Option<ConflictType>,
}

struct LeaseTable {
    leases: BTreeMap<InodeId, LeaseState>,
    waiters: PollSet,
}

static LEASE_BREAK_TIME_SECS: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(DEFAULT_LEASE_BREAK_TIME_SECS);

static LEASE_TABLE: Mutex<LeaseTable> = Mutex::new(LeaseTable {
    leases: BTreeMap::new(),
    waiters: PollSet::new(),
});

fn lease_id(loc: &Location) -> InodeId {
    (loc.mountpoint().device(), loc.inode())
}

fn is_regular_file(loc: &Location) -> bool {
    loc.node_type() == NodeType::RegularFile
}

fn conflict_from_open_flags(flags: i32) -> Option<ConflictType> {
    let flags = flags as u32;
    if flags & O_PATH != 0 {
        return None;
    }
    if flags & O_TRUNC != 0 {
        return Some(ConflictType::WriteAccess);
    }
    Some(match flags & O_ACCMODE {
        O_RDONLY => ConflictType::ReadOpen,
        O_WRONLY => ConflictType::WriteAccess,
        _ => ConflictType::WriteAccess,
    })
}

fn lease_conflicts(lease_type: LeaseType, conflict: ConflictType) -> bool {
    match lease_type {
        LeaseType::Write => true,
        LeaseType::Read => conflict == ConflictType::WriteAccess,
    }
}

fn lease_from_cmd(arg: i32) -> AxResult<LeaseType> {
    match arg as u32 {
        F_RDLCK => Ok(LeaseType::Read),
        F_WRLCK => Ok(LeaseType::Write),
        _ => Err(AxError::InvalidInput),
    }
}

fn current_pid() -> u32 {
    current().as_thread().proc_data.proc.pid()
}

fn current_can_set_lease(owner_uid: u32) -> bool {
    current().try_as_thread().is_some_and(|thr| {
        thr.proc_data.fsuid() == owner_uid || thr.proc_data.has_effective_capability(CAP_LEASE)
    })
}

fn file_open_has_write(file: &File) -> bool {
    let flags = file.inner().flags();
    flags.contains(FileFlags::WRITE) || flags.contains(FileFlags::APPEND)
}

fn count_open_fds_for_location(loc: &Location) -> usize {
    let id = lease_id(loc);
    let table = FD_TABLE.read();
    table
        .ids()
        .filter(|fd| {
            table.get(*fd).is_some_and(|entry| {
                entry
                    .description
                    .inner
                    .downcast_ref::<File>()
                    .is_some_and(|file| lease_id(file.inner().location()) == id)
            })
        })
        .count()
}

fn update_breaking_state(state: &mut LeaseState, conflict: ConflictType) -> bool {
    let next = state
        .breaking
        .map_or(conflict, |current| current.max(conflict));
    if state.breaking == Some(next) {
        false
    } else {
        state.breaking = Some(next);
        true
    }
}

fn mark_breaking_lease(id: InodeId, breaker_pid: u32, conflict: ConflictType) -> Option<u32> {
    let mut table = LEASE_TABLE.lock();
    let state = table.leases.get_mut(&id)?;
    if state.holder_pid == breaker_pid || !lease_conflicts(state.lease_type, conflict) {
        return None;
    }
    update_breaking_state(state, conflict).then_some(state.holder_pid)
}

fn conflict_cleared(id: InodeId, breaker_pid: u32, conflict: ConflictType) -> bool {
    let table = LEASE_TABLE.lock();
    match table.leases.get(&id) {
        None => true,
        Some(state) if state.holder_pid == breaker_pid => true,
        Some(state) => !lease_conflicts(state.lease_type, conflict),
    }
}

fn force_break_lease(id: InodeId) {
    let mut table = LEASE_TABLE.lock();
    if table.leases.remove(&id).is_some() {
        table.waiters.wake();
    }
}

fn wait_for_conflict(id: InodeId, conflict: ConflictType) -> AxResult<()> {
    let breaker_pid = current_pid();
    let deadline = wall_time().saturating_add(Duration::from_secs(lease_break_time_secs() as u64));

    if let Some(holder_pid) = mark_breaking_lease(id, breaker_pid, conflict) {
        let _ = send_signal_to_process(holder_pid, Some(SignalInfo::new_kernel(Signo::SIGIO)));
    }

    match block_on(interruptible(timeout_at(
        Some(deadline),
        poll_fn(|cx| {
            if conflict_cleared(id, breaker_pid, conflict) {
                Poll::Ready(())
            } else {
                LEASE_TABLE.lock().waiters.register(cx.waker());
                if conflict_cleared(id, breaker_pid, conflict) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }),
    ))) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => {
            force_break_lease(id);
            Ok(())
        }
        Err(_) => Err(AxError::Interrupted),
    }
}

fn clear_breaking_if_compatible(state: &mut LeaseState) {
    if state
        .breaking
        .is_some_and(|conflict| !lease_conflicts(state.lease_type, conflict))
    {
        state.breaking = None;
    }
}

pub(crate) fn lease_break_time_secs() -> u32 {
    LEASE_BREAK_TIME_SECS.load(core::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_lease_break_time_secs(value: u32) {
    LEASE_BREAK_TIME_SECS.store(value, core::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn wait_for_open(loc: &Location, flags: i32) -> AxResult<()> {
    if !is_regular_file(loc) {
        return Ok(());
    }
    let Some(conflict) = conflict_from_open_flags(flags) else {
        return Ok(());
    };
    wait_for_conflict(lease_id(loc), conflict)
}

pub(crate) fn wait_for_truncate(loc: &Location) -> AxResult<()> {
    if !is_regular_file(loc) {
        return Ok(());
    }
    wait_for_conflict(lease_id(loc), ConflictType::WriteAccess)
}

pub(crate) fn set_lease(file: &File, owner: LeaseOwner, arg: i32) -> AxResult<()> {
    let loc = file.inner().location();
    if !is_regular_file(loc) || file.inner().is_path() {
        return Err(AxError::InvalidInput);
    }
    if !current_can_set_lease(loc.metadata()?.uid) {
        return Err(AxError::PermissionDenied);
    }

    let id = lease_id(loc);
    let holder_pid = current_pid();
    let mut table = LEASE_TABLE.lock();

    match arg as u32 {
        F_UNLCK => {
            let removed = table
                .leases
                .get(&id)
                .is_some_and(|state| state.owner == owner)
                && table.leases.remove(&id).is_some();
            if removed {
                table.waiters.wake();
            }
            return Ok(());
        }
        _ => {}
    }

    let lease_type = lease_from_cmd(arg)?;

    if matches!(lease_type, LeaseType::Read) && file_open_has_write(file) {
        return Err(AxError::WouldBlock);
    }
    if matches!(lease_type, LeaseType::Write) && count_open_fds_for_location(loc) > 1 {
        return Err(AxError::WouldBlock);
    }

    if let Some(existing) = table.leases.get(&id) {
        if existing.owner != owner {
            return Err(AxError::ResourceBusy);
        }
        if existing
            .breaking
            .is_some_and(|conflict| lease_conflicts(lease_type, conflict))
        {
            return Err(AxError::WouldBlock);
        }
    }

    let state = table.leases.entry(id).or_insert(LeaseState {
        owner,
        holder_pid,
        lease_type,
        breaking: None,
    });
    state.owner = owner;
    state.holder_pid = holder_pid;
    state.lease_type = lease_type;
    clear_breaking_if_compatible(state);
    table.waiters.wake();
    Ok(())
}

pub(crate) fn get_lease(file: &File) -> i32 {
    let loc = file.inner().location();
    if !is_regular_file(loc) {
        return F_UNLCK as i32;
    }
    let table = LEASE_TABLE.lock();
    match table
        .leases
        .get(&lease_id(loc))
        .map(|state| state.lease_type)
    {
        Some(LeaseType::Read) => F_RDLCK as i32,
        Some(LeaseType::Write) => F_WRLCK as i32,
        None => F_UNLCK as i32,
    }
}

pub(crate) fn release_owner(owner: LeaseOwner) {
    let mut table = LEASE_TABLE.lock();
    let before = table.leases.len();
    table.leases.retain(|_, state| state.owner != owner);
    let removed = table.leases.len() != before;
    if removed {
        table.waiters.wake();
    }
}

pub(crate) fn formatted_lease_break_time() -> alloc::string::String {
    alloc::format!("{}\n", lease_break_time_secs())
}
