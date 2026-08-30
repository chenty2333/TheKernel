use alloc::{
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use axfs_ng_vfs::{
    DeviceId, DirEntry, ExportHandle, ExportHandleMode, Filesystem, FilesystemOps, Metadata,
    NodePermission, NodeType, NodeUserData, Reference, StatFs, VfsError, VfsResult,
    WritebackErrorState, path::MAX_NAME_LEN,
};
use axsync::{Mutex as SleepingMutex, MutexGuard as SleepingMutexGuard};
use hashbrown::HashMap;
use kspin::SpinNoPreempt as SpinMutex;
use lwext4_rust::{FileAttr, FsConfig, InodeToken, InodeType, ffi::EXT4_ROOT_INO};
use spin::Once;

use super::{
    Ext4Disk, Inode,
    util::{LwExt4Filesystem, into_vfs_err},
};
use crate::MountedBlockDevice;

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 2048 };
const PROJECT_ID_XATTR: &[u8] = b"trusted.thekernel.project_id";

fn inode_metadata(attr: FileAttr, project_id: u32) -> Metadata {
    Metadata {
        inode: attr.ino as u64,
        device: attr.device,
        nlink: attr.nlink,
        mode: NodePermission::from_bits_truncate(attr.mode as u16),
        node_type: match attr.node_type {
            InodeType::Fifo => NodeType::Fifo,
            InodeType::CharacterDevice => NodeType::CharacterDevice,
            InodeType::Directory => NodeType::Directory,
            InodeType::BlockDevice => NodeType::BlockDevice,
            InodeType::RegularFile => NodeType::RegularFile,
            InodeType::Socket => NodeType::Socket,
            InodeType::Symlink => NodeType::Symlink,
            InodeType::Unknown => NodeType::Unknown,
        },
        uid: attr.uid,
        gid: attr.gid,
        project_id,
        size: attr.size,
        block_size: attr.block_size,
        blocks: attr.blocks,
        rdev: DeviceId(attr.rdev),
        atime: axfs_ng_vfs::Timestamp::new(attr.atime.seconds(), attr.atime.subsec_nanos()),
        btime: axfs_ng_vfs::Timestamp::new(attr.btime.seconds(), attr.btime.subsec_nanos()),
        mtime: axfs_ng_vfs::Timestamp::new(attr.mtime.seconds(), attr.mtime.subsec_nanos()),
        ctime: axfs_ng_vfs::Timestamp::new(attr.ctime.seconds(), attr.ctime.subsec_nanos()),
    }
}

