use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write as _,
    hash::Hash,
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
#[cfg(any(not(test), target_os = "none"))]
pub(super) use axsync::Mutex;
use axtask::current;
use hashbrown::HashMap;
use linux_raw_sys::{
    ctypes::{c_ulong, c_ushort},
    general::*,
};
use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
#[cfg(all(test, not(target_os = "none")))]
pub(super) use spin::Mutex;
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use super::{
    IPC_CREAT, IPC_EXCL, IPC_INFO, IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, IpcAccess,
    IpcAccessContext, IpcNamespace, IpcPerm, IpcPermissionUpdateRequest, SHM_DEST, SHM_INFO,
    SHM_LOCK, SHM_LOCKED, SHM_STAT, SHM_STAT_ANY, SHM_UNLOCK, SHMMIN, ShmLockCharge,
    allocate_ipc_id, shmall_limit, shmmax_limit, shmmni_limit,
};
use crate::{
    mm::{
        Backend, DeferredMappingFinalizer, MappingFinalizer, SharedPages, check_rlimit_as_growth,
        map_usercopy_error,
    },
    task::AsThread,
    time::wall_time,
};

const IPC_MODE_MASK: __kernel_mode_t = 0o777;
const SHM_HUGETLB_FLAG: usize = 0o4000;
const MAX_SHM_ATTACHMENTS: usize = 65_536;
const SHMLBA: usize = PAGE_SIZE_4K;

/// Namespace-global logical identity for one SysV attachment.  A mapping may
/// be split or relocated, so its base address is never its lifetime identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ShmAttachmentId(u64);

static NEXT_SHM_ATTACHMENT_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_attachment_id() -> AxResult<ShmAttachmentId> {
    // Zero is reserved as an impossible identity.  Exhaustion fails closed:
    // reusing an ID could let a stale VMA lease finalize a new attachment.
    NEXT_SHM_ATTACHMENT_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1).filter(|next| *next != 0)
        })
        .map(ShmAttachmentId)
        .map_err(|_| AxError::NoMemory)
}

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

fn can_attach_shm(
    context: &IpcAccessContext,
    perm: &IpcPerm,
    read_only: bool,
    executable: bool,
) -> bool {
    context.allows(perm, IpcAccess::Read)
        && (read_only || context.allows(perm, IpcAccess::Write))
        && (!executable || context.allows(perm, IpcAccess::Execute))
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
        /* attach with execute permission */
        const SHM_EXEC = 0o100000;
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

// These records contain Linux ABI padding through their embedded `IpcPerm`
// and the native-word fields following the two PID values.  Keep the layout
// explicit and materialize a zeroed mirror before copyout so no Rust padding
// bytes are ever sent to userspace.
const _: () = {
    assert!(align_of::<IpcPerm>() == 8);
    assert!(size_of::<IpcPerm>() == 48);
    assert!(offset_of!(IpcPerm, key) == 0);
    assert!(offset_of!(IpcPerm, mode) == 20);
    assert!(offset_of!(IpcPerm, unused0) == 32);
    assert!(offset_of!(IpcPerm, unused1) == 40);
    assert!(align_of::<ShmidDs>() == 8);
    assert!(size_of::<ShmidDs>() == 112);
    assert!(offset_of!(ShmidDs, shm_perm) == 0);
    assert!(offset_of!(ShmidDs, shm_segsz) == 48);
    assert!(offset_of!(ShmidDs, shm_atime) == 56);
    assert!(offset_of!(ShmidDs, shm_dtime) == 64);
    assert!(offset_of!(ShmidDs, shm_ctime) == 72);
    assert!(offset_of!(ShmidDs, shm_cpid) == 80);
    assert!(offset_of!(ShmidDs, shm_lpid) == 84);
    assert!(offset_of!(ShmidDs, shm_nattch) == 88);
    assert!(offset_of!(ShmidDs, unused4) == 96);
    assert!(offset_of!(ShmidDs, unused5) == 104);
};

const _: () = {
    assert!(align_of::<IpcInfo>() == 8);
    assert!(size_of::<IpcInfo>() == 72);
    assert!(align_of::<ShmUsageInfo>() == 8);
    assert!(size_of::<ShmUsageInfo>() == 48);
    assert!(offset_of!(ShmUsageInfo, used_ids) == 0);
    assert!(offset_of!(ShmUsageInfo, shm_tot) == 8);
    assert!(offset_of!(ShmUsageInfo, shm_rss) == 16);
    assert!(offset_of!(ShmUsageInfo, shm_swp) == 24);
    assert!(offset_of!(ShmUsageInfo, swap_attempts) == 32);
    assert!(offset_of!(ShmUsageInfo, swap_successes) == 40);
};

fn initialized_ipc_perm(value: &IpcPerm) -> IpcPerm {
    // SAFETY: every field is an integer scalar and zero is a valid
    // representation.  Starting from zero also initializes the implicit
    // four-byte alignment hole before the native-word fields.
    let mut result: IpcPerm = unsafe { core::mem::zeroed() };
    result.key = value.key;
    result.uid = value.uid;
    result.gid = value.gid;
    result.cuid = value.cuid;
    result.cgid = value.cgid;
    result.mode = value.mode;
    result.pad1 = value.pad1;
    result.seq = value.seq;
    result.pad2 = value.pad2;
    result.unused0 = value.unused0;
    result.unused1 = value.unused1;
    result
}

fn initialized_shmid_ds(value: &ShmidDs) -> ShmidDs {
    // SAFETY: every field is an integer scalar and zero is a valid
    // representation.  The zeroed value initializes the ABI alignment bytes
    // that Rust does not expose as fields.
    let mut result: ShmidDs = unsafe { core::mem::zeroed() };
    result.shm_perm = initialized_ipc_perm(&value.shm_perm);
    result.shm_segsz = value.shm_segsz;
    result.shm_atime = value.shm_atime;
    result.shm_dtime = value.shm_dtime;
    result.shm_ctime = value.shm_ctime;
    result.shm_cpid = value.shm_cpid;
    result.shm_lpid = value.shm_lpid;
    result.shm_nattch = value.shm_nattch;
    result.unused4 = value.unused4;
    result.unused5 = value.unused5;
    result
}

fn initialized_ipc_info(value: &IpcInfo) -> IpcInfo {
    // SAFETY: all fields are native-word integer scalars; zero initializes the
    // complete object even on a target that inserts alignment bytes.
    let mut result: IpcInfo = unsafe { core::mem::zeroed() };
    result.shmmax = value.shmmax;
    result.shmmin = value.shmmin;
    result.shmmni = value.shmmni;
    result.shmseg = value.shmseg;
    result.shmall = value.shmall;
    result.reserved1 = value.reserved1;
    result.reserved2 = value.reserved2;
    result.reserved3 = value.reserved3;
    result.reserved4 = value.reserved4;
    result
}

fn initialized_shm_usage_info(value: &ShmUsageInfo) -> ShmUsageInfo {
    // SAFETY: zeroing initializes the four-byte alignment hole after
    // `used_ids`; every other field is an integer scalar.
    let mut result: ShmUsageInfo = unsafe { core::mem::zeroed() };
    result.used_ids = value.used_ids;
    result.shm_tot = value.shm_tot;
    result.shm_rss = value.shm_rss;
    result.shm_swp = value.shm_swp;
    result.swap_attempts = value.swap_attempts;
    result.swap_successes = value.swap_successes;
    result
}

fn write_shmid_ds<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut ShmidDs,
    value: &ShmidDs,
) -> AxResult<()> {
    // SAFETY: `initialized_shmid_ds` zeroes every padding byte and the layout
    // assertions above cover the complete Linux object extent.
    unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, initialized_shmid_ds(value)) }
        .map_err(map_usercopy_error)
}

fn write_ipc_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut IpcInfo,
    value: &IpcInfo,
) -> AxResult<()> {
    // SAFETY: the mirror is fully initialized, including any target padding.
    unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, initialized_ipc_info(value)) }
        .map_err(map_usercopy_error)
}

fn write_shm_usage_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut ShmUsageInfo,
    value: &ShmUsageInfo,
) -> AxResult<()> {
    // SAFETY: `initialized_shm_usage_info` zeroes the ABI alignment hole and
    // the layout assertions above cover the complete record.
    unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, initialized_shm_usage_info(value)) }
        .map_err(map_usercopy_error)
}

fn read_shmid_ds<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const ShmidDs,
) -> AxResult<ShmidDs> {
    let value = VmPtr::vm_read_uninit(ptr, memory).map_err(map_usercopy_error)?;
    // SAFETY: `vm_read_uninit` initialized every byte of the complete object.
    Ok(unsafe { value.assume_init() })
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

/// One visibility bit shared by every manager/segment index installed for a
/// fork-child SHM inheritance transaction.
struct ShmForkPublication {
    visible: AtomicBool,
}

impl ShmForkPublication {
    const fn new(visible: bool) -> Self {
        Self {
            visible: AtomicBool::new(visible),
        }
    }

    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }
}

/// One process-local attachment map with an independently controlled
/// publication point. Normal shmat buckets start visible; fork preparation
/// installs hidden buckets sharing one [`ShmForkPublication`].
struct ProcessAttachmentMap<T> {
    publication: Arc<ShmForkPublication>,
    // Logical attachment IDs, never virtual addresses, own these entries.
    // A later SHM_REMAP is allowed to install a hidden replacement at the
    // same detach address as its still-live predecessor; an address-keyed
    // map made that valid transaction look like a duplicate attachment.
    entries: HashMap<ShmAttachmentId, ShmAttachment<T>>,
}

impl<T> ProcessAttachmentMap<T> {
    fn is_visible(&self) -> bool {
        self.publication.is_visible()
    }
}

struct ShmAttachment<T> {
    publication: Arc<ShmForkPublication>,
    value: T,
}

impl<T> ShmAttachment<T> {
    fn is_visible(&self) -> bool {
        self.publication.is_visible()
    }
}

/// Both SysV attachment indexes carry the same immutable logical provenance.
/// Keeping the base, full requested range, and shmid in both indexes makes a
/// final VMA release independently revalidatable: neither index may be used
/// as an address-only authority after MAP_FIXED, mremap, or VMA splitting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShmAttachmentRecord {
    pub(crate) id: ShmAttachmentId,
    /// The address accepted by shmdt.  It is logical attachment geometry,
    /// not an index key and need not remain the first extant VMA address.
    pub(crate) base: VirtAddr,
    /// The original SysV segment extent requested by shmat.
    pub(crate) range: VirtAddrRange,
    pub(crate) shmid: i32,
    /// Identity of the owning DeferredMappingFinalizer, or zero for a hidden
    /// fork record whose child VMA has not been rebound yet.
    pub(crate) finalizer_identity: usize,
}

impl ShmAttachmentRecord {
    fn new(id: ShmAttachmentId, base: VirtAddr, range: VirtAddrRange, shmid: i32) -> Self {
        Self {
            id,
            base,
            range,
            shmid,
            finalizer_identity: 0,
        }
    }

    /// Linux's logical `shmdt` address for this attachment.
    pub(crate) fn detach_base(self) -> VirtAddr {
        self.base
    }

    /// Immutable SysV segment extent.  The MM finalizer identity owns the
    /// mutable set of mapped VMA fragments, so fragment splitting never
    /// changes this logical segment geometry.
    pub(crate) fn segment_size(self) -> usize {
        self.range.size()
    }
}

fn attachment_at_base<'a>(
    entries: &'a HashMap<ShmAttachmentId, ShmAttachment<ShmAttachmentRecord>>,
    base: VirtAddr,
) -> Option<&'a ShmAttachment<ShmAttachmentRecord>> {
    entries
        .values()
        .find(|attachment| attachment.value.detach_base() == base)
}

fn attachment_id_at_base(
    entries: &HashMap<ShmAttachmentId, ShmAttachment<ShmAttachmentRecord>>,
    base: VirtAddr,
) -> Option<ShmAttachmentId> {
    entries
        .iter()
        .find(|(_, attachment)| attachment.value.detach_base() == base)
        .map(|(&id, _)| id)
}

