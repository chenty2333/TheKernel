use alloc::sync::{Arc, Weak};
use core::{
    array,
    marker::PhantomPinned,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axdriver::SharedBlockDevice;
use axfs_ng_vfs::{
    DirEntry, Filesystem, FilesystemOps, MetadataUpdateCapabilities, NodeUserData, Reference,
    StatFs, VfsError, VfsResult, path::MAX_NAME_LEN,
};
use hashbrown::HashMap;
use kspin::{SpinNoPreempt as Mutex, SpinNoPreemptGuard as MutexGuard};

use super::{dir::FatDirNode, disk::SeekableDisk, ff, util::into_vfs_err};
use crate::{FatMountOptions, MountedBlockDevice};

pub struct FatFilesystemInner {
    pub inner: ff::FileSystem,
    pub mount_options: FatMountOptions,
    pub root_atime: Duration,
    pub root_mtime: Duration,
    _pinned: PhantomPinned,
}

/// FAT has no persistent inode number. Bound the synthetic identities to the
/// maximum number of simultaneously live VFS nodes instead of growing a slab
/// for every historical lookup.
const MAX_LIVE_FAT_NODES: usize = 65_536;
const INODE_WORD_BITS: usize = usize::BITS as usize;
const INODE_WORDS: usize = MAX_LIVE_FAT_NODES.div_ceil(INODE_WORD_BITS);
const UNINSTALLED_ENTRY_POSITION: u64 = u64::MAX;

struct FatInodeAllocator {
    slots: [AtomicUsize; INODE_WORDS],
    cursor: AtomicUsize,
}

impl FatInodeAllocator {
    fn new() -> Self {
        Self {
            slots: array::from_fn(|_| AtomicUsize::new(0)),
            cursor: AtomicUsize::new(0),
        }
    }

    fn allocate(&self) -> VfsResult<u64> {
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % INODE_WORDS;
        for offset in 0..INODE_WORDS {
            let word_index = (start + offset) % INODE_WORDS;
            let word = &self.slots[word_index];
            let mut occupied = word.load(Ordering::Relaxed);
            loop {
                let free = !occupied;
                if free == 0 {
                    break;
                }
                let bit = free.trailing_zeros() as usize;
                let mask = 1usize << bit;
                match word.compare_exchange_weak(
                    occupied,
                    occupied | mask,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let index = word_index * INODE_WORD_BITS + bit;
                        if index < MAX_LIVE_FAT_NODES {
                            return Ok(index as u64 + 1);
                        }
                        word.fetch_and(!mask, Ordering::Release);
                        break;
                    }
                    Err(current) => occupied = current,
                }
            }
        }
        Err(VfsError::NoMemory)
    }

    fn release(&self, inode: u64) {
        let Some(index) = inode
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
        else {
            return;
        };
        if index >= MAX_LIVE_FAT_NODES {
            return;
        }
        let word = index / INODE_WORD_BITS;
        let bit = index % INODE_WORD_BITS;
        self.slots[word].fetch_and(!(1usize << bit), Ordering::Release);
    }
}

struct FatEntryRegistry {
    next_generation: u64,
    reserved: usize,
    states: HashMap<u64, RegisteredFatEntry>,
}

struct RegisteredFatEntry {
    generation: u64,
    state: Weak<FatEntryRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FatEntryIdentity {
    pub position: Option<u64>,
    pub generation: u64,
}

struct FatEntryRecord {
    fs: Weak<FatFilesystem>,
    position: AtomicU64,
    generation: u64,
    namespace_epoch: AtomicU64,
    /// Runtime state tied to this physical FAT directory-entry generation.
    /// The registry returns this record on relookup while any node is live.
    user_data: NodeUserData,
}

impl Drop for FatEntryRecord {
    fn drop(&mut self) {
        let position = self.position.load(Ordering::Acquire);
        if position == UNINSTALLED_ENTRY_POSITION {
            return;
        }
        if let Some(fs) = self.fs.upgrade() {
            fs.release_entry_record(position, self.generation);
        }
    }
}

#[derive(Clone)]
pub(crate) struct FatEntryState(Arc<FatEntryRecord>);

impl FatEntryState {
    fn try_root(fs: &Arc<FatFilesystem>) -> VfsResult<Self> {
        Arc::try_new(FatEntryRecord {
            fs: Arc::downgrade(fs),
            position: AtomicU64::new(UNINSTALLED_ENTRY_POSITION),
            generation: 0,
            namespace_epoch: AtomicU64::new(0),
            user_data: NodeUserData::default(),
        })
        .map(Self)
        .map_err(|_| VfsError::NoMemory)
    }