fn project_id_from_xattr(value: &[u8]) -> u32 {
    <[u8; 4]>::try_from(value).map(u32::from_le_bytes).unwrap_or(0)
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RuntimeToken {
    ino: u32,
    generation: u32,
}

impl From<InodeToken> for RuntimeToken {
    fn from(token: InodeToken) -> Self {
        Self {
            ino: token.ino(),
            generation: token.generation(),
        }
    }
}

struct RuntimeRegistry {
    entries: HashMap<RuntimeToken, Weak<NodeUserData>>,
    reservations: usize,
}

/// A bounded weak index of writeback-error state currently owned by live inode
/// wrappers.  The wrapper owns the strong reference; this index merely lets a
/// second wrapper for the same `(ino, generation)` share it.  In particular,
/// dead inode generations do not retain error state until unmount.
struct WritebackErrorRegistry {
    entries: HashMap<RuntimeToken, Weak<WritebackErrorState>>,
    reservations: usize,
}

impl WritebackErrorRegistry {
    fn try_new() -> VfsResult<Self> {
        Ok(Self {
            entries: HashMap::new(),
            reservations: 0,
        })
    }

    fn try_reserve(&mut self) -> VfsResult<()> {
        let desired_reservations = self.reservations.checked_add(1).ok_or(VfsError::NoMemory)?;
        self.entries
            .len()
            .checked_add(desired_reservations)
            .ok_or(VfsError::NoMemory)?;
        // `try_reserve` guarantees capacity for all live entries plus every
        // pending publication. There is no fixed registry limit: normal
        // opens are limited only by allocatable memory.
        self.entries
            .try_reserve(desired_reservations)
            .map_err(|_| VfsError::NoMemory)?;
        self.reservations = desired_reservations;
        Ok(())
    }

    /// Returns a live state before admitting a new registry entry.  Reopening
    /// an inode must not fail merely because a registry allocation would fail.
    fn lookup_or_reserve(
        &mut self,
        token: RuntimeToken,
    ) -> VfsResult<Option<Arc<WritebackErrorState>>> {
        if let Some(state) = self.entries.get(&token).and_then(Weak::upgrade) {
            return Ok(Some(state));
        }
        // A failed upgrade is a dead generation; remove it before calculating
        // the actual allocation requirement for the new one.
        self.entries.remove(&token);
        self.try_reserve()?;
        Ok(None)
    }

    fn cancel_reservation(&mut self) {
        debug_assert!(self.reservations != 0);
        self.reservations = self.reservations.saturating_sub(1);
    }

    fn commit(
        &mut self,
        token: RuntimeToken,
        state: Arc<WritebackErrorState>,
    ) -> Arc<WritebackErrorState> {
        self.cancel_reservation();
        if let Some(existing) = self.entries.get(&token).and_then(Weak::upgrade) {
            return existing;
        }
        // Every unpublished dentry reserved one slot, so this insert cannot
        // allocate after the namespace has made a new inode visible.
        self.entries.insert(token, Arc::downgrade(&state));
        state
    }

    /// Remove an entry eagerly when its inode wrapper is its last owner.
    /// Aliased wrappers and in-flight fsync checks keep the weak index intact
    /// until ordinary stale-entry reclamation can safely remove it.
    fn remove_if_sole_owner(&mut self, token: RuntimeToken, state: &Arc<WritebackErrorState>) {
        if Arc::strong_count(state) == 1
            && self
                .entries
                .get(&token)
                .is_some_and(|registered| registered.ptr_eq(&Arc::downgrade(state)))
        {
            self.entries.remove(&token);
        }
    }
}

impl RuntimeRegistry {
    fn try_new() -> VfsResult<Self> {
        Ok(Self {
            entries: HashMap::new(),
            reservations: 0,
        })
    }

    fn try_reserve(&mut self) -> VfsResult<()> {
        // Creation is rare and may pay to reclaim stale weak entries.
        // Ordinary path lookup below remains an O(1) hash probe and pays
        // nothing when no runtime attachments exist.
        self.entries.retain(|_, data| data.strong_count() != 0);
        let desired_reservations = self.reservations.checked_add(1).ok_or(VfsError::NoMemory)?;
        self.entries
            .len()
            .checked_add(desired_reservations)
            .ok_or(VfsError::NoMemory)?;

        // Reserve for every outstanding transaction, not merely this caller:
        // several creators may prepare private inodes before any of them
        // commits. This keeps the post-namespace-publication insert infallible
        // while allowing the registry to grow with legitimate open files.
        self.entries
            .try_reserve(desired_reservations)
            .map_err(|_| VfsError::NoMemory)?;
        self.reservations = desired_reservations;
        Ok(())
    }

    fn cancel_reservation(&mut self) {
        debug_assert!(self.reservations != 0);
        self.reservations = self.reservations.saturating_sub(1);
    }

    fn commit(
        &mut self,
        token: RuntimeToken,
        data: Arc<NodeUserData>,
    ) -> (Arc<NodeUserData>, Option<Weak<NodeUserData>>) {
        self.cancel_reservation();
        if let Some(existing) = self.entries.get(&token).and_then(Weak::upgrade) {
            return (existing, None);
        }
        // try_reserve admitted capacity for every outstanding transaction,
        // and reservations keep entries + pending commits within that
        // capacity. This post-create insert cannot grow.
        let retired = self.entries.insert(token, Arc::downgrade(&data));
        (data, retired)
    }

    fn attachment(&mut self, token: RuntimeToken) -> Option<Arc<NodeUserData>> {
        let data = self.entries.get(&token)?.upgrade();
        if data.is_none() {
            self.entries.remove(&token);
        }
        data
    }
}

#[cfg(test)]
mod runtime_registry_tests {
    use super::*;

    #[test]
    fn ext4_io_and_wrapper_state_use_their_intended_lock_domains() {
        fn assert_lock_types(fs: &Ext4Filesystem) {
            let _: &SleepingMutex<ManuallyDrop<Box<DeferredExt4Finalizer>>> = &fs.inner;
            let _: &SpinMutex<Option<DirEntry>> = &fs.root_dir;
            let _: &SpinMutex<RuntimeRegistry> = &fs.runtime_data;
        }

        // This compile-time shape check prevents the I/O-bearing lwext4 lock
        // from silently being changed back to a non-preemptible spin lock,
        // while preserving spin locks for the two short wrapper-only fields.
        let _ = assert_lock_types as fn(&Ext4Filesystem);
    }

    #[test]
    fn runtime_registry_allocates_capacity_on_demand() {
        let mut registry = RuntimeRegistry::try_new().unwrap();
        assert_eq!(registry.entries.capacity(), 0);

        registry.try_reserve().unwrap();
        assert!(registry.entries.capacity() >= 1);
        assert!(registry.entries.capacity() >= 1);
        registry.cancel_reservation();
    }

    #[test]
    fn runtime_identity_includes_inode_generation_and_reclaims_stale_entries() {
        let mut registry = RuntimeRegistry::try_new().unwrap();
        let token = RuntimeToken {
            ino: 17,
            generation: 3,
        };
        let replacement = RuntimeToken {
            ino: 17,
            generation: 4,
        };
        let data = Arc::new(NodeUserData::new());
        registry.try_reserve().unwrap();
        let (initial_installed, retired) = registry.commit(token, data.clone());
        assert!(retired.is_none());
        assert!(Arc::ptr_eq(&initial_installed, &data));
        assert!(Arc::ptr_eq(&registry.attachment(token).unwrap(), &data));
        assert!(registry.attachment(replacement).is_none());
        drop(initial_installed);

        registry.try_reserve().unwrap();
        let duplicate = Arc::new(NodeUserData::new());
        let (duplicate_installed, retired) = registry.commit(token, duplicate.clone());
        assert!(retired.is_none());
        assert!(Arc::ptr_eq(&duplicate_installed, &data));
        assert!(!Arc::ptr_eq(&duplicate_installed, &duplicate));

        drop(duplicate_installed);
        drop(data);
        assert!(registry.attachment(token).is_none());
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn runtime_reservations_grow_past_the_former_logical_ceiling() {
        let mut registry = RuntimeRegistry::try_new().unwrap();
        for _ in 0..=4_096 {
            registry.try_reserve().unwrap();
        }
        assert!(registry.entries.capacity() >= registry.entries.len() + registry.reservations);
        for _ in 0..=4_096 {
            registry.cancel_reservation();
        }
    }

    #[test]
    fn writeback_errors_share_live_inode_state_and_reclaim_dead_generations() {
        let mut registry = WritebackErrorRegistry::try_new().unwrap();
        let token = RuntimeToken {
            ino: 17,
            generation: 3,
        };

        assert!(registry.lookup_or_reserve(token).unwrap().is_none());
        let initial = registry.commit(token, Arc::new(WritebackErrorState::default()));
        initial.publish(VfsError::Io);

        let reservations_before_relookup = registry.reservations;
        let relookup = registry.lookup_or_reserve(token).unwrap().unwrap();
        assert_eq!(registry.reservations, reservations_before_relookup);
        assert!(Arc::ptr_eq(&initial, &relookup));
        let mut cursor = relookup.sample();
        assert_eq!(cursor, 0);
        assert_eq!(relookup.check_and_advance(&mut cursor), Some(VfsError::Io));
        assert_eq!(relookup.sample(), 1);

        drop(relookup);
        registry.remove_if_sole_owner(token, &initial);
        drop(initial);
        assert!(registry.entries.is_empty());

        assert!(
            registry
                .lookup_or_reserve(RuntimeToken {
                    ino: token.ino,
                    generation: token.generation + 1,
                })
                .unwrap()
                .is_none()
        );
        let reused_ino = registry.commit(
            RuntimeToken {
                ino: token.ino,
                generation: token.generation + 1,
            },
            Arc::new(WritebackErrorState::default()),
        );
        assert_eq!(reused_ino.sample(), 0);
    }

    #[test]
    fn writeback_error_registry_grows_past_the_former_logical_ceiling() {
        let mut registry = WritebackErrorRegistry::try_new().unwrap();
        for _ in 0..=4_096 {
            registry.try_reserve().unwrap();
        }
        assert!(registry.entries.capacity() >= registry.entries.len() + registry.reservations);
        for _ in 0..=4_096 {
            registry.cancel_reservation();
        }
    }
}

pub(crate) struct RuntimeReservation {
    fs: Arc<Ext4Filesystem>,
    committed: bool,
}

pub(crate) struct WritebackErrorReservation {
    fs: Arc<Ext4Filesystem>,
    state: Arc<WritebackErrorState>,
    reserved: bool,
}

impl WritebackErrorReservation {
    pub(crate) fn commit(mut self, token: InodeToken) -> Arc<WritebackErrorState> {
        if !self.reserved {
            return self.state.clone();
        }
        let state = self
            .fs
            .writeback_errors
            .lock()
            .commit(token.into(), self.state.clone());
        self.reserved = false;
        state
    }
}

impl Drop for WritebackErrorReservation {
    fn drop(&mut self) {
        if self.reserved {
            self.fs.writeback_errors.lock().cancel_reservation();
        }
    }
}

impl RuntimeReservation {
    pub(crate) fn commit(
        mut self,
        token: InodeToken,
        data: Arc<NodeUserData>,
    ) -> Arc<NodeUserData> {
        let (installed, retired) = {
            let mut registry = self.fs.runtime_data.lock();
            registry.commit(token.into(), data)
        };
        drop(retired);
        self.committed = true;
        installed
    }
}

impl Drop for RuntimeReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.fs.runtime_data.lock().cancel_reservation();
    }
}