/// This struct is used to maintain the shmem in kernel.
pub struct ShmInner {
    /// Shared memory segment identifier.
    pub shmid: i32,
    /// Number of pages in the shared memory segment.
    pub page_num: usize,
    va_ranges: HashMap<Pid, ProcessAttachmentMap<ShmAttachmentRecord>>,
    /// physical pages
    pub phys_pages: Option<Arc<SharedPages>>,
    /// Canonical pages reserved by concurrent first `shmat` calls before any
    /// one of their MM transactions has committed.
    pending_first_pages: Option<Arc<SharedPages>>,
    pending_first_attaches: usize,
    /// whether remove on last detach, see shm_ctl
    pub rmid: bool,
    /// Mapping flags used for this shared memory segment.
    pub mapping_flags: MappingFlags,
    /// c type struct, used in shm_ctl
    pub shmid_ds: ShmidDs,
    /// Retains the namespace-local RLIMIT_MEMLOCK charge while SHM_LOCK is set.
    lock_charge: Option<ShmLockCharge>,
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
            pending_first_pages: None,
            pending_first_attaches: 0,
            rmid: false,
            mapping_flags,
            shmid_ds: ShmidDs::new(key, size, perm_mode, pid as __kernel_pid_t, uid, gid),
            lock_charge: None,
        }
    }

    /// Updates the pid of last shmop and checks if the size and mapping flags
    /// match.
    fn try_update(
        &self,
        context: &IpcAccessContext,
        size: usize,
        requested_mode: __kernel_mode_t,
    ) -> AxResult<isize> {
        if size > self.shmid_ds.shm_segsz as usize {
            return Err(AxError::InvalidInput);
        }

        if !context.allows_requested_mode(&self.shmid_ds.shm_perm, requested_mode as _) {
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
        self.va_ranges
            .values()
            .filter(|ranges| ranges.is_visible())
            .map(|ranges| {
                ranges
                    .entries
                    .values()
                    .filter(|attachment| attachment.is_visible())
                    .count()
            })
            .sum()
    }

    /// Returns the ABI-visible segment metadata. The attachment count is
    /// derived from published buckets so an in-flight fork reservation can
    /// never leak through IPC_STAT or /proc/sysvipc/shm.
    fn visible_snapshot(&self) -> ShmidDs {
        let mut snapshot = initialized_shmid_ds(&self.shmid_ds);
        snapshot.shm_nattch = self.attach_count() as c_ulong;
        snapshot
    }

    fn refresh_cached_attach_count(&mut self) {
        self.shmid_ds.shm_nattch = self.attach_count() as c_ulong;
    }

    /// Returns whether a live attachment or an invisible fork reservation owns
    /// this segment. IPC_RMID may hide the key immediately, but final page and
    /// shmid destruction must wait for both forms of ownership to disappear.
    fn has_attachment_owners(&self) -> bool {
        self.va_ranges
            .values()
            .any(|ranges| !ranges.entries.is_empty())
    }

    /// Returns the virtual address range associated with an attach address.
    pub fn get_addr_range(&self, pid: Pid, vaddr: VirtAddr) -> Option<VirtAddrRange> {
        self.va_ranges
            .get(&pid)
            .filter(|ranges| ranges.is_visible())
            .and_then(|ranges| attachment_at_base(&ranges.entries, vaddr))
            .filter(|attachment| attachment.is_visible())
            .map(|attachment| attachment.value.range)
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
            let publication =
                Arc::try_new(ShmForkPublication::new(true)).map_err(|_| AxError::NoMemory)?;
            self.va_ranges.insert(
                pid,
                ProcessAttachmentMap {
                    publication,
                    entries: ranges,
                },
            );
        } else {
            let ranges = self.va_ranges.get_mut(&pid).ok_or(AxError::Io)?;
            if !ranges.is_visible() {
                return Err(AxError::ResourceBusy);
            }
            ranges
                .entries
                .try_reserve(additional)
                .map_err(|_| AxError::NoMemory)?;
        }
        Ok(())
    }

    fn cancel_empty_process_reservation(&mut self, pid: Pid) {
        if self
            .va_ranges
            .get(&pid)
            .is_some_and(|ranges| ranges.is_visible() && ranges.entries.is_empty())
        {
            self.va_ranges.remove(&pid);
        }
    }

    /// Called by sys_shmdt
    pub fn detach_process(&mut self, pid: Pid, vaddr: VirtAddr) -> Option<VirtAddrRange> {
        let ranges = self.va_ranges.get_mut(&pid)?;
        if !ranges.is_visible() {
            return None;
        }
        let id = attachment_id_at_base(&ranges.entries, vaddr)?;
        if !ranges
            .entries
            .get(&id)
            .is_some_and(ShmAttachment::is_visible)
        {
            return None;
        }
        let va_range = ranges.entries.remove(&id)?.value.range;
        if ranges.entries.is_empty() {
            self.va_ranges.remove(&pid);
        }
        self.refresh_cached_attach_count();
        self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
        self.shmid_ds.shm_dtime = wall_time().as_secs() as __kernel_time_t;
        Some(va_range)
    }

    /// Removes one exact logical attachment. Address-only teardown is not
    /// sufficient while SHM_REMAP may keep an old and a hidden replacement at
    /// the same detach base in one transaction.
    fn detach_process_exact(&mut self, pid: Pid, record: ShmAttachmentRecord) -> bool {
        let mut empty = false;
        let removed = self.va_ranges.get_mut(&pid).and_then(|ranges| {
            if !ranges.is_visible() {
                return None;
            }
            let attachment = ranges.entries.get(&record.id)?;
            if !attachment.is_visible() || attachment.value != record {
                return None;
            }
            let removed = ranges.entries.remove(&record.id);
            empty = ranges.entries.is_empty();
            removed
        });
        if empty {
            self.va_ranges.remove(&pid);
        }
        if removed.is_some() {
            self.refresh_cached_attach_count();
            self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
            self.shmid_ds.shm_dtime = wall_time().as_secs() as __kernel_time_t;
            true
        } else {
            false
        }
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

/// Owns one reference to a namespace-local first-attach page reservation.
/// It is intentionally independent of the MM lock: the reservation is made
/// under IPC, used after IPC is released, and either committed or precisely
/// released afterwards.
#[must_use = "dropping a first-attach reservation releases its pending reference"]
struct FirstShmatPagesReservation {
    inner: Arc<Mutex<ShmInner>>,
    pages: Arc<SharedPages>,
    committed: bool,
}

impl FirstShmatPagesReservation {
    fn commit(mut self) {
        let mut state = self.inner.lock();
        if state.phys_pages.is_none()
            && state
                .pending_first_pages
                .as_ref()
                .is_some_and(|pages| Arc::ptr_eq(pages, &self.pages))
        {
            state.phys_pages = Some(self.pages.clone());
        }
        state.pending_first_attaches = state.pending_first_attaches.saturating_sub(1);
        if state.phys_pages.is_some() || state.pending_first_attaches == 0 {
            state.pending_first_pages = None;
            state.pending_first_attaches = 0;
        }
        self.committed = true;
    }
}

impl Drop for FirstShmatPagesReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self.inner.lock();
        if state
            .pending_first_pages
            .as_ref()
            .is_some_and(|pages| Arc::ptr_eq(pages, &self.pages))
        {
            state.pending_first_attaches = state.pending_first_attaches.saturating_sub(1);
            if state.pending_first_attaches == 0 && state.phys_pages.is_none() {
                state.pending_first_pages = None;
            }
        }
    }
}

fn reserve_first_shmat_pages(
    inner: Arc<Mutex<ShmInner>>,
    provisional: Arc<SharedPages>,
) -> AxResult<(Arc<SharedPages>, Option<FirstShmatPagesReservation>)> {
    let mut state = inner.lock();
    if let Some(pages) = state.phys_pages.clone() {
        return Ok((pages, None));
    }
    let pages = match state.pending_first_pages.clone() {
        Some(pages) => pages,
        None => {
            state.pending_first_pages = Some(provisional);
            state
                .pending_first_pages
                .as_ref()
                .expect("reserved pending pages")
                .clone()
        }
    };
    state.pending_first_attaches = state
        .pending_first_attaches
        .checked_add(1)
        .ok_or(AxError::NoMemory)?;
    drop(state);
    let reservation = FirstShmatPagesReservation {
        inner,
        pages: pages.clone(),
        committed: false,
    };
    Ok((pages, Some(reservation)))
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
    /// pid -> attach address -> shm_id. Hidden fork buckets are charged here
    /// but filtered by every reader until their shared publication bit flips.
    pid_vaddr_shmid: HashMap<Pid, ProcessAttachmentMap<ShmAttachmentRecord>>,
    attachment_count: usize,
}

impl ShmManager {
    pub(crate) fn new() -> Self {
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
            .filter(|map| map.is_visible())
            .and_then(|map| attachment_at_base(&map.entries, vaddr))
            .filter(|attachment| attachment.is_visible())
            .map(|attachment| attachment.value.shmid)
    }

    // used by garbage collection
    #[allow(dead_code)]
    fn find_vaddr_by_shmid(&self, pid: Pid, shmid: i32) -> Option<VirtAddr> {
        self.pid_vaddr_shmid
            .get(&pid)
            .filter(|map| map.is_visible())
            .and_then(|map| {
                map.entries
                    .iter()
                    .find(|(_, attachment)| {
                        attachment.is_visible() && attachment.value.shmid == shmid
                    })
                    .map(|(_, attachment)| attachment.value.base)
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
            let publication =
                Arc::try_new(ShmForkPublication::new(true)).map_err(|_| AxError::NoMemory)?;
            self.pid_vaddr_shmid.insert(
                pid,
                ProcessAttachmentMap {
                    publication,
                    entries: attachments,
                },
            );
        } else {
            let attachments = self.pid_vaddr_shmid.get_mut(&pid).ok_or(AxError::Io)?;
            if !attachments.is_visible() {
                return Err(AxError::ResourceBusy);
            }
            attachments
                .entries
                .try_reserve(additional)
                .map_err(|_| AxError::NoMemory)?;
        }
        Ok(())
    }

    fn cancel_empty_process_reservation(&mut self, pid: Pid) {
        if self
            .pid_vaddr_shmid
            .get(&pid)
            .is_some_and(|attachments| attachments.is_visible() && attachments.entries.is_empty())
        {
            self.pid_vaddr_shmid.remove(&pid);
        }
    }

    /// Removes the mapping from a process and shared memory address.
    pub fn remove_shmaddr(&mut self, pid: Pid, shmaddr: VirtAddr) {
        let mut empty: bool = false;
        if let Some(map) = self.pid_vaddr_shmid.get_mut(&pid) {
            if !map.is_visible() {
                return;
            }
            let Some(id) = attachment_id_at_base(&map.entries, shmaddr) else {
                return;
            };
            if !map.entries.get(&id).is_some_and(ShmAttachment::is_visible) {
                return;
            }
            if map.entries.remove(&id).is_some() {
                if let Some(next) = self.attachment_count.checked_sub(1) {
                    self.attachment_count = next;
                } else {
                    error!("SysV SHM manager attachment underflow detaching PID {pid}");
                }
            }
            empty = map.entries.is_empty();
        }
        if empty {
            self.pid_vaddr_shmid.remove(&pid);
        }
    }

    // called when a process exit
    fn remove_pid(&mut self, pid: Pid) {
        let visible = self.pid_vaddr_shmid.get(&pid).is_some_and(|attachments| {
            attachments.is_visible() && attachments.entries.values().all(ShmAttachment::is_visible)
        });
        if !visible {
            return;
        }
        if let Some(attachments) = self.pid_vaddr_shmid.remove(&pid) {
            if let Some(next) = self.attachment_count.checked_sub(attachments.entries.len()) {
                self.attachment_count = next;
            } else {
                error!("SysV SHM manager attachment underflow clearing PID {pid}");
            }
        }
    }

    /// Removes the shared memory segment.
    pub fn remove_shmid(&mut self, shmid: i32, page_num: usize) -> AxResult<()> {
        if !self.shmid_inner.contains_key(&shmid) {
            return Ok(());
        }
        let total_pages = self
            .total_pages
            .checked_sub(page_num)
            .ok_or(AxError::BadState)?;
        self.key_shmid.remove_by_value(&shmid);
        if self.shmid_inner.remove(&shmid).is_none() {
            return Err(AxError::BadState);
        }
        self.total_pages = total_pages;
        // Per-process attach maps are cleaned on shmdt/exit. IPC_RMID only
        // removes the segment once the last attach has gone away.
        Ok(())
    }

    fn attachment_by_id(&self, pid: Pid, id: ShmAttachmentId) -> Option<ShmAttachmentRecord> {
        self.pid_vaddr_shmid
            .get(&pid)
            .filter(|attachments| attachments.is_visible())?
            .entries
            .values()
            .find(|attachment| attachment.is_visible() && attachment.value.id == id)
            .map(|attachment| attachment.value)
    }

    fn remove_attachment_exact(&mut self, pid: Pid, record: ShmAttachmentRecord) -> bool {
        let mut empty = false;
        let removed = self.pid_vaddr_shmid.get_mut(&pid).and_then(|attachments| {
            if !attachments.is_visible() {
                return None;
            }
            let attachment = attachments.entries.get(&record.id)?;
            if !attachment.is_visible() || attachment.value != record {
                return None;
            }
            let removed = attachments.entries.remove(&record.id);
            empty = attachments.entries.is_empty();
            removed
        });
        if empty {
            self.pid_vaddr_shmid.remove(&pid);
        }
        if removed.is_some() {
            self.attachment_count = self.attachment_count.checked_sub(1).unwrap_or_else(|| {
                error!("SysV SHM manager attachment underflow finalizing PID {pid}");
                0
            });
            true
        } else {
            false
        }
    }
}

/// Final VMA ownership release for one published SysV attachment.
///
/// The finalizer performs all IPC mutation in task context, after the last
/// VMA fragment's TLB grace period.  It deliberately requires an exact match
/// in both indexes before mutating either one, so a stale lease can never
/// detach a newly attached segment which reused the same virtual address.
pub(crate) struct SysvAttachmentFinalizer {
    namespace: Arc<IpcNamespace>,
    pid: Pid,
    shmid: i32,
    attachment_id: ShmAttachmentId,
}

impl SysvAttachmentFinalizer {
    fn new(
        namespace: Arc<IpcNamespace>,
        pid: Pid,
        shmid: i32,
        attachment_id: ShmAttachmentId,
    ) -> Self {
        Self {
            namespace,
            pid,
            shmid,
            attachment_id,
        }
    }

