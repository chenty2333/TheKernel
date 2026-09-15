use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write as _,
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicI32, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    ctypes::{c_int, c_ulong, c_ushort},
    general::*,
};
use tk_linux_ipc::{
    IpcId, IpcIdTable, SemBuf as AbiSemBuf, SemPlan, ipcid_compose, ipcid_is_stale, ipcid_to_idx,
    plan_sem_op, sem_undo_delta_in_range,
};
use tk_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load, vm_write_slice,
};

use super::{
    GETALL, GETNCNT, GETPID, GETVAL, GETZCNT, IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID,
    IPC_SET, IPC_STAT, IpcAccess, IpcAccessContext, IpcPerm, SEM_INFO,
    SEM_STAT, SEM_STAT_ANY, SETALL, SETVAL, allocate_ipc_id,
};
use crate::{
    mm::map_usercopy_error,
    task::{AsThread, ProcStateHint, has_pending_syscall_signal, with_proc_state_hint},
    time::{TimeValueLike, wall_time},
};

const IPC_MODE_MASK: c_ushort = 0o777;
const SEM_UNDO: i16 = 0x1000;

pub const SEMMSL: usize = 32000;
/// `include/uapi/linux/sem.h:80-81`: `SEMMNI 32000`, `SEMMNS (SEMMNI*SEMMSL)`;
/// `ipc/sem.c:249-256` seeds each IPC namespace with those defaults.
pub const SEMMNI: usize = 32000;
pub const SEMMNS: usize = SEMMSL * SEMMNI;
pub const SEMOPM: usize = 500;
pub const SEMVMX: usize = 32767;
const SEMAEM: usize = SEMVMX;
const SEMUME: usize = SEMOPM;
const SEMUSZ: usize = 20;

/// Never-reused identity source for `SemArray::serial`.  A wrap would require
/// 2^64 array creations, and `fetch_add` wraps rather than panicking.
static SEM_ARRAY_SERIAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn next_sem_array_serial() -> u64 {
    SEM_ARRAY_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// Adjustment values owned by one Linux `sem_undo` list.
///
/// `CLONE_SYSVSEM` shares the `Arc<Mutex<SemUndo>>` supplied by the namespace
/// proxy.  A non-sharing clone gets a fresh list containing a snapshot of the
/// parent's entries.  The proxy calls `apply_sem_undo` only when the final
/// owner exits, which is the lifetime boundary required by Linux.
pub(crate) struct SemUndo {
    entries: BTreeMap<(i32, u16), SemAdjustment>,
}

#[derive(Clone, Copy)]
struct SemAdjustment {
    value: i32,
    generation: u64,
    /// Identity of the array this adjustment was recorded against.  Linux
    /// `freeary()` clears every undo entry that named an array it removed, and
    /// `exit_sem()` re-checks the array after re-obtaining it, so an
    /// adjustment must never reach a *different* array that happens to reuse
    /// the identifier.
    serial: u64,
}

impl SemUndo {
    pub(crate) const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub(crate) fn try_clone(&self) -> AxResult<Self> {
        let mut entries = BTreeMap::new();
        entries.extend(self.entries.iter().map(|(key, value)| (*key, *value)));
        Ok(Self { entries })
    }

    /// Linux `sem_undo.semadj[sem_num]`: the adjustment accumulated so far for
    /// one semaphore of one array.  An entry that belongs to a different array
    /// instance or was cleared by `SETVAL` reads as zero.
    fn prior(&self, semid: i32, serial: u64, semnum: u16) -> i32 {
        self.entries
            .get(&(semid, semnum))
            .filter(|entry| entry.serial == serial)
            .map_or(0, |entry| entry.value)
    }

    /// Reserve every new undo key an operation can commit before the
    /// semaphore values are changed.  `semop` is atomic: an allocation or
    /// SEMUME failure must not be observed after any member of the operation
    /// vector has taken effect.  The caller keeps this list locked through
    /// `record`, so the reservation cannot be consumed by a CLONE_SYSVSEM
    /// sibling in between.
    fn prepare_records(&mut self, semid: i32, ops: &[Sembuf]) -> AxResult<()> {
        let mut additional = 0usize;
        for (index, op) in ops.iter().enumerate() {
            if !plan_sem_op(abi_sem_buf(op)).records_undo() {
                continue;
            }
            let key = (semid, op.sem_num);
            if self.entries.contains_key(&key)
                || ops[..index].iter().any(|prior| {
                    plan_sem_op(abi_sem_buf(prior)).records_undo()
                        && prior.sem_num == op.sem_num
                })
            {
                continue;
            }
            additional = additional.checked_add(1).ok_or(AxError::NoMemory)?;
        }
        if self.entries.len().saturating_add(additional) > SEMUME {
            return Err(AxError::from(LinuxError::ENOSPC));
        }
        // `BTreeMap` has no fallible reservation API.  All state mutation is
        // still deferred until the operation has passed the semantic bounds
        // above; insertion itself owns the map allocation.
        let _ = additional;
        Ok(())
    }

    /// Records the inverse adjustment for one successfully completed SEM_UNDO
    /// operation.
    ///
    /// Linux `ipc/sem.c:perform_atomic_semop()` rejects an adjustment outside
    /// `[-SEMAEM - 1, SEMAEM]` with ERANGE instead of clamping it; the caller
    /// has already made the same check while validating the operation vector,
    /// so this is the transactional backstop rather than the error path.
    pub(crate) fn record(
        &mut self,
        semid: i32,
        semnum: u16,
        sem_op: i16,
        generation: u64,
        serial: u64,
    ) -> AxResult<()> {
        let prior = self
            .entries
            .get(&(semid, semnum))
            .filter(|entry| entry.serial == serial && entry.generation == generation)
            .map_or(0, |entry| entry.value);
        if !sem_undo_delta_in_range(prior, sem_op) {
            return Err(AxError::from(LinuxError::ERANGE));
        }
        let key = (semid, semnum);
        if !self.entries.contains_key(&key) && self.entries.len() >= SEMUME {
            return Err(AxError::from(LinuxError::ENOSPC));
        }
        // Linux writes the identity for a wait-for-zero, which cannot change
        // the accumulated adjustment.
        self.entries.insert(
            key,
            SemAdjustment {
                value: prior - sem_op as i32,
                generation,
                serial,
            },
        );
        Ok(())
    }
}

/// The highest semaphore number one `semop` vector names.
///
/// Linux `__do_semtimedop()` accumulates `max` over the vector while it
/// copies it in and bounds it against `sma->sem_nsems` with EFBIG before it
/// checks permission.
fn highest_sem_num(ops: &[Sembuf]) -> u16 {
    ops.iter().map(|op| op.sem_num).max().unwrap_or(0)
}

/// Translates one userspace `sembuf` into the ABI crate's view.
fn abi_sem_buf(op: &Sembuf) -> AbiSemBuf {
    AbiSemBuf {
        num: op.sem_num,
        op: op.sem_op,
        flags: op.sem_flg,
    }
}

/// The `SEM_UNDO` state one semop vector is validated against.
///
/// Linux `perform_atomic_semop()` walks the operation vector once, checking
/// each operation's semaphore value *and* its deferred adjustment in order,
/// and only then commits.  Staging the adjustments here reproduces that pass
/// without touching the process's undo list until the vector succeeds; a
/// repeated `sem_num` accumulates exactly as `semadj[]` does.
struct SemUndoCheck<'a> {
    undo: Option<&'a SemUndo>,
    semid: i32,
    serial: u64,
    staged: Vec<(u16, i32)>,
}

impl SemUndoCheck<'_> {
    fn prior(&self, semnum: u16) -> i32 {
        if let Some((_, value)) = self.staged.iter().rev().find(|(num, _)| *num == semnum) {
            return *value;
        }
        self.undo
            .map_or(0, |undo| undo.prior(self.semid, self.serial, semnum))
    }

    /// Validates one operation's deferred adjustment, in vector order.
    fn check(&mut self, plan: SemPlan, semnum: u16, sem_op: i16) -> AxResult<()> {
        if !plan.undo() {
            return Ok(());
        }
        let prior = self.prior(semnum);
        if !sem_undo_delta_in_range(prior, sem_op) {
            return Err(AxError::from(LinuxError::ERANGE));
        }
        self.staged.push((semnum, prior - sem_op as i32));
        Ok(())
    }
}