struct DeferredExt4Finalizer {
    fs: LwExt4Filesystem,
    next: AtomicPtr<Self>,
}

// The mounted wrapper was already required to be Send/Sync for VFS use. The
// finalizer transfers its exclusively owned low-level filesystem to one
// kernel worker after the final VFS reference disappears.
unsafe impl Send for DeferredExt4Finalizer {}

static DEFERRED_FINALIZER_HEAD: AtomicPtr<DeferredExt4Finalizer> = AtomicPtr::new(ptr::null_mut());
static DEFERRED_FINALIZER_WAKE: Once<fn()> = Once::new();

fn enqueue_deferred_finalizer(work: Box<DeferredExt4Finalizer>) {
    let raw = Box::into_raw(work);
    let mut head = DEFERRED_FINALIZER_HEAD.load(Ordering::Acquire);
    loop {
        // SAFETY: `raw` is exclusively owned until the successful publish.
        unsafe {
            (*raw).next.store(head, Ordering::Relaxed);
        }
        match DEFERRED_FINALIZER_HEAD.compare_exchange_weak(
            head,
            raw,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => head = observed,
        }
    }
    if let Some(wake) = DEFERRED_FINALIZER_WAKE.get() {
        wake();
    }
}

pub fn set_deferred_finalizer_waker(waker: fn()) -> bool {
    let installed = *DEFERRED_FINALIZER_WAKE.call_once(|| waker);
    if has_deferred_finalizer_work() {
        installed();
    }
    core::ptr::fn_addr_eq(installed, waker)
}