    pub(crate) fn attachment_id(&self) -> ShmAttachmentId {
        self.attachment_id
    }
}

/// Preallocates the VMA lease for `record`.  MM fork/rebind code uses this to
/// replace cloned parent ownership with a child-PID finalizer before the fork
/// metadata is published.
pub(crate) fn try_new_sysv_attachment_finalizer(
    namespace: Arc<IpcNamespace>,
    pid: Pid,
    record: ShmAttachmentRecord,
) -> AxResult<DeferredMappingFinalizer> {
    DeferredMappingFinalizer::try_new(Box::new(SysvAttachmentFinalizer::new(
        namespace,
        pid,
        record.shmid,
        record.id,
    )))
}

/// Returns the published attachment provenance at one logical attach base.
/// This is intentionally metadata-only; address-space discovery stays in MM.
pub(crate) fn shm_attachment_record_in_namespace(
    namespace: &IpcNamespace,
    pid: Pid,
    base: VirtAddr,
) -> Option<ShmAttachmentRecord> {
    let _transaction = namespace.shm_transaction().lock();
    namespace
        .shm_manager()
        .lock()
        .pid_vaddr_shmid
        .get(&pid)
        .filter(|attachments| attachments.is_visible())?
        .entries
        .values()
        .find(|attachment| attachment.value.base == base)
        .filter(|attachment| attachment.is_visible())
        .map(|attachment| attachment.value)
}

/// Resolves VMA-owned finalizer identity back to immutable SysV provenance.
/// Unlike base-address lookup this remains useful after a logical mapping was
/// relocated or split by MM.
pub(crate) fn shm_attachment_record_by_finalizer_identity_in_namespace(
    namespace: &IpcNamespace,
    pid: Pid,
    finalizer_identity: usize,
) -> Option<ShmAttachmentRecord> {
    let _transaction = namespace.shm_transaction().lock();
    namespace
        .shm_manager()
        .lock()
        .pid_vaddr_shmid
        .get(&pid)
        .filter(|attachments| attachments.is_visible())?
        .entries
        .values()
        .find(|attachment| {
            attachment.is_visible() && attachment.value.finalizer_identity == finalizer_identity
        })
        .map(|attachment| attachment.value)
}

/// Binds a prepared child record to the identity of its newly allocated VMA
/// finalizer.  Fork uses this while the child records are still hidden, before
/// its metadata and VMA can become observable.
pub(crate) fn bind_shm_attachment_finalizer_identity_in_namespace(
    namespace: &IpcNamespace,
    pid: Pid,
    record: ShmAttachmentRecord,
    finalizer_identity: usize,
) -> AxResult<ShmAttachmentRecord> {
    if finalizer_identity == 0 {
        return Err(AxError::InvalidInput);
    }
    let _transaction = namespace.shm_transaction().lock();
    let inner = {
        let manager = namespace.shm_manager().lock();
        let matches = manager
            .pid_vaddr_shmid
            .get(&pid)
            .and_then(|attachments| attachments.entries.get(&record.id))
            .is_some_and(|attachment| attachment.value == record);
        if !matches {
            return Err(AxError::InvalidInput);
        }
        manager
            .get_inner_by_shmid(record.shmid)
            .ok_or(AxError::InvalidInput)?
    };
    let segment_matches = inner
        .lock()
        .va_ranges
        .get(&pid)
        .and_then(|ranges| ranges.entries.get(&record.id))
        .is_some_and(|attachment| attachment.value == record);
    if !segment_matches {
        return Err(AxError::BadState);
    }
    let mut bound = record;
    bound.finalizer_identity = finalizer_identity;
    {
        let mut manager = namespace.shm_manager().lock();
        let attachment = manager
            .pid_vaddr_shmid
            .get_mut(&pid)
            .and_then(|attachments| attachments.entries.get_mut(&record.id))
            .ok_or(AxError::BadState)?;
        if attachment.value != record {
            return Err(AxError::BadState);
        }
        attachment.value = bound;
    }
    let mut state = inner.lock();
    let attachment = state
        .va_ranges
        .get_mut(&pid)
        .and_then(|ranges| ranges.entries.get_mut(&record.id))
        .ok_or(AxError::BadState)?;
    if attachment.value != record {
        return Err(AxError::BadState);
    }
    attachment.value = bound;
    Ok(bound)
}

fn finalize_sysv_attachment(
    namespace: &IpcNamespace,
    pid: Pid,
    shmid: i32,
    attachment_id: ShmAttachmentId,
) {
    let _transaction = namespace.shm_transaction().lock();
    let inner = {
        let manager = namespace.shm_manager().lock();
        let Some(inner) = manager.get_inner_by_shmid(shmid) else {
            return;
        };
        inner
    };
    let record = {
        let manager = namespace.shm_manager().lock();
        let Some(record) = manager.attachment_by_id(pid, attachment_id) else {
            return;
        };
        if record.shmid != shmid {
            return;
        }
        record
    };
    let exact_segment = {
        let state = inner.lock();
        state
            .va_ranges
            .get(&pid)
            .filter(|ranges| ranges.is_visible())
            .and_then(|ranges| ranges.entries.get(&record.id))
            .is_some_and(|attachment| attachment.is_visible() && attachment.value == record)
    };
    if !exact_segment {
        error!(
            "SysV SHM finalizer lost exact segment index for PID {} attachment {:?}",
            pid, attachment_id
        );
        return;
    }
    let remove_segment = {
        let mut state = inner.lock();
        // The transaction serializes all SysV metadata writers.  Retain
        // this exact segment record while removing the corresponding
        // manager entry, so there is no observable one-index deletion.
        if !namespace
            .shm_manager()
            .lock()
            .remove_attachment_exact(pid, record)
        {
            return;
        }
        if !state.detach_process_exact(pid, record) {
            error!(
                "SysV SHM finalizer lost segment index while removing PID {} attachment {:?}",
                pid, attachment_id
            );
            return;
        }
        (state.rmid && !state.has_attachment_owners()).then_some(state.page_num)
    };
    if let Some(page_num) = remove_segment
        && let Err(error) = namespace.shm_manager().lock().remove_shmid(shmid, page_num)
    {
        error!("failed to finalize removed SysV SHM segment {shmid}: {error:?}");
    }
}

/// Completes an explicit `shmdt` synchronously after MM has removed the last
/// fragment. The deferred VMA finalizer remains the authority for implicit
/// teardown; if it later runs for this explicit detach, the exact-ID lookup
/// observes that the record is already gone and becomes a no-op.
fn finalize_explicit_shmdt(namespace: &IpcNamespace, pid: Pid, record: ShmAttachmentRecord) {
    finalize_sysv_attachment(namespace, pid, record.shmid, record.id);
}

impl MappingFinalizer for SysvAttachmentFinalizer {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn finalize(self: Box<Self>) {
        finalize_sysv_attachment(&self.namespace, self.pid, self.shmid, self.attachment_id);
    }
}

/// Fully allocated, reader-hidden metadata for one ordinary shmat operation.
/// The address-space mapping is the only remaining fallible step; commit is a
/// single publication store plus an internal cached-count refresh.
#[must_use = "dropping a shmat admission rolls back its hidden metadata"]
struct ShmatAdmission<'a> {
    manager: &'a Mutex<ShmManager>,
    inner: Arc<Mutex<ShmInner>>,
    pid: Pid,
    shmid: i32,
    vaddr: VirtAddr,
    record: ShmAttachmentRecord,
    publication: Arc<ShmForkPublication>,
    finalizer: Option<DeferredMappingFinalizer>,
    committed: bool,
}

impl ShmatAdmission<'_> {
    /// Moves the single preallocated VMA lease into the backend immediately
    /// before publication.  Once moved, a failed map drops that lease through
    /// the deferred path while this admission rolls its hidden record back.
    fn take_finalizer(&mut self) -> DeferredMappingFinalizer {
        self.finalizer
            .take()
            .expect("shmat admission finalizer consumed only once")
    }

    fn commit(mut self) {
        let mut state = self.inner.lock();
        // Manager admission bounds visible plus hidden attachments at 65,536,
        // so adding this exact prepared entry is representable on every target.
        state.shmid_ds.shm_nattch = (state.attach_count() + 1) as c_ulong;
        state.shmid_ds.shm_lpid = self.pid as __kernel_pid_t;
        state.shmid_ds.shm_atime = wall_time().as_secs() as __kernel_time_t;
        drop(state);
        self.publication.visible.store(true, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for ShmatAdmission<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        let exact_manager = {
            let manager = self.manager.lock();
            manager
                .pid_vaddr_shmid
                .get(&self.pid)
                .and_then(|attachments| attachments.entries.get(&self.record.id))
                .is_some_and(|attachment| {
                    !attachment.is_visible()
                        && Arc::ptr_eq(&attachment.publication, &self.publication)
                        && attachment.value == self.record
                })
        };
        let exact_segment = {
            let state = self.inner.lock();
            state
                .va_ranges
                .get(&self.pid)
                .and_then(|ranges| ranges.entries.get(&self.record.id))
                .is_some_and(|attachment| {
                    !attachment.is_visible()
                        && Arc::ptr_eq(&attachment.publication, &self.publication)
                        && attachment.value == self.record
                })
        };
        if !exact_manager || !exact_segment {
            error!(
                "SysV SHM shmat admission for PID {} at {:#x} lost an exact two-index reservation",
                self.pid,
                self.vaddr.as_usize()
            );
            return;
        }

        let removed_manager = {
            let mut manager = self.manager.lock();
            let mut remove_bucket = false;
            let removed = manager
                .pid_vaddr_shmid
                .get_mut(&self.pid)
                .and_then(|attachments| {
                    let exact =
                        attachments
                            .entries
                            .get(&self.record.id)
                            .is_some_and(|attachment| {
                                !attachment.is_visible()
                                    && Arc::ptr_eq(&attachment.publication, &self.publication)
                                    && attachment.value == self.record
                            });
                    let removed = exact
                        .then(|| attachments.entries.remove(&self.record.id))
                        .flatten();
                    remove_bucket = attachments.entries.is_empty();
                    removed
                });
            if remove_bucket {
                manager.pid_vaddr_shmid.remove(&self.pid);
            }
            removed
        };

        let (removed_segment, remove_segment) = {
            let mut state = self.inner.lock();
            let mut remove_bucket = false;
            let removed = state.va_ranges.get_mut(&self.pid).and_then(|ranges| {
                let exact = ranges
                    .entries
                    .get(&self.record.id)
                    .is_some_and(|attachment| {
                        !attachment.is_visible()
                            && Arc::ptr_eq(&attachment.publication, &self.publication)
                            && attachment.value == self.record
                    });
                let removed = exact
                    .then(|| ranges.entries.remove(&self.record.id))
                    .flatten();
                remove_bucket = ranges.entries.is_empty();
                removed
            });
            if remove_bucket {
                state.va_ranges.remove(&self.pid);
            }
            let removed_exact = removed.is_some();
            if !removed_exact {
                error!(
                    "SysV SHM shmat admission lost segment reservation for PID {} at {:#x}",
                    self.pid,
                    self.vaddr.as_usize()
                );
            }
            (
                removed_exact,
                (removed_exact && state.rmid && !state.has_attachment_owners())
                    .then_some((state.shmid, state.page_num)),
            )
        };
        if removed_manager.is_none() || !removed_segment {
            error!(
                "SysV SHM shmat admission for PID {} at {:#x} changed during exact rollback",
                self.pid,
                self.vaddr.as_usize()
            );
            return;
        }
        let refunded = {
            let mut manager = self.manager.lock();
            if let Some(next) = manager.attachment_count.checked_sub(1) {
                manager.attachment_count = next;
                true
            } else {
                error!("SysV SHM shmat rollback attachment underflow");
                false
            }
        };
        if refunded
            && let Some((shmid, page_num)) = remove_segment
            && let Err(error) = self.manager.lock().remove_shmid(shmid, page_num)
        {
            error!("failed to finalize removed SysV SHM segment {shmid}: {error:?}");
        }
    }
}

