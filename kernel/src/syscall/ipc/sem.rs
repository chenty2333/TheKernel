use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write as _,
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
use starry_vm::{VmMutPtr, VmPtr, vm_load, vm_write_slice};

use super::{
    GETALL, GETNCNT, GETPID, GETVAL, GETZCNT, IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID,
    IPC_SET, IPC_STAT, IpcPerm, SEM_INFO, SEM_STAT, SEM_STAT_ANY, SETALL, SETVAL,
    has_ipc_permission, next_ipc_id,
};
use crate::{
    task::{AsThread, ProcStateHint, has_pending_syscall_signal, with_proc_state_hint},
    time::{TimeValueLike, wall_time},
};

const IPC_MODE_MASK: c_ushort = 0o777;
const IPC_NOWAIT: i16 = 0o4000;
const SEM_UNDO: i16 = 0x1000;
const SEM_UNSUPPORTED_FLAGS: i16 = !(IPC_NOWAIT | SEM_UNDO);
const SEM_TIMED_WAIT_SLICE: Duration = Duration::from_millis(100);

pub const SEMMSL: usize = 32000;
pub const SEMMNI: usize = 128;
pub const SEMMNS: usize = SEMMSL * SEMMNI;
pub const SEMOPM: usize = 500;
pub const SEMVMX: usize = 32767;
const SEMAEM: usize = SEMVMX;
const SEMUME: usize = SEMOPM;
const SEMUSZ: usize = 20;

static SEM_MANAGER: Mutex<SemManager> = Mutex::new(SemManager::new());
static SEM_MNI_LIMIT: AtomicUsize = AtomicUsize::new(SEMMNI);
static SEM_MSL_LIMIT: AtomicUsize = AtomicUsize::new(SEMMSL);
static SEM_MNS_LIMIT: AtomicUsize = AtomicUsize::new(SEMMNS);
static SEM_OPM_LIMIT: AtomicUsize = AtomicUsize::new(SEMOPM);
static SEM_NEXT_ID: AtomicI32 = AtomicI32::new(-1);

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
    pid: __kernel_pid_t,
    ncnt: usize,
    zcnt: usize,
}

