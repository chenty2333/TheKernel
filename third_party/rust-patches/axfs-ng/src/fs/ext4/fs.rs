use alloc::{boxed::Box, sync::Arc};
use core::{
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use axfs_ng_vfs::{
    DirEntry, Filesystem, FilesystemOps, Reference, StatFs, VfsError, VfsResult, path::MAX_NAME_LEN,
};
use kspin::{SpinNoPreempt as Mutex, SpinNoPreemptGuard as MutexGuard};
use lwext4_rust::{FsConfig, InodeType, ffi::EXT4_ROOT_INO};
use spin::Once;

use super::{
    Ext4Disk, Inode,
    util::{LwExt4Filesystem, into_vfs_err},
};
use crate::MountedBlockDevice;

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 2048 };

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

pub(crate) struct Ext4Guard<'a>(MutexGuard<'a, ManuallyDrop<Box<DeferredExt4Finalizer>>>);

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
    inner: Mutex<ManuallyDrop<Box<DeferredExt4Finalizer>>>,
    disk: Ext4Disk,
    root_dir: Mutex<Option<DirEntry>>,
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
        let fs = Arc::try_new(Self {
            inner: Mutex::new(ManuallyDrop::new(finalizer)),
            disk,
            root_dir: Mutex::new(None),
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
