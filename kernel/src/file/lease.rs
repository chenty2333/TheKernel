use alloc::vec::Vec;
use core::time::Duration;

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FileFlags;
use axfs_ng_vfs::{Location, NodeType};
use axhal::time::wall_time;
use axpoll::PollSet;
use axsync::Mutex;
use axtask::current;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use linux_raw_sys::general::{
    CAP_LEASE, F_RDLCK, F_UNLCK, F_WRLCK, O_ACCMODE, O_PATH, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY,
};
use thekernel_linux_fd::{LeaseId, LeaseSnapshot, LeaseType as AbiLeaseType};
use thekernel_linux_signal::{SignalInfo, Signo};

use super::File;
use crate::{
    readiness::block_on_poll_set_until,
    task::{AsThread, Kuid, send_signal_to_process},
};

type InodeId = (u64, u64);
type LeaseOwner = u64;

const DEFAULT_LEASE_BREAK_TIME_SECS: u32 = 45;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaseType {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    leases: HashMap<InodeId, LeaseState>,
    owners: HashMap<LeaseOwner, InodeId>,
    open_files: HashMap<InodeId, InodeOpenState>,
    open_file_refs: usize,
    next_open_token: u64,
}

#[derive(Default)]
struct InodeOpenState {
    records: Vec<OpenRecord>,
}

#[derive(Clone, Copy)]
struct OpenRecord {
    token: u64,
    pending_conflict: ConflictType,
    visible_conflict: Option<ConflictType>,
    owner: Option<LeaseOwner>,
}

impl OpenRecord {
    fn conflict(self) -> ConflictType {
        self.owner
            .and(self.visible_conflict)
            .unwrap_or(self.pending_conflict)
    }
}

impl InodeOpenState {
    fn has_visible_owner(&self, owner: LeaseOwner) -> bool {
        self.records
            .iter()
            .any(|record| record.owner == Some(owner))
    }

    fn conflicts_with_lease(&self, lease_type: LeaseType, requester: LeaseOwner) -> bool {
        self.records.iter().any(|record| {
            record.owner != Some(requester) && lease_conflicts(lease_type, record.conflict())
        })
    }
}

#[derive(Clone, Copy)]
struct OpenRegistrationKey {
    id: InodeId,
    token: u64,
}

/// Owns one pending open/truncate admission. An ordinary open transfers this
/// exact record into its `FileDescription`; truncate drops it after commit.
#[must_use = "dropping an open lease admission rolls the pending operation back"]
pub(crate) struct OpenLeaseAdmission {
    key: Option<OpenRegistrationKey>,
    publishable: bool,
}

/// One global open record whose lifetime is exactly one open file
/// description. `dup`, fork, SCM_RIGHTS, and temporary `Arc` clones share the
/// containing `FileDescription` and therefore never create another record.
pub(crate) struct OpenLeaseRegistration {
    key: OpenRegistrationKey,
    owner: LeaseOwner,
}

/// Non-owning, allocation-free commit handle retained by FileDescription.
/// The owning registration lives in deferred cleanup, so final Arc drop never
/// takes the blocking lease-table mutex.
#[derive(Clone, Copy)]
pub(crate) struct OpenLeasePublication {
    key: OpenRegistrationKey,
    owner: LeaseOwner,
}

const MAX_LEASES: usize = 65_536;
const MAX_OPEN_FILE_REFS: usize = 65_536;

static LEASE_BREAK_TIME_SECS: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(DEFAULT_LEASE_BREAK_TIME_SECS);

lazy_static! {
    static ref LEASE_TABLE: Mutex<LeaseTable> = Mutex::new(LeaseTable {
        leases: HashMap::new(),
        owners: HashMap::new(),
        open_files: HashMap::new(),
        open_file_refs: 0,
        next_open_token: 1,
    });
}
static LEASE_WAITERS: PollSet = PollSet::new();

fn lease_id(loc: &Location) -> InodeId {
    (loc.mountpoint().device(), loc.inode())
}

fn is_regular_file(loc: &Location) -> bool {
    loc.node_type() == NodeType::RegularFile
}