impl Semaphore {
    const fn new() -> Self {
        Self {
            value: 0,
            pid: 0,
            ncnt: 0,
            zcnt: 0,
        }
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

    fn nsems(&self) -> usize {
        self.sems.len()
    }

    fn mark_changed(&mut self) {
        self.semid_ds.sem_ctime = ipc_time_secs();
    }

    fn readable(&self, uid: u32, gid: u32) -> bool {
        has_ipc_permission(&self.semid_ds.sem_perm, uid, gid, false)
    }

    fn writable(&self, uid: u32, gid: u32) -> bool {
        has_ipc_permission(&self.semid_ds.sem_perm, uid, gid, true)
    }
}

struct SemManager {
    key_semid: BTreeMap<i32, i32>,
    semid_arrays: BTreeMap<i32, Arc<Mutex<SemArray>>>,
}

impl SemManager {
    const fn new() -> Self {
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

fn allocate_sem_id(manager: &SemManager) -> i32 {
    let desired = SEM_NEXT_ID.swap(-1, Ordering::Relaxed);
    if desired >= 0 && !manager.semid_arrays.contains_key(&desired) {
        desired
    } else {
        loop {
            let candidate = next_ipc_id();
            if !manager.semid_arrays.contains_key(&candidate) {
                return candidate;
            }
        }
    }
}

fn admin_ipc_permission(perm: &IpcPerm, uid: u32) -> bool {
    uid == 0 || perm.uid == uid || perm.cuid == uid
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
    SEM_NEXT_ID.load(Ordering::Relaxed)
}

pub(crate) fn set_sem_next_id(value: i32) -> AxResult<()> {
    if value < -1 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    SEM_NEXT_ID.store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn sysvipc_sem_snapshot() -> String {
    let mut out = String::from(
        "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n",
    );
    let manager = SEM_MANAGER.lock();
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

fn copy_sem_values_to_user(ptr: usize, values: &[u16]) -> AxResult<()> {
    vm_write_slice(ptr as *mut u16, values)?;
    Ok(())
}

fn copy_sem_values_from_user(ptr: usize, nsems: usize) -> AxResult<Vec<u16>> {
    vm_load(ptr as *const u16, nsems).map_err(Into::into)
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
    let current = current();
    let proc_data = &current.as_thread().proc_data;
    let current_uid = proc_data.euid();
    let current_gid = proc_data.egid();
    let create = (semflg & IPC_CREAT) != 0;
    let excl = (semflg & IPC_EXCL) != 0;

    let mut manager = SEM_MANAGER.lock();
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
        if !array.readable(current_uid, current_gid) {
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

    let semid = allocate_sem_id(&manager);
    let array = Arc::new(Mutex::new(SemArray::new(
        semid,
        key,
        nsems as usize,
        (semflg & IPC_MODE_MASK as i32) as _,
        current_uid,
        current_gid,
    )));
    manager.insert(key, semid, array);
    Ok(semid as isize)
}

pub fn sys_semctl(semid: i32, semnum: i32, cmd: i32, arg: usize) -> AxResult<isize> {
    let current_task = current();
    let proc_data = &current_task.as_thread().proc_data;
    let current_uid = proc_data.euid();
    let current_gid = proc_data.egid();
    let cmd = strip_ipc64(cmd);

    if cmd == IPC_INFO {
        let manager = SEM_MANAGER.lock();
        (arg as *mut SemInfo).vm_write(SemInfo::ipc_info())?;
        return Ok(manager.max_active_index());
    }
    if cmd == SEM_INFO {
        let manager = SEM_MANAGER.lock();
        (arg as *mut SemInfo).vm_write(SemInfo::sem_info(&manager))?;
        return Ok(manager.max_active_index());
    }
    if cmd == SEM_STAT || cmd == SEM_STAT_ANY {
        let manager = SEM_MANAGER.lock();
        let array = manager
            .get_array_by_semid(semid)
            .ok_or(AxError::from(LinuxError::EINVAL))?;
        let array = array.lock();
        if array.removed {
            return Err(AxError::from(LinuxError::EINVAL));
        }
        if cmd == SEM_STAT && !array.readable(current_uid, current_gid) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        (arg as *mut SemidDs).vm_write(array.semid_ds)?;
        return Ok(array.semid as isize);
    }

    let array = {
        let manager = SEM_MANAGER.lock();
        manager
            .get_array_by_semid(semid)
            .ok_or(AxError::from(LinuxError::EINVAL))?
    };
    let mut array = array.lock();
    if array.removed {
        return Err(AxError::from(LinuxError::EINVAL));
    }

    match cmd {
        IPC_STAT => {
            if !array.readable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            (arg as *mut SemidDs).vm_write(array.semid_ds)?;
            Ok(0)
        }
        IPC_SET => {
            if !admin_ipc_permission(&array.semid_ds.sem_perm, current_uid) {
                return Err(AxError::from(LinuxError::EPERM));
            }
            let user_ds = (arg as *const SemidDs).vm_read()?;
            array.semid_ds.sem_perm.uid = user_ds.sem_perm.uid;
            array.semid_ds.sem_perm.gid = user_ds.sem_perm.gid;
            array.semid_ds.sem_perm.mode = (array.semid_ds.sem_perm.mode & !IPC_MODE_MASK)
                | (user_ds.sem_perm.mode & IPC_MODE_MASK);
            array.mark_changed();
            Ok(0)
        }
        IPC_RMID => {
            if !admin_ipc_permission(&array.semid_ds.sem_perm, current_uid) {
                return Err(AxError::from(LinuxError::EPERM));
            }
            array.removed = true;
            array.mark_changed();
            let waiters = array.waiters.clone();
            drop(array);
            SEM_MANAGER.lock().remove_semid(semid);
            notify_sem_waiters(waiters);
            Ok(0)
        }
        GETVAL => {
            if !array.readable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].value as isize)
        }
        GETPID => {
            if !array.readable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].pid as isize)
        }
        GETNCNT => {
            if !array.readable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].ncnt as isize)
        }
        GETZCNT => {
            if !array.readable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let index = validate_semnum(&array, semnum)?;
            Ok(array.sems[index].zcnt as isize)
        }
        GETALL => {
            if !array.readable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let values = array.sems.iter().map(|sem| sem.value).collect::<Vec<_>>();
            drop(array);
            copy_sem_values_to_user(arg, &values)?;
            Ok(0)
        }
        SETVAL => {
            if !array.writable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let value = arg as c_int;
            if value < 0 || value as usize > SEMVMX {
                return Err(AxError::from(LinuxError::ERANGE));
            }
            let index = validate_semnum(&array, semnum)?;
            let pid = current().as_thread().proc_data.proc.pid() as __kernel_pid_t;
            array.sems[index].value = value as u16;
            array.sems[index].pid = pid;
            array.mark_changed();
            let waiters = array.waiters.clone();
            drop(array);
            notify_sem_waiters(waiters);
            Ok(0)
        }
        SETALL => {
            if !array.writable(current_uid, current_gid) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            let values = copy_sem_values_from_user(arg, array.nsems())?;
            if values.iter().any(|value| *value as usize > SEMVMX) {
                return Err(AxError::from(LinuxError::ERANGE));
            }
            let pid = current().as_thread().proc_data.proc.pid() as __kernel_pid_t;
            for (sem, value) in array.sems.iter_mut().zip(values) {
                sem.value = value;
                sem.pid = pid;
            }
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

fn try_apply_single_semop(
    array: &mut SemArray,
    op: Sembuf,
    pid: __kernel_pid_t,
) -> AxResult<SemTryResult> {
    let index = op.sem_num as usize;
    if index >= array.sems.len() {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    if op.sem_flg & SEM_UNSUPPORTED_FLAGS != 0 {
        return Err(AxError::from(LinuxError::EINVAL));
    }

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
        if op.sem_flg & SEM_UNSUPPORTED_FLAGS != 0 {
            return Err(AxError::from(LinuxError::EINVAL));
        }
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

fn validate_timeout(timeout: *const timespec) -> AxResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let timeout = unsafe { timeout.vm_read_uninit()?.assume_init() };
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
    let guard = WaitCountGuard {
        array,
        index: blocked_index,
        wait_zero,
    };
    guard
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
        let interrupted = with_proc_state_hint(ProcStateHint::Interruptible, || {
            waiters
                .wait_until_interruptible(|| {
                    sem_wait_ready(array, sem_num, wait_zero, needed_value).unwrap_or(true)
                        || has_pending_syscall_signal(thread)
                })
                .is_err()
        });
        if interrupted || has_pending_syscall_signal(thread) {
            return Err(AxError::Interrupted);
        }
        sem_wait_ready(array, sem_num, wait_zero, needed_value)?;
        return Ok(());
    }

    let sleep_for = deadline
        .map(|deadline| {
            deadline
                .saturating_sub(wall_time_duration())
                .min(SEM_TIMED_WAIT_SLICE)
        })
        .unwrap_or(SEM_TIMED_WAIT_SLICE);
    with_proc_state_hint(ProcStateHint::Interruptible, || {
        waiters.wait_timeout_until(sleep_for, || {
            sem_wait_ready(array, sem_num, wait_zero, needed_value).unwrap_or(true)
                || has_pending_syscall_signal(thread)
                || deadline_elapsed(deadline)
        });
    });
    if sem_wait_ready(array, sem_num, wait_zero, needed_value)? {
        return Ok(());
    }
    Ok(())
}

fn op_has_nowait(ops: &[Sembuf]) -> bool {
    ops.iter().any(|op| op.sem_flg & IPC_NOWAIT != 0)
}

pub fn sys_semop(semid: i32, sops: *const Sembuf, nsops: usize) -> AxResult<isize> {
    sys_semtimedop(semid, sops, nsops, core::ptr::null())
}

pub fn sys_semtimedop(
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

    let deadline = validate_timeout(timeout)?;
    let ops = vm_load(sops, nsops)?;
    let current = current();
    let proc_data = &current.as_thread().proc_data;
    let current_uid = proc_data.euid();
    let current_gid = proc_data.egid();
    let current_pid = proc_data.proc.pid() as __kernel_pid_t;
    let needs_write = ops.iter().any(|op| op.sem_op != 0);

    let array = {
        let manager = SEM_MANAGER.lock();
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
                array.writable(current_uid, current_gid)
            } else {
                array.readable(current_uid, current_gid)
            };
            if !has_permission {
                return Err(AxError::from(LinuxError::EACCES));
            }

            match try_apply_semops(&mut array, &ops, current_pid)? {
                SemTryResult::Ready => {
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