impl Default for SemUndo {
    fn default() -> Self {
        Self::new()
    }
}

fn ipc_time_secs() -> __kernel_time_t {
    wall_time().as_secs() as __kernel_time_t
}

/// Monotonic reading for the `semtimedop` deadline.
///
/// Linux `__do_semtimedop()` derives its deadline with `ktime_add_safe(
/// ktime_get(), ...)` and waits on a `ktime_t` timer, both of which are
/// monotonic.  Anchoring the deadline to the wall clock instead lets a
/// `settimeofday` during the wait expire a blocking `semop` immediately.
fn monotonic_duration() -> Duration {
    Duration::from_nanos(axhal::time::monotonic_time_nanos())
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct SemidDs {
    pub sem_perm: IpcPerm,
    pub sem_otime: __kernel_time_t,
    pub unused1: c_ulong,
    pub sem_ctime: __kernel_time_t,
    pub unused2: c_ulong,
    pub sem_nsems: c_ulong,
    pub unused3: c_ulong,
    pub unused4: c_ulong,
}

// These System V semaphore records contain Linux ABI padding through their
// embedded `IpcPerm`.  Keep the x86_64 layout checked and serialize a zeroed
// copy for output so implicit alignment bytes never escape to userspace.
//
// The two `__unused` words after `sem_otime` and `sem_ctime` are not
// decoration: `arch/x86/include/uapi/asm/sembuf.h` carries them on x86_64
// ("x86_64 and x32 incorrectly added padding here, so the structures are
// still incompatible with the padding on x86"), which is why Linux's
// `semid64_ds` is 104 bytes with `sem_ctime` at 64 and `sem_nsems` at 80
// rather than the 88 bytes a packed reading would give.
const _: () = {
    assert!(align_of::<IpcPerm>() == 8);
    assert!(size_of::<IpcPerm>() == 48);
    assert!(offset_of!(IpcPerm, key) == 0);
    assert!(offset_of!(IpcPerm, mode) == 20);
    assert!(offset_of!(IpcPerm, unused0) == 32);
    assert!(offset_of!(IpcPerm, unused1) == 40);
    assert!(align_of::<SemidDs>() == 8);
    assert!(size_of::<SemidDs>() == 104);
    assert!(offset_of!(SemidDs, sem_perm) == 0);
    assert!(offset_of!(SemidDs, sem_otime) == 48);
    assert!(offset_of!(SemidDs, unused1) == 56);
    assert!(offset_of!(SemidDs, sem_ctime) == 64);
    assert!(offset_of!(SemidDs, unused2) == 72);
    assert!(offset_of!(SemidDs, sem_nsems) == 80);
    assert!(offset_of!(SemidDs, unused3) == 88);
    assert!(offset_of!(SemidDs, unused4) == 96);
};

fn initialized_semid_ds(value: SemidDs) -> SemidDs {
    // SAFETY: all fields are integer scalars; zero is valid and initializes
    // both the embedded IpcPerm alignment hole and the complete record.
    let mut result: SemidDs = unsafe { core::mem::zeroed() };
    let mut perm: IpcPerm = unsafe { core::mem::zeroed() };
    perm.key = value.sem_perm.key;
    perm.uid = value.sem_perm.uid;
    perm.gid = value.sem_perm.gid;
    perm.cuid = value.sem_perm.cuid;
    perm.cgid = value.sem_perm.cgid;
    perm.mode = value.sem_perm.mode;
    perm.pad1 = value.sem_perm.pad1;
    perm.seq = value.sem_perm.seq;
    perm.pad2 = value.sem_perm.pad2;
    perm.unused0 = value.sem_perm.unused0;
    perm.unused1 = value.sem_perm.unused1;
    result.sem_perm = perm;
    result.sem_otime = value.sem_otime;
    result.unused1 = value.unused1;
    result.sem_ctime = value.sem_ctime;
    result.unused2 = value.unused2;
    result.sem_nsems = value.sem_nsems;
    result.unused3 = value.unused3;
    result.unused4 = value.unused4;
    result
}

const _: () = {
    assert!(align_of::<SemInfo>() == 4);
    assert!(size_of::<SemInfo>() == 40);
    assert!(align_of::<Sembuf>() == 2);
    assert!(size_of::<Sembuf>() == 6);
    assert!(offset_of!(Sembuf, sem_num) == 0);
    assert!(offset_of!(Sembuf, sem_op) == 2);
    assert!(offset_of!(Sembuf, sem_flg) == 4);
};

fn write_semid_ds<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut SemidDs,
    value: SemidDs,
) -> AxResult<()> {
    // SAFETY: `initialized_semid_ds` zeroes all padding and the assertions
    // above cover the full Linux object extent.
    unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, initialized_semid_ds(value)) }
        .map_err(map_usercopy_error)
}

fn write_sem_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut SemInfo,
    value: SemInfo,
) -> AxResult<()> {
    // SAFETY: `SemInfo` consists solely of ten initialized i32 words and has
    // no padding on x86_64, as checked above.
    unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, value) }.map_err(map_usercopy_error)
}

