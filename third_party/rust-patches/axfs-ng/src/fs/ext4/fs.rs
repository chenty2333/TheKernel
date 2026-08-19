use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use core::{
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use axfs_ng_vfs::{
    DirEntry, Filesystem, FilesystemOps, NodeUserData, Reference, StatFs, VfsError, VfsResult,
    path::MAX_NAME_LEN,
};
use axsync::{Mutex as SleepingMutex, MutexGuard as SleepingMutexGuard};
use hashbrown::HashMap;
use kspin::SpinNoPreempt as SpinMutex;
use lwext4_rust::{FsConfig, InodeToken, InodeType, ffi::EXT4_ROOT_INO};
use spin::Once;

use super::{
    Ext4Disk, Inode,
    util::{LwExt4Filesystem, into_vfs_err},
};
use crate::MountedBlockDevice;

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 2048 };
const EXT4_RUNTIME_ATTACHMENT_SLOTS: usize = 4_096;

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

impl RuntimeRegistry {
    fn try_new() -> VfsResult<Self> {
        Ok(Self {
            entries: HashMap::new(),
            reservations: 0,
        })
    }

    fn try_reserve(&mut self) -> VfsResult<()> {
        let mut desired_reservations =
            self.reservations.checked_add(1).ok_or(VfsError::NoMemory)?;
        if self
            .entries
            .len()
            .checked_add(desired_reservations)
            .is_none_or(|used| used > EXT4_RUNTIME_ATTACHMENT_SLOTS)
        {
            // Creation is rare and may pay to reclaim stale weak entries.
            // Ordinary path lookup below remains an O(1) hash probe and pays
            // nothing when no runtime attachments exist.
            self.entries.retain(|_, data| data.strong_count() != 0);
        }
        if self
            .entries
            .len()
            .checked_add(desired_reservations)
            .is_none_or(|used| used > EXT4_RUNTIME_ATTACHMENT_SLOTS)
        {
            return Err(VfsError::NoMemory);
        }

        // Reserve for every outstanding transaction, not merely this caller:
        // several creators may prepare private inodes before any of them
        // commits. This keeps the post-namespace-publication insert infallible
        // without charging every ext4 mount for 4096 empty buckets up front.
        if self.entries.try_reserve(desired_reservations).is_err() {
            self.entries.retain(|_, data| data.strong_count() != 0);
            desired_reservations = self.reservations.checked_add(1).ok_or(VfsError::NoMemory)?;
            self.entries
                .try_reserve(desired_reservations)
                .map_err(|_| VfsError::NoMemory)?;
        }
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
        assert!(registry.entries.capacity() < EXT4_RUNTIME_ATTACHMENT_SLOTS);
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
    fn runtime_reservations_enforce_the_logical_ceiling() {
        let mut registry = RuntimeRegistry::try_new().unwrap();
        for _ in 0..EXT4_RUNTIME_ATTACHMENT_SLOTS {
            registry.try_reserve().unwrap();
        }
        assert!(registry.entries.capacity() >= registry.entries.len() + registry.reservations);
        assert!(matches!(registry.try_reserve(), Err(VfsError::NoMemory)));
        for _ in 0..EXT4_RUNTIME_ATTACHMENT_SLOTS {
            registry.cancel_reservation();
        }
        registry.try_reserve().unwrap();
        registry.cancel_reservation();
    }
}

pub(crate) struct RuntimeReservation {
    fs: Arc<Ext4Filesystem>,
    committed: bool,
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
        let fs = Arc::try_new(Self {
            inner: SleepingMutex::new(ManuallyDrop::new(finalizer)),
            disk,
            root_dir: SpinMutex::new(None),
            runtime_data: SpinMutex::new(runtime_data),
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

    fn flush(&self) -> VfsResult<()> {
        crate::highlevel::sync_cached_file_pages_for_filesystem(self)?;
        self.lock().flush().map_err(into_vfs_err)
    }

    fn unmount(&self) {
        self.root_dir.lock().take();
    }
}