fn pending_conflict_from_open_flags(flags: i32) -> Option<ConflictType> {
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
        O_RDWR => ConflictType::WriteAccess,
        // Linux access mode 3 is an ioctl/path-capable no-data OFD. It still
        // counts as another open for a write lease, but carries no write
        // authority and therefore does not conflict with a read lease.
        _ => ConflictType::ReadOpen,
    })
}

fn visible_conflict_from_open_flags(flags: i32) -> Option<ConflictType> {
    let flags = flags as u32;
    if flags & O_PATH != 0 {
        return None;
    }
    Some(match flags & O_ACCMODE {
        O_RDONLY => ConflictType::ReadOpen,
        O_WRONLY => ConflictType::WriteAccess,
        O_RDWR => ConflictType::WriteAccess,
        _ => ConflictType::ReadOpen,
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

fn abi_lease_id(owner: LeaseOwner) -> AxResult<LeaseId> {
    LeaseId::new(owner).ok_or(AxError::BadState)
}

fn abi_lease_type(kind: LeaseType) -> AbiLeaseType {
    match kind {
        LeaseType::Read => AbiLeaseType::Read,
        LeaseType::Write => AbiLeaseType::Write,
    }
}

fn abi_lease_snapshot(state: &LeaseState) -> AxResult<LeaseSnapshot> {
    Ok(LeaseSnapshot {
        lease: Some((abi_lease_id(state.owner)?, abi_lease_type(state.lease_type))),
        breaking: state.breaking.is_some(),
    })
}

fn current_pid() -> u32 {
    current().as_thread().proc_data.proc.pid()
}

fn current_can_set_lease(owner_uid: u32) -> bool {
    current().try_as_thread().is_some_and(|thr| {
        let cred = thr.current_cred();
        Kuid::from_raw(owner_uid) == Some(cred.ids().fsuid)
            || cred.has_effective_capability(CAP_LEASE)
    })
}

fn file_open_has_write(file: &File) -> bool {
    // O_APPEND is mutable status, not write authority. Read-only and reserved
    // no-data descriptions may report it through F_GETFL without conflicting
    // with a read lease.
    file.inner().flags().contains(FileFlags::WRITE)
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
    let removed = {
        let mut table = LEASE_TABLE.lock();
        let removed = table.leases.remove(&id);
        if let Some(state) = removed.as_ref() {
            table.owners.remove(&state.owner);
        }
        removed
    };
    if removed.is_some() {
        LEASE_WAITERS.wake();
    }
    drop(removed);
}

fn wait_for_conflict(id: InodeId, conflict: ConflictType) -> AxResult<()> {
    let breaker_pid = current_pid();
    let deadline = wall_time().saturating_add(Duration::from_secs(lease_break_time_secs() as u64));

    if let Some(holder_pid) = mark_breaking_lease(id, breaker_pid, conflict) {
        let _ = send_signal_to_process(holder_pid, Some(SignalInfo::new_kernel(Signo::SIGIO)));
    }

    match block_on_poll_set_until(&LEASE_WAITERS, Some(deadline), || {
        if conflict_cleared(id, breaker_pid, conflict) {
            Ok(())
        } else {
            Err(AxError::WouldBlock)
        }
    }) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            force_break_lease(id);
            Ok(())
        }
    }
}

impl LeaseTable {
    fn try_register_open(
        &mut self,
        id: InodeId,
        pending_conflict: ConflictType,
        visible_conflict: Option<ConflictType>,
    ) -> AxResult<OpenRegistrationKey> {
        if self.open_file_refs >= MAX_OPEN_FILE_REFS {
            return Err(LinuxError::ENFILE.into());
        }
        if !self.open_files.contains_key(&id) {
            let mut records = Vec::new();
            records.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            self.open_files
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            self.open_files.insert(id, InodeOpenState { records });
        } else {
            self.open_files
                .get_mut(&id)
                .ok_or(AxError::BadState)?
                .records
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
        }
        let token = self.next_open_token;
        let next_open_token = token.checked_add(1).ok_or(LinuxError::ENFILE)?;
        let state = self.open_files.get_mut(&id).ok_or(AxError::BadState)?;
        state.records.push(OpenRecord {
            token,
            pending_conflict,
            visible_conflict,
            owner: None,
        });
        self.next_open_token = next_open_token;
        self.open_file_refs += 1;
        Ok(OpenRegistrationKey { id, token })
    }

    /// Performs the allocation-free Pending -> Visible handoff. Both states
    /// live in this table, so F_SETLEASE can never observe a gap between them.
    fn publish_open(&mut self, key: OpenRegistrationKey, owner: LeaseOwner) -> bool {
        let Some(record) = self.open_files.get_mut(&key.id).and_then(|state| {
            state
                .records
                .iter_mut()
                .find(|record| record.token == key.token)
        }) else {
            return false;
        };
        if record.visible_conflict.is_none() {
            return false;
        }
        match record.owner {
            None => record.owner = Some(owner),
            Some(existing) if existing == owner => {}
            Some(_) => return false,
        }
        true
    }

    fn release_open(&mut self, key: OpenRegistrationKey) -> bool {
        let mut remove_inode = false;
        let removed = if let Some(state) = self.open_files.get_mut(&key.id) {
            if let Some(index) = state
                .records
                .iter()
                .position(|record| record.token == key.token)
            {
                state.records.swap_remove(index);
                remove_inode = state.records.is_empty();
                true
            } else {
                false
            }
        } else {
            false
        };
        if !removed {
            return false;
        }
        if let Some(open_file_refs) = self.open_file_refs.checked_sub(1) {
            self.open_file_refs = open_file_refs;
        } else {
            error!("lease open-file reference count underflow");
        }
        if remove_inode {
            self.open_files.remove(&key.id);
        }
        true
    }

    fn lease_request_conflicts(
        &self,
        id: InodeId,
        lease_type: LeaseType,
        requester: LeaseOwner,
    ) -> Option<bool> {
        let state = self.open_files.get(&id)?;
        state
            .has_visible_owner(requester)
            .then(|| state.conflicts_with_lease(lease_type, requester))
    }
}

fn try_register_open_admission(
    id: InodeId,
    breaker_pid: u32,
    pending_conflict: ConflictType,
    visible_conflict: Option<ConflictType>,
) -> AxResult<OpenLeaseAdmission> {
    let mut table = LEASE_TABLE.lock();
    if table.leases.get(&id).is_some_and(|state| {
        state.holder_pid != breaker_pid && lease_conflicts(state.lease_type, pending_conflict)
    }) {
        return Err(AxError::WouldBlock);
    }
    let key = table.try_register_open(id, pending_conflict, visible_conflict)?;
    Ok(OpenLeaseAdmission {
        key: Some(key),
        publishable: visible_conflict.is_some(),
    })
}

fn admit_conflict(
    loc: &Location,
    pending_conflict: ConflictType,
    visible_conflict: Option<ConflictType>,
) -> AxResult<OpenLeaseAdmission> {
    let id = lease_id(loc);
    let breaker_pid = current_pid();
    loop {
        wait_for_conflict(id, pending_conflict)?;
        match try_register_open_admission(id, breaker_pid, pending_conflict, visible_conflict) {
            Ok(admission) => return Ok(admission),
            Err(AxError::WouldBlock) => continue,
            Err(error) => return Err(error),
        }
    }
}

impl OpenLeaseAdmission {
    const fn none() -> Self {
        Self {
            key: None,
            publishable: true,
        }
    }

    pub(crate) fn into_ofd(mut self, owner: LeaseOwner) -> AxResult<Option<OpenLeaseRegistration>> {
        let Some(key) = self.key else {
            return Ok(None);
        };
        if !self.publishable {
            return Err(AxError::BadState);
        }
        self.key = None;
        Ok(Some(OpenLeaseRegistration { key, owner }))
    }
}

impl Drop for OpenLeaseAdmission {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        if !LEASE_TABLE.lock().release_open(key) {
            error!("lease open admission identity disappeared");
        }
    }
}