impl SemidDs {
    fn new(key: i32, nsems: usize, mode: __kernel_mode_t, uid: u32, gid: u32) -> Self {
        Self {
            sem_perm: IpcPerm {
                key,
                uid,
                gid,
                cuid: uid,
                cgid: gid,
                mode: (mode & IPC_MODE_MASK as __kernel_mode_t) as _,
                pad1: 0,
                seq: 0,
                pad2: 0,
                unused0: 0,
                unused1: 0,
            },
            sem_otime: 0,
            unused1: 0,
            sem_ctime: ipc_time_secs(),
            unused2: 0,
            sem_nsems: nsems as c_ulong,
            unused3: 0,
            unused4: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct SemInfo {
    semmap: c_int,
    semmni: c_int,
    semmns: c_int,
    semmnu: c_int,
    semmsl: c_int,
    semopm: c_int,
    semume: c_int,
    semusz: c_int,
    semvmx: c_int,
    semaem: c_int,
}

impl SemInfo {
    fn ipc_info() -> Self {
        let semmni = semmni_limit().min(c_int::MAX as usize) as c_int;
        let semmsl = semmsl_limit().min(c_int::MAX as usize) as c_int;
        let semmns = semmns_limit().min(c_int::MAX as usize) as c_int;
        Self {
            semmap: semmns,
            semmni,
            semmns,
            semmnu: semmns,
            semmsl,
            semopm: semopm_limit().min(c_int::MAX as usize) as c_int,
            semume: SEMUME as c_int,
            semusz: SEMUSZ as c_int,
            semvmx: SEMVMX as c_int,
            semaem: SEMAEM as c_int,
        }
    }

    // Mirrors the `SEM_INFO` semctl command rather than a Rust constructor
    // convention; the name is the Linux operation it answers.
    #[allow(clippy::self_named_constructors)]
    fn sem_info(manager: &SemManager) -> Self {
        let mut info = Self::ipc_info();
        info.semusz = manager.active_array_count().min(c_int::MAX as usize) as c_int;
        info.semaem = manager.total_semaphores().min(c_int::MAX as usize) as c_int;
        info
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct Sembuf {
    sem_num: u16,
    sem_op: i16,
    sem_flg: i16,
}

#[derive(Clone, Copy)]
struct Semaphore {
    value: u16,
    // SETVAL/SETALL invalidate every owner's earlier adjustment under this
    // same array lock. New SEM_UNDO operations start from zero in this epoch.
    undo_generation: u64,
    pid: __kernel_pid_t,
    ncnt: usize,
    zcnt: usize,
}

impl Semaphore {
    const fn new() -> Self {
        Self {
            value: 0,
            undo_generation: 0,
            pid: 0,
            ncnt: 0,
            zcnt: 0,
        }
    }

    fn reset_value(&mut self, value: u16, pid: __kernel_pid_t) -> AxResult<()> {
        let generation = self
            .undo_generation
            .checked_add(1)
            .ok_or(AxError::OutOfRange)?;
        self.undo_generation = generation;
        self.value = value;
        self.pid = pid;
        Ok(())
    }
}

struct SemArray {
    semid: i32,
    semid_ds: SemidDs,
    sems: Vec<Semaphore>,
    removed: bool,
    waiters: Arc<axtask::WaitQueue>,
    /// Never-reused identity of this array instance.
    ///
    /// Linux `ipc/sem.c:freeary()` walks `sma->list_id` and clears every undo
    /// entry that targeted the array it is removing, and `exit_sem()` looks
    /// the undo structure up again after re-obtaining the object, because
    /// "the sequence number of the semaphore set can be the same" for a set
    /// that was removed and recreated with the same identifier.  A monotonically
    /// assigned serial gives the same guarantee here: an adjustment recorded
    /// against a destroyed array can never be applied to its successor.
    serial: u64,
}

struct WaitCountGuard<'a> {
    array: &'a Arc<Mutex<SemArray>>,
    index: usize,
    wait_zero: bool,
}

impl Drop for WaitCountGuard<'_> {
    fn drop(&mut self) {
        let mut array = self.array.lock();
        if let Some(sem) = array.sems.get_mut(self.index) {
            if self.wait_zero {
                sem.zcnt = sem.zcnt.saturating_sub(1);
            } else {
                sem.ncnt = sem.ncnt.saturating_sub(1);
            }
        }
    }
}

impl SemArray {
    fn new(semid: i32, key: i32, nsems: usize, mode: __kernel_mode_t, uid: u32, gid: u32) -> Self {
        Self {
            semid,
            semid_ds: SemidDs::new(key, nsems, mode, uid, gid),
            sems: alloc::vec![Semaphore::new(); nsems],
            removed: false,
            waiters: Arc::new(axtask::WaitQueue::new()),
            serial: next_sem_array_serial(),
        }
    }

    fn reset_values(&mut self, values: &[u16], pid: __kernel_pid_t) -> AxResult<()> {
        if values.len() != self.sems.len() {
            return Err(AxError::InvalidInput);
        }
        // Preflight the complete SETALL before either values or undo epochs
        // change, preserving the operation's atomicity even on exhaustion.
        if self.sems.iter().any(|sem| sem.undo_generation == u64::MAX) {
            return Err(AxError::OutOfRange);
        }
        for (sem, value) in self.sems.iter_mut().zip(values) {
            sem.reset_value(*value, pid)?;
        }
        Ok(())
    }

    fn nsems(&self) -> usize {
        self.sems.len()
    }

    fn mark_changed(&mut self) {
        self.semid_ds.sem_ctime = ipc_time_secs();
    }

    fn readable(&self, context: &IpcAccessContext) -> bool {
        context.allows(&self.semid_ds.sem_perm, IpcAccess::Read)
    }

    fn writable(&self, context: &IpcAccessContext) -> bool {
        context.allows(&self.semid_ds.sem_perm, IpcAccess::Write)
    }
}

pub(crate) struct SemManager {
    key_semid: BTreeMap<i32, i32>,
    /// index -> semaphore array, exactly `sem_ids(ns).ipcs_idr`
    semid_arrays: BTreeMap<i32, Arc<Mutex<SemArray>>>,
    /// Linux `struct ipc_ids` bookkeeping for this table.
    ids: IpcIdTable,
}

impl SemManager {
    pub(crate) const fn new() -> Self {
        Self {
            key_semid: BTreeMap::new(),
            semid_arrays: BTreeMap::new(),
            ids: IpcIdTable::new(),
        }
    }

    fn get_semid_by_key(&self, key: i32) -> Option<i32> {
        self.key_semid.get(&key).copied()
    }

    /// Allocates the identifier for a new array.  The caller holds the manager
    /// lock, which is this table's `ipc_ids.rwsem`.
    fn allocate_id(&mut self, next_id: &AtomicI32) -> AxResult<IpcId> {
        let arrays = &self.semid_arrays;
        allocate_ipc_id(next_id, &mut self.ids, |index| arrays.contains_key(&index))
    }

    /// Linux `ipc_obtain_object_idr()`: resolve an index without checking its
    /// sequence number, which is what `SEM_STAT`/`SEM_STAT_ANY` need.
    fn get_array_by_index(&self, index: i32) -> Option<Arc<Mutex<SemArray>>> {
        self.semid_arrays.get(&index).cloned()
    }

    /// Linux `ipc_obtain_object_check()`: resolve the index and verify
    /// `ipc_checkid()`, so a stale identifier is rejected with EINVAL instead
    /// of naming whichever array happens to sit at that index now.
    fn get_array_by_semid(&self, semid: i32) -> Option<Arc<Mutex<SemArray>>> {
        if semid < 0 {
            return None;
        }
        let array = self.get_array_by_index(ipcid_to_idx(semid))?;
        let sequence = array.lock().semid_ds.sem_perm.seq as i32;
        if ipcid_is_stale(semid, sequence) {
            return None;
        }
        Some(array)
    }

    fn insert(&mut self, key: i32, semid: i32, array: Arc<Mutex<SemArray>>) {
        if key != IPC_PRIVATE {
            self.key_semid.insert(key, semid);
        }
        self.semid_arrays.insert(ipcid_to_idx(semid), array);
    }

    fn remove_semid(&mut self, semid: i32) {
        self.key_semid.retain(|_, value| *value != semid);
        self.semid_arrays.remove(&ipcid_to_idx(semid));
        // Linux `ipc_rmid()` updates the cached highest index after the IDR
        // removal, which `SEM_INFO`/`IPC_INFO` report.
        let arrays = &self.semid_arrays;
        self.ids
            .release(ipcid_to_idx(semid), |index| arrays.contains_key(&index));
    }

    fn active_array_count(&self) -> usize {
        self.semid_arrays
            .values()
            .filter(|array| !array.lock().removed)
            .count()
    }

    fn total_semaphores(&self) -> usize {
        self.semid_arrays
            .values()
            .map(|array| {
                let array = array.lock();
                if array.removed { 0 } else { array.nsems() }
            })
            .sum()
    }

    fn max_active_index(&self) -> isize {
        // Linux `ipc_get_maxidx()` reports -1 for an empty table and the
        // syscalls map that to 0.
        self.ids.max_index().max(0) as isize
    }
}

/// Applies a final `sem_undo` list without sleeping. Removed arrays and stale
/// semaphore indexes are ignored exactly as Linux ignores undo entries whose
/// target disappeared before the owner exited.
pub(crate) fn apply_sem_undo(manager: &Mutex<SemManager>, undo: &mut SemUndo) {
    let entries = core::mem::take(&mut undo.entries);
    let mut wake = Vec::new();
    let state = manager.lock();
    for ((semid, semnum), adjustment) in entries {
        let Some(array) = state.get_array_by_index(ipcid_to_idx(semid)) else {
            continue;
        };
        let mut array = array.lock();
        if array.removed {
            continue;
        }
        // Linux `freeary()` drops every undo entry that targeted an array it
        // removed, and `exit_sem()` re-checks the array it looked the undo
        // structure up against.  A new array can be created at the same index
        // with the same sequence number, so the array's identity - not just
        // the identifier - has to match before an adjustment is applied.
        if adjustment.serial != array.serial {
            continue;
        }
        let changed = {
            let Some(sem) = array.sems.get_mut(semnum as usize) else {
                continue;
            };
            if adjustment.generation != sem.undo_generation {
                continue;
            }
            let value = (sem.value as i32 + adjustment.value).clamp(0, SEMVMX as i32) as u16;
            if value == sem.value {
                false
            } else {
                sem.value = value;
                true
            }
        };
        if changed {
            array.mark_changed();
            wake.push(array.waiters.clone());
        }
    }
    drop(state);
    for waiters in wake {
        notify_sem_waiters(waiters);
    }
}

/// The four ceilings of `/proc/sys/kernel/sem`, read from the caller's IPC
/// namespace.
///
/// Linux keeps them in `struct ipc_namespace` (`ipc/sem.c:249-256`,
/// `ipc/ipc_sysctl.c:145-160`); every namespace therefore starts from the
/// `include/uapi/linux/sem.h:80-85` defaults and any sysctl write is local to
/// it.
fn sem_limit(index: usize) -> usize {
    let limits = current().as_thread().ipc_ns().sem_limits();
    match index {
        0 => limits.0,
        1 => limits.1,
        2 => limits.2,
        _ => limits.3,
    }
}

pub(crate) fn semmni_limit() -> usize {
    sem_limit(3)
}

pub(crate) fn semmsl_limit() -> usize {
    sem_limit(0)
}

pub(crate) fn semmns_limit() -> usize {
    sem_limit(1)
}

pub(crate) fn semopm_limit() -> usize {
    sem_limit(2)
}

pub(crate) fn set_sem_limits(
    semmsl: usize,
    semmns: usize,
    semopm: usize,
    semmni: usize,
) -> AxResult<()> {
    current()
        .as_thread()
        .ipc_ns()
        .set_sem_limits(semmsl, semmns, semopm, semmni)
}

pub(crate) fn sem_limits_string() -> String {
    let semmsl = semmsl_limit();
    let semmni = semmni_limit();
    let semmns = semmns_limit();
    alloc::format!("{} {} {} {}\n", semmsl, semmns, semopm_limit(), semmni)
}

pub(crate) fn parse_sem_limits(data: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let mut values = data
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| {
            core::str::from_utf8(part)
                .ok()
                .and_then(|it| it.parse::<usize>().ok())
        });
    let semmsl = values.next().flatten()?;
    let semmns = values.next().flatten()?;
    let semopm = values.next().flatten()?;
    let semmni = values.next().flatten()?;
    values
        .next()
        .is_none()
        .then_some((semmsl, semmns, semopm, semmni))
}

pub(crate) fn sem_next_id() -> i32 {
    current()
        .as_thread()
        .ipc_ns()
        .next_sem_id()
        .load(Ordering::Relaxed)
}

pub(crate) fn set_sem_next_id(value: i32) -> AxResult<()> {
    // Linux `ipc/ipc_sysctl.c`: `proc_dointvec_minmax` over `[0, INT_MAX]`,
    // writable only for a task that is `checkpoint_restore_ns_capable()` over
    // the IPC namespace's user namespace.
    if value < 0 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    let ipc_ns = current().as_thread().ipc_ns();
    if !ipc_ns.may_set_next_id() {
        return Err(AxError::from(LinuxError::EPERM));
    }
    ipc_ns.next_sem_id().store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn sysvipc_sem_snapshot() -> String {
    let mut out = String::from(
        "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n",
    );
    let ipc_ns = current().as_thread().ipc_ns();
    let manager = ipc_ns.sem_manager().lock();
    for (index, array) in &manager.semid_arrays {
        let array = array.lock();
        if array.removed {
            continue;
        }
        let ds = array.semid_ds;
        // The table is keyed by index; `ipcs` prints the published identifier.
        let semid = ipcid_compose(*index, ds.sem_perm.seq as i32);
        let _ = writeln!(
            out,
            "{:10} {:10} {:5o} {:10} {:5} {:5} {:5} {:5} {:10} {:10}",
            ds.sem_perm.key,
            semid,
            ds.sem_perm.mode & IPC_MODE_MASK,
            ds.sem_nsems,
            ds.sem_perm.uid,
            ds.sem_perm.gid,
            ds.sem_perm.cuid,
            ds.sem_perm.cgid,
            ds.sem_otime,
            ds.sem_ctime,
        );
    }
    out
}

fn copy_sem_values_to_user<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    values: &[u16],
) -> AxResult<()> {
    vm_write_slice(memory, ptr as *mut u16, values).map_err(map_usercopy_error)?;
    Ok(())
}

fn copy_sem_values_from_user<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    nsems: usize,
) -> AxResult<Vec<u16>> {
    vm_load(memory, ptr as *const u16, nsems).map_err(map_usercopy_error)
}

fn snapshot_setall_values<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    nsems: usize,
) -> AxResult<Vec<u16>> {
    // `vm_load` uses fallible bounded reservation, so the usercopy happens
    // without holding the semaphore-array lock and reports allocation failure
    // as ENOMEM instead of aborting the kernel.
    let values = copy_sem_values_from_user(memory, ptr, nsems)?;
    if values.iter().any(|value| *value as usize > SEMVMX) {
        return Err(AxError::from(LinuxError::ERANGE));
    }
    Ok(values)
}