pub fn has_deferred_finalizer_work() -> bool {
    !DEFERRED_FINALIZER_HEAD.load(Ordering::Acquire).is_null()
}

/// Finalizes one finite batch of detached ext4 filesystems in FIFO order.
///
/// The dedicated worker may block and be preempted during writeback. Scheduler
/// safe points only wake that worker; they never execute this destructor.
/// `between` runs after each completed shutdown so the caller can yield.
pub fn drain_deferred_finalizers(mut between: impl FnMut()) -> usize {
    // Producers publish a LIFO stack. Atomically detaching it gives this
    // consumer exclusive ownership of a finite batch; reversing that batch
    // prevents older shutdowns from starving behind a continuous producer.
    let mut pending = DEFERRED_FINALIZER_HEAD.swap(ptr::null_mut(), Ordering::Acquire);
    let mut fifo = ptr::null_mut();
    while !pending.is_null() {
        // SAFETY: `pending` belongs exclusively to this detached batch.
        let next = unsafe { (*pending).next.load(Ordering::Relaxed) };
        // SAFETY: no producer can reach a node after the batch was detached.
        unsafe {
            (*pending).next.store(fifo, Ordering::Relaxed);
        }
        fifo = pending;
        pending = next;
    }

    let mut drained = 0usize;
    while !fifo.is_null() {
        // SAFETY: `fifo` remains exclusively owned by this consumer.
        let next = unsafe { (*fifo).next.load(Ordering::Relaxed) };
        // SAFETY: removing the node from the private list transfers its unique
        // Box ownership to this task-context worker.
        let mut work = unsafe { Box::from_raw(fifo) };
        if let Err(error) = work.fs.shutdown() {
            log::error!("deferred ext4 shutdown failed: {error}");
        }
        // `shutdown` is idempotent; the low-level Drop remains a fallback if
        // this worker ever exits before explicitly reaching the call above.
        drop(work);
        drained = drained.saturating_add(1);
        between();
        fifo = next;
    }
    drained
}