fn prepare_shmat_admission_with_finalizer_in<'a>(
    manager: &'a Mutex<ShmManager>,
    inner: Arc<Mutex<ShmInner>>,
    pid: Pid,
    shmid: i32,
    va_range: VirtAddrRange,
    record: ShmAttachmentRecord,
    finalizer: Option<DeferredMappingFinalizer>,
    allow_same_detach_base: bool,
) -> AxResult<ShmatAdmission<'a>> {
    let publication =
        Arc::try_new(ShmForkPublication::new(false)).map_err(|_| AxError::NoMemory)?;
    let vaddr = va_range.start;
    if record.base != vaddr || record.range != va_range || record.shmid != shmid {
        return Err(AxError::InvalidInput);
    }
    let next_attachment_count = {
        let mut manager = manager.lock();
        manager.try_reserve_process_attaches(pid, 1)?;
        let Some(attachments) = manager.pid_vaddr_shmid.get(&pid) else {
            manager.cancel_empty_process_reservation(pid);
            return Err(AxError::BadState);
        };
        if !attachments.is_visible()
            || (!allow_same_detach_base
                && attachment_at_base(&attachments.entries, vaddr).is_some())
        {
            manager.cancel_empty_process_reservation(pid);
            return Err(AxError::AlreadyExists);
        }
        let Some(next) = manager
            .attachment_count
            .checked_add(1)
            .filter(|count| *count <= MAX_SHM_ATTACHMENTS)
        else {
            manager.cancel_empty_process_reservation(pid);
            return Err(AxError::NoMemory);
        };
        next
    };

    if let Err(error) = inner.lock().try_reserve_process_attaches(pid, 1) {
        manager.lock().cancel_empty_process_reservation(pid);
        return Err(error);
    }
    {
        let state = inner.lock();
        let Some(ranges) = state.va_ranges.get(&pid) else {
            drop(state);
            inner.lock().cancel_empty_process_reservation(pid);
            manager.lock().cancel_empty_process_reservation(pid);
            return Err(AxError::BadState);
        };
        if !ranges.is_visible()
            || (!allow_same_detach_base && attachment_at_base(&ranges.entries, vaddr).is_some())
        {
            drop(state);
            inner.lock().cancel_empty_process_reservation(pid);
            manager.lock().cancel_empty_process_reservation(pid);
            return Err(AxError::AlreadyExists);
        }
    }

    {
        let mut state = inner.lock();
        let Some(ranges) = state.va_ranges.get_mut(&pid) else {
            drop(state);
            inner.lock().cancel_empty_process_reservation(pid);
            manager.lock().cancel_empty_process_reservation(pid);
            return Err(AxError::BadState);
        };
        let previous = ranges.entries.insert(
            record.id,
            ShmAttachment {
                publication: publication.clone(),
                value: record,
            },
        );
        if let Some(previous) = previous {
            ranges.entries.insert(record.id, previous);
            drop(state);
            inner.lock().cancel_empty_process_reservation(pid);
            manager.lock().cancel_empty_process_reservation(pid);
            return Err(AxError::BadState);
        }
    }

    let previous_manager = {
        let mut manager = manager.lock();
        let Some(attachments) = manager.pid_vaddr_shmid.get_mut(&pid) else {
            drop(manager);
            if let Some(ranges) = inner.lock().va_ranges.get_mut(&pid) {
                ranges.entries.remove(&record.id);
            }
            inner.lock().cancel_empty_process_reservation(pid);
            return Err(AxError::BadState);
        };
        let previous = attachments.entries.insert(
            record.id,
            ShmAttachment {
                publication: publication.clone(),
                value: record,
            },
        );
        if previous.is_none() {
            manager.attachment_count = next_attachment_count;
        }
        previous
    };
    if let Some(previous) = previous_manager {
        if let Some(attachments) = manager.lock().pid_vaddr_shmid.get_mut(&pid) {
            attachments.entries.insert(record.id, previous);
        }
        if let Some(ranges) = inner.lock().va_ranges.get_mut(&pid) {
            ranges.entries.remove(&record.id);
        }
        inner.lock().cancel_empty_process_reservation(pid);
        manager.lock().cancel_empty_process_reservation(pid);
        return Err(AxError::BadState);
    }

    Ok(ShmatAdmission {
        manager,
        inner,
        pid,
        shmid,
        vaddr,
        record,
        publication,
        finalizer,
        committed: false,
    })
}

/// Test and metadata-only admission.  Real `shmat` uses the finalizer-aware
/// variant above after it has preallocated the VMA ownership lease.
fn prepare_shmat_admission_in<'a>(
    manager: &'a Mutex<ShmManager>,
    inner: Arc<Mutex<ShmInner>>,
    pid: Pid,
    shmid: i32,
    va_range: VirtAddrRange,
) -> AxResult<ShmatAdmission<'a>> {
    let record =
        ShmAttachmentRecord::new(allocate_attachment_id()?, va_range.start, va_range, shmid);
    prepare_shmat_admission_with_finalizer_in(
        manager, inner, pid, shmid, va_range, record, None, false,
    )
}

/// Hidden SysV attachment metadata owned by an `mremap` duplicate
/// transaction.  Unlike [`ShmatAdmission`], this owns the namespace rather
/// than borrowing its manager, so it can survive the lock-external backend
/// preparation performed between mremap planning and VMA publication.
#[must_use = "dropping an mremap SysV admission rolls back hidden metadata"]
pub(crate) struct SysvMremapDuplicateAdmission {
    namespace: Arc<IpcNamespace>,
    inner: Arc<Mutex<ShmInner>>,
    pid: Pid,
    record: ShmAttachmentRecord,
    source_finalizer_identity: usize,
    publication: Arc<ShmForkPublication>,
    finalizer: DeferredMappingFinalizer,
    committed: bool,
}

impl SysvMremapDuplicateAdmission {
    /// Every destination fragment belonging to this one logical attachment
    /// must receive a clone of this same lease.
    pub(crate) fn finalizer(&self) -> DeferredMappingFinalizer {
        self.finalizer.clone()
    }

    pub(crate) fn source_finalizer_identity(&self) -> usize {
        self.source_finalizer_identity
    }

    pub(crate) fn commit(&mut self) {
        if self.committed {
            return;
        }
        let _transaction = self.namespace.shm_transaction().lock();
        let mut state = self.inner.lock();
        state.shmid_ds.shm_nattch = (state.attach_count() + 1) as c_ulong;
        state.shmid_ds.shm_lpid = self.pid as __kernel_pid_t;
        state.shmid_ds.shm_atime = wall_time().as_secs() as __kernel_time_t;
        drop(state);
        self.publication.visible.store(true, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for SysvMremapDuplicateAdmission {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _transaction = self.namespace.shm_transaction().lock();
        let removed_manager = {
            let mut manager = self.namespace.shm_manager().lock();
            let Some(attachments) = manager.pid_vaddr_shmid.get_mut(&self.pid) else {
                return;
            };
            let exact = attachments
                .entries
                .get(&self.record.id)
                .is_some_and(|attachment| {
                    !attachment.is_visible()
                        && Arc::ptr_eq(&attachment.publication, &self.publication)
                        && attachment.value == self.record
                });
            let removed = exact
                .then(|| attachments.entries.remove(&self.record.id))
                .flatten();
            let empty = attachments.entries.is_empty();
            if empty {
                manager.pid_vaddr_shmid.remove(&self.pid);
            }
            if removed.is_some() {
                manager.attachment_count =
                    manager.attachment_count.checked_sub(1).unwrap_or_else(|| {
                        error!("SysV SHM mremap admission attachment underflow");
                        0
                    });
            }
            removed.is_some()
        };
        let (removed_segment, remove_segment) = {
            let mut state = self.inner.lock();
            let removed = state.va_ranges.get_mut(&self.pid).and_then(|ranges| {
                let exact = ranges
                    .entries
                    .get(&self.record.id)
                    .is_some_and(|attachment| {
                        !attachment.is_visible()
                            && Arc::ptr_eq(&attachment.publication, &self.publication)
                            && attachment.value == self.record
                    });
                exact
                    .then(|| ranges.entries.remove(&self.record.id))
                    .flatten()
            });
            if state
                .va_ranges
                .get(&self.pid)
                .is_some_and(|ranges| ranges.entries.is_empty())
            {
                state.va_ranges.remove(&self.pid);
            }
            let removed_segment = removed.is_some();
            let remove_segment = (removed_segment && state.rmid && !state.has_attachment_owners())
                .then_some((state.shmid, state.page_num));
            (removed_segment, remove_segment)
        };
        if !removed_manager || !removed_segment {
            error!(
                "SysV SHM mremap admission lost exact hidden reservation for PID {} attachment \
                 {:?}",
                self.pid, self.record.id
            );
            return;
        }
        if let Some((shmid, page_num)) = remove_segment
            && let Err(error) = self
                .namespace
                .shm_manager()
                .lock()
                .remove_shmid(shmid, page_num)
        {
            error!("failed to finalize removed SysV SHM segment {shmid}: {error:?}");
        }
    }
}

/// Prepares one new logical SysV attachment for `mremap` duplication.  The
/// caller supplies a destination detach base computed from the source VMA's
/// shared-object offset; publication is deliberately deferred until all VMA
/// fragments have committed.
pub(crate) fn prepare_sysv_mremap_duplicate_admission(
    namespace: Arc<IpcNamespace>,
    pid: Pid,
    source: ShmAttachmentRecord,
    source_finalizer_identity: usize,
    detach_base: VirtAddr,
) -> AxResult<SysvMremapDuplicateAdmission> {
    let range = VirtAddrRange::try_from_start_size(detach_base, source.segment_size())
        .ok_or(AxError::InvalidInput)?;
    let mut record =
        ShmAttachmentRecord::new(allocate_attachment_id()?, detach_base, range, source.shmid);
    let finalizer = try_new_sysv_attachment_finalizer(namespace.clone(), pid, record)?;
    record.finalizer_identity = finalizer.identity();
    let publication =
        Arc::try_new(ShmForkPublication::new(false)).map_err(|_| AxError::NoMemory)?;
    let _transaction = namespace.shm_transaction().lock();
    let inner = namespace
        .shm_manager()
        .lock()
        .get_inner_by_shmid(source.shmid)
        .ok_or(AxError::InvalidInput)?;
    {
        let mut manager = namespace.shm_manager().lock();
        manager.try_reserve_process_attaches(pid, 1)?;
        if manager
            .pid_vaddr_shmid
            .get(&pid)
            .is_none_or(|entries| entries.entries.contains_key(&record.id))
        {
            manager.cancel_empty_process_reservation(pid);
            return Err(AxError::BadState);
        }
    }
    if let Err(error) = inner.lock().try_reserve_process_attaches(pid, 1) {
        namespace
            .shm_manager()
            .lock()
            .cancel_empty_process_reservation(pid);
        return Err(error);
    }
    {
        let mut state = inner.lock();
        let Some(entries) = state.va_ranges.get_mut(&pid) else {
            return Err(AxError::BadState);
        };
        if entries.entries.contains_key(&record.id) {
            return Err(AxError::BadState);
        }
        entries.entries.insert(
            record.id,
            ShmAttachment {
                publication: publication.clone(),
                value: record,
            },
        );
    }
    {
        let mut manager = namespace.shm_manager().lock();
        let Some(entries) = manager.pid_vaddr_shmid.get_mut(&pid) else {
            inner
                .lock()
                .va_ranges
                .get_mut(&pid)
                .map(|entries| entries.entries.remove(&record.id));
            return Err(AxError::BadState);
        };
        entries.entries.insert(
            record.id,
            ShmAttachment {
                publication: publication.clone(),
                value: record,
            },
        );
        manager.attachment_count = manager
            .attachment_count
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
    }
    Ok(SysvMremapDuplicateAdmission {
        namespace: namespace.clone(),
        inner,
        pid,
        record,
        source_finalizer_identity,
        publication,
        finalizer,
        committed: false,
    })
}

struct InheritedGroup {
    inner: Arc<Mutex<ShmInner>>,
    ranges: HashMap<ShmAttachmentId, ShmAttachment<ShmAttachmentRecord>>,
}

struct InheritedReservation {
    inner: Arc<Mutex<ShmInner>>,
    count: usize,
}

/// A child mapping must replace its cloned parent VMA lease with this newly
/// allocated child identity before it can publish the fork admission.  The MM
/// side consumes these records; this module deliberately does not pretend a
/// cloned parent finalizer is valid for the child PID.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShmForkAttachmentRebind {
    pub(crate) parent: ShmAttachmentRecord,
    pub(crate) child: ShmAttachmentRecord,
}

/// Invisible, fully allocated SysV SHM inheritance for one fork child.
///
/// Prepare installs manager and per-segment buckets sharing one clear
/// publication bit. All regular readers and cleanup paths ignore those buckets,
/// while IPC_RMID retains the segment as an internal in-flight owner. Commit is
/// allocation-free and makes every bucket visible through one release store;
/// Drop removes the exact hidden buckets and releases their accounting charge.
#[must_use = "dropping a SysV SHM fork admission rolls back hidden inheritance"]
pub(crate) struct ProcShmForkAdmission<'a> {
    transaction: &'a Mutex<()>,
    manager: &'a Mutex<ShmManager>,
    child_pid: Pid,
    publication: Option<Arc<ShmForkPublication>>,
    groups: Vec<InheritedReservation>,
    finalizer_rebinds: Vec<ShmForkAttachmentRebind>,
    committed: bool,
}