impl OpenLeaseRegistration {
    pub(crate) const fn publication(&self) -> OpenLeasePublication {
        OpenLeasePublication {
            key: self.key,
            owner: self.owner,
        }
    }
}

impl OpenLeasePublication {
    /// Converts the global record to a visible OFD before fd-table visibility.
    /// Repeated calls are harmless, which is important because descriptor
    /// publication through dup still calls the common commit hook.
    pub(crate) fn publish(&self) {
        if !LEASE_TABLE.lock().publish_open(self.key, self.owner) {
            error!("lease open registration could not become visible");
        }
    }
}

impl Drop for OpenLeaseRegistration {
    fn drop(&mut self) {
        if !LEASE_TABLE.lock().release_open(self.key) {
            error!("lease open registration identity disappeared");
        }
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

pub(crate) fn admit_open(loc: &Location, flags: i32) -> AxResult<OpenLeaseAdmission> {
    if !is_regular_file(loc) {
        return Ok(OpenLeaseAdmission::none());
    }
    let Some(pending_conflict) = pending_conflict_from_open_flags(flags) else {
        return Ok(OpenLeaseAdmission::none());
    };
    let visible_conflict = visible_conflict_from_open_flags(flags).ok_or(AxError::BadState)?;
    admit_conflict(loc, pending_conflict, Some(visible_conflict))
}

pub(crate) fn admit_truncate(loc: &Location) -> AxResult<OpenLeaseAdmission> {
    if !is_regular_file(loc) {
        return Ok(OpenLeaseAdmission::none());
    }
    admit_conflict(loc, ConflictType::WriteAccess, None)
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
    if arg as u32 == F_UNLCK {
        let removed = {
            let mut table = LEASE_TABLE.lock();
            if let Some(state) = table.leases.get(&id).filter(|state| state.owner == owner) {
                abi_lease_snapshot(state)?
                    .plan_release(abi_lease_id(owner)?)
                    .map_err(|_| AxError::BadState)?;
                table.owners.remove(&owner);
                table.leases.remove(&id)
            } else {
                None
            }
        };
        if removed.is_some() {
            LEASE_WAITERS.wake();
        }
        drop(removed);
        return Ok(());
    }

    let lease_type = lease_from_cmd(arg)?;

    if matches!(lease_type, LeaseType::Read) && file_open_has_write(file) {
        return Err(AxError::WouldBlock);
    }

    let mut table = LEASE_TABLE.lock();
    // Pending and visible opens share one global per-inode record set. The
    // requester's exact OFD identity is excluded, while every other process,
    // files table, and retained OFD owner is represented without counting dup
    // descriptors more than once.
    match table.lease_request_conflicts(id, lease_type, owner) {
        Some(false) => {}
        Some(true) => return Err(AxError::WouldBlock),
        None => return Err(AxError::BadState),
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
        // A same-owner lease replacement is an explicit release/admit plan;
        // the surrounding table lock performs its atomic realization.
        let after_release = match abi_lease_snapshot(existing)?
            .plan_release(abi_lease_id(owner)?)
            .map_err(|_| AxError::BadState)?
        {
            thekernel_linux_fd::LeasePlan::Release { after, .. } => after,
            _ => return Err(AxError::BadState),
        };
        after_release
            .plan_admit(abi_lease_id(owner)?, abi_lease_type(lease_type))
            .map_err(|_| AxError::ResourceBusy)?;
    } else {
        LeaseSnapshot::empty()
            .plan_admit(abi_lease_id(owner)?, abi_lease_type(lease_type))
            .map_err(|_| AxError::ResourceBusy)?;
    }

    if !table.leases.contains_key(&id) {
        if table.leases.len() >= MAX_LEASES {
            return Err(LinuxError::ENOLCK.into());
        }
        if table.owners.contains_key(&owner) {
            return Err(AxError::BadState);
        }
        table.leases.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        table.owners.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        table.owners.insert(owner, id);
        table.leases.insert(
            id,
            LeaseState {
                owner,
                holder_pid,
                lease_type,
                breaking: None,
            },
        );
    } else {
        let state = table.leases.get_mut(&id).ok_or(AxError::BadState)?;
        state.owner = owner;
        state.holder_pid = holder_pid;
        state.lease_type = lease_type;
        clear_breaking_if_compatible(state);
    }
    drop(table);
    LEASE_WAITERS.wake();
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
    let removed = {
        let mut table = LEASE_TABLE.lock();
        let Some(id) = table.owners.remove(&owner) else {
            return;
        };
        match table.leases.remove(&id) {
            Some(state) if state.owner == owner => Some(state),
            Some(state) => {
                table.leases.insert(id, state);
                table.owners.insert(owner, id);
                None
            }
            None => None,
        }
    };
    if removed.is_some() {
        LEASE_WAITERS.wake();
    }
    drop(removed);
}

pub(crate) fn formatted_lease_break_time() -> alloc::string::String {
    alloc::format!("{}\n", lease_break_time_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> LeaseTable {
        LeaseTable {
            leases: HashMap::new(),
            owners: HashMap::new(),
            open_files: HashMap::new(),
            open_file_refs: 0,
            next_open_token: 1,
        }
    }

    #[test]
    fn pending_open_blocks_setlease_without_a_handoff_gap() {
        let id = (1, 2);
        let owner = 10;
        let mut table = table();
        let requester = table
            .try_register_open(id, ConflictType::ReadOpen, Some(ConflictType::ReadOpen))
            .unwrap();
        assert!(table.publish_open(requester, owner));

        let pending_write = table
            .try_register_open(
                id,
                ConflictType::WriteAccess,
                Some(ConflictType::WriteAccess),
            )
            .unwrap();
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Read, owner),
            Some(true)
        );
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Write, owner),
            Some(true)
        );