fn prepare_setall_values<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    array: &Arc<Mutex<SemArray>>,
    context: &IpcAccessContext,
) -> AxResult<Vec<u16>> {
    let nsems = {
        let array_guard = array.lock();
        if array_guard.removed {
            return Err(AxError::from(LinuxError::EINVAL));
        }
        if !array_guard.writable(context) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        array_guard.nsems()
    };
    // The guard above is deliberately out of scope before this call.  Keep
    // the usercopy independent from the semaphore-array lock.
    snapshot_setall_values(memory, ptr, nsems)
}

fn sem_array_is_current(semid: i32, array: &Arc<Mutex<SemArray>>) -> bool {
    let ipc_ns = current().as_thread().ipc_ns();
    let manager = ipc_ns.sem_manager().lock();
    manager
        .get_array_by_index(ipcid_to_idx(semid))
        .is_some_and(|current| Arc::ptr_eq(&current, array))
}

fn notify_sem_waiters(waiters: Arc<axtask::WaitQueue>) {
    if waiters.notify_many(usize::MAX, false) > 0 {
        axtask::yield_now();
    }
}

fn validate_semnum(array: &SemArray, semnum: i32) -> AxResult<usize> {
    if semnum < 0 || semnum as usize >= array.nsems() {
        Err(AxError::from(LinuxError::EINVAL))
    } else {
        Ok(semnum as usize)
    }
}

