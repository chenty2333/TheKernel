use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write as _,
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicI32, AtomicUsize, Ordering},
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
use thekernel_linux_ipc::{SemBuf as AbiSemBuf, plan_sem_op};
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load, vm_write_slice,
};

use super::{
    GETALL, GETNCNT, GETPID, GETVAL, GETZCNT, IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID,
    IPC_SET, IPC_STAT, IpcAccess, IpcAccessContext, IpcPerm, IpcPermissionUpdateRequest, SEM_INFO,
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
pub const SEMMNI: usize = 128;
pub const SEMMNS: usize = SEMMSL * SEMMNI;
pub const SEMOPM: usize = 500;
pub const SEMVMX: usize = 32767;
const SEMAEM: usize = SEMVMX;
const SEMUME: usize = SEMOPM;
const SEMUSZ: usize = 20;

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

    /// Reserve every new undo key an operation can commit before the
    /// semaphore values are changed.  `semop` is atomic: an allocation or
    /// SEMUME failure must not be observed after any member of the operation
    /// vector has taken effect.  The caller keeps this list locked through
    /// `record`, so the reservation cannot be consumed by a CLONE_SYSVSEM
    /// sibling in between.
    fn prepare_records(&mut self, semid: i32, ops: &[Sembuf]) -> AxResult<()> {
        let mut additional = 0usize;
        for (index, op) in ops.iter().enumerate() {
            if op.sem_flg & SEM_UNDO == 0 || op.sem_op == 0 {
                continue;
            }
            let key = (semid, op.sem_num);
            if self.entries.contains_key(&key)
                || ops[..index].iter().any(|prior| {
                    prior.sem_flg & SEM_UNDO != 0
                        && prior.sem_op != 0
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
    /// operation. Linux clamps an adjustment instead of overflowing it.
    pub(crate) fn record(
        &mut self,
        semid: i32,
        semnum: u16,
        sem_op: i16,
        generation: u64,
    ) -> AxResult<()> {
        if sem_op == 0 {
            return Ok(());
        }
        let delta = -(sem_op as i32);
        let key = (semid, semnum);
        let prior = self
            .entries
            .get(&key)
            .filter(|entry| entry.generation == generation)
            .map_or(0, |entry| entry.value);
        let next = prior
            .saturating_add(delta)
            .clamp(-(SEMAEM as i32), SEMAEM as i32);
        if !self.entries.contains_key(&key) {
            if self.entries.len() >= SEMUME {
                return Err(AxError::from(LinuxError::ENOSPC));
            }
            // `sys_semtimedop` has already reserved this exact key while the
            // same undo mutex was held.  Do not perform a fallible allocation
            // after mutating the semaphore array: that would break semop's
            // all-or-nothing contract.
        }
        self.entries.insert(
            key,
            SemAdjustment {
                value: next,
                generation,
            },
        );
        Ok(())
    }
}

impl Default for SemUndo {
    fn default() -> Self {
        Self::new()
    }
}

static SEM_MNI_LIMIT: AtomicUsize = AtomicUsize::new(SEMMNI);
static SEM_MSL_LIMIT: AtomicUsize = AtomicUsize::new(SEMMSL);
static SEM_MNS_LIMIT: AtomicUsize = AtomicUsize::new(SEMMNS);
static SEM_OPM_LIMIT: AtomicUsize = AtomicUsize::new(SEMOPM);

fn ipc_time_secs() -> __kernel_time_t {
    wall_time().as_secs() as __kernel_time_t
}

fn wall_time_duration() -> Duration {
    let now = wall_time();
    Duration::new(now.as_secs(), now.subsec_nanos())
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct SemidDs {
    pub sem_perm: IpcPerm,
    pub sem_otime: __kernel_time_t,
    pub sem_ctime: __kernel_time_t,
    pub sem_nsems: c_ulong,
    pub unused3: c_ulong,
    pub unused4: c_ulong,
}

// These System V semaphore records contain Linux ABI padding through their
// embedded `IpcPerm`.  Keep the x86_64 layout checked and serialize a zeroed
// copy for output so implicit alignment bytes never escape to userspace.
const _: () = {
    assert!(align_of::<IpcPerm>() == 8);
    assert!(size_of::<IpcPerm>() == 48);
    assert!(offset_of!(IpcPerm, key) == 0);
    assert!(offset_of!(IpcPerm, mode) == 20);
    assert!(offset_of!(IpcPerm, unused0) == 32);
    assert!(offset_of!(IpcPerm, unused1) == 40);
    assert!(align_of::<SemidDs>() == 8);
    assert!(size_of::<SemidDs>() == 88);
    assert!(offset_of!(SemidDs, sem_perm) == 0);
    assert!(offset_of!(SemidDs, sem_otime) == 48);
    assert!(offset_of!(SemidDs, sem_ctime) == 56);
    assert!(offset_of!(SemidDs, sem_nsems) == 64);
    assert!(offset_of!(SemidDs, unused3) == 72);
    assert!(offset_of!(SemidDs, unused4) == 80);
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
    result.sem_ctime = value.sem_ctime;
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
            sem_ctime: ipc_time_secs(),
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
    semid_arrays: BTreeMap<i32, Arc<Mutex<SemArray>>>,
}

impl SemManager {
    pub(crate) const fn new() -> Self {
        Self {
            key_semid: BTreeMap::new(),
            semid_arrays: BTreeMap::new(),
        }
    }

    fn get_semid_by_key(&self, key: i32) -> Option<i32> {
        self.key_semid.get(&key).copied()
    }

    fn get_array_by_semid(&self, semid: i32) -> Option<Arc<Mutex<SemArray>>> {
        self.semid_arrays.get(&semid).cloned()
    }

    fn insert(&mut self, key: i32, semid: i32, array: Arc<Mutex<SemArray>>) {
        if key != IPC_PRIVATE {
            self.key_semid.insert(key, semid);
        }
        self.semid_arrays.insert(semid, array);
    }

    fn remove_semid(&mut self, semid: i32) {
        self.key_semid.retain(|_, value| *value != semid);
        self.semid_arrays.remove(&semid);
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
        self.semid_arrays
            .iter()
            .filter_map(|(semid, array)| (!array.lock().removed).then_some(*semid as isize))
            .max()
            .unwrap_or(0)
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
        let Some(array) = state.get_array_by_semid(semid) else {
            continue;
        };
        let mut array = array.lock();
        if array.removed {
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

fn allocate_sem_id(manager: &SemManager, cursor: &AtomicI32) -> AxResult<i32> {
    let desired = cursor.swap(-1, Ordering::Relaxed);
    allocate_ipc_id(
        cursor,
        (desired >= 0).then_some(desired),
        manager.semid_arrays.len(),
        |id| manager.semid_arrays.contains_key(&id),
    )
}

pub(crate) fn semmni_limit() -> usize {
    SEM_MNI_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn semmsl_limit() -> usize {
    SEM_MSL_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn semmns_limit() -> usize {
    SEM_MNS_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn semopm_limit() -> usize {
    SEM_OPM_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn set_sem_limits(semmsl: usize, semmns: usize, semopm: usize, semmni: usize) {
    let semmni = semmni.max(1);
    let semmsl = semmsl.max(1);
    let semmns = semmns.max(1);
    SEM_MNI_LIMIT.store(semmni, Ordering::Relaxed);
    SEM_MSL_LIMIT.store(semmsl, Ordering::Relaxed);
    SEM_MNS_LIMIT.store(semmns, Ordering::Relaxed);
    SEM_OPM_LIMIT.store(semopm.max(1), Ordering::Relaxed);
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
    if value < -1 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    current()
        .as_thread()
        .ipc_ns()
        .next_sem_id()
        .store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn sysvipc_sem_snapshot() -> String {
    let mut out = String::from(
        "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n",
    );
    let ipc_ns = current().as_thread().ipc_ns();
    let manager = ipc_ns.sem_manager().lock();
    for (semid, array) in &manager.semid_arrays {
        let array = array.lock();
        if array.removed {
            continue;
        }
        let ds = array.semid_ds;
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
        .semid_arrays
        .get(&semid)
        .is_some_and(|current| Arc::ptr_eq(current, array))
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

fn strip_ipc64(cmd: i32) -> i32 {
    cmd & !0x100
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

    let semid = allocate_sem_id(&manager, ipc_ns.next_sem_id())?;
    let mut array = SemArray::new(
        semid,
        key,
        nsems as usize,
        (semflg & IPC_MODE_MASK as i32) as _,
        current_uid,
        current_gid,
    );
    array.semid_ds.sem_perm.seq = ipc_ns.next_sequence();
    let array = Arc::new(Mutex::new(array));
    manager.insert(key, semid, array);
    Ok(semid as isize)
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
    let cmd = strip_ipc64(cmd);

    if cmd == IPC_INFO {
        let manager = ipc_ns.sem_manager().lock();
        write_sem_info(memory, arg as *mut SemInfo, SemInfo::ipc_info())?;
        return Ok(manager.max_active_index());
    }
    if cmd == SEM_INFO {
        let manager = ipc_ns.sem_manager().lock();
        write_sem_info(memory, arg as *mut SemInfo, SemInfo::sem_info(&manager))?;
        return Ok(manager.max_active_index());
    }
    if cmd == SEM_STAT || cmd == SEM_STAT_ANY {
        let manager = ipc_ns.sem_manager().lock();
        let array = manager
            .get_array_by_semid(semid)
            .ok_or(AxError::from(LinuxError::EINVAL))?;
        let array = array.lock();
        if array.removed {
            return Err(AxError::from(LinuxError::EINVAL));
        }
        if cmd == SEM_STAT && !array.readable(&context) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        write_semid_ds(memory, arg as *mut SemidDs, array.semid_ds)?;
        return Ok(array.semid as isize);
    }

    let array = {
        let manager = ipc_ns.sem_manager().lock();
        manager
            .get_array_by_semid(semid)
            .ok_or(AxError::from(LinuxError::EINVAL))?
    };
    let set_request: Option<IpcPermissionUpdateRequest> = if cmd == IPC_SET {
        let user_ds = VmPtr::vm_read(arg as *const SemidDs, memory).map_err(map_usercopy_error)?;
        Some(context.map_permission_update(
            user_ds.sem_perm.uid,
            user_ds.sem_perm.gid,
            user_ds.sem_perm.mode,
        )?)
    } else {
        None
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
        if array.removed || array.semid != semid || array.nsems() != values.len() {
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
    if array.removed {
        return Err(AxError::from(LinuxError::EINVAL));
    }

    match cmd {
        IPC_STAT => {
            if !array.readable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            write_semid_ds(memory, arg as *mut SemidDs, array.semid_ds)?;
            Ok(0)
        }
        IPC_SET => {
            let prepared = context.prepare_permission_update(
                &array.semid_ds.sem_perm,
                set_request.expect("IPC_SET request was prepared before locking"),
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
            let values = array.sems.iter().map(|sem| sem.value).collect::<Vec<_>>();
            drop(array);
            copy_sem_values_to_user(memory, arg, &values)?;
            Ok(0)
        }
        SETVAL => {
            if !array.writable(&context) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let value = arg as c_int;
            if value < 0 || value as usize > SEMVMX {
                return Err(AxError::from(LinuxError::ERANGE));
            }
            let index = validate_semnum(&array, semnum)?;
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
    },
}

fn validate_semop_flags(flags: i16) -> AxResult<()> {
    // The ABI crate owns the complete `sembuf` flag grammar, including
    // SEM_UNDO.  Adjustment ownership is installed after the operation has
    // committed as one atomic semaphore-array transaction.
    plan_sem_op(AbiSemBuf {
        num: 0,
        op: 1,
        flags,
    })
    .map(|_| ())
    .map_err(|_| AxError::InvalidInput)
}

fn try_apply_single_semop(
    array: &mut SemArray,
    op: Sembuf,
    pid: __kernel_pid_t,
) -> AxResult<SemTryResult> {
    let index = op.sem_num as usize;
    if index >= array.sems.len() {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    validate_semop_flags(op.sem_flg)?;

    let sem = &mut array.sems[index];
    let value = sem.value as i32;
    match op.sem_op {
        op if op > 0 => {
            let new_value = value + op as i32;
            if new_value > SEMVMX as i32 {
                return Err(AxError::from(LinuxError::ERANGE));
            }
            sem.value = new_value as u16;
        }
        op if op < 0 => {
            let delta = -(op as i32);
            if value < delta {
                return Ok(SemTryResult::WouldBlock {
                    sem_num: index,
                    wait_zero: false,
                    needed_value: delta as u16,
                });
            }
            sem.value = (value - delta) as u16;
        }
        _ => {
            if value != 0 {
                return Ok(SemTryResult::WouldBlock {
                    sem_num: index,
                    wait_zero: true,
                    needed_value: 0,
                });
            }
        }
    }

    sem.pid = pid;
    array.semid_ds.sem_otime = ipc_time_secs();
    Ok(SemTryResult::Ready)
}

fn try_apply_semops(
    array: &mut SemArray,
    ops: &[Sembuf],
    pid: __kernel_pid_t,
) -> AxResult<SemTryResult> {
    if let [op] = ops {
        return try_apply_single_semop(array, *op, pid);
    }

    let mut values = array.sems.iter().map(|sem| sem.value).collect::<Vec<_>>();
    for op in ops {
        let index = op.sem_num as usize;
        if index >= values.len() {
            return Err(AxError::from(LinuxError::EFBIG));
        }
        validate_semop_flags(op.sem_flg)?;
        let value = values[index] as i32;
        match op.sem_op {
            op if op > 0 => {
                let new_value = value + op as i32;
                if new_value > SEMVMX as i32 {
                    return Err(AxError::from(LinuxError::ERANGE));
                }
                values[index] = new_value as u16;
            }
            op if op < 0 => {
                let delta = -(op as i32);
                if value < delta {
                    return Ok(SemTryResult::WouldBlock {
                        sem_num: index,
                        wait_zero: false,
                        needed_value: delta as u16,
                    });
                }
                values[index] = (value - delta) as u16;
            }
            _ => {
                if value != 0 {
                    return Ok(SemTryResult::WouldBlock {
                        sem_num: index,
                        wait_zero: true,
                        needed_value: 0,
                    });
                }
            }
        }
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

fn validate_timeout<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timeout: *const timespec,
) -> AxResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let timeout = unsafe {
        VmPtr::vm_read_uninit(timeout, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let tv = timeout.try_into_time_value()?;
    let duration = Duration::from_nanos(tv.as_nanos().min(u64::MAX as u128) as u64);
    Ok(Some(wall_time_duration().saturating_add(duration)))
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
    deadline.is_some_and(|deadline| wall_time_duration() >= deadline)
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
        .saturating_sub(wall_time_duration());
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

fn op_has_nowait(ops: &[Sembuf]) -> bool {
    ops.iter()
        .any(|op| op.sem_flg & thekernel_linux_ipc::IPC_NOWAIT as i16 != 0)
}

pub fn sys_semop<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    semid: i32,
    sops: *const Sembuf,
    nsops: usize,
) -> AxResult<isize> {
    sys_semtimedop(memory, semid, sops, nsops, core::ptr::null())
}

pub fn sys_semtimedop<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    semid: i32,
    sops: *const Sembuf,
    nsops: usize,
    timeout: *const timespec,
) -> AxResult<isize> {
    if nsops == 0 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    if nsops > semopm_limit() {
        return Err(AxError::from(LinuxError::E2BIG));
    }

    let deadline = validate_timeout(memory, timeout)?;
    let ops = vm_load(memory, sops, nsops).map_err(map_usercopy_error)?;
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

            match try_apply_semops(&mut array, &ops, current_pid)? {
                SemTryResult::Ready => {
                    if let Some(undo) = undo_guard.as_deref_mut() {
                        let undo = undo.as_mut().ok_or(AxError::BadState)?;
                        for op in ops.iter().filter(|op| op.sem_flg & SEM_UNDO != 0) {
                            undo.record(
                                semid,
                                op.sem_num,
                                op.sem_op,
                                array.sems[op.sem_num as usize].undo_generation,
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
                } => {
                    if op_has_nowait(&ops) {
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

    use thekernel_linux_usercopy::{UserCopyError, VmResult};

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

    #[test]
    fn setval_clears_all_prior_owners_only_for_the_selected_semaphore() {
        let _context = crate::test_support::scheduler_test_context();
        let array = Arc::new(Mutex::new(SemArray::new(1, 1, 2, 0o600, 0, 0)));
        let manager = Mutex::new(SemManager::new());
        manager.lock().insert(1, 1, array.clone());
        let mut first = SemUndo::new();
        let mut second = SemUndo::new();
        first.record(1, 0, -2, 0).unwrap();
        first.record(1, 1, -3, 0).unwrap();
        second.record(1, 0, -4, 0).unwrap();
        second.record(1, 1, -5, 0).unwrap();
        array.lock().sems[0].reset_value(20, 1).unwrap();
        apply_sem_undo(&manager, &mut first);
        assert_eq!(array.lock().sems[0].value, 20);
        assert_eq!(array.lock().sems[1].value, 3);
        // A new operation replaces, rather than combines with, the cleared
        // adjustment in a surviving process's undo list.
        let generation = array.lock().sems[0].undo_generation;
        second.record(1, 0, -7, generation).unwrap();
        apply_sem_undo(&manager, &mut second);
        assert_eq!(array.lock().sems[0].value, 27);
        assert_eq!(array.lock().sems[1].value, 8);
    }

    #[test]
    fn setall_clears_every_prior_undo_adjustment() {
        let _context = crate::test_support::scheduler_test_context();
        let array = Arc::new(Mutex::new(SemArray::new(1, 1, 2, 0o600, 0, 0)));
        let manager = Mutex::new(SemManager::new());
        manager.lock().insert(1, 1, array.clone());
        let mut undo = SemUndo::new();
        undo.record(1, 0, -2, 0).unwrap();
        undo.record(1, 1, -3, 0).unwrap();
        array.lock().reset_values(&[10, 20], 1).unwrap();
        apply_sem_undo(&manager, &mut undo);
        assert_eq!(array.lock().sems[0].value, 10);
        assert_eq!(array.lock().sems[1].value, 20);
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
