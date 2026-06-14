use alloc::{collections::btree_map::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write as _,
    sync::atomic::{AtomicI32, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::{
    ctypes::{c_ulong, c_ushort},
    general::*,
};
use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use starry_process::Pid;

use super::{
    IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, IpcPerm, SHM_DEST,
    SHM_INFO, SHM_LOCK, SHM_LOCKED, SHM_STAT, SHM_STAT_ANY, SHM_UNLOCK, SHMMIN, next_ipc_id,
    shmall_limit, shmmax_limit, shmmni_limit, shmseg_limit,
};
use crate::{
    mm::{Backend, SharedPages, UserPtr, nullable},
    task::AsThread,
    time::wall_time,
};

const IPC_MODE_MASK: __kernel_mode_t = 0o777;
const SHM_HUGETLB_FLAG: usize = 0o4000;
#[cfg(target_arch = "loongarch64")]
const SHMLBA: usize = 0x10000;
#[cfg(not(target_arch = "loongarch64"))]
const SHMLBA: usize = PAGE_SIZE_4K;

fn align_down_to(value: usize, align: usize) -> usize {
    value / align * align
}

fn align_up_to(value: usize, align: usize) -> Option<usize> {
    value
        .checked_add(align - 1)
        .map(|aligned| align_down_to(aligned, align))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IpcInfo {
    shmmax: c_ulong,
    shmmin: c_ulong,
    shmmni: c_ulong,
    shmseg: c_ulong,
    shmall: c_ulong,
    reserved1: c_ulong,
    reserved2: c_ulong,
    reserved3: c_ulong,
    reserved4: c_ulong,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ShmUsageInfo {
    used_ids: i32,
    shm_tot: c_ulong,
    shm_rss: c_ulong,
    shm_swp: c_ulong,
    swap_attempts: c_ulong,
    swap_successes: c_ulong,
}

fn admin_ipc_permission(perm: &IpcPerm, uid: u32) -> bool {
    uid == 0 || perm.uid == uid || perm.cuid == uid
}

fn can_attach_shm(perm: &IpcPerm, uid: u32, gid: u32, read_only: bool) -> bool {
    super::has_ipc_permission(perm, uid, gid, false)
        && (read_only || super::has_ipc_permission(perm, uid, gid, true))
}

bitflags::bitflags! {
    /// flags for sys_shmat
    #[derive(Debug)]
    struct ShmAtFlags: u32 {
        /* attach read-only else read-write */
        const SHM_RDONLY = 0o10000;
        /* round attach address to SHMLBA */
        const SHM_RND = 0o20000;
        /* take-over region on attach */
        const SHM_REMAP = 0o40000;
    }
}

/// Data structure describing a shared memory segment.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ShmidDs {
    /// operation permission struct
    shm_perm: IpcPerm,
    /// size of segment in bytes
    shm_segsz: __kernel_size_t,
    /// time of last shmat()
    shm_atime: __kernel_time_t,
    /// time of last shmdt()
    shm_dtime: __kernel_time_t,
    /// time of last change by shmctl()
    pub shm_ctime: __kernel_time_t,
    /// pid of creator
    shm_cpid: __kernel_pid_t,
    /// pid of last shmop
    shm_lpid: __kernel_pid_t,
    /// number of current attaches
    shm_nattch: c_ulong,
    unused4: c_ulong,
    unused5: c_ulong,
}

impl ShmidDs {
    fn new(
        key: i32,
        size: usize,
        mode: __kernel_mode_t,
        pid: __kernel_pid_t,
        uid: __kernel_uid_t,
        gid: __kernel_gid_t,
    ) -> Self {
        Self {
            shm_perm: IpcPerm {
                key,
                uid,
                gid,
                cuid: uid,
                cgid: gid,
                mode: (mode & IPC_MODE_MASK) as _,
                pad1: 0,
                seq: 0,
                pad2: 0,
                unused0: 0,
                unused1: 0,
            },
            shm_segsz: size as __kernel_size_t,
            shm_atime: 0,
            shm_dtime: 0,
            shm_ctime: wall_time().as_secs() as __kernel_time_t,
            shm_cpid: pid,
            shm_lpid: pid,
            shm_nattch: 0,
            unused4: 0,
            unused5: 0,
        }
    }
}

/// This struct is used to maintain the shmem in kernel.
pub struct ShmInner {
    /// Shared memory segment identifier.
    pub shmid: i32,
    /// Number of pages in the shared memory segment.
    pub page_num: usize,
    va_ranges: BTreeMap<Pid, BTreeMap<VirtAddr, VirtAddrRange>>,
    /// physical pages
    pub phys_pages: Option<Arc<SharedPages>>,
    /// whether remove on last detach, see shm_ctl
    pub rmid: bool,
    /// Mapping flags used for this shared memory segment.
    pub mapping_flags: MappingFlags,
    /// c type struct, used in shm_ctl
    pub shmid_ds: ShmidDs,
}

impl ShmInner {
    /// Creates a new [`ShmInner`].
    pub fn new(
        key: i32,
        shmid: i32,
        size: usize,
        mapping_flags: MappingFlags,
        perm_mode: __kernel_mode_t,
        pid: Pid,
        uid: __kernel_uid_t,
        gid: __kernel_gid_t,
    ) -> Self {
        ShmInner {
            shmid,
            page_num: memory_addr::align_up_4k(size) / PAGE_SIZE_4K,
            va_ranges: BTreeMap::new(),
            phys_pages: None,
            rmid: false,
            mapping_flags,
            shmid_ds: ShmidDs::new(key, size, perm_mode, pid as __kernel_pid_t, uid, gid),
        }
    }

    /// Updates the pid of last shmop and checks if the size and mapping flags
    /// match.
    pub fn try_update(&mut self, size: usize, requested_mode: __kernel_mode_t) -> AxResult<isize> {
        if size > self.shmid_ds.shm_segsz as usize {
            return Err(AxError::InvalidInput);
        }

        let curr = current();
        let proc_data = &curr.as_thread().proc_data;
        let uid = proc_data.euid();
        let gid = proc_data.egid();
        let wants_read = requested_mode & 0o444 != 0;
        let wants_write = requested_mode & 0o222 != 0;
        if wants_read && !super::has_ipc_permission(&self.shmid_ds.shm_perm, uid, gid, false) {
            return Err(AxError::PermissionDenied);
        }
        if wants_write && !super::has_ipc_permission(&self.shmid_ds.shm_perm, uid, gid, true) {
            return Err(AxError::PermissionDenied);
        }
        Ok(self.shmid as isize)
    }

    /// Maps the given physical shared pages to this shared memory segment.
    pub fn map_to_phys(&mut self, phys_pages: Arc<SharedPages>) {
        self.phys_pages = Some(phys_pages);
    }

    /// Returns the number of processes currently attached to this shared memory
    /// segment.
    pub fn attach_count(&self) -> usize {
        self.va_ranges.values().map(BTreeMap::len).sum()
    }

    /// Returns the virtual address range associated with an attach address.
    pub fn get_addr_range(&self, pid: Pid, vaddr: VirtAddr) -> Option<VirtAddrRange> {
        self.va_ranges
            .get(&pid)
            .and_then(|ranges| ranges.get(&vaddr))
            .cloned()
    }

    /// Called by sys_shmat
    pub fn attach_process(&mut self, pid: Pid, va_range: VirtAddrRange) {
        let old = self
            .va_ranges
            .entry(pid)
            .or_default()
            .insert(va_range.start, va_range);
        assert!(old.is_none());
        self.shmid_ds.shm_nattch += 1;
        self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
        self.shmid_ds.shm_atime = wall_time().as_secs() as __kernel_time_t;
    }

    pub fn inherit_process(&mut self, pid: Pid, va_range: VirtAddrRange) {
        let old = self
            .va_ranges
            .entry(pid)
            .or_default()
            .insert(va_range.start, va_range);
        assert!(old.is_none());
        self.shmid_ds.shm_nattch += 1;
    }

    /// Called by sys_shmdt
    pub fn detach_process(&mut self, pid: Pid, vaddr: VirtAddr) -> Option<VirtAddrRange> {
        let ranges = self.va_ranges.get_mut(&pid)?;
        let va_range = ranges.remove(&vaddr)?;
        if ranges.is_empty() {
            self.va_ranges.remove(&pid);
        }
        self.shmid_ds.shm_nattch -= 1;
        self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
        self.shmid_ds.shm_dtime = wall_time().as_secs() as __kernel_time_t;
        Some(va_range)
    }

    pub fn set_removed(&mut self, removed: bool) {
        self.rmid = removed;
        if removed {
            self.shmid_ds.shm_perm.mode |= SHM_DEST as c_ushort;
        } else {
            self.shmid_ds.shm_perm.mode &= !(SHM_DEST as c_ushort);
        }
    }

    pub fn set_locked(&mut self, locked: bool) {
        if locked {
            self.shmid_ds.shm_perm.mode |= SHM_LOCKED as c_ushort;
        } else {
            self.shmid_ds.shm_perm.mode &= !(SHM_LOCKED as c_ushort);
        }
    }
}

/// A bidirectional BTreeMap, allowing lookup by key or value.
/// TODO: I don't know where to put this, so I put it here.
#[derive(Debug, Clone)]
pub struct BiBTreeMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    forward: BTreeMap<K, V>,
    reverse: BTreeMap<V, K>,
}

impl<K, V> BiBTreeMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    /// Creates a new empty [`BiBTreeMap`].
    pub const fn new() -> Self {
        BiBTreeMap {
            forward: BTreeMap::new(),
            reverse: BTreeMap::new(),
        }
    }

    /// Inserts a key-value pair into the map, replacing any existing mapping
    /// for either key or value.
    pub fn insert(&mut self, key: K, value: V) {
        if let Some(old_key) = self.reverse.insert(value.clone(), key.clone()) {
            self.forward.remove(&old_key);
        }
        if let Some(old_value) = self.forward.insert(key, value.clone()) {
            self.reverse.remove(&old_value);
        }
    }

    /// Returns a reference to the value corresponding to the given key, if it
    /// exists.
    pub fn get_by_key(&self, key: &K) -> Option<&V> {
        self.forward.get(key)
    }

    /// Removes a key-value pair by value, returning the key if it existed.
    pub fn remove_by_value(&mut self, value: &V) -> Option<K> {
        if let Some(key) = self.reverse.remove(value) {
            self.forward.remove(&key);
            Some(key)
        } else {
            None
        }
    }
}

impl<K, V> Default for BiBTreeMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

/// This struct is used to manage the relationship between the shmem and
/// processes. note: this struct do not modify the struct ShmInner, but only
/// manage the mapping.
pub struct ShmManager {
    /// key <-> shm_id
    key_shmid: BiBTreeMap<i32, i32>,
    /// shm_id -> shm_inner
    shmid_inner: BTreeMap<i32, Arc<Mutex<ShmInner>>>,
    /// pid -> attach address -> shm_id
    pid_vaddr_shmid: BTreeMap<Pid, BTreeMap<VirtAddr, i32>>,
}

impl ShmManager {
    const fn new() -> Self {
        ShmManager {
            key_shmid: BiBTreeMap::new(),
            shmid_inner: BTreeMap::new(),
            pid_vaddr_shmid: BTreeMap::new(),
        }
    }

    /// Returns the shared memory ID associated with the given key.
    pub fn get_shmid_by_key(&self, key: i32) -> Option<i32> {
        self.key_shmid.get_by_key(&key).cloned()
    }

    /// Returns the shared memory inner structure [`ShmInner`] associated with
    /// the given shared memory ID.
    pub fn get_inner_by_shmid(&self, shmid: i32) -> Option<Arc<Mutex<ShmInner>>> {
        self.shmid_inner.get(&shmid).cloned()
    }

    pub fn active_segment_count(&self) -> usize {
        self.shmid_inner.len()
    }

    pub fn contains_shmid(&self, shmid: i32) -> bool {
        self.shmid_inner.contains_key(&shmid)
    }

    pub fn max_active_index(&self) -> isize {
        self.shmid_inner
            .keys()
            .map(|&shmid| shmid as isize)
            .max()
            .unwrap_or(0)
    }

    /// Returns the shared memory ID associated with the given pid and virtual
    /// address.
    pub fn get_shmid_by_vaddr(&self, pid: Pid, vaddr: VirtAddr) -> Option<i32> {
        self.pid_vaddr_shmid
            .get(&pid)
            .and_then(|map| map.get(&vaddr))
            .cloned()
    }

    fn get_attachments_by_pid(&self, pid: Pid) -> Option<Vec<(VirtAddr, i32)>> {
        let map = self.pid_vaddr_shmid.get(&pid)?;
        Some(map.iter().map(|(&vaddr, &shmid)| (vaddr, shmid)).collect())
    }

    // used by garbage collection
    #[allow(dead_code)]
    fn find_vaddr_by_shmid(&self, pid: Pid, shmid: i32) -> Option<VirtAddr> {
        self.pid_vaddr_shmid.get(&pid).and_then(|map| {
            map.iter()
                .find(|(_, id)| **id == shmid)
                .map(|(&vaddr, _)| vaddr)
        })
    }

    /// Inserts a mapping from a key to a shared memory ID.
    pub fn insert_key_shmid(&mut self, key: i32, shmid: i32) {
        self.key_shmid.insert(key, shmid);
    }

    /// Inserts a mapping from a shared memory ID to its inner
    /// structure [`ShmInner`].
    pub fn insert_shmid_inner(&mut self, shmid: i32, shm_inner: Arc<Mutex<ShmInner>>) {
        self.shmid_inner.insert(shmid, shm_inner);
    }

    /// Inserts a mapping from a process and shared memory ID to a virtual
    /// address.
    pub fn insert_shmid_vaddr(&mut self, pid: Pid, shmid: i32, vaddr: VirtAddr) {
        // maintain the map 'shmid_vaddr'
        self.pid_vaddr_shmid
            .entry(pid)
            .or_default()
            .insert(vaddr, shmid);
    }

    /// Removes the mapping from a process and shared memory address.
    pub fn remove_shmaddr(&mut self, pid: Pid, shmaddr: VirtAddr) {
        let mut empty: bool = false;
        if let Some(map) = self.pid_vaddr_shmid.get_mut(&pid) {
            map.remove(&shmaddr);
            empty = map.is_empty();
        }
        if empty {
            self.pid_vaddr_shmid.remove(&pid);
        }
    }

    // called when a process exit
    fn remove_pid(&mut self, pid: Pid) {
        self.pid_vaddr_shmid.remove(&pid);
    }

    /// Removes the shared memory segment.
    pub fn remove_shmid(&mut self, shmid: i32) {
        self.key_shmid.remove_by_value(&shmid);
        self.shmid_inner.remove(&shmid);
        // Per-process attach maps are cleaned on shmdt/exit. IPC_RMID only
        // removes the segment once the last attach has gone away.
    }

    /// Clear all shared memory segments related to the process.
    pub fn clear_proc_shm(&mut self, pid: Pid) {
        if let Some(attachments) = self.get_attachments_by_pid(pid) {
            for (vaddr, shmid) in attachments {
                if let Some(shm_inner) = self.get_inner_by_shmid(shmid) {
                    let mut shm_inner = shm_inner.lock();
                    shm_inner.detach_process(pid, vaddr);
                    if shm_inner.rmid && shm_inner.attach_count() == 0 {
                        self.remove_shmid(shmid);
                    }
                }
            }
        }
        self.remove_pid(pid);
    }

    pub fn inherit_proc_shm(&mut self, parent_pid: Pid, child_pid: Pid) {
        let Some(parent_map) = self.pid_vaddr_shmid.get(&parent_pid) else {
            return;
        };
        let inherited: Vec<_> = parent_map
            .iter()
            .map(|(&vaddr, &shmid)| (vaddr, shmid))
            .collect();

        for (vaddr, shmid) in inherited {
            let Some(shm_inner) = self.get_inner_by_shmid(shmid) else {
                continue;
            };
            let mut shm_inner = shm_inner.lock();
            let Some(va_range) = shm_inner.get_addr_range(parent_pid, vaddr) else {
                continue;
            };
            self.insert_shmid_vaddr(child_pid, shmid, vaddr);
            shm_inner.inherit_process(child_pid, va_range);
        }
    }
}

/// Global shared memory manager.
pub static SHM_MANAGER: Mutex<ShmManager> = Mutex::new(ShmManager::new());
static SHM_NEXT_ID: AtomicI32 = AtomicI32::new(-1);

pub fn inherit_proc_shm(parent_pid: Pid, child_pid: Pid) {
    SHM_MANAGER.lock().inherit_proc_shm(parent_pid, child_pid);
}

fn allocate_shm_id(shm_manager: &ShmManager) -> i32 {
    let desired = SHM_NEXT_ID.swap(-1, Ordering::Relaxed);
    if desired >= 0 && !shm_manager.contains_shmid(desired) {
        desired
    } else {
        loop {
            let candidate = next_ipc_id();
            if !shm_manager.contains_shmid(candidate) {
                return candidate;
            }
        }
    }
}

pub(crate) fn shm_next_id() -> i32 {
    SHM_NEXT_ID.load(Ordering::Relaxed)
}

pub(crate) fn set_shm_next_id(value: i32) -> AxResult<()> {
    if value < -1 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    SHM_NEXT_ID.store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn sysvipc_shm_snapshot() -> String {
    let mut out = String::from(
        "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  \
         cgid      atime      dtime      ctime        rss       swap\n",
    );
    let shm_manager = SHM_MANAGER.lock();
    for (&shmid, shm_inner) in &shm_manager.shmid_inner {
        let shm_inner = shm_inner.lock();
        let ds = shm_inner.shmid_ds;
        let rss_bytes = shm_inner.page_num.saturating_mul(PAGE_SIZE_4K);
        let _ = writeln!(
            out,
            "{:10} {:10} {:5o} {:21} {:5} {:5} {:6} {:5} {:5} {:5} {:5} {:10} {:10} {:10} {:10} \
             {:10}",
            ds.shm_perm.key,
            shmid,
            ds.shm_perm.mode & 0o777,
            ds.shm_segsz,
            ds.shm_cpid,
            ds.shm_lpid,
            ds.shm_nattch,
            ds.shm_perm.uid,
            ds.shm_perm.gid,
            ds.shm_perm.cuid,
            ds.shm_perm.cgid,
            ds.shm_atime,
            ds.shm_dtime,
            ds.shm_ctime,
            rss_bytes,
            0,
        );
    }
    out
}

pub fn sys_shmget(key: i32, size: usize, shmflg: usize) -> AxResult<isize> {
    let perm_mode = (shmflg as __kernel_mode_t) & IPC_MODE_MASK;
    let create = shmflg & IPC_CREAT as usize != 0;
    let excl = shmflg & IPC_EXCL as usize != 0;
    let huge = shmflg & SHM_HUGETLB_FLAG != 0;

    let mut mapping_flags = MappingFlags::from_name("USER").unwrap();
    if perm_mode & 0o444 != 0 {
        mapping_flags.insert(MappingFlags::READ);
    }
    if perm_mode & 0o222 != 0 {
        mapping_flags.insert(MappingFlags::WRITE);
    }
    if perm_mode & 0o111 != 0 {
        mapping_flags.insert(MappingFlags::EXECUTE);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let cur_pid = proc_data.proc.pid();
    let euid = proc_data.euid();
    let egid = proc_data.egid();

    if huge {
        return Err(AxError::from(LinuxError::EINVAL));
    }

    let mut shm_manager = SHM_MANAGER.lock();

    if key != IPC_PRIVATE {
        if let Some(shmid) = shm_manager.get_shmid_by_key(key) {
            if create && excl {
                return Err(AxError::from(LinuxError::EEXIST));
            }
            let shm_inner = shm_manager
                .get_inner_by_shmid(shmid)
                .ok_or(AxError::InvalidInput)?;
            let mut shm_inner = shm_inner.lock();
            return shm_inner.try_update(size, perm_mode);
        }
        if !create {
            return Err(AxError::from(LinuxError::ENOENT));
        }
    }

    if size < SHMMIN || size > shmmax_limit() {
        return Err(AxError::InvalidInput);
    }
    let page_num = memory_addr::align_up_4k(size) / PAGE_SIZE_4K;
    if page_num == 0 {
        return Err(AxError::InvalidInput);
    }
    if shm_manager.active_segment_count() >= shmmni_limit() {
        return Err(AxError::from(LinuxError::ENOSPC));
    }

    // Create a new shm_inner
    let shmid = allocate_shm_id(&shm_manager);
    let shm_inner = Arc::new(Mutex::new(ShmInner::new(
        key,
        shmid,
        size,
        mapping_flags,
        perm_mode,
        cur_pid,
        euid,
        egid,
    )));
    shm_manager.insert_key_shmid(key, shmid);
    shm_manager.insert_shmid_inner(shmid, shm_inner);

    Ok(shmid as isize)
}

pub fn sys_shmat(shmid: i32, addr: usize, shmflg: u32) -> AxResult<isize> {
    let shm_inner = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(AxError::InvalidInput)?
    };
    let mut shm_inner = shm_inner.lock();
    if shm_inner.rmid {
        return Err(AxError::from(LinuxError::EIDRM));
    }
    let mut mapping_flags = shm_inner.mapping_flags;
    let shm_flg = ShmAtFlags::from_bits_truncate(shmflg);
    let read_only = shm_flg.contains(ShmAtFlags::SHM_RDONLY);

    if read_only {
        mapping_flags.remove(MappingFlags::WRITE);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let pid = proc_data.proc.pid();
    if !can_attach_shm(
        &shm_inner.shmid_ds.shm_perm,
        proc_data.euid(),
        proc_data.egid(),
        read_only,
    ) {
        return Err(AxError::from(LinuxError::EACCES));
    }
    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let length = shm_inner.page_num * PAGE_SIZE_4K;
    let limit = VirtAddrRange::new(aspace.base(), aspace.end());

    let start_addr = if addr == 0 {
        if shm_flg.contains(ShmAtFlags::SHM_REMAP) {
            return Err(AxError::InvalidInput);
        }
        let search_length = align_up_to(length, SHMLBA).ok_or(AxError::NoMemory)?;
        aspace
            .find_kernel_area(aspace.base(), search_length, limit, SHMLBA)
            .ok_or(AxError::NoMemory)?
    } else {
        if addr % SHMLBA != 0 && !shm_flg.contains(ShmAtFlags::SHM_RND) {
            return Err(AxError::InvalidInput);
        }
        let candidate_addr = if shm_flg.contains(ShmAtFlags::SHM_RND) {
            align_down_to(addr, SHMLBA)
        } else {
            addr
        };
        let candidate = VirtAddr::from(candidate_addr);
        let found = aspace.find_free_area(candidate, length, limit, PAGE_SIZE_4K);

        #[cfg(target_arch = "loongarch64")]
        let (candidate, found) = {
            let mut candidate = candidate;
            let mut found = found;
            if shm_flg.contains(ShmAtFlags::SHM_RND) && found != Some(candidate) {
                let compat_addr = memory_addr::align_down_4k(addr);
                let compat = VirtAddr::from(compat_addr);
                if compat_addr != candidate_addr
                    && aspace.find_free_area(compat, length, limit, PAGE_SIZE_4K) == Some(compat)
                {
                    candidate = compat;
                    found = Some(compat);
                }
            }
            (candidate, found)
        };

        if found != Some(candidate) {
            return Err(AxError::InvalidInput);
        }
        candidate
    };
    let end_addr = VirtAddr::from(start_addr.as_usize() + length);
    let va_range = VirtAddrRange::new(start_addr, end_addr);

    let mut shm_manager = SHM_MANAGER.lock();
    shm_manager.insert_shmid_vaddr(pid, shm_inner.shmid, start_addr);

    // map the virtual address range to the physical address
    if let Some(phys_pages) = shm_inner.phys_pages.clone() {
        // Another proccess has attached the shared memory
        // TODO(mivik): shm page size
        let backend = Backend::new_shared(start_addr, phys_pages);
        aspace.map(start_addr, length, mapping_flags, false, backend)?;
    } else {
        // This is the first process to attach the shared memory
        let pages = Arc::new(SharedPages::new(length, PageSize::Size4K)?);
        let backend = Backend::new_shared(start_addr, pages.clone());
        aspace.map(start_addr, length, mapping_flags, false, backend)?;

        shm_inner.map_to_phys(pages);
    }

    shm_inner.attach_process(pid, va_range);
    Ok(start_addr.as_usize() as isize)
}

pub fn sys_shmctl(shmid: i32, cmd: u32, buf: UserPtr<ShmidDs>) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let current_uid = proc_data.euid();
    let current_gid = proc_data.egid();
    let shm_inner = {
        let shm_manager = SHM_MANAGER.lock();
        let cmd = cmd as i32;
        if cmd == IPC_INFO {
            let info = IpcInfo {
                shmmax: shmmax_limit() as c_ulong,
                shmmin: SHMMIN as c_ulong,
                shmmni: shmmni_limit() as c_ulong,
                shmseg: shmseg_limit() as c_ulong,
                shmall: shmall_limit() as c_ulong,
                reserved1: 0,
                reserved2: 0,
                reserved3: 0,
                reserved4: 0,
            };
            *buf.cast::<IpcInfo>().get_as_mut()? = info;
            return Ok(shm_manager.max_active_index());
        }
        if cmd == SHM_INFO {
            let mut info = ShmUsageInfo {
                used_ids: shm_manager.active_segment_count() as i32,
                shm_tot: 0,
                shm_rss: 0,
                shm_swp: 0,
                swap_attempts: 0,
                swap_successes: 0,
            };
            for shm_inner in shm_manager.shmid_inner.values() {
                let shm_inner = shm_inner.lock();
                info.shm_tot = info.shm_tot.saturating_add(shm_inner.page_num as c_ulong);
                info.shm_rss = info.shm_rss.saturating_add(shm_inner.page_num as c_ulong);
            }
            *buf.cast::<ShmUsageInfo>().get_as_mut()? = info;
            return Ok(shm_manager.max_active_index());
        }
        if cmd == SHM_STAT || cmd == SHM_STAT_ANY {
            let (actual_shmid, shm_inner) = shm_manager
                .get_inner_by_shmid(shmid)
                .map(|inner| (shmid, inner))
                .ok_or(AxError::InvalidInput)?;
            let shm_inner = shm_inner.lock();
            if cmd == SHM_STAT
                && !super::has_ipc_permission(
                    &shm_inner.shmid_ds.shm_perm,
                    current_uid,
                    current_gid,
                    false,
                )
            {
                return Err(AxError::from(LinuxError::EACCES));
            }
            *buf.get_as_mut()? = shm_inner.shmid_ds;
            return Ok(actual_shmid as isize);
        }
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(AxError::InvalidInput)?
    };
    let mut shm_inner = shm_inner.lock();
    let cmd = cmd as i32;

    if cmd == IPC_SET {
        if !admin_ipc_permission(&shm_inner.shmid_ds.shm_perm, current_uid) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        let user_ds = *buf.get_as_mut()?;
        shm_inner.shmid_ds.shm_perm.uid = user_ds.shm_perm.uid;
        shm_inner.shmid_ds.shm_perm.gid = user_ds.shm_perm.gid;
        shm_inner.shmid_ds.shm_perm.mode = (shm_inner.shmid_ds.shm_perm.mode
            & !(IPC_MODE_MASK as c_ushort))
            | (user_ds.shm_perm.mode & IPC_MODE_MASK as c_ushort);
        shm_inner.shmid_ds.shm_ctime = wall_time().as_secs() as __kernel_time_t;
    } else if cmd == IPC_STAT {
        if !super::has_ipc_permission(
            &shm_inner.shmid_ds.shm_perm,
            current_uid,
            current_gid,
            false,
        ) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        if let Some(shmid_ds) = nullable!(buf.get_as_mut())? {
            *shmid_ds = shm_inner.shmid_ds;
        }
    } else if cmd == IPC_RMID {
        if !admin_ipc_permission(&shm_inner.shmid_ds.shm_perm, current_uid) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        shm_inner.set_removed(true);
        if shm_inner.attach_count() == 0 {
            SHM_MANAGER.lock().remove_shmid(shmid);
        }
    } else if cmd == SHM_LOCK {
        if !admin_ipc_permission(&shm_inner.shmid_ds.shm_perm, current_uid) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        shm_inner.set_locked(true);
    } else if cmd == SHM_UNLOCK {
        if !admin_ipc_permission(&shm_inner.shmid_ds.shm_perm, current_uid) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        shm_inner.set_locked(false);
    } else {
        return Err(AxError::InvalidInput);
    }
    Ok(0)
}

// Garbage collection for shared memory:
// 1. when the process call sys_shmdt, delete everything related to shmaddr,
//    including map 'shmid_vaddr';
// 2. when the last process detach the shared memory and this shared memory was
//    specified with IPC_RMID, delete everything related to this shared memory,
//    including all the 3 maps;
// 3. when a process exit, delete everything related to this process, including
//    2 maps: 'shmid_vaddr' and 'shmid_inner';
//
// The attach between the process and the shared memory occurs in sys_shmat,
//  and the detach occurs in sys_shmdt, or when the process exits.

// Note: all the below delete functions only delete the mapping between the
// shm_id and the shm_inner,   but the shm_inner is not deleted or modifyed!
pub fn sys_shmdt(shmaddr: usize) -> AxResult<isize> {
    let shmaddr = VirtAddr::from(shmaddr);

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;

    let pid = proc_data.proc.pid();
    let shmid = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_shmid_by_vaddr(pid, shmaddr)
            .ok_or(AxError::InvalidInput)?
    };

    let shm_inner = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(AxError::InvalidInput)?
    };
    let mut shm_inner = shm_inner.lock();
    let va_range = shm_inner
        .get_addr_range(pid, shmaddr)
        .ok_or(AxError::InvalidInput)?;

    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    aspace.unmap(va_range.start, va_range.size())?;

    let mut shm_manager = SHM_MANAGER.lock();
    shm_manager.remove_shmaddr(pid, shmaddr);
    shm_inner.detach_process(pid, shmaddr);

    if shm_inner.rmid && shm_inner.attach_count() == 0 {
        shm_manager.remove_shmid(shmid);
    }

    Ok(0)
}