    pub(crate) fn identity(&self) -> FatEntryIdentity {
        let position = self.0.position.load(Ordering::Acquire);
        FatEntryIdentity {
            position: (position != UNINSTALLED_ENTRY_POSITION).then_some(position),
            generation: self.0.generation,
        }
    }

    pub(crate) fn namespace_epoch(&self) -> &AtomicU64 {
        &self.0.namespace_epoch
    }

    pub(crate) fn user_data(&self) -> &NodeUserData {
        &self.0.user_data
    }
}

pub(crate) struct FatEntryAdmission {
    fs: Arc<FatFilesystem>,
    state: FatEntryState,
    committed: bool,
}

impl FatEntryAdmission {
    pub(crate) fn state(&self) -> FatEntryState {
        self.state.clone()
    }

    pub(crate) fn commit(mut self, position: u64) {
        self.fs.install_entry_state(position, &self.state);
        self.committed = true;
    }
}

impl Drop for FatEntryAdmission {
    fn drop(&mut self) {
        if !self.committed {
            let mut registry = self.fs.entry_registry.lock();
            registry.reserved = registry.reserved.saturating_sub(1);
        }
    }
}

impl FatEntryRegistry {
    fn reserve_state(&mut self, fs: &Arc<FatFilesystem>) -> VfsResult<FatEntryState> {
        if self.states.len().saturating_add(self.reserved) >= MAX_LIVE_FAT_NODES {
            return Err(VfsError::NoMemory);
        }
        self.states.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(VfsError::NoMemory)?;
        self.reserved += 1;
        match Arc::try_new(FatEntryRecord {
            fs: Arc::downgrade(fs),
            position: AtomicU64::new(UNINSTALLED_ENTRY_POSITION),
            generation,
            namespace_epoch: AtomicU64::new(0),
            user_data: NodeUserData::default(),
        }) {
            Ok(record) => Ok(FatEntryState(record)),
            Err(_) => {
                self.reserved -= 1;
                Err(VfsError::NoMemory)
            }
        }
    }

    fn install_entry_state(&mut self, position: u64, state: &FatEntryState) {
        self.reserved = self.reserved.saturating_sub(1);
        state.0.position.store(position, Ordering::Release);
        self.states.insert(
            position,
            RegisteredFatEntry {
                generation: state.0.generation,
                state: Arc::downgrade(&state.0),
            },
        );
    }

    fn entry_state(&mut self, fs: &Arc<FatFilesystem>, position: u64) -> VfsResult<FatEntryState> {
        if let Some(state) = self
            .states
            .get(&position)
            .and_then(|entry| entry.state.upgrade())
        {
            return Ok(FatEntryState(state));
        }
        self.states.remove(&position);
        let state = self.reserve_state(fs)?;
        self.install_entry_state(position, &state);
        Ok(state)
    }

    fn forget_entry(&mut self, position: u64) {
        self.states.remove(&position);
    }
}

pub struct FatFilesystem {
    inner: Mutex<FatFilesystemInner>,
    entry_registry: Mutex<FatEntryRegistry>,
    inode_allocator: FatInodeAllocator,
    root_dir: Mutex<Option<DirEntry>>,
    device: SharedBlockDevice,
}

impl FatFilesystem {
    pub fn new(dev: MountedBlockDevice) -> VfsResult<Filesystem> {
        Self::new_with_options(dev, FatMountOptions::default())
    }