impl<'a> ProcShmForkAdmission<'a> {
    fn empty(transaction: &'a Mutex<()>, manager: &'a Mutex<ShmManager>, child_pid: Pid) -> Self {
        Self {
            transaction,
            manager,
            child_pid,
            publication: None,
            groups: Vec::new(),
            finalizer_rebinds: Vec::new(),
            committed: false,
        }
    }

    /// Publishes every prepared manager/segment bucket in one release step.
    /// ABI readers derive nattch from published buckets; the subsequent cache
    /// refreshes are internal, allocation-free, and serialized from syscalls.
    pub(crate) fn commit(mut self) {
        let _transaction = self.transaction.lock();
        if let Some(publication) = self.publication.as_ref() {
            for group in &self.groups {
                let mut state = group.inner.lock();
                // The manager-wide admission charge proves this exact sum is
                // bounded and representable before the final publication.
                state.shmid_ds.shm_nattch = (state.attach_count() + group.count) as c_ulong;
            }
            publication.visible.store(true, Ordering::Release);
        }
        self.committed = true;
    }

    /// Provenance for rebinding cloned VMA leases to child-specific logical
    /// attachments.  The caller must complete this before `commit`.
    pub(crate) fn finalizer_rebinds(&self) -> &[ShmForkAttachmentRebind] {
        &self.finalizer_rebinds
    }
}

impl Drop for ProcShmForkAdmission<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let Some(publication) = self.publication.as_ref() else {
            return;
        };

        let _transaction = self.transaction.lock();
        let exact_manager = {
            let manager = self.manager.lock();
            manager
                .pid_vaddr_shmid
                .get(&self.child_pid)
                .is_some_and(|attachments| {
                    !attachments.is_visible()
                        && Arc::ptr_eq(&attachments.publication, publication)
                        && attachments.entries.values().all(|attachment| {
                            !attachment.is_visible()
                                && Arc::ptr_eq(&attachment.publication, publication)
                        })
                })
        };
        let exact_segments = self.groups.iter().all(|group| {
            let state = group.inner.lock();
            state.va_ranges.get(&self.child_pid).is_some_and(|ranges| {
                !ranges.is_visible()
                    && Arc::ptr_eq(&ranges.publication, publication)
                    && ranges.entries.values().all(|attachment| {
                        !attachment.is_visible()
                            && Arc::ptr_eq(&attachment.publication, publication)
                    })
            })
        });
        if !exact_manager || !exact_segments {
            error!(
                "SysV SHM fork admission for PID {} lost an exact manager/segment reservation",
                self.child_pid
            );
            return;
        }

        let removed_manager = {
            let mut manager = self.manager.lock();
            let exact = manager
                .pid_vaddr_shmid
                .get(&self.child_pid)
                .is_some_and(|attachments| {
                    !attachments.is_visible()
                        && Arc::ptr_eq(&attachments.publication, publication)
                        && attachments.entries.values().all(|attachment| {
                            !attachment.is_visible()
                                && Arc::ptr_eq(&attachment.publication, publication)
                        })
                });
            exact
                .then(|| manager.pid_vaddr_shmid.remove(&self.child_pid))
                .flatten()
        };

        if removed_manager.is_none() {
            error!(
                "SysV SHM fork admission for PID {} lost its manager reservation",
                self.child_pid
            );
            return;
        }
        let mut all_segments_removed = true;
        for group in &self.groups {
            let (removed_segment, remove_segment) = {
                let mut state = group.inner.lock();
                let exact = state.va_ranges.get(&self.child_pid).is_some_and(|ranges| {
                    !ranges.is_visible()
                        && Arc::ptr_eq(&ranges.publication, publication)
                        && ranges.entries.values().all(|attachment| {
                            !attachment.is_visible()
                                && Arc::ptr_eq(&attachment.publication, publication)
                        })
                });
                let removed = exact
                    .then(|| state.va_ranges.remove(&self.child_pid))
                    .flatten();
                if removed.is_none() {
                    error!(
                        "SysV SHM fork admission for PID {} lost segment {} reservation",
                        self.child_pid, state.shmid
                    );
                }
                let removed_exact = removed.is_some();
                (
                    removed_exact,
                    (removed_exact && state.rmid && !state.has_attachment_owners())
                        .then_some((state.shmid, state.page_num)),
                )
            };
            all_segments_removed &= removed_segment;
            if let Some((shmid, page_num)) = remove_segment
                && let Err(error) = self.manager.lock().remove_shmid(shmid, page_num)
            {
                error!("failed to finalize removed SysV SHM segment {shmid}: {error:?}");
            }
        }
        if all_segments_removed && let Some(attachments) = removed_manager {
            let mut manager = self.manager.lock();
            if let Some(next) = manager
                .attachment_count
                .checked_sub(attachments.entries.len())
            {
                manager.attachment_count = next;
            } else {
                error!(
                    "SysV SHM fork rollback underflow for PID {}",
                    self.child_pid
                );
            }
        }
    }
}

fn prepare_proc_shm_inheritance_with_live_in<'a>(
    transaction: &'a Mutex<()>,
    manager: &'a Mutex<ShmManager>,
    parent_pid: Pid,
    child_pid: Pid,
    live_finalizer_identities: &[usize],
) -> AxResult<ProcShmForkAdmission<'a>> {
    let _transaction = transaction.lock();
    let required = {
        let manager = manager.lock();
        if manager.pid_vaddr_shmid.contains_key(&child_pid) {
            return Err(AxError::AlreadyExists);
        }
        manager
            .pid_vaddr_shmid
            .get(&parent_pid)
            .filter(|attachments| attachments.is_visible())
            .map_or(0, |attachments| {
                attachments
                    .entries
                    .values()
                    .filter(|attachment| {
                        attachment.is_visible()
                            && live_finalizer_identities
                                .contains(&attachment.value.finalizer_identity)
                    })
                    .count()
            })
    };
    if required == 0 {
        return Ok(ProcShmForkAdmission::empty(transaction, manager, child_pid));
    }

    let publication =
        Arc::try_new(ShmForkPublication::new(false)).map_err(|_| AxError::NoMemory)?;

    let mut source = Vec::new();
    source
        .try_reserve_exact(required)
        .map_err(|_| AxError::NoMemory)?;
    {
        let manager = manager.lock();
        let attachments = manager
            .pid_vaddr_shmid
            .get(&parent_pid)
            .filter(|attachments| attachments.is_visible())
            .ok_or(AxError::Io)?;
        source.extend(
            attachments
                .entries
                .values()
                .filter(|attachment| {
                    attachment.is_visible()
                        && live_finalizer_identities.contains(&attachment.value.finalizer_identity)
                })
                .map(|attachment| attachment.value),
        );
    }
    source.sort_unstable_by_key(|record| (record.shmid, record.base.as_usize()));

    let mut groups = Vec::new();
    groups
        .try_reserve_exact(source.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut inherited = HashMap::new();
    inherited
        .try_reserve(source.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut finalizer_rebinds = Vec::new();
    finalizer_rebinds
        .try_reserve_exact(source.len())
        .map_err(|_| AxError::NoMemory)?;
    for &parent in &source {
        let child = ShmAttachmentRecord::new(
            allocate_attachment_id()?,
            parent.base,
            parent.range,
            parent.shmid,
        );
        inherited.insert(
            child.id,
            ShmAttachment {
                publication: publication.clone(),
                value: child,
            },
        );
        finalizer_rebinds.push(ShmForkAttachmentRebind { parent, child });
    }
    let mut index = 0;
    while index < source.len() {
        let shmid = source[index].shmid;
        let end = source[index..]
            .iter()
            .position(|candidate| candidate.shmid != shmid)
            .map_or(source.len(), |offset| index + offset);
        let inner = manager
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
            for (offset, &parent) in source[index..end].iter().enumerate() {
                // `finalizer_rebinds` is constructed in the same sorted
                // source order, keeping fork preparation linear even at the
                // namespace attachment limit.
                let child = finalizer_rebinds[index + offset].child;
                let inherited_parent = state
                    .va_ranges
                    .get(&parent_pid)
                    .and_then(|ranges| ranges.entries.get(&parent.id))
                    .filter(|attachment| attachment.is_visible())
                    .map(|attachment| attachment.value)
                    .ok_or(AxError::Io)?;
                if inherited_parent != parent {
                    return Err(AxError::Io);
                }
                ranges.insert(
                    child.id,
                    ShmAttachment {
                        publication: publication.clone(),
                        value: child,
                    },
                );
            }
        }
        groups.push(InheritedGroup { inner, ranges });
        index = end;
    }

    let next_attachment_count = {
        let mut manager = manager.lock();
        let next = manager
            .attachment_count
            .checked_add(inherited.len())
            .filter(|total| *total <= MAX_SHM_ATTACHMENTS)
            .ok_or(AxError::NoMemory)?;
        if manager.pid_vaddr_shmid.contains_key(&child_pid) {
            return Err(AxError::AlreadyExists);
        }
        manager
            .pid_vaddr_shmid
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        next
    };

    let mut reservations: Vec<InheritedReservation> = Vec::new();
    reservations
        .try_reserve_exact(groups.len())
        .map_err(|_| AxError::NoMemory)?;

    for group in groups {
        let count = group.ranges.len();
        let mut state = group.inner.lock();
        let previous = state.va_ranges.insert(
            child_pid,
            ProcessAttachmentMap {
                publication: publication.clone(),
                entries: group.ranges,
            },
        );
        if let Some(previous) = previous {
            state.va_ranges.insert(child_pid, previous);
            drop(state);
            for reservation in &reservations {
                reservation.inner.lock().va_ranges.remove(&child_pid);
            }
            return Err(AxError::BadState);
        }
        drop(state);
        reservations.push(InheritedReservation {
            inner: group.inner,
            count,
        });
    }

    let previous_manager = {
        let mut manager = manager.lock();
        let previous = manager.pid_vaddr_shmid.insert(
            child_pid,
            ProcessAttachmentMap {
                publication: publication.clone(),
                entries: inherited,
            },
        );
        if previous.is_none() {
            manager.attachment_count = next_attachment_count;
        }
        previous
    };
    if let Some(previous) = previous_manager {
        manager.lock().pid_vaddr_shmid.insert(child_pid, previous);
        for reservation in &reservations {
            reservation.inner.lock().va_ranges.remove(&child_pid);
        }
        return Err(AxError::BadState);
    }

    Ok(ProcShmForkAdmission {
        transaction,
        manager,
        child_pid,
        publication: Some(publication),
        groups: reservations,
        finalizer_rebinds,
        committed: false,
    })
}

/// Prepares SHM inheritance inside one IPC namespace.  The returned admission
/// borrows the namespace transaction and manager, forcing clone publication to
/// retain the namespace until it either commits or rolls back.
pub(crate) fn prepare_proc_shm_in_namespace<'a>(
    namespace: &'a IpcNamespace,
    parent_pid: Pid,
    child_pid: Pid,
    live_finalizer_identities: &[usize],
) -> AxResult<ProcShmForkAdmission<'a>> {
    prepare_proc_shm_inheritance_with_live_in(
        namespace.shm_transaction(),
        namespace.shm_manager(),
        parent_pid,
        child_pid,
        live_finalizer_identities,
    )
}