pub fn sys_semget(key: i32, nsems: i32, semflg: i32) -> AxResult<isize> {
    // Linux rejects negative nsems before looking up a keyed array.  Zero is
    // valid only for an existing set and remains handled by that branch.
    if nsems < 0 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    let current = current();
    let ipc_ns = current.as_thread().ipc_ns();
    let context = IpcAccessContext::for_ipc_namespace(current.as_thread().current_cred(), &ipc_ns);
    let current_uid = context.effective_uid_raw();
    let current_gid = context.effective_gid_raw();
    let create = (semflg & IPC_CREAT) != 0;
    let excl = (semflg & IPC_EXCL) != 0;

    let mut manager = ipc_ns.sem_manager().lock();
    if key != IPC_PRIVATE
        && let Some(semid) = manager.get_semid_by_key(key)
    {
        // Linux `ipc/util.c:ipcget_public()` rejects an exclusive create
        // before it resolves the object or checks access.
        if create && excl {
            return Err(AxError::from(LinuxError::EEXIST));
        }
        let array = manager
            .get_array_by_semid(semid)
            .ok_or(AxError::from(LinuxError::ENOENT))?;
        let array = array.lock();
        if array.removed {
            return Err(AxError::from(LinuxError::EIDRM));
        }
        if nsems > 0 && nsems as usize > array.nsems() {
            return Err(AxError::from(LinuxError::EINVAL));
        }
        // On an existing set the permission bits in `semflg` describe the
        // access being requested.  Linux does not turn a write-only or
        // zero-mode lookup into an unconditional read check.
        if !context.allows_requested_mode(&array.semid_ds.sem_perm, semflg as _) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        return Ok(semid as isize);
    }

    if key != IPC_PRIVATE && !create {
        return Err(AxError::from(LinuxError::ENOENT));
    }
    if nsems <= 0 || nsems as usize > semmsl_limit() {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    if manager.total_semaphores().saturating_add(nsems as usize) > semmns_limit() {
        return Err(AxError::from(LinuxError::ENOSPC));
    }
    if manager.active_array_count() >= semmni_limit() {
        return Err(AxError::from(LinuxError::ENOSPC));
    }

    let id = manager.allocate_id(ipc_ns.next_sem_id())?;
    let mut array = SemArray::new(
        id.raw(),
        key,
        nsems as usize,
        (semflg & IPC_MODE_MASK as i32) as _,
        current_uid,
        current_gid,
    );
    array.semid_ds.sem_perm.seq = id.sequence() as _;
    let array = Arc::new(Mutex::new(array));
    manager.insert(key, id.raw(), array);
    Ok(id.raw() as isize)
}

pub fn sys_semctl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    semid: i32,
    semnum: i32,
    cmd: i32,
    arg: usize,
) -> AxResult<isize> {
    let current_task = current();
    let ipc_ns = current_task.as_thread().ipc_ns();
    let context =
        IpcAccessContext::for_ipc_namespace(current_task.as_thread().current_cred(), &ipc_ns);
    // Linux `ksys_semctl()` rejects a negative identifier first, and switches
    // on the raw command: `IPC_64` is a userland-header convention the kernel
    // never inspects, so `semctl(id, 0, IPC_STAT | 0x100, buf)` is EINVAL.
    if semid < 0 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    // `semctl_setval()` validates the value before it resolves the array, so
    // an out-of-range value is ERANGE even for an identifier that names
    // nothing.
    if cmd == SETVAL {
        let value = arg as c_int;
        if value < 0 || value > SEMVMX as c_int {
            return Err(AxError::from(LinuxError::ERANGE));
        }
    }

    if cmd == IPC_INFO || cmd == SEM_INFO {
        let (info, index) = {
            let manager = ipc_ns.sem_manager().lock();
            let info = if cmd == IPC_INFO {
                SemInfo::ipc_info()
            } else {
                SemInfo::sem_info(&manager)
            };
            (info, manager.max_active_index())
        };
        write_sem_info(memory, arg as *mut SemInfo, info)?;
        return Ok(index);
    }
    if cmd == SEM_STAT || cmd == SEM_STAT_ANY {
        // Linux `semctl_stat()`: these two commands take an *index*, and
        // `sem_obtain_object()` resolves it without consulting the sequence
        // number.  They answer with the array's full identifier so a caller
        // iterating by index learns the sequence it must use next.
        let index = ipcid_to_idx(semid);
        let array = ipc_ns
            .sem_manager()
            .lock()
            .get_array_by_index(index)
            .ok_or(AxError::from(LinuxError::EINVAL))?;
        let (snapshot, id) = {
            let array = array.lock();
            // `SEM_STAT_ANY` is the unprivileged probe and skips the mode
            // check; `SEM_STAT` requires read permission.
            if cmd == SEM_STAT && !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            // Linux checks `ipc_valid_object()` after the permission test, so
            // an array that is already gone reports EIDRM rather than EACCES.
            if array.removed {
                return Err(AxError::from(LinuxError::EIDRM));
            }
            (
                array.semid_ds,
                ipcid_compose(index, array.semid_ds.sem_perm.seq as i32),
            )
        };
        write_semid_ds(memory, arg as *mut SemidDs, snapshot)?;
        return Ok(id as isize);
    }

    // Linux `ipc/sem.c:ksys_semctl()` copies the `IPC_SET` record out of
    // userspace *before* `semctl_down()` resolves the identifier:
    //
    // ```c
    // 	case IPC_SET:
    // 		if (copy_semid_from_user(&semid64, p, version))
    // 			return -EFAULT;
    // 		fallthrough;
    // 	case IPC_RMID:
    // 		return semctl_down(ns, semid, cmd, &semid64);
    // ```
    //
    // so a faulting buffer is EFAULT even when the identifier names nothing.
    let set_perm = if cmd == IPC_SET {
        let user_ds = VmPtr::vm_read(arg as *const SemidDs, memory).map_err(map_usercopy_error)?;
        Some((
            user_ds.sem_perm.uid,
            user_ds.sem_perm.gid,
            user_ds.sem_perm.mode,
        ))
    } else {
        None
    };

    let array = {
        let manager = ipc_ns.sem_manager().lock();
        manager
            .get_array_by_semid(semid)
            .ok_or(AxError::from(LinuxError::EINVAL))?
    };
    // SETALL snapshots and validates the complete input before acquiring the
    // array lock.  The lock is reacquired below only to revalidate identity,
    // lifecycle, size, and permissions before atomically applying the values.
    let setall_values = if cmd == SETALL {
        Some(prepare_setall_values(memory, arg, &array, &context)?)
    } else {
        None
    };

    if let Some(values) = setall_values {
        // The manager mapping may have been removed and replaced while the
        // faulting usercopy was in progress.  Do not apply the snapshot to a
        // stale Arc retained across IPC_RMID or an ID reuse.
        if !sem_array_is_current(semid, &array) {
            return Err(AxError::from(LinuxError::EINVAL));
        }

        let mut array = array.lock();
        if array.removed {
            return Err(AxError::from(LinuxError::EIDRM));
        }
        if array.semid != semid || array.nsems() != values.len() {
            return Err(AxError::from(LinuxError::EINVAL));
        }
        if !array.writable(&context) {
            return Err(AxError::from(LinuxError::EACCES));
        }

        let pid = current().as_thread().proc_data.proc.pid() as __kernel_pid_t;
        array.reset_values(&values, pid)?;
        array.mark_changed();
        let waiters = array.waiters.clone();
        drop(array);
        notify_sem_waiters(waiters);
        return Ok(0);
    }

    let mut array = array.lock();
    // Linux `ipc_valid_object()` reports a removed object as EIDRM; the flag
    // is only observable through a reference obtained before the removal.
    if array.removed {
        return Err(AxError::from(LinuxError::EIDRM));
    }

    match cmd {
        IPC_STAT => {
            if !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let snapshot = array.semid_ds;
            drop(array);
            write_semid_ds(memory, arg as *mut SemidDs, snapshot)?;
            Ok(0)
        }
        IPC_SET => {
            let (uid, gid, mode) = set_perm.expect("IPC_SET copied its record before the lookup");
            // Linux `semctl_down()` runs `ipcctl_obtain_check()` - the
            // owner-or-CAP_SYS_ADMIN test - before `ipc_update_perm()`
            // translates the requested owner ids, so a caller that may not
            // control the array is refused with EPERM rather than with the
            // EINVAL an unmappable id would produce.
            if !context.may_control(&array.semid_ds.sem_perm) {
                return Err(AxError::from(LinuxError::EPERM));
            }
            let prepared = context.prepare_permission_update(
                &array.semid_ds.sem_perm,
                context.map_permission_update(uid, gid, mode)?,
            )?;
            prepared.commit(&mut array.semid_ds.sem_perm);
            array.mark_changed();
            Ok(0)
        }
        IPC_RMID => {
            if !context.may_control(&array.semid_ds.sem_perm) {
                return Err(AxError::from(LinuxError::EPERM));
            }
            array.removed = true;
            array.mark_changed();
            let waiters = array.waiters.clone();
            drop(array);
            ipc_ns.sem_manager().lock().remove_semid(semid);
            notify_sem_waiters(waiters);
            Ok(0)
        }
        GETVAL => {
            if !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].value as isize)
        }
        GETPID => {
            if !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].pid as isize)
        }
        GETNCNT => {
            if !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].ncnt as isize)
        }
        GETZCNT => {
            if !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].zcnt as isize)
        }
        GETALL => {
            if !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let mut values = Vec::new();
            values
                .try_reserve_exact(array.sems.len())
                .map_err(|_| AxError::NoMemory)?;
            values.extend(array.sems.iter().map(|sem| sem.value));
            drop(array);
            copy_sem_values_to_user(memory, arg, &values)?;
            Ok(0)
        }
        SETVAL => {
            // `semctl_setval()` order: value range (checked before the array
            // lookup), then semnum bounds, then write permission.
            let value = arg as c_int;
            let index = validate_semnum(&array, semnum)?;
            if !array.writable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let pid = current().as_thread().proc_data.proc.pid() as __kernel_pid_t;
            array.sems[index].reset_value(value as u16, pid)?;
            array.mark_changed();
            let waiters = array.waiters.clone();
            drop(array);
            notify_sem_waiters(waiters);
            Ok(0)
        }
        _ => Err(AxError::from(LinuxError::EINVAL)),
    }
}

enum SemTryResult {
    Ready,
    WouldBlock {
        sem_num: usize,
        wait_zero: bool,
        needed_value: u16,
        /// Linux `perform_atomic_semop()` returns EAGAIN only when the
        /// operation that would block carries `IPC_NOWAIT` itself.
        nowait: bool,
    },
}