pub(crate) struct Ext4Guard<'a>(SleepingMutexGuard<'a, ManuallyDrop<Box<DeferredExt4Finalizer>>>);

impl Deref for Ext4Guard<'_> {
    type Target = LwExt4Filesystem;

    fn deref(&self) -> &Self::Target {
        &self.0.fs
    }
}

impl DerefMut for Ext4Guard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0.fs
    }
}

pub struct Ext4Filesystem {
    // lwext4 may perform synchronous block I/O while this serialization lock
    // is held.  The shared block-device path uses sleeping mutexes once its
    // completion broker is active, so this outer lock must leave the current
    // task blockable when another task owns the device queue.
    inner: SleepingMutex<ManuallyDrop<Box<DeferredExt4Finalizer>>>,
    disk: Ext4Disk,
    // These protect only short, non-I/O wrapper state and deliberately keep
    // their original non-preemptible spin semantics.
    root_dir: SpinMutex<Option<DirEntry>>,
    runtime_data: SpinMutex<RuntimeRegistry>,
    writeback_errors: SpinMutex<WritebackErrorRegistry>,
    syncfs_writeback_errors: Arc<WritebackErrorState>,
}

impl Ext4Filesystem {
    pub fn new(dev: MountedBlockDevice) -> VfsResult<Filesystem> {
        let disk = Ext4Disk::new(dev);
        let mut ext4 =
            lwext4_rust::Ext4Filesystem::new(disk.clone(), EXT4_CONFIG).map_err(into_vfs_err)?;
        let (root_token, root_namespace_epoch) = ext4
            .retain_inode_handle(EXT4_ROOT_INO)
            .map_err(into_vfs_err)?;

        let finalizer = Box::try_new(DeferredExt4Finalizer {
            fs: ext4,
            next: AtomicPtr::new(ptr::null_mut()),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let runtime_data = RuntimeRegistry::try_new()?;
        let writeback_errors = WritebackErrorRegistry::try_new()?;
        let syncfs_writeback_errors =
            Arc::try_new(WritebackErrorState::default()).map_err(|_| VfsError::NoMemory)?;
        let fs = Arc::try_new(Self {
            inner: SleepingMutex::new(ManuallyDrop::new(finalizer)),
            disk,
            root_dir: SpinMutex::new(None),
            runtime_data: SpinMutex::new(runtime_data),
            writeback_errors: SpinMutex::new(writeback_errors),
            syncfs_writeback_errors,
        })
        .map_err(|_| VfsError::NoMemory)?;
        // Allocate the wrapper before installing the backend root self-cycle;
        // a later failure can then run `unmount` through normal RAII cleanup.
        let filesystem = Filesystem::try_new(fs.clone())?;
        let prepared =
            Inode::try_prepare_entry(fs.clone(), InodeType::Directory, Reference::root())?;
        let root = prepared.bind(root_token, root_namespace_epoch);
        *fs.root_dir.lock() = Some(root);
        Ok(filesystem)
    }

    pub(crate) fn lock(&self) -> Ext4Guard<'_> {
        Ext4Guard(self.inner.lock())
    }

    pub(crate) fn reserve_runtime_attachment(self: &Arc<Self>) -> VfsResult<RuntimeReservation> {
        self.runtime_data.lock().try_reserve()?;
        Ok(RuntimeReservation {
            fs: self.clone(),
            committed: false,
        })
    }

    pub(crate) fn runtime_attachment(&self, token: InodeToken) -> Option<Arc<NodeUserData>> {
        self.runtime_data.lock().attachment(token.into())
    }

    pub(crate) fn reserve_writeback_error_state(
        self: &Arc<Self>,
        token: Option<InodeToken>,
    ) -> VfsResult<WritebackErrorReservation> {
        if let Some(token) = token
            && let Some(state) = self
                .writeback_errors
                .lock()
                .lookup_or_reserve(token.into())?
        {
            return Ok(WritebackErrorReservation {
                fs: self.clone(),
                state,
                reserved: false,
            });
        }
        if token.is_none() {
            self.writeback_errors.lock().try_reserve()?;
        }
        let state = match Arc::try_new(WritebackErrorState::default()) {
            Ok(state) => state,
            Err(_) => {
                self.writeback_errors.lock().cancel_reservation();
                return Err(VfsError::NoMemory);
            }
        };
        Ok(WritebackErrorReservation {
            fs: self.clone(),
            state,
            reserved: true,
        })
    }

    pub(crate) fn release_writeback_error_state(
        &self,
        token: InodeToken,
        state: &Arc<WritebackErrorState>,
    ) {
        self.writeback_errors
            .lock()
            .remove_if_sole_owner(token.into(), state);
    }

    pub(crate) fn wait_async_write(
        &self,
        submission: &lwext4_rust::AsyncWriteSubmission,
    ) -> VfsResult<()> {
        self.disk.wait_async_write(submission).map_err(into_vfs_err)
    }

    pub(crate) fn wait_async_read(
        &self,
        submission: &lwext4_rust::AsyncReadSubmission,
    ) -> VfsResult<()> {
        self.disk.wait_async_read(submission).map_err(into_vfs_err)
    }

    pub(crate) fn plan_physical_io(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
        overwrite_only: bool,
    ) -> VfsResult<Option<lwext4_rust::PhysicalIoPlan>> {
        self.lock()
            .plan_physical_io(ino, offset, len, overwrite_only)
            .map_err(into_vfs_err)
    }

    pub(crate) fn prepare_physical_io_plan(
        &self,
        ino: u32,
        operation: lwext4_rust::PhysicalIoOperation,
        offset: u64,
        len: usize,
        segments: &[lwext4_rust::PhysicalIoSegment],
    ) -> VfsResult<Option<lwext4_rust::PhysicalIoPlan>> {
        self.lock()
            .prepare_physical_io_plan(ino, operation, offset, len, segments)
            .map_err(into_vfs_err)
    }

    pub(crate) fn validate_physical_io_plan(
        &self,
        plan: lwext4_rust::PhysicalIoPlan,
    ) -> VfsResult<()> {
        self.lock()
            .validate_physical_io_plan(plan)
            .map_err(into_vfs_err)
    }

    pub(crate) fn commit_physical_io_write(
        &self,
        plan: lwext4_rust::PhysicalIoPlan,
    ) -> VfsResult<()> {
        self.lock()
            .commit_physical_io_write(plan)
            .map_err(into_vfs_err)
    }

    pub(crate) fn finalize_physical_io_plan(
        &self,
        plan: lwext4_rust::PhysicalIoPlan,
        all_completions_success: bool,
    ) -> VfsResult<()> {
        self.lock()
            .finalize_physical_io_plan(plan, all_completions_success)
            .map_err(into_vfs_err)
    }

    pub(crate) fn physical_disk(&self) -> Ext4Disk {
        self.disk.clone()
    }
}

unsafe impl Send for Ext4Filesystem {}

unsafe impl Sync for Ext4Filesystem {}

impl Drop for Ext4Filesystem {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        // SAFETY: this is the final Arc release, so no guard can coexist with
        // `&mut self`. The ManuallyDrop slot is consumed exactly once here and
        // the mutex later drops an inert slot.
        let work = unsafe { ManuallyDrop::take(&mut *inner) };
        drop(inner);
        enqueue_deferred_finalizer(work);
    }
}