    pub fn new_with_options(
        dev: MountedBlockDevice,
        mount_options: FatMountOptions,
    ) -> VfsResult<Filesystem> {
        let device = dev.device().clone();
        let inner = FatFilesystemInner {
            inner: ff::FileSystem::new(SeekableDisk::new(dev), fatfs::FsOptions::new())
                .map_err(into_vfs_err)?,
            mount_options,
            root_atime: Duration::ZERO,
            root_mtime: Duration::ZERO,
            _pinned: PhantomPinned,
        };
        let result = Arc::try_new(Self {
            inner: Mutex::new(inner),
            entry_registry: Mutex::new(FatEntryRegistry {
                next_generation: 1,
                reserved: 0,
                states: HashMap::new(),
            }),
            inode_allocator: FatInodeAllocator::new(),
            root_dir: Mutex::default(),
            device,
        })
        .map_err(|_| VfsError::NoMemory)?;

        let root_state = FatEntryState::try_root(&result)?;
        let root_inode = result.alloc_inode()?;
        let root_dir = {
            let fs = result.lock();
            FatDirNode::try_new_initialized(
                result.clone(),
                fs.inner.root_dir(),
                root_inode,
                root_state,
                Reference::root(),
                &fs,
            )?
        };
        *result.root_dir.lock() = Some(root_dir);
        match Filesystem::try_new(result.clone()) {
            Ok(filesystem) => Ok(filesystem),
            Err(error) => {
                result.root_dir.lock().take();
                Err(error)
            }
        }
    }
}

impl FatFilesystem {
    pub(crate) fn lock(&self) -> MutexGuard<'_, FatFilesystemInner> {
        self.inner.lock()
    }

    pub(crate) fn alloc_inode(&self) -> VfsResult<u64> {
        self.inode_allocator.allocate()
    }

    pub(crate) fn release_inode(&self, inode: u64) {
        self.inode_allocator.release(inode);
    }

    pub(crate) fn prepare_entry_state(self: &Arc<Self>) -> VfsResult<FatEntryAdmission> {
        let state = self.entry_registry.lock().reserve_state(self)?;
        Ok(FatEntryAdmission {
            fs: self.clone(),
            state,
            committed: false,
        })
    }

    fn install_entry_state(&self, position: u64, state: &FatEntryState) {
        self.entry_registry
            .lock()
            .install_entry_state(position, state);
    }

    pub(crate) fn entry_state(self: &Arc<Self>, position: u64) -> VfsResult<FatEntryState> {
        self.entry_registry.lock().entry_state(self, position)
    }

    pub(crate) fn forget_entry(&self, position: u64) {
        self.entry_registry.lock().forget_entry(position);
    }

    fn release_entry_record(&self, position: u64, generation: u64) {
        let mut registry = self.entry_registry.lock();
        if registry
            .states
            .get(&position)
            .is_some_and(|entry| entry.generation == generation)
        {
            registry.states.remove(&position);
        }
    }
}

impl FilesystemOps for FatFilesystem {
    fn name(&self) -> &str {
        "vfat"
    }

    fn root_dir(&self) -> DirEntry {
        self.root_dir.lock().clone().unwrap()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        let fs = self.inner.lock();
        let stats = fs.inner.stats().map_err(into_vfs_err)?;
        let cluster_size = stats.cluster_size() as u32;
        Ok(StatFs {
            fs_type: 0x4d44,
            block_size: cluster_size,
            blocks: stats.total_clusters() as _,
            blocks_free: stats.free_clusters() as _,
            blocks_available: stats.free_clusters() as _,

            file_count: 0,
            free_file_count: 0,

            name_length: MAX_NAME_LEN as _,
            fragment_size: cluster_size,
            mount_flags: 0,
        })
    }

    fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        MetadataUpdateCapabilities::ATIME | MetadataUpdateCapabilities::MTIME
    }

    fn flush(&self) -> VfsResult<()> {
        crate::highlevel::sync_cached_file_pages_for_filesystem(self)?;
        self.inner.lock().inner.flush().map_err(into_vfs_err)?;
        self.device.lock().flush().map_err(|_| VfsError::Io)
    }

    fn unmount(&self) {
        self.root_dir.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_inode_slots_have_a_hard_reusable_ceiling() {
        let allocator = FatInodeAllocator::new();
        for _ in 0..MAX_LIVE_FAT_NODES {
            assert!(allocator.allocate().is_ok());
        }
        assert_eq!(allocator.allocate(), Err(VfsError::NoMemory));

        const RELEASED: u64 = 123;
        allocator.release(RELEASED);
        assert_eq!(allocator.allocate(), Ok(RELEASED));
    }
}