fn try_apply_single_semop(
    array: &mut SemArray,
    op: Sembuf,
    pid: __kernel_pid_t,
    undo: &mut SemUndoCheck<'_>,
) -> AxResult<SemTryResult> {
    let index = op.sem_num as usize;
    if index >= array.sems.len() {
        return Err(AxError::from(LinuxError::EFBIG));
    }

    let sem = &mut array.sems[index];
    let value = sem.value as i32;
    let plan = plan_sem_op(abi_sem_buf(&op));
    match op.sem_op {
        delta if delta > 0 => {
            let new_value = value + delta as i32;
            if new_value > SEMVMX as i32 {
                return Err(AxError::from(LinuxError::ERANGE));
            }
            // Linux `perform_atomic_semop()` validates the semaphore value and
            // the deferred adjustment in the same pass and commits only after
            // the whole vector is valid, so a rejected adjustment leaves the
            // semaphore untouched.  A would-block result returns before this
            // check on both kernels.
            undo.check(plan, op.sem_num, op.sem_op)?;
            sem.value = new_value as u16;
        }
        delta if delta < 0 => {
            let amount = -(delta as i32);
            if value < amount {
                return Ok(SemTryResult::WouldBlock {
                    sem_num: index,
                    wait_zero: false,
                    needed_value: amount as u16,
                    nowait: op_has_nowait_flag(op.sem_flg),
                });
            }
            undo.check(plan, op.sem_num, op.sem_op)?;
            sem.value = (value - amount) as u16;
        }
        _ => {
            if value != 0 {
                return Ok(SemTryResult::WouldBlock {
                    sem_num: index,
                    wait_zero: true,
                    needed_value: 0,
                    nowait: op_has_nowait_flag(op.sem_flg),
                });
            }
            undo.check(plan, op.sem_num, op.sem_op)?;
        }
    }

    sem.pid = pid;
    array.semid_ds.sem_otime = ipc_time_secs();
    Ok(SemTryResult::Ready)
}

fn op_has_nowait_flag(flags: i16) -> bool {
    flags & tk_linux_ipc::IPC_NOWAIT as i16 != 0
}

fn try_apply_semops(
    array: &mut SemArray,
    ops: &[Sembuf],
    pid: __kernel_pid_t,
    undo: &mut SemUndoCheck<'_>,
) -> AxResult<SemTryResult> {
    if let [op] = ops {
        return try_apply_single_semop(array, *op, pid, undo);
    }

    let mut values = array.sems.iter().map(|sem| sem.value).collect::<Vec<_>>();
    for op in ops {
        let index = op.sem_num as usize;
        if index >= values.len() {
            return Err(AxError::from(LinuxError::EFBIG));
        }
        let value = values[index] as i32;
        match op.sem_op {
            delta if delta > 0 => {
                let new_value = value + delta as i32;
                if new_value > SEMVMX as i32 {
                    return Err(AxError::from(LinuxError::ERANGE));
                }
                values[index] = new_value as u16;
            }
            delta if delta < 0 => {
                let amount = -(delta as i32);
                if value < amount {
                    return Ok(SemTryResult::WouldBlock {
                        sem_num: index,
                        wait_zero: false,
                        needed_value: amount as u16,
                        nowait: op_has_nowait_flag(op.sem_flg),
                    });
                }
                values[index] = (value - amount) as u16;
            }
            _ => {
                if value != 0 {
                    return Ok(SemTryResult::WouldBlock {
                        sem_num: index,
                        wait_zero: true,
                        needed_value: 0,
                        nowait: op_has_nowait_flag(op.sem_flg),
                    });
                }
            }
        }
        // Linux validates the deferred adjustment in the same pass as the
        // semaphore value, so an operation that would block is reported as
        // such even when a later operation's adjustment is out of range.
        undo.check(plan_sem_op(abi_sem_buf(op)), op.sem_num, op.sem_op)?;
    }

    for (sem, value) in array.sems.iter_mut().zip(values) {
        sem.value = value;
    }
    for op in ops {
        array.sems[op.sem_num as usize].pid = pid;
    }
    array.semid_ds.sem_otime = ipc_time_secs();
    Ok(SemTryResult::Ready)
}

/// Linux `ksys_semtimedop()` copies the relative timeout into kernel memory
/// before it calls `do_semtimedop()`, so an unreadable timeout is EFAULT even
/// when the operation vector is out of range or absent.  The value is not
/// interpreted here: `timespec64_valid()` runs much later, in
/// `__do_semtimedop()`.
fn read_timeout<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timeout: *const timespec,
) -> AxResult<Option<timespec>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let timeout = unsafe {
        VmPtr::vm_read_uninit(timeout, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    Ok(Some(timeout))
}

/// Linux `__do_semtimedop()`: `if (!timespec64_valid(timeout)) return -EINVAL;`
/// - reached only after the operation vector has been copied in.
fn timeout_deadline(timeout: Option<timespec>) -> AxResult<Option<Duration>> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    let tv = timeout.try_into_time_value()?;
    let duration = Duration::from_nanos(tv.as_nanos().min(u64::MAX as u128) as u64);
    Ok(Some(monotonic_duration().saturating_add(duration)))
}

fn add_wait_count(
    array: &Arc<Mutex<SemArray>>,
    blocked_index: usize,
    wait_zero: bool,
) -> WaitCountGuard<'_> {
    {
        let mut array = array.lock();
        if let Some(sem) = array.sems.get_mut(blocked_index) {
            if wait_zero {
                sem.zcnt = sem.zcnt.saturating_add(1);
            } else {
                sem.ncnt = sem.ncnt.saturating_add(1);
            }
        }
    }
    WaitCountGuard {
        array,
        index: blocked_index,
        wait_zero,
    }
}

fn deadline_elapsed(deadline: Option<Duration>) -> bool {
    deadline.is_some_and(|deadline| monotonic_duration() >= deadline)
}

fn sem_wait_ready(
    array: &Arc<Mutex<SemArray>>,
    sem_num: usize,
    wait_zero: bool,
    needed_value: u16,
) -> AxResult<bool> {
    let array = array.lock();
    if array.removed {
        return Err(AxError::from(LinuxError::EIDRM));
    }
    let Some(sem) = array.sems.get(sem_num) else {
        return Ok(true);
    };
    Ok(if wait_zero {
        sem.value == 0
    } else {
        sem.value >= needed_value
    })
}

fn wait_for_sem(
    waiters: Arc<axtask::WaitQueue>,
    deadline: Option<Duration>,
    array: &Arc<Mutex<SemArray>>,
    sem_num: usize,
    wait_zero: bool,
    needed_value: u16,
) -> AxResult<()> {
    let current = current();
    let thread = current.as_thread();
    if sem_wait_ready(array, sem_num, wait_zero, needed_value)? {
        return Ok(());
    }
    if has_pending_syscall_signal(thread) {
        return Err(AxError::Interrupted);
    }
    if deadline_elapsed(deadline) {
        return Err(AxError::from(LinuxError::EAGAIN));
    }
    if deadline.is_none() {
        with_proc_state_hint(ProcStateHint::Interruptible, || {
            waiters.wait_until_interruptible(|| {
                sem_wait_ready(array, sem_num, wait_zero, needed_value).unwrap_or(true)
                    || has_pending_syscall_signal(thread)
            })
        })
        .map_err(AxError::from)?;
        if has_pending_syscall_signal(thread) {
            return Err(AxError::Interrupted);
        }
        sem_wait_ready(array, sem_num, wait_zero, needed_value)?;
        return Ok(());
    }

    let sleep_for = deadline
        .ok_or(AxError::BadState)?
        .saturating_sub(monotonic_duration());
    let timed_out = with_proc_state_hint(ProcStateHint::Interruptible, || {
        waiters.wait_timeout_until_interruptible(sleep_for, || {
            sem_wait_ready(array, sem_num, wait_zero, needed_value).unwrap_or(true)
                || has_pending_syscall_signal(thread)
        })
    })
    .map_err(AxError::from)?;
    if has_pending_syscall_signal(thread) {
        return Err(AxError::Interrupted);
    }
    if sem_wait_ready(array, sem_num, wait_zero, needed_value)? {
        return Ok(());
    }
    if timed_out || deadline_elapsed(deadline) {
        return Err(AxError::from(LinuxError::EAGAIN));
    }
    Ok(())
}

pub fn sys_semop<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    semid: i32,
    sops: *const Sembuf,
    nsops: u32,
) -> AxResult<isize> {
    sys_semtimedop(memory, semid, sops, nsops, core::ptr::null())
}