impl FilesystemOps for Ext4Filesystem {
    fn name(&self) -> &str {
        "ext4"
    }

    fn root_dir(&self) -> DirEntry {
        self.root_dir.lock().clone().unwrap()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        let mut fs = self.lock();
        let stat = fs.stat().map_err(into_vfs_err)?;
        Ok(StatFs {
            fs_type: 0xef53,
            block_size: stat.block_size as _,
            blocks: stat.blocks_count,
            blocks_free: stat.free_blocks_count,
            blocks_available: stat.free_blocks_count,

            file_count: stat.inodes_count as _,
            free_file_count: stat.free_inodes_count as _,

            name_length: MAX_NAME_LEN as _,
            fragment_size: 0,
            mount_flags: 0,
        })
    }

    fn enumerate_inodes(&self, visitor: &mut axfs_ng_vfs::InodeVisitor<'_>) -> VfsResult<()> {
        let mut fs = self.lock();
        let count = fs.stat().map_err(into_vfs_err)?.inodes_count;
        for ino in 1..=count {
            if !fs.inode_is_allocated(ino).map_err(into_vfs_err)? {
                continue;
            }
            let mut attr = FileAttr::default();
            fs.get_attr(ino, &mut attr).map_err(into_vfs_err)?;
            // Quota usage is released at unlink, while lwext4 retains an
            // unlinked inode's blocks until its final open handle closes.
            // Do not resurrect that already-refunded usage during Q_QUOTAON.
            if attr.nlink == 0 {
                continue;
            }
            // Project IDs are an ext4 xattr in this VFS, so retrieve it from
            // the same inode-table entry before publishing the snapshot.
            let mut project_id = 0;
            fs.with_inode_ref(attr.ino, |inode| {
                if let Ok(value) = inode.get_xattr(PROJECT_ID_XATTR) {
                    project_id = project_id_from_xattr(&value);
                }
                Ok(())
            }).map_err(into_vfs_err)?;
            visitor(inode_metadata(attr, project_id))?;
        }
        Ok(())
    }
    fn encode_export_handle(&self, entry: &DirEntry, mode: ExportHandleMode) -> VfsResult<ExportHandle> {
        let inode = entry.downcast::<Inode>()?;
        if !core::ptr::eq(self, inode.filesystem().as_ref()) {
            return Err(VfsError::CrossesDevices);
        }
        let token = inode.export_handle().ok_or(VfsError::NotFound)?;
        let mut bytes = alloc::vec::Vec::new();
        bytes.try_reserve_exact(8).map_err(|_| VfsError::NoMemory)?;
        bytes.extend_from_slice(&(token.ino() as u32).to_ne_bytes());
        bytes.extend_from_slice(&(token.generation() as u32).to_ne_bytes());
        Ok(ExportHandle { handle_type: if mode == ExportHandleMode::Fid { 2 } else { 1 }, bytes })
    }