#[cfg(test)]
fn prepare_proc_shm_inheritance_in<'a>(
    transaction: &'a Mutex<()>,
    manager: &'a Mutex<ShmManager>,
    parent_pid: Pid,
    child_pid: Pid,
) -> AxResult<ProcShmForkAdmission<'a>> {
    let identities = manager
        .lock()
        .pid_vaddr_shmid
        .get(&parent_pid)
        .map(|entries| {
            entries
                .entries
                .values()
                .filter_map(|entry| entry.is_visible().then_some(entry.value.finalizer_identity))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    prepare_proc_shm_inheritance_with_live_in(
        transaction,
        manager,
        parent_pid,
        child_pid,
        &identities,
    )
}

fn clear_proc_shm_in(transaction: &Mutex<()>, manager: &Mutex<ShmManager>, pid: Pid) {
    let _transaction = transaction.lock();
    loop {
        let detached = {
            let mut manager = manager.lock();
            let next = manager
                .pid_vaddr_shmid
                .get(&pid)
                .filter(|attachments| attachments.is_visible())
                .and_then(|attachments| {
                    attachments
                        .entries
                        .iter()
                        .find(|(_, attachment)| attachment.is_visible())
                })
                .map(|(_, attachment)| attachment.value);
            let Some(record) = next else {
                manager.remove_pid(pid);
                break;
            };
            manager.remove_attachment_exact(pid, record);
            (record, manager.get_inner_by_shmid(record.shmid))
        };
        let (record, Some(inner)) = detached else {
            continue;
        };
        let remove = {
            let mut state = inner.lock();
            state.detach_process_exact(pid, record);
            (state.rmid && !state.has_attachment_owners()).then_some(state.page_num)
        };
        if let Some(page_num) = remove
            && let Err(error) = manager.lock().remove_shmid(record.shmid, page_num)
        {
            error!(
                "failed to finalize removed SysV SHM segment {}: {error:?}",
                record.shmid
            );
        }
    }
}

/// Clears every SHM attachment owned by `pid` in exactly `namespace`.
pub(crate) fn clear_proc_shm_in_namespace(namespace: &IpcNamespace, pid: Pid) {
    clear_proc_shm_in(namespace.shm_transaction(), namespace.shm_manager(), pid)
}

fn allocate_shm_id(shm_manager: &ShmManager, cursor: &AtomicI32) -> AxResult<i32> {
    let desired = cursor.swap(-1, Ordering::Relaxed);
    allocate_ipc_id(
        cursor,
        (desired >= 0).then_some(desired),
        shm_manager.shmid_inner.len(),
        |id| shm_manager.contains_shmid(id),
    )
}

pub(crate) fn shm_next_id() -> i32 {
    let curr = current();
    curr.as_thread()
        .ipc_ns()
        .next_shm_id()
        .load(Ordering::Relaxed)
}

pub(crate) fn set_shm_next_id(value: i32) -> AxResult<()> {
    if value < -1 {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    let curr = current();
    curr.as_thread()
        .ipc_ns()
        .next_shm_id()
        .store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn sysvipc_shm_snapshot() -> AxResult<String> {
    let curr = current();
    let ipc_ns = curr.as_thread().ipc_ns();
    let _transaction = ipc_ns.shm_transaction().lock();
    const HEADER: &str = "       key      shmid perms                  size  cpid  lpid nattch   \
                          uid   gid  cuid  cgid      atime      dtime      ctime        rss       \
                          swap\n";
    const MAX_ROW_LEN: usize = 256;
    let segment_count = ipc_ns.shm_manager().lock().shmid_inner.len();
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(segment_count)
        .map_err(|_| AxError::NoMemory)?;
    {
        let manager = ipc_ns.shm_manager().lock();
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
        let ds = shm_inner.visible_snapshot();
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
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let cur_pid = proc_data.proc.pid();
    let ipc_ns = thread.ipc_ns();
    let context = IpcAccessContext::for_ipc_namespace(thread.current_cred(), &ipc_ns);
    let euid = context.effective_uid_raw();
    let egid = context.effective_gid_raw();

    if huge {
        return Err(AxError::from(LinuxError::EINVAL));
    }
    let _transaction = ipc_ns.shm_transaction().lock();

    if key != IPC_PRIVATE {
        let existing = {
            let manager = ipc_ns.shm_manager().lock();
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
            let shm_inner = shm_inner.lock();
            return shm_inner.try_update(&context, size, perm_mode);
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
    let mut manager = ipc_ns.shm_manager().lock();
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
    let shmid = allocate_shm_id(&manager, ipc_ns.next_shm_id())?;

    let mut inner = ShmInner::new(
        key,
        shmid,
        size,
        mapping_flags,
        perm_mode,
        cur_pid,
        euid,
        egid,
    );
    inner.shmid_ds.shm_perm.seq = ipc_ns.next_sequence();
    let shm_inner = Arc::try_new(Mutex::new(inner)).map_err(|_| AxError::NoMemory)?;
    manager.insert_shmid_inner(shmid, page_num, shm_inner)?;
    if key != IPC_PRIVATE {
        manager.insert_key_shmid(key, shmid);
    }

    Ok(shmid as isize)
}

pub fn sys_shmat(shmid: i32, addr: usize, shmflg: u32) -> AxResult<isize> {
    if shmid < 0 {
        return Err(AxError::InvalidInput);
    }
    let shm_flg = ShmAtFlags::from_bits_truncate(shmflg);
    let explicit_addr = if addr == 0 {
        if shm_flg.contains(ShmAtFlags::SHM_REMAP) {
            return Err(AxError::InvalidInput);
        }
        None
    } else {
        let candidate = if addr.is_multiple_of(SHMLBA) {
            addr
        } else if shm_flg.contains(ShmAtFlags::SHM_RND) {
            align_down_to(addr, SHMLBA)
        } else {
            return Err(AxError::InvalidInput);
        };
        if candidate == 0 && shm_flg.contains(ShmAtFlags::SHM_REMAP) {
            return Err(AxError::InvalidInput);
        }
        Some(VirtAddr::from(candidate))
    };

    let curr = current();
    let ipc_ns = curr.as_thread().ipc_ns();
    let _transaction = ipc_ns.shm_transaction().lock();
    let shm_inner = {
        let shm_manager = ipc_ns.shm_manager().lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(AxError::InvalidInput)?
    };
    let read_only = shm_flg.contains(ShmAtFlags::SHM_RDONLY);
    let executable = shm_flg.contains(ShmAtFlags::SHM_EXEC);

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let pid = proc_data.proc.pid();
    let context = IpcAccessContext::for_ipc_namespace(thread.current_cred(), &ipc_ns);
    let (mut mapping_flags, page_num, existing_pages) = {
        let state = shm_inner.lock();
        if state.rmid {
            return Err(AxError::from(LinuxError::EIDRM));
        }
        if !can_attach_shm(&context, &state.shmid_ds.shm_perm, read_only, executable) {
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
    if executable {
        mapping_flags.insert(MappingFlags::EXECUTE);
    } else {
        mapping_flags.remove(MappingFlags::EXECUTE);
    }
    // No IPC transaction may span an address-space lock.  This first phase
    // snapshots immutable segment state only; every mutable fact is checked
    // again after lock-external alias preparation.
    drop(_transaction);
    let aspace_handle = proc_data.aspace();
    let aspace = aspace_handle.lock();
    let length = page_num
        .checked_mul(PAGE_SIZE_4K)
        .ok_or(AxError::NoMemory)?;
    let limit = VirtAddrRange::new(aspace.base(), aspace.end());
    let replace_existing = shm_flg.contains(ShmAtFlags::SHM_REMAP);

    let start_addr = if let Some(candidate) = explicit_addr {
        if !aspace.contains_range(candidate, length) {
            return Err(AxError::InvalidInput);
        }
        if !replace_existing
            && aspace.find_free_area(candidate, length, limit, PAGE_SIZE_4K) != Some(candidate)
        {
            return Err(AxError::InvalidInput);
        }
        candidate
    } else {
        let search_length = align_up_to(length, SHMLBA).ok_or(AxError::NoMemory)?;
        aspace
            .find_kernel_area(aspace.base(), search_length, limit, SHMLBA)
            .ok_or(AxError::NoMemory)?
    };
    let end_addr = VirtAddr::from(
        start_addr
            .as_usize()
            .checked_add(length)
            .ok_or(AxError::NoMemory)?,
    );
    let va_range = VirtAddrRange::new(start_addr, end_addr);
    let address_space_growth = if replace_existing {
        length
            .checked_sub(aspace.mapped_bytes_in_range(start_addr, length)?)
            .ok_or(AxError::BadState)?
    } else {
        length
    };
    check_rlimit_as_growth(proc_data, &aspace, address_space_growth)?;

    let provisional_pages = if let Some(pages) = existing_pages {
        pages
    } else {
        let pages = Arc::try_new(SharedPages::new_sysv_charged(length, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        pages
    };

    drop(aspace);
    // Rebuild the IPC edge and select/reserve canonical first-attach pages.
    let _transaction = ipc_ns.shm_transaction().lock();
    {
        let state = shm_inner.lock();
        if state.rmid {
            return Err(AxError::from(LinuxError::EIDRM));
        }
        if !can_attach_shm(&context, &state.shmid_ds.shm_perm, read_only, executable) {
            return Err(AxError::from(LinuxError::EACCES));
        }
        if state.page_num != page_num {
            // A concurrent first attach may have selected the canonical
            // SharedPages object while this syscall was waiting outside the
            // transaction.  Drop the pending generation and restart from a
            // fresh IPC/mm snapshot rather than reporting an internal fault.
            drop(state);
            drop(_transaction);
            return sys_shmat(shmid, addr, shmflg);
        }
    }
    // SHM attachments are process-wide while IPC selection is task-local.
    // Retain this manager only after every lock-external revalidation and
    // address/rlimit check has succeeded, but before the VMA can become
    // visible, so final teardown is independent of a later setns/unshare.
    proc_data.register_touched_ipc_namespace(ipc_ns.clone())?;
    let (pages, first_pages_reservation) =
        reserve_first_shmat_pages(shm_inner.clone(), provisional_pages)?;
    let backend = Backend::try_new_shared(start_addr, pages.clone())?;
    let mut record =
        ShmAttachmentRecord::new(allocate_attachment_id()?, start_addr, va_range, shmid);
    // Preallocate the final-release lease before installing hidden IPC
    // metadata.  The mapping path below has no allocation edge after this.
    let finalizer = try_new_sysv_attachment_finalizer(ipc_ns.clone(), pid, record)?;
    record.finalizer_identity = finalizer.identity();
    let mut admission = prepare_shmat_admission_with_finalizer_in(
        ipc_ns.shm_manager(),
        shm_inner.clone(),
        pid,
        shmid,
        va_range,
        record,
        Some(finalizer),
        replace_existing,
    )?;
    let backend = backend.with_mapping_finalizer(admission.take_finalizer());
    // Canonical page identity is segment state, not publication of this
    // particular attachment.  Commit it while the existing IPC transaction
    // is still held, before taking MM; if the later mapping fails the segment
    // simply retains its already-charged backing for the next attach.  This
    // also prevents reservation Drop from ever trying to reacquire an IPC
    // transaction held by its own syscall.
    if let Some(reservation) = first_pages_reservation {
        reservation.commit();
    }
    // The hidden admission is now the only IPC state owned by this syscall;
    // release IPC before taking MM.  A stale address snapshot simply drops it
    // and rolls both indexes back.
    drop(_transaction);
    // Alias preparation uses the canonical reservation, never a provisional
    // page object that another concurrent first attach could supersede.
    let key = backend
        .shared_backing_key()
        .expect("SysV attachments use SharedPages");
    let pending_alias = crate::mm::prepare_shared_alias_binding_lock_external(key, &aspace_handle)?;
    let _uprobe_topology = replace_existing.then(crate::uprobe::registration_topology_gate);
    let mut aspace = aspace_handle.lock();
    if !aspace.contains_range(start_addr, length) {
        return Err(AxError::InvalidInput);
    }
    if !replace_existing
        && aspace.find_free_area(start_addr, length, limit, PAGE_SIZE_4K) != Some(start_addr)
    {
        return Err(AxError::InvalidInput);
    }
    let address_space_growth = if replace_existing {
        length
            .checked_sub(aspace.mapped_bytes_in_range(start_addr, length)?)
            .ok_or(AxError::BadState)?
    } else {
        length
    };
    check_rlimit_as_growth(proc_data, &aspace, address_space_growth)?;
    let mut deferred_uffd_wake = crate::mm::DeferredUffdWake::empty();
    let mut displaced_finalizers = if replace_existing {
        aspace.mapping_finalizers_in_range(start_addr, length)?
    } else {
        Vec::new()
    };
    if replace_existing {
        let mut transition = crate::uprobe::PreparedFixedUprobeTransition::prepare_or_defer_locked(
            &aspace_handle,
            &aspace,
            start_addr,
            length,
            &backend,
            mapping_flags,
        );
        match aspace.replace_mapping_fixed_with(
            start_addr,
            length,
            mapping_flags,
            backend,
            false,
            &mut transition,
        ) {
            Ok(wake) => deferred_uffd_wake.merge(wake),
            Err(error) => {
                // The primitive restored the exact outgoing topology. Prune
                // any retained reverse aliases before the incoming pending
                // alias lease drops.
                aspace.finish_shared_alias_binding_transition();
                return Err(error.into_error());
            }
        }
    } else {
        aspace.map(start_addr, length, mapping_flags, false, backend)?;
    }
    aspace.commit_shared_alias_binding(pending_alias);
    if replace_existing {
        // The old mapping may have been shared even when the incoming SysV
        // mapping is not; finish after incoming alias publication either way.
        aspace.finish_shared_alias_binding_transition();
        displaced_finalizers
            .retain(|finalizer| !aspace.has_mapping_finalizer_identity(finalizer.identity()));
    }
    admission.commit();
    drop(aspace);
    for finalizer in displaced_finalizers {
        if let Some(sysv) = finalizer.downcast_ref::<SysvAttachmentFinalizer>() {
            finalize_sysv_attachment(&sysv.namespace, sysv.pid, sysv.shmid, sysv.attachment_id);
        }
    }
    deferred_uffd_wake.finish();
    Ok(start_addr.as_usize() as isize)
}

pub fn sys_shmctl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    shmid: i32,
    cmd: u32,
    buf: usize,
) -> AxResult<isize> {
    let curr = current();
    let ipc_ns = curr.as_thread().ipc_ns();
    let context = IpcAccessContext::for_ipc_namespace(curr.as_thread().current_cred(), &ipc_ns);
    let cmd = cmd as i32;

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
        let index = {
            let _transaction = ipc_ns.shm_transaction().lock();
            ipc_ns.shm_manager().lock().max_active_index()
        };
        write_ipc_info(memory, buf as *mut IpcInfo, &info)?;
        return Ok(index);
    }
    if cmd == SHM_INFO {
        let (info, index) = {
            let _transaction = ipc_ns.shm_transaction().lock();
            let manager = ipc_ns.shm_manager().lock();
            let pages = manager.total_page_count() as c_ulong;
            (
                ShmUsageInfo {
                    used_ids: manager.active_segment_count() as i32,
                    shm_tot: pages,
                    shm_rss: pages,
                    shm_swp: 0,
                    swap_attempts: 0,
                    swap_successes: 0,
                },
                manager.max_active_index(),
            )
        };
        write_shm_usage_info(memory, buf as *mut ShmUsageInfo, &info)?;
        return Ok(index);
    }

    if cmd == SHM_STAT || cmd == SHM_STAT_ANY {
        let snapshot = {
            let _transaction = ipc_ns.shm_transaction().lock();
            let shm_inner = ipc_ns
                .shm_manager()
                .lock()
                .get_inner_by_shmid(shmid)
                .ok_or(AxError::InvalidInput)?;
            let state = shm_inner.lock();
            if cmd == SHM_STAT && !context.allows(&state.shmid_ds.shm_perm, IpcAccess::Read) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            state.visible_snapshot()
        };
        write_shmid_ds(memory, buf as *mut ShmidDs, &snapshot)?;
        return Ok(shmid as isize);
    }

    // Preserve the old lookup-before-copyin errno ordering, but do not keep
    // the transaction lock while usercopy takes the address-space lock.
    let shm_inner = {
        let _transaction = ipc_ns.shm_transaction().lock();
        ipc_ns
            .shm_manager()
            .lock()
            .get_inner_by_shmid(shmid)
            .ok_or(AxError::InvalidInput)?
    };
    let set_request: Option<IpcPermissionUpdateRequest> = if cmd == IPC_SET {
        let user_ds = read_shmid_ds(memory, buf as *const ShmidDs)?;
        Some(context.map_permission_update(
            user_ds.shm_perm.uid,
            user_ds.shm_perm.gid,
            user_ds.shm_perm.mode,
        )?)
    } else {
        None
    };

    let _transaction = ipc_ns.shm_transaction().lock();
    let current_inner = ipc_ns.shm_manager().lock().get_inner_by_shmid(shmid);
    if current_inner
        .as_ref()
        .is_none_or(|current| !Arc::ptr_eq(current, &shm_inner))
    {
        return Err(AxError::InvalidInput);
    }

    if cmd == IPC_SET {
        let mut state = shm_inner.lock();
        let prepared = context.prepare_permission_update(
            &state.shmid_ds.shm_perm,
            set_request.expect("IPC_SET request was prepared before locking"),
        )?;
        prepared.commit(&mut state.shmid_ds.shm_perm);
        state.shmid_ds.shm_ctime = wall_time().as_secs() as __kernel_time_t;
    } else if cmd == IPC_STAT {
        let snapshot = {
            let state = shm_inner.lock();
            if !context.allows(&state.shmid_ds.shm_perm, IpcAccess::Read) {
                return Err(AxError::from(LinuxError::EACCES));
            }
            state.visible_snapshot()
        };
        drop(_transaction);
        write_shmid_ds(memory, buf as *mut ShmidDs, &snapshot)?;
        return Ok(0);
    } else if cmd == IPC_RMID {
        let remove = {
            let mut state = shm_inner.lock();
            if !context.may_control(&state.shmid_ds.shm_perm) {
                return Err(AxError::from(LinuxError::EPERM));
            }
            state.set_removed(true);
            (!state.has_attachment_owners()).then_some(state.page_num)
        };
        let mut manager = ipc_ns.shm_manager().lock();
        manager.remove_key_by_shmid(shmid);
        if let Some(page_num) = remove {
            manager.remove_shmid(shmid, page_num)?;
        }
    } else if cmd == SHM_LOCK {
        let mut state = shm_inner.lock();
        if !context.may_lock_shm(&state.shmid_ds.shm_perm) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        if state.lock_charge.is_none() && !context.bypasses_shm_memlock_limit() {
            let thread = curr.as_thread();
            let limit = thread.proc_data.rlim.read()[RLIMIT_MEMLOCK].current;
            let bytes = state
                .page_num
                .checked_mul(PAGE_SIZE_4K)
                .ok_or(AxError::NoMemory)?;
            state.lock_charge =
                Some(ipc_ns.try_charge_shm_lock(thread.current_cred().ids().ruid, bytes, limit)?);
        }
        state.set_locked(true);
    } else if cmd == SHM_UNLOCK {
        let mut state = shm_inner.lock();
        if !context.may_lock_shm(&state.shmid_ds.shm_perm) {
            return Err(AxError::from(LinuxError::EPERM));
        }
        state.set_locked(false);
        drop(state.lock_charge.take());
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
    if !shmaddr.is_multiple_of(PAGE_SIZE_4K) {
        return Err(AxError::InvalidInput);
    }
    let shmaddr = VirtAddr::from(shmaddr);
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let pid = proc_data.proc.pid();
    let aspace_handle = proc_data.aspace();

    // Linux identifies the first SysV VMA whose object offset has the expected
    // relation to the supplied address. This supports a later fragment after
    // partial munmap/mprotect and a whole mapping moved by mremap; an
    // arbitrary address merely contained inside a VMA does not qualify.
    let candidates = aspace_handle
        .lock()
        .sysv_shmdt_finalizer_candidates(shmaddr)?;
    let namespaces = proc_data.touched_ipc_namespaces_snapshot()?;
    let mut selected = None;
    for identity in candidates {
        let mut provenance = None;
        for namespace in &namespaces {
            let Some(record) =
                shm_attachment_record_by_finalizer_identity_in_namespace(namespace, pid, identity)
            else {
                continue;
            };
            if record.finalizer_identity != identity {
                return Err(AxError::BadState);
            }
            if provenance.is_some() {
                return Err(AxError::BadState);
            }
            provenance = Some((namespace.clone(), record));
        }
        let Some((namespace, record)) = provenance else {
            continue;
        };
        let ranges = aspace_handle.lock().sysv_shmdt_mapping_ranges(
            identity,
            shmaddr,
            record.range.size(),
        )?;
        if ranges.is_empty() {
            continue;
        }
        selected = Some((namespace.clone(), record, ranges));
        break;
    }
    let Some((namespace, record, mut ranges)) = selected else {
        return Err(AxError::InvalidInput);
    };

    // Cross-mm shared-folio demotion cannot retain this mm lock while it
    // locks peer aliases. Recheck the complete logical-fragment snapshot after
    // each lock-external pass; once stable, the final mm lock serializes the
    // all-fragment removal.
    loop {
        for range in &ranges {
            crate::syscall::ensure_4k_granularity_across_aliases(
                &aspace_handle,
                range.start,
                range.size(),
            )?;
        }
        let current_ranges = aspace_handle.lock().sysv_shmdt_mapping_ranges(
            record.finalizer_identity,
            shmaddr,
            record.range.size(),
        )?;
        if current_ranges.is_empty() {
            return Err(AxError::InvalidInput);
        }
        if current_ranges == ranges {
            break;
        }
        ranges = current_ranges;
    }

    let (wake, detached) = {
        let mut aspace = aspace_handle.lock();
        if aspace.sysv_shmdt_mapping_ranges(
            record.finalizer_identity,
            shmaddr,
            record.range.size(),
        )? != ranges
        {
            return Err(AxError::ResourceBusy);
        }
        let wake = aspace.unmap_mapping_finalizer_ranges(record.finalizer_identity, &ranges)?;
        let detached = aspace
            .mapping_ranges_with_finalizer(record.finalizer_identity)?
            .is_empty();
        (wake, detached)
    };
    wake.finish();
    if detached {
        finalize_explicit_shmdt(&namespace, pid, record);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    const PARENT_PID: Pid = 11;
    const CHILD_PID: Pid = 12;
    const SHMID: i32 = 7;
    const ATTACH_ADDR: usize = 0x4000;

    fn test_segment(key: i32, shmid: i32, page_num: usize) -> Arc<Mutex<ShmInner>> {
        Arc::new(Mutex::new(ShmInner {
            shmid,
            page_num,
            va_ranges: HashMap::new(),
            phys_pages: None,
            pending_first_pages: None,
            pending_first_attaches: 0,
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
            lock_charge: None,
        }))
    }

    #[test]
    fn shmctl_output_mirrors_initialize_abi_padding() {
        let mut ds = ShmidDs::new(IPC_PRIVATE, PAGE_SIZE_4K, 0o600, 1, 2, 3);
        ds.shm_perm.pad1 = 0x1111;
        ds.shm_perm.seq = 0x2222;
        ds.shm_perm.pad2 = 0x3333;
        ds.shm_perm.unused0 = 0x4444;
        ds.shm_perm.unused1 = 0x5555;
        let ds = initialized_shmid_ds(&ds);
        // SAFETY: `ds` is a live, fully initialized value for this test and
        // the byte slice covers exactly its object representation.
        let ds_bytes = unsafe {
            core::slice::from_raw_parts((&ds as *const ShmidDs).cast::<u8>(), size_of::<ShmidDs>())
        };
        assert_eq!(&ds_bytes[28..32], &[0; 4]);

        let usage = ShmUsageInfo {
            used_ids: 1,
            shm_tot: 2,
            shm_rss: 3,
            shm_swp: 4,
            swap_attempts: 5,
            swap_successes: 6,
        };
        let usage = initialized_shm_usage_info(&usage);
        // SAFETY: `usage` is a live, fully initialized value for this test
        // and the byte slice covers exactly its object representation.
        let usage_bytes = unsafe {
            core::slice::from_raw_parts(
                (&usage as *const ShmUsageInfo).cast::<u8>(),
                size_of::<ShmUsageInfo>(),
            )
        };
        assert_eq!(&usage_bytes[4..8], &[0; 4]);
    }

    fn attachment_range() -> VirtAddrRange {
        VirtAddrRange::new(
            VirtAddr::from(ATTACH_ADDR),
            VirtAddr::from(ATTACH_ADDR + PAGE_SIZE_4K),
        )
    }

    fn inheritance_fixture() -> (Mutex<()>, Mutex<ShmManager>, Arc<Mutex<ShmInner>>) {
        let inner = test_segment(IPC_PRIVATE, SHMID, 1);
        let mut manager = ShmManager::new();
        manager.try_reserve_segment(false).unwrap();
        manager.insert_shmid_inner(SHMID, 1, inner.clone()).unwrap();
        let manager = Mutex::new(manager);
        prepare_shmat_admission_in(
            &manager,
            inner.clone(),
            PARENT_PID,
            SHMID,
            attachment_range(),
        )
        .unwrap()
        .commit();
        (Mutex::new(()), manager, inner)
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

        manager.remove_shmid(shmid, page_num).unwrap();
        assert!(!manager.contains_shmid(shmid));
        assert_eq!(manager.total_page_count(), 0);
    }

    #[test]
    fn remove_shmid_underflow_preserves_segment_key_and_page_charge() {
        let key = 42;
        let shmid = 7;
        let page_num = 3;
        let inner = test_segment(key, shmid, page_num);
        let mut manager = ShmManager::new();
        manager.try_reserve_segment(true).unwrap();
        manager.insert_shmid_inner(shmid, page_num, inner).unwrap();
        manager.insert_key_shmid(key, shmid);
        manager.total_pages = page_num - 1;

        assert_eq!(
            manager.remove_shmid(shmid, page_num),
            Err(AxError::BadState)
        );
        assert!(manager.contains_shmid(shmid));
        assert_eq!(manager.get_shmid_by_key(key), Some(shmid));
        assert_eq!(manager.total_page_count(), page_num - 1);
    }

    #[test]
    fn shmat_admission_is_hidden_then_commits_or_rolls_back_exactly() {
        let inner = test_segment(IPC_PRIVATE, SHMID, 1);
        let mut manager = ShmManager::new();
        manager.try_reserve_segment(false).unwrap();
        manager.insert_shmid_inner(SHMID, 1, inner.clone()).unwrap();
        let manager = Mutex::new(manager);

        let admission = prepare_shmat_admission_in(
            &manager,
            inner.clone(),
            PARENT_PID,
            SHMID,
            attachment_range(),
        )
        .unwrap();
        assert_eq!(manager.lock().attachment_count, 1);
        assert_eq!(
            manager
                .lock()
                .get_shmid_by_vaddr(PARENT_PID, VirtAddr::from(ATTACH_ADDR)),
            None
        );
        let state = inner.lock();
        assert_eq!(state.attach_count(), 0);
        assert_eq!(state.visible_snapshot().shm_nattch, 0);
        assert!(state.has_attachment_owners());
        drop(state);
        assert_eq!(
            prepare_shmat_admission_in(
                &manager,
                inner.clone(),
                PARENT_PID,
                SHMID,
                attachment_range(),
            )
            .err(),
            Some(AxError::AlreadyExists)
        );

        drop(admission);
        assert_eq!(manager.lock().attachment_count, 0);
        assert!(!manager.lock().pid_vaddr_shmid.contains_key(&PARENT_PID));
        assert!(!inner.lock().va_ranges.contains_key(&PARENT_PID));

        prepare_shmat_admission_in(
            &manager,
            inner.clone(),
            PARENT_PID,
            SHMID,
            attachment_range(),
        )
        .unwrap()
        .commit();
        assert_eq!(manager.lock().attachment_count, 1);
        assert_eq!(
            manager
                .lock()
                .get_shmid_by_vaddr(PARENT_PID, VirtAddr::from(ATTACH_ADDR)),
            Some(SHMID)
        );
        assert_eq!(inner.lock().attach_count(), 1);
        assert_eq!(inner.lock().visible_snapshot().shm_nattch, 1);
    }

    #[test]
    fn pending_shmat_owns_rmid_segment_until_exact_drop() {
        let inner = test_segment(IPC_PRIVATE, SHMID, 1);
        let mut manager = ShmManager::new();
        manager.try_reserve_segment(false).unwrap();
        manager.insert_shmid_inner(SHMID, 1, inner.clone()).unwrap();
        let manager = Mutex::new(manager);
        let admission = prepare_shmat_admission_in(
            &manager,
            inner.clone(),
            PARENT_PID,
            SHMID,
            attachment_range(),
        )
        .unwrap();

        inner.lock().set_removed(true);
        assert!(inner.lock().has_attachment_owners());
        assert!(manager.lock().contains_shmid(SHMID));

        drop(admission);

        assert!(!manager.lock().contains_shmid(SHMID));
        assert_eq!(manager.lock().attachment_count, 0);
        assert_eq!(manager.lock().total_page_count(), 0);
    }

    #[test]
    fn shmat_drop_does_not_finalize_rmid_after_identity_mismatch() {
        let inner = test_segment(IPC_PRIVATE, SHMID, 1);
        let mut manager = ShmManager::new();
        manager.try_reserve_segment(false).unwrap();
        manager.insert_shmid_inner(SHMID, 1, inner.clone()).unwrap();
        let manager = Mutex::new(manager);
        let admission = prepare_shmat_admission_in(
            &manager,
            inner.clone(),
            PARENT_PID,
            SHMID,
            attachment_range(),
        )
        .unwrap();
        inner.lock().set_removed(true);

        manager
            .lock()
            .pid_vaddr_shmid
            .get_mut(&PARENT_PID)
            .unwrap()
            .entries
            .get_mut(&admission.record.id)
            .unwrap()
            .publication = Arc::new(ShmForkPublication::new(false));

        drop(admission);

        let manager = manager.lock();
        assert!(manager.contains_shmid(SHMID));
        assert_eq!(manager.total_page_count(), 1);
        assert_eq!(manager.attachment_count, 1);
        assert!(manager.pid_vaddr_shmid.contains_key(&PARENT_PID));
        assert!(inner.lock().va_ranges.contains_key(&PARENT_PID));
    }

    #[test]
    fn shmat_drop_retains_charge_when_segment_identity_changes() {
        let inner = test_segment(IPC_PRIVATE, SHMID, 1);
        let mut manager = ShmManager::new();
        manager.try_reserve_segment(false).unwrap();
        manager.insert_shmid_inner(SHMID, 1, inner.clone()).unwrap();
        let manager = Mutex::new(manager);
        let admission = prepare_shmat_admission_in(
            &manager,
            inner.clone(),
            PARENT_PID,
            SHMID,
            attachment_range(),
        )
        .unwrap();
        inner.lock().set_removed(true);

        inner
            .lock()
            .va_ranges
            .get_mut(&PARENT_PID)
            .unwrap()
            .entries
            .values_mut()
            .find(|attachment| attachment.value.base.as_usize() == ATTACH_ADDR)
            .unwrap()
            .publication = Arc::new(ShmForkPublication::new(false));
        drop(admission);

        let manager = manager.lock();
        assert!(manager.contains_shmid(SHMID));
        assert_eq!(manager.total_page_count(), 1);
        assert_eq!(manager.attachment_count, 1);
        assert!(manager.pid_vaddr_shmid.contains_key(&PARENT_PID));
        assert!(inner.lock().va_ranges.contains_key(&PARENT_PID));
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
                .is_some_and(|attachments| attachments.entries.is_empty())
        );
        manager.cancel_empty_process_reservation(pid);
        assert!(!manager.pid_vaddr_shmid.contains_key(&pid));

        manager.try_reserve_process_attaches(pid, 2).unwrap();
        let publication = Arc::new(ShmForkPublication::new(true));
        let attachments = manager.pid_vaddr_shmid.get_mut(&pid).unwrap();
        let first = ShmAttachmentRecord::new(
            allocate_attachment_id().unwrap(),
            VirtAddr::from(0x1000),
            VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x2000)),
            1,
        );
        attachments.entries.insert(
            first.id,
            ShmAttachment {
                publication: publication.clone(),
                value: first,
            },
        );
        let second = ShmAttachmentRecord::new(
            allocate_attachment_id().unwrap(),
            VirtAddr::from(0x2000),
            VirtAddrRange::new(VirtAddr::from(0x2000), VirtAddr::from(0x3000)),
            2,
        );
        attachments.entries.insert(
            second.id,
            ShmAttachment {
                publication,
                value: second,
            },
        );
        manager.attachment_count = 2;
        assert_eq!(manager.attachment_count, 2);
        manager.remove_shmaddr(pid, VirtAddr::from(0x1000));
        assert_eq!(manager.attachment_count, 1);
        assert!(manager.pid_vaddr_shmid.contains_key(&pid));
        manager.remove_shmaddr(pid, VirtAddr::from(0x2000));
        assert_eq!(manager.attachment_count, 0);
        assert!(!manager.pid_vaddr_shmid.contains_key(&pid));
    }

    #[test]
    fn fork_prepare_is_hidden_from_every_reader_until_commit() {
        let (transaction, manager, inner) = inheritance_fixture();
        let admission =
            prepare_proc_shm_inheritance_in(&transaction, &manager, PARENT_PID, CHILD_PID).unwrap();

        assert_eq!(manager.lock().attachment_count, 2);
        assert!(
            manager
                .lock()
                .pid_vaddr_shmid
                .get(&CHILD_PID)
                .is_some_and(|attachments| !attachments.is_visible())
        );
        assert!(
            inner
                .lock()
                .va_ranges
                .get(&CHILD_PID)
                .is_some_and(|ranges| !ranges.is_visible())
        );

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..256 {
                    let _transaction = transaction.lock();
                    assert_eq!(
                        manager
                            .lock()
                            .get_shmid_by_vaddr(CHILD_PID, VirtAddr::from(ATTACH_ADDR)),
                        None
                    );
                    let state = inner.lock();
                    assert_eq!(
                        state.get_addr_range(CHILD_PID, VirtAddr::from(ATTACH_ADDR)),
                        None
                    );
                    assert_eq!(state.attach_count(), 1);
                    assert_eq!(state.visible_snapshot().shm_nattch, 1);
                    std::thread::yield_now();
                }
            });
        });

        // Process-exit cleanup must ignore the hidden child bucket as well.
        clear_proc_shm_in(&transaction, &manager, CHILD_PID);
        assert!(manager.lock().pid_vaddr_shmid.contains_key(&CHILD_PID));
        assert!(inner.lock().va_ranges.contains_key(&CHILD_PID));

        admission.commit();

        assert_eq!(
            manager
                .lock()
                .get_shmid_by_vaddr(CHILD_PID, VirtAddr::from(ATTACH_ADDR)),
            Some(SHMID)
        );
        let state = inner.lock();
        assert!(
            state
                .get_addr_range(CHILD_PID, VirtAddr::from(ATTACH_ADDR))
                .is_some()
        );
        assert_eq!(state.attach_count(), 2);
        assert_eq!(state.visible_snapshot().shm_nattch, 2);
        assert_eq!(state.shmid_ds.shm_nattch, 2);
    }

    #[test]
    fn dropped_fork_prepare_refunds_exact_manager_and_segment_reservations() {
        let (transaction, manager, inner) = inheritance_fixture();
        let admission =
            prepare_proc_shm_inheritance_in(&transaction, &manager, PARENT_PID, CHILD_PID).unwrap();
        assert_eq!(manager.lock().attachment_count, 2);
        assert_eq!(inner.lock().attach_count(), 1);

        drop(admission);

        let manager_state = manager.lock();
        assert_eq!(manager_state.attachment_count, 1);
        assert!(!manager_state.pid_vaddr_shmid.contains_key(&CHILD_PID));
        drop(manager_state);
        let state = inner.lock();
        assert!(!state.va_ranges.contains_key(&CHILD_PID));
        assert_eq!(state.attach_count(), 1);
        assert_eq!(state.shmid_ds.shm_nattch, 1);
        drop(state);

        prepare_proc_shm_inheritance_in(&transaction, &manager, PARENT_PID, CHILD_PID)
            .unwrap()
            .commit();
        assert_eq!(manager.lock().attachment_count, 2);
        assert_eq!(inner.lock().attach_count(), 2);
    }

    #[test]
    fn fork_drop_retains_all_capacity_when_one_segment_identity_changes() {
        let (transaction, manager, inner) = inheritance_fixture();
        let admission =
            prepare_proc_shm_inheritance_in(&transaction, &manager, PARENT_PID, CHILD_PID).unwrap();

        inner
            .lock()
            .va_ranges
            .get_mut(&CHILD_PID)
            .unwrap()
            .publication = Arc::new(ShmForkPublication::new(false));
        drop(admission);

        let manager_state = manager.lock();
        assert_eq!(manager_state.attachment_count, 2);
        assert!(manager_state.pid_vaddr_shmid.contains_key(&CHILD_PID));
        assert!(manager_state.contains_shmid(SHMID));
        drop(manager_state);
        assert!(inner.lock().va_ranges.contains_key(&CHILD_PID));
    }

    #[test]
    fn pending_inheritance_keeps_rmid_segment_alive_then_commit_publishes_child() {
        let (transaction, manager, inner) = inheritance_fixture();
        let admission =
            prepare_proc_shm_inheritance_in(&transaction, &manager, PARENT_PID, CHILD_PID).unwrap();

        {
            let _transaction = transaction.lock();
            inner.lock().set_removed(true);
            manager
                .lock()
                .remove_shmaddr(PARENT_PID, VirtAddr::from(ATTACH_ADDR));
            let mut state = inner.lock();
            assert!(
                state
                    .detach_process(PARENT_PID, VirtAddr::from(ATTACH_ADDR))
                    .is_some()
            );
            assert_eq!(state.attach_count(), 0);
            assert_eq!(state.visible_snapshot().shm_nattch, 0);
            assert!(state.has_attachment_owners());
        }
        assert!(manager.lock().contains_shmid(SHMID));

        admission.commit();

        assert_eq!(manager.lock().attachment_count, 1);
        let state = inner.lock();
        assert!(state.rmid);
        assert_eq!(state.attach_count(), 1);
        assert_eq!(state.visible_snapshot().shm_nattch, 1);
        assert!(
            state
                .get_addr_range(CHILD_PID, VirtAddr::from(ATTACH_ADDR))
                .is_some()
        );
        drop(state);

        clear_proc_shm_in(&transaction, &manager, CHILD_PID);
        assert!(!manager.lock().contains_shmid(SHMID));
        assert_eq!(manager.lock().attachment_count, 0);
    }

    #[test]
    fn dropping_pending_inheritance_finalizes_rmid_segment_after_parent_detach() {
        let (transaction, manager, inner) = inheritance_fixture();
        let admission =
            prepare_proc_shm_inheritance_in(&transaction, &manager, PARENT_PID, CHILD_PID).unwrap();

        {
            let _transaction = transaction.lock();
            inner.lock().set_removed(true);
            manager
                .lock()
                .remove_shmaddr(PARENT_PID, VirtAddr::from(ATTACH_ADDR));
            let mut state = inner.lock();
            state.detach_process(PARENT_PID, VirtAddr::from(ATTACH_ADDR));
            assert!(state.has_attachment_owners());
        }

        drop(admission);

        let manager = manager.lock();
        assert!(!manager.contains_shmid(SHMID));
        assert_eq!(manager.total_page_count(), 0);
        assert_eq!(manager.attachment_count, 0);
    }
}