pub fn sys_semtimedop<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    semid: i32,
    sops: *const Sembuf,
    nsops: u32,
    timeout: *const timespec,
) -> AxResult<isize> {
    // Linux `ipc/sem.c` argument order.  `ksys_semtimedop()` fetches the
    // relative timeout, `do_semtimedop()` bounds and copies the operation
    // vector, and only `__do_semtimedop()` rejects the identifier and an
    // invalid `timespec`:
    //
    // ```c
    // 	if (timeout) {
    // 		struct timespec64 ts;
    // 		if (get_timespec64(&ts, timeout))
    // 			return -EFAULT;
    // 		return do_semtimedop(semid, tsops, nsops, &ts);
    // 	}
    // 	return do_semtimedop(semid, tsops, nsops, NULL);
    //
    // 	if (nsops > ns->sc_semopm)
    // 		return -E2BIG;
    // 	if (nsops < 1)
    // 		return -EINVAL;
    // 	if (copy_from_user(sops, tsops, nsops * sizeof(*tsops))) {
    // 		ret =  -EFAULT;
    // 		goto out_free;
    // 	}
    // 	ret = __do_semtimedop(semid, sops, nsops, timeout, ns);
    // ```
    //
    // The parameter is Linux's `unsigned int nsops`, so the register is
    // truncated to 32 bits before the bound and the copy agree on a count.
    let raw_timeout = read_timeout(memory, timeout)?;

    if nsops as usize > semopm_limit() {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    if nsops == 0 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    let ops = vm_load(memory, sops, nsops as usize).map_err(map_usercopy_error)?;

    // `__do_semtimedop()` repeats both bounds, then rejects a negative
    // identifier.  Because the vector has already been copied, a faulting
    // `tsops` is EFAULT even for a negative `semid`.
    if nsops == 0 || semid < 0 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    if nsops as usize > semopm_limit() {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    let deadline = timeout_deadline(raw_timeout)?;
    let current = current();
    let proc_data = &current.as_thread().proc_data;
    let ipc_ns = current.as_thread().ipc_ns();
    let context = IpcAccessContext::for_ipc_namespace(current.as_thread().current_cred(), &ipc_ns);
    let current_pid = proc_data.proc.pid() as __kernel_pid_t;
    let needs_write = ops.iter().any(|op| op.sem_op != 0);
    let sem_undo = current.as_thread().sem_undo();

    let array = {
        let manager = ipc_ns.sem_manager().lock();
        manager
            .get_array_by_semid(semid)
            .ok_or(AxError::from(LinuxError::EINVAL))?
    };
    // Linux `__do_semtimedop()` bounds the vector against the array with
    // EFBIG *before* `ipcperms()`, so an operation that names a semaphore
    // outside the set reports EFBIG even when the caller also lacks write
    // permission.  `sem_nsems` is fixed at creation, so one check suffices.
    if highest_sem_num(&ops) as usize >= array.lock().nsems() {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    let mut wait_guard = None;
    let mut wait_key = None;

    loop {
        let wait_state = {
            let mut array = array.lock();
            if array.removed {
                return Err(AxError::from(LinuxError::EIDRM));
            }
            let has_permission = if needs_write {
                array.writable(&context)
            } else {
                array.readable(&context)
            };
            if !has_permission {
                return Err(AxError::from(LinuxError::EACCES));
            }

            // Keep the undo list locked across the state transition.  Apart
            // from making allocation failure atomic, this serializes shared
            // CLONE_SYSVSEM accounting with any sibling that updates the same
            // undo list.
            let mut undo_guard = ops
                .iter()
                .any(|op| op.sem_flg & SEM_UNDO != 0)
                .then(|| sem_undo.undo().lock());
            if let Some(undo) = undo_guard.as_deref_mut() {
                undo.as_mut()
                    .ok_or(AxError::BadState)?
                    .prepare_records(semid, &ops)?;
            }

            let mut undo_check = SemUndoCheck {
                undo: undo_guard.as_deref().and_then(|undo| undo.as_ref()),
                semid,
                serial: array.serial,
                staged: Vec::new(),
            };
            match try_apply_semops(&mut array, &ops, current_pid, &mut undo_check)? {
                SemTryResult::Ready => {
                    if let Some(undo) = undo_guard.as_deref_mut() {
                        let undo = undo.as_mut().ok_or(AxError::BadState)?;
                        for op in ops.iter() {
                            if !plan_sem_op(abi_sem_buf(op)).records_undo() {
                                continue;
                            }
                            undo.record(
                                semid,
                                op.sem_num,
                                op.sem_op,
                                array.sems[op.sem_num as usize].undo_generation,
                                array.serial,
                            )?;
                        }
                    }
                    let waiters = array.waiters.clone();
                    drop(array);
                    notify_sem_waiters(waiters);
                    break;
                }
                SemTryResult::WouldBlock {
                    sem_num,
                    wait_zero,
                    needed_value,
                    nowait,
                } => {
                    // Linux reports EAGAIN from the flag of the operation that
                    // would block, not from the vector as a whole.
                    if nowait {
                        return Err(AxError::from(LinuxError::EAGAIN));
                    }
                    (array.waiters.clone(), sem_num, wait_zero, needed_value)
                }
            }
        };

        let (waiters, sem_num, wait_zero, needed_value) = wait_state;
        let key = (sem_num, wait_zero);
        if wait_key != Some(key) {
            drop(wait_guard.take());
            wait_guard = Some(add_wait_count(&array, sem_num, wait_zero));
            wait_key = Some(key);
        }
        wait_for_sem(waiters, deadline, &array, sem_num, wait_zero, needed_value)?;
    }
    drop(wait_guard);
    Ok(0)
}

#[cfg(test)]
mod setall_snapshot_tests {
    use alloc::{sync::Arc, vec};
    use core::{
        mem::MaybeUninit,
        ops::Range,
        sync::atomic::{AtomicBool, Ordering},
    };

    use tk_linux_usercopy::{UserCopyError, VmResult};

    use super::*;
    use crate::task::{Cred, UserNamespace};

    struct LockProbeMemory {
        array: Arc<Mutex<SemArray>>,
        bytes: Vec<u8>,
        saw_unlocked: Arc<AtomicBool>,
    }

    impl LockProbeMemory {
        fn range(&self, start: usize, len: usize) -> Result<Range<usize>, UserCopyError> {
            let end = start.checked_add(len).ok_or(UserCopyError::BadAddress)?;
            (end <= self.bytes.len())
                .then_some(start..end)
                .ok_or(UserCopyError::BadAddress)
        }
    }

    // SAFETY: LockProbeMemory bounds-checks the opaque address and initializes
    // every destination byte before returning a successful read.
    unsafe impl UserMemory for LockProbeMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            if self.array.try_lock().is_some() {
                self.saw_unlocked.store(true, Ordering::Relaxed);
            }
            let range = self.range(start, dst.len())?;
            for (output, input) in dst.iter_mut().zip(&self.bytes[range]) {
                output.write(*input);
            }
            Ok(())
        }

        fn write(&mut self, _start: usize, _src: &[u8]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }
    }

    /// Regression: the array table used to be keyed by the published
    /// identifier with nothing validating the sequence.
    /// Linux `perform_atomic_semop()` validates the deferred adjustment in the
    /// same pass as the semaphore value and commits only once the whole vector
    /// is valid: a rejected adjustment must leave the value unchanged, which
    /// the single-operation fast path used to violate.
    #[test]
    fn rejected_undo_adjustment_leaves_a_single_operation_unapplied() {
        let _context = crate::test_support::scheduler_test_context();
        let semid = ipcid_compose(2, 0);
        let mut array = SemArray::new(semid, 1, 1, 0o600, 0, 0);
        array.sems[0].value = 1;
        let serial = array.serial;
        let generation = array.sems[0].undo_generation;

        let mut undo = SemUndo::new();
        // `-SEMAEM - 1` is the lowest legal adjustment; two increments reach it
        // exactly, so a third increment is out of range.
        undo.record(semid, 0, SEMVMX as i16, generation, serial)
            .unwrap();
        undo.record(semid, 0, 1, generation, serial).unwrap();
        assert_eq!(undo.prior(semid, serial, 0), -(SEMAEM as i32) - 1);

        let ops = [Sembuf {
            sem_num: 0,
            sem_op: 1,
            sem_flg: SEM_UNDO,
        }];
        let mut check = SemUndoCheck {
            undo: Some(&undo),
            semid,
            serial,
            staged: Vec::new(),
        };
        assert!(matches!(
            try_apply_semops(&mut array, &ops, 0, &mut check),
            Err(_)
        ));
        assert_eq!(array.sems[0].value, 1);
    }

    /// The bound `semop` applies to the whole vector before it checks
    /// permission: the highest semaphore number, not the first one.
    #[test]
    fn highest_semaphore_number_bounds_the_operation_vector() {
        let op = |sem_num, sem_op| Sembuf {
            sem_num,
            sem_op,
            sem_flg: 0,
        };
        assert_eq!(highest_sem_num(&[op(0, 1)]), 0);
        assert_eq!(highest_sem_num(&[op(0, 1), op(4, 1), op(2, 1)]), 4);
        // `nsops < 1` is rejected before the bound is consulted, so the empty
        // vector's placeholder cannot be observed.
        assert_eq!(highest_sem_num(&[]), 0);
    }

    #[test]
    fn array_lookup_validates_the_sequence_and_stat_uses_the_index() {
        let _context = crate::test_support::scheduler_test_context();
        let mut manager = SemManager::new();
        let index = 5;
        let sequence = 3;
        let published = ipcid_compose(index, sequence);
        assert_eq!(
            manager.allocate_id(&AtomicI32::new(published)),
            Ok(IpcId::from_parts(index, sequence))
        );
        let mut array = SemArray::new(published, 1, 2, 0o600, 0, 0);
        array.semid_ds.sem_perm.seq = sequence as _;
        manager.insert(1, published, Arc::new(Mutex::new(array)));

        assert!(manager.get_array_by_semid(published).is_some());
        assert!(manager.get_array_by_semid(index).is_none());
        assert!(
            manager
                .get_array_by_semid(ipcid_compose(index, sequence + 1))
                .is_none()
        );
        assert!(manager.get_array_by_index(index).is_some());
        assert_eq!(manager.max_active_index(), index as isize);

        manager.remove_semid(published);
        assert!(manager.get_array_by_index(index).is_none());
        assert_eq!(manager.max_active_index(), 0);
    }

    #[test]
    fn setval_clears_all_prior_owners_only_for_the_selected_semaphore() {
        let _context = crate::test_support::scheduler_test_context();
        let array = Arc::new(Mutex::new(SemArray::new(1, 1, 2, 0o600, 0, 0)));
        let serial = array.lock().serial;
        let manager = Mutex::new(SemManager::new());
        manager.lock().insert(1, 1, array.clone());
        let mut first = SemUndo::new();
        let mut second = SemUndo::new();
        first.record(1, 0, -2, 0, serial).unwrap();
        first.record(1, 1, -3, 0, serial).unwrap();
        second.record(1, 0, -4, 0, serial).unwrap();
        second.record(1, 1, -5, 0, serial).unwrap();
        array.lock().sems[0].reset_value(20, 1).unwrap();
        apply_sem_undo(&manager, &mut first);
        assert_eq!(array.lock().sems[0].value, 20);
        assert_eq!(array.lock().sems[1].value, 3);
        // A new operation replaces, rather than combines with, the cleared
        // adjustment in a surviving process's undo list.
        let generation = array.lock().sems[0].undo_generation;
        second.record(1, 0, -7, generation, serial).unwrap();
        apply_sem_undo(&manager, &mut second);
        assert_eq!(array.lock().sems[0].value, 27);
        assert_eq!(array.lock().sems[1].value, 8);
    }

    #[test]
    fn setall_clears_every_prior_undo_adjustment() {
        let _context = crate::test_support::scheduler_test_context();
        let array = Arc::new(Mutex::new(SemArray::new(1, 1, 2, 0o600, 0, 0)));
        let serial = array.lock().serial;
        let manager = Mutex::new(SemManager::new());
        manager.lock().insert(1, 1, array.clone());
        let mut undo = SemUndo::new();
        undo.record(1, 0, -2, 0, serial).unwrap();
        undo.record(1, 1, -3, 0, serial).unwrap();
        array.lock().reset_values(&[10, 20], 1).unwrap();
        apply_sem_undo(&manager, &mut undo);
        assert_eq!(array.lock().sems[0].value, 10);
        assert_eq!(array.lock().sems[1].value, 20);
    }

    /// Regression: `IPC_RMID` followed by `semget()` that recreates the very
    /// same semid used to apply the retired owner's pending adjustments to the
    /// new array, corrupting semaphores the process never touched.
    ///
    /// Linux closes this in two places: `freeary()` clears every undo entry
    /// that named the array it removed, and `exit_sem()` re-looks-up the undo
    /// structure after re-obtaining the object, because the recreated set "can
    /// be the same".  TheKernel's undo lists hang off the thread, so the array
    /// instance carries a never-reused serial that every adjustment records
    /// and `apply_sem_undo` re-checks.
    #[test]
    fn undo_adjustment_never_reaches_an_array_that_reused_the_identifier() {
        let _context = crate::test_support::scheduler_test_context();
        // Both arrays publish the identical identifier, sequence included.
        let semid = ipcid_compose(1, 7);
        let retired = Arc::new(Mutex::new(SemArray::new(semid, 1, 2, 0o600, 0, 0)));
        let retired_serial = retired.lock().serial;
        let manager = Mutex::new(SemManager::new());
        manager.lock().insert(1, semid, retired.clone());

        let mut undo = SemUndo::new();
        undo.record(semid, 0, -3, 0, retired_serial).unwrap();
        undo.record(semid, 1, -4, 0, retired_serial).unwrap();
        // Linux stores `semadj - sem_op`, so a decrement of 3 is recorded as
        // the +3 that restores the semaphore on exit.
        assert_eq!(undo.prior(semid, retired_serial, 0), 3);

        // IPC_RMID: the array leaves the table and is marked removed.
        retired.lock().removed = true;
        manager.lock().remove_semid(semid);

        // The successor takes over the same index and the same sequence.
        let successor = Arc::new(Mutex::new(SemArray::new(semid, 1, 2, 0o600, 0, 0)));
        assert_ne!(successor.lock().serial, retired_serial);
        manager.lock().insert(1, semid, successor.clone());
        successor.lock().sems[0].value = 9;
        successor.lock().sems[1].value = 9;

        apply_sem_undo(&manager, &mut undo);

        assert_eq!(successor.lock().sems[0].value, 9);
        assert_eq!(successor.lock().sems[1].value, 9);
        // The adjustment is inert, not merely skipped: it reads as zero
        // against the successor, exactly as Linux's freshly zeroed `semadj`.
        assert_eq!(undo.prior(semid, successor.lock().serial, 0), 0);
    }

    #[test]
    fn exhausted_setval_does_not_wrap_or_change_semaphore() {
        let mut semaphore = Semaphore::new();
        semaphore.value = 7;
        semaphore.pid = 12;
        semaphore.undo_generation = u64::MAX;
        assert_eq!(semaphore.reset_value(20, 99), Err(AxError::OutOfRange));
        assert_eq!(semaphore.value, 7);
        assert_eq!(semaphore.pid, 12);
        assert_eq!(semaphore.undo_generation, u64::MAX);
    }

    #[test]
    fn failed_setall_preserves_every_value_and_undo_generation() {
        let mut array = SemArray::new(1, 1, 2, 0o600, 0, 0);
        array.sems[0].value = 7;
        array.sems[0].undo_generation = 3;
        array.sems[1].value = 8;
        array.sems[1].undo_generation = u64::MAX;
        assert_eq!(array.reset_values(&[20], 99), Err(AxError::InvalidInput));
        assert_eq!(array.reset_values(&[20, 30], 99), Err(AxError::OutOfRange));
        assert_eq!(array.sems[0].value, 7);
        assert_eq!(array.sems[0].undo_generation, 3);
        assert_eq!(array.sems[0].pid, 0);
        assert_eq!(array.sems[1].value, 8);
        assert_eq!(array.sems[1].undo_generation, u64::MAX);
        assert_eq!(array.sems[1].pid, 0);
    }

    #[test]
    fn setall_snapshot_reads_user_values_after_array_unlock() {
        let root_ns = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(root_ns.clone()).unwrap();
        let context = IpcAccessContext::new(actor, root_ns);
        let array = Arc::new(Mutex::new(SemArray::new(1, 1, 2, 0o600, 0, 0)));
        let saw_unlocked = Arc::new(AtomicBool::new(false));
        let mut provider = LockProbeMemory {
            array: array.clone(),
            bytes: vec![1, 0, 2, 0],
            saw_unlocked: saw_unlocked.clone(),
        };
        let mut memory = UserMemoryContext::new(&mut provider);

        let values = prepare_setall_values(&mut memory, 0, &array, &context).unwrap();

        assert_eq!(values, vec![1, 2]);
        assert!(saw_unlocked.load(Ordering::Relaxed));
    }
}
