use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write as _,
    hash::Hash,
    sync::atomic::{AtomicI32, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
use axsync::Mutex;
use axtask::current;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use linux_raw_sys::{
    ctypes::{c_ulong, c_ushort},
    general::*,
};
use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use starry_process::Pid;

use super::{
    IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, IpcPerm, SHM_DEST,
    SHM_INFO, SHM_LOCK, SHM_LOCKED, SHM_STAT, SHM_STAT_ANY, SHM_UNLOCK, SHMMIN, next_ipc_id,
    shmall_limit, shmmax_limit, shmmni_limit,
};
use crate::{
    mm::{Backend, SharedPages, UserPtr, nullable},
    task::AsThread,
    time::wall_time,
};

const IPC_MODE_MASK: __kernel_mode_t = 0o777;
const SHM_HUGETLB_FLAG: usize = 0o4000;
const MAX_SHM_ATTACHMENTS: usize = 65_536;
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
    va_ranges: HashMap<Pid, HashMap<usize, VirtAddrRange>>,
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
            va_ranges: HashMap::new(),
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
        let ids = curr.as_thread().current_cred().ids();
        let uid = ids.euid;
        let gid = ids.egid;
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
        self.va_ranges.values().map(HashMap::len).sum()
    }

    /// Returns the virtual address range associated with an attach address.
    pub fn get_addr_range(&self, pid: Pid, vaddr: VirtAddr) -> Option<VirtAddrRange> {
        self.va_ranges
            .get(&pid)
            .and_then(|ranges| ranges.get(&vaddr.as_usize()))
            .cloned()
    }

    fn try_reserve_process_attaches(&mut self, pid: Pid, additional: usize) -> AxResult<()> {
        if self.va_ranges.get(&pid).is_none() {
            self.va_ranges
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            let mut ranges = HashMap::new();
            ranges
                .try_reserve(additional)
                .map_err(|_| AxError::NoMemory)?;
            self.va_ranges.insert(pid, ranges);
        } else {
            self.va_ranges
                .get_mut(&pid)
                .ok_or(AxError::Io)?
                .try_reserve(additional)
                .map_err(|_| AxError::NoMemory)?;
        }
        Ok(())
    }

    fn cancel_empty_process_reservation(&mut self, pid: Pid) {
        if self.va_ranges.get(&pid).is_some_and(HashMap::is_empty) {
            self.va_ranges.remove(&pid);
        }
    }

    /// Called by sys_shmat after capacity and address-space publication have
    /// both succeeded. This commit performs no allocation.
    pub fn attach_process(&mut self, pid: Pid, va_range: VirtAddrRange) {
        let old = self
            .va_ranges
            .get_mut(&pid)
            .expect("reserved SysV SHM process bucket")
            .insert(va_range.start.as_usize(), va_range);
        debug_assert!(old.is_none());
        self.shmid_ds.shm_nattch += 1;
        self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
        self.shmid_ds.shm_atime = wall_time().as_secs() as __kernel_time_t;
    }

    pub fn inherit_process(&mut self, pid: Pid, va_range: VirtAddrRange) {
        let old = self
            .va_ranges
            .get_mut(&pid)
            .expect("reserved inherited SysV SHM process bucket")
            .insert(va_range.start.as_usize(), va_range);
        debug_assert!(old.is_none());
        self.shmid_ds.shm_nattch += 1;
    }

    /// Called by sys_shmdt
    pub fn detach_process(&mut self, pid: Pid, vaddr: VirtAddr) -> Option<VirtAddrRange> {
        let ranges = self.va_ranges.get_mut(&pid)?;
        let va_range = ranges.remove(&vaddr.as_usize())?;
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
            self.shmid_ds.shm_perm.key = IPC_PRIVATE;
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

/// A bidirectional map, allowing lookup by key or value.
/// TODO: I don't know where to put this, so I put it here.
#[derive(Debug, Clone)]
pub struct BiBTreeMap<K, V>
where
    K: Eq + Hash + Clone,
    V: Eq + Hash + Clone,
{
    forward: HashMap<K, V>,
    reverse: HashMap<V, K>,
}

impl<K, V> BiBTreeMap<K, V>
where
    K: Eq + Hash + Clone,
    V: Eq + Hash + Clone,
{
    /// Creates a new empty [`BiBTreeMap`].
    pub fn new() -> Self {
        BiBTreeMap {
            forward: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    fn try_reserve(&mut self, additional: usize) -> AxResult<()> {
        self.forward
            .try_reserve(additional)
            .map_err(|_| AxError::NoMemory)?;
        self.reverse
            .try_reserve(additional)
            .map_err(|_| AxError::NoMemory)
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
    K: Eq + Hash + Clone,
    V: Eq + Hash + Clone,
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
    shmid_inner: HashMap<i32, Arc<Mutex<ShmInner>>>,
    /// Total pages reserved by live segments, including segments awaiting
    /// their final detach after IPC_RMID.
    total_pages: usize,
    /// pid -> attach address -> shm_id
    pid_vaddr_shmid: HashMap<Pid, HashMap<usize, i32>>,
    attachment_count: usize,
}

impl ShmManager {
    fn new() -> Self {
        ShmManager {
            key_shmid: BiBTreeMap::new(),
            shmid_inner: HashMap::new(),
            total_pages: 0,
            pid_vaddr_shmid: HashMap::new(),
            attachment_count: 0,
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

    pub fn total_page_count(&self) -> usize {
        self.total_pages
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
            .and_then(|map| map.get(&vaddr.as_usize()))
            .cloned()
    }

    // used by garbage collection
    #[allow(dead_code)]
    fn find_vaddr_by_shmid(&self, pid: Pid, shmid: i32) -> Option<VirtAddr> {
        self.pid_vaddr_shmid.get(&pid).and_then(|map| {
            map.iter()
                .find(|(_, id)| **id == shmid)
                .map(|(&vaddr, _)| VirtAddr::from(vaddr))
        })
    }

    fn try_reserve_segment(&mut self, has_key: bool) -> AxResult<()> {
        self.shmid_inner
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        if has_key {
            self.key_shmid.try_reserve(1)?;
        }
        Ok(())
    }

    /// Inserts a mapping from a key to a shared memory ID after reservation.
    pub fn insert_key_shmid(&mut self, key: i32, shmid: i32) {
        self.key_shmid.insert(key, shmid);
    }

    /// Makes a live-but-destined segment undiscoverable by its old key. Linux
    /// does this at IPC_RMID time even when attachments keep the shmid object
    /// and its pages alive until the final detach.
    fn remove_key_by_shmid(&mut self, shmid: i32) {
        self.key_shmid.remove_by_value(&shmid);
    }

    /// Inserts a mapping from a shared memory ID to its inner
    /// structure [`ShmInner`].
    pub fn insert_shmid_inner(
        &mut self,
        shmid: i32,
        page_num: usize,
        shm_inner: Arc<Mutex<ShmInner>>,
    ) -> AxResult<()> {
        let total_pages = self
            .total_pages
            .checked_add(page_num)
            .ok_or(AxError::NoMemory)?;
        if self.shmid_inner.contains_key(&shmid) {
            return Err(AxError::InvalidInput);
        }
        let old = self.shmid_inner.insert(shmid, shm_inner);
        debug_assert!(old.is_none(), "duplicate shared memory ID {shmid}");
        self.total_pages = total_pages;
        Ok(())
    }

    fn try_reserve_process_attaches(&mut self, pid: Pid, additional: usize) -> AxResult<()> {
        if self
            .attachment_count
            .checked_add(additional)
            .is_none_or(|total| total > MAX_SHM_ATTACHMENTS)
        {
            return Err(AxError::NoMemory);
        }
        if self.pid_vaddr_shmid.get(&pid).is_none() {
            self.pid_vaddr_shmid
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            let mut attachments = HashMap::new();
            attachments
                .try_reserve(additional)
                .map_err(|_| AxError::NoMemory)?;
            self.pid_vaddr_shmid.insert(pid, attachments);
        } else {
            self.pid_vaddr_shmid
                .get_mut(&pid)
                .ok_or(AxError::Io)?
                .try_reserve(additional)
                .map_err(|_| AxError::NoMemory)?;
        }
        Ok(())
    }

    fn cancel_empty_process_reservation(&mut self, pid: Pid) {
        if self
            .pid_vaddr_shmid
            .get(&pid)
            .is_some_and(HashMap::is_empty)
        {
            self.pid_vaddr_shmid.remove(&pid);
        }
    }

    /// Commits a mapping after the process bucket has reserved capacity.
    pub fn insert_shmid_vaddr(&mut self, pid: Pid, shmid: i32, vaddr: VirtAddr) {
        let old = self
            .pid_vaddr_shmid
            .get_mut(&pid)
            .expect("reserved SysV SHM manager process bucket")
            .insert(vaddr.as_usize(), shmid);
        if old.is_none() {
            self.attachment_count = self.attachment_count.saturating_add(1);
        }
    }

    /// Removes the mapping from a process and shared memory address.
    pub fn remove_shmaddr(&mut self, pid: Pid, shmaddr: VirtAddr) {
        let mut empty: bool = false;
        if let Some(map) = self.pid_vaddr_shmid.get_mut(&pid) {
            if map.remove(&shmaddr.as_usize()).is_some() {
                self.attachment_count = self.attachment_count.saturating_sub(1);
            }
            empty = map.is_empty();
        }
        if empty {
            self.pid_vaddr_shmid.remove(&pid);
        }
    }

    // called when a process exit
    fn remove_pid(&mut self, pid: Pid) {
        if let Some(attachments) = self.pid_vaddr_shmid.remove(&pid) {
            self.attachment_count = self.attachment_count.saturating_sub(attachments.len());
        }
    }

    /// Removes the shared memory segment.
    pub fn remove_shmid(&mut self, shmid: i32, page_num: usize) {
        self.key_shmid.remove_by_value(&shmid);
        if self.shmid_inner.remove(&shmid).is_some() {
            let Some(total_pages) = self.total_pages.checked_sub(page_num) else {
                error!("SysV SHM page accounting underflow removing segment {shmid}");
                self.total_pages = 0;
                return;
            };
            self.total_pages = total_pages;
        }
        // Per-process attach maps are cleaned on shmdt/exit. IPC_RMID only
        // removes the segment once the last attach has gone away.
    }
}

lazy_static! {
    /// Global shared memory manager.
    pub static ref SHM_MANAGER: Mutex<ShmManager> = Mutex::new(ShmManager::new());
    /// Serializes cross-object SysV SHM transactions. Code holding this gate
    /// may acquire either the manager or one segment lock, but never both.
    static ref SHM_TRANSACTION: Mutex<()> = Mutex::new(());
}
static SHM_NEXT_ID: AtomicI32 = AtomicI32::new(-1);

struct InheritedGroup {
    inner: Arc<Mutex<ShmInner>>,
    ranges: HashMap<usize, VirtAddrRange>,
}

pub fn inherit_proc_shm(parent_pid: Pid, child_pid: Pid) -> AxResult<()> {
    let _transaction = SHM_TRANSACTION.lock();
    let required = SHM_MANAGER
        .lock()
        .pid_vaddr_shmid
        .get(&parent_pid)
        .map_or(0, HashMap::len);
    if required == 0 {
        return Ok(());
    }

    let mut source = Vec::new();
    source
        .try_reserve_exact(required)
        .map_err(|_| AxError::NoMemory)?;
    {
        let manager = SHM_MANAGER.lock();
        let attachments = manager
            .pid_vaddr_shmid
            .get(&parent_pid)
            .ok_or(AxError::Io)?;
        source.extend(attachments.iter().map(|(&vaddr, &shmid)| (shmid, vaddr)));
    }
    source.sort_unstable_by_key(|&(shmid, vaddr)| (shmid, vaddr));

    let mut groups = Vec::new();
    groups
        .try_reserve_exact(source.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut inherited = HashMap::new();
    inherited
        .try_reserve(source.len())
        .map_err(|_| AxError::NoMemory)?;
    for &(shmid, vaddr) in &source {
        inherited.insert(vaddr, shmid);
    }
    let mut index = 0;
    while index < source.len() {
        let shmid = source[index].0;
        let end = source[index..]
            .iter()
            .position(|&(candidate, _)| candidate != shmid)
            .map_or(source.len(), |offset| index + offset);
        let inner = SHM_MANAGER
            .lock()
            .get_inner_by_shmid(shmid)
            .ok_or(AxError::Io)?;
        let mut ranges = HashMap::new();
        ranges
            .try_reserve(end - index)
            .map_err(|_| AxError::NoMemory)?;
        {
            let mut state = inner.lock();
            if state.va_ranges.contains_key(&child_pid) {
                return Err(AxError::Io);
            }
            state
                .va_ranges
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            for &(_, vaddr) in &source[index..end] {
                let range = state
                    .get_addr_range(parent_pid, VirtAddr::from(vaddr))
                    .ok_or(AxError::Io)?;
                ranges.insert(vaddr, range);
            }
        }
        groups.push(InheritedGroup { inner, ranges });
        index = end;
    }

    {
        let mut manager = SHM_MANAGER.lock();
        if manager
            .attachment_count
            .checked_add(inherited.len())
            .is_none_or(|total| total > MAX_SHM_ATTACHMENTS)
        {
            return Err(AxError::NoMemory);
        }
        if manager.pid_vaddr_shmid.contains_key(&child_pid) {
            return Err(AxError::Io);
        }
        manager
            .pid_vaddr_shmid
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
    }

    let inherited_count = inherited.len();
    {
        let mut manager = SHM_MANAGER.lock();
        manager.pid_vaddr_shmid.insert(child_pid, inherited);
        manager.attachment_count += inherited_count;
    }
    for group in groups {
        let count = group.ranges.len();
        let mut state = group.inner.lock();
        let old = state.va_ranges.insert(child_pid, group.ranges);
        debug_assert!(old.is_none(), "duplicate inherited SysV SHM process bucket");
        // Global admission caps all attachments at 65,536, so this update is
        // both allocation-free and arithmetically exact on every target.
        state.shmid_ds.shm_nattch += count as c_ulong;
    }
    Ok(())
}

pub fn clear_proc_shm(pid: Pid) {
    let _transaction = SHM_TRANSACTION.lock();
    loop {
        let detached = {
            let mut manager = SHM_MANAGER.lock();
            let next = manager
                .pid_vaddr_shmid
                .get(&pid)
                .and_then(|attachments| attachments.iter().next())
                .map(|(&vaddr, &shmid)| (vaddr, shmid));
            let Some((vaddr, shmid)) = next else {
                manager.remove_pid(pid);
                break;
            };
            manager.remove_shmaddr(pid, VirtAddr::from(vaddr));
            (vaddr, shmid, manager.get_inner_by_shmid(shmid))
        };
        let (vaddr, shmid, Some(inner)) = detached else {
            continue;
        };
        let vaddr = VirtAddr::from(vaddr);
        let remove = {
            let mut state = inner.lock();
            state.detach_process(pid, vaddr);
            (state.rmid && state.attach_count() == 0).then_some(state.page_num)
        };
        if let Some(page_num) = remove {
            SHM_MANAGER.lock().remove_shmid(shmid, page_num);
        }
    }
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

pub(crate) fn sysvipc_shm_snapshot() -> AxResult<String> {
    let _transaction = SHM_TRANSACTION.lock();
    const HEADER: &str = "       key      shmid perms                  size  cpid  lpid nattch   \
                          uid   gid  cuid  cgid      atime      dtime      ctime        rss       \
                          swap\n";
    const MAX_ROW_LEN: usize = 256;
    let segment_count = SHM_MANAGER.lock().shmid_inner.len();
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(segment_count)
        .map_err(|_| AxError::NoMemory)?;
    {
        let manager = SHM_MANAGER.lock();
        segments.extend(
            manager
                .shmid_inner
                .iter()
                .map(|(&shmid, inner)| (shmid, inner.clone())),
        );
    }
    let capacity = segment_count
        .checked_mul(MAX_ROW_LEN)
        .and_then(|rows| rows.checked_add(HEADER.len()))
        .ok_or(AxError::NoMemory)?;
    let mut out = String::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    out.push_str(HEADER);
    for (shmid, shm_inner) in segments {
        let shm_inner = shm_inner.lock();
        let ds = shm_inner.shmid_ds;
        let rss_bytes = shm_inner.page_num.saturating_mul(PAGE_SIZE_4K);
        writeln!(
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
        )
        .map_err(|_| AxError::NoMemory)?;
    }
    Ok(out)
}

pub fn sys_shmget(key: i32, size: usize, shmflg: usize) -> AxResult<isize> {
    let perm_mode = (shmflg as __kernel_mode_t) & IPC_MODE_MASK;
    let create = shmflg & IPC_CREAT as usize != 0;
    let excl = shmflg & IPC_EXCL as usize != 0;
    let huge = shmflg & SHM_HUGETLB_FLAG != 0;

    let mut mapping_flags = MappingFlags::USER;
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
    let ids = curr.as_thread().current_cred().ids();
    let euid = ids.euid;
    let egid = ids.egid;

    if huge {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    let _transaction = SHM_TRANSACTION.lock();

    if key != IPC_PRIVATE {
        let existing = {
            let manager = SHM_MANAGER.lock();
            manager.get_shmid_by_key(key).and_then(|shmid| {
                manager
                    .get_inner_by_shmid(shmid)
                    .map(|inner| (shmid, inner))
            })
        };
        if let Some((_shmid, shm_inner)) = existing {
            if create && excl {
                return Err(AxError::from(LinuxError::EEXIST));
            }
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
    let shmid = {
        let mut manager = SHM_MANAGER.lock();
        if manager.active_segment_count() >= shmmni_limit() {
            return Err(AxError::from(LinuxError::ENOSPC));
        }
        if manager
            .total_page_count()
            .checked_add(page_num)
            .is_none_or(|total| total > shmall_limit())
        {
            return Err(AxError::from(LinuxError::ENOSPC));
        }
        manager.try_reserve_segment(key != IPC_PRIVATE)?;
        allocate_shm_id(&manager)
    };

    let shm_inner = Arc::try_new(Mutex::new(ShmInner::new(
        key,
        shmid,
        size,
        mapping_flags,
        perm_mode,
        cur_pid,
        euid,
        egid,
    )))
    .map_err(|_| AxError::NoMemory)?;
    let mut manager = SHM_MANAGER.lock();
    manager.insert_shmid_inner(shmid, page_num, shm_inner)?;
    if key != IPC_PRIVATE {
        manager.insert_key_shmid(key, shmid);
    }

    Ok(shmid as isize)
}

pub fn sys_shmat(shmid: i32, addr: usize, shmflg: u32) -> AxResult<isize> {
    let _transaction = SHM_TRANSACTION.lock();
    let shm_inner = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(AxError::InvalidInput)?
    };
    let shm_flg = ShmAtFlags::from_bits_truncate(shmflg);
    let read_only = shm_flg.contains(ShmAtFlags::SHM_RDONLY);

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let pid = proc_data.proc.pid();
    let ids = curr.as_thread().current_cred().ids();
    let (mut mapping_flags, page_num, existing_pages) = {
        let state = shm_inner.lock();
        if state.rmid {
            return Err(AxError::from(LinuxError::EIDRM));
        }
        if !can_attach_shm(&state.shmid_ds.shm_perm, ids.euid, ids.egid, read_only) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        (
            state.mapping_flags,
            state.page_num,
            state.phys_pages.clone(),
        )
    };
    if read_only {
        mapping_flags.remove(MappingFlags::WRITE);
    }
    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let length = page_num
        .checked_mul(PAGE_SIZE_4K)
        .ok_or(AxError::NoMemory)?;
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
    let end_addr = VirtAddr::from(
        start_addr
            .as_usize()
            .checked_add(length)
            .ok_or(AxError::NoMemory)?,
    );
    let va_range = VirtAddrRange::new(start_addr, end_addr);

    let (pages, backend, first_attach) = if let Some(pages) = existing_pages {
        let backend = Backend::try_new_shared(start_addr, pages.clone())?;
        (pages, backend, false)
    } else {
        let pages = Arc::try_new(SharedPages::new(length, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        let backend = Backend::try_new_shared(start_addr, pages.clone())?;
        (pages, backend, true)
    };

    {
        let mut manager = SHM_MANAGER.lock();
        if manager.get_shmid_by_vaddr(pid, start_addr).is_some() {
            return Err(AxError::InvalidInput);
        }
        manager.try_reserve_process_attaches(pid, 1)?;
    }
    if let Err(error) = shm_inner.lock().try_reserve_process_attaches(pid, 1) {
        SHM_MANAGER.lock().cancel_empty_process_reservation(pid);
        return Err(error);
    }

    if let Err(error) = aspace.map(start_addr, length, mapping_flags, false, backend) {
        drop(aspace);
        shm_inner.lock().cancel_empty_process_reservation(pid);
        SHM_MANAGER.lock().cancel_empty_process_reservation(pid);
        return Err(error);
    }

    {
        let mut state = shm_inner.lock();
        if first_attach {
            state.map_to_phys(pages);
        }
        state.attach_process(pid, va_range);
    }
    SHM_MANAGER
        .lock()
        .insert_shmid_vaddr(pid, shmid, start_addr);
    Ok(start_addr.as_usize() as isize)
}

pub fn sys_shmctl(shmid: i32, cmd: u32, buf: UserPtr<ShmidDs>) -> AxResult<isize> {
    let curr = current();
    let ids = curr.as_thread().current_cred().ids();
    let current_uid = ids.euid;
    let current_gid = ids.egid;
    let cmd = cmd as i32;
    let _transaction = SHM_TRANSACTION.lock();

    if cmd == IPC_INFO {
        let info = IpcInfo {
            shmmax: shmmax_limit() as c_ulong,
            shmmin: SHMMIN as c_ulong,
            shmmni: shmmni_limit() as c_ulong,
            shmseg: shmmni_limit() as c_ulong,
            shmall: shmall_limit() as c_ulong,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            reserved4: 0,
        };
        let index = SHM_MANAGER.lock().max_active_index();
        *buf.cast::<IpcInfo>().get_as_mut()? = info;
        return Ok(index);
    }
    if cmd == SHM_INFO {
        let manager = SHM_MANAGER.lock();
        let pages = manager.total_page_count() as c_ulong;
        let info = ShmUsageInfo {
            used_ids: manager.active_segment_count() as i32,
            shm_tot: pages,
            shm_rss: pages,
            shm_swp: 0,
            swap_attempts: 0,
            swap_successes: 0,
        };
        let index = manager.max_active_index();
        drop(manager);
        *buf.cast::<ShmUsageInfo>().get_as_mut()? = info;
        return Ok(index);
    }

    let shm_inner = SHM_MANAGER
        .lock()
        .get_inner_by_shmid(shmid)
        .ok_or(AxError::InvalidInput)?;
    if cmd == SHM_STAT || cmd == SHM_STAT_ANY {
        let state = shm_inner.lock();
        if cmd == SHM_STAT
            && !super::has_ipc_permission(&state.shmid_ds.shm_perm, current_uid, current_gid, false)
        {
            return Err(AxError::from(LinuxError::EACCES));
        }
        let snapshot = state.shmid_ds;
        drop(state);
        *buf.get_as_mut()? = snapshot;
        return Ok(shmid as isize);
    }

    if cmd == IPC_SET {
        if !admin_ipc_permission(&shm_inner.lock().shmid_ds.shm_perm, current_uid) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        let user_ds = *buf.get_as_mut()?;
        let mut state = shm_inner.lock();
        state.shmid_ds.shm_perm.uid = user_ds.shm_perm.uid;
        state.shmid_ds.shm_perm.gid = user_ds.shm_perm.gid;
        state.shmid_ds.shm_perm.mode = (state.shmid_ds.shm_perm.mode
            & !(IPC_MODE_MASK as c_ushort))
            | (user_ds.shm_perm.mode & IPC_MODE_MASK as c_ushort);
        state.shmid_ds.shm_ctime = wall_time().as_secs() as __kernel_time_t;
    } else if cmd == IPC_STAT {
        let state = shm_inner.lock();
        if !super::has_ipc_permission(&state.shmid_ds.shm_perm, current_uid, current_gid, false) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        let snapshot = state.shmid_ds;
        drop(state);
        if let Some(shmid_ds) = nullable!(buf.get_as_mut())? {
            *shmid_ds = snapshot;
        }
    } else if cmd == IPC_RMID {
        let remove = {
            let mut state = shm_inner.lock();
            if !admin_ipc_permission(&state.shmid_ds.shm_perm, current_uid) {
                return Err(AxError::from(LinuxError::EPERM));
            }
            state.set_removed(true);
            (state.attach_count() == 0).then_some(state.page_num)
        };
        let mut manager = SHM_MANAGER.lock();
        manager.remove_key_by_shmid(shmid);
        if let Some(page_num) = remove {
            manager.remove_shmid(shmid, page_num);
        }
    } else if cmd == SHM_LOCK {
        let mut state = shm_inner.lock();
        if !admin_ipc_permission(&state.shmid_ds.shm_perm, current_uid) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        state.set_locked(true);
    } else if cmd == SHM_UNLOCK {
        let mut state = shm_inner.lock();
        if !admin_ipc_permission(&state.shmid_ds.shm_perm, current_uid) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        state.set_locked(false);
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
    let _transaction = SHM_TRANSACTION.lock();

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
    let va_range = shm_inner
        .lock()
        .get_addr_range(pid, shmaddr)
        .ok_or(AxError::InvalidInput)?;

    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    aspace.unmap(va_range.start, va_range.size())?;

    SHM_MANAGER.lock().remove_shmaddr(pid, shmaddr);
    let remove = {
        let mut state = shm_inner.lock();
        state.detach_process(pid, shmaddr);
        (state.rmid && state.attach_count() == 0).then_some(state.page_num)
    };
    if let Some(page_num) = remove {
        SHM_MANAGER.lock().remove_shmid(shmid, page_num);
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_segment(key: i32, shmid: i32, page_num: usize) -> Arc<Mutex<ShmInner>> {
        Arc::new(Mutex::new(ShmInner {
            shmid,
            page_num,
            va_ranges: HashMap::new(),
            phys_pages: None,
            rmid: false,
            mapping_flags: MappingFlags::empty(),
            shmid_ds: ShmidDs {
                shm_perm: IpcPerm {
                    key,
                    uid: 0,
                    gid: 0,
                    cuid: 0,
                    cgid: 0,
                    mode: 0,
                    pad1: 0,
                    seq: 0,
                    pad2: 0,
                    unused0: 0,
                    unused1: 0,
                },
                shm_segsz: (page_num * PAGE_SIZE_4K) as __kernel_size_t,
                shm_atime: 0,
                shm_dtime: 0,
                shm_ctime: 0,
                shm_cpid: 0,
                shm_lpid: 0,
                shm_nattch: 0,
                unused4: 0,
                unused5: 0,
            },
        }))
    }

    #[test]
    fn rmid_unlinks_key_but_retains_pages_until_final_removal() {
        let key = 42;
        let shmid = 7;
        let page_num = 3;
        let inner = test_segment(key, shmid, page_num);
        let mut manager = ShmManager::new();
        manager.try_reserve_segment(true).unwrap();
        manager
            .insert_shmid_inner(shmid, page_num, inner.clone())
            .unwrap();
        manager.insert_key_shmid(key, shmid);

        inner.lock().set_removed(true);
        assert_eq!(inner.lock().shmid_ds.shm_perm.key, IPC_PRIVATE);
        manager.remove_key_by_shmid(shmid);
        assert_eq!(manager.get_shmid_by_key(key), None);
        assert!(manager.contains_shmid(shmid));
        assert_eq!(manager.total_page_count(), page_num);

        manager.remove_shmid(shmid, page_num);
        assert!(!manager.contains_shmid(shmid));
        assert_eq!(manager.total_page_count(), 0);
    }

    #[test]
    fn process_attachment_reservation_rolls_back_empty_buckets_and_accounts_exactly() {
        let pid = 11;
        let mut manager = ShmManager::new();
        manager.try_reserve_process_attaches(pid, 2).unwrap();
        assert!(
            manager
                .pid_vaddr_shmid
                .get(&pid)
                .is_some_and(HashMap::is_empty)
        );
        manager.cancel_empty_process_reservation(pid);
        assert!(!manager.pid_vaddr_shmid.contains_key(&pid));

        manager.try_reserve_process_attaches(pid, 2).unwrap();
        manager.insert_shmid_vaddr(pid, 1, VirtAddr::from(0x1000));
        manager.insert_shmid_vaddr(pid, 2, VirtAddr::from(0x2000));
        assert_eq!(manager.attachment_count, 2);
        manager.remove_shmaddr(pid, VirtAddr::from(0x1000));
        assert_eq!(manager.attachment_count, 1);
        assert!(manager.pid_vaddr_shmid.contains_key(&pid));
        manager.remove_shmaddr(pid, VirtAddr::from(0x2000));
        assert_eq!(manager.attachment_count, 0);
        assert!(!manager.pid_vaddr_shmid.contains_key(&pid));
    }
}