        assert!(table.release_open(pending_write));
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Read, owner),
            Some(false)
        );
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Write, owner),
            Some(false)
        );
    }

    #[test]
    fn visible_other_ofd_is_global_and_requester_is_excluded() {
        let id = (3, 4);
        let requester = 20;
        let other = 21;
        let mut table = table();
        let requester_key = table
            .try_register_open(id, ConflictType::ReadOpen, Some(ConflictType::ReadOpen))
            .unwrap();
        let other_key = table
            .try_register_open(id, ConflictType::ReadOpen, Some(ConflictType::ReadOpen))
            .unwrap();
        assert!(table.publish_open(requester_key, requester));
        assert!(table.publish_open(other_key, other));

        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Read, requester),
            Some(false)
        );
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Write, requester),
            Some(true)
        );

        assert!(table.release_open(other_key));
        let other_write = table
            .try_register_open(
                id,
                ConflictType::WriteAccess,
                Some(ConflictType::WriteAccess),
            )
            .unwrap();
        assert!(table.publish_open(other_write, other));
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Read, requester),
            Some(true)
        );
    }

    #[test]
    fn duplicate_publication_does_not_duplicate_an_ofd_record() {
        let id = (5, 6);
        let owner = 30;
        let mut table = table();
        let key = table
            .try_register_open(id, ConflictType::ReadOpen, Some(ConflictType::ReadOpen))
            .unwrap();
        assert!(table.publish_open(key, owner));
        assert!(table.publish_open(key, owner));
        assert_eq!(table.open_file_refs, 1);
        assert_eq!(table.open_files.get(&id).unwrap().records.len(), 1);
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Write, owner),
            Some(false)
        );
    }

    #[test]
    fn final_ofd_lifetime_release_removes_global_visible_state() {
        let id = (7, 8);
        let owner = 40;
        let mut table = table();
        let key = table
            .try_register_open(id, ConflictType::ReadOpen, Some(ConflictType::ReadOpen))
            .unwrap();
        assert!(table.publish_open(key, owner));
        assert_eq!(
            table.lease_request_conflicts(id, LeaseType::Write, owner),
            Some(false)
        );

        assert!(table.release_open(key));
        assert!(!table.open_files.contains_key(&id));
        assert_eq!(table.open_file_refs, 0);
    }

    #[test]
    fn truncating_read_open_becomes_a_read_only_visible_ofd() {
        let flags = (O_RDONLY | O_TRUNC) as i32;
        assert_eq!(
            pending_conflict_from_open_flags(flags),
            Some(ConflictType::WriteAccess)
        );
        assert_eq!(
            visible_conflict_from_open_flags(flags),
            Some(ConflictType::ReadOpen)
        );
    }

    #[test]
    fn no_data_open_counts_as_open_without_write_lease_conflict() {
        let flags = O_ACCMODE as i32;
        assert_eq!(
            pending_conflict_from_open_flags(flags),
            Some(ConflictType::ReadOpen)
        );
        assert_eq!(
            visible_conflict_from_open_flags(flags),
            Some(ConflictType::ReadOpen)
        );
        assert!(!lease_conflicts(LeaseType::Read, ConflictType::ReadOpen));
        assert!(lease_conflicts(LeaseType::Write, ConflictType::ReadOpen));
    }
}