    fn decode_export_handle(&self, handle_type: i32, bytes: &[u8]) -> VfsResult<DirEntry> {
        if handle_type != 1 || bytes.len() != 8 { return Err(VfsError::NotFound); }
        let ino = u32::from_ne_bytes(bytes[..4].try_into().map_err(|_| VfsError::NotFound)?);
        let generation = u32::from_ne_bytes(bytes[4..].try_into().map_err(|_| VfsError::NotFound)?);
        let fs = self.root_dir().downcast::<Inode>()?.filesystem();
        let (token, epoch, inode_type) = {
            let mut low = fs.lock();
            let (token, epoch) = low.retain_inode_handle(ino).map_err(into_vfs_err)?;
            if token.generation() != generation {
                low.release_inode_handle(token);
                return Err(VfsError::NotFound);
            }
            let mut attr = FileAttr::default();
            if let Err(error) = low.get_attr(ino, &mut attr) {
                low.release_inode_handle(token);
                return Err(into_vfs_err(error));
            }
            (token, epoch, attr.node_type)
        };
        Inode::try_finish_exported_entry(fs, token, epoch, inode_type)
    }

    fn export_handle_is_descendant(
        &self,
        ancestor: &DirEntry,
        handle_type: i32,
        bytes: &[u8],
    ) -> VfsResult<bool> {
        // Decode first: this checks the candidate's generation before the
        // namespace walk.  Then walk from the mapped directory, preserving
        // real ext4 parent/child topology rather than anonymous references.
        let _ = self.decode_export_handle(handle_type, bytes)?;
        let mut pending = Vec::new();
        pending.try_reserve(16).map_err(|_| VfsError::NoMemory)?;
        pending.push(ancestor.clone());
        while let Some(entry) = pending.pop() {
            let exported = self.encode_export_handle(&entry, ExportHandleMode::Openable)?;
            if exported.handle_type == handle_type && exported.bytes == bytes {
                return Ok(true);
            }
            let Ok(directory) = entry.as_dir() else { continue };
            let mut names = Vec::<String>::new();
            directory.read_dir(0, &mut |name: &str, _: u64, _: NodeType, _: u64| {
                if name != "." && name != ".." {
                    names.push(String::from(name));
                }
                true
            })?;
            for name in names {
                let child = directory.lookup(&name)?;
                if child.is_dir() {
                    pending.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    pending.push(child);
                } else {
                    let exported = self.encode_export_handle(&child, ExportHandleMode::Openable)?;
                    if exported.handle_type == handle_type && exported.bytes == bytes {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn flush(&self) -> VfsResult<()> {
        crate::highlevel::sync_cached_file_pages_for_filesystem(self)?;
        self.lock().flush().map_err(into_vfs_err)
    }

    fn syncfs_writeback_error_state(&self) -> Option<Arc<WritebackErrorState>> {
        Some(self.syncfs_writeback_errors.clone())
    }

    fn unmount(&self) {
        self.root_dir.lock().take();
    }
}
